//! Model-based property tests: the production engine against the reference
//! model in `model/reference.rs`, plus invariants the test computes itself.
//!
//! Generated histories are abstract actions. Each is rendered to a CSV line
//! at run time, against the model's current state, so cancel and modify
//! targets can favour live orders, and placements use fresh IDs derived from
//! the action's position. Both engines get the same `parse_line` result.
//!
//! # Quantity conservation
//!
//! Let `L` be the sum of live remaining quantities. Per line:
//!
//! - an accepted place of size `S` brings `S` in; each fill of `q` takes `q`
//!   from the taker and `q` from the maker; a self-trade cancel takes the
//!   maker's remainder; whatever is left of `S` either rests or is cancelled
//!   as an IOC remainder;
//! - a user cancel takes the order's remainder;
//! - an applied modify (kept or lost) replaces the old remainder `r` with the
//!   new size `n`; a lost modify then matches like a place.
//!
//! So at every step, with all totals widened to `i128`:
//!
//! `L = Σ accepted sizes + Σ (n - r) over modified events
//!      - 2 · Σ trade quantities - Σ cancelled remainders (any reason)`.

// A crate root resolves `mod` paths from its own directory, hence `#[path]`.
#[path = "model/reference.rs"]
mod reference;

use std::collections::{BTreeMap, BTreeSet};

use orderbook::canonical::write_event;
use orderbook::domain::{
    MAX_IDENT_LEN, MAX_PRICE_CENTS, MAX_QUANTITY, MIN_PRICE_CENTS, MIN_QUANTITY, OrderId, Owner,
    Price, Side, TimeInForce,
};
use orderbook::engine::Engine;
use orderbook::event::{
    CancelReason, CommandKind, Cursor, Event, HaltReason, Priority, RejectCode,
};
use orderbook::input::{SeqError, parse_line};
use proptest::prelude::*;
use proptest::sample::{Index, select};
use proptest::strategy::ValueTree;
use proptest::test_runner::{FileFailurePersistence, TestCaseError, TestRunner};

use reference::{Model, OrderView, State, engine_state};

// ---------------------------------------------------------------------------
// Abstract actions and their generators
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum IdStyle {
    /// `f<pos>`.
    Plain,
    /// `f<pos>` padded to the 32-byte maximum.
    Long,
    /// `f<pos>` plus bytes that need CSV quoting and canonical escaping.
    Escaped,
}

#[derive(Debug, Clone, Copy)]
enum PlaceId {
    Fresh(IdStyle),
    /// A deliberate reuse of a reserved ID (fresh if none is reserved yet).
    Duplicate(Index),
}

#[derive(Debug, Clone, Copy)]
enum Target {
    /// A live order, or a reserved ID if none is live.
    Live(Index),
    /// Any reserved ID, live or closed.
    Reserved(Index),
    /// An ID no place ever uses.
    Unknown,
}

#[derive(Debug, Clone, Copy)]
enum ModPrice {
    Same,
    Step(i32),
    New(u32),
}

#[derive(Debug, Clone, Copy)]
enum ModSize {
    Same,
    /// A size from 1 to the current remainder.
    Shrink(Index),
    Grow(u32),
    New(u32),
}

/// A row with a valid sequence number that the parser rejects.
#[derive(Debug, Clone, Copy)]
enum BadRow {
    OffTick,
    UnknownCommand,
    MissingField,
    Unexpected,
    ZeroSize,
    ZeroPrice,
    OverPrice,
    OverSize,
    BadSide,
    BadTif,
    LongId,
    MalformedQuote,
}

const BAD_ROWS: &[BadRow] = &[
    BadRow::OffTick,
    BadRow::UnknownCommand,
    BadRow::MissingField,
    BadRow::Unexpected,
    BadRow::ZeroSize,
    BadRow::ZeroPrice,
    BadRow::OverPrice,
    BadRow::OverSize,
    BadRow::BadSide,
    BadRow::BadTif,
    BadRow::LongId,
    BadRow::MalformedQuote,
];

#[derive(Debug, Clone, Copy)]
enum Action {
    Place {
        id: PlaceId,
        owner: u8,
        side: Side,
        price: u32,
        size: u32,
        tif: TimeInForce,
    },
    Cancel(Target),
    Modify {
        target: Target,
        price: ModPrice,
        size: ModSize,
    },
    BadRow(BadRow),
}

/// Mostly a narrow band of crossing levels around 100.00, sometimes an endpoint.
fn price() -> impl Strategy<Value = u32> {
    prop_oneof![
        24 => 9_997u32..=10_003,
        1 => Just(MIN_PRICE_CENTS),
        1 => Just(MAX_PRICE_CENTS),
        1 => MIN_PRICE_CENTS..=MAX_PRICE_CENTS,
    ]
}

/// Mostly small partial-fill sizes, sometimes an endpoint.
fn size() -> impl Strategy<Value = u32> {
    prop_oneof![
        20 => 1u32..=20,
        2 => Just(MIN_QUANTITY),
        1 => Just(MAX_QUANTITY),
        1 => MIN_QUANTITY..=MAX_QUANTITY,
    ]
}

fn target() -> impl Strategy<Value = Target> {
    prop_oneof![
        8 => any::<Index>().prop_map(Target::Live),
        2 => any::<Index>().prop_map(Target::Reserved),
        1 => Just(Target::Unknown),
    ]
}

fn action() -> impl Strategy<Value = Action> {
    let id = prop_oneof![
        10 => Just(PlaceId::Fresh(IdStyle::Plain)),
        1 => Just(PlaceId::Fresh(IdStyle::Long)),
        1 => Just(PlaceId::Fresh(IdStyle::Escaped)),
        2 => any::<Index>().prop_map(PlaceId::Duplicate),
    ];
    let side = prop_oneof![Just(Side::Buy), Just(Side::Sell)];
    let tif = prop_oneof![
        5 => Just(TimeInForce::Gtc),
        2 => Just(TimeInForce::Ioc),
        2 => Just(TimeInForce::PostOnly),
    ];
    let place =
        (id, 0u8..4, side, price(), size(), tif).prop_map(|(id, owner, side, price, size, tif)| {
            Action::Place {
                id,
                owner,
                side,
                price,
                size,
                tif,
            }
        });
    let mod_price = prop_oneof![
        3 => Just(ModPrice::Same),
        2 => (-3i32..=3).prop_map(ModPrice::Step),
        1 => price().prop_map(ModPrice::New),
    ];
    let mod_size = prop_oneof![
        1 => Just(ModSize::Same),
        3 => any::<Index>().prop_map(ModSize::Shrink),
        1 => (1u32..=10).prop_map(ModSize::Grow),
        1 => size().prop_map(ModSize::New),
    ];
    let modify = (target(), mod_price, mod_size).prop_map(|(target, price, size)| Action::Modify {
        target,
        price,
        size,
    });
    prop_oneof![
        55 => place,
        12 => target().prop_map(Action::Cancel),
        25 => modify,
        5 => select(BAD_ROWS).prop_map(Action::BadRow),
    ]
}

fn history() -> impl Strategy<Value = Vec<Action>> {
    proptest::collection::vec(action(), 1..=60)
}

// ---------------------------------------------------------------------------
// Rendering actions to CSV lines
// ---------------------------------------------------------------------------

/// Quotes a CSV field when it holds a comma or a quote.
fn field(text: &str) -> String {
    if text.contains([',', '"']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

/// Two-decimal price text; `pad` adds a harmless trailing zero digit.
fn price_text(cents: u32, pad: bool) -> String {
    let text = format!("{}.{:02}", cents / 100, cents % 100);
    if pad { text + "0" } else { text }
}

fn side_text(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn tif_text(tif: TimeInForce) -> &'static str {
    match tif {
        TimeInForce::Gtc => "GTC",
        TimeInForce::Ioc => "IOC",
        TimeInForce::PostOnly => "POST_ONLY",
    }
}

fn fresh_id(style: IdStyle, pos: usize) -> String {
    let plain = format!("f{pos}");
    match style {
        IdStyle::Plain => plain,
        IdStyle::Long => format!("{plain:x<width$}", width = MAX_IDENT_LEN),
        IdStyle::Escaped => format!("{plain} ,\"\\\t"),
    }
}

fn resolve(target: Target, pos: usize, model: &Model) -> String {
    let pick = |ids: Vec<OrderId>, index: Index| index.get(&ids).as_str().to_owned();
    let live = model.live_ids();
    let reserved = model.reserved_ids();
    match target {
        Target::Live(i) if !live.is_empty() => pick(live, i),
        Target::Live(i) | Target::Reserved(i) if !reserved.is_empty() => pick(reserved, i),
        _ => format!("u{pos}"),
    }
}

/// Renders an action as the CSV line with sequence number `seq`.
fn render(action: &Action, pos: usize, seq: u64, model: &Model) -> String {
    let pad = pos % 7 == 3;
    match *action {
        Action::Place {
            id,
            owner,
            side,
            price,
            size,
            tif,
        } => {
            let id = match id {
                PlaceId::Fresh(style) => fresh_id(style, pos),
                PlaceId::Duplicate(i) => {
                    let reserved = model.reserved_ids();
                    if reserved.is_empty() {
                        fresh_id(IdStyle::Plain, pos)
                    } else {
                        i.get(&reserved).as_str().to_owned()
                    }
                }
            };
            format!(
                "{seq},place,{},w{owner},{},{},{size},{}",
                field(&id),
                side_text(side),
                price_text(price, pad),
                tif_text(tif)
            )
        }
        Action::Cancel(target) => {
            format!("{seq},cancel,{},,,,,", field(&resolve(target, pos, model)))
        }
        Action::Modify {
            target,
            price,
            size,
        } => {
            let id = resolve(target, pos, model);
            let live = OrderId::new(id.as_bytes())
                .ok()
                .and_then(|id| model.order(&id).cloned());
            let (cents, remaining): (u32, u32) = live
                .as_ref()
                .map_or((10_000, 5), |o| (o.price.cents(), o.remaining));
            let cents = match price {
                ModPrice::Same => cents,
                ModPrice::Step(step) => cents
                    .saturating_add_signed(step)
                    .clamp(MIN_PRICE_CENTS, MAX_PRICE_CENTS),
                ModPrice::New(cents) => cents,
            };
            let size = match size {
                ModSize::Same => remaining,
                ModSize::Shrink(i) => 1 + i.index(remaining as usize) as u32,
                ModSize::Grow(more) => remaining.saturating_add(more).min(MAX_QUANTITY),
                ModSize::New(size) => size,
            };
            format!(
                "{seq},modify,{},,,{},{size},",
                field(&id),
                price_text(cents, pad)
            )
        }
        Action::BadRow(row) => {
            let id = format!("f{pos}");
            match row {
                BadRow::OffTick => format!("{seq},place,{id},w0,buy,1.001,1,GTC"),
                BadRow::UnknownCommand => format!("{seq},amend,{id},,,,,"),
                BadRow::MissingField => format!("{seq},place,{id},w0,buy,1.00,1"),
                BadRow::Unexpected => format!("{seq},cancel,{id},w0,,,,"),
                BadRow::ZeroSize => format!("{seq},place,{id},w0,sell,1.00,0,GTC"),
                BadRow::ZeroPrice => format!("{seq},place,{id},w0,sell,0.00,1,GTC"),
                BadRow::OverPrice => format!("{seq},place,{id},w0,buy,1000000.01,1,GTC"),
                BadRow::OverSize => format!("{seq},place,{id},w0,buy,1.00,1000000001,GTC"),
                BadRow::BadSide => format!("{seq},place,{id},w0,BUY,1.00,1,GTC"),
                BadRow::BadTif => format!("{seq},place,{id},w0,buy,1.00,1,gtc"),
                BadRow::LongId => format!("{seq},cancel,{id:y<33},,,,,"),
                BadRow::MalformedQuote => format!("{seq},modify,\"{id}\"x,,,1.00,1,"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Running both engines and checking them
// ---------------------------------------------------------------------------

/// How often each outcome occurred in production events.
#[derive(Debug, Default, Clone, Copy)]
struct Stats {
    lines: u64,
    accepted: u64,
    trades: u64,
    rested: u64,
    self_trade: u64,
    ioc_remainder: u64,
    user_cancel: u64,
    post_only_place: u64,
    post_only_modify: u64,
    kept: u64,
    lost: u64,
    duplicate: u64,
    unknown: u64,
    closed: u64,
    input: u64,
    endpoint_price_trades: u64,
    endpoint_size_accepts: u64,
}

impl Stats {
    fn add(&mut self, other: &Stats) {
        self.lines += other.lines;
        self.accepted += other.accepted;
        self.trades += other.trades;
        self.rested += other.rested;
        self.self_trade += other.self_trade;
        self.ioc_remainder += other.ioc_remainder;
        self.user_cancel += other.user_cancel;
        self.post_only_place += other.post_only_place;
        self.post_only_modify += other.post_only_modify;
        self.kept += other.kept;
        self.lost += other.lost;
        self.duplicate += other.duplicate;
        self.unknown += other.unknown;
        self.closed += other.closed;
        self.input += other.input;
        self.endpoint_price_trades += other.endpoint_price_trades;
        self.endpoint_size_accepts += other.endpoint_size_accepts;
    }
}

/// The order taking liquidity during one line, as the test tracks it.
struct Taker {
    id: OrderId,
    owner: Owner,
    side: Side,
    limit: Price,
    left: u32,
}

struct Harness {
    engine: Engine,
    model: Model,
    /// Owner and side of every accepted order, from `Accepted` events.
    accepted: BTreeMap<OrderId, (Owner, Side)>,
    /// The expected sum of live remaining quantities (see the module docs).
    ledger: i128,
    production: Vec<Event>,
    reference: Vec<Event>,
    stats: Stats,
}

fn fail(message: String) -> TestCaseError {
    TestCaseError::fail(message)
}

fn encode(events: &[Event]) -> String {
    let mut out = Vec::new();
    for event in events {
        write_event(&mut out, event).expect("writing to a Vec cannot fail");
    }
    String::from_utf8(out).expect("canonical events are ASCII")
}

impl Harness {
    fn new() -> Self {
        Self {
            engine: Engine::new(),
            model: Model::default(),
            accepted: BTreeMap::new(),
            ledger: 0,
            production: Vec::new(),
            reference: Vec::new(),
            stats: Stats::default(),
        }
    }

    /// Feeds one line to both engines and compares their events and complete
    /// state. Returns the production state before the line, and its events or
    /// `None` when both engines were already halted.
    fn feed(&mut self, line: &str) -> Result<(State, Option<Vec<Event>>), TestCaseError> {
        let parsed = parse_line(line.as_bytes());
        let before = engine_state(&self.engine);
        let got = self.engine.apply(parsed.clone());
        let want = self.model.apply(parsed);
        let after = engine_state(&self.engine);
        prop_assert_eq!(&after, &self.model.state(), "state after {:?}", line);
        match (got, want) {
            (Ok(got), Ok(want)) => {
                prop_assert_eq!(&got, &want, "events for {:?}", line);
                self.production.extend(got.iter().cloned());
                self.reference.extend(want);
                Ok((before, Some(got)))
            }
            (Err(_), Err(_)) => {
                prop_assert_eq!(&after, &before, "a halted engine changed on {:?}", line);
                Ok((before, None))
            }
            (got, want) => Err(fail(format!(
                "halt status differs on {line:?}: production {got:?}, reference {want:?}"
            ))),
        }
    }

    /// Feeds a line that must be applied, then checks the step's invariants.
    fn step(&mut self, line: &str) -> Result<(), TestCaseError> {
        let (before, events) = self.feed(line)?;
        let events = events.ok_or_else(|| fail(format!("engine halted before {line:?}")))?;
        self.check_events(line, &before, &events)?;
        self.check_book(line, &before)
    }

    /// Checks each event against the state before the line and updates the
    /// ledger and the counters.
    fn check_events(
        &mut self,
        line: &str,
        before: &State,
        events: &[Event],
    ) -> Result<(), TestCaseError> {
        let pre: BTreeMap<&OrderId, &OrderView> = before
            .bids
            .iter()
            .chain(&before.asks)
            .map(|o| (&o.id, o))
            .collect();
        let mut maker_left: BTreeMap<&OrderId, u32> =
            pre.iter().map(|(id, o)| (*id, o.remaining)).collect();
        let mut taker: Option<Taker> = None;
        self.stats.lines += 1;
        for event in events {
            match event {
                Event::Accepted {
                    id,
                    owner,
                    side,
                    price,
                    size,
                    ..
                } => {
                    let fresh = self
                        .accepted
                        .insert(id.clone(), (owner.clone(), *side))
                        .is_none();
                    prop_assert!(fresh, "accepted a reused ID on {:?}", line);
                    self.ledger += i128::from(size.lots());
                    self.stats.accepted += 1;
                    if size.lots() == MAX_QUANTITY {
                        self.stats.endpoint_size_accepts += 1;
                    }
                    taker = Some(Taker {
                        id: id.clone(),
                        owner: owner.clone(),
                        side: *side,
                        limit: *price,
                        left: size.lots(),
                    });
                }
                Event::Modified {
                    id,
                    price,
                    remaining,
                    priority,
                    ..
                } => {
                    let old = pre
                        .get(id)
                        .ok_or_else(|| fail(format!("modified a non-live order on {line:?}")))?;
                    self.ledger += i128::from(remaining.lots()) - i128::from(old.remaining);
                    match priority {
                        Priority::Kept => self.stats.kept += 1,
                        Priority::Lost => {
                            self.stats.lost += 1;
                            let (owner, side) = self.accepted[id].clone();
                            taker = Some(Taker {
                                id: id.clone(),
                                owner,
                                side,
                                limit: *price,
                                left: remaining.lots(),
                            });
                        }
                    }
                }
                Event::Trade {
                    taker: taker_id,
                    maker,
                    price,
                    quantity,
                    taker_left,
                    maker_left: left_after,
                    ..
                } => {
                    self.stats.trades += 1;
                    if [MIN_PRICE_CENTS, MAX_PRICE_CENTS].contains(&price.cents()) {
                        self.stats.endpoint_price_trades += 1;
                    }
                    let t = taker
                        .as_mut()
                        .ok_or_else(|| fail(format!("trade without a taker on {line:?}")))?;
                    prop_assert_eq!(taker_id, &t.id);
                    let resting = pre.get(maker).ok_or_else(|| {
                        fail(format!("maker was not live before the line {line:?}"))
                    })?;
                    let (maker_owner, maker_side) = &self.accepted[maker];
                    prop_assert_eq!(maker_owner, &resting.owner);
                    prop_assert_eq!(*maker_side, resting.side);
                    prop_assert_ne!(maker_owner, &t.owner, "self-fill on {:?}", line);
                    prop_assert_ne!(resting.side, t.side, "same-side fill on {:?}", line);
                    prop_assert_eq!(*price, resting.price, "not the maker's price on {:?}", line);
                    let within = match t.side {
                        Side::Buy => *price <= t.limit,
                        Side::Sell => *price >= t.limit,
                    };
                    prop_assert!(within, "fill beyond the taker's limit on {:?}", line);
                    let had = maker_left[maker];
                    let q = quantity.lots();
                    prop_assert_eq!(q, t.left.min(had), "fill is not the min on {:?}", line);
                    t.left -= q;
                    prop_assert_eq!(*taker_left, t.left);
                    prop_assert_eq!(*left_after, had - q);
                    maker_left.insert(maker, had - q);
                    self.ledger -= 2 * i128::from(q);
                }
                Event::Cancelled {
                    remaining, reason, ..
                } => {
                    self.ledger -= i128::from(remaining.lots());
                    match reason {
                        CancelReason::User => self.stats.user_cancel += 1,
                        CancelReason::IocRemainder => self.stats.ioc_remainder += 1,
                        CancelReason::SelfTrade => self.stats.self_trade += 1,
                    }
                }
                Event::Rested { .. } => self.stats.rested += 1,
                Event::Rejected {
                    command, reason, ..
                } => match (command, reason) {
                    (_, RejectCode::Input(_)) => self.stats.input += 1,
                    (_, RejectCode::DuplicateId) => self.stats.duplicate += 1,
                    (_, RejectCode::UnknownId) => self.stats.unknown += 1,
                    (_, RejectCode::ClosedId) => self.stats.closed += 1,
                    (CommandKind::Modify, RejectCode::PostOnlyWouldCross) => {
                        self.stats.post_only_modify += 1
                    }
                    (_, RejectCode::PostOnlyWouldCross) => self.stats.post_only_place += 1,
                },
                Event::Halted { .. } => {
                    return Err(fail(format!("unexpected halt on {line:?}")));
                }
            }
        }
        Ok(())
    }

    /// Checks the production book after an applied line, from its public views.
    fn check_book(&self, line: &str, before: &State) -> Result<(), TestCaseError> {
        let engine = &self.engine;
        let after = engine_state(engine);
        let Cursor::Next(previous) = before.cursor else {
            return Err(fail("the tests never exhaust the cursor".to_owned()));
        };
        prop_assert_eq!(after.cursor, Cursor::Next(previous + 1));
        prop_assert_eq!(after.halt, None);
        prop_assert!(
            after.clock == before.clock || after.clock == before.clock + 1,
            "the clock moved by more than one on {:?}",
            line
        );
        let reserved: BTreeSet<&OrderId> = after.reserved.iter().collect();
        prop_assert!(
            before.reserved.iter().all(|id| reserved.contains(id)),
            "reserved IDs shrank on {:?}",
            line
        );
        let mut ids = BTreeSet::new();
        let mut ranks = BTreeSet::new();
        let mut live_total: u128 = 0;
        let records = engine.bids().map(|o| (o, Side::Buy));
        for (order, side) in records.chain(engine.asks().map(|o| (o, Side::Sell))) {
            prop_assert_eq!(order.side, side);
            prop_assert!(order.remaining.lots() >= 1);
            prop_assert!(order.tif != TimeInForce::Ioc, "IOC rests on {:?}", line);
            prop_assert!(ids.insert(&order.id), "duplicate live ID on {:?}", line);
            prop_assert!(ranks.insert(order.rank), "duplicate rank on {:?}", line);
            prop_assert!((1..=after.clock).contains(&order.rank));
            prop_assert!(reserved.contains(&order.id), "live ID not reserved");
            prop_assert_eq!(engine.order(&order.id), Some(order));
            let (owner, accepted_side) = &self.accepted[&order.id];
            prop_assert_eq!(owner, &order.owner);
            prop_assert_eq!(*accepted_side, order.side);
            live_total += u128::from(order.remaining.lots());
        }
        prop_assert_eq!(engine.live_count(), after.bids.len() + after.asks.len());
        let best_bid = engine.bids().map(|o| o.price).max();
        let best_ask = engine.asks().map(|o| o.price).min();
        if let (Some(bid), Some(ask)) = (best_bid, best_ask) {
            prop_assert!(bid < ask, "crossed book after {:?}", line);
        }
        let live_total = i128::try_from(live_total).expect("fits in i128");
        prop_assert_eq!(
            live_total,
            self.ledger,
            "quantity not conserved after {:?}",
            line
        );
        Ok(())
    }

    /// Compares the two full event streams as canonical bytes.
    fn check_encoding(&self) -> Result<(), TestCaseError> {
        prop_assert_eq!(encode(&self.production), encode(&self.reference));
        Ok(())
    }

    /// Renders and applies each action of a sequence-valid history.
    fn run(&mut self, actions: &[Action], first_pos: usize) -> Result<(), TestCaseError> {
        for (offset, action) in actions.iter().enumerate() {
            let line = render(
                action,
                first_pos + offset,
                self.model.next_seq(),
                &self.model,
            );
            self.step(&line)?;
        }
        Ok(())
    }
}

fn run_history(actions: &[Action]) -> Result<Stats, TestCaseError> {
    let mut harness = Harness::new();
    harness.run(actions, 0)?;
    harness.check_encoding()?;
    Ok(harness.stats)
}

// ---------------------------------------------------------------------------
// Sequence failures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum BadSeq {
    /// The expected number plus this much (saturating).
    Gap(u64),
    /// The previous number again (zero when nothing came before).
    Duplicate,
    /// Some number below the expected one.
    Stale(Index),
    Zero,
    /// A line whose sequence number cannot be recovered.
    Lost(SeqError),
}

const LOST: &[SeqError] = &[
    SeqError::Blank,
    SeqError::Malformed,
    SeqError::Empty,
    SeqError::Syntax,
    SeqError::Overflow,
];

fn bad_seq() -> impl Strategy<Value = BadSeq> {
    prop_oneof![
        2 => prop_oneof![1u64..=5, Just(u64::MAX)].prop_map(BadSeq::Gap),
        2 => Just(BadSeq::Duplicate),
        1 => any::<Index>().prop_map(BadSeq::Stale),
        1 => Just(BadSeq::Zero),
        2 => select(LOST).prop_map(BadSeq::Lost),
    ]
}

/// How a line after the halt is numbered.
#[derive(Debug, Clone, Copy)]
enum Later {
    /// The number that was expected when the engine halted.
    Expected,
    /// The number after that.
    Following,
    Blank,
}

fn later() -> impl Strategy<Value = Vec<(Action, Later)>> {
    let numbering = prop_oneof![
        Just(Later::Expected),
        Just(Later::Following),
        Just(Later::Blank)
    ];
    proptest::collection::vec((action(), numbering), 1..=4)
}

fn run_sequence_failure(
    prefix: &[Action],
    bad: BadSeq,
    after: &[(Action, Later)],
) -> Result<(), TestCaseError> {
    let mut harness = Harness::new();
    harness.run(prefix, 0)?;
    let expected = harness.model.next_seq();
    let body = "place,late,w0,buy,100.00,1,GTC";
    let numbered = |seq: u64| (format!("{seq},{body}"), Some(seq));
    let ((line, observed), reason) = match bad {
        BadSeq::Gap(k) => (numbered(expected.saturating_add(k)), HaltReason::Gap),
        BadSeq::Duplicate => (numbered(expected - 1), HaltReason::Stale),
        BadSeq::Stale(i) => (
            numbered(i.index(expected as usize) as u64),
            HaltReason::Stale,
        ),
        BadSeq::Zero => (numbered(0), HaltReason::Stale),
        BadSeq::Lost(error) => {
            let line = match error {
                SeqError::Blank => String::new(),
                SeqError::Malformed => format!("\"{expected},{body}"),
                SeqError::Empty => format!(",{body}"),
                SeqError::Syntax => format!("x{expected},{body}"),
                SeqError::Overflow => format!("18446744073709551616,{body}"),
            };
            ((line, None), HaltReason::Seq(error))
        }
    };
    let (before, events) = harness.feed(&line)?;
    prop_assert_eq!(
        events,
        Some(vec![Event::Halted {
            expected: Cursor::Next(expected),
            reason,
            observed,
        }])
    );
    let halted = engine_state(&harness.engine);
    prop_assert_eq!(
        &halted,
        &State {
            halt: Some(reason),
            ..before
        }
    );
    for (offset, (action, numbering)) in after.iter().enumerate() {
        let line = match numbering {
            Later::Expected => render(action, 1000 + offset, expected, &harness.model),
            Later::Following => render(action, 1000 + offset, expected + 1, &harness.model),
            Later::Blank => String::new(),
        };
        let (_, events) = harness.feed(&line)?;
        prop_assert_eq!(events, None, "a halted engine applied {:?}", line);
        prop_assert_eq!(&engine_state(&harness.engine), &halted);
    }
    harness.check_encoding()
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    // Regressions go to `tests/model.proptest-regressions`, beside this file.
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::WithSource(
            "proptest-regressions",
        ))),
        ..ProptestConfig::with_cases(256)
    })]

    // exkg-verifies: fea-ky6m7m#ac-model
    #[test]
    fn production_agrees_with_reference_model(history in history()) {
        run_history(&history)?;
    }

    // exkg-verifies: fea-ky6m7m#ac-model
    #[test]
    fn bad_sequence_halts_both_engines_for_good(
        prefix in proptest::collection::vec(action(), 0..=30),
        bad in bad_seq(),
        after in later(),
    ) {
        run_sequence_failure(&prefix, bad, &after)?;
    }
}

// exkg-verifies: fea-ky6m7m#ac-model
#[test]
fn generated_histories_cover_every_outcome() {
    let mut runner = TestRunner::deterministic();
    let strategy = history();
    let mut total = Stats::default();
    for _ in 0..256 {
        let history = strategy
            .new_tree(&mut runner)
            .expect("the history strategy never rejects")
            .current();
        total.add(&run_history(&history).expect("the deterministic histories pass"));
    }
    eprintln!("{total:#?}");
    let counts = [
        ("trades", total.trades),
        ("rested", total.rested),
        ("self_trade", total.self_trade),
        ("ioc_remainder", total.ioc_remainder),
        ("user_cancel", total.user_cancel),
        ("post_only_place", total.post_only_place),
        ("post_only_modify", total.post_only_modify),
        ("kept", total.kept),
        ("lost", total.lost),
        ("duplicate", total.duplicate),
        ("unknown", total.unknown),
        ("closed", total.closed),
        ("input", total.input),
        ("endpoint_price_trades", total.endpoint_price_trades),
        ("endpoint_size_accepts", total.endpoint_size_accepts),
    ];
    for (name, count) in counts {
        assert!(
            count > 0,
            "no {name} in the generated histories: {total:#?}"
        );
    }
    // Rejections must not crowd out matching.
    assert!(
        total.trades * 10 > total.lines,
        "too few trades: {total:#?}"
    );
}
