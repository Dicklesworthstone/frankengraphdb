# Atomic relation groups with shared endpoint initialization

Implemented on unreleased `main`, 2026-09-08. The Rust changes and tests in this
continuation are **unverified here**. Owners `fgdb-w4-g1-txn-core-qpmg` and
`fgdb-w10-embedded-54r` remain open; this is not full SSI or the complete
ordered cross-relation workspace contract.

## Product operations

`Database::prepare_atomic_writes(Vec<WriteBatch>)` returns the existing
`PreparedWrite`. `Database::write_atomic(&CommitCx, Vec<WriteBatch>)` prepares
and commits immediately. `WriteTxn::write_atomic(&mut Database, Vec<WriteBatch>)`
stages the groups with prior batches and publishes only at `commit`. Failures
use the existing re-exported `WriteTxnError` family.

A KNOWS edge and a WORKS_AT edge can now be published together with their newly
initialized vertices. A reader sees either the old graph or all admitted
changes at one new sequence, never an intermediate graph containing just the
vertices or one edge type. One canonical template goes through the existing
prepared-write protocol: one capsule, one marker, one delta batch and one
snapshot publication. This is not a loop over independently committed batches.

```rust
let mut entities = WriteBatch::new(RelationId(9));
for id in 1..=3 {
    entities.ensure_vertex(VId(id), vec![], vec![]);
}
let mut social = WriteBatch::new(RelationId(1));
social.ensure_edge_by_triple(EId(10), VId(1), VId(2), vec![]);
let mut employment = WriteBatch::new(RelationId(2));
employment.ensure_edge_by_triple(EId(20), VId(2), VId(3), vec![]);
let sequence = db.write_atomic(&commit, vec![entities, employment, social]).await?;
```

The complete production-runtime example starts with an empty memory database,
stages the vertices and both relations together, queries before publication,
commits once, checks pinned/history reads, audits an artifact, and repeats the
same ensure groups without changing graph rows or version heads:

```text
cargo run -p fgdb --example atomic_relation_writes
```

The example is in `crates/fgdb/examples/atomic_relation_writes.rs`. It no longer
pre-commits the vertices. It is **committed but unrun here**.

## Independent groups retain their existing contract

Same-relation batches are concatenated in original order and evaluated by the
ordinary builder. Different independent groups share the same pinned basis.
Their element read/write footprints must be symmetrically independent: no
group's writes overlap another group's observations or writes. Read/read
sharing, including ordinary edges sharing live endpoints, is legal. Captured
observations include negative reads, conditional no-ops, actual ensure aliases,
endpoint existence and cascade targets.

Already-admissible independent inputs retain the original preparation path,
coordinate assignment and ordinal law. A label/property mutation conflicting
with another group's endpoint observation still returns `AtomicRelationConflict`.
Empty collections and explicitly empty members refuse before publication.
Unknown future delta families require an explicit independence rule; they
cannot become falsely empty write footprints.

## Leading shared vertex-initialization prefix

A new bounded composition rule handles the common dependency needed to create
a multi-relation graph from scratch:

1. The complete input begins with a contiguous run of `create_vertex` or
   `ensure_vertex` intentions, possibly spanning several batches. Discovery
   stops at the first other intention. It never moves later creations ahead
   of an edge, property update, condition or deletion.
2. When another relation needs an absent vertex named in this prefix, the
   prefix is prepared once on its own and prepended to each relation suffix
   for ordinary evaluation. The existing builder resolves every ensure,
   conditional operation, before-image, storage admission and canonical fold.
   There is no shadow database or second intention evaluator.
3. Each prepared suffix must preserve the complete canonical prefix: every
   newly created vertex must still be present with identical labels, values,
   valid-time fields and birth ordinal. Checking only surviving creations
   would miss an erased vertex; the implementation checks the complete expected
   set. Writes to already-live ensured prefix identities are also refused.
4. After that check, repeated prefix creations are removed from the suffix
   payloads. The actual prefix is emitted **once in the least relation
   coordinate**, so canonical replay creates the endpoints before every
   dependent edge. This relies on the bounded engine's existing graph-wide
   vertex identity model; it is not general coordinate relocation authority.
5. Remaining suffix groups must satisfy the ordinary symmetric independence
   rule. Only the newly created, proven-unchanged prefix identities are exempted
   from this intra-command conflict check. Their negative reads remain in the
   final prepared write for validation against external concurrent commits.

The initializer's source batch can have a larger relation ID than the edge
batches. Its creation effects still precede those edges. Empty suffix-only
coordinates do not manufacture separate graph mutations.

Ensures retain their ordinary semantics: an existing vertex keeps its content,
an absent vertex is created, repeated ensures of a prefix-created identity
are no-ops, an unconditional duplicate create refuses, and a spent identity is
not resurrected. No-op visits still occupy ordinal positions. If all referenced
endpoints are already live, the ordinary independent preparation path is used.

A repeated all-ensure initialization can therefore preserve the graph's rows
and version heads. It still uses the normal commit protocol and can advance
the sequence: **graph idempotence is not request deduplication or exactly-once
marker delivery**. Ensure-by-triple is still distinct from the plan's registered
constraint-keyed EnsureEdge operation.

### Order and cost boundaries

The initializer occupies the first checked interval of raw intent visits.
Suffixes follow in canonical relation order, preserving original visit order
within each relation. Every surviving creation has a checked, nonoverlapping
birth ordinal; conditional no-ops still create gaps. The initializer is not
renumbered when its duplicate evaluation is stripped from a suffix. No durable
encoding or version transcript is changed.

Prefix metadata and values are cloned and evaluated through the ordinary
builder for each nonempty suffix. This is a correctness-oriented bounded
composition, **not a one-pass write workspace or a measured preparation-speed
improvement**. Preparation allocation, cancellation and whole-transaction
resource governance are not implemented by this change.

This remains **not arbitrary sequential cross-relation evaluation**. A suffix
cannot change a shared initialized vertex's final content, depend on a vertex
created after the leading prefix, or consume another suffix's new edge or
property change. Same-relation ordered mutations remain supported. A rejected
dependent operation must not be split into separate commits and called atomic.

## Transaction integration and failure behavior

A previously staged leading vertex-initialization batch can supply the prefix
when later calls add relations with `write_atomic`. A rejected addition restores
the prior staged batches and prepared template. Previously acquired read
observations are retained. Ownership and pinned-frontier checks precede group
admission, and no capsule or marker is published until `commit`.

After explicit multi-relation staging, ordinary `write` additions re-enter the
same composition checker, including shared-prefix verification. They cannot
place every staged edge under the first relation. The ordinary single-relation
`write` API retains its `RelationMismatch` behavior until multiple relation
groups have been explicitly admitted.

Point/bulk overlays and the borrowed MATCH source consume the canonical
prepared effects, so the shared creations appear once and their birth ordinals
agree with the grouped template. Creation timestamps before commit remain the
documented basis placeholders. Existing label/table insertion and negative-read
witnesses remain active. Parameterized, budgeted, governed and artifact query
adapters need no separate graph-initialization path.

All preparation dependencies are unioned into the compound prepared write.
Standalone and transaction commits retain full-history-suffix FCW validation,
foreign-owner refusal, ambiguous-outcome fencing and authoritative recovery.
A competing creation of a shared prefix identity is not ignored merely because
that identity was exempted from internal suffix independence. Property-size
admission remains in the ordinary builder before any group can publish.

## Verification boundary

This continuation adds **11 Rust tests**: three prefix-discovery unit laws and
eight public integration tests in `crates/fgdb/tests/atomic_vertex_prefix.rs`.
They cover one-sequence publication from an empty graph, earliest-coordinate
endpoint creation, complete-prefix preservation, erased unused vertices,
independent suffix conflicts, no-op ordinal gaps, mixed existing/new ensures,
repeat submissions, duplicate ensures, spent IDs, late-creation refusal,
oversized properties, external conflicts across intervening commits, ownership,
transaction staging/rollback, additional relations, and evidence invalidation.

Recovery tests enumerate pre-capsule, pre-D1, post-D1, surviving unflushed marker,
torn-marker trailer, synced marker and normal success. Reopen must recover all
new endpoints and both relations or neither. Surviving unflushed bytes are one
possible crash outcome, not a durability guarantee before D2 or a substitute
for the fault-VFS power-loss suite. Historical content, creation ordinals and
version heads are compared through later updates, cross-relation cascades,
compaction, checkpoint reopening and full rebuilding.

The earlier independent-group tests are retained unchanged. Their previous
source-level test additions and this continuation's tests/example are
**ADDED BUT UNRUN in this environment**. Cargo, rustc, rustfmt and rch are
unavailable here; external network access failed. No passing compilation,
Clippy, formatting, complete repository gate or exact-tree verdict is claimed.
No hosted workflow was dispatched and no bead was closed on unexecuted tests.

The earlier 262,144-case Python independence model checked only indexed
footprint admission against a pairwise definition. It does **not** validate
shared-prefix discovery, exact prefix verification, canonical folding, Rust
types or durability. No new model-validation result is claimed for this change.

Remaining: general ordered cross-relation suffix dependencies, complete
constraint and authorization integration, full SSI, whole-operation resource
governance, and native execution of the acceptance suites.
