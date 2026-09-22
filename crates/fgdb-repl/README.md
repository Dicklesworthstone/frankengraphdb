# Aegis replica catch-up composition

This crate connects existing Chronicle bonded object recovery and exact-inventory
replica seeding to `fgdb-order`'s opaque snapshot-transfer capability. It targets
the snapshot offer / SnapshotReady API, not an older InstallSnapshot-only draft.
No snapshot implementation or concurrent upstream work is replaced.

## Driver contract

The ordinary Raft `InstallSnapshot` offer must first finish its required term
publication. Its released `Output::snapshot_transfers` supplies the exact
`SnapshotTransfer` capability. An offer is not an installation acknowledgement.

Authenticate the complete canonical snapshot closure and build the existing
Chronicle `SeedPlan`. Hold the destination writer fence, keep the target offline
or non-serving, and retain the source closure. Start `SnapshotCatchup` with this
node, its authenticated security namespace, the transfer capability and the plan.
The snapshot manifest, application state root, retention floor, domain,
configuration and exact Raft index/term must all agree.

Use Chronicle `BondedPull` through the production ATP driver to recover objects.
Feed only its opaque `VerifiedObject` into `stage`. Publish every full local
object/placement/ownership closure before acknowledging `object_published`.

After every object is durably owned, `begin_publication` evaluates SnapshotReady
and returns ONE `SnapshotPublication`: the seed application/retention view AND
the exact Raft persistent state. Encode them into the SAME canonical destination
root closure at the plan's ObjectId and generation. Do not separately activate
the seed application state and the Raft state. If the exact returned consensus
state cannot be included in that planned root, fail instead of rewriting the plan.

Complete the real Chronicle root sync/reread barriers. Only then call `published`
with the joint publication token and exact root evidence. Only that call releases
the snapshot acknowledgement and application-cut notification. Restore the cut
before replaying the subsequently committed suffix through the durable cursor.

Reacquiring a publication view retains the same state and tokens. Wrong root or
generation evidence, cross-session publication tokens, and cross-node/stale
transfer capabilities cannot release an acknowledgement. Dropping the operation
after root publication can have started forces recovery instead of silently
reactivating an old in-memory voter.

## Scope

This is a native in-process integration component. It does not implement sockets,
fsync, canonical snapshot verification/encoding, live application replacement,
joint membership, quorum signatures, or payload-availability certificates.
The production ReplCx transport/publisher/application adapters still need wiring
and execution validation. No cluster/read/voting capability is enabled here.

The integration tests use the real Chronicle crypto/FEC and Raft/seed kernels,
including donor loss and repair symbols. Their publication evidence models a
completed barrier; it does not substitute for RootStore's filesystem crash tests.
Run in the full workspace with `cargo test -p fgdb-order -p fgdb-repl`.
