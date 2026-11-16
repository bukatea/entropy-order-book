//! A test-only replication simulation.
//!
//! Independent engines share a committed baseline. Gossip arrives in a
//! different order at each node and is applied optimistically; those results
//! are provisional. A canonical block discards them: every node replays the
//! block's commands from its committed baseline with fresh canonical
//! sequence numbers, then rebuilds speculation from what is still pending.
//! A message's identity is not its log position.

use orderbook::canonical::{Digest, write_event, write_state};
use orderbook::domain::{OrderId, Owner, Price, Quantity, Side, TimeInForce};
use orderbook::engine::{Engine, replay};
use orderbook::event::{CancelReason, CommandKind, Cursor, Event, HaltReason, RejectCode};
use orderbook::input::parse_line;

const HEADER: &str = "seq,cmd,id,owner,side,price,size,tif\n";
const RULES_VERSION: u32 = 1;
const BASELINE_HEIGHT: u64 = 1;

/// A gossiped command: a test-only identity and a CSV row without its seq.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Msg {
    msg_id: &'static str,
    body: &'static str,
}

const CANCEL_A1: Msg = Msg {
    msg_id: "m-cancel",
    body: "cancel,a1,,,,,",
};
const BUY_ALL: Msg = Msg {
    msg_id: "m-buy",
    body: "place,b1,bob,buy,100.00,5,GTC",
};
const BUY_PART: Msg = Msg {
    msg_id: "m-buy-part",
    body: "place,b1,bob,buy,100.00,2,GTC",
};

/// Canonical commands for one height, in canonical order.
struct Block {
    height: u64,
    rules_version: u32,
    msgs: Vec<Msg>,
}

/// A trusted state commitment for one height and rules version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Commitment {
    height: u64,
    rules_version: u32,
    digest: Digest,
}

/// One applied message: its identity, the seq it was given and its events.
struct Entry {
    msg_id: &'static str,
    seq: u64,
    events: Vec<Event>,
}

struct Node {
    committed: Engine,
    committed_height: u64,
    /// The entries of the last committed block.
    committed_log: Vec<Entry>,
    speculative: Engine,
    provisional_log: Vec<Entry>,
    pending: Vec<Msg>,
}

impl Node {
    fn new(baseline: &Engine, height: u64) -> Self {
        Self {
            committed: baseline.clone(),
            committed_height: height,
            committed_log: Vec::new(),
            speculative: baseline.clone(),
            provisional_log: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Receives gossip; a message already pending is not applied twice.
    fn gossip(&mut self, msg: Msg) {
        if !self.pending.iter().any(|p| p.msg_id == msg.msg_id) {
            self.pending.push(msg);
        }
        self.speculate();
    }

    /// Rebuilds speculation from the committed state, in local arrival order.
    fn speculate(&mut self) {
        (self.speculative, self.provisional_log) = run(&self.committed, &self.pending);
    }

    /// Commits a block, then rebuilds speculation once.
    fn on_block(&mut self, block: &Block) {
        self.commit(block);
        self.speculate();
    }

    /// Replaces committed state with a known-good baseline plus canonical
    /// blocks. Pending gossip is kept, and speculation is rebuilt once at the
    /// end, even with no blocks, so none of it rests on the replaced state.
    fn resync(&mut self, baseline: &Engine, height: u64, blocks: &[Block]) {
        self.committed = baseline.clone();
        self.committed_height = height;
        self.committed_log = Vec::new();
        for block in blocks {
            self.commit(block);
        }
        self.speculate();
    }

    /// Applies a block to the committed baseline, never to speculation, and
    /// drops its messages from the pending set by identity.
    fn commit(&mut self, block: &Block) {
        assert_eq!(block.height, self.committed_height + 1);
        assert_eq!(block.rules_version, RULES_VERSION);
        (self.committed, self.committed_log) = run(&self.committed, &block.msgs);
        self.committed_height = block.height;
        self.pending
            .retain(|p| !block.msgs.iter().any(|m| m.msg_id == p.msg_id));
    }

    fn commitment(&self) -> Commitment {
        Commitment {
            height: self.committed_height,
            rules_version: RULES_VERSION,
            digest: self.committed.digest(),
        }
    }

    fn committed_events(&self) -> Vec<Event> {
        flatten(&self.committed_log)
    }

    fn provisional_events(&self) -> Vec<Event> {
        flatten(&self.provisional_log)
    }

    fn provisional_seq(&self, msg_id: &str) -> Option<u64> {
        position(&self.provisional_log, msg_id)
    }

    fn committed_seq(&self, msg_id: &str) -> Option<u64> {
        position(&self.committed_log, msg_id)
    }
}

fn next_seq(engine: &Engine) -> u64 {
    match engine.cursor() {
        Cursor::Next(seq) => seq,
        Cursor::Exhausted => panic!("test cursors never run out"),
    }
}

/// Applies `msgs` to a clone of `base`, each at the next seq after its cursor.
fn run(base: &Engine, msgs: &[Msg]) -> (Engine, Vec<Entry>) {
    let mut engine = base.clone();
    let mut log = Vec::new();
    for msg in msgs {
        let seq = next_seq(&engine);
        let row = format!("{seq},{}", msg.body);
        let events = engine
            .apply(parse_line(row.as_bytes()))
            .expect("test engines never halt");
        assert_eq!(engine.halt(), None, "{row}");
        log.push(Entry {
            msg_id: msg.msg_id,
            seq,
            events,
        });
    }
    (engine, log)
}

fn flatten(log: &[Entry]) -> Vec<Event> {
    log.iter().flat_map(|e| e.events.iter().cloned()).collect()
}

fn position(log: &[Entry], msg_id: &str) -> Option<u64> {
    log.iter().find(|e| e.msg_id == msg_id).map(|e| e.seq)
}

/// The block replayed from the baseline, and the commitment it yields.
fn reference(baseline: &Engine, block: &Block) -> (Engine, Vec<Event>, Commitment) {
    let (engine, log) = run(baseline, &block.msgs);
    let commitment = Commitment {
        height: block.height,
        rules_version: block.rules_version,
        digest: engine.digest(),
    };
    (engine, flatten(&log), commitment)
}

/// Checks every node's committed events, state bytes and digest against
/// each other and a reference replay. Returns the reference events.
fn assert_converged(nodes: &[&Node], baseline: &Engine, block: &Block) -> Vec<Event> {
    let (want, want_events, trusted) = reference(baseline, block);
    for (i, node) in nodes.iter().enumerate() {
        assert_eq!(
            encode_events(&node.committed_events()),
            encode_events(&want_events),
            "node {i}"
        );
        assert_eq!(
            encode_state(&node.committed),
            encode_state(&want),
            "node {i}"
        );
        assert_eq!(node.commitment(), trusted, "node {i}");
        assert_eq!(node.committed, want, "node {i}");
        // Nothing is left pending, so speculation is the committed state.
        assert!(node.pending.is_empty(), "node {i}");
        assert_eq!(node.speculative, node.committed, "node {i}");
    }
    want_events
}

fn baseline() -> Engine {
    let (engine, _) = replay(format!("{HEADER}1,place,a1,alice,sell,100.00,5,GTC\n").as_bytes());
    assert_eq!(engine.cursor(), Cursor::Next(2));
    engine
}

fn three_nodes(baseline: &Engine) -> [Node; 3] {
    [0; 3].map(|_| Node::new(baseline, BASELINE_HEIGHT))
}

fn block(msgs: &[Msg]) -> Block {
    Block {
        height: BASELINE_HEIGHT + 1,
        rules_version: RULES_VERSION,
        msgs: msgs.to_vec(),
    }
}

/// Lots of order `of` that were filled as a maker and that were cancelled.
fn settled(events: &[Event], of: &str) -> (u32, u32) {
    let of = id(of);
    let mut filled = 0;
    let mut cancelled = 0;
    for event in events {
        match event {
            Event::Trade {
                maker, quantity, ..
            } if *maker == of => filled += quantity.lots(),
            Event::Cancelled { id, remaining, .. } if *id == of => cancelled += remaining.lots(),
            _ => {}
        }
    }
    (filled, cancelled)
}

/// Visible depth: total lots per side and price, ignoring owners and IDs.
fn levels(engine: &Engine) -> Vec<(Side, u32, u32)> {
    let mut levels: Vec<(Side, u32, u32)> = Vec::new();
    for order in engine.bids().chain(engine.asks()) {
        let key = (order.side, order.price.cents());
        match levels.last_mut() {
            Some(last) if (last.0, last.1) == key => last.2 += order.remaining.lots(),
            _ => levels.push((key.0, key.1, order.remaining.lots())),
        }
    }
    levels
}

fn id(text: &str) -> OrderId {
    OrderId::new(text.as_bytes()).expect("test IDs are valid")
}

fn owner(text: &str) -> Owner {
    Owner::new(text.as_bytes()).expect("test owners are valid")
}

fn price(cents: u32) -> Price {
    Price::from_cents(cents).expect("test prices are in range")
}

fn qty(lots: u32) -> Quantity {
    Quantity::new(lots).expect("test quantities are in range")
}

fn accepted_b1(seq: u64, size: u32) -> Event {
    Event::Accepted {
        seq,
        id: id("b1"),
        owner: owner("bob"),
        side: Side::Buy,
        price: price(10000),
        size: qty(size),
        tif: TimeInForce::Gtc,
    }
}

fn encode_events(events: &[Event]) -> Vec<u8> {
    let mut out = Vec::new();
    for event in events {
        write_event(&mut out, event).expect("writing to a Vec cannot fail");
    }
    out
}

fn encode_state(engine: &Engine) -> Vec<u8> {
    let mut out = Vec::new();
    write_state(&mut out, engine).expect("writing to a Vec cannot fail");
    out
}

// exkg-verifies: fea-ky6m7m#ac-replication
#[test]
fn cancel_first_block_replaces_provisional_fills() {
    let base = baseline();
    let [mut n1, mut n2, mut n3] = three_nodes(&base);
    n1.gossip(BUY_ALL);
    n1.gossip(CANCEL_A1);
    n2.gossip(CANCEL_A1);
    n2.gossip(BUY_ALL);
    // n3 never hears the cancel.
    n3.gossip(BUY_ALL);

    // Provisional views differ: n1 and n3 filled a1, n2 cancelled it.
    assert_eq!(settled(&n1.provisional_events(), "a1"), (5, 0));
    assert_eq!(settled(&n2.provisional_events(), "a1"), (0, 5));
    assert_eq!(settled(&n3.provisional_events(), "a1"), (5, 0));
    assert_ne!(n1.speculative.digest(), n2.speculative.digest());
    assert_ne!(n1.speculative.digest(), n3.speculative.digest());
    assert_ne!(n2.speculative.digest(), n3.speculative.digest());
    // The same payload sat at different local positions.
    assert_eq!(n1.provisional_seq("m-cancel"), Some(3));
    assert_eq!(n2.provisional_seq("m-cancel"), Some(2));
    assert_eq!(n3.provisional_seq("m-cancel"), None);

    let block = block(&[CANCEL_A1, BUY_ALL]);
    for node in [&mut n1, &mut n2, &mut n3] {
        node.on_block(&block);
        assert_eq!(node.committed_seq("m-cancel"), Some(2));
        assert_eq!(node.committed_seq("m-buy"), Some(3));
    }
    let events = assert_converged(&[&n1, &n2, &n3], &base, &block);

    // The cancel wins; the buy finds nothing to cross and rests.
    assert_eq!(
        events,
        [
            Event::Cancelled {
                seq: 2,
                id: id("a1"),
                remaining: qty(5),
                reason: CancelReason::User,
            },
            accepted_b1(3, 5),
            Event::Rested {
                seq: 3,
                id: id("b1"),
                side: Side::Buy,
                price: price(10000),
                remaining: qty(5),
                rank: 2,
            },
        ]
    );
    // n1's and n3's provisional fills are replaced, not settled as well.
    assert_eq!(settled(&n1.committed_events(), "a1"), (0, 5));
}

// exkg-verifies: fea-ky6m7m#ac-replication
#[test]
fn fill_first_block_replaces_provisional_cancels() {
    let base = baseline();
    let [mut n1, mut n2, mut n3] = three_nodes(&base);
    n1.gossip(CANCEL_A1);
    n1.gossip(BUY_ALL);
    n2.gossip(BUY_ALL);
    n2.gossip(CANCEL_A1);
    // n3 never hears the buy.
    n3.gossip(CANCEL_A1);

    assert_eq!(settled(&n1.provisional_events(), "a1"), (0, 5));
    assert_eq!(settled(&n2.provisional_events(), "a1"), (5, 0));
    assert_eq!(settled(&n3.provisional_events(), "a1"), (0, 5));
    assert_ne!(n1.speculative.digest(), n2.speculative.digest());
    assert_ne!(n1.speculative.digest(), n3.speculative.digest());

    let fill_first = block(&[BUY_ALL, CANCEL_A1]);
    for node in [&mut n1, &mut n2, &mut n3] {
        node.on_block(&fill_first);
    }
    let events = assert_converged(&[&n1, &n2, &n3], &base, &fill_first);

    // The crossing buy fills a1; the late cancel finds it closed.
    assert_eq!(
        events,
        [
            accepted_b1(2, 5),
            Event::Trade {
                seq: 2,
                taker: id("b1"),
                maker: id("a1"),
                price: price(10000),
                quantity: qty(5),
                taker_left: 0,
                maker_left: 0,
            },
            Event::Rejected {
                seq: 3,
                command: CommandKind::Cancel,
                id: Some(id("a1")),
                reason: RejectCode::ClosedId,
            },
        ]
    );
    for node in [&n1, &n2, &n3] {
        assert_eq!(settled(&node.committed_events(), "a1"), (5, 0));
    }

    // Canonical order decides the race: the other order is another outcome.
    let (cancel_state, cancel_events, cancel_commitment) =
        reference(&base, &block(&[CANCEL_A1, BUY_ALL]));
    assert_ne!(encode_events(&cancel_events), encode_events(&events));
    assert_ne!(encode_state(&cancel_state), encode_state(&n1.committed));
    assert_ne!(cancel_commitment, n1.commitment());
}

// exkg-verifies: fea-ky6m7m#ac-replication
#[test]
fn partial_fill_then_cancel_settles_the_leftover_once() {
    let base = baseline();
    let [mut n1, mut n2, mut n3] = three_nodes(&base);
    n1.gossip(CANCEL_A1);
    n1.gossip(BUY_PART);
    n2.gossip(BUY_PART);
    n2.gossip(CANCEL_A1);
    // n3 never hears the partial buy.
    n3.gossip(CANCEL_A1);

    assert_eq!(settled(&n1.provisional_events(), "a1"), (0, 5));
    assert_eq!(settled(&n2.provisional_events(), "a1"), (2, 3));
    assert_eq!(settled(&n3.provisional_events(), "a1"), (0, 5));

    let block = block(&[BUY_PART, CANCEL_A1]);
    for node in [&mut n1, &mut n2, &mut n3] {
        node.on_block(&block);
    }
    let events = assert_converged(&[&n1, &n2, &n3], &base, &block);

    assert_eq!(
        events,
        [
            accepted_b1(2, 2),
            Event::Trade {
                seq: 2,
                taker: id("b1"),
                maker: id("a1"),
                price: price(10000),
                quantity: qty(2),
                taker_left: 0,
                maker_left: 3,
            },
            Event::Cancelled {
                seq: 3,
                id: id("a1"),
                remaining: qty(3),
                reason: CancelReason::User,
            },
        ]
    );
    // The provisional cancel of 5 lots is replaced by 2 filled and 3 cancelled.
    for node in [&n1, &n2, &n3] {
        assert_eq!(settled(&node.committed_events(), "a1"), (2, 3));
    }
}

// exkg-verifies: fea-ky6m7m#ac-replication
#[test]
fn pending_messages_get_fresh_positions_after_a_block() {
    let bid_c1 = Msg {
        msg_id: "m-c1",
        body: "place,c1,carol,buy,90.00,1,GTC",
    };
    let bid_c2 = Msg {
        msg_id: "m-c2",
        body: "place,c2,dave,buy,91.00,1,GTC",
    };
    let ask_z1 = Msg {
        msg_id: "m-z1",
        body: "place,z1,zed,sell,120.00,1,GTC",
    };
    let base = baseline();
    let mut node = Node::new(&base, BASELINE_HEIGHT);
    node.gossip(bid_c1);
    node.gossip(bid_c2);
    node.gossip(bid_c1);
    node.gossip(CANCEL_A1);
    assert_eq!(node.pending, [bid_c1, bid_c2, CANCEL_A1]);
    assert_eq!(node.provisional_seq("m-c1"), Some(2));
    assert_eq!(node.provisional_seq("m-c2"), Some(3));
    let local = node
        .provisional_seq("m-cancel")
        .expect("the cancel is pending");
    assert_eq!(local, 4);

    // The block includes a message this node never saw.
    let block = block(&[ask_z1, CANCEL_A1]);
    node.on_block(&block);
    assert_eq!(node.committed_seq("m-z1"), Some(2));
    assert_eq!(node.committed_seq("m-cancel"), Some(3));

    // Included messages leave the pending set by identity; the rest are
    // re-sequenced after the new committed cursor.
    assert_eq!(node.pending, [bid_c1, bid_c2]);
    assert_eq!(node.provisional_seq("m-c1"), Some(4));
    assert_eq!(node.provisional_seq("m-c2"), Some(5));
    let (want, _, _) = reference(&base, &block);
    assert_eq!(node.committed, want);
    assert_eq!(node.speculative, run(&want, &[bid_c1, bid_c2]).0);

    // Reusing the gossip-local position on the canonical log is a gap.
    let (mut after_z1, _) = run(&base, &[ask_z1]);
    let row = format!("{local},{}", CANCEL_A1.body);
    let events = after_z1.apply(parse_line(row.as_bytes())).unwrap();
    assert_eq!(after_z1.halt(), Some(HaltReason::Gap));
    assert!(matches!(events[..], [Event::Halted { .. }]));
}

// exkg-verifies: fea-ky6m7m#ac-replication
#[test]
fn equal_depth_corruption_is_detected_and_rebuilt_from_the_baseline() {
    let good_csv = "1,place,a1,alice,sell,100.00,5,GTC\n2,place,a2,carol,sell,101.00,3,GTC\n";
    // Built independently: a1 has another owner and a2 another ID.
    let bad_csv = "1,place,a1,mallory,sell,100.00,5,GTC\n2,place,x2,carol,sell,101.00,3,GTC\n";
    let (good_base, _) = replay(format!("{HEADER}{good_csv}").as_bytes());
    let (bad_base, _) = replay(format!("{HEADER}{bad_csv}").as_bytes());
    assert_eq!(levels(&good_base), levels(&bad_base));

    let mut honest = Node::new(&good_base, BASELINE_HEIGHT);
    let mut bad = Node::new(&bad_base, BASELINE_HEIGHT);
    let block = block(&[Msg {
        msg_id: "m-d1",
        body: "place,d1,dave,buy,100.00,2,GTC",
    }]);
    honest.on_block(&block);
    bad.on_block(&block);
    let (_, _, trusted) = reference(&good_base, &block);
    assert_eq!(honest.commitment(), trusted);

    // Depth and even the block's events agree; only full state differs.
    assert_eq!(levels(&bad.committed), levels(&honest.committed));
    assert_eq!(
        levels(&bad.committed),
        [(Side::Sell, 10000, 3), (Side::Sell, 10100, 3)]
    );
    assert_eq!(
        encode_events(&bad.committed_events()),
        encode_events(&honest.committed_events())
    );
    let claimed = bad.commitment();
    assert_eq!(
        (claimed.height, claimed.rules_version),
        (trusted.height, trusted.rules_version)
    );
    assert_ne!(claimed, trusted);
    assert_ne!(
        encode_state(&bad.committed),
        encode_state(&honest.committed)
    );

    bad.resync(&good_base, BASELINE_HEIGHT, &[block]);
    assert_eq!(bad.commitment(), trusted);
    assert_eq!(
        encode_state(&bad.committed),
        encode_state(&honest.committed)
    );
    assert_eq!(bad.committed, honest.committed);
}

// exkg-verifies: fea-ky6m7m#ac-replication
#[test]
fn empty_recovery_rebuilds_pending_speculation_on_the_restored_baseline() {
    let good_base = baseline();
    // Built independently: a1 rests one tick higher, so bob's bid cannot cross.
    let (bad_base, _) = replay(format!("{HEADER}1,place,a1,alice,sell,101.00,5,GTC\n").as_bytes());
    let mut node = Node::new(&bad_base, BASELINE_HEIGHT);
    node.gossip(BUY_PART);
    assert_eq!(settled(&node.provisional_events(), "a1"), (0, 0));

    node.resync(&good_base, BASELINE_HEIGHT, &[]);

    assert_eq!(node.commitment().height, BASELINE_HEIGHT);
    assert_eq!(node.committed, good_base);
    assert!(node.committed_events().is_empty());
    assert_eq!(node.pending, [BUY_PART]);
    // The stale resting bid is replaced by a fill against the restored a1.
    assert_eq!(
        node.provisional_events(),
        [
            accepted_b1(2, 2),
            Event::Trade {
                seq: 2,
                taker: id("b1"),
                maker: id("a1"),
                price: price(10000),
                quantity: qty(2),
                taker_left: 0,
                maker_left: 3,
            },
        ]
    );
    assert_eq!(
        encode_state(&node.speculative),
        b"eob-state,1\nnext_seq,3\nhalted,none\npriority_clock,1\n\
          bids,0\nasks,1\norder,a1,alice,sell,10000,3,GTC,1\n\
          ids,2\nid,a1\nid,b1\n"
    );
}

// exkg-verifies: fea-ky6m7m#ac-replication
#[test]
fn multi_block_recovery_drops_included_messages_and_respeculates_the_rest() {
    let bid_c1 = Msg {
        msg_id: "m-c1",
        body: "place,c1,carol,buy,90.00,1,GTC",
    };
    let ask_z1 = Msg {
        msg_id: "m-z1",
        body: "place,z1,zed,sell,120.00,1,GTC",
    };
    let good_base = baseline();
    let (bad_base, _) =
        replay(format!("{HEADER}1,place,a1,mallory,sell,100.00,5,GTC\n").as_bytes());
    let mut node = Node::new(&bad_base, BASELINE_HEIGHT);
    node.gossip(CANCEL_A1);
    node.gossip(bid_c1);
    node.gossip(BUY_PART);
    assert_eq!(node.provisional_seq("m-c1"), Some(3));

    let blocks = [
        block(&[BUY_PART]),
        // Includes a message this node never saw.
        Block {
            height: BASELINE_HEIGHT + 2,
            rules_version: RULES_VERSION,
            msgs: vec![ask_z1, CANCEL_A1],
        },
    ];
    node.resync(&good_base, BASELINE_HEIGHT, &blocks);

    assert_eq!(node.commitment().height, BASELINE_HEIGHT + 2);
    // The committed log holds the last block, at canonical positions.
    assert_eq!(node.committed_seq("m-z1"), Some(3));
    assert_eq!(node.committed_seq("m-cancel"), Some(4));
    assert_eq!(
        node.committed_events(),
        [
            Event::Accepted {
                seq: 3,
                id: id("z1"),
                owner: owner("zed"),
                side: Side::Sell,
                price: price(12000),
                size: qty(1),
                tif: TimeInForce::Gtc,
            },
            Event::Rested {
                seq: 3,
                id: id("z1"),
                side: Side::Sell,
                price: price(12000),
                remaining: qty(1),
                rank: 2,
            },
            Event::Cancelled {
                seq: 4,
                id: id("a1"),
                remaining: qty(3),
                reason: CancelReason::User,
            },
        ]
    );
    assert_eq!(
        encode_state(&node.committed),
        b"eob-state,1\nnext_seq,5\nhalted,none\npriority_clock,2\n\
          bids,0\nasks,1\norder,z1,zed,sell,12000,1,GTC,2\n\
          ids,3\nid,a1\nid,b1\nid,z1\n"
    );

    // Both included messages left by identity; c1 moved to a fresh position.
    assert_eq!(node.pending, [bid_c1]);
    assert_eq!(node.provisional_seq("m-c1"), Some(5));
    assert_eq!(
        encode_state(&node.speculative),
        b"eob-state,1\nnext_seq,6\nhalted,none\npriority_clock,3\n\
          bids,1\norder,c1,carol,buy,9000,1,GTC,3\n\
          asks,1\norder,z1,zed,sell,12000,1,GTC,2\n\
          ids,4\nid,a1\nid,b1\nid,c1\nid,z1\n"
    );
}

// exkg-verifies: fea-ky6m7m#ac-replication
// exkg-verifies: fea-ky6m7m#ac-digest
#[test]
fn different_event_histories_converge_to_equal_state_and_digest() {
    let rest = "1,place,a1,alice,buy,100.00,5,GTC\n";
    let histories = [
        "2,cancel,zz,,,,,\n",
        "2,cancel,yy,,,,,\n",
        "2,modify,qq,,,1.00,1,\n",
    ]
    .map(|tail| replay(format!("{HEADER}{rest}{tail}").as_bytes()));

    let (first, first_events) = &histories[0];
    for (engine, events) in &histories[1..] {
        // Unknown IDs reserve nothing, so only the rejection differs.
        assert_ne!(encode_events(events), encode_events(first_events));
        assert_eq!(encode_state(engine), encode_state(first));
        assert_eq!(engine.digest(), first.digest());
    }
}
