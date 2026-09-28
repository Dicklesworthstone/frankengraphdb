//! Independent typed comparisons plus actual native-query / scratch integration.
use super::*;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_types::{CanonicalF64, CanonicalScalar, EId, PurposeContexts, VId};
use asupersync::lab::run_async_under_lab;

fn typed_cmp(a: &GraphValueRow, b: &GraphValueRow, order: &[GraphValueOrder]) -> Ordering {
    for key in order {
        let a = &a.values()[key.column];
        let b = &b.values()[key.column];
        let cmp = match (a.is_null(), b.is_null()) {
            (true, false) => if key.nulls_first { Ordering::Less } else { Ordering::Greater },
            (false, true) => if key.nulls_first { Ordering::Greater } else { Ordering::Less },
            _ => if key.descending { b.cmp(a) } else { a.cmp(b) },
        };
        if cmp != Ordering::Equal { return cmp; }
    }
    a.cmp(b)
}

#[test]
fn encoded_order_matches_typed_cells_not_length_prefixes() {
    let ((), report) = run_async_under_lab(0x50a7_0001, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let values = vec![
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Bool(false)),
            GraphValue::Scalar(CanonicalScalar::Int(i64::MIN)),
            GraphValue::Scalar(CanonicalScalar::Int(0)),
            GraphValue::Scalar(CanonicalScalar::Int(i64::MAX)),
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(f64::NAN))),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("z").unwrap()),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("aa\0é").unwrap()),
            GraphValue::Vertex(VId(0)), GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Vertices(vec![VId(2)].into_boxed_slice()),
            GraphValue::Vertices(vec![VId(1), VId(9)].into_boxed_slice()),
            GraphValue::Edges(vec![EId(2)].into_boxed_slice()),
            GraphValue::Edges(vec![EId(1), EId(9)].into_boxed_slice()),
            GraphValue::Edge(EId(u128::MAX)),
            GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(2))].into_boxed_slice()),
            GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(1)),
                GraphValue::List(vec![GraphValue::Vertex(VId(9))].into_boxed_slice())].into_boxed_slice()),
            GraphValue::List(Box::new([])),
        ];
        let rows: Vec<_> = values.into_iter().enumerate().map(|(at, value)|
            GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId((at % 3) as u128)), value])
        ).collect();
        let encoded: Vec<_> = rows.iter().map(|row| row.canonical_bytes().unwrap()).collect();
        let mut work = Work { cx: &cx, used: 0, limit: u64::MAX };
        for row in &encoded { canonical::validate(row, 2, &mut work).unwrap(); }
        for descending in [false, true] {
            for nulls_first in [false, true] {
                for column in 0..2 {
                    let order = [GraphValueOrder { column, descending, nulls_first }];
                    for a in 0..rows.len() {
                        for b in 0..rows.len() {
                            assert_eq!(canonical::compare(&encoded[a], &encoded[b], &order, 2, &mut work).unwrap(),
                                typed_cmp(&rows[a], &rows[b], &order));
                        }
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_truncated_frame_and_excess_depth_refuse_without_allocation() {
    let ((), report) = run_async_under_lab(0x50a7_0002, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let row = GraphValueRow::from_owned_values(vec![GraphValue::List(vec![
            GraphValue::Vertex(VId(u128::MAX)), GraphValue::Scalar(CanonicalScalar::Int(-1)),
        ].into_boxed_slice())]).canonical_bytes().unwrap();
        let mut work = Work { cx: &cx, used: 0, limit: u64::MAX };
        for end in 0..row.len() { assert!(canonical::validate(&row[..end], 1, &mut work).is_err()); }
        let mut trailing = row.clone(); trailing.push(0);
        assert!(canonical::validate(&trailing, 1, &mut work).is_err());
        assert!(canonical::validate(&row, 2, &mut work).is_err());
        let mut deep = GraphValue::Scalar(CanonicalScalar::Null);
        for _ in 0..=GraphValue::MAX_LIST_DEPTH { deep = GraphValue::List(vec![deep].into_boxed_slice()); }
        let deep = GraphValueRow::from_owned_values(vec![deep]).canonical_bytes().unwrap();
        assert!(canonical::validate(&deep, 1, &mut work).is_err());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

use asupersync::io::ReadBuf;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::SpillLimits;
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    reads: usize,
    writes: usize,
    pending_read: bool,
    pending_write: bool,
    fail_flush: bool,
}
#[derive(Clone, Default)]
struct File(Arc<Mutex<FileState>>);
impl AsyncRead for File {
    fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let mut file = self.0.lock().unwrap();
        if file.pending_read { return Poll::Pending; }
        let at = file.bytes.position() as usize;
        let len = out.remaining().min(file.bytes.get_ref().len().saturating_sub(at));
        if len != 0 {
            out.put_slice(&file.bytes.get_ref()[at..at + len]);
            file.bytes.set_position((at + len) as u64);
        }
        file.reads += 1;
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for File {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let mut file = self.0.lock().unwrap();
        if file.pending_write { return Poll::Pending; }
        file.writes += 1;
        Poll::Ready(std::io::Write::write(&mut file.bytes, bytes))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(if self.0.lock().unwrap().fail_flush { Err(io::Error::other("injected flush")) } else { Ok(()) })
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
        max_file_bytes: 64_000_000, max_runs: 4096, max_run_bytes: 8_000_000,
    }).await.unwrap();
    (scratch, file)
}
const QUERY: &str = "MATCH (n:L) RETURN n AS id, n.p AS p, n.q AS q";
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 100_000_000, 1_000_000) }
fn plan() -> PreparedNativeRead { PreparedNativeRead::prepare(QUERY, &GqlParameters::new(), resolve).unwrap() }
async fn database(cx: &CommitCx, count: u128) -> Database<crate::MemVfs> {
    let mut db = Database::open_memory(cx, crate::DatabaseKeys::new(
        [0x31; 32], DatabaseSecurityNamespaceId([0x32; 32]), [0x33; 32],
    )).await.unwrap();
    if count != 0 {
        let mut batch = crate::WriteBatch::new(RelationId(1));
        for id in (0..count).rev() {
            let mut props = vec![(PropertyKeyId(2), CanonicalScalar::ucs_basic_text(&"abcé\0".repeat(100)).unwrap())];
            if id % 3 != 0 { props.push((PropertyKeyId(1), CanonicalScalar::Int(-((id % 7) as i64)))); }
            props.sort_by_key(|(key, _)| *key);
            batch.create_vertex(VId(id), vec![LabelId(1)], props);
        }
        db.write(cx, batch).await.unwrap();
    }
    db
}
async fn contents(spool: &NativeResultSpool, file: &mut SpillFile<File>, cx: &QueryCx) -> Vec<Vec<u8>> {
    let mut cursor = spool.reader(file);
    let mut rows = Vec::new();
    while let Some(row) = cursor.next_row(cx).await.unwrap() { rows.push(row.as_ref().to_vec()); }
    assert_eq!(cursor.state(), ScanState::Exhausted);
    rows
}

#[test]
fn real_native_results_merge_beyond_resident_limit_with_gla_order_and_original_stats() {
    let ((), report) = run_async_under_lab(0x50a7_0003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), 41).await;
        let params = GqlParameters::new();
        let prepared = plan();
        let (columns, source) = prepared.stream(&db, &cx, &params, policy()).unwrap();
        let rows: Vec<_> = source.map(|row| row.unwrap()).collect();
        let pool = MemoryPool::new(24_000, 0).unwrap();
        for run_rows in [2, 5, 9] {
            for descending in [false, true] {
                for nulls_first in [false, true] {
                    let (mut source, _) = scratch(&cx, &pool).await;
                    let (mut destination, _) = scratch(&cx, &pool).await;
                    let spool = prepared.spool(&db, &cx, &params, policy(), &mut source, 97, 4096).await.unwrap();
                    assert!(spool.encoded_len() > pool.limit());
                    let original = contents(&spool, &mut source, &cx).await;
                    let order = [GraphValueOrder { column: 1, descending, nulls_first }, GraphValueOrder::descending(0)];
                    let mut expected = rows.clone();
                    expected.sort_by(|a, b| typed_cmp(a, b, &order));
                    let expected: Vec<_> = expected.iter().map(|row| row.canonical_bytes().unwrap()).collect();
                    let (sorted, used) = spool.sort_into(&cx, &mut source, &mut destination, &order,
                        run_rows, 64, 113, u64::MAX).await.unwrap();
                    assert!(used > 0);
                    assert_eq!(sorted.columns(), columns);
                    assert_eq!(sorted.snapshot_seq(), spool.snapshot_seq());
                    assert_eq!(sorted.row_stats(), spool.row_stats());
                    assert_eq!(sorted.evaluator_stats(), spool.evaluator_stats());
                    assert_eq!(sorted.encoded_len(), spool.encoded_len());
                    assert_eq!(pool.used(), 0);
                    assert_eq!(contents(&sorted, &mut destination, &cx).await, expected);
                    assert_eq!(contents(&spool, &mut source, &cx).await, original);
                    assert_eq!(pool.used(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_order_run_admission_and_zero_work_do_not_touch_files() {
    let ((), report) = run_async_under_lab(0x50a7_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), 5).await;
        let pool = MemoryPool::new(24_000, 0).unwrap();
        let (mut source, a) = scratch(&cx, &pool).await;
        let (mut destination, b) = scratch(&cx, &pool).await;
        let spool = plan().spool(&db, &cx, &GqlParameters::new(), policy(), &mut source, 97, 4096).await.unwrap();
        let before = (source.stats(), destination.stats(), a.0.lock().unwrap().reads, b.0.lock().unwrap().writes);
        for order in [vec![], vec![GraphValueOrder::ascending(3)], vec![GraphValueOrder::ascending(0); 2]] {
            assert!(matches!(spool.sort_into(&cx, &mut source, &mut destination, &order, 2, 8, 97, u64::MAX).await,
                Err(NativeSpoolError::SortOrder(_))));
        }
        assert!(matches!(spool.sort_into(&cx, &mut source, &mut destination, &[GraphValueOrder::ascending(1)], 2, 2, 97, u64::MAX).await,
            Err(NativeSpoolError::SortRunLimit { required: 3, limit: 2 })));
        assert!(matches!(spool.sort_into(&cx, &mut source, &mut destination, &[GraphValueOrder::ascending(1)], 2, 8, 97, 0).await,
            Err(NativeSpoolError::SortWorkLimit { attempted: 1, limit: 0 })));
        assert_eq!((source.stats(), destination.stats(), a.0.lock().unwrap().reads, b.0.lock().unwrap().writes), before);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_sort_work_succeeds_and_final_unit_refusal_never_returns_a_result() {
    let ((), report) = run_async_under_lab(0x50a7_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), 7).await;
        let pool = MemoryPool::new(24_000, 0).unwrap();
        let order = [GraphValueOrder::ascending(1)];
        let mut required = None;
        for attempt in 0..3 {
            let (mut source, _) = scratch(&cx, &pool).await;
            let (mut destination, _) = scratch(&cx, &pool).await;
            let spool = plan().spool(&db, &cx, &GqlParameters::new(), policy(), &mut source, 97, 4096).await.unwrap();
            let limit = match (attempt, required) { (2, Some(used)) => used - 1, (_, Some(used)) => used, _ => u64::MAX };
            let result = spool.sort_into(&cx, &mut source, &mut destination, &order, 2, 8, 101, limit).await;
            if attempt == 2 { assert!(matches!(result, Err(NativeSpoolError::SortWorkLimit { .. }))); }
            else { required = Some(result.unwrap().1); }
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_single_run_and_duplicate_frames_preserve_multiplicity() {
    let ((), report) = run_async_under_lab(0x50a7_0006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let pool = MemoryPool::new(24_000, 0).unwrap();
        for count in [0, 1, 7] {
            let db = database(&c.commit(), count).await;
            let (mut source, _) = scratch(&cx, &pool).await;
            let (mut destination, _) = scratch(&cx, &pool).await;
            let mut spool = plan().spool(&db, &cx, &GqlParameters::new(), policy(), &mut source, 97, 4096).await.unwrap();
            // Construct only a private test fixture by duplicating valid native
            // frames. Public callers cannot manufacture a NativeResultSpool.
            if count != 0 {
                let frames = contents(&spool, &mut source, &cx).await;
                let mut writer = source.paged_writer(&cx, 97).unwrap();
                for frame in frames.iter().rev().chain(frames.iter()) {
                    writer.write(&cx, &(frame.len() as u64).to_be_bytes()).await.unwrap();
                    writer.write(&cx, frame).await.unwrap();
                }
                spool.run = writer.finish(&cx).await.unwrap();
                spool.rows.result_rows *= 2;
            }
            let before = contents(&spool, &mut source, &cx).await;
            let (sorted, _) = spool.sort_into(&cx, &mut source, &mut destination, &[GraphValueOrder::descending(0)], 32, 8, 101, u64::MAX).await.unwrap();
            let after = contents(&sorted, &mut destination, &cx).await;
            assert_eq!(after.len(), before.len());
            for row in &before { assert_eq!(after.iter().filter(|other| *other == row).count(), 2); }
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn corruption_flush_failure_and_dropped_io_release_all_sort_allocations() {
    let ((), report) = run_async_under_lab(0x50a7_0007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), 7).await;
        let pool = MemoryPool::new(24_000, 0).unwrap();
        for case in 0..4 {
            let (mut source, a) = scratch(&cx, &pool).await;
            let (mut destination, b) = scratch(&cx, &pool).await;
            let spool = plan().spool(&db, &cx, &GqlParameters::new(), policy(), &mut source, 97, 4096).await.unwrap();
            match case {
                0 => a.0.lock().unwrap().bytes.get_mut()[0] ^= 1,
                1 => b.0.lock().unwrap().fail_flush = true,
                2 => a.0.lock().unwrap().pending_read = true,
                _ => b.0.lock().unwrap().pending_write = true,
            }
            let order = [GraphValueOrder::ascending(1)];
            let future = spool.sort_into(&cx, &mut source, &mut destination, &order, 2, 8, 101, u64::MAX);
            if case < 2 {
                let error = future.await.unwrap_err();
                assert!(matches!(error, NativeSpoolError::Spill(SpillError::ChecksumMismatch | SpillError::Io(_))));
            } else {
                let mut future = Box::pin(future);
                assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
                drop(future);
                if case == 2 { assert!(source.is_poisoned()); }
                else { assert!(destination.is_poisoned()); }
            }
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn captured_path_order_ignores_the_encoded_step_count_until_prefixes_tie() {
    fn encoded(start: u128, steps: &[(u128, u128)]) -> Vec<u8> {
        let mut body = vec![2];
        body.extend_from_slice(&start.to_be_bytes());
        body.extend_from_slice(&(steps.len() as u64).to_be_bytes());
        for (edge, vertex) in steps {
            body.extend_from_slice(&edge.to_be_bytes());
            body.extend_from_slice(&vertex.to_be_bytes());
        }
        let mut value = b"fgdb:graph-value:v1\0".to_vec();
        value.extend_from_slice(&(body.len() as u64).to_be_bytes());
        value.extend_from_slice(&body);
        let mut row = b"fgdb:graph-row:v1\0".to_vec();
        row.extend_from_slice(&1_u64.to_be_bytes());
        row.extend_from_slice(&(value.len() as u64).to_be_bytes());
        row.extend_from_slice(&value);
        row
    }
    let ((), report) = run_async_under_lab(0x50a7_0008, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let paths = [
            (0, vec![]), (0, vec![(2, 1)]), (0, vec![(1, u128::MAX), (9, 0)]),
            (0, vec![(1, u128::MAX)]), (u128::MAX, vec![]), (1, vec![(0, 0)]),
        ];
        let mut work = Work { cx: &cx, used: 0, limit: u64::MAX };
        for a in &paths {
            for b in &paths {
                let a_bytes = encoded(a.0, &a.1);
                let b_bytes = encoded(b.0, &b.1);
                canonical::validate(&a_bytes, 1, &mut work).unwrap();
                canonical::validate(&b_bytes, 1, &mut work).unwrap();
                assert_eq!(canonical::compare(&a_bytes, &b_bytes, &[GraphValueOrder::ascending(0)], 1, &mut work).unwrap(), a.cmp(b));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn run_catalog_capacity_is_admitted_before_source_io() {
    let ((), report) = run_async_under_lab(0x50a7_0009, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), 7).await;
        let pool = MemoryPool::new(24_000, 0).unwrap();
        let (mut source, observer) = scratch(&cx, &pool).await;
        let (mut destination, _) = scratch(&cx, &pool).await;
        let spool = plan().spool(&db, &cx, &GqlParameters::new(), policy(), &mut source, 97, 4096).await.unwrap();
        let held = pool.allocate_zeroed(&cx, pool.limit() - 1).unwrap();
        let before = observer.0.lock().unwrap().reads;
        assert!(matches!(spool.sort_into(&cx, &mut source, &mut destination, &[GraphValueOrder::ascending(0)], 2, 8, 101, u64::MAX).await,
            Err(NativeSpoolError::Spill(SpillError::Memory(_)))));
        assert_eq!(observer.0.lock().unwrap().reads, before);
        assert_eq!(destination.stats().reserved_runs, 0);
        assert_eq!(pool.used(), held.charged_bytes());
        drop(held);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

mod prepared;
