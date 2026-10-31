//! Repeated analytics use the same complete-body and cumulative-budget owner.
use super::*;

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
        let single = prepare(&format!("{BFS} YIELD node,distance RETURN node,distance"), &params);
        let once = execute(&single, &mut cold, &cx, policy(), &spill_pool, 4, WORK)
            .await.unwrap();
        let full = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 4, WORK)
            .await.unwrap();
        let expected = [
            (1, SECOND, 1), (1, SECOND, 1), (1, THIRD, 2),
            (1, THIRD, 2), (2, THIRD, 2),
        ].map(|(cutoff, id, distance)| vec![integer(cutoff), vertex(id), integer(distance)])
         .to_vec();
        assert_eq!(full.rows, expected);
        assert_eq!(full.records, 3 * once.records);
        assert_eq!(
            resident_rows(writer.query(&cx, &text, &params, resolve, policy()).unwrap()),
            expected,
        );
        assert_eq!(
            execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 4, WORK).await.unwrap(),
            full,
        );
        let limited = execute(
            &prepare(&format!("{text} LIMIT 2"), &params), &mut cold, &cx,
            GqlQueryPolicy::new(10_000, 2, WORK, WORK), &spill_pool, 4, WORK,
        ).await.unwrap();
        assert_eq!(limited.rows.as_slice(), &expected[..2]);
        assert_eq!(limited.records, full.records);
        for (allowance, physical) in [
            (GqlQueryPolicy::new(full.records - 1, 100, WORK, WORK), WORK),
            (GqlQueryPolicy::new(10_000, 100, full.evaluator_work - 1, WORK), WORK),
            (GqlQueryPolicy::new(10_000, 100, WORK, full.evaluator_scratch - 1), WORK),
            (policy(), full.sort_work - 1),
        ] {
            assert!(execute(&prepared, &mut cold, &cx, allowance, &spill_pool, 4, physical)
                .await.is_err());
            assert_eq!(spill_pool.used(), 0);
        }
        let nested = format!(
            "UNWIND [1,2] AS tag CALL (tag) {{ CALL (tag) {{ \
             {BFS} YIELD node,distance WHERE distance=tag RETURN node \
             }} RETURN node }} RETURN tag,node ORDER BY tag"
        );
        let nested = execute(&prepare(&nested, &params), &mut cold, &cx,
            policy(), &spill_pool, 4, WORK).await.unwrap();
        assert_eq!(nested.rows, vec![
            vec![integer(1), vertex(SECOND)], vec![integer(2), vertex(THIRD)],
        ]);
        assert_eq!(nested.records, 2 * once.records);
        assert_eq!(
            execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 4, WORK).await.unwrap(),
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
            .await.unwrap();
        assert_eq!(historic.snapshot, CommitSeq(2));
        assert_eq!(historic.rows.len(), 6);
        assert!(historic.rows.iter().all(|row| row[1] != vertex(FOURTH)));
        let current = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 4, WORK)
            .await.unwrap();
        assert_eq!(current.snapshot, CommitSeq(3));
        assert_eq!(current.rows.len(), 8);
        assert_eq!(current.rows.iter().filter(|row| row[1] == vertex(FOURTH)).count(), 2);
        let late = format!(
            "UNWIND [1,0] AS divisor CALL (divisor) {{ {BFS} YIELD node \
             RETURN node,1/divisor AS inverse }} RETURN node,inverse LIMIT 0"
        );
        let error = execute(&prepare(&late, &params), &mut cold, &cx,
            policy(), &spill_pool, 4, WORK).await.unwrap_err();
        assert!(matches!(&error, NativeSpoolError::SetExecution(
            GraphSetExecutionError::Projection { error, .. }
        ) if error.kind == GraphIntegerErrorKind::DivisionByZero), "{error:?}");
        let skipped = "UNWIND [] AS ignored CALL (ignored) { \
            CALL fnx.single_source_shortest_path_length(args => [1/0],relation => 'R') \
            YIELD node RETURN node } RETURN node LIMIT 0";
        let empty = execute(&prepare(skipped, &GqlParameters::new()), &mut cold, &cx,
            policy(), &spill_pool, 4, WORK).await.unwrap();
        assert!(empty.rows.is_empty());
        assert_eq!(empty.records, 0);
        let invalid = "UNWIND [] AS ignored CALL (ignored) { \
            CALL fnx.does_not_exist(relation => 'R') YIELD node RETURN node } \
            RETURN node LIMIT 0";
        assert!(PreparedNativeRead::prepare(invalid, &GqlParameters::new(), resolve).unwrap()
            .prepare_buffered_sealed_order(&GqlParameters::new(), R, memory(), sealing(), RAW)
            .is_err());
        assert_eq!(spill_pool.used(), 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
