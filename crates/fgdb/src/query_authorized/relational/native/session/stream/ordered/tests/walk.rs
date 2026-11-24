//! Authorized returned paths retain native occurrence, scope and spill laws.
use super::*;
use fgdb_gql::algebra::GraphPath;
use fgdb_gql::{GraphAggregateTextSlot, GraphAggregateValue};

fn route(start: u128, steps: &[(u128, u128)]) -> Vec<GraphValue> {
    let end = steps.last().map_or(start, |step| step.1);
    let value = match end {
        1 | 3 => 3,
        2 => 1,
        4 => -1,
        5 => 2,
        _ => unreachable!(),
    };
    vec![
        GraphValue::Vertex(VId(start)),
        GraphValue::Vertex(VId(end)),
        GraphValue::Path(GraphPath::new(
            VId(start),
            steps
                .iter()
                .map(|&(edge, to)| (EId(edge), VId(to)))
                .collect(),
        )),
        scalar(value),
        null(),
    ]
}

fn routes(any_shortest: bool) -> Vec<Vec<GraphValue>> {
    // Five real zero-hop roots (including two isolates), two parallel edges,
    // and their two routes to vertex three. This is independent of either
    // evaluator; ANY chooses the smaller real edge identity in each tie.
    let mut rows: Vec<_> = (1..=5).map(|id| route(id, &[])).collect();
    rows.extend([
        route(1, &[(10, 2)]),
        route(1, &[(10, 2), (12, 3)]),
        route(2, &[(12, 3)]),
    ]);
    if !any_shortest {
        rows.extend([route(1, &[(11, 2)]), route(1, &[(11, 2), (12, 3)])]);
    }
    rows.sort();
    rows
}

#[test]
fn scoped_paths_keep_real_routes_zero_hops_shortest_ties_and_hidden_transcripts() {
    let ((), report) = run_async_under_lab(0x860b_0001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = projected_collection_database(&commit, true).await;
        let slim = projected_collection_database(&commit, false).await;
        let mut hidden = WriteBatch::new(RelationId(1));
        hidden.add_edge(EId(15), VId(99), VId(3), vec![]);
        db.write(&commit, hidden).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let mut reference = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        for (mode, bounds, any_shortest) in [
            ("WALK", "0..2", false),
            ("TRAIL", "0..", false),
            ("ACYCLIC", "0..", false),
            ("SIMPLE", "0..", false),
            ("ANY SHORTEST", "0..", true),
            ("ALL SHORTEST", "0..", false),
        ] {
            let text = format!(
                "MATCH REPEATABLE ELEMENTS p = {mode} (a:L)-[:R*{bounds}]->(b:L) \
                 RETURN a,b,p,b.p AS value,b.hidden AS secret ORDER BY a,b,p"
            );
            let expected = routes(any_shortest);
            let prepared = reference.prepare(&cx, &text, &params).unwrap();
            assert_eq!(
                reference.execute(&cx, &prepared, &params).unwrap(),
                QueryResult::Rows {
                    columns: ["a", "b", "p", "value", "secret"]
                        .map(str::to_owned)
                        .to_vec(),
                    rows: expected
                        .iter()
                        .map(|row| row.iter().cloned().map(QueryValue::Value).collect())
                        .collect(),
                },
                "{text}",
            );
            let mut prior = None;
            for graph in [&db, &slim] {
                let calls = AtomicU64::new(0);
                let clock = || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    100
                };
                let physical = AuthorizedSortLimits {
                    run_rows: 1,
                    ..limits()
                };
                let (mut ordered, scratch, destination) =
                    session(graph, &cx, &issuer, &token, &pool, clock, physical).await;
                let prepared = ordered.prepare(&cx, &text, &params).unwrap();
                let before = calls.load(Ordering::Relaxed);
                let future = ordered.execute_ordered(&cx, &prepared, &params).unwrap();
                fn assert_send<T: Send>(_: &T) {}
                assert_send(&future);
                let mut cursor = future.await.unwrap();
                assert_send(&cursor);
                assert_eq!(values(&mut cursor).await, expected, "{text}");
                let cost = calls.load(Ordering::Relaxed) - before;
                if let Some(prior) = prior {
                    assert_eq!(
                        cost, prior,
                        "hidden graph changed the signed transcript: {text}"
                    );
                }
                prior = Some(cost);
                drop(cursor);
                assert!(!scratch.0.lock().unwrap().bytes.get_ref().is_empty());
                assert!(!destination.0.lock().unwrap().bytes.get_ref().is_empty());
                assert_eq!(pool.used(), 0, "{text}");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn path_relations_apply_distinct_windows_and_exact_terminal_aggregates_after_spilling() {
    let ((), report) = run_async_under_lab(0x860b_0002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let physical = AuthorizedSortLimits {
            run_rows: 1,
            group_capacity: 1,
            ..limits()
        };
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, physical).await;
        for text in [
            "MATCH (a:L)-[:R*0..2]->(b:L) RETURN DISTINCT a,b ORDER BY a,b LIMIT 3",
            "MATCH (a:L)-[:R*0..2]->(b:L) WITH a,b RETURN DISTINCT a,b ORDER BY a,b LIMIT 3",
        ] {
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            let mut cursor = ordered
                .execute_ordered(&cx, &prepared, &params)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(
                values(&mut cursor).await,
                [1, 2, 3]
                    .map(|id| vec![GraphValue::Vertex(VId(1)), GraphValue::Vertex(VId(id))])
                    .to_vec(),
                "{text}",
            );
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
        use GraphAggregateValue::{Count, Integer, Value};
        for (text, expected) in [
            (
                "MATCH (a:L)-[:R*1..2]->(b:L) RETURN b,COUNT(*) AS routes,SUM(b.p) AS total,COUNT(b.hidden) AS secret ORDER BY b",
                vec![
                    vec![
                        Value(GraphValue::Vertex(VId(2))),
                        Count(2),
                        Integer(2),
                        Count(0),
                    ],
                    vec![
                        Value(GraphValue::Vertex(VId(3))),
                        Count(3),
                        Integer(9),
                        Count(0),
                    ],
                ],
            ),
            (
                "MATCH (a:L)-[:R*1..2]->(b:L) WITH b RETURN b,COUNT(*) AS routes ORDER BY b",
                vec![
                    vec![Value(GraphValue::Vertex(VId(2))), Count(2)],
                    vec![Value(GraphValue::Vertex(VId(3))), Count(3)],
                ],
            ),
            (
                "MATCH (a:L {p:9999})-[:R*0..2]->(b:L) RETURN COUNT(*) AS routes,SUM(b.p) AS total",
                vec![vec![Count(0), Value(null())]],
            ),
        ] {
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            let mut cursor = ordered
                .execute_ordered_aggregate(&cx, &prepared, &params)
                .unwrap()
                .await
                .unwrap();
            let slots = cursor.output_slots().to_vec();
            let mut actual = Vec::new();
            while let Some(row) = cursor.next_row().await.unwrap() {
                actual.push(
                    slots
                        .iter()
                        .map(|slot| match *slot {
                            GraphAggregateTextSlot::GroupKey(at) => Value(row.keys()[at].clone()),
                            GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                        })
                        .collect::<Vec<_>>(),
                );
            }
            assert_eq!(actual, expected, "{text}");
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_paths_preflight_every_branch_before_sources_or_scratch() {
    let ((), report) = run_async_under_lab(0x860b_0003, |root| async move {
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
            "MATCH p = ANY SHORTEST (a)-[:R*2..3]->(b) RETURN p LIMIT 0",
            "MATCH (x:L {p:9999}) RETURN x AS node UNION ALL MATCH p = ANY SHORTEST (a)-[:R*2..3]->(b) RETURN b AS node LIMIT 0",
        ] {
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            assert!(
                matches!(
                    ordered.execute_ordered(&cx, &prepared, &params),
                    Err(NativeSpoolError::Authorized(error)) if matches!(*error, QueryError::StreamingUnsupported { .. })
                ),
                "{text}"
            );
            assert!(!ordered.is_closed());
        }
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH p = ALL SHORTEST (a)-[:R*2..3]->(b) RETURN COUNT(*) AS routes LIMIT 0",
                &params,
            )
            .unwrap();
        assert!(matches!(
            ordered.execute_ordered_aggregate(&cx, &prepared, &params),
            Err(NativeSpoolError::Authorized(error)) if matches!(*error, QueryError::StreamingUnsupported { .. })
        ));
        assert!(scratch.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(destination.0.lock().unwrap().bytes.get_ref().is_empty());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reached_path_errors_and_private_input_limits_survive_outer_limit_zero() {
    let ((), report) = run_async_under_lab(0x860b_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, scratch, destination) =
            session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        // Capability preparation refuses engine identities before the path
        // or its arithmetic can execute, even beneath an empty result page.
        let denied = ordered.prepare(
            &cx,
            "MATCH p = WALK (a:L)-[:R*1..2]->(b:L) WHERE 1/(id(b)-2)>0 RETURN p LIMIT 0",
            &params,
        );
        assert!(matches!(
            denied,
            Err(QueryError::Authorization(AuthorizationError::ScopeDenied))
        ));
        assert!(scratch.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(destination.0.lock().unwrap().bytes.get_ref().is_empty());
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH p = WALK (a:L)-[:R*1..2]->(b:L) WHERE 1/(b.p-1)>0 RETURN p LIMIT 0",
                &params,
            )
            .unwrap();
        let result = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await;
        assert!(matches!(result,
            Err(NativeSpoolError::Authorized(error)) if matches!(*error,
                QueryError::EdgeStream(GqlQueryError::Data(error)) if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
            )
        ));
        assert_eq!(pool.used(), 0);
        let physical = AuthorizedSortLimits {
            max_input_rows: 4,
            run_rows: 1,
            ..limits()
        };
        let (mut bounded, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, physical).await;
        let prepared = bounded
            .prepare(
                &cx,
                "MATCH p = WALK (a:L)-[:R*1..2]->(b:L) RETURN p LIMIT 0",
                &params,
            )
            .unwrap();
        let result = bounded
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await;
        assert!(matches!(result,
            Err(NativeSpoolError::Authorized(error)) if matches!(*error,
                QueryError::EdgeStream(GqlQueryError::Rows(error))
                    if error.dimension == GqlBudgetDimension::ResultRows && error.limit == 4 && error.observed == 5
            )
        ));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pending_path_spill_drop_and_expiry_release_the_generation_and_all_frames() {
    let ((), report) = run_async_under_lab(0x860b_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let pins = Arc::strong_count(&db.snapshot);
        for expire in [false, true] {
            let now = Arc::new(AtomicU64::new(100));
            let sampled = now.clone();
            let (mut ordered, scratch, destination) = session(
                &db,
                &cx,
                &issuer,
                &token,
                &pool,
                move || sampled.load(Ordering::SeqCst),
                AuthorizedSortLimits {
                    run_rows: 1,
                    ..limits()
                },
            )
            .await;
            let session_pins = Arc::strong_count(&db.snapshot);
            assert!(session_pins > pins);
            let prepared = ordered
                .prepare(
                    &cx,
                    "MATCH REPEATABLE ELEMENTS p = ALL SHORTEST (a:L)-[:R*0..]->(b:L) RETURN p",
                    &params,
                )
                .unwrap();
            scratch.0.lock().unwrap().pending_write = true;
            destination.0.lock().unwrap().pending_write = true;
            let mut future = Box::pin(ordered.execute_ordered(&cx, &prepared, &params).unwrap());
            let mut context = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut context).is_pending());
            assert!(pool.used() > 0);
            if expire {
                now.store(1001, Ordering::SeqCst);
                let Poll::Ready(result) = future.as_mut().poll(&mut context) else {
                    panic!("expiry must end pending path I/O");
                };
                refusal(result, AuthorizationError::Expired);
            }
            drop(future);
            assert_eq!(pool.used(), 0);
            assert!(ordered.scratch.is_poisoned() || ordered.destination.is_poisoned());
            assert_eq!(ordered.is_closed(), expire);
            if !expire {
                // Dropping the write releases the walk frames. The resident
                // session keeps only its original snapshot until closed.
                assert_eq!(Arc::strong_count(&db.snapshot), session_pins);
                ordered.close();
            }
            assert_eq!(Arc::strong_count(&db.snapshot), pins);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_paths_pin_the_complete_cut_and_rebind_historical_topology_and_fields() {
    let ((), report) = run_async_under_lab(0x860b_0006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let args = |cut| GqlParameters::new().with_uint64("cut", cut).unwrap();
        let text = "MATCH p = WALK (a:L)-[:R*1..2]->(b:L) \
                    FOR SYSTEM_TIME AS OF SEQ $cut RETURN p,b.p AS value ORDER BY p";
        let old_rows: Vec<_> = routes(false)
            .into_iter()
            .filter(|row| matches!(&row[2], GraphValue::Path(path) if !path.steps().is_empty()))
            .map(|row| vec![row[2].clone(), row[3].clone()])
            .collect();
        let mut old_rows = old_rows;
        old_rows.sort();
        let (mut pinned, scratch, destination) =
            session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = pinned.prepare(&cx, text, &args(1)).unwrap();
        let future = pinned.execute_ordered(&cx, &prepared, &args(1)).unwrap();
        assert!(scratch.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(destination.0.lock().unwrap().bytes.get_ref().is_empty());
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_edge(EId(12));
        change.set_vertex_property(VId(2), PropertyKeyId(1), Some(CanonicalScalar::Int(20)));
        db.write(&commit, change).await.unwrap();
        let mut cursor = future.await.unwrap();
        assert_eq!(cursor.snapshot_seq(), CommitSeq(1));
        assert_eq!(values(&mut cursor).await, old_rows);
        drop(cursor);
        assert!(matches!(pinned.execute_ordered(&cx, &prepared, &args(2)),
            Err(NativeSpoolError::Authorized(error)) if matches!(*error,
                QueryError::Read(crate::ReadError::BeyondFrontier { asked: CommitSeq(2), frontier: CommitSeq(1) })
            )
        ));
        let (mut current, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = current.prepare(&cx, text, &args(1)).unwrap();
        let mut new_rows: Vec<_> = [10, 11]
            .into_iter()
            .map(|edge| {
                let row = route(1, &[(edge, 2)]);
                vec![row[2].clone(), scalar(20)]
            })
            .collect();
        new_rows.sort();
        for (cut, expected) in [(0, vec![]), (1, old_rows), (2, new_rows)] {
            let mut cursor = current
                .execute_ordered(&cx, &prepared, &args(cut))
                .unwrap()
                .await
                .unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            assert_eq!(values(&mut cursor).await, expected);
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
