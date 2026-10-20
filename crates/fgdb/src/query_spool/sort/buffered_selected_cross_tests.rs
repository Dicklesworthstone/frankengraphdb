//! Native buffered selection/product regressions using the existing lab,
//! Database/MemVfs and authenticated scratch fixtures, not a model executor.
use super::*;
use fgdb_gql::algebra::GraphValueOrder;
use fgdb_gql::{GraphSetProjection, GraphSetQuantifier, GraphSetValue};

fn equality(left: usize, right: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(left),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(right),
    }
}
fn graph_product() -> PreparedGraphSet {
    leaf(0, false).cross_join(leaf(1, false)).unwrap()
}
fn sequence(name: &str, values: &[Option<i64>]) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            name.into(),
            GraphSetValue::List(
                values
                    .iter()
                    .map(|value| {
                        GraphSetValue::Value(GraphValue::Scalar(
                            value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                        ))
                    })
                    .collect(),
            ),
        )
        .unwrap()
}
// Encode literal rows WITHOUT sorting: sequence preservation is part of the
// claim, and a canonical sort here would conceal an accidental reorder.
fn frames<const N: usize>(rows: &[[Option<i64>; N]]) -> Vec<Vec<u8>> {
    rows.iter()
        .map(|row| {
            GraphValueRow::from_owned_values(
                row.iter()
                    .map(|value| {
                        GraphValue::Scalar(
                            value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                        )
                    })
                    .collect(),
            )
            .canonical_bytes()
            .unwrap()
        })
        .collect()
}
fn is_selected(query: &PreparedGraphSet) -> bool {
    let compiled = AsyncSpillSetPlan::compile(query).unwrap();
    matches!(compiled.source(), AsyncSpillSetSourcePlan::Cross(plan) if plan.has_selection())
}

#[test]
fn selected_product_fits_retained_cap_without_weakening_plain_product_or_child_caps() {
    let ((), report) = run_async_under_lab(0xc01d_3141, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        for cut in [1, 2] {
            let raw = graph_product();
            let selected = raw.clone().filter(&[equality(0, 2)]).unwrap();
            assert!(is_selected(&selected));
            assert!(!is_selected(&raw));
            let mut right_values = [if cut == 1 { 7 } else { 17 }, 8, 9];
            right_values.sort();
            let mut literal = Vec::new();
            for left in [9, 10] {
                for right in right_values {
                    literal.push([Some(1), Some(left), Some(1), Some(right)]);
                }
            }
            let expected = frames(&literal);
            assert_eq!(expected.len(), 6);
            assert_eq!(
                execute(&selected, cut, &mut view, &cx, policy(6), 8, WORK)
                    .await
                    .unwrap()
                    .0,
                expected
            );
            // Eight complete child rows, sixteen candidate pairs, six outputs.
            assert!(matches!(
                execute(&raw, cut, &mut view, &cx, policy(16), 8, WORK).await,
                Err(NativeSpoolError::BufferedExecute(error))
                    if matches!(*error, GqlQueryError::Rows(_))
            ));
            for truth in [Some(false), None] {
                let empty = raw
                    .clone()
                    .filter(&[GraphSetPredicateOp::Truth(truth)])
                    .unwrap()
                    .with_page(0, Some(0));
                assert!(
                    execute(&empty, cut, &mut view, &cx, policy(0), 8, WORK)
                        .await
                        .unwrap()
                        .0
                        .is_empty()
                );
                assert!(matches!(
                    execute(&empty, cut, &mut view, &cx, policy(0), 7, WORK).await,
                    Err(NativeSpoolError::BufferedExecute(error))
                        if matches!(*error, GqlQueryError::Rows(_))
                ));
            }
            // The selected output itself remains capped, even behind LIMIT 0.
            let all = raw
                .filter(&[GraphSetPredicateOp::Truth(Some(true))])
                .unwrap()
                .with_page(0, Some(0));
            assert!(matches!(
                execute(&all, cut, &mut view, &cx, policy(0), 15, WORK).await,
                Err(NativeSpoolError::BufferedExecute(error))
                    if matches!(*error, GqlQueryError::Rows(_))
            ));
            assert!(
                execute(&all, cut, &mut view, &cx, policy(0), 16, WORK)
                    .await
                    .unwrap()
                    .0
                    .is_empty()
            );
            assert_eq!(pool.used(), baseline);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_product_keeps_unsorted_duplicate_sequence_mapped_copies_and_parent_pages() {
    let ((), report) = run_async_under_lab(0xc01d_3142, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let left = sequence("l", &[Some(3), Some(1), Some(3), Some(2)]);
        let right = sequence("r", &[Some(3), Some(4), Some(1), Some(3)]);
        let query = left
            .cross_join(right)
            .unwrap()
            .nested()
            .unwrap()
            .project(
                [(1, "r"), (0, "l"), (1, "again")]
                    .into_iter()
                    .map(|(at, name)| GraphSetProjection::new(name, GraphSetValue::Column(at)))
                    .collect(),
                GraphSetQuantifier::All,
            )
            .unwrap()
            .nested()
            .unwrap()
            .filter(&[equality(0, 1)])
            .unwrap();
        assert!(is_selected(&query));
        let literal = [
            [Some(3); 3],
            [Some(3); 3],
            [Some(1); 3],
            [Some(3); 3],
            [Some(3); 3],
        ];
        let expected = frames(&literal);
        let actual = execute(&query, 2, &mut view, &cx, policy(5), 8, WORK)
            .await
            .unwrap();
        assert_eq!(actual.0, expected);
        assert_eq!(actual.1.snapshot_records, 0);
        for (offset, count) in [(0, 0), (1, 3), (20, 2)] {
            let page = query.clone().with_page(offset, Some(count));
            assert_eq!(
                execute(&page, 2, &mut view, &cx, policy(count), 8, WORK)
                    .await
                    .unwrap()
                    .0,
                expected
                    .iter()
                    .skip(offset as usize)
                    .take(count as usize)
                    .cloned()
                    .collect::<Vec<_>>()
            );
        }
        let ordered = query
            .with_order_by(&[GraphValueOrder::ascending(0)])
            .unwrap();
        let mut sorted = literal;
        sorted.sort();
        assert_eq!(
            execute(&ordered, 2, &mut view, &cx, policy(5), 8, WORK)
                .await
                .unwrap()
                .0,
            frames(&sorted)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_product_preserves_three_valued_not_and_or_without_pruning_unknowns() {
    let ((), report) = run_async_under_lab(0xc01d_3143, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let raw = sequence("l", &[None, Some(2), Some(1), Some(2)])
            .cross_join(sequence("r", &[Some(2), None, Some(3)]))
            .unwrap();
        let cases = [
            (vec![equality(0, 1)], vec![[Some(2), Some(2)]; 2]),
            (
                vec![equality(0, 1), GraphSetPredicateOp::Not],
                vec![
                    [Some(2), Some(3)],
                    [Some(1), Some(2)],
                    [Some(1), Some(3)],
                    [Some(2), Some(3)],
                ],
            ),
            (
                vec![
                    equality(0, 1),
                    GraphSetPredicateOp::Truth(None),
                    GraphSetPredicateOp::Or,
                ],
                vec![[Some(2), Some(2)]; 2],
            ),
        ];
        for (code, literal) in cases {
            let query = raw.clone().filter(&code).unwrap();
            assert!(is_selected(&query));
            assert_eq!(
                execute(&query, 2, &mut view, &cx, policy(100), 12, WORK)
                    .await
                    .unwrap()
                    .0,
                frames(&literal)
            );
        }
        let all = raw
            .filter(&[
                equality(0, 1),
                GraphSetPredicateOp::Truth(Some(true)),
                GraphSetPredicateOp::Or,
            ])
            .unwrap();
        let mut literal = Vec::new();
        for l in [None, Some(2), Some(1), Some(2)] {
            for r in [Some(2), None, Some(3)] {
                literal.push([l, r]);
            }
        }
        assert_eq!(
            execute(&all, 2, &mut view, &cx, policy(12), 12, WORK)
                .await
                .unwrap()
                .0,
            frames(&literal)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_product_does_not_cross_product_order_or_page_barriers() {
    let ((), report) = run_async_under_lab(0xc01d_3144, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let barriers = [
            graph_product().with_page(0, Some(0)),
            graph_product().with_page(1, Some(3)),
            graph_product()
                .with_order_by(&[GraphValueOrder::descending(0)])
                .unwrap(),
        ];
        for input in barriers {
            let query = input
                .filter(&[GraphSetPredicateOp::Truth(Some(false))])
                .unwrap()
                .with_page(0, Some(0));
            assert!(!is_selected(&query));
            assert!(matches!(
                execute(&query, 2, &mut view, &cx, policy(0), 8, WORK).await,
                Err(NativeSpoolError::BufferedExecute(error))
                    if matches!(*error, GqlQueryError::Rows(_))
            ));
            assert!(
                execute(&query, 2, &mut view, &cx, policy(0), 16, WORK)
                    .await
                    .unwrap()
                    .0
                    .is_empty()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_product_empty_left_false_filter_and_zero_page_do_not_skip_child_errors() {
    let ((), report) = run_async_under_lab(0xc01d_3145, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        let right = PreparedGraphSetText::prepare(
            "MATCH (a)-[r:R]->(b) RETURN b.p AS k,1/(a.p-a.p) AS value",
            resolve,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let query = leaf(0, false)
            .with_page(0, Some(0))
            .cross_join(right)
            .unwrap()
            .filter(&[GraphSetPredicateOp::Truth(Some(false))])
            .unwrap()
            .with_page(0, Some(0));
        assert!(is_selected(&query));
        assert!(matches!(
            checked_execution(&query, &mut view, &cx, policy(0), WORK).await,
            Err(NativeSpoolError::SetExecution(
                fgdb_gql::GraphSetExecutionError::Projection { .. }
            ))
        ));
        assert_eq!(pool.used(), baseline);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_product_replays_exact_budgets_and_refunds_each_refusal() {
    let ((), report) = run_async_under_lab(0xc01d_3146, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        for code in [
            vec![equality(0, 2)],
            vec![GraphSetPredicateOp::Truth(Some(false))],
        ] {
            let query = graph_product().filter(&code).unwrap();
            let measured = checked_execution(&query, &mut view, &cx, policy(100), WORK)
                .await
                .unwrap();
            let exact = GqlQueryPolicy::new(
                measured.1.snapshot_records,
                measured.1.result_rows,
                measured.2.work_units,
                measured.2.scratch_entries,
            );
            assert_eq!(
                checked_execution(&query, &mut view, &cx, exact, measured.3)
                    .await
                    .unwrap()
                    .0,
                measured.0
            );
            for dimension in 0..3 {
                let mut allowance = exact;
                let mut physical = measured.3;
                match dimension {
                    0 => allowance.evaluator.max_work_units -= 1,
                    1 => allowance.evaluator.max_scratch_entries -= 1,
                    _ => physical -= 1,
                }
                let error = checked_execution(&query, &mut view, &cx, allowance, physical)
                    .await
                    .expect_err("refusal must not publish a prefix or retry an unfused product");
                if dimension == 2 {
                    assert!(matches!(
                        error,
                        NativeSpoolError::SortWorkLimit { limit, .. } if limit == physical
                    ));
                } else {
                    assert!(matches!(
                        error,
                        NativeSpoolError::BufferedExecute(source)
                            if matches!(*source, GqlQueryError::Evaluator(_))
                    ));
                }
                assert_eq!(pool.used(), baseline);
            }
            assert_eq!(
                checked_execution(&query, &mut view, &cx, exact, measured.3)
                    .await
                    .unwrap()
                    .0,
                measured.0
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_product_spills_only_matches_when_candidate_count_exceeds_private_row_cap() {
    let ((), report) = run_async_under_lab(0xc01d_3147, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let left: Vec<_> = (0..64).rev().map(Some).collect();
        let right: Vec<_> = (0..64).map(Some).collect();
        let raw = sequence("l", &left)
            .cross_join(sequence("r", &right))
            .unwrap();
        let selected = raw.clone().filter(&[equality(0, 1)]).unwrap();
        let literal: Vec<_> = left.iter().map(|value| [*value, *value]).collect();
        let measured = execute(&selected, 2, &mut view, &cx, policy(64), 128, WORK)
            .await
            .unwrap();
        assert_eq!(measured.0, frames(&literal));
        assert_eq!(measured.1.snapshot_records, 0);
        // 4,096 candidates do not become a 4,096-row scratch intermediate.
        // The same 128-row allowance still refuses an actual plain product.
        assert!(matches!(
            execute(&raw, 2, &mut view, &cx, policy(4096), 128, WORK).await,
            Err(NativeSpoolError::BufferedExecute(error))
                if matches!(*error, GqlQueryError::Rows(_))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[path = "buffered_language_order_tests.rs"]
mod language_order_tests;
