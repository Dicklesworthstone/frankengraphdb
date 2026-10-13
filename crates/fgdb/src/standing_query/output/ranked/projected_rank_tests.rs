//! Computed rank cells are retained, never evaluated by the ordered index.
use super::*;
use crate::standing_query::StandingQueryStats;
use fgdb_gql::{GqlQueryPolicy, GraphAggregateValue as Value, GraphExactAverage};
use fgdb_types::{CanonicalScalar, VId};

fn rank(key: u128, score: Value, descending: bool, nulls: GraphNullPlacement) -> Rank {
    let output = GraphAggregateRow::from_group_values(vec![], vec![score, Value::Count(99)]);
    Rank {
        order: Arc::from([GraphAggregateOrder {
            column: GraphAggregateColumn::Aggregate(0),
            descending,
            nulls,
        }]),
        // The raw count deliberately has the opposite order to some scores.
        group: Arc::new(GraphAggregateRow::from_group_values(
            vec![GraphValue::Vertex(VId(key))],
            vec![Value::Count(u64::MAX - key.min(u128::from(u64::MAX)) as u64)],
        )),
        projected: Some(Arc::new(output)),
    }
}

#[test]
fn projected_rank_preserves_extreme_fractions_nulls_and_original_key_ties() {
    let a = Value::Average(GraphExactAverage::new(i128::MAX, u64::MAX).unwrap());
    let b = Value::Average(GraphExactAverage::new(i128::MAX - 1, u64::MAX - 1).unwrap());
    // (M-1)/(D-1) > M/D when M>D. Cross-multiplication would overflow i128.
    for descending in [false, true] {
        for nulls in [GraphNullPlacement::First, GraphNullPlacement::Last] {
            let left = rank(9, a.clone(), descending, nulls);
            let right = rank(1, b.clone(), descending, nulls);
            let expected = if descending { Ordering::Greater } else { Ordering::Less };
            assert_eq!(left.cmp(&right), expected);
            assert_eq!(right.cmp(&left), expected.reverse());
            let null = rank(0, Value::Value(GraphValue::Scalar(CanonicalScalar::Null)), descending, nulls);
            assert_eq!(null.cmp(&left), if nulls == GraphNullPlacement::First {
                Ordering::Less
            } else {
                Ordering::Greater
            });
            let left = rank(1, Value::Count(2), descending, nulls);
            let right = rank(u128::MAX, Value::Average(GraphExactAverage::new(4, 2).unwrap()), descending, nulls);
            assert_eq!(left.cmp(&right), Ordering::Less, "complete 128-bit keys break numeric ties");
        }
    }
}

#[test]
fn equal_rank_replacement_refreshes_complete_and_projected_rows_without_copying_payloads() {
    let old = rank(1, Value::Integer(7), true, GraphNullPlacement::Last);
    let mut next = old.clone();
    next.group = Arc::new(GraphAggregateRow::from_group_values(
        old.group.keys().to_vec(), vec![Value::Count(17)],
    ));
    next.projected = Some(Arc::new(GraphAggregateRow::from_group_values(
        vec![], vec![Value::Integer(7), Value::Count(100)],
    )));
    assert_eq!(old.cmp(&next), Ordering::Equal, "unreferenced cells do not reorder a group");
    let mut base = Ranked::new();
    base.insert(old.clone(), Arc::clone(old.projected.as_ref().unwrap()));
    let mut patch = RankPatch::new();
    patch.insert(next.clone(), Some(Arc::clone(next.projected.as_ref().unwrap())));
    publish_ranked(&mut base, patch);
    let (stored, output) = base.first_key_value().unwrap();
    assert_eq!(stored.group.values(), &[Value::Count(17)]);
    assert_eq!(output.values()[1], Value::Count(100));
    assert!(Arc::ptr_eq(stored.projected.as_ref().unwrap(), output));
    assert!(Arc::ptr_eq(output, next.projected.as_ref().unwrap()));
}

#[test]
fn projected_index_merges_deletions_and_score_changes_before_refilling_each_window() {
    for descending in [false, true] {
        for removed in 0..5 {
            let make = |key: usize, score: i128| {
                rank(key as u128, Value::Integer(score), descending, GraphNullPlacement::Last)
            };
            let scores = [7, -2, 7, 99, 0];
            let mut base = Ranked::new();
            for (key, score) in scores.iter().copied().enumerate() {
                let candidate = make(key, score);
                base.insert(candidate.clone(), Arc::clone(candidate.projected.as_ref().unwrap()));
            }
            let mut patch = RankPatch::new();
            patch.insert(make(removed, scores[removed]), None);
            let replacement = make(removed, -20);
            patch.insert(replacement.clone(), Some(Arc::clone(replacement.projected.as_ref().unwrap())));
            let mut expected: Vec<_> = scores.iter().copied().enumerate().map(|(key, score)| {
                (key, if key == removed { -20 } else { score })
            }).collect();
            expected.sort_by(|(ka, a), (kb, b)| {
                (if descending { b.cmp(a) } else { a.cmp(b) }).then_with(|| ka.cmp(kb))
            });
            for offset in [0, 1, 3, 8, u64::MAX] {
                for count in [None, Some(0), Some(1), Some(3), Some(u64::MAX)] {
                    let mut checkpoint = || Ok(());
                    let mut meter = Meter {
                        policy: GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                        stats: StandingQueryStats::default(),
                        checkpoint: &mut checkpoint,
                    };
                    let rows = select_page(&base, &patch, offset, count, &mut meter).unwrap();
                    let values: Vec<_> = expected.iter()
                        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                        .take(count.and_then(|n| usize::try_from(n).ok()).unwrap_or(usize::MAX))
                        .map(|(_, score)| Value::Integer(*score)).collect();
                    assert_eq!(rows.iter().map(|row| row.values()[0].clone()).collect::<Vec<_>>(), values);
                    assert_eq!(base.len(), 5, "selection must not publish its tentative patch");
                }
            }
        }
    }
}

#[test]
fn distinct_class_refill_uses_complete_key_after_equal_projected_numeric_values() {
    let a = rank(1, Value::Count(2), true, GraphNullPlacement::Last);
    let b = rank(3, Value::Average(GraphExactAverage::new(4, 2).unwrap()), true, GraphNullPlacement::Last);
    let c = rank(2, Value::Integer(2), true, GraphNullPlacement::Last);
    let mut base = Ranked::new();
    for candidate in [&a, &b] {
        base.insert(candidate.clone(), Arc::clone(candidate.projected.as_ref().unwrap()));
    }
    let mut patch = RankPatch::new();
    patch.insert(a, None);
    patch.insert(c.clone(), Some(Arc::clone(c.projected.as_ref().unwrap())));
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: GqlQueryPolicy::new(100, 100, 100_000, 100_000),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let (representative, value) = first_after(Some(&base), &patch, &mut meter).unwrap().unwrap();
    assert_eq!(representative.group.keys(), &[GraphValue::Vertex(VId(2))]);
    assert_eq!(value.values()[0], Value::Integer(2), "retain the winning representation");
}
