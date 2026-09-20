//! Exact retractable grouped statistics over native maintained row bags.
//!
//! This adapter feeds the existing integer aggregate kernel; it never expands
//! multiplicities or executes a graph query. One selected Int64/NULL column
//! supplies COUNT, SUM, AVG, DISTINCT statistics and extrema. Group keys retain
//! canonical scalar/vertex domains. An empty key list means one global group,
//! including the zero-row case. Results retain ZWeight precision, not a narrowed
//! GQL scalar or an approximate floating-point average.

pub mod definition;

use crate::GraphSetColumnType;
use crate::algebra::{GraphValue, GraphValueRow, MAX_PATTERN_VERTICES};
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::zset::aggregate::{AggregateError, AggregateUpdate, AggregateValues, IncrementalAggregate};
use fgdb_delta_types::{LimbLimit, ZSet, ZSetError, ZSetEvent, ZWeight};
use fgdb_types::CanonicalScalar;
use std::sync::Arc;

type Key = Arc<[GraphValue]>;
static ZERO_COUNT: ZWeight = ZWeight::ZERO;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowAggregateBuildError {
    EmptyInput,
    TooManyColumns,
    UnsupportedColumn { column: usize },
    UnknownColumn { column: usize },
    DuplicateKey { column: usize },
    RequiresScalarArgument,
}
impl core::fmt::Display for RowAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "row aggregate definition: {self:?}")
    }
}
impl core::error::Error for RowAggregateBuildError {}

/// Positional definition over the COMPLETE upstream result, including any
/// upstream DISTINCT, filters, joins and pages. The value column must contain
/// Int64 or NULL at execution; scalar schema alone does not imply numeric data.
/// Grouping NULLs compare equal. Keys may include the selected value column.
#[derive(Clone, PartialEq, Eq)]
pub struct RowAggregateSpec {
    schema: Box<[GraphSetColumnType]>,
    keys: Box<[usize]>,
    value: usize,
}
impl RowAggregateSpec {
    pub fn new(schema: &[GraphSetColumnType], keys: &[usize], value: usize)
        -> Result<Self, RowAggregateBuildError> {
        if schema.is_empty() { return Err(RowAggregateBuildError::EmptyInput); }
        if schema.len() > MAX_PATTERN_VERTICES || keys.len() > MAX_PATTERN_VERTICES {
            return Err(RowAggregateBuildError::TooManyColumns);
        }
        for (column, kind) in schema.iter().enumerate() {
            if !matches!(kind, GraphSetColumnType::Scalar | GraphSetColumnType::Vertex) {
                return Err(RowAggregateBuildError::UnsupportedColumn { column });
            }
        }
        for (at, &column) in keys.iter().enumerate() {
            if column >= schema.len() { return Err(RowAggregateBuildError::UnknownColumn { column }); }
            if keys[..at].contains(&column) { return Err(RowAggregateBuildError::DuplicateKey { column }); }
        }
        match schema.get(value) {
            None => return Err(RowAggregateBuildError::UnknownColumn { column: value }),
            Some(GraphSetColumnType::Scalar) => {}
            Some(_) => return Err(RowAggregateBuildError::RequiresScalarArgument),
        }
        Ok(Self { schema: schema.into(), keys: keys.into(), value })
    }
    pub fn input_types(&self) -> &[GraphSetColumnType] { &self.schema }
    pub fn key_columns(&self) -> &[usize] { &self.keys }
    pub fn value_column(&self) -> usize { self.value }
    pub fn is_global(&self) -> bool { self.keys.is_empty() }
}
impl core::fmt::Debug for RowAggregateSpec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowAggregateSpec").field("input_columns", &self.schema.len())
            .field("group_columns", &self.keys.len()).finish_non_exhaustive()
    }
}

/// One complete exact group. A None summary represents ONLY the empty global
/// group. It has zero counts and NULL sum/average/extrema, not a synthetic row
/// in the aggregate's input. Arc clones never duplicate promoted integers.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RowAggregateRow {
    key: Key,
    summary: Option<Arc<AggregateValues>>,
}
impl RowAggregateRow {
    pub fn keys(&self) -> &[GraphValue] { &self.key }
    pub fn count_rows(&self) -> &ZWeight {
        self.summary.as_ref().map_or(&ZERO_COUNT, |s| s.count_rows())
    }
    pub fn count_values(&self) -> &ZWeight {
        self.summary.as_ref().map_or(&ZERO_COUNT, |s| s.count_values())
    }
    pub fn count_distinct(&self) -> &ZWeight {
        self.summary.as_ref().map_or(&ZERO_COUNT, |s| s.count_distinct())
    }
    pub fn sum(&self) -> Option<&ZWeight> { self.summary.as_ref().and_then(|s| s.sum()) }
    pub fn sum_distinct(&self) -> Option<&ZWeight> { self.summary.as_ref().and_then(|s| s.sum_distinct()) }
    pub fn minimum(&self) -> Option<i128> { self.summary.as_ref().and_then(|s| s.minimum()) }
    pub fn maximum(&self) -> Option<i128> { self.summary.as_ref().and_then(|s| s.maximum()) }
    /// Exact numerator and positive denominator; no floating rounding/coercion.
    pub fn average_parts(&self) -> Option<(&ZWeight, &ZWeight)> {
        self.summary.as_ref().and_then(|s| s.average_parts())
    }
    pub fn average_distinct_parts(&self) -> Option<(&ZWeight, &ZWeight)> {
        self.summary.as_ref().and_then(|s| s.average_distinct_parts())
    }
}
impl core::fmt::Debug for RowAggregateRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowAggregateRow").field("key_columns", &self.key.len())
            .field("values", &"[REDACTED]").finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowAggregateError<E> {
    Delta(ZSetError<E>),
    InputSchema,
    NonIntegerValue { column: usize },
    NegativeMultiplicity,
    ResultBudget { limit: u64 },
    InvalidResult,
}
impl<E> From<ZSetError<E>> for RowAggregateError<E> {
    fn from(error: ZSetError<E>) -> Self { Self::Delta(error) }
}
impl<E: core::fmt::Display> core::fmt::Display for RowAggregateError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::InputSchema => f.write_str("aggregate input has an incompatible row"),
            Self::NonIntegerValue { column } => write!(f, "aggregate column {column} requires Int64 or NULL"),
            Self::NegativeMultiplicity => f.write_str("negative integrated aggregate input multiplicity"),
            Self::ResultBudget { limit } => write!(f, "aggregate result groups exceed {limit}"),
            Self::InvalidResult => f.write_str("inconsistent aggregate result multiplicity"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for RowAggregateError<E> {}
fn charge<E>(control: &mut impl FnMut(ZSetEvent) -> Result<(), E>, event: ZSetEvent)
    -> Result<(), ZSetError<E>> { control(event).map_err(ZSetError::Control) }
fn reserve<E>(value: &GraphValue, control: &mut impl FnMut(ZSetEvent) -> Result<(), E>)
    -> Result<(), ZSetError<E>> {
    charge(control, ZSetEvent::Work)?;
    for _ in 0..=value.payload_units() { charge(control, ZSetEvent::ScratchEntry)?; }
    Ok(())
}

/// Exact in-memory grouped reduction with transactional preparation. Retain raw
/// compressed input counts so an invalid retraction cannot disappear through a
/// many-to-one projection. Only changed rows/groups and invalidated extrema are
/// visited on a tick. Logical work/payload units are not allocator-byte limits;
/// this is neither spill nor a durable registration/subscription implementation.
#[derive(PartialEq, Eq)]
pub struct IncrementalRowAggregate {
    spec: RowAggregateSpec,
    input: ZSet<GraphValueRow>,
    aggregate: IncrementalAggregate<Key>,
    rows: ZSet<RowAggregateRow>,
}
impl IncrementalRowAggregate {
    pub fn new(spec: RowAggregateSpec) -> Self {
        Self { spec, input: ZSet::new(), aggregate: IncrementalAggregate::new(), rows: ZSet::new() }
    }
    pub fn spec(&self) -> &RowAggregateSpec { &self.spec }
    /// The first accepted prepare (even empty) installs the global zero group.
    pub fn rows(&self) -> &ZSet<RowAggregateRow> { &self.rows }
    pub fn prepare<E>(&mut self, delta: &ZSet<GraphValueRow>, limbs: LimbLimit,
        max_result_rows: Option<u64>, control: &mut impl FnMut(ZSetEvent) -> Result<(), E>)
        -> Result<RowAggregateUpdate<'_>, RowAggregateError<E>> {
        charge(control, ZSetEvent::Work)?;
        let mut contributions = Vec::new();
        for (row, change) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            if row.len() != self.spec.schema.len() { return Err(RowAggregateError::InputSchema); }
            for (value, kind) in row.values().iter().zip(self.spec.schema.iter()) {
                charge(control, ZSetEvent::Work)?;
                if !kind.accepts(value) { return Err(RowAggregateError::InputSchema); }
                // Reserve the raw input's retained/staged payload copy.
                reserve(value, control)?;
            }
            let next = match self.input.weight(row) {
                Some(old) => old.checked_add(change, limbs),
                None => change.checked_clone(limbs),
            }.map_err(ZSetError::Arithmetic)?;
            if next < ZWeight::ZERO { return Err(RowAggregateError::NegativeMultiplicity); }
            let value = match &row.values()[self.spec.value] {
                GraphValue::Scalar(CanonicalScalar::Null) => None,
                GraphValue::Scalar(CanonicalScalar::Int(value)) => Some(i128::from(*value)),
                _ => return Err(RowAggregateError::NonIntegerValue { column: self.spec.value }),
            };
            charge(control, ZSetEvent::ScratchEntry)?;
            let mut key = Vec::with_capacity(self.spec.keys.len());
            for &column in &self.spec.keys {
                let value = &row.values()[column];
                reserve(value, control)?;
                key.push(value.clone());
            }
            charge(control, ZSetEvent::ScratchEntry)?;
            let key: Key = key.into();
            charge(control, ZSetEvent::Work)?;
            let weight = change.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
            charge(control, ZSetEvent::ScratchEntry)?;
            contributions.push(((key, value), weight));
        }
        let contributions = ZSet::from_updates(contributions, limbs, control)?;
        let global = self.spec.is_global();
        let old_empty = self.aggregate.group_count() == 0;
        let had_output = !self.rows.is_empty();
        let input = self.input.prepare_update(delta, limbs, control)?;
        let aggregate = self.aggregate.prepare(&contributions, limbs, control).map_err(|error| match error {
            AggregateError::ZSet(error) => RowAggregateError::Delta(error),
            AggregateError::NegativeMultiplicity => RowAggregateError::NegativeMultiplicity,
        })?;
        let mut changes = Vec::new();
        for ((key, summary), weight) in aggregate.delta().iter() {
            charge(control, ZSetEvent::Work)?;
            charge(control, ZSetEvent::ScratchEntry)?;
            changes.push((RowAggregateRow { key: Arc::clone(key), summary: Some(Arc::clone(summary)) },
                weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?));
        }
        if global {
            charge(control, ZSetEvent::ScratchEntry)?;
            let key: Key = Arc::from(Vec::<GraphValue>::new());
            let new_empty = aggregate.get(&key).is_none();
            if had_output && old_empty && !new_empty {
                charge(control, ZSetEvent::ScratchEntry)?;
                changes.push((RowAggregateRow { key: Arc::clone(&key), summary: None }, ZWeight::from_i128(-1)));
            }
            if new_empty && (!had_output || !old_empty) {
                charge(control, ZSetEvent::ScratchEntry)?;
                changes.push((RowAggregateRow { key, summary: None }, ZWeight::ONE));
            }
        }
        let changes = ZSet::from_updates(changes, limbs, control)?;
        let change = changes.total_weight(limbs, control)?;
        charge(control, ZSetEvent::Work)?;
        let size = i128::try_from(self.rows.len()).map_err(|_| RowAggregateError::InvalidResult)?;
        let count = ZWeight::from_i128(size).checked_add(&change, limbs).map_err(ZSetError::Arithmetic)?;
        if count < ZWeight::ZERO { return Err(RowAggregateError::InvalidResult); }
        if let Some(limit) = max_result_rows {
            if count > ZWeight::from_i128(i128::from(limit)) { return Err(RowAggregateError::ResultBudget { limit }); }
        }
        let output = self.rows.prepare_update(&changes, limbs, control)?;
        for (row, _) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            if output.weight(row).is_some_and(|weight| weight != &ZWeight::ONE) {
                return Err(RowAggregateError::InvalidResult);
            }
        }
        charge(control, ZSetEvent::Work)?;
        Ok(RowAggregateUpdate { input, aggregate, output, delta: changes })
    }
}
impl core::fmt::Debug for IncrementalRowAggregate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalRowAggregate").field("spec", &self.spec)
            .field("groups", &self.rows.len()).field("data", &"[REDACTED]").finish()
    }
}
#[must_use = "dropping a row aggregate update preserves input, support and result"]
pub struct RowAggregateUpdate<'a> {
    input: ZSetUpdate<'a, GraphValueRow>,
    aggregate: AggregateUpdate<'a, Key>,
    output: ZSetUpdate<'a, RowAggregateRow>,
    delta: ZSet<RowAggregateRow>,
}
impl RowAggregateUpdate<'_> {
    pub fn delta(&self) -> &ZSet<RowAggregateRow> { &self.delta }
    /// No recoverable arithmetic, callback or interruption between publications.
    pub fn commit(self) -> ZSet<RowAggregateRow> {
        let Self { input, aggregate, output, delta } = self;
        input.commit();
        let _ = aggregate.commit();
        output.commit();
        delta
    }
}
impl core::fmt::Debug for RowAggregateUpdate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowAggregateUpdate").field("changed_rows", &self.delta.len()).finish_non_exhaustive()
    }
}
#[cfg(test)]
mod tests;
