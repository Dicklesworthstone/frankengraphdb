use super::*;
use crate::row_join::IncrementalRowJoin;
use crate::row_projection::IncrementalRowProjection;
use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, ZSet, ZWeight};
use fgdb_types::CanonicalScalar;
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(4);
type Bag = BTreeMap<GraphValueRow, i128>;

fn allow(_: GlaExecutionEvent) -> Result<(), usize> { Ok(()) }
fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n.k AS k, n.p AS p", |kind, name| {
        match (kind, name) {
            (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap().with_duplicates().into()
}
fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a), comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(b),
    }
}
fn row(values: &[Option<i64>]) -> GraphValueRow {
    GraphValueRow::from_owned_values(values.iter().map(|value| {
        GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
    }).collect())
}
fn bag(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(rows.into_iter().map(|(row, weight)| (row, ZWeight::from_i128(weight))),
        LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter().map(|(row, weight)| (row.clone(), weight.to_i128().unwrap())).collect()
}
fn difference(after: &ZSet<GraphValueRow>, before: &ZSet<GraphValueRow>) -> ZSet<GraphValueRow> {
    bag(plain(after).into_iter().chain(plain(before).into_iter().map(|(row, n)| (row, -n))))
}

#[test]
fn keys_are_necessary_conditions_and_original_names_survive() {
    for (code, keys) in [
        (vec![equal(0, 2)], vec![(0, 0)]),
        (vec![equal(2, 0), equal(1, 3), GraphSetPredicateOp::And], vec![(0, 0), (1, 1)]),
        (vec![equal(0, 2), GraphSetPredicateOp::Truth(Some(true)), GraphSetPredicateOp::Or], vec![]),
        (vec![equal(0, 2), GraphSetPredicateOp::Not], vec![]),
        (vec![equal(0, 2), equal(2, 0), GraphSetPredicateOp::Or], vec![(0, 0)]),
    ] {
        let query = leaf().cross_join(leaf()).unwrap().nested().unwrap().filter(&code).unwrap();
        let frozen = query.canonical_bytes();
        let (left, right, spec, output) = query.incremental_selected_join_with_control(&mut allow)
            .unwrap().unwrap();
        assert_eq!(left, &leaf());
        assert_eq!(right, &leaf());
        assert_eq!(spec.keys(), keys);
        assert_eq!(spec.predicate(), Some(code.as_slice()));
        assert_eq!(spec.kind(), crate::row_join::RowJoinKind::Inner);
        assert_eq!(output.columns().collect::<Vec<_>>(), vec!["k", "p", "k", "p"]);
        assert_eq!(output.input_types(), &[GraphSetColumnType::Scalar; 4]);
        assert_eq!(query.canonical_bytes(), frozen);
    }
}

// Independent primitive-value oracle. NULL never joins and multiplicities
// multiply, including two changes at once. No predicate interpreter is used.
fn input(mask: usize, right: bool) -> ZSet<GraphValueRow> {
    let data = if right { [(Some(1), Some(20)), (None, Some(40))] }
        else { [(Some(1), Some(10)), (Some(1), Some(30))] };
    bag(data.into_iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
        .map(|(at, (a, b))| (row(&[a, b]), (at + 1) as i128)))
}
fn oracle(left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>) -> Bag {
    let mut out = Bag::new();
    for (a, aw) in left.iter() {
        for (b, bw) in right.iter() {
            if !a.values()[0].is_null() && a.values()[0] == b.values()[0]
                && matches!((&a.values()[1], &b.values()[1]),
                    (GraphValue::Scalar(CanonicalScalar::Int(a)),
                     GraphValue::Scalar(CanonicalScalar::Int(b))) if a < b)
            {
                let pair = GraphValueRow::from_owned_values(a.values().iter().chain(b.values()).cloned().collect());
                *out.entry(pair).or_default() += aw.to_i128().unwrap() * bw.to_i128().unwrap();
            }
        }
    }
    out
}

#[test]
fn simultaneous_updates_and_retractions_match_independent_bag_oracle() {
    let code = [equal(0, 2), GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(1), comparison: IntegerComparison::Less,
        right: GraphSetOperand::Column(3),
    }, GraphSetPredicateOp::And];
    let query = leaf().cross_join(leaf()).unwrap().filter(&code).unwrap();
    for before in 0..16 {
        for after in 0..16 {
            let (_, _, spec, output) = query.incremental_selected_join_with_control(&mut allow)
                .unwrap().unwrap();
            let mut joined = IncrementalRowJoin::new(spec);
            let mut projected = IncrementalRowProjection::new(output);
            let old_left = input(before & 3, false);
            let old_right = input(before >> 2, true);
            let initial = joined.prepare(&old_left, &old_right, LIMBS, None,
                &mut |_| Ok::<_, ()>(())).unwrap().commit();
            projected.prepare(&initial, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
            assert_eq!(plain(projected.rows()), oracle(&old_left, &old_right));
            let next_left = input(after & 3, false);
            let next_right = input(after >> 2, true);
            let delta = joined.prepare(&difference(&next_left, &old_left),
                &difference(&next_right, &old_right), LIMBS, None, &mut |_| Ok::<_, ()>(()))
                .unwrap().commit();
            projected.prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
            assert_eq!(plain(projected.rows()), oracle(&next_left, &next_right));
        }
    }
}

#[test]
fn pages_sorts_and_distinct_between_filter_and_product_remain_barriers() {
    let product = leaf().cross_join(leaf()).unwrap();
    for child in [
        product.clone().with_page(0, Some(0)),
        product.clone().with_page(1, None),
        product.clone().with_order_by(&[GraphValueOrder::descending(0)]).unwrap(),
        product.clone().project(vec![GraphSetProjection::new("a", GraphSetValue::Column(0)),
            GraphSetProjection::new("b", GraphSetValue::Column(2))], GraphSetQuantifier::Distinct).unwrap(),
    ] {
        let width = child.column_types().len();
        let code = if width == 2 { equal(0, 1) } else { equal(0, 2) };
        let query = child.nested().unwrap().filter(&[code]).unwrap();
        assert!(query.incremental_selected_join_with_control(&mut allow).unwrap().is_none());
    }
    let selected = product.filter(&[equal(0, 2)]).unwrap();
    for query in [
        selected.clone().with_page(0, Some(0)),
        selected.clone().with_page(1, None),
        selected.with_order_by(&[GraphValueOrder::descending(0)]).unwrap(),
    ] {
        assert!(query.incremental_selected_join_with_control(&mut allow).unwrap().is_none());
    }
    // A child's complete selection remains attached to that child, not moved
    // through the join. Shape admission does not imply source derivative support.
    let left = leaf().with_page(1, Some(2));
    let right = leaf().with_page(0, Some(3));
    let selected = left.clone().cross_join(right.clone()).unwrap().filter(&[equal(0, 2)]).unwrap();
    let (a, b, _, _) = selected.incremental_selected_join_with_control(&mut allow).unwrap().unwrap();
    assert_eq!(a, &left);
    assert_eq!(b, &right);
}

#[test]
fn dynamic_any_comparisons_are_residuals_not_a_schema_narrowing() {
    let dynamic = PreparedGraphSet::singleton().unwind("item".into(), GraphSetValue::List(vec![
        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
    ])).unwrap();
    let code = [equal(0, 1)];
    let query = dynamic.cross_join(leaf()).unwrap().filter(&code).unwrap();
    let (_, _, spec, output) = query.incremental_selected_join_with_control(&mut allow).unwrap().unwrap();
    assert_eq!(spec.left_types(), &[GraphSetColumnType::Any]);
    assert!(spec.keys().is_empty());
    assert_eq!(spec.predicate(), Some(code.as_slice()));
    assert_eq!(output.column_types(), &[GraphSetColumnType::Any, GraphSetColumnType::Scalar, GraphSetColumnType::Scalar]);
}

#[test]
fn every_preparation_checkpoint_refuses_without_mutating_the_definition() {
    let query = leaf().cross_join(leaf()).unwrap().filter(&[
        equal(0, 2), equal(1, 3), GraphSetPredicateOp::And,
    ]).unwrap();
    let frozen = query.canonical_bytes();
    let mut checkpoints = 0;
    let expected = query.incremental_selected_join_with_control(&mut |_| {
        checkpoints += 1; Ok::<_, usize>(())
    }).unwrap().unwrap();
    for stop in 1..=checkpoints {
        let mut seen = 0;
        let result = query.incremental_selected_join_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(at) if at == stop));
        assert_eq!(seen, stop);
        assert_eq!(query.canonical_bytes(), frozen);
    }
    assert_eq!(query.incremental_selected_join_with_control(&mut allow).unwrap().unwrap(), expected);
}

fn selected_bag(
    query: &PreparedGraphSet, left: &ZSet<GraphValueRow>, right: &ZSet<GraphValueRow>,
) -> Bag {
    let (_, _, spec, output) = query.incremental_selected_join_with_control(&mut allow).unwrap().unwrap();
    let mut joined = IncrementalRowJoin::new(spec);
    let mut projected = IncrementalRowProjection::new(output);
    let delta = joined.prepare(left, right, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
    projected.prepare(&delta, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
    plain(projected.rows())
}

#[test]
fn every_projected_operand_remaps_without_changing_nullable_bag_semantics() {
    let left = bag([(row(&[Some(1), Some(10)]), 2), (row(&[None, Some(10)]), 1)]);
    let right = bag([(row(&[Some(1), Some(30)]), 3),
        (row(&[Some(1), None]), 1), (row(&[Some(2), Some(10)]), 2)]);
    for a in 0..4 {
        for b in 0..4 {
            for c in 0..4 {
                let slots = [a, b, c, a];
                let projection = slots.iter().enumerate().map(|(at, &input)| {
                    GraphSetProjection::new(format!("c{at}"), GraphSetValue::Column(input))
                }).collect();
                let query = leaf().cross_join(leaf()).unwrap()
                    .project(projection, GraphSetQuantifier::All).unwrap()
                    .nested().unwrap().filter(&[equal(0, 1), GraphSetPredicateOp::IsNull {
                        operand: GraphSetOperand::Column(2), is_null: false,
                    }, GraphSetPredicateOp::And]).unwrap();
                let mut expected = Bag::new();
                for (l, lw) in left.iter() {
                    for (r, rw) in right.iter() {
                        let cells: Vec<_> = l.values().iter().chain(r.values()).collect();
                        if !cells[a].is_null() && cells[a] == cells[b] && !cells[c].is_null() {
                            let result = GraphValueRow::from_owned_values(slots.iter()
                                .map(|&at| (*cells[at]).clone()).collect());
                            *expected.entry(result).or_default() += lw.to_i128().unwrap() * rw.to_i128().unwrap();
                        }
                    }
                }
                assert_eq!(selected_bag(&query, &left, &right), expected, "slots {slots:?}");
                let (_, _, _, output) = query.incremental_selected_join_with_control(&mut allow).unwrap().unwrap();
                assert_eq!(output.columns().collect::<Vec<_>>(), vec!["c0", "c1", "c2", "c3"]);
            }
        }
    }
}

#[test]
fn projected_collisions_integrate_both_parent_retractions_before_publication() {
    let query = leaf().cross_join(leaf()).unwrap().project(vec![
        GraphSetProjection::new("left_key", GraphSetValue::Column(0)),
        GraphSetProjection::new("right_key", GraphSetValue::Column(2)),
    ], GraphSetQuantifier::All).unwrap().filter(&[equal(0, 1)]).unwrap();
    let (_, _, spec, output) = query.incremental_selected_join_with_control(&mut allow).unwrap().unwrap();
    assert_eq!(spec.keys(), &[(0, 0)]);
    assert_eq!(spec.predicate(), Some([equal(0, 2)].as_slice()));
    let mut joined = IncrementalRowJoin::new(spec);
    let mut projected = IncrementalRowProjection::new(output);
    let left = bag([(row(&[Some(1), Some(10)]), 2), (row(&[Some(1), Some(30)]), 1)]);
    let right = bag([(row(&[Some(1), Some(20)]), 1), (row(&[Some(1), Some(50)]), 3)]);
    let initial = joined.prepare(&left, &right, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
    projected.prepare(&initial, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
    assert_eq!(plain(projected.rows()), Bag::from([(row(&[Some(1), Some(1)]), 12)]));
    let left_delta = bag([(row(&[Some(1), Some(10)]), -2)]);
    let right_delta = bag([(row(&[Some(1), Some(50)]), -3)]);
    let pending = joined.prepare(&left_delta, &right_delta, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
    let delta = projected.prepare(&pending, LIMBS, None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
    assert_eq!(plain(&delta), Bag::from([(row(&[Some(1), Some(1)]), -11)]));
    assert_eq!(plain(projected.rows()), Bag::from([(row(&[Some(1), Some(1)]), 1)]));
}

#[test]
fn projection_lowering_preserves_barriers_and_refuses_at_every_metadata_checkpoint() {
    let projection = vec![
        GraphSetProjection::new("right_value", GraphSetValue::Column(3)),
        GraphSetProjection::new("left_key", GraphSetValue::Column(0)),
        GraphSetProjection::new("left_value", GraphSetValue::Column(1)),
        GraphSetProjection::new("right_key", GraphSetValue::Column(2)),
    ];
    let projected = leaf().cross_join(leaf()).unwrap().project(projection, GraphSetQuantifier::All).unwrap();
    let query = projected.clone().filter(&[equal(1, 3)]).unwrap();
    let frozen = query.canonical_bytes();
    let mut total = 0;
    let expected = query.incremental_selected_join_with_control(&mut |_| {
        total += 1; Ok::<_, usize>(())
    }).unwrap().unwrap();
    for stop in 1..=total {
        let mut seen = 0;
        assert!(matches!(query.incremental_selected_join_with_control(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(at) if at == stop));
        assert_eq!(query.canonical_bytes(), frozen);
    }
    assert_eq!(query.incremental_selected_join_with_control(&mut allow).unwrap().unwrap(), expected);
    let second = projected.clone().project(vec![
        GraphSetProjection::new("a", GraphSetValue::Column(1)),
        GraphSetProjection::new("b", GraphSetValue::Column(3)),
    ], GraphSetQuantifier::All).unwrap().filter(&[equal(0, 1)]).unwrap();
    assert!(second.incremental_selected_join_with_control(&mut allow).unwrap().is_none());
    for query in [
        projected.clone().with_page(0, Some(0)).filter(&[equal(1, 3)]).unwrap(),
        projected.with_order_by(&[GraphValueOrder::descending(0)]).unwrap().filter(&[equal(1, 3)]).unwrap(),
    ] {
        assert!(query.incremental_selected_join_with_control(&mut allow).unwrap().is_none());
    }
    let computed = leaf().cross_join(leaf()).unwrap().project(vec![
        GraphSetProjection::new("k", GraphSetValue::Column(0)),
        GraphSetProjection::new("r", GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1)))),
    ], GraphSetQuantifier::All).unwrap().filter(&[equal(0, 1)]).unwrap();
    assert!(computed.incremental_selected_join_with_control(&mut allow).unwrap().is_none());
}
