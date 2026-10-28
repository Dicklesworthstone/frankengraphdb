//! End-to-end admission through native text, exact snapshots and pinned storage.
use super::*;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphAggregateValue, GraphExactAverage};

const EXACT: &str = "COUNT(*) AS rows, COUNT(n.score) AS nonnull, COUNT(DISTINCT n.score) AS different, SUM(n.score) AS sum, SUM(DISTINCT n.score) AS distinct_sum, AVG(n.score) AS average, AVG(DISTINCT n.score) AS distinct_average, MIN(n.score) AS minimum, MAX(n.score) AS maximum";

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
