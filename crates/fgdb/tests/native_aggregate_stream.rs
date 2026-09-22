//! Native aggregate text must use the real pinned pull source, not an eager bag.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, PreparedNativeRead, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::stream::{VertexScanError, VertexScanState};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, RelationBind};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

#[path = "native_aggregate_stream/extended.rs"]
mod extended;

const LABEL: LabelId = LabelId(3);
const SCORE: PropertyKeyId = PropertyKeyId(7);
const TAG: PropertyKeyId = PropertyKeyId(8);
const SUMMARY: &str =
    "MATCH (n:L) RETURN COUNT(*) AS total, COUNT(n.score) AS present, SUM(n.score) AS sum";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn symbols() -> RelationBind {
    RelationBind::new()
        .with_label("L", LABEL)
        .with_relation("R", RelationId(1))
        .with_property("score", SCORE)
        .with_property("tag", TAG)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value) in [
        (0, Some(-9)),
        (1, Some(14)),
        (2, None),
        (u128::MAX, Some(2)),
    ] {
        batch.create_vertex(
            VId(id),
            vec![LABEL],
            vec![
                (
                    SCORE,
                    value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                ),
                (
                    TAG,
                    CanonicalScalar::ucs_basic_text("' $not_syntax").unwrap(),
                ),
            ],
        );
    }
    batch
}

#[test]
fn native_aliases_typed_arguments_and_empty_inputs_equal_the_ordinary_engine() {
    let ((), report) = run_async_under_lab(0xa66e_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let cases = [
            (SUMMARY, GqlParameters::new()),
            (
                "MATCH (n:L) WHERE n.score >= $floor RETURN SUM(n.score) AS sum, COUNT(*) AS count",
                GqlParameters::new().with_int64("floor", 3).unwrap(),
            ),
            (
                "MATCH (n:L) WHERE n.score >= $floor RETURN COUNT(*) AS count, SUM(n.score) AS sum",
                GqlParameters::new().with_int64("floor", 100).unwrap(),
            ),
            (
                "MATCH (n:L) WHERE n.tag=$tag RETURN COUNT(n.tag) AS tags, COUNT(*) AS rows",
                GqlParameters::new()
                    .with_text("tag", "' $not_syntax")
                    .unwrap(),
            ),
            (
                "MATCH (n:L) RETURN COUNT(*) AS first, COUNT(*) AS second",
                GqlParameters::new(),
            ),
        ];
        for (text, params) in cases {
            let QueryResult::Rows { columns, rows } =
                db.query(&cx, text, &params, symbols(), wide()).unwrap()
            else {
                panic!("read returned a write");
            };
            let mut cursor = db
                .query_aggregate_stream(&cx, text, &params, symbols(), wide())
                .unwrap();
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            assert_eq!(cursor.row_stats().result_rows, 0);
            let row = cursor.next().unwrap().unwrap();
            assert!(row.keys().is_empty());
            assert_eq!(rows, vec![row.values().to_vec()]);
            assert_eq!(cursor.row_stats().result_rows, 1);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            let stats = (cursor.row_stats(), cursor.evaluator_stats());
            assert!(cursor.next().is_none());
            cursor.close();
            assert_eq!(stats, (cursor.row_stats(), cursor.evaluator_stats()));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_cursor_owns_definition_arguments_and_generation_not_the_live_writer() {
    let ((), report) = run_async_under_lab(0xa66e_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let text = String::from(
            "MATCH (n:L) WHERE n.score >= $floor RETURN COUNT(*) AS count, SUM(n.score) AS sum",
        );
        let args = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(&text, &args, symbols()).unwrap();
        let mut first = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let other = GqlParameters::new().with_int64("floor", 10).unwrap();
        let mut second = prepared
            .stream_aggregate_in_view(&view, &cx, &other, wide())
            .unwrap();
        drop(prepared);
        drop(text);
        drop(args);
        drop(other);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(-100)));
        db.write(&commit, edit).await.unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        drop(view);
        drop(db);
        for (cursor, count, sum) in [(&mut first, 2, 16), (&mut second, 1, 14)] {
            assert_eq!(cursor.snapshot_seq(), CommitSeq(1));
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            let row = cursor.next().unwrap().unwrap();
            assert_eq!(row.values()[0].as_count(), Some(count));
            assert_eq!(row.values()[1].as_integer(), Some(sum));
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_rebinding_stays_at_the_exact_cut_and_refuses_future_view_reads() {
    let ((), report) = run_async_under_lab(0xa66e_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let text = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $seq RETURN COUNT(*) AS count, SUM(n.score) AS sum";
        let args = GqlParameters::new().with_uint64("seq", 1).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.delete_vertex(VId(1));
        db.write(&commit, edit).await.unwrap();
        for seq in 0..=2 {
            let args = GqlParameters::new().with_uint64("seq", seq).unwrap();
            let QueryResult::Rows { columns, rows } =
                prepared.execute(&db, &cx, &args, wide()).unwrap()
            else {
                panic!("not rows")
            };
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.snapshot_seq(), CommitSeq(seq));
            assert_eq!(
                rows,
                vec![cursor.next().unwrap().unwrap().values().to_vec()]
            );
        }
        let future = GqlParameters::new().with_uint64("seq", 2).unwrap();
        let error = view
            .query_aggregate_stream(
                &cx,
                text,
                &future,
                symbols(),
                GqlQueryPolicy::new(0, 0, 0, 0),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            QueryError::AggregateStream(GqlQueryError::Source(GraphAggregateError::Source(
                VertexScanError::Source(_)
            )))
        ));
        let wrong = GqlParameters::new().with_int64("seq", 1).unwrap();
        assert!(matches!(
            prepared.stream_aggregate_in_view(&view, &cx, &wrong, wide()),
            Err(QueryError::TemporalText(_))
        ));
        let mut historical = prepared
            .stream_aggregate_in_view(&view, &cx, &args, wide())
            .unwrap();
        assert_eq!(
            historical.next().unwrap().unwrap().values()[0].as_count(),
            Some(4)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_aggregate_shapes_are_never_stripped_or_eagerly_retried() {
    let ((), report) = run_async_under_lab(0xa66e_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let db = Database::open_memory(&commit, keys()).await.unwrap();
        for text in [
            "MATCH (n:L) WITH n.score AS score RETURN COUNT(*) AS count",
            "MATCH (n:L) WITH n.score AS score RETURN SUM(score) AS total",
            "MATCH (a:L)-[:R]->(b:L) WITH b.score AS s RETURN SUM(s) AS total",
            "MATCH (a:L)-[:R]->(b:L) WITH b.score AS s RETURN COUNT(*) AS count",
        ] {
            // Ensure a valid native definition, not a vacuous syntax refusal.
            let prepared =
                PreparedNativeRead::prepare(text, &GqlParameters::new(), symbols()).unwrap();
            assert!(
                matches!(
                    prepared.stream_aggregate(
                        &db,
                        &cx,
                        &GqlParameters::new(),
                        GqlQueryPolicy::new(0, 0, 0, 0),
                    ),
                    Err(QueryError::AggregateStreamPlan(_) | QueryError::EdgeAggregateStreamPlan(_))
                ),
                "{text}"
            );
        }
        let scan =
            PreparedNativeRead::prepare("MATCH (n:L) RETURN n", &GqlParameters::new(), symbols())
                .unwrap();
        assert!(matches!(
            scan.stream_aggregate(&db, &cx, &GqlParameters::new(), wide()),
            Err(QueryError::StreamingUnsupported { .. })
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_streams_keep_exact_budgets_typed_late_failures_and_nondraining_close() {
    let ((), report) = run_async_under_lab(0xa66e_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(SUMMARY, &args, symbols()).unwrap();
        let mut baseline = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let expected = baseline.next().unwrap().unwrap();
        let rows = baseline.row_stats();
        let work = baseline.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            1,
            work.work_units,
            work.scratch_entries,
        );
        let mut cursor = prepared.stream_aggregate(&db, &cx, &args, exact).unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), expected);
        for policy in [
            GqlQueryPolicy::new(rows.snapshot_records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, work.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, work.scratch_entries - 1),
        ] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, policy).unwrap();
            assert!(matches!(
                cursor.next(),
                Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)))
            ));
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert!(cursor.next().is_none());
        }
        let mut closed = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        closed.close();
        closed.close();
        assert_eq!(closed.state(), VertexScanState::Closed);
        assert!(closed.next().is_none());
        assert_eq!(closed.row_stats().snapshot_records, 0);
        assert_eq!(closed.evaluator_stats().work_units, 0);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(
            VId(u128::MAX),
            SCORE,
            Some(CanonicalScalar::ucs_basic_text("private incompatible operand").unwrap()),
        );
        db.write(&commit, edit).await.unwrap();
        let mut invalid = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        assert!(matches!(
            invalid.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerSum { aggregate: 2 }
            )))
        ));
        assert_eq!(invalid.row_stats().result_rows, 0);
        assert_eq!(invalid.state(), VertexScanState::Failed);
        assert!(!format!("{invalid:?}").contains("private incompatible operand"));
        assert!(invalid.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
