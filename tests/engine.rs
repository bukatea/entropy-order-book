use orderbook::domain::{OrderId, Side, TimeInForce};
use orderbook::engine::{AlreadyHalted, Engine, RestingOrder, replay};
use orderbook::event::{CancelReason, Cursor, Event, HaltReason, Priority};
use orderbook::input::{HeaderError, parse_line};

fn side(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn tif(tif: TimeInForce) -> &'static str {
    match tif {
        TimeInForce::Gtc => "GTC",
        TimeInForce::Ioc => "IOC",
        TimeInForce::PostOnly => "POST_ONLY",
    }
}

/// A compact test-only rendering, with prices in cents. It is not the
/// canonical encoder; it only keeps the expected events readable.
fn show(event: &Event) -> String {
    match event {
        Event::Accepted {
            seq,
            id,
            owner,
            side: s,
            price,
            size,
            tif: t,
        } => format!(
            "{seq},accepted,{},{},{},{},{},{}",
            id.as_str(),
            owner.as_str(),
            side(*s),
            price.cents(),
            size.lots(),
            tif(*t)
        ),
        Event::Rejected {
            seq,
            command,
            id,
            reason,
        } => format!(
            "{seq},rejected,{command:?},{},{reason:?}",
            id.as_ref().map_or("", |id| id.as_str())
        ),
        Event::Trade {
            seq,
            taker,
            maker,
            price,
            quantity,
            taker_left,
            maker_left,
        } => format!(
            "{seq},trade,{},{},{},{},{taker_left},{maker_left}",
            taker.as_str(),
            maker.as_str(),
            price.cents(),
            quantity.lots()
        ),
        Event::Rested {
            seq,
            id,
            side: s,
            price,
            remaining,
            rank,
        } => format!(
            "{seq},rested,{},{},{},{},{rank}",
            id.as_str(),
            side(*s),
            price.cents(),
            remaining.lots()
        ),
        Event::Cancelled {
            seq,
            id,
            remaining,
            reason,
        } => {
            let reason = match reason {
                CancelReason::User => "user",
                CancelReason::IocRemainder => "ioc_remainder",
                CancelReason::SelfTrade => "self_trade",
            };
            format!(
                "{seq},cancelled,{},{},{reason}",
                id.as_str(),
                remaining.lots()
            )
        }
        Event::Modified {
            seq,
            id,
            price,
            remaining,
            priority,
            rank,
        } => {
            let priority = match priority {
                Priority::Kept => "kept",
                Priority::Lost => "lost",
            };
            format!(
                "{seq},modified,{},{},{},{priority},{rank}",
                id.as_str(),
                price.cents(),
                remaining.lots()
            )
        }
        Event::Halted {
            expected,
            reason,
            observed,
        } => {
            let expected = match expected {
                Cursor::Next(seq) => seq.to_string(),
                Cursor::Exhausted => "exhausted".to_owned(),
            };
            let observed = observed.map_or("none".to_owned(), |seq| seq.to_string());
            format!("{expected},halted,{reason:?},{observed}")
        }
    }
}

/// Applies one CSV line, checks every invariant and renders the events.
fn feed(engine: &mut Engine, line: &str) -> Vec<String> {
    let events = engine
        .apply(parse_line(line.as_bytes()))
        .expect("engine is running");
    check(engine);
    events.iter().map(show).collect()
}

fn row(order: &RestingOrder) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}",
        order.id.as_str(),
        order.owner.as_str(),
        order.price.cents(),
        order.remaining.lots(),
        tif(order.tif),
        order.rank
    )
}

fn bids(engine: &Engine) -> Vec<String> {
    engine.bids().map(row).collect()
}

fn asks(engine: &Engine) -> Vec<String> {
    engine.asks().map(row).collect()
}

fn ids(engine: &Engine) -> Vec<&str> {
    engine.reserved_ids().map(|id| id.as_str()).collect()
}

/// Checks the sorted views against each other and against the ID index.
fn check(engine: &Engine) {
    let bids: Vec<_> = engine.bids().collect();
    let asks: Vec<_> = engine.asks().collect();
    for pair in bids.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        assert!(
            a.price > b.price || (a.price == b.price && a.rank < b.rank),
            "bid order"
        );
    }
    for pair in asks.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        assert!(
            a.price < b.price || (a.price == b.price && a.rank < b.rank),
            "ask order"
        );
    }
    if let (Some(bid), Some(ask)) = (bids.first(), asks.first()) {
        assert!(bid.price < ask.price, "crossed book");
    }
    assert_eq!(engine.live_count(), bids.len() + asks.len());
    let reserved: Vec<_> = engine.reserved_ids().collect();
    assert!(
        reserved.windows(2).all(|w| w[0] < w[1]),
        "reserved IDs sorted"
    );
    let mut ranks = Vec::new();
    for (orders, expected_side) in [(&bids, Side::Buy), (&asks, Side::Sell)] {
        for order in orders.iter() {
            assert_eq!(order.side, expected_side);
            assert_eq!(engine.order(&order.id), Some(*order));
            assert!(reserved.contains(&&order.id));
            assert!(order.tif != TimeInForce::Ioc, "IOC never rests");
            assert!((1..=engine.priority_clock()).contains(&order.rank));
            ranks.push(order.rank);
        }
    }
    ranks.sort_unstable();
    ranks.dedup();
    assert_eq!(ranks.len(), engine.live_count(), "ranks are unique");
}

fn place(seq: u64, id: &str, owner: &str, side: &str, price: &str, size: u32, tif: &str) -> String {
    format!("{seq},place,{id},{owner},{side},{price},{size},{tif}")
}

fn cancel(seq: u64, id: &str) -> String {
    format!("{seq},cancel,{id},,,,,")
}

fn modify(seq: u64, id: &str, price: &str, size: u32) -> String {
    format!("{seq},modify,{id},,,{price},{size},")
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[test]
fn new_engine_is_empty_and_running() {
    let engine = Engine::new();
    assert_eq!(engine.cursor(), Cursor::Next(1));
    assert_eq!(engine.halt(), None);
    assert_eq!(engine.priority_clock(), 0);
    assert_eq!(engine.live_count(), 0);
    assert!(bids(&engine).is_empty() && asks(&engine).is_empty() && ids(&engine).is_empty());
    check(&engine);
}

// exkg-verifies: fea-ky6m7m#ac-matching
#[test]
fn ioc_buy_sweeps_asks_by_price_then_rank() {
    let mut e = Engine::new();
    feed(&mut e, &place(1, "m1", "o1", "sell", "101", 2, "GTC"));
    feed(&mut e, &place(2, "m2", "o2", "sell", "100", 1, "GTC"));
    assert_eq!(
        feed(&mut e, &place(3, "m3", "o3", "sell", "101", 1, "GTC")),
        [
            "3,accepted,m3,o3,sell,10100,1,GTC",
            "3,rested,m3,sell,10100,1,3"
        ]
    );
    assert_eq!(
        asks(&e),
        [
            "m2:o2:10000:1:GTC:2",
            "m1:o1:10100:2:GTC:1",
            "m3:o3:10100:1:GTC:3"
        ]
    );
    assert_eq!(
        feed(&mut e, &place(4, "t1", "o4", "buy", "102", 4, "IOC")),
        [
            "4,accepted,t1,o4,buy,10200,4,IOC",
            "4,trade,t1,m2,10000,1,3,0",
            "4,trade,t1,m1,10100,2,1,0",
            "4,trade,t1,m3,10100,1,0,0",
        ]
    );
    assert!(asks(&e).is_empty() && bids(&e).is_empty());
    assert_eq!(e.priority_clock(), 3);
    assert_eq!(ids(&e), ["m1", "m2", "m3", "t1"]);
}

// exkg-verifies: fea-ky6m7m#ac-matching
#[test]
fn partial_bid_maker_keeps_priority_over_equal_price() {
    let mut e = Engine::new();
    feed(&mut e, &place(1, "b1", "o1", "buy", "100", 3, "GTC"));
    feed(&mut e, &place(2, "b2", "o2", "buy", "100", 2, "GTC"));
    feed(&mut e, &place(3, "b0", "o5", "buy", "99", 9, "GTC"));
    assert_eq!(
        feed(&mut e, &place(4, "s1", "o3", "sell", "100", 1, "IOC")),
        [
            "4,accepted,s1,o3,sell,10000,1,IOC",
            "4,trade,s1,b1,10000,1,0,2"
        ]
    );
    assert_eq!(
        bids(&e),
        [
            "b1:o1:10000:2:GTC:1",
            "b2:o2:10000:2:GTC:2",
            "b0:o5:9900:9:GTC:3"
        ]
    );
    assert_eq!(
        feed(&mut e, &place(5, "s2", "o4", "sell", "100", 5, "GTC")),
        [
            "5,accepted,s2,o4,sell,10000,5,GTC",
            "5,trade,s2,b1,10000,2,3,0",
            "5,trade,s2,b2,10000,2,1,0",
            "5,rested,s2,sell,10000,1,4",
        ]
    );
    assert_eq!(bids(&e), ["b0:o5:9900:9:GTC:3"]);
    assert_eq!(asks(&e), ["s2:o4:10000:1:GTC:4"]);
    assert_eq!(e.order(&OrderId::new(b"b1").unwrap()), None);
}

// exkg-verifies: fea-ky6m7m#ac-modify
#[test]
fn modify_keeps_rank_only_without_price_change_or_increase() {
    let mut e = Engine::new();
    feed(&mut e, &place(1, "A", "o1", "buy", "100", 5, "GTC"));
    feed(&mut e, &place(2, "B", "o2", "buy", "100", 5, "GTC"));
    assert_eq!(
        feed(&mut e, &modify(3, "A", "100", 3)),
        ["3,modified,A,10000,3,kept,1"]
    );
    assert_eq!(bids(&e), ["A:o1:10000:3:GTC:1", "B:o2:10000:5:GTC:2"]);
    assert_eq!(
        feed(&mut e, &modify(4, "A", "100.00", 3)),
        ["4,modified,A,10000,3,kept,1"]
    );
    assert_eq!(e.priority_clock(), 2);
    assert_eq!(
        feed(&mut e, &modify(5, "A", "100", 4)),
        ["5,modified,A,10000,4,lost,3", "5,rested,A,buy,10000,4,3"]
    );
    assert_eq!(bids(&e), ["B:o2:10000:5:GTC:2", "A:o1:10000:4:GTC:3"]);
    assert_eq!(
        feed(&mut e, &place(6, "S", "o3", "sell", "100", 6, "IOC")),
        [
            "6,accepted,S,o3,sell,10000,6,IOC",
            "6,trade,S,B,10000,5,1,0",
            "6,trade,S,A,10000,1,0,3",
        ]
    );
    assert_eq!(bids(&e), ["A:o1:10000:3:GTC:3"]);
    assert_eq!(e.priority_clock(), 3);
    // A price change loses priority even when the size decreases.
    assert_eq!(
        feed(&mut e, &modify(7, "A", "99", 2)),
        ["7,modified,A,9900,2,lost,4", "7,rested,A,buy,9900,2,4"]
    );
}

// exkg-verifies: fea-ky6m7m#ac-tif-stp
#[test]
fn self_trade_cancels_own_makers_and_keeps_other_fills() {
    let mut e = Engine::new();
    feed(&mut e, &place(1, "a1", "x1", "sell", "99", 1, "GTC"));
    feed(&mut e, &place(2, "a2", "me", "sell", "100", 2, "GTC"));
    feed(&mut e, &place(3, "a3", "x2", "sell", "100", 1, "GTC"));
    feed(&mut e, &place(4, "a4", "me", "sell", "101", 1, "GTC"));
    feed(&mut e, &place(5, "a5", "x3", "sell", "101", 1, "GTC"));
    feed(&mut e, &place(6, "a6", "x4", "sell", "102", 1, "GTC"));
    assert_eq!(
        feed(&mut e, &place(7, "t", "me", "buy", "101", 5, "IOC")),
        [
            "7,accepted,t,me,buy,10100,5,IOC",
            "7,trade,t,a1,9900,1,4,0",
            "7,cancelled,a2,2,self_trade",
            "7,trade,t,a3,10000,1,3,0",
            "7,cancelled,a4,1,self_trade",
            "7,trade,t,a5,10100,1,2,0",
            "7,cancelled,t,2,ioc_remainder",
        ]
    );
    assert_eq!(asks(&e), ["a6:x4:10200:1:GTC:6"]);
    assert_eq!(e.priority_clock(), 6);
}

// exkg-verifies: fea-ky6m7m#ac-tif-stp
#[test]
fn self_trade_on_bid_side_then_gtc_rests() {
    let mut e = Engine::new();
    feed(&mut e, &place(1, "b1", "me", "buy", "100", 1, "GTC"));
    feed(&mut e, &place(2, "b2", "x", "buy", "100", 1, "GTC"));
    assert_eq!(
        feed(&mut e, &place(3, "s", "me", "sell", "100", 3, "GTC")),
        [
            "3,accepted,s,me,sell,10000,3,GTC",
            "3,cancelled,b1,1,self_trade",
            "3,trade,s,b2,10000,1,2,0",
            "3,rested,s,sell,10000,2,3",
        ]
    );
    assert!(bids(&e).is_empty());
    assert_eq!(asks(&e), ["s:me:10000:2:GTC:3"]);
}

// exkg-verifies: fea-ky6m7m#ac-tif-stp
#[test]
fn crossing_post_only_rejects_before_self_trade() {
    let mut e = Engine::new();
    feed(&mut e, &place(1, "a1", "alice", "sell", "100", 1, "GTC"));
    let before = asks(&e);
    assert_eq!(
        feed(
            &mut e,
            &place(2, "b1", "alice", "buy", "100", 1, "POST_ONLY")
        ),
        ["2,rejected,Place,b1,PostOnlyWouldCross"]
    );
    assert_eq!(asks(&e), before);
    assert!(bids(&e).is_empty());
    assert_eq!(ids(&e), ["a1", "b1"]);
    assert_eq!(e.priority_clock(), 1);
    assert_eq!(e.cursor(), Cursor::Next(3));
    assert_eq!(
        feed(
            &mut e,
            &place(3, "b2", "alice", "buy", "99.99", 1, "POST_ONLY")
        ),
        [
            "3,accepted,b2,alice,buy,9999,1,POST_ONLY",
            "3,rested,b2,buy,9999,1,2"
        ]
    );
    assert_eq!(
        feed(
            &mut e,
            &place(4, "a2", "bob", "sell", "99.99", 4, "POST_ONLY")
        ),
        ["4,rejected,Place,a2,PostOnlyWouldCross"]
    );
    assert_eq!(bids(&e), ["b2:alice:9999:1:POST_ONLY:2"]);
}

// exkg-verifies: fea-ky6m7m#ac-modify
#[test]
fn priority_losing_modify_matches_then_rests_with_its_one_rank() {
    let mut e = Engine::new();
    feed(&mut e, &place(1, "b1", "Alice", "buy", "100", 1, "GTC"));
    feed(&mut e, &place(2, "m1", "Bob", "sell", "101", 2, "GTC"));
    assert_eq!(
        feed(&mut e, &modify(3, "b1", "101", 3)),
        [
            "3,modified,b1,10100,3,lost,3",
            "3,trade,b1,m1,10100,2,1,0",
            "3,rested,b1,buy,10100,1,3",
        ]
    );
    assert_eq!(bids(&e), ["b1:Alice:10100:1:GTC:3"]);
    assert!(asks(&e).is_empty());
    assert_eq!(e.priority_clock(), 3);
    // A fully filled priority-losing modify still issues its rank.
    feed(&mut e, &place(4, "m2", "Carol", "sell", "102", 1, "GTC"));
    assert_eq!(
        feed(&mut e, &modify(5, "b1", "102", 1)),
        ["5,modified,b1,10200,1,lost,5", "5,trade,b1,m2,10200,1,0,0"]
    );
    assert!(bids(&e).is_empty() && asks(&e).is_empty());
    assert_eq!(e.priority_clock(), 5);
    assert_eq!(
        feed(&mut e, &cancel(6, "b1")),
        ["6,rejected,Cancel,b1,ClosedId"]
    );
}

// exkg-verifies: fea-ky6m7m#ac-modify
#[test]
fn priority_losing_modify_uses_self_trade_prevention() {
    let mut e = Engine::new();
    feed(&mut e, &place(1, "b1", "me", "buy", "99", 2, "GTC"));
    feed(&mut e, &place(2, "a1", "me", "sell", "100", 1, "GTC"));
    feed(&mut e, &place(3, "a2", "x", "sell", "100", 1, "GTC"));
    assert_eq!(
        feed(&mut e, &modify(4, "b1", "100", 2)),
        [
            "4,modified,b1,10000,2,lost,4",
            "4,cancelled,a1,1,self_trade",
            "4,trade,b1,a2,10000,1,1,0",
            "4,rested,b1,buy,10000,1,4",
        ]
    );
}

// exkg-verifies: fea-ky6m7m#ac-modify
#[test]
fn crossing_post_only_modify_rejects_and_keeps_the_original() {
    let mut e = Engine::new();
    feed(
        &mut e,
        &place(1, "b1", "alice", "buy", "100", 2, "POST_ONLY"),
    );
    feed(&mut e, &place(2, "m1", "bob", "sell", "101", 3, "GTC"));
    assert_eq!(
        feed(&mut e, &modify(3, "b1", "101", 3)),
        ["3,rejected,Modify,b1,PostOnlyWouldCross"]
    );
    assert_eq!(bids(&e), ["b1:alice:10000:2:POST_ONLY:1"]);
    assert_eq!(asks(&e), ["m1:bob:10100:3:GTC:2"]);
    assert_eq!(e.priority_clock(), 2);
    assert_eq!(e.cursor(), Cursor::Next(4));
    assert_eq!(
        feed(&mut e, &modify(4, "b1", "100", 5)),
        ["4,modified,b1,10000,5,lost,3", "4,rested,b1,buy,10000,5,3"]
    );
    assert_eq!(bids(&e), ["b1:alice:10000:5:POST_ONLY:3"]);
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[test]
fn ids_distinguish_duplicate_unknown_and_closed() {
    let mut e = Engine::new();
    feed(&mut e, &place(1, "a1", "o1", "sell", "100", 1, "GTC"));
    assert_eq!(
        feed(&mut e, &place(2, "b1", "o2", "buy", "100", 1, "GTC")),
        [
            "2,accepted,b1,o2,buy,10000,1,GTC",
            "2,trade,b1,a1,10000,1,0,0"
        ]
    );
    feed(&mut e, &place(3, "c1", "o3", "buy", "90", 1, "GTC"));
    assert_eq!(feed(&mut e, &cancel(4, "c1")), ["4,cancelled,c1,1,user"]);
    feed(&mut e, &place(5, "d1", "o4", "sell", "95", 1, "GTC"));
    assert_eq!(
        feed(&mut e, &place(6, "e1", "o4", "buy", "95", 1, "IOC")),
        [
            "6,accepted,e1,o4,buy,9500,1,IOC",
            "6,cancelled,d1,1,self_trade",
            "6,cancelled,e1,1,ioc_remainder",
        ]
    );
    feed(&mut e, &place(7, "f1", "o6", "sell", "100", 1, "GTC"));
    assert_eq!(
        feed(&mut e, &place(8, "p1", "o5", "buy", "100", 1, "POST_ONLY")),
        ["8,rejected,Place,p1,PostOnlyWouldCross"]
    );
    assert_eq!(
        feed(&mut e, &place(9, "x1", "o1", "buy", "abc", 1, "GTC")),
        ["9,rejected,Input,,Input(Invalid(Price, Syntax))"]
    );
    assert_eq!(ids(&e), ["a1", "b1", "c1", "d1", "e1", "f1", "p1"]);

    let dup = |seq, id| format!("{seq},rejected,Place,{id},DuplicateId");
    assert_eq!(
        feed(&mut e, &place(10, "a1", "o1", "buy", "1", 1, "GTC")),
        [dup(10, "a1")]
    );
    assert_eq!(
        feed(&mut e, &place(11, "c1", "o1", "buy", "1", 1, "GTC")),
        [dup(11, "c1")]
    );
    assert_eq!(
        feed(&mut e, &place(12, "p1", "o1", "buy", "1", 1, "GTC")),
        [dup(12, "p1")]
    );
    // A duplicate that would cross leaves the live original untouched.
    assert_eq!(
        feed(&mut e, &place(13, "f1", "o9", "buy", "100", 5, "GTC")),
        [dup(13, "f1")]
    );
    assert_eq!(asks(&e), ["f1:o6:10000:1:GTC:4"]);

    assert_eq!(
        feed(&mut e, &cancel(14, "d1")),
        ["14,rejected,Cancel,d1,ClosedId"]
    );
    assert_eq!(
        feed(&mut e, &modify(15, "e1", "1", 1)),
        ["15,rejected,Modify,e1,ClosedId"]
    );
    assert_eq!(
        feed(&mut e, &cancel(16, "p1")),
        ["16,rejected,Cancel,p1,ClosedId"]
    );
    assert_eq!(
        feed(&mut e, &modify(17, "a1", "1", 1)),
        ["17,rejected,Modify,a1,ClosedId"]
    );
    assert_eq!(
        feed(&mut e, &cancel(18, "zz")),
        ["18,rejected,Cancel,zz,UnknownId"]
    );
    assert_eq!(
        feed(&mut e, &modify(19, "zz", "1", 1)),
        ["19,rejected,Modify,zz,UnknownId"]
    );
    assert_eq!(
        feed(&mut e, &cancel(20, "x1")),
        ["20,rejected,Cancel,x1,UnknownId"]
    );
    assert_eq!(
        feed(&mut e, &place(21, "x1", "o1", "buy", "90", 1, "GTC")),
        [
            "21,accepted,x1,o1,buy,9000,1,GTC",
            "21,rested,x1,buy,9000,1,5"
        ]
    );
    assert_eq!(e.cursor(), Cursor::Next(22));
    assert_eq!(e.priority_clock(), 5);
}

fn halts_with(prefix: &[String], line: &str, expected: &str) {
    let mut e = Engine::new();
    for l in prefix {
        feed(&mut e, l);
    }
    let before = e.clone();
    let events = e.apply(parse_line(line.as_bytes())).expect("running");
    assert_eq!(
        events.iter().map(show).collect::<Vec<_>>(),
        [expected],
        "{line}"
    );
    assert!(e.halt().is_some());
    // The halting line and everything after it change nothing else.
    let mut halted_state = e.clone();
    assert_eq!(e.cursor(), before.cursor());
    assert_eq!(bids(&e), bids(&before));
    assert_eq!(ids(&e), ids(&before));
    let next = place(before_next(&before), "z9", "o", "buy", "1", 1, "GTC");
    assert_eq!(e.apply(parse_line(next.as_bytes())), Err(AlreadyHalted));
    assert_eq!(e.reject_header(HeaderError::Mismatch), Err(AlreadyHalted));
    assert_eq!(e, halted_state);
    halted_state.apply(parse_line(b"")).unwrap_err();
}

fn before_next(engine: &Engine) -> u64 {
    match engine.cursor() {
        Cursor::Next(seq) => seq,
        Cursor::Exhausted => 0,
    }
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[test]
fn sequence_violations_halt_before_field_errors() {
    let one = [place(1, "a1", "o1", "buy", "100", 1, "GTC")];
    let bad = |seq: &str| format!("{seq},place,b1,o1,buy,1.001,1,GTC");
    halts_with(&one, &bad("3"), "2,halted,Gap,3");
    halts_with(&one, &bad("1"), "2,halted,Stale,1");
    halts_with(&one, &bad("0"), "2,halted,Stale,0");
    let three = [
        place(1, "a1", "o1", "buy", "100", 1, "GTC"),
        place(2, "a2", "o1", "buy", "100", 1, "GTC"),
        place(3, "a3", "o1", "buy", "100", 1, "GTC"),
    ];
    halts_with(&three, &bad("2"), "4,halted,Stale,2");
    halts_with(&one, &bad("x"), "2,halted,Seq(Syntax),none");
    halts_with(&one, "", "2,halted,Seq(Blank),none");
    halts_with(
        &one,
        &bad("18446744073709551616"),
        "2,halted,Seq(Overflow),none",
    );
    halts_with(
        &[],
        &bad("18446744073709551615"),
        "1,halted,Gap,18446744073709551615",
    );
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[test]
fn valid_sequence_is_consumed_by_rejected_rows() {
    let mut e = Engine::new();
    assert_eq!(
        feed(&mut e, "001,place,a1,o1,buy,1.001,1,GTC"),
        ["1,rejected,Input,,Input(Invalid(Price, OffTick))"]
    );
    assert_eq!(
        feed(&mut e, "2,nope,,,,,,"),
        ["2,rejected,Input,,Input(UnknownCommand)"]
    );
    assert_eq!(
        feed(&mut e, "3,\"cancel"),
        ["3,rejected,Input,,Input(MalformedCsv)"]
    );
    assert_eq!(
        feed(&mut e, &cancel(4, "a1")),
        ["4,rejected,Cancel,a1,UnknownId"]
    );
    assert_eq!(e.cursor(), Cursor::Next(5));
    assert_eq!(e.priority_clock(), 0);
    assert!(ids(&e).is_empty());
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[test]
fn header_failure_halts_at_sequence_one() {
    let mut e = Engine::new();
    let events = e.reject_header(HeaderError::Mismatch).unwrap();
    assert_eq!(
        events.iter().map(show).collect::<Vec<_>>(),
        ["1,halted,Header(Mismatch),none"]
    );
    assert_eq!(e.halt(), Some(HaltReason::Header(HeaderError::Mismatch)));
    let line = place(1, "a1", "o1", "buy", "1", 1, "GTC");
    assert_eq!(e.apply(parse_line(line.as_bytes())), Err(AlreadyHalted));
    assert!(ids(&e).is_empty());

    let (e, events) = replay(b"");
    assert_eq!(
        events.iter().map(show).collect::<Vec<_>>(),
        ["1,halted,Header(Empty),none"]
    );
    assert_eq!(e.halt(), Some(HaltReason::Header(HeaderError::Empty)));
    let (_, events) = replay(b"seq,cmd,id,owner,side,price,size,tif \n1,cancel,a,,,,,\n");
    assert_eq!(
        events.iter().map(show).collect::<Vec<_>>(),
        ["1,halted,Header(Mismatch),none"]
    );
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[test]
fn replay_stops_at_the_first_halt() {
    let input = b"seq,cmd,id,owner,side,price,size,tif\r\n\
        1,place,a1,o1,buy,100,1,GTC\r\n\
        3,place,a2,o1,buy,100,1,GTC\r\n\
        2,place,a3,o1,buy,100,1,GTC\r\n";
    let (e, events) = replay(input);
    assert_eq!(
        events.iter().map(show).collect::<Vec<_>>(),
        [
            "1,accepted,a1,o1,buy,10000,1,GTC",
            "1,rested,a1,buy,10000,1,1",
            "2,halted,Gap,3"
        ]
    );
    assert_eq!(ids(&e), ["a1"]);
    assert_eq!(e.halt(), Some(HaltReason::Gap));
    assert_eq!(e.cursor(), Cursor::Next(2));
}

// exkg-verifies: fea-ky6m7m#ac-matching
#[test]
fn brief_sample_matches_hand_analysis() {
    let lines = [
        "1,place,a1,Alice,buy,100.25,5,GTC",
        "2,place,b1,Bob,sell,100.50,3,GTC",
        "3,place,b2,Bob,sell,100.25,2,IOC",
        "4,place,b3,Carol,sell,100.25,1,POST_ONLY",
        "5,modify,b1,,,100.40,2,",
        "6,place,a2,Alice,sell,100.25,1,GTC",
        "7,cancel,a1,,,,,",
    ];
    let expected: [&[&str]; 7] = [
        &[
            "1,accepted,a1,Alice,buy,10025,5,GTC",
            "1,rested,a1,buy,10025,5,1",
        ],
        &[
            "2,accepted,b1,Bob,sell,10050,3,GTC",
            "2,rested,b1,sell,10050,3,2",
        ],
        &[
            "3,accepted,b2,Bob,sell,10025,2,IOC",
            "3,trade,b2,a1,10025,2,0,3",
        ],
        &["4,rejected,Place,b3,PostOnlyWouldCross"],
        &["5,modified,b1,10040,2,lost,3", "5,rested,b1,sell,10040,2,3"],
        &[
            "6,accepted,a2,Alice,sell,10025,1,GTC",
            "6,cancelled,a1,3,self_trade",
            "6,rested,a2,sell,10025,1,4",
        ],
        &["7,rejected,Cancel,a1,ClosedId"],
    ];
    let mut e = Engine::new();
    for (line, want) in lines.iter().zip(expected) {
        assert_eq!(feed(&mut e, line), want, "{line}");
    }
    assert!(bids(&e).is_empty());
    assert_eq!(asks(&e), ["a2:Alice:10025:1:GTC:4", "b1:Bob:10040:2:GTC:3"]);
    assert_eq!(ids(&e), ["a1", "a2", "b1", "b2", "b3"]);
    assert_eq!(e.cursor(), Cursor::Next(8));
    assert_eq!(e.priority_clock(), 4);
    assert_eq!(e.halt(), None);

    let input = format!("seq,cmd,id,owner,side,price,size,tif\n{}", lines.join("\n"));
    let (replayed, events) = replay(input.as_bytes());
    assert_eq!(replayed, e);
    assert_eq!(
        events.len(),
        expected.iter().map(|step| step.len()).sum::<usize>()
    );
}
