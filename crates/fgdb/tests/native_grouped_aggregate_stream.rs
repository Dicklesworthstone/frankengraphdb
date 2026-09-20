//! Native GROUP BY uses the existing governed group cursor and explicit layout.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, PreparedNativeRead, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphAggregateTextSlot, GraphAggregateValue, RelationBind,
};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const L: LabelId = LabelId(3);
const SCORE: PropertyKeyId = PropertyKeyId(7);
const A: PropertyKeyId = PropertyKeyId(8);
const B: PropertyKeyId = PropertyKeyId(9);
const SUMMARY: &str = "MATCH (n:L) RETURN COUNT(*) AS total, n.b AS b, SUM(n.score) AS sum, n.a AS a, AVG(n.score) AS avg, MIN(n.score) AS min, MAX(n.score) AS max GROUP BY n.a, n.b";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x41; 32], DatabaseSecurityNamespaceId([0x42; 32]), [0x43; 32])
}
fn symbols() -> RelationBind {
    RelationBind::new().with_label("L", L).with_property("score", SCORE)
        .with_property("a", A).with_property("b", B)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn text(value: &str) -> CanonicalScalar { CanonicalScalar::ucs_basic_text(value).unwrap() }
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, score, a, b) in [
        (0, Some(-9), Some(CanonicalScalar::Null), text("east")),
        (1, Some(5), Some(CanonicalScalar::Int(1)), text("west")),
        (2, Some(-2), Some(CanonicalScalar::Int(1)), text("west")),
        (3, None, None, text("east")),
        (4, Some(i64::MAX), Some(CanonicalScalar::Int(1)), text("east")),
        (5, Some(5), Some(CanonicalScalar::Int(1)), text("west")),
        (u128::MAX, Some(i64::MIN), Some(text("' $private_key")), CanonicalScalar::Bool(true)),
    ] {
        let mut props = vec![(SCORE, score.map_or(CanonicalScalar::Null, CanonicalScalar::Int))];
        if let Some(a) = a { props.push((A, a)); }
        props.push((B, b));
        batch.create_vertex(VId(id), vec![L], props);
    }
    batch
}
fn projected(row: &GraphAggregateRow, slots: &[GraphAggregateTextSlot]) -> Vec<GraphAggregateValue> {
    slots.iter().map(|slot| match *slot {
        GraphAggregateTextSlot::GroupKey(at) => GraphAggregateValue::Value(row.keys()[at].clone()),
        GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
    }).collect()
}

#[test]
fn native_grouped_layouts_preserve_return_aliases_keys_exact_values_and_group_order() {
    let ((), report) = run_async_under_lab(0xa66e_4001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        for statement in [
            SUMMARY,
            "MATCH (n:L) RETURN n.b AS b, COUNT(n.score) AS present, n.a AS a GROUP BY n.a, n.b",
            "MATCH (n:L) RETURN n.a AS first, n.a AS again, COUNT(*) AS total GROUP BY n.a",
            "MATCH (n:L) RETURN COUNT(*) AS total, n AS id, MIN(n.score) AS score GROUP BY n",
            "MATCH (n:L) RETURN n.a AS a, SUM(n.score) AS sum GROUP BY n.a",
            "MATCH (n:L) RETURN COUNT(DISTINCT n.score) AS present, n.b AS b, SUM(DISTINCT n.score) AS sum, AVG(DISTINCT n.score) AS avg, n.a AS a GROUP BY n.a, n.b",
        ] {
            let QueryResult::Rows { columns, rows } = db.query(&cx, statement, &params, symbols(), wide()).unwrap()
                else { panic!("expected read rows") };
            let mut cursor = db.query_aggregate_stream(&cx, statement, &params, symbols(), wide()).unwrap();
            assert_eq!(cursor.kind(), ScanKind::Vertex);
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.columns().len(), cursor.output_slots().len());
            assert!(!cursor.key_columns().is_empty());
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            assert_eq!(cursor.row_stats().result_rows, 0);
            let slots = cursor.output_slots().to_vec();
            let key_width = cursor.key_columns().len();
            let aggregate_width = cursor.aggregate_columns().len();
            let mut actual = Vec::new();
            while let Some(row) = cursor.next() {
                let row = row.unwrap();
                assert_eq!(row.keys().len(), key_width);
                assert_eq!(row.values().len(), aggregate_width);
                actual.push(projected(&row, &slots));
            }
            assert_eq!(actual, rows, "{statement}");
            assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert!(cursor.next().is_none());
            assert!(!format!("{cursor:?}").contains("private_key"));
        }
        let mut cursor = db.query_aggregate_stream(&cx, SUMMARY, &params, symbols(), wide()).unwrap();
        assert_eq!(cursor.key_columns(), ["a", "b"]);
        assert_eq!(cursor.aggregate_columns(), ["total", "sum", "avg", "min", "max"]);
        let slots = cursor.output_slots().to_vec();
        let groups = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(groups.len(), 4);
        // Missing and stored NULL keys coalesce. Duplicates remain in ordinary AVG.
        assert_eq!(groups[0].values()[0].as_count(), Some(2));
        let west = groups.iter().find(|row| row.values()[0].as_count() == Some(3)
            && row.values()[1].as_integer() == Some(8)).unwrap();
        let avg = west.values()[2].as_average().unwrap();
        assert_eq!((avg.numerator(), avg.denominator()), (8, 3));
        assert_eq!(projected(west, &slots)[0].as_count(), Some(3));
        let text = "MATCH (n:L) RETURN COUNT(DISTINCT n.score) AS present, n.b AS b, SUM(DISTINCT n.score) AS sum, AVG(DISTINCT n.score) AS avg, n.a AS a GROUP BY n.a, n.b";
        let groups = db.query_aggregate_stream(&cx, text, &params, symbols(), wide()).unwrap()
            .collect::<Result<Vec<_>, _>>().unwrap();
        let west = groups.iter().find(|row| row.values()[0].as_count() == Some(2)
            && row.values()[1].as_integer() == Some(3)).unwrap();
        let avg = west.values()[2].as_average().unwrap();
        assert_eq!((avg.numerator(), avg.denominator()), (3, 2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_temporal_templates_and_open_cursors_keep_their_generation_and_layout() {
    let ((), report) = run_async_under_lab(0xa66e_4002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let statement = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut WHERE n.score >= $floor RETURN SUM(n.score) AS sum, n.a AS key, COUNT(*) AS total GROUP BY n.a";
        let params = GqlParameters::new().with_uint64("cut", 1).unwrap().with_int64("floor", -10).unwrap();
        let prepared = PreparedNativeRead::prepare(statement, &params, symbols()).unwrap();
        let QueryResult::Rows { columns, rows: frozen } = view.query(&cx, statement, &params, symbols(), wide()).unwrap()
            else { panic!("expected rows") };
        let mut pinned = prepared.stream_aggregate_in_view(&view, &cx, &params, wide()).unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), A, Some(CanonicalScalar::Int(2)));
        edit.delete_vertex(VId(2));
        edit.create_vertex(VId(20), vec![L], vec![(SCORE, CanonicalScalar::Int(4)), (A, CanonicalScalar::Int(1))]);
        db.write(&commit, edit).await.unwrap();
        for cut in 0..=2 {
            let args = GqlParameters::new().with_uint64("cut", cut).unwrap().with_int64("floor", -10).unwrap();
            let QueryResult::Rows { rows, .. } = prepared.execute(&db, &cx, &args, wide()).unwrap()
                else { panic!("expected rows") };
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            let slots = cursor.output_slots().to_vec();
            let actual: Vec<_> = cursor.by_ref().map(|row| projected(&row.unwrap(), &slots)).collect();
            assert_eq!(actual, rows);
        }
        let future = GqlParameters::new().with_uint64("cut", 2).unwrap().with_int64("floor", -10).unwrap();
        assert!(matches!(prepared.stream_aggregate_in_view(&view, &cx, &future, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(QueryError::AggregateStream(GqlQueryError::Source(_)))));
        drop(prepared); drop(params); drop(future); drop(view); drop(db);
        fn requires_send(_: &impl Send) {}
        requires_send(&pinned);
        assert_eq!(pinned.columns(), columns);
        let slots = pinned.output_slots().to_vec();
        let actual: Vec<_> = pinned.by_ref().map(|row| projected(&row.unwrap(), &slots)).collect();
        assert_eq!(actual, frozen);
        assert_eq!(pinned.state(), VertexScanState::Exhausted);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_budgets_are_cumulative_and_delivery_refusals_preserve_only_complete_prefixes() {
    let ((), report) = run_async_under_lab(0xa66e_4003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(SUMMARY, &args, symbols()).unwrap();
        let mut full = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let usage = full.evaluator_stats(); let rows = full.row_stats();
        let exact = GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows, usage.work_units, usage.scratch_entries);
        assert_eq!(prepared.stream_aggregate(&db, &cx, &args, exact).unwrap().collect::<Result<Vec<_>, _>>().unwrap(), expected);
        // These two admission failures happen while grouping, before any output.
        for policy in [
            GqlQueryPolicy::new(rows.snapshot_records - 1, rows.result_rows, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows - 1, u64::MAX, u64::MAX),
        ] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, policy).unwrap();
            assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        // Work/scratch are shared across accumulation AND every delivered group.
        for policy in [
            GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows, usage.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(rows.snapshot_records, rows.result_rows, u64::MAX, usage.scratch_entries - 1),
        ] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, policy).unwrap();
            let mut prefix = Vec::new();
            loop {
                match cursor.next().expect("one-less budget must fail") {
                    Ok(row) => prefix.push(row),
                    Err(GqlQueryError::Evaluator(_)) => break,
                    Err(other) => panic!("unexpected error: {other:?}"),
                }
            }
            assert!(expected.starts_with(&prefix));
            assert!(prefix.len() < expected.len());
            assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            let stats = (cursor.row_stats(), cursor.evaluator_stats());
            assert!(cursor.next().is_none()); cursor.close();
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
        }
        let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), expected[0]);
        let stats = (cursor.row_stats(), cursor.evaluator_stats());
        cursor.close(); cursor.close();
        assert_eq!(cursor.state(), VertexScanState::Closed);
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(u128::MAX), SCORE, Some(CanonicalScalar::Bool(false)));
        db.write(&commit, edit).await.unwrap();
        let mut invalid = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        assert!(matches!(invalid.next(), Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerSum { .. }
        )))));
        assert_eq!(invalid.row_stats().result_rows, 0);
        assert!(invalid.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_grouped_input_differs_from_global_input_and_unsupported_clauses_stay_intact() {
    let ((), report) = run_async_under_lab(0xa66e_4004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let db = Database::open_memory(&commit, keys()).await.unwrap();
        let args = GqlParameters::new();
        let zero = GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX);
        let mut grouped = db.query_aggregate_stream(&cx, SUMMARY, &args, symbols(), zero).unwrap();
        assert!(grouped.next().is_none());
        assert_eq!(grouped.state(), VertexScanState::Exhausted);
        assert_eq!(grouped.row_stats().result_rows, 0);
        let mut global = db.query_aggregate_stream(&cx, "MATCH (n:L) RETURN COUNT(*) AS total", &args, symbols(), zero).unwrap();
        assert!(matches!(global.next(), Some(Err(GqlQueryError::Rows(_)))));
        for statement in [
            "MATCH (n:L) RETURN COUNT(*) AS total GROUP BY n.a",
            "MATCH (n:L) RETURN n.a AS key, COUNT(*) AS total GROUP BY n.a HAVING total>0",
            "MATCH (n:L) RETURN n.a AS key, COUNT(*) AS total GROUP BY n.a ORDER BY key",
            "MATCH (n:L) RETURN n.a AS key, COUNT(*) AS total GROUP BY n.a LIMIT 0",
            "MATCH (n:L) RETURN n.a AS key, COLLECT(n.score) AS values GROUP BY n.a",
            "MATCH (n:L) RETURN n.a + 1 AS key, COUNT(*) AS total GROUP BY n.a + 1",
        ] {
            let prepared = PreparedNativeRead::prepare(statement, &args, symbols()).unwrap();
            assert!(matches!(prepared.stream_aggregate(&db, &cx, &args, zero),
                Err(QueryError::AggregateStreamPlan(_))), "{statement}");
        }
        let mut closed = db.query_aggregate_stream(&cx, SUMMARY, &args, symbols(), wide()).unwrap();
        closed.close();
        assert!(closed.next().is_none());
        assert_eq!(closed.evaluator_stats().work_units, 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
