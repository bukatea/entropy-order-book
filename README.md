# entropy-order-book

A deterministic single-market limit order book in Rust, as the `orderbook` library and command-line program. It replays a sequenced CSV log of place, cancel and modify commands with price-time priority, self-trade prevention and GTC/IOC/POST_ONLY orders, and writes a canonical event stream and a BLAKE3 digest of the final state. The same log gives the same bytes on every run and build profile.

Part 2 is in [PART2.md](PART2.md).

## Quick start

```bash
cargo run --locked -- tests/fixtures/sample.csv          # events, then the digest
cargo run --locked -- --state tests/fixtures/sample.csv  # final state only
```

The sample log, [`tests/fixtures/sample.csv`](tests/fixtures/sample.csv), exercises a partial fill, an IOC, a rejected POST_ONLY, a priority-losing modify, self-trade prevention and a cancel of a closed order:

```
seq,cmd,id,owner,side,price,size,tif
1,place,a1,alice,buy,100.25,5,GTC
2,place,b1,bob,sell,100.50,3,GTC
3,place,b2,bob,sell,100.25,2,IOC
4,place,b3,carol,sell,100.25,1,POST_ONLY
5,modify,b1,,,100.40,2,
6,place,a2,alice,sell,100.25,1,GTC
7,cancel,a1,,,,,
```

It exits 0. The final book has no bids and two asks, a2 (alice, 1 lot at 100.25) and b1 (bob, 2 lots at 100.40), and all five IDs stay reserved. The output ends with:

```
digest,blake3,e4ed1025c63c5351284c7d064871d6ca94feb4c0539c5b8a97e5ab7152d77b10
```

The full expected output is in [`sample.events`](tests/fixtures/sample.events), [`sample.state`](tests/fixtures/sample.state) and [`sample.blake3`](tests/fixtures/sample.blake3).

## Build and test

```bash
cargo test --locked
cargo test --release --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
```

`rust-toolchain.toml` pins Rust 1.99.1 and `Cargo.lock` is committed. Run one test file with `cargo test --locked --test <name>`:

- `domain`, `input`, `engine`, `canonical`: hand-written rule scenarios, byte goldens and digest fixtures.
- `cli`: the compiled binary, run in separate processes against the committed goldens.
- `model`: `proptest` histories compared, events and full state after every line, with a slow independent reference model (`tests/model/reference.rs`), plus book, priority and quantity-conservation invariants. Shrunk failures go to `tests/model.proptest-regressions`.
- `replication`: simulated nodes with provisional gossip views replay canonical blocks from a committed baseline; a corrupted node is caught by a trusted digest and rebuilt.

## Input format

- The header is exactly `seq,cmd,id,owner,side,price,size,tif`, with LF or CRLF and no BOM, trimming or reordering. Empty input or any other header is terminal.
- Each later physical line is one command; the last may lack a newline. A quoted field cannot span lines, so no field contains CR or LF.
- Standard double-quote CSV quoting (`""` for a quote), no backslash escapes. Malformed quoting is rejected, not repaired.
- `seq` is decimal digits fitting a u64. A blank line or an unrecoverable or invalid seq is terminal; any other bad row is rejected with its seq.
- `place` needs every field, `cancel` only `id`, and `modify` `id`, `price` and `size`. Unused fields must be empty.
- `id` and `owner` are 1–32 ASCII bytes, kept exactly, with no trimming or case folding. Any ASCII byte except CR and LF is allowed, including spaces, controls, NUL and DEL.
- `price` is digits with optional `.digits`, above 0 and at most 1000000.00 (1 to 100,000,000 cents), tick 0.01. Extra fractional zeros are accepted exactly (`1.000` is 1.00), never rounded; any nonzero digit beyond cents rejects as `invalid_price_off_tick`.
- `size` is digits, 1 to 1,000,000,000. `side` is `buy` or `sell`; `tif` is `GTC`, `IOC` or `POST_ONLY`. Keywords are case-sensitive.
- Prices are integer cents and quantities integer lots throughout. No floats.
- A row with several problems reports the first in this order: quoting, field count, command keyword, then `id`, `owner`, `side`, `price`, `size`, `tif`.

## Rules

- **Sequence.** The expected seq is checked first. A gap (`seq_gap`), a lower number such as a duplicate, zero or reversal (`seq_stale`), an unrecoverable seq or a header failure halts without applying the line or any later line. A valid expected seq is consumed even when the row is rejected. After `u64::MAX` the cursor is `exhausted` and any further line halts.
- **Invalid rows.** A row with the expected seq that fails parsing is rejected with command `input`, an empty ID and a parser reason such as `invalid_price_off_tick`. It consumes its seq and reserves nothing.
- **IDs.** A place with all fields valid reserves its ID before business checks, even if it is then rejected as a crossing POST_ONLY. A malformed place reserves nothing. Reusing a reserved ID rejects as `duplicate_id` and leaves any live original alone. Cancel and modify reject a never-reserved ID as `unknown_order` and a reserved ID with no live order as `order_closed`.
- **Matching.** Best price, then oldest rank. Each fill is the smaller of the two remaining quantities, at the maker's price. A partially filled maker keeps its rank. A filled maker leaves the book and the ID index; its ID stays reserved.
- **Time in force.** GTC rests its remainder. IOC cancels it (`ioc_remainder`; a zero remainder emits nothing). POST_ONLY rejects as a whole (`post_only_would_cross`) if it would cross the best opposite order, checked before self-trade prevention.
- **Self-trade prevention.** An incoming order that meets a resting order of the same owner cancels that resting order (`self_trade`) and keeps matching. Earlier fills stand; there are no self-fills.
- **Modify** sets the new remaining size. Same price without a size increase keeps the rank and emits only `modified`, including an exact no-op. A price change or size increase takes one new rank, then the order re-enters under its original time in force and any remainder rests with that rank. A crossing POST_ONLY modify rejects with the original order, rank and clock unchanged.
- **Ranks.** The priority clock advances once when a new order rests and once per priority-losing modify, never for rejections, fills or priority-keeping modifies.
- **Event order.** An accepted place emits `accepted`, its trades and cancellations, then `rested` or an IOC `cancelled`. A rejected place emits only `rejected`. A modify emits `modified` before its matching events. A cancel's `cancelled` (`user`) is its acknowledgment. Parser rejections use command `input` and no ID; business rejections name their command and ID. A halt reports the expected cursor, the reason and the observed seq if one was recovered.

## Canonical encoding and digest (version 1)

Shared byte rules:

- ASCII, comma-separated, fixed field order, LF after every record including the last. No added whitespace or CSV quoting.
- Integers are shortest unsigned decimal. Event prices have two decimals from integer cents (`100.25`); state prices are integer cents (`10025`). Enum spellings are case-sensitive.
- IDs and owners: printable 0x20–0x7E is unchanged except comma, quote and backslash, so a space stays literal (owner `o w` is written `o w`). Those three, 0x00–0x1F and 0x7F become `\xHH` with uppercase hex. Escaping backslash keeps the output unambiguous.
- An empty ID field (parser rejections) differs from every valid ID.

Event layouts:

```
eob-events,1
<seq>,accepted,<id>,<owner>,<buy|sell>,<price>,<size>,<GTC|IOC|POST_ONLY>
<seq>,rejected,<input|place|cancel|modify>,<id-or-empty>,<reason>
<seq>,trade,<taker-id>,<maker-id>,<price>,<quantity>,<taker-left>,<maker-left>
<seq>,rested,<id>,<buy|sell>,<price>,<remaining>,<rank>
<seq>,cancelled,<id>,<cancelled-remaining>,<user|ioc_remainder|self_trade>
<seq>,modified,<id>,<price>,<new-remaining>,<kept|lost>,<rank>
<expected-seq-or-exhausted>,halted,<reason>,<observed-seq-or-none>
```

- Business rejections: `duplicate_id`, `unknown_order`, `order_closed`, `post_only_would_cross`.
- Parser rejections: `malformed_csv`, `field_count`, `unknown_command`, `unexpected_<field>`, `invalid_<field>_<value>`, with fields `id`, `owner`, `side`, `price`, `size`, `tif` and values `empty`, `syntax`, `non_ascii`, `too_long`, `off_tick`, `out_of_range`.
- Halts: `header_empty`, `header_mismatch`, `seq_blank`, `seq_malformed`, `seq_empty`, `seq_syntax`, `seq_overflow`, `seq_gap`, `seq_stale`, `seq_exhausted`. A header halt has expected 1 and observed `none`; an unparseable seq has observed `none`.

State layout:

```
eob-state,1
next_seq,<number-or-exhausted>
halted,<none-or-halt-reason>
priority_clock,<last-issued-rank>
bids,<count>
order,<id>,<owner>,buy,<cents>,<remaining>,<tif>,<rank>
asks,<count>
order,<id>,<owner>,sell,<cents>,<remaining>,<tif>,<rank>
ids,<count>
id,<id>
```

- Each count line is followed by exactly that many `order` or `id` records, possibly none, so an empty bid side is `bids,0` directly followed by `asks,<count>`.
- Bids and asks are in execution priority. Reserved IDs, including closed orders and business-rejected places, are sorted by original bytes, not escaped text. Lookup indexes are excluded.
- The digest is unkeyed BLAKE3, 32 bytes as 64 lowercase hex, of exactly these state bytes. It commits to state, not event history, so a rejected place that reserves an ID changes the digest while the book looks unchanged.

## Command line

- `orderbook [--state] <log.csv>`. `--help` shows usage; `--` ends options before a filename starting with a dash.
- By default stdout is the event stream, then `digest,blake3,<64 hex>` and LF. The trailer is neither an event nor state.
- With `--state`, stdout is only the canonical state bytes: no events, no trailer.
- Diagnostics and usage errors go to stderr.
- Exit codes: 0 success (rejected rows are ordinary); 1 the log halted; 2 usage error; 3 I/O failure (no trailer guaranteed). On a halt, reading stops; normal mode still prints the earlier events, the halt record and the digest, and state mode prints the halted state.
- The program never opens an output file or writes its input; a shell `> same-file` truncates the log before the program starts.

## Library

```rust
let input = std::fs::read("tests/fixtures/sample.csv")?;
let (engine, events) = orderbook::engine::replay(&input);
println!("{} events, digest {}", events.len(), engine.digest());
```

`replay` stops at the first halt. `Engine::apply` takes one line at a time and, after a halt, returns `Err(AlreadyHalted)` without changes. The engine has read-only views and no setters; `canonical` writes the bytes. See `cargo doc --open` for the full API.

## Design

- Modules: `domain` (bounded value types), `input` (strict framing and parsing, no state between lines), `engine` (state and matching, changed only through `apply` and `reject_header`), `event` (typed events), `canonical` (bytes and digest), `main` (CLI).
- Each side is a `BTreeMap` keyed by price and rank, plus an ID index of live orders. Bids use `Reverse(price)`, so the first entry is the best price and oldest rank. Cancel and modify are O(log n), and a command touching k makers is O(k log n). No price level is scanned and removed orders leave no tombstones.
- Runtime dependencies are `csv` (quote-aware splitting) and `blake3`; `proptest` is test-only. The `csv` reader silently strips a leading UTF-8 BOM, so `src/input.rs` never lets it decode an unquoted first field and a BOM header stays terminal.

## Determinism evidence

- `tests/cli.rs` and `tests/canonical.rs` check the binary and the encoder against committed byte and digest goldens. The sample events were derived by hand. The sample digest was computed from the literal state bytes by an independent BLAKE3 implementation, checked against the 35 official test vectors, and confirmed with `b3sum`.
- The model's reference engine keeps live orders in an unsorted `Vec` and finds makers by linear scan. It shares types, `ParsedLine`, `Event` and `canonical::write_event` with production but no matching, index, rank or sequence code, and reads `Engine` only through public views. The hand-derived goldens check the shared encoder.
- CI (`.github/workflows/ci.yml`) runs formatting, Clippy and debug and release tests on `ubuntu-24.04` (x86-64) and `macos-15` (arm64). [Run 43852067486](https://github.com/bukatea/entropy-order-book/actions/runs/43852067486) passed at commit `1579077` with Rust 1.99.1: all 98 tests in both profiles on both machines, against the same committed goldens.

## AI tools

Claude and GPT models assisted extensively with implementation, tests, review and documentation, and those tools ran the checks described here. For example, a review found that the `csv` library silently strips a leading BOM, and the parser was changed so it cannot reach that path. The sample state bytes and digest were checked with the official `b3sum`. Separate review passes covered sequence handling, FIFO priority, self-trade prevention, modify atomicity, encoding and failing writes.

## What I'd do next

- Profile before optimizing, and set measured limits on event-log and ID-history memory for long-running use with untrusted input.
- For production replication, agree on finality, authenticated checkpoints and durable logs. Cloning `Engine` is not enough.
