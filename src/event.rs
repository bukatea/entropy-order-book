//! Typed events the engine emits for each input line.
//!
//! Every event carries the sequence number of the line that caused it. The
//! variants and fields follow the version 1 event records of the execution
//! contract; turning them into bytes is the encoder's job, not this module's.
//! No event carries raw input or library and OS error messages.

use crate::domain::{OrderId, Owner, Price, Quantity, Side, TimeInForce};
use crate::input::{HeaderError, RejectReason, SeqError};

/// The engine's sequence cursor: the next sequence number it will accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cursor {
    /// The next line must carry this sequence number.
    Next(u64),
    /// Sequence number `u64::MAX` was consumed; any further line halts.
    Exhausted,
}

/// The command a rejection refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandKind {
    /// The line is not a valid command; the parser rejected it.
    Input,
    Place,
    Cancel,
    Modify,
}

/// Why a line with the expected sequence number was rejected.
///
/// A rejection consumes the sequence number and changes no resting order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectCode {
    /// The parser rejected the line (command kind [`CommandKind::Input`]).
    Input(RejectReason),
    /// A place reuses an ID that was already reserved.
    DuplicateId,
    /// A cancel or modify names an ID that was never reserved.
    UnknownId,
    /// A cancel or modify names a reserved ID with no live order.
    ClosedId,
    /// A POST_ONLY place or priority-losing modify would cross the best
    /// opposite order.
    PostOnlyWouldCross,
}

/// Why a resting or incoming quantity was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CancelReason {
    /// A cancel command removed the order.
    User,
    /// An IOC order's unfilled remainder.
    IocRemainder,
    /// An incoming order met a resting order of the same owner.
    SelfTrade,
}

/// Whether a modify kept the order's place in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Priority {
    /// Same price and no size increase: the order keeps its rank.
    Kept,
    /// The order was re-entered with a newly issued rank.
    Lost,
}

/// Why the engine stopped processing input. Halting is permanent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HaltReason {
    /// The header is missing or wrong.
    Header(HeaderError),
    /// The line's sequence number could not be recovered.
    Seq(SeqError),
    /// The sequence number is greater than expected (a gap).
    Gap,
    /// The sequence number is less than expected: a duplicate, zero or an
    /// out-of-order line.
    Stale,
    /// A line arrived after sequence number `u64::MAX` was consumed.
    Exhausted,
}

/// One consequence of applying an input line.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Event {
    /// A place passed every check; its fills and remainder follow.
    Accepted {
        seq: u64,
        id: OrderId,
        owner: Owner,
        side: Side,
        price: Price,
        size: Quantity,
        tif: TimeInForce,
    },
    /// The line was rejected. `id` is `None` exactly for parser rejections.
    Rejected {
        seq: u64,
        command: CommandKind,
        id: Option<OrderId>,
        reason: RejectCode,
    },
    /// One fill at the maker's price, with both remainders after it.
    Trade {
        seq: u64,
        taker: OrderId,
        maker: OrderId,
        price: Price,
        quantity: Quantity,
        taker_left: u32,
        maker_left: u32,
    },
    /// A remainder entered the book with this rank.
    Rested {
        seq: u64,
        id: OrderId,
        side: Side,
        price: Price,
        remaining: Quantity,
        rank: u64,
    },
    /// A live or incoming remainder was removed. Never zero.
    Cancelled {
        seq: u64,
        id: OrderId,
        remaining: Quantity,
        reason: CancelReason,
    },
    /// A modify was applied. With [`Priority::Lost`], matching and resting
    /// events may follow; `rank` is the order's rank after the modify.
    Modified {
        seq: u64,
        id: OrderId,
        price: Price,
        remaining: Quantity,
        priority: Priority,
        rank: u64,
    },
    /// The engine halted. `expected` is the cursor at the time; `observed`
    /// is the line's sequence number when one was recovered.
    Halted {
        expected: Cursor,
        reason: HaltReason,
        observed: Option<u64>,
    },
}
