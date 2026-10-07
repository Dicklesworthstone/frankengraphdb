//! Immutable text-prepared query-selected writes. The native graph lexer owns
//! construction and binding; these facades cannot be executed as reads.

use crate::algebra::GraphValueOrder;
use crate::insertion::GraphInsertBuildError;
use crate::set_text::{ReadPageNumber, ReadProjectionTemplate};
use crate::{
    GqlScalarParameter, GraphDeleteBuildError, GraphEdgeMergeBuildError, GraphIntegerBuildError,
    GraphIntegerOp, GraphMutationAction, GraphMutationBuildError, GraphPatternTextError,
    GraphPatternTextErrorKind, GraphVertexMergeBuildError, PreparedGraphText,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};

#[derive(Clone)]
pub(crate) enum MutationIntegerTemplateOp {
    Bound(GraphIntegerOp),
    Parameter {
        index: usize,
        at: usize,
    },
    /// A static path within one immutable map argument. The selected scalar
    /// becomes an ordinary native operand at binding, before graph execution.
    ParameterField {
        index: usize,
        keys: Box<[Box<str>]>,
        at: usize,
    },
}

impl MutationIntegerTemplateOp {
    /// Append the resolved unbound template: retained program instructions or
    /// a parameter hole identified by its argument index. Values never enter.
    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        match self {
            Self::Bound(op) => {
                bytes.push(0);
                op.append_template_transcript(bytes);
            }
            Self::Parameter { index, .. } => {
                bytes.push(1);
                bytes.extend_from_slice(&(*index as u64).to_be_bytes());
            }
            Self::ParameterField { index, keys, .. } => {
                bytes.push(2);
                bytes.extend_from_slice(&(*index as u64).to_be_bytes());
                bytes.extend_from_slice(&(keys.len() as u64).to_be_bytes());
                for key in keys {
                    bytes.extend_from_slice(&(key.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(key.as_bytes());
                }
            }
        }
    }
}

#[derive(Clone)]
pub(crate) enum MutationActionTemplate {
    Bound(GraphMutationAction),
    ParameterProperty {
        target: usize,
        key: PropertyKeyId,
        parameter: usize,
    },
    IntegerProperty {
        target: usize,
        key: PropertyKeyId,
        program: Vec<MutationIntegerTemplateOp>,
        at: usize,
    },
}

/// MATCH [WALK] ... SET / REMOVE / DETACH DELETE, with one parameter schema and
/// catalog pass. The retained native selection is private: its generated value
/// projection is compiler metadata, never a synthesized RETURN query string.
/// Its original statement bytes are retained only for export and diagnostics.
/// Actions are simultaneous over one pre-statement overlay, not sequential
/// expressions observing preceding assignments.
#[derive(Clone)]
pub struct PreparedGraphMutationText {
    pub(crate) selection: PreparedGraphText,
    pub(crate) relation: RelationId,
    pub(crate) actions: Vec<MutationActionTemplate>,
}
impl core::fmt::Debug for PreparedGraphMutationText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphMutationText")
            .field("actions", &self.actions.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMutationTextError {
    pub offset: usize,
    pub kind: GraphMutationTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphMutationTextErrorKind {
    Query(GraphPatternTextErrorKind),
    Build(GraphMutationBuildError),
    IntegerExpression(GraphIntegerBuildError),
    IntegerOperand,
    IntegerNesting {
        limit: usize,
    },
    /// A RETURN expression, page or projection name after SET/REMOVE.
    Relation(crate::GraphSetTextErrorKind),
    ReturnBuild(crate::GraphMutationQueryBuildError),
}
impl From<GraphPatternTextError> for GraphMutationTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphMutationTextErrorKind::Query(error.kind),
        }
    }
}
impl From<crate::GraphSetTextError> for GraphMutationTextError {
    fn from(error: crate::GraphSetTextError) -> Self {
        let kind = match error.kind {
            crate::GraphSetTextErrorKind::Pattern(kind) => GraphMutationTextErrorKind::Query(kind),
            kind => GraphMutationTextErrorKind::Relation(kind),
        };
        Self {
            offset: error.offset,
            kind,
        }
    }
}
impl core::fmt::Display for GraphMutationTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "graph mutation text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphMutationTextError {}

#[derive(Clone)]
pub(crate) struct MutationReturnTemplate {
    pub bindings: Vec<crate::GraphMutationBinding>,
    pub projection: Vec<ReadProjectionTemplate>,
    pub quantifier: crate::GraphSetQuantifier,
    pub order: Vec<GraphValueOrder>,
    pub offset: ReadPageNumber,
    pub count: Option<ReadPageNumber>,
    pub at: usize,
}

/// One `MATCH ... SET/REMOVE/DETACH DELETE ... RETURN ...` statement. RETURN
/// rows are the statement's own selection occurrences: a matched element's
/// property reads the value the statement assigns to it (REMOVE reads NULL)
/// and otherwise its pre-statement value, so `SET n.p = n.p + 1 RETURN n.p`
/// returns the incremented value. Assignments are simultaneous and
/// conflict-checked, so every read has one answer, never an order-dependent
/// one. RETURN accepts native scalar/CASE/list expressions, graph functions,
/// DISTINCT, output-column ordering, SKIP and LIMIT; paging never limits
/// which occurrences are updated.
///
/// `labels()` after a label action or DETACH DELETE, an edge property after
/// DETACH DELETE (cascades are not visible to the statement) and aggregate
/// RETURN are refused; a vertex property of an element the statement
/// deletes is a typed execution error.
#[derive(Clone)]
pub struct PreparedGraphMutationQueryText {
    pub(crate) statement: String,
    pub(crate) mutation: PreparedGraphMutationText,
    pub(crate) returning: MutationReturnTemplate,
}
impl core::fmt::Debug for PreparedGraphMutationQueryText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphMutationQueryText")
            .field("output_columns", &self.returning.projection.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

/// Native MATCH ... DELETE preparation. The generated vertex projection is
/// compiler metadata and is never inserted into the user's source text. Storage
/// incidence is checked later by the write-capable adapter, not by the parser.
#[derive(Clone)]
pub struct PreparedGraphDeleteText {
    pub(crate) selection: PreparedGraphText,
    pub(crate) relation: RelationId,
    pub(crate) targets: Vec<usize>,
}
impl core::fmt::Debug for PreparedGraphDeleteText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphDeleteText")
            .field("targets", &self.targets.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphDeleteTextError {
    pub offset: usize,
    pub kind: GraphDeleteTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphDeleteTextErrorKind {
    Query(GraphPatternTextErrorKind),
    Build(GraphDeleteBuildError),
}
impl From<GraphPatternTextError> for GraphDeleteTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphDeleteTextErrorKind::Query(error.kind),
        }
    }
}
impl core::fmt::Display for GraphDeleteTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "graph DELETE text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphDeleteTextError {}

#[derive(Clone)]
pub(crate) enum VertexMergeValueTemplate {
    Bound(GqlScalarParameter),
    Parameter { index: usize, at: usize },
}

/// Native bounded `MERGE (n[:Label...] {key:value,...})`. The same property
/// values drive exact-equality MATCH predicates and the single-vertex creation
/// template. This profile intentionally excludes relationship patterns and
/// ON MATCH/ON CREATE actions; those require their own ordered-write semantics.
#[derive(Clone)]
pub struct PreparedGraphVertexMergeText {
    pub(crate) selection: PreparedGraphText,
    pub(crate) relation: RelationId,
    pub(crate) labels: Vec<LabelId>,
    pub(crate) properties: Vec<(PropertyKeyId, VertexMergeValueTemplate)>,
}
impl core::fmt::Debug for PreparedGraphVertexMergeText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexMergeText")
            .field("labels", &self.labels.len())
            .field("properties", &self.properties.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphVertexMergeTextError {
    pub offset: usize,
    pub kind: GraphVertexMergeTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphVertexMergeTextErrorKind {
    Query(GraphPatternTextErrorKind),
    InsertBuild(GraphInsertBuildError),
    MergeBuild(GraphVertexMergeBuildError),
}
impl From<GraphPatternTextError> for GraphVertexMergeTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphVertexMergeTextErrorKind::Query(error.kind),
        }
    }
}
impl core::fmt::Display for GraphVertexMergeTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "graph vertex MERGE text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphVertexMergeTextError {}

/// Native `MATCH ... MERGE (a)-[:R]->(b)` over already-bound endpoint variables.
/// Relationship properties are intentionally absent: the current typed edge
/// MERGE matches relation+endpoints only, so accepting a property map here would
/// silently misstate pattern identity semantics.
#[derive(Clone)]
pub struct PreparedGraphEdgeMergeText {
    pub(crate) selection: PreparedGraphText,
    pub(crate) relation: RelationId,
    pub(crate) source: usize,
    pub(crate) destination: usize,
}
impl core::fmt::Debug for PreparedGraphEdgeMergeText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeMergeText")
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphEdgeMergeTextError {
    pub offset: usize,
    pub kind: GraphEdgeMergeTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeMergeTextErrorKind {
    Query(GraphPatternTextErrorKind),
    Build(GraphEdgeMergeBuildError),
}
impl From<GraphPatternTextError> for GraphEdgeMergeTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphEdgeMergeTextErrorKind::Query(error.kind),
        }
    }
}
impl core::fmt::Display for GraphEdgeMergeTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "graph relationship MERGE text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphEdgeMergeTextError {}
