//! Prism's snapshot-local compatibility boundary to FrankenNetworkX.
//!
//! [`SnapshotGraphView`] implements the *upstream* [`GraphView`], not a second
//! lookalike trait. Its explicitly labelled `DECODED_CACHE` owns flat `usize`
//! adjacency rows. Stable 128-bit vertex identities are mapped to dense local
//! ordinals; neither identities nor compressed storage ordinals are cast to
//! `usize`. Clones share one immutable projection, including both adjacency
//! directions, its weights, source binding and reduction policy.
//!
//! [`SealedGraphView`] instead borrows Strata's authenticated compressed rows
//! through a fallible cursor. It retains a vertex directory and degrees, not
//! flat adjacency. That cursor is deliberately not disguised as the upstream
//! infallible slice contract; unsupported projection directions fail closed.
//!
//! This is the in-core projection kernel, not storage or an authorization
//! authority. Input rows must already belong to one admitted snapshot. The
//! trusted embedded composition layer supplies them today; the future secure
//! view must supply authorized rows before an untrusted client can invoke this
//! kernel. A caller-supplied source binding is provenance, not a permit.
//!
//! The adapters have explicit admission limits. These are not external-memory
//! implementations or hard process-memory caps. Algorithms without a bounded
//! cursor or spill implementation must not acquire the larger-than-memory
//! database-operator exposure merely by using them.

#![forbid(unsafe_code)]

mod call;
mod clustering;
mod execute;
mod input;
mod projection;
mod sealed;
mod sealed_execute;
mod shortest_path;
mod traversal;

pub use call::{
    FNX_DISCRETE_PROFILE, FNX_NUMERIC_PROFILE, FNX_SIGNATURE_REGISTRY_VERSION, FnxAlgorithm,
    FnxArgument, FnxBindError, FnxBindErrorKind, FnxCallSpec, FnxGraphKind, FnxImplementationClass,
    FnxOutput, FnxOutputColumn, FnxParameterSpec, FnxParameterType, FnxParameters, FnxSignature,
    FnxSignatureRegistry, MAX_FNX_CALL_BYTES, PageRankOptions,
};
pub use clustering::{TriangleStatistics, triangle_statistics};
pub use execute::{
    ComplexityWitness, FnxCertificate, FnxExecutionError, FnxExecutionLimits, FnxResult, FnxValue,
};
pub use input::{
    FnxReadError, FnxReadOptions, FnxReadResult, FnxSealedReadError, FnxSelection, FnxSourceLimits,
    FnxWeightError, FnxWeightSpec, MissingWeightPolicy,
};
pub use sealed::{
    SealedGraphView, SealedNeighborCursor, SealedProjectionError, SealedProjectionSpec,
};
pub use sealed_execute::{FnxMemoryLimits, FnxSealedExecutionError};
pub use shortest_path::{
    DijkstraComparison, DijkstraOptions, DijkstraOutput, FNX_DIJKSTRA_EPSILON, dijkstra,
};

pub use fnx_algorithms::GraphView;
pub use projection::{
    AdapterPath, Directedness, PROJECTED_WEIGHT_ATTRIBUTE, ParallelEdgePolicy,
    ProjectionBuildError, ProjectionEdge, ProjectionError, ProjectionLimits, ProjectionSpec,
    SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
};

/// The same immutable foundation revision as the workspace topology registry.
pub const FNX_IMPLEMENTATION_REVISION: &str = "9d710b1c33e99412c94de7fa4de2f7ce4954110f";
