//! Graph-backed UNWIND through authenticated cold reads and external stages.
//! Literal expectations describe the fixture, independently of either executor.

use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys, MemVfs, MemoryPool,
    NativeResultSpool, NativeSpoolError, PreparedBufferedOrder, PreparedNativeRead, WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateValue, GraphIntegerErrorKind,
    GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
};
use fgdb_strata::tiered::memory::{SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use std::future::poll_fn;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

const WORK: u64 = 1_000_000_000;
const SCRATCH_BYTES: usize = 512 * 1024;
const PAYLOAD_BYTES: usize = 8400;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xe1; 32],
        DatabaseSecurityNamespaceId([0xe2; 32]),
        [0xe3; 32],
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
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, rows, WORK, WORK)
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn prepare(text: &str, params: &GqlParameters) -> PreparedBufferedOrder {
    PreparedNativeRead::prepare(text, params, resolve)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .prepare_buffered_order(params)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}
async fn fixture(cx: &CommitCx) -> MemVfs {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 1..=3 {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                (
                    PropertyKeyId(2),
                    CanonicalScalar::ucs_basic_text(&"x".repeat(PAYLOAD_BYTES)).unwrap(),
                ),
            ],
        );
    }
    for (id, from, to, value) in [(1, 1, 2, 7), (2, 1, 2, 8), (3, 2, 3, 9)] {
        batch.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(13)));
    assert_eq!(db.write(cx, change).await.unwrap(), CommitSeq(2));
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

// The graph uses real MemVfs storage. This adapter only exposes scratch write
// suspension so dropping execution can be checked at actual asynchronous I/O.
#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    suspend_on: Option<&'static [u8]>,
    suspended: bool,
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
        if state
            .suspend_on
            .is_some_and(|marker| bytes.windows(marker.len()).any(|part| part == marker))
        {
            state.suspended = true;
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
            max_runs: 4096,
            max_run_bytes: 8_000_000,
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
) -> Vec<Vec<GraphValue>> {
    let mut reader = spool.reader(file);
    let mut rows = Vec::new();
    while let Some(bytes) = reader.next_row(cx).await.unwrap() {
        rows.push(
            GraphValueRow::decode_canonical(bytes.as_ref())
                .unwrap()
                .values()
                .to_vec(),
        );
    }
    rows
}

#[test]
fn cold_unwind_keeps_nested_values_multiplicity_history_and_completed_pages() {
    let ((), report) = run_async_under_lab(0xc01d_4101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (text, cut, expected) in [
            (
                "MATCH (n) WITH n.p AS p UNWIND [p,NULL,p] AS item RETURN p,item ORDER BY p,item",
                2,
                vec![
                    vec![scalar(1), scalar(1)],
                    vec![scalar(1), scalar(1)],
                    vec![scalar(1), null()],
                    vec![scalar(2), scalar(2)],
                    vec![scalar(2), scalar(2)],
                    vec![scalar(2), null()],
                    vec![scalar(13), scalar(13)],
                    vec![scalar(13), scalar(13)],
                    vec![scalar(13), null()],
                ],
            ),
            (
                "MATCH (n) WITH NULL AS items UNWIND items AS item RETURN item",
                2,
                vec![],
            ),
            (
                "MATCH (n) WITH [] AS items UNWIND items AS item RETURN item",
                2,
                vec![],
            ),
            (
                "MATCH (n) WITH n.p AS p UNWIND [[],NULL,[p,p+1]] AS items UNWIND items AS item RETURN item ORDER BY item",
                2,
                [1, 2, 2, 3, 13, 14]
                    .into_iter()
                    .map(|n| vec![scalar(n)])
                    .collect(),
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 WITH [{owner:n.p,items:[n.p,n.p+10]}] AS records UNWIND records AS record UNWIND record.items AS item RETURN record.owner AS owner,item ORDER BY owner,item",
                1,
                vec![
                    vec![scalar(1), scalar(1)],
                    vec![scalar(1), scalar(11)],
                    vec![scalar(2), scalar(2)],
                    vec![scalar(2), scalar(12)],
                    vec![scalar(3), scalar(3)],
                    vec![scalar(3), scalar(13)],
                ],
            ),
            (
                "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT 2 UNWIND [p,p-1,p] AS item WITH DISTINCT item ORDER BY item SKIP 1 LIMIT 2 RETURN item+100 AS value ORDER BY value",
                2,
                vec![vec![scalar(102)], vec![scalar(112)]],
            ),
            (
                "MATCH (a)-[r:R]->(b) WITH r.p AS p UNWIND [p,p] AS item RETURN item ORDER BY item",
                2,
                [7, 7, 8, 8, 9, 9]
                    .into_iter()
                    .map(|n| vec![scalar(n)])
                    .collect(),
            ),
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH r.p+s.p AS p UNWIND [p,p+1] AS item RETURN item ORDER BY item",
                2,
                [16, 17, 17, 18]
                    .into_iter()
                    .map(|n| vec![scalar(n)])
                    .collect(),
            ),
            (
                "MATCH (n) WITH n.p AS p ORDER BY p LIMIT 0 UNWIND [1/(p-p)] AS item RETURN item",
                2,
                vec![],
            ),
        ] {
            let prepared = prepare(text, &GqlParameters::new());
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(expected.len() as u64),
                    &mut scratch,
                    &mut destination,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    1000,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
            assert_eq!(spool.row_count(), expected.len() as u64);
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                expected,
                "{text}"
            );
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
        }
        assert!(view.buffer_stats().bypasses > 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expanded_occurrences_feed_external_aggregates_including_empty_and_distinct_inputs() {
    let ((), report) = run_async_under_lab(0xc01d_4102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (text, count, sum) in [
            (
                "MATCH (n) WITH n.p AS p UNWIND [p,p,NULL] AS item RETURN COUNT(*) AS rows,SUM(item) AS total",
                9,
                GraphAggregateValue::Integer(32),
            ),
            (
                "MATCH (n) WITH n.p AS p UNWIND [p,p,NULL] AS item RETURN COUNT(DISTINCT item) AS rows,SUM(DISTINCT item) AS total",
                3,
                GraphAggregateValue::Integer(16),
            ),
            (
                "MATCH (n) WITH n.p AS p UNWIND [p,p+1] AS item WITH DISTINCT item ORDER BY item DESC LIMIT 2 RETURN COUNT(*) AS rows,SUM(item) AS total",
                2,
                GraphAggregateValue::Integer(27),
            ),
            (
                "MATCH (n) WITH [] AS items UNWIND items AS item RETURN COUNT(*) AS rows,SUM(item) AS total",
                0,
                GraphAggregateValue::Value(null()),
            ),
        ] {
            let params = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(text, &params, resolve)
                .unwrap()
                .prepare_buffered_aggregate(&params)
                .unwrap();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
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
                    32 * 1024,
                    1000,
                    WORK,
                    None,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            let mut reader = spool.reader(&mut destination, None);
            let row = reader.next_row(&cx).await.unwrap().unwrap();
            assert!(row.keys().is_empty());
            assert_eq!(
                row.values(),
                &[GraphAggregateValue::Count(count), sum],
                "{text}"
            );
            drop(row);
            assert!(reader.next_row(&cx).await.unwrap().is_none());
            drop(reader);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_unwind_errors_precede_parent_expressions_and_final_zero_limits() {
    let ((), report) = run_async_under_lab(0xc01d_4103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for tail in ["LIMIT 0", "LIMIT 1", "SKIP 1000 LIMIT 1"] {
            let text = format!(
                "MATCH (n) WITH n.p AS p UNWIND [[p],2] AS items UNWIND items AS item RETURN 1/(p-p) AS value {tail}"
            );
            let prepared = prepare(&text, &GqlParameters::new());
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let error = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(1),
                    &mut scratch,
                    &mut destination,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    1000,
                    WORK,
                )
                .await
                .unwrap_err();
            // An incorrectly fused parent would divide by zero on its first
            // row. The child must finish and report its second-row type error.
            assert!(
                matches!(error, NativeSpoolError::SetExecution(
                GraphSetExecutionError::Projection { row: 1, column: 2, error }
            ) if error.kind == GraphIntegerErrorKind::IncompatibleOperands),
                "{text}: {error:?}"
            );
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expanded_input_quota_and_native_work_are_independent_of_the_final_page() {
    let ((), report) = run_async_under_lab(0xc01d_4104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let prepared = prepare(
            "MATCH (n) WITH n.p AS p UNWIND [p,p,p] AS item RETURN item ORDER BY item DESC LIMIT 1",
            &GqlParameters::new(),
        );
        let mut exact = None;
        for trial in 0..6 {
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let mut budget = policy(1);
            let mut external = WORK;
            if let Some((native, entries, sort)) = exact {
                budget.evaluator.max_work_units = native - u64::from(trial == 3);
                budget.evaluator.max_scratch_entries = entries - u64::from(trial == 4);
                external = sort - u64::from(trial == 5);
            }
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    budget,
                    &mut scratch,
                    &mut destination,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    if trial == 2 { 8 } else { 9 },
                    external,
                )
                .await;
            if trial < 2 {
                let (spool, work) = result.unwrap();
                assert_eq!(
                    contents(&spool, &mut destination, &cx).await,
                    vec![vec![scalar(13)]]
                );
                let stats = spool.evaluator_stats();
                let measured = (stats.work_units, stats.scratch_entries, work);
                if let Some(prior) = exact {
                    assert_eq!(prior, measured);
                }
                exact = Some(measured);
            } else {
                let error = result.unwrap_err();
                match trial {
                    2 => assert!(
                        matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                        if matches!(**inner, GqlQueryError::Rows(_))),
                        "{error:?}"
                    ),
                    3 | 4 => assert!(
                        matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                        if matches!(**inner, GqlQueryError::Evaluator(_))),
                        "{error:?}"
                    ),
                    5 => assert!(
                        matches!(error, NativeSpoolError::SortWorkLimit { .. }),
                        "{error:?}"
                    ),
                    _ => unreachable!(),
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
fn one_large_expansion_exceeds_scratch_capacity_without_retaining_emitted_rows() {
    const ITEMS: usize = 128;
    let ((), report) = run_async_under_lab(0xc01d_4105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        assert!(ITEMS * PAYLOAD_BYTES > SCRATCH_BYTES);
        let params = GqlParameters::new()
            .with_list("items", (0..ITEMS as i64).map(scalar).collect())
            .unwrap();
        let prepared = prepare(
            "MATCH (n) WHERE n.p=1 WITH n.q AS payload UNWIND $items AS item RETURN payload,item ORDER BY item DESC",
            &params,
        );
        drop(params);
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let (mut scratch, _) = file(&cx, &spill_pool).await;
        let (mut destination, _) = file(&cx, &spill_pool).await;
        let (spool, _) = prepared
            .spool_in_view(
                &mut view,
                &cx,
                policy(ITEMS as u64),
                &mut scratch,
                &mut destination,
                1,
                512,
                1024,
                32 * 1024,
                ITEMS as u64,
                WORK,
            )
            .await
            .unwrap();
        assert_eq!(spool.row_count(), ITEMS as u64);
        assert!(
            scratch.stats().reserved_bytes + destination.stats().reserved_bytes
                > SCRATCH_BYTES as u64
        );
        let payload = GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(&"x".repeat(PAYLOAD_BYTES)).unwrap(),
        );
        let mut reader = spool.reader(&mut destination);
        for value in (0..ITEMS as i64).rev() {
            let bytes = reader.next_row(&cx).await.unwrap().unwrap();
            let row = GraphValueRow::decode_canonical(bytes.as_ref()).unwrap();
            assert_eq!(row.values(), &[payload.clone(), scalar(value)]);
        }
        assert!(reader.next_row(&cx).await.unwrap().is_none());
        drop(reader);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_suspended_expansion_output_releases_all_row_and_cursor_reservations() {
    const MARKER: &str = "unwind-output-witness-";
    let ((), report) = run_async_under_lab(0xc01d_4106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        // The marker exists only in the UNWIND instruction, never in source
        // or WITH rows. Repeating it across several short pages guarantees
        // one complete occurrence reaches an append. A second list element
        // remains owned by the cursor during the first output's suspension.
        let text = format!(
            "MATCH (n) WHERE n.p=1 WITH n.q AS payload UNWIND ['{}','remaining-element'] AS item RETURN payload,item",
            MARKER.repeat(16),
        );
        let prepared = prepare(&text, &GqlParameters::new());
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let (mut scratch, a) = file(&cx, &spill_pool).await;
        let (mut destination, b) = file(&cx, &spill_pool).await;
        a.0.lock().unwrap().suspend_on = Some(MARKER.as_bytes());
        b.0.lock().unwrap().suspend_on = Some(MARKER.as_bytes());
        let mut future = prepared.spool_in_view(
            &mut view,
            &cx,
            policy(2),
            &mut scratch,
            &mut destination,
            1,
            512,
            63,
            32 * 1024,
            3,
            WORK,
        );
        let blocked = poll_fn(|task| {
            let result = future.as_mut().poll(task);
            let suspended = [a.0.lock().unwrap().suspended, b.0.lock().unwrap().suspended];
            if let Some(index) = suspended.iter().position(|&seen| seen) {
                assert!(result.is_pending());
                assert_eq!(suspended.iter().filter(|&&seen| seen).count(), 1);
                Poll::Ready(index)
            } else {
                assert!(result.is_pending(), "UNWIND output append must be reached");
                Poll::Pending
            }
        })
        .await;
        assert!(spill_pool.used() > PAYLOAD_BYTES * 2);
        drop(future);
        assert!([&scratch, &destination][blocked].is_poisoned());
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
