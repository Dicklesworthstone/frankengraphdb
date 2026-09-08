# Atomic independent relation groups

Implemented on unreleased `main`, 2026-09-08. The Rust changes and tests in this
continuation are **unverified here**. Owners `fgdb-w4-g1-txn-core-qpmg` and
`fgdb-w10-embedded-54r` remain open; this is not full SSI or the complete
cross-relation workspace contract.

## Product operations

`Database::prepare_atomic_writes(Vec<WriteBatch>)` returns the existing
`PreparedWrite`. `Database::write_atomic(&CommitCx, Vec<WriteBatch>)` prepares
and commits immediately. `WriteTxn::write_atomic(&mut Database, Vec<WriteBatch>)`
stages the groups with any previous batches and publishes only at `commit`.
Failures use the existing re-exported `WriteTxnError` family.

For example, a KNOWS edge and a WORKS_AT edge over existing vertices can be
published together. A reader sees either the old graph or both relations at
one new sequence, not an intermediate graph containing only the first group.
The implementation commits one canonical template through the existing
prepared-write protocol: one capsule, one marker, one delta batch and one
snapshot publication. It does not loop over independently committed batches.

The complete production-runtime example creates an in-memory database,
stages a two-relation path, queries the overlay before publication, commits,
checks a pinned view and the old sequence, and audits a durable result artifact:

```text
cargo run -p fgdb --example atomic_relation_writes
```

## Independence is an enforced boundary

Same-relation batches are concatenated in their original order and evaluated
by the ordinary builder. Different relation groups are evaluated at the same
basis. Their element read/write footprints must be symmetrically independent:
no group's writes may overlap another group's observations or writes.
Read/read sharing, such as two unconstrained edges sharing existing endpoints,
is legal. Captured observations include negative reads, conditional no-ops,
actual ensure aliases, endpoint existence and cascade targets.

This is intentionally conservative and **not arbitrary sequential
cross-relation evaluation**. A vertex mutation conflicting with another
group's endpoint dependency returns `AtomicRelationConflict`. A reference to
a vertex newly created in another relation group cannot be resolved from the
common basis and returns the ordinary preparation error. Same-relation
create-then-use prefixes continue to work. Empty collections and explicitly
empty members refuse. Do not split a logically atomic dependent operation
into separate commits to work around this boundary.

Groups have canonical relation order. Their creation birth ordinals occupy
checked, nonoverlapping intervals of raw intent visits, including no-ops.
Within-group evaluation order is preserved. No durable encoding is changed.
Unknown future delta families require an explicit independence rule and
otherwise refuse, rather than becoming empty write footprints.

## Transaction integration and failure behavior

A rejected staging attempt restores the prior staged batches and prepared
template. Previously observed dependencies are retained. Owner and pinned
frontier checks precede group admission. After explicit multi-relation staging,
ordinary `write` additions re-enter the same independence checker; they cannot
re-home all staged edges onto the first relation. The existing single-relation
`write` API retains its legacy `RelationMismatch` behavior until several
relation groups have been explicitly admitted.

Vertex point and bulk overlays now consume the prepared canonical effects,
matching the existing edge overlay. Birth ordinals therefore agree with the
actual grouped template, and an erased transient creation is not reintroduced.
The pre-commit creation timestamp remains the documented basis placeholder.
The ordinary negative-read and table-insertion witnesses remain active.

All captured preparation dependencies are unioned into the compound prepared
write. Standalone commits and transaction publication retain the complete
history-suffix FCW validation, foreign-owner rejection, ambiguous-outcome
fencing and authoritative recovery. Property-size admission remains in each
group's ordinary builder, before any group can publish.

## Verification boundary

Added tests: one independence unit law, five atomic database tests, four
transaction integration tests and three recovery/storage-admission tests.
Coverage includes one-sequence publication, cross-relation paths, ordered
same-relation prefixes, birth ordinals, conditional and cascade conflicts,
failed-staging rollback, owner/lifecycle checks, history resets, evidence,
pinned views, oversized properties, compaction and checkpoint/full rebuild
agreement. The marker test enumerates pre-capsule, pre-D1, post-D1, surviving
unflushed marker, torn-marker trailer, synced marker and normal success.
Surviving unflushed bytes are explicitly one possible crash outcome, not a
claim of durability before D2 or a replacement for the fault-VFS power-loss suite.

These 13 Rust tests and the example are **ADDED BUT UNRUN**. Cargo, rustc,
rustfmt and rch are unavailable here. No compilation, Clippy, formatting,
full-repository gate or exact-tree green verdict is claimed. No hosted workflow
was dispatched and no bead was closed.

A separate Python model checked the indexed independence admission rule
against the symmetric pairwise definition for all 262,144 three-group
read/write-footprint combinations over a three-element universe. It passed.
That finite model does not validate dependency capture, canonical folding,
Rust types, durability, or the committed product implementation.

Remaining: general ordered cross-relation dependencies, complete constraint
and authorization integration, full SSI, whole-operation resource governance,
and native execution of the acceptance suites.
