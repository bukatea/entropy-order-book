# entropy-order-book

A deterministic single-market order book in Rust (library package `orderbook`). Currently only the input layer exists: typed `place`/`cancel`/`modify` commands with bounded values (`src/domain.rs`) and a strict CSV parser that works one physical line at a time (`src/input.rs`). Matching, event encoding, state digest and the CLI are not implemented yet.

## Build and test

```bash
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
```

The toolchain is pinned to Rust 1.99.1 in `rust-toolchain.toml` and `Cargo.lock` is committed.

## Input format

- Header is exactly `seq,cmd,id,owner,side,price,size,tif` (LF or CRLF; no BOM, trimming or reordering). Empty input or any other header is terminal.
- Each later physical line is one command; the final line may lack a newline. A quoted field cannot span lines.
- Standard double-quote CSV quoting (doubled `""` for a quote); no backslash escapes. Malformed quoting is rejected, not repaired.
- `seq`: unsigned decimal digits fitting u64. A blank line or an unrecoverable/invalid seq is terminal; otherwise a bad row is rejected with its seq.
- `place` needs every field; `cancel` needs only `id`; `modify` needs `id`, `price`, `size`. Unused fields must be empty.
- `id`/`owner`: 1–32 ASCII bytes, kept exactly. `price`: digits with optional `.digits`, whole cents 1 to 100,000,000 (extra fractional digits must be zero). `size`: digits, 1 to 1,000,000,000. `side`: `buy`/`sell`. `tif`: `GTC`/`IOC`/`POST_ONLY`. Keywords are case-sensitive.
- When a row has several problems, the first in this order is reported: field quoting, field count, command keyword, then id, owner, side, price, size, tif from left to right.
