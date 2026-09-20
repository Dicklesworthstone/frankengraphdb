use super::*;
use crate::algebra::GraphValue;
use crate::row_window::{IncrementalRowWindow, RowWindowSpec};
use crate::{GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryExecution,
    GraphSetOperand, GraphSetPredicateOp, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_delta_types::{LimbLimit, PropertyKeyId, ZSet, ZWeight};
use fgdb_types::CanonicalScalar;

fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 10_000_000, 10_000_000) }
fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p,n.q AS q", |kind, name: &str| {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }).unwrap().bind_parameters(&GqlParameters::new()).unwrap().into()
}
fn rows() -> Vec<GraphValueRow> {
    [(Some(3), 1), (None, 2), (Some(2), 7), (Some(2), 7), (Some(2), 8), (Some(0), 9)]
        .map(|(p, q)| GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(p.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
            GraphValue::Scalar(CanonicalScalar::Int(q)),
        ])).to_vec()
}
fn run(query: &PreparedGraphSet) -> Vec<GraphValueRow> {
    // The source fixture represents six vertices' p/q properties. Only the
    // production relational executor, not this callback, handles row stages.
    query.execute_governed(policy(), |_, _| {
        let rows = rows();
        Ok::<_, GqlQueryError<(), ()>>(GqlQueryExecution {
            rows: GqlExecutionStats { snapshot_records: rows.len() as u64, result_rows: rows.len() as u64 },
            evaluator: GlaExecutionStats::default(), value: rows,
        })
    }, || Ok(())).unwrap().value
}
fn nonnull(query: PreparedGraphSet) -> PreparedGraphSet {
    query.filter(&[GraphSetPredicateOp::IsNull {
        operand: GraphSetOperand::Column(0), is_null: false,
    }]).unwrap()
}
fn reverse_columns(query: PreparedGraphSet, q: GraphSetQuantifier) -> PreparedGraphSet {
    query.project(vec![GraphSetProjection::new("q", GraphSetValue::Column(1)),
        GraphSetProjection::new("p", GraphSetValue::Column(0))], q).unwrap()
}
fn expand(query: PreparedGraphSet) -> PreparedGraphSet {
    query.unwind("x".into(), GraphSetValue::List(vec![
        GraphSetValue::Column(1), GraphSetValue::Column(0),
    ])).unwrap()
}
fn via_window(query: &PreparedGraphSet) -> Vec<GraphValueRow> {
    let (input, order, offset, count) = query.split_incremental_window().unwrap();
    let spec = RowWindowSpec::new(input.column_types().to_vec(), order.to_vec(),
        GraphSetQuantifier::All, offset, count).unwrap();
    let input = ZSet::from_updates(run(&input).into_iter().map(|row| (row, ZWeight::ONE)),
        LimbLimit::new(4), &mut |_| Ok::<_, ()>(())).unwrap();
    let mut window = IncrementalRowWindow::new(spec);
    window.prepare(&input, LimbLimit::new(4), None, &mut |_| Ok::<_, ()>(())).unwrap().commit();
    let mut out = Vec::new();
    for (row, weight) in window.rows() {
        for _ in 0..weight.to_i128().unwrap() { out.push(row.clone()); }
    }
    out
}

#[test]
fn nested_split_keeps_complete_children_quantifiers_and_definition_identity() {
    let ranked = leaf().with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(1, Some(4));
    let input = nonnull(ranked.clone()).nested().unwrap();
    let query = input.clone().with_page(1, Some(2));
    let before = query.canonical_bytes();
    let (child, order, offset, count) = query.split_incremental_window().unwrap();
    assert!(query.incremental_ordered_window().is_some());
    assert_eq!(child.canonical_bytes(), input.canonical_bytes());
    assert_eq!(order, &[GraphValueOrder::descending(0)]);
    assert_eq!((offset, count), (1, 2));
    assert_eq!(query.canonical_bytes(), before);
    assert_eq!(run(&query), via_window(&query));
    assert_eq!(input.incremental_result_order(), Some((&[GraphValueOrder::descending(0)][..], Some(4))));
    assert!(input.split_incremental_window().is_none());
}

#[test]
fn canonical_pages_do_not_require_an_explicit_order_by() {
    for input in [leaf(), nonnull(leaf()), leaf().nested().unwrap(),
        reverse_columns(leaf(), GraphSetQuantifier::All),
        leaf().combine(GraphSetOperation::Union, GraphSetQuantifier::All, leaf()).unwrap()] {
        let query = input.with_page(1, Some(3));
        assert!(query.split_incremental_window().unwrap().1.is_empty());
        assert!(query.incremental_ordered_window().is_none());
        assert_eq!(run(&query), via_window(&query));
    }
}

#[test]
fn expansion_product_and_order_preserving_projection_never_guess_canonical_order() {
    let expanded = expand(leaf());
    let product = leaf().cross_join(leaf()).unwrap();
    for input in [expanded.clone(), product, nonnull(expanded.clone()),
        reverse_columns(expanded.clone(), GraphSetQuantifier::All)] {
        assert!(input.incremental_result_order().is_none());
        for count in [0, 1, 4] {
            assert!(input.clone().with_page(1, Some(count)).split_incremental_window().is_none());
        }
        let ranked = input.with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(1, Some(4));
        assert_eq!(run(&ranked), via_window(&ranked));
    }
    // DISTINCT explicitly canonicalizes even an otherwise ordered enumeration.
    let distinct = reverse_columns(expanded, GraphSetQuantifier::Distinct).with_page(1, Some(2));
    assert!(distinct.split_incremental_window().unwrap().1.is_empty());
    assert_eq!(run(&distinct), via_window(&distinct));
}

#[test]
fn finite_upper_bounds_allow_resorting_or_skipping_without_an_artificial_unbounded_limit() {
    let input = leaf().with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(0, Some(4));
    for offset in [0, 1, 4, u64::MAX] {
        let query = nonnull(input.clone()).with_order_by(&[GraphValueOrder::ascending(1)]).unwrap()
            .with_page(offset, None);
        let (_, order, skip, take) = query.split_incremental_window().unwrap();
        assert!(query.incremental_ordered_window().is_some());
        assert_eq!(order, &[GraphValueOrder::ascending(1)]);
        assert_eq!(skip, offset);
        assert_eq!(take, 4_u64.saturating_sub(offset));
        assert_eq!(run(&query), via_window(&query));
    }
    let query = input.nested().unwrap().with_page(1, None);
    assert_eq!(query.split_incremental_window().unwrap().3, 3);
    assert_eq!(run(&query), via_window(&query));
    assert!(leaf().with_page(1, None).split_incremental_window().is_none());
    assert!(leaf().with_order_by(&[GraphValueOrder::descending(0)]).unwrap().split_incremental_window().is_none());
}

#[test]
fn projection_resets_rank_in_its_output_schema_instead_of_inheriting_old_column_positions() {
    let input = leaf().with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(0, Some(4));
    for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        let query = reverse_columns(input.clone(), quantifier).with_page(0, Some(2));
        let (_, order, _, _) = query.split_incremental_window().unwrap();
        assert!(order.is_empty());
        assert_eq!(run(&query), via_window(&query));
    }
}

#[test]
fn decomposed_windows_match_snapshot_sequence_for_nulls_duplicates_directions_and_scopes() {
    let inputs = [leaf(), nonnull(leaf()), expand(leaf()),
        leaf().cross_join(leaf()).unwrap(),
        reverse_columns(leaf(), GraphSetQuantifier::Distinct),
        nonnull(leaf().with_order_by(&[GraphValueOrder::descending(1)]).unwrap().with_page(1, Some(4)))];
    for input in inputs {
        for descending in [false, true] {
            for nulls_first in [false, true] {
                let order = [GraphValueOrder { column: 0, descending, nulls_first }];
                for offset in [0, 1, 3, u64::MAX] {
                    for count in [0, 1, 3, u64::MAX] {
                        let query = input.clone().with_order_by(&order).unwrap().with_page(offset, Some(count));
                        let frozen = query.canonical_bytes();
                        assert_eq!(run(&query), via_window(&query));
                        assert_eq!(query.canonical_bytes(), frozen);
                    }
                }
            }
        }
    }
}

#[test]
fn set_bounds_follow_bag_laws_and_overflow_never_becomes_an_unlimited_sentinel() {
    let bounded = |count| leaf().with_order_by(&[GraphValueOrder::descending(0)]).unwrap().with_page(0, Some(count));
    for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        for (operation, cap) in [(GraphSetOperation::Union, 5),
            (GraphSetOperation::Intersect, 2), (GraphSetOperation::Except, 3)] {
            let query = bounded(3).combine(operation, quantifier, bounded(2)).unwrap()
                .with_order_by(&[GraphValueOrder::ascending(1)]).unwrap();
            assert_eq!(query.split_incremental_window().unwrap().3, cap);
            assert_eq!(run(&query), via_window(&query));
        }
        let intersect = leaf().combine(GraphSetOperation::Intersect, quantifier, bounded(2)).unwrap()
            .with_order_by(&[GraphValueOrder::ascending(1)]).unwrap();
        assert_eq!(intersect.split_incremental_window().unwrap().3, 2);
        assert_eq!(run(&intersect), via_window(&intersect));
        let too_large = bounded(u64::MAX).combine(GraphSetOperation::Union, quantifier, bounded(1)).unwrap()
            .with_order_by(&[GraphValueOrder::descending(0)]).unwrap();
        assert!(too_large.split_incremental_window().is_none());
        assert!(too_large.incremental_result_order().unwrap().1.is_none());
        let explicit = too_large.with_page(0, Some(4));
        assert_eq!(run(&explicit), via_window(&explicit));
    }
}
