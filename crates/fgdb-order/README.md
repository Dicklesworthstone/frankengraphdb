# Aegis ordering kernel

Workstream: `fgdb-w11-raft-multi-aqw`; plan §14.1.

This is the deterministic transition layer of `fgdb-order`, not a second
storage engine. It implements fixed-configuration Raft elections, bounded
AppendEntries replication, conflicting-suffix reconciliation, learner
replication, current-term quorum commitment and committed-prefix replay.
Quorum one uses the same implementation. Commands should be bounded canonical
command references, not unbounded payload byte vectors.

## Integration contract

1. Authenticate the peer, complete consensus domain, and configuration identity
   before constructing an envelope. `step` also checks the identities against
   the installed configuration. Resolve and validate the exact payload closure
   and availability certificate before admitting a proposal or append.
2. Call `step`. When `Persistence::requires_write()` is true, encode and publish
   its complete state through the Appendix A Chronicle root closure and complete
   the required sync barriers under the replication capability context.
3. Only then call `persisted(id)`. Its output contains outbound messages and
   newly committed entries. A false `requires_write` means the existing durable
   root already covers the transition; it does not require another fsync.
4. Apply committed entries in index order, with a durable application cursor.
   No-op entries carry `None` and never advance semantic sequence counters.
5. On unknown or failed publication, call `publication_failed` and recover under
   an exclusive writer fence. Never report abort merely because a response was
   lost. A dropped persistence view leaves the machine blocked. Tokens cannot
   cross nodes, generations, or recovered machine incarnations.

The deterministic test driver models published disk state separately and checks
committed-prefix agreement after every delivered input. It exercises one,
three and five voters, learners, partitions, leader replacement, stale and
malformed messages, duplicate votes and replies, failed/cancelled publication,
recovery, bounded append batches, and seeded loss/reordering/duplication.

## Deliberate capability boundary

The kernel alone does not enable database clustering. Exact Appendix A durable
codecs/root publication, signed payload certificates, the asupersync runtime and
transport driver, snapshot/retention-floor installation, joint reconfiguration,
linearizable read/audit visibility contracts and database apply integration are
not supplied by this increment. No alternate on-disk or wire format is minted.
The full W11 bead remains open until its integration and fault gates pass.

Verification at authoring: Rust tests, rustfmt, clippy, UBS and the full local
proof gate were **UNRUN**, because the editing environment had no Rust toolchain
or repository checkout and could not reach the dependency hosts. This note is
not a passing verdict. Run `cargo test -p fgdb-order`, then the repository's
normal exact-tree proof workflow in the configured build environment.
