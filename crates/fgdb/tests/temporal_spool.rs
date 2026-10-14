//! Temporal scalar profiles must survive the real native query/spill boundary.
//! Expected values use typed ordering, never the borrowed frame comparator.

use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, NativeSpoolError, PreparedNativeRead, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, GraphValueRow};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalDate, CanonicalDuration, CanonicalLocalDateTime, CanonicalLocalTime, CanonicalScalar,
    CanonicalTime, DatabaseSecurityNamespaceId, PurposeContexts, VId,
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
