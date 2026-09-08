# Bounded GLA execution and prepared-write validation

Status at 2026-09-08: the source changes below are on `main`. Rust compilation,
tests and repository gates for this continuation remain **unverified**.
Owners `fgdb-boundplan-gla-lowering-seam-r2kd` and
`fgdb-w4-g1-txn-core-qpmg` remain open pending their complete acceptance proof.

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

This continuation adds 12 Rust tests: four validator laws, one shared-admission
check, four standalone prepared-write integration tests, two canonical edge
overlay tests and one independent query-oracle matrix. The last enumerates
649 plan variants and compares live, historical and pinned results before and
after graph updates and compaction. It shares neither GLA lowering nor predicate
implementation with production.

The older 118-plan transaction/durable suite remains useful for cross-surface
agreement, but its two production surfaces now share GLA: it is no longer an
independent executor oracle. The new nested-loop test and the existing
small-multigraph enumeration provide the independent algorithmic checks.

All newly added Rust tests are **UNRUN** here. Source diffs and token delimiters
were inspected, and the separate existing Python query-semantics specification
was rerun successfully for 32,400 cases. That model does not compile, execute
or prove the committed Rust implementation. Earlier Python interleaving/metering
results are likewise only model checks, not a product-build verdict.

Cargo, rustc and rustfmt are unavailable in this environment; external network
access also failed. Cargo tests, formatting, Clippy and the committed-tree
repository proof are therefore unrun, not passing. No GitHub Actions workflow
was dispatched and no bead was closed on unexecuted tests.

The registered FreeJoin/authorized-Strata access path, whole-operation resource
and spill governance, typed parameters, general GQL semantics and full SSI
remain outstanding. No runtime speedup, complete replay proof or Genesis-gate
completion is claimed by this source-level continuation.
