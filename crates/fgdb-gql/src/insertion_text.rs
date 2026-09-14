//! Parse-once query-selected CREATE templates. The shared MATCH/scalar parser
//! owns syntax; the insertion kernel owns proposals; WriteTxn owns staging.

use crate::{GraphMutationTextError, GraphMutationTextErrorKind, GraphPatternTextError,
    GraphPatternTextErrorKind, PreparedGraphText};
use crate::insertion::{GraphInsertBuildError, GraphInsertEndpoint};
use crate::set_text::ReadValueTemplate;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};

#[derive(Debug)]
pub enum GraphInsertTextErrorKind {
    Query(GraphPatternTextErrorKind),
    Expression(GraphMutationTextErrorKind),
    Build(GraphInsertBuildError),
    RelationCoordinate { expected: RelationId, found: RelationId },
}
#[derive(Debug)]
pub struct GraphInsertTextError {
    pub offset: usize,
    pub kind: GraphInsertTextErrorKind,
}
impl core::fmt::Display for GraphInsertTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph insertion text at byte {}: {:?}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphInsertTextError {}
impl From<GraphPatternTextError> for GraphInsertTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self { offset: error.offset, kind: GraphInsertTextErrorKind::Query(error.kind) }
    }
}
impl From<GraphMutationTextError> for GraphInsertTextError {
    fn from(error: GraphMutationTextError) -> Self {
        let kind = match error.kind {
            GraphMutationTextErrorKind::Query(kind) => GraphInsertTextErrorKind::Query(kind),
            kind => GraphInsertTextErrorKind::Expression(kind),
        };
        Self { offset: error.offset, kind }
    }
}

#[derive(Clone)]
pub(crate) struct InsertVertexTemplate {
    pub labels: Vec<LabelId>,
    pub properties: Vec<(PropertyKeyId, ReadValueTemplate)>,
}
#[derive(Clone)]
pub(crate) struct InsertEdgeTemplate {
    pub source: GraphInsertEndpoint,
    pub destination: GraphInsertEndpoint,
    pub properties: Vec<(PropertyKeyId, ReadValueTemplate)>,
}

/// Native bounded MATCH ... CREATE preparation with explicit identity supply
/// at execution. Declare new named vertices before edge clauses; edge endpoints
/// name matched vertices or those declarations. Every edge names the explicit
/// target relation coordinate. Property values use existing matched bindings,
/// not newly created property state. This is not standalone CREATE, MERGE,
/// inline endpoint creation, mixed-relation insertion or write-returning.
#[derive(Clone)]
pub struct PreparedGraphInsertText {
    pub(crate) selection: PreparedGraphText,
    pub(crate) relation: RelationId,
    pub(crate) vertices: Vec<InsertVertexTemplate>,
    pub(crate) edges: Vec<InsertEdgeTemplate>,
    pub(crate) create_at: usize,
}
impl core::fmt::Debug for PreparedGraphInsertText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphInsertText").field("vertices_per_row", &self.vertices.len())
            .field("edges_per_row", &self.edges.len()).field("definition", &"[REDACTED]").finish()
    }
}
