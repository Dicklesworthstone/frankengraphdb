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
starting vertex. Lowering now emits explicit identity operators after each
slot becomes bound, before that slot's property observations. The first
occurrence is the representative of its alias class. This covers all five
three-position alias partitions in outgoing, incoming and undirected patterns.

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

## Admit durable input once, then execute that exact input

`fgdb::gql_exec::AdmittedGqlSnapshot` owns the admitted source table together
with its lowered plan, reader and exact sequence. Ordinary, limited, budgeted
and governed durable adapters share this admission owner. Budgeted execution
checks its record count and then executes those same rows; it no longer
discards a counting scan and reads the table again. Node predicates reuse
admitted vertex rows.

Even a logically empty forged plan crosses the source's ordinary admission
checks. Future or fenced snapshots therefore cannot become successful empty
reads or be masked by evaluator limits. A transaction's foreign-handle check
still precedes data observation and witness mutation.

The older transaction admission/final-row budget API still has its existing
overlay counting step. The governed transaction call constructs one overlay
for both its count and execution, but this does not eliminate all repeated
staged-row visits or predicate-source reads.

## Evaluator limits and cancellation

`GlaPlan::execute_with_control` is the shared execution body. Its callback runs
before admitted-row work, operator visits, scratch insertions and final output
row copies. It propagates a caller-defined cancellation/resource error without
returning partial rows. Unlimited execution uses the same body with an inert
control callback.

`GlaExecutionLimits::new(max_work_units, max_scratch_entries)` bounds evaluator
events and accumulated adjacency occurrences, predicate-cache entries and
distinct-result entries. Successful `GlaExecution` returns rows and exact
`GlaExecutionStats`; Debug redacts the rows. Typed errors distinguish source
failure from a work/scratch refusal. Exact limits succeed, one-over refuses, and
the observed count uses u128 to represent one past u64::MAX without wrapping.
A final `LIMIT 1` does not excuse unlimited work finding that answer.

`ResultRow` is emitted after distinct/order/SKIP/LIMIT and before each final
vector insertion. Work counters now include these output-copy events. The
vector is not preallocated to the full result length before its guard runs.
Projected distinct IDs are still held separately; the scratch-entry limit, not
the final-row budget, governs that intermediate representation.

`Database` and `EmbeddedReadView` expose `execute_prepared_query_limited` and
`execute_prepared_query_limited_at`; `WriteTxn` exposes the corresponding
`execute_prepared_query_limited(database, query, limits)` method. They accept the
existing coherent `PreparedGqlQuery` rather than a second preparation format.

### One combined policy with the real query context

`GqlQueryPolicy` combines the existing row budget with evaluator limits:

```rust
let policy = fgdb_gql::GqlQueryPolicy::new(
    100_000, // admitted snapshot records
    1_000,   // final returned rows
    1_000_000, // evaluator work units
    200_000, // scratch entries
);
let execution = db.execute_prepared_query_governed(&query_cx, &query, policy)?;
```

`Database` and `EmbeddedReadView` expose `execute_prepared_query_governed` and
`execute_prepared_query_governed_at`. `WriteTxn` exposes
`execute_prepared_query_governed(database, query_cx, query, policy)`.
All use the same GLA evaluator and meter; no second execution is needed to
combine policies. `GqlQueryExecution` returns rows plus the row and evaluator
counters from that exact run, with redacted Debug output.

`GqlQueryError` distinguishes `Source`, `Rows`, `Evaluator`, and `Interrupted`.
The interruption arm retains the original `QueryCx::checkpoint` error, rather
than relabeling cancellation as exhaustion. Direct-query owner, handle-state
and future-frontier checks precede cancellation. The context's ambient
restriction wraps execution. Checkpoints run before admission, at evaluator
events and at completion, including when the result is empty.

A transaction interrupted before admission gains no fictitious scan. Once the
source has been admitted, interruption or budget refusal retains its witnesses.
Node queries use the existing label-scoped insertion dependencies, not a new
global vertex-insertion fence. Unrelated unlabeled insertions can still commit;
matching insertions or later matching label membership changes are detected.

These are **not allocator-byte, storage-I/O, wall-clock or spill limits**.
Source materialization, overlay construction and predicate-source internals
remain outside the evaluator accounting. A checkpoint does not interrupt a
single storage merge or sort midway; bounded deadline responsiveness and the
whole-operation resource ledger remain incomplete. Existing unguided query
APIs have not silently acquired a new default policy.

### Governed artifact replay

A small evidence envelope can describe an expensive query. Limiting only its
encoded bytes and declared rows does not limit the work needed to reproduce it.
`Database` and `EmbeddedReadView` therefore expose
`audit_prepared_query_artifact_governed(query_cx, query, bytes, evidence_limits, policy)`;
`WriteTxn` exposes the overlay counterpart with the database argument first.

These methods preflight artifact bytes and row counts under `GqlEvidenceLimits`,
then verify the existing input, plan, snapshot/basis, result and staged-effect
contracts. Their one replay runs under `GqlQueryPolicy` and the real QueryCx.
Ordinary and governed audits share the verification helpers and certificate
authority. A governed audit never runs the ordinary unbounded audit first,
skips an identity check, or returns a merely decoded artifact when replay fails.
Interruption and query-policy errors stay nested under the existing evidence
execution error; decoding and artifact-admission errors retain their own arms.

The governed overlay method checks lifecycle/ownership and the source handle
before admission. Stale staged effects still refuse even if result rows would
be unchanged. Terminal checkpoints also precede the successful artifact return.
Encoded-input decoding and digest checks are not individually preemptible.
No evidence encoding or authorization rule changes. Existing page/cursor APIs
retain their prior audit paths; they do not implicitly inherit governed replay.

## Prepared writes validate their own committed history

`Database::prepare_write` now captures compact element and adjacency dependencies
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
because they share endpoints. Ensure-by-triple and vertex deletion do retain
adjacency witnesses because insertions can invalidate those operations.

`WriteTxn` retains its existing additional read/mutation validation and scan
insertion witnesses, including label-scoped node-query witnesses. Empty,
filtered, skipped and refused queries cannot erase admitted dependencies.
These remain conservative scan-backed guards, not a complete predicate/range
SSI implementation. Independent atomic relation groups are described in
`docs/atomic_relation_writes.md`; general ordered cross-relation dependencies
remain unsupported by that API.

## Overlay edge reads use actual net effects

Transaction point and bulk edge reads now apply the exact canonical prepared
net template, not raw edge-create intentions with the ensure flag ignored.
An ensure resolved by an existing edge cannot invent the unused requested EId
or overwrite the existing edge's properties. Bulk admission counts consequently
do not include such imaginary aliases.

Ordered ensure/delete/re-ensure batches, property updates and canonical cascades
are reflected through the same prepared effects that commit publishes. Negative
read identities and the table insertion witness remain recorded even when the
result contains no edges. This is still ensure-by-triple, not the registered
constraint-keyed EnsureEdge contract.

## Verification state and remaining work

The alias/governance continuation adds 15 Rust tests: three alias laws, four
combined-policy laws, two context/admission tests, one shared-audit replay law
and five public governed-query/artifact integration tests. The alias matrix
covers 64 graphs, all five three-position alias partitions, three directions
and each projected position. Governance cases cover all four dimensions,
exact boundaries, interruption at every low-level checkpoint, pre-admission
refusal, post-admission conflict retention, source refusal precedence, artifact
replay, stale overlays and preservation of label-scoped conflicts. These tests
and the updated production-runtime example are **ADDED BUT UNRUN** here.

A separate Python specification check executed 2,880 finite cases comparing the
representative-slot alias rule with the all-pairs equality definition. It
passed. It did not execute the Rust parser, lowering or evaluator and is not a
native build or product-validation verdict.

The numeric-parameter continuation previously added 15 Rust tests: eight
argument/template laws, four native AST operand/span/normalization tests, and
three product integration tests. The original parser test block was retained.
Those connector-authored additions have no native-test verdict from this
environment either; source identity and delimiter checks do not establish one.

Earlier GLA/prepared-write work added 12 Rust tests: four validator laws, one
shared-admission check, four standalone prepared-write integration tests, two
canonical edge-overlay tests and an independent query-oracle matrix. The last
enumerates 649 plan variants and compares live, historical and pinned results
before and after graph updates and compaction. It shares neither GLA lowering
nor predicate implementation with production.

The older 118-plan transaction/durable suite remains useful for cross-surface
agreement, but its two production surfaces now share GLA: it is no longer an
independent executor oracle. The nested-loop test and the existing small-
multigraph enumeration provide independent algorithmic checks.

Earlier Python query-semantics/interleaving/metering checks were separate model
checks, not a Rust build verdict. Cargo, rustc and rustfmt are unavailable here
and external network access failed. Cargo tests, formatting, Clippy and the
committed-tree repository proof remain unrun, not passing. No GitHub Actions
workflow was dispatched and no bead was closed on unexecuted tests.

The registered FreeJoin/authorized-Strata access path, whole-operation resource
and spill governance, nonnumeric parameters, final prepared-session/catalog
invalidation protocols, general GQL semantics, general ordered cross-relation
transactions and full SSI remain outstanding. No runtime speedup, complete
replay proof or Genesis-gate completion is claimed by this source-level work.
