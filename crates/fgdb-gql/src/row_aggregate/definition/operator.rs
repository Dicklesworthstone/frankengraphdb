//! Native complete groups over exact row changes, using the shared Z-set
//! aggregate kernel. This adapter owns no graph source or expression evaluator.

mod collection;

use super::GroupDefinition;
use crate::algebra::{GraphValue, GraphValueRow, MAX_PATTERN_VERTICES};
use crate::{
    GlaExecutionEvent, GqlQueryError, GraphAggregateFunction as Function, GraphAggregateRow,
    GraphAggregateValue as Value, GraphExactAverage, GraphSetColumnType,
};
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::zset::aggregate::{
    AggregateError, AggregateUpdate, AggregateValues, IncrementalAggregate,
};
use fgdb_delta_types::{LimbLimit, ZSet, ZSetError, ZSetEvent, ZWeight};
use fgdb_types::CanonicalScalar;
use std::collections::BTreeSet;
use std::ops::Bound;
use std::sync::Arc;

type Group = Arc<[GraphValue]>;
type Key = (Group, usize, Option<Arc<GraphValue>>);
type Contributions = ZSet<(Key, Option<i128>)>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupBuildError {
    InputWidth,
    InputSchema { column: usize },
    UnsupportedDefinition,
    UnsupportedAggregate { aggregate: usize },
}
impl core::fmt::Display for GroupBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "native group definition: {self:?}")
    }
}
impl core::error::Error for GroupBuildError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupError<E> {
    Delta(ZSetError<E>),
    InputSchema,
    NegativeMultiplicity,
    NonInteger { column: usize },
    Arithmetic,
    NonIntegerHaving,
    InvalidResult,
    ResultBudget { limit: u64 },
}
impl<E> From<ZSetError<E>> for GroupError<E> {
    fn from(value: ZSetError<E>) -> Self {
        Self::Delta(value)
    }
}
impl<E: core::fmt::Display> core::fmt::Display for GroupError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(e) => e.fmt(f),
            Self::InputSchema => f.write_str("invalid native aggregate input row"),
            Self::NegativeMultiplicity => f.write_str("negative native aggregate input count"),
            Self::NonInteger { column } => {
                write!(f, "aggregate input {column} requires Int64 or NULL")
            }
            Self::Arithmetic => {
                f.write_str("native aggregate result is outside its bounded exact domain")
            }
            Self::NonIntegerHaving => f.write_str("incompatible numeric HAVING operands"),
            Self::InvalidResult => f.write_str("invalid native aggregate result"),
            Self::ResultBudget { limit } => {
                write!(f, "native aggregate group limit {limit} exceeded")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GroupError<E> {}
fn charge<E>(
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    event: ZSetEvent,
) -> Result<(), GroupError<E>> {
    control(event).map_err(|e| GroupError::Delta(ZSetError::Control(e)))
}
fn reserve<E>(
    value: &GraphValue,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), GroupError<E>> {
    charge(control, ZSetEvent::Work)?;
    for _ in 0..=value.payload_units() {
        charge(control, ZSetEvent::ScratchEntry)?;
    }
    Ok(())
}
fn primary(group: &Group, index: usize) -> Key {
    (Arc::clone(group), index, None)
}
fn support(function: Function) -> bool {
    matches!(
        function,
        Function::CountDistinct | Function::Min | Function::Max
    )
}
fn bounds(group: &Group, index: usize) -> (Bound<Key>, Bound<Key>) {
    // Aggregate indexes were bounded by MAX_PATTERN_VERTICES at construction.
    (
        Bound::Excluded(primary(group, index)),
        Bound::Excluded(primary(group, index + 1)),
    )
}
fn first<'a, E>(
    keys: impl Iterator<Item = &'a Key>,
    present: impl Fn(&Key) -> bool,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Option<Arc<GraphValue>>, GroupError<E>> {
    for key in keys {
        charge(control, ZSetEvent::Work)?;
        if present(key) {
            return Ok(key.2.as_ref().map(Arc::clone));
        }
    }
    Ok(None)
}
fn current_extremum<E>(
    state: &IncrementalAggregate<Key>,
    group: &Group,
    index: usize,
    maximum: bool,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Option<Arc<GraphValue>>, GroupError<E>> {
    let keys = state.range(bounds(group, index)).map(|(k, _)| k);
    if maximum {
        first(keys.rev(), |_| true, control)
    } else {
        first(keys, |_| true, control)
    }
}
fn pending_extremum<E>(
    state: &AggregateUpdate<'_, Key>,
    group: &Group,
    index: usize,
    maximum: bool,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Option<Arc<GraphValue>>, GroupError<E>> {
    let retained = state.retained_range(bounds(group, index)).map(|(k, _)| k);
    let a = if maximum {
        first(retained.rev(), |k| state.get(k).is_some(), control)?
    } else {
        first(retained, |k| state.get(k).is_some(), control)?
    };
    let changed = state.changed_range(bounds(group, index)).map(|(k, _)| k);
    let b = if maximum {
        first(changed.rev(), |k| state.get(k).is_some(), control)?
    } else {
        first(changed, |k| state.get(k).is_some(), control)?
    };
    Ok(match (a, b) {
        (Some(a), Some(b)) => {
            for _ in 0..=a.payload_units().max(b.payload_units()) {
                charge(control, ZSetEvent::Work)?;
            }
            Some(if maximum { a.max(b) } else { a.min(b) })
        }
        (a, b) => a.or(b),
    })
}
fn count<E>(n: &ZWeight) -> Result<u64, GroupError<E>> {
    n.to_i128()
        .and_then(|n| u64::try_from(n).ok())
        .ok_or(GroupError::Arithmetic)
}
fn render<'a, D: GroupDefinition, E>(
    definition: &D,
    group: &Group,
    mut get: impl FnMut(usize) -> Option<&'a AggregateValues>,
    mut extreme: impl FnMut(
        usize,
        bool,
        &mut dyn FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Option<Arc<GraphValue>>, GroupError<E>>,
    mut collect: impl FnMut(
        usize,
        bool,
        &mut dyn FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<GraphValue, GroupError<E>>,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Option<GraphAggregateRow>, GroupError<E>> {
    let mut keys = Vec::new();
    charge(control, ZSetEvent::ScratchEntry)?;
    for key in group.iter() {
        reserve(key, control)?;
        keys.push(key.clone());
    }
    let mut values = Vec::new();
    for (index, (function, column)) in definition.aggregate_specs().enumerate() {
        charge(control, ZSetEvent::Work)?;
        charge(control, ZSetEvent::ScratchEntry)?;
        let state = get(index);
        let null = Value::Value(GraphValue::Scalar(CanonicalScalar::Null));
        let value = match function {
            Function::CountRows => Value::Count(state.map_or(Ok(0), |s| count(s.count_rows()))?),
            Function::Count | Function::CountDistinct => {
                Value::Count(state.map_or(Ok(0), |s| count(s.count_values()))?)
            }
            Function::SumInt | Function::SumIntDistinct => match state.and_then(|s| {
                if function == Function::SumInt {
                    s.sum()
                } else {
                    s.sum_distinct()
                }
            }) {
                None => null,
                Some(sum) => Value::Integer(sum.to_i128().ok_or(GroupError::Arithmetic)?),
            },
            Function::AverageInt | Function::AverageIntDistinct => match state.and_then(|s| {
                if function == Function::AverageInt {
                    s.average_parts()
                } else {
                    s.average_distinct_parts()
                }
            }) {
                None => null,
                Some((sum, n)) => {
                    let sum = sum.to_i128().ok_or(GroupError::Arithmetic)?;
                    let n = count(n)?;
                    for _ in 0..128 {
                        charge(control, ZSetEvent::Work)?;
                    }
                    Value::Average(GraphExactAverage::new(sum, n).ok_or(GroupError::InvalidResult)?)
                }
            },
            Function::Min | Function::Max => {
                match extreme(index, function == Function::Max, control)? {
                    None => null,
                    Some(value) => {
                        reserve(&value, control)?;
                        Value::Value(value.as_ref().clone())
                    }
                }
            }
            Function::Collect | Function::CollectDistinct => Value::Value(collect(
                column.ok_or(GroupError::InvalidResult)?,
                function == Function::CollectDistinct,
                control,
            )?),
        };
        values.push(value);
    }
    let row = definition
        .materialize_incremental_row(keys, values)
        .ok_or(GroupError::InvalidResult)?;
    let keep = definition
        .evaluate_incremental_having(&row, &mut |event| {
            control(match event {
                GlaExecutionEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => ZSetEvent::Work,
            })
        })
        .map_err(|error| match error {
            GqlQueryError::Interrupted(e) => GroupError::Delta(ZSetError::Control(e)),
            _ => GroupError::NonIntegerHaving,
        })?
        .ok_or(GroupError::InvalidResult)?;
    Ok(keep.then_some(row))
}

/// Exact complete groups after HAVING, before output projection/ranking.
/// The immutable native definition retains the full relational source contract.
/// Raw counts, typed argument support, summaries and visible groups publish as
/// one prepared update. Only changed tuples/groups and invalidated extrema are
/// visited. COLLECT and COLLECT DISTINCT additionally retain one shared ordered
/// tuple arrangement, and rerender only affected groups. Only emitted collection
/// elements expand multiplicities; DISTINCT never expands duplicate occurrences.
/// Input order must be proved by the complete relational definition. Unknown
/// positional order and ordinary graph visitation refuse rather than invent an
/// ordering. Result lists obey GraphValue depth/node bounds; exceeding them is
/// an Arithmetic refusal, even beneath HAVING or a later output LIMIT 0.
/// Logical payload/event quotas are not allocator-byte or spill bounds.
/// Unsupported output transforms refuse.
/// Completed relational rows may contain any bounded native value: keys,
/// COUNT/DISTINCT and extrema retain canonical typed equality/order. Numeric
/// reducers admit Scalar or Any and check every changed operand for Int64/NULL.
#[derive(PartialEq, Eq)]
pub struct IncrementalGroupAggregate<D: GroupDefinition> {
    definition: D,
    schema: Box<[GraphSetColumnType]>,
    input: ZSet<GraphValueRow>,
    aggregate: IncrementalAggregate<Key>,
    collections: Option<collection::State>,
    rows: ZSet<GraphAggregateRow>,
    initialized: bool,
}
impl<D: GroupDefinition> IncrementalGroupAggregate<D> {
    pub fn new(definition: D, schema: &[GraphSetColumnType]) -> Result<Self, GroupBuildError> {
        if schema.len() > MAX_PATTERN_VERTICES {
            return Err(GroupBuildError::InputWidth);
        }
        if !definition.supports_incremental_maintenance_with_having() {
            return Err(GroupBuildError::UnsupportedDefinition);
        }
        for (column, kind) in schema.iter().enumerate() {
            if definition.incremental_input_column_type(column) != Some(*kind) {
                return Err(GroupBuildError::InputSchema { column });
            }
        }
        if definition
            .incremental_input_column_type(schema.len())
            .is_some()
        {
            return Err(GroupBuildError::InputSchema {
                column: schema.len(),
            });
        }
        for (aggregate, (function, column)) in definition.aggregate_specs().enumerate() {
            let valid = match function {
                Function::CountRows => column.is_none(),
                Function::Count | Function::CountDistinct | Function::Min | Function::Max => {
                    column.is_some()
                }
                Function::SumInt
                | Function::SumIntDistinct
                | Function::AverageInt
                | Function::AverageIntDistinct => column.is_some_and(|column| {
                    matches!(
                        schema.get(column),
                        Some(GraphSetColumnType::Scalar | GraphSetColumnType::Any)
                    )
                }),
                Function::Collect | Function::CollectDistinct => {
                    column.is_some() && definition.incremental_collection_order().is_some()
                }
            };
            if !valid {
                return Err(GroupBuildError::UnsupportedAggregate { aggregate });
            }
        }
        let collections = if definition
            .aggregate_specs()
            .any(|(function, _)| matches!(function, Function::Collect | Function::CollectDistinct))
        {
            let order = definition
                .incremental_collection_order()
                .ok_or(GroupBuildError::UnsupportedDefinition)?;
            Some(collection::State::new(order))
        } else {
            None
        };
        Ok(Self {
            definition,
            schema: schema.into(),
            input: ZSet::new(),
            aggregate: IncrementalAggregate::new(),
            collections,
            rows: ZSet::new(),
            initialized: false,
        })
    }
    pub fn definition(&self) -> &D {
        &self.definition
    }
    pub fn input_types(&self) -> &[GraphSetColumnType] {
        &self.schema
    }
    pub fn rows(&self) -> &ZSet<GraphAggregateRow> {
        &self.rows
    }

    pub fn prepare<E>(
        &mut self,
        changes: &ZSet<GraphValueRow>,
        limbs: LimbLimit,
        max_groups: Option<u64>,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<GroupUpdate<'_>, GroupError<E>> {
        charge(control, ZSetEvent::Work)?;
        // Validate individual raw counts before their aggregate images can cancel.
        for (row, change) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            if row.len() != self.schema.len() {
                return Err(GroupError::InputSchema);
            }
            for (value, kind) in row.values().iter().zip(self.schema.iter()) {
                charge(control, ZSetEvent::Work)?;
                if !kind.accepts(value) || !value.validate_bounds() {
                    return Err(GroupError::InputSchema);
                }
                reserve(value, control)?;
            }
            let next = match self.input.weight(row) {
                Some(old) => old.checked_add(change, limbs),
                None => change.checked_clone(limbs),
            }
            .map_err(ZSetError::Arithmetic)?;
            if next < ZWeight::ZERO {
                return Err(GroupError::NegativeMultiplicity);
            }
        }
        let mut contributions = Vec::new();
        for (row, weight) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            charge(control, ZSetEvent::ScratchEntry)?;
            let mut key = Vec::new();
            for &column in self.definition.group_key_columns() {
                let value = &row.values()[column];
                reserve(value, control)?;
                key.push(value.clone());
            }
            let group: Group = key.into();
            for (index, (function, column)) in self.definition.aggregate_specs().enumerate() {
                charge(control, ZSetEvent::Work)?;
                let value = column.map(|column| &row.values()[column]);
                if support(function) {
                    charge(control, ZSetEvent::ScratchEntry)?;
                    contributions.push((
                        (primary(&group, index), None),
                        weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?,
                    ));
                    if let Some(value) = value.filter(|value| !value.is_null()) {
                        reserve(value, control)?;
                        charge(control, ZSetEvent::ScratchEntry)?;
                        contributions.push((
                            (
                                (Arc::clone(&group), index, Some(Arc::new(value.clone()))),
                                Some(0),
                            ),
                            weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?,
                        ));
                    }
                } else {
                    let value = match value {
                        None => Some(0),
                        Some(v) if v.is_null() => None,
                        Some(_)
                            if matches!(
                                function,
                                Function::Count | Function::Collect | Function::CollectDistinct
                            ) =>
                        {
                            Some(0)
                        }
                        Some(GraphValue::Scalar(CanonicalScalar::Int(n))) => Some(i128::from(*n)),
                        _ => {
                            return Err(GroupError::NonInteger {
                                column: column.ok_or(GroupError::InvalidResult)?,
                            });
                        }
                    };
                    charge(control, ZSetEvent::ScratchEntry)?;
                    contributions.push((
                        (primary(&group, index), value),
                        weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?,
                    ));
                }
            }
        }
        let mut delta: Contributions = ZSet::from_updates(contributions, limbs, control)?;
        let mut crossings = Vec::new();
        for ((key, _), change) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            if key.2.is_none() {
                continue;
            }
            let old = self.aggregate.get(key).map(AggregateValues::count_rows);
            let next = match old {
                Some(old) => old.checked_add(change, limbs),
                None => change.checked_clone(limbs),
            }
            .map_err(ZSetError::Arithmetic)?;
            if next < ZWeight::ZERO {
                return Err(GroupError::NegativeMultiplicity);
            }
            let was = old.is_some_and(|n| !n.is_zero());
            let now = !next.is_zero();
            if was != now {
                charge(control, ZSetEvent::ScratchEntry)?;
                crossings.push((
                    (primary(&key.0, key.1), Some(0)),
                    ZWeight::from_i128(if now { 1 } else { -1 }),
                ));
            }
        }
        let crossings = ZSet::from_updates(crossings, limbs, control)?;
        delta.integrate(&crossings, limbs, control)?;
        let collections = self
            .collections
            .as_mut()
            .map(|state| {
                state.prepare(changes, self.definition.group_key_columns(), limbs, control)
            })
            .transpose()?;
        let mut groups = BTreeSet::new();
        // Argument replacements and sort-key changes can leave COUNT exactly
        // unchanged. The collection index, not count crossings, invalidates
        // those groups, including a new first representative for DISTINCT.
        if let Some(collections) = &collections {
            for group in collections.changed_groups() {
                charge(control, ZSetEvent::Work)?;
                charge(control, ZSetEvent::ScratchEntry)?;
                groups.insert(Arc::clone(group));
            }
        }
        for (((group, _, _), _), _) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            if !groups.contains(group) {
                charge(control, ZSetEvent::ScratchEntry)?;
                groups.insert(Arc::clone(group));
            }
        }
        let global = self.definition.group_key_columns().is_empty();
        if global && !self.initialized {
            charge(control, ZSetEvent::ScratchEntry)?;
            groups.insert(Arc::from(Vec::<GraphValue>::new()));
        }
        let mut before = Vec::new();
        for group in groups {
            charge(control, ZSetEvent::Work)?;
            let old = if self.initialized
                && (global || self.aggregate.get(&primary(&group, 0)).is_some())
            {
                render(
                    &self.definition,
                    &group,
                    |index| self.aggregate.get(&primary(&group, index)),
                    |index, maximum, c| {
                        current_extremum(&self.aggregate, &group, index, maximum, &mut |e| c(e))
                    },
                    |column, distinct, c| {
                        collections
                            .as_ref()
                            .ok_or(GroupError::InvalidResult)?
                            .render(&group, column, distinct, false, &mut |event| c(event))
                    },
                    control,
                )?
            } else {
                None
            };
            charge(control, ZSetEvent::ScratchEntry)?;
            before.push((group, old));
        }
        let input = self.input.prepare_update(changes, limbs, control)?;
        let aggregate = self
            .aggregate
            .prepare(&delta, limbs, control)
            .map_err(|e| match e {
                AggregateError::ZSet(e) => GroupError::Delta(e),
                AggregateError::NegativeMultiplicity => GroupError::NegativeMultiplicity,
            })?;
        let mut updates = Vec::new();
        let mut size = self.rows.len() as u128;
        for (group, old) in before {
            charge(control, ZSetEvent::Work)?;
            let new = if global || aggregate.get(&primary(&group, 0)).is_some() {
                render(
                    &self.definition,
                    &group,
                    |index| aggregate.get(&primary(&group, index)),
                    |index, maximum, c| {
                        pending_extremum(&aggregate, &group, index, maximum, &mut |e| c(e))
                    },
                    |column, distinct, c| {
                        collections
                            .as_ref()
                            .ok_or(GroupError::InvalidResult)?
                            .render(&group, column, distinct, true, &mut |event| c(event))
                    },
                    control,
                )?
            } else {
                None
            };
            size = size
                .checked_sub(u128::from(old.is_some()))
                .and_then(|n| n.checked_add(u128::from(new.is_some())))
                .ok_or(GroupError::InvalidResult)?;
            if old != new {
                if let Some(row) = old {
                    charge(control, ZSetEvent::ScratchEntry)?;
                    updates.push((row, ZWeight::from_i128(-1)));
                }
                if let Some(row) = new {
                    charge(control, ZSetEvent::ScratchEntry)?;
                    updates.push((row, ZWeight::ONE));
                }
            }
        }
        if let Some(limit) = max_groups {
            if size > u128::from(limit) {
                return Err(GroupError::ResultBudget { limit });
            }
        }
        let delta = ZSet::from_updates(updates, limbs, control)?;
        for (row, _) in delta.iter() {
            for key in row.keys() {
                reserve(key, control)?;
            }
            for value in row.values() {
                if let Value::Value(v) = value {
                    reserve(v, control)?;
                }
            }
        }
        let output = self.rows.prepare_update(&delta, limbs, control)?;
        for (row, _) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            if output.weight(row).is_some_and(|n| n != &ZWeight::ONE) {
                return Err(GroupError::InvalidResult);
            }
        }
        charge(control, ZSetEvent::Work)?;
        Ok(GroupUpdate {
            input,
            aggregate,
            collections,
            output,
            initialized: &mut self.initialized,
            delta,
        })
    }
}
impl<D: GroupDefinition> core::fmt::Debug for IncrementalGroupAggregate<D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalGroupAggregate")
            .field("groups", &self.rows.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[must_use = "dropping a native group update preserves all accepted state"]
pub struct GroupUpdate<'a> {
    input: ZSetUpdate<'a, GraphValueRow>,
    aggregate: AggregateUpdate<'a, Key>,
    collections: Option<collection::Update<'a>>,
    output: ZSetUpdate<'a, GraphAggregateRow>,
    initialized: &'a mut bool,
    delta: ZSet<GraphAggregateRow>,
}
impl GroupUpdate<'_> {
    pub fn delta(&self) -> &ZSet<GraphAggregateRow> {
        &self.delta
    }
    pub fn commit(self) -> ZSet<GraphAggregateRow> {
        let Self {
            input,
            aggregate,
            collections,
            output,
            initialized,
            delta,
        } = self;
        input.commit();
        let _ = aggregate.commit();
        if let Some(collections) = collections {
            collections.commit();
        }
        output.commit();
        *initialized = true;
        delta
    }
}
impl core::fmt::Debug for GroupUpdate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GroupUpdate")
            .field("changed_groups", &self.delta.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod value_tests;
