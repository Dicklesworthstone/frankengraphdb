# Prism: decoded snapshot analytics

`fgdb-prism` implements the pinned FrankenNetworkX `GraphView` trait over an
immutable, clone-shared `DECODED_CACHE`. Stable 128-bit vertex identities are
mapped to dense snapshot-local indices; compressed storage ordinals are never
cast into fnx indices. Forward/reverse adjacency, isolates, explicit multigraph
reductions and resolved weights belong to the same snapshot binding.

## Embedded entrypoints

`Database::call_fnx` and `EmbeddedReadView::call_fnx` bind a registered statement
and execute it with an explicit `FnxReadOptions`. `execute_fnx` accepts a reusable
`FnxCallSpec`. A retained view remains the only data source after the writer
advances or drops; `as_of` selects history no later than that view's frontier.

The current registry contains **`fnx.pagerank`**:

```text
CALL fnx.pagerank($alpha, 1000, 1e-12, true)
YIELD vertex AS id, score AS rank
```

Arguments are `alpha`, `max_iter`, `tol`, and `weighted`; omitted trailing
arguments use `0.85`, `100`, `1e-6`, and `true`. The projection is the implicit,
explicitly supplied graph input. `YIELD *` and a single output column are also
supported. Unknown procedures, unknown/duplicate outputs, missing parameters,
wrong types, invalid numerical domains, and trailing statements refuse during
binding, before graph access.

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
// result.analytics contains typed rows and the original fnx complexity witness.
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

`EmbeddedReadView::prism_projection_at` prepares a cache for several bound calls.
Clones share it without rebuilding adjacency. Calling `FnxCallSpec::execute`
directly on that cache returns projection/call/result evidence; the higher-level
embedded result additionally binds the selection recipe. Different roots or
historical cuts never share a projection digest, even when scores coincide.
Neither digest is an authorization token, signed proof, or durable CGSE record.

## Boundaries

This is the **in-core decoded compatibility path**, not a zero-copy Strata
adapter. It does not add secure-view enforcement, PrismRegion scheduling,
external-memory spill, a durable materialization lifecycle, or a general GQL
procedure catalog. `Database::query` is unchanged: use the explicit analytics
entrypoint so graph policies and limits cannot be silently invented. The pinned
fnx revision exposes generic PageRank entrypoints; algorithms whose public APIs
still require concrete fnx graphs are not advertised as implemented calls.

Cancellation is checked throughout borrowed-source traversal and at adapter
boundaries. The unmodified upstream PageRank loop cannot be interrupted in its
interior. Cancellation observed after fnx returns discards the whole result.
Nonconvergence and invalid numeric results also return typed errors, not scores
masquerading as a successful computation. Upstream fnx working allocations and
visitor scratch are not governed by the projection-byte admission model.

## Validation

Regression sources cover all 512 three-node topologies in three directions,
standalone fnx and dense-matrix parity, sparse 128-bit identities, multigraph and
weight laws, immutable caches, typed binding/refusal, all available cancellation
checkpoints, real Chronicle/Strata snapshots, historical property successors,
retirements and writer-independent reads.

Run `cargo test -p fgdb-prism` and `cargo test -p fgdb --test prism_bridge` in the
repository's supported Rust environment, followed by its prescribed formatting,
Clippy and registry gates. These Rust commands have **not been run** in the
connector-only implementation environment; no W8 acceptance bead is closed.
