//! Public native edge GROUP BY must use the same pinned join and exact cells.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, PreparedNativeRead, QueryResult, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateTextSlot,
    GraphAggregateValue, RelationBind,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}
fn symbols() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_property("score", P)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn seed() -> WriteBatch {
    let mut b = WriteBatch::new(R);
    for (id, value) in [(0, Some(2)), (1, None), (u128::MAX, Some(7))] {
        b.create_vertex(
            VId(id),
            vec![],
            vec![(P, value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))],
        );
    }
    for (id, a, z, value) in [
        (1, 0, 1, 5),
        (2, 0, 1, 5),
        (3, u128::MAX, 1, 1),
        (4, 1, u128::MAX, -2),
    ] {
        b.add_edge(
            EId(id),
            VId(a),
            VId(z),
            vec![(P, CanonicalScalar::Int(value))],
        );
    }
    b
}
fn cells(
    row: &fgdb_gql::GraphAggregateRow,
    slots: &[GraphAggregateTextSlot],
) -> Vec<GraphAggregateValue> {
    slots
        .iter()
        .map(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(i) => {
                GraphAggregateValue::Value(row.keys()[i].clone())
            }
            GraphAggregateTextSlot::Aggregate(i) => row.values()[i].clone(),
        })
        .collect()
}

#[test]
fn native_edge_groups_keep_textual_layouts_and_all_exact_aggregate_domains() {
    let ((), report) = run_async_under_lab(0xa66e_5101, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let query = cx.query();
        let commit = cx.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS total, AVG(r.score) AS mean, MIN(r.score) AS low, MAX(r.score) AS high, COUNT(DISTINCT r.score) AS support, SUM(DISTINCT r.score) AS total_distinct, AVG(DISTINCT r.score) AS mean_distinct",
            "MATCH (a)-[r:R]->(b)-[:R]->(c) RETURN COUNT(*) AS total, b AS middle, AVG(r.score) AS mean, COUNT(DISTINCT r.score) AS support GROUP BY b",
            "MATCH (a)-[r:R]->(b)-[:R]->(c) RETURN a AS source, AVG(DISTINCT r.score) AS mean, b.score AS key, MIN(a) AS first, MAX(a) AS last GROUP BY a, b.score",
            "MATCH (a)-[r:R]-(b) RETURN r.score AS score, COUNT(*) AS total, b AS target, SUM(r.score) AS sum GROUP BY r.score, b",
            "MATCH (a)-[r:R]->(b)-[:R]->(c) RETURN r AS edge, COUNT(DISTINCT c) AS targets, AVG(r.score) AS mean GROUP BY r",
        ] {
            let args = GqlParameters::new();
            let QueryResult::Rows { columns, rows } =
                db.query(&query, text, &args, symbols(), wide()).unwrap()
            else {
                panic!("aggregate read returned a write");
            };
            let mut cursor = db
                .query_aggregate_stream(&query, text, &args, symbols(), wide())
                .unwrap();
            assert_eq!(cursor.kind(), ScanKind::Edge);
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            let slots = cursor.output_slots().to_vec();
            let actual: Vec<_> = cursor
                .by_ref()
                .map(|row| cells(&row.unwrap(), &slots))
                .collect();
            assert_eq!(actual, rows);
            assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_edge_history_parameters_and_views_survive_writer_and_template_drop() {
    let ((), report) = run_async_under_lab(0xa66e_5102, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let query = cx.query();
        let commit = cx.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (a)-[r:R]->(b)-[:R]->(c) FOR SYSTEM_TIME AS OF SEQ $cut WHERE r.score >= $floor RETURN b AS middle, COUNT(*) AS total, AVG(DISTINCT r.score) AS mean GROUP BY b";
        let args = GqlParameters::new()
            .with_uint64("cut", 1)
            .unwrap()
            .with_int64("floor", 0)
            .unwrap();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        let view = db.read_session().unwrap();
        let QueryResult::Rows { rows: expected, .. } =
            prepared.execute(&db, &query, &args, wide()).unwrap()
        else {
            panic!("not rows");
        };
        let mut retained = prepared
            .stream_aggregate_in_view(&view, &query, &args, wide())
            .unwrap();
        let slots = retained.output_slots().to_vec();
        let mut edit = WriteBatch::new(R);
        edit.delete_edge(EId(2));
        edit.add_edge(EId(5), VId(0), VId(1), vec![(P, CanonicalScalar::Int(9))]);
        db.write(&commit, edit).await.unwrap();
        for cut in 0..=2 {
            let args = GqlParameters::new()
                .with_uint64("cut", cut)
                .unwrap()
                .with_int64("floor", 0)
                .unwrap();
            let QueryResult::Rows { rows, .. } =
                prepared.execute(&db, &query, &args, wide()).unwrap()
            else {
                panic!("not rows");
            };
            let mut cursor = prepared
                .stream_aggregate(&db, &query, &args, wide())
                .unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            let positions = cursor.output_slots().to_vec();
            assert_eq!(
                cursor
                    .by_ref()
                    .map(|row| cells(&row.unwrap(), &positions))
                    .collect::<Vec<_>>(),
                rows
            );
        }
        let future = GqlParameters::new()
            .with_uint64("cut", 2)
            .unwrap()
            .with_int64("floor", 0)
            .unwrap();
        assert!(
            prepared
                .stream_aggregate_in_view(&view, &query, &future, GqlQueryPolicy::new(0, 0, 0, 0))
                .is_err()
        );
        drop(prepared);
        drop(view);
        drop(args);
        drop(db);
        assert_eq!(
            retained
                .by_ref()
                .map(|row| cells(&row.unwrap(), &slots))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(retained.snapshot_seq(), CommitSeq(1));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_group_quotas_and_late_domain_failure_do_not_publish_partial_groups() {
    let ((), report) = run_async_under_lab(0xa66e_5103, |root| async move {
        let cx = PurposeContexts::narrow_runtime_root(&root);
        let query = cx.query();
        let commit = cx.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (a)-[r:R]->(b) RETURN b AS target, COUNT(*) AS total, AVG(DISTINCT r.score) AS mean GROUP BY b";
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        let mut baseline = prepared
            .stream_aggregate(&db, &query, &args, wide())
            .unwrap();
        let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let r = baseline.row_stats();
        let e = baseline.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            r.snapshot_records,
            r.result_rows,
            e.work_units,
            e.scratch_entries,
        );
        assert_eq!(
            prepared
                .stream_aggregate(&db, &query, &args, exact)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected
        );
        let mut short = prepared
            .stream_aggregate(
                &db,
                &query,
                &args,
                GqlQueryPolicy::new(u64::MAX, r.result_rows - 1, u64::MAX, u64::MAX),
            )
            .unwrap();
        assert!(matches!(short.next(), Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(short.row_stats().result_rows, 0);
        assert!(short.next().is_none());
        let mut edit = WriteBatch::new(R);
        edit.add_edge(
            EId(6),
            VId(0),
            VId(1),
            vec![(P, CanonicalScalar::Bool(true))],
        );
        db.write(&commit, edit).await.unwrap();
        let mut invalid = prepared
            .stream_aggregate(&db, &query, &args, wide())
            .unwrap();
        assert!(matches!(
            invalid.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerAverage { aggregate: 1 }
            )))
        ));
        assert_eq!(invalid.row_stats().result_rows, 0);
        assert_eq!(invalid.state(), VertexScanState::Failed);
        assert!(invalid.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
