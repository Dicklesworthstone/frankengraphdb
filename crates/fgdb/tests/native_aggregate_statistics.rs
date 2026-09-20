//! Exact global statistics through native text and the pinned MVCC pull source.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, RelationBind};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const LABEL: LabelId = LabelId(3);
const SCORE: PropertyKeyId = PropertyKeyId(7);
const TAG: PropertyKeyId = PropertyKeyId(8);
const STATS: &str = "MATCH (n:L) RETURN AVG(n.score) AS avg, MIN(n.score) AS min, MAX(n.score) AS max, COUNT(*) AS count";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
}
fn symbols() -> RelationBind {
    RelationBind::new()
        .with_label("L", LABEL)
        .with_property("score", SCORE)
        .with_property("tag", TAG)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

#[test]
fn native_statistics_preserve_exact_fractions_and_pinned_history_across_writes() {
    let ((), report) = run_async_under_lab(0xa66e_2001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(0), vec![LABEL], vec![(SCORE, CanonicalScalar::Int(i64::MAX))]);
        seed.create_vertex(VId(1), vec![LABEL], vec![(SCORE, CanonicalScalar::Int(i64::MAX - 1))]);
        seed.create_vertex(VId(2), vec![LABEL], vec![(SCORE, CanonicalScalar::Null)]);
        seed.create_vertex(VId(u128::MAX), vec![LABEL], vec![]);
        db.write(&commit, seed).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        let mut pinned = view.query_aggregate_stream(&cx, STATS, &args, symbols(), wide()).unwrap();
        assert_eq!(pinned.row_stats().snapshot_records, 0);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(0), SCORE, Some(CanonicalScalar::Int(-7)));
        edit.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(3)));
        db.write(&commit, edit).await.unwrap();
        let temporal = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $seq RETURN AVG(n.score) AS avg, MIN(n.score) AS min, MAX(n.score) AS max, COUNT(*) AS count";
        for seq in 0..=2 {
            let args = GqlParameters::new().with_uint64("seq", seq).unwrap();
            let QueryResult::Rows { columns, rows } =
                db.query(&cx, temporal, &args, symbols(), wide()).unwrap() else {
                    panic!("aggregate is a read");
                };
            let mut cursor = db.query_aggregate_stream(&cx, temporal, &args, symbols(), wide()).unwrap();
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.snapshot_seq(), CommitSeq(seq));
            assert_eq!(rows, vec![cursor.next().unwrap().unwrap().values().to_vec()]);
            assert!(cursor.next().is_none());
        }
        let mut current = db.query_aggregate_stream(&cx, STATS, &args, symbols(), wide()).unwrap();
        drop(view);
        drop(db);
        let result = pinned.next().unwrap().unwrap();
        let avg = result.values()[0].as_average().unwrap();
        assert_eq!(avg.numerator(), 2 * i128::from(i64::MAX) - 1);
        assert_eq!(avg.denominator(), 2);
        assert_eq!(result.values()[3].as_count(), Some(4));
        let result = current.next().unwrap().unwrap();
        let avg = result.values()[0].as_average().unwrap();
        assert_eq!((avg.numerator(), avg.denominator()), (-2, 1));
        assert!(result.values()[0].as_integer().is_none());
        assert_eq!(pinned.state(), VertexScanState::Exhausted);
        assert_eq!(current.state(), VertexScanState::Exhausted);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_extrema_keep_scalar_domains_and_full_width_identities_under_one_budget() {
    let ((), report) = run_async_under_lab(0xa66e_2002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(0), vec![LABEL], vec![(TAG, CanonicalScalar::Bool(false))]);
        seed.create_vertex(VId(1), vec![LABEL], vec![(TAG, CanonicalScalar::ucs_basic_text(&"é".repeat(4096)).unwrap())]);
        seed.create_vertex(VId(u128::MAX), vec![LABEL], vec![(TAG, CanonicalScalar::bytes(vec![255; 8192]).unwrap())]);
        db.write(&commit, seed).await.unwrap();
        let text = "MATCH (n:L) RETURN MIN(n.tag) AS min_tag, MAX(n.tag) AS max_tag, MIN(n) AS min_id, MAX(n) AS max_id";
        let args = GqlParameters::new();
        let QueryResult::Rows { columns, rows } =
            db.query(&cx, text, &args, symbols(), wide()).unwrap() else { panic!("not rows") };
        let mut baseline = db.query_aggregate_stream(&cx, text, &args, symbols(), wide()).unwrap();
        assert_eq!(baseline.columns(), columns);
        let result = baseline.next().unwrap().unwrap();
        assert_eq!(rows, vec![result.values().to_vec()]);
        assert_eq!(result.values()[2].as_value(), Some(&GraphValue::Vertex(VId(0))));
        assert_eq!(result.values()[3].as_value(), Some(&GraphValue::Vertex(VId(u128::MAX))));
        let usage = baseline.evaluator_stats();
        let records = baseline.row_stats().snapshot_records;
        let exact = GqlQueryPolicy::new(records, 1, usage.work_units, usage.scratch_entries);
        let mut cursor = db.query_aggregate_stream(&cx, text, &args, symbols(), exact).unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), result);
        for policy in [
            GqlQueryPolicy::new(records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(records, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(records, 1, usage.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(records, 1, u64::MAX, usage.scratch_entries - 1),
        ] {
            let mut cursor = db.query_aggregate_stream(&cx, text, &args, symbols(), policy).unwrap();
            assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)))));
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_average_reports_late_type_errors_and_does_not_admit_computed_inputs() {
    let ((), report) = run_async_under_lab(0xa66e_2003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(0), vec![LABEL], vec![(SCORE, CanonicalScalar::Int(3))]);
        seed.create_vertex(VId(u128::MAX), vec![LABEL], vec![(SCORE, CanonicalScalar::Bool(false))]);
        db.write(&commit, seed).await.unwrap();
        let args = GqlParameters::new();
        let mut cursor = db.query_aggregate_stream(&cx, STATS, &args, symbols(), wide()).unwrap();
        assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Source(
            GraphAggregateError::NonIntegerAverage { aggregate: 0 }
        )))));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
        for text in [
            "MATCH (n:L) RETURN AVG(DISTINCT n.score + 1) AS avg",
            "MATCH (n:L) RETURN MIN(n.score + 1) AS min",
        ] {
            let prepared = fgdb::PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
            assert!(matches!(prepared.stream_aggregate(&db, &cx, &args, wide()),
                Err(QueryError::AggregateStreamPlan(_))));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
