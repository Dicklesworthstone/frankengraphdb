//! The pull API drives the actual sorter, authenticated pages and merge owner.
use super::*;
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_gql::algebra::GraphValue;
use fgdb_strata::tiered::memory::SpillLimits;
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalF64, CanonicalScalar, PurposeContexts, VId};
use std::collections::BTreeMap;
use std::future::ready;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct State {
    bytes: Cursor<Vec<u8>>,
    reads: usize,
    writes: usize,
    pending_read: bool,
    panic_read: bool,
}
#[derive(Clone, Default)]
struct File(Arc<Mutex<State>>);
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
        if state.panic_read {
            drop(state); // keep the test's observation mutex usable after unwind
            panic!("injected scratch read unwind");
        }
        if state.pending_read {
            return Poll::Pending;
        }
        let at = state.bytes.position() as usize;
        let count = out.remaining().min(state.bytes.get_ref().len().saturating_sub(at));
        if count != 0 {
            out.put_slice(&state.bytes.get_ref()[at..at + count]);
            state.bytes.set_position((at + count) as u64);
        }
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for File {
    fn poll_write(
        self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        state.writes += 1;
        Poll::Ready(std::io::Write::write(&mut state.bytes, bytes))
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
    let file = SpillFile::new(cx, backing.clone(), pool.clone(), SpillLimits {
        max_file_bytes: 64_000_000,
        max_runs: 500_000,
        max_run_bytes: 16_000_000,
    }).await.unwrap();
    (file, backing)
}
fn row(value: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(value))])
}
async fn seed(cx: &QueryCx, file: &mut SpillFile<File>, rows: &[GraphValueRow]) -> NativeResultSpool {
    let mut writer = file.paged_writer(cx, 127).unwrap();
    let mut largest = 0;
    for row in rows {
        let bytes = row.canonical_bytes().unwrap();
        largest = largest.max(bytes.len());
        writer.write(cx, &(bytes.len() as u64).to_be_bytes()).await.unwrap();
        writer.write(cx, &bytes).await.unwrap();
    }
    NativeResultSpool {
        columns: vec!["value".into()].into(),
        encoded_columns: 1,
        snapshot: CommitSeq(17),
        kind: ScanKind::Values,
        rows: GqlExecutionStats { snapshot_records: 0, result_rows: rows.len() as u64 },
        evaluator: GlaExecutionStats::default(),
        max_row_bytes: largest,
        run: writer.finish(cx).await.unwrap(),
    }
}
type Changes = Vec<(GraphValueRow, i128)>;
fn expected(before: &[GraphValueRow], after: &[GraphValueRow]) -> Changes {
    let mut bag = BTreeMap::<GraphValueRow, i128>::new();
    for (rows, sign) in [(before, -1), (after, 1)] {
        for row in rows {
            *bag.entry(row.clone()).or_default() += sign;
        }
    }
    bag.into_iter().filter(|(_, weight)| *weight != 0).collect()
}
async fn drain(cursor: &mut NativeSpoolDifference<'_, File>) -> VisitResult<Changes> {
    let mut changes = Vec::new();
    while let Some((weight, frame)) = cursor.next_change().await? {
        changes.push((GraphValueRow::decode_canonical(frame.as_ref()).unwrap(), weight));
    }
    Ok(changes)
}

#[test]
fn pull_and_visitor_share_ordered_groups_and_the_same_work_schedule() {
    let ((), report) = run_async_under_lab(0xd1ff_5101, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        for mut code in 0..81 {
            let mut counts = [0_usize; 4];
            for count in &mut counts { *count = code % 3; code /= 3; }
            let make = |offset| (0_usize..2).rev().flat_map(|key| {
                std::iter::repeat_n(row(key as i64), counts[offset + key])
            }).collect::<Vec<_>>();
            let (before, after) = (make(0), make(2));
            let (mut source, _) = file(&cx, &pool).await;
            let (mut scratch, _) = file(&cx, &pool).await;
            let a = seed(&cx, &mut source, &before).await;
            let b = seed(&cx, &mut source, &after).await;
            for run_rows in [1, 3] {
                for (a, b, left, right) in [(&a, &b, &before, &after), (&b, &a, &after, &before)] {
                    let mut cursor = a.difference_stream(b, &cx, &mut source, &mut scratch,
                        run_rows, 100, 61, 2, u64::MAX).await.unwrap();
                    assert_eq!(cursor.changed_rows(), 0);
                    assert_eq!(cursor.state(), ScanState::Open);
                    let changes = drain(&mut cursor).await.unwrap();
                    assert_eq!(changes, expected(left, right));
                    assert_eq!(cursor.state(), ScanState::Exhausted);
                    let measured = (cursor.changed_rows(), cursor.work_units());
                    cursor.close();
                    assert_eq!(cursor.state(), ScanState::Exhausted);
                    assert!(cursor.next_change().await.unwrap().is_none());
                    assert_eq!((cursor.changed_rows(), cursor.work_units()), measured);
                    drop(cursor);
                    let mut visited = Vec::new();
                    let (n, work) = a.visit_difference(b, &cx, &mut source, &mut scratch,
                        run_rows, 100, 61, 2, u64::MAX, |weight, frame| {
                            visited.push((GraphValueRow::decode_canonical(frame.as_ref()).unwrap(), weight));
                            ready(Ok(()))
                        }).await.unwrap();
                    assert_eq!(visited, changes);
                    // Exactly one extra admission after each external callback.
                    // Factoring out the merge must not reset or duplicate work.
                    assert_eq!((n, work), (measured.0, measured.1 + measured.0));
                    assert_eq!(pool.used(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn idle_close_and_unpolled_drop_do_not_pull_and_inputs_can_drop_after_open() {
    let ((), report) = run_async_under_lab(0xd1ff_5102, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, backing) = file(&cx, &pool).await;
        let a = seed(&cx, &mut source, &[]).await;
        let mut b = seed(&cx, &mut source, &(0..20).map(row).collect::<Vec<_>>()).await;
        b.snapshot = CommitSeq(18); // metadata-only fixture, not a history assertion
        let mut cursor = a.difference_stream(&b, &cx, &mut source, &mut scratch,
            3, 100, 17, 20, u64::MAX).await.unwrap();
        drop(a);
        drop(b);
        drop(source); // compile-time proof that the original owner is not borrowed
        assert_eq!(cursor.columns(), ["value"]);
        assert_eq!((cursor.before(), cursor.after()), (CommitSeq(17), CommitSeq(18)));
        let io = { let state = backing.0.lock().unwrap(); (state.reads, state.writes) };
        let work = cursor.work_units();
        drop(cursor.next_change()); // constructing a future is not a pull
        assert_eq!(cursor.work_units(), work);
        assert_eq!(cursor.state(), ScanState::Open);
        assert_eq!({ let state = backing.0.lock().unwrap(); (state.reads, state.writes) }, io);
        let (weight, frame) = cursor.next_change().await.unwrap().unwrap();
        assert_eq!(weight, 1);
        assert_eq!(GraphValueRow::decode_canonical(frame.as_ref()).unwrap(), row(0));
        let io = { let state = backing.0.lock().unwrap(); (state.reads, state.writes) };
        let work = cursor.work_units();
        assert_eq!(cursor.changed_rows(), 1);
        assert!(pool.used() > frame.charged_bytes());
        cursor.close();
        cursor.close();
        assert_eq!(cursor.state(), ScanState::Closed);
        assert!(cursor.next_change().await.unwrap().is_none());
        assert_eq!(cursor.work_units(), work);
        assert_eq!({ let state = backing.0.lock().unwrap(); (state.reads, state.writes) }, io);
        assert_eq!(pool.used(), frame.charged_bytes());
        drop(frame);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_limits_persist_across_pulls_and_a_full_prefix_is_not_completion() {
    let ((), report) = run_async_under_lab(0xd1ff_5103, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, backing) = file(&cx, &pool).await;
        let a = seed(&cx, &mut source, &[row(3), row(1), row(1)]).await;
        let b = seed(&cx, &mut source, &[row(2), row(1)]).await;
        let mut baseline = a.difference_stream(&b, &cx, &mut source, &mut scratch,
            1, 100, 61, 3, u64::MAX).await.unwrap();
        let wanted = drain(&mut baseline).await.unwrap();
        let work = baseline.work_units();
        assert_eq!(wanted.len(), 3);
        drop(baseline);
        for (rows, allowance, okay) in [(3, work, true), (3, work - 1, false), (2, work, false)] {
            let mut cursor = a.difference_stream(&b, &cx, &mut source, &mut scratch,
                1, 100, 61, rows, allowance).await.unwrap();
            let mut actual = Vec::new();
            let mut failed = false;
            loop {
                match cursor.next_change().await {
                    Ok(Some((weight, frame))) => {
                        actual.push((GraphValueRow::decode_canonical(frame.as_ref()).unwrap(), weight));
                    }
                    Ok(None) => break,
                    Err(error) => {
                        if rows == 2 {
                            assert!(matches!(error, GqlQueryError::Rows(error)
                                if error.limit == 2 && error.observed == 3));
                        } else {
                            assert!(matches!(error, GqlQueryError::Source(NativeSpoolError::SortWorkLimit { .. })));
                        }
                        failed = true;
                        break;
                    }
                }
            }
            assert_eq!(failed, !okay);
            assert!(wanted.starts_with(&actual));
            if okay {
                assert_eq!(actual, wanted);
                assert_eq!(cursor.state(), ScanState::Exhausted);
                assert_eq!(cursor.work_units(), work);
            } else {
                assert_eq!(cursor.state(), ScanState::Failed);
                // A missing final checkpoint must fail even after all rows left.
                assert_eq!(actual.len() as u64, rows);
                let reads = backing.0.lock().unwrap().reads;
                let used = cursor.work_units();
                cursor.close();
                assert_eq!(cursor.state(), ScanState::Failed);
                assert!(cursor.next_change().await.unwrap().is_none());
                assert_eq!(backing.0.lock().unwrap().reads, reads);
                assert_eq!(cursor.work_units(), used);
            }
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_a_pending_pull_fuses_the_cursor_and_keeps_the_file_poisoned() {
    let ((), report) = run_async_under_lab(0xd1ff_5104, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, backing) = file(&cx, &pool).await;
        let a = seed(&cx, &mut source, &[]).await;
        let rows: Vec<_> = (0..20).map(row).collect();
        let b = seed(&cx, &mut source, &rows).await;
        let mut cursor = a.difference_stream(&b, &cx, &mut source, &mut scratch,
            3, 100, 17, 20, u64::MAX).await.unwrap();
        let (_, first) = cursor.next_change().await.unwrap().unwrap();
        drop(first);
        assert!(pool.used() > 0);
        backing.0.lock().unwrap().pending_read = true;
        {
            // A Context is not Send: keep it out of the lab future's later awaits.
            let mut future = cursor.next_change();
            let mut task = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut task).is_pending());
            drop(future);
        }
        assert_eq!(cursor.state(), ScanState::Failed);
        assert_eq!(cursor.changed_rows(), 1);
        assert_eq!(pool.used(), 0);
        let reads = backing.0.lock().unwrap().reads;
        assert!(cursor.next_change().await.unwrap().is_none());
        assert_eq!(backing.0.lock().unwrap().reads, reads);
        drop(cursor);
        assert!(scratch.is_poisoned());
        // A new destination is mandatory; never "repair" a pending I/O owner.
        let (mut replacement, _) = file(&cx, &pool).await;
        let mut retried = a.difference_stream(&b, &cx, &mut source, &mut replacement,
            3, 100, 17, 20, u64::MAX).await.unwrap();
        assert_eq!(drain(&mut retried).await.unwrap(), expected(&[], &rows));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unwind_and_late_corruption_never_resurrect_a_partial_merge() {
    let ((), report) = run_async_under_lab(0xd1ff_5105, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        for unwind in [true, false] {
            let (mut source, _) = file(&cx, &pool).await;
            let (mut scratch, backing) = file(&cx, &pool).await;
            let a = seed(&cx, &mut source, &[]).await;
            let b = seed(&cx, &mut source, &(0..20).map(row).collect::<Vec<_>>()).await;
            let mut cursor = a.difference_stream(&b, &cx, &mut source, &mut scratch,
                3, 100, 17, 20, u64::MAX).await.unwrap();
            let (_, first) = cursor.next_change().await.unwrap().unwrap();
            if unwind {
                backing.0.lock().unwrap().panic_read = true;
                let mut future = cursor.next_change();
                let mut task = Context::from_waker(Waker::noop());
                assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _ = future.as_mut().poll(&mut task);
                })).is_err());
                drop(future);
            } else {
                for byte in backing.0.lock().unwrap().bytes.get_mut() {
                    *byte ^= 1;
                }
                assert!(matches!(cursor.next_change().await,
                    Err(GqlQueryError::Source(NativeSpoolError::Spill(SpillError::ChecksumMismatch)))));
            }
            assert_eq!(cursor.state(), ScanState::Failed);
            assert_eq!(cursor.changed_rows(), 1);
            assert_eq!(GraphValueRow::decode_canonical(first.as_ref()).unwrap(), row(0));
            assert_eq!(pool.used(), first.charged_bytes());
            let reads = backing.0.lock().unwrap().reads;
            assert!(cursor.next_change().await.unwrap().is_none());
            assert_eq!(backing.0.lock().unwrap().reads, reads);
            drop(first);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_observed_pull_cancellation_points_refuse_with_fused_state_and_no_leak() {
    let ((), report) = run_async_under_lab(0xd1ff_5106, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let a = seed(&cx, &mut source, &[row(1)]).await;
        let b = seed(&cx, &mut source, &[row(2)]).await;
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
        let mut cursor = a.difference_stream(&b, &observed, &mut source, &mut scratch,
            1, 100, 61, 2, u64::MAX).await.unwrap();
        let opening = probe.calls();
        assert_eq!(drain(&mut cursor).await.unwrap().len(), 2);
        let total = probe.calls();
        assert!(total > opening + 5);
        drop(cursor);
        for stop in opening + 1..=total {
            let (mut source, _) = file(&cx, &pool).await;
            let (mut scratch, _) = file(&cx, &pool).await;
            let a = seed(&cx, &mut source, &[row(1)]).await;
            let b = seed(&cx, &mut source, &[row(2)]).await;
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut cursor = a.difference_stream(&b, &interrupted, &mut source, &mut scratch,
                1, 100, 61, 2, u64::MAX).await.unwrap();
            assert_eq!(probe.calls(), opening);
            assert!(drain(&mut cursor).await.is_err(), "checkpoint {stop}");
            assert_eq!(probe.calls(), stop);
            assert_eq!(cursor.state(), ScanState::Failed);
            assert!(cursor.next_change().await.unwrap().is_none());
            assert_eq!(probe.calls(), stop);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn large_duplicate_inputs_keep_typed_domains_and_zero_cap_cancellation() {
    let ((), report) = run_async_under_lab(0xd1ff_5107, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let text = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(&"payload\0é".repeat(80)).unwrap(),
        )]);
        let mut before = vec![text.clone(); 256];
        before.push(row(7));
        let mut after = vec![text; 255];
        after.extend([
            GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(7.0)))]),
            GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(u128::MAX))]),
        ]);
        let a = seed(&cx, &mut source, &before).await;
        let b = seed(&cx, &mut source, &after).await;
        assert!(a.encoded_len() > pool.limit());
        assert!(b.encoded_len() > pool.limit());
        let mut cursor = a.difference_stream(&b, &cx, &mut source, &mut scratch,
            3, 100, 61, 4, u64::MAX).await.unwrap();
        assert_eq!(drain(&mut cursor).await.unwrap(), expected(&before, &after));
        drop(cursor);
        assert_eq!(pool.used(), 0);
        let mut cursor = a.difference_stream(&a, &cx, &mut source, &mut scratch,
            3, 100, 61, 0, u64::MAX).await.unwrap();
        assert!(cursor.next_change().await.unwrap().is_none());
        assert_eq!(cursor.state(), ScanState::Exhausted);
        assert_eq!(cursor.changed_rows(), 0);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
