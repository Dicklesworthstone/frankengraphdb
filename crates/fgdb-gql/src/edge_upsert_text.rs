//! Parse-once native relationship MERGE with ON MATCH / ON CREATE SET actions.

use crate::{
    GqlScalarParameter, GraphEdgeMergeTextErrorKind, GraphEdgeUpsertBuildError,
    GraphPatternTextError, GraphPatternTextErrorKind, PreparedGraphEdgeMergeText,
};
use fgdb_delta_types::PropertyKeyId;

#[derive(Clone)]
pub(crate) enum EdgeUpsertValueTemplate {
    Bound(GqlScalarParameter),
    Parameter { index: usize, at: usize },
}
#[derive(Clone)]
pub(crate) struct EdgeUpsertActionTemplate {
    pub(crate) key: PropertyKeyId,
    pub(crate) value: EdgeUpsertValueTemplate,
}

#[derive(Clone)]
pub struct PreparedGraphEdgeUpsertText {
    pub(crate) merge: PreparedGraphEdgeMergeText,
    pub(crate) on_match: Vec<EdgeUpsertActionTemplate>,
    pub(crate) on_create: Vec<EdgeUpsertActionTemplate>,
}
impl core::fmt::Debug for PreparedGraphEdgeUpsertText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeUpsertText")
            .field("on_match", &self.on_match.len())
            .field("on_create", &self.on_create.len())
            .field("definition", &"[REDACTED]").finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEdgeUpsertTextError {
    pub offset: usize,
    pub kind: GraphEdgeUpsertTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeUpsertTextErrorKind {
    Query(GraphPatternTextErrorKind),
    Merge(GraphEdgeMergeTextErrorKind),
    UpsertBuild(GraphEdgeUpsertBuildError),
    DuplicateBranch,
}
impl From<GraphPatternTextError> for GraphEdgeUpsertTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self { offset: error.offset, kind: GraphEdgeUpsertTextErrorKind::Query(error.kind) }
    }
}
impl core::fmt::Display for GraphEdgeUpsertTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph relationship MERGE action text error at byte {}: {:?}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphEdgeUpsertTextError {}

impl PreparedGraphEdgeUpsertText {
    #[must_use]
    pub fn statement(&self) -> &str { self.merge.statement() }
    #[must_use]
    pub fn parameter_schema(&self) -> &[crate::GqlParameterSpec] { self.merge.parameter_schema() }
}
