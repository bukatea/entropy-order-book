//! A deterministic order book for one market.
//!
//! This slice provides the input layer: typed commands with bounded values
//! ([`domain`]) and a strict, line-at-a-time CSV parser ([`input`]).

pub mod domain;
pub mod input;
