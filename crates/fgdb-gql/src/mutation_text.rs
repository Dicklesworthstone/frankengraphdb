//! Immutable text-prepared query-selected writes. The native graph lexer owns
//! construction and binding; these facades cannot be executed as reads.

use crate::algebra::GraphValueOrder;
use crate::insertion::GraphInsertBuildError;
use crate::set_text::{ReadPageNumber, ReadProjectionTemplate, ReadValueTemplate};
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
    /// `IN $list` (fgdb-dn9q3): the list argument's members become scalar
    /// operands and one InList over them at binding, so a membership over a
    /// list argument is the literal list's.
    ListMembers {
        index: usize,
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
            Self::ListMembers { index, .. } => {
                bytes.push(3);
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
    /// A value-language RHS over the same frozen selection. Binding retains
    /// typed parameter holes and pinned temporal capabilities until it emits
    /// the existing checked GraphMutationValue::Composite execution plan.
    CompositeProperty {
        target: usize,
        key: PropertyKeyId,
        value: crate::set_text::ReadValueTemplate,
    },
    /// A value-language right-hand side over the WITH row of a write after
    /// WITH (fgdb-sl4ym), read from the bound row relation.
    ValueProperty {
        target: usize,
        key: PropertyKeyId,
        value: crate::set_text::ReadValueTemplate,
        at: usize,
    },
    /// A computed whole-map merge or replacement. The catalog request
    /// shares the statement's frozen resolution slot with metadata reads.
    SetProperties {
        target: usize,
        value: crate::set_text::ReadValueTemplate,
        current: Option<crate::set_text::ReadValueTemplate>,
        catalog: crate::set_text::ElementCatalogSlot,
        at: usize,
    },
    /// DELETE or DETACH DELETE of a value the WITH row computes (TCK
    /// Delete5): `friends[$i]`, `map.key`.
    DeleteValue {
        value: crate::set_text::ReadValueTemplate,
        detach: bool,
        at: usize,
    },
}

#[derive(Clone)]
pub(crate) enum MutationTextInput {
    Pattern(Box<PreparedGraphText>),
    /// A complete native read relation. Its first leaf may be the previous
    /// write's owned rows; no graph pattern is invented for that input.
    Relation {
        statement: String,
        input: Box<crate::set_text::multipart::BoundReadInput>,
    },
}

/// A native read relation feeding SET / REMOVE / DELETE, with one parameter
/// schema and preparation-time catalog binding. The retained input is private:
/// its value projection is compiler metadata, never a synthesized RETURN query.
/// Original statement bytes are retained only for export and diagnostics.
/// Actions are simultaneous over one frozen input, not sequential expressions
/// observing preceding assignments.
#[derive(Clone)]
pub struct PreparedGraphMutationText {
    pub(crate) input: MutationTextInput,
    pub(crate) relation: RelationId,
    pub(crate) actions: Vec<MutationActionTemplate>,
    /// A leading OPTIONAL MATCH: binding left-joins the unit relation with
    /// the selection, so an empty match still yields one all-NULL row.
    pub(crate) optional_root: bool,
}
impl core::fmt::Debug for PreparedGraphMutationText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphMutationText")
            .field("actions", &self.actions.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

/// A chain step's SET, REMOVE or DELETE over a complete native read relation
/// (fgdb-cpny9). A later step's first leaf holds the previous write's rows;
/// further reads observe its staged graph. Actions address the final row.
#[derive(Clone)]
pub(crate) struct PreparedGraphMutationRowsText {
    pub(crate) relation: RelationId,
    pub(crate) actions: Vec<MutationActionTemplate>,
    pub(crate) input: crate::set_text::multipart::BoundReadInput,
    /// The write's own clause, where its binding errors point.
    pub(crate) at: usize,
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
impl core::fmt::Display for GraphMutationTextErrorKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Query(kind) => kind.fmt(f),
            Self::Build(error) => error.fmt(f),
            Self::IntegerExpression(error) => error.fmt(f),
            Self::IntegerOperand => f.write_str("expected an integer operand"),
            Self::IntegerNesting { limit } => {
                write!(f, "integer expression nests deeper than {limit} levels")
            }
            Self::Relation(kind) => kind.fmt(f),
            Self::ReturnBuild(error) => error.fmt(f),
        }
    }
}
impl GraphMutationTextErrorKind {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        use crate::GqlRefusalClass as C;
        match self {
            Self::Query(kind) => kind.class(),
            Self::Relation(kind) => kind.class(),
            Self::IntegerOperand => C::Syntax,
            Self::Build(_)
            | Self::IntegerExpression(_)
            | Self::IntegerNesting { .. }
            | Self::ReturnBuild(_) => C::Statement,
        }
    }
}
impl GraphMutationTextError {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        self.kind.class()
    }
}
impl core::fmt::Display for GraphMutationTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "byte {}: {}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphMutationTextError {}

/// Shared unbound grouping over a write statement's own frozen occurrences.
/// Inputs contain native value expressions; output slots live in the enclosing
/// RETURN projection, after key-first native aggregation.
#[derive(Clone)]
pub(crate) struct WriteReturnGroupTemplate {
    pub inputs: Vec<ReadProjectionTemplate>,
    pub keys: Vec<usize>,
    pub aggregates: Vec<crate::set_text::aggregate::ReadAggregateSpec>,
}

impl WriteReturnGroupTemplate {
    /// The raw write binding a WITH item carries unchanged. A grouped item
    /// must be a direct identity key, never a MIN/MAX/COLLECT representative.
    /// The caller still checks the binding's actual vertex/edge domain.
    pub(crate) fn carried_binding(
        group: Option<&Self>,
        value: &ReadValueTemplate,
    ) -> Option<usize> {
        let ReadValueTemplate::Column(column) = value else {
            return None;
        };
        let Some(group) = group else {
            return Some(*column);
        };
        let input = *group.keys.get(*column)?;
        match &group.inputs.get(input)?.value {
            ReadValueTemplate::Column(binding) => Some(*binding),
            _ => None,
        }
    }

    /// Retain a final-state field of an already-kept identity as a hidden
    /// grouping key. It is a function of that identity, so it cannot split
    /// its group or change occurrence counts. No representative is chosen.
    /// Native aggregate arguments keep their input slots; only key-first
    /// output slots move when a new key precedes the summaries.
    pub(crate) fn hoist_property(
        &mut self,
        projection: &mut [ReadProjectionTemplate],
        binding: usize,
        at: usize,
    ) -> Result<usize, GraphPatternTextError> {
        let existing = self.inputs.iter().position(
            |input| matches!(input.value, ReadValueTemplate::Column(column) if column == binding),
        );
        let input = existing.unwrap_or(self.inputs.len());
        if let Some(key) = self.keys.iter().position(|&column| column == input) {
            return Ok(key);
        }
        let old_keys = self.keys.len();
        let width = old_keys + self.aggregates.len();
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        let observed = (self.inputs.len() + usize::from(existing.is_none())).max(width + 1);
        if observed > limit {
            return Err(GraphPatternTextError {
                offset: at,
                kind: GraphPatternTextErrorKind::Build(
                    crate::algebra::PatternBuildError::LimitExceeded {
                        dimension: crate::algebra::PatternLimitDimension::Columns,
                        limit,
                        observed,
                    },
                ),
            });
        }
        // Validate every nested output before changing the grouping shape.
        // Remap native expression references, including compound summaries.
        Self::remap_hidden_key_outputs(projection, old_keys, width, at)?;
        if existing.is_none() {
            self.inputs.push(ReadProjectionTemplate {
                name: format!("__write_input_{input}"),
                value: ReadValueTemplate::Column(binding),
            });
        }
        self.keys.push(input);

        Ok(old_keys)
    }
}

#[derive(Clone)]
pub(crate) struct MutationReturnTemplate {
    pub bindings: Vec<crate::GraphMutationBinding>,
    pub projection: Vec<ReadProjectionTemplate>,
    pub grouping: Option<WriteReturnGroupTemplate>,
    pub quantifier: crate::GraphSetQuantifier,
    pub order: Vec<GraphValueOrder>,
    pub offset: ReadPageNumber,
    pub count: Option<ReadPageNumber>,
    pub at: usize,
    /// The row stages after a WITH head (`SET ... WITH n WHERE ... RETURN`),
    /// over the head's own output columns, hidden boundary reads included.
    pub tail: Option<Vec<crate::set_text::ReadStageTemplate>>,
}

/// One `MATCH ... SET/REMOVE/DETACH DELETE ... RETURN ...` statement. RETURN
/// rows are the statement's own selection occurrences: a matched element's
/// property reads the value the statement assigns to it (REMOVE reads NULL)
/// and otherwise its pre-statement value, so `SET n.p = n.p + 1 RETURN n.p`
/// returns the incremented value. Assignments are simultaneous and
/// conflict-checked, so every read has one answer, never an order-dependent
/// one. RETURN accepts native scalar/CASE/list expressions, graph functions,
/// grouped COUNT/SUM/AVG/MIN/MAX/COLLECT, argument DISTINCT, output DISTINCT,
/// ordering, SKIP and LIMIT. Grouping counts selection occurrences, not unique
/// changed identities; paging never limits which occurrences are updated.
///
/// `labels()` after a label action or DETACH DELETE, an edge property after
/// DETACH DELETE (cascades are not visible to the statement) are refused; a
/// property of a deleted vertex is a typed execution error, including inside
/// an aggregate. Grouped output keeps the ordinary row numeric domain.
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
impl core::fmt::Display for GraphDeleteTextErrorKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Query(kind) => kind.fmt(f),
            Self::Build(error) => error.fmt(f),
        }
    }
}
impl GraphDeleteTextErrorKind {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        match self {
            Self::Query(kind) => kind.class(),
            Self::Build(_) => crate::GqlRefusalClass::Statement,
        }
    }
}
impl GraphDeleteTextError {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        self.kind.class()
    }
}
impl core::fmt::Display for GraphDeleteTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "byte {}: {}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphDeleteTextError {}

#[derive(Clone)]
pub(crate) enum VertexMergeValueTemplate {
    Bound(GqlScalarParameter),
    Parameter {
        index: usize,
        at: usize,
    },
    /// A vertex MERGE's constant list expression or declared list parameter.
    /// Binding retains the same stored value for selection and creation.
    Stored {
        value: Box<crate::set_text::ReadValueTemplate>,
        at: usize,
    },
}

/// A map value of a vertex MERGE step (fgdb-cpny9): a literal or parameter,
/// a column of the row the step runs over, or a value computed from that
/// row (`prop.login`, `y + 1`) by the shared value compiler.
#[derive(Clone)]
pub(crate) enum VertexMergeRowValueTemplate {
    Value(VertexMergeValueTemplate),
    Column(usize),
    Computed {
        value: Box<crate::set_text::ReadValueTemplate>,
        at: usize,
    },
}

/// The rows a vertex MERGE step reads (fgdb-cpny9).
#[derive(Clone)]
pub(crate) enum VertexMergeRowsTextInput {
    /// The one empty row of a statement whose first write is this MERGE.
    Unit,
    /// The previous write's rows, of these column types.
    Owned(Vec<crate::GraphSetColumnType>),
    /// A statement's leading UNWIND.
    Relation(Box<crate::set_text::BoundSetTextInput>),
}

/// The RETURN of a vertex MERGE step, resolved (fgdb-cpny9).
#[derive(Clone)]
pub(crate) struct VertexMergeRowsReturnTemplate {
    pub(crate) bindings: Vec<crate::GraphVertexMergeRowsBinding>,
    pub(crate) projection: Vec<ReadProjectionTemplate>,
    pub(crate) grouping: Option<WriteReturnGroupTemplate>,
    pub(crate) quantifier: crate::GraphSetQuantifier,
    pub(crate) order: Vec<GraphValueOrder>,
    pub(crate) offset: ReadPageNumber,
    pub(crate) count: Option<ReadPageNumber>,
    pub(crate) at: usize,
    /// Relational stages after a WITH head, ending at the next write or RETURN.
    pub(crate) tail: Option<Vec<crate::set_text::ReadStageTemplate>>,
}

/// A vertex MERGE step of a chained write (fgdb-cpny9), resolved: each map
/// value is a literal, a parameter or a column of the row it runs over.
#[derive(Clone)]
pub(crate) struct PreparedGraphVertexMergeRowsText {
    pub(crate) relation: RelationId,
    pub(crate) labels: Vec<LabelId>,
    pub(crate) properties: Vec<(PropertyKeyId, VertexMergeRowValueTemplate)>,
    pub(crate) input: VertexMergeRowsTextInput,
    /// Leading row columns the output keeps (the visible ones).
    pub(crate) keep: usize,
    /// The RETURN of the statement's last write, if any.
    pub(crate) returning: Option<VertexMergeRowsReturnTemplate>,
    /// ON MATCH, ON CREATE and trailing SET clauses, if any.
    pub(crate) actions: Option<Box<VertexMergeRowActionTemplates>>,
    pub(crate) parameters: Vec<crate::GqlParameterSpec>,
    pub(crate) parameter_offsets: Vec<usize>,
    pub(crate) end: usize,
}

/// One ON/SET assignment of a vertex MERGE step, resolved (fgdb-cpny9).
#[derive(Clone)]
pub(crate) enum VertexMergeRowActionTemplate {
    /// A constant property, constant List or label assignment.
    Vertex(crate::vertex_upsert_text::VertexUpsertActionTemplate),
    /// `v.key = expression` over the merged vertex's properties and the row.
    Expression {
        key: PropertyKeyId,
        inputs: Vec<crate::GraphVertexMergeRowInput>,
        program: Vec<MutationIntegerTemplateOp>,
        at: usize,
    },
    /// `v.key = value` whose right-hand side reads the row and not the
    /// merged vertex, by the shared value compiler.
    Value {
        key: PropertyKeyId,
        value: Box<crate::set_text::ReadValueTemplate>,
        at: usize,
    },
    /// `b.key = value` on the vertex in row column `column`, which only a
    /// relationship MERGE step parses (fgdb-at06q).
    RowVertex {
        column: usize,
        key: PropertyKeyId,
        value: Box<crate::set_text::ReadValueTemplate>,
        at: usize,
    },
}

/// The ON/SET clauses of a vertex MERGE step, resolved (fgdb-cpny9).
#[derive(Clone, Default)]
pub(crate) struct VertexMergeRowActionTemplates {
    pub(crate) on_match: Vec<VertexMergeRowActionTemplate>,
    pub(crate) on_create: Vec<VertexMergeRowActionTemplate>,
    pub(crate) after: Vec<VertexMergeRowActionTemplate>,
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
    /// A leading UNWIND, or a RETURN expression, page or projection name of
    /// a vertex MERGE step (fgdb-cpny9).
    Relation(crate::GraphSetTextErrorKind),
    ReturnBuild(crate::GraphVertexMergeRowsQueryBuildError),
    /// An ON/SET clause of a vertex MERGE step (fgdb-cpny9).
    Action(Box<crate::GraphVertexUpsertTextErrorKind>),
}
impl From<crate::GraphVertexUpsertTextError> for GraphVertexMergeTextError {
    fn from(error: crate::GraphVertexUpsertTextError) -> Self {
        let kind = match error.kind {
            crate::GraphVertexUpsertTextErrorKind::Query(kind) => {
                GraphVertexMergeTextErrorKind::Query(kind)
            }
            crate::GraphVertexUpsertTextErrorKind::Merge(kind) => kind,
            kind => GraphVertexMergeTextErrorKind::Action(Box::new(kind)),
        };
        Self {
            offset: error.offset,
            kind,
        }
    }
}
impl From<GraphPatternTextError> for GraphVertexMergeTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphVertexMergeTextErrorKind::Query(error.kind),
        }
    }
}
impl From<crate::GraphSetTextError> for GraphVertexMergeTextError {
    fn from(error: crate::GraphSetTextError) -> Self {
        let kind = match error.kind {
            crate::GraphSetTextErrorKind::Pattern(kind) => {
                GraphVertexMergeTextErrorKind::Query(kind)
            }
            kind => GraphVertexMergeTextErrorKind::Relation(kind),
        };
        Self {
            offset: error.offset,
            kind,
        }
    }
}
impl core::fmt::Display for GraphVertexMergeTextErrorKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Query(kind) => kind.fmt(f),
            Self::InsertBuild(error) => error.fmt(f),
            Self::MergeBuild(error) => error.fmt(f),
            Self::Relation(kind) => kind.fmt(f),
            Self::ReturnBuild(error) => error.fmt(f),
            Self::Action(kind) => kind.fmt(f),
        }
    }
}
impl GraphVertexMergeTextErrorKind {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        match self {
            Self::Query(kind) => kind.class(),
            Self::Relation(kind) => kind.class(),
            Self::Action(kind) => kind.class(),
            Self::InsertBuild(_) | Self::MergeBuild(_) | Self::ReturnBuild(_) => {
                crate::GqlRefusalClass::Statement
            }
        }
    }
}
impl GraphVertexMergeTextError {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        self.kind.class()
    }
}
impl core::fmt::Display for GraphVertexMergeTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "byte {}: {}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphVertexMergeTextError {}

/// Where a relationship MERGE's endpoint rows come from (fgdb-cpny9).
#[derive(Clone)]
pub(crate) enum EdgeMergeTextInput {
    /// `MATCH ... MERGE`: the MERGE's own MATCH prefix.
    Match(Box<PreparedGraphText>),
    /// `MATCH ... WITH ... MERGE`, or a chained write's owned rows: the row
    /// relation whose vertex columns are the endpoints.
    Row {
        statement: String,
        input: Box<crate::set_text::BoundSetTextInput>,
    },
    /// A complete multipart read (`MATCH ... WITH ... MATCH ... MERGE`):
    /// its relation's vertex or open columns are the endpoints.
    Multipart {
        statement: String,
        input: Box<crate::set_text::multipart::BoundReadInput>,
    },
}

/// Native `MATCH ... MERGE (a)-[r:R {k: v}]->(b)` over already-bound
/// endpoint variables, its reverse arrow, or an undirected `-[:R]-`. The
/// property map's values are the MERGE's exact-equality predicates and its
/// creation fields (fgdb-at06q).
#[derive(Clone)]
pub struct PreparedGraphEdgeMergeText {
    pub(crate) input: EdgeMergeTextInput,
    pub(crate) relation: RelationId,
    pub(crate) source: usize,
    pub(crate) destination: usize,
    pub(crate) properties: Vec<(PropertyKeyId, VertexMergeValueTemplate)>,
    pub(crate) undirected: bool,
    /// Chain-only ON/SET templates, compiled by the shared row-action grammar.
    pub(crate) row_actions: Option<VertexMergeRowActionTemplates>,
}

/// `MATCH ... MERGE (a)-[r:R {k: v}]->(b) RETURN ...` (fgdb-at06q). Every
/// selected row continues once per relationship its endpoint pair resolved
/// to. RETURN reads MATCH bindings, the relationship `r`, `r.k` after the
/// statement, and `startNode(r)` / `endNode(r)` with their properties.
#[derive(Clone)]
pub struct PreparedGraphEdgeMergeQueryText {
    pub(crate) statement: String,
    pub(crate) merge: PreparedGraphEdgeMergeText,
    pub(crate) returning: EdgeMergeReturnTemplate,
}
impl core::fmt::Debug for PreparedGraphEdgeMergeQueryText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeMergeQueryText")
            .field("output_columns", &self.returning.projection.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct EdgeMergeReturnTemplate {
    pub bindings: Vec<crate::GraphEdgeMergeBinding>,
    pub projection: Vec<crate::set_text::ReadProjectionTemplate>,
    pub grouping: Option<WriteReturnGroupTemplate>,
    pub quantifier: crate::GraphSetQuantifier,
    pub order: Vec<crate::algebra::GraphValueOrder>,
    pub offset: crate::set_text::ReadPageNumber,
    pub count: Option<crate::set_text::ReadPageNumber>,
    pub at: usize,
    /// Native row stages following a WITH over the MERGE's own occurrences.
    pub tail: Option<Vec<crate::set_text::ReadStageTemplate>>,
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
    /// A RETURN expression, page or projection name after MERGE (fgdb-at06q).
    Relation(crate::GraphSetTextErrorKind),
    ReturnBuild(crate::GraphEdgeMergeQueryBuildError),
    /// The shared row-action compiler's syntax, typing or expression refusal.
    RowActions(Box<crate::GraphVertexUpsertTextErrorKind>),
    RowActionsBuild(crate::GraphEdgeUpsertBuildError),
}
impl From<crate::GraphSetTextError> for GraphEdgeMergeTextError {
    fn from(error: crate::GraphSetTextError) -> Self {
        let kind = match error.kind {
            crate::GraphSetTextErrorKind::Pattern(kind) => GraphEdgeMergeTextErrorKind::Query(kind),
            kind => GraphEdgeMergeTextErrorKind::Relation(kind),
        };
        Self {
            offset: error.offset,
            kind,
        }
    }
}
impl From<GraphPatternTextError> for GraphEdgeMergeTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphEdgeMergeTextErrorKind::Query(error.kind),
        }
    }
}
impl From<crate::GraphVertexUpsertTextError> for GraphEdgeMergeTextError {
    fn from(error: crate::GraphVertexUpsertTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphEdgeMergeTextErrorKind::RowActions(Box::new(error.kind)),
        }
    }
}
impl core::fmt::Display for GraphEdgeMergeTextErrorKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Query(kind) => kind.fmt(f),
            Self::Build(error) => error.fmt(f),
            Self::Relation(kind) => kind.fmt(f),
            Self::ReturnBuild(error) => error.fmt(f),
            Self::RowActions(error) => error.fmt(f),
            Self::RowActionsBuild(error) => error.fmt(f),
        }
    }
}
impl GraphEdgeMergeTextErrorKind {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        match self {
            Self::Query(kind) => kind.class(),
            Self::Relation(kind) => kind.class(),
            Self::RowActions(kind) => kind.class(),
            Self::Build(_) | Self::ReturnBuild(_) | Self::RowActionsBuild(_) => {
                crate::GqlRefusalClass::Statement
            }
        }
    }
}
impl GraphEdgeMergeTextError {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        self.kind.class()
    }
}
impl core::fmt::Display for GraphEdgeMergeTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "byte {}: {}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphEdgeMergeTextError {}
