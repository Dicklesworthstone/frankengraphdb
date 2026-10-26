//! Temporal scalar profiles must survive the real native query/spill boundary.
//! Expected values use typed ordering, never the borrowed frame comparator.

use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EmbeddedReadView, MemVfs, NativeSpoolError, PreparedNativeRead,
    WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, GraphValueRow};
use fgdb_gql::{
    GlaLimitDimension, GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphIntegerErrorKind, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
};
use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalDate, CanonicalDuration, CanonicalLocalDateTime, CanonicalLocalTime, CanonicalScalar,
    CanonicalTime, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};

// Query-owned in-memory I/O for the production authenticated paged spill file.
// It does not implement framing, sorting, budgets, or a graph evaluator.
#[derive(Default)]
struct File(Cursor<Vec<u8>>);

impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let file = &mut self.get_mut().0;
        let at = file.position() as usize;
        let count = out.remaining().min(file.get_ref().len().saturating_sub(at));
        if count != 0 {
            out.put_slice(&file.get_ref()[at..at + count]);
            file.set_position((at + count) as u64);
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
        Poll::Ready(std::io::Write::write(&mut self.get_mut().0, bytes))
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
        Poll::Ready(self.get_mut().0.seek(at))
    }
}

fn families() -> Vec<Vec<CanonicalScalar>> {
    let day = |n| CanonicalDate::from_epoch_day(n).unwrap();
    let time = |n| CanonicalLocalTime::from_nanos_of_day(n).unwrap();
    vec![
        [-1, 0, 1]
            .into_iter()
            .map(|n| CanonicalScalar::Date(day(n)))
            .collect(),
        [0, 1, 86_399_999_999_999]
            .into_iter()
            .map(|n| CanonicalScalar::LocalTime(time(n)))
            .collect(),
        // Local order differs from UTC order; equal UTC times keep their
        // distinct offsets/local times rather than collapsing during DISTINCT.
        [(0, 3600), (0, 0), (3_600_000_000_000, 3600), (1, 0)]
            .into_iter()
            .map(|(n, offset)| CanonicalScalar::Time(CanonicalTime::new(time(n), offset).unwrap()))
            .collect(),
        [(-1, 86_399_999_999_999), (0, 0), (0, 1)]
            .into_iter()
            .map(|(d, t)| {
                CanonicalScalar::LocalDateTime(CanonicalLocalDateTime::new(day(d), time(t)))
            })
            .collect(),
        [(-1, 0, 0), (0, -1, 0), (0, 0, -1), (0, 0, 1)]
            .into_iter()
            .map(|(months, days, nanos)| {
                CanonicalScalar::Duration(CanonicalDuration::new(months, days, nanos).unwrap())
            })
            .collect(),
    ]
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 10_000_000, 1_000_000)
}
async fn scratch(cx: &fgdb_types::QueryCx, pool: &MemoryPool) -> SpillFile<File> {
    SpillFile::new(
        cx,
        File::default(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 4_000_000,
            max_runs: 512,
            max_run_bytes: 1_000_000,
        },
    )
    .await
    .unwrap()
}
async fn database(
    cx: &fgdb_types::CommitCx,
    samples: &[CanonicalScalar],
) -> (Database<MemVfs>, Vec<(VId, CanonicalScalar)>) {
    let mut db = Database::open_memory(
        cx,
        DatabaseKeys::new(
            [0x61; 32],
            DatabaseSecurityNamespaceId([0x62; 32]),
            [0x63; 32],
        ),
    )
    .await
    .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    let mut rows = Vec::new();
    // Deliberately reverse typed order, repeat every value, and keep NULL.
    for (at, scalar) in samples
        .iter()
        .rev()
        .chain(samples)
        .chain([&CanonicalScalar::Null])
        .enumerate()
    {
        let id = VId(at as u128 + 1);
        batch.create_vertex(
            id,
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), scalar.clone())],
        );
        rows.push((id, scalar.clone()));
    }
    db.write(cx, batch).await.unwrap();
    (db, rows)
}

#[derive(Debug, PartialEq, Eq)]
struct ElementSpoolResult {
    rows: Vec<GraphValueRow>,
    stats: fgdb_gql::GqlExecutionStats,
    evaluator: fgdb_gql::GlaExecutionStats,
    snapshot: fgdb_types::CommitSeq,
}

async fn element_spool(
    view: &EmbeddedReadView,
    cx: &fgdb_types::QueryCx,
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    allowance: GqlQueryPolicy,
    run_rows: usize,
) -> Result<ElementSpoolResult, NativeSpoolError> {
    let pool = MemoryPool::new(256_000, 0).unwrap();
    let mut source = scratch(cx, &pool).await;
    let mut destination = scratch(cx, &pool).await;
    let result = prepared
        .spool_ordered_in_view(
            view,
            cx,
            params,
            allowance,
            &mut source,
            &mut destination,
            run_rows,
            64,
            127,
            4096,
            100,
            10_000_000,
        )
        .await;
    let (spool, _) = match result {
        Ok(spool) => spool,
        Err(error) => {
            assert_eq!(pool.used(), 0);
            return Err(error);
        }
    };
    let mut rows = Vec::new();
    let mut reader = spool.reader(&mut destination);
    while let Some(bytes) = reader.next_row(cx).await.unwrap() {
        rows.push(GraphValueRow::decode_stored(bytes.as_ref()).unwrap());
    }
    drop(reader);
    assert_eq!(pool.used(), 0);
    Ok(ElementSpoolResult {
        rows,
        stats: spool.row_stats(),
        evaluator: spool.evaluator_stats(),
        snapshot: spool.snapshot_seq(),
    })
}

#[test]
fn temporal_rows_merge_with_original_values_stats_and_work_refusal() {
    let ((), report) = run_async_under_lab(0x7e4d_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let samples: Vec<_> = families().into_iter().flatten().collect();
        let (db, rows) = database(&contexts.commit(), &samples).await;
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n:L) RETURN n AS id,n.p AS p",
            &GqlParameters::new(),
            symbols,
        )
        .unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for run_rows in [1, 4] {
            let mut expected = rows.clone();
            expected.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let expected: Vec<_> = expected
                .into_iter()
                .map(|(id, scalar)| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(id),
                        GraphValue::Scalar(scalar),
                    ])
                })
                .collect();
            let mut required = None;
            for attempt in 0..3 {
                let mut source = scratch(&cx, &pool).await;
                let mut destination = scratch(&cx, &pool).await;
                let spool = prepared
                    .spool(
                        &db,
                        &cx,
                        &GqlParameters::new(),
                        policy(),
                        &mut source,
                        31,
                        1024,
                    )
                    .await
                    .unwrap();
                let limit = match (attempt, required) {
                    (2, Some(work)) => work - 1,
                    (_, Some(work)) => work,
                    _ => u64::MAX,
                };
                let order = [GraphValueOrder::descending(1).with_nulls_first(false)];
                let result = spool
                    .sort_into(
                        &cx,
                        &mut source,
                        &mut destination,
                        &order,
                        run_rows,
                        64,
                        37,
                        limit,
                    )
                    .await;
                if attempt == 2 {
                    assert!(matches!(
                        result,
                        Err(NativeSpoolError::SortWorkLimit { .. })
                    ));
                } else {
                    let (sorted, work) = result.unwrap();
                    required = Some(work);
                    assert_eq!(sorted.snapshot_seq(), spool.snapshot_seq());
                    assert_eq!(sorted.row_stats(), spool.row_stats());
                    assert_eq!(sorted.evaluator_stats(), spool.evaluator_stats());
                    let mut cursor = sorted.reader(&mut destination);
                    let mut actual = Vec::new();
                    while let Some(bytes) = cursor.next_row(&cx).await.unwrap() {
                        actual.push(GraphValueRow::decode_canonical(bytes.as_ref()).unwrap());
                    }
                    assert_eq!(actual, expected);
                    drop(cursor);
                    assert!(destination.stats().published_runs > 1);
                }
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_distinct_and_hidden_keys_are_applied_before_the_final_page() {
    let ((), report) = run_async_under_lab(0x7e4d_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for samples in families() {
            let (db, rows) = database(&contexts.commit(), &samples).await;
            for hidden in [false, true] {
                let text = if hidden {
                    "MATCH (n:L) RETURN n AS id ORDER BY n.p DESC NULLS LAST SKIP 1 LIMIT 3"
                } else {
                    "MATCH (n:L) RETURN DISTINCT n.p AS p ORDER BY p DESC NULLS LAST SKIP 1 LIMIT 3"
                };
                let mut expected = rows.clone();
                expected.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                if !hidden {
                    expected.dedup_by(|a, b| a.1 == b.1);
                }
                let expected: Vec<_> = expected
                    .into_iter()
                    .skip(1)
                    .take(3)
                    .map(|(id, scalar)| {
                        GraphValueRow::from_owned_values(vec![if hidden {
                            GraphValue::Vertex(id)
                        } else {
                            GraphValue::Scalar(scalar)
                        }])
                    })
                    .collect();
                let prepared =
                    PreparedNativeRead::prepare(text, &GqlParameters::new(), symbols).unwrap();
                let mut source = scratch(&cx, &pool).await;
                let mut destination = scratch(&cx, &pool).await;
                let (spool, _) = prepared
                    .spool_ordered(
                        &db,
                        &cx,
                        &GqlParameters::new(),
                        policy(),
                        &mut source,
                        &mut destination,
                        1,
                        64,
                        31,
                        1024,
                        100,
                        1_000_000,
                    )
                    .await
                    .unwrap();
                assert_eq!(spool.columns(), &[if hidden { "id" } else { "p" }]);
                assert_eq!(spool.row_stats().result_rows, expected.len() as u64);
                let mut cursor = spool.reader(&mut destination);
                let mut actual = Vec::new();
                while let Some(bytes) = cursor.next_row(&cx).await.unwrap() {
                    actual.push(GraphValueRow::decode_canonical(bytes.as_ref()).unwrap());
                }
                assert_eq!(actual, expected);
                drop(cursor);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_set_order_finishes_native_stages_before_the_final_page() {
    let ((), report) = run_async_under_lab(0x7e4d_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let samples = [
            CanonicalScalar::Int(9),
            CanonicalScalar::Int(1),
            CanonicalScalar::Int(4),
        ];
        let (db, _) = database(&contexts.commit(), &samples).await;
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        let pool = MemoryPool::new(64_000, 0).unwrap();
        for text in [
            "MATCH (n:L) RETURN n.p + 1 AS value ORDER BY value DESC SKIP 1 LIMIT 3",
            "MATCH (n:L) RETURN n AS id,n.p + 1 AS value ORDER BY value DESC,id LIMIT 3",
            "MATCH (n:L) RETURN DISTINCT n.p % 3 AS value ORDER BY value DESC NULLS FIRST",
            "MATCH (n:L) RETURN n.p + 1 AS value",
            "MATCH (n:L) WITH n.p AS value ORDER BY value DESC LIMIT 3 RETURN value + 2 AS value ORDER BY value",
            "MATCH (n:L) WITH n.p AS value WHERE value > 1 UNWIND [value, value + 1] AS item RETURN item ORDER BY item DESC LIMIT 5",
            "MATCH (n:L) WITH n AS item RETURN CASE WHEN TRUE THEN 7 ELSE [item][0].p END AS value ORDER BY value DESC",
            "MATCH (n:L) RETURN 7 AS constant ORDER BY constant LIMIT 2",
        ] {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let PreparedNativeRead::Set(template) = &prepared else {
                panic!("expected a relational definition: {text}");
            };
            let bound = template.bind_parameters(&params).unwrap();
            let expected = view
                .execute_graph_set_governed_at(&cx, &bound, view.frontier(), policy())
                .unwrap()
                .value;
            let mut source = scratch(&cx, &pool).await;
            let mut destination = scratch(&cx, &pool).await;
            let (spool, work) = prepared
                .spool_ordered_in_view(
                    &view,
                    &cx,
                    &params,
                    policy(),
                    &mut source,
                    &mut destination,
                    2,
                    64,
                    127,
                    4096,
                    100,
                    10_000_000,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert!(work > 0);
            assert_eq!(spool.snapshot_seq(), view.frontier());
            assert_eq!(spool.row_stats().snapshot_records, 7);
            assert_eq!(spool.row_count(), expected.len() as u64);
            assert_eq!(spool.columns(), bound.columns());
            let mut cursor = spool.reader(&mut destination);
            let mut actual = Vec::new();
            while let Some(bytes) = cursor.next_row(&cx).await.unwrap() {
                actual.push(GraphValueRow::decode_stored(bytes.as_ref()).unwrap());
            }
            assert_eq!(actual, expected, "{text}");
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_temporal_sets_bind_and_pin_before_polling() {
    let ((), report) = run_async_under_lab(0x7e4d_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, _) = database(
            &contexts.commit(),
            &[CanonicalScalar::Int(2), CanonicalScalar::Int(6)],
        )
        .await;
        let old = db.read_session().unwrap();
        let first = old.frontier();
        let text = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at WITH n AS item RETURN [item][0].p + $bump AS value ORDER BY value DESC LIMIT $cap";
        let params = GqlParameters::new()
            .with_uint64("at", first.0)
            .unwrap()
            .with_int64("bump", 5)
            .unwrap()
            .with_uint64("cap", 2)
            .unwrap();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        assert!(matches!(prepared, PreparedNativeRead::TemporalSet(_)));
        let pool = MemoryPool::new(64_000, 0).unwrap();
        let mut source = scratch(&cx, &pool).await;
        let mut destination = scratch(&cx, &pool).await;
        let future = prepared.spool_ordered_in_view(
            &old,
            &cx,
            &params,
            policy(),
            &mut source,
            &mut destination,
            2,
            64,
            127,
            4096,
            100,
            10_000_000,
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(100)));
        change.delete_vertex(VId(2));
        change.create_vertex(
            VId(8),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(200))],
        );
        let second = db.write(&contexts.commit(), change).await.unwrap();
        drop(old);
        drop(params);
        drop(prepared);
        let (spool, _) = future.await.unwrap();
        assert_eq!(spool.snapshot_seq(), first);
        let mut cursor = spool.reader(&mut destination);
        for _ in 0..2 {
            let row = cursor.next_row(&cx).await.unwrap().unwrap();
            assert_eq!(
                GraphValueRow::decode_stored(row.as_ref()).unwrap(),
                GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(
                    11
                ))])
            );
        }
        assert!(cursor.next_row(&cx).await.unwrap().is_none());
        drop(cursor);
        assert_eq!(pool.used(), 0);

        // The same template can bind both historical and current cuts from a
        // newer generation, with new expression and page parameters each time.
        let initial = GqlParameters::new()
            .with_uint64("at", first.0)
            .unwrap()
            .with_int64("bump", 20)
            .unwrap()
            .with_uint64("cap", 3)
            .unwrap();
        let prepared = PreparedNativeRead::prepare(text, &initial, symbols).unwrap();
        for (at, expected) in [(first, [26, 26, 22]), (second, [220, 120, 26])] {
            let params = GqlParameters::new()
                .with_uint64("at", at.0)
                .unwrap()
                .with_int64("bump", 20)
                .unwrap()
                .with_uint64("cap", 3)
                .unwrap();
            let mut source = scratch(&cx, &pool).await;
            let mut destination = scratch(&cx, &pool).await;
            let (spool, _) = prepared
                .spool_ordered(
                    &db,
                    &cx,
                    &params,
                    policy(),
                    &mut source,
                    &mut destination,
                    2,
                    64,
                    127,
                    4096,
                    100,
                    10_000_000,
                )
                .await
                .unwrap();
            assert_eq!(spool.snapshot_seq(), at);
            let mut cursor = spool.reader(&mut destination);
            for value in expected {
                let row = cursor.next_row(&cx).await.unwrap().unwrap();
                assert_eq!(
                    GraphValueRow::decode_stored(row.as_ref()).unwrap(),
                    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                        CanonicalScalar::Int(value)
                    )])
                );
            }
            assert!(cursor.next_row(&cx).await.unwrap().is_none());
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_set_errors_follow_complete_source_order_even_for_empty_pages() {
    let ((), report) = run_async_under_lab(0x7e4d_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, _) = database(
            &contexts.commit(),
            &[CanonicalScalar::Int(1), CanonicalScalar::Int(2)],
        )
        .await;
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        let pool = MemoryPool::new(64_000, 0).unwrap();
        for page in [0, 1] {
            // Identity order starts with p=2 and would fail at row zero. The
            // native leaf orders its property tuples first, so the two p=1
            // occurrences precede that failure even when the final page is 0.
            let text =
                format!("MATCH (n:L) RETURN 12 / (n.p - 2) AS value ORDER BY value LIMIT {page}");
            let prepared = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
            let PreparedNativeRead::Set(template) = &prepared else {
                panic!("expected relational arithmetic");
            };
            let bound = template.bind_parameters(&params).unwrap();
            let GqlQueryError::Source(GraphSetExecutionError::Projection {
                row: expected_row,
                column: expected_column,
                error: expected_error,
            }) = view
                .execute_graph_set_governed_at(&cx, &bound, view.frontier(), policy())
                .unwrap_err()
            else {
                panic!("expected native division-by-zero error");
            };
            assert!(expected_row >= 2);
            assert_eq!(expected_error.kind, GraphIntegerErrorKind::DivisionByZero);
            let mut source = scratch(&cx, &pool).await;
            let mut destination = scratch(&cx, &pool).await;
            let result = prepared
                .spool_ordered_in_view(
                    &view,
                    &cx,
                    &params,
                    policy(),
                    &mut source,
                    &mut destination,
                    1,
                    64,
                    127,
                    4096,
                    100,
                    10_000_000,
                )
                .await;
            assert!(matches!(
                result,
                Err(NativeSpoolError::SetExecution(GraphSetExecutionError::Projection {
                    row,
                    column,
                    error,
                })) if row == expected_row && column == expected_column && error == expected_error
            ));
            assert_eq!(pool.used(), 0);
        }
        // The first identity has p=2 and takes the successful CASE arm. The
        // second performs a deferred element lookup whose actual p=1 must
        // still raise the late division error under LIMIT zero.
        for page in [0, 1] {
            let text = format!(
                "MATCH (n:L) WITH n AS item,n.p AS value RETURN CASE WHEN value = 2 THEN 7 ELSE 12 / ([item][0].p - 1) END AS output ORDER BY output LIMIT {page}"
            );
            let prepared = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
            let mut source = scratch(&cx, &pool).await;
            let mut destination = scratch(&cx, &pool).await;
            let result = prepared
                .spool_ordered_in_view(
                    &view,
                    &cx,
                    &params,
                    policy(),
                    &mut source,
                    &mut destination,
                    1,
                    64,
                    127,
                    4096,
                    100,
                    10_000_000,
                )
                .await;
            assert!(matches!(
                result,
                Err(NativeSpoolError::SetExecution(GraphSetExecutionError::Projection {
                    row: 1,
                    error,
                    ..
                })) if error.kind == GraphIntegerErrorKind::DivisionByZero
            ));
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_set_stages_share_work_and_keep_intermediate_and_final_row_quotas_separate() {
    let ((), report) = run_async_under_lab(0x7e4d_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, _) = database(
            &contexts.commit(),
            &[CanonicalScalar::Int(2), CanonicalScalar::Int(6)],
        )
        .await;
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n:L) WITH n.p AS value UNWIND [value, value + 1] AS item RETURN item * 2 AS result ORDER BY result DESC LIMIT 2",
            &params,
            symbols,
        )
        .unwrap();
        let pool = MemoryPool::new(64_000, 0).unwrap();
        let mut measured = None;
        for attempt in 0..8 {
            let mut allowance = GqlQueryPolicy::new(5, 2, 10_000_000, 1_000_000);
            let mut max_input = 10;
            let mut max_sort = 10_000_000;
            if let Some((stats, work)) = measured {
                let stats: fgdb_gql::GlaExecutionStats = stats;
                allowance.evaluator.max_work_units = stats.work_units;
                allowance.evaluator.max_scratch_entries = stats.scratch_entries;
                max_sort = work;
            }
            match attempt {
                2 => allowance.evaluator.max_work_units -= 1,
                3 => allowance.evaluator.max_scratch_entries -= 1,
                4 => max_sort -= 1,
                5 => allowance.rows = fgdb_gql::GqlExecutionBudget::new(5, 1),
                6 => max_input = 9,
                7 => allowance.rows = fgdb_gql::GqlExecutionBudget::new(4, 2),
                _ => {}
            }
            let mut source = scratch(&cx, &pool).await;
            let mut destination = scratch(&cx, &pool).await;
            let result = prepared
                .spool_ordered(
                    &db,
                    &cx,
                    &params,
                    allowance,
                    &mut source,
                    &mut destination,
                    2,
                    64,
                    127,
                    4096,
                    max_input,
                    max_sort,
                )
                .await;
            if attempt < 2 {
                let (spool, work) = result.unwrap();
                assert_eq!(spool.row_stats().snapshot_records, 5);
                assert_eq!(spool.row_stats().result_rows, 2);
                assert_eq!(spool.row_count(), 2);
                if let Some(expected) = measured {
                    assert_eq!((spool.evaluator_stats(), work), expected);
                }
                measured = Some((spool.evaluator_stats(), work));
                let mut cursor = spool.reader(&mut destination);
                for _ in 0..2 {
                    let row = cursor.next_row(&cx).await.unwrap().unwrap();
                    assert_eq!(
                        GraphValueRow::decode_stored(row.as_ref()).unwrap(),
                        GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                            CanonicalScalar::Int(14)
                        )])
                    );
                }
                assert!(cursor.next_row(&cx).await.unwrap().is_none());
            } else if attempt == 4 {
                assert!(matches!(
                    result,
                    Err(NativeSpoolError::SortWorkLimit { .. })
                ));
            } else {
                let error = result.unwrap_err();
                assert!(error.execution_error().is_some());
                let refusal = match error {
                    NativeSpoolError::Execute(error) => match *error {
                        GqlQueryError::Evaluator(error) => (Some(error.dimension), None),
                        GqlQueryError::Rows(error) => (None, Some(error.dimension)),
                        other => panic!("unexpected source refusal: {other:?}"),
                    },
                    other => panic!("unexpected quota refusal: {other:?}"),
                };
                assert_eq!(
                    refusal,
                    match attempt {
                        2 => (Some(GlaLimitDimension::WorkUnits), None),
                        3 => (Some(GlaLimitDimension::ScratchEntries), None),
                        5 | 6 => (None, Some(GqlBudgetDimension::ResultRows)),
                        7 => (None, Some(GqlBudgetDimension::SnapshotRecords)),
                        _ => unreachable!(),
                    }
                );
            }
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_order_refuses_unowned_sources_and_future_cuts_before_scratch() {
    let ((), report) = run_async_under_lab(0x7e4d_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, _) = database(&contexts.commit(), &[CanonicalScalar::Int(1)]).await;
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(64_000, 0).unwrap();
        for text in [
            "RETURN 1 + 2 AS value LIMIT 0",
            "MATCH (a:L) RETURN a.p + 1 AS value UNION ALL MATCH (b:L) RETURN b.p + 1 AS value LIMIT 0",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 999 RETURN n.p + 1 AS value LIMIT 0",
            "MATCH (a:L) FOR SYSTEM_TIME AS OF SEQ 999 RETURN a.p + 1 AS value UNION ALL MATCH (b:L) RETURN b.p + 1 AS value LIMIT 0",
        ] {
            let params = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let temporal = matches!(prepared, PreparedNativeRead::TemporalSet(_));
            let mut source = scratch(&cx, &pool).await;
            let mut destination = scratch(&cx, &pool).await;
            let result = prepared
                .spool_ordered_in_view(
                    &view,
                    &cx,
                    &params,
                    policy(),
                    &mut source,
                    &mut destination,
                    1,
                    64,
                    127,
                    4096,
                    100,
                    10_000_000,
                )
                .await;
            let NativeSpoolError::Prepare(error) = result.unwrap_err() else {
                panic!("expected preparation refusal: {text}");
            };
            if temporal {
                assert!(matches!(
                    *error,
                    fgdb::QueryError::Stream(GqlQueryError::Source(
                        fgdb_gql::stream::VertexScanError::Source(
                            fgdb::ReadError::BeyondFrontier {
                                asked: fgdb_types::CommitSeq(999),
                                ..
                            }
                        )
                    ))
                ));
            } else {
                assert!(matches!(
                    *error,
                    fgdb::QueryError::StreamingUnsupported { .. }
                ));
            }
            assert_eq!(
                source.stats(),
                fgdb_strata::tiered::memory::SpillStats::default()
            );
            assert_eq!(
                destination.stats(),
                fgdb_strata::tiered::memory::SpillStats::default()
            );
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn carried_properties_preserve_scalar_and_collection_values_across_spill_runs() {
    let ((), report) = run_async_under_lab(0x7e4d_0008, |root| async move {
        use fgdb_types::{
            CanonicalList, CanonicalMap, CanonicalMapEntry, CanonicalPropertyValue, CanonicalText,
        };

        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let samples: Vec<_> = families().into_iter().flatten().collect();
        let (mut db, _) = database(&contexts.commit(), &samples).await;
        let collection = CanonicalMap::try_new(vec![CanonicalMapEntry::new(
            CanonicalText::new_ucs_basic("nested").unwrap(),
            CanonicalPropertyValue::List(
                CanonicalList::try_new(vec![
                    CanonicalScalar::bytes(vec![0xa7; 257]).unwrap().into(),
                    CanonicalScalar::Int(17).into(),
                ])
                .unwrap(),
            ),
        )])
        .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex_values(
            VId(u128::MAX),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalPropertyValue::Map(collection))],
        );
        db.write(&contexts.commit(), batch).await.unwrap();
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        let text = "MATCH (n:L) WITH n AS item RETURN [item][0].p AS value ORDER BY value DESC";
        let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
        let PreparedNativeRead::Set(template) = &prepared else {
            panic!("carried property must use the unary relation plan");
        };
        let bound = template.bind_parameters(&params).unwrap();
        let expected = view
            .execute_graph_set_governed_at(&cx, &bound, view.frontier(), policy())
            .unwrap()
            .value;
        let mut measured = None;
        for run_rows in [1, 4] {
            let actual = element_spool(&view, &cx, &prepared, &params, policy(), run_rows)
                .await
                .unwrap();
            assert_eq!(actual.rows, expected);
            assert_eq!(actual.stats.snapshot_records, 2 * expected.len() as u64);
            assert_eq!(actual.snapshot, view.frontier());
            if let Some(previous) = measured {
                assert_eq!(actual, previous);
            }
            measured = Some(actual);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn carried_element_reads_share_snapshot_records_work_and_the_exact_history_cut() {
    let ((), report) = run_async_under_lab(0x7e4d_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, _) = database(&contexts.commit(), &[CanonicalScalar::Int(2)]).await;
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(
            VId(u128::MAX),
            vec![LabelId(1)],
            (1..=17)
                .map(|key| (PropertyKeyId(key), CanonicalScalar::Int(9)))
                .collect(),
        );
        batch.create_vertex(
            VId(6),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(17))],
        );
        batch.add_edge(
            EId(u128::MAX),
            VId(1),
            VId(u128::MAX),
            (1..=17)
                .map(|key| (PropertyKeyId(key), CanonicalScalar::Int(11)))
                .collect(),
        );
        batch.add_edge(EId(2), VId(1), VId(3), vec![]);
        batch.add_edge(
            EId(3),
            VId(1),
            VId(2),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(13))],
        );
        let first = db.write(&contexts.commit(), batch).await.unwrap();
        for value in 20..28 {
            let mut batch = WriteBatch::new(RelationId(1));
            batch.set_vertex_property(
                VId(u128::MAX),
                PropertyKeyId(1),
                Some(CanonicalScalar::Int(value)),
            );
            batch.set_edge_property(
                EId(u128::MAX),
                PropertyKeyId(1),
                Some(CanonicalScalar::Int(value + 100)),
            );
            if value == 27 {
                batch.delete_vertex(VId(6));
                batch.delete_edge(EId(3));
            }
            db.write(&contexts.commit(), batch).await.unwrap();
        }
        let view = db.read_session().unwrap();
        let items = vec![
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Edge(EId(u128::MAX)),
            GraphValue::Vertex(VId(3)),
            GraphValue::Edge(EId(2)),
            GraphValue::Vertex(VId(6)),
            GraphValue::Edge(EId(3)),
            GraphValue::Vertex(VId(u128::MAX - 1)),
            GraphValue::Edge(EId(u128::MAX - 1)),
            GraphValue::Vertex(VId(u128::MAX)),
        ];
        for historical in [true, false] {
            let temporal = if historical {
                "FOR SYSTEM_TIME AS OF SEQ $at"
            } else {
                ""
            };
            let prefix = format!(
                "MATCH (seed:L) {temporal} WITH seed LIMIT 1 WITH $items AS items UNWIND items AS item"
            );
            let text = format!(
                "{prefix} WITH item,[item][0].p AS value RETURN value,coalesce([item][0].p,0) AS repeated ORDER BY value NULLS FIRST,repeated"
            );
            let skip_text = format!(
                "{prefix} RETURN CASE WHEN true THEN 0 ELSE [item][0].p END AS value,coalesce(0,[item][0].p) AS repeated ORDER BY value NULLS FIRST,repeated"
            );
            let mut params = GqlParameters::new()
                .with_list("items", items.clone())
                .unwrap();
            if historical {
                params = params.with_uint64("at", first.0).unwrap();
            }
            let prepared = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
            let skipped = PreparedNativeRead::prepare(&skip_text, &params, symbols).unwrap();
            let (bound, as_of) = match &prepared {
                PreparedNativeRead::TemporalSet(template) => {
                    let bound = template.bind_parameters(&params).unwrap();
                    (bound.query().clone(), bound.as_of())
                }
                PreparedNativeRead::Set(template) => {
                    (template.bind_parameters(&params).unwrap(), view.frontier())
                }
                _ => panic!("expected unary element stages"),
            };
            let expected = view
                .execute_graph_set_governed_at(&cx, &bound, as_of, policy())
                .unwrap()
                .value;
            let skipped = element_spool(&view, &cx, &skipped, &params, policy(), 1)
                .await
                .unwrap();
            assert_eq!(skipped.rows.len(), items.len());
            assert!(skipped.rows.iter().all(|row| {
                row.values()
                    == [
                        GraphValue::Scalar(CanonicalScalar::Int(0)),
                        GraphValue::Scalar(CanonicalScalar::Int(0)),
                    ]
            }));
            let actual = element_spool(&view, &cx, &prepared, &params, policy(), 1)
                .await
                .unwrap();
            assert_eq!(actual.rows, expected);
            assert_eq!(actual.snapshot, as_of);
            assert_eq!(
                actual.stats.snapshot_records,
                skipped.stats.snapshot_records + 2 * items.len() as u64
            );
            let mut values = if historical {
                vec![
                    Some(9),
                    Some(11),
                    None,
                    None,
                    Some(17),
                    Some(13),
                    None,
                    None,
                    Some(9),
                ]
            } else {
                vec![
                    Some(27),
                    Some(127),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(27),
                ]
            };
            values.sort();
            let explicit: Vec<_> = values
                .into_iter()
                .map(|value| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Scalar(
                            value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                        ),
                        GraphValue::Scalar(CanonicalScalar::Int(value.unwrap_or(0))),
                    ])
                })
                .collect();
            assert_eq!(actual.rows, explicit);
            let exact = GqlQueryPolicy::new(
                actual.stats.snapshot_records,
                actual.stats.result_rows,
                actual.evaluator.work_units,
                actual.evaluator.scratch_entries,
            );
            assert_eq!(
                element_spool(&view, &cx, &prepared, &params, exact, 1)
                    .await
                    .unwrap(),
                actual
            );
            for dimension in [0, 1, 2] {
                let mut below = exact;
                match dimension {
                    0 => {
                        below.rows = fgdb_gql::GqlExecutionBudget::new(
                            actual.stats.snapshot_records - 1,
                            actual.stats.result_rows,
                        )
                    }
                    1 => below.evaluator.max_work_units -= 1,
                    2 => below.evaluator.max_scratch_entries -= 1,
                    _ => unreachable!(),
                }
                let error = element_spool(&view, &cx, &prepared, &params, below, 1)
                    .await
                    .unwrap_err();
                match (dimension, error.execution_error()) {
                    (0, Some(GqlQueryError::Rows(error))) => {
                        assert_eq!(error.dimension, GqlBudgetDimension::SnapshotRecords);
                        assert_eq!(error.limit, actual.stats.snapshot_records - 1);
                        assert_eq!(error.observed, actual.stats.snapshot_records);
                    }
                    (1, Some(GqlQueryError::Evaluator(error))) => {
                        assert_eq!(error.dimension, GlaLimitDimension::WorkUnits);
                        assert_eq!(error.limit, actual.evaluator.work_units - 1);
                        assert_eq!(error.observed, u128::from(actual.evaluator.work_units));
                    }
                    (2, Some(GqlQueryError::Evaluator(error))) => {
                        assert_eq!(error.dimension, GlaLimitDimension::ScratchEntries);
                        assert_eq!(error.limit, actual.evaluator.scratch_entries - 1);
                        assert_eq!(error.observed, u128::from(actual.evaluator.scratch_entries));
                    }
                    _ => panic!("wrong cumulative refusal: {error:?}"),
                }
            }
            // A zero lookup budget above the source count still succeeds
            // when CASE and coalesce never reach their element operands.
            let skipped_prepared =
                PreparedNativeRead::prepare(&skip_text, &params, symbols).unwrap();
            let no_reads = GqlQueryPolicy::new(
                skipped.stats.snapshot_records,
                skipped.stats.result_rows,
                skipped.evaluator.work_units,
                skipped.evaluator.scratch_entries,
            );
            assert_eq!(
                element_spool(&view, &cx, &skipped_prepared, &params, no_reads, 1)
                    .await
                    .unwrap(),
                skipped
            );
        }
        // Edge-rooted scans use the same reader and retain full-width EIds.
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(
            "MATCH ()-[r:R]->() WITH r AS item RETURN [item][0].p AS value ORDER BY value NULLS FIRST",
            &params,
            symbols,
        )
        .unwrap();
        let actual = element_spool(&view, &cx, &prepared, &params, policy(), 1)
            .await
            .unwrap();
        assert_eq!(
            actual.rows,
            [CanonicalScalar::Null, CanonicalScalar::Int(127)]
                .map(|value| GraphValueRow::from_owned_values(vec![GraphValue::Scalar(value)]))
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

mod authorized_carried_elements {
    use super::*;
    use asupersync::security::key::AuthKey;
    use fgdb::{
        AuthorizedOrderedReadSession, AuthorizedSortLimits, AuthorizedSpoolCursor, QueryError,
    };
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_types::{CommitCx, QueryCx};
    use fgdb_warden::{
        Authority, Error as AuthorizationError, Grant, LimitDimension, QueryLimits, Scope,
    };

    const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x62; 32]);
    type Resolver = fn(GraphSymbolKind, &str) -> Option<GraphSymbol>;
    type Session<'a> = AuthorizedOrderedReadSession<'a, Resolver, fn() -> u64, File, File>;

    fn authority() -> Authority {
        Authority::new(
            AuthKey::from_seed(0x7e4d_0010),
            NS,
            "graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap()
    }

    fn grant(nodes: u64) -> Grant {
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: nodes,
                max_work: 10_000_000,
                max_rows: 100,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([RelationId(1)]);
        grant.properties = Scope::only([PropertyKeyId(1)]);
        grant
    }

    fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        if (kind, name) == (GraphSymbolKind::Property, "hidden") {
            Some(GraphSymbol::Property(PropertyKeyId(2)))
        } else {
            symbols(kind, name)
        }
    }

    fn now() -> u64 {
        100
    }

    async fn session<'a>(
        db: &Database<MemVfs>,
        cx: &QueryCx,
        issuer: &'a Authority,
        scope: &Grant,
        pool: &MemoryPool,
        allowance: GqlQueryPolicy,
    ) -> Session<'a> {
        let token = issuer.issue_at(scope, 100).unwrap();
        let source = scratch(cx, pool).await;
        let destination = scratch(cx, pool).await;
        db.authorized_ordered_read_session(
            cx,
            issuer,
            &token,
            "main",
            resolve as Resolver,
            allowance,
            now as fn() -> u64,
            source,
            destination,
            AuthorizedSortLimits {
                run_rows: 2,
                max_runs: 16,
                page_bytes: 31,
                max_row_bytes: 4096,
                max_input_rows: 100,
                max_sort_work: 1_000_000,
            },
        )
        .unwrap()
    }

    async fn fixture(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
        let mut db = Database::open_memory(cx, DatabaseKeys::new([0x61; 32], NS, [0x63; 32]))
            .await
            .unwrap();
        let props = |value| {
            let mut props = vec![(PropertyKeyId(1), CanonicalScalar::Int(value))];
            if hidden {
                props.extend(
                    (2..18).map(|key| (PropertyKeyId(key), CanonicalScalar::Int(key as i64))),
                );
            }
            props
        };
        let mut batch = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, 2), (6, 13), (7, 19), (u128::MAX, 7)] {
            batch.create_vertex(
                VId(id),
                if hidden {
                    vec![LabelId(1), LabelId(99)]
                } else {
                    vec![LabelId(1)]
                },
                props(value),
            );
        }
        for (id, to, value) in [(u128::MAX, u128::MAX, 11), (6, 6, 17), (7, 7, 23)] {
            batch.add_edge(EId(id), VId(1), VId(to), props(value));
        }
        if hidden {
            batch.create_vertex(VId(99), vec![LabelId(99)], props(999));
            batch.add_edge(EId(13), VId(1), VId(99), props(999));
        }
        db.write(cx, batch).await.unwrap();
        if hidden {
            let mut foreign = WriteBatch::new(RelationId(99));
            foreign.add_edge(EId(20), VId(1), VId(u128::MAX), props(999));
            db.write(cx, foreign).await.unwrap();
            for value in 0..3 {
                let mut change = WriteBatch::new(RelationId(1));
                change.set_vertex_property(
                    VId(u128::MAX),
                    PropertyKeyId(2),
                    Some(CanonicalScalar::Int(value)),
                );
                change.set_edge_property(
                    EId(u128::MAX),
                    PropertyKeyId(2),
                    Some(CanonicalScalar::Int(value)),
                );
                db.write(cx, change).await.unwrap();
            }
        }
        db
    }

    fn expected(values: &[Option<i64>]) -> Vec<Vec<GraphValue>> {
        values
            .iter()
            .map(|value| {
                vec![GraphValue::Scalar(
                    value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                )]
            })
            .collect()
    }

    async fn rows(cursor: &mut AuthorizedSpoolCursor<'_, File>) -> Vec<Vec<GraphValue>> {
        let mut rows = Vec::new();
        while let Some(bytes) = cursor.next_row().await.unwrap() {
            rows.push(
                GraphValueRow::decode_canonical(bytes.as_ref())
                    .unwrap()
                    .values()
                    .to_vec(),
            );
        }
        rows
    }

    type DifferenceBag = std::collections::BTreeMap<Vec<fgdb::QueryValue>, i128>;

    fn native_bag(result: fgdb::QueryResult) -> DifferenceBag {
        let fgdb::QueryResult::Rows { columns, rows } = result else {
            panic!("expected native row result")
        };
        assert_eq!(columns, ["value"]);
        let mut bag = DifferenceBag::new();
        for row in rows {
            *bag.entry(row).or_default() += 1;
        }
        bag
    }

    fn subtract(before: &DifferenceBag, after: DifferenceBag) -> DifferenceBag {
        let mut difference = after;
        for (row, count) in before {
            *difference.entry(row.clone()).or_default() -= count;
        }
        difference.retain(|_, count| *count != 0);
        difference
    }

    async fn changes(
        cursor: &mut fgdb::AuthorizedDifferenceCursor<'_, File>,
    ) -> Result<DifferenceBag, NativeSpoolError> {
        let mut difference = DifferenceBag::new();
        while let Some((weight, frame)) = cursor.next_change().await? {
            assert_ne!(weight, 0);
            let row = GraphValueRow::decode_canonical(frame.as_ref())
                .unwrap()
                .values()
                .iter()
                .cloned()
                .map(fgdb::QueryValue::Value)
                .collect();
            assert!(difference.insert(row, weight).is_none());
        }
        assert_eq!(cursor.state(), fgdb_gql::scan_stream::ScanState::Exhausted);
        Ok(difference)
    }

    #[test]
    fn public_authorized_staged_differences_compare_complete_masked_endpoints() {
        let ((), report) = run_async_under_lab(0xd1ff_9101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let issuer = authority();
            let pool = MemoryPool::new(256_000, 0).unwrap();
            let params = GqlParameters::new()
                .with_list(
                    "items",
                    vec![
                        GraphValue::Vertex(VId(u128::MAX)),
                        GraphValue::Edge(EId(u128::MAX)),
                        GraphValue::Vertex(VId(6)),
                        GraphValue::Edge(EId(6)),
                        GraphValue::Vertex(VId(99)),
                        GraphValue::Edge(EId(13)),
                        GraphValue::Edge(EId(20)),
                        GraphValue::Vertex(VId(u128::MAX)),
                    ],
                )
                .unwrap();
            let mut visible_results = None;
            for hidden in [false, true] {
                let mut db = fixture(&commit, hidden).await;
                let old = db.pinned_read_view().unwrap();
                let before = old.frontier();
                let mut batch = WriteBatch::new(RelationId(1));
                batch.set_vertex_property(
                    VId(u128::MAX),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(70)),
                );
                batch.set_edge_property(
                    EId(u128::MAX),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(110)),
                );
                batch.set_vertex_label(VId(6), LabelId(1), false);
                batch.delete_vertex(VId(7));
                let after = db.write(&commit, batch).await.unwrap();
                let new = db.pinned_read_view().unwrap();
                let token = issuer.issue_at(&grant(100), 100).unwrap();
                let mut results = Vec::new();
                for (ordinal, text) in [
                    "MATCH (n) RETURN n.p + 1 AS value ORDER BY value",
                    "MATCH (n) WITH n AS item RETURN [item][0].p AS value ORDER BY value",
                    "MATCH (n) WITH n.p AS v ORDER BY v DESC LIMIT 3 \
                     UNWIND [v, v, 1] AS x RETURN x AS value ORDER BY value SKIP 1 LIMIT 6",
                    "MATCH (n) WITH DISTINCT n.p % 3 AS v \
                     RETURN v + 2 AS value ORDER BY value",
                    "MATCH (a)-[e:R]->(b) WITH e AS item \
                     RETURN [item][0].p AS value ORDER BY value",
                    "MATCH (seed:L) WITH seed LIMIT 1 WITH $items AS items \
                     UNWIND items AS item RETURN [item][0].p AS value ORDER BY value",
                ]
                .into_iter()
                .enumerate()
                {
                    let params = if text.contains("$items") {
                        params.clone()
                    } else {
                        GqlParameters::new()
                    };
                    let read = |view: &EmbeddedReadView| {
                        view.authorized_read_session(
                            &cx,
                            &issuer,
                            &token,
                            "main",
                            resolve as Resolver,
                            policy(),
                            now as fn() -> u64,
                        )
                        .unwrap()
                        .query(&cx, text, &params)
                        .map(native_bag)
                        .unwrap_or_else(|error| panic!("native {text}: {error:?}"))
                    };
                    let expected = subtract(&read(&old), read(&new));
                    let mut ordered =
                        session(&db, &cx, &issuer, &grant(100), &pool, policy()).await;
                    let prepared = ordered.prepare(&cx, text, &params).unwrap();
                    let execution = ordered
                        .difference_stream(&cx, &prepared, &params, before, after)
                        .unwrap_or_else(|error| panic!("open {text}: {error:?}"));
                    assert_eq!(execution.columns(), ["value"]);
                    assert_eq!((execution.before(), execution.after()), (before, after));
                    if ordinal == 0 {
                        let mut later = WriteBatch::new(RelationId(1));
                        later.set_vertex_property(
                            VId(u128::MAX),
                            PropertyKeyId(1),
                            Some(CanonicalScalar::Int(999)),
                        );
                        db.write(&commit, later).await.unwrap();
                        db.compact(&commit).await.unwrap();
                    }
                    let mut cursor = execution
                        .await
                        .unwrap_or_else(|error| panic!("execute {text}: {error:?}"));
                    assert_eq!(cursor.columns(), ["value"]);
                    let actual = changes(&mut cursor).await.unwrap();
                    assert_eq!(actual, expected, "{text}");
                    results.push(actual);
                    drop(cursor);
                    assert!(!ordered.is_closed());
                    assert_eq!(pool.used(), 0);
                }
                if let Some(visible) = &visible_results {
                    assert_eq!(&results, visible, "hidden history cannot alter changes");
                } else {
                    visible_results = Some(results);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn public_authorized_staged_differences_share_both_endpoint_allowances() {
        let ((), report) = run_async_under_lab(0xd1ff_9102, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let issuer = authority();
            let pool = MemoryPool::new(256_000, 0).unwrap();
            let params = GqlParameters::new();
            for hidden in [false, true] {
                let mut db = fixture(&contexts.commit(), hidden).await;
                let before = db.frontier().unwrap();
                let mut batch = WriteBatch::new(RelationId(1));
                batch.set_vertex_property(
                    VId(u128::MAX),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(70)),
                );
                let after = db.write(&contexts.commit(), batch).await.unwrap();
                for (records, nodes, refused) in
                    [(16, 16, None), (15, 16, Some(false)), (16, 15, Some(true))]
                {
                    let mut scope = grant(nodes);
                    scope.limits.max_rows = 2;
                    let mut ordered = session(
                        &db,
                        &cx,
                        &issuer,
                        &scope,
                        &pool,
                        GqlQueryPolicy::new(records, 2, 1_000_000, 1_000_000),
                    )
                    .await;
                    let prepared = ordered
                        .prepare(
                            &cx,
                            "MATCH (n) WITH n AS item RETURN [item][0].p AS value ORDER BY value",
                            &params,
                        )
                        .unwrap();
                    let result = ordered
                        .difference_stream(&cx, &prepared, &params, before, after)
                        .unwrap()
                        .await;
                    match refused {
                        Some(false) => assert!(matches!(result,
                            Err(NativeSpoolError::Authorized(error))
                            if matches!(*error, QueryError::Set(GqlQueryError::Rows(error))
                                if error.dimension == GqlBudgetDimension::SnapshotRecords
                                    && error.limit == 7 && error.observed == 8))),
                        Some(true) => assert!(matches!(result,
                            Err(NativeSpoolError::Authorized(error))
                            if matches!(*error, QueryError::Authorization(
                                AuthorizationError::LimitExceeded(LimitDimension::Nodes))))),
                        None => {
                            let mut cursor = result.unwrap();
                            let actual = changes(&mut cursor).await.unwrap();
                            let cell = |n| {
                                vec![fgdb::QueryValue::Value(GraphValue::Scalar(
                                    CanonicalScalar::Int(n),
                                ))]
                            };
                            assert_eq!(actual, DifferenceBag::from([(cell(7), -1), (cell(70), 1)]));
                        }
                    }
                    assert_eq!(pool.used(), 0);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn public_authorized_carried_reads_pin_history_and_mask_supplied_identities() {
        let ((), report) = run_async_under_lab(0x7e4d_0010, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let mut db = fixture(&commit, true).await;
            let issuer = authority();
            let pool = MemoryPool::new(256_000, 0).unwrap();
            let cut = db.frontier().unwrap();
            let items = vec![
                GraphValue::Vertex(VId(u128::MAX)),
                GraphValue::Edge(EId(u128::MAX)),
                GraphValue::Vertex(VId(6)),
                GraphValue::Edge(EId(6)),
                GraphValue::Vertex(VId(7)),
                GraphValue::Edge(EId(7)),
                GraphValue::Vertex(VId(99)),
                GraphValue::Edge(EId(13)),
                GraphValue::Edge(EId(20)),
                GraphValue::Vertex(VId(u128::MAX - 1)),
                GraphValue::Edge(EId(u128::MAX - 1)),
                GraphValue::Vertex(VId(u128::MAX)),
            ];
            let params = GqlParameters::new()
                .with_list("items", items.clone())
                .unwrap();
            let suffix = "WITH seed LIMIT 1 WITH $items AS items UNWIND items AS item \
                          RETURN [item][0].p AS value ORDER BY value NULLS FIRST";
            let mut pinned = session(&db, &cx, &issuer, &grant(100), &pool, policy()).await;
            let prepared = pinned
                .prepare(&cx, &format!("MATCH (seed:L) {suffix}"), &params)
                .unwrap();
            let execution = pinned.execute_ordered(&cx, &prepared, &params).unwrap();
            assert_eq!(execution.snapshot_seq(), cut);

            let mut change = WriteBatch::new(RelationId(1));
            change.set_vertex_property(
                VId(u128::MAX),
                PropertyKeyId(1),
                Some(CanonicalScalar::Int(70)),
            );
            change.set_edge_property(
                EId(u128::MAX),
                PropertyKeyId(1),
                Some(CanonicalScalar::Int(110)),
            );
            change.set_vertex_label(VId(6), LabelId(1), false);
            change.delete_vertex(VId(7));
            db.write(&commit, change).await.unwrap();
            db.compact(&commit).await.unwrap();

            let old = expected(&[
                None,
                None,
                None,
                None,
                None,
                Some(7),
                Some(7),
                Some(11),
                Some(13),
                Some(17),
                Some(19),
                Some(23),
            ]);
            let mut cursor = execution.await.unwrap();
            assert_eq!(cursor.snapshot_seq(), cut);
            assert_eq!(rows(&mut cursor).await, old);
            drop(cursor);
            assert_eq!(pool.used(), 0);

            for historical in [true, false] {
                let mut params = GqlParameters::new()
                    .with_list("items", items.clone())
                    .unwrap();
                let temporal = if historical {
                    params = params.with_uint64("cut", cut.0).unwrap();
                    "FOR SYSTEM_TIME AS OF SEQ $cut"
                } else {
                    ""
                };
                let mut ordered = session(&db, &cx, &issuer, &grant(100), &pool, policy()).await;
                let prepared = ordered
                    .prepare(&cx, &format!("MATCH (seed:L) {temporal} {suffix}"), &params)
                    .unwrap();
                let mut cursor = ordered
                    .execute_ordered(&cx, &prepared, &params)
                    .unwrap()
                    .await
                    .unwrap();
                assert_eq!(
                    cursor.snapshot_seq(),
                    if historical {
                        cut
                    } else {
                        db.frontier().unwrap()
                    }
                );
                assert_eq!(
                    rows(&mut cursor).await,
                    if historical {
                        old.clone()
                    } else {
                        expected(&[
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some(70),
                            Some(70),
                            Some(110),
                        ])
                    }
                );
                drop(cursor);
                assert_eq!(pool.used(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn public_authorized_staged_difference_reports_second_endpoint_errors_before_empty_delivery() {
        let ((), report) = run_async_under_lab(0xd1ff_9103, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let issuer = authority();
            let pool = MemoryPool::new(256_000, 0).unwrap();
            let params = GqlParameters::new();
            let mut db = fixture(&contexts.commit(), true).await;
            let before = db.frontier().unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            batch.set_vertex_property(
                VId(u128::MAX),
                PropertyKeyId(1),
                Some(CanonicalScalar::Int(70)),
            );
            let after = db.write(&contexts.commit(), batch).await.unwrap();
            let mut scope = grant(100);
            scope.limits.max_rows = 0;
            let mut ordered = session(
                &db,
                &cx,
                &issuer,
                &scope,
                &pool,
                GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000),
            )
            .await;
            let prepared = ordered
                .prepare(
                    &cx,
                    "MATCH (n) WITH n AS item \
                     RETURN 10 / ([item][0].p - 70) AS value ORDER BY value LIMIT 0",
                    &params,
                )
                .unwrap();
            let error = ordered
                .difference_stream(&cx, &prepared, &params, before, after)
                .unwrap()
                .await
                .unwrap_err();
            assert!(matches!(error, NativeSpoolError::Authorized(error)
                if matches!(*error, QueryError::Set(GqlQueryError::Source(
                    GraphSetExecutionError::Projection { row: 3, column: 0, error }))
                    if error.kind == GraphIntegerErrorKind::DivisionByZero)));
            assert!(!ordered.is_closed());
            assert_eq!(pool.used(), 0);

            // A zero changed-row allowance also permits complete equal-cut
            // evaluation: endpoint occurrences are never delivered rows.
            // The failed writer poisoned its private scratch file, so this
            // independent execution owns fresh host-provided files.
            drop(ordered);
            let mut ordered = session(
                &db,
                &cx,
                &issuer,
                &scope,
                &pool,
                GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000),
            )
            .await;
            let prepared = ordered
                .prepare(
                    &cx,
                    "MATCH (n) WITH n AS item RETURN [item][0].p AS value ORDER BY value",
                    &params,
                )
                .unwrap();
            let mut cursor = ordered
                .difference_stream(&cx, &prepared, &params, after, after)
                .unwrap()
                .await
                .unwrap();
            assert!(changes(&mut cursor).await.unwrap().is_empty());
            drop(cursor);
            assert!(!ordered.is_closed());
            assert_eq!(pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn public_authorized_carried_reads_share_limits_and_keep_late_errors() {
        let ((), report) = run_async_under_lab(0x7e4d_0011, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let issuer = authority();
            let pool = MemoryPool::new(256_000, 0).unwrap();
            let params = GqlParameters::new();
            // Private keys, labels, identities and historical versions cannot
            // change these public record/node boundaries or delivered values.
            for hidden in [false, true] {
                let db = fixture(&contexts.commit(), hidden).await;
                for (expression, records, nodes, values, refused) in [
                    (
                        "CASE WHEN true THEN 7 ELSE [item][0].p END",
                        4,
                        4,
                        vec![Some(7); 4],
                        None,
                    ),
                    ("coalesce(7, [item][0].p)", 4, 4, vec![Some(7); 4], None),
                    ("[item][0].hidden", 4, 4, vec![None; 4], None),
                    (
                        "[item][0].p",
                        8,
                        8,
                        vec![Some(2), Some(7), Some(13), Some(19)],
                        None,
                    ),
                    ("[item][0].p", 7, 8, vec![], Some(false)),
                    ("[item][0].p", 8, 7, vec![], Some(true)),
                ] {
                    let mut ordered = session(
                        &db,
                        &cx,
                        &issuer,
                        &grant(nodes),
                        &pool,
                        GqlQueryPolicy::new(records, 100, 1_000_000, 1_000_000),
                    )
                    .await;
                    let text = format!(
                        "MATCH (n) WITH n AS item RETURN {expression} AS value ORDER BY value"
                    );
                    let prepared = ordered.prepare(&cx, &text, &params).unwrap();
                    let result = ordered
                        .execute_ordered(&cx, &prepared, &params)
                        .unwrap()
                        .await;
                    match refused {
                        Some(true) => assert!(matches!(result,
                            Err(NativeSpoolError::Authorized(error))
                            if matches!(*error, QueryError::Authorization(
                                AuthorizationError::LimitExceeded(LimitDimension::Nodes))))),
                        Some(false) => assert!(matches!(result,
                            Err(NativeSpoolError::Authorized(error))
                            if matches!(*error, QueryError::Set(GqlQueryError::Rows(error))
                                if error.dimension == GqlBudgetDimension::SnapshotRecords
                                    && error.limit == 7 && error.observed == 8))),
                        None => {
                            let mut cursor = result.unwrap();
                            assert_eq!(rows(&mut cursor).await, expected(&values), "{text}");
                        }
                    }
                    assert_eq!(pool.used(), 0);
                }

                let mut ordered = session(&db, &cx, &issuer, &grant(100), &pool, policy()).await;
                let prepared = ordered
                    .prepare(
                        &cx,
                        "MATCH (n) WITH n AS item \
                     RETURN 10 / ([item][0].p - 7) AS value ORDER BY value LIMIT 0",
                        &params,
                    )
                    .unwrap();
                let error = ordered
                    .execute_ordered(&cx, &prepared, &params)
                    .unwrap()
                    .await
                    .unwrap_err();
                assert!(matches!(error, NativeSpoolError::Authorized(error)
                    if matches!(*error, QueryError::Set(GqlQueryError::Source(
                        GraphSetExecutionError::Projection { row: 3, column: 0, error }))
                        if error.kind == GraphIntegerErrorKind::DivisionByZero)));
                assert!(!ordered.is_closed());
                assert_eq!(pool.used(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
