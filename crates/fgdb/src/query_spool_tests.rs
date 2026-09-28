//! Real native cursor -> paged scratch -> canonical row differential tests.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::{MemoryPool, SpillLimits, SpillStats};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

const NODE: &str = "MATCH (n:L) RETURN n AS id, n.p AS p, n.q AS q";
const EDGE: &str = "MATCH (a)-[e:R]->(b) RETURN e AS edge, a AS source, b AS target, e.p AS value";

fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 100_000_000, 1_000_000) }
fn plan(text: &str) -> PreparedNativeRead {
    PreparedNativeRead::prepare(text, &GqlParameters::new(), resolve).unwrap()
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32])
}
async fn seed(cx: &CommitCx, count: u128) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    let text = CanonicalScalar::ucs_basic_text(&"payload:é\n".repeat(60)).unwrap();
    for id in 0..count {
        let mut props = vec![(PropertyKeyId(1), text.clone())];
        if id % 3 == 0 { props.push((PropertyKeyId(2), CanonicalScalar::Null)); }
        else if id % 3 == 1 { props.push((PropertyKeyId(2), CanonicalScalar::Int(-(id as i64)))); }
        batch.create_vertex(VId(id), vec![LabelId(1)], props);
        if id != 0 { batch.add_edge(EId(id), VId(0), VId(id), vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))]); }
    }
    if count > 1 { // Parallel relationship identity must survive spooling.
        batch.add_edge(EId(u128::MAX), VId(0), VId(1), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[derive(Default)]
struct State {
    bytes: Cursor<Vec<u8>>,
    reads: usize,
    writes: usize,
    pending_read: bool,
    pending_write: bool,
    write_limit: Option<usize>,
    fail_flush: bool,
}
#[derive(Clone, Default)]
struct File(Arc<Mutex<State>>);
impl AsyncRead for File {
    fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        if state.pending_read { return Poll::Pending; }
        let at = state.bytes.position() as usize;
        let count = out.remaining().min(state.bytes.get_ref().len().saturating_sub(at));
        if count != 0 {
            out.put_slice(&state.bytes.get_ref()[at..at + count]);
            state.bytes.set_position((at + count) as u64);
        }
        state.reads += 1;
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for File {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        if state.pending_write { return Poll::Pending; }
        let count = state.write_limit.unwrap_or(bytes.len()).min(bytes.len());
        if count == 0 && !bytes.is_empty() { return Poll::Ready(Err(io::Error::other("injected write failure"))); }
        let result = std::io::Write::write(&mut state.bytes, &bytes[..count]);
        if let Some(left) = &mut state.write_limit { *left -= count; }
        state.writes += 1;
        Poll::Ready(result)
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(if self.0.lock().unwrap().fail_flush { Err(io::Error::other("injected flush failure")) } else { Ok(()) })
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> { Poll::Ready(Ok(())) }
}
impl AsyncSeek for File {
    fn poll_seek(self: Pin<&mut Self>, _: &mut Context<'_>, at: SeekFrom) -> Poll<io::Result<u64>> {
        Poll::Ready(self.0.lock().unwrap().bytes.seek(at))
    }
}
async fn scratch(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, File) {
    let file = File::default();
    let scratch = SpillFile::new(cx, file.clone(), pool.clone(), SpillLimits {
        max_file_bytes: 4_000_000, max_runs: 100, max_run_bytes: 1_000_000,
    }).await.unwrap();
    (scratch, file)
}
async fn contents(spool: &NativeResultSpool, scratch: &mut SpillFile<File>, cx: &QueryCx) -> Vec<Vec<u8>> {
    let mut reader = spool.reader(scratch);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await.unwrap() { rows.push(row.as_ref().to_vec()); }
    assert_eq!(reader.state(), ScanState::Exhausted);
    assert!(reader.next_row(cx).await.unwrap().is_none());
    rows
}

#[test]
fn native_vertex_and_edge_rows_spill_beyond_the_pool_without_changing_order_or_counters() {
    let ((), report) = run_async_under_lab(0x5b01_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit(), 100).await;
        let pool = MemoryPool::new(16_384, 0).unwrap();
        let (mut scratch, _) = scratch(&cx, &pool).await;
        for (text, kind) in [(NODE, ScanKind::Vertex), (EDGE, ScanKind::Edge)] {
            let prepared = plan(text);
            let params = GqlParameters::new();
            let (columns, mut incumbent) = prepared.stream(&db, &cx, &params, policy()).unwrap();
            let expected: Vec<_> = incumbent.by_ref().map(|row| row.unwrap().canonical_bytes().unwrap()).collect();
            let rows = incumbent.row_stats();
            let evaluator = incumbent.evaluator_stats();
            let spool = prepared.spool(&db, &cx, &params, policy(), &mut scratch, 257, 4096).await.unwrap();
            assert_eq!(spool.columns(), columns);
            assert_eq!(spool.kind(), kind);
            assert_eq!(spool.snapshot_seq(), db.frontier().unwrap());
            assert_eq!(spool.row_stats(), rows);
            assert_eq!(spool.evaluator_stats(), evaluator);
            assert_eq!(spool.row_count(), expected.len() as u64);
            assert_eq!(spool.encoded_len(), expected.iter().map(|row| 8 + row.len()).sum::<usize>());
            if kind == ScanKind::Vertex { assert!(spool.encoded_len() > pool.limit()); }
            assert_eq!(pool.used(), 0);
            assert_eq!(contents(&spool, &mut scratch, &cx).await, expected);
            assert_eq!(pool.used(), 0);
            assert_eq!(contents(&spool.clone(), &mut scratch, &cx).await, expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn future_pins_at_open_and_completed_spool_retains_neither_snapshot_nor_template() {
    let ((), report) = run_async_under_lab(0x5b01_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = seed(&contexts.commit(), 8).await;
        let view = db.read_session().unwrap();
        let weak = Arc::downgrade(&db.snapshot);
        let at = db.frontier().unwrap();
        let prepared = plan(NODE);
        let params = GqlParameters::new();
        let (_, cursor) = prepared.stream_in_view(&view, &cx, &params, policy()).unwrap();
        let expected: Vec<_> = cursor.map(|row| row.unwrap().canonical_bytes().unwrap()).collect();
        let pool = MemoryPool::new(16_384, 0).unwrap();
        let (mut first_file, _) = scratch(&cx, &pool).await;
        let (mut second_file, _) = scratch(&cx, &pool).await;
        let first = prepared.spool(&db, &cx, &params, policy(), &mut first_file, 113, 4096);
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(2));
        change.create_vertex(VId(1000), vec![LabelId(1)], vec![]);
        db.write(&contexts.commit(), change).await.unwrap();
        let second = prepared.spool_in_view(&view, &cx, &params, policy(), &mut second_file, 113, 4096);
        drop(db);
        drop(view);
        drop(params);
        drop(prepared);
        assert!(weak.upgrade().is_some());
        let first = first.await.unwrap();
        let second = second.await.unwrap();
        assert!(weak.upgrade().is_none(), "completed result accidentally retains graph source");
        assert_eq!(first.snapshot_seq(), at);
        assert_eq!(second.snapshot_seq(), at);
        assert_eq!(contents(&first, &mut first_file, &cx).await, expected);
        assert_eq!(contents(&second, &mut second_file, &cx).await, expected);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unpolled_and_unsupported_requests_do_not_reserve_scratch_and_empty_results_keep_schema() {
    let ((), report) = run_async_under_lab(0x5b01_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit(), 4).await;
        let pool = MemoryPool::new(16_384, 0).unwrap();
        let (mut scratch, file) = scratch(&cx, &pool).await;
        let params = GqlParameters::new();
        drop(plan(NODE).spool(&db, &cx, &params, policy(), &mut scratch, 512, 4096));
        assert_eq!(scratch.stats(), SpillStats::default());
        let aggregate = plan("MATCH (n:L) RETURN count(*) AS count");
        let error = aggregate.spool(&db, &cx, &params, policy(), &mut scratch, 512, 4096).await.unwrap_err();
        assert!(matches!(error.prepare_error(), Some(QueryError::StreamingUnsupported { .. })));
        assert_eq!(scratch.stats(), SpillStats::default());
        assert_eq!(file.0.lock().unwrap().writes, 0);
        let empty = plan("MATCH (n:L) RETURN n AS id LIMIT 0")
            .spool(&db, &cx, &params, policy(), &mut scratch, 512, 4096).await.unwrap();
        assert_eq!(empty.columns(), &["id".to_owned()]);
        assert_eq!(empty.row_count(), 0);
        assert_eq!(empty.encoded_len(), 0);
        assert_eq!(empty.page_count(), 0);
        let reads = file.0.lock().unwrap().reads;
        assert!(contents(&empty, &mut scratch, &cx).await.is_empty());
        assert_eq!(file.0.lock().unwrap().reads, reads);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_native_budget_failure_never_exposes_a_completed_prefix() {
    let ((), report) = run_async_under_lab(0x5b01_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit(), 10).await;
        let pool = MemoryPool::new(16_384, 0).unwrap();
        let (mut scratch, file) = scratch(&cx, &pool).await;
        let prepared = plan(NODE);
        let params = GqlParameters::new();
        let limit = GqlQueryPolicy::new(100, 3, 1_000_000, 100_000);
        let (_, mut control) = prepared.stream(&db, &cx, &params, limit).unwrap();
        for _ in 0..3 { control.next().unwrap().unwrap(); }
        assert!(matches!(control.next().unwrap(), Err(GqlQueryError::Rows(_))));
        let error = prepared.spool(&db, &cx, &params, limit, &mut scratch, 128, 4096).await.unwrap_err();
        assert!(matches!(error.execution_error(), Some(GqlQueryError::Rows(_))));
        assert!(file.0.lock().unwrap().writes > 0, "fixture must have written a private prefix");
        assert_eq!(scratch.stats().published_runs, 0);
        assert!(scratch.is_poisoned());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn row_ceiling_file_quota_and_actual_io_failures_refund_without_acceptance() {
    let ((), report) = run_async_under_lab(0x5b01_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit(), 10).await;
        let pool = MemoryPool::new(16_384, 0).unwrap();
        for case in 0..4 {
            let file = File::default();
            file.0.lock().unwrap().write_limit = (case == 2).then_some(31);
            file.0.lock().unwrap().fail_flush = case == 3;
            let mut scratch = SpillFile::new(&cx, file, pool.clone(), SpillLimits {
                max_file_bytes: if case == 1 { 600 } else { 4_000_000 },
                max_runs: 10, max_run_bytes: 1_000_000,
            }).await.unwrap();
            let error = plan(NODE).spool(&db, &cx, &GqlParameters::new(), policy(),
                &mut scratch, 128, if case == 0 { 5 } else { 4096 }).await.unwrap_err();
            match case {
                0 => assert!(matches!(error, NativeSpoolError::RowTooLarge { limit: 5, .. })),
                1 => assert!(matches!(error.spill_error(), Some(SpillError::FileLimit { .. }))),
                _ => assert!(matches!(error.spill_error(), Some(SpillError::Io(_)))),
            }
            assert_eq!(scratch.stats().published_runs, 0);
            assert!(scratch.is_poisoned());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn consumption_close_drop_foreign_file_and_memory_pressure_are_fused_and_retryable_by_new_cursor() {
    let ((), report) = run_async_under_lab(0x5b01_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit(), 6).await;
        let pool = MemoryPool::new(16_384, 0).unwrap();
        let (mut scratch, file) = scratch(&cx, &pool).await;
        let spool = plan(NODE).spool(&db, &cx, &GqlParameters::new(), policy(), &mut scratch, 257, 4096).await.unwrap();
        let mut reader = spool.reader(&mut scratch);
        let row = reader.next_row(&cx).await.unwrap().unwrap();
        assert!(pool.used() > row.charged_bytes());
        drop(row);
        let reads = file.0.lock().unwrap().reads;
        reader.close();
        assert_eq!(reader.state(), ScanState::Closed);
        assert!(reader.next_row(&cx).await.unwrap().is_none());
        assert_eq!(file.0.lock().unwrap().reads, reads);
        drop(reader);
        assert_eq!(pool.used(), 0);
        let other_file = File::default();
        let mut foreign = SpillFile::new(&cx, other_file.clone(), pool.clone(), SpillLimits {
            max_file_bytes: 1000, max_runs: 10, max_run_bytes: 1000,
        }).await.unwrap();
        let mut reader = spool.reader(&mut foreign);
        assert!(matches!(reader.next_row(&cx).await, Err(SpillError::ForeignRun)));
        assert_eq!(reader.state(), ScanState::Failed);
        assert!(reader.next_row(&cx).await.unwrap().is_none());
        assert_eq!(other_file.0.lock().unwrap().reads, 0);
        drop(reader);
        let blocker = pool.allocate_zeroed(&cx, pool.available()).unwrap();
        let mut reader = spool.reader(&mut scratch);
        assert!(matches!(reader.next_row(&cx).await, Err(SpillError::Memory(_))));
        assert_eq!(reader.state(), ScanState::Failed);
        drop(reader);
        drop(blocker);
        assert!(!scratch.is_poisoned());
        assert_eq!(contents(&spool, &mut scratch, &cx).await.len(), 6);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn later_corruption_reports_one_error_without_retracting_an_already_delivered_row() {
    let ((), report) = run_async_under_lab(0x5b01_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit(), 6).await;
        let pool = MemoryPool::new(16_384, 0).unwrap();
        let (mut scratch, file) = scratch(&cx, &pool).await;
        let spool = plan(NODE).spool(&db, &cx, &GqlParameters::new(), policy(), &mut scratch, 64, 4096).await.unwrap();
        let mut reader = spool.reader(&mut scratch);
        let delivered = reader.next_row(&cx).await.unwrap().unwrap();
        let original = delivered.as_ref().to_vec();
        // External corruption is injected only by this test backend. The
        // production file owner does not expose mutable access to its bytes.
        for byte in file.0.lock().unwrap().bytes.get_mut() { *byte ^= 1; }
        assert!(matches!(reader.next_row(&cx).await, Err(SpillError::ChecksumMismatch)));
        assert_eq!(reader.state(), ScanState::Failed);
        assert_eq!(delivered.as_ref(), original);
        assert!(reader.next_row(&cx).await.unwrap().is_none());
        drop(reader);
        drop(delivered);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropped_pending_production_and_consumption_release_buffers_and_fence_ambiguous_io() {
    let ((), report) = run_async_under_lab(0x5b01_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit(), 4).await;
        let pool = MemoryPool::new(16_384, 0).unwrap();
        let (mut blocked, file) = scratch(&cx, &pool).await;
        file.0.lock().unwrap().pending_write = true;
        {
            let prepared = plan(NODE);
            let params = GqlParameters::new();
            let mut future = std::pin::pin!(prepared.spool(&db, &cx, &params, policy(), &mut blocked, 32, 4096));
            assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        }
        assert!(blocked.is_poisoned());
        assert_eq!(blocked.stats().published_runs, 0);
        assert_eq!(pool.used(), 0);
        let (mut scratch, file) = scratch(&cx, &pool).await;
        let spool = plan(NODE).spool(&db, &cx, &GqlParameters::new(), policy(), &mut scratch, 32, 4096).await.unwrap();
        file.0.lock().unwrap().pending_read = true;
        let mut reader = spool.reader(&mut scratch);
        {
            let mut future = std::pin::pin!(reader.next_row(&cx));
            assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        }
        assert_eq!(reader.state(), ScanState::Failed);
        assert!(reader.next_row(&cx).await.unwrap().is_none());
        drop(reader);
        assert!(scratch.is_poisoned());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
