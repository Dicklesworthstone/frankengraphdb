//! Residual witnesses are per occurrence, not per sorted equality-key group.
use super::*;

fn filtered_spec(kind: RowJoinKind, keys: &[(usize, usize)], rhs: usize) -> RowJoinSpec {
    let types = [GraphSetColumnType::Scalar; 2];
    RowJoinSpec::new(&types, &types, keys)
        .unwrap()
        .with_kind(kind)
        .with_predicate(&[GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(1),
            comparison: IntegerComparison::Less,
            right: GraphSetOperand::Column(rhs),
        }])
        .unwrap()
}

fn relation(text: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(text, resolve)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .with_duplicates()
        .into()
}

// Literal nested-loop oracle: no production matcher, key comparator, or join
// projector decides witness truth, multiplicity, or outer null extensions.
fn oracle(
    left: &[[Option<i64>; 2]],
    right: &[[Option<i64>; 2]],
    keys: &[(usize, usize)],
    kind: RowJoinKind,
    rhs: usize,
) -> Vec<Vec<u8>> {
    let matches = |l: &[Option<i64>; 2], r: &[Option<i64>; 2]| {
        keys.iter().all(|&(a, b)| l[a].is_some() && l[a] == r[b])
            && l[1].zip(r[rhs - 2]).is_some_and(|(a, b)| a < b)
    };
    let presence = matches!(kind, RowJoinKind::Semi | RowJoinKind::Anti);
    let project = |l: Option<&[Option<i64>; 2]>, r: Option<&[Option<i64>; 2]>| {
        let mut row = l.copied().unwrap_or([None; 2]).to_vec();
        if !presence {
            row.extend(r.copied().unwrap_or([None; 2]));
        }
        GraphValueRow::from_owned_values(
            row.into_iter()
                .map(|value| value.map_or(GraphValue::Scalar(CanonicalScalar::Null), scalar))
                .collect(),
        )
    };
    let mut output = Vec::new();
    for l in left {
        let mut found = false;
        for r in right {
            if matches(l, r) {
                found = true;
                if !presence {
                    output.push(project(Some(l), Some(r)));
                }
            }
        }
        let selected = match kind {
            RowJoinKind::Semi => found,
            RowJoinKind::Anti | RowJoinKind::Left | RowJoinKind::Full => !found,
            _ => false,
        };
        if selected {
            output.push(project(Some(l), None));
        }
    }
    if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
        for r in right {
            if !left.iter().any(|l| matches(l, r)) {
                output.push(project(None, Some(r)));
            }
        }
    }
    output.sort();
    output
        .into_iter()
        .map(|row| row.canonical_bytes().unwrap())
        .collect()
}

#[test]
fn filtered_merge_preserves_partial_group_witnesses_historical_bags_and_pages() {
    let ((), report) = run_async_under_lab(0xc01d_3131, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        for cut in [1, 2] {
            for kind in KINDS {
                let query = joined(kind, true, true, false);
                let compiled = AsyncSpillSetPlan::compile(&query).unwrap();
                let AsyncSpillSetSourcePlan::RowJoin(plan) = compiled.source() else {
                    panic!("expected a relational join");
                };
                assert!(plan.supports_merge_filtered());
                assert!(!plan.supports_merge_presence());
                assert!(!plan.supports_merge_pairs());
                let literal = expected(cut, kind, true);
                assert_eq!(
                    execute(&query, cut, &mut view, &cx, policy(100), 100, WORK)
                        .await
                        .unwrap()
                        .0,
                    literal,
                    "{cut} {kind:?}"
                );
                // At cut 2, right (1,17) matches both left occurrences but
                // right (1,8) and (1,9) remain unmatched in the SAME key group.
                if cut == 2 && kind == RowJoinKind::Full {
                    assert_eq!(literal.len(), 7);
                }
                let page = query.with_page(1, Some(2));
                assert_eq!(
                    execute(&page, cut, &mut view, &cx, policy(2), 100, WORK)
                        .await
                        .unwrap()
                        .0,
                    literal.iter().skip(1).take(2).cloned().collect::<Vec<_>>()
                );
                assert_eq!(pool.used(), baseline);
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_merge_preserves_mapped_repeated_keys_and_original_predicate_operands() {
    let ((), report) = run_async_under_lab(0xc01d_3132, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let right = relation("MATCH (a)-[r:R]->(b) RETURN r.p AS value,b.p AS k");
        let left_rows = [
            [Some(3), Some(17)],
            [Some(3), Some(8)],
            [Some(1), Some(9)],
            [Some(1), Some(10)],
        ];
        let right_rows = [
            [Some(17), Some(1)],
            [Some(8), Some(1)],
            [Some(9), Some(1)],
            [Some(10), Some(14)],
        ];
        let definitions: &[&[(usize, usize)]] = &[
            &[(0, 1)],
            &[(0, 1), (0, 1)],
            &[(0, 1), (1, 0), (0, 1)],
            &[(0, 1), (0, 0)],
            &[(0, 1), (1, 1)],
        ];
        for keys in definitions {
            for kind in KINDS {
                let query = leaf(0, false)
                    .join(right.clone(), filtered_spec(kind, keys, 2))
                    .unwrap();
                assert_eq!(
                    execute(&query, 2, &mut view, &cx, policy(100), 100, WORK)
                        .await
                        .unwrap()
                        .0,
                    oracle(&left_rows, &right_rows, keys, kind, 2),
                    "{kind:?} keys={keys:?}"
                );
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_merge_distinguishes_duplicate_products_null_keys_and_unknown_on() {
    let ((), report) = run_async_under_lab(0xc01d_3133, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        for null_key in [false, true] {
            for unknown in [false, true] {
                let key = if null_key { "missing" } else { "p" };
                let value = if unknown { "r.missing" } else { "1" };
                let left = relation(&format!(
                    "MATCH (a)-[r:R]->(b) RETURN a.{key} AS k,0 AS value"
                ));
                let right = relation(&format!(
                    "MATCH (a)-[r:R]->(b) RETURN b.{key} AS k,{value} AS value"
                ));
                let key = |k| (!null_key).then_some(k);
                let value = (!unknown).then_some(1);
                let l = [
                    [key(3), Some(0)],
                    [key(3), Some(0)],
                    [key(1), Some(0)],
                    [key(1), Some(0)],
                ];
                let r = [
                    [key(1), value],
                    [key(1), value],
                    [key(1), value],
                    [key(14), value],
                ];
                for kind in KINDS {
                    let query = left
                        .clone()
                        .join(right.clone(), filtered_spec(kind, &[(0, 0)], 3))
                        .unwrap();
                    assert_eq!(
                        execute(&query, 2, &mut view, &cx, policy(100), 100, WORK)
                            .await
                            .unwrap()
                            .0,
                        oracle(&l, &r, &[(0, 0)], kind, 3)
                    );
                    if !null_key && !unknown && kind == RowJoinKind::Full {
                        // Six matching duplicate pairs plus three padded rows:
                        // final LIMIT zero cannot mask the ninth private row.
                        let hidden = query.with_page(0, Some(0));
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
                    }
                }
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// Keep the scratch pool visible on error, unlike the convenience fixture's
// early-return path. All admitted pages and decoded/output guards must refund.
async fn checked_execution(
    query: &PreparedGraphSet,
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    allowance: GqlQueryPolicy,
    work: u64,
) -> Result<Measured> {
    let prepared =
        PreparedBufferedOrder::from_prepared_relation(query, Some(CommitSeq(2))).unwrap();
    let pool = MemoryPool::new(2_000_000, 0).unwrap();
    let (mut scratch, _) = file(cx, &pool).await;
    let (mut destination, _) = file(cx, &pool).await;
    let (mut auxiliary, _) = file(cx, &pool).await;
    let result = prepared
        .spool_in_view(
            view,
            cx,
            allowance,
            &mut scratch,
            &mut destination,
            &mut auxiliary,
            2,
            64,
            2,
            128,
            127,
            8192,
            100,
            work,
        )
        .await;
    let result = match result {
        Ok((spool, work)) => {
            let rows = contents(&spool, &mut destination, cx).await;
            Ok((rows, spool.row_stats(), spool.evaluator_stats(), work))
        }
        Err(error) => Err(error),
    };
    assert_eq!(
        pool.used(),
        0,
        "refusal must not retain a page, group, or tentative output"
    );
    result
}

#[test]
fn filtered_merge_shares_exact_budgets_across_right_absence_and_refunds_refusals() {
    let ((), report) = run_async_under_lab(0xc01d_3134, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        for kind in KINDS {
            let query = joined(kind, true, true, false);
            let (literal, records, native, physical) =
                checked_execution(&query, &mut view, &cx, policy(100), WORK)
                    .await
                    .unwrap();
            let exact = GqlQueryPolicy::new(
                records.snapshot_records,
                records.result_rows,
                native.work_units,
                native.scratch_entries,
            );
            assert_eq!(
                checked_execution(&query, &mut view, &cx, exact, physical)
                    .await
                    .unwrap()
                    .0,
                literal
            );
            for dimension in 0..3 {
                let mut allowance = exact;
                let mut work = physical;
                match dimension {
                    0 => allowance.evaluator.max_work_units -= 1,
                    1 => allowance.evaluator.max_scratch_entries -= 1,
                    _ => work -= 1,
                }
                let error = checked_execution(&query, &mut view, &cx, allowance, work)
                    .await
                    .expect_err("refusal must not retry the unkeyed path");
                if dimension == 2 {
                    assert!(matches!(
                        error,
                        NativeSpoolError::SortWorkLimit { limit, .. } if limit == work
                    ));
                } else {
                    assert!(matches!(
                        error,
                        NativeSpoolError::BufferedExecute(source)
                            if matches!(*source, GqlQueryError::Evaluator(_))
                    ));
                }
            }
            assert_eq!(
                checked_execution(&query, &mut view, &cx, exact, physical)
                    .await
                    .unwrap()
                    .0,
                literal
            );
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_merge_empty_children_keep_outer_and_presence_semantics() {
    let ((), report) = run_async_under_lab(0xc01d_3135, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let l = [
            [Some(3), Some(17)],
            [Some(3), Some(8)],
            [Some(1), Some(9)],
            [Some(1), Some(10)],
        ];
        let r = [
            [Some(1), Some(17)],
            [Some(1), Some(8)],
            [Some(1), Some(9)],
            [Some(14), Some(10)],
        ];
        for (empty_left, empty_right) in [(true, false), (false, true), (true, true)] {
            for kind in KINDS {
                let query = leaf(0, false)
                    .with_page(0, empty_left.then_some(0))
                    .join(
                        leaf(1, false).with_page(0, empty_right.then_some(0)),
                        filtered_spec(kind, &[(0, 0)], 3),
                    )
                    .unwrap();
                assert_eq!(
                    execute(&query, 2, &mut view, &cx, policy(100), 100, WORK)
                        .await
                        .unwrap()
                        .0,
                    oracle(
                        if empty_left { &[] } else { &l },
                        if empty_right { &[] } else { &r },
                        &[(0, 0)],
                        kind,
                        3,
                    )
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_merge_does_not_hide_failed_children_behind_empty_outer_or_zero_page() {
    let ((), report) = run_async_under_lab(0xc01d_3136, |root| async move {
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
        for kind in KINDS {
            let query = leaf(0, false)
                .with_page(0, Some(0))
                .join(right.clone(), filtered_spec(kind, &[(0, 0)], 3))
                .unwrap()
                .with_page(0, Some(0));
            assert!(matches!(
                checked_execution(&query, &mut view, &cx, policy(0), WORK).await,
                Err(NativeSpoolError::SetExecution(
                    fgdb_gql::GraphSetExecutionError::Projection { .. }
                ))
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_merge_selective_keys_avoid_all_pairs_when_on_rejects_every_candidate() {
    let ((), report) = run_async_under_lab(0xc01d_3137, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let values = (0..32)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let input = PreparedGraphSetText::prepare(
            &format!("UNWIND [{values}] AS n RETURN n AS k,n AS value"),
            resolve,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let types = [GraphSetColumnType::Scalar; 2];
        let query = |keyed| {
            let spec = if keyed {
                RowJoinSpec::new(&types, &types, &[(0, 0)])
            } else {
                RowJoinSpec::cross(&types, &types)
            }
            .unwrap()
            .with_kind(RowJoinKind::Full)
            .with_predicate(&[GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(1),
                comparison: IntegerComparison::Less,
                right: GraphSetOperand::Column(1),
            }])
            .unwrap();
            input.clone().join(input.clone(), spec).unwrap()
        };
        let allowance = GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000);
        let keyed = execute(&query(true), 2, &mut view, &cx, allowance, 100, 100_000_000)
            .await
            .unwrap();
        let theta = execute(&query(false), 2, &mut view, &cx, allowance, 100, 100_000_000)
            .await
            .unwrap();
        assert_eq!(keyed.0, theta.0);
        assert_eq!(keyed.0.len(), 64);
        assert_eq!(keyed.1.snapshot_records, 0);
        assert!(
            keyed.2.work_units < theta.2.work_units,
            "selective groups must not revisit every unrelated key"
        );
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
