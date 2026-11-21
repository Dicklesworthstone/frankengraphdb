//! Exact terminal groups share the ordered session's authority and scratch.
use super::*;
use crate::NativeAggregateSpoolError;
use fgdb_gql::GraphAggregateValue::{Average, Count, Integer, Value};
use fgdb_gql::{GraphAggregateTextSlot, GraphAggregateValue, GraphExactAverage};

#[allow(clippy::too_many_arguments)]
async fn observed_session<'a, C: FnMut() -> u64 + Send>(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    authority: &'a Authority,
    token: &CapabilityToken,
    pool: &MemoryPool,
    clock: C,
    limits: AuthorizedSortLimits,
) -> (Session<'a, C>, [File; 3]) {
    let (scratch, a) = file(cx, pool).await;
    let (destination, b) = file(cx, pool).await;
    let (auxiliary, c) = file(cx, pool).await;
    let session = db
        .authorized_ordered_read_session(
            cx,
            authority,
            token,
            "main",
            symbols as Resolver,
            policy(),
            clock,
            scratch,
            destination,
            auxiliary,
            limits,
        )
        .unwrap();
    (session, [a, b, c])
}

async fn aggregate_values(
    cursor: &mut AuthorizedAggregateSpoolCursor<'_, File>,
) -> Vec<Vec<GraphAggregateValue>> {
    let slots = cursor.output_slots().to_vec();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next_row().await.unwrap() {
        rows.push(
            slots
                .iter()
                .map(|slot| match *slot {
                    GraphAggregateTextSlot::GroupKey(at) => Value(row.keys()[at].clone()),
                    GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                })
                .collect(),
        );
    }
    assert_eq!(cursor.state(), ScanState::Exhausted);
    rows
}

fn list(values: &[i64]) -> GraphAggregateValue {
    Value(GraphValue::List(
        values.iter().copied().map(scalar).collect(),
    ))
}

#[test]
fn terminal_groups_keep_exact_numeric_cells_scoped_inputs_and_collection_order() {
    let ((), report) = run_async_under_lab(0x860a_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        for (text, expected) in [
            (
                "MATCH (n:L) RETURN COUNT(*) AS rows,SUM(n.p) AS total,AVG(n.p) AS mean,COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS unique_items,COUNT(n.hidden) AS hidden,COLLECT(n.hidden) AS hidden_items",
                vec![
                    Count(5),
                    Integer(8),
                    Average(GraphExactAverage::new(8, 5).unwrap()),
                    list(&[3, 1, 3, -1, 2]),
                    list(&[3, 1, -1, 2]),
                    Count(0),
                    list(&[]),
                ],
            ),
            (
                "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS rows,SUM(r.p) AS total,AVG(r.p) AS mean,COUNT(r.hidden) AS hidden",
                vec![
                    Count(3),
                    Integer(21),
                    Average(GraphExactAverage::new(7, 1).unwrap()),
                    Count(0),
                ],
            ),
            (
                "UNWIND [9223372036854775807,9223372036854775807,2] AS x RETURN COUNT(*) AS rows,SUM(x) AS total,AVG(x) AS mean",
                vec![
                    Count(3),
                    Integer(1_i128 << 64),
                    Average(GraphExactAverage::new(1_i128 << 64, 3).unwrap()),
                ],
            ),
        ] {
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            let future = ordered
                .execute_ordered_aggregate(&cx, &prepared, &params)
                .unwrap();
            fn assert_send<T: Send>(_: &T) {}
            assert_send(&future);
            let mut cursor = future.await.unwrap();
            assert_send(&cursor);
            assert_eq!(
                aggregate_values(&mut cursor).await,
                vec![expected],
                "{text}"
            );
            drop(cursor);
            assert_eq!(pool.used(), 0, "{text}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn terminal_group_final_clauses_and_empty_inputs_use_the_complete_relation() {
    let ((), report) = run_async_under_lab(0x860a_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        // Four groups exceed group_capacity=2. HAVING, hidden ORDER operands,
        // DISTINCT and final paging must operate after complete grouping.
        for (text, expected) in [
            (
                "MATCH (n:L) RETURN n.p AS p,COUNT(*) AS rows GROUP BY n.p HAVING SUM(n.p)>0 ORDER BY rows DESC,p ASC SKIP 1 LIMIT 2",
                vec![
                    vec![Value(scalar(1)), Count(1)],
                    vec![Value(scalar(2)), Count(1)],
                ],
            ),
            (
                "MATCH (n:L) RETURN DISTINCT COUNT(*) AS rows GROUP BY n.p ORDER BY SUM(n.p) DESC",
                vec![vec![Count(2)], vec![Count(1)]],
            ),
            (
                "MATCH (n:L) WHERE n.p>100 RETURN COUNT(*) AS rows,SUM(n.p) AS total,AVG(n.p) AS mean,COLLECT(n.p) AS items",
                vec![vec![Count(0), Value(null()), Value(null()), list(&[])]],
            ),
            (
                "MATCH (n:L) WHERE n.p>100 RETURN n.p AS p,COUNT(*) AS rows GROUP BY n.p",
                vec![],
            ),
        ] {
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            let mut cursor = ordered
                .execute_ordered_aggregate(&cx, &prepared, &params)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(aggregate_values(&mut cursor).await, expected, "{text}");
            drop(cursor);
            assert_eq!(pool.used(), 0, "{text}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn terminal_groups_pin_history_and_scope_carried_element_properties() {
    let ((), report) = run_async_under_lab(0x860a_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let old_params = GqlParameters::new().with_uint64("cut", 1).unwrap();
        let new_params = GqlParameters::new().with_uint64("cut", 2).unwrap();
        let text = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut WITH n AS item RETURN COUNT(*) AS rows,SUM(item.p) AS total,COUNT(item.hidden) AS hidden";
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = ordered.prepare(&cx, text, &old_params).unwrap();
        let mut cursor = ordered
            .execute_ordered_aggregate(&cx, &prepared, &old_params)
            .unwrap()
            .await
            .unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(30)));
        db.write(&contexts.commit(), change).await.unwrap();
        assert_eq!(cursor.snapshot_seq(), CommitSeq(1));
        assert_eq!(
            aggregate_values(&mut cursor).await,
            vec![vec![Count(5), Integer(8), Count(0)]]
        );
        drop(cursor);
        assert!(
            ordered
                .execute_ordered_aggregate(&cx, &prepared, &new_params)
                .is_err()
        );
        assert!(!ordered.is_closed());
        let (mut fresh, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let fresh_prepared = fresh.prepare(&cx, text, &old_params).unwrap();
        for (params, cut, total) in [(&new_params, 2, 35), (&old_params, 1, 8)] {
            let mut cursor = fresh
                .execute_ordered_aggregate(&cx, &fresh_prepared, params)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            assert_eq!(
                aggregate_values(&mut cursor).await,
                vec![vec![Count(5), Integer(total), Count(0)]]
            );
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_admission_and_unpolled_drop_do_not_touch_scratch_or_reserve_memory() {
    let ((), report) = run_async_under_lab(0x860a_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, files) =
            observed_session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let mut foreign = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let other = foreign
            .prepare(&cx, "MATCH (n:L) RETURN COUNT(*) AS rows", &params)
            .unwrap();
        refusal(
            ordered.execute_ordered_aggregate(&cx, &other, &params),
            AuthorizationError::WrongAuthority,
        );
        // Completed output groups have no privileged graph reader. This must
        // refuse before I/O even when a final empty window hides every output.
        let lookup = ordered
            .prepare(
                &cx,
                "MATCH (n:L) RETURN COLLECT(n)[0].p AS p LIMIT 0",
                &params,
            )
            .unwrap();
        assert!(
            matches!(ordered.execute_ordered_aggregate(&cx, &lookup, &params),
            Err(NativeSpoolError::Authorized(error)) if matches!(*error, QueryError::StreamingUnsupported { .. }))
        );
        let prepared = ordered
            .prepare(&cx, "MATCH (n:L) RETURN COUNT(*) AS rows", &params)
            .unwrap();
        let future = ordered
            .execute_ordered_aggregate(&cx, &prepared, &params)
            .unwrap();
        assert_eq!(future.columns(), &["rows"]);
        assert_eq!(future.snapshot_seq(), CommitSeq(1));
        drop(future);
        for file in &files {
            assert!(file.0.lock().unwrap().bytes.get_ref().is_empty());
        }
        assert_eq!(pool.used(), 0);
        assert!(!ordered.is_closed());
        let mut cursor = ordered
            .execute_ordered_aggregate(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(aggregate_values(&mut cursor).await, vec![vec![Count(5)]]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn limit_zero_evaluates_reached_inputs_and_complete_group_outputs() {
    let ((), report) = run_async_under_lab(0x860a_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let input = ordered
            .prepare(
                &cx,
                "UNWIND [1,0] AS x WITH 1/x AS quotient RETURN SUM(quotient) AS total LIMIT 0",
                &params,
            )
            .unwrap();
        let error = ordered
            .execute_ordered_aggregate(&cx, &input, &params)
            .unwrap()
            .await
            .unwrap_err();
        assert!(matches!(error, NativeSpoolError::Authorized(error)
            if matches!(*error, QueryError::Set(GqlQueryError::Source(
                fgdb_gql::GraphSetExecutionError::Projection { error, .. }))
                if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)));
        assert_eq!(pool.used(), 0);
        assert!(!ordered.is_closed());
        for text in [
            "UNWIND [1,2] AS x RETURN 1/(COUNT(*)-2) AS value LIMIT 0",
            "MATCH (n:L) RETURN 1/(SUM(n.p)-8) AS value LIMIT 0",
        ] {
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            let error = ordered
                .execute_ordered_aggregate(&cx, &prepared, &params)
                .unwrap()
                .await
                .unwrap_err();
            assert!(
                matches!(error, NativeSpoolError::Aggregate(error)
                if matches!(*error, NativeAggregateSpoolError::Execute(error)
                    if matches!(*error, GqlQueryError::Source(fgdb_gql::GraphAggregateError::OutputExpression { error, .. })
                        if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero))),
                "{text}"
            );
            assert!(!ordered.is_closed());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn terminal_rows_are_charged_on_delivery_and_child_budget_errors_stay_typed() {
    let ((), report) = run_async_under_lab(0x860a_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let mut scope = grant();
        scope.limits.max_rows = 1;
        let token = issuer.issue_at(&scope, 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let global = ordered
            .prepare(&cx, "MATCH (n:L) RETURN COUNT(*) AS rows", &params)
            .unwrap();
        let mut cursor = ordered
            .execute_ordered_aggregate(&cx, &global, &params)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(aggregate_values(&mut cursor).await, vec![vec![Count(5)]]);
        drop(cursor);
        let grouped = ordered
            .prepare(
                &cx,
                "MATCH (n:L) RETURN n.p AS p,COUNT(*) AS rows GROUP BY n.p ORDER BY p",
                &params,
            )
            .unwrap();
        let mut cursor = ordered
            .execute_ordered_aggregate(&cx, &grouped, &params)
            .unwrap()
            .await
            .unwrap();
        let first = cursor.next_row().await.unwrap().unwrap();
        assert_eq!(first.keys(), &[scalar(-1)]);
        assert_eq!(first.values(), &[Count(1)]);
        drop(first);
        refusal(
            cursor.next_row().await,
            AuthorizationError::LimitExceeded(LimitDimension::Rows),
        );
        assert_eq!(cursor.state(), ScanState::Failed);
        assert!(cursor.next_row().await.unwrap().is_none());
        drop(cursor);
        assert!(!ordered.is_closed());
        assert_eq!(pool.used(), 0);
        let mut bounded = limits();
        bounded.max_input_rows = 2;
        let (mut blocked, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, bounded).await;
        let prepared = blocked
            .prepare(
                &cx,
                "UNWIND [1,2] AS x UNWIND [3,4] AS y RETURN COUNT(*) AS rows LIMIT 0",
                &params,
            )
            .unwrap();
        let error = blocked
            .execute_ordered_aggregate(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap_err();
        assert!(matches!(error, NativeSpoolError::Authorized(error)
            if matches!(*error, QueryError::Set(GqlQueryError::Rows(_)))));
        assert!(!blocked.is_closed());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_rows_hold_memory_and_pending_read_drop_fuses_only_the_cursor() {
    let ((), report) = run_async_under_lab(0x860a_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, _, b) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n:L) RETURN n.p AS p,COLLECT(n) AS nodes GROUP BY n.p ORDER BY p",
                &params,
            )
            .unwrap();
        let mut cursor = ordered
            .execute_ordered_aggregate(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        let row = cursor.next_row().await.unwrap().unwrap();
        assert!(pool.used() > 0);
        cursor.close();
        drop(cursor);
        assert!(
            pool.used() > 0,
            "returned row keeps its own decoding reservation"
        );
        drop(row);
        assert_eq!(pool.used(), 0);
        let mut cursor = ordered
            .execute_ordered_aggregate(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        b.0.lock().unwrap().pending_read = true;
        let mut read = Box::pin(cursor.next_row());
        assert!(
            read.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(read);
        assert_eq!(cursor.state(), ScanState::Failed);
        assert!(cursor.next_row().await.unwrap().is_none());
        cursor.close();
        drop(cursor);
        assert!(!ordered.is_closed());
        assert!(ordered.destination.is_poisoned());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_expiry_interrupts_pending_writes_and_rechecks_final_row_reads() {
    let ((), report) = run_async_under_lab(0x860a_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let pins = Arc::strong_count(&db.snapshot);
        let now = Arc::new(AtomicU64::new(100));
        let sampled = Arc::clone(&now);
        let (mut ordered, files) = observed_session(
            &db,
            &cx,
            &issuer,
            &token,
            &pool,
            move || sampled.load(Ordering::SeqCst),
            limits(),
        )
        .await;
        let prepared = ordered
            .prepare(&cx, "MATCH (n:L) RETURN COUNT(*) AS rows", &params)
            .unwrap();
        for file in &files {
            file.0.lock().unwrap().pending_write = true;
        }
        let mut future = Box::pin(
            ordered
                .execute_ordered_aggregate(&cx, &prepared, &params)
                .unwrap(),
        );
        let mut context = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut context).is_pending());
        now.store(1001, Ordering::SeqCst);
        assert!(matches!(future.as_mut().poll(&mut context),
            Poll::Ready(Err(NativeSpoolError::Authorized(error)))
            if matches!(*error, QueryError::Authorization(AuthorizationError::Expired))));
        drop(future);
        assert!(ordered.is_closed());
        assert_eq!(Arc::strong_count(&db.snapshot), pins);
        assert_eq!(pool.used(), 0);
        now.store(100, Ordering::SeqCst);
        let sampled = Arc::clone(&now);
        let (mut fresh, _, b) = session(
            &db,
            &cx,
            &issuer,
            &token,
            &pool,
            move || sampled.load(Ordering::SeqCst),
            limits(),
        )
        .await;
        let prepared = fresh
            .prepare(&cx, "MATCH (n:L) RETURN COUNT(*) AS rows", &params)
            .unwrap();
        let mut cursor = fresh
            .execute_ordered_aggregate(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        b.0.lock().unwrap().expire_after_read = Some(Arc::clone(&now));
        refusal(cursor.next_row().await, AuthorizationError::Expired);
        assert_eq!(cursor.state(), ScanState::Failed);
        assert!(cursor.next_row().await.unwrap().is_none());
        drop(cursor);
        assert!(fresh.is_closed());
        assert_eq!(Arc::strong_count(&db.snapshot), pins);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
