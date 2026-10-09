//! Production source -> input partitions -> shared reducer -> typed result laws.
use super::*;
use crate::{NativeAggregateSpool, NativeAggregateSpoolError};
use fgdb_gql::algebra::GraphValue;
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
fn argument_distinct_external_passes_preserve_bags_typed_support_and_output_clauses() {
    let ((), report) = run_async_under_lab(0x5ba1_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for text in [
            "MATCH (n:L) RETURN COUNT(*) AS rows,COUNT(DISTINCT n.p) AS unique,SUM(n.p) AS bag_sum,SUM(DISTINCT n.p) AS total,AVG(DISTINCT n.p) AS average",
            "MATCH (n:L) RETURN n.p%3 AS category,COUNT(DISTINCT n.q) AS present,SUM(DISTINCT n.p) AS total,AVG(DISTINCT n.p) AS average GROUP BY n.p%3 ORDER BY total DESC",
            "MATCH (n:L) RETURN DISTINCT COUNT(DISTINCT n.p) AS unique GROUP BY n.q%3 HAVING SUM(DISTINCT n.p)>0 ORDER BY AVG(DISTINCT n.p) DESC SKIP 1 LIMIT 2",
            "MATCH (n:L) RETURN {unique:COUNT(DISTINCT n.q),total:SUM(DISTINCT n.p)} AS result GROUP BY n.p ORDER BY SUM(DISTINCT n.q) DESC",
            "MATCH (n:L) RETURN COUNT(DISTINCT [n.p%3,n.p]) AS lists,COUNT(DISTINCT {bucket:n.p%3}) AS maps",
            "MATCH (n:L) RETURN COUNT(DISTINCT CASE WHEN n.p%2=0 THEN 1 ELSE 1.0 END) AS unique,SUM(DISTINCT CASE WHEN n.p%2=0 THEN 1 ELSE 1.0 END) AS total,AVG(DISTINCT CASE WHEN n.p%2=0 THEN 1 ELSE 1.0 END) AS average",
            "MATCH (a)-[e:R]->(b) RETURN e.p%3 AS category,COUNT(*) AS rows,COUNT(DISTINCT a) AS sources,COUNT(DISTINCT e.q) AS values,SUM(DISTINCT e.q) AS total GROUP BY e.p%3",
            "MATCH (a)-[e:R]-(b) RETURN COUNT(*) AS rows,COUNT(DISTINCT e.q) AS values,AVG(DISTINCT e.q) AS average GROUP BY b.p ORDER BY SUM(DISTINCT e.q) DESC",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(DISTINCT n.p) AS unique ORDER BY SUM(DISTINCT n.q) LIMIT 0",
            "MATCH (n:L) WHERE n.p<0 RETURN COUNT(DISTINCT n.q) AS unique,SUM(DISTINCT n.q) AS total,AVG(DISTINCT n.q) AS average",
            "MATCH (n:L) WHERE n.p<0 RETURN n.p AS category,COUNT(DISTINCT n.q) AS unique",
        ] {
            let prepared = plan(text);
            let parameters = GqlParameters::new();
            let ordinary = prepared
                .stream_aggregate_in_view(&view, &cx, &parameters, policy())
                .unwrap();
            let columns = ordinary.columns().to_vec();
            let expected = ordinary.collect::<Result<Vec<_>, _>>().unwrap();
            for (capacity, run_rows) in [(1, 1), (3, 3)] {
                let pool = MemoryPool::new(1_000_000, 0).unwrap();
                let (mut a, _) = aggregate_scratch(&cx, &pool).await;
                let (mut b, _) = aggregate_scratch(&cx, &pool).await;
                let (mut c, _) = aggregate_scratch(&cx, &pool).await;
                let (spool, _) = prepared
                    .spool_aggregate_in_view(
                        &view,
                        &cx,
                        &parameters,
                        GqlQueryPolicy::new(10_000, expected.len() as u64, 100_000_000, 1_000_000),
                        &mut a,
                        &mut b,
                        &mut c,
                        capacity,
                        256,
                        run_rows,
                        128,
                        257,
                        16_384,
                        1000,
                        100_000_000,
                        None,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!(spool.columns(), columns, "{text}");
                assert_eq!(
                    aggregate_contents(&spool, &mut c, &cx).await,
                    expected,
                    "{text}"
                );
                assert_eq!(pool.used(), 0, "{text}");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn one_group_argument_support_larger_than_memory_uses_external_sort() {
    let ((), report) = run_async_under_lab(0x5ba1_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 0..1024 {
            batch.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(
                    PropertyKeyId(1),
                    CanonicalScalar::ucs_basic_text(&format!(
                        "{:04}-{}",
                        id % 512,
                        "x".repeat(768)
                    ))
                    .unwrap(),
                )],
            );
        }
        db.write(&contexts.commit(), batch).await.unwrap();
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(131_072, 0).unwrap();
        assert!(
            512 * 768 > pool.limit(),
            "even unique payloads exceed the complete pool"
        );
        let (mut a, _) = aggregate_scratch(&cx, &pool).await;
        let (mut b, _) = aggregate_scratch(&cx, &pool).await;
        let (mut c, _) = aggregate_scratch(&cx, &pool).await;
        let (spool, _) = plan("MATCH (n:L) RETURN COUNT(*) AS rows,COUNT(DISTINCT n.p) AS unique")
            .spool_aggregate_in_view(
                &view,
                &cx,
                &GqlParameters::new(),
                GqlQueryPolicy::new(1024, 1, 100_000_000, 1_000_000),
                &mut a,
                &mut b,
                &mut c,
                1,
                1,
                16,
                64,
                257,
                4096,
                1024,
                100_000_000,
                None,
            )
            .await
            .unwrap();
        assert!(
            b.stats().published_runs > 64,
            "the single group requires real merge passes"
        );
        let rows = aggregate_contents(&spool, &mut c, &cx).await;
        assert_eq!(
            rows[0].values(),
            &[
                GraphAggregateValue::Count(1024),
                GraphAggregateValue::Count(512)
            ]
        );
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_argument_type_errors_precede_sorting_even_when_no_result_is_requested() {
    let ((), report) = run_async_under_lab(0x5ba1_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = aggregate_seed(&contexts.commit()).await;
        let mut extra = WriteBatch::new(RelationId(1));
        extra.create_vertex(
            VId(1000),
            vec![LabelId(1)],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(100)),
                (PropertyKeyId(2), CanonicalScalar::Bool(true)),
            ],
        );
        db.write(&contexts.commit(), extra).await.unwrap();
        let view = db.read_session().unwrap();
        for window in ["", " LIMIT 0", " LIMIT 1", " SKIP 1000 LIMIT 1"] {
            let text = format!(
                "MATCH (n:L) RETURN n.p AS category,COUNT(DISTINCT n.q) AS unique,SUM(DISTINCT n.q) AS total ORDER BY category{window}"
            );
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            let error = plan(&text)
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
                .unwrap_err();
            let NativeAggregateSpoolError::Execute(error) = error else {
                panic!("input must reach its native numeric type validation");
            };
            assert!(
                matches!(
                    *error,
                    fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphAggregateError::NonIntegerSum {
                        aggregate: 1
                    })
                ),
                "{text}"
            );
            assert_eq!(
                a.stats().published_runs,
                0,
                "no complete input exists after a source type failure"
            );
            assert_eq!(b.stats().published_runs, 0);
            assert_eq!(c.stats().published_runs, 0);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_support_write_failure_and_dropped_append_refund_owned_group_state() {
    let ((), report) = run_async_under_lab(0x5ba1_0013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let prepared = plan(
            "MATCH (n:L) RETURN COUNT(*) AS rows,COUNT(DISTINCT n.p) AS unique,SUM(DISTINCT n.q) AS total",
        );
        for pending in [false, true] {
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, backing) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            {
                let mut file = backing.0.lock().unwrap();
                file.pending_write = pending;
                file.write_limit = Some(0);
            }
            {
                let mut future = prepared.spool_aggregate_in_view(
                    &view,
                    &cx,
                    &GqlParameters::new(),
                    policy(),
                    &mut a,
                    &mut b,
                    &mut c,
                    1,
                    1,
                    2,
                    32,
                    257,
                    4096,
                    64,
                    100_000_000,
                    None,
                );
                if pending {
                    assert!(
                        future
                            .as_mut()
                            .poll(&mut Context::from_waker(Waker::noop()))
                            .is_pending()
                    );
                    assert!(
                        pool.used() > 0,
                        "the pending support sort owns the group and sort workspace"
                    );
                } else {
                    assert!(matches!(
                        future.await,
                        Err(NativeAggregateSpoolError::Spool(NativeSpoolError::Spill(_)))
                    ));
                }
            }
            assert_eq!(
                a.stats().published_runs,
                1,
                "source validation and drainage finished before support sorting"
            );
            assert_eq!(b.stats().published_runs, 0);
            assert!(
                b.stats().reserved_runs > 0,
                "failed attempts never refund append quota"
            );
            assert_eq!(c.stats().published_runs, 0);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn output_distinct_spills_visible_classes_and_preserves_the_first_ranked_representation() {
    let ((), report) = run_async_under_lab(0x5ba1_000c, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for (case, text) in [
            "MATCH (n:L) RETURN DISTINCT COUNT(*) AS rows GROUP BY n.p ORDER BY SUM(n.p) DESC",
            "MATCH (n:L) RETURN DISTINCT MIN(n.p)%3 AS category,COUNT(*) AS rows GROUP BY n.p",
            "MATCH (n:L) RETURN DISTINCT MIN(n.p)%3 AS category,COUNT(*) AS rows GROUP BY n.p ORDER BY SUM(n.p) DESC",
            "MATCH (n:L) RETURN DISTINCT MIN(n.p)%3 AS category,COUNT(*) AS rows GROUP BY n.p ORDER BY SUM(n.p) DESC SKIP 1 LIMIT 1",
            "MATCH (n:L) RETURN DISTINCT CASE WHEN n.p%2=0 THEN COUNT(*) ELSE COUNT(*)+0 END AS rows GROUP BY n.p ORDER BY n.p DESC",
            "MATCH (n:L) RETURN DISTINCT CASE WHEN n.p%2=0 THEN COUNT(*) ELSE COUNT(*)+0 END AS rows GROUP BY n.p",
            "MATCH (n:L) RETURN DISTINCT {count:COUNT(*),values:range(0,COUNT(*)-7)} AS value GROUP BY n.p ORDER BY SUM(n.p) DESC",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN DISTINCT COUNT(*) AS rows GROUP BY n.p HAVING n.p>1 ORDER BY SUM(n.p) DESC",
            "MATCH (a)-[e:R]->(b) RETURN DISTINCT COUNT(*) AS rows GROUP BY e.p ORDER BY SUM(e.q) DESC",
            "MATCH (a)-[e:R]-(b) RETURN DISTINCT COUNT(*) AS rows GROUP BY b.p ORDER BY SUM(e.q) DESC SKIP 1 LIMIT 1",
            "MATCH (n:L) WHERE n.p<0 RETURN DISTINCT COUNT(*)+1 AS rows",
            "MATCH (n:L) WHERE n.p<0 RETURN DISTINCT COUNT(*) AS rows GROUP BY n.p",
        ].into_iter().enumerate() {
            let prepared = plan(text);
            let parameters = GqlParameters::new();
            let ordinary = prepared
                .stream_aggregate_in_view(&view, &cx, &parameters, policy())
                .unwrap();
            let columns = ordinary.columns().to_vec();
            let slots = ordinary.output_slots().to_vec();
            let expected = ordinary.collect::<Result<Vec<_>, _>>().unwrap();
            match case {
                0 | 7 => assert_eq!(expected[0].values(), &[GraphAggregateValue::Count(8)]),
                1..=3 => {
                    let categories: Vec<_> = expected.iter().map(|row| row.values()[0].clone()).collect();
                    let values: &[i64] = match case {
                        1 => &[0, 1, 2],
                        2 => &[1, 0, 2],
                        _ => &[0],
                    };
                    assert_eq!(categories, values.iter().copied().map(|value| GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)))).collect::<Vec<_>>());
                }
                4 => assert_eq!(expected[0].values(), &[GraphAggregateValue::Integer(8)]),
                5 => assert_eq!(expected[0].values(), &[GraphAggregateValue::Count(8)]),
                10 => assert_eq!(expected[0].values(), &[GraphAggregateValue::Integer(1)]),
                11 => assert!(expected.is_empty()),
                _ => {}
            }
            for (capacity, run_rows) in [(1, 1), (3, 3)] {
                let pool = MemoryPool::new(4_000_000, 0).unwrap();
                let (mut a, _) = aggregate_scratch(&cx, &pool).await;
                let (mut b, _) = aggregate_scratch(&cx, &pool).await;
                let (mut c, _) = aggregate_scratch(&cx, &pool).await;
                let (spool, _) = prepared.spool_aggregate_in_view(
                    &view, &cx, &parameters,
                    GqlQueryPolicy::new(10_000, expected.len() as u64, 100_000_000, 1_000_000),
                    &mut a, &mut b, &mut c, capacity, 256, run_rows, 128, 257, 16_384, 1000, 100_000_000, None,
                ).await.unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!(spool.columns(), columns, "{text}");
                assert_eq!(spool.output_slots(), slots, "{text}");
                assert_eq!(spool.row_count(), expected.len() as u64, "{text}");
                assert_eq!(aggregate_contents(&spool, &mut c, &cx).await, expected, "{text}");
                assert_eq!(pool.used(), 0, "{text}");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_output_errors_and_projection_admission_do_not_depend_on_the_page() {
    let ((), report) = run_async_under_lab(0x5ba1_000d, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let mut baseline = None;
        for window in ["", " LIMIT 0", " LIMIT 1", " SKIP 1000 LIMIT 1"] {
            let pool = MemoryPool::new(4_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            let text = format!(
                "MATCH (n:L) RETURN DISTINCT range(0,COUNT(*)-7) AS value GROUP BY n.p ORDER BY SUM(n.p) DESC{window}"
            );
            let (spool, _) = plan(&text)
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
                    1,
                    128,
                    257,
                    16_384,
                    1000,
                    100_000_000,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(
                spool.row_count(),
                u64::from(window.is_empty() || window == " LIMIT 1")
            );
            let entries = spool.evaluator_stats().scratch_entries;
            if let Some(expected) = baseline {
                assert_eq!(
                    entries, expected,
                    "all qualified outputs run once before DISTINCT/window"
                );
            } else {
                baseline = Some(entries);
            }
            assert_eq!(pool.used(), 0);

            let text = format!(
                "MATCH (n:L) RETURN DISTINCT 10/(SUM(n.p)-56) AS value GROUP BY n.p ORDER BY n.p{window}"
            );
            let result = plan(&text)
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
                    1,
                    128,
                    257,
                    4096,
                    1000,
                    100_000_000,
                    None,
                )
                .await;
            assert!(
                matches!(result, Err(NativeAggregateSpoolError::Execute(error))
                if matches!(*error, GqlQueryError::Source(fgdb_gql::GraphAggregateError::OutputExpression { error, .. })
                    if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)),
                "{text}"
            );
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_partitioning_comparison_and_ranking_share_exact_cumulative_budgets() {
    let ((), report) = run_async_under_lab(0x5ba1_000e, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let prepared = plan(
            "MATCH (n:L) RETURN DISTINCT MIN(n.p)%3 AS category,COUNT(*) AS rows GROUP BY n.p ORDER BY SUM(n.p) DESC SKIP 1 LIMIT 2",
        );
        let mut budget = policy();
        let mut spill_work = 100_000_000;
        let mut baseline = None;
        for case in 0..7 {
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            let result = prepared
                .spool_aggregate_in_view(
                    &view,
                    &cx,
                    &GqlParameters::new(),
                    budget,
                    &mut a,
                    &mut b,
                    &mut c,
                    1,
                    256,
                    1,
                    128,
                    257,
                    4096,
                    64,
                    spill_work,
                    None,
                )
                .await;
            if case < 2 {
                let (spool, used) = result.unwrap();
                assert_eq!(spool.row_count(), 2);
                let rows = aggregate_contents(&spool, &mut c, &cx).await;
                assert_eq!(
                    rows[0].values()[0],
                    GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(0)))
                );
                assert_eq!(
                    rows[1].values()[0],
                    GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(2)))
                );
                if let Some((expected, _, _, original_work)) = &baseline {
                    assert_eq!(&rows, expected);
                    assert_eq!(used, *original_work);
                } else {
                    baseline = Some((rows, spool.row_stats(), spool.evaluator_stats(), used));
                }
            } else {
                assert!(
                    result.is_err(),
                    "one-less allowance {case} must refuse the whole DISTINCT result"
                );
            }
            assert_eq!(pool.used(), 0);
            let (_, rows, evaluator, used) = baseline.as_ref().unwrap();
            budget = GqlQueryPolicy::new(
                rows.snapshot_records - u64::from(case == 1),
                rows.result_rows - u64::from(case == 2),
                evaluator.work_units - u64::from(case == 3),
                evaluator.scratch_entries - u64::from(case == 4),
            );
            spill_work = *used - u64::from(case == 5);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_distinct_sort_write_failure_after_projection_releases_all_memory_without_a_result() {
    let ((), report) = run_async_under_lab(0x5ba1_000f, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let (mut a, file) = aggregate_scratch(&cx, &pool).await;
        let (mut b, _) = aggregate_scratch(&cx, &pool).await;
        let (mut c, _) = aggregate_scratch(&cx, &pool).await;
        // These two queries share the complete input, reduction, canonical
        // order and projection stages. The non-DISTINCT run then windows
        // directly from b, so a's exact end is the next DISTINCT write boundary.
        let (ordinary, _) = plan("MATCH (n:L) RETURN COUNT(*)+0 AS rows GROUP BY n.p")
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
                1,
                128,
                257,
                4096,
                64,
                100_000_000,
                None,
            )
            .await
            .unwrap();
        assert_eq!(ordinary.row_count(), 8);
        let before_distinct = file.0.lock().unwrap().bytes.get_ref().len();
        let projected_runs = b.stats().published_runs;
        assert_eq!(pool.used(), 0);

        let (mut a, file) = aggregate_scratch(&cx, &pool).await;
        let (mut b, _) = aggregate_scratch(&cx, &pool).await;
        let (mut c, _) = aggregate_scratch(&cx, &pool).await;
        {
            let mut file = file.0.lock().unwrap();
            file.write_limit = Some(
                before_distinct
                    .checked_sub(file.bytes.get_ref().len())
                    .unwrap(),
            );
        }
        let result = plan("MATCH (n:L) RETURN DISTINCT COUNT(*)+0 AS rows GROUP BY n.p")
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
                1,
                128,
                257,
                4096,
                64,
                100_000_000,
                None,
            )
            .await;
        assert!(matches!(
            result,
            Err(NativeAggregateSpoolError::Spool(NativeSpoolError::Spill(_)))
        ));
        assert_eq!(
            b.stats().published_runs,
            projected_runs,
            "all projected groups precede the injected equality-sort write failure"
        );
        assert_eq!(
            file.0.lock().unwrap().bytes.get_ref().len(),
            before_distinct
        );
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_vertex_and_edge_inputs_partition_the_native_projected_schema() {
    let ((), report) = run_async_under_lab(0x5ba1_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for delta in [
            CanonicalScalar::Int(1),
            CanonicalScalar::Float(fgdb_types::CanonicalF64::new(0.25)),
        ] {
            let params = GqlParameters::new()
                .with_map(
                    "weights",
                    vec![("delta".into(), GraphValue::Scalar(delta.clone()))],
                )
                .unwrap();
            for (case, text) in [
                "MATCH (n:L) RETURN n.p%3 AS category,COUNT(*) AS rows,SUM(n.p*3+$weights.delta) AS total,AVG(n.p+$weights.delta) AS average GROUP BY n.p%3 ORDER BY category",
                "MATCH (n:L) RETURN COUNT(*) AS rows GROUP BY n.p%3 HAVING SUM(n.p+$weights.delta)>0 ORDER BY AVG(n.p+$weights.delta) DESC SKIP 1 LIMIT 1",
                "MATCH (a)-[e:R]->(b) RETURN e.p%3 AS category,SUM(e.q*$weights.delta) AS total,AVG(b.p+$weights.delta) AS average GROUP BY e.p%3 ORDER BY total DESC",
                "MATCH (a)-[e:R]-(b) RETURN a.p%3 AS category,COUNT(*) AS rows,SUM(b.p+$weights.delta) AS total GROUP BY a.p%3 ORDER BY category",
            ].into_iter().enumerate() {
                let prepared = PreparedNativeRead::prepare(text, &params, resolve).unwrap();
                let ordinary = prepared.stream_aggregate_in_view(&view, &cx, &params, policy()).unwrap();
                let columns = ordinary.columns().to_vec();
                let expected = ordinary.collect::<Result<Vec<_>, _>>().unwrap();
                if case == 0 && delta == CanonicalScalar::Int(1) {
                    assert_eq!(expected.iter().map(|row| row.values()[0].clone()).collect::<Vec<_>>(), vec![GraphAggregateValue::Count(24), GraphAggregateValue::Count(24), GraphAggregateValue::Count(16)]);
                    assert_eq!(expected.iter().map(|row| row.values()[1].clone()).collect::<Vec<_>>(), vec![GraphAggregateValue::Integer(240), GraphAggregateValue::Integer(312), GraphAggregateValue::Integer(184)]);
                }
                let pool = MemoryPool::new(4_000_000, 0).unwrap();
                let (mut a, _) = aggregate_scratch(&cx, &pool).await;
                let (mut b, _) = aggregate_scratch(&cx, &pool).await;
                let (mut c, _) = aggregate_scratch(&cx, &pool).await;
                let (spool, _) = prepared.spool_aggregate_in_view(
                    &view, &cx, &params,
                    GqlQueryPolicy::new(10_000, expected.len() as u64, 100_000_000, 1_000_000),
                    &mut a, &mut b, &mut c, 1, 256, 2, 128, 257, 4096, 1000, 100_000_000, None,
                ).await.unwrap();
                assert!(b.stats().published_runs > 0, "{text}");
                assert_eq!(spool.columns(), columns);
                assert_eq!(spool.row_count(), expected.len() as u64);
                assert_eq!(aggregate_contents(&spool, &mut c, &cx).await, expected, "{text}");
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_input_failure_cannot_hide_behind_an_empty_or_full_result_page() {
    let ((), report) = run_async_under_lab(0x5ba1_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for text in [
            "MATCH (n:L) RETURN n.p AS category,SUM(10/(n.p-7)) AS total GROUP BY n.p ORDER BY category LIMIT 0",
            "MATCH (n:L) RETURN n.p AS category,SUM(10/(n.p-7)) AS total GROUP BY n.p ORDER BY category LIMIT 1",
            "MATCH (a)-[e:R]->(b) RETURN e.p AS category,SUM(10/(e.p-7)) AS total GROUP BY e.p LIMIT 0",
        ] {
            let pool = MemoryPool::new(4_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            let result = plan(text)
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
                .await;
            assert!(
                matches!(result, Err(NativeAggregateSpoolError::Execute(error))
                if matches!(*error, GqlQueryError::Source(fgdb_gql::GraphAggregateError::InputExpression { row: 0, error, .. })
                    if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)),
                "{text}"
            );
            assert_eq!(c.stats().published_runs, 0);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_outputs_keep_exact_cells_hidden_rank_and_native_collection_expressions() {
    let ((), report) = run_async_under_lab(0x5ba1_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for (case, text) in [
            "MATCH (n:L) RETURN n.p AS category,COUNT(*)+1 AS rows,SUM(n.p)*2 AS total,AVG(n.p) AS average GROUP BY n.p ORDER BY category",
            "MATCH (n:L) RETURN SUM(n.q)*2 AS total,AVG(n.p) AS average,toString(COUNT(*)) AS count",
            "MATCH (n:L) RETURN {category:n.p,rows:COUNT(*),sample:range(0,COUNT(*)-6)} AS value GROUP BY n.p ORDER BY SUM(n.p) DESC SKIP 1 LIMIT 3",
            "MATCH (n:L) RETURN n.p%3 AS category,SUM(n.p*2)+COUNT(*) AS total GROUP BY n.p%3 HAVING SUM(n.p)>0 ORDER BY AVG(n.p) DESC LIMIT 2",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN CASE WHEN COUNT(*)>0 THEN SUM(n.p)*2 ELSE 0 END AS total GROUP BY n.p ORDER BY SUM(n.p) DESC LIMIT 3",
            "MATCH (a)-[e:R]->(b) RETURN e.p AS category,SUM(e.q)*2 AS total,upper(toString(COUNT(*))) AS count GROUP BY e.p ORDER BY AVG(e.q) DESC SKIP 1 LIMIT 3",
            "MATCH (a)-[e:R]-(b) RETURN {category:b.p,rows:COUNT(*)} AS value GROUP BY b.p ORDER BY SUM(e.q) DESC LIMIT 3",
            "MATCH (n:L) WHERE n.p<0 RETURN COUNT(*)+1 AS count,COALESCE(SUM(n.p),42) AS total",
        ].into_iter().enumerate() {
            let prepared = plan(text);
            let parameters = GqlParameters::new();
            let ordinary = prepared
                .stream_aggregate_in_view(&view, &cx, &parameters, policy())
                .unwrap();
            let columns = ordinary.columns().to_vec();
            let slots = ordinary.output_slots().to_vec();
            let expected = ordinary.collect::<Result<Vec<_>, _>>().unwrap();
            if case == 0 {
                assert_eq!(expected.len(), 8);
                for (category, row) in expected.iter().enumerate() {
                    assert!(row.keys().is_empty());
                    assert_eq!(row.values()[0], GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(category as i64))));
                    assert_eq!(row.values()[1], GraphAggregateValue::Integer(9));
                    assert_eq!(row.values()[2], GraphAggregateValue::Integer(16 * category as i128));
                }
            } else if case == 1 {
                let total: i128 = (0..64_i128)
                    .filter(|id| id % 5 != 0)
                    .map(|id| i128::from(i64::MAX) - id)
                    .sum();
                assert_eq!(expected.len(), 1);
                assert_eq!(expected[0].values()[0], GraphAggregateValue::Integer(total * 2));
                assert_eq!(expected[0].values()[1], GraphAggregateValue::Average(fgdb_gql::GraphExactAverage::new(7, 2).unwrap()));
                assert_eq!(expected[0].values()[2], GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::ucs_basic_text("64").unwrap())));
            }
            for capacity in [1, 3] {
                let pool = MemoryPool::new(4_000_000, 0).unwrap();
                let (mut a, _) = aggregate_scratch(&cx, &pool).await;
                let (mut b, _) = aggregate_scratch(&cx, &pool).await;
                let (mut c, _) = aggregate_scratch(&cx, &pool).await;
                let (spool, _) = prepared.spool_aggregate_in_view(
                    &view, &cx, &parameters,
                    GqlQueryPolicy::new(10_000, expected.len() as u64, 100_000_000, 1_000_000),
                    &mut a, &mut b, &mut c, capacity, 256, 2, 128, 257, 16_384, 1000, 100_000_000, None,
                ).await.unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!(spool.columns(), columns, "{text}");
                assert_eq!(spool.output_slots(), slots, "{text}");
                assert_eq!(spool.row_count(), expected.len() as u64, "{text}");
                assert_eq!(aggregate_contents(&spool, &mut c, &cx).await, expected, "{text}");
                assert_eq!(pool.used(), 0, "{text}");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_output_errors_precede_every_page_and_having_controls_evaluation() {
    let ((), report) = run_async_under_lab(0x5ba1_000a, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for text in [
            "MATCH (n:L) RETURN n.p AS category,10/(SUM(n.p)-56) AS value GROUP BY n.p ORDER BY n.p LIMIT 0",
            "MATCH (n:L) RETURN n.p AS category,10/(SUM(n.p)-56) AS value GROUP BY n.p ORDER BY n.p LIMIT 1",
            "MATCH (n:L) RETURN n.p AS category,10/(SUM(n.p)-56) AS value GROUP BY n.p ORDER BY n.p DESC SKIP 1000 LIMIT 1",
            "MATCH (a)-[e:R]->(b) RETURN e.p AS category,10/(SUM(e.p)-56) AS value GROUP BY e.p LIMIT 0",
            "MATCH (n:L) WHERE n.p<0 RETURN 1/COUNT(*) AS value LIMIT 0",
        ] {
            let pool = MemoryPool::new(4_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            let result = plan(text)
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
                .await;
            assert!(
                matches!(result, Err(NativeAggregateSpoolError::Execute(error))
                if matches!(*error, GqlQueryError::Source(fgdb_gql::GraphAggregateError::OutputExpression { error, .. })
                    if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)),
                "{text}"
            );
            assert_eq!(pool.used(), 0, "{text}");
        }
        let text = "MATCH (n:L) RETURN 10/(SUM(n.p)-56) AS value GROUP BY n.p HAVING n.p<7 ORDER BY n.p LIMIT 1";
        let pool = MemoryPool::new(4_000_000, 0).unwrap();
        let (mut a, _) = aggregate_scratch(&cx, &pool).await;
        let (mut b, _) = aggregate_scratch(&cx, &pool).await;
        let (mut c, _) = aggregate_scratch(&cx, &pool).await;
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
        assert_eq!(spool.row_count(), 1);
        assert_eq!(
            aggregate_contents(&spool, &mut c, &cx).await[0].values(),
            &[GraphAggregateValue::Integer(0)]
        );
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_projection_runs_once_per_qualified_group_and_reserves_expansion_before_delivery() {
    let ((), report) = run_async_under_lab(0x5ba1_000b, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let mut scratch_entries = None;
        for limit in [0, 1, 8] {
            let text = format!(
                "MATCH (n:L) RETURN {{category:n.p,rows:COUNT(*)+1}} AS value GROUP BY n.p ORDER BY n.p LIMIT {limit}"
            );
            let pool = MemoryPool::new(4_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            let (spool, _) = plan(&text)
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
            assert_eq!(spool.row_count(), limit);
            let entries = spool.evaluator_stats().scratch_entries;
            if let Some(expected) = scratch_entries {
                assert_eq!(
                    entries, expected,
                    "projection must neither stop early nor rerun selected groups"
                );
            } else {
                scratch_entries = Some(entries);
            }
            assert_eq!(pool.used(), 0);
        }
        // The input is only 64 small rows and the reduction has one group.
        // Expansion is proportional to its output, not its source frame size.
        let text = "MATCH (n:L) RETURN range(0,COUNT(*)*100) AS values LIMIT 0";
        let pool = MemoryPool::new(131_072, 0).unwrap();
        let (mut a, _) = aggregate_scratch(&cx, &pool).await;
        let (mut b, _) = aggregate_scratch(&cx, &pool).await;
        let (mut c, _) = aggregate_scratch(&cx, &pool).await;
        let result = plan(text)
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
                1_000_000,
                1000,
                100_000_000,
                None,
            )
            .await;
        assert!(matches!(
            result,
            Err(NativeAggregateSpoolError::Spool(NativeSpoolError::Spill(
                SpillError::Memory(
                    fgdb_strata::tiered::memory::MemoryError::ResourceExhausted { .. }
                )
            )))
        ));
        assert!(
            c.stats().published_runs > 0,
            "the complete numeric reduction must precede the output-expansion refusal"
        );
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
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
                "MATCH (n:L) RETURN n.p AS category, COLLECT(n.q) AS rows LIMIT 0"
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
fn completed_group_clauses_match_native_results_after_forced_partitioning() {
    let ((), report) = run_async_under_lab(0x5ba1_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = aggregate_seed(&contexts.commit()).await;
        let mut changes = WriteBatch::new(RelationId(1));
        for (id, key, value) in [
            (1000, -1, Some(i64::MIN)),
            (1001, -1, Some(i64::MIN)),
            (1002, 8, None),
        ] {
            let mut properties = vec![(PropertyKeyId(1), CanonicalScalar::Int(key))];
            if let Some(value) = value {
                properties.push((PropertyKeyId(2), CanonicalScalar::Int(value)));
            }
            changes.create_vertex(VId(id), vec![LabelId(1)], properties);
        }
        changes.set_vertex_property(VId(63), PropertyKeyId(1), None);
        db.write(&contexts.commit(), changes).await.unwrap();
        let view = db.read_session().unwrap();
        for text in [
            "MATCH (n:L) RETURN n.p AS category, COUNT(*) AS rows, SUM(n.q) AS total, AVG(n.q) AS average, MIN(n.q) AS minimum, MAX(n.q) AS maximum ORDER BY average DESC NULLS FIRST,rows DESC SKIP 1 LIMIT 4",
            "MATCH (n:L) RETURN COUNT(*) AS rows GROUP BY n.p ORDER BY SUM(n.q) ASC NULLS LAST SKIP 1 LIMIT 3",
            "MATCH (n:L) RETURN n.p AS first,n.p AS again,COUNT(*) AS rows GROUP BY n.p ORDER BY rows DESC",
            "MATCH (n:L) RETURN n.p AS category,COUNT(*) AS rows GROUP BY n.p HAVING rows >= 2 AND (AVG(n.q) > 0 OR AVG(n.q) IS NULL) ORDER BY AVG(n.q) DESC LIMIT 4",
            "MATCH (n:L) RETURN n.p AS category,COUNT(*) AS rows HAVING rows > 1000 ORDER BY rows LIMIT 1",
            "MATCH (n:L) RETURN COUNT(*) AS rows HAVING rows >= 0 ORDER BY rows LIMIT 0",
            "MATCH (n:L) WHERE n.p < -100 RETURN COUNT(*) AS rows,SUM(n.q) AS total HAVING rows = 0 ORDER BY rows LIMIT 1",
            "MATCH (n:L) RETURN n.p AS category,COUNT(*) AS rows SKIP 18446744073709551615 LIMIT 1",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS rows GROUP BY n.p ORDER BY AVG(n.q) DESC LIMIT 2",
            "MATCH (a)-[e:R]->(b) RETURN e.p AS category,COUNT(*) AS rows GROUP BY e.p HAVING rows > 0 ORDER BY AVG(e.q) ASC NULLS LAST SKIP 1 LIMIT 3",
            "MATCH (a)-[e:R]-(b) RETURN COUNT(*) AS rows GROUP BY b.p ORDER BY SUM(e.q) DESC LIMIT 3",
        ] {
            let prepared = plan(text);
            let parameters = GqlParameters::new();
            let ordinary = prepared
                .stream_aggregate_in_view(&view, &cx, &parameters, policy())
                .unwrap();
            let columns = ordinary.columns().to_vec();
            let slots = ordinary.output_slots().to_vec();
            let seq = ordinary.snapshot_seq();
            let expected = ordinary.collect::<Result<Vec<_>, _>>().unwrap();
            for capacity in [1, 3] {
                let pool = MemoryPool::new(1_000_000, 0).unwrap();
                let (mut a, _) = aggregate_scratch(&cx, &pool).await;
                let (mut b, _) = aggregate_scratch(&cx, &pool).await;
                let (mut c, _) = aggregate_scratch(&cx, &pool).await;
                let selected_only =
                    GqlQueryPolicy::new(10_000, expected.len() as u64, 100_000_000, 1_000_000);
                let (spool, _) = prepared
                    .spool_aggregate_in_view(
                        &view,
                        &cx,
                        &parameters,
                        selected_only,
                        &mut a,
                        &mut b,
                        &mut c,
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
                    .unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!(spool.columns(), columns, "{text}");
                assert_eq!(spool.output_slots(), slots, "{text}");
                assert_eq!(spool.snapshot_seq(), seq, "{text}");
                assert_eq!(spool.row_count(), expected.len() as u64, "{text}");
                assert_eq!(
                    aggregate_contents(&spool, &mut c, &cx).await,
                    expected,
                    "{text}"
                );
                assert_eq!(pool.used(), 0, "{text}");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn completed_group_sort_and_window_keep_every_cumulative_allowance() {
    let ((), report) = run_async_under_lab(0x5ba1_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = aggregate_seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let prepared = plan(
            "MATCH (n:L) RETURN COUNT(*) AS rows,COUNT(DISTINCT n.q) AS unique GROUP BY n.p HAVING rows > 1 ORDER BY AVG(DISTINCT n.q) DESC SKIP 1 LIMIT 2",
        );
        let mut budget = policy();
        let mut spill_work = 100_000_000;
        let mut baseline = None;
        for case in 0..7 {
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            let result = prepared
                .spool_aggregate_in_view(
                    &view,
                    &cx,
                    &GqlParameters::new(),
                    budget,
                    &mut a,
                    &mut b,
                    &mut c,
                    1,
                    256,
                    2,
                    128,
                    257,
                    4096,
                    64,
                    spill_work,
                    None,
                )
                .await;
            if case < 2 {
                let (spool, used) = result.unwrap();
                assert_eq!(spool.row_count(), 2);
                let rows = aggregate_contents(&spool, &mut c, &cx).await;
                if let Some((expected, _, _, original_work)) = &baseline {
                    assert_eq!(&rows, expected);
                    assert_eq!(used, *original_work);
                } else {
                    baseline = Some((rows, spool.row_stats(), spool.evaluator_stats(), used));
                }
            } else {
                assert!(
                    result.is_err(),
                    "allowance {case} must span result sorting/windowing"
                );
            }
            assert_eq!(pool.used(), 0);
            let (_, rows, evaluator, used) = baseline.as_ref().unwrap();
            budget = GqlQueryPolicy::new(
                rows.snapshot_records - u64::from(case == 1),
                rows.result_rows - u64::from(case == 2),
                evaluator.work_units - u64::from(case == 3),
                evaluator.scratch_entries - u64::from(case == 4),
            );
            spill_work = *used - u64::from(case == 5);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn limit_zero_and_full_pages_cannot_hide_a_later_invalid_having_group() {
    let ((), report) = run_async_under_lab(0x5ba1_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = aggregate_seed(&contexts.commit()).await;
        let mut extra = WriteBatch::new(RelationId(1));
        extra.create_vertex(
            VId(1000),
            vec![LabelId(1)],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(100)),
                (PropertyKeyId(2), CanonicalScalar::Bool(true)),
            ],
        );
        db.write(&contexts.commit(), extra).await.unwrap();
        let view = db.read_session().unwrap();
        for window in [" LIMIT 0", " LIMIT 1", " SKIP 1000 LIMIT 1"] {
            let text = format!(
                "MATCH (n:L) RETURN n.p AS category,MIN(n.q) AS minimum HAVING minimum > 0 OR TRUE ORDER BY category{window}"
            );
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let (mut a, _) = aggregate_scratch(&cx, &pool).await;
            let (mut b, _) = aggregate_scratch(&cx, &pool).await;
            let (mut c, _) = aggregate_scratch(&cx, &pool).await;
            let result = plan(&text)
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
                .await;
            assert!(
                matches!(result, Err(NativeAggregateSpoolError::Execute(error))
                if matches!(*error, GqlQueryError::Source(fgdb_gql::GraphAggregateError::NonIntegerHaving { .. }))),
                "{text}"
            );
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
