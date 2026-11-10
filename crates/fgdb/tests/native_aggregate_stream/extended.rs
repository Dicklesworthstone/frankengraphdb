//! End-to-end admission through native text, exact snapshots and pinned storage.
use super::*;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphAggregateValue, GraphExactAverage};

const EXACT: &str = "COUNT(*) AS rows, COUNT(n.score) AS nonnull, COUNT(DISTINCT n.score) AS different, SUM(n.score) AS sum, SUM(DISTINCT n.score) AS distinct_sum, AVG(n.score) AS average, AVG(DISTINCT n.score) AS distinct_average, MIN(n.score) AS minimum, MAX(n.score) AS maximum";

#[test]
fn transaction_catalog_names_share_the_query_allowance_and_keep_staged_values() {
    use fgdb_gql::{GlaLimitDimension, PreparedGraphText};
    let ((), report) = run_async_under_lab(0xa66e_2303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut basis = WriteBatch::new(RelationId(1));
        for id in 0..2 {
            basis.create_vertex(VId(id), vec![LABEL], vec![]);
        }
        basis.add_edge(EId(1), VId(0), VId(1), vec![]);
        db.write(&contexts.commit(), basis).await.unwrap();
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut staged = WriteBatch::new(RelationId(1));
        staged.create_vertex(VId(2), vec![LABEL, LabelId(4)], vec![]);
        staged.add_edge(EId(2), VId(1), VId(2), vec![]);
        txn.write(&mut db, staged).unwrap();
        let catalog = || symbols().with_label("Extra", LabelId(4));
        let text = |text: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(text).unwrap());
        let labels =
            |values: &[&str]| GraphValue::List(values.iter().map(|name| text(name)).collect());
        let args = GqlParameters::new();
        for (statement, expected) in [
            (
                "MATCH (n:L) RETURN labels(n)",
                vec![
                    vec![labels(&["L"])],
                    vec![labels(&["L"])],
                    vec![labels(&["L", "Extra"])],
                ],
            ),
            (
                "MATCH (a)-[r:R]->(b) RETURN type(r)",
                vec![vec![text("R")], vec![text("R")]],
            ),
        ] {
            let pattern = PreparedGraphText::prepare_with_resolver(statement, catalog())
                .unwrap()
                .bind_parameters(&args)
                .unwrap();
            let result = txn
                .execute_graph_pattern_governed(&db, &cx, &pattern, wide())
                .unwrap();
            assert_eq!(
                result
                    .value
                    .iter()
                    .map(|row| row.values().to_vec())
                    .collect::<Vec<_>>(),
                expected
            );
            let exact = GqlQueryPolicy::new(
                result.rows.snapshot_records,
                result.rows.result_rows,
                result.evaluator.work_units,
                result.evaluator.scratch_entries,
            );
            assert_eq!(
                txn.execute_graph_pattern_governed(&db, &cx, &pattern, exact)
                    .unwrap(),
                result
            );
            for (dimension, allowance) in [
                (
                    GlaLimitDimension::ScratchEntries,
                    GqlQueryPolicy::new(
                        u64::MAX,
                        u64::MAX,
                        u64::MAX,
                        exact.evaluator.max_scratch_entries - 1,
                    ),
                ),
                (
                    GlaLimitDimension::WorkUnits,
                    GqlQueryPolicy::new(
                        u64::MAX,
                        u64::MAX,
                        exact.evaluator.max_work_units - 1,
                        u64::MAX,
                    ),
                ),
            ] {
                assert!(
                    matches!(txn.execute_graph_pattern_governed(&db, &cx, &pattern, allowance), Err(GqlQueryError::Evaluator(limit)) if limit.dimension == dimension)
                );
            }
        }
        let PreparedNativeRead::Aggregate(prepared) = PreparedNativeRead::prepare(
            "MATCH (n:L) RETURN labels(n) AS labels,count(*) AS n ORDER BY labels",
            &args,
            catalog(),
        )
        .unwrap() else {
            panic!("expected aggregate");
        };
        let aggregate = prepared.bind_parameters(&args).unwrap();
        let result = txn
            .execute_graph_aggregate_governed(&db, &cx, &aggregate, wide())
            .unwrap();
        assert_eq!(
            result
                .value
                .iter()
                .map(|row| (row.keys().to_vec(), row.values().to_vec()))
                .collect::<Vec<_>>(),
            vec![
                (vec![labels(&["L"])], vec![GraphAggregateValue::Count(2)]),
                (
                    vec![labels(&["L", "Extra"])],
                    vec![GraphAggregateValue::Count(1)]
                ),
            ]
        );
        let exact = GqlQueryPolicy::new(
            result.rows.snapshot_records,
            result.rows.result_rows,
            result.evaluator.work_units,
            result.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, exact)
                .unwrap(),
            result
        );
        assert!(
            matches!(txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, exact.evaluator.max_scratch_entries - 1)), Err(GqlQueryError::Evaluator(limit)) if limit.dimension == GlaLimitDimension::ScratchEntries)
        );
        // A staged vertex/edge remains private after successful and refused reads.
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(db.edges().unwrap().len(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordinary_composite_aggregate_inputs_use_the_existing_pull_reducers() {
    use fgdb_gql::GraphAggregateTextSlot;

    let ((), report) = run_async_under_lab(0xa66e_2301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        let mut totals = [0_i128; 4];
        for id in 0..64_u128 {
            let group = (id % 4) as usize;
            let score = (id % 5) as i64;
            totals[group] += i128::from(score);
            batch.create_vertex(
                VId(id),
                vec![LABEL],
                vec![
                    (SCORE, CanonicalScalar::Int(score)),
                    (
                        TAG,
                        CanonicalScalar::ucs_basic_text(&"x".repeat(group + 1)).unwrap(),
                    ),
                ],
            );
        }
        let mut edge_total = 0_i128;
        for id in 0..128_u128 {
            let score = (id % 7) as i64;
            edge_total += i128::from(score);
            batch.add_edge(
                EId(1000 + id),
                VId(id % 64),
                VId((id + 1) % 64),
                vec![(SCORE, CanonicalScalar::Int(score))],
            );
        }
        let basis = db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        let width =
            |value| GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value)));
        let mut retained = Vec::new();
        for (text, records, expected) in [
            (
                "MATCH (n:L) RETURN size(n.tag) AS width,count(*) AS n,sum(n.score) AS total ORDER BY width",
                64,
                (0..4)
                    .map(|group| {
                        vec![
                            width(group as i64 + 1),
                            GraphAggregateValue::Count(16),
                            GraphAggregateValue::Integer(totals[group]),
                        ]
                    })
                    .collect::<Vec<_>>(),
            ),
            (
                "MATCH (n:L) WITH size(reverse(n.tag)) AS width,n.score AS score RETURN width,count(*) AS n,sum(score) AS total ORDER BY width",
                64,
                (0..4)
                    .map(|group| {
                        vec![
                            width(group as i64 + 1),
                            GraphAggregateValue::Count(16),
                            GraphAggregateValue::Integer(totals[group]),
                        ]
                    })
                    .collect(),
            ),
            (
                "MATCH (a)-[r:R]->(b) RETURN size([r.score,r.score+1]) AS width,count(*) AS n,sum(r.score) AS total ORDER BY width",
                128,
                vec![vec![
                    width(2),
                    GraphAggregateValue::Count(128),
                    GraphAggregateValue::Integer(edge_total),
                ]],
            ),
            (
                "MATCH (a)-[r:R]->(b) WITH size([r.score]) AS width,r.score AS score RETURN width,count(*) AS n,sum(score) AS total ORDER BY width",
                128,
                vec![vec![
                    width(1),
                    GraphAggregateValue::Count(128),
                    GraphAggregateValue::Integer(edge_total),
                ]],
            ),
        ] {
            let (definition, columns, slots) =
                match PreparedNativeRead::prepare(text, &params, symbols()).unwrap() {
                    PreparedNativeRead::Aggregate(prepared) => (
                        prepared.bind_parameters(&params).unwrap(),
                        prepared.columns().to_vec(),
                        prepared.output_slots().to_vec(),
                    ),
                    PreparedNativeRead::PipelineAggregate(prepared) => (
                        prepared.bind_parameters(&params).unwrap(),
                        prepared.columns().to_vec(),
                        prepared.output_slots().to_vec(),
                    ),
                    _ => panic!("expected native graph aggregate: {text}"),
                };
            assert!(
                fgdb_gql::scan_stream::AggregateScanPlan::compile_numeric(&definition).is_some(),
                "{text}; input {:?}; explicit edge compiler {:?}",
                definition.input_pattern().plan().operators(),
                fgdb_gql::edge_stream::aggregate::EdgeAggregatePlan::compile(&definition).err(),
            );
            let mut cursor = db
                .query_aggregate_stream(&cx, text, &params, symbols(), wide())
                .unwrap();
            let groups = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(cursor.row_stats().snapshot_records, records);
            let exact = GqlQueryPolicy::new(
                records,
                groups.len() as u64,
                cursor.evaluator_stats().work_units,
                cursor.evaluator_stats().scratch_entries,
            );
            let execution = db
                .execute_graph_aggregate_governed(&cx, &definition, exact)
                .unwrap();
            assert!(execution.value == groups);
            assert_eq!(execution.rows, cursor.row_stats());
            assert_eq!(execution.evaluator, cursor.evaluator_stats());
            let typed = groups
                .iter()
                .map(|row| {
                    slots
                        .iter()
                        .map(|slot| match *slot {
                            GraphAggregateTextSlot::GroupKey(at) => {
                                GraphAggregateValue::Value(row.keys()[at].clone())
                            }
                            GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert!(typed == expected, "typed literal rows differ: {text}");
            let expected = QueryResult::Rows {
                columns,
                rows: typed,
            };
            assert_eq!(
                db.query(&cx, text, &params, symbols(), exact).unwrap(),
                expected
            );
            assert_eq!(
                view.query(&cx, text, &params, symbols(), exact).unwrap(),
                expected
            );
            assert!(matches!(
                db.query(
                    &cx,
                    text,
                    &params,
                    symbols(),
                    GqlQueryPolicy::new(records - 1, u64::MAX, u64::MAX, u64::MAX)
                ),
                Err(QueryError::Aggregate(GqlQueryError::Rows(_)))
            ));
            retained.push((text, expected, records == 64));
        }
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(0), SCORE, Some(CanonicalScalar::Int(100)));
        update.set_vertex_property(
            VId(0),
            TAG,
            Some(CanonicalScalar::ucs_basic_text("longer").unwrap()),
        );
        update.set_edge_property(EId(1000), SCORE, Some(CanonicalScalar::Int(1000)));
        update.delete_edge(EId(1001));
        update.add_edge(
            EId(2000),
            VId(0),
            VId(1),
            vec![(SCORE, CanonicalScalar::Int(7))],
        );
        assert!(db.write(&commit, update).await.unwrap() > basis);
        for (text, expected, vertex) in retained {
            let mut changed = expected.clone();
            let QueryResult::Rows { rows, .. } = &mut changed else {
                unreachable!();
            };
            if vertex {
                rows[0][1] = GraphAggregateValue::Count(15);
                rows.push(vec![
                    width(6),
                    GraphAggregateValue::Count(1),
                    GraphAggregateValue::Integer(100),
                ]);
            } else {
                rows[0][2] = GraphAggregateValue::Integer(edge_total + 1006);
            }
            assert_ne!(changed, expected);
            assert_eq!(
                db.query(&cx, text, &params, symbols(), wide()).unwrap(),
                changed
            );
            assert_eq!(
                view.query(&cx, text, &params, symbols(), wide()).unwrap(),
                expected
            );
            let historical = text.replacen(
                " RETURN ",
                &format!(" FOR SYSTEM_TIME AS OF SEQ {} RETURN ", basis.0),
                1,
            );
            // A selector belongs to MATCH, before WITH in a pipeline.
            let historical = if let Some((head, tail)) = text.split_once(" WITH ") {
                format!("{head} FOR SYSTEM_TIME AS OF SEQ {} WITH {tail}", basis.0)
            } else {
                historical
            };
            assert_eq!(
                db.query(&cx, &historical, &params, symbols(), wide())
                    .unwrap(),
                expected
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordinary_composite_aggregates_keep_empty_late_fault_and_cancellation_contracts() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0xa66e_2302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let empty = db.query(
            &cx,
            "MATCH (n:L) WHERE n.score > 1000 RETURN size(reverse(n.tag)) AS width,SUM(n.score) AS total",
            &params,
            symbols(),
            wide(),
        ).unwrap();
        assert!(matches!(empty, QueryResult::Rows { rows, .. } if rows.is_empty()));

        let text = "MATCH (n:L) RETURN size(reverse(n.tag)) AS width,SUM(n.score) AS total";
        let expected = db.query(&cx, text, &params, symbols(), wide()).unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        assert_eq!(
            db.query(
                &cx.with_checkpoint_probe(Arc::clone(&probe)),
                text,
                &params,
                symbols(),
                wide()
            )
            .unwrap(),
            expected
        );
        let calls = probe.calls();
        assert!(calls > 2);
        for stop in [1, calls / 2, calls] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            assert!(matches!(
                db.query(
                    &cx.with_checkpoint_probe(Arc::clone(&probe)),
                    text,
                    &params,
                    symbols(),
                    wide()
                ),
                Err(QueryError::Aggregate(GqlQueryError::Interrupted(_)))
            ));
            assert_eq!(probe.calls(), stop);
        }
        let pinned = db.read_session().unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(
            VId(u128::MAX),
            SCORE,
            Some(CanonicalScalar::ucs_basic_text("late invalid sum operand").unwrap()),
        );
        db.write(&commit, update).await.unwrap();
        for limit in [0, 1] {
            assert!(matches!(
                db.query(
                    &cx,
                    &format!("{text} LIMIT {limit}"),
                    &params,
                    symbols(),
                    wide()
                ),
                Err(QueryError::Aggregate(GqlQueryError::Source(
                    GraphAggregateError::NonIntegerSum { aggregate: 0 }
                )))
            ));
        }
        assert_eq!(
            pinned.query(&cx, text, &params, symbols(), wide()).unwrap(),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_nine_native_aggregates_preserve_aliases_parameters_nulls_and_duplicate_support() {
    let ((), report) = run_async_under_lab(0xa66e_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let text = format!("MATCH (n:L) RETURN {EXACT}");
        let mut empty = db
            .query_aggregate_stream(&cx, &text, &GqlParameters::new(), symbols(), wide())
            .unwrap();
        let row = empty.next().unwrap().unwrap();
        assert!(
            row.values()[..3]
                .iter()
                .all(|value| value.as_count() == Some(0))
        );
        assert!(row.values()[3..].iter().all(GraphAggregateValue::is_null));
        assert_eq!(empty.row_stats().result_rows, 1);
        assert!(empty.next().is_none());
        db.write(&commit, seed()).await.unwrap();
        let mut duplicate = WriteBatch::new(RelationId(1));
        duplicate.create_vertex(
            VId(3),
            vec![LABEL],
            vec![
                (SCORE, CanonicalScalar::Int(14)),
                (
                    TAG,
                    CanonicalScalar::ucs_basic_text("' $not_syntax").unwrap(),
                ),
            ],
        );
        db.write(&commit, duplicate).await.unwrap();
        let mut all = db
            .query_aggregate_stream(&cx, &text, &GqlParameters::new(), symbols(), wide())
            .unwrap();
        let row = all.next().unwrap().unwrap();
        assert_eq!(row.values()[0].as_count(), Some(5));
        assert_eq!(row.values()[1].as_count(), Some(4));
        assert_eq!(row.values()[2].as_count(), Some(3));
        assert_eq!(row.values()[3].as_integer(), Some(21));
        assert_eq!(row.values()[4].as_integer(), Some(7));
        assert_eq!(row.values()[5].as_average(), GraphExactAverage::new(21, 4));
        assert_eq!(row.values()[6].as_average(), GraphExactAverage::new(7, 3));
        for (text, args) in [
            (text, GqlParameters::new()),
            (
                format!("MATCH (n:L) WHERE n.score >= $floor RETURN {EXACT}"),
                GqlParameters::new().with_int64("floor", 0).unwrap(),
            ),
            (
                format!("MATCH (n:L) WHERE n.score >= $floor RETURN {EXACT}"),
                GqlParameters::new().with_int64("floor", 100).unwrap(),
            ),
            (
                String::from(
                    "MATCH (n:L) WHERE n.tag=$tag RETURN MAX(n) AS last, MIN(n.tag) AS tag, AVG(DISTINCT n.score) AS first_average, AVG(DISTINCT n.score) AS second_average",
                ),
                GqlParameters::new()
                    .with_text("tag", "' $not_syntax")
                    .unwrap(),
            ),
        ] {
            let QueryResult::Rows { columns, rows } =
                db.query(&cx, &text, &args, symbols(), wide()).unwrap()
            else {
                panic!("expected read rows");
            };
            let mut cursor = db
                .query_aggregate_stream(&cx, &text, &args, symbols(), wide())
                .unwrap();
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            assert_eq!(
                rows,
                vec![cursor.next().unwrap().unwrap().values().to_vec()]
            );
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_exact_values_and_owned_extrema_survive_writes_compaction_and_source_drop() {
    let ((), report) = run_async_under_lab(0xa66e_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let old_view = db.read_session().unwrap();
        let text = format!(
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $seq RETURN {EXACT}, COUNT(DISTINCT n) AS vertices, MIN(n) AS first, MAX(n) AS last, MIN(n.tag) AS first_tag, MAX(n.tag) AS last_tag"
        );
        let args = GqlParameters::new().with_uint64("seq", 1).unwrap();
        let prepared = PreparedNativeRead::prepare(&text, &args, symbols()).unwrap();
        let QueryResult::Rows { rows: old_rows, .. } =
            prepared.execute(&db, &cx, &args, wide()).unwrap()
        else {
            panic!("expected read rows");
        };
        let mut pinned = prepared
            .stream_aggregate_in_view(&old_view, &cx, &args, wide())
            .unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(0));
        change.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(i64::MAX)));
        change.set_vertex_property(
            VId(1),
            TAG,
            Some(CanonicalScalar::ucs_basic_text(&"private-text".repeat(512)).unwrap()),
        );
        db.write(&commit, change).await.unwrap();
        for seq in 0..=2 {
            let args = GqlParameters::new().with_uint64("seq", seq).unwrap();
            let QueryResult::Rows { columns, rows } =
                prepared.execute(&db, &cx, &args, wide()).unwrap()
            else {
                panic!("expected read rows");
            };
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(seq));
            assert_eq!(cursor.columns(), columns);
            assert_eq!(
                rows,
                vec![cursor.next().unwrap().unwrap().values().to_vec()]
            );
            assert!(cursor.next().is_none());
        }
        let future = GqlParameters::new().with_uint64("seq", 2).unwrap();
        assert!(matches!(
            prepared.stream_aggregate_in_view(
                &old_view,
                &cx,
                &future,
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(QueryError::AggregateStream(GqlQueryError::Source(
                GraphAggregateError::Source(VertexScanError::Source(_))
            )))
        ));
        // A second pin carries a large retained scalar, independent of this
        // template, parameter map, view and the writer's subsequent lifetime.
        let QueryResult::Rows {
            rows: latest_rows, ..
        } = prepared.execute(&db, &cx, &future, wide()).unwrap()
        else {
            panic!("expected read rows");
        };
        let mut latest = prepared
            .stream_aggregate(&db, &cx, &future, wide())
            .unwrap();
        db.compact(&commit).await.unwrap();
        drop(prepared);
        drop(args);
        drop(future);
        drop(text);
        drop(old_view);
        drop(db);
        let old = pinned.next().unwrap().unwrap();
        assert_eq!(vec![old.values().to_vec()], old_rows);
        assert_eq!(old.values()[9].as_count(), Some(4));
        assert_eq!(
            old.values()[10].as_value(),
            Some(&GraphValue::Vertex(VId(0)))
        );
        assert_eq!(
            old.values()[11].as_value(),
            Some(&GraphValue::Vertex(VId(u128::MAX)))
        );
        let row = latest.next().unwrap().unwrap();
        assert_eq!(vec![row.values().to_vec()], latest_rows);
        assert!(!format!("{row:?} {latest:?}").contains("private-text"));
        assert!(pinned.next().is_none());
        assert!(latest.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn new_native_aggregate_state_obeys_exact_cumulative_quotas_and_nondraining_close() {
    let ((), report) = run_async_under_lab(0xa66e_2003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(
            VId(u128::MAX),
            TAG,
            Some(CanonicalScalar::ucs_basic_text(&"z-payload".repeat(256)).unwrap()),
        );
        db.write(&commit, change).await.unwrap();
        let text = format!(
            "MATCH (n:L) RETURN {EXACT}, COUNT(DISTINCT n.tag) AS tags, MIN(n.tag) AS first_tag, MAX(n.tag) AS last_tag"
        );
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(&text, &args, symbols()).unwrap();
        let mut baseline = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let expected = baseline.next().unwrap().unwrap();
        let rows = baseline.row_stats();
        let stats = baseline.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            1,
            stats.work_units,
            stats.scratch_entries,
        );
        let mut admitted = prepared.stream_aggregate(&db, &cx, &args, exact).unwrap();
        assert_eq!(admitted.next().unwrap().unwrap(), expected);
        for policy in [
            GqlQueryPolicy::new(rows.snapshot_records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, stats.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, stats.scratch_entries - 1),
        ] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, policy).unwrap();
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            assert!(matches!(
                cursor.next(),
                Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)))
            ));
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            let counters = (cursor.row_stats(), cursor.evaluator_stats());
            assert!(cursor.next().is_none());
            cursor.close();
            assert_eq!(counters, (cursor.row_stats(), cursor.evaluator_stats()));
        }
        let mut closed = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        closed.close();
        assert!(closed.next().is_none());
        assert_eq!(closed.row_stats().snapshot_records, 0);
        assert_eq!(closed.evaluator_stats().work_units, 0);
        assert_eq!(closed.evaluator_stats().scratch_entries, 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_numeric_domain_errors_do_not_publish_prior_distinct_or_extremum_cells() {
    let ((), report) = run_async_under_lab(0xa66e_2004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(
            VId(u128::MAX),
            SCORE,
            Some(CanonicalScalar::ucs_basic_text("private invalid operand").unwrap()),
        );
        db.write(&commit, change).await.unwrap();
        for (function, average) in [
            ("AVG(n.score)", true),
            ("AVG(DISTINCT n.score)", true),
            ("SUM(DISTINCT n.score)", false),
        ] {
            let text = format!(
                "MATCH (n:L) RETURN COUNT(DISTINCT n.tag) AS tags, MIN(n.tag) AS first_tag, {function} AS numeric"
            );
            let mut cursor = db
                .query_aggregate_stream(&cx, &text, &GqlParameters::new(), symbols(), wide())
                .unwrap();
            let error = cursor.next().unwrap().unwrap_err();
            if average {
                assert!(matches!(
                    error,
                    GqlQueryError::Source(GraphAggregateError::NonIntegerAverage { aggregate: 2 })
                ));
            } else {
                assert!(matches!(
                    error,
                    GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 2 })
                ));
            }
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(!format!("{cursor:?}").contains("private invalid operand"));
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordinary_queries_use_the_selected_numeric_cursor_and_its_exact_allowances() {
    use fgdb_gql::{GraphAggregateTextSlot, PreparedGraphAggregateText};

    let ((), report) = run_async_under_lab(0xa66e_2101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 0..32_u128 {
            batch.create_vertex(
                VId(id),
                vec![LABEL],
                vec![
                    (SCORE, CanonicalScalar::Int(id as i64 - 12)),
                    (TAG, CanonicalScalar::Int((id % 4) as i64)),
                ],
            );
        }
        for id in 0..32_u128 {
            for offset in 0..8 {
                batch.add_edge(
                    EId(1000 + id * 8 + offset),
                    VId(id),
                    VId((id + offset) % 32),
                    vec![(SCORE, CanonicalScalar::Int(offset as i64 - 2))],
                );
            }
        }
        db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        for (text, records) in [
            (
                "MATCH (n:L) RETURN n.tag AS tag,count(*) AS n,sum(n.score) AS total ORDER BY tag",
                32,
            ),
            (
                "MATCH (a)-[r:R]->(b) RETURN a AS owner,count(*) AS degree ORDER BY degree DESC,owner LIMIT 4",
                256,
            ),
        ] {
            let mut resolver = symbols();
            let prepared = PreparedGraphAggregateText::prepare(text, |kind, name| {
                fgdb_gql::GraphSymbolResolver::resolve_symbol(&mut resolver, kind, name)
            })
            .unwrap();
            let definition = prepared.bind_parameters(&args).unwrap();
            assert!(
                fgdb_gql::scan_stream::AggregateScanPlan::compile_numeric(&definition).is_some()
            );
            let mut cursor = db
                .query_aggregate_stream(&cx, text, &args, symbols(), wide())
                .unwrap();
            let logical = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            let stats = cursor.row_stats();
            let work = cursor.evaluator_stats();
            assert_eq!((stats.snapshot_records, stats.result_rows), (records, 4));
            let exact = GqlQueryPolicy::new(
                stats.snapshot_records,
                stats.result_rows,
                work.work_units,
                work.scratch_entries,
            );
            let typed = db
                .execute_graph_aggregate_governed(&cx, &definition, exact)
                .unwrap();
            assert_eq!(typed.value, logical);
            assert_eq!(typed.rows, stats);
            assert_eq!(typed.evaluator, work);
            let rows: Vec<Vec<_>> = logical
                .iter()
                .map(|row| {
                    prepared
                        .output_slots()
                        .iter()
                        .map(|slot| match *slot {
                            GraphAggregateTextSlot::GroupKey(at) => {
                                GraphAggregateValue::Value(row.keys()[at].clone())
                            }
                            GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                        })
                        .collect()
                })
                .collect();
            for (at, row) in rows.iter().enumerate() {
                if records == 32 {
                    assert_eq!(
                        row[0].as_value(),
                        Some(&GraphValue::Scalar(CanonicalScalar::Int(at as i64)))
                    );
                    assert_eq!(row[1].as_count(), Some(8));
                    assert_eq!(row[2].as_integer(), Some(16 + at as i128 * 8));
                } else {
                    assert_eq!(
                        row[0].as_value(),
                        Some(&GraphValue::Vertex(VId(at as u128)))
                    );
                    assert_eq!(row[1].as_count(), Some(8));
                }
            }
            let expected = QueryResult::Rows {
                columns: prepared.columns().to_vec(),
                rows,
            };
            assert_eq!(
                db.query(&cx, text, &args, symbols(), exact).unwrap(),
                expected
            );
            assert_eq!(
                view.query(&cx, text, &args, symbols(), exact).unwrap(),
                expected
            );
            for short in [
                GqlQueryPolicy::new(records - 1, 4, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(u64::MAX, 3, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(u64::MAX, 4, work.work_units - 1, u64::MAX),
                GqlQueryPolicy::new(u64::MAX, 4, u64::MAX, work.scratch_entries - 1),
            ] {
                assert!(matches!(
                    db.query(&cx, text, &args, symbols(), short),
                    Err(QueryError::Aggregate(
                        GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)
                    ))
                ));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordinary_filtered_edge_aggregates_keep_pull_budgets_history_and_refusal() {
    use fgdb_gql::PreparedGraphAggregateText;
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0xa66e_2103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 0..5_u128 {
            let mut properties = vec![(SCORE, CanonicalScalar::Int(id as i64 * 10))];
            if id != 0 && id != 2 {
                properties.push((TAG, CanonicalScalar::Int(1)));
            }
            batch.create_vertex(
                VId(id),
                if id == 2 { vec![] } else { vec![LABEL] },
                properties,
            );
        }
        // Include parallel edges, self loops, and endpoints outside the label.
        // The totals below are sums over this literal relation, independently
        // of either evaluator's filtering or aggregation implementation.
        for (id, from, to, score) in [
            (10, 0, 1, 2),
            (11, 0, 1, 3),
            (12, 1, 0, 5),
            (13, 1, 1, 7),
            (14, 1, 2, 11),
            (15, 2, 3, 13),
            (16, 3, 4, 17),
            (17, 4, 3, 19),
            (18, 4, 4, 23),
            (19, 3, 0, 29),
            (20, 0, 3, 31),
        ] {
            batch.add_edge(
                EId(id),
                VId(from),
                VId(to),
                vec![(SCORE, CanonicalScalar::Int(score))],
            );
        }
        let basis = db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        for (input, count, sum) in [
            ("MATCH (a:L)-[r:R]->(b:L)", 9, 136),
            ("MATCH (a:L)-[r:R]->(b:L) WHERE a <> b", 7, 106),
            ("MATCH (a:L)-[r:R]->(b:L) WHERE a.score < b.score", 4, 53),
            ("MATCH (a)-[r:R]->(b) WHERE a.tag IS NULL", 4, 49),
            ("MATCH (a:L)<-[r:R]-(b:L) WHERE a.score > b.score", 4, 53),
            (
                "MATCH (a)-[r:R]->(b) WHERE r.score > a.score OR b.tag IS NULL",
                6,
                81,
            ),
            ("MATCH (a:L)-[r:R]-(b:L) WHERE a <> b", 14, 212),
        ] {
            let text = format!("{input} RETURN count(*) AS n,sum(r.score) AS total");
            let mut resolver = symbols();
            let prepared = PreparedGraphAggregateText::prepare(&text, |kind, name| {
                fgdb_gql::GraphSymbolResolver::resolve_symbol(&mut resolver, kind, name)
            })
            .unwrap();
            let definition = prepared.bind_parameters(&args).unwrap();
            assert!(
                fgdb_gql::scan_stream::AggregateScanPlan::compile_numeric(&definition).is_some(),
                "{text}"
            );
            let mut cursor = db
                .query_aggregate_stream(&cx, &text, &args, symbols(), wide())
                .unwrap();
            let logical = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            let stats = cursor.row_stats();
            let work = cursor.evaluator_stats();
            assert_eq!((stats.snapshot_records, stats.result_rows), (11, 1));
            let exact = GqlQueryPolicy::new(
                stats.snapshot_records,
                stats.result_rows,
                work.work_units,
                work.scratch_entries,
            );
            let typed = db
                .execute_graph_aggregate_governed(&cx, &definition, exact)
                .unwrap();
            assert_eq!(typed.value, logical);
            assert_eq!(typed.rows, stats);
            assert_eq!(typed.evaluator, work);
            let expected = QueryResult::Rows {
                columns: vec!["n".into(), "total".into()],
                rows: vec![vec![
                    GraphAggregateValue::Count(count),
                    GraphAggregateValue::Integer(sum),
                ]],
            };
            assert_eq!(
                db.query(&cx, &text, &args, symbols(), exact).unwrap(),
                expected,
                "{text}"
            );
            assert_eq!(
                view.query(&cx, &text, &args, symbols(), exact).unwrap(),
                expected,
                "{text}"
            );
            for short in [
                GqlQueryPolicy::new(10, 1, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(11, 0, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(11, 1, work.work_units - 1, u64::MAX),
                GqlQueryPolicy::new(11, 1, u64::MAX, work.scratch_entries - 1),
            ] {
                assert!(matches!(
                    db.query(&cx, &text, &args, symbols(), short),
                    Err(QueryError::Aggregate(
                        GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)
                    ))
                ));
            }
        }

        // The Boolean VM evaluates both operands. A decisive sibling and an
        // empty output page must not hide a reached arithmetic error.
        for predicate in [
            "r.score > 0 OR 1/(r.score-r.score) > 0",
            "r.score < 0 AND 1/(r.score-r.score) > 0",
        ] {
            for limit in [0, 1] {
                let text = format!(
                    "MATCH (a)-[r:R]->(b) WHERE {predicate} RETURN sum(r.score) AS total LIMIT {limit}"
                );
                assert!(matches!(
                    db.query(&cx, &text, &args, symbols(), wide()),
                    Err(QueryError::Aggregate(GqlQueryError::Data(error)))
                        if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
                ));
            }
        }

        let text = "MATCH (a:L)-[r:R]->(b:L) RETURN sum(r.score) AS total";
        let expected = db.query(&cx, text, &args, symbols(), wide()).unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        assert_eq!(
            db.query(
                &cx.with_checkpoint_probe(Arc::clone(&probe)),
                text,
                &args,
                symbols(),
                wide(),
            )
            .unwrap(),
            expected
        );
        let calls = probe.calls();
        assert!(calls > 2);
        for stop in [1, calls / 2, calls] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            assert!(matches!(
                db.query(
                    &cx.with_checkpoint_probe(Arc::clone(&probe)),
                    text,
                    &args,
                    symbols(),
                    wide(),
                ),
                Err(QueryError::Aggregate(GqlQueryError::Interrupted(_)))
            ));
            assert_eq!(probe.calls(), stop);
        }
        let mut update = WriteBatch::new(RelationId(1));
        update.set_edge_property(
            EId(20),
            SCORE,
            Some(CanonicalScalar::ucs_basic_text("late invalid edge score").unwrap()),
        );
        db.write(&commit, update).await.unwrap();
        for limit in [0, 1] {
            let text = format!("{text} LIMIT {limit}");
            assert!(matches!(
                db.query(&cx, &text, &args, symbols(), wide()),
                Err(QueryError::Aggregate(GqlQueryError::Source(
                    GraphAggregateError::NonIntegerSum { aggregate: 0 }
                )))
            ));
        }
        assert_eq!(
            view.query(&cx, text, &args, symbols(), wide()).unwrap(),
            expected
        );
        let historical = format!(
            "MATCH (a:L)-[r:R]->(b:L) FOR SYSTEM_TIME AS OF SEQ {} RETURN sum(r.score) AS total",
            basis.0
        );
        assert_eq!(
            db.query(&cx, &historical, &args, symbols(), wide())
                .unwrap(),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordinary_numeric_queries_keep_pinned_history_and_refuse_late_errors_without_retry() {
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0xa66e_2102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        let text = "MATCH (n:L) RETURN n.score AS score,count(*) AS n ORDER BY score";
        let expected = db.query(&cx, text, &args, symbols(), wide()).unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        assert_eq!(
            db.query(
                &cx.with_checkpoint_probe(Arc::clone(&probe)),
                text,
                &args,
                symbols(),
                wide(),
            )
            .unwrap(),
            expected
        );
        let calls = probe.calls();
        assert!(calls > 2);
        for stop in [1, calls / 2, calls] {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            assert!(matches!(
                db.query(
                    &cx.with_checkpoint_probe(Arc::clone(&probe)),
                    text,
                    &args,
                    symbols(),
                    wide(),
                ),
                Err(QueryError::Aggregate(GqlQueryError::Interrupted(_)))
            ));
            assert_eq!(probe.calls(), stop);
        }
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(
            VId(u128::MAX),
            SCORE,
            Some(CanonicalScalar::ucs_basic_text("late invalid numeric input").unwrap()),
        );
        db.write(&commit, update).await.unwrap();
        for limit in [0, 1] {
            let text = format!(
                "MATCH (n:L) RETURN n.tag AS tag,SUM(n.score) AS total ORDER BY total LIMIT {limit}"
            );
            assert!(matches!(
                db.query(&cx, &text, &args, symbols(), wide()),
                Err(QueryError::Aggregate(GqlQueryError::Source(
                    GraphAggregateError::NonIntegerSum { aggregate: 0 }
                )))
            ));
        }
        assert_eq!(
            view.query(&cx, text, &args, symbols(), wide()).unwrap(),
            expected
        );
        let historical = format!(
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ {} RETURN n.score AS score,count(*) AS n ORDER BY score",
            basis.0
        );
        assert_eq!(
            db.query(&cx, &historical, &args, symbols(), wide())
                .unwrap(),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// A stored List reads on the numeric cursor as the general evaluator reads
/// it (TCK Return5 [5]): it groups, counts, orders as an extremum and counts
/// as a DISTINCT value. Only a scalar-only aggregate refuses it, with that
/// aggregate's own typed error (fgdb-k3nub).
#[test]
fn stored_collections_group_and_aggregate_on_the_numeric_cursor() {
    use fgdb_gql::PreparedGraphAggregateText;

    let ((), report) = run_async_under_lab(0xa66e_2201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let text =
            |value: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
        let red = GraphValue::List(vec![text("red")].into());
        let blue = GraphValue::List(vec![text("blue")].into());
        // A stored property is never a map (its ruling is pending), so the
        // fourth collection is a longer list.
        let pair = GraphValue::List(vec![text("blue"), text("red")].into());
        let mut batch = WriteBatch::new(RelationId(1));
        for (id, tag) in [(1_u128, &red), (2, &blue), (3, &red), (4, &pair)] {
            batch.create_vertex_values(
                VId(id),
                vec![LABEL],
                vec![
                    (SCORE, CanonicalScalar::Int(id as i64).into()),
                    (TAG, tag.to_written_property().unwrap()),
                ],
            );
        }
        batch.create_vertex(VId(5), vec![LABEL], vec![(SCORE, CanonicalScalar::Int(5))]);
        db.write(&commit, batch).await.unwrap();
        let args = GqlParameters::new();
        let value = |value: &GraphValue| GraphAggregateValue::Value(value.clone());
        let null = GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
        // Cells redact their Debug text, so compare the bags directly.
        let same_rows = |result: QueryResult, expected: &[Vec<GraphAggregateValue>]| {
            let QueryResult::Rows { rows, .. } = result else {
                panic!("expected rows");
            };
            rows.len() == expected.len() && expected.iter().all(|row| rows.contains(row))
        };
        let grouped = "MATCH (n:L) RETURN n.tag AS tag, count(*) AS rows, sum(n.score) AS total, \
                       count(n.tag) AS tags, min(n.tag) AS least";
        let global = "MATCH (n:L) WHERE n.score < 4 \
                      RETURN min(n.tag) AS least, max(n.tag) AS most, count(DISTINCT n.tag) AS kinds";
        for statement in [grouped, global] {
            // The law is about the numeric cursor: it must be the path taken.
            let prepared = PreparedGraphAggregateText::prepare(statement, |kind, name| {
                fgdb_gql::GraphSymbolResolver::resolve_symbol(&mut symbols(), kind, name)
            })
            .unwrap();
            let definition = prepared.bind_parameters(&args).unwrap();
            assert!(
                fgdb_gql::scan_stream::AggregateScanPlan::compile_numeric(&definition).is_some(),
                "{statement}"
            );
        }
        let group = |tag: GraphAggregateValue, rows: u64, total: i128, tags: u64| {
            vec![
                tag.clone(),
                GraphAggregateValue::Count(rows),
                GraphAggregateValue::Integer(total),
                GraphAggregateValue::Count(tags),
                tag,
            ]
        };
        assert!(same_rows(
            db.query(&cx, grouped, &args, symbols(), wide()).unwrap(),
            &[
                group(value(&red), 2, 4, 2),
                group(value(&blue), 1, 2, 1),
                group(value(&pair), 1, 4, 1),
                group(null, 1, 5, 0),
            ]
        ));
        assert!(same_rows(
            db.query(&cx, global, &args, symbols(), wide()).unwrap(),
            &[vec![
                value(&blue),
                value(&red),
                GraphAggregateValue::Count(2)
            ]]
        ));
        assert!(matches!(
            db.query(
                &cx,
                "MATCH (n:L) RETURN sum(n.tag) AS s",
                &args,
                symbols(),
                wide()
            ),
            Err(QueryError::Aggregate(GqlQueryError::Source(
                GraphAggregateError::NonIntegerSum { aggregate: 0 }
            )))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
