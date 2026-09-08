# Bounded GLA execution: transaction-first migration

Status at 2026-09-08: implemented on `main`, Rust build and tests **unverified**.
Owner: `fgdb-boundplan-gla-lowering-seam-r2kd`, still open.

## Executable slice

`fgdb_gql::algebra::GlaPlan::lower` translates the existing `BoundPlan` into
immutable scan, select, vertex-identity, expand, project, distinct, order and
limit operators. Positional integer-comparison fields are consumed only by the
lowering adapter. Predicate evaluation has one position-independent definition.
The logical transcript is domain-separated application data, not a registered
durable format or a replacement for the existing result certificates.

The evaluator indexes each requested relation/orientation pair once, streams
binding rows through expansion, caches predicate results per operator/vertex,
and collects distinct projected IDs without retaining the Cartesian path
intermediate. Parallel edges remain present until the terminal set projection.
This preserves the bounded API's sorted, unique vertex-ID results; it does not
claim general GQL multiset or path-identity semantics.

`WriteTxn` node and edge MATCH execution now use this evaluator. Text, bound,
owned-prepared, and staged-overlay evidence/cursor entrypoints converge there.
Transaction-owned snapshot admission, ordered staged effects, owner checks and
conservative observed-element dependencies remain in their existing adapters.
A predicate source error propagates without returning partial query rows.

The live, historical and immutable read-view executor in `fgdb/src/gql_exec.rs`
**has not been cut over**. It remains the independent comparison implementation
for the transaction-first migration. No parser grammar, dependency, durable
format, authorization rule or commit protocol was changed.

## Regression coverage and evidence boundary

Added Rust coverage includes an independent small-multigraph enumeration oracle,
single edge admission, predicate memoization, late-read failure, pagination,
canonical lowering, and integer boundary/missing-property behavior.
`fgdb/tests/gla_overlay_integration.rs` adds 118 plan variants compared against
durable and pinned reads before and after ordered transaction staging, including
128-bit IDs, artifact replay, stale-overlay refusal, filtered-out read dependencies
and a disjoint-commit control.

A separate Python old/new semantic specification cross-check executed 32,400
randomized cases successfully. It did not compile or execute the Rust code.
Cargo tests, Clippy, rustfmt, the `/dsr` full gates and repository proof remain
**UNRUN** in the connector environment, which has no Rust toolchain or reachable
build worker. These additions do not close the owning bead or satisfy a Genesis
gate. No throughput measurements are claimed.

## Remaining work

Run the existing and added Rust suites; complete the live/historical/pinned
adapter cutover and retire the legacy inline kernel after differential coverage
passes. Integrate the registered GLA/FreeJoin physical family, authorized Strata
access paths, resource/cancellation/spill contracts and executable-plan evidence.
Typed parameters, general GQL semantics and full SSI are not implemented by this
slice. The scratch indexes here are not a replacement storage engine.
