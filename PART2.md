# Part 2: Replication

Consensus chooses the canonical block; the engine only executes the agreed
sequence deterministically. Speculative books are disposable.

## 1. Optimistic matches when the block arrives

Gossip changes only a speculative copy of the committed book. When the
block arrives, the node discards the copy and applies the block's commands
to the committed book in order. It replays commands, not provisional fills;
commands it missed through gossip come from the block. Only these results
are final; provisional ones are corrected, never settled.

Included messages leave the pending set by stable message identity, not
local sequence position. The rest of the speculation is rebuilt on the new
committed state with fresh positions; some may now reject.

## 2. Cancel versus fill

Alice's cancel of her resting 5-lot sell races Bob's crossing buy. Node A
hears the cancel first, node B the buy; each applies both provisionally in
that order. The block decides:

- **Cancel first:** A shows 5 cancelled, B a 5-lot fill. Canonically Bob's
  buy finds nothing, and B withdraws its fill.
- **5-lot buy first:** same views. Canonically Alice is fully filled and
  the cancel rejects because the order is closed; A replaces its
  cancellation with the fill.
- **2-lot buy first:** A shows 5 cancelled; B shows 2 filled and 3
  cancelled. Canonically 2 trade and the cancel removes the other 3, so A
  corrects Alice to match B.

The commands do not commute; canonical order decides. Every node reaches
the same events and Alice sees one final outcome.

## 3. Detecting and repairing divergence

Nodes compare the engine's BLAKE3 full-state digest, plus a proposed
replication-layer hash of canonical events (not an engine feature), at the
same block and rules/encoding version, never at speculative tips. They
compare against a trusted reference such as the agreed block record or
validator attestations; a peer's hash alone does not show who is right.
Full state matters, not just price levels: owners, queue priority, the
sequence cursor and reserved IDs affect future behaviour.

On a mismatch, the node stops serving authoritative results (and voting, if
a validator). It preserves state and inputs as evidence, checks its software
and rules version, restores an authenticated known-good checkpoint, replays
canonical blocks and verifies the hashes before rejoining. It must not copy
another node's book or overwrite the evidence first. If replay still
disagrees, it stays out until the cause is fixed.

A test-only simulation covers these races, identity-based replay and
digest-detected corruption rebuilt from a baseline.
