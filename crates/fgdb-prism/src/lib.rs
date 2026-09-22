//! Prism's snapshot-local compatibility boundary to FrankenNetworkX.
//!
//! [`SnapshotGraphView`] implements the *upstream* [`GraphView`], not a second
//! lookalike trait. Its explicitly labelled `DECODED_CACHE` owns flat `usize`
//! adjacency rows. Stable 128-bit vertex identities are mapped to dense local
//! ordinals; neither identities nor compressed storage ordinals are cast to
//! `usize`. Clones share one immutable projection, including both adjacency
//! directions, its weights, source binding and reduction policy.
//!
//! This is the in-core projection kernel, not storage or an authorization
//! authority. Input rows must already belong to one admitted snapshot. The
//! trusted embedded composition layer supplies them today; the future secure
//! view must supply authorized rows before an untrusted client can invoke this
//! kernel. A caller-supplied source binding is provenance, not a permit.
//!
//! The cache has explicit admission limits. These are not an external-memory
//! implementation, a hard process-memory cap, or a claim of zero-copy access to
//! Strata. Algorithms without a bounded cursor or spill implementation must not
//! acquire the larger-than-memory database-operator exposure merely by using it.

#![forbid(unsafe_code)]

mod projection;

pub use fnx_algorithms::GraphView;
pub use projection::{
    AdapterPath, Directedness, ParallelEdgePolicy, ProjectionEdge, ProjectionError,
    ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
    PROJECTED_WEIGHT_ATTRIBUTE,
};

/// The same immutable foundation revision as the workspace topology registry.
pub const FNX_IMPLEMENTATION_REVISION: &str = "9d710b1c33e99412c94de7fa4de2f7ce4954110f";
