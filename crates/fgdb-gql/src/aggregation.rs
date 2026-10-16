//! Grouped summaries over a compiled pattern or single-source row pipeline.
//!
//! Ordinary inputs stream through the existing GLA binding visitor without an
//! intermediate bag. Computed inputs and relational pipelines use a bounded
//! materialized path and the same accumulators/result engine. Keys, extrema and
//! distinct arguments borrow the admitted input until owned rows are released.

mod computed;
mod groups;
mod numeric;
mod positioned;
mod relational;
mod result;
mod weighted;
pub use numeric::GraphExactAverage;
pub use positioned::{PositionedEdge, PositionedGraph};
pub use result::{
    GraphAggregateColumn, GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest,
    GraphHavingError, GraphHavingExpression, GraphHavingOp, GraphHavingOperand, GraphNullPlacement,
    MAX_AGGREGATE_FILTERS, MAX_HAVING_INSTRUCTIONS,
};

use crate::algebra::{
    GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GlaOperator, GraphPath, GraphValue, GraphValueRow,
    MAX_PATTERN_VERTICES, PreparedGraphPattern, ValueProjection, VertexPredicate,
};
use crate::{
    GlaExecutionEvent, GlaExecutionStats, GqlBudgetDimension, GqlExecutionStats, GqlQueryError,
    GqlQueryExecution, GqlQueryPolicy,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphAggregateFunction {
    CountRows,
    Count,
    CountDistinct,
    SumInt,
    Min,
    Max,
    SumIntDistinct,
    AverageInt,
    AverageIntDistinct,
    Collect,
    CollectDistinct,
}

/// A named aggregate over a zero-based column of the child value pattern.
/// CountRows alone has no argument. Names are borrowed until preparation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct GraphAggregate<'a> {
    name: &'a str,
    function: GraphAggregateFunction,
    column: Option<usize>,
}

impl<'a> GraphAggregate<'a> {
    #[must_use]
    pub const fn count_rows(name: &'a str) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::CountRows,
            column: None,
        }
    }
    #[must_use]
    pub const fn count(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::Count,
            column: Some(column),
        }
    }
    #[must_use]
    pub const fn count_distinct(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::CountDistinct,
            column: Some(column),
        }
    }
    /// Collect nonnull inputs in visitation order, retaining duplicates.
    #[must_use]
    pub const fn collect(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::Collect,
            column: Some(column),
        }
    }
    /// Collect the first occurrence of each canonical nonnull input value.
    #[must_use]
    pub const fn collect_distinct(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::CollectDistinct,
            column: Some(column),
        }
    }
    #[must_use]
    pub const fn sum_int(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::SumInt,
            column: Some(column),
        }
    }
    #[must_use]
    pub const fn sum_int_distinct(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::SumIntDistinct,
            column: Some(column),
        }
    }
    #[must_use]
    pub const fn average_int(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::AverageInt,
            column: Some(column),
        }
    }
    #[must_use]
    pub const fn average_int_distinct(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::AverageIntDistinct,
            column: Some(column),
        }
    }
    /// Least nonnull argument in language order. Equal numeric values retain
    /// the least canonical representation, independently of occurrence order.
    #[must_use]
    pub const fn min(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::Min,
            column: Some(column),
        }
    }
    /// Greatest nonnull argument in language order. Equal numeric values retain
    /// the greatest canonical representation, independently of occurrence order.
    #[must_use]
    pub const fn max(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::Max,
            column: Some(column),
        }
    }
}

impl core::fmt::Debug for GraphAggregate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphAggregate")
            .field("function", &self.function)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphAggregateBuildError {
    RequiresUnpaginatedAll,
    RequiresSingleGraphSource,
    RelationalInput(crate::GraphSetBuildError),
    EmptyAggregates,
    TooManyColumns { limit: usize, observed: usize },
    UnknownColumn { column: usize },
    DuplicateKey { column: usize },
    InvalidName,
    DuplicateName,
    UnknownOutputColumn { column: GraphAggregateColumn },
    TooManyFilters { limit: usize, observed: usize },
    DuplicateOrder { column: GraphAggregateColumn },
    RequiresOutputProjection,
    UnknownProjectedOrderColumn { column: usize },
    DuplicateProjectedOrder { column: usize },
    InputProjection(crate::GraphSetProjectionError),
    InputRows(crate::row_projection::RowProjectionBuildError),
    OutputProjection(crate::GraphSetProjectionError),
}

impl core::fmt::Display for GraphAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RequiresUnpaginatedAll => {
                f.write_str("aggregate input must preserve duplicates and have no pagination")
            }
            Self::RequiresSingleGraphSource => {
                f.write_str("aggregate row pipeline requires exactly one graph source")
            }
            Self::RelationalInput(error) => error.fmt(f),
            Self::EmptyAggregates => f.write_str("at least one aggregate is required"),
            Self::TooManyColumns { limit, observed } => {
                write!(f, "aggregate output has {observed} columns, limit {limit}")
            }
            Self::UnknownColumn { column } => write!(f, "unknown aggregate input column {column}"),
            Self::DuplicateKey { column } => write!(f, "group key repeats input column {column}"),
            Self::InvalidName => f.write_str("invalid aggregate output name"),
            Self::DuplicateName => f.write_str("aggregate output names must be unique"),
            Self::UnknownOutputColumn { column } => {
                write!(f, "unknown aggregate output column {column:?}")
            }
            Self::TooManyFilters { limit, observed } => write!(
                f,
                "aggregate has {observed} HAVING predicates, limit {limit}"
            ),
            Self::DuplicateOrder { column } => {
                write!(f, "aggregate ORDER BY repeats column {column:?}")
            }
            Self::RequiresOutputProjection => {
                f.write_str("projected aggregate ordering requires an output projection")
            }
            Self::UnknownProjectedOrderColumn { column } => {
                write!(f, "unknown projected aggregate ORDER BY column {column}")
            }
            Self::DuplicateProjectedOrder { column } => {
                write!(f, "projected aggregate ORDER BY repeats column {column}")
            }
            Self::InputProjection(error) | Self::OutputProjection(error) => error.fmt(f),
            Self::InputRows(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for GraphAggregateBuildError {}

/// Preserve source errors and distinguish data/arithmetic refusals. Neither
/// error exposes an argument value. A refusal releases no aggregate rows.
#[derive(Debug, PartialEq, Eq)]
pub enum GraphAggregateError<E> {
    Source(E),
    /// A completed row stage failed before grouping; keep its typed cause.
    InputRelation(crate::GraphSetExecutionError<E>),
    NonIntegerSum {
        aggregate: usize,
    },
    NonIntegerAverage {
        aggregate: usize,
    },
    ArithmeticOverflow {
        aggregate: usize,
    },
    NonIntegerHaving {
        predicate: usize,
    },
    /// A computed input failed before grouping or output pagination.
    InputExpression {
        row: usize,
        column: usize,
        error: crate::GraphIntegerError,
    },
    /// A scalar output failed after grouping and HAVING, before pagination.
    OutputExpression {
        column: usize,
        error: crate::GraphIntegerError,
    },
    ResultCountOverflow,
    /// A physical weighted binding did not match its admitted topology.
    MultiplicityUnavailable,
    /// A physical path violated its ascending, contiguous root-group contract.
    NonMonotonicGroups,
    /// A private partition row, state or key did not match its checked reducer.
    InvalidReductionInput,
}

impl<E> GraphAggregateError<E> {
    pub fn map_source<T>(self, map: impl FnOnce(E) -> T) -> GraphAggregateError<T> {
        match self {
            Self::Source(error) => GraphAggregateError::Source(map(error)),
            Self::InputRelation(error) => GraphAggregateError::InputRelation(error.map_source(map)),
            Self::NonIntegerSum { aggregate } => GraphAggregateError::NonIntegerSum { aggregate },
            Self::NonIntegerAverage { aggregate } => {
                GraphAggregateError::NonIntegerAverage { aggregate }
            }
            Self::ArithmeticOverflow { aggregate } => {
                GraphAggregateError::ArithmeticOverflow { aggregate }
            }
            Self::NonIntegerHaving { predicate } => {
                GraphAggregateError::NonIntegerHaving { predicate }
            }
            Self::InputExpression { row, column, error } => {
                GraphAggregateError::InputExpression { row, column, error }
            }
            Self::OutputExpression { column, error } => {
                GraphAggregateError::OutputExpression { column, error }
            }
            Self::ResultCountOverflow => GraphAggregateError::ResultCountOverflow,
            Self::MultiplicityUnavailable => GraphAggregateError::MultiplicityUnavailable,
            Self::NonMonotonicGroups => GraphAggregateError::NonMonotonicGroups,
            Self::InvalidReductionInput => GraphAggregateError::InvalidReductionInput,
        }
    }
}
impl<E: core::fmt::Display> core::fmt::Display for GraphAggregateError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => core::fmt::Display::fmt(error, f),
            Self::InputRelation(error) => write!(f, "aggregate input relation: {error}"),
            Self::NonIntegerSum { aggregate } => write!(
                f,
                "SUM aggregate {aggregate} requires integer, float or null input"
            ),
            Self::NonIntegerAverage { aggregate } => write!(
                f,
                "AVG aggregate {aggregate} requires integer, float or null input"
            ),
            Self::ArithmeticOverflow { aggregate } => write!(
                f,
                "aggregate {aggregate} exceeded its exact integer result range"
            ),
            Self::NonIntegerHaving { predicate } => write!(
                f,
                "HAVING predicate {predicate} requires exact numeric or null input"
            ),
            Self::InputExpression { row, column, error } => {
                write!(f, "aggregate input row {row} column {column}: {error}")
            }
            Self::OutputExpression { column, error } => {
                write!(f, "aggregate output column {column}: {error}")
            }
            Self::ResultCountOverflow => f.write_str("aggregate result count overflow"),
            Self::MultiplicityUnavailable => {
                f.write_str("aggregate binding has no admitted topology multiplicity")
            }
            Self::NonMonotonicGroups => {
                f.write_str("aggregate input violated its root-group ordering contract")
            }
            Self::InvalidReductionInput => {
                f.write_str("aggregate partition input does not match its checked definition")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphAggregateError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::InputRelation(error) => Some(error),
            Self::InputExpression { error, .. } | Self::OutputExpression { error, .. } => {
                Some(error)
            }
            _ => None,
        }
    }
}

/// Counts, sums and averages have exact domains, not lossy scalar/float casts.
/// MIN/MAX preserve the original typed value. COLLECT returns Value(List),
/// including an empty list for no nonnull inputs; other empty non-count
/// aggregates produce Value(Scalar(Null)).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphAggregateValue {
    Count(u64),
    Integer(i128),
    Value(GraphValue),
    Average(GraphExactAverage),
}
impl GraphAggregateValue {
    #[must_use]
    pub fn as_count(&self) -> Option<u64> {
        match self {
            Self::Count(value) => Some(*value),
            _ => None,
        }
    }
    #[must_use]
    pub fn as_integer(&self) -> Option<i128> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }
    #[must_use]
    /// Return the exact fraction, including denominator one. This never
    /// coerces it into the distinct Integer or Value result variants.
    pub fn as_average(&self) -> Option<GraphExactAverage> {
        match self {
            Self::Average(value) => Some(*value),
            _ => None,
        }
    }
    #[must_use]
    pub fn as_value(&self) -> Option<&GraphValue> {
        match self {
            Self::Value(value) => Some(value),
            _ => None,
        }
    }
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Value(value) if value.is_null())
    }
}
impl core::fmt::Debug for GraphAggregateValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphAggregateValue([REDACTED])")
    }
}

/// One group and its aggregate values. Positions correspond to key_columns()
/// and aggregate_columns() on the immutable prepared definition. No result
/// lifetime keeps the source snapshot or transaction borrowed after execution.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphAggregateRow {
    keys: Box<[GraphValue]>,
    values: Box<[GraphAggregateValue]>,
}
impl GraphAggregateRow {
    #[must_use]
    pub fn keys(&self) -> &[GraphValue] {
        &self.keys
    }
    #[must_use]
    pub fn values(&self) -> &[GraphAggregateValue] {
        &self.values
    }
    #[must_use]
    pub fn get(&self, aggregate: usize) -> Option<&GraphAggregateValue> {
        self.values.get(aggregate)
    }
}
impl core::fmt::Debug for GraphAggregateRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphAggregateRow")
            .field("key_columns", &self.keys.len())
            .field("aggregate_columns", &self.values.len())
            .field("values", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoundAggregate {
    function: GraphAggregateFunction,
    column: Option<usize>,
}

impl BoundAggregate {
    /// The evaluated aggregate function, including hidden suffix aggregates.
    #[must_use]
    pub fn function(&self) -> GraphAggregateFunction {
        self.function
    }
    /// The value column the aggregate reads; `None` is COUNT(*)-style.
    #[must_use]
    pub fn argument_column(&self) -> Option<usize> {
        self.column
    }
}

/// Only a nonidentity output projection allocates this metadata. Evaluation
/// continues to use the original key sequence, including its tie-break order.
#[derive(Clone, PartialEq, Eq)]
struct KeyProjection {
    columns: Box<[usize]>,
    names: Box<[String]>,
}

/// The native graph owner always retains a real input. The private owned-row
/// reducer below explicitly has no graph source and never exposes graph
/// execution, input accessors or source certificates.
#[derive(Clone, PartialEq, Eq)]
struct AggregateInput(Option<PreparedGraphPattern<GraphValueRow>>);

impl core::ops::Deref for AggregateInput {
    type Target = PreparedGraphPattern<GraphValueRow>;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("a graph aggregate owner always has its admitted source")
    }
}

/// A source-independent owner for the same native accumulator/result engine.
/// Only a complete, schema-checked occurrence bag may enter this private seam.
/// It cannot be converted to a public graph aggregate or execute a source.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct OwnedRowAggregate {
    definition: PreparedGraphAggregate,
}

impl OwnedRowAggregate {
    pub(crate) fn prepare(
        columns: &[String],
        keys: &[usize],
        aggregates: &[GraphAggregate<'_>],
    ) -> Result<Self, GraphAggregateBuildError> {
        Ok(Self {
            definition: PreparedGraphAggregate::prepare_row_columns(
                columns, keys, aggregates, 0, None,
            )?,
        })
    }

    pub(crate) fn summarize<E, C>(
        &self,
        rows: &[GraphValueRow],
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<Vec<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>> {
        self.definition.summarize_projected_rows(rows, control)
    }

    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:owned-row-group:v1\0".to_vec();
        self.definition.append_grouping_transcript(&mut bytes);
        bytes
    }
}

/// Logical GroupAggregate over an immutable child. Ordinary ALL patterns stream
/// directly; computed input and explicit single-source relational pipelines own
/// bounded rows before grouping. A pipeline's existing pages remain input
/// boundaries; offset/count here apply separately to the group output.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphAggregate {
    input: AggregateInput,
    computed_input: Option<Vec<crate::GraphSetProjection>>,
    relational_input: Option<crate::PreparedGraphSet>,
    keys: Vec<usize>,
    aggregates: Vec<BoundAggregate>,
    key_names: Vec<String>,
    key_output: Option<KeyProjection>,
    aggregate_names: Vec<String>,
    output_aggregates: usize,
    output_projection: Option<Vec<crate::GraphSetProjection>>,
    output_names: Vec<String>,
    output_distinct: bool,
    offset: u64,
    count: Option<u64>,
    having: Vec<GraphAggregateFilter>,
    having_expression: Option<GraphHavingExpression>,
    ordering: Vec<GraphAggregateOrder>,
    // Positions in output_projection, never in the evaluation group schema.
    // The two ordering setters make these mutually exclusive with ordering.
    projected_ordering: Vec<crate::algebra::GraphValueOrder>,
}
impl core::fmt::Debug for PreparedGraphAggregate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphAggregate")
            .field("key_columns", &self.key_columns().len())
            .field("evaluated_keys", &self.keys.len())
            .field("aggregate_columns", &self.output_aggregates)
            .field("evaluated_aggregates", &self.aggregates.len())
            .field("computed_input", &self.computed_input.is_some())
            .field("relational_input", &self.relational_input.is_some())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphAggregate {
    pub fn prepare(
        input: PreparedGraphPattern<GraphValueRow>,
        keys: &[usize],
        aggregates: &[GraphAggregate<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<Self, GraphAggregateBuildError> {
        Self::prepare_input(input, None, None, keys, aggregates, offset, count)
    }

    fn prepare_input(
        input: PreparedGraphPattern<GraphValueRow>,
        computed_input: Option<Vec<crate::GraphSetProjection>>,
        relational_input: Option<crate::PreparedGraphSet>,
        keys: &[usize],
        aggregates: &[GraphAggregate<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<Self, GraphAggregateBuildError> {
        // A plain graph aggregate reads its input only through the projected
        // columns, so a named relationship none of them reads need not be
        // captured (fgdb-61p1n): the definition is its unnamed spelling's.
        // Only counts and extrema take that definition. Their answers do not
        // depend on the order occurrences arrive in, which an identified and
        // an unidentified plan need not share; COLLECT and the sums keep
        // every capture.
        let order_free = aggregates.iter().all(|aggregate| {
            matches!(
                aggregate.function,
                GraphAggregateFunction::CountRows
                    | GraphAggregateFunction::Count
                    | GraphAggregateFunction::CountDistinct
                    | GraphAggregateFunction::Min
                    | GraphAggregateFunction::Max
            )
        });
        let input = if order_free && computed_input.is_none() && relational_input.is_none() {
            input.without_unread_captures()
        } else {
            input
        };
        if relational_input.is_none()
            && (!input.preserves_duplicates()
                || !matches!(
                    input.plan().operators().last(),
                    Some(GlaOperator::Limit {
                        offset: 0,
                        count: None
                    })
                ))
        {
            return Err(GraphAggregateBuildError::RequiresUnpaginatedAll);
        }
        Self::admit_group_width(keys.len(), aggregates.len())?;
        let projected_columns = computed_input
            .as_deref()
            .map(|projection| computed::projected_schema(&input, projection))
            .transpose()?;
        let columns = relational_input
            .as_ref()
            .map(crate::PreparedGraphSet::columns)
            .or(projected_columns.as_deref())
            .unwrap_or(input.columns());
        let mut definition = Self::prepare_row_columns(columns, keys, aggregates, offset, count)?;
        definition.input = AggregateInput(Some(input));
        definition.computed_input = computed_input;
        definition.relational_input = relational_input;
        Ok(definition)
    }

    fn admit_group_width(keys: usize, aggregates: usize) -> Result<(), GraphAggregateBuildError> {
        if aggregates == 0 {
            return Err(GraphAggregateBuildError::EmptyAggregates);
        }
        let width = keys.saturating_add(aggregates);
        if width > MAX_PATTERN_VERTICES {
            return Err(GraphAggregateBuildError::TooManyColumns {
                limit: MAX_PATTERN_VERTICES,
                observed: width,
            });
        }
        Ok(())
    }

    /// Shared column/key/function admission. An absent source is private to
    /// OwnedRowAggregate; public constructors install their actual source
    /// before returning the prepared definition.
    fn prepare_row_columns(
        columns: &[String],
        keys: &[usize],
        aggregates: &[GraphAggregate<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<Self, GraphAggregateBuildError> {
        Self::admit_group_width(keys.len(), aggregates.len())?;
        let mut names = BTreeSet::new();
        for (at, column) in keys.iter().enumerate() {
            let name = columns
                .get(*column)
                .ok_or(GraphAggregateBuildError::UnknownColumn { column: *column })?;
            if keys[..at].contains(column) {
                return Err(GraphAggregateBuildError::DuplicateKey { column: *column });
            }
            names.insert(name.as_str());
        }
        for aggregate in aggregates {
            if let Some(column) = aggregate.column
                && column >= columns.len()
            {
                return Err(GraphAggregateBuildError::UnknownColumn { column });
            }
            if !crate::algebra::valid_column_name(aggregate.name) {
                return Err(GraphAggregateBuildError::InvalidName);
            }
            if !names.insert(aggregate.name) {
                return Err(GraphAggregateBuildError::DuplicateName);
            }
        }
        let key_names = keys.iter().map(|column| columns[*column].clone()).collect();
        let aggregate_names = aggregates
            .iter()
            .map(|aggregate| aggregate.name.to_owned())
            .collect();
        let output_aggregates = aggregates.len();
        let aggregates = aggregates
            .iter()
            .map(|aggregate| BoundAggregate {
                function: aggregate.function,
                column: aggregate.column,
            })
            .collect();
        Ok(Self {
            input: AggregateInput(None),
            computed_input: None,
            relational_input: None,
            keys: keys.to_vec(),
            aggregates,
            key_names,
            key_output: None,
            aggregate_names,
            output_aggregates,
            output_projection: None,
            output_names: Vec::new(),
            output_distinct: false,
            offset,
            count,
            having: Vec::new(),
            having_expression: None,
            ordering: Vec::new(),
            projected_ordering: Vec::new(),
        })
    }

    /// The actual sole graph source for storage admission, not the transformed
    /// relation or group output. execute_governed owns every row stage, including
    /// any local page or DISTINCT, before invoking the shared group engine.
    #[must_use]
    pub fn input_pattern(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.input
    }
    /// Whether execute_governed answers this definition exactly from
    /// `(source, relation, target)` topology although its plan requires
    /// identified edges: a clause-complete DIFFERENT EDGES count (fgdb-ujaad)
    /// or a support-only quantified reach (fgdb-20vwx). Hosts that route on
    /// `requires_identified_edges` use this to keep such definitions on the
    /// binding visitor instead of materializing every pattern row.
    #[must_use]
    pub fn answers_identified_edges_from_topology(&self) -> bool {
        self.input.plan().requires_identified_edges() && weighted::answers_identified_exactly(self)
    }
    #[must_use]
    pub fn key_columns(&self) -> &[String] {
        if self.output_projection.is_some() {
            return &[];
        }
        match &self.key_output {
            Some(projection) => &projection.names,
            None => &self.key_names,
        }
    }
    /// Full grouping schema used by HAVING, ORDER BY and canonical tie breaks.
    /// Output projection never removes a key from this evaluation schema.
    #[must_use]
    pub fn evaluation_key_columns(&self) -> &[String] {
        &self.key_names
    }
    #[must_use]
    pub fn aggregate_columns(&self) -> &[String] {
        if self.output_projection.is_some() {
            return &self.output_names;
        }
        &self.aggregate_names[..self.output_aggregates]
    }

    /// The evaluation aggregate specs, including hidden suffix aggregates.
    /// These describe EVALUATION, not the visible output schema: output
    /// projection, distinct and pagination modifiers stay independent.
    #[must_use]
    pub fn aggregates(&self) -> &[BoundAggregate] {
        &self.aggregates
    }

    /// The evaluation grouping-key columns, before any output projection.
    #[must_use]
    pub fn group_key_columns(&self) -> &[usize] {
        &self.keys
    }

    /// Whether this definition has the untransformed aggregate shape supported
    /// by the current delta maintainer. This does not validate the input pattern
    /// or aggregate functions; the consumer must check those separately.
    #[must_use]
    pub fn supports_incremental_maintenance(&self) -> bool {
        self.computed_input.is_none()
            && self.relational_input.is_none()
            && self.having.is_empty()
            && self.having_expression.is_none()
            && self.output_projection.is_none()
            && !self.output_distinct
            && self.offset == 0
            && self.count.is_none()
            && self.ordering.is_empty()
            && self.key_output.is_none()
            && self.output_aggregates == self.aggregates.len()
    }

    /// Construct the plain global COUNT/SUM row used by a delta maintainer.
    /// Refuses transformed definitions and mismatched result domains.
    pub fn incremental_global_row(
        &self,
        values: Vec<GraphAggregateValue>,
    ) -> Option<GraphAggregateRow> {
        if !self.supports_incremental_maintenance()
            || !self.keys.is_empty()
            || values.len() != self.aggregates.len()
            || !self
                .aggregates
                .iter()
                .zip(&values)
                .all(|(aggregate, value)| {
                    matches!(
                        (aggregate.function, value),
                        (
                            GraphAggregateFunction::CountRows | GraphAggregateFunction::Count,
                            GraphAggregateValue::Count(_)
                        ) | (
                            GraphAggregateFunction::SumInt,
                            GraphAggregateValue::Integer(_)
                        )
                    ) || (aggregate.function == GraphAggregateFunction::SumInt && value.is_null())
                })
        {
            return None;
        }
        Some(GraphAggregateRow {
            keys: Box::new([]),
            values: values.into_boxed_slice(),
        })
    }

    /// The full evaluation schema used by HAVING, ORDER BY, and aggregate
    /// error indices. A suffix can be internal to those clauses and therefore
    /// absent from aggregate_columns() and GraphAggregateRow::values().
    #[must_use]
    pub fn evaluation_aggregate_columns(&self) -> &[String] {
        &self.aggregate_names
    }

    /// Select grouping-key positions for owned output, in the requested order.
    /// Indices address evaluation_key_columns(), never a previous projection.
    /// Repetitions and the empty selection are valid within the fixed column
    /// bound. All keys still define groups, participate in clauses and break
    /// sort ties before pagination. Equal projected rows remain separate bag
    /// occurrences unless with_distinct_output(true) is selected explicitly.
    /// Hidden keys retain source reads, failures and transaction observations,
    /// but their payloads are not cloned into the returned rows.
    pub fn with_key_output_columns(
        mut self,
        columns: &[usize],
    ) -> Result<Self, GraphAggregateBuildError> {
        // Reserve width for every evaluated aggregate, even a currently hidden
        // one, so restoring its output prefix cannot invalidate this bound.
        let width = columns.len().saturating_add(self.aggregates.len());
        if width > MAX_PATTERN_VERTICES {
            return Err(GraphAggregateBuildError::TooManyColumns {
                limit: MAX_PATTERN_VERTICES,
                observed: width,
            });
        }
        for &column in columns {
            if column >= self.keys.len() {
                return Err(GraphAggregateBuildError::UnknownOutputColumn {
                    column: GraphAggregateColumn::GroupKey(column),
                });
            }
        }
        self.key_output = if columns.iter().copied().eq(0..self.keys.len()) {
            None
        } else {
            Some(KeyProjection {
                columns: columns.into(),
                names: columns
                    .iter()
                    .map(|column| self.key_names[*column].clone())
                    .collect(),
            })
        };
        Ok(self)
    }

    /// Name grouping keys by the RETURN aliases that show them (fgdb-cxjqp).
    /// Each pair renames evaluation key `at`; every other key keeps its input
    /// column's name. Only names change, never which cells group, evaluate,
    /// order or appear, and names are not part of the canonical bytes. So an
    /// aliased key reports the name its result column has, as a hidden-free
    /// graph aggregate already does. An alias may not repeat or name a summary.
    pub fn with_key_aliases(
        mut self,
        aliases: &[(usize, String)],
    ) -> Result<Self, GraphAggregateBuildError> {
        for (index, (at, alias)) in aliases.iter().enumerate() {
            if *at >= self.keys.len() {
                return Err(GraphAggregateBuildError::UnknownOutputColumn {
                    column: GraphAggregateColumn::GroupKey(*at),
                });
            }
            if !crate::algebra::valid_column_name(alias) {
                return Err(GraphAggregateBuildError::InvalidName);
            }
            if aliases[..index]
                .iter()
                .any(|(earlier, name)| earlier == at || name == alias)
                || self.aggregate_names.contains(alias)
            {
                return Err(GraphAggregateBuildError::DuplicateName);
            }
        }
        for (at, alias) in aliases {
            self.key_names[*at] = alias.clone();
        }
        if let Some(projection) = &mut self.key_output {
            projection.names = (projection.columns.iter())
                .map(|column| self.key_names[*column].clone())
                .collect();
        }
        Ok(self)
    }

    /// Select DISTINCT over the visible keys and aggregate prefix. Filtering
    /// and complete group ordering precede duplicate elimination; pagination
    /// follows it. Equal output tuples retain their first ranked group, even
    /// when ranking uses hidden cells. No child matches are deduplicated.
    /// This setting survives later projection changes. With all grouping keys
    /// visible, uniqueness is structural and no distinct buffer is necessary.
    #[must_use]
    pub fn with_distinct_output(mut self, distinct: bool) -> Self {
        self.output_distinct = distinct;
        self
    }

    /// Return only the first `count` aggregate values without changing grouping
    /// or the key output projection. All aggregates remain evaluated and
    /// available to HAVING and ORDER BY. This is late projection, not permission
    /// to omit source reads, arithmetic, failures, or transaction observations.
    /// A zero prefix hides every summary. Replacing this prefix never renumbers
    /// the evaluation schema.
    pub fn with_aggregate_output_prefix(
        mut self,
        count: usize,
    ) -> Result<Self, GraphAggregateBuildError> {
        if count > self.aggregates.len() {
            return Err(GraphAggregateBuildError::TooManyColumns {
                limit: self.aggregates.len(),
                observed: count,
            });
        }
        self.output_aggregates = count;
        Ok(self)
    }

    /// Application logical transcript: GroupAggregate(child, keys, functions),
    /// canonical group ordering and pagination. Aliases are schema metadata.
    /// This is neither a durable-format declaration nor an evidence certificate.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:graph-group-aggregate:v1\0".to_vec();
        let child = self.input.canonical_bytes();
        bytes.extend_from_slice(&(child.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&child);
        self.append_grouping_transcript(&mut bytes);
        bytes.extend_from_slice(&self.offset.to_be_bytes());
        bytes.push(u8::from(self.count.is_some()));
        if let Some(count) = self.count {
            bytes.extend_from_slice(&count.to_be_bytes());
        }
        self.append_result_transcript(&mut bytes);
        if let Some(expression) = &self.having_expression {
            expression.append_transcript(&mut bytes);
        }
        if self.output_aggregates != self.aggregates.len() {
            // The unchanged self-delimiting prefix still describes the full
            // evaluation. Identity projection preserves all existing bytes.
            bytes.extend_from_slice(b"fgdb:aggregate-output-prefix:v1\0");
            bytes.extend_from_slice(&(self.output_aggregates as u64).to_be_bytes());
        }
        if let Some(projection) = &self.key_output {
            bytes.extend_from_slice(b"fgdb:aggregate-key-output:v1\0");
            bytes.extend_from_slice(&(projection.columns.len() as u64).to_be_bytes());
            for column in &projection.columns {
                bytes.extend_from_slice(&(*column as u64).to_be_bytes());
            }
        }
        if self.output_distinct {
            bytes.extend_from_slice(b"fgdb:aggregate-output-distinct:v1\0");
        }
        if let Some(projection) = &self.output_projection {
            bytes.extend_from_slice(b"fgdb:aggregate-output-projection:v1\0");
            bytes.extend_from_slice(&(projection.len() as u64).to_be_bytes());
            for expression in projection {
                expression.value().append_canonical_bytes(&mut bytes);
            }
        }
        if !self.projected_ordering.is_empty() {
            // Only the new result-order profile extends the transcript.
            // Projection expressions above retain their exact numeric domains.
            bytes.extend_from_slice(b"fgdb:aggregate-projected-order:v1\0");
            bytes.extend_from_slice(&(self.projected_ordering.len() as u64).to_be_bytes());
            for order in &self.projected_ordering {
                bytes.extend_from_slice(&(order.column as u64).to_be_bytes());
                bytes.push(u8::from(order.descending));
                bytes.push(u8::from(order.nulls_first));
            }
        }
        self.append_input_projection(&mut bytes);
        if let Some(input) = &self.relational_input {
            bytes.extend_from_slice(b"fgdb:aggregate-relational-input:v1\0");
            let relation = input.canonical_bytes();
            bytes.extend_from_slice(&(relation.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&relation);
        }
        bytes
    }

    fn append_grouping_transcript(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&(self.keys.len() as u64).to_be_bytes());
        for column in &self.keys {
            bytes.extend_from_slice(&(*column as u64).to_be_bytes());
        }
        bytes.extend_from_slice(&(self.aggregates.len() as u64).to_be_bytes());
        for aggregate in &self.aggregates {
            bytes.push(match aggregate.function {
                GraphAggregateFunction::CountRows => 0,
                GraphAggregateFunction::Count => 1,
                GraphAggregateFunction::CountDistinct => 2,
                GraphAggregateFunction::SumInt => 3,
                // Tags 4/5 denoted canonical storage-order extrema.
                GraphAggregateFunction::Min => 11,
                GraphAggregateFunction::Max => 12,
                GraphAggregateFunction::SumIntDistinct => 6,
                GraphAggregateFunction::AverageInt => 7,
                GraphAggregateFunction::AverageIntDistinct => 8,
                GraphAggregateFunction::Collect => 9,
                GraphAggregateFunction::CollectDistinct => 10,
            });
            if let Some(column) = aggregate.column {
                bytes.extend_from_slice(&(column as u64).to_be_bytes());
            }
        }
    }

    /// Evaluate an already admitted source under one policy. Storage callers
    /// deduct source work/scratch first and add that consumption back to the
    /// returned counters. Count/null/sum semantics are applied per occurrence.
    /// Keyless aggregation yields one zero/null row on empty input; keyed
    /// aggregation yields no rows, even when every key is hidden from output.
    /// Group ordering and any output DISTINCT precede output pagination.
    /// Positive topology-only summaries may factor parallel occurrences into
    /// checked weights. Property reads, predicates, scoped matches, and mixed
    /// directed/undirected use of one relation keep ordinary visitation. The
    /// physical optimization does not change the logical transcript or source
    /// record count; its preprocessing shares the evaluator's resource meter.
    /// A finite page grouped by the original edge-scan source identity can
    /// retire each completed group immediately when physical access proves
    /// contiguity. It retains one active group and a bounded ranked prefix of
    /// compact summaries; source/index admission and cumulative scratch charges
    /// remain independent of this live group-state bound.
    /// Computed inputs and explicit relational pipelines instead use bounded
    /// owned rows and the same group accumulators. Their transformed keys and
    /// values never enter the physical contiguity or multiplicity shortcut.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl crate::GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        type NoTest<E> = fn(u32, &[VertexPredicate]) -> Result<bool, E>;
        self.execute_input(
            snapshot_records,
            vertices,
            edges,
            None::<(&PositionedGraph<'_, ()>, NoTest<E>)>,
            test_vertex,
            property,
            policy,
            checkpoint,
        )
    }

    /// [`Self::execute_governed`] over a positioned graph: its vertices,
    /// and its edges as identity triples in arrival order, with the same
    /// value, statistics and refusals. `test_at` is `test_vertex` for the
    /// vertex at a position. Every reader must be pure (the same answer for
    /// the same input, with no effect), so a visitor that keeps per-vertex
    /// state by position may skip a read that only repeats one it made;
    /// every event is still charged as the identity-keyed visitor charges it.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_positioned<'a, T, E, C>(
        &self,
        snapshot_records: u64,
        graph: &PositionedGraph<'_, T>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        test_at: impl FnMut(u32, &[VertexPredicate]) -> Result<bool, E>,
        property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl crate::GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        self.execute_input(
            snapshot_records,
            graph.vertices(),
            graph.triples(),
            Some((graph, test_at)),
            test_vertex,
            property,
            policy,
            checkpoint,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_input<'a, T, A, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        positioned: Option<(&PositionedGraph<'_, T>, A)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl crate::GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    where
        A: FnMut(u32, &[VertexPredicate]) -> Result<bool, E>,
    {
        if self.input.plan().projects_property_maps() {
            checkpoint.unit().map_err(GqlQueryError::Interrupted)?;
            return Err(crate::GraphIntegerError {
                instruction: 0,
                kind: crate::GraphIntegerErrorKind::PropertyMapSourceRequired,
            }
            .into());
        }
        if self.input.plan().requires_identified_edges()
            && !weighted::answers_identified_exactly(self)
        {
            return Err(GqlQueryError::IdentifiedEdgesRequired);
        }
        if self.relational_input.is_some() {
            return self.execute_relational_governed(
                snapshot_records,
                vertices,
                edges,
                test_vertex,
                property,
                policy,
                || checkpoint.unit(),
            );
        }
        if self.computed_input.is_some() {
            return self.execute_projected_governed(
                snapshot_records,
                vertices,
                edges,
                test_vertex,
                property,
                policy,
                || checkpoint.unit(),
            );
        }
        checkpoint.unit().map_err(GqlQueryError::Interrupted)?;
        policy
            .rows
            .check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(GqlQueryError::Rows)?;
        // Evaluator runs, for a host that takes them: while the evaluator's
        // allowances cover two whole runs, events are counted and their host
        // units, work and scratch charged together (the host's in one call).
        // The Work and scratch fast path is all the closure holds, so it
        // inlines into the hottest loops; everything else is `Charges::slow`.
        let mut charges = Charges {
            checkpoint,
            policy,
            rows: GqlExecutionStats {
                snapshot_records,
                result_rows: 0,
            },
            evaluator: GlaExecutionStats::default(),
            left: 0,
            opened: 0,
            scratch: 0,
        };
        let mut control = |event| charges.event(event);
        let mut streaming = result::RootGroups::new(self);
        let mut groups = groups::GroupTable::new(self.keys.len());
        if self.keys.is_empty() {
            control(GlaExecutionEvent::ScratchEntry)?;
            groups.insert(Vec::new(), new_group(&self.aggregates, &mut control)?);
        }
        // One binding's values and group key, reused by every binding. A
        // fresh pair of pattern-wide arrays per binding was about 5 KB of
        // stores each, most of a vertex count's time. A column past the
        // binding's own is null, as in a fresh array.
        let mut values = [ValueRef::Scalar(&NULL); MAX_PATTERN_VERTICES];
        let mut key = [ValueRef::Scalar(&NULL); MAX_PATTERN_VERTICES];
        let mut written = 0;
        let visited = weighted::visit_bindings(
            self,
            vertices,
            edges,
            positioned.map(|(graph, mut test_at)| weighted::Positioned {
                graph,
                test_at: move |at, predicates: &[VertexPredicate]| {
                    test_at(at, predicates)
                        .map_err(|error| GqlQueryError::Source(GraphAggregateError::Source(error)))
                },
            }),
            |vid, predicates| {
                test_vertex(vid, predicates)
                    .map_err(|error| GqlQueryError::Source(GraphAggregateError::Source(error)))
            },
            |vid, key| {
                property(vid, key)
                    .map_err(|error| GqlQueryError::Source(GraphAggregateError::Source(error)))
            },
            &mut control,
            |columns, bindings, property, control, multiplicity| {
                values[columns.len()..written.max(columns.len())].fill(ValueRef::Scalar(&NULL));
                written = columns.len();
                for (at, column) in columns.iter().enumerate() {
                    control(GlaExecutionEvent::Work)?;
                    values[at] = match column {
                        ValueProjection::Vertex { slot } => bindings[slot.ordinal() as usize]
                            .map_or(ValueRef::Scalar(&NULL), ValueRef::Vertex),
                        ValueProjection::Property { slot, key } => {
                            let value = match bindings[slot.ordinal() as usize] {
                                Some(vid) => property(vid, *key)?,
                                None => None,
                            };
                            ValueRef::Scalar(value.unwrap_or(&NULL))
                        }
                        ValueProjection::Path { .. }
                        | ValueProjection::EdgeProperty { .. }
                        | ValueProjection::Properties { .. }
                        | ValueProjection::EdgeProperties { .. }
                        | ValueProjection::Labels { .. }
                        | ValueProjection::Type { .. } => {
                            return Err(GqlQueryError::IdentifiedEdgesRequired);
                        }
                    };
                    for _ in 0..values[at].payload_units() {
                        control(GlaExecutionEvent::Work)?;
                    }
                }
                if let Some(streaming) = &mut streaming {
                    return streaming.push(&values[..columns.len()], multiplicity, control);
                }
                for (at, column) in self.keys.iter().enumerate() {
                    control(GlaExecutionEvent::Work)?;
                    key[at] = values[*column];
                }
                let key = &key[..self.keys.len()];
                control(GlaExecutionEvent::Work)?;
                // One probe for an existing group, two for a new one (it was
                // three tree descents), with the same events in the same order.
                if let Some(state) = groups.get_mut(key) {
                    return update_group(&self.aggregates, state, &values, multiplicity, control);
                }
                control(GlaExecutionEvent::ScratchEntry)?;
                let mut owned_key = Vec::new();
                for value in key {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    owned_key.push(*value);
                }
                let state = new_group(&self.aggregates, control)?;
                let state = groups.insert(owned_key, state);
                update_group(&self.aggregates, state, &values, multiplicity, control)
            },
        );
        let finished = visited.and_then(|()| {
            let value = match streaming {
                Some(streaming) => streaming.finish(&mut control)?,
                None => self.finish_groups(&groups.into_tree(), &mut control)?,
            };
            // Even empty and zero-count outputs observe a terminal checkpoint.
            control(GlaExecutionEvent::Work)?;
            Ok(value)
        });
        // A run's units are charged before the outcome is judged; a host
        // refusal among them is the one per-event charging met first. A
        // settled or refused run leaves none, so a host that has refused is
        // not called again.
        charges.settle()?;
        Ok(GqlQueryExecution {
            value: finished?,
            rows: charges.rows,
            evaluator: charges.evaluator,
        })
    }
}

/// Events one evaluator run defers.
const EVALUATOR_RUN: u64 = 1024;

/// A governed aggregate's charges: the host checkpoint, row and evaluator
/// counters, and the open evaluator run.
struct Charges<K> {
    checkpoint: K,
    policy: GqlQueryPolicy,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
    /// Events the open run may still take; `opened` is its size, so it has
    /// taken `opened - left`, each owing its host unit and evaluator work.
    left: u64,
    opened: u64,
    /// Scratch entries the open run has taken, owing their scratch charge.
    scratch: u64,
}

impl<K> Charges<K> {
    /// A Work event or scratch entry inside a run is only counted: the
    /// closure's whole body, so it inlines into the hottest loops.
    #[inline(always)]
    fn event<E, C>(
        &mut self,
        event: GlaExecutionEvent,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    where
        K: crate::GovernedCheckpoint<C>,
    {
        if self.left > 0 {
            match event {
                GlaExecutionEvent::Work => {
                    self.left -= 1;
                    return Ok(());
                }
                GlaExecutionEvent::ScratchEntry => {
                    self.left -= 1;
                    self.scratch += 1;
                    return Ok(());
                }
                GlaExecutionEvent::ResultRow => {}
            }
        }
        self.slow(event)
    }

    /// Every event the fast path does not take. A spent run settles; the
    /// next opens while the host takes runs and the evaluator's work and
    /// scratch allowances each cover two whole runs, so no evaluator refusal
    /// can fall inside one. An event outside a run is charged on the spot:
    /// host, rows, then evaluator, the per-event order.
    #[inline(never)]
    fn slow<E, C>(
        &mut self,
        event: GlaExecutionEvent,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    where
        K: crate::GovernedCheckpoint<C>,
    {
        if self.left == 0 {
            self.settle()?;
            let limits = self.policy.evaluator;
            let covered = |limit: u64, used: u64| limit.saturating_sub(used) >= 2 * EVALUATOR_RUN;
            if !K::RUNS
                || !covered(limits.max_work_units, self.evaluator.work_units)
                || !covered(limits.max_scratch_entries, self.evaluator.scratch_entries)
            {
                self.checkpoint.unit().map_err(GqlQueryError::Interrupted)?;
                return self.judge(event);
            }
            self.left = EVALUATOR_RUN;
            self.opened = EVALUATOR_RUN;
            if event != GlaExecutionEvent::ResultRow {
                return self.event(event);
            }
        }
        // Inside a run, a result row's limit is judged now while its host
        // unit and evaluator work wait. Per event, the host was charged for
        // it and for every event before it first, so a host refusal among
        // them wins.
        self.left -= 1;
        let next = self.rows.result_rows + 1;
        if let Err(error) = self.policy.rows.check(GqlBudgetDimension::ResultRows, next) {
            self.settle()?;
            return Err(GqlQueryError::Rows(error));
        }
        self.rows.result_rows = next;
        Ok(())
    }

    /// The row and evaluator charges of one event.
    fn judge<E, C>(
        &mut self,
        event: GlaExecutionEvent,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        if event == GlaExecutionEvent::ResultRow {
            let next = self.rows.result_rows + 1;
            self.policy
                .rows
                .check(GqlBudgetDimension::ResultRows, next)
                .map_err(GqlQueryError::Rows)?;
            self.rows.result_rows = next;
        }
        self.evaluator
            .charge_event(self.policy.evaluator, event)
            .map_err(GqlQueryError::Evaluator)
    }

    /// Charge the run: its host units in one checkpoint call, in event order,
    /// then its events' evaluator work and its scratch entries. Nothing is
    /// left pending, so a host that refused here is never called again. A
    /// run opens only while the evaluator's allowances cover two whole runs,
    /// so only the host can refuse here, at its own per-unit ordinal.
    fn settle<E, C>(&mut self) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>
    where
        K: crate::GovernedCheckpoint<C>,
    {
        let taken = self.opened - self.left;
        let scratch = std::mem::take(&mut self.scratch);
        self.left = 0;
        self.opened = 0;
        if taken > 0 {
            self.checkpoint
                .units(taken)
                .map_err(GqlQueryError::Interrupted)?;
            self.evaluator
                .charge_work_units(self.policy.evaluator, taken)
                .map_err(GqlQueryError::Evaluator)?;
        }
        if scratch > 0 {
            self.evaluator
                .charge_scratch_entries(self.policy.evaluator, scratch)
                .map_err(GqlQueryError::Evaluator)?;
        }
        Ok(())
    }
}

static NULL: CanonicalScalar = CanonicalScalar::Null;

// Ordering is the same disjoint typed-cell ordering as GraphValue. Borrowed
// keys and distinct arguments remain tied to one immutable source execution.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ValueRef<'a> {
    Scalar(&'a CanonicalScalar),
    Vertex(VId),
    Path(&'a GraphPath),
    Vertices(&'a [VId]),
    Edges(&'a [EId]),
    Edge(EId),
    List(&'a [GraphValue]),
    Map {
        keys: &'a [Box<str>],
        values: &'a [GraphValue],
    },
}
impl<'a> ValueRef<'a> {
    fn orderability_value(self) -> crate::algebra::ValueRef<'a> {
        use crate::algebra::ValueRef as Value;
        match self {
            Self::Scalar(value) => Value::Scalar(value),
            Self::Vertex(value) => Value::Vertex(value),
            Self::Edge(value) => Value::Edge(value),
            Self::Path(value) => Value::Path(value),
            Self::Vertices(value) => Value::Vertices(value),
            Self::Edges(value) => Value::Edges(value),
            Self::List(value) => Value::List(value),
            Self::Map { keys, values } => Value::Map { keys, values },
        }
    }

    fn is_null(self) -> bool {
        matches!(self, Self::Scalar(CanonicalScalar::Null))
    }
    fn payload_units(self) -> usize {
        if let Self::List(values) = self {
            return values.iter().fold(values.len(), |units, value| {
                units.saturating_add(value.payload_units())
            });
        }
        if let Self::Map { keys, values } = self {
            let keys = keys.iter().fold(values.len(), |units, key| {
                units.saturating_add(key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES))
            });
            return values.iter().fold(keys, |units, value| {
                units.saturating_add(value.payload_units())
            });
        }
        let bytes = match self {
            Self::Scalar(CanonicalScalar::Bytes(value)) => value.as_slice().len(),
            Self::Scalar(CanonicalScalar::Text(value)) => {
                value.len() + value.canonical_sort_key().map_or(0, <[u8]>::len)
            }
            Self::Scalar(CanonicalScalar::Timestamp(value)) => {
                value.zone().map_or(0, |zone| zone.identifier().len())
            }
            Self::Path(value) => core::mem::size_of_val(value.steps()),
            Self::Vertices(value) => core::mem::size_of_val(value),
            Self::Edges(value) => core::mem::size_of_val(value),
            _ => 0,
        };
        bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
    }
    fn copy_owned<E>(
        self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValue, E> {
        if let Self::List(values) = self {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut owned = Vec::new();
            for value in values {
                owned.push(value.copy_with_control(control)?);
            }
            return Ok(GraphValue::List(owned.into_boxed_slice()));
        }
        if let Self::Map { keys, values } = self {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            for key in keys {
                for _ in 0..=key.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
            }
            let mut owned = Vec::new();
            for value in values {
                owned.push(value.copy_with_control(control)?);
            }
            return Ok(GraphValue::Map {
                keys: keys.into(),
                values: owned.into_boxed_slice(),
            });
        }
        control(GlaExecutionEvent::ScratchEntry)?;
        for _ in 0..self.payload_units() {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        Ok(match self {
            Self::Scalar(value) => GraphValue::Scalar(value.clone()),
            Self::Vertex(value) => GraphValue::Vertex(value),
            Self::Path(value) => GraphValue::Path(value.clone()),
            Self::Vertices(value) => GraphValue::Vertices(value.into()),
            Self::Edges(value) => GraphValue::Edges(value.into()),
            Self::Edge(value) => GraphValue::Edge(value),
            Self::List(_) | Self::Map { .. } => {
                unreachable!("list and map copying is recursively governed above")
            }
        })
    }
}

enum Accumulator<'a> {
    Count(u64),
    Distinct(BTreeSet<ValueRef<'a>>),
    Sum {
        value: i128,
        present: bool,
    },
    Extreme(Option<ValueRef<'a>>),
    Numeric(numeric::NumericAccumulator),
    Collect {
        values: Vec<GraphValue>,
        seen: Option<BTreeSet<ValueRef<'a>>>,
        max_payload: usize,
    },
}

fn new_group<'a, E>(
    aggregates: &[BoundAggregate],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Vec<Accumulator<'a>>, E> {
    let mut state = Vec::new();
    for aggregate in aggregates {
        control(GlaExecutionEvent::ScratchEntry)?;
        state.push(match aggregate.function {
            GraphAggregateFunction::CountRows | GraphAggregateFunction::Count => {
                Accumulator::Count(0)
            }
            GraphAggregateFunction::CountDistinct => Accumulator::Distinct(BTreeSet::new()),
            GraphAggregateFunction::SumInt => Accumulator::Sum {
                value: 0,
                present: false,
            },
            GraphAggregateFunction::SumIntDistinct => {
                Accumulator::Numeric(numeric::NumericAccumulator::new(false, true))
            }
            GraphAggregateFunction::AverageInt => {
                Accumulator::Numeric(numeric::NumericAccumulator::new(true, false))
            }
            GraphAggregateFunction::AverageIntDistinct => {
                Accumulator::Numeric(numeric::NumericAccumulator::new(true, true))
            }
            GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct => {
                Accumulator::Collect {
                    values: Vec::new(),
                    seen: (aggregate.function == GraphAggregateFunction::CollectDistinct)
                        .then(BTreeSet::new),
                    max_payload: 0,
                }
            }
            GraphAggregateFunction::Min | GraphAggregateFunction::Max => Accumulator::Extreme(None),
        });
    }
    Ok(state)
}

fn update_group<'a, E, C>(
    aggregates: &[BoundAggregate],
    states: &mut [Accumulator<'a>],
    values: &[ValueRef<'a>],
    multiplicity: weighted::Multiplicity,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
    for (at, (aggregate, state)) in aggregates.iter().zip(states).enumerate() {
        control(GlaExecutionEvent::Work)?;
        let value = aggregate.column.map(|column| values[column]);
        update(state, aggregate.function, value, multiplicity, at, control)?;
    }
    Ok(())
}

fn update<'a, E, C>(
    state: &mut Accumulator<'a>,
    function: GraphAggregateFunction,
    value: Option<ValueRef<'a>>,
    multiplicity: weighted::Multiplicity,
    aggregate: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
    if function != GraphAggregateFunction::CountRows && value.is_none_or(ValueRef::is_null) {
        return Ok(());
    }
    let overflow = || GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate });
    match state {
        Accumulator::Count(count) => {
            let delta = multiplicity.exact_count().ok_or_else(overflow)?;
            *count = count.checked_add(delta).ok_or_else(overflow)?;
        }
        Accumulator::Distinct(seen) => {
            let value = value.expect("non-count argument was checked");
            if !seen.contains(&value) {
                control(GlaExecutionEvent::ScratchEntry)?;
                seen.insert(value);
            }
        }
        Accumulator::Collect {
            values,
            seen,
            max_payload,
        } => {
            // Collection must visit every occurrence; topology weights do not
            // retain the row order and are deliberately ineligible.
            if multiplicity != weighted::Multiplicity::ONE {
                return Err(GqlQueryError::Source(
                    GraphAggregateError::MultiplicityUnavailable,
                ));
            }
            let value = value.expect("nonnull collection argument was checked");
            if let Some(seen) = seen {
                let payload = value.payload_units();
                *max_payload = (*max_payload).max(payload);
                // Bound both tree searches and recursive payload comparisons
                // before the set can inspect or retain the argument.
                let levels = (seen.len().saturating_add(1)).ilog2() as usize + 1;
                for _ in 0..levels
                    .saturating_mul(24)
                    .saturating_mul(max_payload.saturating_add(1))
                {
                    control(GlaExecutionEvent::Work)?;
                }
                if seen.contains(&value) {
                    return Ok(());
                }
                control(GlaExecutionEvent::ScratchEntry)?;
                for _ in 0..payload {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                seen.insert(value);
            }
            control(GlaExecutionEvent::Work)?;
            values.push(value.copy_owned(control)?);
        }
        Accumulator::Sum {
            value: sum,
            present,
        } => {
            // Weighted execution never admits SUM. Preserve its ordinary
            // checked per-occurrence semantics and fail closed on misrouting.
            if multiplicity != weighted::Multiplicity::ONE {
                return Err(GqlQueryError::Source(
                    GraphAggregateError::MultiplicityUnavailable,
                ));
            }
            let value = match value {
                Some(ValueRef::Scalar(CanonicalScalar::Int(value))) => value,
                Some(ValueRef::Scalar(CanonicalScalar::Float(_))) => {
                    // The first binary64 input moves this SUM to the exact
                    // numeric state; its integer prefix carries over exactly.
                    let mut exact = numeric::NumericAccumulator::continue_sum(*sum, *present);
                    exact.update(
                        value.expect("a binary64 input is present"),
                        aggregate,
                        control,
                    )?;
                    *state = Accumulator::Numeric(exact);
                    return Ok(());
                }
                _ => {
                    return Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
                        aggregate,
                    }));
                }
            };
            *sum = sum.checked_add(i128::from(*value)).ok_or_else(overflow)?;
            *present = true;
        }
        Accumulator::Numeric(state) => {
            // Property-aware numeric functions retain ordinary visitation.
            // Never accept a support-only/overflowed topology weight here.
            if multiplicity != weighted::Multiplicity::ONE {
                return Err(GqlQueryError::Source(
                    GraphAggregateError::MultiplicityUnavailable,
                ));
            }
            state.update(
                value.expect("non-count argument was checked"),
                aggregate,
                control,
            )?;
        }
        Accumulator::Extreme(current) => {
            let value = value.expect("non-count argument was checked");
            let replace = match *current {
                None => true,
                Some(old) => {
                    let order = value
                        .orderability_value()
                        .cmp_orderability_with_control(old.orderability_value(), control)?
                        .then_with(|| value.cmp(&old));
                    if function == GraphAggregateFunction::Min {
                        order.is_lt()
                    } else {
                        order.is_gt()
                    }
                }
            };
            if replace {
                *current = Some(value);
            }
        }
    }
    Ok(())
}
