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
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, VId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphVertexMergeBuildError {
    TargetColumn {
        column: usize,
    },
    CreationMustBeStandalone,
    CreationMustContainOneVertex {
        observed: usize,
    },
    CreationMustNotContainEdges {
        observed: usize,
    },
    RelationMismatch {
        selection: RelationId,
        creation: RelationId,
    },
}
impl core::fmt::Display for GraphVertexMergeBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TargetColumn { column } => {
                write!(f, "MERGE selects column {column}, which is not a vertex")
            }
            Self::CreationMustBeStandalone => {
                f.write_str("the vertex MERGE pattern must not read an input row")
            }
            Self::CreationMustContainOneVertex { observed } => write!(
                f,
                "a vertex MERGE pattern must create exactly one vertex, not {observed}"
            ),
            Self::CreationMustNotContainEdges { observed } => write!(
                f,
                "a vertex MERGE pattern must not create relationships ({observed} found)"
            ),
            Self::RelationMismatch { .. } => {
                f.write_str("the MERGE selection and creation use different relation coordinates")
            }
        }
    }
}
impl core::error::Error for GraphVertexMergeBuildError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphVertexMergePolicy {
    /// One cumulative allowance for match selection, uniqueness reduction and
    /// the standalone creation arm when it is needed.
    pub query: GqlQueryPolicy,
    /// Creation proposals, not matched vertices. Zero still permits a match,
    /// but refuses a missing vertex before requesting an external identity.
    pub max_created_vertices: u64,
}
impl GraphVertexMergePolicy {
    #[must_use]
    pub const fn new(query: GqlQueryPolicy) -> Self {
        Self {
            query,
            max_created_vertices: 1,
        }
    }

    #[must_use]
    pub const fn with_creation_limit(mut self, limit: u64) -> Self {
        self.max_created_vertices = limit;
        self
    }
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
        match self {
            Self::Matched(vertex) | Self::Created(vertex) => vertex,
        }
    }
    #[must_use]
    pub const fn created(self) -> bool {
        matches!(self, Self::Created(_))
    }
}
impl core::fmt::Debug for GraphVertexMergeOutcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct(match self {
            Self::Matched(_) => "Matched",
            Self::Created(_) => "Created",
        })
        .field("vertex", &"[REDACTED]")
        .finish()
    }
}

#[derive(Debug)]
pub enum GraphVertexMergeError<E, A> {
    Source(E),
    InvalidSourceStatistics,
    InputSchema {
        row: usize,
        column: usize,
    },
    AmbiguousMatches {
        observed: u64,
    },
    Creation(GraphInsertError<E, A>),
    AccountingOverflow,
    /// A MERGE over input rows read NULL or a non-scalar from its row for a
    /// property value (openCypher refuses a null MERGE property).
    RowValue {
        row: usize,
        column: usize,
    },
    /// A MERGE over input rows could not build one row's MERGE.
    RowMerge {
        row: usize,
        source: Box<crate::GraphVertexMergeTextError>,
    },
    /// The leading UNWIND relation of a MERGE over input rows refused.
    InputRelation(crate::GraphSetExecutionError<E>),
    /// An ON/SET expression of a MERGE over input rows refused. Clause 0 is
    /// the selected ON clause; clause 1 is the trailing SET.
    RowAction {
        row: usize,
        clause: usize,
        action: usize,
        source: crate::GraphIntegerError,
    },
    /// A property value a MERGE over input rows computes from its row (in
    /// property-key order) raised an arithmetic exception.
    RowExpression {
        row: usize,
        property: usize,
        source: crate::GraphIntegerError,
    },
    /// A property value a MERGE over input rows computes from its row was
    /// NULL or no scalar (openCypher refuses a null MERGE property).
    RowExpressionValue {
        row: usize,
        property: usize,
    },
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphVertexMergeError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::InvalidSourceStatistics => {
                f.write_str("MERGE source returned inconsistent statistics")
            }
            Self::InputSchema { row, column } => {
                write!(f, "MERGE row {row} has incompatible column {column}")
            }
            Self::AmbiguousMatches { observed } => write!(
                f,
                "MERGE matched {observed} distinct vertices; unique match required"
            ),
            Self::Creation(error) => write!(f, "MERGE creation: {error}"),
            Self::AccountingOverflow => {
                f.write_str("MERGE cumulative resource accounting overflow")
            }
            Self::RowValue { row, column } => write!(
                f,
                "MERGE row {row} column {column} is not a non-null scalar property value"
            ),
            Self::RowMerge { row, source } => write!(f, "MERGE row {row}: {source}"),
            Self::InputRelation(error) => write!(f, "MERGE input rows: {error}"),
            Self::RowAction {
                row,
                clause,
                action,
                source,
            } => write!(
                f,
                "MERGE row {row} clause {clause} action {action}: {source}"
            ),
            Self::RowExpression {
                row,
                property,
                source,
            } => write!(f, "MERGE row {row} property {property}: {source}"),
            Self::RowExpressionValue { row, property } => write!(
                f,
                "MERGE row {row} property {property} is not a non-null scalar property value"
            ),
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
            Self::RowMerge { source, .. } => Some(&**source),
            Self::InputRelation(error) => Some(error),
            Self::RowAction { source, .. } | Self::RowExpression { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// A vertex MERGE selection's identity key: its labels and property
/// equalities with query-equality value encodings (PreparedGraphVertexMerge::
/// equality_key). Exact Int/Float peers share one key. Equal keys may match the
/// same vertices; unequal keys of one template cannot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct MergeEqualityKey {
    pub(crate) labels: Vec<LabelId>,
    pub(crate) properties: Vec<(PropertyKeyId, Vec<u8>)>,
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
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphVertexMerge {
    pub fn prepare(
        selection: PreparedGraphPattern<GraphValueRow>,
        relation: RelationId,
        target: usize,
        creation: PreparedGraphInsert,
    ) -> Result<Self, GraphVertexMergeBuildError> {
        if !matches!(
            selection.value_columns().get(target),
            Some(ValueProjection::Vertex { .. })
        ) {
            return Err(GraphVertexMergeBuildError::TargetColumn { column: target });
        }
        if !creation.is_standalone() {
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
                selection: relation,
                creation: creation.relation(),
            });
        }
        Ok(Self {
            selection,
            relation,
            target,
            creation,
        })
    }

    #[must_use]
    pub fn selection(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.selection
    }
    #[must_use]
    pub const fn relation(&self) -> RelationId {
        self.relation
    }
    #[must_use]
    pub const fn target_column(&self) -> usize {
        self.target
    }
    #[must_use]
    pub fn creation(&self) -> &PreparedGraphInsert {
        &self.creation
    }

    /// The selection's identity key when it is nothing but one vertex scan
    /// under label and non-null property equalities (fgdb-2l3pt): the sorted
    /// labels, and each property key with its query-equality value encoding,
    /// sorted. Exact Int/Float peers share an encoding. Two bound merges of one
    /// template whose keys differ in any value match disjoint vertex sets,
    /// since a vertex holds one value per property. Any other
    /// operator or predicate (ranges, NULL tests, Boolean selections, edges)
    /// returns None. Definition-only: no source or callback.
    pub(crate) fn equality_key(&self) -> Option<MergeEqualityKey> {
        use crate::algebra::{GlaOperator, IntegerComparison, VertexPredicate};
        let operators = self.selection.plan().operators();
        let (GlaOperator::ScanVertices, rest) = operators.split_first()? else {
            return None;
        };
        let mut labels = Vec::new();
        let mut properties = Vec::new();
        for operator in rest {
            match operator {
                GlaOperator::Select { slot, predicates } if slot.ordinal() == 0 => {
                    for predicate in predicates {
                        match predicate {
                            VertexPredicate::HasLabel(label) => labels.push(*label),
                            VertexPredicate::IntegerProperty {
                                key,
                                comparison: IntegerComparison::Equal,
                                value,
                            } => properties.push((
                                *key,
                                crate::algebra::equality_value_bytes(&CanonicalScalar::Int(
                                    *value,
                                ))?,
                            )),
                            VertexPredicate::ScalarProperty { key, predicate }
                                if predicate.comparison() == IntegerComparison::Equal
                                    && !matches!(predicate.value(), CanonicalScalar::Null) =>
                            {
                                properties.push((
                                    *key,
                                    crate::algebra::equality_value_bytes(predicate.value())?,
                                ));
                            }
                            _ => return None,
                        }
                    }
                }
                GlaOperator::ProjectValues { .. }
                | GlaOperator::Distinct
                | GlaOperator::OrderByValues
                | GlaOperator::Limit { .. } => {}
                _ => return None,
            }
        }
        if properties.is_empty() {
            return None;
        }
        labels.sort_unstable();
        properties.sort_unstable();
        Some(MergeEqualityKey { labels, properties })
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:unique-vertex-merge:v1\0".to_vec();
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        bytes.extend_from_slice(&(self.target as u64).to_be_bytes());
        for definition in [
            self.selection.canonical_bytes(),
            self.creation.canonical_bytes(),
        ] {
            bytes.extend_from_slice(&(definition.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&definition);
        }
        bytes
    }
}

/// One property value of a vertex MERGE over input rows (fgdb-cpny9).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum GraphVertexMergeRowValue {
    /// A literal or bound parameter, the same for every row.
    Bound(crate::GqlScalarParameter),
    /// One column of the input row.
    Column(usize),
    /// A value computed from the input row (`prop.login`, `y + 1`).
    Computed(crate::GraphSetValue),
}

/// A vertex MERGE over input rows: a step of a chained write (`MERGE (a:A)
/// MERGE (b:B)`, `CREATE (x) MERGE (:X {k: v})`, fgdb-cpny9). Each row's
/// property values form a tuple, and rows with equal tuples share one MERGE.
/// A tuple no vertex matches creates exactly one. A tuple several distinct
/// vertices match binds each of them, one output row apiece, as openCypher
/// does. The output row is the input row's visible columns followed by the
/// merged vertex.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphVertexMergeRows {
    pub(crate) relation: RelationId,
    pub(crate) labels: Vec<fgdb_delta_types::LabelId>,
    /// In property-key order.
    pub(crate) properties: Vec<(fgdb_delta_types::PropertyKeyId, GraphVertexMergeRowValue)>,
    pub(crate) input: GraphVertexMergeRowsInput,
    /// Leading input columns the output keeps. Hidden reads after them are
    /// the MERGE's own inputs, read by no later write.
    pub(crate) keep: usize,
    /// ON MATCH, ON CREATE and trailing SET clauses, if any.
    pub(crate) actions: Option<Box<GraphVertexMergeRowActions>>,
}

/// The ON/SET clauses of a vertex MERGE over input rows (fgdb-cpny9). Each
/// row runs its own MERGE against the state earlier rows left, so a vertex
/// an earlier row created is MATCHED by a later row with the same values,
/// as openCypher's row-by-row MERGE does. The selected ON clause applies to
/// every vertex the row merged, then the trailing SET. Each clause reads all
/// of its right-hand sides before staging any of them.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct GraphVertexMergeRowActions {
    pub on_match: Vec<GraphVertexMergeRowAction>,
    pub on_create: Vec<GraphVertexMergeRowAction>,
    pub after: Vec<GraphVertexMergeRowAction>,
}

/// One assignment of a vertex MERGE over input rows.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphVertexMergeRowAction {
    /// A constant property, constant List or label assignment, exactly as a
    /// unique MERGE's (never its vertex-only SetExpression).
    Vertex(crate::GraphVertexUpsertAction),
    /// `v.key = expression`. The expression's columns address `inputs` in
    /// order: the merged vertex's properties before this clause, and columns
    /// of the row.
    Expression {
        key: fgdb_delta_types::PropertyKeyId,
        inputs: Vec<GraphVertexMergeRowInput>,
        value: crate::GraphIntegerExpression,
    },
    /// `v.key = value` whose right-hand side reads the row and not the
    /// merged vertex (`prop.name`, `xs + [y]`).
    Value {
        key: fgdb_delta_types::PropertyKeyId,
        value: GraphVertexMergeRowComputed,
    },
}
impl core::fmt::Debug for GraphVertexMergeRowAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Vertex(action) => core::fmt::Debug::fmt(action, f),
            Self::Expression { inputs, .. } => f
                .debug_struct("Expression")
                .field("inputs", &inputs.len())
                .field("definition", &"[REDACTED]")
                .finish(),
            Self::Value { .. } => f
                .debug_struct("Value")
                .field("definition", &"[REDACTED]")
                .finish(),
        }
    }
}

/// A value a vertex MERGE over input rows computes from one row
/// (fgdb-cpny9), by the shared value compiler.
#[derive(Clone, PartialEq, Eq)]
pub struct GraphVertexMergeRowComputed(pub(crate) crate::GraphSetValue);

impl GraphVertexMergeRowComputed {
    /// The property value written for `row`, each evaluation event charged
    /// to `control`. A List is the stored List it is. `None` is a value no
    /// property holds, by the rule CREATE writes with: a Map at any depth, a
    /// graph element, or a value beyond the stored format's bounds.
    pub fn written<C>(
        &self,
        row: &GraphValueRow,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, crate::GraphIntegerEvaluationError<C>>
    {
        let value =
            crate::set_ops::evaluate_value(&self.0, row, 0, control).map_err(|failure| {
                match failure {
                    crate::set_ops::ProjectionFailure::Control(error) => {
                        crate::GraphIntegerEvaluationError::Control(error)
                    }
                    crate::set_ops::ProjectionFailure::Arithmetic { error, .. } => {
                        crate::GraphIntegerEvaluationError::Value(error)
                    }
                }
            })?;
        Ok(value.to_written_property())
    }
}

/// One input of a row action's expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphVertexMergeRowInput {
    /// A property of the merged vertex; missing reads NULL.
    Property(fgdb_delta_types::PropertyKeyId),
    /// A column of the row the MERGE read.
    Column(usize),
}

/// The rows a vertex MERGE step runs over (fgdb-cpny9).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum GraphVertexMergeRowsInput {
    /// The one empty row of a statement whose first write is this MERGE.
    Unit,
    /// The previous write's rows, of these column types.
    Owned(Vec<crate::GraphSetColumnType>),
    /// A statement's leading UNWIND, run as a relation (`UNWIND [1, 2] AS x
    /// MERGE (n {id: x})`).
    Relation(Box<crate::PreparedGraphSet>),
}
impl core::fmt::Debug for PreparedGraphVertexMergeRows {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexMergeRows")
            .field("properties", &self.properties.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphVertexMergeRows {
    #[must_use]
    pub const fn relation(&self) -> RelationId {
        self.relation
    }

    /// The MERGE's property keys, in the order of [`Self::tuple`]'s values.
    pub fn property_keys(&self) -> impl Iterator<Item = fgdb_delta_types::PropertyKeyId> + '_ {
        self.properties.iter().map(|(key, _)| *key)
    }

    /// The input row's column types: the previous write's row, the leading
    /// UNWIND's columns, or none for a first write over one empty row.
    #[must_use]
    pub fn input_column_types(&self) -> &[crate::GraphSetColumnType] {
        match &self.input {
            GraphVertexMergeRowsInput::Unit => &[],
            GraphVertexMergeRowsInput::Owned(types) => types,
            GraphVertexMergeRowsInput::Relation(relation) => relation.column_types(),
        }
    }

    /// The rows this MERGE runs over, with the statistics of producing them.
    /// The rows are `owned` (the previous write's), the leading UNWIND's
    /// rows, or the one empty row of a first write. Owned rows must have the
    /// input width; a first write takes none.
    pub fn select_governed<E, A, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GqlQueryPolicy,
        patterns: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<
            crate::GqlQueryExecution<GraphValueRow>,
            crate::GqlQueryError<E, C>,
        >,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<
        crate::GqlQueryExecution<GraphValueRow>,
        crate::GqlQueryError<GraphVertexMergeError<E, A>, C>,
    > {
        let schema = |row, column| {
            crate::GqlQueryError::Source(GraphVertexMergeError::InputSchema { row, column })
        };
        let rows = match (&self.input, owned) {
            (GraphVertexMergeRowsInput::Unit, None) => {
                vec![GraphValueRow::from_owned_values(Vec::new())]
            }
            (GraphVertexMergeRowsInput::Owned(types), Some(rows)) => {
                if let Some(row) = rows.iter().position(|row| row.len() != types.len()) {
                    return Err(schema(row, types.len()));
                }
                rows
            }
            (GraphVertexMergeRowsInput::Relation(relation), None) => {
                return relation
                    .execute_governed(policy, patterns, checkpoint)
                    .map_err(|error| error.map_source(GraphVertexMergeError::InputRelation));
            }
            (input, _) => {
                return Err(schema(
                    0,
                    match input {
                        GraphVertexMergeRowsInput::Owned(types) => types.len(),
                        _ => 0,
                    },
                ));
            }
        };
        Ok(crate::GqlQueryExecution {
            value: rows,
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
        })
    }

    /// The property tuple of input row `at`, in property-key order. A NULL
    /// or non-scalar row value refuses: openCypher cannot MERGE on null. A
    /// computed value charges each evaluation event to `control`.
    #[allow(clippy::type_complexity)]
    pub fn tuple<E, A, C>(
        &self,
        at: usize,
        row: &GraphValueRow,
        control: &mut impl FnMut(
            crate::GlaExecutionEvent,
        )
            -> Result<(), crate::GqlQueryError<GraphVertexMergeError<E, A>, C>>,
    ) -> Result<
        Vec<fgdb_types::CanonicalScalar>,
        crate::GqlQueryError<GraphVertexMergeError<E, A>, C>,
    > {
        let scalar = |value: &crate::algebra::GraphValue| {
            value
                .as_scalar()
                .filter(|value| !matches!(value, fgdb_types::CanonicalScalar::Null))
                .cloned()
        };
        let mut tuple = Vec::with_capacity(self.properties.len());
        for (property, (_, value)) in self.properties.iter().enumerate() {
            tuple.push(match value {
                GraphVertexMergeRowValue::Bound(value) => value.value().clone(),
                GraphVertexMergeRowValue::Column(column) => {
                    row.values().get(*column).and_then(scalar).ok_or(
                        crate::GqlQueryError::Source(GraphVertexMergeError::RowValue {
                            row: at,
                            column: *column,
                        }),
                    )?
                }
                GraphVertexMergeRowValue::Computed(value) => {
                    let value = crate::set_ops::evaluate_value(value, row, property, control)
                        .map_err(|failure| match failure {
                            crate::set_ops::ProjectionFailure::Control(error) => error,
                            crate::set_ops::ProjectionFailure::Arithmetic { error, .. } => {
                                crate::GqlQueryError::Source(GraphVertexMergeError::RowExpression {
                                    row: at,
                                    property,
                                    source: error,
                                })
                            }
                        })?;
                    scalar(&value).ok_or(crate::GqlQueryError::Source(
                        GraphVertexMergeError::RowExpressionValue { row: at, property },
                    ))?
                }
            });
        }
        Ok(tuple)
    }

    /// The ON/SET clauses, if any. With clauses, every row runs its own
    /// MERGE; without, rows with equal tuples share one.
    #[must_use]
    pub fn actions(&self) -> Option<&GraphVertexMergeRowActions> {
        self.actions.as_deref()
    }

    /// One output row: the input row's kept columns, then the vertex.
    #[must_use]
    pub fn output_row(&self, row: &GraphValueRow, vertex: VId) -> GraphValueRow {
        let mut values = row.values()[..self.keep.min(row.len())].to_vec();
        values.push(crate::algebra::GraphValue::Vertex(vertex));
        GraphValueRow::from_owned_values(values)
    }
}

/// One input column of a RETURN after a vertex MERGE step (fgdb-cpny9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphVertexMergeRowsBinding {
    /// A column of the row the MERGE read, hidden reads included. MERGE
    /// changes no element that row carries, so its value is also final.
    Input(usize),
    /// The vertex this row merged.
    Vertex,
    /// A property of that vertex after the statement. A missing field reads
    /// NULL.
    Property(fgdb_delta_types::PropertyKeyId),
    /// A property, after the statement, of the vertex or relationship in a
    /// row column: a carried element. A missing field or a NULL column reads
    /// NULL.
    InputProperty {
        column: usize,
        key: fgdb_delta_types::PropertyKeyId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphVertexMergeRowsQueryBuildError {
    TooManyBindings {
        limit: usize,
        observed: usize,
    },
    /// An Input binding names no input column.
    Binding {
        binding: usize,
    },
    Projection(crate::row_projection::RowProjectionBuildError),
    Aggregate(crate::GraphAggregateBuildError),
}
impl core::fmt::Display for GraphVertexMergeRowsQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyBindings { limit, observed } => {
                write!(f, "RETURN has {observed} bindings, limit {limit}")
            }
            Self::Binding { binding } => {
                write!(f, "RETURN binding {binding} names no input column")
            }
            Self::Projection(error) => error.fmt(f),
            Self::Aggregate(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for GraphVertexMergeRowsQueryBuildError {}

/// A refusal of a vertex MERGE step with or without RETURN: the MERGE, or
/// its RETURN.
#[derive(Debug)]
pub enum GraphVertexMergeRowsQueryError<E, A> {
    Merge(GraphVertexMergeError<E, A>),
    Returning(crate::GraphSetExecutionError<E>),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphVertexMergeRowsQueryError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Merge(error) => error.fmt(f),
            Self::Returning(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphVertexMergeRowsQueryError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Merge(error) => Some(error),
            Self::Returning(error) => Some(error),
        }
    }
}

/// `... MERGE (n ...) RETURN ...` as the last write of a chain, or over a
/// leading UNWIND (fgdb-cpny9). RETURN sees one row per input row and vertex
/// that row merged, in input order and then identity order. Aggregation,
/// projection, DISTINCT, ORDER BY and paging use the shared write-RETURN
/// kernels.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphVertexMergeRowsQuery {
    merge: PreparedGraphVertexMergeRows,
    bindings: Vec<GraphVertexMergeRowsBinding>,
    input_types: Vec<crate::GraphSetColumnType>,
    projection: Vec<crate::GraphSetProjection>,
    grouping: Option<crate::set_ops::WriteReturnGroup>,
    quantifier: crate::GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<crate::GraphSetColumnType>,
    order: Vec<crate::algebra::GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
}
impl core::fmt::Debug for PreparedGraphVertexMergeRowsQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexMergeRowsQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphVertexMergeRowsQuery {
    pub(crate) fn prepare_with_grouping(
        merge: PreparedGraphVertexMergeRows,
        bindings: Vec<GraphVertexMergeRowsBinding>,
        projection: Vec<crate::GraphSetProjection>,
        quantifier: crate::GraphSetQuantifier,
        grouping: Option<crate::set_ops::WriteReturnGroupSpec>,
    ) -> Result<Self, GraphVertexMergeRowsQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphVertexMergeRowsQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        let input = merge.input_column_types();
        let mut input_types = Vec::with_capacity(bindings.len());
        for (at, binding) in bindings.iter().enumerate() {
            input_types.push(match *binding {
                GraphVertexMergeRowsBinding::Input(column) => input
                    .get(column)
                    .copied()
                    .ok_or(GraphVertexMergeRowsQueryBuildError::Binding { binding: at })?,
                GraphVertexMergeRowsBinding::Vertex => crate::GraphSetColumnType::Vertex,
                GraphVertexMergeRowsBinding::Property(_) => crate::GraphSetColumnType::Stored,
                GraphVertexMergeRowsBinding::InputProperty { column, .. } => {
                    match input.get(column) {
                        Some(
                            crate::GraphSetColumnType::Vertex | crate::GraphSetColumnType::Edge,
                        ) => crate::GraphSetColumnType::Stored,
                        _ => {
                            return Err(GraphVertexMergeRowsQueryBuildError::Binding {
                                binding: at,
                            });
                        }
                    }
                }
            });
        }
        let grouping = grouping
            .map(|group| group.prepare(input_types.clone()))
            .transpose()
            .map_err(GraphVertexMergeRowsQueryBuildError::Aggregate)?;
        let output_input = grouping
            .as_ref()
            .map(|group| group.column_types().to_vec())
            .unwrap_or_else(|| input_types.clone());
        let checked = crate::row_projection::RowProjectionSpec::new(
            output_input,
            projection.clone(),
            quantifier,
        )
        .map_err(GraphVertexMergeRowsQueryBuildError::Projection)?;
        Ok(Self {
            merge,
            bindings,
            input_types,
            columns: checked.columns().map(str::to_owned).collect(),
            types: checked.column_types().to_vec(),
            projection,
            grouping,
            quantifier,
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }

    #[must_use]
    pub const fn merge(&self) -> &PreparedGraphVertexMergeRows {
        &self.merge
    }
    #[must_use]
    pub fn bindings(&self) -> &[GraphVertexMergeRowsBinding] {
        &self.bindings
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    #[must_use]
    pub fn column_types(&self) -> &[crate::GraphSetColumnType] {
        &self.types
    }

    pub fn with_order_by(
        mut self,
        order: &[crate::algebra::GraphValueOrder],
    ) -> Result<Self, crate::algebra::GraphOrderError> {
        use crate::algebra::GraphOrderError;
        if order.is_empty() {
            return Err(GraphOrderError::EmptyOrder);
        }
        if order.len() > self.columns.len() {
            return Err(GraphOrderError::TooManyColumns {
                limit: self.columns.len(),
                observed: order.len(),
            });
        }
        for (at, key) in order.iter().enumerate() {
            if key.column >= self.columns.len() {
                return Err(GraphOrderError::UnknownColumn { column: key.column });
            }
            if order[..at].iter().any(|prior| prior.column == key.column) {
                return Err(GraphOrderError::DuplicateColumn { column: key.column });
            }
        }
        self.order = order.to_vec();
        Ok(self)
    }
    #[must_use]
    pub fn with_page(mut self, offset: u64, count: Option<u64>) -> Self {
        self.offset = offset;
        self.count = count;
        self
    }

    /// Finalize the statement's rows, one per input row and vertex it merged,
    /// under the statement's cumulative counters. Every cell is checked
    /// against its binding before any projection runs. The host keeps its
    /// rollback guard until this succeeds. LIMIT 0 discards output, never
    /// the MERGE or its field reads.
    pub fn finish_governed<E, C>(
        &self,
        rows: Vec<Vec<crate::algebra::GraphValue>>,
        selection: GqlExecutionStats,
        mut evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<
        crate::GqlQueryExecution<GraphValueRow>,
        crate::GqlQueryError<crate::GraphSetExecutionError<E>, C>,
    > {
        use crate::algebra::GraphValue;
        use crate::{GqlQueryError, GraphSetExecutionError};
        let mut work = || -> Result<(), GqlQueryError<GraphSetExecutionError<E>, C>> {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let observed = u128::from(evaluator.work_units) + 1;
            if observed > u128::from(policy.evaluator.max_work_units) {
                return Err(GqlQueryError::Evaluator(crate::GlaLimitExceeded {
                    dimension: crate::GlaLimitDimension::WorkUnits,
                    limit: policy.evaluator.max_work_units,
                    observed,
                }));
            }
            evaluator.work_units = observed as u64;
            Ok(())
        };
        work()?;
        let invalid = || GqlQueryError::Source(GraphSetExecutionError::InputSchema { operand: 0 });
        for row in &rows {
            if row.len() != self.bindings.len() {
                return Err(invalid());
            }
            for ((binding, kind), value) in self.bindings.iter().zip(&self.input_types).zip(row) {
                work()?;
                let admitted = match binding {
                    GraphVertexMergeRowsBinding::Input(_)
                    | GraphVertexMergeRowsBinding::Property(_)
                    | GraphVertexMergeRowsBinding::InputProperty { .. } => kind.accepts(value),
                    GraphVertexMergeRowsBinding::Vertex => matches!(value, GraphValue::Vertex(_)),
                };
                if !admitted {
                    return Err(invalid());
                }
            }
        }
        drop(work);
        let mut returning = crate::GqlQueryExecution {
            value: rows
                .into_iter()
                .map(GraphValueRow::from_owned_values)
                .collect(),
            rows: selection,
            evaluator,
        };
        if let Some(grouping) = &self.grouping {
            returning = crate::set_ops::finish_owned_aggregate(
                returning,
                policy,
                grouping,
                &mut checkpoint,
            )?;
        }
        crate::set_ops::finish_owned_projection(
            returning,
            policy,
            &self.projection,
            self.quantifier,
            &self.order,
            (self.offset, self.count),
            checkpoint,
        )
    }
}
