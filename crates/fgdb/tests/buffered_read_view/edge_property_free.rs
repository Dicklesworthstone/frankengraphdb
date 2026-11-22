//! Compiler-proven topology demand over real buffered GQL sources. Expected
//! identities are literal; property demand and source failures remain visible.

use super::*;
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use fgdb::{NativeAggregateSpoolError, PreparedNativeRead};
use fgdb_gql::{GqlBudgetDimension, GraphAggregateValue};
use fgdb_strata::tiered::memory::{SpillFile, SpillLimits};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};

fn memory() -> MemoryPool {
    MemoryPool::new(64 * 1024 * 1024, 0).unwrap()
}

fn small_run_limits() -> SealedLimits {
    SealedLimits {
        max_rows: 4,
        max_incidences: 16,
        max_image_bytes: 4096,
        max_property_bytes: 128,
    }
}

async fn open(
    contexts: &PurposeContexts,
    vfs: &MemVfs,
    memory: &MemoryPool,
    native: Option<SealedLimits>,
) -> BufferedReadView<MemVfs> {
    let mut read_limits = limits();
    read_limits.max_work = 1_000_000;
    read_limits.buffer.max_extent_bytes = 64 * 1024;
    let mut view = Database::open_buffered_read_view_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        memory.clone(),
        read_limits,
    )
    .await
    .unwrap();
    if let Some(native) = native {
        view.set_native_run_limits(&contexts.query(), Some(native))
            .unwrap();
    }
    view
}

async fn collect(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    prepared: &PreparedGraphPattern<GraphValueRow>,
    cut: CommitSeq,
) -> (Vec<Vec<GraphValue>>, fgdb_gql::GqlExecutionStats) {
    let mut cursor = view
        .stream_graph_edges_governed_at(cx, prepared, cut, native_run_policy(100))
        .unwrap();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().await {
        rows.push(row.unwrap().row().values().to_vec());
    }
    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
    assert!(cursor.next().await.is_none());
    (rows, cursor.row_stats())
}

fn topology_expected(
    cut: u64,
    direction: usize,
    any: bool,
    labelled: bool,
) -> Vec<Vec<GraphValue>> {
    let mut rows = Vec::new();
    for (eid, src, dst, birth, retire, other_relation) in [
        (0, 4, 1, 4, None, false),
        (42, 2, 4, 5, None, false),
        (101, 1, 2, 1, None, false),
        (102, 1, 3, 1, Some(2), false),
        (103, 4, 1, 1, None, false),
        (104, 1, 2, 1, Some(5), false),
        (105, 1, 4, 3, None, true),
        (u128::MAX, 1, 1, 4, None, false),
    ] {
        if birth > cut || retire.is_some_and(|retire| retire <= cut) || (!any && other_relation) {
            continue;
        }
        let orientations = match direction {
            0 => vec![(src, dst)],
            1 => vec![(dst, src)],
            _ if src == dst => vec![(src, dst)],
            _ => vec![(src, dst), (dst, src)],
        };
        for (src, dst) in orientations {
            if labelled && cut >= 5 && src == 1 {
                continue;
            }
            rows.push(vec![
                GraphValue::Edge(EId(eid)),
                GraphValue::Vertex(VId(src)),
                GraphValue::Vertex(VId(dst)),
            ]);
        }
    }
    rows.sort();
    rows
}

#[test]
fn edge_topology_preserves_history_labels_directions_and_pinned_generations() {
    let ((), report) = run_async_under_lab(0x6275_ef01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        for native in [false, true] {
            let (vfs, mut writer, _) =
                native_run_history(&contexts.commit(), &cx, CommitSeq(0)).await;
            let memory = memory();
            let profile = native.then(native_run_limits);
            let mut pinned = open(&contexts, &vfs, &memory, profile).await;
            let mut edit = WriteBatch::new(RELATION);
            edit.set_vertex_label(VId(1), LabelId(1), false);
            edit.delete_edge(EId(104));
            edit.add_edge(
                EId(42),
                VId(2),
                VId(4),
                vec![(PROPERTY, CanonicalScalar::Int(42))],
            );
            assert_eq!(
                writer.write(&contexts.commit(), edit).await.unwrap(),
                CommitSeq(5)
            );
            writer.compact(&contexts.commit()).await.unwrap();
            let mut current = open(&contexts, &vfs, &memory, profile).await;
            for (view, frontier) in [(&mut pinned, 4), (&mut current, 5)] {
                for cut in 0..=frontier {
                    for direction in 0..3 {
                        for any in [false, true] {
                            for labelled in [false, true] {
                                let a = if labelled { "a:L" } else { "a" };
                                let r = if any { "r" } else { "r:R" };
                                let pattern = match direction {
                                    0 => format!("({a})-[{r}]->(b)"),
                                    1 => format!("({a})<-[{r}]-(b)"),
                                    _ => format!("({a})-[{r}]-(b)"),
                                };
                                let prepared = query(&format!("MATCH {pattern} RETURN r,a,b"));
                                let (actual, stats) =
                                    collect(view, &cx, &prepared, CommitSeq(cut)).await;
                                assert_eq!(
                                    actual,
                                    topology_expected(cut, direction, any, labelled)
                                );
                                assert_eq!(
                                    stats.snapshot_records,
                                    6 + u64::from(frontier == 5) + u64::from(any)
                                );
                            }
                        }
                    }
                }
                // A subsequent property query still receives the actual row;
                // the topology projection never populates a partial row cache.
                let prepared = query("MATCH (a)-[r:R]->(b) RETURN r,a,r.p");
                let (rows, _) = collect(view, &cx, &prepared, CommitSeq(frontier)).await;
                assert_eq!(
                    rows[0],
                    vec![
                        GraphValue::Edge(EId(0)),
                        GraphValue::Vertex(VId(4)),
                        GraphValue::Scalar(CanonicalScalar::Int(71))
                    ]
                );
            }
            drop(pinned);
            drop(current);
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

const A: VId = VId(1_u128 << 100);
const B: VId = VId(u128::MAX);

fn payload() -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(&"x".repeat(12 * 1024)).unwrap()
}

async fn large_fixture(
    contexts: &PurposeContexts,
) -> (MemVfs, Database<MemVfs>, AdjRunSegmentVersion) {
    let vfs = MemVfs::new().unwrap();
    let commit = contexts.commit();
    let mut writer = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(RELATION);
    for vid in [A, B] {
        first.create_vertex(
            vid,
            vec![LabelId(1)],
            vec![(PROPERTY, CanonicalScalar::Int(1))],
        );
    }
    first.add_edge(EId(0), A, B, vec![(PROPERTY, CanonicalScalar::Int(7))]);
    first.add_edge(
        EId(u128::MAX),
        B,
        B,
        vec![(PROPERTY, CanonicalScalar::Int(8))],
    );
    assert_eq!(writer.write(&commit, first).await.unwrap(), CommitSeq(1));
    let run = writer
        .seal_relation(
            &commit,
            &contexts.query(),
            RELATION,
            CommitSeq(0),
            small_run_limits(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
    // The bounded base remains small. Large post-seal edge properties stay in
    // independent sidecars; native base-image admission is unchanged.
    let mut second = WriteBatch::new(RELATION);
    for vid in [A, B] {
        second.set_vertex_property(vid, PROPERTY, Some(payload()));
    }
    for eid in [EId(0), EId(u128::MAX)] {
        second.set_edge_property(eid, PROPERTY, Some(payload()));
    }
    assert_eq!(writer.write(&commit, second).await.unwrap(), CommitSeq(2));
    (vfs, writer, run)
}

#[test]
fn property_free_edge_rows_fit_when_unused_payload_copies_would_exhaust_the_pool() {
    let ((), report) = run_async_under_lab(0x6275_ef02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (vfs, _writer, _) = large_fixture(&contexts).await;
        for native in [false, true] {
            let memory = memory();
            let mut view = open(&contexts, &vfs, &memory, native.then(small_run_limits)).await;
            let identities = std::iter::once("r".to_owned())
                .chain((1..32).map(|at| format!("a AS copy_{at}")))
                .collect::<Vec<_>>()
                .join(",");
            let properties = std::iter::once("r".to_owned())
                .chain((1..32).map(|at| format!("a.p AS copy_{at}")))
                .collect::<Vec<_>>()
                .join(",");
            let identity_query = query(&format!("MATCH (a:L)-[r:R]->(b:L) RETURN {identities}"));
            let property_query = query(&format!("MATCH (a:L)-[r:R]->(b:L) RETURN {properties}"));
            // Two normal decode workspaces, topology owners and native image
            // admission fit; 32 copies of the unrequested payloads do not.
            let occupied = memory
                .reserve(&cx, memory.available() - 3 * 1024 * 1024)
                .unwrap();
            let (rows, stats) = collect(&mut view, &cx, &identity_query, CommitSeq(2)).await;
            assert_eq!(stats.snapshot_records, 2);
            assert_eq!(rows.len(), 2);
            for (row, eid, vid) in [(&rows[0], EId(0), A), (&rows[1], EId(u128::MAX), B)] {
                assert_eq!(row[0], GraphValue::Edge(eid));
                assert_eq!(row[1..], vec![GraphValue::Vertex(vid); 31]);
            }
            let mut cursor = view
                .stream_graph_edges_governed(&cx, &property_query, native_run_policy(100))
                .unwrap();
            let error = cursor.next().await.unwrap().unwrap_err();
            assert!(
                matches!(
                    error,
                    GqlQueryError::Source(EdgeScanError::Source(
                        BufferedReadError::Memory(_)
                            | BufferedReadError::Buffer(
                                fgdb_strata::tiered::buffer::BufferError::Memory(_)
                            )
                    ))
                ),
                "{error:?}"
            );
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.next().await.is_none());
            drop(cursor);
            drop(occupied);
            let (rows, _) = collect(&mut view, &cx, &property_query, CommitSeq(2)).await;
            assert_eq!(rows.len(), 2);
            for row in rows {
                assert_eq!(row[1..], vec![GraphValue::Scalar(payload()); 31]);
            }
            drop(view);
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[derive(Default)]
struct File(Cursor<Vec<u8>>);
impl AsyncRead for File {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let at = self.0.position() as usize;
        let count = output
            .remaining()
            .min(self.0.get_ref().len().saturating_sub(at));
        if count != 0 {
            output.put_slice(&self.0.get_ref()[at..at + count]);
        }
        self.0.set_position((at + count) as u64);
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for File {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(std::io::Write::write(&mut self.0, bytes))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
impl AsyncSeek for File {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        at: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(self.0.seek(at))
    }
}

fn projection_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RELATION)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PROPERTY)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

fn selected_query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, projection_symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

async fn count(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    argument: &str,
) -> Result<u64, NativeAggregateSpoolError> {
    let values = aggregate_cells(
        view,
        cx,
        &format!("MATCH (a)-[r:R]->(b) RETURN count({argument}) AS n"),
    )
    .await?;
    let [GraphAggregateValue::Count(count)] = values.as_slice() else {
        panic!("one count cell")
    };
    Ok(*count)
}

async fn aggregate_cells(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    text: &str,
) -> Result<Vec<GraphAggregateValue>, NativeAggregateSpoolError> {
    let parameters = GqlParameters::new();
    let prepared = PreparedNativeRead::prepare(text, &parameters, projection_symbols)
        .unwrap()
        .prepare_buffered_aggregate(&parameters)
        .unwrap();
    let pool = MemoryPool::new(4 * 1024 * 1024, 0).unwrap();
    let limits = SpillLimits {
        max_file_bytes: 1024 * 1024,
        max_runs: 64,
        max_run_bytes: 512 * 1024,
    };
    let mut source = SpillFile::new(cx, File::default(), pool.clone(), limits)
        .await
        .unwrap();
    let mut partition = SpillFile::new(cx, File::default(), pool.clone(), limits)
        .await
        .unwrap();
    let mut destination = SpillFile::new(cx, File::default(), pool, limits)
        .await
        .unwrap();
    let (spool, _) = prepared
        .spool_in_view(
            view,
            cx,
            native_run_policy(100),
            &mut source,
            &mut partition,
            &mut destination,
            1,
            16,
            1,
            16,
            127,
            64 * 1024,
            100,
            1_000_000,
            None,
        )
        .await?;
    let mut reader = spool.reader(&mut destination, None);
    let row = reader.next_row(cx).await?.unwrap();
    let values = row.values().to_vec();
    drop(row);
    assert!(reader.next_row(cx).await?.is_none());
    assert_eq!(spool.row_stats().snapshot_records, 2);
    Ok(values)
}

#[test]
fn topology_and_count_inputs_skip_unused_sidecars_but_preserve_demand_and_selected_source_errors() {
    let ((), report) = run_async_under_lab(0x6275_ef03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        for native in [false, true] {
            let (vfs, writer, run) = large_fixture(&contexts).await;
            let memory = memory();
            let mut view = open(&contexts, &vfs, &memory, native.then(small_run_limits)).await;
            let store = BlockStore::open_existing_for_read_with_vfs(
                &contexts.commit(),
                vfs.clone(),
                vfs.database_dir(),
                [0xb1; 32],
                DatabaseSecurityNamespaceId([0xb2; 32]),
            )
            .await
            .unwrap();
            let root = store
                .get_root(&contexts.commit(), writer.partition_root().unwrap())
                .await
                .unwrap();
            let mut bodies = std::collections::BTreeMap::new();
            for reference in root.blocks {
                if reference.last_seq <= CommitSeq(1) {
                    continue;
                }
                let bytes = store
                    .get_bytes(
                        &contexts.commit(),
                        fgdb_strata::DeltaBlockVersion(reference.block_id),
                    )
                    .await
                    .unwrap();
                if let Some((id, _)) = fgdb_strata::decode_block_with_properties(&bytes).unwrap().1
                {
                    let path = store.path(id);
                    bodies.insert(path.clone(), vfs.read(&path).await.unwrap());
                }
            }
            assert!(!bodies.is_empty());
            for path in bodies.keys() {
                vfs.write(path, b"damaged unrequested edge sidecar")
                    .await
                    .unwrap();
            }
            let prepared = query("MATCH (a:L)-[r:R]->(b:L) RETURN r,a,b");
            let (rows, stats) = collect(&mut view, &cx, &prepared, CommitSeq(2)).await;
            assert_eq!(
                rows,
                vec![
                    vec![
                        GraphValue::Edge(EId(0)),
                        GraphValue::Vertex(A),
                        GraphValue::Vertex(B)
                    ],
                    vec![
                        GraphValue::Edge(EId(u128::MAX)),
                        GraphValue::Vertex(B),
                        GraphValue::Vertex(B)
                    ]
                ]
            );
            assert_eq!(stats.snapshot_records, 2);
            for argument in ["*", "r", "a"] {
                assert_eq!(count(&mut view, &cx, argument).await.unwrap(), 2);
            }
            assert!(count(&mut view, &cx, "r.p").await.is_err());
            for text in [
                "MATCH (a)-[r:R]->(b) RETURN r,a,r.p",
                "MATCH (a)-[r:R]->(b) WHERE r.p IS NOT NULL RETURN r,a,b",
            ] {
                let required = query(text);
                let mut cursor = view
                    .stream_graph_edges_governed(&cx, &required, native_run_policy(100))
                    .unwrap();
                assert!(cursor.next().await.unwrap().is_err(), "{text}");
                assert_eq!(cursor.state(), EdgeScanState::Failed);
                assert!(cursor.next().await.is_none());
            }
            for (path, bytes) in bodies {
                vfs.write(&path, &bytes).await.unwrap();
            }
            assert_eq!(count(&mut view, &cx, "r.p").await.unwrap(), 2);
            if native {
                vfs.write(&store.path(run.0), b"damaged selected run")
                    .await
                    .unwrap();
                let mut cursor = view
                    .stream_graph_edges_governed(&cx, &prepared, native_run_policy(100))
                    .unwrap();
                assert!(cursor.next().await.unwrap().is_err());
                assert_eq!(cursor.state(), EdgeScanState::Failed);
                assert!(cursor.next().await.is_none());
                drop(cursor);
                // Explicit reselection works; the failed native cursor did not
                // silently retry intact Tier D after selected-run corruption.
                view.set_native_run_limits(&cx, None).unwrap();
                assert_eq!(
                    collect(&mut view, &cx, &prepared, CommitSeq(2))
                        .await
                        .0
                        .len(),
                    2
                );
            }
            drop(view);
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn topology_candidates_retain_budgets_cancellation_and_output_ownership() {
    let ((), report) = run_async_under_lab(0x6275_ef04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (vfs, _writer, _) = large_fixture(&contexts).await;
        let prepared = query("MATCH (a)-[r:R]->(b) RETURN r,a,b");
        for native in [false, true] {
            let memory = memory();
            let profile = native.then(small_run_limits);
            let mut view = open(&contexts, &vfs, &memory, profile).await;
            let baseline = memory.used();
            let before = view.buffer_stats();
            let mut cursor = view
                .stream_graph_edges_governed(&cx, &prepared, native_run_policy(100))
                .unwrap();
            drop(cursor.next());
            cursor.close();
            assert!(cursor.next().await.is_none());
            drop(cursor);
            assert_eq!(view.buffer_stats(), before);
            assert_eq!(memory.used(), baseline);
            let mut cursor = view
                .stream_graph_edges_governed(&cx, &prepared, native_run_policy(0))
                .unwrap();
            assert!(
                matches!(cursor.next().await.unwrap(), Err(GqlQueryError::Rows(ref error)) if error.dimension == GqlBudgetDimension::SnapshotRecords && error.observed == 1)
            );
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.next().await.is_none());
            drop(cursor);
            assert_eq!(memory.used(), baseline);
            drop(view);
            assert_eq!(memory.used(), 0);
            // Measure and replay from the same cold owner state. Warming a
            // shared frame cache could otherwise shift the checkpoint count.
            let mut view = open(&contexts, &vfs, &memory, profile).await;
            let trace = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = cx.with_checkpoint_probe(Arc::clone(&trace));
            let mut cursor = view
                .stream_graph_edges_governed(&observed, &prepared, native_run_policy(100))
                .unwrap();
            drop(cursor.next().await.unwrap().unwrap());
            let first = trace.calls();
            assert!(first > 0);
            cursor.close();
            drop(cursor);
            drop(view);
            assert_eq!(memory.used(), 0);
            let mut view = open(&contexts, &vfs, &memory, profile).await;
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(first + 1)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut cursor = view
                .stream_graph_edges_governed(&interrupted, &prepared, native_run_policy(100))
                .unwrap();
            let held = cursor.next().await.unwrap().unwrap();
            let error = cursor.next().await.unwrap().unwrap_err();
            assert!(
                matches!(
                    error,
                    GqlQueryError::Interrupted(_)
                        | GqlQueryError::Source(EdgeScanError::Source(
                            BufferedReadError::Interrupted(_)
                        ))
                ),
                "{error:?}"
            );
            assert_eq!(probe.calls(), first + 1);
            assert_eq!(cursor.row_stats().result_rows, 1);
            assert_eq!(cursor.state(), EdgeScanState::Failed);
            assert!(cursor.next().await.is_none());
            drop(cursor);
            drop(view);
            assert!(memory.used() > 0);
            assert_eq!(
                held.row().values(),
                &[
                    GraphValue::Edge(EId(0)),
                    GraphValue::Vertex(A),
                    GraphValue::Vertex(B)
                ]
            );
            drop(held);
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_edge_properties_preserve_history_orientation_labels_and_removal() {
    let ((), report) = run_async_under_lab(0x6275_ef05, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let q = PropertyKeyId(2);
        let (vfs, mut writer, _) = large_fixture(&contexts).await;
        let mut add = WriteBatch::new(RELATION);
        add.set_edge_property(EId(0), q, Some(CanonicalScalar::Int(9)));
        add.set_edge_property(EId(u128::MAX), q, Some(CanonicalScalar::Int(11)));
        assert_eq!(
            writer.write(&contexts.commit(), add).await.unwrap(),
            CommitSeq(3)
        );
        let memories = [memory(), memory()];
        let mut pinned = Vec::new();
        for (native, memory) in [false, true].into_iter().zip(&memories) {
            pinned.push(open(&contexts, &vfs, memory, native.then(small_run_limits)).await);
        }
        let mut edit = WriteBatch::new(RELATION);
        edit.set_edge_property(EId(0), q, Some(CanonicalScalar::Int(21)));
        edit.set_edge_property(EId(u128::MAX), q, None);
        edit.set_vertex_label(B, LabelId(1), false);
        assert_eq!(
            writer.write(&contexts.commit(), edit).await.unwrap(),
            CommitSeq(4)
        );
        writer.compact(&contexts.commit()).await.unwrap();

        for ((native, mut pinned), memory) in
            [false, true].into_iter().zip(pinned).zip(&memories)
        {
            let mut current = open(&contexts, &vfs, memory, native.then(small_run_limits)).await;
            for (view, frontier) in [(&mut pinned, 3), (&mut current, 4)] {
                for cut in 0..=frontier {
                    for direction in 0..3 {
                        for labelled in [false, true] {
                            let a = if labelled { "a:L" } else { "a" };
                            let b = if labelled { "b:L" } else { "b" };
                            let pattern = match direction {
                                0 => format!("({a})-[r:R]->({b})"),
                                1 => format!("({a})<-[r:R]-({b})"),
                                _ => format!("({a})-[r:R]-({b})"),
                            };
                            let prepared =
                                selected_query(&format!("MATCH {pattern} RETURN r,a,b,r.q"));
                            let (rows, stats) =
                                collect(view, &cx, &prepared, CommitSeq(cut)).await;
                            let mut expected = Vec::new();
                            if cut > 0 && !(labelled && cut >= 4) {
                                for (eid, src, dst, value) in [
                                    (EId(0), A, B, if cut >= 4 { 21 } else { 9 }),
                                    (EId(u128::MAX), B, B, 11),
                                ] {
                                    let value = if cut < 3 || (eid == EId(u128::MAX) && cut >= 4) {
                                        CanonicalScalar::Null
                                    } else {
                                        CanonicalScalar::Int(value)
                                    };
                                    let orientations = match direction {
                                        0 => vec![(src, dst)],
                                        1 => vec![(dst, src)],
                                        _ if src == dst => vec![(src, dst)],
                                        _ => vec![(src, dst), (dst, src)],
                                    };
                                    for (src, dst) in orientations {
                                        expected.push(vec![
                                            GraphValue::Edge(eid),
                                            GraphValue::Vertex(src),
                                            GraphValue::Vertex(dst),
                                            GraphValue::Scalar(value.clone()),
                                        ]);
                                    }
                                }
                            }
                            expected.sort();
                            assert_eq!(
                                rows, expected,
                                "native {native}, cut {cut}, direction {direction}, labels {labelled}"
                            );
                            assert_eq!(stats.snapshot_records, 2);
                        }
                    }
                }
                let missing = selected_query(
                    "MATCH (a)-[r:R]->(b) WHERE r.q IS NULL RETURN r,a,b,r.q",
                );
                let (rows, _) = collect(view, &cx, &missing, CommitSeq(frontier)).await;
                if frontier == 3 {
                    assert!(rows.is_empty());
                } else {
                    assert_eq!(
                        rows,
                        vec![vec![
                            GraphValue::Edge(EId(u128::MAX)),
                            GraphValue::Vertex(B),
                            GraphValue::Vertex(B),
                            GraphValue::Scalar(CanonicalScalar::Null),
                        ]]
                    );
                }
                assert_eq!(
                    aggregate_cells(
                        view,
                        &cx,
                        "MATCH (a)-[r:R]->(b) RETURN count(r.q) AS n,sum(r.q) AS total",
                    )
                    .await
                    .unwrap(),
                    if frontier == 3 {
                        vec![GraphAggregateValue::Count(2), GraphAggregateValue::Integer(20)]
                    } else {
                        vec![GraphAggregateValue::Count(1), GraphAggregateValue::Integer(21)]
                    }
                );
            }
            drop(pinned);
            drop(current);
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_edge_property_queries_fit_without_unrelated_payloads_and_keep_output_guards() {
    let ((), report) = run_async_under_lab(0x6275_ef06, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (vfs, mut writer, _) = large_fixture(&contexts).await;
        let mut add = WriteBatch::new(RELATION);
        add.set_edge_property(EId(0), PropertyKeyId(2), Some(CanonicalScalar::Int(9)));
        add.set_edge_property(
            EId(u128::MAX),
            PropertyKeyId(2),
            Some(CanonicalScalar::Int(11)),
        );
        assert_eq!(
            writer.write(&contexts.commit(), add).await.unwrap(),
            CommitSeq(3)
        );
        let columns = std::iter::once("r".to_owned())
            .chain((1..32).map(|at| format!("r.q AS copy_{at}")))
            .collect::<Vec<_>>()
            .join(",");
        let prepared = selected_query(&format!(
            "MATCH (a:L)-[r:R]->(b:L) WHERE r.q>=0 RETURN {columns}"
        ));
        for native in [false, true] {
            let memory = memory();
            let mut view = open(&contexts, &vfs, &memory, native.then(small_run_limits)).await;
            let occupied = memory
                .reserve(&cx, memory.available() - 3 * 1024 * 1024)
                .unwrap();
            let (rows, stats) = collect(&mut view, &cx, &prepared, CommitSeq(3)).await;
            assert_eq!(stats.snapshot_records, 2);
            assert_eq!(rows.len(), 2);
            for (row, eid, value) in [(&rows[0], EId(0), 9), (&rows[1], EId(u128::MAX), 11)] {
                assert_eq!(row[0], GraphValue::Edge(eid));
                assert_eq!(
                    row[1..],
                    vec![GraphValue::Scalar(CanonicalScalar::Int(value)); 31]
                );
            }
            // A second edge key or any endpoint property keeps complete
            // records. Repeated selected columns cannot hide that demand.
            for field in ["r.p", "a.p"] {
                let required = selected_query(&format!(
                    "MATCH (a)-[r:R]->(b) RETURN {columns},{field} AS required"
                ));
                let mut cursor = view
                    .stream_graph_edges_governed(&cx, &required, native_run_policy(100))
                    .unwrap();
                let error = cursor.next().await.unwrap().unwrap_err();
                assert!(
                    matches!(
                        error,
                        GqlQueryError::Source(EdgeScanError::Source(
                            BufferedReadError::Memory(_)
                                | BufferedReadError::Buffer(
                                    fgdb_strata::tiered::buffer::BufferError::Memory(_)
                                )
                        ))
                    ),
                    "{field}: {error:?}"
                );
                assert_eq!(cursor.state(), EdgeScanState::Failed);
                assert!(cursor.next().await.is_none());
            }
            drop(occupied);
            let complete = selected_query(&format!(
                "MATCH (a)-[r:R]->(b) RETURN {columns},r.p AS required"
            ));
            let (rows, _) = collect(&mut view, &cx, &complete, CommitSeq(3)).await;
            assert_eq!(rows.len(), 2);
            assert_eq!(
                rows[0].last(),
                Some(&GraphValue::Scalar(payload())),
                "complete fallback still exposes its required payload"
            );
            let mut cursor = view
                .stream_graph_edges_governed(&cx, &prepared, native_run_policy(100))
                .unwrap();
            let held = cursor.next().await.unwrap().unwrap();
            drop(cursor);
            drop(view);
            assert!(memory.used() > 0, "selected output owns its independent guard");
            assert_eq!(held.row().values()[0], GraphValue::Edge(EId(0)));
            assert_eq!(
                held.row().values()[1],
                GraphValue::Scalar(CanonicalScalar::Int(9))
            );
            drop(held);
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
