//! Real native endpoint reads feed exact result-bag differences, not event replay.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EmbeddedReadView, MemVfs, PreparedNativeRead, QueryError,
    QueryResult, ReadError, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::result_diff::{DiffEndpoint, GraphDiffError, GraphResultDiff};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphAggregateValue, GraphSymbolKind, RelationBind};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::BTreeMap;

type Bag = BTreeMap<Vec<GraphAggregateValue>, i128>;
const L: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const BUCKET: PropertyKeyId = PropertyKeyId(2);
const SCORES: &str = "MATCH (n:L) RETURN ALL n.score AS score";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x45;32], DatabaseSecurityNamespaceId([0x46;32]), [0x47;32])
}
fn symbols() -> RelationBind {
    RelationBind::new().with_label("L", L).with_relation("R", R)
        .with_property("score", SCORE).with_property("bucket", BUCKET)
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn value(n: i64) -> GraphAggregateValue { GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n))) }
fn observed(result: &GraphResultDiff) -> Bag {
    result.changes().iter().map(|(row, weight)| (row.to_vec(), weight.to_i128().unwrap())).collect()
}
fn bag(result: QueryResult) -> (Vec<String>, Bag) {
    let QueryResult::Rows { columns, rows } = result else { panic!("expected read rows") };
    let mut bag = Bag::new();
    for row in rows { *bag.entry(row).or_default() += 1; }
    (columns, bag)
}
fn difference(before: &Bag, after: &Bag) -> Bag {
    let mut result = after.clone();
    for (row, count) in before { *result.entry(row.clone()).or_default() -= count; }
    result.retain(|_, count| *count != 0);
    result
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, score, bucket) in [(0, Some(7), Some(1)), (1, Some(7), Some(1)),
        (2, None, Some(2)), (u128::MAX, Some(-3), None)] {
        let mut props = Vec::new();
        if let Some(score) = score { props.push((SCORE, CanonicalScalar::Int(score))); }
        if let Some(bucket) = bucket { props.push((BUCKET, CanonicalScalar::Int(bucket))); }
        batch.create_vertex(VId(id), vec![L], props);
    }
    batch.add_edge(EId(0), VId(0), VId(1), vec![(SCORE, CanonicalScalar::Int(5))]);
    batch.add_edge(EId(1), VId(0), VId(1), vec![(SCORE, CanonicalScalar::Int(5))]);
    batch.add_edge(EId(u128::MAX), VId(2), VId(2), vec![]);
    batch
}
fn edit() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(9)));
    batch.set_vertex_property(VId(u128::MAX), BUCKET, Some(CanonicalScalar::Int(2)));
    batch.delete_edge(EId(0));
    batch.add_edge(EId(3), VId(1), VId(2), vec![(SCORE, CanonicalScalar::Int(-4))]);
    batch
}
fn check(
    old: &EmbeddedReadView, new: &EmbeddedReadView, cx: &fgdb_types::QueryCx,
    text: &str, params: &GqlParameters,
) -> GraphResultDiff {
    let (columns, before) = bag(old.query(cx, text, params, symbols(), wide()).unwrap());
    let (after_columns, after) = bag(new.query(cx, text, params, symbols(), wide()).unwrap());
    assert_eq!(columns, after_columns);
    let result = new.query_diff(cx, text, params, symbols(), old.frontier(), new.frontier(), wide()).unwrap();
    assert_eq!(result.columns(), columns);
    assert_eq!(observed(&result), difference(&before, &after), "{text}");
    assert_eq!(result.row_stats().result_rows, result.changes().len() as u64);
    let reverse = new.query_diff(cx, text, params, symbols(), new.frontier(), old.frontier(), wide()).unwrap();
    assert_eq!(observed(&reverse), observed(&result).into_iter().map(|(r,w)| (r,-w)).collect());
    result
}

#[test]
fn duplicates_pages_reverse_endpoints_and_net_cancellation_are_exact() {
    let ((), report) = run_async_under_lab(0xd1ff_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let empty = db.read_session().unwrap();
        db.write(&commit, seed()).await.unwrap(); let before = db.read_session().unwrap();
        db.write(&commit, edit()).await.unwrap(); let after = db.read_session().unwrap();
        let args = GqlParameters::new();
        let initial = check(&empty, &before, &cx, SCORES, &args);
        assert_eq!(initial.before(), CommitSeq(0));
        assert_eq!(initial.after(), CommitSeq(1));
        assert_eq!(observed(&initial)[&vec![value(7)]], 2);
        let result = check(&before, &after, &cx, SCORES, &args);
        assert_eq!(observed(&result), Bag::from([(vec![value(7)], -1), (vec![value(9)], 1)]));
        let distinct = check(&before, &after, &cx, "MATCH (n:L) RETURN DISTINCT n.score AS score", &args);
        assert_eq!(observed(&distinct), Bag::from([(vec![value(9)], 1)]));
        let selected = check(&before, &after, &cx,
            "MATCH (n:L) RETURN n.score AS score ORDER BY score DESC NULLS LAST LIMIT 1", &args);
        assert_eq!(observed(&selected), observed(&result));
        let selected = check(&before, &after, &cx,
            "MATCH (n:L) RETURN n.score AS score ORDER BY score DESC NULLS LAST SKIP 1 LIMIT 1", &args);
        assert!(selected.changes().is_empty());
        let text = "MATCH (n:L) WHERE n.score >= $floor RETURN n.score AS score";
        let args = GqlParameters::new().with_int64("floor", 8).unwrap();
        assert_eq!(observed(&check(&before, &after, &cx, text, &args)), Bag::from([(vec![value(9)], 1)]));
        let mut restore = WriteBatch::new(R);
        restore.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(7)));
        let restored = db.write(&commit, restore).await.unwrap();
        let net = db.query_diff(&cx, SCORES, &GqlParameters::new(), symbols(), before.frontier(), restored, wide()).unwrap();
        assert!(net.changes().is_empty()); // intervening changes are not CDC events.
        assert_eq!(net.before(), before.frontier()); assert_eq!(net.after(), restored);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_native_families_keep_groups_aliases_compound_inputs_and_exact_cells() {
    let ((), report) = run_async_under_lab(0xd1ff_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap(); let before = db.read_session().unwrap();
        db.write(&commit, edit()).await.unwrap(); let after = db.read_session().unwrap();
        let args = GqlParameters::new();
        for text in [
            "MATCH (n:L) RETURN n AS id, n.score AS score",
            "MATCH (n:L) RETURN COUNT(*) AS n, SUM(n.score) AS sum, AVG(n.score) AS avg, COUNT(DISTINCT n.score) AS d",
            "MATCH (n:L) RETURN SUM(n.score) AS total, n.bucket AS bucket, AVG(n.score) AS avg GROUP BY n.bucket",
            "MATCH (n:L) RETURN n.bucket AS first, n.bucket AS again, COUNT(*) AS n GROUP BY n.bucket",
            "MATCH (n:L) RETURN n.bucket AS bucket, SUM(n.score) AS total GROUP BY n.bucket HAVING total > 0 ORDER BY total DESC LIMIT 1",
            "MATCH (n:L) WITH n.score AS score RETURN SUM(score) AS sum, AVG(score) AS avg",
            "MATCH (n:L) RETURN n.score AS score UNION ALL MATCH (m:L) RETURN m.score AS score",
            "MATCH (n:L) RETURN n.score AS score EXCEPT ALL MATCH (m:L) WHERE m.score < 0 RETURN m.score AS score",
            "MATCH (a)-[r:R]->(b) RETURN r AS edge, a AS src, b AS dst, r.score AS score",
            "MATCH p=(a)-[:R]->(b) RETURN p AS path",
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n, COUNT(DISTINCT r) AS edges, AVG(r.score) AS average",
            "MATCH (n:L) RETURN COLLECT(n.score) AS scores",
        ] {
            check(&before, &after, &cx, text, &args);
        }
        // Exact average 11/3 -> 13/3, without scalar or float conversion.
        let avg = after.query_diff(&cx, "MATCH (n:L) RETURN AVG(n.score) AS avg", &args,
            symbols(), before.frontier(), after.frontier(), wide()).unwrap();
        let expected = Bag::from([
            (vec![GraphAggregateValue::Average(fgdb_gql::GraphExactAverage::new(11, 3).unwrap())], -1),
            (vec![GraphAggregateValue::Average(fgdb_gql::GraphExactAverage::new(13, 3).unwrap())], 1),
        ]);
        assert_eq!(observed(&avg), expected);
        // Vertex deletion cascades through the existing edge snapshot reader.
        let mut remove = WriteBatch::new(R); remove.delete_vertex(VId(1));
        db.write(&commit, remove).await.unwrap(); let final_view = db.read_session().unwrap();
        check(&after, &final_view, &cx, "MATCH p=(a)-[:R]->(b) RETURN p AS path", &args);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn endpoint_work_is_cumulative_and_changed_tuple_quota_is_not_an_input_row_limit() {
    let ((), report) = run_async_under_lab(0xd1ff_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..64 { batch.create_vertex(VId(id), vec![L], vec![(SCORE, CanonicalScalar::Int(7))]); }
        let before = db.write(&commit, batch).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(u128::MAX), vec![L], vec![(SCORE, CanonicalScalar::Int(7))]);
        let after = db.write(&commit, batch).await.unwrap();
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(SCORES, &args, symbols()).unwrap();
        let full = prepared.diff(&db, &cx, &args, before, after, wide()).unwrap();
        assert_eq!(observed(&full), Bag::from([(vec![value(7)], 1)]));
        let rows = full.row_stats(); let work = full.evaluator_stats();
        assert!(rows.snapshot_records >= 129);
        let exact = GqlQueryPolicy::new(rows.snapshot_records, 1, work.work_units, work.scratch_entries);
        assert_eq!(prepared.diff(&db, &cx, &args, before, after, exact).unwrap(), full);
        for policy in [GqlQueryPolicy::new(rows.snapshot_records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, work.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, work.scratch_entries - 1)] {
            assert!(matches!(prepared.diff(&db, &cx, &args, before, after, policy),
                Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))));
        }
        let no_changes = prepared.diff(&db, &cx, &args, before, before,
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX)).unwrap();
        assert!(no_changes.changes().is_empty());
        assert!(no_changes.row_stats().snapshot_records > 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pinned_revision_differences_survive_writer_changes_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xd1ff_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let before = db.write(&commit, seed()).await.unwrap();
        let after = db.write(&commit, edit()).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(SCORES, &args, symbols()).unwrap();
        let expected = prepared.diff_in_view(&view, &cx, &args, before, after, wide()).unwrap();
        let mut update = WriteBatch::new(R); update.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(100)));
        let latest = db.write(&commit, update).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(observed(&prepared.diff(&db, &cx, &args, before, after, wide()).unwrap()), observed(&expected));
        drop(db);
        assert_eq!(prepared.diff_in_view(&view, &cx, &args, before, after, wide()).unwrap(), expected);
        // Exact metadata, never clamped to the pinned view or live writer.
        for (from, to, side) in [(latest, before, DiffEndpoint::Before),
            (before, latest, DiffEndpoint::After), (latest, latest, DiffEndpoint::Before)] {
            let mut called = false;
            let error = view.query_diff(&cx, "not valid syntax", &args,
                |_: GraphSymbolKind, _: &str| { called = true; None },
                from, to, GqlQueryPolicy::new(0, 0, 0, 0)).unwrap_err();
            assert!(!called);
            assert!(matches!(error, GqlQueryError::Source(GraphDiffError::Endpoint {
                endpoint, source: QueryError::Read(ReadError::BeyondFrontier { .. }),
            })) if endpoint == side);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_overrides_bad_binding_and_late_endpoint_failures_never_become_empty_diffs() {
    let ((), report) = run_async_under_lab(0xd1ff_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        for text in [
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n AS id",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS n",
        ] {
            assert!(matches!(db.query_diff(&cx, text, &args, symbols(), before, before, wide()),
                Err(GqlQueryError::Source(GraphDiffError::TemporalSelector))));
        }
        let params = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n:L) WHERE n.score >= $floor RETURN n.score AS score", &params, symbols()).unwrap();
        assert!(matches!(prepared.diff(&db, &cx, &args, before, before, wide()),
            Err(GqlQueryError::Source(GraphDiffError::Definition(QueryError::PatternText(_))))));
        assert!(db.query_diff(&cx, "CREATE (n:L)", &args, symbols(), before, before, wide()).is_err());
        assert_eq!(db.frontier().unwrap(), before);
        let mut invalid = WriteBatch::new(R);
        invalid.set_vertex_property(VId(u128::MAX), SCORE,
            Some(CanonicalScalar::ucs_basic_text("private incompatible number").unwrap()));
        let after = db.write(&commit, invalid).await.unwrap();
        for (from, to, failed) in [(before, after, DiffEndpoint::After),
            (after, before, DiffEndpoint::Before), (after, after, DiffEndpoint::Before)] {
            let error = db.query_diff(&cx, "MATCH (n:L) RETURN SUM(n.score) AS sum", &args,
                symbols(), from, to, GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX)).unwrap_err();
            assert!(!format!("{error}").contains("private incompatible"));
            assert!(matches!(error, GqlQueryError::Source(GraphDiffError::Endpoint {
                endpoint, source: QueryError::Aggregate(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { .. })),
            })) if endpoint == failed);
        }
        // The accepted historical cut is still usable after the bad new data.
        assert!(db.query_diff(&cx, "MATCH (n:L) RETURN SUM(n.score) AS sum", &args,
            symbols(), before, before, wide()).unwrap().changes().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
