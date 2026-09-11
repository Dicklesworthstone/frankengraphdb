//! Streaming grouped summaries over a compiled, unpaginated ALL pattern.
//!
//! The child is evaluated by the existing GLA binding visitor. No intermediate
//! bag of projected rows is materialized. Keys, extrema and distinct arguments
//! borrow the admitted source until final, owned aggregate rows are released.

mod result;
mod weighted;
pub use result::{
    GraphAggregateColumn, GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest,
    GraphNullPlacement, MAX_AGGREGATE_FILTERS,
};

use crate::algebra::{
    GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GlaOperator, GraphValue, GraphValueRow, MAX_PATTERN_NAME_BYTES,
    MAX_PATTERN_VERTICES, PreparedGraphPattern, ValueProjection, VertexPredicate,
};
use crate::{
    GlaExecutionEvent, GlaExecutionStats, GqlBudgetDimension, GqlExecutionStats, GqlQueryError,
    GqlQueryExecution, GqlQueryPolicy,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphAggregateFunction {
    CountRows,
    Count,
    CountDistinct,
    SumInt,
    Min,
    Max,
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
    #[must_use]
    pub const fn sum_int(name: &'a str, column: usize) -> Self {
        Self {
            name,
            function: GraphAggregateFunction::SumInt,
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
    EmptyAggregates,
    TooManyColumns { limit: usize, observed: usize },
    UnknownColumn { column: usize },
    DuplicateKey { column: usize },
    InvalidName,
    DuplicateName,
    UnknownOutputColumn { column: GraphAggregateColumn },
    TooManyFilters { limit: usize, observed: usize },
    DuplicateOrder { column: GraphAggregateColumn },
}

impl core::fmt::Display for GraphAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RequiresUnpaginatedAll => {
                f.write_str("aggregate input must preserve duplicates and have no pagination")
            }
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
        }
    }
}
impl core::error::Error for GraphAggregateBuildError {}

/// Preserve source errors and distinguish data/arithmetic refusals. Neither
/// error exposes an argument value. A refusal releases no aggregate rows.
#[derive(Debug, PartialEq, Eq)]
pub enum GraphAggregateError<E> {
    Source(E),
    NonIntegerSum { aggregate: usize },
    ArithmeticOverflow { aggregate: usize },
    NonIntegerHaving { predicate: usize },
    /// A physical weighted binding did not match its admitted topology.
    MultiplicityUnavailable,
}

impl<E> GraphAggregateError<E> {
    pub fn map_source<T>(self, map: impl FnOnce(E) -> T) -> GraphAggregateError<T> {
        match self {
            Self::Source(error) => GraphAggregateError::Source(map(error)),
            Self::NonIntegerSum { aggregate } => GraphAggregateError::NonIntegerSum { aggregate },
            Self::ArithmeticOverflow { aggregate } => {
                GraphAggregateError::ArithmeticOverflow { aggregate }
            }
            Self::NonIntegerHaving { predicate } => {
                GraphAggregateError::NonIntegerHaving { predicate }
            }
            Self::MultiplicityUnavailable => GraphAggregateError::MultiplicityUnavailable,
        }
    }
}
impl<E: core::fmt::Display> core::fmt::Display for GraphAggregateError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => core::fmt::Display::fmt(error, f),
            Self::NonIntegerSum { aggregate } => write!(
                f,
                "SUM_INT aggregate {aggregate} requires integer or null input"
            ),
            Self::ArithmeticOverflow { aggregate } => write!(
                f,
                "aggregate {aggregate} exceeded its exact integer result range"
            ),
            Self::NonIntegerHaving { predicate } => write!(
                f,
                "HAVING predicate {predicate} requires integer or null input"
            ),
            Self::MultiplicityUnavailable => {
                f.write_str("aggregate binding has no admitted topology multiplicity")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphAggregateError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            _ => None,
        }
    }
}

/// Counts and sums have explicit exact domains, not lossy canonical-i64 casts.
/// MIN/MAX preserve the original canonical scalar or vertex value. Empty
/// non-count aggregates produce Value(Scalar(Null)).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum GraphAggregateValue {
    Count(u64),
    Integer(i128),
    Value(GraphValue),
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

/// Logical GroupAggregate over an immutable ALL child. Pagination belongs to
/// the group output, never to the child. Execution consumes each complete child
/// binding through the one GLA visitor rather than materializing the child bag.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphAggregate {
    input: PreparedGraphPattern<GraphValueRow>,
    keys: Vec<usize>,
    aggregates: Vec<BoundAggregate>,
    key_names: Vec<String>,
    aggregate_names: Vec<String>,
    offset: u64,
    count: Option<u64>,
    having: Vec<GraphAggregateFilter>,
    ordering: Vec<GraphAggregateOrder>,
}
impl core::fmt::Debug for PreparedGraphAggregate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphAggregate")
            .field("key_columns", &self.keys.len())
            .field("aggregate_columns", &self.aggregates.len())
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
        if !input.preserves_duplicates()
            || !matches!(
                input.plan().operators().last(),
                Some(GlaOperator::Limit {
                    offset: 0,
                    count: None
                })
            )
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
        let columns = input.columns();
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
        let aggregates = aggregates
            .iter()
            .map(|aggregate| BoundAggregate {
                function: aggregate.function,
                column: aggregate.column,
            })
            .collect();
        Ok(Self {
            input,
            keys: keys.to_vec(),
            aggregates,
            key_names,
            aggregate_names,
            offset,
            count,
            having: Vec::new(),
            ordering: Vec::new(),
        })
    }

    /// Explicit child definition for source admission. This is not the summary
    /// result and must not be independently executed to implement aggregation.
    #[must_use]
    pub fn input_pattern(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.input
    }
    #[must_use]
    pub fn key_columns(&self) -> &[String] {
        &self.key_names
    }
    #[must_use]
    pub fn aggregate_columns(&self) -> &[String] {
        &self.aggregate_names
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
        bytes
    }

    /// Evaluate an already admitted source under one policy. Storage callers
    /// deduct source work/scratch first and add that consumption back to the
    /// returned counters. Count/null/sum semantics are applied per occurrence.
    /// Keyless aggregation yields one zero/null row on empty input; keyed
    /// aggregation yields no rows. Group ordering precedes output pagination.
    /// Positive topology-only summaries may factor parallel occurrences into
    /// checked weights. Property reads, predicates, scoped matches, and mixed
    /// directed/undirected use of one relation keep ordinary visitation. The
    /// physical optimization does not change the logical transcript or source
    /// record count; its preprocessing shares the evaluator's resource meter.
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
                    };
                    for _ in 0..values[at].payload_units() {
                        control(GlaExecutionEvent::Work)?;
                    }
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
                for (at, (aggregate, state)) in self.aggregates.iter().zip(state).enumerate() {
                    control(GlaExecutionEvent::Work)?;
                    let value = aggregate.column.map(|column| values[column]);
                    update(state, aggregate.function, value, multiplicity, at, control)?;
                }
                Ok(())
            },
        )?;
        let value = self.finish_groups(&groups, &mut control)?;
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

// Ordering is the same disjoint Scalar/Vertex ordering as GraphValue. Borrowed
// keys and distinct arguments remain tied to one immutable source execution.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ValueRef<'a> {
    Scalar(&'a CanonicalScalar),
    Vertex(VId),
}
impl ValueRef<'_> {
    fn is_null(self) -> bool {
        matches!(self, Self::Scalar(CanonicalScalar::Null))
    }
    fn payload_units(self) -> usize {
        let bytes = match self {
            Self::Scalar(CanonicalScalar::Bytes(value)) => value.as_slice().len(),
            Self::Scalar(CanonicalScalar::Text(value)) => {
                value.len() + value.canonical_sort_key().map_or(0, <[u8]>::len)
            }
            Self::Scalar(CanonicalScalar::Timestamp(value)) => {
                value.zone().map_or(0, |zone| zone.identifier().len())
            }
            _ => 0,
        };
        bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
    }
    fn copy_owned<E>(
        self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValue, E> {
        control(GlaExecutionEvent::ScratchEntry)?;
        for _ in 0..self.payload_units() {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        Ok(match self {
            Self::Scalar(value) => GraphValue::Scalar(value.clone()),
            Self::Vertex(value) => GraphValue::Vertex(value),
        })
    }
}

enum Accumulator<'a> {
    Count(u64),
    Distinct(BTreeSet<ValueRef<'a>>),
    Sum { value: i128, present: bool },
    Extreme(Option<ValueRef<'a>>),
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
            GraphAggregateFunction::Min | GraphAggregateFunction::Max => Accumulator::Extreme(None),
        });
    }
    Ok(state)
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
        Accumulator::Sum {
            value: sum,
            present,
        } => {
            // Weighted execution never admits SUM. Preserve its ordinary
            // checked per-occurrence semantics and fail closed on misrouting.
            if multiplicity != weighted::Multiplicity::ONE {
                return Err(GqlQueryError::Source(GraphAggregateError::MultiplicityUnavailable));
            }
            let Some(ValueRef::Scalar(CanonicalScalar::Int(value))) = value else {
                return Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
                    aggregate,
                }));
            };
            *sum = sum.checked_add(i128::from(*value)).ok_or_else(overflow)?;
            *present = true;
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
