# Bounded GLA execution, typed parameters and prepared-write validation

Status at 2026-09-08: the source changes below are on `main`. Rust compilation,
tests and repository gates for these connector-authored changes remain
**unverified here**. Owners `fgdb-boundplan-gla-lowering-seam-r2kd`,
`fgdb-w10-embedded-54r` and `fgdb-w4-g1-txn-core-qpmg` remain open pending their
complete acceptance proof.

## One bounded logical evaluator across the read surfaces

`fgdb_gql::algebra::GlaPlan::lower` translates the existing `BoundPlan` into
immutable scan, select, vertex-identity, expand, project, distinct, order and
limit operators. Positional integer-comparison fields are consumed at lowering;
predicates have one position-independent evaluator.

Ordinary live, historical, immutable pinned, bound/prepared, budgeted, limited,
governed and transaction MATCH execution use this evaluator. Certificate and
replay adapters reach it through their existing execution entrypoints. The
legacy inline adjacency/predicate engine and root-level
`execute_bound_plan_over`, `apply_skip` and `apply_limit` helpers were removed.
The bounded read cutover is not an opt-in or transaction-only migration.

Requested relation/orientation pairs are indexed once. Binding rows stream
through expansion, predicate conjunctions are cached per operator/vertex, and
projected IDs are collected without retaining a Cartesian path result. Parallel
edge occurrences survive until the explicit final distinct operation. This
preserves the bounded API's sorted, unique vertex-ID contract; it is not general
GQL multiset or path-identity semantics.

The logical transcript remains application data, not a registered durable
format. Existing statement, bind, plan, result and overlay certificate formats
were not changed by this cutover. It does not add executable-plan cost evidence
or make those certificates attest the full registered physical operator family.

### Repeated pattern bindings are identities

`MATCH (a)-[:R]->(a) RETURN a` requires a self-loop; it must not match an
ordinary edge merely because both positions use the same name. Likewise,
`MATCH (a)-[:R]->(b)-[:S]->(a) RETURN a` requires the far end to equal the
starting vertex. Lowering emits explicit identity operators after each slot
becomes bound, before that slot's property observations. The first occurrence
is the representative of its alias class. This covers all five three-position
alias partitions in outgoing, incoming and undirected patterns.

Renaming variables preserves the logical transcript only when it preserves
aliasing. Closed and unconstrained open paths therefore have different lowered
transcripts. The shared correction applies to ordinary, parameterized,
historical, staged and evidence-replayed queries. It does not add an edge
uniqueness or general path-identity contract.

## Native typed numeric parameters

`PreparedGqlTemplate::prepare(statement, names)` parses `$name` tokens as native
`NumericOperand::Parameter` AST nodes in the existing parser. The ordinary
binder assigns each occurrence its numeric target and original source span;
there is no separate role-inference parser or preparation-time string
substitution. Literal query preparation continues to reject unbound operands.

`parameter_schema()` reports names, types, occurrence counts and whether an
unsigned argument must be positive. `GqlParameters` supplies exact-case names
without `$`, using `with_int64` for integer property predicates and `with_uint64`
for SKIP/LIMIT. The full i64/u64 ranges are accepted where meaningful; SKIP 0 is
legal, while the parameter schema requires a positive LIMIT argument. Repeated
names share a value and must have compatible uses. Missing, extra, duplicate,
wrong-type and nonpositive LIMIT arguments are explicit errors; a rejected
duplicate never replaces the existing value.

`bind_parameters` validates the whole argument set before cloning and filling
the bound numeric slots. It does not parse again or resolve schema names again.
The result is the existing immutable `PreparedGqlQuery`, so live, historical,
pinned, staged, budgeted, limited, governed, artifact and cursor APIs need no
separate parameter execution engine. Reusing a template never changes earlier
bindings. Names, labels, relation names, property names, operators and clauses
are not parameter positions. Strings, floats, nulls and collections are not
implemented by this bounded numeric slice.

```rust
use fgdb_gql::{GqlParameters, PreparedGqlTemplate};

let template = PreparedGqlTemplate::prepare(
    "MATCH (a)-[:KNOWS]->(b) WHERE b.age >= $min_age RETURN b LIMIT $count",
    &names,
)?;
let arguments = GqlParameters::new()
    .with_int64("min_age", 40)?
    .with_uint64("count", 10)?;
let query = template.bind_parameters(&arguments)?;
let rows = db.execute_prepared_query(&query)?;
```

The complete production-runtime example, including database creation, repeated
bindings, combined limits, pinned views, bounded evidence replay and runtime
cancellation, is `crates/fgdb/examples/parameterized_queries.rs`:

```text
cargo run -p fgdb --example parameterized_queries
```

The executable plan is instantiated structurally. Separately, concrete statement
serialization places only decimal encodings of typed values at parser-proven
spans for the existing query/evidence format. That rendered text is not the
input to normal execution. Explicit artifact auditing may reparse it to check
coherence. Existing result evidence binds the concrete values; another binding
is not interchangeable merely because it happens to return the same rows.
`binding_digest(arguments)` additionally identifies the original template,
canonical name bindings and typed argument map as an application transcript;
it is not an authorization token or a new snapshot/result certificate.

Template, argument-map and value Debug output redact data. Explicit getters,
concrete `statement()` access and `canonical_bytes()` are data exports, not
redacted logging interfaces. Original parser error offsets refer to the original
template bytes, including when whitespace is multibyte.

## Borrow admitted durable state instead of copying its properties

`fgdb::gql_exec::AdmittedGqlSnapshot` binds one lowered plan and exact sequence
to its reader's private immutable `Snapshot`. Production snapshot constructors
own content-address, topology and version-chain validation; callers cannot
construct a `Snapshot`. The source scanner consumes that admitted state, not
arbitrary block arrays. Raw storage readers and validators are unchanged. The
independently fallible owned-source trait methods remain for test readers.

Source materialization is deferred until the execution policy is selected.
Ordinary, limited, budgeted and governed durable MATCH adapters share the
borrowed source path in `gql_exec/source.rs`:

- Vertex scans merge sorted typed patches with one reusable heap cursor per
  nonempty patch. They retain references to visible winning rows rather than
  a whole-history map or cloned scalar values.
- Edge scans retain at most one candidate per EId at the requested cut, not
  one candidate per content version. They copy only `(src, relation, dst)` for
  visible winners. Edge property sidecars are not requested at all.
- Predicates borrow the visible vertex rows needed by requested relation
  endpoints. Point selection uses typed patch metadata and binary search;
  neither a property clone nor a per-vertex history map is needed.

Latest creation sequence at or before the cut wins; equal-sequence statements
use publication order, including retirement restatements. Visibility is checked
only after choosing that winner. Filtering retired statements first would
resurrect an older version. Parallel edge identities remain distinct until
GLA's final projection/distinct contract. Historical and pinned views use the
same selectors at their exact cut.

The source retains metadata, not an alternative storage engine. Its inputs
are the immutable decoded generation already present in the embedded handle.
This is not on-demand object loading, a replacement for Strata's registered
access paths, or larger-than-memory storage.

Even a logically empty forged plan crosses the ordinary source admission
checks. Future or fenced snapshots cannot become successful empty reads or
be masked by resource limits. A transaction's foreign-handle check precedes
data observation and witness mutation.

## One borrowed canonical source for transaction MATCH

Ordinary, prepared, certified, row-budgeted, evaluator-limited and governed
transaction MATCH now share `query_source::OverlayQuerySource` in the existing
`write_txn_parts/gql_overlay_graph.rs`. Their source is the admitted snapshot at
`WriteTxn::basis` plus the exact canonical `PreparedWrite` net effects. They no
longer build query topology by interpreting raw intentions, call bulk owned
edge/vertex APIs to materialize their query rows, or replay the whole staged
intent sequence separately for every predicate vertex.

The durable selectors expose controlled borrowed visitors, which both durable
collectors and the transaction source use. Transaction topology is keyed by
EId while effects are folded, preserving parallel identities and enabling
canonical removals. Vertex deletion consumes the engine-derived sorted cascade
IDs directly; it does not scan the entire edge map for each deletion.

Vertex views borrow their base label/property slices from the snapshot or a
canonical CreateVertex effect. Keyed label overrides and property overrides
refer to the canonical final effect values. No source row clones a scalar,
string, byte array, or full property list, including when a property changes.
An explicit absent override shadows the base value. Predicates resolve the
borrowed value and use the shared GLA predicate semantics; missing and
non-integer properties do not satisfy integer comparisons, including NotEqual.

Preparation remains the only interpreter of statement order, conditional
operations and ensure semantics. Raw staged intentions are visited for
negative-read identities only: an unused ensure alias or erased transient
creation cannot become a query row. Canonical effects are visited once for
node queries; edge queries collect relevant vertex effects once and apply
them once by key after selecting predicate endpoints. This removes the
per-vertex full-template replay, not every metadata lookup or sorting step.

The borrowed source is tied to the transaction and database by Rust lifetimes.
It does not outlive the prepared template or immutable generation, and mutable
staging/publication cannot occur while its references are held. A transaction
that reads after another writer advances the live frontier still selects its
original basis and overlays only its own effects; it does not silently rebase.

Public point/bulk transaction row APIs retain their separate retrieval
implementation and remain useful comparison surfaces. This cutover governs
MATCH source construction, not all transaction operations or write preparation.

## Evaluator limits and cancellation

`GlaPlan::execute_with_control` is the shared execution body. Its callback runs
at admitted-row work, operator visits, scratch insertions and final output row
copies. It propagates the original caller-defined cancellation/resource error
without returning partial result rows. Unlimited execution uses the same body
with an inert control callback.

`GlaExecutionLimits::new(max_work_units, max_scratch_entries)` bounds evaluator
events and accumulated adjacency occurrences, predicate-cache entries and
distinct-result entries. Successful `GlaExecution` returns rows and exact
`GlaExecutionStats`; Debug redacts rows. Typed errors distinguish source failure
from work/scratch refusal. Exact limits succeed, one-over refuses, and observed
work/scratch counts use u128 to represent one past u64::MAX without wrapping.
A final `LIMIT 1` does not excuse unlimited work finding that answer.

### Interruptible adjacency ordering and output

Index construction uses deterministic iterative heapsort rather than one opaque
`sort_unstable` call per adjacency. The existing Work/checkpoint seam runs before
every value comparison and swap. Sorting uses the admitted neighbor slice in
place, without an additional scratch vector or recursive stack. A refusal
discards the private partial index; no partially sorted result escapes. Equal
IDs retain their multiplicity. No measured speedup over standard sorting is
claimed; this is work and cancellation coverage.

`ResultRow` is emitted after distinct/order/SKIP/LIMIT and before each final
vector insertion. Work counters include these output-copy events. The vector
is not preallocated to the full result length before its guard runs. Projected
distinct IDs are held separately; scratch-entry limits, not the final-row
budget, govern that intermediate representation.

`Database` and `EmbeddedReadView` expose `execute_prepared_query_limited` and
`execute_prepared_query_limited_at`; `WriteTxn` exposes the corresponding
`execute_prepared_query_limited(database, query, limits)`. These APIs remain
**evaluator-only**: the transaction source cutover does not silently add a
source-work policy to an existing limit contract. Ordinary budgeted APIs retain
their admitted-record/final-row scope. Their result-row checks precede each
output copy through the same evaluator. Governed calls add source controls.

### One combined policy with the real query context

`GqlQueryPolicy` combines row budgets with work/scratch limits:

```rust
let policy = fgdb_gql::GqlQueryPolicy::new(
    100_000,   // visible base records, or final transaction-overlay records
    1_000,     // final returned rows after query pagination
    1_000_000, // source plus evaluator work
    200_000,   // source plus evaluator logical scratch entries
);
let execution = db.execute_prepared_query_governed(&query_cx, &query, policy)?;
```

`Database` and `EmbeddedReadView` expose `execute_prepared_query_governed` and
`execute_prepared_query_governed_at`. `WriteTxn` exposes
`execute_prepared_query_governed(database, query_cx, query, policy)`. All execute
the same GLA. `GqlQueryExecution` returns rows and counters from that exact run,
with redacted Debug output.

For **durable, pinned and transaction governed MATCH**, the policy governs
borrowed source construction as well as evaluation. Checkpoints and work
charges cover history visits, patch merging and predicate-source selection.
Transaction calls additionally charge staged-ID visits, canonical-effect
visits, cascade removals, override-map entries and conflict-witness retention.
Scratch checks precede new candidate entries, cursors, row references, triples
and witness slots. No owned property table is produced before the guard runs.

SnapshotRecords counts the visible source table that execution actually uses,
not every historical statement or predicate endpoint. For transactions it is
checked while retaining the **final canonical overlay rows**: a basis edge or
vertex removed by the staged effects consumes source work/scratch, but not a
final-record allowance. An empty final overlay can therefore succeed with a
zero record allowance. The first excess final record refuses before insertion.
This does not bound all earlier source candidates; their work/scratch limits
are the controls for that phase. Predicate-source references do not inflate
the base-record count but do consume scratch and work.

Both paths use the same `AdmissionUsage` arithmetic. Evaluation receives only
the remaining work/scratch allowance. Successful counters include both phases;
a later evaluator refusal is translated to the original configured limit and
combined observed count. Neither phase gets a fresh full allowance. Individual
counter updates are atomic on refusal and use nonwrapping arithmetic.

`GqlQueryError` distinguishes `Source`, `Rows`, `Evaluator`, and `Interrupted`.
The interruption arm retains the original `QueryCx::checkpoint` error. The
context's ambient restriction wraps execution. Direct-query owner, health and
frontier checks precede cancellation; terminal checkpoints cover empty results.

### Transaction observations survive later refusal

The source records its label/table insertion witness only after the relevant
controlled admission step succeeds. A cancellation or zero work allowance that
refuses before that step does not invent a scan. Once retained, the witness
and every admitted element dependency survive subsequent source or evaluator
refusal. Predicates, DISTINCT, SKIP and LIMIT cannot shrink that footprint.

Node queries retain the existing label-scoped insertion dependencies, not a
new global vertex-insertion fence. Unrelated unlabeled insertions can commit;
matching insertions and later matching label membership are detected. Edge
queries retain the edge-table witness, observed EIds and both endpoints.
An unrelated vertex-only insertion need not conflict with an edge scan.

Each new per-query identity reserves a local witness entry and a persistent
transaction witness slot before retaining either. The logical reservation is
charged consistently even if the persistent read set already contains the
identity, so repeating the same query does not get a cheaper allowance merely
by warming that set. This is conservative per-query accounting, not a byte
cap or lifetime bound on the transaction's accumulated read set.

### Remaining resource boundaries

Scratch counts accumulated logical entries, **not allocator bytes or exact
peak resident memory**. Released intermediate entries do not refund the
allowance. The decoded generation, initial open/recovery/decoding, staging and
canonical write preparation, allocator internals, individual predicate
comparisons, cleanup and commit validation are not an end-to-end resource
ledger. B-tree/heap operations are bounded metadata steps, not checkpoints at
every machine instruction. These changes do not prove a wall-clock cancellation
deadline, bound a whole transaction lifetime, add spill, or implement on-demand
larger-than-memory storage.

Physical counters describe the admitted representation and operation. More
historical versions increase source work; compaction may reduce it. Transaction
witnesses and overrides add costs absent from a durable read. Identical logical
results do not imply identical physical counters. Existing logical-result
bytes, query signatures and evidence formats remain unchanged by source work.

### Governed artifact replay

A small evidence envelope can describe an expensive query. Limiting only its
encoded bytes and declared rows does not bound replay work. `Database` and
`EmbeddedReadView` expose
`audit_prepared_query_artifact_governed(query_cx, query, bytes, evidence_limits, policy)`;
`WriteTxn` exposes the overlay counterpart with the database argument first.

These methods preflight bytes and row counts under `GqlEvidenceLimits`, verify
the existing input, plan, snapshot/basis, result and staged-effect contracts,
and replay once under `GqlQueryPolicy` and the real QueryCx. Governed overlay
replay now inherits canonical transaction-source accounting too. Ordinary and
governed audits share verification helpers and certificate authority. Governed
audit never runs an unbounded audit first or returns a decoded-but-unreplayed
artifact on failure. Source interruption and policy errors stay nested under
the existing evidence execution error family.

The governed overlay method checks lifecycle/ownership and source health before
admission. Stale staged effects refuse even when result rows would be unchanged.
Terminal checkpoints precede successful artifact return. Encoded-input decoding
and digest checks are not individually preemptible. Existing page/cursor APIs
retain their prior audit paths; they do not implicitly inherit governed replay.
No evidence encoding or authorization rule changes.

## Prepared writes validate their own committed history

`Database::prepare_write` captures element and adjacency dependencies before
canonicalization removes conditional no-ops or ensure operations. Observations
include explicit targets, required endpoints, actual existing ensure aliases,
absent-triple insertion witnesses and engine-derived cascade targets. They are
retained privately with the immutable template and basis; dependency Debug
redacts identities.

Every `commit_prepared` attempt reconstructs its validator from the complete
retained committed suffix strictly after that prepared basis. An unrelated
ordinary write resetting the coordinator's prior validator cannot erase an
older conflict. This protects standalone PreparedWrite callers as well as
WriteTxn publication. Writes at or before the basis are excluded.

The template is committed exactly as prepared, never silently rebased. Foreign
opened-handle ownership refuses before validator installation. An unavailable
history prefix becomes `WriteError::PreparedHistory`, not an empty conflict map.
Conflicts retain the first-committer-wins error family. The production Chronicle
crash/publication tail is unchanged.

The validator distinguishes adjacency insertion witnesses from vertex writes:
unconstrained parallel-edge creations need not conflict merely because they
share endpoints. Ensure-by-triple and vertex deletion retain adjacency
witnesses because insertions can invalidate their assumptions.

WriteTxn also retains its additional read/mutation validation and scan
witnesses. These remain conservative guards, not complete predicate/range SSI.
Independent atomic relation groups are described in `docs/atomic_relation_writes.md`;
general ordered cross-relation dependencies remain unsupported by that API.

Point/bulk transaction edge reads apply canonical net effects as well. An ensure
resolved by an existing EId cannot invent the unused requested EId or overwrite
creation properties of the existing edge. This is still ensure-by-triple, not
the registered constraint-keyed EnsureEdge contract.

## Verification state and remaining work

The canonical transaction-source continuation adds **nine Rust tests**: two
borrowed-view laws and seven public integration tests in
`crates/fgdb/tests/gql_transaction_source.rs`. They cover actual base/override
pointer borrowing, label/property removal, missing integer comparisons, ordered
creates/updates/CAS/no-ops, ensures, erased transients, cascade removals,
parallel IDs, all query postures, governed evidence replay, every work/scratch
prefix, exact shared limits, payload-size-independent metadata costs and
repeated-query accounting. Final overlay record counts are checked separately
from removed basis candidates. Interleaved live writes must not rebase the
borrowed transaction, and failed multi-relation staging must leave it unchanged.

Expected query rows are derived by a separate nested-loop oracle over ordinary
transaction point/bulk reads, then compared with committed, historical and
pinned results. That is independent of query-source materialization and GLA,
not an independent proof of the entire storage/prepare/commit stack.

Existing source-selector tests and query tests are retained. The interruption
boundary test now distinguishes pre-source, pre-witness and post-witness
cancellation; the public label-conflict test refuses after witness retention,
not before it. Concurrent shared-predicate and node-meter improvements were
preserved; their presence does not establish a native validation verdict here.
The nine additions and expanded tests are **ADDED BUT UNRUN** in this environment.
No new Python model result is claimed for the transaction-source implementation.

Earlier borrowed durable-source/ordering work added ten Rust tests: independent
owned Strata comparisons, pointer borrowing, historical retirements and parallel
IDs, every source cancellation point, shared-meter boundaries, compaction and
reopening, and exhaustive controlled-sorting checks. Separate Python models
executed then: source selection compared 102,000 snapshots and 612,000 points
across 12,000 history/layout cases; ordering checked 4,280 arrays and 88,706
interrupted prefixes. Both passed as finite algorithm models, not as Rust
execution, durable replay proof or acceptance evidence for this continuation.

Earlier alias/governance, numeric-parameter and GLA/prepared-write tests remain,
including the independent nested-loop matrix of 649 plan variants. The older
118-plan transaction/durable suite compares two GLA-backed surfaces and is not
an independent executor oracle; separate nested-loop and multigraph laws are.

Cargo, rustc, rustfmt and rch are unavailable here; external network access
failed. Native tests, formatting, Clippy and exact-tree repository proof are
**UNRUN**, not passing. No hosted workflow was dispatched and no bead was closed
on unexecuted tests. Source inspection does not establish compilation.

The registered FreeJoin/authorized-Strata access path, whole-operation resource
and spill governance, nonnumeric parameters, final prepared-session/catalog
invalidation protocols, general GQL semantics, general ordered cross-relation
transactions and full SSI remain outstanding. No measured runtime speedup,
complete replay proof or Genesis-gate completion is claimed by this work.
