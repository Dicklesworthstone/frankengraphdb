//! End-to-end relational ORDER BY under the existing database/lab/spill owner.
//! Literal expected sequences deliberately do not call a production comparator.
use super::*;
use fgdb_types::{CanonicalDecimal, CanonicalF64};

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn float(value: f64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
}
fn decimal(value: i128) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Decimal(
        CanonicalDecimal::from_integer(value).unwrap(),
    ))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn list(values: Vec<GraphValue>) -> GraphValue {
    GraphValue::List(values.into_boxed_slice())
}
fn values(input: &[GraphValue]) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "value".into(),
            GraphSetValue::List(input.iter().cloned().map(GraphSetValue::Value).collect()),
        )
        .unwrap()
}
fn encode_rows(rows: impl IntoIterator<Item = Vec<GraphValue>>) -> Vec<Vec<u8>> {
    rows.into_iter()
        .map(|row| GraphValueRow::from_owned_values(row).canonical_bytes().unwrap())
        .collect()
}
fn encode_values(values: &[GraphValue]) -> Vec<Vec<u8>> {
    encode_rows(values.iter().cloned().map(|value| vec![value]))
}
fn pair_input(rows: &[[GraphValue; 2]]) -> PreparedGraphSet {
    let input: Vec<_> = rows.iter().map(|row| list(row.to_vec())).collect();
    values(&input)
        .project(
            [("key", 0), ("tie", 1)]
                .into_iter()
                .map(|(name, at)| {
                    GraphSetProjection::new(
                        name,
                        GraphSetValue::Index {
                            list: Box::new(GraphSetValue::Column(0)),
                            index: Box::new(GraphSetValue::Value(int(at))),
                        },
                    )
                })
                .collect(),
            GraphSetQuantifier::All,
        )
        .unwrap()
}

#[test]
fn relational_numeric_order_keeps_typed_ties_in_both_directions_and_null_placements() {
    let ((), report) = run_async_under_lab(0xc01d_3151, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        // Each group is a language-order tie, already in literal typed order.
        // DESC reverses groups, never the canonical tie-break within a group.
        let groups = [
            vec![float(f64::NEG_INFINITY)],
            vec![float(-1.5)],
            vec![int(0), float(-0.0)],
            vec![int(2), decimal(2), float(2.0)],
            vec![int(7), int(7), float(7.0)],
            vec![float(9_007_199_254_740_992.0)],
            vec![int(9_007_199_254_740_993)],
            vec![float(f64::INFINITY)],
            vec![float(f64::NAN)],
        ];
        let mut input: Vec<_> = groups.iter().flatten().cloned().collect();
        input.reverse();
        input.insert(3, null());
        let original = values(&input);
        for descending in [false, true] {
            for nulls_first in [false, true] {
                let mut expected = Vec::new();
                for at in 0..groups.len() {
                    let at = if descending { groups.len() - 1 - at } else { at };
                    expected.extend(groups[at].iter().cloned());
                }
                if nulls_first {
                    expected.insert(0, null());
                } else {
                    expected.push(null());
                }
                let query = original
                    .clone()
                    .with_order_by(&[GraphValueOrder { column: 0, descending, nulls_first }])
                    .unwrap();
                let encoded = encode_values(&expected);
                let actual = execute(&query, 2, &mut view, &cx, policy(100), 100, WORK)
                    .await
                    .unwrap();
                assert_eq!(actual.0, encoded);
                assert_eq!(actual.1.snapshot_records, 0);
                for (offset, count) in [(0, 0), (1, 5), (100, 2)] {
                    let page = query.clone().with_page(offset, Some(count));
                    assert_eq!(
                        execute(&page, 2, &mut view, &cx, policy(count), 100, WORK)
                            .await
                            .unwrap()
                            .0,
                        encoded.iter().skip(offset as usize).take(count as usize).cloned().collect::<Vec<_>>()
                    );
                }
                assert_eq!(pool.used(), baseline);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_numeric_ties_obey_later_keys_and_child_pages_before_outer_reordering() {
    let ((), report) = run_async_under_lab(0xc01d_3152, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let input = [
            [int(7), int(20)], [float(7.0), int(10)], [decimal(7), int(15)],
            [int(6), int(99)], [float(7.0), int(20)], [int(7), int(20)],
        ];
        let expected = [
            [int(6), int(99)], [float(7.0), int(10)], [decimal(7), int(15)],
            [int(7), int(20)], [int(7), int(20)], [float(7.0), int(20)],
        ];
        let query = pair_input(&input)
            .with_order_by(&[GraphValueOrder::ascending(0), GraphValueOrder::ascending(1)])
            .unwrap();
        assert_eq!(
            execute(&query, 2, &mut view, &cx, policy(6), 100, WORK).await.unwrap().0,
            encode_rows(expected.iter().map(|row| row.to_vec()))
        );
        let outer = query.with_page(1, Some(3)).nested().unwrap()
            .with_order_by(&[GraphValueOrder::descending(1)]).unwrap();
        assert_eq!(
            execute(&outer, 2, &mut view, &cx, policy(3), 100, WORK).await.unwrap().0,
            encode_rows([
                vec![int(7), int(20)], vec![decimal(7), int(15)], vec![float(7.0), int(10)],
            ])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_distinct_keeps_numeric_representations_and_implicit_canonical_order() {
    let ((), report) = run_async_under_lab(0xc01d_3153, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let distinct = values(&[int(9), float(7.0), int(7), null(), float(1.0), int(7)])
            .project(
                vec![GraphSetProjection::new("value", GraphSetValue::Column(0))],
                GraphSetQuantifier::Distinct,
            )
            .unwrap();
        assert_eq!(
            execute(&distinct, 2, &mut view, &cx, policy(5), 100, WORK).await.unwrap().0,
            encode_values(&[null(), int(7), int(9), float(1.0), float(7.0)])
        );
        let ordered = distinct.with_order_by(&[GraphValueOrder::ascending(0)]).unwrap();
        assert_eq!(
            execute(&ordered, 2, &mut view, &cx, policy(5), 100, WORK).await.unwrap().0,
            encode_values(&[float(1.0), int(7), float(7.0), int(9), null()])
        );
        let page = ordered.with_page(1, Some(2));
        assert_eq!(
            execute(&page, 2, &mut view, &cx, policy(2), 100, WORK).await.unwrap().0,
            encode_values(&[int(7), float(7.0)])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_composite_order_uses_map_size_and_one_identity_list_family() {
    let ((), report) = run_async_under_lab(0xc01d_3154, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let text = GraphValue::Scalar(CanonicalScalar::ucs_basic_text("a").unwrap());
        let expected = vec![
            GraphValue::map(vec![]).unwrap(),
            GraphValue::map(vec![("z".into(), int(1))]).unwrap(),
            GraphValue::map(vec![("a".into(), int(0)), ("b".into(), int(0))]).unwrap(),
            GraphValue::Vertex(VId(2)), GraphValue::Edge(EId(3)),
            list(vec![]),
            GraphValue::Vertices(vec![VId(1)].into_boxed_slice()),
            list(vec![GraphValue::Vertex(VId(1))]),
            list(vec![text.clone()]), list(vec![int(1)]), list(vec![float(1.0)]),
            list(vec![null()]),
            GraphValue::Path(fgdb_gql::algebra::GraphPath::new(VId(0), Box::new([]))),
            text, GraphValue::Scalar(CanonicalScalar::Bool(true)), int(2), null(),
        ];
        let input: Vec<_> = expected.iter().rev().cloned().collect();
        let query = values(&input).with_order_by(&[GraphValueOrder::ascending(0)]).unwrap();
        assert_eq!(
            execute(&query, 2, &mut view, &cx, policy(100), 100, WORK).await.unwrap().0,
            encode_values(&expected)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_order_replays_exact_budgets_and_refunds_refusals() {
    let ((), report) = run_async_under_lab(0xc01d_3155, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        let query = values(&[int(8), float(0.5), int(1), float(1.0), decimal(1), null()])
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap();
        let measured = checked_execution(&query, &mut view, &cx, policy(100), WORK).await.unwrap();
        assert_eq!(measured.0, encode_values(&[float(0.5), int(1), decimal(1), float(1.0), int(8), null()]));
        let exact = GqlQueryPolicy::new(
            measured.1.snapshot_records, measured.1.result_rows,
            measured.2.work_units, measured.2.scratch_entries,
        );
        assert_eq!(
            checked_execution(&query, &mut view, &cx, exact, measured.3).await.unwrap().0,
            measured.0
        );
        for dimension in 0..3 {
            let mut allowance = exact;
            let mut work = measured.3;
            match dimension {
                0 => allowance.evaluator.max_work_units -= 1,
                1 => allowance.evaluator.max_scratch_entries -= 1,
                _ => work -= 1,
            }
            let error = checked_execution(&query, &mut view, &cx, allowance, work)
                .await.expect_err("an ordering refusal must not publish a page or retry typed order");
            if dimension == 2 {
                assert!(matches!(error, NativeSpoolError::SortWorkLimit { limit, .. } if limit == work));
            } else {
                assert!(matches!(error, NativeSpoolError::BufferedExecute(error)
                    if matches!(*error, GqlQueryError::Evaluator(_))));
            }
            assert_eq!(pool.used(), baseline);
        }
        assert_eq!(
            checked_execution(&query, &mut view, &cx, exact, measured.3).await.unwrap().0,
            measured.0
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relational_graph_history_orders_computed_mixed_numbers_at_the_requested_cut() {
    let ((), report) = run_async_under_lab(0xc01d_3156, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let query = PreparedGraphSetText::prepare(
            "MATCH (a)-[r:R]->(b) WITH r.p AS v UNWIND [v, v / 2.0] AS key RETURN key ORDER BY key",
            resolve,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        for (cut, expected) in [
            (1, vec![float(3.5), float(4.0), float(4.5), float(5.0), int(7), int(8), int(9), int(10)]),
            (2, vec![float(4.0), float(4.5), float(5.0), int(8), float(8.5), int(9), int(10), int(17)]),
        ] {
            let measured = execute(&query, cut, &mut view, &cx, policy(8), 100, WORK).await.unwrap();
            assert_eq!(measured.0, encode_values(&expected));
            assert!(measured.1.snapshot_records > 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
