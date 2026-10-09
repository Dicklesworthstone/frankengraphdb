//! Real buffered graph joins through the existing external ordering/stage host.
//! Literal fixture expectations do not execute another copy of the join driver.

use super::*;
use crate::{BufferLimits, BufferedReadLimits, Database, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::{MemoryPool, SpillLimits};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

const WORK: u64 = 100_000_000;
const MATCH: &str = "MATCH (a)-[r:R]->(b)-[s:R]->(c)";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "missing") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, rows, WORK, WORK)
}
fn prepare(text: &str) -> PreparedBufferedOrder {
    let parameters = GqlParameters::new();
    PreparedNativeRead::prepare(text, &parameters, resolve)
        .unwrap()
        .prepare_buffered_order(&parameters)
        .unwrap()
}
async fn fixture(cx: &CommitCx) -> MemVfs {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(RelationId(1));
    for (id, value) in [(1, 3), (2, 1), (3, 4)] {
        first.create_vertex(
            VId(id),
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    for (id, from, to, value) in [(1, 1, 2, 7), (2, 1, 2, 8), (3, 2, 2, 9), (4, 2, 3, 10)] {
        first.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, first).await.unwrap(), CommitSeq(1));
    let mut update = WriteBatch::new(RelationId(1));
    update.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(14)));
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
                max_frames: 1,
                max_ghost_entries: 2,
                max_extent_bytes: 16 * 1024,
            },
        },
    )
    .await
    .unwrap()
}

// An observable awaitable scratch file. Graph storage is the real MemVfs;
// this test file changes only when a spill write can finish or fail.
#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    writes: usize,
    pending: bool,
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
        if state.pending {
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
            max_runs: 512,
            max_run_bytes: 1_000_000,
        },
    )
    .await
    .unwrap();
    (file, backing)
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn frames(values: Vec<GraphValue>) -> Vec<Vec<u8>> {
    values
        .into_iter()
        .map(|value| {
            GraphValueRow::from_owned_values(vec![value])
                .canonical_bytes()
                .unwrap()
        })
        .collect()
}
async fn contents(
    spool: &NativeResultSpool,
    file: &mut SpillFile<File>,
    cx: &QueryCx,
) -> Vec<Vec<u8>> {
    let mut reader = spool.reader(file);
    let mut values = Vec::new();
    while let Some(row) = reader.next_row(cx).await.unwrap() {
        values.push(row.as_ref().to_vec());
    }
    assert_eq!(reader.state(), ScanState::Exhausted);
    values
}

#[test]
fn cold_multi_hop_order_hidden_keys_distinct_and_computed_stages_preserve_exact_historical_rows() {
    let ((), report) = run_async_under_lab(0xc01d_3001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let source_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let spill_pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for cut in [1, 2] {
            let end = if cut == 1 { 4 } else { 14 };
            let pairs: [(i64, i64); 6] = if cut == 1 {
                [(16, 1), (17, 4), (17, 1), (18, 4), (18, 1), (19, 4)]
            } else {
                [(26, 1), (27, 14), (17, 1), (18, 14), (18, 1), (19, 14)]
            };
            let mut maps: Vec<_> = pairs
                .into_iter()
                .map(|(sum, target)| GraphValue::Map {
                    keys: vec!["target".into(), "total".into()].into_boxed_slice(),
                    values: vec![scalar(target), scalar(sum)].into_boxed_slice(),
                })
                .collect();
            maps.sort();
            let cases = [
                (
                    "RETURN c.p AS value ORDER BY s.p DESC SKIP 1 LIMIT 4",
                    frames(vec![scalar(end), scalar(end), scalar(1), scalar(1)]),
                ),
                (
                    "RETURN DISTINCT c.p AS value ORDER BY value DESC",
                    frames(vec![scalar(end), scalar(1)]),
                ),
                (
                    "RETURN r.p+s.p AS value ORDER BY value DESC SKIP 1 LIMIT 3",
                    frames(if cut == 1 {
                        vec![scalar(18), scalar(18), scalar(17)]
                    } else {
                        vec![scalar(26), scalar(19), scalar(18)]
                    }),
                ),
                (
                    "WITH s.p AS x ORDER BY x DESC SKIP 1 LIMIT 3 RETURN x*2 AS value",
                    frames(vec![scalar(20), scalar(20), scalar(18)]),
                ),
                (
                    "RETURN {target:c.p,total:r.p+s.p} AS value ORDER BY value",
                    frames(maps),
                ),
                (
                    "RETURN DISTINCT a.missing AS value ORDER BY value",
                    frames(vec![GraphValue::Scalar(CanonicalScalar::Null)]),
                ),
            ];
            for (tail, expected) in cases {
                let text = format!("{MATCH} FOR SYSTEM_TIME AS OF SEQ {cut} {tail}");
                let prepared = prepare(&text);
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let result = prepared
                    .spool_in_view(
                        &mut view,
                        &cx,
                        policy(expected.len() as u64),
                        &mut scratch,
                        &mut destination,
                        1,
                        32,
                        31,
                        8192,
                        100,
                        WORK,
                    )
                    .await;
                let (spool, _) = result.unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
                assert_eq!(spool.kind(), ScanKind::Edge);
                assert_eq!(spool.row_count(), expected.len() as u64);
                assert_eq!(spool.columns(), &["value"]);
                assert_eq!(
                    contents(&spool, &mut destination, &cx).await,
                    expected,
                    "{text}"
                );
                assert!(
                    spool.row_stats().snapshot_records > 4,
                    "nested reads count too"
                );
                assert_eq!(source_pool.used(), baseline);
                assert_eq!(spill_pool.used(), 0);
            }
            let text = format!(
                "{MATCH}-[t:R]->(d) FOR SYSTEM_TIME AS OF SEQ {cut} RETURN d.p AS value ORDER BY value DESC SKIP 1 LIMIT 3"
            );
            let prepared = prepare(&text);
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(3),
                    &mut scratch,
                    &mut destination,
                    1,
                    32,
                    31,
                    8192,
                    100,
                    WORK,
                )
                .await
                .unwrap();
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                frames(vec![scalar(end), scalar(end), scalar(1)])
            );
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(spill_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_join_spill_keeps_input_quota_separate_and_carries_native_and_external_work_across_phases() {
    let ((), report) = run_async_under_lab(0xc01d_3002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let source_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let spill_pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let prepared = prepare(&format!(
            "{MATCH} RETURN c.p AS value ORDER BY value DESC LIMIT 2"
        ));
        let mut exact = None;
        for trial in 0..5 {
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let mut allowance = policy(2);
            let (external, native) = exact.unwrap_or((WORK, WORK));
            if trial == 3 {
                allowance.evaluator.max_work_units = native - 1;
            }
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    allowance,
                    &mut scratch,
                    &mut destination,
                    1,
                    32,
                    31,
                    8192,
                    if trial == 4 { 5 } else { 6 },
                    if trial == 2 { external - 1 } else { external },
                )
                .await;
            match trial {
                2 => assert!(matches!(
                    result,
                    Err(NativeSpoolError::SortWorkLimit { .. })
                )),
                3 => assert!(
                    matches!(result, Err(NativeSpoolError::BufferedExecute(error))
                    if matches!(*error, GqlQueryError::Evaluator(_)))
                ),
                4 => assert!(
                    matches!(result, Err(NativeSpoolError::BufferedExecute(error))
                    if matches!(*error, GqlQueryError::Rows(_)))
                ),
                _ => {
                    let (spool, work) = result.unwrap();
                    assert_eq!(
                        contents(&spool, &mut destination, &cx).await,
                        frames(vec![scalar(14), scalar(14)])
                    );
                    let counters = (work, spool.evaluator_stats().work_units);
                    if let Some(old) = exact {
                        assert_eq!(counters, old);
                    }
                    exact = Some(counters);
                }
            }
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_join_complete_stage_errors_are_not_hidden_by_downstream_zero_limit() {
    let ((), report) = run_async_under_lab(0xc01d_3003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let source_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let spill_pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let prepared = prepare(&format!(
            "{MATCH} FOR SYSTEM_TIME AS OF SEQ 1 WITH 12/(4-c.p) AS value RETURN 1/(value-4) AS result LIMIT 0"
        ));
        let (mut scratch, _) = file(&cx, &spill_pool).await;
        let (mut destination, _) = file(&cx, &spill_pool).await;
        let error = prepared
            .spool_in_view(
                &mut view,
                &cx,
                policy(0),
                &mut scratch,
                &mut destination,
                1,
                32,
                31,
                8192,
                100,
                WORK,
            )
            .await
            .unwrap_err();
        // The child's c.p=4 is the fourth canonical row. Fusing the parent
        // would instead fail at its very first row (value=4). Preserve the barrier.
        assert!(
            matches!(
                error,
                NativeSpoolError::SetExecution(fgdb_gql::GraphSetExecutionError::Projection {
                    row: 3,
                    ..
                })
            ),
            "{error:?}"
        );
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropped_pending_join_spill_refunds_native_records_output_and_encoder_but_not_disk_attempts() {
    let ((), report) = run_async_under_lab(0xc01d_3004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let source_pool = MemoryPool::new(32_000_000, 0).unwrap();
        let spill_pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let prepared = prepare(&format!("{MATCH} RETURN c.p AS value ORDER BY value"));
        let (mut scratch, backing) = file(&cx, &spill_pool).await;
        let (mut destination, _) = file(&cx, &spill_pool).await;
        {
            let mut state = backing.0.lock().unwrap();
            state.pending = true;
            state.writes = 0;
        }
        let mut future = prepared.spool_in_view(
            &mut view,
            &cx,
            policy(100),
            &mut scratch,
            &mut destination,
            1,
            32,
            1,
            8192,
            100,
            WORK,
        );
        std::future::poll_fn(|task| match future.as_mut().poll(task) {
            Poll::Ready(_) => panic!("the blocked append cannot complete"),
            Poll::Pending if backing.0.lock().unwrap().writes > 0 => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        })
        .await;
        assert!(source_pool.used() > baseline);
        assert!(spill_pool.used() > 0);
        drop(future);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        assert!(scratch.is_poisoned());
        assert_eq!(scratch.stats().published_runs, 0);
        assert!(scratch.stats().reserved_bytes > 0);
        assert_eq!(destination.stats().published_runs, 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
