//! Real authenticated scratch runs. The expected order uses decoded native
//! values, not the production frame comparator or a second merge schedule.

use super::*;
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_gql::algebra::GraphValue;
use fgdb_strata::tiered::memory::SpillLimits;
use fgdb_types::{CanonicalScalar, PurposeContexts};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::task::{Context, Poll};

#[derive(Default)]
struct State {
    bytes: Cursor<Vec<u8>>,
    reads: usize,
    blocked_writes: usize,
    block_writes: bool,
}
#[derive(Clone, Default)]
struct File(Arc<Mutex<State>>);
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
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
        if state.block_writes {
            state.blocked_writes += 1;
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
    fn poll_seek(self: Pin<&mut Self>, _: &mut Context<'_>, to: SeekFrom) -> Poll<io::Result<u64>> {
        Poll::Ready(self.0.lock().unwrap().bytes.seek(to))
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
            max_runs: 500_000,
            max_run_bytes: 16_000_000,
        },
    )
    .await
    .unwrap();
    (file, backing)
}
fn rows(count: usize) -> Vec<GraphValueRow> {
    (0..count)
        .map(|id| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Scalar(if id % 11 == 0 {
                    CanonicalScalar::Null
                } else {
                    CanonicalScalar::Int((id * 13 % 19) as i64 - 9)
                }),
                GraphValue::Scalar(CanonicalScalar::Int(id as i64)),
            ])
        })
        .collect()
}
fn encoded(rows: &[GraphValueRow]) -> Vec<Vec<u8>> {
    rows.iter()
        .map(|row| row.canonical_bytes().unwrap())
        .collect()
}
async fn seed(
    cx: &QueryCx,
    file: &mut SpillFile<File>,
    rows: &[GraphValueRow],
) -> NativeResultSpool {
    let frames = encoded(rows);
    let mut writer = file.paged_writer(cx, 127).unwrap();
    for frame in &frames {
        writer
            .write(cx, &(frame.len() as u64).to_be_bytes())
            .await
            .unwrap();
        writer.write(cx, frame).await.unwrap();
    }
    NativeResultSpool {
        columns: vec!["key".into(), "ordinal".into()].into(),
        encoded_columns: 2,
        snapshot: CommitSeq(17),
        kind: ScanKind::Edge,
        rows: GqlExecutionStats {
            snapshot_records: 91,
            result_rows: rows.len() as u64,
        },
        evaluator: GlaExecutionStats {
            work_units: 101,
            scratch_entries: 103,
        },
        max_row_bytes: frames.iter().map(Vec::len).max().unwrap_or(0),
        run: writer.finish(cx).await.unwrap(),
    }
}
async fn read(cx: &QueryCx, file: &mut SpillFile<File>, spool: &NativeResultSpool) -> Vec<Vec<u8>> {
    let mut reader = spool.reader(file);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await.unwrap() {
        rows.push(row.as_ref().to_vec());
    }
    assert_eq!(reader.state(), ScanState::Exhausted);
    rows
}

#[test]
fn level_catalog_bound_includes_the_top_u64_bit_without_overflow() {
    assert_eq!(catalog_slots(0), 1);
    for bit in 0..u64::BITS {
        let count = 1_u64 << bit;
        assert_eq!(catalog_slots(count), bit as usize + 1);
        if bit > 0 {
            assert_eq!(catalog_slots(count - 1), bit as usize);
        }
    }
    assert_eq!(catalog_slots(u64::MAX), 64);
}

#[test]
fn many_tiny_runs_fit_when_the_old_run_catalog_alone_exceeds_the_pool() {
    let ((), report) = run_async_under_lab(0x50a7_1001, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let count = 1024_usize;
        assert!(size_of::<Run>() * (count + count.div_ceil(2)) > pool.limit());
        let (mut source, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let input = rows(count);
        let spool = seed(&cx, &mut source, &input).await;
        assert_eq!(pool.used(), 0);
        let (result, _) = spool
            .sort_into(
                &cx,
                &mut source,
                &mut destination,
                &[GraphValueOrder {
                    column: 0,
                    descending: false,
                    nulls_first: true,
                }],
                1,
                count,
                127,
                u64::MAX,
            )
            .await
            .unwrap();
        let mut expected = input.clone();
        expected.sort();
        assert_eq!(
            read(&cx, &mut destination, &result).await,
            encoded(&expected)
        );
        assert_eq!(read(&cx, &mut source, &spool).await, encoded(&input));
        assert_eq!(result.snapshot_seq(), spool.snapshot_seq());
        assert_eq!(result.row_stats(), spool.row_stats());
        assert_eq!(result.evaluator_stats(), spool.evaluator_stats());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn uneven_run_counts_preserve_frames_and_empty_results() {
    let ((), report) = run_async_under_lab(0x50a7_1002, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        for count in [0, 1, 2, 3, 5, 7, 8, 9, 15, 16, 17, 31, 33, 65] {
            for width in [1_usize, 2, 7] {
                let pool = MemoryPool::new(1_000_000, 0).unwrap();
                let (mut source, _) = file(&cx, &pool).await;
                let (mut destination, _) = file(&cx, &pool).await;
                let input = rows(count);
                let spool = seed(&cx, &mut source, &input).await;
                let (sorted, _) = spool
                    .sort_into(
                        &cx,
                        &mut source,
                        &mut destination,
                        &[GraphValueOrder {
                            column: 0,
                            descending: false,
                            nulls_first: true,
                        }],
                        width,
                        count.div_ceil(width).max(1),
                        31,
                        u64::MAX,
                    )
                    .await
                    .unwrap();
                let mut expected = input.clone();
                expected.sort();
                assert_eq!(
                    read(&cx, &mut destination, &sorted).await,
                    encoded(&expected)
                );
                assert_eq!(read(&cx, &mut source, &spool).await, encoded(&input));
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// Equal comparator keys need not imply identical encoded rows (for example,
// external join keys). With one row per initial run, left-first merging must
// preserve source order even for runs accumulated in different scratch files.
struct EqualOrder {
    validated: Arc<AtomicUsize>,
}
impl FrameOrder for EqualOrder {
    type Error = NativeSpoolError;
    fn admit(&mut self, _: usize) -> Result<()> {
        Ok(())
    }
    fn validate(&mut self, row: &[u8], columns: usize, work: &mut Work<'_>) -> Result<()> {
        canonical::validate(row, columns, work)?;
        self.validated.fetch_add(1, AtomicOrdering::Relaxed);
        Ok(())
    }
    fn compare(&mut self, _: &[u8], _: &[u8], _: usize, work: &mut Work<'_>) -> Result<Ordering> {
        work.charge(1)?;
        Ok(Ordering::Equal)
    }
}

#[test]
fn equal_key_representatives_keep_left_to_right_source_order() {
    let ((), report) = run_async_under_lab(0x50a7_1003, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        for count in [3, 5, 6, 7, 9, 13, 23, 31, 33] {
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let (mut source, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let input = rows(count);
            let spool = seed(&cx, &mut source, &input).await;
            let mut order = EqualOrder {
                validated: Arc::new(AtomicUsize::new(0)),
            };
            let (result, _) = spool
                .sort_with(
                    &cx,
                    &mut source,
                    &mut destination,
                    &mut order,
                    1,
                    count,
                    31,
                    u64::MAX,
                )
                .await
                .unwrap();
            assert_eq!(order.validated.load(AtomicOrdering::Relaxed), count);
            assert_eq!(read(&cx, &mut destination, &result).await, encoded(&input));
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn total_run_and_exact_work_limits_are_not_reset_by_carry_merges() {
    let ((), report) = run_async_under_lab(0x50a7_1004, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let (mut source, backing) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let spool = seed(&cx, &mut source, &rows(13)).await;
        let order = [GraphValueOrder::ascending(0)];
        let before = (source.stats(), destination.stats());
        backing.0.lock().unwrap().reads = 0;
        assert!(matches!(
            spool
                .sort_into(
                    &cx,
                    &mut source,
                    &mut destination,
                    &order,
                    1,
                    12,
                    127,
                    u64::MAX,
                )
                .await,
            Err(NativeSpoolError::SortRunLimit {
                required: 13,
                limit: 12
            })
        ));
        assert_eq!(backing.0.lock().unwrap().reads, 0);
        assert_eq!((source.stats(), destination.stats()), before);
        let (wide, measured) = spool
            .sort_into(
                &cx,
                &mut source,
                &mut destination,
                &order,
                1,
                13,
                127,
                u64::MAX,
            )
            .await
            .unwrap();
        let expected = read(&cx, &mut destination, &wide).await;
        let (exact, exact_work) = spool
            .sort_into(
                &cx,
                &mut source,
                &mut destination,
                &order,
                1,
                13,
                127,
                measured,
            )
            .await
            .unwrap();
        assert_eq!(exact_work, measured);
        assert_eq!(read(&cx, &mut destination, &exact).await, expected);
        assert!(matches!(spool.sort_into(
            &cx, &mut source, &mut destination, &order, 1, 13, 127, measured - 1,
        ).await, Err(NativeSpoolError::SortWorkLimit { limit, .. }) if limit == measured - 1));
        assert_eq!(pool.used(), 0);
        // A late refusal neither revokes a previously completed result nor
        // restores consumed append quota. Earlier run handles remain readable.
        assert_eq!(read(&cx, &mut destination, &wide).await, expected);
        assert!(source.stats().reserved_bytes > before.0.reserved_bytes);
        let (_, continued) = spool
            .sort_inner(
                &cx,
                &mut source,
                &mut destination,
                &mut CanonicalOrder(&order),
                1,
                13,
                127,
                measured + 29,
                29,
            )
            .await
            .unwrap();
        assert_eq!(continued, measured + 29);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropped_carry_merge_releases_generation_and_catalog_memory() {
    let ((), report) = run_async_under_lab(0x50a7_1005, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let (mut source, backing) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let spool = seed(&cx, &mut source, &rows(17)).await;
        let validated = Arc::new(AtomicUsize::new(0));
        let mut order = EqualOrder {
            validated: Arc::clone(&validated),
        };
        // Initial runs go to destination. The FIRST carry merges them back
        // into source, so this blocks an actual merge, not input generation.
        backing.0.lock().unwrap().block_writes = true;
        let mut future = Box::pin(spool.sort_with(
            &cx,
            &mut source,
            &mut destination,
            &mut order,
            1,
            17,
            31,
            u64::MAX,
        ));
        std::future::poll_fn(|task| match future.as_mut().poll(task) {
            Poll::Ready(_) => panic!("blocked carry append cannot complete"),
            Poll::Pending if backing.0.lock().unwrap().blocked_writes > 0 => Poll::Ready(()),
            Poll::Pending => Poll::Pending,
        })
        .await;
        assert_eq!(validated.load(AtomicOrdering::Relaxed), 2);
        assert!(pool.used() > 0);
        drop(future);
        assert_eq!(pool.used(), 0);
        assert!(source.is_poisoned());
        assert!(source.stats().reserved_runs > source.stats().published_runs);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bounded_query_prefix_matches_native_order_and_writes_one_run() {
    use fgdb_types::{CanonicalDecimal, CanonicalF64};

    let ((), report) = run_async_under_lab(0x50a7_1010, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let values = [
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(7)),
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(7.0))),
            GraphValue::Scalar(CanonicalScalar::Decimal(
                CanonicalDecimal::from_scaled_half_even(70, 1).unwrap(),
            )),
            GraphValue::Scalar(CanonicalScalar::Int(-4)),
            GraphValue::List(vec![GraphValue::Scalar(CanonicalScalar::Int(7))].into_boxed_slice()),
            GraphValue::List(
                vec![GraphValue::Scalar(CanonicalScalar::Float(
                    CanonicalF64::new(7.0),
                ))]
                .into_boxed_slice(),
            ),
        ];
        let mut input: Vec<_> = (0..70)
            .map(|at| {
                GraphValueRow::from_owned_values(vec![
                    values[at % values.len()].clone(),
                    GraphValue::Scalar(CanonicalScalar::Int((at * 11 % 5) as i64)),
                ])
            })
            .collect();
        input.rotate_left(13);
        input.reverse();
        for descending in [false, true] {
            for nulls_first in [false, true] {
                for key_count in [1, 2] {
                    let keys = [
                        GraphValueOrder {
                            column: 0,
                            descending,
                            nulls_first,
                        },
                        GraphValueOrder {
                            column: 1,
                            descending: !descending,
                            nulls_first: true,
                        },
                    ];
                    let order = &keys[..key_count];
                    // Numeric/list representation ties must reach every explicit
                    // key before typed row identity. Native values are the oracle.
                    let mut expected = input.clone();
                    expected.sort_by(|a, b| {
                        for key in order {
                            let left = &a.values()[key.column];
                            let right = &b.values()[key.column];
                            let comparison = match (left.is_null(), right.is_null()) {
                                (true, false) if key.nulls_first => Ordering::Less,
                                (true, false) => Ordering::Greater,
                                (false, true) if key.nulls_first => Ordering::Greater,
                                (false, true) => Ordering::Less,
                                _ if key.descending => right.compare_orderability(left),
                                _ => left.compare_orderability(right),
                            };
                            if comparison != Ordering::Equal {
                                return comparison;
                            }
                        }
                        a.cmp(b)
                    });
                    let expected = encoded(&expected);
                    let pool = MemoryPool::new(1_000_000, 0).unwrap();
                    let (mut source, _) = file(&cx, &pool).await;
                    let (mut destination, _) = file(&cx, &pool).await;
                    let spool = seed(&cx, &mut source, &input).await;
                    let (complete, _) = spool
                        .sort_query_continuing(
                            &cx,
                            &mut source,
                            &mut destination,
                            order,
                            true,
                            9,
                            8,
                            127,
                            u64::MAX,
                            0,
                        )
                        .await
                        .unwrap();
                    assert_eq!(read(&cx, &mut destination, &complete).await, expected);
                    for prefix in [0, 1, 2, 7, 19, 70, 77] {
                        let before = destination.stats().published_runs;
                        let (selected, _) = spool
                            .sort_query_window_continuing(
                                &cx,
                                &mut source,
                                &mut destination,
                                order,
                                true,
                                Some(prefix),
                                prefix + 1,
                                1,
                                127,
                                u64::MAX,
                                23,
                            )
                            .await
                            .unwrap();
                        assert_eq!(destination.stats().published_runs, before + 1);
                        assert_eq!(selected.row_count(), prefix.min(input.len()) as u64);
                        assert_eq!(selected.snapshot_seq(), spool.snapshot_seq());
                        assert_eq!(selected.row_stats().snapshot_records, 91);
                        assert_eq!(selected.evaluator_stats(), spool.evaluator_stats());
                        assert_eq!(
                            read(&cx, &mut destination, &selected).await,
                            expected[..prefix.min(expected.len())]
                        );
                        assert_eq!(pool.used(), 0);
                    }
                    assert_eq!(read(&cx, &mut source, &spool).await, encoded(&input));
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bounded_query_prefix_rejects_discarded_malformed_frames_and_false_counts() {
    let ((), report) = run_async_under_lab(0x50a7_1011, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let input = rows(2);
        let good = seed(&cx, &mut source, &input).await;
        let keys = [GraphValueOrder {
            column: 0,
            descending: false,
            nulls_first: true,
        }];
        for prefix in [0, 1] {
            for declared in [0, 1, 3] {
                let mut forged = good.clone();
                forged.rows.result_rows = declared;
                let before = destination.stats();
                let error = forged
                    .sort_query_window_continuing(
                        &cx,
                        &mut source,
                        &mut destination,
                        &keys,
                        true,
                        Some(prefix),
                        8,
                        1,
                        127,
                        u64::MAX,
                        0,
                    )
                    .await
                    .unwrap_err();
                if declared == 0 {
                    assert!(matches!(error, NativeSpoolError::IncompleteCursor));
                }
                assert_eq!(destination.stats(), before);
                assert!(!source.is_poisoned());
                assert!(!destination.is_poisoned());
                assert_eq!(pool.used(), 0);
            }
        }
        // The authentic tail is structurally invalid. Its key would lose to
        // the first NULL row, and LIMIT 0 retains neither row.
        let mut frames = encoded(&input);
        frames.last_mut().unwrap().push(0);
        let mut writer = source.paged_writer(&cx, 127).unwrap();
        for frame in &frames {
            writer
                .write(&cx, &(frame.len() as u64).to_be_bytes())
                .await
                .unwrap();
            writer.write(&cx, frame).await.unwrap();
        }
        let mut malformed = good.clone();
        malformed.run = writer.finish(&cx).await.unwrap();
        malformed.max_row_bytes = frames.iter().map(Vec::len).max().unwrap();
        for prefix in [0, 1] {
            let before = destination.stats();
            assert!(matches!(
                malformed
                    .sort_query_window_continuing(
                        &cx,
                        &mut source,
                        &mut destination,
                        &keys,
                        true,
                        Some(prefix),
                        8,
                        1,
                        127,
                        u64::MAX,
                        0,
                    )
                    .await,
                Err(NativeSpoolError::Spill(SpillError::InvalidRun))
            ));
            assert_eq!(destination.stats(), before);
            assert_eq!(pool.used(), 0);
        }
        assert_eq!(read(&cx, &mut source, &good).await, encoded(&input));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bounded_query_prefix_memory_fallback_keeps_original_work_allowance() {
    let ((), report) = run_async_under_lab(0x50a7_1012, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(32 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        // Seven wide winners cannot coexist. Each ordinary eight-row run has
        // only one, and a merge needs at most two row heads.
        let input: Vec<_> = (0..64)
            .map(|at| {
                let wide = at % 8 == 0;
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Scalar(CanonicalScalar::Int(if wide { -1 } else { 0 })),
                    GraphValue::Scalar(
                        CanonicalScalar::bytes(if wide { vec![7; 8000] } else { vec![] }).unwrap(),
                    ),
                ])
            })
            .collect();
        let spool = seed(&cx, &mut source, &input).await;
        let order = [GraphValueOrder::ascending(0)];
        let (complete, complete_work) = spool
            .sort_query_continuing(
                &cx,
                &mut source,
                &mut destination,
                &order,
                true,
                8,
                8,
                127,
                u64::MAX,
                0,
            )
            .await
            .unwrap();
        let expected = read(&cx, &mut destination, &complete).await;
        let (fallback, measured) = spool
            .sort_query_window_continuing(
                &cx,
                &mut source,
                &mut destination,
                &order,
                true,
                Some(7),
                8,
                8,
                127,
                u64::MAX,
                29,
            )
            .await
            .unwrap();
        // Fallback returns the complete sorted relation to the final window.
        assert_eq!(fallback.row_count(), input.len() as u64);
        assert!(measured > complete_work + 29);
        assert_eq!(read(&cx, &mut destination, &fallback).await, expected);
        assert_eq!(fallback.evaluator_stats(), spool.evaluator_stats());
        assert_eq!(pool.used(), 0);
        let (exact, exact_work) = spool
            .sort_query_window_continuing(
                &cx,
                &mut source,
                &mut destination,
                &order,
                true,
                Some(7),
                8,
                8,
                127,
                measured,
                29,
            )
            .await
            .unwrap();
        assert_eq!(exact_work, measured);
        assert_eq!(read(&cx, &mut destination, &exact).await, expected);
        assert!(matches!(
            spool
                .sort_query_window_continuing(
                    &cx, &mut source, &mut destination, &order, true,
                    Some(7), 8, 8, 127, measured - 1, 29,
                )
                .await,
            Err(NativeSpoolError::SortWorkLimit { limit, .. }) if limit == measured - 1
        ));
        assert_eq!(pool.used(), 0);
        assert_eq!(read(&cx, &mut source, &spool).await, encoded(&input));
        assert_eq!(read(&cx, &mut destination, &complete).await, expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
