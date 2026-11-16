//! Canonical version 1 bytes for events and state, and the state digest.
//!
//! Records are ASCII, comma separated and LF terminated. Identifiers and
//! owners escape every byte outside printable ASCII, and the comma, quote and
//! backslash, as `\xHH`, so no field can be mistaken for a separator.

use std::fmt;
use std::io::{self, Write};

use crate::domain::{Price, Side, TimeInForce, ValueError};
use crate::engine::{Engine, RestingOrder};
use crate::event::{CancelReason, CommandKind, Cursor, Event, HaltReason, Priority, RejectCode};
use crate::input::{Field, HeaderError, RejectReason, SeqError};

/// The first record of a canonical event stream.
pub const EVENTS_HEADER: &[u8] = b"eob-events,1\n";
/// The first record of canonical state.
pub const STATE_HEADER: &[u8] = b"eob-state,1\n";

/// Writes the event stream header record.
pub fn write_events_header<W: Write + ?Sized>(out: &mut W) -> io::Result<()> {
    out.write_all(EVENTS_HEADER)
}

/// Writes one event record, including its final LF.
pub fn write_event<W: Write + ?Sized>(out: &mut W, event: &Event) -> io::Result<()> {
    write_event_fields(out, event)?;
    out.write_all(b"\n")
}

/// Writes the engine's complete canonical state.
pub fn write_state<W: Write + ?Sized>(out: &mut W, engine: &Engine) -> io::Result<()> {
    out.write_all(STATE_HEADER)?;
    out.write_all(b"next_seq,")?;
    write_cursor(out, engine.cursor())?;
    out.write_all(b"\n")?;
    writeln!(out, "halted,{}", engine.halt().map_or("none", halt_name))?;
    writeln!(out, "priority_clock,{}", engine.priority_clock())?;
    writeln!(out, "bids,{}", engine.bids().count())?;
    for order in engine.bids() {
        write_order(out, order)?;
    }
    writeln!(out, "asks,{}", engine.asks().count())?;
    for order in engine.asks() {
        write_order(out, order)?;
    }
    writeln!(out, "ids,{}", engine.reserved_ids().count())?;
    for id in engine.reserved_ids() {
        out.write_all(b"id,")?;
        write_text(out, id.as_bytes())?;
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Writes `digest,blake3,<64 lowercase hex digits>` and LF.
pub fn write_digest_trailer<W: Write + ?Sized>(out: &mut W, digest: &Digest) -> io::Result<()> {
    writeln!(out, "digest,blake3,{digest}")
}

/// An unkeyed 32-byte BLAKE3 digest of canonical state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// The 32 digest bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Engine {
    /// Writes canonical state; the same bytes [`Engine::digest`] hashes.
    pub fn write_state<W: Write + ?Sized>(&self, out: &mut W) -> io::Result<()> {
        write_state(out, self)
    }

    /// The BLAKE3 digest of the canonical state bytes.
    pub fn digest(&self) -> Digest {
        let mut hasher = blake3::Hasher::new();
        // Writing into a hasher cannot fail, so the io result is always `Ok`.
        write_state(&mut hasher, self).expect("writing to a hasher cannot fail");
        Digest(*hasher.finalize().as_bytes())
    }
}

/// Writes an event's fields, without its LF.
fn write_event_fields<W: Write + ?Sized>(out: &mut W, event: &Event) -> io::Result<()> {
    match event {
        Event::Accepted {
            seq,
            id,
            owner,
            side,
            price,
            size,
            tif,
        } => {
            write!(out, "{seq},accepted,")?;
            write_text(out, id.as_bytes())?;
            out.write_all(b",")?;
            write_text(out, owner.as_bytes())?;
            write!(out, ",{},", side_name(*side))?;
            write_price(out, *price)?;
            write!(out, ",{},{}", size.lots(), tif_name(*tif))
        }
        Event::Rejected {
            seq,
            command,
            id,
            reason,
        } => {
            write!(out, "{seq},rejected,{},", command_name(*command))?;
            if let Some(id) = id {
                write_text(out, id.as_bytes())?;
            }
            out.write_all(b",")?;
            write_reject(out, *reason)
        }
        Event::Trade {
            seq,
            taker,
            maker,
            price,
            quantity,
            taker_left,
            maker_left,
        } => {
            write!(out, "{seq},trade,")?;
            write_text(out, taker.as_bytes())?;
            out.write_all(b",")?;
            write_text(out, maker.as_bytes())?;
            out.write_all(b",")?;
            write_price(out, *price)?;
            write!(out, ",{},{taker_left},{maker_left}", quantity.lots())
        }
        Event::Rested {
            seq,
            id,
            side,
            price,
            remaining,
            rank,
        } => {
            write!(out, "{seq},rested,")?;
            write_text(out, id.as_bytes())?;
            write!(out, ",{},", side_name(*side))?;
            write_price(out, *price)?;
            write!(out, ",{},{rank}", remaining.lots())
        }
        Event::Cancelled {
            seq,
            id,
            remaining,
            reason,
        } => {
            write!(out, "{seq},cancelled,")?;
            write_text(out, id.as_bytes())?;
            write!(out, ",{},{}", remaining.lots(), cancel_name(*reason))
        }
        Event::Modified {
            seq,
            id,
            price,
            remaining,
            priority,
            rank,
        } => {
            write!(out, "{seq},modified,")?;
            write_text(out, id.as_bytes())?;
            out.write_all(b",")?;
            write_price(out, *price)?;
            write!(
                out,
                ",{},{},{rank}",
                remaining.lots(),
                priority_name(*priority)
            )
        }
        Event::Halted {
            expected,
            reason,
            observed,
        } => {
            write_cursor(out, *expected)?;
            write!(out, ",halted,{},", halt_name(*reason))?;
            match observed {
                Some(seq) => write!(out, "{seq}"),
                None => out.write_all(b"none"),
            }
        }
    }
}

/// Writes one LF-terminated `order` record of the state.
fn write_order<W: Write + ?Sized>(out: &mut W, order: &RestingOrder) -> io::Result<()> {
    out.write_all(b"order,")?;
    write_text(out, order.id.as_bytes())?;
    out.write_all(b",")?;
    write_text(out, order.owner.as_bytes())?;
    write!(out, ",{},", side_name(order.side))?;
    writeln!(
        out,
        "{},{},{},{}",
        order.price.cents(),
        order.remaining.lots(),
        tif_name(order.tif),
        order.rank
    )
}

/// Writes an identifier or owner, escaping every byte outside printable ASCII
/// and the three bytes that would otherwise be ambiguous.
fn write_text<W: Write + ?Sized>(out: &mut W, bytes: &[u8]) -> io::Result<()> {
    for &byte in bytes {
        match byte {
            // Bytes above 0x7E are escaped as well, so the output stays ASCII.
            0x20..=0x7E if !matches!(byte, b',' | b'"' | b'\\') => out.write_all(&[byte])?,
            _ => write!(out, "\\x{byte:02X}")?,
        }
    }
    Ok(())
}

/// Writes a price as decimal dollars and two cents digits.
fn write_price<W: Write + ?Sized>(out: &mut W, price: Price) -> io::Result<()> {
    let cents = price.cents();
    write!(out, "{}.{:02}", cents / 100, cents % 100)
}

fn write_cursor<W: Write + ?Sized>(out: &mut W, cursor: Cursor) -> io::Result<()> {
    match cursor {
        Cursor::Next(seq) => write!(out, "{seq}"),
        Cursor::Exhausted => out.write_all(b"exhausted"),
    }
}

fn write_reject<W: Write + ?Sized>(out: &mut W, reason: RejectCode) -> io::Result<()> {
    match reason {
        RejectCode::Input(RejectReason::MalformedCsv) => out.write_all(b"malformed_csv"),
        RejectCode::Input(RejectReason::FieldCount) => out.write_all(b"field_count"),
        RejectCode::Input(RejectReason::UnknownCommand) => out.write_all(b"unknown_command"),
        RejectCode::Input(RejectReason::Unexpected(field)) => {
            write!(out, "unexpected_{}", field_name(field))
        }
        RejectCode::Input(RejectReason::Invalid(field, value)) => {
            write!(out, "invalid_{}_{}", field_name(field), value_name(value))
        }
        RejectCode::DuplicateId => out.write_all(b"duplicate_id"),
        RejectCode::UnknownId => out.write_all(b"unknown_order"),
        RejectCode::ClosedId => out.write_all(b"order_closed"),
        RejectCode::PostOnlyWouldCross => out.write_all(b"post_only_would_cross"),
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Buy => "buy",
        Side::Sell => "sell",
    }
}

fn tif_name(tif: TimeInForce) -> &'static str {
    match tif {
        TimeInForce::Gtc => "GTC",
        TimeInForce::Ioc => "IOC",
        TimeInForce::PostOnly => "POST_ONLY",
    }
}

fn command_name(command: CommandKind) -> &'static str {
    match command {
        CommandKind::Input => "input",
        CommandKind::Place => "place",
        CommandKind::Cancel => "cancel",
        CommandKind::Modify => "modify",
    }
}

fn cancel_name(reason: CancelReason) -> &'static str {
    match reason {
        CancelReason::User => "user",
        CancelReason::IocRemainder => "ioc_remainder",
        CancelReason::SelfTrade => "self_trade",
    }
}

fn priority_name(priority: Priority) -> &'static str {
    match priority {
        Priority::Kept => "kept",
        Priority::Lost => "lost",
    }
}

fn field_name(field: Field) -> &'static str {
    match field {
        Field::Id => "id",
        Field::Owner => "owner",
        Field::Side => "side",
        Field::Price => "price",
        Field::Size => "size",
        Field::Tif => "tif",
    }
}

fn value_name(value: ValueError) -> &'static str {
    match value {
        ValueError::Empty => "empty",
        ValueError::Syntax => "syntax",
        ValueError::NonAscii => "non_ascii",
        ValueError::TooLong => "too_long",
        ValueError::OffTick => "off_tick",
        ValueError::OutOfRange => "out_of_range",
    }
}

fn halt_name(reason: HaltReason) -> &'static str {
    match reason {
        HaltReason::Header(HeaderError::Empty) => "header_empty",
        HaltReason::Header(HeaderError::Mismatch) => "header_mismatch",
        HaltReason::Seq(SeqError::Blank) => "seq_blank",
        HaltReason::Seq(SeqError::Malformed) => "seq_malformed",
        HaltReason::Seq(SeqError::Empty) => "seq_empty",
        HaltReason::Seq(SeqError::Syntax) => "seq_syntax",
        HaltReason::Seq(SeqError::Overflow) => "seq_overflow",
        HaltReason::Gap => "seq_gap",
        HaltReason::Stale => "seq_stale",
        HaltReason::Exhausted => "seq_exhausted",
    }
}
