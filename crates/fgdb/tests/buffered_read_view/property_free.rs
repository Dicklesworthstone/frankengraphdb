//! Public cold vertex queries prove that property-free plans retain complete
//! history, visibility and controls while avoiding unused payload retention.

use super::*;
use asupersync::fs::Vfs;
use fgdb::BufferedReadView;
use fgdb_gql::edge_stream::{EdgeScanError, EdgeScanState};
use fgdb_strata::store::BlockStore;
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
                ("MATCH (n) WHERE NOT (n<>n OR false) RETURN n", false, 1),
                (
                    "MATCH (n:L) WHERE n=n AND true RETURN n,n AS again",
                    true,
                    2,
                ),
                ("MATCH (n) WHERE true OR NULL RETURN n", false, 1),
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
        let properties = query(&format!("MATCH (n:L) RETURN {property_columns}"));

        // Leave enough for the normal two scan workspaces and one frame, but
        // not complete copied payloads for every projected output cell. A
        // whole-record reservation would reject even repeated identity cells.
        let occupied = memory
            .reserve(&cx, memory.available() - 2304 * 1024)
            .unwrap();
        for predicate in [
            "",
            " WHERE true",
            " WHERE n=n AND true",
            " WHERE true OR n<>n",
        ] {
            let text = format!("MATCH (n:L){predicate} RETURN {identity_columns}");
            let identities = query(&text);
            let (rows, stats) = collect(&mut view, &cx, &identities, CommitSeq(1)).await;
            assert_eq!(rows.len(), 1, "{text}");
            assert_eq!(rows[0].values(), vec![GraphValue::Vertex(selected); 32]);
            assert_eq!(stats.snapshot_records, 2);
            assert_eq!(stats.result_rows, 1);
        }

        // Element identity preserves all 128 bits. The integer id() function
        // must report overflow when this same element has no signed image.
        let numeric = query(&format!(
            "MATCH (n:L) WHERE id(n)=id(n) AND NOT false RETURN {identity_columns}"
        ));
        let mut cursor = view
            .stream_graph_values_governed(&cx, &numeric, query_policy())
            .unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Data(fgdb_gql::GraphIntegerError {
                kind: fgdb_gql::GraphIntegerErrorKind::Overflow,
                ..
            })))
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().await.is_none());
        drop(cursor);

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
        let prepared = query("MATCH (n:L) WHERE n=n AND true RETURN n,n AS again");
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

#[test]
fn boolean_edge_queries_skip_unused_sidecars_but_keep_eager_property_reads() {
    let ((), report) = run_async_under_lab(0x6275_6613, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, reference) = history(&commit).await;
        let memory = pool();
        let mut view = open(&commit, &vfs, &memory).await;
        let store = BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
        )
        .await
        .unwrap();
        let root = store
            .get_root(&commit, reference.partition_root())
            .await
            .unwrap();
        let mut bodies = std::collections::BTreeMap::new();
        let mut nested_sidecars = std::collections::BTreeSet::new();
        for block in root.blocks {
            let bytes = store
                .get_bytes(&commit, fgdb_strata::DeltaBlockVersion(block.block_id))
                .await
                .unwrap();
            let (entries, properties) = fgdb_strata::decode_block_with_properties(&bytes).unwrap();
            if let Some((id, _)) = properties {
                let path = store.path(id);
                if entries
                    .first()
                    .is_some_and(|edge| edge.relation == RelationId(8))
                {
                    nested_sidecars.insert(path.clone());
                }
                bodies.insert(path.clone(), vfs.read(&path).await.unwrap());
            }
        }
        assert!(
            !bodies.is_empty(),
            "the fixture has actual property sidecars"
        );
        for path in bodies.keys() {
            vfs.write(path, b"damaged unused Boolean-query sidecar")
                .await
                .unwrap();
        }

        for cut in [1, 3] {
            let expected: Vec<_> = [(101, 1, 2), (102, 1, 3), (103, 4, 1), (104, 1, 2)]
                .into_iter()
                .filter(|(eid, _, _)| *eid != 102 || cut == 1)
                .map(|(eid, src, dst)| {
                    vec![
                        GraphValue::Edge(EId(eid)),
                        GraphValue::Vertex(VId(src)),
                        GraphValue::Vertex(VId(dst)),
                    ]
                })
                .collect();
            for (predicate, selects) in [
                ("a<>b AND r=r", true),
                ("id(r)=id(r) AND NOT (a=b)", true),
                ("true OR NULL", true),
                ("false AND a<>b", false),
                ("NULL OR false", false),
            ] {
                let text = format!("MATCH (a)-[r:R]->(b) WHERE {predicate} RETURN r,a,b");
                let prepared = query(&text);
                let mut cursor = view
                    .stream_graph_edges_governed_at(&cx, &prepared, CommitSeq(cut), query_policy())
                    .unwrap();
                let mut rows = Vec::new();
                while let Some(row) = cursor.next().await {
                    rows.push(row.unwrap().row().values().to_vec());
                }
                if selects {
                    assert_eq!(rows, expected, "{text} at {cut}");
                } else {
                    assert!(rows.is_empty(), "{text} at {cut}");
                }
                assert_eq!(cursor.row_stats().snapshot_records, 4);
                assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
                assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                assert!(cursor.next().await.is_none());
            }
        }

        // Boolean evaluation is eager: a decisive constant does not remove
        // an actual property input or its source-error boundary.
        for predicate in ["true OR r.p IS NOT NULL", "false AND r.p IS NOT NULL"] {
            let prepared = query(&format!(
                "MATCH (a)-[r:R]->(b) WHERE {predicate} RETURN r,a,b"
            ));
            let mut cursor = view
                .stream_graph_edges_governed(&cx, &prepared, query_policy())
                .unwrap();
            assert!(matches!(
                cursor.next().await,
                Some(Err(GqlQueryError::Source(EdgeScanError::Source(_))))
            ));
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert_eq!(cursor.row_stats().result_rows, 0);
            let failed = cursor.row_stats();
            assert!(cursor.next().await.is_none());
            assert_eq!(cursor.row_stats(), failed);
        }
        // Both the R root and its S child have damaged edge sidecars.
        assert!(!nested_sidecars.is_empty() && nested_sidecars.len() < bodies.len());
        let topology = query("MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN r,a,s,c");
        let mut join = view
            .stream_graph_edge_joins_governed(&cx, &topology, query_policy())
            .unwrap();
        assert_eq!(
            join.next().await.unwrap().unwrap().values(),
            &[
                GraphValue::Edge(EId(103)),
                GraphValue::Vertex(VId(4)),
                GraphValue::Edge(EId(105)),
                GraphValue::Vertex(VId(4))
            ]
        );
        assert!(join.next().await.is_none());
        assert_eq!(join.state(), EdgeScanState::Exhausted);
        drop(join);
        let endpoints =
            query("MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN r,a,s,c,b.p AS middle,c.p AS last");
        let mut join = view
            .stream_graph_edge_joins_governed(&cx, &endpoints, query_policy())
            .unwrap();
        assert_eq!(
            join.next().await.unwrap().unwrap().values(),
            &[
                GraphValue::Edge(EId(103)),
                GraphValue::Vertex(VId(4)),
                GraphValue::Edge(EId(105)),
                GraphValue::Vertex(VId(4)),
                GraphValue::Scalar(CanonicalScalar::Int(11)),
                GraphValue::Scalar(CanonicalScalar::Int(4)),
            ]
        );
        assert!(join.next().await.is_none());
        drop(join);
        let needed = query("MATCH (a)-[r:R]->(b)-[s:S]->(c) RETURN r,a,s,r.p AS value");
        let mut join = view
            .stream_graph_edge_joins_governed(&cx, &needed, query_policy())
            .unwrap();
        assert!(matches!(
            join.next().await,
            Some(Err(GqlQueryError::Source(EdgeScanError::Source(_))))
        ));
        assert_eq!(join.state(), EdgeScanState::Failed);
        drop(join);
        for (path, bytes) in bodies {
            vfs.write(&path, &bytes).await.unwrap();
        }
        let prepared = query("MATCH (a)-[r:R]->(b) WHERE true OR r.p IS NULL RETURN r,a,b");
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &prepared, query_policy())
            .unwrap();
        let mut rows = 0;
        while let Some(row) = cursor.next().await {
            row.unwrap();
            rows += 1;
        }
        assert_eq!(rows, 3, "restored demanded sidecars are read normally");
        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
        drop(cursor);
        drop(view);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn boolean_vertex_filters_keep_unknown_results_and_eager_arithmetic_errors() {
    let ((), report) = run_async_under_lab(0x6275_6614, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let memory = pool();

        for predicate in ["false AND n=n", "NULL OR false", "NOT (NULL AND true)"] {
            let prepared = query(&format!("MATCH (n) WHERE {predicate} RETURN n"));
            let mut view = open(&commit, &vfs, &memory).await;
            for cut in 0..=3 {
                let (rows, stats) = collect(&mut view, &cx, &prepared, CommitSeq(cut)).await;
                assert!(rows.is_empty(), "{predicate} at {cut}");
                assert_eq!(stats.snapshot_records, 4);
                assert_eq!(stats.result_rows, 0);
            }
            drop(view);
            assert_eq!(memory.used(), 0);
        }

        for (predicate, has_prefix, records) in [
            ("true OR 10 / (n.p - 2) > 0", true, 2),
            ("false AND 10 / (n.p - 2) > 0", false, 2),
            ("true OR 1 / 0 = 1", false, 1),
            ("false AND 1 / 0 = 1", false, 1),
        ] {
            let prepared = query(&format!("MATCH (n) WHERE {predicate} RETURN n"));
            let mut view = open(&commit, &vfs, &memory).await;
            let mut cursor = view
                .stream_graph_values_governed(&cx, &prepared, query_policy())
                .unwrap();
            let held = if has_prefix {
                Some(cursor.next().await.unwrap().unwrap())
            } else {
                None
            };
            assert!(
                matches!(cursor.next().await, Some(Err(GqlQueryError::Data(_)))),
                "{predicate}"
            );
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert_eq!(cursor.row_stats().snapshot_records, records);
            assert_eq!(cursor.row_stats().result_rows, u64::from(has_prefix));
            let failed = cursor.row_stats();
            assert!(cursor.next().await.is_none());
            assert_eq!(cursor.row_stats(), failed);
            drop(cursor);
            drop(view);
            if let Some(held) = held {
                assert!(memory.used() > 0, "the delivered prefix owns its charge");
                assert_eq!(held.values(), [GraphValue::Vertex(VId(1))]);
                drop(held);
            }
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn boolean_edge_filters_keep_hidden_endpoint_inputs_and_eager_arithmetic_errors() {
    let ((), report) = run_async_under_lab(0x6275_6615, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let memory = pool();
        for (predicate, prefix, records) in [
            ("true OR 10 / (r.p - 10) > 0", vec![101, 103], 4),
            ("false AND 10 / (r.p - 17) > 0", vec![], 1),
            ("true OR 10 / (a.p - 11) > 0", vec![], 1),
            ("false AND 10 / (b.p - 2) > 0", vec![], 1),
            ("true OR 1 / 0 = 1", vec![], 1),
            ("false AND 1 / 0 = 1", vec![], 1),
        ] {
            let prepared = query(&format!(
                "MATCH (a)-[r:R]->(b) WHERE {predicate} RETURN r,a,b"
            ));
            let mut view = open(&commit, &vfs, &memory).await;
            let mut cursor = view
                .stream_graph_edges_governed(&cx, &prepared, query_policy())
                .unwrap();
            let mut held = Vec::new();
            for eid in &prefix {
                let row = cursor.next().await.unwrap().unwrap();
                assert_eq!(row.row().values()[0], GraphValue::Edge(EId(*eid)));
                held.push(row);
            }
            assert!(
                matches!(cursor.next().await, Some(Err(GqlQueryError::Data(_)))),
                "{predicate}"
            );
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert_eq!(cursor.row_stats().snapshot_records, records);
            assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
            let failed = cursor.row_stats();
            assert!(cursor.next().await.is_none());
            assert_eq!(cursor.row_stats(), failed);
            drop(cursor);
            drop(view);
            assert_eq!(!held.is_empty(), memory.used() > 0);
            for (row, eid) in held.iter().zip(prefix) {
                assert_eq!(row.row().values()[0], GraphValue::Edge(EId(eid)));
            }
            drop(held);
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

const SELECTED_VERTEX_KEY: PropertyKeyId = PropertyKeyId((1_u64 << 40) | 1);
const SELECTED_VERTICES: [VId; 3] = [VId(1_u128 << 100), VId((1_u128 << 100) + 1), VId(u128::MAX)];

fn selected_vertex_query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, |kind, name| match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PROPERTY)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(SELECTED_VERTEX_KEY)),
        (GraphSymbolKind::Property, "r") => Some(GraphSymbol::Property(PropertyKeyId(u64::MAX))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

async fn selected_vertex_fixture(contexts: &PurposeContexts) -> (MemVfs, Database<MemVfs>) {
    let vfs = MemVfs::new().unwrap();
    let commit = contexts.commit();
    let mut writer = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(RELATION);
    for (vid, value) in SELECTED_VERTICES.into_iter().zip([
        Some(CanonicalScalar::Int(9)),
        Some(CanonicalScalar::Null),
        None,
    ]) {
        let mut properties = vec![(
            PROPERTY,
            CanonicalScalar::ucs_basic_text(&"x".repeat(12 * 1024)).unwrap(),
        )];
        if let Some(value) = value {
            properties.push((SELECTED_VERTEX_KEY, value));
        }
        first.create_vertex(vid, vec![LabelId(1)], properties);
    }
    assert_eq!(writer.write(&commit, first).await.unwrap(), CommitSeq(1));
    let mut second = WriteBatch::new(RELATION);
    second.set_vertex_property(
        SELECTED_VERTICES[0],
        SELECTED_VERTEX_KEY,
        Some(CanonicalScalar::Int(11)),
    );
    second.set_vertex_property(
        SELECTED_VERTICES[1],
        SELECTED_VERTEX_KEY,
        Some(CanonicalScalar::Int(5)),
    );
    second.delete_vertex(SELECTED_VERTICES[2]);
    assert_eq!(writer.write(&commit, second).await.unwrap(), CommitSeq(2));
    (vfs, writer)
}

async fn open_selected_vertices(
    contexts: &PurposeContexts,
    vfs: &MemVfs,
    memory: &MemoryPool,
) -> BufferedReadView<MemVfs> {
    let mut read_limits = limits();
    read_limits.buffer.max_extent_bytes = 64 * 1024;
    Database::open_buffered_read_view_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        memory.clone(),
        read_limits,
    )
    .await
    .unwrap()
}

#[test]
fn selected_vertex_queries_keep_full_width_keys_nulls_labels_and_pinned_history() {
    let ((), report) = run_async_under_lab(0x6275_6616, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (vfs, mut writer) = selected_vertex_fixture(&contexts).await;
        let old_reference = writer.read_session().unwrap();
        let memory = pool();
        let mut pinned = open_selected_vertices(&contexts, &vfs, &memory).await;
        let mut third = WriteBatch::new(RELATION);
        third.set_vertex_property(SELECTED_VERTICES[0], SELECTED_VERTEX_KEY, None);
        third.set_vertex_property(
            SELECTED_VERTICES[1],
            SELECTED_VERTEX_KEY,
            Some(CanonicalScalar::Int(21)),
        );
        third.set_vertex_label(SELECTED_VERTICES[1], LabelId(1), false);
        assert_eq!(
            writer.write(&contexts.commit(), third).await.unwrap(),
            CommitSeq(3)
        );
        writer.compact(&contexts.commit()).await.unwrap();
        let current_reference = writer.read_session().unwrap();
        let mut current = open_selected_vertices(&contexts, &vfs, &memory).await;
        for (view, reference, frontier) in [
            (&mut pinned, &old_reference, 2),
            (&mut current, &current_reference, 3),
        ] {
            for (text, labelled, null_filter) in [
                ("MATCH (n) RETURN n,n.q", false, None),
                ("MATCH (n:L) RETURN n,n.q", true, None),
                (
                    "MATCH (n) WHERE n.q IS NULL RETURN n,n.q",
                    false,
                    Some(true),
                ),
                (
                    "MATCH (n:L) WHERE n.q IS NOT NULL RETURN n,n.q",
                    true,
                    Some(false),
                ),
                ("MATCH (n) WHERE true OR n.q=999 RETURN n,n.q", false, None),
                (
                    "MATCH (n) WHERE coalesce(n.q,0)>=0 RETURN n,n.q",
                    false,
                    None,
                ),
            ] {
                let prepared = selected_vertex_query(text);
                for cut in 0..=frontier {
                    let mut values = match cut {
                        0 => vec![],
                        1 => vec![(0, Some(9)), (1, None), (2, None)],
                        2 => vec![(0, Some(11)), (1, Some(5))],
                        _ => vec![(0, None), (1, Some(21))],
                    };
                    values.retain(|(at, value)| {
                        !(labelled && cut == 3 && *at == 1)
                            && null_filter.is_none_or(|is_null| value.is_none() == is_null)
                    });
                    let expected: Vec<_> = values
                        .into_iter()
                        .map(|(at, value)| {
                            GraphValueRow::from_owned_values(vec![
                                GraphValue::Vertex(SELECTED_VERTICES[at]),
                                GraphValue::Scalar(
                                    value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                                ),
                            ])
                        })
                        .collect();
                    let (actual, stats) = collect(view, &cx, &prepared, CommitSeq(cut)).await;
                    assert_eq!(actual, expected, "{text} at {cut}, frontier {frontier}");
                    assert_eq!(stats.snapshot_records, 3, "retired identities still count");
                    let resident = reference
                        .execute_graph_pattern_governed_at(
                            &cx,
                            &prepared,
                            CommitSeq(cut),
                            query_policy(),
                        )
                        .unwrap();
                    assert_eq!(actual, resident.value, "resident parity: {text} at {cut}");
                }
            }
            // Adding another demanded field after selected pulls must recover
            // the actual large field, never a partial cached property map.
            let complete = selected_vertex_query("MATCH (n) RETURN n,n.q,n.p");
            let expected = reference
                .execute_graph_pattern_governed_at(
                    &cx,
                    &complete,
                    CommitSeq(frontier),
                    query_policy(),
                )
                .unwrap();
            assert_eq!(
                collect(view, &cx, &complete, CommitSeq(frontier)).await.0,
                expected.value
            );
        }
        drop(pinned);
        drop(current);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_vertex_query_omits_unrelated_payload_charges_and_retains_output_ownership() {
    let ((), report) = run_async_under_lab(0x6275_6617, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (vfs, mut writer) = selected_vertex_fixture(&contexts).await;
        let mut third = WriteBatch::new(RELATION);
        for (vid, value) in [(SELECTED_VERTICES[0], 101), (SELECTED_VERTICES[1], 55)] {
            third.set_vertex_property(
                vid,
                PropertyKeyId(u64::MAX),
                Some(CanonicalScalar::Int(value)),
            );
        }
        assert_eq!(
            writer.write(&contexts.commit(), third).await.unwrap(),
            CommitSeq(3)
        );
        let memory = pool();
        let mut view = open_selected_vertices(&contexts, &vfs, &memory).await;
        let columns = std::iter::once("n".to_owned())
            .chain((1..64).map(|at| {
                let field = if at % 2 == 1 { "q" } else { "r" };
                format!("n.{field} AS copy_{at}")
            }))
            .collect::<Vec<_>>()
            .join(",");
        let occupied = memory
            .reserve(&cx, memory.available() - 3 * 1024 * 1024)
            .unwrap();
        let prepared = selected_vertex_query(&format!(
            "MATCH (n:L) WHERE n.q>=5 AND n.r>0 RETURN {columns}"
        ));
        let (rows, stats) = collect(&mut view, &cx, &prepared, CommitSeq(3)).await;
        assert_eq!(stats.snapshot_records, 3);
        assert_eq!(rows.len(), 2);
        for (row, (vid, q, r)) in rows.iter().zip([
            (SELECTED_VERTICES[0], 11, 101),
            (SELECTED_VERTICES[1], 5, 55),
        ]) {
            assert_eq!(row.get(0), Some(&GraphValue::Vertex(vid)));
            for (at, cell) in row.values().iter().enumerate().skip(1) {
                let value = if at % 2 == 1 { q } else { r };
                assert_eq!(*cell, GraphValue::Scalar(CanonicalScalar::Int(value)));
            }
        }
        // Requesting the large property must charge its 12 KiB for every
        // cell. Even 64 * 4 * 12 KiB fills the remaining 3 MiB before
        // accounting for scan state. The extra projection is column 65.
        for text in [
            format!("MATCH (n:L) RETURN {columns},n.p"),
            format!("MATCH (n:L) WHERE n.p IS NOT NULL RETURN {columns}"),
        ] {
            let complete = selected_vertex_query(&text);
            let mut cursor = view
                .stream_graph_values_governed(&cx, &complete, query_policy())
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
                "{text}: {error:?}"
            );
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().await.is_none());
        }
        let before = view.buffer_stats();
        let mut refused = view
            .stream_graph_values_governed(
                &cx,
                &prepared,
                GqlQueryPolicy::new(0, 10, 1_000_000, 1_000_000),
            )
            .unwrap();
        assert!(matches!(
            refused.next().await.unwrap(),
            Err(GqlQueryError::Rows(_))
        ));
        assert!(refused.next().await.is_none());
        drop(refused);
        assert_eq!(
            view.buffer_stats(),
            before,
            "candidate refusal precedes I/O"
        );

        let mut cursor = view
            .stream_graph_values_governed(&cx, &prepared, query_policy())
            .unwrap();
        let held = cursor.next().await.unwrap().unwrap();
        drop(cursor);
        drop(view);
        drop(occupied);
        assert!(
            memory.used() > 0,
            "the selected output retains its own reservation"
        );
        assert_eq!(
            held.get(1),
            Some(&GraphValue::Scalar(CanonicalScalar::Int(11)))
        );
        assert_eq!(
            held.get(2),
            Some(&GraphValue::Scalar(CanonicalScalar::Int(101)))
        );
        drop(held);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
