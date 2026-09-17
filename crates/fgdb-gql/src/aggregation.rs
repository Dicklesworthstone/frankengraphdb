//! Grouped summaries over a compiled pattern or single-source row pipeline.
//!
//! Ordinary inputs stream through the existing GLA binding visitor without an
//! intermediate bag. Computed inputs and relational pipelines use a bounded
//! materialized path and the same accumulators/result engine. Keys, extrema and
//! distinct arguments borrow the admitted input until owned rows are released.

mod computed;
mod numeric;
mod relational;
mod result;
mod weighted;
pub use numeric::GraphExactAverage;
pub use result::{
    GraphAggregateColumn, GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest,
    GraphHavingError, GraphHavingExpression, GraphHavingOp, GraphHavingOperand, GraphNullPlacement,
    MAX_AGGREGATE_FILTERS, MAX_HAVING_INSTRUCTIONS,
};

use crate::algebra::{
    GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GlaOperator, GraphPath, GraphValue, GraphValueRow,
    MAX_PATTERN_NAME_BYTES, MAX_PATTERN_VERTICES, PreparedGraphPattern, ValueProjection,
    VertexPredicate,
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
    #[must_use]
    pub const fn min(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::Min,
            column: Some(column),
        }
    }
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
    InputProjection(crate::GraphSetProjectionError),
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
            Self::InputProjection(error) => error.fmt(f),
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
    ResultCountOverflow,
    /// A physical weighted binding did not match its admitted topology.
    MultiplicityUnavailable,
    /// A physical path violated its ascending, contiguous root-group contract.
    NonMonotonicGroups,
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
            Self::ResultCountOverflow => GraphAggregateError::ResultCountOverflow,
            Self::MultiplicityUnavailable => GraphAggregateError::MultiplicityUnavailable,
            Self::NonMonotonicGroups => GraphAggregateError::NonMonotonicGroups,
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
                "SUM_INT aggregate {aggregate} requires integer or null input"
            ),
            Self::NonIntegerAverage { aggregate } => write!(
                f,
                "AVG_INT aggregate {aggregate} requires integer or null input"
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
            Self::ResultCountOverflow => f.write_str("aggregate result count overflow"),
            Self::MultiplicityUnavailable => {
                f.write_str("aggregate binding has no admitted topology multiplicity")
            }
            Self::NonMonotonicGroups => {
                f.write_str("aggregate input violated its root-group ordering contract")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphAggregateError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::InputRelation(error) => Some(error),
            Self::InputExpression { error, .. } => Some(error),
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
struct BoundAggregate {
    function: GraphAggregateFunction,
    column: Option<usize>,
}

/// Only a nonidentity output projection allocates this metadata. Evaluation
/// continues to use the original key sequence, including its tie-break order.
#[derive(Clone, PartialEq, Eq)]
struct KeyProjection {
    columns: Box<[usize]>,
    names: Box<[String]>,
}

/// Logical GroupAggregate over an immutable child. Ordinary ALL patterns stream
/// directly; computed input and explicit single-source relational pipelines own
/// bounded rows before grouping. A pipeline's existing pages remain input
/// boundaries; offset/count here apply separately to the group output.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphAggregate {
    input: PreparedGraphPattern<GraphValueRow>,
    computed_input: Option<Vec<crate::GraphSetProjection>>,
    relational_input: Option<crate::PreparedGraphSet>,
    keys: Vec<usize>,
    aggregates: Vec<BoundAggregate>,
    key_names: Vec<String>,
    key_output: Option<KeyProjection>,
    aggregate_names: Vec<String>,
    output_aggregates: usize,
    output_distinct: bool,
    offset: u64,
    count: Option<u64>,
    having: Vec<GraphAggregateFilter>,
    having_expression: Option<GraphHavingExpression>,
    ordering: Vec<GraphAggregateOrder>,
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
        if aggregates.is_empty() {
            return Err(GraphAggregateBuildError::EmptyAggregates);
        }
        let width = keys.len().saturating_add(aggregates.len());
        if width > MAX_PATTERN_VERTICES {
            return Err(GraphAggregateBuildError::TooManyColumns {
                limit: MAX_PATTERN_VERTICES,
                observed: width,
            });
        }
        let projected_columns = computed_input
            .as_deref()
            .map(|projection| computed::projected_schema(&input, projection))
            .transpose()?;
        let columns = relational_input
            .as_ref()
            .map(crate::PreparedGraphSet::columns)
            .or(projected_columns.as_deref())
            .unwrap_or(input.columns());
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
            let name = aggregate.name.as_bytes();
            if name.is_empty()
                || name.len() > MAX_PATTERN_NAME_BYTES
                || !(name[0].is_ascii_alphabetic() || name[0] == b'_')
                || !name
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
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
            input,
            computed_input,
            relational_input,
            keys: keys.to_vec(),
            aggregates,
            key_names,
            key_output: None,
            aggregate_names,
            output_aggregates,
            output_distinct: false,
            offset,
            count,
            having: Vec::new(),
            having_expression: None,
            ordering: Vec::new(),
        })
    }

    /// The actual sole graph source for storage admission, not the transformed
    /// relation or group output. execute_governed owns every row stage, including
    /// any local page or DISTINCT, before invoking the shared group engine.
    #[must_use]
    pub fn input_pattern(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.input
    }
    #[must_use]
    pub fn key_columns(&self) -> &[String] {
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
        &self.aggregate_names[..self.output_aggregates]
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
                GraphAggregateFunction::Min => 4,
                GraphAggregateFunction::Max => 5,
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
        self.append_input_projection(&mut bytes);
        if let Some(input) = &self.relational_input {
            bytes.extend_from_slice(b"fgdb:aggregate-relational-input:v1\0");
            let relation = input.canonical_bytes();
            bytes.extend_from_slice(&(relation.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&relation);
        }
        bytes
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
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        if self.input.plan().requires_identified_edges() {
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
                checkpoint,
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
                checkpoint,
            );
        }
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        policy
            .rows
            .check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(GqlQueryError::Rows)?;
        let mut rows = GqlExecutionStats {
            snapshot_records,
            result_rows: 0,
        };
        let mut evaluator = GlaExecutionStats::default();
        let mut control = |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            if event == GlaExecutionEvent::ResultRow {
                let next = rows.result_rows + 1;
                policy
                    .rows
                    .check(GqlBudgetDimension::ResultRows, next)
                    .map_err(GqlQueryError::Rows)?;
                rows.result_rows = next;
            }
            evaluator
                .charge_event(policy.evaluator, event)
                .map_err(GqlQueryError::Evaluator)
        };
        let mut streaming = result::RootGroups::new(self);
        let mut groups: BTreeMap<Vec<ValueRef<'a>>, Vec<Accumulator<'a>>> = BTreeMap::new();
        if self.keys.is_empty() {
            control(GlaExecutionEvent::ScratchEntry)?;
            groups.insert(Vec::new(), new_group(&self.aggregates, &mut control)?);
        }
        weighted::visit_bindings(
            self,
            vertices,
            edges,
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
                let mut values = [ValueRef::Scalar(&NULL); MAX_PATTERN_VERTICES];
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
                        ValueProjection::Path { .. } => {
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
                let mut key = [ValueRef::Scalar(&NULL); MAX_PATTERN_VERTICES];
                for (at, column) in self.keys.iter().enumerate() {
                    control(GlaExecutionEvent::Work)?;
                    key[at] = values[*column];
                }
                let key = &key[..self.keys.len()];
                control(GlaExecutionEvent::Work)?;
                if !groups.contains_key(key) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    let mut owned_key = Vec::new();
                    for value in key {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        owned_key.push(*value);
                    }
                    let state = new_group(&self.aggregates, control)?;
                    groups.insert(owned_key, state);
                }
                let state = groups
                    .get_mut(key)
                    .expect("the admitted group was initialized");
                update_group(&self.aggregates, state, &values, multiplicity, control)
            },
        )?;
        let value = match streaming {
            Some(streaming) => streaming.finish(&mut control)?,
            None => self.finish_groups(&groups, &mut control)?,
        };
        // Even empty and zero-count outputs observe a terminal checkpoint.
        control(GlaExecutionEvent::Work)?;
        Ok(GqlQueryExecution {
            value,
            rows,
            evaluator,
        })
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
}
impl ValueRef<'_> {
    fn is_null(self) -> bool {
        matches!(self, Self::Scalar(CanonicalScalar::Null))
    }
    fn payload_units(self) -> usize {
        if let Self::List(values) = self {
            return values.iter().fold(values.len(), |units, value| {
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
            Self::List(_) => unreachable!("list copying is recursively governed above"),
        })
    }
}

enum Accumulator<'a> {
    Count(u64),
    Distinct(BTreeSet<ValueRef<'a>>),
    Sum { value: i128, present: bool },
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
        Accumulator::Collect { values, seen, max_payload } => {
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
                for _ in 0..levels.saturating_mul(24)
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
            let Some(ValueRef::Scalar(CanonicalScalar::Int(value))) = value else {
                return Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
                    aggregate,
                }));
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
            if current.is_none_or(|old| {
                if function == GraphAggregateFunction::Min {
                    value < old
                } else {
                    value > old
                }
            }) {
                *current = Some(value);
            }
        }
    }
    Ok(())
}
