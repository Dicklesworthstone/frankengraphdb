//! Local fixed-hop projections retain each completed row independently.

use super::*;

#[test]
fn local_computed_joins_match_eager_history_hidden_keys_and_final_pages() {
    let ((), report) = run_async_under_lab(0x6275_6720, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, reference) = history(&commit).await;
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        for by in [7, 20] {
            let args = GqlParameters::new().with_int64("by", by).unwrap();
            for text in [
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,r.p+s.p+a.p+c.p+$by AS cost ORDER BY first,second",
                "MATCH (a)<-[r:R]-(b)-[s:R]->(c) RETURN r AS first,s AS second,r.p+s.p+a.p+c.p+$by AS cost ORDER BY first,second",
                "MATCH (a)-[r:R]-(b)-[s:R]->(c) RETURN r AS first,a AS origin,s AS second,r.p+s.p+a.p+c.p+$by AS cost ORDER BY first,origin,second",
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN {first:r,second:s,cost:r.p+s.p+a.p+c.p+$by,missing:c.missing} AS doc ORDER BY r,s SKIP 1 LIMIT 1",
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN $by AS repeated ORDER BY r,s",
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,r.p+s.p+$by AS cost LIMIT 0",
            ] {
                let prepared = projection(text, &args);
                for seq in 0..=3 {
                    let at = CommitSeq(seq);
                    let expected = reference
                        .execute_graph_set_governed_at(&cx, &prepared, at, query_policy())
                        .unwrap()
                        .value;
                    let mut cursor = view
                        .stream_graph_projection_governed_at(&cx, &prepared, at, query_policy())
                        .unwrap();
                    assert_eq!(cursor.snapshot_seq(), at);
                    let mut actual = Vec::new();
                    while let Some(row) = cursor.next().await {
                        actual.push(row.unwrap().as_ref().clone());
                    }
                    assert_eq!(actual, expected, "{text} at {seq}, by {by}");
                    assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                    assert!(cursor.next().await.is_none());
                }
            }
        }

        let prepared = projection(
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,r.p+s.p+a.p+c.p AS cost",
            &GqlParameters::new(),
        );
        for (at, expected) in [
            (CommitSeq(1), vec![(101, 22), (102, 24), (104, 25)]),
            (CommitSeq(3), vec![(101, 32), (104, 25)]),
        ] {
            let mut cursor = view
                .stream_graph_projection_governed_at(&cx, &prepared, at, query_policy())
                .unwrap();
            for (second, cost) in expected {
                let row = cursor.next().await.unwrap().unwrap();
                assert_eq!(
                    row.values(),
                    &[
                        GraphValue::Edge(EId(103)),
                        GraphValue::Edge(EId(second)),
                        GraphValue::Scalar(CanonicalScalar::Int(cost)),
                    ]
                );
            }
            assert!(cursor.next().await.is_none());
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn local_computed_join_refusals_budgets_and_suffix_failures_preserve_completion_rules() {
    let ((), report) = run_async_under_lab(0x6275_6721, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let args = GqlParameters::new();
        let baseline = pool.used();
        let before = view.buffer_stats();
        for text in [
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,r.p+s.p AS cost ORDER BY first LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,r.p+s.p AS cost ORDER BY second,first LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r.p+s.p AS cost ORDER BY r,s LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN DISTINCT r,s,r.p+s.p AS cost ORDER BY r,s LIMIT 0",
        ] {
            let prepared = projection(text, &args);
            assert!(
                view.stream_graph_projection_governed(&cx, &prepared, query_policy(),)
                    .is_err(),
                "{text}"
            );
            assert_eq!(view.buffer_stats(), before);
            assert_eq!(pool.used(), baseline);
        }

        for count in [0, 1] {
            let prepared = projection(
                &format!(
                    "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS first,s AS second,12/(s.p-10) AS cost ORDER BY first,second LIMIT {count}",
                ),
                &args,
            );
            let mut policy = query_policy();
            policy.rows = GqlExecutionBudget::new(1000, count);
            let mut cursor = view
                .stream_graph_projection_governed(&cx, &prepared, policy)
                .unwrap();
            if count == 1 {
                let row = cursor.next().await.unwrap().unwrap();
                assert_eq!(row.values()[0], GraphValue::Edge(EId(103)));
                assert_eq!(row.values()[1], GraphValue::Edge(EId(101)));
            }
            assert!(matches!(
                cursor.next().await,
                Some(Err(GqlQueryError::Source(ScanError::Projection {
                    row: 1,
                    column: 2,
                    ..
                })))
            ));
            assert_eq!(cursor.row_stats().result_rows, count);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().await.is_none());
        }

        let prepared = projection(
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,r.p+s.p AS cost",
            &args,
        );
        let mut one = query_policy();
        one.rows = GqlExecutionBudget::new(1000, 1);
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, one)
            .unwrap();
        drop(cursor.next().await.unwrap().unwrap());
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);

        let trace = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&trace));
        let cursor = view
            .stream_graph_projection_governed(&controlled, &prepared, query_policy())
            .unwrap();
        let opening = trace.calls();
        drop(cursor);
        let probe = Arc::new(SimulationCheckpointProbe::new(Some(opening + 1)));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let mut cursor = view
            .stream_graph_projection_governed(&controlled, &prepared, query_policy())
            .unwrap();
        assert!(cursor.next().await.unwrap().is_err());
        assert_eq!(probe.calls(), opening + 1);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);

        for count in [0, 1] {
            let page = projection(
                &format!(
                    "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,r.p+s.p AS cost LIMIT {count}",
                ),
                &args,
            );
            let mut policy = query_policy();
            policy.rows = GqlExecutionBudget::new(1000, count);
            let mut cursor = view
                .stream_graph_projection_governed(&cx, &page, policy)
                .unwrap();
            let mut delivered = 0;
            while let Some(row) = cursor.next().await {
                row.unwrap();
                delivered += 1;
            }
            assert_eq!(delivered, count);
            assert!(cursor.row_stats().snapshot_records > 0);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        let mut count = 0;
        while let Some(row) = cursor.next().await {
            row.unwrap();
            count += 1;
        }
        assert_eq!(count, 2);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        drop(cursor);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn local_computed_join_siblings_retain_and_refund_independent_vm_reservations() {
    let ((), report) = run_async_under_lab(0x6275_6722, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let payload = CanonicalScalar::bytes(vec![0x6a; 4096]).unwrap();
        let mut batch = WriteBatch::new(RELATION);
        for vid in 1..=4 {
            batch.create_vertex(VId(vid), vec![], vec![]);
        }
        batch.add_edge(EId(101), VId(1), VId(2), vec![(PROPERTY, payload.clone())]);
        batch.add_edge(EId(201), VId(2), VId(3), vec![]);
        batch.add_edge(EId(202), VId(2), VId(4), vec![]);
        db.write(&commit, batch).await.unwrap();
        let pool = MemoryPool::new(64 * 1024 * 1024, 0).unwrap();
        let mut view = db
            .buffered_read_view(&cx, pool.clone(), limits())
            .await
            .unwrap();
        let prepared = projection(
            &format!(
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r,s,[{}] AS copies",
                vec!["r.p"; 64].join(","),
            ),
            &GqlParameters::new(),
        );

        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        // Native input copies fit; the 64 repeated VM payloads need their own
        // independent admission before each allocation.
        let occupied = pool.reserve(&cx, pool.available() - 512 * 1024).unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Source(ScanError::Edge(
                fgdb_gql::edge_stream::EdgeScanError::Source(BufferedReadError::Memory(_))
            ))))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(occupied);

        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        let first = cursor.next().await.unwrap().unwrap();
        let second = cursor.next().await.unwrap().unwrap();
        for (row, child) in [(&first, EId(201)), (&second, EId(202))] {
            assert_eq!(
                row.values(),
                &[
                    GraphValue::Edge(EId(101)),
                    GraphValue::Edge(child),
                    GraphValue::List(
                        vec![GraphValue::Scalar(payload.clone()); 64].into_boxed_slice()
                    ),
                ]
            );
        }
        assert!(
            cursor.next().await.is_none(),
            "the traversal has backtracked past both siblings"
        );
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        drop(cursor);
        drop(view);
        let held_bytes = pool.used();
        assert!(held_bytes >= 128 * 4096);
        drop(first);
        let remaining = pool.used();
        assert!(
            held_bytes - remaining >= 64 * 4096,
            "the first completed row refunds its own VM charge"
        );
        assert!(
            remaining >= 64 * 4096,
            "the second row still owns its copies"
        );
        drop(second);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
