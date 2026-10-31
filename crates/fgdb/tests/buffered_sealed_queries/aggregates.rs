//! Terminal reducers over the real buffered sealed procedure source.
//! Literal answers independently constrain the resident/external differential.

use super::*;
use fgdb::{NativeAggregateSpoolError, PreparedBufferedSealedAggregate};
use fgdb_gql::{GraphAggregateError, GraphAggregateTextSlot, GraphExactAverage};

#[derive(Clone, Copy)]
struct Limits {
    group_capacity: usize,
    run_rows: usize,
    input_rows: u64,
    work: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            group_capacity: 2,
            run_rows: 1,
            input_rows: 1000,
            work: WORK,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct AggregateAnswer {
    columns: Vec<String>,
    snapshot: CommitSeq,
    rows: Vec<Vec<QueryValue>>,
    records: u64,
    evaluator_work: u64,
    evaluator_scratch: u64,
    physical_work: u64,
}

fn prepare_aggregate(text: &str, params: &GqlParameters) -> PreparedBufferedSealedAggregate {
    PreparedNativeRead::prepare(text, params, resolve)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .prepare_buffered_sealed_aggregate(params, R, memory(), sealing(), RAW)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

async fn execute_aggregate(
    prepared: &PreparedBufferedSealedAggregate,
    cold: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    allowance: GqlQueryPolicy,
    pool: &MemoryPool,
    limits: Limits,
) -> Result<AggregateAnswer, NativeAggregateSpoolError> {
    let (mut source, _) = file(cx, pool).await;
    let (mut partition, _) = file(cx, pool).await;
    let (mut destination, _) = file(cx, pool).await;
    let (spool, physical_work) = prepared
        .spool_in_view(
            cold,
            cx,
            allowance,
            &mut source,
            &mut partition,
            &mut destination,
            limits.group_capacity,
            512,
            limits.run_rows,
            512,
            127,
            32 * 1024,
            limits.input_rows,
            limits.work,
            None,
        )
        .await?;
    let mut reader = spool.reader(&mut destination, None);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await? {
        rows.push(
            spool
                .output_slots()
                .iter()
                .map(|slot| match slot {
                    GraphAggregateTextSlot::GroupKey(index) => {
                        QueryValue::Value(row.keys()[*index].clone())
                    }
                    GraphAggregateTextSlot::Aggregate(index) => row.values()[*index].clone(),
                })
                .collect(),
        );
    }
    assert_eq!(spool.row_count(), rows.len() as u64);
    assert_eq!(spool.row_stats().result_rows, spool.row_count());
    Ok(AggregateAnswer {
        columns: spool.columns().to_vec(),
        snapshot: spool.snapshot_seq(),
        rows,
        records: spool.row_stats().snapshot_records,
        evaluator_work: spool.evaluator_stats().work_units,
        evaluator_scratch: spool.evaluator_stats().scratch_entries,
        physical_work,
    })
}

fn values(values: Vec<GraphValue>) -> QueryValue {
    QueryValue::Value(GraphValue::List(values.into_boxed_slice()))
}

fn assert_resident(
    writer: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    params: &GqlParameters,
    actual: &AggregateAnswer,
) {
    let QueryResult::Rows { columns, rows } =
        writer.query(cx, text, params, resolve, policy()).unwrap()
    else {
        panic!("an aggregate read must return rows")
    };
    assert_eq!(actual.columns, columns, "{text}");
    assert_eq!(actual.rows, rows, "{text}");
}

#[test]
fn exact_numeric_distinct_collection_and_empty_results_survive_terminal_spilling() {
    let ((), report) = run_async_under_lab(0x6273_5201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let retained = cold.sealed_read_cache_pool().clone();
        let cases = [
            (
                format!(
                    "{BFS} YIELD node,distance WHERE distance>0 \
                     RETURN MAX(distance) AS farthest, \
                     SUM(9223372036854775807) AS wide,COUNT(*) AS reached, \
                     AVG(distance) AS mean,MIN(distance) AS nearest"
                ),
                vec![vec![
                    QueryValue::Value(integer(2)),
                    QueryValue::Integer(2 * i128::from(i64::MAX)),
                    QueryValue::Count(2),
                    QueryValue::Average(GraphExactAverage::new(3, 2).unwrap()),
                    QueryValue::Value(integer(1)),
                ]],
            ),
            (
                format!(
                    "{BFS} YIELD node,distance UNWIND [distance,distance,NULL] AS d \
                     WITH d ORDER BY d DESC \
                     RETURN COUNT(*) AS occurrences,COUNT(d) AS nonnull, \
                     COUNT(DISTINCT d) AS unique,SUM(DISTINCT d) AS total, \
                     AVG(DISTINCT d) AS mean,COLLECT(d) AS bag, \
                     COLLECT(DISTINCT d) AS distinct_bag"
                ),
                vec![vec![
                    QueryValue::Count(9),
                    QueryValue::Count(6),
                    QueryValue::Count(3),
                    QueryValue::Integer(3),
                    QueryValue::Average(GraphExactAverage::new(1, 1).unwrap()),
                    values([2, 2, 1, 1, 0, 0].into_iter().map(integer).collect()),
                    values([2, 1, 0].into_iter().map(integer).collect()),
                ]],
            ),
            (
                format!(
                    "{BFS} YIELD node,distance WHERE distance<0 \
                     RETURN COUNT(*) AS reached,COLLECT(node) AS nodes"
                ),
                vec![vec![QueryValue::Count(0), values(Vec::new())]],
            ),
            (
                format!(
                    "{WCC} YIELD node,component RETURN DISTINCT COUNT(*) AS size \
                     GROUP BY component ORDER BY size DESC"
                ),
                vec![vec![QueryValue::Count(3)], vec![QueryValue::Count(1)]],
            ),
        ];
        for (text, expected) in cases {
            let params = if text.contains("$start") {
                parameters()
            } else {
                GqlParameters::new()
            };
            let prepared = prepare_aggregate(&text, &params);
            for (group_capacity, run_rows) in [(1, 1), (2, 3)] {
                let actual = execute_aggregate(
                    &prepared,
                    &mut cold,
                    &cx,
                    policy(),
                    &spill_pool,
                    Limits {
                        group_capacity,
                        run_rows,
                        ..Limits::default()
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                assert_eq!(actual.snapshot, CommitSeq(2));
                assert_eq!(actual.rows, expected, "{text}");
                assert_resident(&writer, &cx, &text, &params, &actual);
                assert_eq!(spill_pool.used(), 0);
            }
        }
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn component_summary(p: i64, members: &[u128]) -> Vec<QueryValue> {
    vec![
        QueryValue::Count(members.len() as u64),
        QueryValue::Value(vertex(1)),
        QueryValue::Value(integer(p)),
        values(members.iter().copied().map(vertex).collect()),
    ]
}

fn singleton_summary(id: u128, p: i64) -> Vec<QueryValue> {
    vec![
        QueryValue::Count(1),
        QueryValue::Value(vertex(id)),
        QueryValue::Value(integer(p)),
        values(vec![vertex(id)]),
    ]
}

#[test]
fn component_summaries_preserve_group_keys_properties_order_and_historical_cuts() {
    let ((), report) = run_async_under_lab(0x6273_5202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut writer) = fixture(&commit).await;
        let params = GqlParameters::new();
        let old_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let current_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut old = view(&commit, &vfs, &old_pool).await;
        let text = format!(
            "{WCC} YIELD node,component MATCH (component:Person) \
             WITH component,component.p AS p,node ORDER BY node DESC \
             RETURN COUNT(*) AS size,component,p,COLLECT(node) AS members \
             ORDER BY size DESC,component"
        );
        let earlier = vec![
            component_summary(10, &[THIRD, SECOND, 1]),
            singleton_summary(FOURTH, 40),
            singleton_summary(u128::MAX, 50),
        ];
        let prepared = prepare_aggregate(&text, &params);
        let limits = Limits {
            group_capacity: 1,
            ..Limits::default()
        };
        let first = execute_aggregate(&prepared, &mut old, &cx, policy(), &spill_pool, limits)
            .await
            .unwrap();
        assert_eq!(first.rows, earlier);
        assert_resident(&writer, &cx, &text, &params, &first);

        let mut change = WriteBatch::new(R);
        change.add_edge(EId(BASE + 8), VId(FOURTH), VId(THIRD), vec![]);
        change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(100)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(3));
        writer.compact(&commit).await.unwrap();
        let mut current = view(&commit, &vfs, &current_pool).await;
        let later = vec![
            component_summary(100, &[FOURTH, THIRD, SECOND, 1]),
            singleton_summary(u128::MAX, 50),
        ];
        let latest = execute_aggregate(&prepared, &mut current, &cx, policy(), &spill_pool, limits)
            .await
            .unwrap();
        assert_eq!(latest.snapshot, CommitSeq(3));
        assert_eq!(latest.rows, later);
        assert_resident(&writer, &cx, &text, &params, &latest);

        let owner_cut = prepared.clone().with_as_of(CommitSeq(2)).unwrap();
        assert!(owner_cut.clone().with_as_of(CommitSeq(3)).is_err());
        let owner_selected =
            execute_aggregate(&owner_cut, &mut current, &cx, policy(), &spill_pool, limits)
                .await
                .unwrap();
        assert_eq!(owner_selected.snapshot, CommitSeq(2));
        assert_eq!(owner_selected.rows, earlier);

        let pinned = execute_aggregate(&prepared, &mut old, &cx, policy(), &spill_pool, limits)
            .await
            .unwrap();
        assert_eq!(pinned.snapshot, CommitSeq(2));
        assert_eq!(pinned.rows, earlier);
        assert_eq!(spill_pool.used(), 0);
        drop(old);
        drop(current);
        assert_eq!(old_pool.used(), 0);
        assert_eq!(current_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cached_summaries_replay_usage_and_separate_private_input_from_final_result_limits() {
    let ((), report) = run_async_under_lab(0x6273_5203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = fixture(&commit).await;
        let params = parameters();
        let text = format!("{BFS} YIELD node,distance RETURN COUNT(*) AS reached");
        let prepared = prepare_aggregate(&text, &params);
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let retained = cold.sealed_read_cache_pool().clone();
        let one_result = GqlQueryPolicy::new(10_000, 1, WORK, WORK);
        let first = execute_aggregate(
            &prepared,
            &mut cold,
            &cx,
            one_result,
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(first.rows, vec![vec![QueryValue::Count(3)]]);
        assert!(first.records > 3 && first.physical_work > 0);
        let faults = cold.buffer_stats();
        let baseline = source_pool.used();
        let retained_bytes = retained.used();
        assert!(retained_bytes > 0);
        let warm = execute_aggregate(
            &prepared,
            &mut cold,
            &cx,
            one_result,
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(warm, first);
        assert_eq!(cold.buffer_stats(), faults);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(retained.used(), retained_bytes);

        let no_results = GqlQueryPolicy::new(10_000, 0, WORK, WORK);
        let error = execute_aggregate(
            &prepared,
            &mut cold,
            &cx,
            no_results,
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeAggregateSpoolError::Execute(inner)
                if matches!(inner.as_ref(), GqlQueryError::Rows(refusal)
                    if refusal.dimension == GqlBudgetDimension::ResultRows
                        && refusal.limit == 0 && refusal.observed == 1)),
            "{error:?}",
        );
        assert_eq!(spill_pool.used(), 0);

        let error = execute_aggregate(
            &prepared,
            &mut cold,
            &cx,
            one_result,
            &spill_pool,
            Limits {
                input_rows: 2,
                ..Limits::default()
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error,
                NativeAggregateSpoolError::Spool(NativeSpoolError::Procedure(inner))
                if matches!(inner.as_ref(), GqlQueryError::Rows(refusal)
                    if refusal.dimension == GqlBudgetDimension::ResultRows
                        && refusal.limit == 2 && refusal.observed == 3)),
            "{error:?}",
        );
        assert_eq!(spill_pool.used(), 0);

        let limited_work = execute_aggregate(
            &prepared,
            &mut cold,
            &cx,
            one_result,
            &spill_pool,
            Limits {
                work: first.physical_work - 1,
                ..Limits::default()
            },
        )
        .await;
        assert!(
            limited_work.is_err(),
            "source and reducer work must not reset"
        );
        assert_eq!(spill_pool.used(), 0);

        let suppressed = prepare_aggregate(&format!("{text} LIMIT 0"), &params);
        let none = execute_aggregate(
            &suppressed,
            &mut cold,
            &cx,
            no_results,
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap();
        assert!(none.rows.is_empty());
        assert_eq!(none.records, first.records);
        assert_eq!(spill_pool.used(), 0);

        let again = execute_aggregate(
            &prepared,
            &mut cold,
            &cx,
            one_result,
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(again, first);
        assert_eq!(source_pool.used(), baseline);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn terminal_limit_zero_preserves_late_input_and_completed_group_errors() {
    let ((), report) = run_async_under_lab(0x6273_5204, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = fixture(&commit).await;
        let params = parameters();
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let valid = prepare_aggregate(
            &format!("{BFS} YIELD node,distance RETURN COUNT(*) AS reached"),
            &params,
        );
        let expected = execute_aggregate(
            &valid,
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap();
        let baseline = source_pool.used();
        let input = prepare_aggregate(
            &format!(
                "{BFS} YIELD node,distance \
                 RETURN SUM(1/(2-distance)) AS broken LIMIT 0"
            ),
            &params,
        );
        let error = execute_aggregate(
            &input,
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeAggregateSpoolError::Spool(
                NativeSpoolError::SetExecution(GraphSetExecutionError::Projection {
                    error, ..
                })
            ) if error.kind == GraphIntegerErrorKind::DivisionByZero),
            "{error:?}",
        );
        assert_eq!(spill_pool.used(), 0);
        assert_eq!(source_pool.used(), baseline);

        let output = prepare_aggregate(
            &format!(
                "{BFS} YIELD node,distance \
                 RETURN 1/(SUM(distance)-3) AS broken LIMIT 0"
            ),
            &params,
        );
        let error = execute_aggregate(
            &output,
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeAggregateSpoolError::Execute(inner)
                if matches!(inner.as_ref(), GqlQueryError::Source(
                    GraphAggregateError::OutputExpression { error, .. }
                ) if error.kind == GraphIntegerErrorKind::DivisionByZero)),
            "{error:?}",
        );
        assert_eq!(spill_pool.used(), 0);
        assert_eq!(source_pool.used(), baseline);
        let retry = execute_aggregate(
            &valid,
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(retry, expected);
        assert_eq!(spill_pool.used(), 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_terminal_aggregates_pin_both_procedures_and_properties_before_spilling() {
    let ((), report) = run_async_under_lab(0x6273_5205, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let text = format!(
            "{BFS} YIELD node,distance UNWIND [1,2] AS factor \
             {WCC} YIELD node AS member,component \
             FOR SYSTEM_TIME AS OF SEQ $at \
             WITH node,distance,factor,member,component WHERE node=member \
             MATCH (node:Person) \
             RETURN COUNT(*) AS reached,SUM(node.p*factor) AS weighted, \
             SUM(distance) AS hops,SUM(9223372036854775807) AS wide, \
             COUNT(DISTINCT component) AS groups"
        );
        let parameters_at = |at| parameters().with_uint64("at", at).unwrap();
        let initial_params = parameters_at(2);
        let native = PreparedNativeRead::prepare(&text, &initial_params, resolve).unwrap();
        assert!(matches!(&native, PreparedNativeRead::TemporalAggregate(_)));
        assert!(
            native.prepare_buffered_aggregate(&initial_params).is_err(),
            "ordinary buffered aggregation must not acquire a procedure host"
        );
        let historical = native
            .prepare_buffered_sealed_aggregate(&initial_params, R, memory(), sealing(), RAW)
            .unwrap();
        assert!(historical.clone().with_as_of(CommitSeq(3)).is_err());

        let mut change = WriteBatch::new(R);
        change.add_edge(EId(BASE + 8), VId(THIRD), VId(FOURTH), vec![]);
        change.set_vertex_property(VId(THIRD), P, Some(CanonicalScalar::Int(300)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(3));
        writer.compact(&commit).await.unwrap();
        let mut current = view(&commit, &vfs, &source_pool).await;
        let retained = current.sealed_read_cache_pool().clone();
        let expected = |count, weighted, hops| {
            vec![vec![
                QueryValue::Count(count),
                QueryValue::Integer(weighted),
                QueryValue::Integer(hops),
                QueryValue::Integer(i128::from(count) * i128::from(i64::MAX)),
                QueryValue::Count(1),
            ]]
        };
        let resident = writer
            .prepare_sealed_read(
                &cx,
                Some(CommitSeq(2)),
                R,
                sealing(),
                fgdb_strata::store::RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            resident
                .execute_prepared(&cx, &native, &initial_params, policy(), memory())
                .unwrap(),
            QueryResult::Rows {
                columns: ["reached", "weighted", "hops", "wide", "groups"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect(),
                rows: expected(6, 180, 6),
            }
        );
        assert!(
            resident
                .execute_prepared(
                    &cx,
                    &native,
                    &initial_params,
                    policy(),
                    FnxMemoryLimits {
                        max_kernel_workspace_bytes: 0,
                        ..memory()
                    },
                )
                .is_err(),
            "temporal CALLs must retain the sealed kernel's memory admission"
        );
        assert!(
            matches!(
                resident.execute_prepared(
                    &cx, &native, &parameters_at(3), policy(), memory(),
                ),
                Err(fgdb::SealedQueryError::SnapshotMismatch {
                    requested: CommitSeq(3), retained: CommitSeq(2),
                })
            ),
            "a temporal binding cannot replace the resident sealed owner's cut"
        );

        for (at, count, weighted, hops) in
            [(2, 6, 180, 6), (3, 8, 1110, 12), (2, 6, 180, 6)]
        {
            let params = parameters_at(at);
            let prepared = native
                .prepare_buffered_sealed_aggregate(&params, R, memory(), sealing(), RAW)
                .unwrap();
            for (group_capacity, run_rows) in [(1, 1), (2, 3)] {
                let actual = execute_aggregate(
                    &prepared,
                    &mut current,
                    &cx,
                    policy(),
                    &spill_pool,
                    Limits {
                        group_capacity,
                        run_rows,
                        ..Limits::default()
                    },
                )
                .await
                .unwrap();
                assert_eq!(actual.snapshot, CommitSeq(at));
                assert_eq!(actual.rows, expected(count, weighted, hops));
                assert_resident(&writer, &cx, &text, &params, &actual);
                assert_eq!(spill_pool.used(), 0);
            }
        }

        let one_result = GqlQueryPolicy::new(10_000, 1, WORK, WORK);
        let first = execute_aggregate(
            &historical,
            &mut current,
            &cx,
            one_result,
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(first.rows, expected(6, 180, 6));
        assert!(first.physical_work > 0);
        let warm = execute_aggregate(
            &historical,
            &mut current,
            &cx,
            one_result,
            &spill_pool,
            Limits::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            warm, first,
            "historical cache reuse must replay the complete usage"
        );
        for limits in [
            Limits {
                input_rows: 5,
                ..Limits::default()
            },
            Limits {
                work: first.physical_work - 1,
                ..Limits::default()
            },
        ] {
            assert!(
                execute_aggregate(
                    &historical,
                    &mut current,
                    &cx,
                    one_result,
                    &spill_pool,
                    limits,
                )
                .await
                .is_err(),
                "private input and cumulative physical work remain bounded"
            );
            assert_eq!(spill_pool.used(), 0);
        }

        // The same source-aware temporal binder also serves the ordinary
        // buffered host when the relation has no procedure source.
        let ordinary_text = "UNWIND [1,2] AS factor MATCH (node:Person) \
                             FOR SYSTEM_TIME AS OF SEQ $at \
                             RETURN COUNT(*) AS reached,SUM(node.p*factor) AS weighted";
        let ordinary = PreparedNativeRead::prepare(
            ordinary_text,
            &GqlParameters::new().with_uint64("at", 2).unwrap(),
            resolve,
        )
        .unwrap();
        for (at, weighted) in [(2, 450), (3, 1260)] {
            let prepared = ordinary
                .prepare_buffered_aggregate(
                    &GqlParameters::new().with_uint64("at", at).unwrap(),
                )
                .unwrap();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut current,
                    &cx,
                    policy(),
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    127,
                    32 * 1024,
                    1000,
                    WORK,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(spool.snapshot_seq(), CommitSeq(at));
            assert_eq!(spool.row_count(), 1);
            let mut reader = spool.reader(&mut destination, None);
            assert_eq!(
                reader.next_row(&cx).await.unwrap().unwrap().values(),
                &[QueryValue::Count(10), QueryValue::Integer(weighted)]
            );
            assert!(reader.next_row(&cx).await.unwrap().is_none());
            drop(reader);
            assert_eq!(spill_pool.used(), 0);
        }

        let future = native
            .prepare_buffered_sealed_aggregate(&parameters_at(4), R, memory(), sealing(), RAW)
            .unwrap();
        let buffer = current.buffer_stats();
        let source_baseline = source_pool.used();
        let (mut source, _) = file(&cx, &spill_pool).await;
        let (mut partition, _) = file(&cx, &spill_pool).await;
        let (mut destination, _) = file(&cx, &spill_pool).await;
        let error = future
            .spool_in_view(
                &mut current,
                &cx,
                policy(),
                &mut source,
                &mut partition,
                &mut destination,
                1,
                512,
                1,
                512,
                127,
                32 * 1024,
                1000,
                WORK,
                None,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                NativeAggregateSpoolError::Spool(NativeSpoolError::BufferedRead(error))
                    if matches!(*error, fgdb::BufferedReadError::BeyondPublication {
                        requested: CommitSeq(4), publication: CommitSeq(3),
                    })
            ),
            "future selection must refuse before either procedure or scratch IO"
        );
        for stats in [source.stats(), partition.stats(), destination.stats()] {
            assert_eq!(stats, fgdb_strata::tiered::memory::SpillStats::default());
        }
        assert_eq!(current.buffer_stats(), buffer);
        assert_eq!(source_pool.used(), source_baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(current);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
