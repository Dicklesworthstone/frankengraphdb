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

#[path = "native_edge_aggregate_stream/statistics.rs"]
mod statistics;

const L: LabelId = LabelId(3);
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(7);
const IDS: [u128; 3] = [0, 1, u128::MAX];
const EDGES: [(usize, usize, Option<i64>); 6] = [
    (0, 1, Some(5)),
    (0, 1, None),
    (1, 2, Some(-2)),
    (2, 0, Some(1)),
    (1, 1, None),
    (0, 0, Some(-4)),
];
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn symbols() -> RelationBind {
    RelationBind::new()
        .with_label("L", L)
        .with_relation("R", R)
        .with_property("score", P)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, score) in IDS.into_iter().zip([Some(-3), None, Some(7)]) {
        batch.create_vertex(
            VId(id),
            vec![L],
            vec![(P, score.map_or(CanonicalScalar::Null, CanonicalScalar::Int))],
        );
    }
    // An isolate must not enter an edge aggregate's match domain.
    batch.create_vertex(VId(42), vec![L], vec![(P, CanonicalScalar::Int(99))]);
    for (at, (from, to, score)) in EDGES.into_iter().enumerate() {
        let id = if at == 5 { u128::MAX } else { at as u128 };
        batch.add_edge(
            EId(id),
            VId(IDS[from]),
            VId(IDS[to]),
            score
                .map(|value| vec![(P, CanonicalScalar::Int(value))])
                .unwrap_or_default(),
        );
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
    tree_oracle(&vec![direction; hops], &(0..hops).collect::<Vec<_>>())
}

fn tree_oracle(directions: &[usize], parents: &[usize]) -> Vec<GraphAggregateValue> {
    assert_eq!(directions.len(), parents.len());
    assert!(!parents.is_empty());
    // One MATCH binds its edges under DIFFERENT EDGES. Both orientations of an
    // undirected edge share its index, so no match uses a stored edge twice.
    // Enumerate the full Cartesian assignment bag, retaining every binding;
    // the production cursor instead resumes an indexed depth-first traversal.
    let mut matches = vec![(Vec::new(), Vec::new(), None)];
    for (at, (&direction, &parent)) in directions.iter().zip(parents).enumerate() {
        let mut oriented = Vec::new();
        for (edge, (from, to, score)) in EDGES.into_iter().enumerate() {
            if direction == 1 {
                oriented.push((edge, to, from, score));
            } else {
                oriented.push((edge, from, to, score));
                if direction == 2 && from != to {
                    oriented.push((edge, to, from, score));
                }
            }
        }
        let mut extended = Vec::new();
        for (used, vertices, score) in &matches {
            for &(edge, from, to, first_score) in &oriented {
                if (at == 0 || vertices[parent] == from) && !used.contains(&edge) {
                    let mut used = used.clone();
                    used.push(edge);
                    let mut vertices = vertices.clone();
                    if at == 0 {
                        vertices.push(from);
                    }
                    vertices.push(to);
                    extended.push((used, vertices, if at == 0 { first_score } else { *score }));
                }
            }
        }
        matches = extended;
    }
    let mut present = 0;
    let mut weight = None;
    let mut endpoint = None;
    for (_, vertices, score) in &matches {
        if let Some(score) = score {
            present += 1;
            weight = Some(weight.unwrap_or(0) + i128::from(*score));
        }
        let end = *vertices.last().unwrap();
        if let Some(score) = [Some(-3_i128), None, Some(7)][end] {
            endpoint = Some(endpoint.unwrap_or(0) + score);
        }
    }
    vec![
        GraphAggregateValue::Count(matches.len() as u64),
        GraphAggregateValue::Count(present),
        sum(weight),
        sum(endpoint),
    ]
}

#[test]
fn native_edge_and_multihop_aggregates_preserve_bags_orientation_and_exact_domains() {
    let ((), report) = run_async_under_lab(0xa66e_3001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        for direction in 0..3 {
            for hops in 1..=5 {
                let mut pattern = "(a:L)".to_owned();
                for at in 0..hops {
                    let end = ["b", "c", "d", "e", "f"][at];
                    let atom = match direction {
                        0 => format!("-[e{at}:R]->({end}:L)"),
                        1 => format!("<-[e{at}:R]-({end}:L)"),
                        _ => format!("-[e{at}:R]-({end}:L)"),
                    };
                    pattern.push_str(&atom);
                }
                let end = ["b", "c", "d", "e", "f"][hops - 1];
                let text = format!(
                    "MATCH {pattern} RETURN COUNT(*) AS total, COUNT(e0.score) AS present, SUM(e0.score) AS weight, SUM({end}.score) AS endpoint"
                );
                let params = GqlParameters::new();
                let QueryResult::Rows { columns, rows } =
                    db.query(&cx, &text, &params, symbols(), wide()).unwrap()
                else {
                    panic!("expected read rows")
                };
                let mut cursor = db
                    .query_aggregate_stream(&cx, &text, &params, symbols(), wide())
                    .unwrap();
                assert_eq!(cursor.kind(), ScanKind::Edge);
                assert_eq!(cursor.columns(), columns);
                assert_eq!(cursor.row_stats().snapshot_records, 0);
                assert_eq!(cursor.evaluator_stats().work_units, 0);
                let row = cursor.next().unwrap().unwrap();
                assert!(row.keys().is_empty());
                assert_eq!(row.values(), oracle(direction, hops), "{text}");
                assert_eq!(rows, vec![row.values().to_vec()], "{text}");
                if hops >= 2 {
                    let mut resolver = symbols();
                    let prepared =
                        fgdb_gql::PreparedGraphAggregateText::prepare(&text, |kind, name| {
                            fgdb_gql::GraphSymbolResolver::resolve_symbol(&mut resolver, kind, name)
                        })
                        .unwrap();
                    let definition = prepared.bind_parameters(&params).unwrap();
                    assert!(matches!(
                        fgdb_gql::scan_stream::AggregateScanPlan::compile_numeric(&definition),
                        Some(fgdb_gql::scan_stream::AggregateScanPlan::Edge(_))
                    ));
                    let stats = cursor.row_stats();
                    let work = cursor.evaluator_stats();
                    // Incident candidates share this allowance with root
                    // histories; it is not just the stored edge count.
                    let exact = GqlQueryPolicy::new(
                        stats.snapshot_records,
                        stats.result_rows,
                        work.work_units,
                        work.scratch_entries,
                    );
                    let ordinary = db
                        .execute_graph_aggregate_governed(&cx, &definition, exact)
                        .unwrap();
                    assert_eq!(ordinary.value, vec![row.clone()]);
                    assert_eq!(ordinary.rows, stats);
                    assert_eq!(ordinary.evaluator, work);
                    let expected = QueryResult::Rows {
                        columns: columns.clone(),
                        rows: rows.clone(),
                    };
                    assert_eq!(
                        db.query(&cx, &text, &params, symbols(), exact).unwrap(),
                        expected
                    );
                    assert_eq!(
                        view.query(&cx, &text, &params, symbols(), exact).unwrap(),
                        expected
                    );
                }
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
            let QueryResult::Rows { rows, .. } =
                db.query(&cx, text, &params, symbols(), wide()).unwrap()
            else {
                panic!("expected read rows")
            };
            let mut cursor = db
                .query_aggregate_stream(&cx, text, &params, symbols(), wide())
                .unwrap();
            assert_eq!(cursor.kind(), ScanKind::Edge);
            assert_eq!(
                rows,
                vec![cursor.next().unwrap().unwrap().values().to_vec()]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordinary_branching_numeric_aggregates_use_one_indexed_cursor_and_match_assignments() {
    let ((), report) = run_async_under_lab(0xa66e_3011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let cases: [(&str, &str, &[usize], &[usize]); 4] = [
            (
                "(a:L)-[r:R]->(b:L), (a)-[s:R]->(c:L)",
                "c",
                &[0, 0],
                &[0, 0],
            ),
            (
                "(a)-[r:R]->(b), (a)-[s:R]->(c), (b)-[t:R]->(d), (c)-[u:R]->(e)",
                "e",
                &[0, 0, 0, 0],
                &[0, 0, 1, 2],
            ),
            (
                "(a)<-[r:R]-(b), (a)-[s:R]->(c), (b)<-[t:R]-(d)",
                "d",
                &[1, 0, 1],
                &[0, 0, 1],
            ),
            (
                "(a)-[r:R]-(b), (b)<-[s:R]-(c), (a)-[t:R]->(d), (c)-[u:R]-(e)",
                "e",
                &[2, 1, 0, 2],
                &[0, 1, 0, 2],
            ),
        ];
        for (pattern, end, directions, parents) in cases {
            let text = format!(
                "MATCH {pattern} RETURN COUNT(*) AS total, COUNT(r.score) AS present, SUM(r.score) AS weight, SUM({end}.score) AS endpoint"
            );
            let params = GqlParameters::new();
            let mut resolver = symbols();
            let template = fgdb_gql::PreparedGraphAggregateText::prepare(&text, |kind, name| {
                fgdb_gql::GraphSymbolResolver::resolve_symbol(&mut resolver, kind, name)
            })
            .unwrap();
            let definition = template.bind_parameters(&params).unwrap();
            assert!(matches!(
                fgdb_gql::scan_stream::AggregateScanPlan::compile_numeric(&definition),
                Some(fgdb_gql::scan_stream::AggregateScanPlan::Edge(_))
            ));
            let mut cursor = db
                .query_aggregate_stream(&cx, &text, &params, symbols(), wide())
                .unwrap();
            assert_eq!(cursor.kind(), ScanKind::Edge);
            let row = cursor.next().unwrap().unwrap();
            assert_eq!(row.values(), tree_oracle(directions, parents), "{text}");
            let stats = cursor.row_stats();
            let work = cursor.evaluator_stats();
            let exact = GqlQueryPolicy::new(
                stats.snapshot_records,
                stats.result_rows,
                work.work_units,
                work.scratch_entries,
            );
            let ordinary = db
                .execute_graph_aggregate_governed(&cx, &definition, exact)
                .unwrap();
            assert_eq!(ordinary.value, vec![row.clone()], "{text}");
            assert_eq!(ordinary.rows, stats, "{text}");
            assert_eq!(ordinary.evaluator, work, "{text}");
            let expected = QueryResult::Rows {
                columns: cursor.columns().to_vec(),
                rows: vec![row.values().to_vec()],
            };
            assert_eq!(
                db.query(&cx, &text, &params, symbols(), exact).unwrap(),
                expected,
                "{text}"
            );
            assert_eq!(
                view.query(&cx, &text, &params, symbols(), exact).unwrap(),
                expected,
                "{text}"
            );
            assert!(cursor.next().is_none());
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_templates_freeze_parameters_and_history_without_borrowing_the_writer() {
    let ((), report) = run_async_under_lab(0xa66e_3002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let text = "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ $cut WHERE b.score>=$floor RETURN COUNT(*) AS total, SUM(r.score) AS weight";
        let params = GqlParameters::new()
            .with_uint64("cut", 1)
            .unwrap()
            .with_int64("floor", 0)
            .unwrap();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols()).unwrap();
        let mut pinned = prepared
            .stream_aggregate_in_view(&view, &cx, &params, wide())
            .unwrap();
        fn requires_send(_: &impl Send) {}
        requires_send(&pinned);
        let mut edit = WriteBatch::new(R);
        edit.add_edge(
            EId(10),
            VId(0),
            VId(u128::MAX),
            vec![(P, CanonicalScalar::Int(6))],
        );
        db.write(&commit, edit).await.unwrap();
        for cut in 0..=2 {
            let args = GqlParameters::new()
                .with_uint64("cut", cut)
                .unwrap()
                .with_int64("floor", 0)
                .unwrap();
            let QueryResult::Rows { rows, .. } = prepared.execute(&db, &cx, &args, wide()).unwrap()
            else {
                panic!("expected read rows")
            };
            let mut stream = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
            assert_eq!(stream.snapshot_seq(), CommitSeq(cut));
            assert_eq!(
                rows,
                vec![stream.next().unwrap().unwrap().values().to_vec()]
            );
        }
        let future = GqlParameters::new()
            .with_uint64("cut", 2)
            .unwrap()
            .with_int64("floor", 0)
            .unwrap();
        let refusal = prepared
            .stream_aggregate_in_view(&view, &cx, &future, GqlQueryPolicy::new(0, 0, 0, 0))
            .unwrap_err();
        assert!(matches!(
            &refusal,
            QueryError::EdgeAggregateStream(GqlQueryError::Source(GraphAggregateError::Source(
                EdgeScanError::Source(_)
            )))
        ));
        assert!(std::error::Error::source(&refusal).is_some());
        let empty = GqlParameters::new()
            .with_uint64("cut", 1)
            .unwrap()
            .with_int64("floor", 1_000)
            .unwrap();
        let mut none = prepared.stream_aggregate(&db, &cx, &empty, wide()).unwrap();
        let mut current = prepared
            .stream_aggregate(&db, &cx, &future, wide())
            .unwrap();
        drop(prepared);
        drop(params);
        drop(future);
        drop(empty);
        drop(view);
        drop(db);
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
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    let ((), report) = run_async_under_lab(0xa66e_3003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for pattern in [
            "(a)-[r:R]->(b)-[s:R]->(c)",
            "(a)-[r:R]->(b)-[s:R]->(c)-[t:R]->(d)",
            "(a)-[r:R]->(b), (a)-[s:R]->(c), (b)-[t:R]->(d)",
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let basis = db.write(&commit, seed()).await.unwrap();
            let view = db.read_session().unwrap();
            let text = format!(
                "MATCH {pattern} RETURN COUNT(*) AS total, SUM(r.score) AS weight"
            );
            let params = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(&text, &params, symbols()).unwrap();
            let mut full = prepared
                .stream_aggregate(&db, &cx, &params, wide())
                .unwrap();
            let expected = full.next().unwrap().unwrap();
            let records = full.row_stats().snapshot_records;
            let work = full.evaluator_stats();
            let exact = GqlQueryPolicy::new(records, 1, work.work_units, work.scratch_entries);
            assert_eq!(
                prepared
                    .stream_aggregate(&db, &cx, &params, exact)
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap(),
                expected
            );
            let original = prepared.execute(&db, &cx, &params, exact).unwrap();
            let QueryResult::Rows { rows, .. } = &original else {
                panic!("expected aggregate rows");
            };
            assert_eq!(rows, &vec![expected.values().to_vec()]);
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            assert_eq!(
                prepared
                    .execute(
                        &db,
                        &cx.with_checkpoint_probe(Arc::clone(&probe)),
                        &params,
                        exact
                    )
                    .unwrap(),
                original
            );
            let calls = probe.calls();
            assert!(calls > 2);
            for stop in [1, calls / 2, calls] {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                assert!(matches!(
                    prepared.execute(
                        &db,
                        &cx.with_checkpoint_probe(Arc::clone(&probe)),
                        &params,
                        exact,
                    ),
                    Err(QueryError::Aggregate(GqlQueryError::Interrupted(_)))
                ));
                assert_eq!(probe.calls(), stop);
            }
            for policy in [
                GqlQueryPolicy::new(records - 1, 1, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
                GqlQueryPolicy::new(u64::MAX, 1, work.work_units - 1, u64::MAX),
                GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, work.scratch_entries - 1),
            ] {
                let mut stream = prepared
                    .stream_aggregate(&db, &cx, &params, policy)
                    .unwrap();
                assert!(matches!(
                    stream.next(),
                    Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)))
                ));
                assert_eq!(stream.kind(), ScanKind::Edge);
                assert_eq!(stream.state(), VertexScanState::Failed);
                assert_eq!(stream.row_stats().result_rows, 0);
                let stats = (stream.row_stats(), stream.evaluator_stats());
                assert!(stream.next().is_none());
                stream.close();
                assert_eq!((stream.row_stats(), stream.evaluator_stats()), stats);
                assert!(matches!(
                    prepared.execute(&db, &cx, &params, policy),
                    Err(QueryError::Aggregate(
                        GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)
                    ))
                ));
            }
            let mut closed = prepared
                .stream_aggregate(&db, &cx, &params, wide())
                .unwrap();
            closed.close();
            closed.close();
            assert_eq!(closed.state(), VertexScanState::Closed);
            assert_eq!(closed.row_stats().snapshot_records, 0);
            assert_eq!(closed.evaluator_stats().work_units, 0);
            assert!(closed.next().is_none());
            // Every reached predicate evaluates even if a Boolean sibling
            // decides the answer or the final output page is empty.
            for limit in [0, 1] {
                let invalid_filter = format!(
                    "MATCH {pattern} WHERE s.score IS NULL OR 1/(r.score-r.score)>0 RETURN SUM(r.score) AS weight LIMIT {limit}"
                );
                assert!(matches!(
                    db.query(&cx, &invalid_filter, &params, symbols(), wide()),
                    Err(QueryError::Aggregate(GqlQueryError::Data(error)))
                        if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
                ));
            }
            let mut edit = WriteBatch::new(R);
            edit.add_edge(
                EId(11),
                VId(1),
                VId(0),
                vec![(
                    P,
                    CanonicalScalar::ucs_basic_text("private incompatible edge").unwrap(),
                )],
            );
            db.write(&commit, edit).await.unwrap();
            let mut invalid = prepared
                .stream_aggregate(&db, &cx, &params, wide())
                .unwrap();
            assert!(matches!(
                invalid.next(),
                Some(Err(GqlQueryError::Source(
                    GraphAggregateError::NonIntegerSum { aggregate: 1 }
                )))
            ));
            assert_eq!(invalid.state(), VertexScanState::Failed);
            assert_eq!(invalid.row_stats().result_rows, 0);
            assert!(!format!("{invalid:?}").contains("private incompatible edge"));
            assert!(invalid.next().is_none());
            for limit in [0, 1] {
                assert!(matches!(
                    db.query(
                        &cx,
                        &format!("{text} LIMIT {limit}"),
                        &params,
                        symbols(),
                        wide(),
                    ),
                    Err(QueryError::Aggregate(GqlQueryError::Source(
                        GraphAggregateError::NonIntegerSum { aggregate: 1 }
                    )))
                ));
            }
            assert_eq!(
                view.query(&cx, &text, &params, symbols(), exact).unwrap(),
                original
            );
            let historical = format!(
                "MATCH {pattern} FOR SYSTEM_TIME AS OF SEQ {} RETURN COUNT(*) AS total, SUM(r.score) AS weight",
                basis.0
            );
            assert_eq!(
                db.query(&cx, &historical, &params, symbols(), wide())
                    .unwrap(),
                original
            );
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_collections_stream_natively_and_bad_arguments_never_switch_to_vertex_or_eager_execution() {
    let ((), report) = run_async_under_lab(0xa66e_3004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let db = Database::open_memory(&commit, keys()).await.unwrap();
        // A distinct edge-property collection streams natively now (it was
        // refused before the native edge collection cursors; fgdb-4wrnd):
        // the stream is the eager answer, not an eager retry behind it.
        let mut seeded = Database::open_memory(&commit, keys()).await.unwrap();
        seeded.write(&commit, seed()).await.unwrap();
        let text = "MATCH (a)-[r:R]->(b) RETURN COLLECT(DISTINCT r.score) AS total";
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        let QueryResult::Rows { columns, rows } =
            prepared.execute(&seeded, &cx, &args, wide()).unwrap()
        else {
            panic!("{text}: not rows")
        };
        let mut cursor = prepared
            .stream_aggregate(&seeded, &cx, &args, wide())
            .unwrap();
        assert_eq!(cursor.kind(), ScanKind::Edge);
        assert_eq!(cursor.columns(), columns);
        let streamed: Vec<_> = cursor
            .by_ref()
            .map(|row| row.unwrap().values().to_vec())
            .collect();
        assert_eq!(streamed, rows);
        let text = "MATCH (a)-[r:R]->(b) WHERE b.score >= $floor RETURN COUNT(*) AS total";
        let params = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &params, symbols()).unwrap();
        assert!(matches!(
            prepared.stream_aggregate(&db, &cx, &GqlParameters::new(), wide()),
            Err(QueryError::PatternText(_))
        ));
        let wrong = GqlParameters::new().with_bool("floor", true).unwrap();
        assert!(matches!(
            prepared.stream_aggregate(&db, &cx, &wrong, wide()),
            Err(QueryError::PatternText(_))
        ));
        let mut vertex = db
            .query_aggregate_stream(
                &cx,
                "MATCH (n:L) RETURN COUNT(*) AS total",
                &GqlParameters::new(),
                symbols(),
                wide(),
            )
            .unwrap();
        assert_eq!(vertex.kind(), ScanKind::Vertex);
        assert_eq!(
            vertex.next().unwrap().unwrap().values()[0].as_count(),
            Some(0)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
