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

Ordinary live, historical, immutable pinned, bound/prepared, budgeted, limited
and transaction MATCH execution now use this evaluator. Certificate and replay
adapters reach it through their existing execution entrypoints. The legacy
inline adjacency/predicate engine and the root-level `execute_bound_plan_over`,
`apply_skip` and `apply_limit` helpers have been removed. The bounded read cutover
is no longer an opt-in limited-query path or a transaction-only migration.

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
pinned, staged, budgeted, limited, artifact and cursor APIs need no separate
parameter execution engine. Reusing a template never changes earlier bindings.
Names, labels, relation names, property names, operators and clauses are not
parameter positions. Strings, floats, nulls and collections are not implemented
by this bounded numeric slice.

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
bindings, limits, pinned views and evidence replay, is
`crates/fgdb/examples/parameterized_queries.rs`:

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
with its lowered plan, reader and exact sequence. Ordinary, limited and budgeted
durable adapters share this admission owner. Budgeted execution checks its
record count and then executes those same rows; it no longer discards a counting
scan and reads the table again. Node predicates reuse admitted vertex rows.

Even a logically empty forged plan crosses the source's ordinary admission
checks. Future or fenced snapshots therefore cannot become successful empty
reads or be masked by evaluator limits. A transaction's foreign-handle check
still precedes data observation and witness mutation.

The transaction admission/final-row budget API still has its existing overlay
counting step. Its traversal is shared GLA, but this change does not claim to
have eliminated every repeated overlay materialization or staged-row visit.

## Evaluator limits and cancellation

`GlaPlan::execute_with_control` is the shared execution body. Its callback runs
before admitted-row work, operator visits and scratch insertions, and propagates
a caller-defined cancellation/resource error without returning partial rows.
Unlimited execution uses the same body with an inert control callback.

`GlaExecutionLimits::new(max_work_units, max_scratch_entries)` bounds evaluator
events and accumulated adjacency occurrences, predicate-cache entries and
distinct-result entries. Successful `GlaExecution` returns rows and exact
`GlaExecutionStats`; Debug redacts the rows. Typed errors distinguish source
failure from a work/scratch refusal. Exact limits succeed, one-over refuses, and
the observed count uses u128 to represent one past u64::MAX without wrapping.
A final `LIMIT 1` does not excuse unlimited work finding that answer.

`Database` and `EmbeddedReadView` expose `execute_prepared_query_limited` and
`execute_prepared_query_limited_at`; `WriteTxn` exposes the corresponding
`execute_prepared_query_limited(database, query, limits)` method. They accept the
existing coherent `PreparedGqlQuery` rather than a second preparation format.

These are **not allocator-byte, storage-I/O, wall-clock or spill limits**.
Source materialization, overlay construction and predicate-source internals
remain outside the evaluator accounting. The control hook is not an end-to-end
QueryCx deadline, runtime-task cancellation contract or resource-ledger proof.
The existing `GqlExecutionBudget` admission/final-row API remains distinct.

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

`WriteTxn` retains its existing additional read/mutation validation and explicit
vertex/edge table-scan insertion witnesses. Empty, filtered, skipped and refused
queries cannot erase the admitted dependencies. These remain conservative
scan-backed guards, not a complete predicate/range SSI implementation.

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

The numeric-parameter continuation adds 15 Rust tests: eight argument/template
laws, four native AST operand/span/normalization tests, and three product
integration tests. Coverage includes comparison with ordinary literal binding,
integer boundaries, immutable rebinding, strict argument errors, all existing
read surfaces, budgets, evaluator limits, artifacts, cursor resumption,
compaction, stale-overlay refusal and retained empty-scan conflict witnesses.
The complete preexisting parser test block was retained byte-for-byte. The
new example and all new parameter tests are **UNRUN** here.

Source copies and the native parser upload were checked by Git blob identity;
Rust-token delimiters were inspected. These are source-integrity checks, not
Rust parsing, typechecking, compilation, formatting or behavioral test results.
The concurrent main-branch storage changes were preserved during integration.

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
checks, not a Rust build verdict. In particular they do not validate the new
native parameter parser. Cargo, rustc and rustfmt are unavailable here and
external network access failed. Cargo tests, formatting, Clippy and the
committed-tree repository proof for the new parameter implementation remain
unrun, not passing. No GitHub Actions workflow was dispatched and no bead was
closed on unexecuted tests.

The registered FreeJoin/authorized-Strata access path, whole-operation resource
and spill governance, nonnumeric parameters, final prepared-session/catalog
invalidation protocols, general GQL semantics and full SSI remain outstanding.
No runtime speedup, complete replay proof or Genesis-gate completion is claimed
by this source-level continuation.
