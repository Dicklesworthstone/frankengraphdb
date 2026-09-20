//! Native aggregate dispatch must preserve the indexed edge operator's semantics.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, PreparedNativeRead, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::edge_stream::EdgeScanError;
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateValue,
    RelationBind,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const L: LabelId = LabelId(3);
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(7);
const IDS: [u128; 3] = [0, 1, u128::MAX];
const EDGES: [(usize, usize, Option<i64>); 6] = [
    (0, 1, Some(5)), (0, 1, None), (1, 2, Some(-2)),
    (2, 0, Some(1)), (1, 1, None), (0, 0, Some(-4)),
];
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
}
fn symbols() -> RelationBind {
    RelationBind::new().with_label("L", L).with_relation("R", R).with_property("score", P)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, score) in IDS.into_iter().zip([Some(-3), None, Some(7)]) {
        batch.create_vertex(VId(id), vec![L], vec![(P, score.map_or(CanonicalScalar::Null, CanonicalScalar::Int))]);
    }
    // An isolate must not enter an edge aggregate's match domain.
    batch.create_vertex(VId(42), vec![L], vec![(P, CanonicalScalar::Int(99))]);
    for (at, (from, to, score)) in EDGES.into_iter().enumerate() {
        let id = if at == 5 { u128::MAX } else { at as u128 };
        batch.add_edge(EId(id), VId(IDS[from]), VId(IDS[to]),
            score.map(|value| vec![(P, CanonicalScalar::Int(value))]).unwrap_or_default());
    }
    batch
}
fn sum(value: Option<i128>) -> GraphAggregateValue {
    value.map_or_else(
        || GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
        GraphAggregateValue::Integer,
    )
}
// Independent enumeration of stored edge occurrences/orientations. This does
// not use GLA, storage readers or the production join/probe/aggregate machine.
fn oracle(direction: usize, hops: usize) -> Vec<GraphAggregateValue> {
    let mut oriented = Vec::new();
    for (from, to, score) in EDGES {
        if direction == 1 {
            oriented.push((to, from, score));
        } else {
            oriented.push((from, to, score));
            if direction == 2 && from != to { oriented.push((to, from, score)); }
        }
    }
    let mut matches = Vec::new();
    for &(from, to, score) in &oriented { matches.push((from, to, score)); }
    for _ in 1..hops {
        let mut extended = Vec::new();
        for &(root, end, score) in &matches {
            for &(from, to, _) in &oriented {
                if end == from { extended.push((root, to, score)); }
            }
        }
        matches = extended;
    }
    let mut present = 0;
    let mut weight = None;
    let mut endpoint = None;
    for &(_, end, score) in &matches {
        if let Some(score) = score {
            present += 1;
            weight = Some(weight.unwrap_or(0) + i128::from(score));
        }
        if let Some(score) = [Some(-3_i128), None, Some(7)][end] {
            endpoint = Some(endpoint.unwrap_or(0) + score);
        }
    }
    vec![GraphAggregateValue::Count(matches.len() as u64), GraphAggregateValue::Count(present),
        sum(weight), sum(endpoint)]
}

#[test]
fn native_edge_and_multihop_aggregates_preserve_bags_orientation_and_exact_domains() {
    let ((), report) = run_async_under_lab(0xa66e_3001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for direction in 0..3 {
            for hops in 1..=3 {
                let mut pattern = "(a:L)".to_owned();
                for at in 0..hops {
                    let end = ["b", "c", "d"][at];
                    let atom = match direction {
                        0 => format!("-[e{at}:R]->({end}:L)"),
                        1 => format!("<-[e{at}:R]-({end}:L)"),
                        _ => format!("-[e{at}:R]-({end}:L)"),
                    };
                    pattern.push_str(&atom);
                }
                let end = ["b", "c", "d"][hops - 1];
                let text = format!("MATCH {pattern} RETURN COUNT(*) AS total, COUNT(e0.score) AS present, SUM(e0.score) AS weight, SUM({end}.score) AS endpoint");
                let params = GqlParameters::new();
                let QueryResult::Rows { columns, rows } = db.query(&cx, &text, &params, symbols(), wide()).unwrap()
                    else { panic!("expected read rows") };
                let mut cursor = db.query_aggregate_stream(&cx, &text, &params, symbols(), wide()).unwrap();
                assert_eq!(cursor.kind(), ScanKind::Edge);
                assert_eq!(cursor.columns(), columns);
                assert_eq!(cursor.row_stats().snapshot_records, 0);
                assert_eq!(cursor.evaluator_stats().work_units, 0);
                let row = cursor.next().unwrap().unwrap();
                assert!(row.keys().is_empty());
                assert_eq!(row.values(), oracle(direction, hops), "{text}");
                assert_eq!(rows, vec![row.values().to_vec()], "{text}");
                assert_eq!(cursor.row_stats().result_rows, 1);
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
        }
        for text in [
            "MATCH (a)-[r:R]->(b)-[s:R]->(c)-[t:R]->(a) RETURN COUNT(*) AS total, SUM(r.score) AS weight",
            "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) WHERE c.score>0 } RETURN COUNT(*) AS total, SUM(r.score) AS weight",
            "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R]->(c) WHERE c.score>0 } RETURN COUNT(*) AS total, SUM(r.score) AS weight",
        ] {
            let params = GqlParameters::new();
            let QueryResult::Rows { rows, .. } = db.query(&cx, text, &params, symbols(), wide()).unwrap()
                else { panic!("expected read rows") };
            let mut cursor = db.query_aggregate_stream(&cx, text, &params, symbols(), wide()).unwrap();
            assert_eq!(cursor.kind(), ScanKind::Edge);
            assert_eq!(rows, vec![cursor.next().unwrap().unwrap().values().to_vec()]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_templates_freeze_parameters_and_history_without_borrowing_the_writer() {
    let ((), report) = run_async_under_lab(0xa66e_3002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let text = "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ $cut WHERE b.score>=$floor RETURN COUNT(*) AS total, SUM(r.score) AS weight";
        let params = GqlParameters::new().with_uint64("cut", 1).unwrap().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols()).unwrap();
        let mut pinned = prepared.stream_aggregate_in_view(&view, &cx, &params, wide()).unwrap();
        fn requires_send(_: &impl Send) {}
        requires_send(&pinned);
        let mut edit = WriteBatch::new(R);
        edit.add_edge(EId(10), VId(0), VId(u128::MAX), vec![(P, CanonicalScalar::Int(6))]);
        db.write(&commit, edit).await.unwrap();
        for cut in 0..=2 {
            let args = GqlParameters::new().with_uint64("cut", cut).unwrap().with_int64("floor", 0).unwrap();
            let QueryResult::Rows { rows, .. } = prepared.execute(&db, &cx, &args, wide()).unwrap()
                else { panic!("expected read rows") };
            let mut stream = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
            assert_eq!(stream.snapshot_seq(), CommitSeq(cut));
            assert_eq!(rows, vec![stream.next().unwrap().unwrap().values().to_vec()]);
        }
        let future = GqlParameters::new().with_uint64("cut", 2).unwrap().with_int64("floor", 0).unwrap();
        let refusal = prepared.stream_aggregate_in_view(&view, &cx, &future, GqlQueryPolicy::new(0, 0, 0, 0)).unwrap_err();
        assert!(matches!(&refusal, QueryError::EdgeAggregateStream(GqlQueryError::Source(
            GraphAggregateError::Source(EdgeScanError::Source(_))
        ))));
        assert!(std::error::Error::source(&refusal).is_some());
        let empty = GqlParameters::new().with_uint64("cut", 1).unwrap().with_int64("floor", 1_000).unwrap();
        let mut none = prepared.stream_aggregate(&db, &cx, &empty, wide()).unwrap();
        let mut current = prepared.stream_aggregate(&db, &cx, &future, wide()).unwrap();
        drop(prepared); drop(params); drop(future); drop(empty); drop(view); drop(db);
        for (stream, count, weight) in [(&mut pinned, 1, -2), (&mut current, 2, 4)] {
            let row = stream.next().unwrap().unwrap();
            assert_eq!(row.values()[0].as_count(), Some(count));
            assert_eq!(row.values()[1].as_integer(), Some(weight));
        }
        let row = none.next().unwrap().unwrap();
        assert_eq!(row.values()[0].as_count(), Some(0));
        assert!(row.values()[1].is_null());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_edge_aggregate_quotas_late_errors_and_close_keep_one_cumulative_cursor() {
    let ((), report) = run_async_under_lab(0xa66e_3003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN COUNT(*) AS total, SUM(r.score) AS weight";
        let params = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols()).unwrap();
        let mut full = prepared.stream_aggregate(&db, &cx, &params, wide()).unwrap();
        let expected = full.next().unwrap().unwrap();
        let records = full.row_stats().snapshot_records;
        let work = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(records, 1, work.work_units, work.scratch_entries);
        assert_eq!(prepared.stream_aggregate(&db, &cx, &params, exact).unwrap().next().unwrap().unwrap(), expected);
        for policy in [
            GqlQueryPolicy::new(records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, work.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, work.scratch_entries - 1),
        ] {
            let mut stream = prepared.stream_aggregate(&db, &cx, &params, policy).unwrap();
            assert!(matches!(stream.next(), Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)))));
            assert_eq!(stream.kind(), ScanKind::Edge);
            assert_eq!(stream.state(), VertexScanState::Failed);
            assert_eq!(stream.row_stats().result_rows, 0);
            let stats = (stream.row_stats(), stream.evaluator_stats());
            assert!(stream.next().is_none()); stream.close();
            assert_eq!((stream.row_stats(), stream.evaluator_stats()), stats);
        }
        let mut closed = prepared.stream_aggregate(&db, &cx, &params, wide()).unwrap();
        closed.close(); closed.close();
        assert_eq!(closed.state(), VertexScanState::Closed);
        assert_eq!(closed.row_stats().snapshot_records, 0);
        assert_eq!(closed.evaluator_stats().work_units, 0);
        assert!(closed.next().is_none());
        let mut edit = WriteBatch::new(R);
        edit.add_edge(EId(11), VId(1), VId(0), vec![(P, CanonicalScalar::ucs_basic_text("private incompatible edge").unwrap())]);
        db.write(&commit, edit).await.unwrap();
        let mut invalid = prepared.stream_aggregate(&db, &cx, &params, wide()).unwrap();
        assert!(matches!(invalid.next(), Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerSum { aggregate: 1 }
        )))));
        assert_eq!(invalid.state(), VertexScanState::Failed);
        assert_eq!(invalid.row_stats().result_rows, 0);
        assert!(!format!("{invalid:?}").contains("private incompatible edge"));
        assert!(invalid.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_edge_shapes_and_bad_arguments_never_switch_to_vertex_or_eager_execution() {
    let ((), report) = run_async_under_lab(0xa66e_3004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let db = Database::open_memory(&commit, keys()).await.unwrap();
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r.score) AS total",
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS total LIMIT 0",
            "MATCH (a)-[r:R]->(b) RETURN b.score AS score, COUNT(*) AS total GROUP BY b.score",
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS total HAVING total>0",
        ] {
            let prepared = PreparedNativeRead::prepare(text, &GqlParameters::new(), symbols()).unwrap();
            let error = prepared.stream_aggregate(&db, &cx, &GqlParameters::new(), GqlQueryPolicy::new(0, 0, 0, 0)).unwrap_err();
            assert!(matches!(&error, QueryError::EdgeAggregateStreamPlan(_)), "{text}: {error}");
            assert!(std::error::Error::source(&error).is_some());
        }
        let text = "MATCH (a)-[r:R]->(b) WHERE b.score >= $floor RETURN COUNT(*) AS total";
        let params = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols()).unwrap();
        assert!(matches!(prepared.stream_aggregate(&db, &cx, &GqlParameters::new(), wide()), Err(QueryError::PatternText(_))));
        let wrong = GqlParameters::new().with_bool("floor", true).unwrap();
        assert!(matches!(prepared.stream_aggregate(&db, &cx, &wrong, wide()), Err(QueryError::PatternText(_))));
        let mut vertex = db.query_aggregate_stream(&cx, "MATCH (n:L) RETURN COUNT(*) AS total", &GqlParameters::new(), symbols(), wide()).unwrap();
        assert_eq!(vertex.kind(), ScanKind::Vertex);
        assert_eq!(vertex.next().unwrap().unwrap().values()[0].as_count(), Some(0));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
