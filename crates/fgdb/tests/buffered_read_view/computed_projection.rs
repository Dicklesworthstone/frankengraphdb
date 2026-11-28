//! The owner-level cold API runs the same compiled projection kernel as FGP.

use super::*;

#[path = "computed_edge.rs"]
mod edge;
use fgdb_gql::scan_stream::ScanError;
use fgdb_gql::{GqlExecutionBudget, PreparedGraphSet, PreparedGraphSetText};
use fgdb_types::context::SimulationCheckpointProbe;
use std::sync::Arc;

fn projection(text: &str, args: &GqlParameters) -> PreparedGraphSet {
    let declarations: Vec<_> = args.parameter_types().collect();
    PreparedGraphSetText::prepare_with_parameter_types(text, &declarations, |kind, name| {
        match (kind, name) {
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PROPERTY)),
            (GraphSymbolKind::Property, "missing") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RELATION)),
            _ => None,
        }
    })
    .unwrap()
    .bind_parameters(args)
    .unwrap()
}

#[test]
fn buffered_computed_rows_preserve_history_hidden_order_keys_and_final_pages() {
    let ((), report) = run_async_under_lab(0x6275_6701, |root| async move {
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
        for by in [10, 20] {
            let args = GqlParameters::new().with_int64("by", by).unwrap();
            for text in [
                "MATCH (n:L) WHERE n.p>0 RETURN n AS id,n.p+$by AS shifted,[n.p,n.missing] AS fields ORDER BY id SKIP 1 LIMIT 2",
                "MATCH (n:L) RETURN {node:n,shifted:n.p+$by,missing:n.missing} AS doc ORDER BY n",
                "MATCH (n) RETURN $by AS repeated ORDER BY n",
                "MATCH (n) RETURN n AS id,n.p+$by AS shifted ORDER BY id LIMIT 0",
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
                    assert_eq!(cursor.row_stats().snapshot_records, 0);
                    let mut actual = Vec::new();
                    while let Some(row) = cursor.next().await {
                        actual.push(row.unwrap().as_ref().clone());
                    }
                    assert_eq!(actual, expected, "{text} at {seq}, by {by}");
                    assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                    assert!(cursor.next().await.is_none());
                    if seq == 3 {
                        assert_eq!(
                            cursor.row_stats().snapshot_records,
                            4,
                            "private pages and LIMIT 0 must validate the whole source",
                        );
                    }
                }
            }
        }
        let args = GqlParameters::new().with_int64("by", 7).unwrap();
        let prepared = projection("MATCH (n) RETURN $by AS repeated ORDER BY n", &args);
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        for _ in 0..3 {
            let row = cursor.next().await.unwrap().unwrap();
            assert_eq!(row.values(), [GraphValue::Scalar(CanonicalScalar::Int(7))]);
        }
        assert!(
            cursor.next().await.is_none(),
            "duplicate projected values survive"
        );
        drop(cursor);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_buffered_admission_refuses_before_faults_and_final_pages_do_not_hide_errors() {
    let ((), report) = run_async_under_lab(0x6275_6702, |root| async move {
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
        let before = pool.used();
        let before_cache = view.buffer_stats();
        for text in [
            "MATCH (n) RETURN n.p+1 AS shifted ORDER BY n LIMIT 0",
            "MATCH (n) RETURN n AS id,n.p+1 AS shifted ORDER BY id DESC LIMIT 0",
            "MATCH (n) RETURN DISTINCT n AS id,n.p+1 AS shifted ORDER BY id LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r AS id,s AS next_id,r.p+1 AS shifted ORDER BY id,next_id LIMIT 0",
        ] {
            let prepared = projection(text, &args);
            assert!(
                matches!(
                    view.stream_graph_projection_governed(&cx, &prepared, query_policy()),
                    Err(GqlQueryError::Source(ScanError::Vertex(
                        VertexScanError::Plan(_)
                    )))
                ),
                "{text}"
            );
            assert_eq!(pool.used(), before);
            assert_eq!(view.buffer_stats(), before_cache);
        }
        let prepared = projection("MATCH (n) RETURN n,n.p+1 AS shifted LIMIT 0", &args);
        assert!(matches!(
            view.stream_graph_projection_governed_at(&cx, &prepared, CommitSeq(4), query_policy(),),
            Err(GqlQueryError::Source(ScanError::Vertex(
                VertexScanError::Source(BufferedReadError::BeyondPublication {
                    requested: CommitSeq(4),
                    publication: CommitSeq(3),
                })
            )))
        ));
        assert_eq!(pool.used(), before);
        assert_eq!(view.buffer_stats(), before_cache);

        for count in [0, 1] {
            let text =
                format!("MATCH (n) RETURN n AS id,12/(n.p-4) AS value ORDER BY id LIMIT {count}");
            let prepared = projection(&text, &args);
            let mut policy = query_policy();
            policy.rows = GqlExecutionBudget::new(100, count);
            let mut cursor = view
                .stream_graph_projection_governed(&cx, &prepared, policy)
                .unwrap();
            if count == 1 {
                let row = cursor.next().await.unwrap().unwrap();
                assert_eq!(row.values()[0], GraphValue::Vertex(VId(1)));
            }
            assert!(matches!(
                cursor.next().await,
                Some(Err(GqlQueryError::Source(ScanError::Projection {
                    row: 2,
                    column: 1,
                    ..
                })))
            ));
            assert_eq!(cursor.row_stats().result_rows, count);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().await.is_none());
        }

        // A successful empty output still scans/evaluates every occurrence,
        // without charging intermediate native rows against the result cap.
        for count in [0, 1] {
            let prepared = projection(
                &format!("MATCH (n) RETURN n,n.p+1 AS value LIMIT {count}"),
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
            assert_eq!(cursor.row_stats().snapshot_records, 4);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_buffered_budgets_accumulate_and_interruption_releases_the_source() {
    let ((), report) = run_async_under_lab(0x6275_6703, |root| async move {
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
        let prepared = projection("MATCH (n) RETURN n,n.p+1 AS value", &GqlParameters::new());
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        drop(cursor.next().await.unwrap().unwrap());
        let first_scratch = cursor.evaluator_stats().scratch_entries;
        assert!(first_scratch > 0);
        cursor.close();
        drop(cursor);

        let mut scratch = query_policy();
        scratch.evaluator.max_scratch_entries = first_scratch;
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, scratch)
            .unwrap();
        drop(cursor.next().await.unwrap().unwrap());
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Evaluator(_)))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert!(cursor.next().await.is_none());
        drop(cursor);

        let mut one = query_policy();
        one.rows = GqlExecutionBudget::new(100, 1);
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, one)
            .unwrap();
        drop(cursor.next().await.unwrap().unwrap());
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert!(cursor.next().await.is_none());
        drop(cursor);

        // Observe constructor checkpoints, then interrupt the first pull's
        // next checkpoint. The injected error affects only this request.
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

        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        let mut count = 0;
        while let Some(row) = cursor.next().await {
            row.unwrap();
            count += 1;
        }
        assert_eq!(count, 3);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        drop(cursor);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_buffered_payloads_remain_charged_after_the_source_and_view_are_dropped() {
    let ((), report) = run_async_under_lab(0x6275_6704, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let payload = CanonicalScalar::bytes(vec![0x65; 4096]).unwrap();
        let mut batch = WriteBatch::new(RELATION);
        batch.create_vertex(VId(1), vec![], vec![(PROPERTY, payload.clone())]);
        db.write(&commit, batch).await.unwrap();
        let pool = pool();
        let mut view = db
            .buffered_read_view(&cx, pool.clone(), limits())
            .await
            .unwrap();
        let text = format!(
            "MATCH (n) RETURN n,[{}] AS payload LIMIT 1",
            vec!["n.p"; 64].join(","),
        );
        let prepared = projection(&text, &GqlParameters::new());
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        cursor.close();
        drop(cursor);
        drop(view);
        assert_eq!(
            held.values(),
            &[
                GraphValue::Vertex(VId(1)),
                GraphValue::List(vec![GraphValue::Scalar(payload); 64].into_boxed_slice()),
            ],
        );
        assert!(
            pool.used() >= 64 * 4096,
            "the raw two-column copy charge alone cannot cover 64 owned payloads",
        );
        drop(held);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_buffered_vm_allocations_refuse_host_memory_before_copying_and_can_retry() {
    let ((), report) = run_async_under_lab(0x6275_6705, |root| async move {
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
        let payload = CanonicalScalar::bytes(vec![0x66; 32 * 1024]).unwrap();
        let args = GqlParameters::new()
            .with_scalar("payload", payload.clone())
            .unwrap();
        let prepared = projection(
            "MATCH (n) RETURN n,{first:$payload,second:$payload} AS payload LIMIT 1",
            &args,
        );
        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        let occupied = pool.reserve(&cx, pool.available() - 96 * 1024).unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Source(ScanError::Vertex(
                VertexScanError::Source(BufferedReadError::Memory(_))
            ))))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(occupied);

        let mut cursor = view
            .stream_graph_projection_governed(&cx, &prepared, query_policy())
            .unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        cursor.close();
        drop(cursor);
        drop(view);
        assert_eq!(
            held.values()[1],
            GraphValue::map(vec![
                ("first".into(), GraphValue::Scalar(payload.clone())),
                ("second".into(), GraphValue::Scalar(payload)),
            ])
            .unwrap(),
        );
        assert!(pool.used() >= 64 * 1024);
        drop(held);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
