//! The matching engine: sequenced command lines in, typed events out.
//!
//! [`Engine::apply`] takes one parsed line and returns the events it causes;
//! [`replay`] runs a whole input and stops at the first halt. The engine is
//! the only owner of market state: the sequence cursor, the resting orders,
//! the reserved IDs and the priority clock. Nothing else mutates it.
//!
//! # Rules
//!
//! Each line is judged in this order, and the first failure wins:
//!
//! 1. A halted engine returns [`AlreadyHalted`] and changes nothing.
//! 2. A line whose sequence number could not be recovered halts the engine.
//! 3. A line with the wrong sequence number halts it too, unapplied: a gap
//!    is greater than the cursor, a stale line is less than it, and any line
//!    after `u64::MAX` halts because the cursor is exhausted.
//! 4. The sequence number is then consumed, so a rejected line still uses it.
//!    The command is checked against the reserved IDs, the live orders and
//!    the post-only rule. A place that rests and a priority-losing modify
//!    each issue one rank from the clock. Nothing else does.
//!
//! # Invariants
//!
//! - Ranks are unique, and the clock is the last rank issued, so every rank
//!   is at most the clock.
//! - Each live order sits in exactly one book, under its own key, and has
//!   exactly one entry in the ID index with the same side, price and rank.
//! - Live IDs are a subset of reserved IDs. Reserved IDs never shrink.
//! - The books never cross once a line has been applied.
//! - Halting is permanent. A halted engine's state never changes again.
//!
//! # Complexity
//!
//! Finding the best order, inserting one, and finding one by ID are each
//! `O(log n)` in the number of live orders. A line costs `O(k log n)`, where
//! `k` is the number of resting orders it fills or removes, plus one insert.
//! Nothing scans the book, and no line leaves tombstones behind.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{
    Cancel, Command, Modify, OrderId, Owner, Place, Price, Quantity, Side, TimeInForce,
};
use crate::event::{CancelReason, CommandKind, Cursor, Event, HaltReason, Priority, RejectCode};
use crate::input::{HeaderError, ParsedLine, SeqError, commands};

/// A live order resting on one side of the book.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RestingOrder {
    /// The order's ID, as given by the place that created it.
    pub id: OrderId,
    /// The owner that placed it; used for self-trade prevention.
    pub owner: Owner,
    /// The side of the book the order rests on.
    pub side: Side,
    /// The limit price.
    pub price: Price,
    /// The quantity still open.
    pub remaining: Quantity,
    /// The time in force it was placed or last re-entered with.
    pub tif: TimeInForce,
    /// Time priority: among equal prices, lower ranks execute first.
    pub rank: u64,
}

/// Returned when a halted engine is asked to apply input.
///
/// Nothing is applied and no events are produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AlreadyHalted;

/// The matching engine.
///
/// Its state changes only through its methods. Once halted, it accepts no
/// further input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Engine {
    /// The next sequence number to accept, or exhaustion.
    cursor: Cursor,
    /// Why the engine stopped, once it has.
    halt: Option<HaltReason>,
    /// The last rank issued. Every rank on the book is at most this.
    clock: u64,
    /// Buy orders, best first: highest price, then oldest rank.
    bids: BTreeMap<(Reverse<Price>, u64), RestingOrder>,
    /// Sell orders, best first: lowest price, then oldest rank.
    asks: BTreeMap<(Price, u64), RestingOrder>,
    /// Live orders by ID, with the side, price and rank that find the book entry.
    index: BTreeMap<OrderId, (Side, Price, u64)>,
    /// Every ID a place has claimed, including rejected post-only places.
    reserved: BTreeSet<OrderId>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

/// An order taking liquidity during matching: its identity and its limit.
struct Taker<'a> {
    /// The sequence number of the line that brought the order in.
    seq: u64,
    id: &'a OrderId,
    owner: &'a Owner,
    side: Side,
    /// The worst price the taker accepts.
    limit: Price,
}

impl Engine {
    /// Creates an engine at sequence number 1, with an empty book.
    pub fn new() -> Self {
        Self {
            cursor: Cursor::Next(1),
            halt: None,
            clock: 0,
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            index: BTreeMap::new(),
            reserved: BTreeSet::new(),
        }
    }

    /// Applies one parsed line, or a line whose sequence number was lost.
    ///
    /// Returns the events the line causes, in order. A line that halts the
    /// engine produces one `Halted` event and is not applied. Returns
    /// [`AlreadyHalted`] without changing anything if the engine has halted.
    pub fn apply(
        &mut self,
        line: Result<ParsedLine, SeqError>,
    ) -> Result<Vec<Event>, AlreadyHalted> {
        if self.halt.is_some() {
            return Err(AlreadyHalted);
        }
        let line = match line {
            Ok(line) => line,
            Err(error) => return Ok(self.halt_with(HaltReason::Seq(error), None)),
        };
        let expected = match self.cursor {
            Cursor::Exhausted => return Ok(self.halt_with(HaltReason::Exhausted, Some(line.seq))),
            Cursor::Next(next) => next,
        };
        if line.seq != expected {
            let reason = if line.seq > expected {
                HaltReason::Gap
            } else {
                HaltReason::Stale
            };
            return Ok(self.halt_with(reason, Some(line.seq)));
        }
        // The sequence number is consumed before the command is judged, so
        // a rejected line still advances the cursor.
        self.cursor = expected
            .checked_add(1)
            .map_or(Cursor::Exhausted, Cursor::Next);

        let seq = line.seq;
        let mut events = Vec::new();
        match line.result {
            Err(reason) => events.push(Event::Rejected {
                seq,
                command: CommandKind::Input,
                id: None,
                reason: RejectCode::Input(reason),
            }),
            Ok(Command::Place(order)) => self.place(seq, order, &mut events),
            Ok(Command::Cancel(order)) => self.cancel(seq, order, &mut events),
            Ok(Command::Modify(order)) => self.modify(seq, order, &mut events),
        }
        Ok(events)
    }

    /// Reports a header problem, which halts the engine at its cursor.
    ///
    /// Returns [`AlreadyHalted`] without changing anything if the engine has
    /// already halted.
    pub fn reject_header(&mut self, error: HeaderError) -> Result<Vec<Event>, AlreadyHalted> {
        if self.halt.is_some() {
            return Err(AlreadyHalted);
        }
        Ok(self.halt_with(HaltReason::Header(error), None))
    }

    /// The next sequence number to accept, or [`Cursor::Exhausted`].
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Why the engine halted, or `None` while it is running.
    pub fn halt(&self) -> Option<HaltReason> {
        self.halt
    }

    /// The last priority rank issued.
    pub fn priority_clock(&self) -> u64 {
        self.clock
    }

    /// Live buy orders, in execution priority: highest price, then oldest rank.
    pub fn bids(&self) -> impl Iterator<Item = &RestingOrder> {
        self.bids.values()
    }

    /// Live sell orders, in execution priority: lowest price, then oldest rank.
    pub fn asks(&self) -> impl Iterator<Item = &RestingOrder> {
        self.asks.values()
    }

    /// The live order with this ID, or `None` if it is unknown or closed.
    pub fn order(&self, id: &OrderId) -> Option<&RestingOrder> {
        self.live(id)
    }

    /// The number of live orders.
    pub fn live_count(&self) -> usize {
        self.index.len()
    }

    /// Every reserved ID, sorted by bytes.
    pub fn reserved_ids(&self) -> impl Iterator<Item = &OrderId> {
        self.reserved.iter()
    }

    /// Halts the engine, and returns the one `Halted` event it produces.
    fn halt_with(&mut self, reason: HaltReason, observed: Option<u64>) -> Vec<Event> {
        self.halt = Some(reason);
        vec![Event::Halted {
            expected: self.cursor,
            reason,
            observed,
        }]
    }

    /// Handles a place, which may reject, match and rest.
    fn place(&mut self, seq: u64, order: Place, events: &mut Vec<Event>) {
        let Place {
            id,
            owner,
            side,
            price,
            size,
            tif,
        } = order;
        if self.reserved.contains(&id) {
            events.push(rejected(
                seq,
                CommandKind::Place,
                id,
                RejectCode::DuplicateId,
            ));
            return;
        }
        self.reserved.insert(id.clone());
        if tif == TimeInForce::PostOnly && self.would_cross(side, price) {
            events.push(rejected(
                seq,
                CommandKind::Place,
                id,
                RejectCode::PostOnlyWouldCross,
            ));
            return;
        }
        events.push(Event::Accepted {
            seq,
            id: id.clone(),
            owner: owner.clone(),
            side,
            price,
            size,
            tif,
        });
        let taker = Taker {
            seq,
            id: &id,
            owner: &owner,
            side,
            limit: price,
        };
        let left = self.match_incoming(&taker, size.lots(), events);
        // `None` when nothing is left to rest or cancel.
        let Some(remaining) = Quantity::new(left) else {
            return;
        };
        match tif {
            TimeInForce::Ioc => events.push(Event::Cancelled {
                seq,
                id,
                remaining,
                reason: CancelReason::IocRemainder,
            }),
            TimeInForce::Gtc | TimeInForce::PostOnly => {
                let rank = self.issue_rank();
                events.push(Event::Rested {
                    seq,
                    id: id.clone(),
                    side,
                    price,
                    remaining,
                    rank,
                });
                self.rest(RestingOrder {
                    id,
                    owner,
                    side,
                    price,
                    remaining,
                    tif,
                    rank,
                });
            }
        }
    }

    /// Handles a cancel of a live order.
    fn cancel(&mut self, seq: u64, order: Cancel, events: &mut Vec<Event>) {
        let id = order.id;
        match self.remove_live(&id) {
            Some(live) => events.push(Event::Cancelled {
                seq,
                id,
                remaining: live.remaining,
                reason: CancelReason::User,
            }),
            None => {
                let reason = self.missing(&id);
                events.push(rejected(seq, CommandKind::Cancel, id, reason));
            }
        }
    }

    /// Handles a modify. A same-price, not-larger modify keeps its rank;
    /// anything else re-enters the book with a new rank and may trade.
    fn modify(&mut self, seq: u64, order: Modify, events: &mut Vec<Event>) {
        let Modify { id, price, size } = order;
        let Some(old) = self.live(&id).cloned() else {
            let reason = self.missing(&id);
            events.push(rejected(seq, CommandKind::Modify, id, reason));
            return;
        };
        if price == old.price && size.lots() <= old.remaining.lots() {
            // The order keeps its place in line; only its size changes.
            self.live_mut(&id)
                .expect("the order was found live above")
                .remaining = size;
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
        if old.tif == TimeInForce::PostOnly && self.would_cross(old.side, price) {
            events.push(rejected(
                seq,
                CommandKind::Modify,
                id,
                RejectCode::PostOnlyWouldCross,
            ));
            return;
        }
        // The order leaves the book and re-enters under one new rank.
        self.remove_live(&id);
        let rank = self.issue_rank();
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
        let left = self.match_incoming(&taker, size.lots(), events);
        if let Some(remaining) = Quantity::new(left) {
            events.push(Event::Rested {
                seq,
                id: id.clone(),
                side: old.side,
                price,
                remaining,
                rank,
            });
            self.rest(RestingOrder {
                id,
                owner: old.owner,
                side: old.side,
                price,
                remaining,
                tif: old.tif,
                rank,
            });
        }
    }

    /// Matches a taker against the opposite book and returns its unfilled
    /// quantity.
    ///
    /// Each resting order that crosses is either filled, in part or in full,
    /// or cancelled when it has the taker's owner (self-trade prevention).
    /// Matching stops when nothing is left or no order crosses.
    fn match_incoming(&mut self, taker: &Taker<'_>, mut left: u32, events: &mut Vec<Event>) -> u32 {
        while left > 0 {
            let Some(maker) = self
                .best_opposite(taker.side)
                .filter(|maker| crosses(taker.side, taker.limit, maker.price))
                .cloned()
            else {
                break;
            };
            if maker.owner == *taker.owner {
                self.remove_live(&maker.id);
                events.push(Event::Cancelled {
                    seq: taker.seq,
                    id: maker.id,
                    remaining: maker.remaining,
                    reason: CancelReason::SelfTrade,
                });
                continue;
            }
            let quantity = left.min(maker.remaining.lots());
            left -= quantity;
            let maker_left = maker.remaining.lots() - quantity;
            events.push(Event::Trade {
                seq: taker.seq,
                taker: taker.id.clone(),
                maker: maker.id.clone(),
                price: maker.price,
                // `left` was positive before this fill, so the fill is at least one lot.
                quantity: Quantity::new(quantity).expect("a fill is at least one lot"),
                taker_left: left,
                maker_left,
            });
            // `Quantity::new` is `None` exactly when the maker is filled.
            if let Some(remaining) = Quantity::new(maker_left) {
                self.live_mut(&maker.id)
                    .expect("the maker was found live above")
                    .remaining = remaining;
            } else {
                self.remove_live(&maker.id);
            }
        }
        left
    }

    /// Issues the next priority rank.
    fn issue_rank(&mut self) -> u64 {
        // Each line issues at most one rank, and the clock never passes the
        // number of consumed sequence numbers, which is at most `u64::MAX`.
        self.clock = self
            .clock
            .checked_add(1)
            .expect("the clock counts consumed sequence numbers and cannot overflow");
        self.clock
    }

    /// The best resting order on the side opposite `side`.
    fn best_opposite(&self, side: Side) -> Option<&RestingOrder> {
        match side {
            Side::Buy => self.asks.first_key_value().map(|(_, order)| order),
            Side::Sell => self.bids.first_key_value().map(|(_, order)| order),
        }
    }

    /// Whether an order on `side` at `limit` would trade with the best
    /// opposite order right now, regardless of owner.
    fn would_cross(&self, side: Side, limit: Price) -> bool {
        self.best_opposite(side)
            .is_some_and(|best| crosses(side, limit, best.price))
    }

    /// The reason a cancel or modify of `id` has no live order to act on.
    fn missing(&self, id: &OrderId) -> RejectCode {
        if self.reserved.contains(id) {
            RejectCode::ClosedId
        } else {
            RejectCode::UnknownId
        }
    }

    /// Inserts an order into its book and the ID index.
    fn rest(&mut self, order: RestingOrder) {
        self.index
            .insert(order.id.clone(), (order.side, order.price, order.rank));
        match order.side {
            Side::Buy => self.bids.insert((Reverse(order.price), order.rank), order),
            Side::Sell => self.asks.insert((order.price, order.rank), order),
        };
    }

    /// Removes a live order from its book and the ID index.
    fn remove_live(&mut self, id: &OrderId) -> Option<RestingOrder> {
        let (side, price, rank) = self.index.remove(id)?;
        match side {
            Side::Buy => self.bids.remove(&(Reverse(price), rank)),
            Side::Sell => self.asks.remove(&(price, rank)),
        }
    }

    /// The live order with this ID.
    fn live(&self, id: &OrderId) -> Option<&RestingOrder> {
        let &(side, price, rank) = self.index.get(id)?;
        match side {
            Side::Buy => self.bids.get(&(Reverse(price), rank)),
            Side::Sell => self.asks.get(&(price, rank)),
        }
    }

    /// The live order with this ID, mutably. Its key never changes.
    fn live_mut(&mut self, id: &OrderId) -> Option<&mut RestingOrder> {
        let &(side, price, rank) = self.index.get(id)?;
        match side {
            Side::Buy => self.bids.get_mut(&(Reverse(price), rank)),
            Side::Sell => self.asks.get_mut(&(price, rank)),
        }
    }
}

/// Whether an order on `side` at `limit` trades against a resting `price`.
fn crosses(side: Side, limit: Price, resting: Price) -> bool {
    match side {
        Side::Buy => resting <= limit,
        Side::Sell => resting >= limit,
    }
}

/// A rejection of a command with a known ID.
fn rejected(seq: u64, command: CommandKind, id: OrderId, reason: RejectCode) -> Event {
    Event::Rejected {
        seq,
        command,
        id: Some(id),
        reason,
    }
}

/// Replays a whole input: the header, then each command line.
///
/// Stops after the first line that halts the engine, so later lines are not
/// applied. Returns the engine as it stands and every event produced.
pub fn replay(input: &[u8]) -> (Engine, Vec<Event>) {
    let mut engine = Engine::new();
    let mut events = Vec::new();
    match commands(input) {
        Ok(lines) => {
            for line in lines {
                // The loop stops as soon as the engine halts, so it is never halted here.
                if let Ok(batch) = engine.apply(line) {
                    events.extend(batch);
                }
                if engine.halt().is_some() {
                    break;
                }
            }
        }
        Err(error) => {
            if let Ok(batch) = engine.reject_header(error) {
                events.extend(batch);
            }
        }
    }
    (engine, events)
}

// exkg-verifies: fea-ky6m7m#ac-identity
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ValueError;
    use crate::input::{Field, RejectReason, parse_line};

    /// An engine whose next sequence number is `u64::MAX`.
    fn at_max() -> Engine {
        Engine {
            cursor: Cursor::Next(u64::MAX),
            ..Engine::new()
        }
    }

    #[test]
    fn last_sequence_number_is_consumed_then_halts() {
        let mut e = at_max();
        let rejected_line = parse_line(b"18446744073709551615,place,a1,o1,buy,1.001,1,GTC");
        assert_eq!(
            e.apply(rejected_line),
            Ok(vec![Event::Rejected {
                seq: u64::MAX,
                command: CommandKind::Input,
                id: None,
                reason: RejectCode::Input(RejectReason::Invalid(Field::Price, ValueError::OffTick)),
            }])
        );
        assert_eq!(e.cursor(), Cursor::Exhausted);
        assert_eq!(e.halt(), None);
        assert_eq!(e.reserved_ids().count(), 0);

        let late = parse_line(b"18446744073709551615,cancel,a1,,,,,");
        assert_eq!(
            e.apply(late),
            Ok(vec![Event::Halted {
                expected: Cursor::Exhausted,
                reason: HaltReason::Exhausted,
                observed: Some(u64::MAX),
            }])
        );
        assert_eq!(
            e.apply(parse_line(b"18446744073709551615,cancel,a1,,,,,")),
            Err(AlreadyHalted)
        );
    }

    #[test]
    fn exhausted_engine_halts_on_a_lost_sequence_number() {
        let mut e = Engine {
            cursor: Cursor::Exhausted,
            ..Engine::new()
        };
        assert_eq!(
            e.apply(parse_line(b"")),
            Ok(vec![Event::Halted {
                expected: Cursor::Exhausted,
                reason: HaltReason::Seq(SeqError::Blank),
                observed: None,
            }])
        );
    }
}
