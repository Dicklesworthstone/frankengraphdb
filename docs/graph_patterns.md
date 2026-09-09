# Typed connected graph patterns

Source implemented on unreleased `main`, September 9, 2026. Native Rust build,
tests and repository gates for this continuation are **unverified here**.
Owners `fgdb-boundplan-gla-lowering-seam-r2kd` and `fgdb-w10-embedded-54r` remain
open pending their complete acceptance evidence.

## Capability

`fgdb_gql::algebra::GraphPatternBuilder` prepares connected, positive graph
patterns that are not limited to the legacy text parser's two edge positions.
It supports longer fixed-length paths, branching motifs, closed walks, chords,
self-loops, mixed edge directions, predicates on any declared vertex, and
explicit equality/inequality between any pair of declared vertices. Any one
vertex may be projected, with sorted distinct IDs and offset/count pagination.

The prepared result is `PreparedGraphPattern`. It lowers to the existing
`GlaPlan` operators: `ScanVertices`, `ScanEdges`, `Expand`, `Select`,
`VertexIdentity`, `Project`, `Distinct`, `OrderByVertexId`, and `Limit`.
There is no second executor, graph extraction loop, parser-interprets-AST path,
new positional `BoundPlan` fields, or extra dependency.

**This is a typed Rust preparation API, not an extension of the GQL text
syntax.** It uses caller-resolved label/property/relation IDs in the same sense
as the existing bound-plan API. It neither resolves catalog names nor grants
authorization. The text parser, numeric parameter templates and their existing
statement/evidence contracts are unchanged.

## Preparation

Declare each variable once. Reuse its name in any number of edge atoms and
filters. Edge direction is relative to the two named endpoints: `Forward`
means source to destination, `Reverse` means destination to source, and
`Undirected` permits either orientation.

```rust
use fgdb_gql::algebra::{GlaDirection, GraphPatternBuilder};

let mut builder = GraphPatternBuilder::new();
for name in ["a", "b", "c", "d"] {
    builder.vertex(name)?;
}
builder.edge("a", r, GlaDirection::Forward, "b")?;
builder.edge("c", s, GlaDirection::Reverse, "d")?;
builder.edge("b", r, GlaDirection::Forward, "c")?;
builder.edge("d", t, GlaDirection::Forward, "a")?;
builder.identity("a", "d", false)?;
let pattern = builder.prepare("d", 0, Some(100))?;
let result = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
```

The first edge seeds a binding row. The compiler then selects the first
remaining edge connected to an already bound variable. If only its destination
is bound, it reverses the traversal direction, not the atom's meaning. It can
therefore defer an initially disconnected atom until a later connector makes
it available. Every declared vertex must ultimately belong to the same
edge-connected component; a missing connector produces `Disconnected`, never
a silently omitted component or implicit Cartesian product.

A previously bound endpoint becomes an explicit identity check after expansion.
A newly bound endpoint receives its predicates once. Explicit equalities and
inequalities are emitted as soon as both variables are available. A single
vertex with no edges is a deliberate vertex scan and may be unlabeled; more
than one edgeless variable refuses.

The definition ceilings are structural admission bounds, not performance SLOs:
64 edge atoms, 65 declared vertices, 256 total predicates, 64 explicit identity
constraints, and 128 ASCII bytes per variable name. Names follow
`[A-Za-z_][A-Za-z0-9_]*`. Every builder mutator validates its arguments and limits
before changing state. Missing, duplicate and invalid names, disconnected
patterns and excess definitions have typed errors that do not echo data.

An immutable prepared pattern does not change when its builder is subsequently
edited. Prepared/builder Debug output redacts definitions. `plan()` and
`canonical_bytes()` are explicit exports, not redacted logging interfaces.
Names disappear from lowering, so alpha-renaming preserves the transcript.
This does not claim a full graph-isomorphism or logical-orbit canonicalizer:
edge ordering can choose a different logical pipeline for an equivalent pattern.

## Semantics and cost

The current contract is a connected positive pattern with **vertex identity**
constraints. The same variable always means the same vertex. Different names
may bind the same vertex unless an inequality disallows it. An edge atom may
reuse the edge occurrence that satisfies another atom. A self-loop atom must
bind equal endpoints.

Parallel edge occurrences survive in the evaluator until the terminal distinct
projection. The returned value is a sorted unique vector of the selected
vertex IDs, **not a binding table, bag, path object, cycle enumeration, or count
of matching paths**. Consequently, the operation does not assert TRAIL,
ACYCLIC, SIMPLE, edge-variable identity, or the default morphism semantics of
the complete GQL language. WALK-like fixed-length reuse is explicit here.

Labels and integer comparisons use the same `VertexPredicate` implementation
as existing queries. Missing and noninteger properties fail integer comparisons,
including NotEqual; no new numeric coercion rule is introduced. A zero return
count is legal and yields an empty result after ordinary admission and resource
checks. `LIMIT` does not excuse unlimited work finding the answer.

The compiler's deterministic connected-edge order is not a cost-based optimizer
or the registered FreeJoin physical family. Each atom still uses existing
scan/index/expansion machinery. A 64-edge definition is finite but may enumerate
an enormous number of bindings on a dense or cyclic graph; the edge ceiling is
not a promise of cheap execution. Product execution requires a policy.

## Product reads

`Database` and `EmbeddedReadView` expose:

- `execute_graph_pattern_governed(query_cx, pattern, policy)`;
- `execute_graph_pattern_governed_at(query_cx, pattern, sequence, policy)`.

`WriteTxn` exposes
`execute_graph_pattern_governed(database, query_cx, pattern, policy)`.

All five return the existing `GqlQueryExecution`, containing the projected IDs,
source/result row counts, and work/scratch counters from that execution. Errors
use the existing `GqlQueryError` source/row/evaluator/interruption distinction.
Source, ownership, health and frontier refusals precede cancellation/resource
refusals in the same manner as the existing governed query APIs. No query
result is returned on failure.

The durable methods admit the same immutable snapshot and use the same borrowed
source as text queries. Historical reads keep their exact sequence. Pinned
views cannot observe a later generation. The transaction method folds the same
canonical prepared effects over its original basis and borrows property values;
it never reinterprets raw intentions to create rows.

Source and evaluator consume one `GqlQueryPolicy` allowance. SnapshotRecords
counts the whole admitted base table or final transaction overlay, not only
the relations surviving pattern filters. Final result rows are charged after
distinct/order/pagination. Work/scratch accounting, interruptible adjacency
ordering, terminal checkpoints and error translation are shared with existing
queries. Physical counters can differ across layouts and read surfaces even
when logical answers agree.

Transaction reads retain their existing conservative edge-table, element and
endpoint dependencies. Node-only patterns with a positive required label retain
a label-scoped insertion witness; an unlabeled node pattern needs a whole-table
witness. A later refusal cannot erase already admitted dependencies. A new
matching disconnected path/cycle can conflict with an earlier empty edge scan.
An unrelated vertex-only insertion need not conflict with an edge scan, and an
unrelated unlabeled insertion need not conflict with a label-scoped node scan.

The pattern is not stuffed into a legacy two-hop `BoundPlan` merely to obtain
an existing certificate. No new public artifact/audit API is advertised for
these typed patterns. Their canonical logical bytes are application identity,
not snapshot evidence, an authorization token, or a registered replay manifest.

## Complete example

`crates/fgdb/examples/graph_patterns.rs` initializes a multi-relation graph in
one atomic write, prepares a five-edge branching cycle with a chord and a risk
predicate, checks exact execution limits, evaluates a staged update, commits,
checks historical and pinned reads, and exercises real QueryCx cancellation.

```text
cargo run -p fgdb --example graph_patterns
```

The example is committed but **unrun in this environment**.

## Verification and remaining work

Twelve Rust tests were added: six builder/lowering laws and six public
integration tests. The lowering oracle enumerates complete variable assignments
independently of GLA slot allocation, traversal indexes and edge scheduling.
The product oracle evaluates assignments over ordinary owned storage rows,
independently of the new pattern compiler and borrowed query source.
Coverage includes mixed directions, deferred connected atoms, cycles, branching,
self-loops, all projections, parallel edges, arbitrary identities, the 64-edge
ceiling, alpha-renaming, failed-edit atomicity, every low-level interruption
checkpoint, exact/one-below limits, historical and staged changes, compaction,
reopen, ensures, cascades, phantom retention and authority error precedence.
Existing source/evaluator/transaction tests were retained.

These tests and the example are **ADDED BUT UNRUN** here. Cargo, rustc and
rustfmt are unavailable. There is no claimed passing native build, Clippy,
formatting, exact-tree proof or repository gate. No hosted workflow was
dispatched and no bead was closed on source inspection.

A separate Python specification model passed 15,147 projected-variable
comparisons across 2,304 exhaustive graph/pattern combinations and 2,000
randomized connected patterns, plus directed maximum-depth controls at 3, 10,
32 and 64 edges. An initial model run timed out while enumerating a 64-edge
mixed-direction fixture; maximum-depth controls were changed to directed paths,
while the bounded mixed-direction and cyclic cases remained. That finite model
does not execute Rust, source admission, transaction validation or durability.

Remaining: text grammar/binder integration for general patterns, general
binding-table outputs, typed pattern evidence and catalog contracts, registered
FreeJoin/authorized-Strata physical access, variable-length/path-mode semantics,
spill and byte-accurate whole-operation resource governance. The existing
64-edge executor is not larger-than-memory storage or a complete GQL engine.
