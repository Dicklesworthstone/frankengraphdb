//! Real authenticated spill/sort/merge with independent visible-tuple counts.
use super::*;
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_strata::tiered::memory::SpillLimits;
use fgdb_types::{CanonicalF64, PurposeContexts};
use std::collections::BTreeMap;
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
            drop(state); // the injected panic must not poison the test mutex
            panic!("injected aggregate difference read unwind");
        }
        if state.pending_read {
            return Poll::Pending;
        }
        let at = state.bytes.position() as usize;
        let n = out
            .remaining()
            .min(state.bytes.get_ref().len().saturating_sub(at));
        if n != 0 {
            out.put_slice(&state.bytes.get_ref()[at..at + n]);
            state.bytes.set_position((at + n) as u64);
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
    fn poll_seek(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        position: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(self.0.lock().unwrap().bytes.seek(position))
    }
}
async fn file(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, File) {
    let backing = File::default();
    let scratch = SpillFile::new(
        cx,
        backing.clone(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 64_000_000,
            max_runs: 100_000,
            max_run_bytes: 16_000_000,
        },
    )
    .await
    .unwrap();
    (scratch, backing)
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn row(key: i64, value: GraphAggregateValue) -> GraphAggregateRow {
    GraphAggregateRow::from_group_values(vec![scalar(key)], vec![value])
}
async fn seed(
    cx: &QueryCx,
    file: &mut SpillFile<File>,
    at: CommitSeq,
    rows: &[GraphAggregateRow],
    slots: &[GraphAggregateTextSlot],
) -> NativeAggregateSpool {
    let mut writer = file.paged_writer(cx, 127).unwrap();
    let mut largest = 0;
    for row in rows {
        let bytes = envelope(row).canonical_bytes().unwrap();
        largest = largest.max(bytes.len());
        writer
            .write(cx, &(bytes.len() as u64).to_be_bytes())
            .await
            .unwrap();
        writer.write(cx, &bytes).await.unwrap();
    }
    NativeAggregateSpool {
        inner: NativeResultSpool {
            columns: vec!["private-group".into(), "private-aggregate".into()].into(),
            encoded_columns: 2,
            snapshot: at,
            kind: ScanKind::Vertex,
            rows: GqlExecutionStats {
                snapshot_records: 0,
                result_rows: rows.len() as u64,
            },
            evaluator: GlaExecutionStats::default(),
            max_row_bytes: largest,
            run: writer.finish(cx).await.unwrap(),
        },
        columns: (0..slots.len())
            .map(|i| format!("out{i}"))
            .collect::<Vec<_>>()
            .into(),
        slots: slots.to_vec().into(),
        keys: vec!["private-group".into()].into(),
        aggregates: vec!["private-aggregate".into()].into(),
    }
}
type Bag = BTreeMap<Vec<GraphAggregateValue>, i128>;
fn projected(
    row: &GraphAggregateRow,
    slots: &[GraphAggregateTextSlot],
) -> Vec<GraphAggregateValue> {
    // Independent direct native mapping; deliberately do not call VisibleOrder/Cell.
    slots
        .iter()
        .map(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(at) => {
                GraphAggregateValue::Value(row.keys()[at].clone())
            }
            GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
        })
        .collect()
}
fn expected(
    before: &[GraphAggregateRow],
    after: &[GraphAggregateRow],
    slots: &[GraphAggregateTextSlot],
) -> Bag {
    let mut bag = Bag::new();
    for (rows, sign) in [(before, -1), (after, 1)] {
        for row in rows {
            *bag.entry(projected(row, slots)).or_default() += sign;
        }
    }
    bag.retain(|_, weight| *weight != 0);
    bag
}

#[allow(clippy::too_many_arguments)]
async fn collect(
    before: &NativeAggregateSpool,
    after: &NativeAggregateSpool,
    cx: &QueryCx,
    source: &mut SpillFile<File>,
    scratch: &mut SpillFile<File>,
    limit: u64,
    work: u64,
) -> (DifferenceResult<(u64, u64)>, Bag) {
    let mut rows = Bag::new();
    let mut previous = None;
    let result = before
        .visit_difference(
            after,
            cx,
            source,
            scratch,
            2,
            1000,
            61,
            limit,
            work,
            None,
            |weight, row| {
                assert_ne!(weight, 0);
                assert!(row.keys().is_empty());
                assert_eq!(row.values().len(), before.columns().len());
                let tuple = row.values().to_vec();
                if let Some(prior) = &previous {
                    assert!(prior < &tuple);
                }
                previous = Some(tuple.clone());
                assert!(rows.insert(tuple, weight).is_none());
                std::future::ready(Ok(()))
            },
        )
        .await;
    (result, rows)
}

#[test]
fn hidden_groups_consolidate_visible_rows_and_repeated_slots_keep_native_order() {
    let ((), report) = run_async_under_lab(0xad1f_1001, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        for slots in [
            vec![GraphAggregateTextSlot::Aggregate(0)],
            vec![
                GraphAggregateTextSlot::Aggregate(0),
                GraphAggregateTextSlot::GroupKey(0),
                GraphAggregateTextSlot::Aggregate(0),
            ],
        ] {
            for mut code in 0..81 {
                let (mut source, _) = file(&cx, &pool).await;
                let (mut scratch, _) = file(&cx, &pool).await;
                let mut counts = [0_usize; 4];
                for count in &mut counts {
                    *count = code % 3;
                    code /= 3;
                }
                let make = |offset: usize, start: i64| {
                    (0_usize..2)
                        .rev()
                        .flat_map(|value| {
                            (0..counts[offset + value]).map(move |id| {
                                row(
                                    start + 10 * value as i64 + id as i64,
                                    GraphAggregateValue::Count(value as u64),
                                )
                            })
                        })
                        .collect::<Vec<_>>()
                };
                let (before, after) = (make(0, 0), make(2, 100));
                let a = seed(&cx, &mut source, CommitSeq(1), &before, &slots).await;
                let b = seed(&cx, &mut source, CommitSeq(2), &after, &slots).await;
                for (a, b, left, right) in [(&a, &b, &before, &after), (&b, &a, &after, &before)] {
                    let (result, changes) =
                        collect(a, b, &cx, &mut source, &mut scratch, 100, u64::MAX).await;
                    assert_eq!(result.unwrap().0, changes.len() as u64);
                    assert_eq!(changes, expected(left, right, &slots));
                }
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn full_width_counts_sums_exact_averages_and_lists_keep_distinct_native_domains() {
    let ((), report) = run_async_under_lab(0xad1f_1002, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let values = [
            GraphAggregateValue::Count(1),
            GraphAggregateValue::Count(u64::MAX),
            GraphAggregateValue::Integer(i128::MIN),
            GraphAggregateValue::Integer(-1),
            GraphAggregateValue::Integer(1),
            GraphAggregateValue::Integer(i128::MAX),
            GraphAggregateValue::Value(scalar(1)),
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Float(
                CanonicalF64::new(1.0),
            ))),
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            GraphAggregateValue::Value(GraphValue::List(
                vec![scalar(1), scalar(2)].into_boxed_slice(),
            )),
            GraphAggregateValue::Value(GraphValue::List(
                vec![scalar(2), scalar(1)].into_boxed_slice(),
            )),
            GraphAggregateValue::Average(GraphExactAverage::new(1, 1).unwrap()),
            GraphAggregateValue::Average(GraphExactAverage::new(-1, 3).unwrap()),
            GraphAggregateValue::Average(GraphExactAverage::new(i128::MAX, u64::MAX).unwrap()),
        ];
        let after = values
            .iter()
            .enumerate()
            .rev()
            .map(|(i, value)| row(i as i64, value.clone()))
            .collect::<Vec<_>>();
        let slots = [GraphAggregateTextSlot::Aggregate(0)];
        let a = seed(&cx, &mut source, CommitSeq(1), &[], &slots).await;
        let b = seed(&cx, &mut source, CommitSeq(2), &after, &slots).await;
        let (result, changes) = collect(
            &a,
            &b,
            &cx,
            &mut source,
            &mut scratch,
            values.len() as u64,
            u64::MAX,
        )
        .await;
        assert_eq!(result.unwrap().0, values.len() as u64);
        assert_eq!(changes, expected(&[], &after, &slots));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn duplicate_payloads_larger_than_pool_share_exact_work_and_final_only_row_limits() {
    let ((), report) = run_async_under_lab(0xad1f_1003, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let text = GraphAggregateValue::Value(GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(&"value-é".repeat(128)).unwrap(),
        ));
        let before = (0..100).map(|i| row(i, text.clone())).collect::<Vec<_>>();
        let after = (0..99)
            .map(|i| row(i + 1000, text.clone()))
            .chain([row(2000, GraphAggregateValue::Integer(i128::MIN))])
            .collect::<Vec<_>>();
        let slots = [
            GraphAggregateTextSlot::Aggregate(0),
            GraphAggregateTextSlot::Aggregate(0),
        ];
        let a = seed(&cx, &mut source, CommitSeq(1), &before, &slots).await;
        let b = seed(&cx, &mut source, CommitSeq(2), &after, &slots).await;
        assert!(a.inner.encoded_len() > pool.limit());
        let (result, changes) = collect(&a, &b, &cx, &mut source, &mut scratch, 2, u64::MAX).await;
        let (rows, work) = result.unwrap();
        assert_eq!((rows, &changes), (2, &expected(&before, &after, &slots)));
        let (exact, again) = collect(&a, &b, &cx, &mut source, &mut scratch, rows, work).await;
        assert_eq!(exact.unwrap(), (rows, work));
        assert_eq!(again, changes);
        let (short, _) = collect(&a, &b, &cx, &mut source, &mut scratch, rows, work - 1).await;
        assert!(matches!(
            short,
            Err(GqlQueryError::Source(NativeAggregateSpoolError::Spool(
                NativeSpoolError::SortWorkLimit { .. }
            )))
        ));
        let (short, partial) = collect(&a, &b, &cx, &mut source, &mut scratch, 1, work).await;
        assert!(matches!(short, Err(GqlQueryError::Rows(_))));
        assert_eq!(partial.len(), 1);
        let (same, empty) = collect(&a, &a, &cx, &mut source, &mut scratch, 0, u64::MAX).await;
        assert_eq!(same.unwrap().0, 0);
        assert!(empty.is_empty());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bad_schemas_slots_and_second_run_cap_refuse_before_io() {
    let ((), report) = run_async_under_lab(0xad1f_1004, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        let (mut source, src) = file(&cx, &pool).await;
        let (mut scratch, dst) = file(&cx, &pool).await;
        let slots = [GraphAggregateTextSlot::Aggregate(0)];
        let a = seed(
            &cx,
            &mut source,
            CommitSeq(1),
            &[row(0, GraphAggregateValue::Count(1))],
            &slots,
        )
        .await;
        let b = seed(
            &cx,
            &mut source,
            CommitSeq(2),
            &[
                row(1, GraphAggregateValue::Count(2)),
                row(2, GraphAggregateValue::Count(3)),
            ],
            &slots,
        )
        .await;
        let io = |file: &File| {
            let s = file.0.lock().unwrap();
            (s.reads, s.writes)
        };
        let original = (io(&src), io(&dst));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let visitor = |_, _: NativeAggregateSpoolRow| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Ok(()))
        };
        assert!(matches!(
            a.visit_difference(
                &b,
                &cx,
                &mut source,
                &mut scratch,
                1,
                1,
                61,
                100,
                u64::MAX,
                None,
                visitor
            )
            .await,
            Err(GqlQueryError::Source(NativeAggregateSpoolError::Spool(
                NativeSpoolError::SortRunLimit {
                    required: 2,
                    limit: 1
                }
            )))
        ));
        let mut wrong = b.clone();
        wrong.columns = vec!["different".into()].into();
        assert!(
            a.visit_difference(
                &wrong,
                &cx,
                &mut source,
                &mut scratch,
                1,
                100,
                61,
                100,
                u64::MAX,
                None,
                visitor
            )
            .await
            .is_err()
        );
        let mut wrong = a.clone();
        wrong.slots = vec![GraphAggregateTextSlot::Aggregate(5)].into();
        assert!(
            wrong
                .visit_difference(
                    &wrong,
                    &cx,
                    &mut source,
                    &mut scratch,
                    1,
                    100,
                    61,
                    100,
                    u64::MAX,
                    None,
                    visitor
                )
                .await
                .is_err()
        );
        assert_eq!((io(&src), io(&dst)), original);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn omitted_aggregate_fields_are_validated_before_any_callback() {
    let ((), report) = run_async_under_lab(0xad1f_1005, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let slots = [GraphAggregateTextSlot::GroupKey(0)];
        let a = seed(&cx, &mut source, CommitSeq(1), &[], &slots).await;
        let mut b = seed(&cx, &mut source, CommitSeq(2), &[], &slots).await;
        // Authenticated scratch containing a valid row frame but invalid private
        // aggregate envelope. The invalid cell is NOT selected for delivery.
        let malformed = GraphValueRow::from_owned_values(vec![scalar(7), scalar(42)])
            .canonical_bytes()
            .unwrap();
        let mut writer = source.paged_writer(&cx, 61).unwrap();
        writer
            .write(&cx, &(malformed.len() as u64).to_be_bytes())
            .await
            .unwrap();
        writer.write(&cx, &malformed).await.unwrap();
        b.inner.run = writer.finish(&cx).await.unwrap();
        b.inner.rows.result_rows = 1;
        b.inner.max_row_bytes = malformed.len();
        let (result, partial) = collect(&a, &b, &cx, &mut source, &mut scratch, 1, u64::MAX).await;
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(NativeAggregateSpoolError::Spool(
                NativeSpoolError::Spill(SpillError::InvalidRun)
            )))
        ));
        assert!(partial.is_empty());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn callback_backpressure_retained_output_and_callback_failure_release_merge_state() {
    let ((), report) = run_async_under_lab(0xad1f_1006, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        let (mut source, src) = file(&cx, &pool).await;
        let (mut scratch, dst) = file(&cx, &pool).await;
        let slots = [GraphAggregateTextSlot::Aggregate(0)];
        let a = seed(&cx, &mut source, CommitSeq(1), &[], &slots).await;
        let rows = (0..20)
            .map(|i| row(i, GraphAggregateValue::Count(i as u64)))
            .collect::<Vec<_>>();
        let b = seed(&cx, &mut source, CommitSeq(2), &rows, &slots).await;
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let mut future = Box::pin(a.visit_difference(
            &b,
            &cx,
            &mut source,
            &mut scratch,
            2,
            100,
            61,
            100,
            u64::MAX,
            None,
            |_, row| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async move {
                    std::future::pending::<()>().await;
                    drop(row);
                    Ok(())
                }
            },
        ));
        let mut task = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut task).is_pending());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let reads = (src.0.lock().unwrap().reads, dst.0.lock().unwrap().reads);
        assert!(pool.used() > 0);
        assert!(future.as_mut().poll(&mut task).is_pending());
        assert_eq!(
            (src.0.lock().unwrap().reads, dst.0.lock().unwrap().reads),
            reads
        );
        drop(future);
        assert_eq!(pool.used(), 0);
        assert!(!source.is_poisoned() && !scratch.is_poisoned());
        calls.store(0, std::sync::atomic::Ordering::SeqCst);
        let error = a
            .visit_difference(
                &b,
                &cx,
                &mut source,
                &mut scratch,
                2,
                100,
                61,
                100,
                u64::MAX,
                None,
                |_, _| {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    std::future::ready(Err(SpillError::InvalidRun.into()))
                },
            )
            .await;
        assert!(error.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(pool.used(), 0);
        let mut retained = Vec::new();
        let result = a
            .visit_difference(
                &b,
                &cx,
                &mut source,
                &mut scratch,
                2,
                100,
                61,
                100,
                u64::MAX,
                None,
                |weight, row| {
                    retained.push((weight, row));
                    std::future::ready(Ok(()))
                },
            )
            .await
            .unwrap();
        assert_eq!(result.0, 20);
        assert!(pool.used() > 0);
        drop(source);
        drop(scratch);
        assert_eq!(retained[0].1.values(), [GraphAggregateValue::Count(0)]);
        drop(retained);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pending_io_drop_and_unwind_do_not_make_poisoned_files_reusable() {
    let ((), report) = run_async_under_lab(0xad1f_1007, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        for panic in [false, true] {
            let (mut source, src) = file(&cx, &pool).await;
            let (mut scratch, _) = file(&cx, &pool).await;
            let slots = [GraphAggregateTextSlot::Aggregate(0)];
            let a = seed(
                &cx,
                &mut source,
                CommitSeq(1),
                &[row(0, GraphAggregateValue::Count(1))],
                &slots,
            )
            .await;
            let b = seed(&cx, &mut source, CommitSeq(2), &[], &slots).await;
            {
                let mut state = src.0.lock().unwrap();
                state.pending_read = !panic;
                state.panic_read = panic;
            }
            let future = Box::pin(a.visit_difference(
                &b,
                &cx,
                &mut source,
                &mut scratch,
                2,
                100,
                61,
                100,
                u64::MAX,
                None,
                |_, _| std::future::ready(Ok(())),
            ));
            // Dropping an unpolled future performs no I/O.
            drop(future);
            assert_eq!(src.0.lock().unwrap().reads, 0);
            let mut future = Box::pin(a.visit_difference(
                &b,
                &cx,
                &mut source,
                &mut scratch,
                2,
                100,
                61,
                100,
                u64::MAX,
                None,
                |_, _| std::future::ready(Ok(())),
            ));
            let mut task = Context::from_waker(Waker::noop());
            if panic {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _ = future.as_mut().poll(&mut task);
                    }))
                    .is_err()
                );
            } else {
                assert!(future.as_mut().poll(&mut task).is_pending());
            }
            drop(future);
            assert!(source.is_poisoned());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_observed_checkpoint_refuses_without_claiming_a_complete_transition() {
    use fgdb_types::context::SimulationCheckpointProbe;
    let ((), report) = run_async_under_lab(0xad1f_1008, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        let slots = [GraphAggregateTextSlot::Aggregate(0)];
        let before = [row(0, GraphAggregateValue::Count(1))];
        let after = [row(1, GraphAggregateValue::Count(2))];
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let measured = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let a = seed(&cx, &mut source, CommitSeq(1), &before, &slots).await;
        let b = seed(&cx, &mut source, CommitSeq(2), &after, &slots).await;
        let (result, complete) =
            collect(&a, &b, &measured, &mut source, &mut scratch, 2, u64::MAX).await;
        result.unwrap();
        assert_eq!(complete, expected(&before, &after, &slots));
        assert!(probe.calls() > 5);
        for stop in 1..=probe.calls() {
            let (mut source, _) = file(&cx, &pool).await;
            let (mut scratch, _) = file(&cx, &pool).await;
            let a = seed(&cx, &mut source, CommitSeq(1), &before, &slots).await;
            let b = seed(&cx, &mut source, CommitSeq(2), &after, &slots).await;
            let control = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&control));
            let (result, partial) =
                collect(&a, &b, &interrupted, &mut source, &mut scratch, 2, u64::MAX).await;
            assert!(result.is_err(), "checkpoint {stop}");
            assert_eq!(control.calls(), stop);
            for (row, weight) in partial {
                assert_eq!(complete.get(&row), Some(&weight));
            }
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn real_historical_aggregate_spools_match_query_diff_after_writer_and_templates_drop() {
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    use fgdb_types::{DatabaseSecurityNamespaceId, VId};
    let ((), report) = run_async_under_lab(0xad1f_1009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(
            &contexts.commit(),
            crate::DatabaseKeys::new(
                [0x41; 32],
                DatabaseSecurityNamespaceId([0x42; 32]),
                [0x43; 32],
            ),
        )
        .await
        .unwrap();
        let mut batch = crate::WriteBatch::new(RelationId(1));
        for (id, p, q) in [(1, i64::MAX, 0), (2, i64::MAX, 0), (3, 1, 1), (4, 2, 1)] {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(p)),
                    (PropertyKeyId(2), CanonicalScalar::Int(q)),
                ],
            );
        }
        let first = db.write(&contexts.commit(), batch).await.unwrap();
        let before = db.read_session().unwrap();
        let mut batch = crate::WriteBatch::new(RelationId(1));
        batch.delete_vertex(VId(2));
        batch.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(3)));
        let last = db.write(&contexts.commit(), batch).await.unwrap();
        let after = db.read_session().unwrap();
        drop(db);
        let resolver = |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        };
        let policy = GqlQueryPolicy::new(100_000, 100_000, 100_000_000, 100_000_000);
        for text in [
            "MATCH (n) RETURN COUNT(*) AS n,SUM(n.p) AS total,AVG(n.p) AS average",
            "MATCH (n) RETURN COUNT(*) AS n GROUP BY n.q",
            "MATCH (n) RETURN n.q AS category,SUM(n.p) AS total,n.q AS again GROUP BY n.q",
            "MATCH (n) RETURN AVG(n.p) AS average,COUNT(*) AS n GROUP BY n.q ORDER BY average DESC LIMIT 1",
            "MATCH (n) RETURN COLLECT(n.p) AS items,COLLECT(DISTINCT n.q) AS groups",
            "MATCH (n) RETURN COUNT(*) AS n GROUP BY n.q HAVING SUM(n.p)>0 LIMIT 0",
        ] {
            let prepared =
                PreparedNativeRead::prepare(text, &GqlParameters::new(), resolver).unwrap();
            let delta = prepared
                .diff_in_view(&after, &cx, &GqlParameters::new(), first, last, policy)
                .unwrap();
            let expected: Bag = delta
                .changes()
                .iter()
                .map(|(row, weight)| (row.to_vec(), weight.to_i128().unwrap()))
                .collect();
            let pool = MemoryPool::new(512 * 1024, 0).unwrap();
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut auxiliary, _) = file(&cx, &pool).await;
            let (mut source, _) = file(&cx, &pool).await;
            let (a, _) = prepared
                .spool_aggregate_in_view(
                    &before,
                    &cx,
                    &GqlParameters::new(),
                    policy,
                    &mut scratch,
                    &mut auxiliary,
                    &mut source,
                    1,
                    100,
                    2,
                    100,
                    127,
                    8192,
                    1000,
                    100_000_000,
                    None,
                )
                .await
                .unwrap();
            let (b, _) = prepared
                .spool_aggregate_in_view(
                    &after,
                    &cx,
                    &GqlParameters::new(),
                    policy,
                    &mut scratch,
                    &mut auxiliary,
                    &mut source,
                    1,
                    100,
                    2,
                    100,
                    127,
                    8192,
                    1000,
                    100_000_000,
                    None,
                )
                .await
                .unwrap();
            drop(prepared);
            drop(auxiliary);
            assert_eq!((a.snapshot_seq(), b.snapshot_seq()), (first, last));
            let (result, actual) = collect(
                &a,
                &b,
                &cx,
                &mut source,
                &mut scratch,
                expected.len() as u64,
                u64::MAX,
            )
            .await;
            result.unwrap();
            assert_eq!(actual, expected, "{text}");
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_authenticated_corruption_cannot_turn_a_delivered_prefix_into_success() {
    let ((), report) = run_async_under_lab(0xad1f_1010, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, backing) = file(&cx, &pool).await;
        let slots = [GraphAggregateTextSlot::Aggregate(0)];
        let rows = (0..32)
            .map(|i| row(i, GraphAggregateValue::Count(i as u64)))
            .collect::<Vec<_>>();
        let a = seed(&cx, &mut source, CommitSeq(1), &[], &slots).await;
        let b = seed(&cx, &mut source, CommitSeq(2), &rows, &slots).await;
        let mut prefix = Vec::new();
        let result = a
            .visit_difference(
                &b,
                &cx,
                &mut source,
                &mut scratch,
                2,
                100,
                61,
                32,
                u64::MAX,
                None,
                |weight, row| {
                    if prefix.is_empty() {
                        assert_eq!(row.values(), [GraphAggregateValue::Count(0)]);
                        // Test-only backend corruption, not a production file accessor.
                        for byte in backing.0.lock().unwrap().bytes.get_mut() {
                            *byte ^= 1;
                        }
                    }
                    prefix.push((weight, row));
                    std::future::ready(Ok(()))
                },
            )
            .await;
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(NativeAggregateSpoolError::Spool(
                NativeSpoolError::Spill(SpillError::ChecksumMismatch)
            )))
        ));
        assert!(!prefix.is_empty() && prefix.len() < 32);
        assert_eq!(prefix[0].1.values(), [GraphAggregateValue::Count(0)]);
        drop(prefix);
        assert_eq!(pool.used(), 0);
        // The corrupted destination is retired; original source handles survive.
        drop(scratch);
        let (mut fresh, _) = file(&cx, &pool).await;
        let (retry, changes) = collect(&a, &b, &cx, &mut source, &mut fresh, 32, u64::MAX).await;
        assert_eq!(retry.unwrap().0, 32);
        assert_eq!(changes, expected(&[], &rows, &slots));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
