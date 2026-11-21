# Prism: snapshot analytics

`fgdb-prism` implements the pinned FrankenNetworkX `GraphView` trait over an
immutable, clone-shared `DECODED_CACHE`. Stable 128-bit vertex identities are
mapped to dense snapshot-local indices; compressed storage ordinals are never
cast into fnx indices. Forward/reverse adjacency, isolates, explicit multigraph
reductions and resolved weights belong to the same snapshot binding.
`SealedGraphView` supplies a second execution path through authenticated,
fallible compressed-row cursors. Registered native kernels can consume those
cursors directly, with separate workspace and result admission.

## Embedded entrypoints and registered calls

`Database::call_fnx` and `EmbeddedReadView::call_fnx` bind a registered statement
and execute it with explicit `FnxReadOptions`. `execute_fnx` accepts a reusable
`FnxCallSpec`. A retained view remains the only data source after the writer
advances or drops; `as_of` selects history no later than that view's frontier.

Registry version **36** contains **32 registered procedures**. The table below
is a selection of examples; `FnxSignatureRegistry::signatures()` is the complete
catalog, including each procedure's graph laws, output order and execution
profile.

| Procedure | Arguments | Output fields | Projection requirement |
| --- | --- | --- | --- |
| `fnx.pagerank` | `alpha=0.85, max_iter=100, tol=1e-6, weighted=true` | `vertex, score` | Any direction; finite nonnegative weights when weighted |
| `fnx.single_source_shortest_path_length` | Required `source`, `cutoff=NULL` | `vertex, distance` | Any direction; unweighted outgoing reachability |
| `fnx.connected_components` | None | `vertex, component` | Explicitly undirected |
| `fnx.weakly_connected_components` | None | `vertex, component` | Directed or reversed |
| `fnx.strongly_connected_components` | None | `vertex, component` | Directed or reversed |
| `fnx.topological_sort` | None | `vertex, rank` (`node` aliases `vertex`) | Directed or reversed; any retained directed cycle refuses |
| `fnx.is_directed_acyclic_graph` | None | `acyclic` | Directed or reversed; one Boolean row, including an empty graph |

```text
CALL fnx.pagerank($alpha, 1000, 1e-12, true)
YIELD vertex AS id, score AS rank

CALL fnx.single_source_shortest_path_length($source, 3)
YIELD distance AS hops, vertex

CALL fnx.strongly_connected_components()
YIELD vertex, component AS group_id
```

For these standalone entrypoints, the projection is the explicitly supplied
graph input. Arguments follow the registered positional or named parameter
schema; omitted optional arguments use registered defaults. A source is a full-width `FnxArgument::Vertex`
or a nonnegative integer; decimal literals cover the entire `u128` VId domain
without conversion through f64 or usize. An absent source returns `UnknownSource`.
The optional cutoff is an inclusive nonnegative hop count; zero yields only the
source. Unreachable vertices are omitted, not emitted with a fabricated distance.

Each signature defines its output order. Per-vertex distance and component rows
use ascending VId order; topological rows use the ordering described below.
Distance values are exact `u64`
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

### Dependency order and directed acyclicity

`FnxCallSpec::topological_sort()` returns every projected vertex with its exact,
zero-based `FnxValue::Integer` rank. It visits roots and successors in ascending
full-width VId order, then emits DFS reverse postorder, matching the pinned
FrankenNetworkX procedure. This defines deterministic ties between disconnected
components and independent vertices; it is not a smallest-ready-vertex sort.
Isolates participate in the same ordering. An empty graph yields no ordering rows.

`FnxCallSpec::is_directed_acyclic_graph()` returns exactly one
`FnxValue::Boolean` cell named `acyclic`. An empty graph yields `true`; a directed
cycle anywhere in the admitted projection yields `false`. A cycle causes
topological sorting to return `FnxExecutionError::DirectedCycle`, with no partial
ordering. Both procedures finish checking all projected rows, including later
disconnected components, so discovering a cycle does not hide a later source
error or cancellation.

Both signatures accept directed and reversed projections and reject undirected
projections with the typed graph-kind refusal. A reversed projection orders the
reversed dependencies. The caller's parallel-edge reduction still applies;
weights do not affect the DAG computation after projection admission. A retained
self-loop is a directed cycle. An explicit `SelfLoopPolicy::Drop` removes it
during projection construction. Use unit weights when properties are irrelevant.

Native GQL hosts compose registered analytics with `WHERE`, `WITH`, `RETURN`,
aggregates and paging. For example, `Database::query` accepts:

```text
CALL fnx.topological_sort()
YIELD node, rank
RETURN node, rank

CALL fnx.is_directed_acyclic_graph()
YIELD acyclic
RETURN acyclic
```

The topological CALL and ordinary `ALL` projections preserve its sequence;
explicit `ORDER BY` can choose a different order. The rank remains available for
ordering after other relational operations. GQL uses the host's registered
projection defaults or explicit projection selectors. Pinned read views and
transaction queries use their own source snapshots, including staged topology
in a write transaction. Capability-authorized hosts apply their source masks
before projection and execution, so hidden edges cannot create a visible cycle.

The standalone API makes the host's graph laws and resource allowances explicit:

```rust
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::*;

let options = FnxReadOptions {
    as_of: None,
    selection: FnxSelection {
        vertex_labels: Some(LabelId(1)).into(),
        relations: Some(RelationId(1)).into(),
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
slice and its position-aligned weights. PageRank borrows outgoing CSR offsets
for unweighted degrees. Its scratch includes three O(n) f64 vectors and O(n)
dangling-vertex indices; weighted PageRank also uses O(m) normalized incoming
weights and an O(n) construction cursor. Triangle counts and clustering build
temporary degree-oriented forward CSR rows using O(n+m) scratch, then discover
each triangle once. They skip endpoints and intersections that cannot contain
a triangle. This scratch never becomes a storage or authorization root. BFS
and components use O(n) arrays and queues. Strong components use iterative
Kosaraju with explicit bounded stacks.

The DAG procedures share one iterative DFS state machine across decoded,
compressed and cooperative execution. It uses O(n) color/parent arrays and, for
topological sorting, an O(n) ordinal permutation. Decoded execution retains
integer row positions; compressed execution retains authenticated cursors only
along the active DFS path using the shared frame slabs. Neither driver copies
the graph's adjacency into another graph. Each vertex enters once and each
projected arc is examined once, including when a cycle has been detected.

Every kernel checks cancellation during vertex and edge passes, including hub
rows, PageRank iterations and SCC discovery/relabeling. An observed cancellation
or admission failure discards the entire result. These synchronous checkpoints
do not promise async scheduling, interruption of allocation/cache sorting, a
hard deadline or a constant wall-clock checkpoint interval. Nonconvergence and
invalid numeric results are typed errors, not successful scores.

`execute_sealed` and `execute_sealed_rows` run the DAG kernels over compressed
rows. Their cooperative counterparts use the host's yield callback and a shared
quantum for scalar transitions, raw incidence pulls and output conversion. They
check live control on both sides of a suspension. Collected and retained delivery
produce the same typed cells and result digest; a retained cursor holds completed
kernel values without an additional result-row table. Cooperative certificates
identify the cooperative kernel. Allocation and compressed lookups remain
indivisible operations, so the quantum is not a wall-clock latency guarantee.

With n projected vertices and m outgoing adjacency entries, baseline
full-population work models include `(n+m)*(max_iter+1)` for PageRank, `n+m` for
BFS/CC, `n+2*m` for WCC and `2*(n+m)` for SCC. A certificate records the selected
kernel's actual admission model. DAG scalar admission is
`(12+c)*n + 2*m + c + 32`, where c is the selected output-column count;
compressed execution adds the existing retained-history/directional scan bound.
These are declared models, not CPU instruction counts. Projection/source
admission remains separate. Iteration limits apply to iterative procedures;
the DAG procedures have no iteration parameter. Component/PageRank row counts are
admitted before execution; BFS admits each newly reached row before enqueueing,
so a small reachable result is not rejected solely because many isolates exist.
Topological sorting admits n result rows; acyclicity admits one result row even
when n is zero. The compressed paths also admit result bytes before traversal.
Kernel workspace evidence covers peak vector backing bytes, excluding the
existing cache, result rows and allocator overhead. PageRank records actual
reserved capacities, including temporary construction storage. DAG kernels charge
actual array and frame-slab capacities. Other kernels
use their declared allocation models; the triangle model is a conservative
bound because proven-empty intersections avoid some allocations. This evidence
is not a process-memory quota. Visitor scratch and staging are governed by their
separate allowances.

Certificates bind the snapshot, projection, typed call, canonical typed rows,
actual kernel identifier, source-bundle digest, numeric profile, workspace model
and observed traversal counters. The pinned fnx revision identifies the semantic
differential oracle, not a claim that fnx executed a native computation. Native
counters use fnx's `ComplexityWitness` schema. PageRank preserves the pinned fnx
scalar evaluation order; exact traversal outputs follow their registered row and
component-label laws, including the topological reverse-postorder sequence.
Neither digest is a signed proof, executable
artifact hash, authorization token or durable CGSE record.

The higher-level embedded result additionally binds the selection recipe.
Different roots or historical cuts never share a projection digest, even when
the resulting values coincide.

## Boundaries and validation

The decoded and compressed adapters are snapshot-local execution boundaries.
Authorization belongs to the host's admitted source and live control; a caller's
snapshot binding or projection is provenance, not a permission grant. Secure
hosts mask the source before invoking Prism. Compressed kernels declare their
support explicitly and refuse unavailable execution paths. These in-core arrays
and completed result cursors do not establish an external-memory spill operator,
a durable materialization lifecycle or PrismRegion admission. Only registered
calls are executable; arbitrary fnx procedures are not exposed.

Regression sources cover all 512 three-node topologies in three directions,
weighted/unweighted bitwise PageRank parity, standalone fnx BFS/component parity,
dense-matrix PageRank checks, full-width IDs, multigraph laws, typed refusal,
every available cancellation checkpoint, deep 25,000-vertex SCC chains/cycles,
and real Chronicle/Strata historical generations that survive writer progress.

DAG-specific regression sources add pinned FNX comparisons for every three-node
directed topology, retained self-loops, full-width identities, disconnected ties,
explicit parallel reductions and reversed projections. They cover a 32,768-node
iterative chain, source failures after a cycle, exact resource refusals, empty
Boolean results, cooperative cancellation, native GQL composition, staged
cycles, historical replay and capability masks that hide cyclic topology.

**DAG qualification is pending:** the added tests and execution paths require
the focused checks below and the full workspace gates; their presence alone
does not claim a completed qualification run.

```sh
cargo test -p fgdb-prism --test dag
cargo test -p fgdb-prism dag::
cargo test -p fgdb --test gql_call_fnx dag_
cargo test -p fgdb --test prism_cooperative dag::
cargo test -p fgdb --test authorized_prism dag_procedures
```

Run `cargo test -p fgdb-prism`, `cargo test -p fgdb --test prism_bridge` and
`cargo test -p fgdb --test prism_traversal` in the supported Rust environment,
followed by the prescribed formatting, Clippy and registry gates. The ignored
`forward_csr_paired_incumbent_measurement` test compares the triangle kernel with
its previous native implementation in the same process, including temporary
CSR construction and result consumption. Run it with optimized code and
`--ignored --nocapture --test-threads=1` to inspect every paired sample. These
decoded-kernel measurements do not establish end-to-end throughput, GAPBS
parity or W8 acceptance; the full workspace gates remain separate requirements.
