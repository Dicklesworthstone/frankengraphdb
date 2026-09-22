# Prism: decoded snapshot analytics

`fgdb-prism` implements the pinned FrankenNetworkX `GraphView` trait over an
immutable, clone-shared `DECODED_CACHE`. Stable 128-bit vertex identities are
mapped to dense snapshot-local indices; compressed storage ordinals are never
cast into fnx indices. Forward/reverse adjacency, isolates, explicit multigraph
reductions and resolved weights belong to the same snapshot binding.

## Embedded entrypoints and registered calls

`Database::call_fnx` and `EmbeddedReadView::call_fnx` bind a registered statement
and execute it with explicit `FnxReadOptions`. `execute_fnx` accepts a reusable
`FnxCallSpec`. A retained view remains the only data source after the writer
advances or drops; `as_of` selects history no later than that view's frontier.

Registry version 2 contains these in-core procedures:

| Procedure | Arguments | Output fields | Projection requirement |
| --- | --- | --- | --- |
| `fnx.pagerank` | `alpha=0.85, max_iter=100, tol=1e-6, weighted=true` | `vertex, score` | Any direction; finite nonnegative weights when weighted |
| `fnx.single_source_shortest_path_length` | Required `source`, `cutoff=NULL` | `vertex, distance` | Any direction; unweighted outgoing reachability |
| `fnx.connected_components` | None | `vertex, component` | Explicitly undirected |
| `fnx.weakly_connected_components` | None | `vertex, component` | Directed or reversed |
| `fnx.strongly_connected_components` | None | `vertex, component` | Directed or reversed |

```text
CALL fnx.pagerank($alpha, 1000, 1e-12, true)
YIELD vertex AS id, score AS rank

CALL fnx.single_source_shortest_path_length($source, 3)
YIELD distance AS hops, vertex

CALL fnx.strongly_connected_components()
YIELD vertex, component AS group_id
```

The projection is the explicitly supplied graph input; no call silently chooses
multigraph or directedness laws. Arguments are positional and omitted trailing
arguments use registered defaults. A source is a full-width `FnxArgument::Vertex`
or a nonnegative integer; decimal literals cover the entire `u128` VId domain
without conversion through f64 or usize. An absent source returns `UnknownSource`.
The optional cutoff is an inclusive nonnegative hop count; zero yields only the
source. Unreachable vertices are omitted, not emitted with a fabricated distance.

All result rows are in ascending VId order. Distance values are exact `u64`
`FnxValue::Integer`s. Each component is represented by one row per member and
labelled with its minimum member VId, never a transient ordinal. Isolates remain
single-vertex components. Components and BFS ignore projected weights; their
source selection/weight recipe remains explicit and is still validated while
building the projection. Use `FnxWeightSpec::Unit` when properties are irrelevant.

`YIELD *`, output reordering, aliases and a single output column are supported.
Each procedure has its own output schema. Unknown procedures, unknown/duplicate
outputs, missing parameters or required arguments, wrong types, invalid numeric
domains and trailing statements refuse during binding, before graph access.
Graph-kind checks refuse incompatible projections instead of silently reducing
them. `FnxCallSpec::algorithm()` exposes the bound typed selector; `options()`
returns `Some(PageRankOptions)` only for PageRank.

The host chooses every graph law and resource allowance; there is deliberately
no default multigraph projection:

```rust
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::*;

let options = FnxReadOptions {
    as_of: None,
    selection: FnxSelection {
        vertex_label: Some(LabelId(1)),
        relation: Some(RelationId(1)),
        weight: FnxWeightSpec::Property {
            key: PropertyKeyId(1),
            missing: MissingWeightPolicy::Reject,
        },
    },
    projection: ProjectionSpec {
        directedness: Directedness::Directed,
        parallel_edges: ParallelEdgePolicy::Sum,
        self_loops: SelfLoopPolicy::Keep,
    },
    source_limits: FnxSourceLimits {
        max_work_units: 1_000_000,
        max_scratch_entries: 100_000,
        max_staging_bytes: 8 * 1024 * 1024,
    },
    projection_limits: ProjectionLimits {
        max_vertices: 10_000,
        max_input_edges: 100_000,
        max_adjacency_entries: 200_000,
        max_workspace_bytes: 64 * 1024 * 1024,
    },
    execution_limits: FnxExecutionLimits {
        max_iterations: 1000,
        max_result_rows: 10_000,
        max_estimated_work: 200_000_000,
    },
};
// Given a Database, QueryCx and FnxParameters:
// let result = database.call_fnx(&query_cx, statement, &parameters, options)?;
// result.analytics contains typed rows and evidence identifying the actual kernel.
```

These numbers are example **admission allowances**, not recommended production
sizing or a hard process-memory quota. The source borrows the same admitted
Strata rows as the native GLA reader, retaining only selected IDs and resolved
weights. A label selection is an induced graph: both endpoints must be visible
and selected. This is not an authorization filter. Relation and endpoint
selection precede weight resolution, so discarded properties cannot make a
selected projection fail. `CollapseUnit` never reads weights; `Drop` never reads
self-loop weights. A property weight accepts finite floats or exactly
representable integers, never silent decimal/string/boolean coercion.

## Execution, resources and evidence

`EmbeddedReadView::prism_projection_at` prepares a cache for several bound calls.
Clones share it without rebuilding adjacency. `projected_row` borrows a target
slice and its position-aligned weights. No executing kernel constructs a second
Graph/DiGraph or normalized adjacency copy. PageRank uses three O(n) f64 vectors;
BFS and components use O(n) arrays and queues. Strong components use iterative
Kosaraju with explicit bounded stacks, not recursive process-stack traversal.

Every kernel checks cancellation during vertex and edge passes, including hub
rows, PageRank iterations and SCC discovery/relabeling. An observed cancellation
or admission failure discards the entire result. These synchronous checkpoints
do not promise async scheduling, interruption of allocation/cache sorting, a
hard deadline or a constant wall-clock checkpoint interval. Nonconvergence and
invalid numeric results are typed errors, not successful scores.

With n projected vertices and m outgoing adjacency entries, execution work
admission is `(n+m)*(max_iter+1)` for PageRank, `n+m` for BFS/CC, `n+2*m` for WCC,
and `2*(n+m)` for SCC. These are declared models, not CPU instruction counts;
source traversal, initialization and output conversion are separate work.
The iteration cap applies only to PageRank. Component/PageRank row counts are
admitted before execution; BFS admits each newly reached row before enqueueing,
so a small reachable result is not rejected solely because many isolates exist.
Kernel workspace evidence reports requested vector backing bytes, excluding the
existing cache, result rows and allocator overhead. It is not a process-memory
quota. Visitor scratch and staging are governed by their separate allowances.

Certificates bind the snapshot, projection, typed call, canonical typed rows,
actual kernel identifier, source-bundle digest, numeric profile, workspace model
and observed traversal counters. The pinned fnx revision identifies the semantic
differential oracle, not a claim that fnx executed a native computation. Native
counters use fnx's `ComplexityWitness` schema. PageRank preserves the pinned fnx
scalar evaluation order; exact traversal outputs are normalized to the registered
VId row and component-label laws. Neither digest is a signed proof, executable
artifact hash, authorization token or durable CGSE record.

The higher-level embedded result additionally binds the selection recipe.
Different roots or historical cuts never share a projection digest, even when
the resulting values coincide.

## Boundaries and validation

This is the **in-core decoded compatibility path**, not a zero-copy compressed
Strata adapter. It does not add secure-view enforcement, PrismRegion scheduling,
external-memory spill, a durable materialization lifecycle or general GQL CALL
composition. `Database::query` is unchanged: use the explicit analytics entrypoint
so graph policies and limits cannot be silently invented. Only registered calls
are executable; no arbitrary fnx procedure is claimed to be supported.

Regression sources cover all 512 three-node topologies in three directions,
weighted/unweighted bitwise PageRank parity, standalone fnx BFS/component parity,
dense-matrix PageRank checks, full-width IDs, multigraph laws, typed refusal,
every available cancellation checkpoint, deep 25,000-vertex SCC chains/cycles,
and real Chronicle/Strata historical generations that survive writer progress.

Run `cargo test -p fgdb-prism`, `cargo test -p fgdb --test prism_bridge` and
`cargo test -p fgdb --test prism_traversal` in the supported Rust environment,
followed by the prescribed formatting, Clippy and registry gates. These Rust
commands have **not been run** in the connector-only implementation environment.
Independent Python algorithm-model comparisons are not Rust execution or W8
acceptance evidence. No W8 acceptance bead is closed by these changes.
