use super::*;

#[test]
fn sorted_future_keeps_the_opening_generation_without_borrowing_the_writer() {
    let ((), report) = run_async_under_lab(0x50a7_0010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = database(&c.commit(), 13).await;
        let opening = db.frontier().unwrap();
        let weak = Arc::downgrade(&db.snapshot);
        let params = GqlParameters::new();
        let prepared = plan();
        let (columns, cursor) = prepared.stream(&db, &cx, &params, policy()).unwrap();
        let mut expected: Vec<_> = cursor.map(|row| row.unwrap()).collect();
        let order = [GraphValueOrder::descending(1), GraphValueOrder::descending(0)];
        expected.sort_by(|a, b| typed_cmp(a, b, &order));
        let expected: Vec<_> = expected.iter().map(|row| row.canonical_bytes().unwrap()).collect();
        let pool = MemoryPool::new(24_000, 0).unwrap();
        let (mut source, _) = scratch(&cx, &pool).await;
        let (mut destination, _) = scratch(&cx, &pool).await;

        let future = prepared.spool_sorted(&db, &cx, &params, policy(), &mut source,
            &mut destination, &order, 2, 32, 101, 4096, u64::MAX);
        // These operations are a compile-time check of the precise capture set.
        drop(prepared);
        drop(params);
        let mut change = crate::WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(1));
        change.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]);
        db.write(&c.commit(), change).await.unwrap();
        drop(db);
        assert!(weak.upgrade().is_some());
        let (sorted, work) = future.await.unwrap();
        assert!(work > 0);
        assert!(weak.upgrade().is_none());
        assert_eq!(sorted.snapshot_seq(), opening);
        assert_eq!(sorted.columns(), columns);
        assert_eq!(contents(&sorted, &mut destination, &cx).await, expected);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn view_sorted_execution_preserves_the_native_page_before_reordering() {
    let ((), report) = run_async_under_lab(0x50a7_0011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = database(&c.commit(), 12).await;
        let at = db.frontier().unwrap();
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(
            &format!("{QUERY} SKIP 3 LIMIT 4"), &params, resolve,
        ).unwrap();
        let (_, cursor) = prepared.stream_in_view(&view, &cx, &params, policy()).unwrap();
        let mut expected: Vec<_> = cursor.map(|row| row.unwrap()).collect();
        assert_eq!(expected.iter().map(|row| row.values()[0].as_vertex().unwrap()).collect::<Vec<_>>(),
            vec![VId(3), VId(4), VId(5), VId(6)]);
        expected.reverse();
        let expected: Vec<_> = expected.iter().map(|row| row.canonical_bytes().unwrap()).collect();
        let mut change = crate::WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(3));
        db.write(&c.commit(), change).await.unwrap();
        let pool = MemoryPool::new(24_000, 0).unwrap();
        let (mut source, _) = scratch(&cx, &pool).await;
        let (mut destination, _) = scratch(&cx, &pool).await;
        let order = [GraphValueOrder::descending(0)];
        let future = prepared.spool_sorted_in_view(&view, &cx, &params, policy(),
            &mut source, &mut destination, &order, 2, 8, 101, 4096, u64::MAX);
        drop(view);
        drop(prepared);
        drop(params);
        drop(db);
        let (sorted, _) = future.await.unwrap();
        assert_eq!(sorted.snapshot_seq(), at);
        assert_eq!(sorted.row_count(), 4);
        assert_eq!(contents(&sorted, &mut destination, &cx).await, expected);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn preflight_and_unpolled_sorted_execution_reserve_no_scratch() {
    let ((), report) = run_async_under_lab(0x50a7_0012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), 8).await;
        let pool = MemoryPool::new(24_000, 0).unwrap();
        let (mut source, a) = scratch(&cx, &pool).await;
        let (mut destination, b) = scratch(&cx, &pool).await;
        let prepared = plan();
        let params = GqlParameters::new();
        let valid = [GraphValueOrder::ascending(0)];
        for (order, run_rows, max_runs, page, units) in [
            (vec![], 2, 8, 101, 100),
            (vec![GraphValueOrder::ascending(3)], 2, 8, 101, 100),
            (valid.to_vec(), 0, 8, 101, 100),
            (valid.to_vec(), 2, 0, 101, 100),
            (valid.to_vec(), 2, 8, 0, 100),
            (valid.to_vec(), 2, 8, 65537, 100),
            (valid.to_vec(), 2, 8, 101, 0),
        ] {
            assert!(prepared.spool_sorted(&db, &cx, &params, policy(), &mut source,
                &mut destination, &order, run_rows, max_runs, page, 4096, units).await.is_err());
        }
        drop(prepared.spool_sorted(&db, &cx, &params, policy(), &mut source,
            &mut destination, &valid, 2, 8, 101, 4096, u64::MAX));
        assert_eq!(source.stats().reserved_runs, 0);
        assert_eq!(destination.stats().reserved_runs, 0);
        for file in [a, b] {
            let state = file.0.lock().unwrap();
            assert_eq!((state.reads, state.writes), (0, 0));
        }
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_preparation_and_late_query_refusals_are_not_retried_as_sorting() {
    let ((), report) = run_async_under_lab(0x50a7_0013, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), 8).await;
        let pool = MemoryPool::new(24_000, 0).unwrap();
        let params = GqlParameters::new();
        let order = [GraphValueOrder::ascending(0)];
        let aggregate = PreparedNativeRead::prepare(
            "MATCH (n:L) RETURN COUNT(*) AS count", &params, resolve,
        ).unwrap();
        let (mut source, _) = scratch(&cx, &pool).await;
        let (mut destination, _) = scratch(&cx, &pool).await;
        let error = aggregate.spool_sorted(&db, &cx, &params, policy(), &mut source,
            &mut destination, &order, 2, 8, 101, 4096, u64::MAX).await.unwrap_err();
        assert!(error.prepare_error().is_some());
        assert_eq!(source.stats().reserved_runs, 0);
        assert_eq!(destination.stats().reserved_runs, 0);

        let error = plan().spool_sorted(&db, &cx, &params,
            GqlQueryPolicy::new(10_000, 1, 100_000_000, 1_000_000),
            &mut source, &mut destination, &order, 2, 8, 101, 4096, u64::MAX).await.unwrap_err();
        assert!(error.execution_error().is_some());
        assert!(source.is_poisoned());
        assert_eq!(source.stats().published_runs, 0);
        assert_eq!(destination.stats().reserved_runs, 0);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sorting_releases_the_native_pin_before_a_pending_merge_read() {
    let ((), report) = run_async_under_lab(0x50a7_0014, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), 7).await;
        let weak = Arc::downgrade(&db.snapshot);
        let pool = MemoryPool::new(24_000, 0).unwrap();
        let (mut source, observer) = scratch(&cx, &pool).await;
        let (mut destination, _) = scratch(&cx, &pool).await;
        // Drain writes succeed. The FIRST scratch read is the sort's input,
        // after drain's explicit source-cursor drop and root publication.
        observer.0.lock().unwrap().pending_read = true;
        let order = [GraphValueOrder::ascending(1)];
        let future = plan().spool_sorted(&db, &cx, &GqlParameters::new(), policy(),
            &mut source, &mut destination, &order, 2, 8, 101, 4096, u64::MAX);
        drop(db);
        assert!(weak.upgrade().is_some());
        let mut future = Box::pin(future);
        assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        assert!(weak.upgrade().is_none());
        drop(future);
        assert!(source.is_poisoned());
        assert_eq!(source.stats().published_runs, 1);
        assert_eq!(destination.stats().reserved_runs, 0);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn integrated_and_explicit_spool_sort_share_results_and_logical_allowance() {
    let ((), report) = run_async_under_lab(0x50a7_0015, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), 9).await;
        let pool = MemoryPool::new(24_000, 0).unwrap();
        let (mut source, _) = scratch(&cx, &pool).await;
        let (mut destination, _) = scratch(&cx, &pool).await;
        let order = [GraphValueOrder::ascending(1).with_nulls_first(true)];
        let spool = plan().spool(&db, &cx, &GqlParameters::new(), policy(), &mut source, 101, 4096)
            .await.unwrap();
        let (sorted, used) = spool.sort_into(&cx, &mut source, &mut destination, &order,
            3, 8, 101, u64::MAX).await.unwrap();
        let expected = contents(&sorted, &mut destination, &cx).await;
        let (mut source, _) = scratch(&cx, &pool).await;
        let (mut destination, _) = scratch(&cx, &pool).await;
        let (actual, actual_used) = plan().spool_sorted(&db, &cx, &GqlParameters::new(),
            policy(), &mut source, &mut destination, &order, 3, 8, 101, 4096, used)
            .await.unwrap();
        assert_eq!(actual_used, used);
        assert_eq!(actual.row_stats(), sorted.row_stats());
        assert_eq!(actual.evaluator_stats(), sorted.evaluator_stats());
        assert_eq!(contents(&actual, &mut destination, &cx).await, expected);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
