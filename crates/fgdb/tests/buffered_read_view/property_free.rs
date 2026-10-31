//! Public cold vertex queries prove that property-free plans retain complete
//! history, visibility and controls while avoiding unused payload retention.

use super::*;
use fgdb::BufferedReadView;
use fgdb_types::QueryCx;
use fgdb_types::context::SimulationCheckpointProbe;
use std::sync::Arc;

async fn open(commit: &CommitCx, vfs: &MemVfs, memory: &MemoryPool) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        commit,
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        memory.clone(),
        limits(),
    )
    .await
    .unwrap()
}

async fn collect(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    prepared: &PreparedGraphPattern<GraphValueRow>,
    cut: CommitSeq,
) -> (Vec<GraphValueRow>, fgdb_gql::GqlExecutionStats) {
    let mut cursor = view
        .stream_graph_values_governed_at(cx, prepared, cut, query_policy())
        .unwrap();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().await {
        rows.push(row.unwrap().as_ref().clone());
    }
    assert_eq!(cursor.snapshot_seq(), cut);
    assert_eq!(cursor.state(), VertexScanState::Exhausted);
    assert!(cursor.next().await.is_none());
    (rows, cursor.row_stats())
}

#[test]
fn property_free_queries_keep_historical_labels_and_pinned_publication_after_compaction() {
    let ((), report) = run_async_under_lab(0x6275_6610, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, old_reference) = history(&commit).await;
        let memory = pool();
        let mut pinned = open(&commit, &vfs, &memory).await;
        let mut writer = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut edit = WriteBatch::new(RELATION);
        edit.set_vertex_label(VId(2), LabelId(1), false);
        edit.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(111)));
        edit.create_vertex(
            VId(u128::MAX),
            vec![LabelId(1)],
            vec![(PROPERTY, CanonicalScalar::Int(99))],
        );
        assert_eq!(writer.write(&commit, edit).await.unwrap(), CommitSeq(4));
        writer.compact(&commit).await.unwrap();
        let current_reference = writer.read_session().unwrap();
        let mut current = open(&commit, &vfs, &memory).await;

        for (view, reference, frontier, candidates) in [
            (&mut pinned, &old_reference, 3, 4),
            (&mut current, &current_reference, 4, 5),
        ] {
            assert_eq!(view.frontier(), CommitSeq(frontier));
            for (text, labelled, width) in [
                ("MATCH (n) RETURN n", false, 1),
                ("MATCH (n:L) RETURN n", true, 1),
                ("MATCH (n:L) RETURN n,n AS again,n AS third", true, 3),
                ("MATCH (n) WHERE n=n RETURN n", false, 1),
            ] {
                let prepared = query(text);
                for cut in 0..=frontier {
                    let mut identities = match cut {
                        0 => vec![],
                        1 => vec![1, 2, 3, 4],
                        2 | 3 => vec![1, 2, 4],
                        _ => vec![1, 2, 4, u128::MAX],
                    };
                    if labelled && cut == 4 {
                        identities.retain(|vid| *vid != 2);
                    }
                    let expected: Vec<_> = identities
                        .into_iter()
                        .map(|vid| {
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Vertex(VId(vid));
                                width
                            ])
                        })
                        .collect();
                    let resident = reference
                        .execute_graph_pattern_governed_at(
                            &cx,
                            &prepared,
                            CommitSeq(cut),
                            query_policy(),
                        )
                        .unwrap();
                    let (actual, stats) = collect(view, &cx, &prepared, CommitSeq(cut)).await;
                    assert_eq!(actual, expected, "{text} at {cut}, publication {frontier}");
                    assert_eq!(actual, resident.value, "resident parity: {text} at {cut}");
                    assert_eq!(
                        stats.snapshot_records, candidates,
                        "invisible roots still count"
                    );
                    assert_eq!(stats.result_rows, actual.len() as u64);
                }
            }
            // A subsequent property plan must still receive complete rows;
            // cached bytes never turn an omitted field into a missing value.
            let prepared = query("MATCH (n) RETURN n,n.p");
            let expected = reference
                .execute_graph_pattern_governed_at(
                    &cx,
                    &prepared,
                    CommitSeq(frontier),
                    query_policy(),
                )
                .unwrap();
            let (actual, _) = collect(view, &cx, &prepared, CommitSeq(frontier)).await;
            assert_eq!(actual, expected.value);
        }
        drop(pinned);
        drop(current);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn property_free_projection_fits_a_pool_that_refuses_the_requested_payload() {
    let ((), report) = run_async_under_lab(0x6275_6611, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut writer =
            Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let payload = CanonicalScalar::ucs_basic_text(&"x".repeat(12 * 1024)).unwrap();
        let mut batch = WriteBatch::new(RELATION);
        let selected = VId(1_u128 << 100);
        batch.create_vertex(
            selected,
            vec![LabelId(1)],
            vec![(PROPERTY, payload.clone())],
        );
        // Keep the authenticated patch live while the first result is copied;
        // exhausting a one-row source would refund its decode workspace first.
        batch.create_vertex(VId(u128::MAX), vec![], vec![]);
        writer.write(&commit, batch).await.unwrap();
        let memory = pool();
        let mut view = open(&commit, &vfs, &memory).await;
        let identity_columns = std::iter::once("n".to_owned())
            .chain((1..32).map(|at| format!("n AS copy_{at}")))
            .collect::<Vec<_>>()
            .join(",");
        let property_columns = std::iter::once("n".to_owned())
            .chain((1..32).map(|at| format!("n.p AS copy_{at}")))
            .collect::<Vec<_>>()
            .join(",");
        let identities = query(&format!("MATCH (n:L) RETURN {identity_columns}"));
        let properties = query(&format!("MATCH (n:L) RETURN {property_columns}"));

        // Leave enough for the normal two scan workspaces and one frame, but
        // not complete copied payloads for every projected output cell. A
        // whole-record reservation would reject even repeated identity cells.
        let occupied = memory
            .reserve(&cx, memory.available() - 2304 * 1024)
            .unwrap();
        let (rows, stats) = collect(&mut view, &cx, &identities, CommitSeq(1)).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values(), vec![GraphValue::Vertex(selected); 32]);
        assert_eq!(stats.snapshot_records, 2);
        assert_eq!(stats.result_rows, 1);

        let mut cursor = view
            .stream_graph_values_governed(&cx, &properties, query_policy())
            .unwrap();
        let error = cursor.next().await.unwrap().unwrap_err();
        assert!(
            matches!(
                error,
                GqlQueryError::Source(VertexScanError::Source(
                    BufferedReadError::Memory(_)
                        | BufferedReadError::Buffer(
                            fgdb_strata::tiered::buffer::BufferError::Memory(_)
                        )
                ))
            ),
            "{error:?}"
        );
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.row_stats().snapshot_records, 1);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(occupied);

        let (rows, stats) = collect(&mut view, &cx, &properties, CommitSeq(1)).await;
        assert_eq!(stats.snapshot_records, 2);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get(0), Some(&GraphValue::Vertex(selected)));
        for value in &rows[0].values()[1..] {
            assert_eq!(value, &GraphValue::Scalar(payload.clone()));
        }
        drop(view);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn property_free_candidates_keep_record_admission_and_fuse_on_cancellation() {
    let ((), report) = run_async_under_lab(0x6275_6612, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let memory = pool();
        let prepared = query("MATCH (n:L) RETURN n,n AS again");
        let mut view = open(&commit, &vfs, &memory).await;
        let mut cursor = view
            .stream_graph_values_governed(
                &cx,
                &prepared,
                GqlQueryPolicy::new(0, 10, 1_000_000, 1_000_000),
            )
            .unwrap();
        let error = cursor.next().await.unwrap().unwrap_err();
        assert!(
            matches!(error, GqlQueryError::Rows(ref limit)
                if limit.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                    && limit.limit == 0 && limit.observed == 1),
            "{error:?}"
        );
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(
            view.buffer_stats().misses,
            0,
            "record admission precedes payload I/O"
        );
        let mut cursor = view
            .stream_graph_values_governed(
                &cx,
                &prepared,
                GqlQueryPolicy::new(3, 10, 1_000_000, 1_000_000),
            )
            .unwrap();
        assert!(cursor.next().await.unwrap().is_ok());
        assert!(cursor.next().await.unwrap().is_ok());
        let error = cursor.next().await.unwrap().unwrap_err();
        assert!(
            matches!(error, GqlQueryError::Rows(ref limit)
                if limit.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                    && limit.limit == 3 && limit.observed == 4),
            "retired vertex 3 still spends its admission: {error:?}"
        );
        assert_eq!(cursor.row_stats().result_rows, 2);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(view);
        assert_eq!(memory.used(), 0);

        let mut view = open(&commit, &vfs, &memory).await;
        let trace = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&trace));
        let mut cursor = view
            .stream_graph_values_governed(&controlled, &prepared, query_policy())
            .unwrap();
        assert!(cursor.next().await.unwrap().is_ok());
        let after_first = trace.calls();
        assert!(after_first > 0);
        drop(cursor);
        drop(view);
        assert_eq!(memory.used(), 0);

        let mut view = open(&commit, &vfs, &memory).await;
        let probe = Arc::new(SimulationCheckpointProbe::new(Some(after_first + 1)));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let mut cursor = view
            .stream_graph_values_governed(&controlled, &prepared, query_policy())
            .unwrap();
        assert!(cursor.next().await.unwrap().is_ok());
        let error = cursor.next().await.unwrap().unwrap_err();
        assert!(
            matches!(
                error,
                GqlQueryError::Interrupted(_)
                    | GqlQueryError::Source(VertexScanError::Source(
                        BufferedReadError::Interrupted(_)
                    ))
            ),
            "{error:?}"
        );
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 1);
        let stats = cursor.row_stats();
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats(), stats);
        assert_eq!(probe.calls(), after_first + 1);
        drop(cursor);
        drop(view);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
