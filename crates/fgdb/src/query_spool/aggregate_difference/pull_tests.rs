//! Real authenticated spill/sort/merge with independent visible-tuple counts.
use super::*;
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_strata::tiered::memory::SpillLimits;
use fgdb_types::PurposeContexts;
use std::collections::BTreeMap;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct State {
    bytes: Cursor<Vec<u8>>,
    reads: usize,
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
async fn collect(cursor: &mut NativeAggregateSpoolDifference<'_, File>) -> DifferenceResult<Bag> {
    let mut rows = Bag::new();
    let mut previous = None;
    while let Some((weight, row)) = cursor.next_change().await? {
        assert_ne!(weight, 0);
        assert!(row.keys().is_empty());
        assert_eq!(row.values().len(), cursor.columns().len());
        let tuple = row.values().to_vec();
        if let Some(previous) = previous {
            assert!(previous < tuple);
        }
        previous = Some(tuple.clone());
        assert!(rows.insert(tuple, weight).is_none());
    }
    assert_eq!(cursor.state(), ScanState::Exhausted);
    Ok(rows)
}

#[test]
fn hidden_groups_consolidate_visible_rows_and_repeated_slots_keep_native_order() {
    let ((), report) = run_async_under_lab(0xad1f_0001, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        let (mut source_file, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        for slots in [
            vec![GraphAggregateTextSlot::Aggregate(0)],
            vec![
                GraphAggregateTextSlot::Aggregate(0),
                GraphAggregateTextSlot::GroupKey(0),
                GraphAggregateTextSlot::Aggregate(0),
            ],
        ] {
            for mut code in 0..81 {
                let mut counts = [0; 4];
                for n in &mut counts {
                    *n = code % 3;
                    code /= 3;
                }
                let make = |offset: usize, start: i64| {
                    (0..2)
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
                let a = seed(&cx, &mut source_file, CommitSeq(1), &before, &slots).await;
                let b = seed(&cx, &mut source_file, CommitSeq(2), &after, &slots).await;
                for (left, right, before, after) in
                    [(&a, &b, &before, &after), (&b, &a, &after, &before)]
                {
                    let mut cursor = left
                        .difference_stream(
                            right,
                            &cx,
                            &mut source_file,
                            &mut scratch,
                            1,
                            100,
                            61,
                            100,
                            u64::MAX,
                            None,
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        collect(&mut cursor).await.unwrap(),
                        expected(before, after, &slots)
                    );
                }
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_shared_work_row_limits_and_duplicate_payloads_larger_than_the_pool() {
    let ((), report) = run_async_under_lab(0xad1f_0003, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source_file, _) = file(&cx, &pool).await;
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
        let a = seed(&cx, &mut source_file, CommitSeq(1), &before, &slots).await;
        let b = seed(&cx, &mut source_file, CommitSeq(2), &after, &slots).await;
        assert!(a.inner.encoded_len() > pool.limit());
        let mut baseline = a
            .difference_stream(
                &b,
                &cx,
                &mut source_file,
                &mut scratch,
                2,
                1000,
                127,
                2,
                u64::MAX,
                None,
            )
            .await
            .unwrap();
        let changes = collect(&mut baseline).await.unwrap();
        assert_eq!(changes, expected(&before, &after, &slots));
        let work = baseline.work_units();
        drop(baseline);
        let mut exact = a
            .difference_stream(
                &b,
                &cx,
                &mut source_file,
                &mut scratch,
                2,
                1000,
                127,
                2,
                work,
                None,
            )
            .await
            .unwrap();
        assert_eq!(collect(&mut exact).await.unwrap(), changes);
        assert_eq!(exact.work_units(), work);
        drop(exact);
        let mut short = a
            .difference_stream(
                &b,
                &cx,
                &mut source_file,
                &mut scratch,
                2,
                1000,
                127,
                2,
                work - 1,
                None,
            )
            .await
            .unwrap();
        assert!(collect(&mut short).await.is_err());
        assert_eq!(short.state(), ScanState::Failed);
        assert!(short.next_change().await.unwrap().is_none());
        drop(short);
        let mut short = a
            .difference_stream(
                &b,
                &cx,
                &mut source_file,
                &mut scratch,
                2,
                1000,
                127,
                1,
                work,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            collect(&mut short).await,
            Err(GqlQueryError::Rows(_))
        ));
        assert_eq!(short.changed_rows(), 1);
        drop(short);
        let mut cancelled = a
            .difference_stream(
                &a,
                &cx,
                &mut source_file,
                &mut scratch,
                2,
                1000,
                127,
                0,
                u64::MAX,
                None,
            )
            .await
            .unwrap();
        assert!(collect(&mut cancelled).await.unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pending_io_and_unwind_fuse_delivery_without_clearing_the_file_poison() {
    let ((), report) = run_async_under_lab(0xad1f_0006, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(128 * 1024, 0).unwrap();
        for panic in [false, true] {
            let (mut source_file, _) = file(&cx, &pool).await;
            let (mut scratch, backing) = file(&cx, &pool).await;
            let slots = [GraphAggregateTextSlot::Aggregate(0)];
            let a = seed(&cx, &mut source_file, CommitSeq(1), &[], &slots).await;
            let b = seed(
                &cx,
                &mut source_file,
                CommitSeq(2),
                &[row(0, GraphAggregateValue::Count(1))],
                &slots,
            )
            .await;
            let mut cursor = a
                .difference_stream(
                    &b,
                    &cx,
                    &mut source_file,
                    &mut scratch,
                    1,
                    100,
                    61,
                    1,
                    u64::MAX,
                    None,
                )
                .await
                .unwrap();
            {
                let mut state = backing.0.lock().unwrap();
                state.pending_read = !panic;
                state.panic_read = panic;
            }
            let mut future = cursor.next_change();
            let mut task = Context::from_waker(Waker::noop());
            if panic {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        future.as_mut().poll(&mut task)
                    }))
                    .is_err()
                );
            } else {
                assert!(future.as_mut().poll(&mut task).is_pending());
            }
            drop(future);
            assert_eq!(cursor.state(), ScanState::Failed);
            let reads = backing.0.lock().unwrap().reads;
            assert!(cursor.next_change().await.unwrap().is_none());
            cursor.close();
            assert_eq!(cursor.state(), ScanState::Failed);
            assert_eq!(backing.0.lock().unwrap().reads, reads);
            drop(cursor);
            assert!(scratch.is_poisoned());
            drop(scratch);
            assert_eq!(pool.used(), 0);
            let (mut fresh, _) = file(&cx, &pool).await;
            let mut retry = a
                .difference_stream(
                    &b,
                    &cx,
                    &mut source_file,
                    &mut fresh,
                    1,
                    100,
                    61,
                    1,
                    u64::MAX,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(collect(&mut retry).await.unwrap().len(), 1);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn real_historical_aggregate_spools_match_query_diff_after_writer_and_templates_drop() {
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GraphSymbol, GraphSymbolKind};
    use fgdb_types::{DatabaseSecurityNamespaceId, VId};

    let ((), report) = run_async_under_lab(0xad1f_0008, |root| async move {
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
            let expected = prepared
                .diff_in_view(&after, &cx, &GqlParameters::new(), first, last, policy)
                .unwrap();
            let expected: Bag = expected
                .changes()
                .iter()
                .map(|(row, weight)| (row.to_vec(), weight.to_i128().unwrap()))
                .collect();
            let pool = MemoryPool::new(512 * 1024, 0).unwrap();
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut auxiliary, _) = file(&cx, &pool).await;
            let (mut source_file, _) = file(&cx, &pool).await;
            let (a, _) = prepared
                .spool_aggregate_in_view(
                    &before,
                    &cx,
                    &GqlParameters::new(),
                    policy,
                    &mut scratch,
                    &mut auxiliary,
                    &mut source_file,
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
                    &mut source_file,
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
            let mut cursor = a
                .difference_stream(
                    &b,
                    &cx,
                    &mut source_file,
                    &mut scratch,
                    1,
                    100,
                    61,
                    expected.len() as u64,
                    u64::MAX,
                    None,
                )
                .await
                .unwrap();
            drop(a);
            drop(b);
            drop(source_file);
            assert_eq!(collect(&mut cursor).await.unwrap(), expected, "{text}");
            assert_eq!((cursor.before(), cursor.after()), (first, last));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
