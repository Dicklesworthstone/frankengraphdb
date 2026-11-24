//! Scoped FnX leaves compose through the ordinary external relation owner.
use super::*;

const CORE: &str = "CALL fnx.core_number({relations:['R'],direction:'undirected',\
                   parallel:'collapse',self_loops:'drop'}) YIELD core,vertex AS node";
const BFS: &str = "fnx.single_source_shortest_path_length({relations:['R'],\
                  direction:'directed',parallel:'collapse',self_loops:'drop'}";

#[test]
fn fnx_ordered_rows_match_native_scoped_topology_and_mask_carried_fields() {
    let ((), report) = run_async_under_lab(0x860c_0001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut full = projected_collection_database(&c.commit(), true).await;
        let slim = projected_collection_database(&c.commit(), false).await;
        let mut hidden = WriteBatch::new(RelationId(1));
        hidden.add_edge(EId(15), VId(99), VId(3), vec![]);
        full.write(&c.commit(), hidden).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let text = format!(
            "{CORE} RETURN core,node,node.p AS value,node.hidden AS secret ORDER BY core,node"
        );
        // The visible graph is a three-vertex path and two isolates. The
        // hidden endpoint closes a four-cycle only in the unmasked graph.
        let expected = [(4, 0, -1), (5, 0, 2), (1, 1, 3), (2, 1, 1), (3, 1, 3)]
            .map(|(id, core, p)| vec![scalar(core), GraphValue::Vertex(VId(id)), scalar(p), null()])
            .to_vec();
        for graph in [&full, &slim] {
            let mut reference = graph
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
                .unwrap();
            let prepared = reference.prepare(&cx, &text, &params).unwrap();
            assert_eq!(
                reference.execute(&cx, &prepared, &params).unwrap(),
                QueryResult::Rows {
                    columns: ["core", "node", "value", "secret"]
                        .map(str::to_owned)
                        .to_vec(),
                    rows: expected
                        .iter()
                        .map(|row| row.iter().cloned().map(QueryValue::Value).collect())
                        .collect(),
                }
            );
            let physical = AuthorizedSortLimits {
                run_rows: 1,
                ..limits()
            };
            let (mut ordered, _, _) =
                session(graph, &cx, &issuer, &token, &pool, || 100, physical).await;
            let prepared = ordered.prepare(&cx, &text, &params).unwrap();
            let future = ordered.execute_ordered(&cx, &prepared, &params).unwrap();
            fn require_send<T: Send>(_: &T) {}
            require_send(&future);
            let mut cursor = future.await.unwrap();
            require_send(&cursor);
            assert_eq!(values(&mut cursor).await, expected);
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn repeated_fnx_bodies_evaluate_imported_properties_and_share_signed_nodes() {
    let ((), report) = run_async_under_lab(0x860c_0002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let text = format!(
            "MATCH (root:L {{p:1}}) UNWIND [0,0] AS occurrence CALL (root) {{ \
             CALL {BFS},root,root.p) YIELD vertex AS node,distance \
             RETURN node,distance }} RETURN node,distance ORDER BY node,distance"
        );
        let prepared = ordered.prepare(&cx, &text, &params).unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(
            values(&mut cursor).await,
            vec![
                vec![GraphValue::Vertex(VId(2)), scalar(0)],
                vec![GraphValue::Vertex(VId(2)), scalar(0)],
                vec![GraphValue::Vertex(VId(3)), scalar(1)],
                vec![GraphValue::Vertex(VId(3)), scalar(1)],
            ]
        );
        drop(cursor);
        assert_eq!(pool.used(), 0);

        // Each reached procedure admits the same five visible vertices. A
        // second Apply occurrence cannot obtain another signed node allowance.
        for (nodes, succeeds) in [(9, false), (10, true)] {
            let mut allowed = grant();
            allowed.limits.max_nodes = nodes;
            allowed.limits.max_rows = 1;
            let token = issuer.issue_at(&allowed, 100).unwrap();
            let (mut ordered, _, _) =
                session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
            let text = format!(
                "UNWIND [1,1] AS invocation CALL (invocation) {{ {CORE} RETURN node }} \
                 RETURN node ORDER BY node LIMIT 1"
            );
            let prepared = ordered.prepare(&cx, &text, &params).unwrap();
            let result = ordered
                .execute_ordered(&cx, &prepared, &params)
                .unwrap()
                .await;
            if succeeds {
                let mut cursor = result.unwrap();
                assert_eq!(
                    values(&mut cursor).await,
                    vec![vec![GraphValue::Vertex(VId(1))]]
                );
            } else {
                refusal(
                    result,
                    AuthorizationError::LimitExceeded(LimitDimension::Nodes),
                );
            }
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn fnx_static_preflight_visits_inactive_bodies_and_all_siblings_before_io() {
    let ((), report) = run_async_under_lab(0x860c_0003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let mut allowed = grant();
        allowed.limits.max_nodes = 0;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, scratch, destination) =
            session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        for text in [
            "UNWIND [] AS unused CALL (unused) { \
             CALL fnx.connected_components({relations:['R'],direction:'directed',\
             parallel:'collapse',self_loops:'drop'}) YIELD vertex AS node,component \
             RETURN node } RETURN node LIMIT 0"
                .to_owned(),
            format!(
                "{CORE} RETURN node UNION ALL \
                 CALL hybrid.search() YIELD vertex AS node RETURN node LIMIT 0"
            ),
            "UNWIND [] AS unused CALL (unused) { \
             CALL fnx.no_such_kernel() YIELD vertex AS node RETURN node } \
             RETURN node LIMIT 0"
                .to_owned(),
        ] {
            let prepared = ordered.prepare(&cx, &text, &params).unwrap();
            assert!(
                matches!(
                    ordered.execute_ordered(&cx, &prepared, &params),
                    Err(NativeSpoolError::Authorized(error))
                        if matches!(*error, QueryError::StreamingUnsupported { .. })
                ),
                "{text}"
            );
            assert!(!ordered.is_closed());
            assert!(scratch.0.lock().unwrap().bytes.get_ref().is_empty());
            assert!(destination.0.lock().unwrap().bytes.get_ref().is_empty());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn fnx_arguments_stay_lazy_but_reached_errors_survive_limit_zero() {
    let ((), report) = run_async_under_lab(0x860c_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let mut allowed = grant();
        allowed.limits.max_nodes = 0;
        let token = issuer.issue_at(&allowed, 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        for active in [false, true] {
            let rows = if active { "[1]" } else { "[]" };
            let text = format!(
                "UNWIND {rows} AS input CALL (input) {{ \
                 CALL {BFS},1/(input-input)) YIELD vertex AS node,distance \
                 RETURN node }} RETURN node LIMIT 0"
            );
            let prepared = ordered.prepare(&cx, &text, &params).unwrap();
            let result = ordered
                .execute_ordered(&cx, &prepared, &params)
                .unwrap()
                .await;
            if active {
                assert!(matches!(result.err(),
                    Some(NativeSpoolError::Authorized(error)) if matches!(*error,
                        QueryError::Set(GqlQueryError::Source(
                            GraphSetExecutionError::Projection { error, .. }
                        )) if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
                    )
                ));
            } else {
                let mut cursor = result.unwrap();
                assert!(values(&mut cursor).await.is_empty());
            }
            assert!(!ordered.is_closed());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancelling_pending_fnx_spool_drops_kernel_rows_and_affine_reservations() {
    let ((), report) = run_async_under_lab(0x860c_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let cancel_root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let query_contexts = PurposeContexts::narrow_runtime_root(&cancel_root);
        let cx = query_contexts.query();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, scratch, destination) =
            session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let text = format!("{CORE} RETURN node,core ORDER BY node");
        let prepared = ordered.prepare(&cx, &text, &params).unwrap();
        // Source selection alternates scratch roles. Pause either backend so
        // the first retained FnX row, before final delivery, witnesses cleanup.
        scratch.0.lock().unwrap().pending_write = true;
        destination.0.lock().unwrap().pending_write = true;
        let mut future = Box::pin(ordered.execute_ordered(&cx, &prepared, &params).unwrap());
        let mut context = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut context).is_pending());
        assert!(pool.used() > 0);
        cancel_root.set_cancel_requested(true);
        let Poll::Ready(result) = future.as_mut().poll(&mut context) else {
            panic!("cancelled FnX spool stayed pending");
        };
        assert!(matches!(result.err(),
            Some(NativeSpoolError::Authorized(error))
                if matches!(*error, QueryError::Pattern(GqlQueryError::Interrupted(_)))
        ));
        drop(future);
        assert!(!ordered.is_closed());
        assert!(ordered.scratch.is_poisoned() || ordered.destination.is_poisoned());
        assert_eq!(pool.used(), 0);
        cancel_root.set_cancel_requested(false);
        assert!(ordered.prepare(&cx, &text, &params).is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
