//! A deliberately simple reference engine for the model-based tests.
//!
//! Sharing boundary: the model shares only the vocabulary with production.
//! It uses the domain types, the parser's `ParsedLine` (both engines get the
//! very same parsed line) and the typed `Event` enums. It never calls
//! `orderbook::engine::Engine` to decide an outcome and reuses none of its
//! matching, index, rank or sequence code: live orders sit in one unsorted
//! `Vec`, and the best maker is found by a linear scan. `Engine` appears here
//! only in [`engine_state`], which reads its public views for comparison.
//!
//! The tests encode both event lists with the shared `canonical::write_event`,
//! so equal bytes show the event streams agree; they do not independently
//! validate the encoder. The hand-derived goldens in `tests/canonical.rs` do.

use std::collections::BTreeSet;

use orderbook::domain::{Command, OrderId, Owner, Place, Price, Quantity, Side, TimeInForce};
use orderbook::engine::{Engine, RestingOrder};
use orderbook::event::{
    CancelReason, CommandKind, Cursor, Event, HaltReason, Priority, RejectCode,
};
use orderbook::input::{ParsedLine, SeqError};

/// One live order, as both engines are compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderView {
    pub id: OrderId,
    pub owner: Owner,
    pub side: Side,
    pub price: Price,
    pub remaining: u32,
    pub tif: TimeInForce,
    pub rank: u64,
}

/// The complete market state, normalized for comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub cursor: Cursor,
    pub halt: Option<HaltReason>,
    pub clock: u64,
    /// Highest price first, then lowest rank.
    pub bids: Vec<OrderView>,
    /// Lowest price first, then lowest rank.
    pub asks: Vec<OrderView>,
    /// Sorted by bytes.
    pub reserved: Vec<OrderId>,
}

/// Builds the normalized state of the production engine from its read-only views.
pub fn engine_state(engine: &Engine) -> State {
    let view = |order: &RestingOrder| OrderView {
        id: order.id.clone(),
        owner: order.owner.clone(),
        side: order.side,
        price: order.price,
        remaining: order.remaining.lots(),
        tif: order.tif,
        rank: order.rank,
    };
    State {
        cursor: engine.cursor(),
        halt: engine.halt(),
        clock: engine.priority_clock(),
        bids: engine.bids().map(view).collect(),
        asks: engine.asks().map(view).collect(),
        reserved: engine.reserved_ids().cloned().collect(),
    }
}

/// Returned when the halted model is given a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Halted;

/// The reference engine.
#[derive(Debug, Clone)]
pub struct Model {
    cursor: Cursor,
    halt: Option<HaltReason>,
    clock: u64,
    /// Live orders in no particular order.
    live: Vec<OrderView>,
    reserved: BTreeSet<OrderId>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            cursor: Cursor::Next(1),
            halt: None,
            clock: 0,
            live: Vec::new(),
            reserved: BTreeSet::new(),
        }
    }
}

/// The order currently taking liquidity.
struct Taker<'a> {
    seq: u64,
    id: &'a OrderId,
    owner: &'a Owner,
    side: Side,
    limit: Price,
}

impl Model {
    /// Applies one line by the written rules.
    pub fn apply(&mut self, line: Result<ParsedLine, SeqError>) -> Result<Vec<Event>, Halted> {
        if self.halt.is_some() {
            return Err(Halted);
        }
        let line = match line {
            Ok(line) => line,
            Err(error) => return Ok(self.stop(HaltReason::Seq(error), None)),
        };
        let Cursor::Next(expected) = self.cursor else {
            return Ok(self.stop(HaltReason::Exhausted, Some(line.seq)));
        };
        if line.seq > expected {
            return Ok(self.stop(HaltReason::Gap, Some(line.seq)));
        }
        if line.seq < expected {
            return Ok(self.stop(HaltReason::Stale, Some(line.seq)));
        }
        self.cursor = if expected == u64::MAX {
            Cursor::Exhausted
        } else {
            Cursor::Next(expected + 1)
        };
        let seq = line.seq;
        let mut events = Vec::new();
        match line.result {
            Err(reason) => events.push(Event::Rejected {
                seq,
                command: CommandKind::Input,
                id: None,
                reason: RejectCode::Input(reason),
            }),
            Ok(Command::Place(place)) => self.place(seq, place, &mut events),
            Ok(Command::Cancel(cancel)) => {
                let id = cancel.id;
                match self.position(&id) {
                    Some(at) => {
                        let order = self.live.remove(at);
                        events.push(cancelled(seq, id, order.remaining, CancelReason::User));
                    }
                    None => {
                        let reason = self.missing(&id);
                        events.push(rejected(seq, CommandKind::Cancel, id, reason));
                    }
                }
            }
            Ok(Command::Modify(modify)) => {
                self.modify(seq, modify.id, modify.price, modify.size, &mut events)
            }
        }
        Ok(events)
    }

    /// The normalized complete state.
    pub fn state(&self) -> State {
        let mut bids: Vec<OrderView> = self
            .live
            .iter()
            .filter(|o| o.side == Side::Buy)
            .cloned()
            .collect();
        bids.sort_by(|a, b| b.price.cmp(&a.price).then(a.rank.cmp(&b.rank)));
        let mut asks: Vec<OrderView> = self
            .live
            .iter()
            .filter(|o| o.side == Side::Sell)
            .cloned()
            .collect();
        asks.sort_by(|a, b| a.price.cmp(&b.price).then(a.rank.cmp(&b.rank)));
        State {
            cursor: self.cursor,
            halt: self.halt,
            clock: self.clock,
            bids,
            asks,
            reserved: self.reserved.iter().cloned().collect(),
        }
    }

    /// The live order IDs, in the model's arbitrary but deterministic order.
    pub fn live_ids(&self) -> Vec<OrderId> {
        self.live.iter().map(|o| o.id.clone()).collect()
    }

    /// Every reserved ID, sorted.
    pub fn reserved_ids(&self) -> Vec<OrderId> {
        self.reserved.iter().cloned().collect()
    }

    /// The live order with this ID.
    pub fn order(&self, id: &OrderId) -> Option<&OrderView> {
        self.live.iter().find(|o| o.id == *id)
    }

    /// The sequence number the model expects next. The tests never exhaust it.
    pub fn next_seq(&self) -> u64 {
        match self.cursor {
            Cursor::Next(seq) => seq,
            Cursor::Exhausted => panic!("the tests never exhaust the cursor"),
        }
    }

    fn stop(&mut self, reason: HaltReason, observed: Option<u64>) -> Vec<Event> {
        self.halt = Some(reason);
        vec![Event::Halted {
            expected: self.cursor,
            reason,
            observed,
        }]
    }

    fn place(&mut self, seq: u64, place: Place, events: &mut Vec<Event>) {
        if self.reserved.contains(&place.id) {
            events.push(rejected(
                seq,
                CommandKind::Place,
                place.id,
                RejectCode::DuplicateId,
            ));
            return;
        }
        self.reserved.insert(place.id.clone());
        if place.tif == TimeInForce::PostOnly && self.would_cross(place.side, place.price) {
            events.push(rejected(
                seq,
                CommandKind::Place,
                place.id,
                RejectCode::PostOnlyWouldCross,
            ));
            return;
        }
        events.push(Event::Accepted {
            seq,
            id: place.id.clone(),
            owner: place.owner.clone(),
            side: place.side,
            price: place.price,
            size: place.size,
            tif: place.tif,
        });
        let taker = Taker {
            seq,
            id: &place.id,
            owner: &place.owner,
            side: place.side,
            limit: place.price,
        };
        let left = self.take(&taker, place.size.lots(), events);
        if left == 0 {
            return;
        }
        if place.tif == TimeInForce::Ioc {
            events.push(cancelled(seq, place.id, left, CancelReason::IocRemainder));
            return;
        }
        self.clock += 1;
        events.push(Event::Rested {
            seq,
            id: place.id.clone(),
            side: place.side,
            price: place.price,
            remaining: lots(left),
            rank: self.clock,
        });
        self.live.push(OrderView {
            id: place.id,
            owner: place.owner,
            side: place.side,
            price: place.price,
            remaining: left,
            tif: place.tif,
            rank: self.clock,
        });
    }

    fn modify(
        &mut self,
        seq: u64,
        id: OrderId,
        price: Price,
        size: Quantity,
        events: &mut Vec<Event>,
    ) {
        let Some(at) = self.position(&id) else {
            let reason = self.missing(&id);
            events.push(rejected(seq, CommandKind::Modify, id, reason));
            return;
        };
        let old = self.live[at].clone();
        if price == old.price && size.lots() <= old.remaining {
            self.live[at].remaining = size.lots();
            events.push(Event::Modified {
                seq,
                id,
                price,
                remaining: size,
                priority: Priority::Kept,
                rank: old.rank,
            });
            return;
        }
        // The cross check runs with the original still on its own side, which
        // never affects the best opposite order.
        if old.tif == TimeInForce::PostOnly && self.would_cross(old.side, price) {
            events.push(rejected(
                seq,
                CommandKind::Modify,
                id,
                RejectCode::PostOnlyWouldCross,
            ));
            return;
        }
        self.live.remove(at);
        self.clock += 1;
        let rank = self.clock;
        events.push(Event::Modified {
            seq,
            id: id.clone(),
            price,
            remaining: size,
            priority: Priority::Lost,
            rank,
        });
        let taker = Taker {
            seq,
            id: &id,
            owner: &old.owner,
            side: old.side,
            limit: price,
        };
        let left = self.take(&taker, size.lots(), events);
        if left > 0 {
            events.push(Event::Rested {
                seq,
                id: id.clone(),
                side: old.side,
                price,
                remaining: lots(left),
                rank,
            });
            self.live.push(OrderView {
                id,
                price,
                remaining: left,
                rank,
                ..old
            });
        }
    }

    /// Matches a taker against the best crossing makers; returns what is left.
    fn take(&mut self, taker: &Taker<'_>, mut left: u32, events: &mut Vec<Event>) -> u32 {
        while left > 0 {
            let Some(at) = self.best_maker(taker.side) else {
                break;
            };
            if !crosses(taker.side, taker.limit, self.live[at].price) {
                break;
            }
            if self.live[at].owner == *taker.owner {
                let maker = self.live.remove(at);
                events.push(cancelled(
                    taker.seq,
                    maker.id,
                    maker.remaining,
                    CancelReason::SelfTrade,
                ));
                continue;
            }
            let maker = &mut self.live[at];
            let fill = left.min(maker.remaining);
            left -= fill;
            maker.remaining -= fill;
            events.push(Event::Trade {
                seq: taker.seq,
                taker: taker.id.clone(),
                maker: maker.id.clone(),
                price: maker.price,
                quantity: lots(fill),
                taker_left: left,
                maker_left: maker.remaining,
            });
            if maker.remaining == 0 {
                self.live.remove(at);
            }
        }
        left
    }

    /// The index of the best live order opposite a taker on `side`, by a
    /// linear scan: best price, then lowest rank.
    fn best_maker(&self, side: Side) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (at, order) in self.live.iter().enumerate() {
            if order.side == side {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) => {
                    let current = &self.live[b];
                    let better_price = match order.side {
                        Side::Sell => order.price < current.price,
                        Side::Buy => order.price > current.price,
                    };
                    better_price || (order.price == current.price && order.rank < current.rank)
                }
            };
            if better {
                best = Some(at);
            }
        }
        best
    }

    fn would_cross(&self, side: Side, limit: Price) -> bool {
        self.best_maker(side)
            .is_some_and(|at| crosses(side, limit, self.live[at].price))
    }

    fn position(&self, id: &OrderId) -> Option<usize> {
        self.live.iter().position(|o| o.id == *id)
    }

    fn missing(&self, id: &OrderId) -> RejectCode {
        if self.reserved.contains(id) {
            RejectCode::ClosedId
        } else {
            RejectCode::UnknownId
        }
    }
}

fn crosses(taker: Side, limit: Price, maker: Price) -> bool {
    match taker {
        Side::Buy => maker <= limit,
        Side::Sell => maker >= limit,
    }
}

fn lots(n: u32) -> Quantity {
    Quantity::new(n).expect("the model only reports positive quantities")
}

fn rejected(seq: u64, command: CommandKind, id: OrderId, reason: RejectCode) -> Event {
    Event::Rejected {
        seq,
        command,
        id: Some(id),
        reason,
    }
}

fn cancelled(seq: u64, id: OrderId, remaining: u32, reason: CancelReason) -> Event {
    Event::Cancelled {
        seq,
        id,
        remaining: lots(remaining),
        reason,
    }
}
