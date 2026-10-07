//! Production source -> input partitions -> shared reducer -> typed result laws.
use super::*;
use crate::{NativeAggregateSpool, NativeAggregateSpoolError};
use fgdb_gql::{GraphAggregateRow, GraphAggregateValue};

const AGGREGATE: &str = "MATCH (n:L) RETURN n.p AS category, COUNT(*) AS rows, COUNT(n.q) AS present, SUM(n.q) AS total, AVG(n.q) AS average, MIN(n.q) AS minimum, MAX(n.q) AS maximum";

async fn aggregate_scratch(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, File) {
    let file = File::default();
    let scratch = SpillFile::new(
        cx,
        file.clone(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 32_000_000,
            max_runs: 2048,
            max_run_bytes: 8_000_000,
        },
    )
    .await
    .unwrap();
    (scratch, file)
}
async fn aggregate_seed(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 0..64_u128 {
        let mut props = vec![(PropertyKeyId(1), CanonicalScalar::Int((id % 8) as i64))];
        if id % 5 != 0 {
            props.push((PropertyKeyId(2), CanonicalScalar::Int(i64::MAX - id as i64)));
        }
        batch.create_vertex(VId(id), vec![LabelId(1)], props);
        if id != 0 {
            batch.add_edge(
                EId(id),
                VId(0),
                VId(id),
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int((id % 8) as i64)),
                    (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
                ],
            );
        }
    }
    // An independent parallel edge must remain another input occurrence.
    batch.add_edge(
        EId(999),
        VId(0),
        VId(1),
        vec![
            (PropertyKeyId(1), CanonicalScalar::Int(1)),
            (PropertyKeyId(2), CanonicalScalar::Int(1)),
        ],
    );
    db.write(cx, batch).await.unwrap();
    db
}
async fn aggregate_contents(
    spool: &NativeAggregateSpool,
    file: &mut SpillFile<File>,
    cx: &QueryCx,
) -> Vec<GraphAggregateRow> {
    let mut reader = spool.reader(file, None);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await.unwrap() {
        rows.push((*row).clone());
    }
    assert_eq!(reader.state(), ScanState::Exhausted);
    assert!(reader.next_row(cx).await.unwrap().is_none());
    rows
}

#[test]
fn grace_partitions_match_native_numeric_vertex_edge_and_historical_results() {
    let ((), report) = run_async_under_lab(0x5ba1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(63));
        db.write(&contexts.commit(), change).await.unwrap();
        for text in [
            AGGREGATE,
            "MATCH (a)-[e:R]->(b) RETURN e.p AS category, COUNT(*) AS rows, SUM(e.q) AS total, AVG(e.q) AS average",
            "MATCH (a)-[e:R]-(b) RETURN e.p AS category, COUNT(*) AS rows",
        ] {
            let prepared = plan(text);
            let params = GqlParameters::new();
            let ordinary = prepared
                .stream_aggregate_in_view(&view, &cx, &params, policy())
                .unwrap();
            let columns = ordinary.columns().to_vec();
            let slots = ordinary.output_slots().to_vec();
            let expected: Vec<_> = ordinary.map(|row| row.unwrap()).collect();
            for capacity in [1, 3] {
                let pool = MemoryPool::new(4_000_000, 0).unwrap();
                let (mut source, _) = aggregate_scratch(&cx, &pool).await;
                let (mut partition, _) = aggregate_scratch(&cx, &pool).await;
                let (mut destination, _) = aggregate_scratch(&cx, &pool).await;
                let (spool, work) = prepared
                    .spool_aggregate_in_view(
                        &view,
                        &cx,
                        &params,
                        policy(),
                        &mut source,
                        &mut partition,
                        &mut destination,
                        capacity,
                        256,
                        2,
                        128,
                        257,
                        4096,
                        1000,
                        100_000_000,
                        None,
                    )
                    .await
                    .unwrap();
                assert!(work > 0);
                assert_eq!(spool.columns(), columns);
                assert_eq!(spool.output_slots(), slots);
                assert_eq!(spool.snapshot_seq(), view.frontier());
                assert_eq!(spool.row_count(), expected.len() as u64);
                assert!(
                    partition.stats().published_runs > 0,
                    "must really partition input"
                );
                assert_eq!(pool.used(), 0);
                assert_eq!(
                    aggregate_contents(&spool, &mut destination, &cx).await,
                    expected
                );
                assert_eq!(pool.used(), 0);
                // A result handle is bound to its declared final file.
                let mut wrong = spool.reader(&mut source, None);
                assert!(wrong.next_row(&cx).await.is_err());
                assert_eq!(wrong.state(), ScanState::Failed);
                assert!(wrong.next_row(&cx).await.unwrap().is_none());
            }
            if text == AGGREGATE {
                assert!(expected.iter().any(|row| matches!(row.values()[2], GraphAggregateValue::Integer(total) if total > i128::from(i64::MAX))));
                assert!(
                    expected
                        .iter()
                        .any(|row| matches!(row.values()[3], GraphAggregateValue::Average(_)))
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_global_grouped_and_all_partition_refusals_publish_no_partial_result() {
    let ((), report) = run_async_under_lab(0x5ba1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for (text, count) in [
            (
                "MATCH (n:L) WHERE n.p = -1 RETURN COUNT(*) AS rows, SUM(n.q) AS total, AVG(n.q) AS average",
                1,
            ),
            (
                "MATCH (n:L) WHERE n.p = -1 RETURN n.p AS category, COUNT(*) AS rows",
                0,
            ),
        ] {
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            // Total-created allowance must not be reserved as resident live
            // metadata: binary DFS keeps at most257 pending partitions.
            let (spool, _) = plan(text)
                .spool_aggregate_in_view(
                    &view,
                    &cx,
                    &GqlParameters::new(),
                    policy(),
                    &mut a,
                    &mut b,
                    &mut c,
                    1,
                    usize::MAX,
                    2,
                    128,
                    257,
                    4096,
                    1000,
                    100_000_000,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(spool.row_count(), count);
            let rows = aggregate_contents(&spool, &mut c, &cx).await;
            if count == 1 {
                assert_eq!(rows[0].values()[0], GraphAggregateValue::Count(0));
                assert!(rows[0].values()[1].is_null());
                assert!(rows[0].values()[2].is_null());
            }
            assert_eq!(pool.used(), 0);
        }
        for refusal in 0..6 {
            let pool = MemoryPool::new(4_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            let policy = if refusal == 4 {
                GqlQueryPolicy::new(1000, 0, 100_000_000, 1_000_000)
            } else {
                policy()
            };
            let text = if refusal == 5 {
                "MATCH (n:L) RETURN n.p AS category, COUNT(*) AS rows LIMIT 0"
            } else {
                AGGREGATE
            };
            let result = plan(text)
                .spool_aggregate_in_view(
                    &view,
                    &cx,
                    &GqlParameters::new(),
                    policy,
                    &mut a,
                    &mut b,
                    &mut c,
                    1,
                    if refusal == 0 { 1 } else { 256 },
                    2,
                    128,
                    257,
                    if refusal == 1 { 32 } else { 4096 },
                    if refusal == 2 { 63 } else { 1000 },
                    if refusal == 3 { 1 } else { 100_000_000 },
                    None,
                )
                .await;
            match refusal {
                0 => assert!(matches!(
                    result,
                    Err(NativeAggregateSpoolError::PartitionLimit { .. })
                )),
                1 => assert!(matches!(
                    result,
                    Err(NativeAggregateSpoolError::Spool(
                        NativeSpoolError::RowTooLarge { .. }
                    ))
                )),
                2 => assert!(matches!(
                    result,
                    Err(NativeAggregateSpoolError::InputRows { .. })
                )),
                3 => assert!(matches!(
                    result,
                    Err(NativeAggregateSpoolError::Spool(
                        NativeSpoolError::SortWorkLimit { .. }
                    ))
                )),
                4 => assert!(matches!(result, Err(NativeAggregateSpoolError::Execute(_)))),
                5 => {
                    assert!(matches!(
                        result,
                        Err(NativeAggregateSpoolError::Unsupported)
                    ));
                    assert_eq!(a.stats().published_runs, 0);
                }
                _ => unreachable!(),
            }
            assert_eq!(c.stats().published_runs, 0);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_reader_keeps_result_memory_charged_and_fuses_after_dropped_pending_read() {
    let ((), report) = run_async_under_lab(0x5ba1_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(4_000_000, 0).unwrap();
        let (mut a, _) = aggregate_scratch(&cx, &pool).await;
        let (mut b, _) = aggregate_scratch(&cx, &pool).await;
        let (mut c, file) = aggregate_scratch(&cx, &pool).await;
        let (spool, _) = plan(AGGREGATE)
            .spool_aggregate_in_view(
                &view,
                &cx,
                &GqlParameters::new(),
                policy(),
                &mut a,
                &mut b,
                &mut c,
                1,
                256,
                2,
                128,
                257,
                4096,
                1000,
                100_000_000,
                None,
            )
            .await
            .unwrap();
        let mut reader = spool.reader(&mut c, None);
        let row = reader.next_row(&cx).await.unwrap().unwrap();
        assert!(pool.used() > 0);
        reader.close();
        drop(reader);
        assert!(
            pool.used() > 0,
            "decoded ownership must retain its reservation"
        );
        drop(row);
        assert_eq!(pool.used(), 0);
        file.0.lock().unwrap().pending_read = true;
        let mut reader = spool.reader(&mut c, None);
        {
            let mut future = std::pin::pin!(reader.next_row(&cx));
            assert!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        assert_eq!(reader.state(), ScanState::Failed);
        assert!(reader.next_row(&cx).await.unwrap().is_none());
        drop(reader);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
