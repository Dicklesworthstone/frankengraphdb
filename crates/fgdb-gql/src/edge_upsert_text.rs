//! Parse-once native relationship MERGE with computed ON and trailing SET clauses.

use crate::{
    GqlScalarParameter, GraphEdgeMergeTextErrorKind, GraphEdgeUpsertBuildError,
    GraphMutationTextError, GraphMutationTextErrorKind, GraphPatternTextError,
    GraphPatternTextErrorKind, PreparedGraphEdgeMergeText,
};
use fgdb_delta_types::PropertyKeyId;

#[derive(Clone)]
pub(crate) enum EdgeUpsertValueTemplate {
    Bound(GqlScalarParameter),
    Parameter {
        index: usize,
        at: usize,
    },
    Expression {
        properties: Vec<PropertyKeyId>,
        program: Vec<crate::mutation_text::MutationIntegerTemplateOp>,
        at: usize,
    },
    /// A constant list value (`['a', 'b']`, `$tags`), read in the value
    /// language and evaluated when the statement binds (fgdb-k3nub).
    Composite {
        value: crate::set_text::ReadValueTemplate,
        at: usize,
    },
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
    pub(crate) after: Vec<EdgeUpsertActionTemplate>,
}
impl core::fmt::Debug for PreparedGraphEdgeUpsertText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeUpsertText")
            .field("on_match", &self.on_match.len())
            .field("on_create", &self.on_create.len())
            .field("after", &self.after.len())
            .field("definition", &"[REDACTED]")
            .finish()
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
    Expression(GraphMutationTextErrorKind),
    DuplicateBranch,
    Relation(crate::GraphSetTextErrorKind),
    ReturnBuild(crate::GraphEdgeUpsertQueryBuildError),
}
impl From<GraphPatternTextError> for GraphEdgeUpsertTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphEdgeUpsertTextErrorKind::Query(error.kind),
        }
    }
}
impl From<GraphMutationTextError> for GraphEdgeUpsertTextError {
    fn from(error: GraphMutationTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphEdgeUpsertTextErrorKind::Expression(error.kind),
        }
    }
}
impl core::fmt::Display for GraphEdgeUpsertTextErrorKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Query(kind) => kind.fmt(f),
            Self::Merge(kind) => kind.fmt(f),
            Self::UpsertBuild(error) => error.fmt(f),
            Self::Expression(kind) => kind.fmt(f),
            Self::DuplicateBranch => f.write_str("MERGE repeats an ON CREATE or ON MATCH branch"),
            Self::Relation(kind) => kind.fmt(f),
            Self::ReturnBuild(error) => error.fmt(f),
        }
    }
}
impl GraphEdgeUpsertTextErrorKind {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        match self {
            Self::Query(kind) => kind.class(),
            Self::Merge(kind) => kind.class(),
            Self::Expression(kind) => kind.class(),
            Self::Relation(kind) => kind.class(),
            Self::DuplicateBranch => crate::GqlRefusalClass::Syntax,
            Self::UpsertBuild(_) | Self::ReturnBuild(_) => crate::GqlRefusalClass::Statement,
        }
    }
}
impl GraphEdgeUpsertTextError {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        self.kind.class()
    }
}
impl core::fmt::Display for GraphEdgeUpsertTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "byte {}: {}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphEdgeUpsertTextError {}

impl PreparedGraphEdgeUpsertText {
    #[must_use]
    pub fn statement(&self) -> &str {
        self.merge.statement()
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[crate::GqlParameterSpec] {
        self.merge.parameter_schema()
    }
}

impl From<crate::GraphSetTextError> for GraphEdgeUpsertTextError {
    fn from(error: crate::GraphSetTextError) -> Self {
        let kind = match error.kind {
            crate::GraphSetTextErrorKind::Pattern(kind) => {
                GraphEdgeUpsertTextErrorKind::Query(kind)
            }
            kind => GraphEdgeUpsertTextErrorKind::Relation(kind),
        };
        Self {
            offset: error.offset,
            kind,
        }
    }
}

#[derive(Clone)]
pub(crate) struct EdgeReturnTemplate {
    pub bindings: Vec<crate::GraphEdgeReturnBinding>,
    pub projection: Vec<crate::set_text::ReadProjectionTemplate>,
    pub grouping: Option<crate::mutation_text::WriteReturnGroupTemplate>,
    pub quantifier: crate::GraphSetQuantifier,
    pub order: Vec<crate::algebra::GraphValueOrder>,
    pub offset: crate::set_text::ReadPageNumber,
    pub count: Option<crate::set_text::ReadPageNumber>,
    pub at: usize,
    /// Native row stages over the WITH head's own post-action bindings.
    pub tail: Option<Vec<crate::set_text::ReadStageTemplate>>,
}

/// Parse once, bind repeatedly: MATCH-fed directed relationship MERGE, optional
/// ON/SET clauses, then RETURN or WITH over the chosen edge's final properties.
/// WITH continues through the native row pipeline within the same statement.
/// Scalar/collection expressions, grouped aggregates, DISTINCT and pagination
/// use the ordinary RETURN compiler. This is the unique-endpoint-pair profile;
/// endpoint RETURN, wildcard/property-map reads, undirected MERGE and MERGE
/// identity property maps are not admitted here. No text or authority executes
/// during preparation, and unsupported syntax is never discarded.
#[derive(Clone)]
pub struct PreparedGraphEdgeUpsertQueryText {
    pub(crate) statement: String,
    pub(crate) upsert: PreparedGraphEdgeUpsertText,
    pub(crate) returning: EdgeReturnTemplate,
}
impl core::fmt::Debug for PreparedGraphEdgeUpsertQueryText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeUpsertQueryText")
            .field("output_columns", &self.returning.projection.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
