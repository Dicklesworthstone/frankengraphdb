//! Bounded unique-vertex MERGE definition.
//!
//! This is the typed get-or-create core for later native MERGE syntax. One
//! prepared MATCH chooses a vertex-valued target column. Duplicate occurrences
//! of the same identity are one match; two distinct identities are ambiguous.
//! If no target exists, one validated standalone insertion creates exactly one
//! vertex. There are deliberately no relationship MERGEs or ON MATCH/ON CREATE
//! actions in this increment; those compose above this exact primitive.

use crate::algebra::{GraphValueRow, PreparedGraphPattern, ValueProjection};
use crate::insertion::{GraphInsertError, PreparedGraphInsert};
use crate::{GlaExecutionStats, GqlExecutionStats, GqlQueryPolicy};
use fgdb_delta_types::RelationId;
use fgdb_types::VId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphVertexMergeBuildError {
    TargetColumn { column: usize },
    CreationMustBeStandalone,
    CreationMustContainOneVertex { observed: usize },
    CreationMustNotContainEdges { observed: usize },
    RelationMismatch { selection: RelationId, creation: RelationId },
}
impl core::fmt::Display for GraphVertexMergeBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph vertex MERGE definition: {self:?}")
    }
}
impl core::error::Error for GraphVertexMergeBuildError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphVertexMergePolicy {
    /// One cumulative allowance for match selection, uniqueness reduction and
    /// the standalone creation arm when it is needed.
    pub query: GqlQueryPolicy,
}
impl GraphVertexMergePolicy {
    #[must_use]
    pub const fn new(query: GqlQueryPolicy) -> Self { Self { query } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphVertexMergeStats {
    /// The MATCH phase only. A create branch's relational unit is internal and
    /// does not masquerade as a matched row.
    pub match_selection: GqlExecutionStats,
    /// Cumulative match-reduction plus optional creation evaluator work.
    pub evaluator: GlaExecutionStats,
    pub created_vertices: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphVertexMergeOutcome {
    Matched(VId),
    Created(VId),
}
impl GraphVertexMergeOutcome {
    #[must_use]
    pub const fn vertex(self) -> VId {
        match self { Self::Matched(vertex) | Self::Created(vertex) => vertex }
    }
    #[must_use]
    pub const fn created(self) -> bool { matches!(self, Self::Created(_)) }
}
impl core::fmt::Debug for GraphVertexMergeOutcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct(match self { Self::Matched(_) => "Matched", Self::Created(_) => "Created" })
            .field("vertex", &"[REDACTED]").finish()
    }
}

#[derive(Debug)]
pub enum GraphVertexMergeError<E, A> {
    Source(E),
    InvalidSourceStatistics,
    InputSchema { row: usize, column: usize },
    AmbiguousMatches { observed: u64 },
    Creation(GraphInsertError<E, A>),
    AccountingOverflow,
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display for GraphVertexMergeError<E, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::InvalidSourceStatistics => f.write_str("MERGE source returned inconsistent statistics"),
            Self::InputSchema { row, column } => write!(f, "MERGE row {row} has incompatible column {column}"),
            Self::AmbiguousMatches { observed } => write!(f, "MERGE matched {observed} distinct vertices; unique match required"),
            Self::Creation(error) => write!(f, "MERGE creation: {error}"),
            Self::AccountingOverflow => f.write_str("MERGE cumulative resource accounting overflow"),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphVertexMergeError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Creation(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphVertexMerge {
    selection: PreparedGraphPattern<GraphValueRow>,
    relation: RelationId,
    target: usize,
    creation: PreparedGraphInsert,
}
impl core::fmt::Debug for PreparedGraphVertexMerge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexMerge")
            .field("target", &self.target)
            .field("definition", &"[REDACTED]").finish()
    }
}
impl PreparedGraphVertexMerge {
    pub fn prepare(
        selection: PreparedGraphPattern<GraphValueRow>,
        relation: RelationId,
        target: usize,
        creation: PreparedGraphInsert,
    ) -> Result<Self, GraphVertexMergeBuildError> {
        if !matches!(selection.value_columns().get(target), Some(ValueProjection::Vertex { .. })) {
            return Err(GraphVertexMergeBuildError::TargetColumn { column: target });
        }
        if creation.selection().is_some() {
            return Err(GraphVertexMergeBuildError::CreationMustBeStandalone);
        }
        if creation.vertices_per_row() != 1 {
            return Err(GraphVertexMergeBuildError::CreationMustContainOneVertex {
                observed: creation.vertices_per_row(),
            });
        }
        if creation.edges_per_row() != 0 {
            return Err(GraphVertexMergeBuildError::CreationMustNotContainEdges {
                observed: creation.edges_per_row(),
            });
        }
        if creation.relation() != relation {
            return Err(GraphVertexMergeBuildError::RelationMismatch {
                selection: relation, creation: creation.relation(),
            });
        }
        Ok(Self { selection, relation, target, creation })
    }

    #[must_use]
    pub fn selection(&self) -> &PreparedGraphPattern<GraphValueRow> { &self.selection }
    #[must_use]
    pub const fn relation(&self) -> RelationId { self.relation }
    #[must_use]
    pub const fn target_column(&self) -> usize { self.target }
    #[must_use]
    pub fn creation(&self) -> &PreparedGraphInsert { &self.creation }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:unique-vertex-merge:v1\0".to_vec();
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        bytes.extend_from_slice(&(self.target as u64).to_be_bytes());
        for definition in [self.selection.canonical_bytes(), self.creation.canonical_bytes()] {
            bytes.extend_from_slice(&(definition.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&definition);
        }
        bytes
    }
}
