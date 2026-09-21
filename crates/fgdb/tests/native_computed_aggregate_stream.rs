//! One-row scalar evaluation through native prepared/pinned aggregate streams.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, PreparedNativeRead, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphAggregateRow, GraphAggregateTextSlot, GraphAggregateValue, RelationBind};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(3);
const PRICE: PropertyKeyId = PropertyKeyId(1);
const QTY: PropertyKeyId = PropertyKeyId(2);
const KIND: PropertyKeyId = PropertyKeyId(3);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xd1; 32], DatabaseSecurityNamespaceId([0xd2; 32]), [0xd3; 32])
}
fn symbols() -> RelationBind {
    RelationBind::new().with_relation("R", R).with_label("L", L)
        .with_property("price", PRICE).with_property("qty", QTY).with_property("kind", KIND)
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, price, qty, kind) in [
        (0, Some(2), 3, "A"), (1, Some(-2), 3, "a"),
        (2, None, 5, "B"), (u128::MAX, Some(4), 2, "b"),
    ] {
        batch.create_vertex(VId(id), vec![L], vec![
            (PRICE, price.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
            (QTY, CanonicalScalar::Int(qty)),
            (KIND, CanonicalScalar::ucs_basic_text(kind).unwrap()),
        ]);
    }
    for (id, from, to, price, qty, kind) in [
        (0, 0, 1, Some(2), 3, "A"), (1, 0, 1, Some(-2), 3, "a"),
        (2, 1, 2, None, 5, "B"), (u128::MAX, u128::MAX, 0, Some(4), 2, "b"),
    ] {
        batch.add_edge(EId(id), VId(from), VId(to), vec![
            (PRICE, price.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
            (QTY, CanonicalScalar::Int(qty)),
            (KIND, CanonicalScalar::ucs_basic_text(kind).unwrap()),
        ]);
    }
    batch
}
fn visible(row: &GraphAggregateRow, slots: &[GraphAggregateTextSlot]) -> Vec<GraphAggregateValue> {
    slots.iter().map(|slot| match *slot {
        GraphAggregateTextSlot::GroupKey(at) => GraphAggregateValue::Value(row.keys()[at].clone()),
        GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
    }).collect()
}
const SUMMARY: &str = "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n, LOWER(r.kind) AS bucket, SUM(r.price*r.qty) AS total, AVG(DISTINCT ABS(r.price*r.qty)) AS mean GROUP BY LOWER(r.kind)";

#[test]
fn native_computed_keys_aliases_and_duplicate_arguments_match_batch_and_hand_answers() {
    let ((), report) = run_async_under_lab(0xa66e_c001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for text in [SUMMARY,
            "MATCH (a)-[r:R]->(b)-[:R]->(c) RETURN COALESCE(r.price,0)*2 AS bucket, SUM(r.qty*c.qty) AS total, COUNT(*) AS n GROUP BY COALESCE(r.price,0)*2",
            "MATCH (a)-[r:R]-(b) RETURN SUM(ABS(r.price)) AS total, COUNT(DISTINCT ABS(r.price)) AS support",
            "MATCH (a)-[r:R]->(b) RETURN SUM(COALESCE(1,1/0)) AS total, AVG(COALESCE(r.price,0)) AS mean",
        ] {
            let args = GqlParameters::new();
            let QueryResult::Rows { columns, rows } = db.query(&cx, text, &args, symbols(), wide()).unwrap()
                else { panic!("expected rows") };
            let mut cursor = db.query_aggregate_stream(&cx, text, &args, symbols(), wide()).unwrap();
            assert_eq!(cursor.kind(), ScanKind::Edge);
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            let slots = cursor.output_slots().to_vec();
            let actual = cursor.by_ref().map(|r| visible(&r.unwrap(), &slots)).collect::<Vec<_>>();
            assert_eq!(actual, rows, "{text}");
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        let rows = db.query_aggregate_stream(&cx, SUMMARY, &GqlParameters::new(), symbols(), wide())
            .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].values()[0].as_count(), Some(2));
        assert_eq!(rows[0].values()[1].as_integer(), Some(0));
        assert_eq!(rows[0].values()[2].as_average().unwrap().numerator(), 6);
        assert_eq!(rows[0].values()[2].as_average().unwrap().denominator(), 1);
        assert_eq!(rows[1].values()[1].as_integer(), Some(8));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_parameters_and_temporal_cuts_are_frozen_without_borrowing_the_writer() {
    let ((), report) = run_async_under_lab(0xa66e_c002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let text = "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ $cut RETURN SUM(r.price*$scale) AS total, COUNT(COALESCE(r.kind,$fallback)) AS n";
        let args = GqlParameters::new().with_uint64("cut", 1).unwrap()
            .with_int64("scale", 3).unwrap().with_text("fallback", "' $literal").unwrap();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        let mut pinned = prepared.stream_aggregate_in_view(&view, &cx, &args, wide()).unwrap();
        fn is_send(_: &impl Send) {} is_send(&pinned);
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(EId(0), PRICE, Some(CanonicalScalar::Int(10)));
        db.write(&commit, edit).await.unwrap();
        for cut in 0..=2 {
            let params = GqlParameters::new().with_uint64("cut", cut).unwrap()
                .with_int64("scale", -2).unwrap().with_text("fallback", "text").unwrap();
            let QueryResult::Rows { rows, .. } = prepared.execute(&db, &cx, &params, wide()).unwrap()
                else { panic!("expected rows") };
            let mut cursor = prepared.stream_aggregate(&db, &cx, &params, wide()).unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            assert_eq!(rows, vec![cursor.next().unwrap().unwrap().values().to_vec()]);
            if cut == 2 {
                assert!(matches!(prepared.stream_aggregate_in_view(&view, &cx, &params, GqlQueryPolicy::new(0,0,0,0)),
                    Err(QueryError::EdgeAggregateStream(_))));
            }
        }
        drop(prepared); drop(args); drop(view); drop(db);
        assert_eq!(pinned.row_stats().snapshot_records, 0);
        let row = pinned.next().unwrap().unwrap();
        assert_eq!(row.values()[0].as_integer(), Some(12));
        assert_eq!(row.values()[1].as_count(), Some(4));
        assert!(pinned.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projection_quota_and_late_overflow_refusals_are_terminal_and_never_publish_partial_groups() {
    let ((), report) = run_async_under_lab(0xa66e_c003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let args = GqlParameters::new();
        let mut empty = db.query_aggregate_stream(&cx, SUMMARY, &args, symbols(), wide()).unwrap();
        assert!(empty.next().is_none());
        db.write(&commit, seed()).await.unwrap();
        let prepared = PreparedNativeRead::prepare(SUMMARY, &args, symbols()).unwrap();
        let mut baseline = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let expected = baseline.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let r = baseline.row_stats(); let e = baseline.evaluator_stats();
        let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
        assert_eq!(prepared.stream_aggregate(&db, &cx, &args, exact).unwrap().collect::<Result<Vec<_>, _>>().unwrap(), expected);
        for quota in [
            GqlQueryPolicy::new(r.snapshot_records-1, r.result_rows, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, r.result_rows-1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, r.result_rows, e.work_units-1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, r.result_rows, u64::MAX, e.scratch_entries-1),
        ] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, quota).unwrap();
            assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let mut closed = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        closed.close(); assert!(closed.next().is_none());
        assert_eq!(closed.row_stats().snapshot_records, 0);
        assert_eq!(closed.evaluator_stats().work_units, 0);
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(EId(u128::MAX), PRICE, Some(CanonicalScalar::Int(i64::MAX)));
        db.write(&commit, edit).await.unwrap();
        let mut bad = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        assert!(matches!(bad.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::InputExpression { .. })))));
        assert_eq!(bad.row_stats().result_rows, 0);
        assert!(bad.next().is_none());
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN SUM(r.price*2) AS n LIMIT 0",
            "MATCH (a)-[r:R]->(b) RETURN SUM(r.price*2) AS n HAVING n>0",
            "MATCH (a)-[r:R]->(b) RETURN SUM(r.price*2) AS n ORDER BY n",
            "MATCH (a)-[r:R]->(b) RETURN SUM(r.price*2)+1 AS n",
        ] {
            let query = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
            assert!(matches!(query.stream_aggregate(&db, &cx, &args, GqlQueryPolicy::new(0,0,0,0)),
                Err(QueryError::EdgeAggregateStreamPlan(_))), "{text}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

const VERTEX_SUMMARY: &str = "MATCH (n:L) RETURN COUNT(*) AS n, LOWER(n.kind) AS bucket, SUM(n.price*n.qty) AS total, AVG(DISTINCT ABS(n.price*n.qty)) AS mean GROUP BY LOWER(n.kind)";

#[test]
fn native_vertex_computation_preserves_group_layouts_and_post_expression_distinct() {
    let ((), report) = run_async_under_lab(0xa66e_c004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for text in [VERTEX_SUMMARY,
            "MATCH (n:L) RETURN SUM(n.price*n.qty) AS total, COUNT(DISTINCT ABS(n.price*n.qty)) AS support, AVG(ABS(n.price*n.qty)) AS mean",
            "MATCH (n:L) RETURN COALESCE(n.price,0)+1 AS bucket, SUM(n.qty) AS total, n AS id GROUP BY n, COALESCE(n.price,0)+1",
            "MATCH (n:L) RETURN MIN(LOWER(n.kind)) AS first, MAX(LOWER(n.kind)) AS last, COUNT(DISTINCT LOWER(n.kind)) AS kinds",
            "MATCH (n:L) RETURN SUM(COALESCE(1,1/0)) AS total, COUNT(NULL) AS missing",
        ] {
            let args = GqlParameters::new();
            let QueryResult::Rows { columns, rows } = db.query(&cx, text, &args, symbols(), wide()).unwrap()
                else { panic!("expected rows") };
            let mut cursor = db.query_aggregate_stream(&cx, text, &args, symbols(), wide()).unwrap();
            assert_eq!(cursor.kind(), ScanKind::Vertex);
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            let slots = cursor.output_slots().to_vec();
            let actual: Vec<_> = cursor.by_ref().map(|row| visible(&row.unwrap(), &slots)).collect();
            assert_eq!(actual, rows, "{text}");
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert!(cursor.next().is_none());
        }
        let rows = db.query_aggregate_stream(&cx, VERTEX_SUMMARY, &GqlParameters::new(), symbols(), wide())
            .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].values()[0].as_count(), Some(2));
        assert_eq!(rows[0].values()[1].as_integer(), Some(0));
        let mean = rows[0].values()[2].as_average().unwrap();
        assert_eq!((mean.numerator(), mean.denominator()), (6, 1));
        assert_eq!(rows[1].values()[1].as_integer(), Some(8));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_vertex_history_and_scalar_parameters_survive_compaction_and_handle_drop() {
    let ((), report) = run_async_under_lab(0xa66e_c005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let text = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut RETURN SUM(n.price*$scale) AS total, MIN(COALESCE(n.kind,$fallback)) AS first";
        let args = GqlParameters::new().with_uint64("cut", 1).unwrap()
            .with_int64("scale", 3).unwrap().with_text("fallback", "' $not_syntax").unwrap();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        let mut pinned = prepared.stream_aggregate_in_view(&view, &cx, &args, wide()).unwrap();
        fn is_send(_: &impl Send) {} is_send(&pinned);
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(0), PRICE, Some(CanonicalScalar::Int(10)));
        edit.set_vertex_property(VId(1), KIND, None);
        db.write(&commit, edit).await.unwrap();
        for cut in 0..=2 {
            let params = GqlParameters::new().with_uint64("cut", cut).unwrap()
                .with_int64("scale", -2).unwrap().with_text("fallback", "fallback").unwrap();
            let QueryResult::Rows { rows, .. } = prepared.execute(&db, &cx, &params, wide()).unwrap()
                else { panic!("expected rows") };
            let mut cursor = prepared.stream_aggregate(&db, &cx, &params, wide()).unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            assert_eq!(rows, vec![cursor.next().unwrap().unwrap().values().to_vec()]);
            if cut == 2 {
                assert!(matches!(prepared.stream_aggregate_in_view(&view, &cx, &params, GqlQueryPolicy::new(0,0,0,0)),
                    Err(QueryError::AggregateStream(_))));
            }
        }
        let latest = GqlParameters::new().with_uint64("cut", 2).unwrap()
            .with_int64("scale", 3).unwrap().with_text("fallback", "fallback").unwrap();
        let mut current = prepared.stream_aggregate(&db, &cx, &latest, wide()).unwrap();
        db.compact(&commit).await.unwrap();
        drop(prepared); drop(args); drop(latest); drop(view); drop(db);
        assert_eq!(pinned.row_stats().snapshot_records, 0);
        assert_eq!(pinned.next().unwrap().unwrap().values()[0].as_integer(), Some(12));
        assert_eq!(current.next().unwrap().unwrap().values()[0].as_integer(), Some(36));
        assert!(pinned.next().is_none() && current.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn computed_vertex_empty_results_quotas_and_late_errors_do_not_bypass_admission() {
    let ((), report) = run_async_under_lab(0xa66e_c006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let args = GqlParameters::new();
        let mut grouped = db.query_aggregate_stream(&cx, VERTEX_SUMMARY, &args, symbols(), wide()).unwrap();
        assert!(grouped.next().is_none());
        // No binding exists on which the division could run. Its aggregate is NULL.
        let mut empty = db.query_aggregate_stream(&cx,
            "MATCH (n:L) RETURN COUNT(*) AS n, SUM(1/0) AS total", &args, symbols(), wide()).unwrap();
        let row = empty.next().unwrap().unwrap();
        assert_eq!(row.values()[0].as_count(), Some(0));
        assert!(row.values()[1].is_null());
        db.write(&commit, seed()).await.unwrap();
        let prepared = PreparedNativeRead::prepare(VERTEX_SUMMARY, &args, symbols()).unwrap();
        let mut full = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let want = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let r = full.row_stats(); let e = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
        assert_eq!(prepared.stream_aggregate(&db, &cx, &args, exact).unwrap().collect::<Result<Vec<_>, _>>().unwrap(), want);
        for quota in [
            GqlQueryPolicy::new(r.snapshot_records-1, r.result_rows, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, r.result_rows-1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, r.result_rows, e.work_units-1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, r.result_rows, u64::MAX, e.scratch_entries-1),
        ] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, quota).unwrap();
            assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(cursor.state(), VertexScanState::Failed);
            let stats = (cursor.row_stats(), cursor.evaluator_stats());
            assert!(cursor.next().is_none()); cursor.close();
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
        }
        let mut closed = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        closed.close(); assert!(closed.next().is_none());
        assert_eq!(closed.evaluator_stats().work_units, 0);
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(u128::MAX), PRICE, Some(CanonicalScalar::Int(i64::MAX)));
        db.write(&commit, edit).await.unwrap();
        let mut bad = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        assert!(matches!(bad.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::InputExpression { row: 0, .. })))));
        assert_eq!(bad.row_stats().result_rows, 0); assert!(bad.next().is_none());
        for text in [
            "MATCH (n:L) RETURN SUM(n.price*2) AS n LIMIT 0",
            "MATCH (n:L) RETURN SUM(n.price*2) AS n HAVING n>0",
            "MATCH (n:L) RETURN SUM(n.price*2) AS n ORDER BY n",
            "MATCH (n:L) RETURN SUM(n.price)+1 AS n",
        ] {
            let query = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
            assert!(matches!(query.stream_aggregate(&db, &cx, &args, GqlQueryPolicy::new(0,0,0,0)),
                Err(QueryError::AggregateStreamPlan(_))), "{text}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
