//! Exercise the real paged store/sorter with an independent typed bag oracle.
use super::*;
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_gql::algebra::GraphValue;
use fgdb_strata::tiered::memory::SpillLimits;
use fgdb_types::{CanonicalF64, CanonicalScalar, EId, PurposeContexts, VId};
use std::collections::BTreeMap;
use std::future::{pending, ready};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct State {
    bytes: Cursor<Vec<u8>>,
    reads: usize,
    writes: usize,
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
            max_runs: 500_000,
            max_run_bytes: 16_000_000,
        },
    )
    .await
    .unwrap();
    (file, backing)
}
fn row(n: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(n))])
}
async fn seed(
    cx: &QueryCx,
    file: &mut SpillFile<File>,
    rows: &[GraphValueRow],
) -> NativeResultSpool {
    let mut writer = file.paged_writer(cx, 127).unwrap();
    let mut largest = 0;
    for row in rows {
        let bytes = row.canonical_bytes().unwrap();
        largest = largest.max(bytes.len());
        writer
            .write(cx, &(bytes.len() as u64).to_be_bytes())
            .await
            .unwrap();
        writer.write(cx, &bytes).await.unwrap();
    }
    NativeResultSpool {
        columns: vec!["value".into()].into(),
        encoded_columns: 1,
        snapshot: CommitSeq(17),
        kind: ScanKind::Values,
        rows: GqlExecutionStats {
            snapshot_records: 0,
            result_rows: rows.len() as u64,
        },
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
#[allow(clippy::too_many_arguments)]
async fn collect(
    before: &NativeResultSpool,
    after: &NativeResultSpool,
    cx: &QueryCx,
    source: &mut SpillFile<File>,
    scratch: &mut SpillFile<File>,
    run_rows: usize,
    rows: u64,
    work: u64,
) -> (VisitResult<(u64, u64)>, Changes) {
    let mut out = Vec::new();
    let result = before
        .visit_difference(
            after,
            cx,
            source,
            scratch,
            run_rows,
            100_000,
            61,
            rows,
            work,
            |weight, frame| {
                out.push((
                    GraphValueRow::decode_canonical(frame.as_ref()).unwrap(),
                    weight,
                ));
                ready(Ok(()))
            },
        )
        .await;
    (result, out)
}
async fn read(
    spool: &NativeResultSpool,
    cx: &QueryCx,
    file: &mut SpillFile<File>,
) -> Vec<GraphValueRow> {
    let mut reader = spool.reader(file);
    let mut rows = Vec::new();
    while let Some(bytes) = reader.next_row(cx).await.unwrap() {
        rows.push(GraphValueRow::decode_canonical(bytes.as_ref()).unwrap());
    }
    rows
}

#[test]
fn all_small_bags_match_counts_in_both_directions_across_run_boundaries() {
    let ((), report) = run_async_under_lab(0xd1ff_5001, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        for mut code in 0..81 {
            let mut counts = [0_usize; 4];
            for count in &mut counts {
                *count = code % 3;
                code /= 3;
            }
            let make = |offset| {
                (0_usize..2)
                    .rev()
                    .flat_map(|key| std::iter::repeat_n(row(key as i64), counts[offset + key]))
                    .collect::<Vec<_>>()
            };
            let (before, after) = (make(0), make(2));
            for run_rows in [1, 3] {
                let (mut source, _) = file(&cx, &pool).await;
                let (mut scratch, _) = file(&cx, &pool).await;
                let a = seed(&cx, &mut source, &before).await;
                let b = seed(&cx, &mut source, &after).await;
                for (a, b, left, right) in [(&a, &b, &before, &after), (&b, &a, &after, &before)] {
                    let (result, changes) =
                        collect(a, b, &cx, &mut source, &mut scratch, run_rows, 2, u64::MAX).await;
                    assert_eq!(result.unwrap().0, changes.len() as u64);
                    assert_eq!(changes, expected(left, right));
                }
                assert_eq!(read(&a, &cx, &mut source).await, before);
                assert_eq!(read(&b, &cx, &mut source).await, after);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn full_native_domains_use_typed_equality_not_numeric_equivalence_or_byte_length() {
    let ((), report) = run_async_under_lab(0xd1ff_5002, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let values = vec![
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(7)),
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(7.0))),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("aa\0é").unwrap()),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("z").unwrap()),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Edge(EId(u128::MAX)),
            GraphValue::List(
                vec![
                    GraphValue::Vertex(VId(0)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ]
                .into_boxed_slice(),
            ),
            GraphValue::map(vec![("k".into(), GraphValue::Edge(EId(0)))]).unwrap(),
        ];
        let before: Vec<_> = values
            .iter()
            .cloned()
            .map(|v| GraphValueRow::from_owned_values(vec![v]))
            .collect();
        let after: Vec<_> = before
            .iter()
            .rev()
            .skip(1)
            .cloned()
            .chain([row(7), row(7)])
            .collect();
        let a = seed(&cx, &mut source, &before).await;
        let b = seed(&cx, &mut source, &after).await;
        let (result, changes) =
            collect(&a, &b, &cx, &mut source, &mut scratch, 2, 100, u64::MAX).await;
        result.unwrap();
        assert_eq!(changes, expected(&before, &after));
        assert!(
            changes
                .iter()
                .any(|(r, weight)| *r == row(7) && *weight == 2)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn duplicate_heavy_results_exceed_the_pool_but_deliver_only_compressed_changes() {
    let ((), report) = run_async_under_lab(0xd1ff_5003, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let text = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(&"payload\0é".repeat(80)).unwrap(),
        )]);
        let before = vec![text.clone(); 256];
        let mut after = vec![text; 255];
        after.extend([row(-1), row(-1), row(-1)]);
        let a = seed(&cx, &mut source, &before).await;
        let b = seed(&cx, &mut source, &after).await;
        assert!(a.encoded_len() > pool.limit());
        assert!(b.encoded_len() > pool.limit());
        let (result, changes) =
            collect(&a, &b, &cx, &mut source, &mut scratch, 3, 2, u64::MAX).await;
        assert_eq!(result.unwrap().0, 2);
        assert_eq!(changes, expected(&before, &after));
        let (result, changes) =
            collect(&a, &a, &cx, &mut source, &mut scratch, 3, 0, u64::MAX).await;
        assert_eq!(result.unwrap().0, 0);
        assert!(changes.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn both_sorts_merge_and_delivery_share_one_exact_work_allowance() {
    let ((), report) = run_async_under_lab(0xd1ff_5004, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let a = seed(&cx, &mut source, &[row(3), row(1), row(1)]).await;
        let b = seed(&cx, &mut source, &[row(2), row(1)]).await;
        let (result, expected) =
            collect(&a, &b, &cx, &mut source, &mut scratch, 1, 3, u64::MAX).await;
        let (rows, work) = result.unwrap();
        assert_eq!(rows, 3);
        let (exact, changes) = collect(&a, &b, &cx, &mut source, &mut scratch, 1, rows, work).await;
        assert_eq!(exact.unwrap(), (rows, work));
        assert_eq!(changes, expected);
        let (short, _) = collect(&a, &b, &cx, &mut source, &mut scratch, 1, rows, work - 1).await;
        assert!(matches!(
            short,
            Err(GqlQueryError::Source(
                NativeSpoolError::SortWorkLimit { .. }
            ))
        ));
        let (short, partial) =
            collect(&a, &b, &cx, &mut source, &mut scratch, 1, rows - 1, work).await;
        assert!(
            matches!(short, Err(GqlQueryError::Rows(error)) if error.limit == 2 && error.observed == 3)
        );
        assert_eq!(partial.len(), 2); // a prefix is NOT complete success
        let keys = [GraphValueOrder::ascending(0).with_nulls_first(true)];
        let (_, first_only) = a
            .sort_continuing(
                &cx,
                &mut source,
                &mut scratch,
                &keys,
                1,
                100,
                61,
                u64::MAX,
                0,
            )
            .await
            .unwrap();
        // Independently count this fixture's preflight: initial checkpoint,
        // one name length check plus five bytes, two run caps, one sort key.
        // The first sort fits EXACTLY; a fresh allowance for the second sort
        // would incorrectly let this three-row transition reach the visitor.
        let first_complete = first_only + 1 + 1 + 5 + 2 + 1;
        let (refused, output) =
            collect(&a, &b, &cx, &mut source, &mut scratch, 1, 3, first_complete).await;
        assert!(refused.is_err());
        assert!(output.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn schema_and_both_run_caps_refuse_before_io_and_bad_second_input_before_callbacks() {
    let ((), report) = run_async_under_lab(0xd1ff_5005, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, backing) = file(&cx, &pool).await;
        let (mut scratch, dest) = file(&cx, &pool).await;
        let a = seed(&cx, &mut source, &[row(1)]).await;
        let b = seed(&cx, &mut source, &[row(3), row(2), row(1)]).await;
        let before = {
            let s = backing.0.lock().unwrap();
            (s.reads, s.writes)
        };
        let destination_writes = dest.0.lock().unwrap().writes;
        let calls = AtomicUsize::new(0);
        let visitor = |_, _: TrackedBytes| {
            calls.fetch_add(1, AtomicOrdering::SeqCst);
            ready(Ok(()))
        };
        assert!(matches!(
            a.visit_difference(
                &b,
                &cx,
                &mut source,
                &mut scratch,
                1,
                2,
                61,
                100,
                u64::MAX,
                visitor
            )
            .await,
            Err(GqlQueryError::Source(NativeSpoolError::SortRunLimit {
                required: 3,
                limit: 2
            }))
        ));
        let mut wrong = b.clone();
        wrong.columns = vec!["another-name".into()].into();
        assert!(
            a.visit_difference(
                &wrong,
                &cx,
                &mut source,
                &mut scratch,
                1,
                10,
                61,
                100,
                u64::MAX,
                visitor
            )
            .await
            .is_err()
        );
        assert_eq!(
            {
                let s = backing.0.lock().unwrap();
                (s.reads, s.writes)
            },
            before
        );
        assert_eq!(dest.0.lock().unwrap().writes, destination_writes);
        let (mut foreign, _) = file(&cx, &pool).await;
        let foreign = seed(&cx, &mut foreign, &[row(9)]).await;
        assert!(
            a.visit_difference(
                &foreign,
                &cx,
                &mut source,
                &mut scratch,
                1,
                10,
                61,
                100,
                u64::MAX,
                visitor
            )
            .await
            .is_err()
        );
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(read(&a, &cx, &mut source).await, [row(1)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn visitor_failure_and_drop_apply_backpressure_without_consuming_original_runs() {
    let ((), report) = run_async_under_lab(0xd1ff_5006, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, backing) = file(&cx, &pool).await;
        let (mut scratch, dest) = file(&cx, &pool).await;
        let a = seed(&cx, &mut source, &[]).await;
        let rows: Vec<_> = (0..40).map(row).collect();
        let b = seed(&cx, &mut source, &rows).await;
        let calls = AtomicUsize::new(0);
        let error = a
            .visit_difference(
                &b,
                &cx,
                &mut source,
                &mut scratch,
                3,
                100,
                61,
                100,
                u64::MAX,
                |_, _| {
                    calls.fetch_add(1, AtomicOrdering::SeqCst);
                    ready(Err(SpillError::InvalidRun.into()))
                },
            )
            .await;
        assert!(matches!(
            error,
            Err(GqlQueryError::Source(NativeSpoolError::Spill(
                SpillError::InvalidRun
            )))
        ));
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        calls.store(0, AtomicOrdering::SeqCst);
        let mut future = Box::pin(a.visit_difference(
            &b,
            &cx,
            &mut source,
            &mut scratch,
            3,
            100,
            61,
            100,
            u64::MAX,
            |_, row| {
                calls.fetch_add(1, AtomicOrdering::SeqCst);
                async move {
                    pending::<()>().await;
                    drop(row);
                    Ok(())
                }
            },
        ));
        {
            // A Context is not Send: keep it out of the lab future's later awaits.
            let mut task = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut task).is_pending());
            assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
            let reads = (
                backing.0.lock().unwrap().reads,
                dest.0.lock().unwrap().reads,
            );
            assert!(future.as_mut().poll(&mut task).is_pending());
            assert_eq!(
                (
                    backing.0.lock().unwrap().reads,
                    dest.0.lock().unwrap().reads
                ),
                reads
            );
        }
        drop(future);
        assert_eq!(read(&b, &cx, &mut source).await, rows);
        let (result, changes) =
            collect(&a, &b, &cx, &mut source, &mut scratch, 3, 100, u64::MAX).await;
        assert_eq!(result.unwrap().0, 40);
        assert_eq!(changes, expected(&[], &rows));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_historical_results_survive_writer_destruction_and_keep_original_rows() {
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    use fgdb_types::DatabaseSecurityNamespaceId;
    let ((), report) = run_async_under_lab(0xd1ff_5007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(
            &commit,
            crate::DatabaseKeys::new(
                [0x61; 32],
                DatabaseSecurityNamespaceId([0x62; 32]),
                [0x63; 32],
            ),
        )
        .await
        .unwrap();
        let mut batch = crate::WriteBatch::new(RelationId(1));
        for (id, value) in [(1, 3), (2, 3), (3, 7)] {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
            );
        }
        let first = db.write(&commit, batch).await.unwrap();
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n) RETURN n, n.p AS p",
            &params,
            |kind, name: &str| match (kind, name) {
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                _ => None,
            },
        )
        .unwrap();
        let policy = GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000);
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let a = prepared
            .spool(&db, &cx, &params, policy, &mut source, 97, 4096)
            .await
            .unwrap();
        let mut batch = crate::WriteBatch::new(RelationId(1));
        batch.delete_vertex(VId(2));
        batch.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
        batch.create_vertex(
            VId(4),
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(3))],
        );
        let last = db.write(&commit, batch).await.unwrap();
        let b = prepared
            .spool(&db, &cx, &params, policy, &mut source, 37, 4096)
            .await
            .unwrap();
        assert_eq!(a.snapshot_seq(), first);
        assert_eq!(b.snapshot_seq(), last);
        drop(db);
        drop(prepared);
        drop(params);
        let native = |id, value| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Int(value)),
            ])
        };
        let before = vec![native(1, 3), native(2, 3), native(3, 7)];
        let after = vec![native(1, 9), native(3, 7), native(4, 3)];
        let (result, changes) =
            collect(&a, &b, &cx, &mut source, &mut scratch, 1, 4, u64::MAX).await;
        assert_eq!(result.unwrap().0, 4);
        assert_eq!(changes, expected(&before, &after));
        assert_eq!(read(&a, &cx, &mut source).await, before);
        assert_eq!(read(&b, &cx, &mut source).await, after);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_observed_cancellation_boundary_aborts_instead_of_reporting_complete_delivery() {
    use fgdb_types::context::SimulationCheckpointProbe;
    let ((), report) = run_async_under_lab(0xd1ff_5008, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let a = seed(&cx, &mut source, &[row(1)]).await;
        let b = seed(&cx, &mut source, &[row(2)]).await;
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (result, expected) =
            collect(&a, &b, &observed, &mut source, &mut scratch, 1, 2, u64::MAX).await;
        result.unwrap();
        assert_eq!(expected.len(), 2);
        assert!(probe.calls() > 5);
        for stop in 1..=probe.calls() {
            // An interrupted writer may poison its file: never label a retry
            // through that owner as a successful recovery. Use fresh scratch.
            let (mut source, _) = file(&cx, &pool).await;
            let (mut scratch, _) = file(&cx, &pool).await;
            let a = seed(&cx, &mut source, &[row(1)]).await;
            let b = seed(&cx, &mut source, &[row(2)]).await;
            let control = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&control));
            let (result, partial) = collect(
                &a,
                &b,
                &interrupted,
                &mut source,
                &mut scratch,
                1,
                2,
                u64::MAX,
            )
            .await;
            assert!(result.is_err(), "checkpoint {stop}");
            assert_eq!(control.calls(), stop);
            assert!(expected.starts_with(&partial));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
