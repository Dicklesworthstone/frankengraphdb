//! Parse-once native vertex MERGE with bounded ON and trailing SET clauses.

use crate::{
    GqlScalarParameter, GraphMutationTextError, GraphMutationTextErrorKind, GraphPatternTextError,
    GraphPatternTextErrorKind, GraphVertexMergeTextErrorKind, GraphVertexUpsertBuildError,
    PreparedGraphVertexMergeText,
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
    Expression {
        key: PropertyKeyId,
        properties: Vec<PropertyKeyId>,
        program: Vec<crate::mutation_text::MutationIntegerTemplateOp>,
        at: usize,
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
    pub(crate) after: Vec<VertexUpsertActionTemplate>,
}
impl core::fmt::Debug for PreparedGraphVertexUpsertText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexUpsertText")
            .field("on_match", &self.on_match.len())
            .field("on_create", &self.on_create.len())
            .field("after", &self.after.len())
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
    Expression(GraphMutationTextErrorKind),
    DuplicateBranch,
    /// A RETURN expression, page or projection name after MERGE.
    Relation(crate::GraphSetTextErrorKind),
    ReturnBuild(crate::GraphVertexUpsertQueryBuildError),
}
impl From<crate::GraphSetTextError> for GraphVertexUpsertTextError {
    fn from(error: crate::GraphSetTextError) -> Self {
        let kind = match error.kind {
            crate::GraphSetTextErrorKind::Pattern(kind) => {
                GraphVertexUpsertTextErrorKind::Query(kind)
            }
            kind => GraphVertexUpsertTextErrorKind::Relation(kind),
        };
        Self {
            offset: error.offset,
            kind,
        }
    }
}
impl From<GraphPatternTextError> for GraphVertexUpsertTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphVertexUpsertTextErrorKind::Query(error.kind),
        }
    }
}
impl From<GraphMutationTextError> for GraphVertexUpsertTextError {
    fn from(error: GraphMutationTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphVertexUpsertTextErrorKind::Expression(error.kind),
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

#[derive(Clone)]
pub(crate) struct VertexReturnTemplate {
    pub bindings: Vec<crate::GraphVertexReturnBinding>,
    pub projection: Vec<crate::set_text::ReadProjectionTemplate>,
    pub grouping: Option<crate::mutation_text::WriteReturnGroupTemplate>,
    pub quantifier: crate::GraphSetQuantifier,
    pub order: Vec<crate::algebra::GraphValueOrder>,
    pub offset: crate::set_text::ReadPageNumber,
    pub count: Option<crate::set_text::ReadPageNumber>,
    pub at: usize,
}

/// `MERGE (n ...) [ON MATCH SET ...] [ON CREATE SET ...] [SET ...] RETURN ...`.
/// MERGE chooses exactly one vertex, so RETURN projects one row: the merge
/// variable's identity and its properties after every clause, read from the
/// staged transaction state, plus expressions, DISTINCT, ordering and paging
/// over them, including grouped COUNT/SUM/AVG/MIN/MAX/COLLECT with argument
/// DISTINCT. A MERGE with no SET clause is accepted here. Grouped output keeps
/// the ordinary row numeric domain; `labels(n)` remains outside this shape.
#[derive(Clone)]
pub struct PreparedGraphVertexUpsertQueryText {
    pub(crate) statement: String,
    pub(crate) upsert: PreparedGraphVertexUpsertText,
    pub(crate) returning: VertexReturnTemplate,
}
impl core::fmt::Debug for PreparedGraphVertexUpsertQueryText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexUpsertQueryText")
            .field("output_columns", &self.returning.projection.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
