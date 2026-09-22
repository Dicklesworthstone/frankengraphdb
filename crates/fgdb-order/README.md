# Aegis ordering kernel

Workstream: `fgdb-w11-raft-multi-aqw`; plan §14.1.

This is the deterministic transition layer of `fgdb-order`, not a second
storage engine. It implements stable and joint-configuration elections,
bounded AppendEntries replication, conflicting-suffix reconciliation, learner
replication, current-term quorum commitment, snapshot catch-up, bounded log
compaction and absolute-index committed-prefix replay. Quorum one uses the
same implementation. Commands should be bounded canonical references, not
unbounded payload byte vectors.

Joint configurations keep old and new voter sets separately. Elections require
both majorities, and commitment uses the lower of their independent majority
match indexes, still subject to the current-term rule. The union is only the
routing/candidate universe. Learners never count toward either majority.
This supports a fixed authenticated joint configuration, not the live ordered
membership-transition/payload-floor/retirement ladder.

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

An `InstallSnapshot` message is only an offer. Its persisted output requests an
exact `SnapshotTransfer`; the runtime must acquire, authenticate and durably own
that complete closure before submitting its `SnapshotReady` token. Publication
of the resulting persistence view must atomically install the application cut
and Raft state before releasing a successful response. Chronicle's manifest-
bound `ReplicaSeed::begin_pull` joins bonded object recovery to the object/root
publication gates, but wiring those gates to the Raft runtime remains explicit.

`Compact` requires the generated state-at-cut proof, locally applied/audit-visible
position and complete retention floor. A committed index is not enough. Recovery
uses the installed snapshot plus the retained suffix; replay below that cut
returns `SnapshotRequired`, never incomplete history. Neither snapshots nor
compaction grant membership, read access or permission to retire obligations.

The deterministic tests exercise ordinary and joint groups, partitions, stale
votes/replies, learner exclusion, donor/installation failure boundaries,
snapshot transfer cancellation, absolute-index bounds and crash/restart. The
private quorum tests enumerate 30,752 vote subsets and 18,225 match vectors
against independent counting oracles. Snapshot and joint integration drivers
check publication before output and committed-prefix agreement.

## Deliberate capability boundary

The kernel alone does not enable database clustering. Exact Appendix A durable
codecs/root publication, signed payload certificates, the authenticated
asupersync ATP/Raft runtime driver, live membership changes, linearizable
read/audit visibility contracts and database apply integration remain required.
No alternate on-disk or wire format is minted. The full W11 bead remains open
until its integration and fault gates pass.

Verification at authoring: the independent Python quorum arithmetic model
passed 48,977 cases; this is not execution of the Rust implementation. Rust
compilation/tests, rustfmt, clippy, UBS and the full local proof gate were
**UNRUN**, because this editing environment has no Rust toolchain or complete
checkout and cannot reach dependency hosts. Run `cargo test -p fgdb-order`,
then the repository's normal exact-tree proof workflow in the configured build
environment. Chronicle's bonded-seed tests also require its normal crate tests.
