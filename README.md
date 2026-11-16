# entropy-order-book

A deterministic single-market order book in Rust (library package `orderbook`). The input layer (`src/domain.rs`, `src/input.rs`) and the matching engine with typed events (`src/engine.rs`, `src/event.rs`) exist. Canonical event bytes, the state digest and the CLI are not implemented yet.

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

## Engine API

- `engine::Engine::new()` starts with next sequence 1, running, priority clock 0, empty books and no reserved IDs. It derives `Clone` and `Eq`.
- `Engine::apply(Result<ParsedLine, SeqError>)` returns the line's typed `event::Event`s. `Engine::reject_header(HeaderError)` records a header failure. Both return `Err(AlreadyHalted)` once the engine has halted, with no events and no state change.
- `engine::replay(input)` parses a whole input with `input::commands` and stops at the first halt.
- Read-only views: `cursor()`, `halt()`, `priority_clock()`, `bids()` and `asks()` in execution priority, `order(id)` for a live order, `live_count()`, and `reserved_ids()` in byte order. There are no setters.
- Each side is a `BTreeMap` keyed by price and rank (bids use `Reverse(price)`, so the first entry is the highest price and oldest rank), plus an ID index of live orders. Cancel and modify are O(log n); a command that touches k makers is O(k log n). Unlike a price-level FIFO queue, no level is ever scanned and removed orders leave no tombstones.

## Rules

- Sequence: the expected sequence number is checked before anything else in the line. A gap (`Gap`), a lower number such as a duplicate, zero or reversal (`Stale`), an unrecoverable seq or a header failure halts without applying the line; no later line is applied. A valid expected seq is consumed even when the row is rejected. After `u64::MAX` the cursor is `Exhausted` and any further line halts.
- IDs: a place whose fields are all valid reserves its ID before business checks, even when it is rejected as a crossing POST_ONLY. A malformed place reserves nothing. Reusing a reserved ID rejects as `DuplicateId` and leaves any live original alone. Cancel and modify reject an ID never reserved as `UnknownId` and a reserved ID with no live order as `ClosedId`.
- A rejection can still change full state (the cursor, a reserved ID) while the live book stays the same.
- Matching: best price first, then oldest rank. Each fill is the min of the two remainders at the maker's price. A partial maker keeps its rank; a filled maker leaves the book and the ID index but its ID stays reserved.
- Time in force: GTC rests its remainder, IOC cancels it (`ioc_remainder`; a zero remainder emits nothing), and POST_ONLY rejects as a whole (`PostOnlyWouldCross`) if it would cross the best opposite order, checked before self-trade prevention.
- Self-trade prevention: an incoming order that meets a resting order of the same owner cancels that resting order (`self_trade`) and keeps matching. Earlier fills stand; there are no self-fills.
- Modify sets the new remaining size. Same price and no size increase keeps the rank (an exact no-op is acknowledged too) and emits only `modified`. A price change or size increase issues one new rank before matching, then the order re-enters under its original time in force; a remainder rests with that same rank. A crossing POST_ONLY modify rejects with the original order, rank and clock unchanged.
- Ranks: the priority clock advances once when a new order rests and once per priority-losing modify, never for rejections, fills or priority-keeping modifies.
- Events per line: an accepted place is acknowledged (`accepted`) before its trades and cancellations, then `rested` or an IOC `cancelled`. A rejected place emits only `rejected`. A modify's `modified` comes before its matching events. A cancel's `cancelled` (`user`) is its acknowledgment. Parser rejections use command `Input` with no ID; business rejections name their command and ID. Halts report the expected cursor, the reason and the observed seq when one was recovered.
