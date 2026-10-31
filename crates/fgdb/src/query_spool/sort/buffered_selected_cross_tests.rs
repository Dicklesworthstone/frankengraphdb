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

fn value_sequence(name: &str, values: &[GraphValue]) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            name.into(),
            GraphSetValue::List(values.iter().cloned().map(GraphSetValue::Value).collect()),
        )
        .unwrap()
}

fn tuple_sequence<const N: usize>(name: &str, values: &[[i64; N]]) -> PreparedGraphSet {
    let lists: Vec<_> = values
        .iter()
        .map(|row| GraphValue::List(row.iter().copied().map(scalar).collect()))
        .collect();
    value_sequence(name, &lists)
        .project(
            (0..N)
                .map(|column| {
                    GraphSetProjection::new(
                        format!("{name}{column}"),
                        GraphSetValue::Index {
                            list: Box::new(GraphSetValue::Column(0)),
                            index: Box::new(GraphSetValue::Value(scalar(column as i64))),
                        },
                    )
                })
                .collect(),
            GraphSetQuantifier::All,
        )
        .unwrap()
}

fn is_keyed(query: &PreparedGraphSet) -> bool {
    let compiled = AsyncSpillSetPlan::compile(query).unwrap();
    matches!(compiled.source(), AsyncSpillSetSourcePlan::Cross(plan)
        if plan.supports_keyed_selection())
}

#[test]
fn keyed_product_restores_occurrences_before_parent_pages_and_collect() {
    let ((), report) = run_async_under_lab(0xc01d_3151, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let raw = tuple_sequence("l", &[[2, 0], [1, 1], [2, 2]])
            .cross_join(tuple_sequence("r", &[[2, 0], [1, 1], [2, 2]]))
            .unwrap();
        let query = raw.clone().filter(&[equality(0, 2)]).unwrap();
        let ordinary = raw
            .filter(&[
                equality(0, 2),
                GraphSetPredicateOp::Not,
                GraphSetPredicateOp::Not,
            ])
            .unwrap();
        assert!(is_keyed(&query));
        assert!(!is_keyed(&ordinary));
        let literal = [
            [Some(2), Some(0), Some(2), Some(0)],
            [Some(2), Some(0), Some(2), Some(2)],
            [Some(1), Some(1), Some(1), Some(1)],
            [Some(2), Some(2), Some(2), Some(0)],
            [Some(2), Some(2), Some(2), Some(2)],
        ];
        let expected = frames(&literal);
        for input in [&query, &ordinary] {
            assert_eq!(
                execute(input, 2, &mut view, &cx, policy(5), 16, WORK)
                    .await
                    .unwrap()
                    .0,
                expected
            );
            let page = input.clone().with_page(1, Some(3));
            assert_eq!(
                execute(&page, 2, &mut view, &cx, policy(3), 16, WORK)
                    .await
                    .unwrap()
                    .0,
                expected[1..4]
            );
            let collected = input
                .clone()
                .group_by(
                    &[],
                    &[
                        fgdb_gql::GraphAggregate::collect("left_ids", 1),
                        fgdb_gql::GraphAggregate::collect("right_ids", 3),
                    ],
                )
                .unwrap();
            let expected_collection = GraphValueRow::from_owned_values(vec![
                GraphValue::List([0, 0, 1, 2, 2].into_iter().map(scalar).collect()),
                GraphValue::List([0, 2, 1, 0, 2].into_iter().map(scalar).collect()),
            ])
            .canonical_bytes()
            .unwrap();
            assert_eq!(
                execute(&collected, 2, &mut view, &cx, policy(1), 16, WORK)
                    .await
                    .unwrap()
                    .0,
                [expected_collection]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn keyed_product_semantic_groups_keep_numeric_and_composite_residual_equality() {
    use fgdb_types::{CanonicalDecimal, CanonicalF64, VId};
    let ((), report) = run_async_under_lab(0xc01d_3152, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let float = || GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(7.0)));
        let decimal = || {
            GraphValue::Scalar(CanonicalScalar::Decimal(
                CanonicalDecimal::from_scaled_half_even(7, 0).unwrap(),
            ))
        };
        let list = |value| GraphValue::List(vec![value].into_boxed_slice());
        let map = |value| GraphValue::Map {
            keys: vec![Box::<str>::from("k")].into_boxed_slice(),
            values: vec![value].into_boxed_slice(),
        };
        // Class numbers are independently specified TRUE equality classes.
        // Decimal candidates share language order with Int/Float but MUST NOT
        // match them, including recursively inside maps and lists.
        let fixtures = vec![
            (map(scalar(7)), Some(0)),
            (scalar(7), Some(1)),
            (list(scalar(7)), Some(2)),
            (float(), Some(1)),
            (map(float()), Some(0)),
            (decimal(), Some(3)),
            (list(decimal()), Some(4)),
            (map(decimal()), Some(5)),
            (list(GraphValue::Scalar(CanonicalScalar::Null)), None),
            (list(float()), Some(2)),
            (GraphValue::Scalar(CanonicalScalar::Null), None),
            (
                GraphValue::Vertices(vec![VId(7)].into_boxed_slice()),
                Some(6),
            ),
            (list(GraphValue::Vertex(VId(7))), Some(6)),
        ];
        let left: Vec<_> = fixtures.iter().map(|(value, _)| value.clone()).collect();
        let right: Vec<_> = fixtures
            .iter()
            .rev()
            .map(|(value, _)| value.clone())
            .collect();
        let raw = value_sequence("l", &left)
            .cross_join(value_sequence("r", &right))
            .unwrap();
        let keyed = raw.clone().filter(&[equality(0, 1)]).unwrap();
        let ordinary = raw
            .filter(&[
                equality(0, 1),
                GraphSetPredicateOp::Not,
                GraphSetPredicateOp::Not,
            ])
            .unwrap();
        assert!(is_keyed(&keyed));
        assert!(!is_keyed(&ordinary));
        let mut expected = Vec::new();
        for (left, class) in &fixtures {
            for (right, other) in fixtures.iter().rev() {
                if class.is_some() && class == other {
                    expected.push(
                        GraphValueRow::from_owned_values(vec![left.clone(), right.clone()])
                            .canonical_bytes()
                            .unwrap(),
                    );
                }
            }
        }
        for query in [&keyed, &ordinary] {
            assert_eq!(
                execute(query, 2, &mut view, &cx, policy(100), 100, WORK)
                    .await
                    .unwrap()
                    .0,
                expected
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn keyed_product_repeated_tuple_components_and_residuals_keep_exact_pairs() {
    let ((), report) = run_async_under_lab(0xc01d_3153, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let raw = tuple_sequence("l", &[[2, 1], [1, 3], [2, 4]])
            .cross_join(tuple_sequence(
                "r",
                &[[2, 2, 3], [2, 1, 9], [1, 1, 4], [2, 2, 5]],
            ))
            .unwrap();
        let code = [
            equality(0, 2),
            equality(0, 3),
            GraphSetPredicateOp::And,
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(1),
                comparison: IntegerComparison::Less,
                right: GraphSetOperand::Column(4),
            },
            GraphSetPredicateOp::And,
        ];
        let keyed = raw.clone().filter(&code).unwrap();
        let mut opaque = code.to_vec();
        opaque.extend([GraphSetPredicateOp::Not, GraphSetPredicateOp::Not]);
        let ordinary = raw.filter(&opaque).unwrap();
        assert!(is_keyed(&keyed));
        assert!(!is_keyed(&ordinary));
        let expected = frames(&[
            [Some(2), Some(1), Some(2), Some(2), Some(3)],
            [Some(2), Some(1), Some(2), Some(2), Some(5)],
            [Some(1), Some(3), Some(1), Some(1), Some(4)],
            [Some(2), Some(4), Some(2), Some(2), Some(5)],
        ]);
        for query in [&keyed, &ordinary] {
            assert_eq!(
                execute(query, 2, &mut view, &cx, policy(4), 16, WORK)
                    .await
                    .unwrap()
                    .0,
                expected
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn keyed_product_candidate_work_scales_with_keys_instead_of_the_cartesian_bag() {
    let ((), report) = run_async_under_lab(0xc01d_3154, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let mut previous = None;
        for size in [64, 128] {
            let left: Vec<_> = (0..size).rev().map(Some).collect();
            let right: Vec<_> = (0..size).map(Some).collect();
            let raw = sequence("l", &left)
                .cross_join(sequence("r", &right))
                .unwrap();
            let keyed = raw.clone().filter(&[equality(0, 1)]).unwrap();
            let ordinary = raw
                .filter(&[
                    equality(0, 1),
                    GraphSetPredicateOp::Not,
                    GraphSetPredicateOp::Not,
                ])
                .unwrap();
            assert!(is_keyed(&keyed));
            assert!(!is_keyed(&ordinary));
            let allowance = policy(size as u64);
            let fast = execute(
                &keyed,
                2,
                &mut view,
                &cx,
                allowance,
                (size * 2) as u64,
                WORK,
            )
            .await
            .unwrap();
            let slow = execute(
                &ordinary,
                2,
                &mut view,
                &cx,
                allowance,
                (size * 2) as u64,
                WORK,
            )
            .await
            .unwrap();
            let expected = frames(
                &left
                    .iter()
                    .map(|value| [*value, *value])
                    .collect::<Vec<_>>(),
            );
            assert_eq!(fast.0, expected);
            assert_eq!(slow.0, expected);
            assert!(fast.2.work_units * 2 < slow.2.work_units);
            if let Some((fast_before, slow_before)) = previous {
                assert!(fast.2.work_units <= fast_before * 3);
                assert!(slow.2.work_units > slow_before * 3);
            }
            previous = Some((fast.2.work_units, slow.2.work_units));
            // These are native candidate-work counters, not timing claims:
            // authenticated ordering/transfer work has a separate allowance.
            eprintln!(
                "selected cross size={size}: keyed native={} ordinary native={} keyed physical={} ordinary physical={}",
                fast.2.work_units, slow.2.work_units, fast.3, slow.3
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn keyed_product_private_ordinals_do_not_reduce_the_public_row_size_allowance() {
    let ((), report) = run_async_under_lab(0xc01d_3155, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let query = sequence("l", &[Some(7)])
            .cross_join(sequence("r", &[Some(7)]))
            .unwrap()
            .filter(&[equality(0, 1)])
            .unwrap();
        assert!(is_keyed(&query));
        let expected = frames(&[[Some(7), Some(7)]]);
        let limit = expected[0].len();
        let prepared =
            PreparedBufferedOrder::from_prepared_relation(&query, Some(CommitSeq(2))).unwrap();
        for max_row_bytes in [limit, limit - 1] {
            let scratch_pool = MemoryPool::new(2_000_000, 0).unwrap();
            let (mut scratch, _) = file(&cx, &scratch_pool).await;
            let (mut destination, _) = file(&cx, &scratch_pool).await;
            let (mut auxiliary, _) = file(&cx, &scratch_pool).await;
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(1),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    2,
                    64,
                    2,
                    128,
                    127,
                    max_row_bytes,
                    2,
                    WORK,
                )
                .await;
            if max_row_bytes == limit {
                let (spool, _) = result.unwrap();
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
            } else {
                assert!(matches!(
                    result,
                    Err(NativeSpoolError::RowTooLarge { bytes, limit: actual })
                        if bytes == limit && actual == max_row_bytes
                ));
            }
            assert_eq!(scratch_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[path = "buffered_language_order_tests.rs"]
mod language_order_tests;
