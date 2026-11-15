//! Strict CSV input, one physical line per command.
//!
//! A line is everything up to an LF, with one CR removed just before that
//! LF. A quoted field can never span lines, so a broken quote affects only
//! its own line. A small lexical guard enforces quote shape; the `csv` crate
//! decodes the fields of lines that pass it.
//!
//! [`parse_line`] reports three outcomes. A line whose sequence number cannot
//! be recovered is a [`SeqError`], which is terminal. Otherwise the line has
//! a sequence number and either a valid [`Command`] or a [`RejectReason`].
//! The parser keeps no state between lines: checking the sequence order and
//! the IDs seen so far belongs to the engine.
//!
//! # Validation order
//!
//! Each line is checked in this fixed order, and the first failure wins:
//!
//! 1. Blank line, then the first field's quote shape, then its value as
//!    unsigned decimal digits that fit in `u64` (terminal [`SeqError`]s).
//! 2. Quote shape of the rest of the line ([`RejectReason::MalformedCsv`]).
//! 3. Exactly eight fields ([`RejectReason::FieldCount`]).
//! 4. The command keyword ([`RejectReason::UnknownCommand`]).
//! 5. The remaining fields from left to right: id, owner, side, price, size,
//!    tif. A required field must be valid ([`RejectReason::Invalid`]) and an
//!    unused one must be empty ([`RejectReason::Unexpected`]). Within one
//!    field, the order is the one documented on its domain parser.

use std::fmt;

use crate::domain::{
    Cancel, Command, Modify, OrderId, Owner, Place, Price, Quantity, Side, TimeInForce, ValueError,
};

/// The only accepted header line, without its terminator.
pub const HEADER: &[u8] = b"seq,cmd,id,owner,side,price,size,tif";

/// The number of fields in every command line.
pub const FIELD_COUNT: usize = 8;

/// A terminal problem with the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeaderError {
    /// The input has no bytes at all.
    Empty,
    /// The first line is not exactly [`HEADER`].
    Mismatch,
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "input is empty",
            Self::Mismatch => "header does not match",
        })
    }
}

impl std::error::Error for HeaderError {}

/// A terminal problem: the line's sequence number cannot be recovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SeqError {
    /// The line is empty.
    Blank,
    /// The first field's CSV quoting is malformed.
    Malformed,
    /// The first field is empty.
    Empty,
    /// The first field is not unsigned decimal digits.
    Syntax,
    /// The first field is larger than `u64::MAX`.
    Overflow,
}

impl fmt::Display for SeqError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Blank => "line is blank",
            Self::Malformed => "sequence field has malformed quoting",
            Self::Empty => "sequence field is empty",
            Self::Syntax => "sequence field is not decimal digits",
            Self::Overflow => "sequence number is too large",
        })
    }
}

impl std::error::Error for SeqError {}

/// A command field after `seq` and `cmd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    Id,
    Owner,
    Side,
    Price,
    Size,
    Tif,
}

impl Field {
    fn name(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Owner => "owner",
            Self::Side => "side",
            Self::Price => "price",
            Self::Size => "size",
            Self::Tif => "tif",
        }
    }
}

/// Why a line with a valid sequence number is not a command.
///
/// These identities are stable; none carries raw input or parser messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// A field after the first has malformed CSV quoting or a stray CR.
    MalformedCsv,
    /// The line does not have exactly [`FIELD_COUNT`] fields.
    FieldCount,
    /// The command keyword is not `place`, `cancel` or `modify`.
    UnknownCommand,
    /// A field the command needs is empty or invalid.
    Invalid(Field, ValueError),
    /// A field the command does not use is not empty.
    Unexpected(Field),
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedCsv => f.write_str("malformed CSV"),
            Self::FieldCount => f.write_str("wrong number of fields"),
            Self::UnknownCommand => f.write_str("unknown command"),
            Self::Invalid(field, error) => write!(f, "invalid {}: {error}", field.name()),
            Self::Unexpected(field) => write!(f, "unexpected {}", field.name()),
        }
    }
}

impl std::error::Error for RejectReason {}

/// A command line whose sequence number was recovered.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParsedLine {
    /// The observed sequence number; the engine checks it before `result`.
    pub seq: u64,
    /// The typed command, or why the line is rejected.
    pub result: Result<Command, RejectReason>,
}

/// Checks a header line, given without its terminator.
pub fn parse_header(line: &[u8]) -> Result<(), HeaderError> {
    if line == HEADER {
        Ok(())
    } else {
        Err(HeaderError::Mismatch)
    }
}

/// Parses one command line, given without its terminator.
pub fn parse_line(line: &[u8]) -> Result<ParsedLine, SeqError> {
    if line.is_empty() {
        return Err(SeqError::Blank);
    }
    let first_end = scan_field(line, 0).ok_or(SeqError::Malformed)?;
    let seq = parse_seq(decode(&line[..first_end]).get(0).unwrap_or_default())?;
    let result = if rest_is_well_formed(line, first_end) {
        parse_fields(&decode(line))
    } else {
        Err(RejectReason::MalformedCsv)
    };
    Ok(ParsedLine { seq, result })
}

/// Checks the header and returns an iterator over the command lines.
pub fn commands(input: &[u8]) -> Result<CommandLines<'_>, HeaderError> {
    let mut lines = PhysicalLines::new(input);
    parse_header(lines.next().ok_or(HeaderError::Empty)?)?;
    Ok(CommandLines { lines })
}

/// Iterator over the parsed command lines after the header.
///
/// It parses every remaining line; stopping after a terminal error is the
/// caller's decision.
#[derive(Debug, Clone)]
pub struct CommandLines<'a> {
    lines: PhysicalLines<'a>,
}

impl Iterator for CommandLines<'_> {
    type Item = Result<ParsedLine, SeqError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.lines.next().map(parse_line)
    }
}

/// Iterator over physical lines, without their LF or CRLF terminators.
///
/// An unterminated final line is a line; the empty remainder after a final
/// LF is not. A CR that does not directly precede an LF is kept.
#[derive(Debug, Clone)]
pub struct PhysicalLines<'a> {
    rest: &'a [u8],
}

impl<'a> PhysicalLines<'a> {
    pub fn new(input: &'a [u8]) -> Self {
        Self { rest: input }
    }
}

impl<'a> Iterator for PhysicalLines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        match self.rest.iter().position(|&b| b == b'\n') {
            Some(lf) => {
                let line = &self.rest[..lf];
                self.rest = &self.rest[lf + 1..];
                Some(line.strip_suffix(b"\r").unwrap_or(line))
            }
            None => Some(std::mem::take(&mut self.rest)),
        }
    }
}

/// Returns the end of the field starting at `start` if its shape is valid.
///
/// A quote may only open a field; inside it, a doubled quote is a quote and
/// the closing quote must be followed by a comma or the end of the line.
/// CR and LF are never valid inside a line.
fn scan_field(line: &[u8], start: usize) -> Option<usize> {
    let is_break = |b: &u8| matches!(b, b'\r' | b'\n');
    if line.get(start) != Some(&b'"') {
        let len = line[start..]
            .iter()
            .position(|&b| b == b',')
            .unwrap_or(line.len() - start);
        let field = &line[start..start + len];
        return (!field.iter().any(|b| *b == b'"' || is_break(b))).then_some(start + len);
    }
    let mut i = start + 1;
    loop {
        match line.get(i)? {
            b'"' if line.get(i + 1) == Some(&b'"') => i += 2,
            b'"' => {
                let end = i + 1;
                return matches!(line.get(end), None | Some(b',')).then_some(end);
            }
            b if is_break(b) => return None,
            _ => i += 1,
        }
    }
}

fn rest_is_well_formed(line: &[u8], mut end: usize) -> bool {
    while end < line.len() {
        // `scan_field` only stops at a comma or the end of the line.
        match scan_field(line, end + 1) {
            Some(next) => end = next,
            None => return false,
        }
    }
    true
}

/// Decodes a line whose quote shape is already valid.
fn decode(line: &[u8]) -> csv::ByteRecord {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(line);
    let mut record = csv::ByteRecord::new();
    match reader.read_byte_record(&mut record) {
        Ok(true) => record,
        _ => csv::ByteRecord::new(),
    }
}

fn parse_seq(text: &[u8]) -> Result<u64, SeqError> {
    if text.is_empty() {
        return Err(SeqError::Empty);
    }
    if !text.iter().all(u8::is_ascii_digit) {
        return Err(SeqError::Syntax);
    }
    text.iter()
        .try_fold(0u64, |value, &b| {
            value.checked_mul(10)?.checked_add(u64::from(b - b'0'))
        })
        .ok_or(SeqError::Overflow)
}

fn parse_fields(record: &csv::ByteRecord) -> Result<Command, RejectReason> {
    if record.len() != FIELD_COUNT {
        return Err(RejectReason::FieldCount);
    }
    let field = |i| &record[i];
    let id = || OrderId::new(field(2)).map_err(|e| RejectReason::Invalid(Field::Id, e));
    let empty = |i, name| {
        if field(i).is_empty() {
            Ok(())
        } else {
            Err(RejectReason::Unexpected(name))
        }
    };
    let invalid = |name| move |e| RejectReason::Invalid(name, e);
    match field(1) {
        b"place" => Ok(Command::Place(Place {
            id: id()?,
            owner: Owner::new(field(3)).map_err(invalid(Field::Owner))?,
            side: Side::parse(field(4)).map_err(invalid(Field::Side))?,
            price: Price::parse(field(5)).map_err(invalid(Field::Price))?,
            size: Quantity::parse(field(6)).map_err(invalid(Field::Size))?,
            tif: TimeInForce::parse(field(7)).map_err(invalid(Field::Tif))?,
        })),
        b"cancel" => {
            let id = id()?;
            empty(3, Field::Owner)?;
            empty(4, Field::Side)?;
            empty(5, Field::Price)?;
            empty(6, Field::Size)?;
            empty(7, Field::Tif)?;
            Ok(Command::Cancel(Cancel { id }))
        }
        b"modify" => {
            let id = id()?;
            empty(3, Field::Owner)?;
            empty(4, Field::Side)?;
            let price = Price::parse(field(5)).map_err(invalid(Field::Price))?;
            let size = Quantity::parse(field(6)).map_err(invalid(Field::Size))?;
            empty(7, Field::Tif)?;
            Ok(Command::Modify(Modify { id, price, size }))
        }
        _ => Err(RejectReason::UnknownCommand),
    }
}
