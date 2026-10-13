//! Vertex completed-output clauses through real native binding and snapshots.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateTextSlot,
    GraphAggregateValue, GraphExactAverage, RelationBind,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const L: LabelId = LabelId(1);
const G: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn symbols() -> RelationBind {
    RelationBind::new()
        .with_label("L", L)
        .with_property("g", G)
        .with_property("p", P)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, g, p) in [
        (0, Some(0), Some(7)),
        (1, Some(0), None),
        (2, Some(1), Some(2)),
        (3, Some(2), Some(2)),
        (4, None, Some(-3)),
        (u128::MAX, Some(3), Some(7)),
    ] {
        let mut props = Vec::new();
        if let Some(g) = g {
            props.push((G, CanonicalScalar::Int(g)));
        }
        if let Some(p) = p {
            props.push((P, CanonicalScalar::Int(p)));
        }
        batch.create_vertex(VId(id), vec![L], props);
    }
    batch
}
fn cells(
    row: &fgdb_gql::GraphAggregateRow,
    slots: &[GraphAggregateTextSlot],
) -> Vec<GraphAggregateValue> {
    slots
        .iter()
        .map(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(at) => {
                GraphAggregateValue::Value(row.keys()[at].clone())
            }
            GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
        })
        .collect()
}
fn rows(result: QueryResult) -> Vec<Vec<GraphAggregateValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected read")
    };
    rows
}
const PROJECTED: &str = "MATCH (v:L) RETURN COUNT(*) AS total,v.g AS first,v.g AS again,SUM(v.p)+COUNT(*) AS adjusted,AVG(v.p) AS mean GROUP BY v.g HAVING total >= $floor ORDER BY mean DESC NULLS LAST,total DESC SKIP $skip LIMIT $limit";
fn args(skip: u64, limit: u64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("floor", 1)
        .unwrap()
        .with_uint64("skip", skip)
        .unwrap()
        .with_uint64("limit", limit)
        .unwrap()
}

#[test]
fn native_layouts_and_pinned_cursors_survive_writes_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x7678_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let parameters = args(0, 3);
        let expected = rows(
            view.query(&cx, PROJECTED, &parameters, symbols(), wide())
                .unwrap(),
        );
        let prepared = PreparedNativeRead::prepare(PROJECTED, &parameters, symbols()).unwrap();
        let mut cursor = prepared
            .stream_aggregate(&db, &cx, &parameters, wide())
            .unwrap();
        assert_eq!(cursor.kind(), ScanKind::Vertex);
        assert_eq!(cursor.snapshot_seq(), basis);
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        let slots = cursor.output_slots().to_vec();
        let names = cursor.columns().to_vec();
        assert_eq!(names, vec!["total", "first", "again", "adjusted", "mean"]);
        drop(prepared);
        drop(parameters);
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
        update.delete_vertex(VId(2));
        db.write(&commit, update).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        // The unpolled cursor retains its original generation and bindings.
        let actual = cursor
            .by_ref()
            .map(|row| cells(&row.unwrap(), &slots))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert_eq!(cursor.row_stats().result_rows, 3);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(cursor.next().is_none());
        for skip in [0, 1, 9, u64::MAX] {
            for limit in [0, 1, 3] {
                let parameters = args(skip, limit);
                let expected = rows(
                    db.query(&cx, PROJECTED, &parameters, symbols(), wide())
                        .unwrap(),
                );
                let mut cursor = db
                    .query_aggregate_stream(&cx, PROJECTED, &parameters, symbols(), wide())
                    .unwrap();
                let slots = cursor.output_slots().to_vec();
                assert_eq!(
                    cursor
                        .by_ref()
                        .map(|row| cells(&row.unwrap(), &slots))
                        .collect::<Vec<_>>(),
                    expected
                );
                let expected = rows(
                    view.query(&cx, PROJECTED, &parameters, symbols(), wide())
                        .unwrap(),
                );
                let mut cursor = view
                    .query_aggregate_stream(&cx, PROJECTED, &parameters, symbols(), wide())
                    .unwrap();
                let slots = cursor.output_slots().to_vec();
                assert_eq!(
                    cursor
                        .by_ref()
                        .map(|row| cells(&row.unwrap(), &slots))
                        .collect::<Vec<_>>(),
                    expected
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_distinct_and_hidden_rank_keep_exact_cells_and_select_unique_classes_before_pages() {
    let ((), report) = run_async_under_lab(0x7678_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let text = "MATCH (v:L) RETURN DISTINCT AVG(v.p) AS mean GROUP BY v.g ORDER BY SUM(v.p) DESC SKIP 1 LIMIT 1";
        let mut cursor = db
            .query_aggregate_stream(&cx, text, &parameters, symbols(), wide())
            .unwrap();
        let row = cursor.next().unwrap().unwrap();
        // Independent fixture result: classes 7/1, 2/1, -3/1, in that order.
        assert_eq!(
            row.values(),
            &[GraphAggregateValue::Average(
                GraphExactAverage::new(2, 1).unwrap()
            )]
        );
        assert!(cursor.next().is_none());
        for text in [
            "MATCH (v:L) RETURN COUNT(*) AS total HAVING total>0 ORDER BY total LIMIT 1",
            "MATCH (v:L) RETURN v AS id,COUNT(*) AS total GROUP BY v ORDER BY id DESC LIMIT 1",
            "MATCH (v:L) RETURN DISTINCT COUNT(*) AS total GROUP BY v.g ORDER BY SUM(v.p) DESC LIMIT 2",
            "MATCH (v:L) RETURN DISTINCT SUM(v.p+1) AS total GROUP BY v.g HAVING total>0 ORDER BY total DESC",
            "MATCH (v:L) RETURN AVG(DISTINCT v.p) AS mean GROUP BY v.g HAVING COUNT(*)>0 ORDER BY mean DESC NULLS FIRST",
            "MATCH (v:L) RETURN v.g AS first,v.g AS again,COUNT(*) AS total GROUP BY v.g ORDER BY total DESC LIMIT 2",
        ] {
            let expected = rows(db.query(&cx, text, &parameters, symbols(), wide()).unwrap());
            let mut cursor = db
                .query_aggregate_stream(&cx, text, &parameters, symbols(), wide())
                .unwrap();
            let slots = cursor.output_slots().to_vec();
            let actual = cursor
                .by_ref()
                .map(|row| cells(&row.unwrap(), &slots))
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{text}");
        }
        let text =
            "MATCH (v:L) RETURN v AS id,COUNT(*) AS total GROUP BY v ORDER BY id DESC LIMIT 1";
        let mut cursor = db
            .query_aggregate_stream(&cx, text, &parameters, symbols(), wide())
            .unwrap();
        assert_eq!(
            cursor.next().unwrap().unwrap().keys(),
            &[GraphValue::Vertex(VId(u128::MAX))]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_selected_quotas_and_late_failures_preserve_error_domains_and_completion() {
    let ((), report) = run_async_under_lab(0x7678_0103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 0..128 {
            batch.create_vertex(
                VId(id),
                vec![L],
                vec![
                    (G, CanonicalScalar::Int(id as i64)),
                    (P, CanonicalScalar::Int(1)),
                ],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let parameters = GqlParameters::new();
        for statement in [
            "MATCH (v:L) WITH DISTINCT v.p AS p RETURN SUM(p) AS total",
            "MATCH (v:L) WITH v.p AS p LIMIT 1 RETURN COUNT(*) AS total",
        ] {
            // The native grammar admits this complete relation, but the
            // vertex-only physical reducer must not substitute its first leaf.
            let prepared = PreparedNativeRead::prepare(statement, &parameters, symbols()).unwrap();
            assert!(
                prepared
                    .stream_aggregate(&db, &cx, &parameters, wide())
                    .is_err()
            );
        }
        let text = "MATCH (v:L) RETURN DISTINCT COUNT(*) AS total GROUP BY v.g ORDER BY SUM(v.p) DESC LIMIT 1";
        let mut full = db
            .query_aggregate_stream(&cx, text, &parameters, symbols(), wide())
            .unwrap();
        let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let r = full.row_stats();
        let e = full.evaluator_stats();
        assert_eq!(expected.len(), 1);
        let exact = GqlQueryPolicy::new(r.snapshot_records, 1, e.work_units, e.scratch_entries);
        assert_eq!(
            db.query_aggregate_stream(&cx, text, &parameters, symbols(), exact)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected
        );
        for p in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, e.work_units, e.scratch_entries - 1),
        ] {
            let mut cursor = db
                .query_aggregate_stream(&cx, text, &parameters, symbols(), p)
                .unwrap();
            assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let mut untouched = db
            .query_aggregate_stream(&cx, text, &parameters, symbols(), wide())
            .unwrap();
        untouched.close();
        assert_eq!(untouched.row_stats().snapshot_records, 0);
        assert!(untouched.next().is_none());
        for limit in [0, 1] {
            let text = format!(
                "MATCH (v:L) RETURN 1/(COUNT(*)-1) AS bad GROUP BY v.g ORDER BY v.g LIMIT {limit}"
            );
            let mut cursor = db
                .query_aggregate_stream(&cx, &text, &parameters, symbols(), wide())
                .unwrap();
            assert!(matches!(
                cursor.next(),
                Some(Err(GqlQueryError::Source(
                    GraphAggregateError::OutputExpression { .. }
                )))
            ));
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert!(cursor.next().is_none());
        }
        let future = "MATCH (v:L) FOR SYSTEM_TIME AS OF SEQ 2 RETURN COUNT(*) AS total ORDER BY total LIMIT 0";
        assert!(
            db.query_aggregate_stream(&cx, future, &parameters, symbols(), wide())
                .is_err()
        );
        let mut bad = WriteBatch::new(RelationId(1));
        bad.set_vertex_property(VId(127), P, Some(CanonicalScalar::Bool(true)));
        db.write(&commit, bad).await.unwrap();
        let text = "MATCH (v:L) RETURN SUM(v.p) AS total GROUP BY v.g ORDER BY total LIMIT 0";
        let mut cursor = db
            .query_aggregate_stream(&cx, text, &parameters, symbols(), wide())
            .unwrap();
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerSum { .. }
            )))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
