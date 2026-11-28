//! The local computed facade keeps edge and endpoint histories in one source.

use super::*;

async fn edge_history(cx: &CommitCx) -> (MemVfs, EmbeddedReadView) {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(RELATION);
    for (vid, value) in [(1, 1), (2, 2)] {
        first.create_vertex(
            VId(vid),
            vec![LabelId(1)],
            vec![(PROPERTY, CanonicalScalar::Int(value))],
        );
    }
    for (eid, from, to, value) in [(101, 1, 2, 3), (102, 1, 2, 4), (103, 2, 2, 5)] {
        first.add_edge(
            EId(eid),
            VId(from),
            VId(to),
            vec![(PROPERTY, CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, first).await.unwrap(), CommitSeq(1));
    let mut second = WriteBatch::new(RELATION);
    second.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(11)));
    second.set_vertex_property(VId(2), PROPERTY, Some(CanonicalScalar::Int(22)));
    second.set_edge_property(EId(101), PROPERTY, Some(CanonicalScalar::Int(17)));
    assert_eq!(db.write(cx, second).await.unwrap(), CommitSeq(2));
    (vfs, db.read_session().unwrap())
}

#[test]
fn local_computed_edges_preserve_endpoint_history_orientation_and_loop_multiplicity() {
    let ((), report) = run_async_under_lab(0x6275_6710, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, reference) = edge_history(&commit).await;
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
                "MATCH (a)-[r:R]->(b) RETURN r AS id,r.p+a.p+b.p+$by AS cost ORDER BY id",
                "MATCH (a)<-[r:R]-(b) RETURN r AS id,a.p-b.p+r.p+$by AS cost ORDER BY id",
                "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,r.p+a.p+$by AS cost ORDER BY id,origin",
                "MATCH (a)-[r:R]-(b) RETURN {edge:r,origin:a,cost:r.p+a.p+b.p+$by,missing:b.missing} AS doc ORDER BY r,a SKIP 1 LIMIT 3",
                "MATCH (a)-[r:R]-(b) RETURN $by AS repeated ORDER BY r,a",
                "MATCH (a)-[r:R]->(b) RETURN r AS id,r.p+a.p+$by AS cost ORDER BY id LIMIT 0",
            ] {
                let prepared = projection(text, &args);
                for seq in 0..=2 {
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
                    if seq == 2 {
                        assert_eq!(cursor.row_stats().snapshot_records, 3);
                    }
                }
            }
        }
        let prepared = projection(
            "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,r.p+a.p+7 AS cost ORDER BY id,origin",
            &GqlParameters::new(),
        );
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        for (eid, from, cost) in [
            (101, 1, 35),
            (101, 2, 46),
            (102, 1, 22),
            (102, 2, 33),
            (103, 2, 34),
        ] {
            let row = cursor.next().await.unwrap().unwrap();
            assert_eq!(
                row.values(),
                &[
                    GraphValue::Edge(EId(eid)),
                    GraphValue::Vertex(VId(from)),
                    GraphValue::Scalar(CanonicalScalar::Int(cost)),
                ]
            );
        }
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats().snapshot_records, 3);
        assert_eq!(cursor.row_stats().result_rows, 5);
        drop(cursor);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn local_computed_edge_refusals_precede_faults_and_final_pages_validate_every_orientation() {
    let ((), report) = run_async_under_lab(0x6275_6711, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = edge_history(&commit).await;
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
        let before = view.buffer_stats();
        let baseline = pool.used();
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN r.p+a.p AS cost ORDER BY r LIMIT 0",
            "MATCH (a)-[r:R]->(b) RETURN r AS id,r.p+1 AS cost ORDER BY id DESC LIMIT 0",
            "MATCH (a)-[r:R]-(b) RETURN r AS id,r.p+1 AS cost ORDER BY id LIMIT 0",
            "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,r.p+1 AS cost ORDER BY origin,id LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS id,s AS next_id,r.p+s.p AS cost ORDER BY next_id,id LIMIT 0",
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
                    "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,12/(r.p-5) AS cost ORDER BY id,origin LIMIT {count}",
                ),
                &args,
            );
            let mut policy = query_policy();
            policy.rows = GqlExecutionBudget::new(100, count);
            let mut cursor = view
                .stream_graph_projection_governed(&cx, &prepared, policy)
                .unwrap();
            if count == 1 {
                let row = cursor.next().await.unwrap().unwrap();
                assert_eq!(row.values()[0], GraphValue::Edge(EId(101)));
            }
            assert!(matches!(
                cursor.next().await,
                Some(Err(GqlQueryError::Source(ScanError::Projection {
                    row: 4,
                    column: 2,
                    ..
                })))
            ));
            assert_eq!(cursor.row_stats().result_rows, count);
            assert_eq!(cursor.row_stats().snapshot_records, 3);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().await.is_none());
        }
        for count in [0, 1] {
            let prepared = projection(
                &format!(
                    "MATCH (a)-[r:R]-(b) RETURN r AS id,a AS origin,r.p+a.p AS cost ORDER BY id,origin LIMIT {count}",
                ),
                &args,
            );
            let mut policy = query_policy();
            policy.rows = GqlExecutionBudget::new(100, count);
            let mut cursor = view
                .stream_graph_projection_governed(&cx, &prepared, policy)
                .unwrap();
            let mut delivered = 0;
            while let Some(row) = cursor.next().await {
                row.unwrap();
                delivered += 1;
            }
            assert_eq!(delivered, count);
            assert_eq!(cursor.row_stats().snapshot_records, 3);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_edge_orientations_share_growing_endpoint_payload_admission_until_last_output_drop() {
    let ((), report) = run_async_under_lab(0x6275_6712, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let first_payload = CanonicalScalar::bytes(vec![0x67; 4096]).unwrap();
        let second_payload = CanonicalScalar::bytes(vec![0x68; 4096]).unwrap();
        let mut batch = WriteBatch::new(RELATION);
        for (vid, payload) in [(1, first_payload.clone()), (2, second_payload.clone())] {
            batch.create_vertex(VId(vid), vec![], vec![(PROPERTY, payload)]);
        }
        batch.add_edge(EId(101), VId(1), VId(2), vec![]);
        db.write(&commit, batch).await.unwrap();
        let pool = MemoryPool::new(64 * 1024 * 1024, 0).unwrap();
        let mut view = db
            .buffered_read_view(&cx, pool.clone(), limits())
            .await
            .unwrap();
        let prepared = projection(
            &format!(
                "MATCH (a)-[r:R]-(b) RETURN r,a,[{}] AS copies",
                vec!["a.p"; 64].join(",")
            ),
            &GqlParameters::new(),
        );
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        let first = cursor.next().await.unwrap().unwrap();
        let first_charge = pool.used();
        let second = cursor.next().await.unwrap().unwrap();
        assert!(
            pool.used() > first_charge,
            "the same record admits the second orientation's VM"
        );
        for (row, from, payload) in [
            (&first, VId(1), first_payload),
            (&second, VId(2), second_payload),
        ] {
            assert_eq!(
                row.values(),
                &[
                    GraphValue::Edge(EId(101)),
                    GraphValue::Vertex(from),
                    GraphValue::List(vec![GraphValue::Scalar(payload); 64].into_boxed_slice()),
                ]
            );
        }
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(view);
        assert!(
            pool.used() >= 128 * 4096,
            "raw source copies cannot cover 128 VM-owned payloads"
        );
        drop(first);
        assert!(
            pool.used() >= 64 * 4096,
            "the second output retains the shared evaluation charge"
        );
        drop(second);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn local_computed_edge_limits_and_interruption_fuse_without_losing_reader_reuse() {
    let ((), report) = run_async_under_lab(0x6275_6713, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = edge_history(&commit).await;
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
        let prepared = projection(
            "MATCH (a)-[r:R]-(b) RETURN r,a,r.p+a.p AS cost",
            &GqlParameters::new(),
        );
        let mut one = query_policy();
        one.rows = GqlExecutionBudget::new(100, 1);
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, one)
            .unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(
            cursor.row_stats().snapshot_records,
            1,
            "the pending orientation uses the same record"
        );
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(held);

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

        let payload = CanonicalScalar::bytes(vec![0x69; 32 * 1024]).unwrap();
        let args = GqlParameters::new()
            .with_scalar("payload", payload)
            .unwrap();
        let large = projection(
            "MATCH (a)-[r:R]->(b) RETURN r,{first:$payload,second:$payload} AS copies LIMIT 1",
            &args,
        );
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &large, query_policy())
            .unwrap();
        let occupied = pool.reserve(&cx, pool.available() - 96 * 1024).unwrap();
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
        let mut count = 0;
        while let Some(row) = cursor.next().await {
            row.unwrap();
            count += 1;
        }
        assert_eq!(count, 5);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        drop(cursor);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
