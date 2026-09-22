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
