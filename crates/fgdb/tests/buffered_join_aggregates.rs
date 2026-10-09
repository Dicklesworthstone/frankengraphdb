//! Cold fixed-hop queries into the existing external numeric reducer. Literal
//! summaries below come from the fixture's finite relation, not the join cursor.

use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys, MemVfs, MemoryPool,
    NativeAggregateSpool, NativeAggregateSpoolError, NativeSpoolError, PreparedBufferedAggregate,
    PreparedNativeRead, WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateValue, GraphSymbol, GraphSymbolKind,
};
use fgdb_strata::tiered::memory::{SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use std::future::{Future, poll_fn};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

const WORK: u64 = 100_000_000;
const MATCH: &str = "MATCH (a)-[r:R]->(b)-[s:R]->(c)";
const TOTAL: &str = "RETURN COUNT(*) AS rows, SUM(r.p+s.p) AS total";
type LogicalRows = Vec<(Vec<GraphValue>, Vec<GraphAggregateValue>)>;
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedBufferedAggregate {
    let params = GqlParameters::new();
    PreparedNativeRead::prepare(text, &params, resolve)
        .unwrap()
        .prepare_buffered_aggregate(&params)
        .unwrap()
}
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, rows, WORK, WORK)
}
async fn fixture(cx: &CommitCx) -> MemVfs {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(RelationId(1));
    for id in 1..=4 {
        first.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                (
                    PropertyKeyId(2),
                    CanonicalScalar::ucs_basic_text(&format!("{id}:{}", "payload".repeat(80)))
                        .unwrap(),
                ),
            ],
        );
    }
    for (id, from, to, value) in [
        (1, 1, 2, 7),
        (2, 1, 2, 8),
        (3, 2, 2, 9),
        (4, 2, 3, 10),
        (5, 3, 4, 11),
    ] {
        first.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, first).await.unwrap(), CommitSeq(1));
    let mut second = WriteBatch::new(RelationId(1));
    second.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(17)));
    second.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(13)));
    assert_eq!(db.write(cx, second).await.unwrap(), CommitSeq(2));
    let mut third = WriteBatch::new(RelationId(1));
    third.delete_vertex(VId(4));
    assert_eq!(db.write(cx, third).await.unwrap(), CommitSeq(3));
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
            max_root_bytes: 64 * 1024,
            max_source_bytes: 16_000_000,
            max_blocks: 512,
            max_vertex_patches: 512,
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
            max_file_bytes: 64_000_000,
            max_runs: 2048,
            max_run_bytes: 4_000_000,
        },
    )
    .await
    .unwrap();
    (file, backing)
}
async fn contents(
    spool: &NativeAggregateSpool,
    file: &mut SpillFile<File>,
    cx: &QueryCx,
) -> LogicalRows {
    let mut reader = spool.reader(file, None);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await.unwrap() {
        rows.push((row.keys().to_vec(), row.values().to_vec()));
    }
    rows
}
fn summary(count: u64, sum: i128) -> LogicalRows {
    vec![(
        vec![],
        vec![
            GraphAggregateValue::Count(count),
            GraphAggregateValue::Integer(sum),
        ],
    )]
}

#[test]
fn cold_join_aggregates_preserve_history_retirement_grouping_distinct_and_having() {
    let ((), report) = run_async_under_lab(0xc01d_a001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        for (cut, count, sum, triples, triple_sum, distinct_sum, winner, winning_sum) in [
            (1, 7, 126, 9, 246, 30, 3, 54),
            (2, 7, 146, 9, 276, 30, 13, 64),
            (3, 6, 125, 6, 179, 19, 13, 64),
        ] {
            for (text, expected) in [
                (
                    format!("{MATCH} FOR SYSTEM_TIME AS OF SEQ {cut} {TOTAL}"),
                    summary(count, sum),
                ),
                (
                    format!(
                        "{MATCH} FOR SYSTEM_TIME AS OF SEQ {cut} RETURN COUNT(*) AS rows, COUNT(DISTINCT s.p) AS support, SUM(DISTINCT s.p) AS total"
                    ),
                    vec![(
                        vec![],
                        vec![
                            GraphAggregateValue::Count(count),
                            GraphAggregateValue::Count(if cut == 3 { 2 } else { 3 }),
                            GraphAggregateValue::Integer(distinct_sum),
                        ],
                    )],
                ),
                (
                    format!(
                        "{MATCH}-[t:R]->(d) FOR SYSTEM_TIME AS OF SEQ {cut} RETURN COUNT(*) AS rows, SUM(r.p+s.p+t.p) AS total"
                    ),
                    summary(triples, triple_sum),
                ),
                (
                    format!(
                        "{MATCH} FOR SYSTEM_TIME AS OF SEQ {cut} RETURN c.p AS destination, COUNT(*) AS rows, SUM(r.p+s.p) AS total HAVING COUNT(*) >= 2 ORDER BY total DESC LIMIT 1"
                    ),
                    vec![(
                        vec![GraphValue::Scalar(CanonicalScalar::Int(winner))],
                        vec![
                            GraphAggregateValue::Count(3),
                            GraphAggregateValue::Integer(winning_sum),
                        ],
                    )],
                ),
            ] {
                let prepared = prepare(&text);
                let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
                let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
                let mut view = view(&commit, &vfs, &source_pool).await;
                let baseline = source_pool.used();
                let (mut source, _) = file(&cx, &spill_pool).await;
                let (mut partition, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                // A single resident group forces grouped input through the
                // real external partitioner. Final quota is ONE, not input N.
                let (spool, _) = prepared
                    .spool_in_view(
                        &mut view,
                        &cx,
                        policy(1),
                        &mut source,
                        &mut partition,
                        &mut destination,
                        1,
                        512,
                        1,
                        512,
                        1024,
                        16 * 1024,
                        1000,
                        WORK,
                        None,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
                assert_eq!(spool.row_count(), 1);
                assert_eq!(spool.row_stats().result_rows, 1);
                assert!(
                    spool.row_stats().snapshot_records > 5,
                    "nested reads share the source meter"
                );
                assert_eq!(
                    contents(&spool, &mut destination, &cx).await,
                    expected,
                    "{text}"
                );
                assert_eq!(source_pool.used(), baseline);
                assert_eq!(spill_pool.used(), 0);
                drop(view);
                assert_eq!(source_pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn private_input_and_final_output_limits_remain_distinct_and_all_work_is_cumulative() {
    let ((), report) = run_async_under_lab(0xc01d_a002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare(&format!("{MATCH} {TOTAL}"));
        let mut exact = None;
        for case in 0..8 {
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let mut budget = policy(if case == 2 { 0 } else { 1 });
            let mut work = WORK;
            if let Some((records, native_work, scratch, external)) = exact {
                budget = GqlQueryPolicy::new(
                    records - u64::from(case == 4),
                    if case == 2 { 0 } else { 1 },
                    native_work - u64::from(case == 5),
                    scratch - u64::from(case == 6),
                );
                work = external - u64::from(case == 7);
            }
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    budget,
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    16 * 1024,
                    if case == 3 { 5 } else { 6 },
                    work,
                    None,
                )
                .await;
            if case < 2 {
                let (spool, spent) = result.unwrap();
                assert_eq!(
                    contents(&spool, &mut destination, &cx).await,
                    summary(6, 125)
                );
                let stats = spool.evaluator_stats();
                let measured = (
                    spool.row_stats().snapshot_records,
                    stats.work_units,
                    stats.scratch_entries,
                    spent,
                );
                if let Some(exact) = exact {
                    assert_eq!(measured, exact);
                }
                exact = Some(measured);
            } else {
                let error = result.unwrap_err();
                match (case, error) {
                    (2, NativeAggregateSpoolError::Execute(error)) => {
                        assert!(matches!(*error, GqlQueryError::Rows(_)));
                    }
                    (
                        3,
                        NativeAggregateSpoolError::InputRows {
                            attempted: 6,
                            limit: 5,
                        },
                    ) => {}
                    (4, NativeAggregateSpoolError::BufferedExecute(error)) => {
                        assert!(matches!(*error, GqlQueryError::Rows(_)));
                    }
                    (5 | 6, NativeAggregateSpoolError::Execute(error)) => {
                        assert!(matches!(*error, GqlQueryError::Evaluator(_)));
                    }
                    (5 | 6, NativeAggregateSpoolError::BufferedExecute(error)) => {
                        assert!(matches!(*error, GqlQueryError::Evaluator(_)));
                    }
                    (
                        7,
                        NativeAggregateSpoolError::Spool(NativeSpoolError::SortWorkLimit {
                            ..
                        }),
                    ) => {}
                    (_, error) => panic!("wrong quota refusal in case {case}: {error}"),
                }
                // No partial result handle exists on any input/reduction or
                // final-admission refusal, even if scratch contains prefixes.
                assert!(view.buffer_stats().bypasses > 0);
            }
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_joined_expression_failures_precede_limit_zero_and_empty_global_count_survives() {
    let ((), report) = run_async_under_lab(0xc01d_a003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        for (text, succeeds, outputs) in [
            (
                format!("{MATCH} RETURN SUM(1/(s.p-10)) AS total LIMIT 0"),
                false,
                0,
            ),
            (format!("{MATCH} {TOTAL} LIMIT 0"), true, 0),
            (
                format!("{MATCH} WHERE a.p > 100 RETURN COUNT(*) AS rows"),
                true,
                1,
            ),
        ] {
            let prepared = prepare(&text);
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(outputs),
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    16 * 1024,
                    1000,
                    WORK,
                    None,
                )
                .await;
            if succeeds {
                let (spool, _) = result.unwrap();
                let expected = if outputs == 0 {
                    vec![]
                } else {
                    vec![(vec![], vec![GraphAggregateValue::Count(0)])]
                };
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
            } else {
                assert!(matches!(
                    result,
                    Err(NativeAggregateSpoolError::BufferedExecute(_))
                ));
            }
            assert!(view.buffer_stats().bypasses > 0);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
        for text in [
            "MATCH (a)-[r:R]->(b)-[:R*1..2]->(c) RETURN COUNT(*) AS rows LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) WHERE EXISTS { MATCH (c)-[:R]->(d) } RETURN COUNT(*) AS rows LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN COLLECT(s.p) AS values LIMIT 0",
        ] {
            let params = GqlParameters::new();
            let native = PreparedNativeRead::prepare(text, &params, resolve).unwrap();
            assert!(
                native.prepare_buffered_aggregate(&params).is_err(),
                "{text}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_a_blocked_aggregate_append_releases_cold_ancestors_computed_row_and_encoder() {
    let ((), report) = run_async_under_lab(0xc01d_a004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare(&format!(
            "{MATCH} RETURN a.q AS label, SUM(r.p+s.p) AS total"
        ));
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let (mut source, backing) = file(&cx, &spill_pool).await;
        let (mut partition, _) = file(&cx, &spill_pool).await;
        let (mut destination, _) = file(&cx, &spill_pool).await;
        {
            let mut state = backing.0.lock().unwrap();
            state.pending = true;
            state.writes = 0;
        }
        let mut future = prepared.spool_in_view(
            &mut view,
            &cx,
            policy(10),
            &mut source,
            &mut partition,
            &mut destination,
            1,
            512,
            1,
            512,
            63,
            16 * 1024,
            1000,
            WORK,
            None,
        );
        poll_fn(|task| {
            let result = future.as_mut().poll(task);
            if backing.0.lock().unwrap().writes != 0 {
                assert!(result.is_pending());
                Poll::Ready(())
            } else {
                assert!(
                    result.is_pending(),
                    "source failed before the injected scratch suspension"
                );
                Poll::Pending
            }
        })
        .await;
        assert!(source_pool.used() > baseline);
        assert!(spill_pool.used() > 0);
        drop(future);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        assert!(source.is_poisoned());
        assert_eq!(source.stats().published_runs, 0);
        assert_eq!(destination.stats().published_runs, 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn buffered_join_summary_stays_pinned_across_writer_compaction_and_result_ownership() {
    let ((), report) = run_async_under_lab(0xc01d_a005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare(&format!("{MATCH} {TOTAL}"));
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut old = view(&commit, &vfs, &source_pool).await;
        let mut writer = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(77)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(4));
        writer.compact(&commit).await.unwrap();
        drop(writer);
        for (cut, sum) in [(3, 125), (4, 245)] {
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut old,
                    &cx,
                    policy(1),
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    16 * 1024,
                    1000,
                    WORK,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
            let mut reader = spool.reader(&mut destination, None);
            let row = reader.next_row(&cx).await.unwrap().unwrap();
            assert_eq!(
                row.values(),
                &[
                    GraphAggregateValue::Count(6),
                    GraphAggregateValue::Integer(sum)
                ]
            );
            assert!(reader.next_row(&cx).await.unwrap().is_none());
            drop(reader);
            drop(old);
            assert_eq!(source_pool.used(), 0);
            assert!(
                spill_pool.used() > 0,
                "the delivered row retains its decoded reservation"
            );
            drop(row);
            assert_eq!(spill_pool.used(), 0);
            old = view(&commit, &vfs, &source_pool).await;
        }
        drop(old);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
