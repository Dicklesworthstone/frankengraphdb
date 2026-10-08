//! Buffered storage and native blocking operators share one admission boundary.
//! These laws exercise real sources; the final one isolates an awaited append
//! so it can observe the source row and encoder reservations independently.

use super::*;
use crate::{BufferLimits, BufferedReadLimits, BufferedReadView, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphAggregateValue, GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::{MemoryPool, SpillLimits, SpillStats};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

const PAGE: usize = 63;
const WORK: u64 = 100_000_000;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn policy(results: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(4, results, WORK, 1_000_000)
}
fn payload(id: u128) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(&format!("{id:02}-{}", "payload-é".repeat(64))).unwrap()
}
async fn fixture(cx: &CommitCx) -> MemVfs {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 1..=4 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                (PropertyKeyId(2), payload(id)),
            ],
        );
    }
    for (id, from, to, value) in [(1, 1, 2, 7), (2, 1, 2, 8), (3, 2, 2, 9), (4, 2, 3, 10)] {
        batch.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
    let mut update = WriteBatch::new(RelationId(1));
    update.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(13)));
    update.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(17)));
    assert_eq!(db.write(cx, update).await.unwrap(), CommitSeq(2));
    drop(db);
    vfs
}
async fn view(cx: &CommitCx, vfs: &MemVfs, pool: &MemoryPool) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        cx,
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        pool.clone(),
        BufferedReadLimits {
            max_root_bytes: 8192,
            max_source_bytes: 16_000_000,
            max_blocks: 128,
            max_vertex_patches: 128,
            max_work: WORK as usize,
            buffer: BufferLimits {
                max_frames: 2,
                max_ghost_entries: 4,
                max_extent_bytes: 4096,
            },
        },
    )
    .await
    .unwrap()
}

#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    writes: usize,
    pending_write: bool,
}
#[derive(Clone, Default)]
struct File(Arc<Mutex<FileState>>);
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        let at = state.bytes.position() as usize;
        let count = output
            .remaining()
            .min(state.bytes.get_ref().len().saturating_sub(at));
        if count != 0 {
            output.put_slice(&state.bytes.get_ref()[at..at + count]);
            state.bytes.set_position((at + count) as u64);
        }
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for File {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        state.writes += 1;
        if state.pending_write {
            Poll::Pending
        } else {
            Poll::Ready(std::io::Write::write(&mut state.bytes, bytes))
        }
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
impl AsyncSeek for File {
    fn poll_seek(self: Pin<&mut Self>, _: &mut Context<'_>, at: SeekFrom) -> Poll<io::Result<u64>> {
        Poll::Ready(self.0.lock().unwrap().bytes.seek(at))
    }
}
async fn file(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, File) {
    let backing = File::default();
    let file = SpillFile::new(
        cx,
        backing.clone(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 8_000_000,
            max_runs: 256,
            max_run_bytes: 1_000_000,
        },
    )
    .await
    .unwrap();
    (file, backing)
}
async fn contents(
    spool: &NativeResultSpool,
    file: &mut SpillFile<File>,
    cx: &QueryCx,
) -> Vec<Vec<u8>> {
    let mut reader = spool.reader(file);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await.unwrap() {
        rows.push(row.as_ref().to_vec());
    }
    assert_eq!(reader.state(), ScanState::Exhausted);
    rows
}
fn scalar_frames(values: &[i64]) -> Vec<Vec<u8>> {
    values
        .iter()
        .map(|&value| {
            GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(value))])
                .canonical_bytes()
                .unwrap()
        })
        .collect()
}

#[test]
fn bound_historical_order_keeps_hidden_keys_and_cumulative_encoding_sort_window_work() {
    let ((), report) = run_async_under_lab(0xb0ff_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        for (text, floor, inputs, kind, expected) in [
            (
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 WHERE n.p > $floor RETURN n.p AS value ORDER BY n.q DESC SKIP $skip LIMIT $take",
                1,
                3,
                ScanKind::Vertex,
                vec![3, 2],
            ),
            (
                "MATCH (a)-[r:R]-(b) FOR SYSTEM_TIME AS OF SEQ 1 WHERE r.p >= $floor RETURN r.p AS value ORDER BY a.p DESC,value ASC SKIP $skip LIMIT $take",
                7,
                7,
                ScanKind::Edge,
                vec![7, 8],
            ),
        ] {
            let parameters = GqlParameters::new()
                .with_int64("floor", floor)
                .unwrap()
                .with_int64("skip", 1)
                .unwrap()
                .with_int64("take", 2)
                .unwrap();
            let prepared = PreparedNativeRead::prepare(text, &parameters, resolve)
                .unwrap()
                .prepare_buffered_order(&parameters)
                .unwrap();
            drop(parameters); // The admitted plan owns its one-time binding.
            let expected = scalar_frames(&expected);
            let mut exact = None;
            for trial in 0..3 {
                let source_pool = MemoryPool::new(8_000_000, 0).unwrap();
                let spill_pool = MemoryPool::new(262_144, 0).unwrap();
                let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
                assert_eq!(view.frontier(), CommitSeq(2));
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let limit = exact.map_or(WORK, |work| work - u64::from(trial == 2));
                let result = prepared
                    .spool_in_view(
                        &mut view,
                        &cx,
                        policy(2),
                        &mut scratch,
                        &mut destination,
                        1,
                        16,
                        PAGE,
                        4096,
                        inputs,
                        limit,
                    )
                    .await;
                if trial == 2 {
                    assert!(matches!(result, Err(NativeSpoolError::SortWorkLimit {
                        attempted, limit: reported,
                    }) if reported == limit && attempted > limit));
                } else {
                    let (spool, spent) = result.unwrap_or_else(|error| panic!("{text}: {error}"));
                    assert_eq!(spool.snapshot_seq(), CommitSeq(1));
                    assert_eq!(spool.kind(), kind);
                    assert_eq!(spool.columns(), &["value"]);
                    assert_eq!(spool.row_stats().snapshot_records, 4);
                    assert_eq!(spool.row_count(), 2);
                    assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
                    assert!(scratch.stats().published_runs > 1);
                    if let Some(exact) = exact {
                        assert_eq!(spent, exact);
                    }
                    exact = Some(spent);
                }
                assert_eq!(spill_pool.used(), 0);
                drop(view);
                assert_eq!(source_pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_computed_aggregates_charge_only_final_results_and_keep_exact_work() {
    let ((), report) = run_async_under_lab(0xb0ff_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        for (text, inputs, kind, total) in [
            (
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS rows,SUM(n.p*$factor) AS total",
                4,
                ScanKind::Vertex,
                30,
            ),
            (
                "MATCH (a)-[r:R]-(b) FOR SYSTEM_TIME AS OF SEQ 1 WHERE a <> b RETURN COUNT(*) AS rows,SUM(r.p*$factor) AS total",
                6,
                ScanKind::Edge,
                150,
            ),
        ] {
            let parameters = GqlParameters::new().with_int64("factor", 3).unwrap();
            let prepared = PreparedNativeRead::prepare(text, &parameters, resolve)
                .unwrap()
                .prepare_buffered_aggregate(&parameters)
                .unwrap();
            drop(parameters);
            let mut exact = None;
            for trial in 0..3 {
                let source_pool = MemoryPool::new(8_000_000, 0).unwrap();
                let spill_pool = MemoryPool::new(262_144, 0).unwrap();
                let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
                let (mut source, _) = file(&cx, &spill_pool).await;
                let (mut partition, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let limit = exact.map_or(WORK, |work| work - u64::from(trial == 2));
                let result = prepared
                    .spool_in_view(
                        &mut view,
                        &cx,
                        policy(1),
                        &mut source,
                        &mut partition,
                        &mut destination,
                        1,
                        16,
                        1,
                        16,
                        PAGE,
                        4096,
                        inputs,
                        limit,
                        None,
                    )
                    .await;
                if trial == 2 {
                    assert!(matches!(result, Err(NativeAggregateSpoolError::Spool(
                        NativeSpoolError::SortWorkLimit { attempted, limit: reported },
                    )) if reported == limit && attempted > limit));
                } else {
                    let (spool, spent) = result.unwrap_or_else(|error| panic!("{text}: {error}"));
                    assert_eq!(spool.snapshot_seq(), CommitSeq(1));
                    assert_eq!(spool.kind(), kind);
                    assert_eq!(spool.columns(), &["rows", "total"]);
                    assert_eq!(spool.row_stats().snapshot_records, 4);
                    assert_eq!(spool.row_count(), 1);
                    let mut reader = spool.reader(&mut destination, None);
                    let row = reader.next_row(&cx).await.unwrap().unwrap();
                    assert_eq!(
                        row.values(),
                        &[
                            GraphAggregateValue::Count(inputs),
                            GraphAggregateValue::Integer(total),
                        ]
                    );
                    drop(row);
                    assert!(reader.next_row(&cx).await.unwrap().is_none());
                    drop(reader);
                    if let Some(exact) = exact {
                        assert_eq!(spent, exact);
                    }
                    exact = Some(spent);
                }
                assert_eq!(spill_pool.used(), 0);
                drop(view);
                assert_eq!(source_pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

async fn writer_reservation(cx: &QueryCx) -> usize {
    let pool = MemoryPool::new(262_144, 0).unwrap();
    let (mut file, _) = file(cx, &pool).await;
    let writer = file.paged_writer(cx, PAGE).unwrap();
    let bytes = pool.used();
    drop(writer);
    assert_eq!(pool.used(), 0);
    bytes
}

#[test]
fn buffered_row_cap_and_encoding_work_precede_encoder_memory_and_scratch_writes() {
    let ((), report) = run_async_under_lab(0xb0ff_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let parameters = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n:L) RETURN n.q AS payload ORDER BY payload DESC LIMIT 0",
            &parameters,
            resolve,
        )
        .unwrap()
        .prepare_buffered_order(&parameters)
        .unwrap();
        let encoded_len = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(payload(1))])
            .canonical_bytes()
            .unwrap()
            .len();
        let writer_bytes = writer_reservation(&cx).await;
        for (case, row_limit, work_limit) in [(0, 1, 1), (1, 4096, 1), (2, 4096, WORK)] {
            let source_pool = MemoryPool::new(8_000_000, 0).unwrap();
            // This admits precisely the writer, and no encoder reservation.
            let spill_pool = MemoryPool::new(writer_bytes, 0).unwrap();
            let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, backing) = file(&cx, &spill_pool).await;
            let error = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(0),
                    &mut scratch,
                    &mut destination,
                    1,
                    16,
                    PAGE,
                    row_limit,
                    4,
                    work_limit,
                )
                .await
                .unwrap_err();
            match case {
                0 => assert!(
                    matches!(error, NativeSpoolError::RowTooLarge { bytes, limit: 1 }
                    if bytes == encoded_len)
                ),
                1 => assert!(
                    matches!(error, NativeSpoolError::SortWorkLimit { attempted, limit: 1 }
                    if attempted > 1)
                ),
                _ => assert!(matches!(
                    error,
                    NativeSpoolError::Spill(SpillError::Memory(_))
                )),
            }
            assert_eq!(destination.stats().reserved_runs, 1);
            assert_eq!(destination.stats().published_runs, 0);
            assert_eq!(destination.stats().reserved_bytes, 0);
            assert_eq!(backing.0.lock().unwrap().writes, 0);
            assert_eq!(scratch.stats(), SpillStats::default());
            assert_eq!(spill_pool.used(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

struct OneGuardedInput {
    row: Option<SpoolRow>,
    exhausted: bool,
}
impl SpoolInput for OneGuardedInput {
    async fn pull(&mut self) -> Option<Result<SpoolRow, NativeSpoolError>> {
        if let Some(row) = self.row.take() {
            Some(Ok(row))
        } else {
            self.exhausted = true;
            None
        }
    }
    fn spool_state(&self) -> ScanState {
        if self.exhausted {
            ScanState::Exhausted
        } else {
            ScanState::Open
        }
    }
    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (
            CommitSeq(1),
            ScanKind::Vertex,
            GqlExecutionStats {
                snapshot_records: 1,
                result_rows: u64::from(self.row.is_none()),
            },
            GlaExecutionStats::default(),
        )
    }
}

#[test]
fn dropping_pending_input_append_refunds_source_row_and_encoder_together() {
    let ((), report) = run_async_under_lab(0xb0ff_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let source_pool = MemoryPool::new(1024, 0).unwrap();
        let spill_pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut file, backing) = file(&cx, &spill_pool).await;
        backing.0.lock().unwrap().pending_write = true;
        let row = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(payload(1))]);
        let encoded_len = row.canonical_bytes().unwrap().len();
        let input = OneGuardedInput {
            row: Some(SpoolRow::buffered((
                row,
                source_pool.reserve(&cx, 1024).unwrap(),
            ))),
            exhausted: false,
        };
        let mut work = sort::Work {
            cx: &cx,
            used: 0,
            limit: WORK,
        };
        let mut future = Box::pin(drain(
            &cx,
            vec!["payload".into()],
            1,
            input,
            &mut file,
            PAGE,
            4096,
            Some(&mut work),
        ));
        let mut task = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut task).is_pending());
        assert!(backing.0.lock().unwrap().writes > 0);
        assert_eq!(
            source_pool.used(),
            1024,
            "the source row is owned by the pending append"
        );
        assert!(
            spill_pool.used() >= encoded_len * 8 + 128,
            "the native encoder reservation survives the awaited page write"
        );
        drop(future);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(spill_pool.used(), 0);
        assert!(file.is_poisoned());
        assert_eq!(file.stats().published_runs, 0);
        assert!(
            file.stats().reserved_bytes > 0,
            "cancellation does not refund disk admission"
        );
        assert!(
            work.used >= encoded_len as u64,
            "cancellation does not refund work"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
