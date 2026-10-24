//! Literal bag oracles for the native bounded pair-merge executor.
use super::*;

type PairRows = Vec<[Option<i64>; 2]>;

// No production comparator, witness test, sorter or projection kernel is used
// to decide expected matches or their occurrence multiplicities.
fn pair_expected(
    left: &[[Option<i64>; 2]],
    right: &[[Option<i64>; 2]],
    keys: &[(usize, usize)],
    kind: RowJoinKind,
) -> Vec<Vec<u8>> {
    let matches = |left: &[Option<i64>; 2], right: &[Option<i64>; 2]| {
        keys.iter()
            .all(|&(l, r)| left[l].is_some() && left[l] == right[r])
    };
    let mut output = Vec::new();
    for left_row in left {
        let mut found = false;
        for right_row in right {
            if matches(left_row, right_row) {
                found = true;
                output.push([left_row[0], left_row[1], right_row[0], right_row[1]]);
            }
        }
        if !found && matches!(kind, RowJoinKind::Left | RowJoinKind::Full) {
            output.push([left_row[0], left_row[1], None, None]);
        }
    }
    if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
        for right_row in right {
            if !left.iter().any(|left_row| matches(left_row, right_row)) {
                output.push([None, None, right_row[0], right_row[1]]);
            }
        }
    }
    let mut rows: Vec<_> = output
        .into_iter()
        .map(|row| {
            GraphValueRow::from_owned_values(
                row.into_iter()
                    .map(|value| value.map_or(GraphValue::Scalar(CanonicalScalar::Null), scalar))
                    .collect(),
            )
        })
        .collect();
    rows.sort();
    rows.into_iter()
        .map(|row| row.canonical_bytes().unwrap())
        .collect()
}

fn pair_inputs(cut: u64, null_key: bool) -> (PairRows, PairRows) {
    let first = if cut == 1 { 7 } else { 17 };
    let last = if cut == 1 { 4 } else { 14 };
    let key = |value| (!null_key).then_some(value);
    (
        vec![
            [key(3), Some(first)],
            [key(3), Some(8)],
            [key(1), Some(9)],
            [key(1), Some(10)],
        ],
        vec![
            [key(1), Some(first)],
            [key(1), Some(8)],
            [key(1), Some(9)],
            [key(last), Some(10)],
        ],
    )
}

#[test]
fn merge_pairs_all_four_kinds_preserve_historical_bags_nulls_and_pages() {
    let ((), report) = run_async_under_lab(0xc01d_3121, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        for cut in [1, 2] {
            for null_key in [false, true] {
                let (left, right) = pair_inputs(cut, null_key);
                for kind in KINDS.into_iter().take(4) {
                    let query = joined(kind, true, false, null_key);
                    let expected = pair_expected(&left, &right, &[(0, 0)], kind);
                    let compiled = AsyncSpillSetPlan::compile(&query).unwrap();
                    let AsyncSpillSetSourcePlan::RowJoin(plan) = compiled.source() else {
                        panic!("expected a relational join");
                    };
                    assert!(plan.supports_merge_pairs());
                    let actual = execute(&query, cut, &mut view, &cx, policy(100), 100, WORK)
                        .await
                        .unwrap()
                        .0;
                    assert_eq!(actual, expected, "{cut} {kind:?} null={null_key}");
                    let page = query.with_page(1, Some(2));
                    assert_eq!(
                        execute(&page, cut, &mut view, &cx, policy(2), 100, WORK)
                            .await
                            .unwrap()
                            .0,
                        expected.iter().skip(1).take(2).cloned().collect::<Vec<_>>()
                    );
                    assert_eq!(pool.used(), baseline);
                }
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_pairs_replay_duplicate_groups_and_use_mapped_repeated_keys() {
    let ((), report) = run_async_under_lab(0xc01d_3122, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        // Each side declares its relation's own column domains (a property is
        // Stored, fgdb-kr8j1; a computed value Scalar).
        let types = leaf(0, false).column_types().to_vec();
        let definitions: &[&[(usize, usize)]] = &[
            &[(0, 0)],
            &[(1, 0), (0, 1)],
            &[(1, 1), (0, 0), (1, 1)],
            &[(0, 0), (0, 1)],
            &[(0, 0), (1, 0)],
        ];
        for cut in [1, 2] {
            let (left, right) = pair_inputs(cut, false);
            for keys in definitions {
                for kind in KINDS.into_iter().take(4) {
                    let query = leaf(0, false)
                        .join(
                            leaf(1, false),
                            RowJoinSpec::new(&types, &types, keys)
                                .unwrap()
                                .with_kind(kind),
                        )
                        .unwrap();
                    assert_eq!(
                        execute(&query, cut, &mut view, &cx, policy(100), 100, WORK)
                            .await
                            .unwrap()
                            .0,
                        pair_expected(&left, &right, keys, kind),
                        "{cut} {kind:?} keys={keys:?}"
                    );
                    assert_eq!(pool.used(), baseline);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_pairs_empty_inputs_keep_exact_outer_null_extensions() {
    let ((), report) = run_async_under_lab(0xc01d_3123, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let types = leaf(0, false).column_types().to_vec();
        let (left_rows, right_rows) = pair_inputs(2, false);
        for (empty_left, empty_right) in [(true, false), (false, true), (true, true)] {
            for kind in KINDS.into_iter().take(4) {
                let left = leaf(0, false).with_page(0, empty_left.then_some(0));
                let right = leaf(1, false).with_page(0, empty_right.then_some(0));
                let query = left
                    .join(
                        right,
                        RowJoinSpec::new(&types, &types, &[(0, 0)])
                            .unwrap()
                            .with_kind(kind),
                    )
                    .unwrap();
                let expected = pair_expected(
                    if empty_left { &[] } else { &left_rows },
                    if empty_right { &[] } else { &right_rows },
                    &[(0, 0)],
                    kind,
                );
                assert_eq!(
                    execute(&query, 2, &mut view, &cx, policy(100), 100, WORK)
                        .await
                        .unwrap()
                        .0,
                    expected
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_pairs_share_native_and_physical_budgets_and_do_not_hide_private_products() {
    let ((), report) = run_async_under_lab(0xc01d_3124, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        for kind in KINDS.into_iter().take(4) {
            let query = joined(kind, true, false, false);
            let (expected, records, native, physical) =
                execute(&query, 2, &mut view, &cx, policy(100), 100, WORK)
                    .await
                    .unwrap();
            let exact = GqlQueryPolicy::new(
                records.snapshot_records,
                records.result_rows,
                native.work_units,
                native.scratch_entries,
            );
            assert_eq!(
                execute(&query, 2, &mut view, &cx, exact, 100, physical)
                    .await
                    .unwrap()
                    .0,
                expected
            );
            for dimension in 0..3 {
                let mut policy = exact;
                let mut external = physical;
                match dimension {
                    0 => policy.evaluator.max_work_units -= 1,
                    1 => policy.evaluator.max_scratch_entries -= 1,
                    _ => external -= 1,
                }
                assert!(
                    execute(&query, 2, &mut view, &cx, policy, 100, external)
                        .await
                        .is_err()
                );
                assert_eq!(pool.used(), baseline);
            }
        }
        // Four left plus four right inputs fit eight. FULL produces nine
        // occurrences, so an empty parent page must not mask the private cap.
        let hidden = joined(RowJoinKind::Full, true, false, false).with_page(0, Some(0));
        assert!(matches!(
            execute(&hidden, 2, &mut view, &cx, policy(0), 8, WORK).await,
            Err(NativeSpoolError::BufferedExecute(error))
                if matches!(*error, GqlQueryError::Rows(_))
        ));
        assert!(
            execute(&hidden, 2, &mut view, &cx, policy(0), 9, WORK)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        assert_eq!(pool.used(), baseline);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_pairs_zero_page_and_empty_left_do_not_skip_failed_right_expressions() {
    let ((), report) = run_async_under_lab(0xc01d_3125, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let right = PreparedGraphSetText::prepare(
            "MATCH (a)-[r:R]->(b) RETURN b.p AS k,1/(a.p-a.p) AS value",
            resolve,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let left_types = leaf(0, false).column_types().to_vec();
        let right_types = right.column_types().to_vec();
        for kind in KINDS.into_iter().take(4) {
            let query = leaf(0, false)
                .with_page(0, Some(0))
                .join(
                    right.clone(),
                    RowJoinSpec::new(&left_types, &right_types, &[(0, 0)])
                        .unwrap()
                        .with_kind(kind),
                )
                .unwrap()
                .with_page(0, Some(0));
            assert!(matches!(
                execute(&query, 2, &mut view, &cx, policy(0), 100, WORK).await,
                Err(NativeSpoolError::SetExecution(
                    fgdb_gql::GraphSetExecutionError::Projection { .. }
                ))
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn merge_pairs_exact_duplicate_occurrences_multiply_without_duplicate_outer_padding() {
    let ((), report) = run_async_under_lab(0xc01d_3126, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let keys_only = |side| -> PreparedGraphSet {
            let endpoint = if side == 0 { "a" } else { "b" };
            PreparedGraphText::prepare(
                &format!("MATCH (a)-[r:R]->(b) RETURN {endpoint}.p AS k"),
                resolve,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .with_duplicates()
            .into()
        };
        let types = keys_only(0).column_types().to_vec();
        for kind in KINDS.into_iter().take(4) {
            let query = keys_only(0)
                .join(
                    keys_only(1),
                    RowJoinSpec::new(&types, &types, &[(0, 0)])
                        .unwrap()
                        .with_kind(kind),
                )
                .unwrap();
            // Two identical left 1s times three identical right 1s: six
            // occurrences, not one distinct pair or three witnesses.
            let mut expected = vec![[Some(1), Some(1)]; 6];
            if matches!(kind, RowJoinKind::Left | RowJoinKind::Full) {
                expected.extend([[Some(3), None]; 2]);
            }
            if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
                expected.push([None, Some(14)]);
            }
            let mut expected: Vec<_> = expected
                .into_iter()
                .map(|row| {
                    GraphValueRow::from_owned_values(
                        row.into_iter()
                            .map(|value| {
                                value.map_or(GraphValue::Scalar(CanonicalScalar::Null), scalar)
                            })
                            .collect(),
                    )
                })
                .collect();
            expected.sort();
            let expected: Vec<_> = expected
                .into_iter()
                .map(|row| row.canonical_bytes().unwrap())
                .collect();
            assert_eq!(
                execute(&query, 2, &mut view, &cx, policy(100), 100, WORK)
                    .await
                    .unwrap()
                    .0,
                expected
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
