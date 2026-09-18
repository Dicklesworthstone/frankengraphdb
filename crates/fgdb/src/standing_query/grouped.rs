//! Changed-group projection over the existing retractable aggregate operator.
//! No query AST interpreter or second graph source lives here. The admitted
//! GLA supplies predicates, binding slots and grouping positions.

mod support;

use super::*;
use fgdb_delta_types::zset::aggregate::{AggregateError, AggregateValues};
use fgdb_delta_types::ZWeight;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphAggregateValue, GraphExactAverage};

// Share immutable key/argument payloads across prepared patches and summaries.
// None addresses the aggregate summary; Some(value) addresses exact scalar
// support for COUNT DISTINCT and extrema. Integer DISTINCT uses the core's
// existing per-value counts directly. No digest stands in for value equality.
type GroupKey = Arc<[GraphValue]>;
pub(super) type AggregateKey = (GroupKey, usize, Option<Arc<GraphValue>>);
pub(super) type Contribution = ((AggregateKey, Option<i128>), ZWeight);

fn copied_key(
    key: &[GraphValue],
    meter: &mut Meter<'_>,
) -> Result<Vec<GraphValue>, StandingQueryFailure> {
    let mut result = Vec::new();
    meter.charge(ZSetEvent::ScratchEntry)?;
    for value in key {
        meter.charge(ZSetEvent::Work)?;
        meter.units(ZSetEvent::ScratchEntry, support::value_units(value)?)?;
        result.push(value.clone());
    }
    Ok(result)
}

pub(super) fn contributions(
    query: &PreparedGraphAggregate,
    vid: VId,
    state: &VertexState,
    sign: i128,
    output: &mut Vec<Contribution>,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    binding_contributions(query, &[(vid, state)], sign, output, meter)
}

/// Project one admitted binding through the same predicate and aggregate
/// domains as a vertex scan. The one-hop maintainer supplies two endpoint
/// slots, not another text parser or a second expression evaluator.
pub(super) fn binding_contributions(
    query: &PreparedGraphAggregate,
    binding: &[(VId, &VertexState)],
    sign: i128,
    output: &mut Vec<Contribution>,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    if !keeps(query.input_pattern().plan().operators(), binding, meter)? {
        return Ok(());
    }
    project_contributions(query, |slot| {
        binding.get(slot as usize).copied().map(Some)
            .ok_or(StandingQueryFailure::InvalidDelta)
    }, sign, output, meter)
}

/// Test only the admitted positive scope. Null extension must never rerun its
/// child predicates; a failed child is precisely what produces that null row.
pub(super) fn keeps(
    operators: &[GlaOperator],
    binding: &[(VId, &VertexState)],
    meter: &mut Meter<'_>,
) -> Result<bool, StandingQueryFailure> {
    for op in operators {
        match op {
            GlaOperator::Select { slot, predicates } => {
                let (_, state) = binding.get(slot.ordinal() as usize)
                    .copied().ok_or(StandingQueryFailure::InvalidDelta)?;
                for predicate in predicates {
                    meter.units(ZSetEvent::Work, 1 + predicate.comparison_work_units())?;
                    if !predicate.matches_borrowed(
                        state.labels.iter().copied(),
                        state.props.iter().map(|(key, value)| (*key, value)),
                    ) {
                        return Ok(false);
                    }
                }
            }
            GlaOperator::VertexIdentity { left, right, equal } => {
                meter.charge(ZSetEvent::Work)?;
                let left = binding.get(left.ordinal() as usize)
                    .ok_or(StandingQueryFailure::InvalidDelta)?.0;
                let right = binding.get(right.ordinal() as usize)
                    .ok_or(StandingQueryFailure::InvalidDelta)?.0;
                if (left == right) != *equal { return Ok(false); }
            }
            GlaOperator::CompareProperties { left, left_key, right, right_key, comparison } => {
                let (_, left) = binding.get(left.ordinal() as usize)
                    .copied().ok_or(StandingQueryFailure::InvalidDelta)?;
                let (_, right) = binding.get(right.ordinal() as usize)
                    .copied().ok_or(StandingQueryFailure::InvalidDelta)?;
                let left = left.props.get(left_key);
                let right = right.props.get(right_key);
                // Reserve variable payload comparison work before invoking
                // the canonical borrowed comparator. No encoding, coercion or
                // predicate-literal allocation occurs in this execution path.
                meter.charge(ZSetEvent::Work)?;
                meter.units(ZSetEvent::Work,
                    left.map_or(0, scalar_units).max(right.map_or(0, scalar_units)))?;
                // Ordinary WHERE keeps only TRUE; NULL/missing and incompatible
                // kinds do not pass even !=. This bool must never be negated as
                // though it represented a three-valued Boolean expression.
                if !comparison.accepts_scalar_pair(left, right) { return Ok(false); }
            }
            _ => {}
        }
    }
    Ok(true)
}

fn projected_value<'a>(
    column: ValueProjection,
    binding: &mut impl FnMut(u32) -> Result<Option<(VId, &'a VertexState)>, StandingQueryFailure>,
    meter: &mut Meter<'_>,
) -> Result<GraphValue, StandingQueryFailure> {
    match column {
        ValueProjection::Vertex { slot } => {
            let value = binding(slot.ordinal())?;
            meter.charge(ZSetEvent::ScratchEntry)?;
            Ok(value.map_or(GraphValue::Scalar(CanonicalScalar::Null), |(vid, _)| GraphValue::Vertex(vid)))
        }
        ValueProjection::Property { slot, key } => {
            let state = binding(slot.ordinal())?;
            let null = CanonicalScalar::Null;
            let value = state.and_then(|(_, state)| state.props.get(&key)).unwrap_or(&null);
            meter.units(ZSetEvent::ScratchEntry, scalar_units(value))?;
            Ok(GraphValue::Scalar(value.clone()))
        }
        _ => Err(StandingQueryFailure::InvalidDelta),
    }
}

/// Project an already qualified binding. A missing slot is malformed; a present
/// nullable slot is SQL NULL, including COUNT(vertex) and grouping by identity.
pub(super) fn project_contributions<'a>(
    query: &PreparedGraphAggregate,
    mut binding: impl FnMut(u32) -> Result<Option<(VId, &'a VertexState)>, StandingQueryFailure>,
    sign: i128,
    output: &mut Vec<Contribution>,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    meter.charge(ZSetEvent::ScratchEntry)?;
    let mut key = Vec::new();
    for &column in query.group_key_columns() {
        meter.charge(ZSetEvent::Work)?;
        key.push(projected_value(query.input_pattern().value_columns()[column], &mut binding, meter)?);
    }
    meter.charge(ZSetEvent::ScratchEntry)?;
    let key: GroupKey = Arc::from(key.into_boxed_slice());
    for (index, aggregate) in query.aggregates().iter().enumerate() {
        meter.charge(ZSetEvent::Work)?;
        let column = aggregate.argument_column()
            .map(|column| query.input_pattern().value_columns()[column]);
        if support::uses_support(aggregate.function()) {
            let column = column.ok_or(StandingQueryFailure::InvalidDelta)?;
            // Preserve source-row existence separately from distinct support:
            // nonempty all-null groups must survive with zero/null summaries.
            meter.charge(ZSetEvent::ScratchEntry)?;
            output.push(((support::primary(&key, index), None), ZWeight::from_i128(sign)));
            let value = projected_value(column, &mut binding, meter)?;
            if !value.is_null() {
                meter.charge(ZSetEvent::ScratchEntry)?;
                let value = Arc::new(value);
                meter.charge(ZSetEvent::ScratchEntry)?;
                output.push((((Arc::clone(&key), index, Some(value)), Some(0)), ZWeight::from_i128(sign)));
            }
            continue;
        }
        let value = match column {
            None => Some(0),
            Some(ValueProjection::Vertex { slot }) => binding(slot.ordinal())?.map(|_| 0),
            Some(ValueProjection::Property { slot, key }) => {
                match binding(slot.ordinal())?.and_then(|(_, state)| state.props.get(&key)) {
                    None | Some(CanonicalScalar::Null) => None,
                    Some(_) if aggregate.function() == GraphAggregateFunction::Count => Some(0),
                    Some(CanonicalScalar::Int(value)) => Some(i128::from(*value)),
                    _ => return Err(StandingQueryFailure::NonIntegerSum),
                }
            }
            _ => return Err(StandingQueryFailure::InvalidDelta),
        };
        meter.charge(ZSetEvent::ScratchEntry)?;
        output.push(((support::primary(&key, index), value), ZWeight::from_i128(sign)));
    }
    Ok(())
}

fn count(weight: &ZWeight) -> Result<u64, StandingQueryFailure> {
    u64::try_from(weight.to_i128().ok_or(StandingQueryFailure::Arithmetic)?)
        .map_err(|_| StandingQueryFailure::Arithmetic)
}

fn render<'a>(
    definition: &PreparedGraphAggregate,
    key: &GroupKey,
    mut summary: impl FnMut(usize) -> Option<&'a AggregateValues>,
    mut extremum: impl FnMut(usize, bool, &mut Meter<'_>) -> Result<Option<Arc<GraphValue>>, StandingQueryFailure>,
    meter: &mut Meter<'_>,
) -> Result<GraphAggregateRow, StandingQueryFailure> {
    let mut values = Vec::new();
    for (index, spec) in definition.aggregates().iter().enumerate() {
        meter.charge(ZSetEvent::Work)?;
        meter.charge(ZSetEvent::ScratchEntry)?;
        let summary = summary(index);
        let null = GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
        let value = match spec.function() {
            GraphAggregateFunction::CountRows => {
                GraphAggregateValue::Count(summary.map_or(Ok(0), |s| count(s.count_rows()))?)
            }
            GraphAggregateFunction::Count | GraphAggregateFunction::CountDistinct => {
                GraphAggregateValue::Count(summary.map_or(Ok(0), |s| count(s.count_values()))?)
            }
            GraphAggregateFunction::SumInt | GraphAggregateFunction::SumIntDistinct => {
                let sum = summary.and_then(|s| if spec.function() == GraphAggregateFunction::SumIntDistinct {
                    s.sum_distinct()
                } else { s.sum() });
                match sum {
                    None => null,
                    Some(sum) => GraphAggregateValue::Integer(
                        sum.to_i128().ok_or(StandingQueryFailure::Arithmetic)?,
                    ),
                }
            }
            GraphAggregateFunction::AverageInt | GraphAggregateFunction::AverageIntDistinct => {
                let parts = summary.and_then(|s| if spec.function() == GraphAggregateFunction::AverageIntDistinct {
                    s.average_distinct_parts()
                } else { s.average_parts() });
                match parts {
                    None => null,
                    Some((sum, denominator)) => {
                        let sum = sum.to_i128().ok_or(StandingQueryFailure::Arithmetic)?;
                        let denominator = count(denominator)?;
                        meter.units(ZSetEvent::Work, 128)?;
                        GraphAggregateValue::Average(
                            GraphExactAverage::new(sum, denominator)
                                .ok_or(StandingQueryFailure::InvalidDelta)?,
                        )
                    }
                }
            }
            GraphAggregateFunction::Min | GraphAggregateFunction::Max => {
                match extremum(index, spec.function() == GraphAggregateFunction::Max, meter)? {
                    None => null,
                    Some(value) => {
                        meter.units(ZSetEvent::ScratchEntry, support::value_units(&value)?)?;
                        GraphAggregateValue::Value(value.as_ref().clone())
                    }
                }
            }
            _ => return Err(StandingQueryFailure::InvalidDelta),
        };
        values.push(value);
    }
    let keys = copied_key(key, meter)?;
    definition.materialize_incremental_row(keys, values).ok_or(StandingQueryFailure::InvalidDelta)
}

impl StandingQuery {
    pub(super) fn integrate(
        &mut self,
        updates: Vec<Contribution>,
        meter: &mut Meter<'_>,
    ) -> Result<(), StandingQueryFailure> {
        let limbs = LimbLimit::new(4);
        let mut delta = ZSet::from_updates(updates, limbs, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        support::augment(&self.aggregate, &mut delta, meter)?;
        let mut groups = BTreeSet::new();
        for (((key, _, _), _), _) in delta.iter() {
            meter.charge(ZSetEvent::Work)?;
            if !groups.contains(key) {
                meter.charge(ZSetEvent::ScratchEntry)?;
                groups.insert(Arc::clone(key));
            }
        }
        let global = self.definition.group_key_columns().is_empty();
        if global && self.rows.is_empty() && groups.is_empty() {
            // Global aggregation has one row even on empty input. A grouped
            // empty input has none; an all-null nonempty group has one.
            meter.charge(ZSetEvent::ScratchEntry)?;
            groups.insert(Arc::from(Vec::<GraphValue>::new().into_boxed_slice()));
        }
        let mut previous = Vec::new();
        for key in groups {
            meter.charge(ZSetEvent::Work)?;
            let exists = if global {
                !self.rows.is_empty()
            } else {
                self.aggregate.get(&support::primary(&key, 0)).is_some()
            };
            let old = if exists {
                Some(render(
                    &self.definition, &key,
                    |index| self.aggregate.get(&support::primary(&key, index)),
                    |index, maximum, meter| support::current_extremum(&self.aggregate, &key, index, maximum, meter),
                    meter,
                )?)
            } else {
                None
            };
            meter.charge(ZSetEvent::ScratchEntry)?;
            previous.push((key, old));
        }
        let prepared = self.aggregate
            .prepare(&delta, limbs, &mut |event| meter.charge(event))
            .map_err(|error| match error {
                AggregateError::ZSet(error) => zset_error(error),
                AggregateError::NegativeMultiplicity => StandingQueryFailure::InvalidDelta,
            })?;
        let mut result_count = self.rows.len() as u128;
        let mut changes = Vec::new();
        for (key, old) in previous {
            meter.charge(ZSetEvent::Work)?;
            let exists = global || prepared.get(&support::primary(&key, 0)).is_some();
            let new = if exists {
                Some(render(
                    &self.definition, &key,
                    |index| prepared.get(&support::primary(&key, index)),
                    |index, maximum, meter| support::pending_extremum(&prepared, &key, index, maximum, meter),
                    meter,
                )?)
            } else {
                None
            };
            result_count = result_count.checked_sub(u128::from(old.is_some()))
                .ok_or(StandingQueryFailure::InvalidDelta)?;
            result_count += u128::from(new.is_some());
            if old == new { continue; }
            if let Some(row) = old {
                meter.charge(ZSetEvent::ScratchEntry)?;
                changes.push((row, ZWeight::from_i128(-1)));
            }
            if let Some(row) = new {
                meter.charge(ZSetEvent::ScratchEntry)?;
                changes.push((row, ZWeight::ONE));
            }
        }
        // Bound actual result groups, not internal scalar-support entries.
        if self.policy.rows.max_result_rows().is_some_and(|limit| result_count > u128::from(limit)) {
            return Err(StandingQueryFailure::ResultBudget);
        }
        let changes = ZSet::from_updates(changes, limbs, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        let sink = self.rows.prepare_update(&changes, limbs, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        (meter.checkpoint)()?;
        // Support, numeric summaries and output publish together. Nothing
        // after this boundary can perform recoverably fallible work.
        let _ = prepared.commit();
        sink.commit();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use crate::{DatabaseKeys, WriteBatch};
    use fgdb_delta_types::RelationId;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
    use fgdb_gql::GraphAggregate;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn definition() -> PreparedGraphAggregate {
        let mut input = GraphPatternBuilder::new();
        input.vertex("n").unwrap();
        let input = input.prepare_values(&[
            GraphColumn::property("group", "n", PropertyKeyId(1)),
            GraphColumn::property("value", "n", PropertyKeyId(2)),
        ], 0, None).unwrap().with_duplicates();
        PreparedGraphAggregate::prepare(input, &[0], &[
            GraphAggregate::count_rows("count"), GraphAggregate::sum_int("sum", 1),
        ], 0, None).unwrap()
    }
    fn seeded(initial: &LogicalDeltaBatch) -> StandingQuery {
        let policy = GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000);
        let mut query = StandingQuery {
            definition: definition(), policy, vertices: BTreeMap::new(),
            edges: None,
            aggregate: IncrementalAggregate::new(), rows: ZSet::new(),
            frontier: CommitSeq::ORIGIN, stats: StandingQueryStats::default(), failure: None,
        };
        let mut checkpoint = || Ok(());
        let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
        query.maintain(initial, &mut meter).unwrap();
        query.frontier = initial.commit_seq();
        query
    }

    #[test]
    fn every_maintenance_checkpoint_preserves_input_aggregate_and_result_then_retries() {
        let ((), report) = run_async_under_lab(0x6a06, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32]);
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut first = WriteBatch::new(RelationId(1));
            for id in [1, 2] {
                first.create_vertex(VId(id), vec![], vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                    (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
                ]);
            }
            let basis = db.write(&commit, first).await.unwrap();
            let initial = db.delta_index().unwrap().get(basis).unwrap().clone();
            let mut next = WriteBatch::new(RelationId(1));
            next.delete_vertex(VId(1));
            next.set_vertex_property(VId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(3)));
            next.set_vertex_property(VId(2), PropertyKeyId(2), Some(CanonicalScalar::Int(9)));
            next.create_vertex(VId(3), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(4))]);
            let at = db.write(&commit, next).await.unwrap();
            let batch = db.delta_index().unwrap().get(at).unwrap().clone();
            let before = seeded(&initial);
            let mut success = seeded(&initial);
            let policy = success.policy;
            let mut total = 0;
            {
                let mut checkpoint = || { total += 1; Ok(()) };
                let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                success.maintain(&batch, &mut meter).unwrap();
            }
            for stop in 1..=total {
                let mut candidate = seeded(&initial);
                let mut seen = 0;
                {
                    let mut checkpoint = || {
                        seen += 1;
                        if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
                    };
                    let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                    assert_eq!(candidate.maintain(&batch, &mut meter), Err(StandingQueryFailure::Interrupted));
                }
                assert_eq!(seen, stop);
                assert_eq!(candidate.vertices, before.vertices);
                assert_eq!(candidate.aggregate, before.aggregate);
                assert_eq!(candidate.rows, before.rows);
                assert_eq!(candidate.frontier, basis);
                let mut checkpoint = || Ok(());
                let mut meter = Meter { policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
                candidate.maintain(&batch, &mut meter).unwrap();
                assert_eq!(candidate.vertices, success.vertices);
                assert_eq!(candidate.aggregate, success.aggregate);
                assert_eq!(candidate.rows, success.rows);
            }
            // The same transaction at the output-group limit also refuses
            // without partially advancing the aggregate or source projection.
            let mut bounded = seeded(&initial);
            bounded.policy = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
            let mut checkpoint = || Ok(());
            let mut meter = Meter { policy: bounded.policy, stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            assert_eq!(bounded.maintain(&batch, &mut meter), Err(StandingQueryFailure::ResultBudget));
            assert_eq!(bounded.vertices, before.vertices);
            assert_eq!(bounded.aggregate, before.aggregate);
            assert_eq!(bounded.rows, before.rows);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
