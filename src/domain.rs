//! Typed commands and the bounded values they carry.
//!
//! Every constructor validates its bounds, so a value of these types is
//! always in range. Text forms are parsed from raw bytes without floats and
//! without overflow, whatever the input length.

use std::fmt;

/// Lowest valid price, in cents.
pub const MIN_PRICE_CENTS: u32 = 1;
/// Highest valid price, in cents (1,000,000.00).
pub const MAX_PRICE_CENTS: u32 = 100_000_000;
/// Lowest valid order or modify quantity, in lots.
pub const MIN_QUANTITY: u32 = 1;
/// Highest valid order or modify quantity, in lots.
pub const MAX_QUANTITY: u32 = 1_000_000_000;
/// Longest valid order ID or owner, in bytes.
pub const MAX_IDENT_LEN: usize = 32;

/// Why a field value is invalid. These are stable identities, not messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueError {
    /// A required value is empty.
    Empty,
    /// The text is not in the field's format (or not a known keyword).
    Syntax,
    /// An identifier contains a byte outside ASCII.
    NonAscii,
    /// An identifier is longer than [`MAX_IDENT_LEN`] bytes.
    TooLong,
    /// A price has a nonzero digit beyond whole cents.
    OffTick,
    /// A number is well formed but outside its inclusive bounds.
    OutOfRange,
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "value is empty",
            Self::Syntax => "value has invalid syntax",
            Self::NonAscii => "value contains a non-ASCII byte",
            Self::TooLong => "value is too long",
            Self::OffTick => "price is not a whole number of cents",
            Self::OutOfRange => "value is out of range",
        })
    }
}

impl std::error::Error for ValueError {}

/// A limit price in integer cents, from [`MIN_PRICE_CENTS`] to [`MAX_PRICE_CENTS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Price(u32);

impl Price {
    /// Returns the price for `cents`, or `None` when it is out of range.
    pub fn from_cents(cents: u32) -> Option<Self> {
        (MIN_PRICE_CENTS..=MAX_PRICE_CENTS)
            .contains(&cents)
            .then_some(Self(cents))
    }

    /// The price in cents.
    pub fn cents(self) -> u32 {
        self.0
    }

    /// Parses decimal text: digits, optionally a dot followed by digits.
    ///
    /// Leading zeros are allowed. Digits beyond the second fractional digit
    /// must all be zero. Checks run in a fixed order: [`ValueError::Empty`],
    /// [`ValueError::Syntax`], [`ValueError::OffTick`], [`ValueError::OutOfRange`].
    pub fn parse(text: &[u8]) -> Result<Self, ValueError> {
        if text.is_empty() {
            return Err(ValueError::Empty);
        }
        let (whole, fraction) = match text.iter().position(|&b| b == b'.') {
            Some(dot) => (&text[..dot], Some(&text[dot + 1..])),
            None => (text, None),
        };
        if !is_digits(whole) || fraction.is_some_and(|f| !is_digits(f)) {
            return Err(ValueError::Syntax);
        }
        let fraction = fraction.unwrap_or_default();
        let (cents_digits, excess) = fraction.split_at(fraction.len().min(2));
        if excess.iter().any(|&b| b != b'0') {
            return Err(ValueError::OffTick);
        }
        let mut padded = [b'0'; 2];
        padded[..cents_digits.len()].copy_from_slice(cents_digits);
        let dollars = digits_value(whole, MAX_PRICE_CENTS / 100).ok_or(ValueError::OutOfRange)?;
        let cents = digits_value(&padded, 99).unwrap_or_default();
        Self::from_cents(dollars * 100 + cents).ok_or(ValueError::OutOfRange)
    }
}

/// A quantity in lots, from [`MIN_QUANTITY`] to [`MAX_QUANTITY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Quantity(u32);

impl Quantity {
    /// Returns the quantity for `lots`, or `None` when it is out of range.
    pub fn new(lots: u32) -> Option<Self> {
        (MIN_QUANTITY..=MAX_QUANTITY)
            .contains(&lots)
            .then_some(Self(lots))
    }

    /// The quantity in lots.
    pub fn lots(self) -> u32 {
        self.0
    }

    /// Parses decimal digits; leading zeros are allowed.
    ///
    /// Checks run in a fixed order: [`ValueError::Empty`],
    /// [`ValueError::Syntax`], [`ValueError::OutOfRange`].
    pub fn parse(text: &[u8]) -> Result<Self, ValueError> {
        if text.is_empty() {
            return Err(ValueError::Empty);
        }
        if !is_digits(text) {
            return Err(ValueError::Syntax);
        }
        digits_value(text, MAX_QUANTITY)
            .and_then(Self::new)
            .ok_or(ValueError::OutOfRange)
    }
}

/// An order ID: 1 to [`MAX_IDENT_LEN`] ASCII bytes, kept exactly as given.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrderId(String);

/// An order owner: 1 to [`MAX_IDENT_LEN`] ASCII bytes, kept exactly as given.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Owner(String);

macro_rules! ident {
    ($name:ident) => {
        impl $name {
            /// Validates `bytes` without trimming, normalizing or case folding.
            ///
            /// Any ASCII byte is accepted, including controls, NUL and DEL.
            /// Checks run in a fixed order: [`ValueError::Empty`],
            /// [`ValueError::NonAscii`], [`ValueError::TooLong`].
            pub fn new(bytes: &[u8]) -> Result<Self, ValueError> {
                validate_ident(bytes).map(Self)
            }

            /// The identifier's bytes as given.
            pub fn as_bytes(&self) -> &[u8] {
                self.0.as_bytes()
            }

            /// The identifier as text; it is always ASCII.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

ident!(OrderId);
ident!(Owner);

fn validate_ident(bytes: &[u8]) -> Result<String, ValueError> {
    if bytes.is_empty() {
        return Err(ValueError::Empty);
    }
    let text = std::str::from_utf8(bytes)
        .ok()
        .filter(|text| text.is_ascii())
        .ok_or(ValueError::NonAscii)?;
    if text.len() > MAX_IDENT_LEN {
        return Err(ValueError::TooLong);
    }
    Ok(text.to_owned())
}

/// The side of the book an order joins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// Parses the case-sensitive keyword `buy` or `sell`.
    pub fn parse(text: &[u8]) -> Result<Self, ValueError> {
        match text {
            b"" => Err(ValueError::Empty),
            b"buy" => Ok(Self::Buy),
            b"sell" => Ok(Self::Sell),
            _ => Err(ValueError::Syntax),
        }
    }
}

/// How long an order may stay on the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimeInForce {
    /// Good till cancelled: match, then rest any remainder.
    Gtc,
    /// Immediate or cancel: match, then cancel any remainder.
    Ioc,
    /// Rest without taking liquidity; reject if it would cross.
    PostOnly,
}

impl TimeInForce {
    /// Parses the case-sensitive keyword `GTC`, `IOC` or `POST_ONLY`.
    pub fn parse(text: &[u8]) -> Result<Self, ValueError> {
        match text {
            b"" => Err(ValueError::Empty),
            b"GTC" => Ok(Self::Gtc),
            b"IOC" => Ok(Self::Ioc),
            b"POST_ONLY" => Ok(Self::PostOnly),
            _ => Err(ValueError::Syntax),
        }
    }
}

/// A new limit order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Place {
    pub id: OrderId,
    pub owner: Owner,
    pub side: Side,
    pub price: Price,
    pub size: Quantity,
    pub tif: TimeInForce,
}

/// Cancels the live remainder of an order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Cancel {
    pub id: OrderId,
}

/// Amends a live order's price and remaining quantity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Modify {
    pub id: OrderId,
    pub price: Price,
    /// The new remaining quantity, not a lifetime total.
    pub size: Quantity,
}

/// A fully validated command.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Command {
    Place(Place),
    Cancel(Cancel),
    Modify(Modify),
}

pub(crate) fn is_digits(text: &[u8]) -> bool {
    !text.is_empty() && text.iter().all(u8::is_ascii_digit)
}

/// Returns the value of nonempty ASCII `digits` if it is at most `max`.
///
/// Leading zeros are skipped and every step is checked, so arbitrarily long
/// input neither overflows nor behaves differently between build profiles.
pub(crate) fn digits_value(digits: &[u8], max: u32) -> Option<u32> {
    digits.iter().try_fold(0u32, |value, &b| {
        value
            .checked_mul(10)?
            .checked_add(u32::from(b - b'0'))
            .filter(|&v| v <= max)
    })
}
