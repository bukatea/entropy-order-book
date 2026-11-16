# Part 2: Replication

Assume the nodes agree on which block is canonical. Keep the last committed
book separate from the speculative book built from gossip.

## 1. When the canonical block arrives

Discard local speculation and replay the block's commands, in order, from
the committed book. The block supplies any commands missed through gossip.
Replace provisional trade and cancellation notifications with these results;
never settle a provisional fill.

Remove included commands from the pending set by message ID, not local
sequence position. Replay the remaining commands on the new committed state
with fresh positions. Some may now reject. Gossip arrival positions are not
canonical sequence numbers.

## 2. Cancel versus fill

Alice has a resting sell for five lots. Node A sees her cancel before Bob's
crossing buy; node B sees the buy first. Each applies both commands locally,
so Alice can see different provisional results.

- **Cancel first in the block:** five lots cancel. Bob cannot fill Alice's
  order; B withdraws its provisional fill.
- **Five-lot buy first:** all five lots fill, then the cancel rejects because
  the order is closed. A replaces its provisional cancellation with the fill.
- **Two-lot buy first:** two lots fill and the cancel removes the three left.
  B already shows that partial fill and cancellation; A corrects its earlier
  five-lot cancellation.

Both nodes replay commands, not fill events. Alice gets one final outcome.

## 3. Detecting and repairing divergence

At each canonical block, compare the full-state digest and a canonical-event
hash against a trusted commitment in the agreed block record or validator
attestations. Bind the comparison to the block identity and rules/encoding
version. The engine supplies the state digest; the event hash belongs to the
proposed replication layer.

Compare committed states, not speculative tips. Owners, FIFO order, the
sequence cursor and reserved IDs matter even when visible depth matches.
A peer's hash alone cannot tell which node is right.

On a mismatch, stop serving authoritative results and, if applicable, voting.
Preserve the state and log for diagnosis. Check the software version, restore
an authenticated known-good checkpoint and replay canonical blocks. Rejoin
only after the hashes match. If replay still disagrees, keep the node out of
service until the cause is fixed.
