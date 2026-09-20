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
