//! Parse-once native vertex MERGE with bounded ON MATCH / ON CREATE actions.

use crate::{
    GqlScalarParameter, GraphPatternTextError, GraphPatternTextErrorKind,
    GraphVertexMergeTextErrorKind, GraphVertexUpsertBuildError, PreparedGraphVertexMergeText,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};

#[derive(Clone)]
pub(crate) enum VertexUpsertValueTemplate {
    Bound(GqlScalarParameter),
    Parameter { index: usize, at: usize },
}

#[derive(Clone)]
pub(crate) enum VertexUpsertActionTemplate {
    Property {
        key: PropertyKeyId,
        value: VertexUpsertValueTemplate,
    },
    Label {
        label: LabelId,
        present: bool,
    },
}

#[derive(Clone)]
pub struct PreparedGraphVertexUpsertText {
    pub(crate) merge: PreparedGraphVertexMergeText,
    pub(crate) on_match: Vec<VertexUpsertActionTemplate>,
    pub(crate) on_create: Vec<VertexUpsertActionTemplate>,
}
impl core::fmt::Debug for PreparedGraphVertexUpsertText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexUpsertText")
            .field("on_match", &self.on_match.len())
            .field("on_create", &self.on_create.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphVertexUpsertTextError {
    pub offset: usize,
    pub kind: GraphVertexUpsertTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphVertexUpsertTextErrorKind {
    Query(GraphPatternTextErrorKind),
    Merge(GraphVertexMergeTextErrorKind),
    UpsertBuild(GraphVertexUpsertBuildError),
    DuplicateBranch,
}
impl From<GraphPatternTextError> for GraphVertexUpsertTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphVertexUpsertTextErrorKind::Query(error.kind),
        }
    }
}
impl core::fmt::Display for GraphVertexUpsertTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "graph vertex MERGE action text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphVertexUpsertTextError {}

impl PreparedGraphVertexUpsertText {
    #[must_use]
    pub fn statement(&self) -> &str {
        self.merge.statement()
    }
    #[must_use]
    pub fn relation(&self) -> RelationId {
        self.merge.relation
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[crate::GqlParameterSpec] {
        self.merge.parameter_schema()
    }
}
