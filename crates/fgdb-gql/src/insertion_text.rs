//! Parse-once standalone and query-selected CREATE templates. The shared
//! MATCH/scalar parser owns syntax; insertion owns proposals; WriteTxn stages.

use crate::{GqlParameterSpec, GraphMutationTextError, GraphMutationTextErrorKind,
    GraphPatternTextError, GraphPatternTextErrorKind, PreparedGraphText};
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

/// Unit statements retain their actual argument table, not a forged MATCH or
/// an invalid PreparedGraphText. Only a matched input owns graph read syntax.
#[derive(Clone)]
pub(crate) enum InsertTextInput {
    Match(PreparedGraphText),
    Unit {
        statement: String,
        parameters: Vec<GqlParameterSpec>,
        parameter_offsets: Vec<usize>,
    },
}

/// Native bounded CREATE, optionally preceded by MATCH. Standalone CREATE
/// initializes an empty graph or adds one structure without scanning it. Node
/// patterns can declare vertices inline in directed chains, reuse a named node,
/// or create anonymous nodes. A name's first occurrence declares its labels and
/// properties; later references must be bare. Matched nodes cannot be redeclared.
///
/// Every edge names the explicit target relation coordinate. Property values
/// read matched bindings or typed constants/parameters, not freshly created
/// property state. There is no MERGE, undirected/quantified creation, arbitrary
/// mixed-relation insertion, or write-returning in this bounded profile.
#[derive(Clone)]
pub struct PreparedGraphInsertText {
    pub(crate) input: InsertTextInput,
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
