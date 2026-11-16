# Part 2: Replication

I assume the canonical block is already agreed; choosing it is outside the
matching engine.

## 1. When the canonical block arrives

Deterministic replay needs the same starting state and command order.
Gossip gives neither, so I wouldn't apply the canonical block on top of
local speculation.

I'd keep a committed book and speculate on a copy. When the block arrives,
I'd discard the copy and replay the block's commands from committed state,
including anything missed through gossip. For this scope, replaying is easier
to check than undoing trades whose effects may have influenced later commands.
I'd show early fills as provisional because the block may change them.
Settlement would wait for the canonical result.

Arrival positions differ between nodes, so I'd remove included commands by
message ID and retry the rest with fresh sequence positions. A pending
cancel might now fail because the block already filled its order.

## 2. Cancel versus fill

Alice's five-lot sell rests in the book. Node A sees her cancel first;
node B sees Bob's crossing buy first. Each applies both in that local
order, so A reports a cancellation while B reports a fill.

- **Cancel first in the block:** Alice's order has gone before Bob can
  match it. B withdraws its fill notice.
- **Five-lot buy first:** Alice fills completely, leaving nothing to cancel.
  The cancel therefore rejects, and A corrects its earlier cancellation.
- **Two-lot buy first:** two of Alice's five lots trade, leaving three.
  The cancel removes those three. B already showed that result, while A
  corrects its earlier five-lot cancellation.

Both replay from the same committed book, so they report the same confirmed
result to Alice.

## 3. Detecting and repairing divergence

Visible depth isn't enough: owners, FIFO order, the sequence cursor and
reserved IDs affect the next command. I'd compare full-state BLAKE3 digests
against an agreed block commitment or validator attestations. They must refer
to the same block and rules/encoding version; otherwise legitimate differences
would look like faults. I'd also hash canonical events, since different fills
can leave the same final state. The engine provides the state digest; the
event hash would belong to replication.

A mismatch proves disagreement, not which peer is right. I'd stop the node
serving authoritative results and, if it is a validator, voting. I'd keep
its state and log so the fault can be diagnosed, check its version, then
restore an authenticated known-good checkpoint rather than blindly copy a
peer. It could rejoin only after replaying canonical blocks and matching
the expected hashes; otherwise it stays out for diagnosis.
