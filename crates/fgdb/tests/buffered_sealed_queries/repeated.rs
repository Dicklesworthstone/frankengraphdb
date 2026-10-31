//! Repeated analytics use the same complete-body and cumulative-budget owner.
use super::*;

#[test]
fn row_arguments_preserve_full_width_ids_duplicate_seeds_and_nested_scope() {
    let ((), report) = run_async_under_lab(0x6273_5311, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let retained = cold.sealed_read_cache_pool().clone();
        let params = GqlParameters::new()
            .with_list(
                "starts",
                [SECOND, 1, SECOND, u128::MAX]
                    .into_iter()
                    .map(vertex)
                    .collect(),
            )
            .unwrap();
        let call = BFS.replace("$start", "start");
        let expected = [
            (1, 1, 0),
            (1, SECOND, 1),
            (1, THIRD, 2),
            (SECOND, SECOND, 0),
            (SECOND, SECOND, 0),
            (SECOND, THIRD, 1),
            (SECOND, THIRD, 1),
            (u128::MAX, u128::MAX, 0),
        ]
        .map(|(start, node, distance)| vec![vertex(start), vertex(node), integer(distance)])
        .to_vec();
        let queries = [
            format!(
                "UNWIND [77] AS tag UNWIND $starts AS start {call} \
                 YIELD node,distance RETURN start,node,distance ORDER BY start,node"
            ),
            format!(
                "UNWIND $starts AS seed CALL (seed) {{ WITH seed AS start \
                 {call} YIELD node,distance RETURN node,distance }} \
                 RETURN seed,node,distance ORDER BY seed,node"
            ),
        ];
        for text in queries {
            let native = PreparedNativeRead::prepare(&text, &params, resolve).unwrap();
            assert!(native.prepare_buffered_order(&params).is_err());
            let prepared = prepare(&text, &params);
            assert_eq!(
                resident_rows(
                    writer
                        .query(&cx, &text, &params, resolve, policy())
                        .unwrap()
                ),
                expected,
                "{text}",
            );
            for run_rows in [1, 4] {
                let actual = execute(
                    &prepared,
                    &mut cold,
                    &cx,
                    policy(),
                    &spill_pool,
                    run_rows,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                assert_eq!(actual.rows, expected, "{text}");
                assert_eq!(actual.snapshot, CommitSeq(2));
                assert_eq!(spill_pool.used(), 0);
            }
        }

        // A previous call's output is an ordinary input binding to the next
        // one, including an identity wider than every native integer scalar.
        let text = format!(
            "{BFS} YIELD node AS start {call} YIELD node,distance \
             RETURN start,node,distance ORDER BY start,node"
        );
        let expected = [
            (1, 1, 0),
            (1, SECOND, 1),
            (1, THIRD, 2),
            (SECOND, SECOND, 0),
            (SECOND, THIRD, 1),
            (THIRD, SECOND, 1),
            (THIRD, THIRD, 0),
        ]
        .map(|(start, node, distance)| vec![vertex(start), vertex(node), integer(distance)])
        .to_vec();
        let params = parameters();
        let actual = execute(
            &prepare(&text, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            2,
            WORK,
        )
        .await
        .unwrap();
        assert_eq!(actual.rows, expected);
        assert_eq!(
            resident_rows(
                writer
                    .query(&cx, &text, &params, resolve, policy())
                    .unwrap()
            ),
            expected,
        );
        let property_call = BFS.replace("$start", "start.p/10");
        let text = format!(
            "MATCH (start:Person) WHERE start.p=10 {property_call} \
             YIELD node,distance RETURN node,distance ORDER BY node"
        );
        let params = GqlParameters::new();
        let actual = execute(
            &prepare(&text, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap();
        assert_eq!(actual.rows, bfs_rows());
        assert_eq!(
            resident_rows(
                writer
                    .query(&cx, &text, &params, resolve, policy())
                    .unwrap()
            ),
            bfs_rows(),
        );
        assert_eq!(spill_pool.used(), 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn matched_row_arguments_pin_history_and_share_query_allowances() {
    let ((), report) = run_async_under_lab(0x6273_5312, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, mut writer) = fixture(&commit).await;
        let mut change = WriteBatch::new(R);
        change.add_edge(EId(BASE + 8), VId(THIRD), VId(FOURTH), vec![]);
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(3));
        writer.compact(&commit).await.unwrap();
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let params = GqlParameters::new();
        let call = BFS.replace("$start", "start");
        let text = format!(
            "MATCH (start:Person) {call} YIELD node,distance \
             RETURN start,node,distance ORDER BY start,node"
        );
        let prepared = prepare(&text, &params);
        let old = prepared.clone().with_as_of(CommitSeq(2)).unwrap();
        let historic = execute(&old, &mut cold, &cx, policy(), &spill_pool, 2, WORK)
            .await
            .unwrap();
        let historical_expected = [
            (1, 1, 0),
            (1, SECOND, 1),
            (1, THIRD, 2),
            (SECOND, SECOND, 0),
            (SECOND, THIRD, 1),
            (THIRD, SECOND, 1),
            (THIRD, THIRD, 0),
            (FOURTH, FOURTH, 0),
            (u128::MAX, u128::MAX, 0),
        ]
        .map(|(start, node, distance)| vec![vertex(start), vertex(node), integer(distance)])
        .to_vec();
        assert_eq!(historic.snapshot, CommitSeq(2));
        assert_eq!(historic.rows, historical_expected);
        let full = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 2, WORK)
            .await
            .unwrap();
        let current_expected = [
            (1, 1, 0),
            (1, SECOND, 1),
            (1, THIRD, 2),
            (1, FOURTH, 3),
            (SECOND, SECOND, 0),
            (SECOND, THIRD, 1),
            (SECOND, FOURTH, 2),
            (THIRD, SECOND, 1),
            (THIRD, THIRD, 0),
            (THIRD, FOURTH, 1),
            (FOURTH, FOURTH, 0),
            (u128::MAX, u128::MAX, 0),
        ]
        .map(|(start, node, distance)| vec![vertex(start), vertex(node), integer(distance)])
        .to_vec();
        assert_eq!(full.snapshot, CommitSeq(3));
        assert_eq!(full.rows, current_expected);
        assert_eq!(
            resident_rows(
                writer
                    .query(&cx, &text, &params, resolve, policy())
                    .unwrap()
            ),
            current_expected,
        );
        for allowance in [
            GqlQueryPolicy::new(full.records - 1, 100, WORK, WORK),
            GqlQueryPolicy::new(10_000, 100, full.evaluator_work - 1, WORK),
            GqlQueryPolicy::new(10_000, 100, WORK, full.evaluator_scratch - 1),
        ] {
            assert!(
                execute(&prepared, &mut cold, &cx, allowance, &spill_pool, 2, WORK)
                    .await
                    .is_err()
            );
            assert_eq!(spill_pool.used(), 0);
        }
        assert_eq!(
            execute(&old, &mut cold, &cx, policy(), &spill_pool, 2, WORK)
                .await
                .unwrap(),
            historic,
        );
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn row_argument_errors_remain_lazy_but_cannot_hide_behind_limit_zero() {
    let ((), report) = run_async_under_lab(0x6273_5313, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let params = GqlParameters::new();
        let call = BFS.replace("$start", "1/divisor");
        let late = format!("UNWIND [1,0] AS divisor {call} YIELD node RETURN node LIMIT 0");
        let error = execute(
            &prepare(&late, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeSpoolError::SetExecution(
                GraphSetExecutionError::Projection { error, .. }
            ) if error.kind == GraphIntegerErrorKind::DivisionByZero),
            "{error:?}",
        );
        assert!(
            writer
                .query(&cx, &late, &params, resolve, policy())
                .is_err()
        );
        assert_eq!(spill_pool.used(), 0);
        let skipped = format!("UNWIND [] AS divisor {call} YIELD node RETURN node LIMIT 0");
        let empty = execute(
            &prepare(&skipped, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap();
        assert!(empty.rows.is_empty());
        assert_eq!(empty.records, 0);
        assert!(
            resident_rows(
                writer
                    .query(&cx, &skipped, &params, resolve, policy())
                    .unwrap()
            )
            .is_empty()
        );
        assert_eq!(spill_pool.used(), 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn repeated_calls_preserve_bags_outer_filters_nested_frames_and_allowances() {
    let ((), report) = run_async_under_lab(0x6273_5301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let retained = cold.sealed_read_cache_pool().clone();
        let params = parameters();
        let text = format!(
            "UNWIND [1,1,2] AS cutoff CALL (cutoff) {{ \
             {BFS} YIELD node,distance WHERE distance>=cutoff \
             RETURN node,distance }} \
             RETURN cutoff,node,distance ORDER BY cutoff,node"
        );
        let native = PreparedNativeRead::prepare(&text, &params, resolve).unwrap();
        assert!(native.prepare_buffered_order(&params).is_err());
        let prepared = prepare(&text, &params);
        let single = prepare(
            &format!("{BFS} YIELD node,distance RETURN node,distance"),
            &params,
        );
        let once = execute(&single, &mut cold, &cx, policy(), &spill_pool, 4, WORK)
            .await
            .unwrap();
        let full = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 4, WORK)
            .await
            .unwrap();
        let expected = [
            (1, SECOND, 1),
            (1, SECOND, 1),
            (1, THIRD, 2),
            (1, THIRD, 2),
            (2, THIRD, 2),
        ]
        .map(|(cutoff, id, distance)| vec![integer(cutoff), vertex(id), integer(distance)])
        .to_vec();
        assert_eq!(full.rows, expected);
        assert_eq!(full.records, 3 * once.records);
        assert_eq!(
            resident_rows(
                writer
                    .query(&cx, &text, &params, resolve, policy())
                    .unwrap()
            ),
            expected,
        );
        assert_eq!(
            execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 4, WORK)
                .await
                .unwrap(),
            full,
        );
        let limited = execute(
            &prepare(&format!("{text} LIMIT 2"), &params),
            &mut cold,
            &cx,
            GqlQueryPolicy::new(10_000, 2, WORK, WORK),
            &spill_pool,
            4,
            WORK,
        )
        .await
        .unwrap();
        assert_eq!(limited.rows.as_slice(), &expected[..2]);
        assert_eq!(limited.records, full.records);
        for (allowance, physical) in [
            (GqlQueryPolicy::new(full.records - 1, 100, WORK, WORK), WORK),
            (
                GqlQueryPolicy::new(10_000, 100, full.evaluator_work - 1, WORK),
                WORK,
            ),
            (
                GqlQueryPolicy::new(10_000, 100, WORK, full.evaluator_scratch - 1),
                WORK,
            ),
            (policy(), full.sort_work - 1),
        ] {
            assert!(
                execute(
                    &prepared,
                    &mut cold,
                    &cx,
                    allowance,
                    &spill_pool,
                    4,
                    physical
                )
                .await
                .is_err()
            );
            assert_eq!(spill_pool.used(), 0);
        }
        let nested = format!(
            "UNWIND [1,2] AS tag CALL (tag) {{ CALL (tag) {{ \
             {BFS} YIELD node,distance WHERE distance=tag RETURN node \
             }} RETURN node }} RETURN tag,node ORDER BY tag"
        );
        let nested = execute(
            &prepare(&nested, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            4,
            WORK,
        )
        .await
        .unwrap();
        assert_eq!(
            nested.rows,
            vec![
                vec![integer(1), vertex(SECOND)],
                vec![integer(2), vertex(THIRD)],
            ]
        );
        assert_eq!(nested.records, 2 * once.records);
        assert_eq!(
            execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 4, WORK)
                .await
                .unwrap(),
            full,
        );
        assert_eq!(spill_pool.used(), 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn repeated_bodies_keep_historical_cuts_and_cannot_hide_reached_errors() {
    let ((), report) = run_async_under_lab(0x6273_5302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, mut writer) = fixture(&commit).await;
        let mut change = WriteBatch::new(R);
        change.add_edge(EId(BASE + 8), VId(THIRD), VId(FOURTH), vec![]);
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(3));
        writer.compact(&commit).await.unwrap();
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let params = parameters();
        let text = format!(
            "UNWIND [1,1] AS tag CALL (tag) {{ {BFS} YIELD node,distance \
             RETURN node,distance }} RETURN tag,node,distance ORDER BY node,tag"
        );
        let prepared = prepare(&text, &params);
        let old = prepared.clone().with_as_of(CommitSeq(2)).unwrap();
        let historic = execute(&old, &mut cold, &cx, policy(), &spill_pool, 4, WORK)
            .await
            .unwrap();
        assert_eq!(historic.snapshot, CommitSeq(2));
        assert_eq!(historic.rows.len(), 6);
        assert!(historic.rows.iter().all(|row| row[1] != vertex(FOURTH)));
        let current = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 4, WORK)
            .await
            .unwrap();
        assert_eq!(current.snapshot, CommitSeq(3));
        assert_eq!(current.rows.len(), 8);
        assert_eq!(
            current
                .rows
                .iter()
                .filter(|row| row[1] == vertex(FOURTH))
                .count(),
            2
        );
        let late = format!(
            "UNWIND [1,0] AS divisor CALL (divisor) {{ {BFS} YIELD node \
             RETURN node,1/divisor AS inverse }} RETURN node,inverse LIMIT 0"
        );
        let error = execute(
            &prepare(&late, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            4,
            WORK,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeSpoolError::SetExecution(
            GraphSetExecutionError::Projection { error, .. }
        ) if error.kind == GraphIntegerErrorKind::DivisionByZero),
            "{error:?}"
        );
        let skipped = "UNWIND [] AS ignored CALL (ignored) { \
            CALL fnx.single_source_shortest_path_length(args => [1/0],relation => 'R') \
            YIELD node RETURN node } RETURN node LIMIT 0";
        let empty = execute(
            &prepare(skipped, &GqlParameters::new()),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            4,
            WORK,
        )
        .await
        .unwrap();
        assert!(empty.rows.is_empty());
        assert_eq!(empty.records, 0);
        let invalid = "UNWIND [] AS ignored CALL (ignored) { \
            CALL fnx.does_not_exist(relation => 'R') YIELD node RETURN node } \
            RETURN node LIMIT 0";
        assert!(
            PreparedNativeRead::prepare(invalid, &GqlParameters::new(), resolve)
                .unwrap()
                .prepare_buffered_sealed_order(&GqlParameters::new(), R, memory(), sealing(), RAW)
                .is_err()
        );
        assert_eq!(spill_pool.used(), 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
