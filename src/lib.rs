//! A deterministic order book for one market.
//!
//! Typed commands with bounded values ([`domain`]) come from a strict,
//! line-at-a-time CSV parser ([`input`]). The [`engine`] checks sequence
//! numbers and IDs, matches orders and reports typed [`event`]s. Parsing and
//! matching stay separate: the engine consumes the parser's results.

pub mod canonical;
pub mod domain;
pub mod engine;
pub mod event;
pub mod input;
