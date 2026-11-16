# entropy-order-book

A deterministic single-market limit order book in Rust: a library package `orderbook` and an `orderbook` command-line program. It replays a sequenced CSV command log (place, cancel, modify) with price-time priority, self-trade prevention and GTC/IOC/POST_ONLY orders, and writes a canonical event stream and a BLAKE3 digest of the final state. It is built so that the same log produces the same bytes on every run and build profile.

Part 2 (the written answers) is in [PART2.md](PART2.md).

## Quick start

```bash
cargo run --locked -- tests/fixtures/sample.csv
cargo run --locked -- --state tests/fixtures/sample.csv
```

`tests/fixtures/sample.csv`:

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

Event stream and digest (exit 0):

```
eob-events,1
1,accepted,a1,alice,buy,100.25,5,GTC
1,rested,a1,buy,100.25,5,1
2,accepted,b1,bob,sell,100.50,3,GTC
2,rested,b1,sell,100.50,3,2
3,accepted,b2,bob,sell,100.25,2,IOC
3,trade,b2,a1,100.25,2,0,3
4,rejected,place,b3,post_only_would_cross
5,modified,b1,100.40,2,lost,3
5,rested,b1,sell,100.40,2,3
6,accepted,a2,alice,sell,100.25,1,GTC
6,cancelled,a1,3,self_trade
6,rested,a2,sell,100.25,1,4
7,rejected,cancel,a1,order_closed
digest,blake3,e4ed1025c63c5351284c7d064871d6ca94feb4c0539c5b8a97e5ab7152d77b10
```

With `--state` (exit 0):

```
eob-state,1
next_seq,8
halted,none
priority_clock,4
bids,0
asks,2
order,a2,alice,sell,10025,1,GTC,4
order,b1,bob,sell,10040,2,GTC,3
ids,5
id,a1
id,a2
id,b1
id,b2
id,b3
```

## Build and test

```bash
cargo test --locked
cargo test --release --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
```

The toolchain is pinned to Rust 1.99.1 in `rust-toolchain.toml` and `Cargo.lock` is committed.

Run one test file with `cargo test --locked --test <name>`:

- `domain`, `input`, `engine`, `canonical`: hand-written rule scenarios, byte goldens and digest fixtures.
- `cli`: runs the compiled binary in separate processes against the committed goldens.
- `model`: `proptest` histories checked against a slow, independent reference model in `tests/model/reference.rs`. Events and complete state are compared after every line, with book, priority and quantity-conservation invariants. Failing cases shrink, and proptest saves them to `tests/model.proptest-regressions`.
- `replication`: a test-only simulation of nodes with provisional gossip views. Each node replays canonical blocks from a committed baseline, and a node with corrupted state is caught by a trusted digest and rebuilt from a known-good baseline.

Tests compare full state (owners, priority ranks, time in force, cursor and reserved IDs), not just visible depth.

## Input format

- Header is exactly `seq,cmd,id,owner,side,price,size,tif` (LF or CRLF; no BOM, trimming or reordering). Empty input or any other header is terminal.
- Each later physical line is one command; the final line may lack a newline. A quoted field cannot span lines, so no field can contain CR or LF.
- Standard double-quote CSV quoting (doubled `""` for a quote); no backslash escapes. Malformed quoting is rejected, not repaired.
- `seq`: unsigned decimal digits fitting u64. A blank line or an unrecoverable/invalid seq is terminal; otherwise a bad row is rejected with its seq.
- `place` needs every field; `cancel` needs only `id`; `modify` needs `id`, `price`, `size`. Unused fields must be empty.
- `id`/`owner`: 1–32 ASCII bytes, kept exactly (no trimming or case folding). Any ASCII byte is allowed, including spaces, controls, NUL and DEL, except CR and LF as above.
- `price`: digits with optional `.digits`, greater than 0 and at most 1000000.00 (whole cents 1 to 100,000,000), tick 0.01. Fractional digits beyond cents that are all zero are accepted exactly (`1.000` is 1.00), never rounded; any nonzero extra digit rejects as `invalid_price_off_tick`.
- `size`: digits, 1 to 1,000,000,000. `side`: `buy`/`sell`. `tif`: `GTC`/`IOC`/`POST_ONLY`. Keywords are case-sensitive.
- Prices are integer cents and quantities integer lots throughout; floats are never used.
- When a row has several problems, the first in this order is reported: field quoting, field count, command keyword, then id, owner, side, price, size, tif from left to right.

## Rules

- Sequence: the expected sequence number is checked before anything else in the line. A gap (`seq_gap`), a lower number such as a duplicate, zero or reversal (`seq_stale`), an unrecoverable seq or a header failure halts without applying the line; no later line is applied. A valid expected seq is consumed even when the row is rejected. After `u64::MAX` the cursor is `exhausted` and any further line halts.
- Invalid input: a row with the expected seq that fails parsing is rejected with command `input`, an empty ID and a parser reason (for example `invalid_price_off_tick`). It consumes its seq and reserves nothing.
- IDs: a place whose fields are all valid reserves its ID before business checks, even when it is rejected as a crossing POST_ONLY. A malformed place reserves nothing. Reusing a reserved ID rejects as `duplicate_id` and leaves any live original alone. Cancel and modify reject an ID never reserved as `unknown_order` and a reserved ID with no live order as `order_closed`.
- A rejection can still change full state (the cursor, a reserved ID) while the live book stays the same.
- Matching: best price first, then oldest rank. Each fill is the min of the two remainders at the maker's price. A partial maker keeps its rank; a filled maker leaves the book and the ID index but its ID stays reserved.
- Time in force: GTC rests its remainder, IOC cancels it (`ioc_remainder`; a zero remainder emits nothing), and POST_ONLY rejects as a whole (`post_only_would_cross`) if it would cross the best opposite order, checked before self-trade prevention.
- Self-trade prevention: an incoming order that meets a resting order of the same owner cancels that resting order (`self_trade`) and keeps matching. Earlier fills stand; there are no self-fills.
- Modify sets the new remaining size. Same price and no size increase keeps the rank (an exact no-op is acknowledged too) and emits only `modified`. A price change or size increase issues one new rank before matching, then the order re-enters under its original time in force; a remainder rests with that same rank. A crossing POST_ONLY modify rejects with the original order, rank and clock unchanged.
- Ranks: the priority clock advances once when a new order rests and once per priority-losing modify, never for rejections, fills or priority-keeping modifies.
- Events per line: an accepted place is acknowledged (`accepted`) before its trades and cancellations, then `rested` or an IOC `cancelled`. A rejected place emits only `rejected`. A modify's `modified` comes before its matching events. A cancel's `cancelled` (`user`) is its acknowledgment. Parser rejections use command `input` with no ID; business rejections name their command and ID. Halts report the expected cursor, the reason and the observed seq when one was recovered.

## Canonical encoding and digest (version 1)

Byte rules shared by events and state:

- ASCII, comma-separated, fixed field order; LF after every record, including the last; no added whitespace; not CSV-quoted.
- Integers are shortest unsigned decimal.
- Event prices are exactly two decimals from integer cents (`100.25`); state prices are integer cents (`10025`).
- Enum spellings are case-sensitive.
- ID/owner escaping: printable 0x20–0x7E is unchanged except comma, quote and backslash, so a space stays literal (owner `o w` is written `o w`). Those three, 0x00–0x1F and 0x7F are written `\xHH` with uppercase hex. Backslash is always escaped so output is unambiguous.
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

- Rejection reasons, business: `duplicate_id`, `unknown_order`, `order_closed`, `post_only_would_cross`.
- Rejection reasons, parser: `malformed_csv`, `field_count`, `unknown_command`, `unexpected_<field>`, `invalid_<field>_<value>` with fields `id`, `owner`, `side`, `price`, `size`, `tif` and values `empty`, `syntax`, `non_ascii`, `too_long`, `off_tick`, `out_of_range`.
- Halt reasons: `header_empty`, `header_mismatch`, `seq_blank`, `seq_malformed`, `seq_empty`, `seq_syntax`, `seq_overflow`, `seq_gap`, `seq_stale`, `seq_exhausted`. A header halt has expected 1 and observed `none`; an unparseable seq has observed `none`.

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

- Each count line is followed by exactly that many records, zero or more: `bids,<count>` and `asks,<count>` by `<count>` `order` lines, and `ids,<count>` by `<count>` `id` lines. The sample above has `bids,0` directly followed by `asks,2`.
- Bids and asks are in execution priority. Reserved IDs (including closed and business-rejected places) are sorted by original bytes, not escaped text. Lookup indexes are excluded.
- Digest: unkeyed BLAKE3, 32 bytes, 64 lowercase hex, of exactly these state bytes. It commits to state, not event history, so a rejected place that reserves an ID changes the digest while the book looks unchanged.

## Command line

- `orderbook [--state] <log.csv>`. `--help` shows usage; `--` ends options before a dash-prefixed filename.
- Normal stdout: the event stream, then `digest,blake3,<64 hex>` and LF. The trailer is not an event and not state.
- `--state`: stdout is only the canonical state bytes, with no events and no digest trailer.
- Diagnostics and usage errors go to stderr.
- Exit codes: 0 success (rejected rows are ordinary); 1 the log halted (normal mode still prints prefix events, the halt record and the digest; state mode prints the halted state; reading stops at the halt); 2 usage error; 3 I/O failure (no trailer guaranteed).
- The program never opens an output file or writes its input. Shell redirection is the caller's; `> same-file` truncates before the program starts.

## Engine API

- `engine::Engine::new()` starts with next sequence 1, running, priority clock 0, empty books and no reserved IDs. It derives `Clone` and `Eq`.
- `Engine::apply(Result<ParsedLine, SeqError>)` returns the line's typed `event::Event`s. `Engine::reject_header(HeaderError)` records a header failure. Both return `Err(AlreadyHalted)` once the engine has halted, with no events and no state change.
- `engine::replay(input)` parses a whole input with `input::commands` and stops at the first halt.
- Read-only views: `cursor()`, `halt()`, `priority_clock()`, `bids()` and `asks()` in execution priority, `order(id)` for a live order, `live_count()`, and `reserved_ids()` in byte order. There are no setters.
- `Engine::write_state(&mut impl Write)` writes canonical state.
- `Engine::digest()` returns a `canonical::Digest` (32 bytes, `Display` is 64 lowercase hex), computed by streaming the same state encoder into an unkeyed BLAKE3 hasher.
- `canonical::write_events_header`, `write_event` and `write_digest_trailer` write to any `std::io::Write` and propagate its errors.

## Design

- Modules: `domain` (bounded value types), `input` (strict line framing and parsing), `engine` (state and matching), `event` (typed events), `canonical` (bytes and digest), `main` (CLI).
- Each side is a `BTreeMap` keyed by price and rank (bids use `Reverse(price)`, so the first entry is the highest price and oldest rank), plus an ID index of live orders. Cancel and modify are O(log n); a command that touches k makers is O(k log n). No price level is ever scanned and removed orders leave no tombstones.
- The engine is the only owner of state and changes only through `apply` and `reject_header`; the parser keeps no state between lines.
- Dependencies: exactly two runtime crates, `csv` (quote-aware field splitting) and `blake3` (digest); `proptest` is test-only. The `csv` reader silently strips a leading UTF-8 BOM, so `src/input.rs` never lets it decode an unquoted first field, and a header with a BOM is terminal.

## Determinism evidence

- `tests/cli.rs` runs the compiled binary in separate processes against committed byte goldens (`tests/fixtures/sample.events`, `sample.state`, `sample.blake3`).
- `tests/canonical.rs` has fixed byte and digest goldens, including `empty.state` and `empty.blake3`.
- The sample digest was computed from the literal state bytes by an independent BLAKE3 implementation checked against the 35 official test vectors, and was also checked with the official `b3sum`. The sample events are hand-derived.
- The model test's reference engine (`tests/model/reference.rs`) keeps live orders in one unsorted `Vec` and finds the best maker by linear scan. It shares only vocabulary with production: the domain types, the parser's `ParsedLine`, the typed `Event` enums, and the shared `canonical::write_event` for comparison. It never uses the engine's matching, index, rank or sequence code, and reads `Engine` only through its public views. Because both event streams go through the same encoder, encoder correctness is checked separately by the hand-derived byte goldens in `tests/canonical.rs`.
- The suite passes in debug and release (`cargo test --locked`, `cargo test --release --locked`), and the model agrees with the engine on generated histories. All of this ran locally on Linux x86-64 only.
- `.github/workflows/ci.yml` defines the same formatting, Clippy and debug and release test checks for `ubuntu-24.04` (x86-64) and `macos-15` (arm64), triggered only by a pull request or manual dispatch. It has not been run, so no macOS or cross-machine result is claimed.

## AI tools

Claude and GPT models assisted extensively with implementation, tests, review and documentation, and the checks recorded here were run by those tools. Concrete examples:

- A review found that the `csv` library silently normalizes a leading BOM; the parser was changed so it cannot reach that path.
- The exact sample state bytes and hash were independently checked with the official `b3sum`.
- Separate review passes checked sequence handling, FIFO priority, self-trade prevention, modify atomicity, encoding and failing writes.

## What I'd do next

- Run the same goldens on actual macOS arm64 hardware.
- Profile before optimizing, and set measured limits on event-log and ID-history memory for untrusted, long-running use.
- For production replication, agree on finality, authenticated checkpoints and durable logs; cloning `Engine` is not enough.
