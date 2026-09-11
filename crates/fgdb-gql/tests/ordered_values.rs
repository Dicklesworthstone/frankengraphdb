//! Ordered pages agree with independent complete-row sorting, not another plan.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphColumn, GraphMatchClause, GraphOrderError, GraphPatternBuilder,
    GraphValue, GraphValueOrder, GraphValueRow, GlaDirection, PreparedGraphPattern};
use fgdb_gql::{GqlQueryError, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, VId};
use std::cmp::Ordering;

fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, u64::MAX, u64::MAX) }
fn builder() -> GraphPatternBuilder {
    let mut b = GraphPatternBuilder::new();
    b.vertex("n").unwrap();
    b
}
fn pattern(distinct: bool, offset: u64, count: Option<u64>, order: &[GraphValueOrder])
    -> PreparedGraphPattern<GraphValueRow> {
    let b = builder();
    let p = b.prepare_values(&[GraphColumn::vertex("id", "n"),
        GraphColumn::property("score", "n", PropertyKeyId(1)),
        GraphColumn::property("tie", "n", PropertyKeyId(2))], offset, count).unwrap();
    let p = if distinct { p } else { p.with_duplicates() };
    p.with_order_by(order).unwrap()
}
fn compare(a: &[GraphValue], b: &[GraphValue], order: &[GraphValueOrder]) -> Ordering {
    for key in order {
        let (left, right) = (&a[key.column], &b[key.column]);
        if left.is_null() != right.is_null() {
            return if left.is_null() == key.nulls_first { Ordering::Less } else { Ordering::Greater };
        }
        let cmp = if key.descending { right.cmp(left) } else { left.cmp(right) };
        if cmp != Ordering::Equal { return cmp; }
    }
    a.cmp(b)
}

#[test]
fn every_small_ordered_page_matches_full_sort_and_whole_row_distinctness() {
    let ids = [VId(0), VId(1_u128 << 100), VId(u128::MAX)];
    let choices = [CanonicalScalar::Null, CanonicalScalar::Int(-9), CanonicalScalar::Int(7)];
    let ties = [CanonicalScalar::Bool(true), CanonicalScalar::Bool(false), CanonicalScalar::Bool(true)];
    let input = [ids[2], ids[0], ids[1], ids[0], ids[2]];
    for code in 0..27 {
        let scores = [&choices[code % 3], &choices[(code / 3) % 3], &choices[(code / 9) % 3]];
        for descending in [false, true] { for nulls_first in [false, true] {
            let order = [GraphValueOrder { column: 1, descending, nulls_first }, GraphValueOrder::descending(2)];
            for distinct in [false, true] { for offset in 0..=3 { for count in [Some(0), Some(1), Some(3), None] {
                let query = pattern(distinct, offset, count, &order);
                let frozen = query.canonical_bytes();
                let mut expected: Vec<Vec<GraphValue>> = input.iter().map(|id| {
                    let at = ids.iter().position(|candidate| candidate == id).unwrap();
                    vec![GraphValue::Vertex(*id), GraphValue::Scalar((*scores[at]).clone()), GraphValue::Scalar(ties[at].clone())]
                }).collect();
                expected.sort_by(|a, b| compare(a, b, &order));
                if distinct { expected.dedup(); }
                let expected: Vec<_> = expected.into_iter().skip(offset as usize)
                    .take(count.unwrap_or(u64::MAX) as usize).collect();
                for input in [input.to_vec(), input.iter().rev().copied().collect()] {
                    let actual = query.plan().execute_governed_with_properties(5, input, [],
                        |_, _| Ok::<_, ()>(true), |id, key| {
                            let at = ids.iter().position(|candidate| *candidate == id).unwrap();
                            Ok(Some(if key == PropertyKeyId(1) { scores[at] } else { &ties[at] }))
                        }, wide(), || Ok::<_, ()>(())).unwrap();
                    assert_eq!(actual.value.iter().map(|row| row.values().to_vec()).collect::<Vec<_>>(), expected);
                }
                assert_eq!(query.canonical_bytes(), frozen);
            }}}
        }}
    }
}

#[test]
fn ordered_definition_checks_columns_and_preserves_schema_and_multiplicity() {
    let b = builder();
    let base = b.prepare_values(&[GraphColumn::vertex("first", "n")], 1, Some(2)).unwrap();
    let original = base.canonical_bytes();
    assert_eq!(base.clone().with_order_by(&[]).unwrap_err(), GraphOrderError::EmptyOrder);
    assert_eq!(base.clone().with_order_by(&[GraphValueOrder::ascending(1)]).unwrap_err(), GraphOrderError::UnknownColumn { column: 1 });
    assert_eq!(base.clone().with_order_by(&[GraphValueOrder::ascending(0), GraphValueOrder::descending(0)]).unwrap_err(), GraphOrderError::DuplicateColumn { column: 0 });
    let order = [GraphValueOrder::descending(0)];
    let ranked = base.clone().with_order_by(&order).unwrap();
    assert_eq!(ranked.columns(), base.columns());
    assert_eq!(ranked.value_columns(), base.value_columns());
    assert_eq!(ranked.clone().with_order_by(&order).unwrap(), ranked);
    assert_ne!(ranked.canonical_bytes(), original);
    assert_eq!(base.canonical_bytes(), original);
    assert_eq!(base.clone().with_duplicates().with_order_by(&order).unwrap(), ranked.clone().with_duplicates());
    let renamed = b.prepare_values(&[GraphColumn::vertex("second", "n")], 1, Some(2)).unwrap()
        .with_order_by(&order).unwrap();
    assert_eq!(ranked.canonical_bytes(), renamed.canonical_bytes());
    assert!(!ranked.preserves_duplicates());
    assert!(ranked.clone().with_duplicates().preserves_duplicates());
    let reverse = ranked.with_order_by(&[GraphValueOrder::ascending(0)]).unwrap();
    let result = reverse.plan().execute_governed_with_properties(3, [VId(3),VId(1),VId(2)], [],
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_vertex(), Some(VId(2)));
}

#[test]
fn ordering_preserves_optional_null_bindings_and_parallel_occurrences() {
    let mut outer = GraphPatternBuilder::new(); outer.vertex("a").unwrap();
    let mut inner = GraphPatternBuilder::new(); inner.vertex("a").unwrap(); inner.vertex("b").unwrap();
    inner.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
    let query = outer.prepare_values_with_clauses(&[GraphMatchClause::optional(&inner)],
        &[GraphColumn::vertex("owner", "a"), GraphColumn::property("score", "b", PropertyKeyId(1))],
        0, None).unwrap().with_duplicates().with_order_by(&[GraphValueOrder::descending(1)]).unwrap();
    let score = CanonicalScalar::Int(9);
    let result = query.plan().execute_governed_with_properties(5, [VId(0), VId(1), VId(2)],
        [(VId(1),RelationId(1),VId(8));2], |_, _| Ok::<_, ()>(true), |id, _| {
            assert_eq!(id, VId(8), "a null binding must never reach property storage"); Ok(Some(&score))
        }, wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value.iter().map(|row| row.get(0).unwrap().as_vertex().unwrap()).collect::<Vec<_>>(),
        vec![VId(1), VId(1), VId(0), VId(2)]);
    assert!(result.value[2].get(1).unwrap().is_null());
}

#[test]
fn ordered_reads_keep_source_failures_even_when_zero_or_full_pages_discard_rows() {
    let scalar = CanonicalScalar::Int(1);
    for count in [Some(0), Some(1)] {
        let query = pattern(false, 0, count, &[GraphValueOrder::ascending(0)]);
        let mut reads = 0;
        let result = query.plan().execute_governed_with_properties(2, [VId(0),VId(1)], [],
            |_, _| Ok::<_, &str>(true), |id, key| {
                reads += 1;
                if id == VId(1) && key == PropertyKeyId(2) { Err("late discarded field") } else { Ok(Some(&scalar)) }
            }, wide(), || Ok::<_, ()>(()));
        assert!(matches!(result, Err(GqlQueryError::Source("late discarded field"))));
        assert_eq!(reads, 4);
    }
}

#[test]
fn ranked_pages_share_exact_limits_and_every_interruption_checkpoint() {
    let query = pattern(false, 1, Some(2), &[GraphValueOrder::descending(1)]);
    let scores = [CanonicalScalar::Int(1), CanonicalScalar::Int(4), CanonicalScalar::Int(2), CanonicalScalar::Null];
    let ids = [VId(0),VId(1),VId(2),VId(3),VId(1)];
    let mut calls = 0;
    let measured = query.plan().execute_governed_with_properties(5, ids, [],
        |_, _| Ok::<_, ()>(true), |id, _| Ok(Some(&scores[id.0 as usize])), wide(), || {
            calls += 1; Ok::<_, usize>(())
        }).unwrap();
    let exact = GqlQueryPolicy::new(5,2,measured.evaluator.work_units,measured.evaluator.scratch_entries);
    let run = |policy| query.plan().execute_governed_with_properties(5, ids, [],
        |_, _| Ok::<_, ()>(true), |id, _| Ok(Some(&scores[id.0 as usize])), policy, || Ok::<_, usize>(()));
    assert_eq!(run(exact).unwrap(), measured);
    for policy in [GqlQueryPolicy::new(5,1,u64::MAX,u64::MAX),
        GqlQueryPolicy::new(5,2,measured.evaluator.work_units-1,u64::MAX),
        GqlQueryPolicy::new(5,2,u64::MAX,measured.evaluator.scratch_entries-1)] {
        assert!(run(policy).is_err());
    }
    for stop in 1..=calls {
        let mut at = 0;
        let result = query.plan().execute_governed_with_properties(5, ids, [],
            |_, _| Ok::<_, ()>(true), |id, _| Ok(Some(&scores[id.0 as usize])), wide(), || {
                at += 1; if at == stop { Err(stop) } else { Ok(()) }
            });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value==stop));
        assert_eq!(at,stop);
    }
}
