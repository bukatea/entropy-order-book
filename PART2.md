# Part 2: Replication

Consensus chooses the canonical block; the matching engine does not
implement consensus, it only executes the agreed sequence deterministically.
Optimistic books and results are disposable.

## 1. Optimistic matches when the block arrives

Gossip changes only an optimistic copy of the committed book. When the
block arrives, the node discards the copy and applies the block's commands
to the committed book in order. It replays commands, not provisional fill
events; commands missed through gossip come from the block itself. Only
these results are final. Provisional results shown to traders are
corrected, never settled.

The node removes included messages from its pending set by stable
identity, not local position, then rebuilds the remaining speculation on
the new committed state with fresh sequence positions. Some may reject.

## 2. Cancel versus fill

Alice's cancel of her resting 5-lot sell races Bob's crossing buy. Node A
sees the cancel first and shows a provisional cancellation. Node B sees
Bob's buy first and shows a provisional 5-lot fill.

- **Cancel first:** the order is gone; Bob's buy cannot fill it.
  Node B withdraws its fill.
- **Buy first:** the fill stands; the later cancel rejects because the
  order is already closed. Node A withdraws its cancellation.
- **Partial buy (2 lots) first:** 2 lots trade, then the cancel removes the
  remaining 3. Node A adds the fill; node B shrinks its fill to 2.

All nodes reach the same events; Alice sees one final outcome. The
commands need not commute; canonical order decides.

## 3. Detecting and repairing divergence

Compare the engine's existing BLAKE3 full-state digest and a hash of
canonical events at the same block identity and rules/encoding version,
never at optimistic tips, against a trusted reference such as the agreed
block record or validator attestations. The event hash is a proposed
replication-layer check, not an engine feature. A peer's hash
alone does not show which node is right. Full state matters, not just price
levels: owners, queue priority, sequence cursor and remembered IDs affect
future behaviour.

On mismatch, the node stops serving authoritative results (and voting, if
a validator). It preserves state and inputs as evidence, checks its
software and rules version, restores an authenticated known-good
checkpoint, replays canonical blocks and verifies hashes before rejoining.
It must not copy another node's book or overwrite the evidence first. If
replay still disagrees, it stays out until the cause is fixed.

A test-only simulation exercises speculative views, identity-based replay,
the three race outcomes, and digest-detected corruption rebuilt from a
baseline.
