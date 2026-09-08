# Bounded GLA execution and transaction validation

Status at 2026-09-08: source implemented on `main`; Rust build and tests remain
**unverified** in the connector environment. Owners:
`fgdb-boundplan-gla-lowering-seam-r2kd` and `fgdb-w4-g1-txn-core-qpmg` remain open.

## Logical execution

`fgdb_gql::algebra::GlaPlan::lower` translates the existing `BoundPlan` into
immutable scan, select, vertex-identity, expand, project, distinct, order and
limit operators. Positional integer-comparison fields are consumed at lowering.
Predicates have one position-independent evaluator. The canonical logical
transcript is application data, not a registered durable format or a replacement
for existing result certificates.

Requested relation/orientation pairs are indexed once. Binding rows stream
through expansion; predicate conjunctions are cached per operator/vertex. The
executor retains distinct projected IDs rather than a Cartesian path result.
Parallel edge occurrences remain until the terminal set projection. This is the
bounded API's sorted, unique vertex-ID contract, not general GQL multiset or
path-identity semantics.

`WriteTxn` node and edge MATCH execution use this evaluator, including text,
bound, owned-prepared and overlay-evidence/cursor adapters that converge there.

## Early evaluator limits and cancellation

`GlaPlan::execute_with_control` is the shared execution body. Its callback runs
before admitted-row work, operator visits and scratch insertions. It may return
a caller-defined cancellation/resource error; that exact error propagates
without returning partial rows. Ordinary unlimited execution uses this same
body with an inert control callback.

`GlaPlan::execute_with_limits` supplies deterministic accounting:

- `GlaExecutionLimits::new(max_work_units, max_scratch_entries)` bounds evaluator
  events and the accumulated adjacency, predicate-cache and distinct-result
  entries before they grow.
- `GlaExecution` returns rows plus exact `GlaExecutionStats` on success. Debug
  output redacts rows.
- `GlaExecutionError::Source` preserves the source error;
  `GlaExecutionError::Limit` carries dimension, configured limit and observed
  count. Exact limits succeed; one-over refuses. The observed field is u128 so
  one past u64::MAX is representable rather than wrapped or saturated.

A final `LIMIT 1` does not excuse unbounded work finding that answer. Evaluator
limits can interrupt index construction or path expansion even when the final
projected result would be small.

These are **not allocator-byte, storage-I/O, wall-clock or spill limits**.
Snapshot materialization, transaction overlay construction and predicate-source
internals are outside this accounting. The control hook is not by itself an
end-to-end QueryCx deadline, runtime task cancellation or resource-ledger proof.
The existing `GqlExecutionBudget` admission/final-row API remains separate.

## Product entrypoints

`Database` and `EmbeddedReadView` expose:

- `execute_prepared_query_limited(query, limits)`;
- `execute_prepared_query_limited_at(query, sequence, limits)`.

`WriteTxn` exposes `execute_prepared_query_limited(database, query, limits)`.
All five use the same controlled GLA evaluator. The durable adapters admit their
source table once rather than counting it, discarding it and reading it again;
node predicates reuse admitted rows. A future/fenced snapshot is refused before
an evaluator limit can mask the source error. A foreign transaction handle is
refused before observing its data or changing the owner's witnesses.

These methods accept the existing coherent `PreparedGqlQuery`; preparation and
binding are not duplicated. Limited execution does not issue a certificate or
partial artifact on refusal.

**The general unbounded durable cutover is still incomplete.** Ordinary live,
historical and immutable read-view GQL calls still use `fgdb/src/gql_exec.rs`.
They remain an independent comparison path for the new limited GLA adapters.
The registered FreeJoin/authorized-Strata/spill integration has not landed.

## Transaction conflict gaps addressed

Empty scans now retain explicit table-insertion witnesses. This covers bulk
vertex/edge reads, node/edge MATCH, skipped/filtered output and budget refusals.
A newly inserted disconnected vertex or path can no longer escape merely
because its identity was absent from the original read set. The witnesses are
conservative: a vertex scan conflicts with vertex insertions, and an edge scan
with edge insertions. Point reads are not upgraded to a global commit fence.

`WriteTxn` validates both its read and mutation footprints against one complete
retained delta suffix before consuming the prepared write. Intervening ordinary
writes resetting the coordinator's FCW map no longer erase an older transaction's
mutation conflicts. The footprint includes explicit targets, edge endpoints,
engine-derived cascade targets and conditional no-op dependencies. A retired
conflict-history prefix remains a typed error rather than a false no-conflict
verdict. This additional guard is **WriteTxn-specific**; the standalone
`PreparedWrite` API has not received the same suffix validation here.

Ensure-by-triple needs an extra observation: an existing edge may satisfy the
triple under an EId different from the requested alias. Before preparing such a
batch, the transaction observes actual edge identities and the table insertion
witness through its existing edge-read path. Deleting that actual edge therefore
conflicts even if canonicalization emitted no ensure delta. This is conservative
scan-backed behavior, not constraint-keyed EnsureEdge or full predicate SSI.

## Verification state

The original small-multigraph oracle and 118-plan transaction/durable comparison
suite remain. This continuation adds 18 Rust tests covering scan phantoms,
interleaved blind writes, conditional no-ops, cascade races, ensure aliases,
exact/one-below evaluator limits, path fanout, cancellation, accounting overflow,
single source admission and limited execution across the product read surfaces.
They are **added but not executed** in this environment.

Independent Python specification checks executed successfully: 5,456 modeled
write-history interleavings, 11,843 metering boundary cases and the existing
32,400-case old/new query-semantics comparison. These are model checks, not
execution or compilation of the Rust implementation, and do not close a bead.

Cargo, rustc and rustfmt are absent locally; external DNS/network access failed.
`cargo test`, Clippy, formatting and `scripts/check.sh`/committed-tree local proof
are therefore **UNRUN**, not passing. No GitHub Actions workflow was dispatched.
No runtime performance measurement, full SSI, full GQL, replay-completeness or
Genesis-gate claim is made.
