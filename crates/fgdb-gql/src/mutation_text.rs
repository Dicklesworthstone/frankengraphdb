//! Immutable text-prepared query-selected writes. The native graph lexer owns
//! construction and binding; these facades cannot be executed as reads.

use crate::insertion::GraphInsertBuildError;
use crate::{
    GqlScalarParameter, GraphDeleteBuildError, GraphEdgeMergeBuildError, GraphIntegerBuildError,
    GraphIntegerOp, GraphMutationAction, GraphMutationBuildError, GraphPatternTextError,
    GraphPatternTextErrorKind, GraphVertexMergeBuildError, PreparedGraphText,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};

#[derive(Clone)]
pub(crate) enum MutationIntegerTemplateOp {
    Bound(GraphIntegerOp),
    Parameter { index: usize, at: usize },
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
    IntegerNesting { limit: usize },
}
impl From<GraphPatternTextError> for GraphMutationTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphMutationTextErrorKind::Query(error.kind),
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
