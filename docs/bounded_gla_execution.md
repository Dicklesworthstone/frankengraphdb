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
governed and transaction MATCH execution now use this evaluator. Certificate
and replay adapters reach it through their existing execution entrypoints. The
legacy inline adjacency/predicate engine and the root-level
`execute_bound_plan_over`, `apply_skip` and `apply_limit` helpers have been
removed. The bounded read cutover is no longer an opt-in limited-query path or
a transaction-only migration.

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

`parameter_schema()` reports the names, types, occurrence counts and whether an
unsigned value must be positive. `GqlParameters` supplies exact-case names
without `$`, using `with_int64` for integer property predicates and `with_uint64`
for SKIP/LIMIT. The full i64/u64 ranges are accepted where meaningful; SKIP 0 is
legal, LIMIT 0 is not. Repeated names share a value and must have compatible
uses. Missing, extra, duplicate, wrong-type and nonpositive LIMIT arguments are
explicit errors; a rejected duplicate never replaces the existing value.

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
construct a `Snapshot`. The new source scanner consumes that admitted state,
not arbitrary block arrays. Raw storage readers and validators are unchanged.
The independently fallible owned-source trait methods remain for test readers.

Source materialization is deferred until the execution policy is selected.
All ordinary, limited, budgeted and governed durable MATCH adapters share the
borrowed source path in `gql_exec/source.rs`:

- Vertex scans merge sorted typed patches with one reusable heap cursor per
  nonempty patch. They retain references to visible winning rows rather than
  a whole-history map or cloned scalar values.
- Edge scans retain at most one candidate per EId at the requested cut, not
  one candidate per content version. They copy only `(src, relation, dst)` for
  visible winners. Edge property sidecars are not requested at all.
- Predicates borrow the visible vertex rows needed by the requested relation
  endpoints. Each point selection walks typed patch metadata with binary
  search; neither a property clone nor a per-vertex history map is needed.

Latest creation sequence at or before the cut wins; equal-sequence statements
use publication order, including retirement restatements. Visibility is checked
only after choosing that winner. Filtering retired statements first would
resurrect an older version. Parallel edge identities remain distinct until
GLA's final projection/distinct contract. Historical and pinned views use the
same selectors at their exact cut.

The source retains bounded metadata, not an alternative storage engine. Its
inputs are the immutable decoded generation already present in the embedded
handle. This is not on-demand object loading, a replacement for Strata's
registered access paths, or larger-than-memory storage.

Even a logically empty forged plan crosses the source's ordinary admission
checks. Future or fenced snapshots therefore cannot become successful empty
reads or be masked by resource limits. A transaction's foreign-handle check
still precedes data observation and witness mutation.

## Evaluator limits and cancellation

`GlaPlan::execute_with_control` is the shared execution body. Its callback runs
at admitted-row work, operator visits, scratch insertions and final output row
copies. It propagates the original caller-defined cancellation/resource error
without returning partial result rows. Unlimited execution uses the same body
with an inert control callback.

`GlaExecutionLimits::new(max_work_units, max_scratch_entries)` bounds evaluator
events and accumulated adjacency occurrences, predicate-cache entries and
distinct-result entries. Successful `GlaExecution` returns rows and exact
`GlaExecutionStats`; Debug redacts the rows. Typed errors distinguish source
failure from a work/scratch refusal. Exact limits succeed, one-over refuses, and
the observed count uses u128 to represent one past u64::MAX without wrapping.
A final `LIMIT 1` does not excuse unlimited work finding that answer.

### Interruptible adjacency ordering

Index construction no longer invokes one opaque `sort_unstable` per adjacency.
A deterministic iterative heapsort crosses the existing Work/checkpoint seam
before every value comparison and swap. It uses the admitted neighbor slice in
place, with no additional scratch vector or recursive stack. A refusal discards
the private partial index; no partially sorted query result escapes. Equal IDs
retain their full multiplicity. This does not claim a measured speedup over the
standard-library sort; the purpose is bounded work and cancellation coverage.

`ResultRow` is emitted after distinct/order/SKIP/LIMIT and before each final
vector insertion. Work counters include these output-copy events. The vector
is not preallocated to the full result length before its guard runs. Projected
distinct IDs are still held separately; the scratch-entry limit, not the final
row budget, governs that intermediate representation.

`Database` and `EmbeddedReadView` expose `execute_prepared_query_limited` and
`execute_prepared_query_limited_at`; `WriteTxn` exposes the corresponding
`execute_prepared_query_limited(database, query, limits)` method. These existing
limited APIs remain evaluator-only; they have not silently acquired a new
source policy. The ordinary budgeted APIs also retain their documented
admitted-record/final-row scope. Their result-row checks run before each output
copy through the shared evaluator, rather than after building the full result.

### One combined policy with the real query context

`GqlQueryPolicy` combines the existing row budget with work/scratch limits:

```rust
let policy = fgdb_gql::GqlQueryPolicy::new(
    100_000,   // visible base snapshot records
    1_000,     // final returned rows
    1_000_000, // source plus evaluator work, on durable governed reads
    200_000,   // source plus evaluator logical scratch entries
);
let execution = db.execute_prepared_query_governed(&query_cx, &query, policy)?;
```

`Database` and `EmbeddedReadView` expose `execute_prepared_query_governed` and
`execute_prepared_query_governed_at`. `WriteTxn` exposes
`execute_prepared_query_governed(database, query_cx, query, policy)`.
All execute the same GLA. `GqlQueryExecution` returns rows and the counters
from that exact run, with redacted Debug output.

For **durable governed reads**, the selected policy now governs borrowed source
materialization as well as evaluation. Checkpoints and work charges occur
through history visits, patch-merge steps and predicate-source searches.
Scratch checks precede new candidate entries, initial patch cursors, visible
row references and edge triples. SnapshotRecords is checked before retaining
each visible base record, so a refusal reports the first excess prefix, not
the size of a fully copied table. Predicate-source references do not inflate
the base-record count but do consume scratch and work.

The evaluator receives only the remaining work/scratch allowance. Successful
counters include both phases, and a later evaluator refusal is translated back
to the original configured limit and combined observed count. Giving each
phase a fresh allowance would incorrectly permit twice the budget. Counter
updates are atomic on refusal and use nonwrapping arithmetic.

`GqlQueryError` distinguishes `Source`, `Rows`, `Evaluator`, and `Interrupted`.
The interruption arm retains the original `QueryCx::checkpoint` error. The
context's ambient restriction wraps execution. Direct-query owner, health and
future-frontier checks precede cancellation; checkpoints also run at completion,
including an empty result.

### Remaining resource boundaries

The borrowed source/meter cutover is **durable and pinned only**. Transactions
still construct their own canonical overlay; their source construction and
predicate-source internals remain outside evaluator accounting. Their logical
rows and row counts must agree with durable execution, but physical work and
scratch counters need not. Existing transaction budgeted reads reuse the actual
admitted rows, and governed reads use one overlay for count and execution.
Neither claim means every staged effect is visited only once.

A transaction interrupted before admission gains no fictitious scan. Once its
source has been admitted, interruption or budget refusal retains its witnesses.
Node queries retain label-scoped insertion dependencies, not a global
vertex-insertion fence. Unrelated unlabeled insertions can still commit;
matching insertions or later matching label membership changes are detected.

Scratch counts accumulated logical entries, **not allocator bytes or exact
peak resident memory**. Released intermediate entries do not refund this
conservative allowance. The source's existing decoded generation, initial
open/recovery/decoding, allocator internals, individual predicate comparisons
and cleanup are not end-to-end resource-ledger accounting. A B-tree/heap
operation is one bounded metadata step, not one checkpoint per machine
instruction. These changes do not prove a wall-clock cancellation deadline,
add spill, or complete whole-operation resource governance.

Physical counters describe the admitted representation: extra historical
versions increase scan work, and compaction may reduce it. Identical logical
results across different layouts do not imply identical operation counters.
Strict logical-result bytes and existing result evidence are unchanged.

### Governed artifact replay

A small evidence envelope can describe an expensive query. Limiting only its
encoded bytes and declared rows does not limit the work needed to reproduce it.
`Database` and `EmbeddedReadView` expose
`audit_prepared_query_artifact_governed(query_cx, query, bytes, evidence_limits, policy)`;
`WriteTxn` exposes the overlay counterpart with the database argument first.

These methods preflight artifact bytes and row counts under `GqlEvidenceLimits`,
then verify the existing input, plan, snapshot/basis, result and staged-effect
contracts. Their one replay runs under `GqlQueryPolicy` and the real QueryCx.
Durable and pinned governed replay therefore also use the new source meter.
Ordinary and governed audits share verification helpers and certificate
authority. A governed audit never runs the ordinary unbounded audit first,
skips an identity check, or returns a merely decoded artifact when replay fails.
Interruption and query-policy errors stay nested under the existing evidence
execution error; decoding and artifact-admission errors retain their own arms.

The governed overlay method checks lifecycle/ownership and the source handle
before admission. Stale staged effects still refuse even if result rows would
be unchanged. Terminal checkpoints precede the successful artifact return.
Encoded-input decoding and digest checks are not individually preemptible.
No evidence encoding or authorization rule changes. Existing page/cursor APIs
retain their prior audit paths; they do not implicitly inherit governed replay.

## Prepared writes validate their own committed history

`Database::prepare_write` captures compact element and adjacency dependencies
before canonicalization removes conditional no-ops or ensure operations. Those
observations include explicit targets, required edge endpoints, actual existing
ensure aliases, absent-triple insertion witnesses and engine-derived cascade
targets. They are retained privately with the immutable template and its basis;
the dependency Debug representation redacts identities.

Every `commit_prepared` attempt reconstructs its validator from the complete
retained committed suffix strictly after that prepared basis. An unrelated
ordinary write resetting the coordinator's prior validator cannot erase the
conflict. Writes at or before the basis are excluded. This protection applies
to **standalone PreparedWrite callers as well as WriteTxn publication**.

The template is committed exactly as prepared, never silently rebased. A foreign
opened-handle owner is refused before installing a validator. An unavailable
history prefix becomes `WriteError::PreparedHistory`, not an empty conflict map.
Observed conflicts retain the existing first-committer-wins error family. The
production Chronicle crash/publication tail is unchanged.

The validator distinguishes adjacency insertion witnesses from vertex writes:
independent unconstrained parallel-edge creations need not conflict merely
because they share endpoints. Ensure-by-triple and vertex deletion retain
adjacency witnesses because insertions can invalidate those operations.

`WriteTxn` retains its additional read/mutation validation and scan insertion
witnesses, including label-scoped node-query witnesses. Empty, filtered,
skipped and refused queries cannot erase admitted dependencies. These remain
conservative scan-backed guards, not a complete predicate/range SSI
implementation. Independent atomic relation groups are described in
`docs/atomic_relation_writes.md`; general ordered cross-relation dependencies
remain unsupported by that API.

## Overlay edge reads use actual net effects

Transaction point and bulk edge reads apply the exact canonical prepared net
template, not raw edge-create intentions with the ensure flag ignored. An
ensure resolved by an existing edge cannot invent the unused requested EId or
overwrite the existing edge's properties. Bulk admission counts consequently
do not include such imaginary aliases.

Ordered ensure/delete/re-ensure batches, property updates and canonical cascades
are reflected through the same prepared effects that commit publishes. Negative
read identities and the table insertion witness remain recorded even when the
result contains no edges. This is still ensure-by-triple, not the registered
constraint-keyed EnsureEdge contract.

## Verification state and remaining work

The borrowed-source/ordering continuation adds **10 Rust tests**: three source
selector/cancellation laws, two source-meter laws, two public source-history
integration tests, and three controlled-ordering tests. Source tests compare
against the independent owned Strata merges across time cuts, confirm actual
pointer borrowing, preserve parallel IDs and retirements, and interrupt every
low-level source event. Meter tests cover shared allowances, original-limit
errors, first-excess row refusal and counter overflow. Product cases cover
history updates, large edge properties, pinned views, cascades, compaction and
reopening. Sort tests compare exhaustive small arrays against the standard
sort, check a loose operation-count bound, interrupt every sorting event and
prove the evaluator can refuse in sorting before any predicate read.

These tests are **ADDED BUT UNRUN** here. An existing cross-surface test now
compares transaction/durable logical rows and row counts rather than claiming
identical physical work for distinct admission paths. No prior test was removed.

Actually executed, separate Python model checks:

- Source selection: 12,000 history/layout cases, 102,000 snapshot comparisons
  and 612,000 point comparisons against a full statement-map reference.
- Interruptible ordering: 4,280 arrays and 88,706 interrupted prefixes, checking
  sorted results, occurrence preservation, stopping position and the declared
  finite operation-count bound.

Both models passed. They do not compile or execute Rust, validate snapshot
construction, prove durable replay, or satisfy a repository gate.

The earlier alias/governance continuation added 15 Rust tests covering alias
partitions, combined-policy boundaries, interruption, source precedence,
artifact identity, stale overlays and conflict retention. Numeric parameters
added 15 argument/template/native-AST/product tests. Earlier GLA/prepared-write
work added 12 tests, including an independent nested-loop matrix of 649 plan
variants across updates and compaction. The older 118-plan transaction/durable
suite now compares two GLA-backed surfaces, so it is not an independent
executor oracle; the separate nested-loop and small-multigraph tests are.

Cargo, rustc, rustfmt and rch are unavailable here; external network access
failed. Native tests, formatting, Clippy and the exact-tree repository proof
are **UNRUN**, not passing. No GitHub Actions workflow was dispatched and no
bead was closed on unexecuted tests. Earlier Python models likewise establish
no native-build verdict.

The registered FreeJoin/authorized-Strata access path, transaction source
resource integration, whole-operation resource/spill governance, nonnumeric
parameters, final prepared-session/catalog invalidation protocols, general
GQL semantics, general ordered cross-relation transactions and full SSI remain
outstanding. No measured runtime speedup, complete replay proof or Genesis-gate
completion is claimed by this source-level work.
