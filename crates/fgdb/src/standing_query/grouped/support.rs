//! Canonical scalar support for DISTINCT and retractable extrema.
//!
//! A support key is (group, aggregate, Some(value)); its integer aggregate
//! counts occurrences of that exact typed value. The corresponding None key
//! retains every input row as NULL, plus one nonnull zero per supported value.
//! Thus count_values on the None key is COUNT(DISTINCT), while all-null groups
//! still exist. Support and summaries publish in ONE existing aggregate guard.
//! Numeric DISTINCT uses the integer operator directly, not this encoding.

use super::*;
use fgdb_delta_types::zset::aggregate::AggregateUpdate;
use std::ops::Bound;

pub(super) fn uses_support(function: GraphAggregateFunction) -> bool {
    matches!(
        function,
        GraphAggregateFunction::CountDistinct
            | GraphAggregateFunction::Min
            | GraphAggregateFunction::Max
    )
}

pub(super) fn primary(key: &GroupKey, index: usize) -> AggregateKey {
    (Arc::clone(key), index, None)
}

pub(super) fn value_units(value: &GraphValue) -> Result<usize, StandingQueryFailure> {
    match value {
        GraphValue::Scalar(value) => Ok(scalar_units(value)),
        GraphValue::Vertex(_) => Ok(1),
        _ => Err(StandingQueryFailure::InvalidDelta),
    }
}

/// Add one synthetic summary contribution for each whole-tick zero crossing.
/// Consolidating the input first is essential: a delete and reinsert of the
/// same value in one commit must not transiently remove its distinct support.
pub(super) fn augment(
    aggregate: &IncrementalAggregate<AggregateKey>,
    delta: &mut ZSet<(AggregateKey, Option<i128>)>,
    meter: &mut Meter<'_>,
) -> Result<(), StandingQueryFailure> {
    let limbs = LimbLimit::new(4);
    let mut crossings = Vec::new();
    for ((key, value), change) in delta.iter() {
        meter.charge(ZSetEvent::Work)?;
        let (group, index, Some(argument)) = key else {
            continue;
        };
        if *value != Some(0) || argument.is_null() {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let old = aggregate.get(key).map(AggregateValues::count_rows);
        let next = match old {
            Some(old) => old.checked_add(change, limbs),
            None => change.checked_clone(limbs),
        }
        .map_err(|_| StandingQueryFailure::Arithmetic)?;
        if next < ZWeight::ZERO {
            return Err(StandingQueryFailure::InvalidDelta);
        }
        let was_present = old.is_some_and(|weight| !weight.is_zero());
        let is_present = !next.is_zero();
        if was_present != is_present {
            meter.charge(ZSetEvent::ScratchEntry)?;
            crossings.push((
                (primary(group, *index), Some(0)),
                ZWeight::from_i128(if is_present { 1 } else { -1 }),
            ));
        }
    }
    if !crossings.is_empty() {
        let crossings = ZSet::from_updates(crossings, limbs, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
        delta
            .integrate(&crossings, limbs, &mut |event| meter.charge(event))
            .map_err(zset_error)?;
    }
    Ok(())
}

fn bounds(
    key: &GroupKey,
    index: usize,
) -> Result<(Bound<AggregateKey>, Bound<AggregateKey>), StandingQueryFailure> {
    let next = index
        .checked_add(1)
        .ok_or(StandingQueryFailure::InvalidDelta)?;
    Ok((
        Bound::Excluded(primary(key, index)),
        Bound::Excluded(primary(key, next)),
    ))
}

fn first<'a>(
    keys: impl Iterator<Item = &'a AggregateKey>,
    mut present: impl FnMut(&AggregateKey) -> bool,
    meter: &mut Meter<'_>,
) -> Result<Option<Arc<GraphValue>>, StandingQueryFailure> {
    for key in keys {
        meter.charge(ZSetEvent::Work)?;
        if present(key) {
            let value = key.2.as_ref().ok_or(StandingQueryFailure::InvalidDelta)?;
            return Ok(Some(Arc::clone(value)));
        }
    }
    Ok(None)
}

pub(super) fn current_extremum(
    aggregate: &IncrementalAggregate<AggregateKey>,
    key: &GroupKey,
    index: usize,
    maximum: bool,
    meter: &mut Meter<'_>,
) -> Result<Option<Arc<GraphValue>>, StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    let keys = aggregate.range(bounds(key, index)?).map(|(key, _)| key);
    if maximum {
        first(keys.rev(), |_| true, meter)
    } else {
        first(keys, |_| true, meter)
    }
}

pub(super) fn pending_extremum(
    aggregate: &AggregateUpdate<'_, AggregateKey>,
    key: &GroupKey,
    index: usize,
    maximum: bool,
    meter: &mut Meter<'_>,
) -> Result<Option<Arc<GraphValue>>, StandingQueryFailure> {
    meter.charge(ZSetEvent::Work)?;
    let old = aggregate
        .retained_range(bounds(key, index)?)
        .map(|(key, _)| key);
    // A staged removal shadows old support. Only the invalidated prefix or
    // suffix is visited, never every unchanged value in the group.
    let old = if maximum {
        first(old.rev(), |key| aggregate.get(key).is_some(), meter)?
    } else {
        first(old, |key| aggregate.get(key).is_some(), meter)?
    };
    meter.charge(ZSetEvent::Work)?;
    let changed = aggregate
        .changed_range(bounds(key, index)?)
        .map(|(key, _)| key);
    let changed = if maximum {
        first(changed.rev(), |key| aggregate.get(key).is_some(), meter)?
    } else {
        first(changed, |key| aggregate.get(key).is_some(), meter)?
    };
    Ok(match (old, changed) {
        (Some(left), Some(right)) => {
            meter.units(
                ZSetEvent::Work,
                value_units(&left)?.max(value_units(&right)?),
            )?;
            Some(if maximum {
                left.max(right)
            } else {
                left.min(right)
            })
        }
        (left, right) => left.or(right),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_gql::GraphAggregate;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
    }

    fn definition(grouped: bool, numeric: bool) -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder
            .prepare_values(
                &[
                    GraphColumn::property("g", "n", PropertyKeyId(1)),
                    GraphColumn::property("v", "n", PropertyKeyId(2)),
                    GraphColumn::vertex("id", "n"),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let mut aggregates = vec![
            GraphAggregate::count_rows("rows"),
            GraphAggregate::count("nonnull", 1),
            GraphAggregate::count_distinct("different", 1),
            GraphAggregate::min("lo", 1),
            GraphAggregate::max("hi", 1),
            GraphAggregate::count_distinct("vertices", 2),
            GraphAggregate::min("first_vertex", 2),
            GraphAggregate::max("last_vertex", 2),
        ];
        if numeric {
            aggregates.extend([
                GraphAggregate::sum_int("sum", 1),
                GraphAggregate::sum_int_distinct("distinct_sum", 1),
                GraphAggregate::average_int("average", 1),
                GraphAggregate::average_int_distinct("distinct_average", 1),
            ]);
        }
        PreparedGraphAggregate::prepare(
            input,
            if grouped { &[0] } else { &[] },
            &aggregates,
            0,
            None,
        )
        .unwrap()
    }

    fn empty(definition: PreparedGraphAggregate) -> StandingQuery {
        assert!(eligible(&definition));
        StandingQuery {
            definition,
            policy: policy(),
            vertices: BTreeMap::new(),
            edges: None,
            aggregate: IncrementalAggregate::new(),
            rows: ZSet::new(),
            last_delta: None,
            frontier: CommitSeq::ORIGIN,
            stats: StandingQueryStats::default(),
            failure: None,
        }
    }

    fn state(group: i64, value: Option<CanonicalScalar>) -> VertexState {
        let mut props = BTreeMap::from([(PropertyKeyId(1), CanonicalScalar::Int(group))]);
        if let Some(value) = value {
            props.insert(PropertyKeyId(2), value);
        }
        VertexState {
            labels: BTreeSet::new(),
            props,
        }
    }

    fn transition(
        query: &mut StandingQuery,
        before: &BTreeMap<VId, VertexState>,
        after: &BTreeMap<VId, VertexState>,
        checkpoint: &mut dyn FnMut() -> Result<(), StandingQueryFailure>,
    ) -> Result<StandingQueryStats, StandingQueryFailure> {
        let mut meter = Meter {
            policy: query.policy,
            stats: StandingQueryStats::default(),
            checkpoint,
        };
        let mut updates = Vec::new();
        for (vid, old) in before {
            if after.get(vid) != Some(old) {
                contributions(&query.definition, *vid, old, -1, &mut updates, &mut meter)?;
            }
        }
        for (vid, new) in after {
            if before.get(vid) != Some(new) {
                contributions(&query.definition, *vid, new, 1, &mut updates, &mut meter)?;
            }
        }
        query.integrate(updates, &mut meter)?;
        Ok(meter.stats)
    }

    fn oracle(
        query: &StandingQuery,
        source: &BTreeMap<VId, VertexState>,
    ) -> ZSet<GraphAggregateRow> {
        let rows = query
            .definition
            .execute_governed(
                source.len() as u64,
                source.keys().copied(),
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, key| Ok(source.get(&vid).and_then(|state| state.props.get(&key))),
                policy(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        ZSet::from_updates(
            rows.into_iter().map(|row| (row, ZWeight::ONE)),
            LimbLimit::new(4),
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap()
    }

    #[test]
    fn scalar_support_matches_base_execution_through_all_small_bag_states() {
        let values = [
            None,
            Some(CanonicalScalar::Null),
            Some(CanonicalScalar::Bool(true)),
            Some(CanonicalScalar::Int(-7)),
            Some(CanonicalScalar::bytes(vec![3, 1, 4]).unwrap()),
        ];
        for grouped in [false, true] {
            let mut query = empty(definition(grouped, false));
            let mut source = BTreeMap::new();
            // Duplicates have separate identities; one is deliberately at the
            // full VId width, so no lossy integer/hash identity encoding works.
            for code in (0..243_usize).chain((0..243).rev()) {
                let mut n = code;
                let mut next = BTreeMap::new();
                for (at, value) in values.iter().enumerate() {
                    for duplicate in 0..(n % 3) {
                        let vid = VId(u128::MAX - (at * 3 + duplicate) as u128);
                        next.insert(vid, state((at % 2) as i64, value.clone()));
                    }
                    n /= 3;
                }
                transition(&mut query, &source, &next, &mut || Ok(())).unwrap();
                assert_eq!(
                    query.rows,
                    oracle(&query, &next),
                    "grouped={grouped}, code={code}"
                );
                source = next;
            }
        }
    }

    #[test]
    fn distinct_numeric_averages_remain_exact_under_duplicates_and_group_moves() {
        for grouped in [false, true] {
            let mut query = empty(definition(grouped, true));
            let mut before = BTreeMap::new();
            for step in 0..80 {
                let mut after = BTreeMap::new();
                for id in 0..8_u128 {
                    if (id + step) % 4 != 0 {
                        let value = match (id + step) % 5 {
                            0 => None,
                            1 => Some(CanonicalScalar::Null),
                            2 => Some(CanonicalScalar::Int(i64::MIN)),
                            _ => Some(CanonicalScalar::Int(7)),
                        };
                        after.insert(VId(id), state(((id + step) % 2) as i64, value));
                    }
                }
                transition(&mut query, &before, &after, &mut || Ok(())).unwrap();
                assert_eq!(query.rows, oracle(&query, &after));
                before = after;
            }
        }
    }

    #[test]
    fn every_support_checkpoint_aborts_all_state_and_permits_retry() {
        let before = BTreeMap::from([
            (VId(1), state(0, Some(CanonicalScalar::Int(-9)))),
            (VId(2), state(0, Some(CanonicalScalar::Int(-9)))),
            (VId(3), state(0, Some(CanonicalScalar::Int(8)))),
        ]);
        let after = BTreeMap::from([
            (VId(2), state(1, Some(CanonicalScalar::Int(2)))),
            (VId(4), state(0, Some(CanonicalScalar::Int(3)))),
            (VId(5), state(0, Some(CanonicalScalar::Null))),
        ]);
        let seed = || {
            let mut query = empty(definition(true, true));
            transition(&mut query, &BTreeMap::new(), &before, &mut || Ok(())).unwrap();
            query
        };
        let original = seed();
        let mut complete = seed();
        let mut calls = 0;
        transition(&mut complete, &before, &after, &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(complete.rows, oracle(&complete, &after));
        for stop in 1..=calls {
            let mut query = seed();
            let mut seen = 0;
            let result = transition(&mut query, &before, &after, &mut || {
                seen += 1;
                if seen == stop {
                    Err(StandingQueryFailure::Interrupted)
                } else {
                    Ok(())
                }
            });
            assert_eq!(result, Err(StandingQueryFailure::Interrupted));
            assert_eq!(seen, stop);
            assert_eq!(query.aggregate, original.aggregate);
            assert_eq!(query.rows, original.rows);
            assert_eq!(query.last_delta, original.last_delta);
            transition(&mut query, &before, &after, &mut || Ok(())).unwrap();
            assert_eq!(query.aggregate, complete.aggregate);
            assert_eq!(query.rows, complete.rows);
        }
    }

    #[test]
    fn extrema_do_not_scan_unchanged_interiors_or_unrelated_groups() {
        let measure = |large: bool| {
            let mut source = BTreeMap::from([
                (VId(1), state(0, Some(CanonicalScalar::Int(0)))),
                (VId(2), state(0, Some(CanonicalScalar::Int(5000)))),
                (VId(3), state(0, Some(CanonicalScalar::Int(10000)))),
            ]);
            if large {
                for id in 10..1010 {
                    source.insert(VId(id), state(0, Some(CanonicalScalar::Int(id as i64))));
                    source.insert(
                        VId(id + 10000),
                        state(id as i64, Some(CanonicalScalar::Int(7))),
                    );
                }
            }
            let mut query = empty(definition(true, true));
            transition(&mut query, &BTreeMap::new(), &source, &mut || Ok(())).unwrap();
            let mut updates = Vec::new();
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            contributions(
                &query.definition,
                VId(2),
                source.get(&VId(2)).unwrap(),
                -1,
                &mut updates,
                &mut meter,
            )
            .unwrap();
            contributions(
                &query.definition,
                VId(2),
                &state(0, Some(CanonicalScalar::Int(5001))),
                1,
                &mut updates,
                &mut meter,
            )
            .unwrap();
            query.integrate(updates, &mut meter).unwrap();
            meter.stats
        };
        assert_eq!(measure(false), measure(true));
    }
}
