//! Native definitions over fixture-owned completed relational runs. Production
//! authentication, partitioning, cell updates, output ordering and memory
//! admission execute below; these are not database-open or authorization tests.
use super::*;
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_gql::spill_aggregate::{AsyncSpillAggregatePlan, SpillAggregateBuildError};
use fgdb_strata::tiered::memory::SpillLimits;
use fgdb_types::PurposeContexts;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};

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

#[derive(Default)]
struct Meter {
    calls: usize,
    stop: Option<usize>,
    rows: u64,
}
impl GroupInput for Meter {
    fn next_input(&mut self) -> crate::SendFuture<'_, Result<Option<SpoolRow>>> {
        panic!("a completed relation cannot reopen its source")
    }
    fn charge(&mut self, _: VertexScanEvent) -> core::result::Result<(), ExecutionError> {
        self.calls += 1;
        if self.stop == Some(self.calls) {
            return Err(GqlQueryError::Source(
                fgdb_gql::GraphAggregateError::ResultCountOverflow,
            ));
        }
        Ok(())
    }
    fn finish_result(&mut self) -> core::result::Result<(), ExecutionError> {
        self.rows += 1;
        Ok(())
    }
    fn exhausted(&self) -> bool {
        true
    }
    fn snapshot_seq(&self) -> CommitSeq {
        CommitSeq(0)
    }
    fn kind(&self) -> ScanKind {
        ScanKind::Vertex
    }
    fn row_stats(&self) -> GqlExecutionStats {
        GqlExecutionStats {
            snapshot_records: 0,
            result_rows: self.rows,
        }
    }
    fn evaluator_stats(&self) -> GlaExecutionStats {
        GlaExecutionStats::default()
    }
}

fn opened(text: &str, stop: Option<usize>) -> Opened<'static> {
    let parameters = GqlParameters::new();
    let prepared = PreparedNativeRead::prepare(text, &parameters, |_, _| None).unwrap();
    // This also exercises the public buffered compiler's class selection.
    prepared.prepare_buffered_aggregate(&parameters).unwrap();
    let PreparedNativeRead::PipelineAggregate(template) = prepared else {
        panic!("fixture must have a complete relational input: {text}");
    };
    let bound = template.bind_relation_parameters(&parameters).unwrap();
    let plan = AsyncSpillAggregatePlan::compile_relation(&bound).unwrap();
    assert!(matches!(plan, AsyncSpillAggregatePlan::Relation { .. }));
    assert!(plan.definition().has_collections());
    Opened {
        input: Box::new(Meter {
            stop,
            ..Meter::default()
        }),
        definition: plan.definition().clone(),
        columns: template.columns().to_vec(),
        slots: template.output_slots().to_vec(),
    }
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn text(value: &str) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())
}
fn list(values: Vec<GraphValue>) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::List(values.into_boxed_slice()))
}
const GLOBAL: &str =
    "UNWIND [0] AS v RETURN COLLECT(v) AS items,COLLECT(DISTINCT v) AS unique_items";
const GROUPED: &str = "UNWIND [{g:0,v:0}] AS r WITH r.g AS g,r.v AS v RETURN g,COLLECT(v) AS items,COLLECT(DISTINCT v) AS unique_items ORDER BY g";

async fn scratch(cx: &QueryCx, pool: &MemoryPool) -> SpillFile<File> {
    SpillFile::new(
        cx,
        File::default(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 256_000_000,
            max_runs: 8192,
            max_run_bytes: 32_000_000,
        },
    )
    .await
    .unwrap()
}

async fn input_run(
    cx: &QueryCx,
    file: &mut SpillFile<File>,
    width: usize,
    rows: Vec<Vec<GraphValue>>,
) -> NativeResultSpool {
    let count = rows.len() as u64;
    let mut largest = 0;
    let mut writer = file.paged_writer(cx, 257).unwrap();
    let mut work = Work {
        cx,
        used: 0,
        limit: u64::MAX,
    };
    for row in rows {
        assert_eq!(row.len(), width);
        let bytes = GraphValueRow::from_owned_values(row)
            .canonical_bytes()
            .unwrap();
        largest = largest.max(bytes.len());
        work.write(&mut writer, &bytes).await.unwrap();
    }
    let run = writer.finish(cx).await.unwrap();
    NativeResultSpool {
        columns: Arc::from([]),
        encoded_columns: width,
        snapshot: CommitSeq(0),
        kind: ScanKind::Vertex,
        rows: GqlExecutionStats {
            snapshot_records: 0,
            result_rows: count,
        },
        evaluator: GlaExecutionStats::default(),
        max_row_bytes: largest,
        run,
    }
}

async fn execute_fixture(
    cx: &QueryCx,
    opened: Opened<'_>,
    rows: Vec<Vec<GraphValue>>,
    pool: &MemoryPool,
    capacity: usize,
    max_row_bytes: usize,
) -> Result<Vec<GraphAggregateRow>> {
    let mut a = scratch(cx, pool).await;
    let mut b = scratch(cx, pool).await;
    let mut c = scratch(cx, pool).await;
    let initial = input_run(cx, &mut a, opened.definition.input_width(), rows).await;
    let (spool, _) = execute(
        opened,
        Some(initial),
        0,
        cx,
        &mut a,
        &mut b,
        &mut c,
        capacity,
        127,
        1,
        512,
        257,
        max_row_bytes,
        10_000,
        1_000_000_000,
        None,
    )
    .await?;
    assert_eq!(pool.used(), 0, "no retained group state after publication");
    let mut actual = Vec::new();
    let mut reader = spool.reader(&mut c, None);
    while let Some(row) = reader.next_row(cx).await? {
        actual.push((*row).clone());
    }
    assert_eq!(actual.len() as u64, spool.row_count());
    drop(reader);
    assert_eq!(pool.used(), 0);
    Ok(actual)
}

#[test]
fn external_collection_keeps_null_elision_nested_values_and_first_occurrence_order() {
    let ((), report) = run_async_under_lab(0xc011_0001, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let nested = GraphValue::map(vec![(
            "k".into(),
            GraphValue::List(vec![int(7), int(2)].into_boxed_slice()),
        )])
        .unwrap();
        let input = vec![
            int(9),
            GraphValue::Scalar(CanonicalScalar::Null),
            int(2),
            int(9),
            text("2"),
            nested.clone(),
            nested.clone(),
        ];
        let rows = execute_fixture(
            &cx,
            opened(GLOBAL, None),
            input.into_iter().map(|v| vec![v]).collect(),
            &pool,
            1,
            65_536,
        )
        .await
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].values(),
            &[
                list(vec![
                    int(9),
                    int(2),
                    int(9),
                    text("2"),
                    nested.clone(),
                    nested.clone()
                ]),
                list(vec![int(9), int(2), text("2"), nested]),
            ]
        );
        for input in [
            vec![],
            vec![vec![GraphValue::Scalar(CanonicalScalar::Null)]],
        ] {
            let rows = execute_fixture(&cx, opened(GLOBAL, None), input, &pool, 1, 65_536)
                .await
                .unwrap();
            assert_eq!(rows[0].values(), &[list(vec![]), list(vec![])]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn partition_replay_preserves_collection_order_and_numeric_distinct_in_the_same_group() {
    let ((), report) = run_async_under_lab(0xc011_0002, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let query = "UNWIND [{g:0,v:0}] AS r WITH r.g AS g,r.v AS v RETURN g,COLLECT(v) AS items,COLLECT(DISTINCT v) AS unique_items,SUM(DISTINCT v) AS total ORDER BY g";
        let mut input = Vec::new();
        for value in [9, 2, 9, 4] {
            for key in (0..12).rev() {
                input.push(vec![int(key), int(value)]);
            }
        }
        for capacity in [1, 3, 16] {
            let rows = execute_fixture(
                &cx,
                opened(query, None),
                input.clone(),
                &pool,
                capacity,
                65_536,
            )
            .await
            .unwrap();
            assert_eq!(rows.len(), 12);
            for (key, row) in rows.iter().enumerate() {
                assert_eq!(row.keys(), &[int(key as i64)]);
                assert_eq!(
                    row.values(),
                    &[
                        list(vec![int(9), int(2), int(9), int(4)]),
                        list(vec![int(9), int(2), int(4)]),
                        GraphAggregateValue::Integer(15),
                    ]
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn many_collection_groups_remain_external_when_the_combined_payload_exceeds_the_pool() {
    let ((), report) = run_async_under_lab(0xc011_0003, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(524_288, 0).unwrap();
        let mut input = Vec::new();
        for occurrence in 0..3 {
            for key in 0..64 {
                input.push(vec![
                    int(key),
                    text(&format!("{occurrence}:{}", "x".repeat(2048))),
                ]);
            }
        }
        assert!(2 * 64 * 3 * 2048 > pool.limit());
        let rows = execute_fixture(&cx, opened(GROUPED, None), input, &pool, 1, 65_536)
            .await
            .unwrap();
        assert_eq!(rows.len(), 64);
        for (key, row) in rows.iter().enumerate() {
            assert_eq!(row.keys(), &[int(key as i64)]);
            let expected = list(
                (0..3)
                    .map(|occurrence| text(&format!("{occurrence}:{}", "x".repeat(2048))))
                    .collect(),
            );
            assert_eq!(row.values(), &[expected.clone(), expected]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn oversized_collections_and_native_refusals_publish_nothing_and_refund_memory() {
    let ((), report) = run_async_under_lab(0xc011_0004, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let input: Vec<_> = (0..80)
            .map(|i| vec![text(&format!("{i}:{}", "x".repeat(512)))])
            .collect();
        let error = execute_fixture(&cx, opened(GLOBAL, None), input, &pool, 1, 65_536)
            .await
            .unwrap_err();
        assert!(capacity_refusal(&error), "{error}");
        assert_eq!(pool.used(), 0);
        for tail in ["", " LIMIT 0"] {
            let input: Vec<_> = (0..8)
                .map(|i| vec![text(&format!("{i}:{}", "x".repeat(128)))])
                .collect();
            let error = execute_fixture(
                &cx,
                opened(&format!("{GLOBAL}{tail}"), None),
                input,
                &pool,
                1,
                512,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(
                    error,
                    NativeAggregateSpoolError::Spool(NativeSpoolError::RowTooLarge { .. })
                ),
                "{error}"
            );
            assert_eq!(pool.used(), 0);
        }
        let input = vec![vec![int(1)], vec![int(2)], vec![int(3)]];
        let error = execute_fixture(&cx, opened(GLOBAL, Some(8)), input, &pool, 1, 65_536)
            .await
            .unwrap_err();
        assert!(
            matches!(error, NativeAggregateSpoolError::Execute(_)),
            "{error}"
        );
        assert!(!capacity_refusal(&error));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_wider_later_row_does_not_refund_still_live_collection_payloads() {
    let ((), report) = run_async_under_lab(0xc011_0005, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root).query();
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut retained = Vec::new();
        for wider in [false, true] {
            let mut opened = opened(GLOBAL, None);
            let mut rows: Vec<_> = (0..40).map(|value| vec![int(value)]).collect();
            if wider {
                rows.push(vec![text(&"x".repeat(512))]);
            }
            let mut file = scratch(&cx, &pool).await;
            let initial = input_run(&cx, &mut file, opened.definition.input_width(), rows).await;
            let mut work = Work {
                cx: &cx,
                used: 0,
                limit: u64::MAX,
            };
            let groups = reduce_partition(
                &initial,
                &mut file,
                &opened.definition,
                opened.input.as_mut(),
                1,
                &mut work,
                None,
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(groups.values.len(), 1);
            retained.push(groups.values[0]._charge.bytes());
            drop(groups);
            assert_eq!(pool.used(), 0);
        }
        // The added row retains every preceding occurrence and DISTINCT
        // membership entry. Replacing the group guard with its new widest-row
        // allowance would discard those older reservations and fail this law.
        assert!(retained[1] > retained[0], "{retained:?}");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn direct_graph_collection_still_requires_its_own_order_proof() {
    let parameters = GqlParameters::new();
    let prepared = PreparedNativeRead::prepare(
        "MATCH (n) RETURN COLLECT(n) AS nodes",
        &parameters,
        |_, _| None,
    )
    .unwrap();
    let PreparedNativeRead::Aggregate(template) = prepared else {
        panic!("direct aggregate");
    };
    let bound = template.bind_parameters(&parameters).unwrap();
    assert!(matches!(
        SpillAggregatePlan::compile(&bound),
        Err(SpillAggregateBuildError::Unsupported)
    ));
    assert!(matches!(
        AsyncSpillAggregatePlan::compile(&bound),
        Err(SpillAggregateBuildError::Unsupported)
    ));
}
