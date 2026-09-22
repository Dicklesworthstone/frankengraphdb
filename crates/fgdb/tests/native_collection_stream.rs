//! List-valued native results through the same snapshot and dispatch paths as
//! numeric streams. No result oracle invokes a production collection reducer.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, NativeAggregateCursor, PreparedNativeRead, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateTextSlot, GraphAggregateValue, RelationBind};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const B: PropertyKeyId = PropertyKeyId(2);
const HIGH: VId = VId(u128::MAX);
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x41; 32], DatabaseSecurityNamespaceId([0x42; 32]), [0x43; 32]) }
fn symbols() -> RelationBind { RelationBind::new().with_relation("R", R).with_label("L", L).with_property("p", P).with_property("bucket", B) }
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn list(values: Vec<GraphValue>) -> GraphAggregateValue { GraphAggregateValue::Value(GraphValue::List(values.into_boxed_slice())) }
fn vertices(cut: u64) -> Vec<(VId, i64, Option<i64>)> {
    if cut == 0 { return vec![]; }
    let mut rows = vec![(VId(0), 0, Some(9)), (VId(1), 1, Some(-4)), (VId(2), 0, Some(9)), (HIGH, 1, None)];
    if cut >= 2 { rows[0].2 = Some(2); rows.retain(|(id, _, _)| *id != VId(1)); }
    if cut >= 3 { rows.push((VId(3), 1, None)); rows.sort_by_key(|(id, _, _)| *id); }
    rows
}
fn seed() -> WriteBatch {
    let mut b = WriteBatch::new(R);
    for (id, bucket, p) in vertices(1) {
        let mut props: Vec<_> = p.into_iter().map(|p| (P, CanonicalScalar::Int(p))).collect();
        props.push((B, CanonicalScalar::Int(bucket))); b.create_vertex(id, vec![L], props);
    }
    b
}
fn args(cut: u64, skip: u64, limit: u64) -> GqlParameters {
    GqlParameters::new().with_uint64("cut", cut).unwrap().with_uint64("skip", skip).unwrap().with_uint64("limit", limit).unwrap()
}
fn drain(c: &mut NativeAggregateCursor<'_>) -> Vec<Vec<GraphAggregateValue>> {
    let slots = c.output_slots().to_vec();
    c.by_ref().map(|row| { let row = row.unwrap(); slots.iter().map(|slot| match *slot {
        GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
        GraphAggregateTextSlot::GroupKey(at) => GraphAggregateValue::Value(row.keys()[at].clone()),
    }).collect() }).collect()
}
const TEXT: &str = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut RETURN COLLECT(n.p) AS vals,n.bucket AS bucket,COLLECT(DISTINCT n.p) AS support,COLLECT(n) AS ids,COUNT(*) AS total GROUP BY n.bucket HAVING total>0 ORDER BY bucket DESC SKIP $skip LIMIT $limit";
fn expected(cut: u64, skip: usize, limit: usize) -> Vec<Vec<GraphAggregateValue>> {
    let mut groups = BTreeMap::<i64, Vec<(VId, Option<i64>)>>::new();
    for (id, bucket, value) in vertices(cut) { groups.entry(bucket).or_default().push((id, value)); }
    groups.into_iter().rev().skip(skip).take(limit).map(|(bucket, rows)| {
        let mut seen = BTreeSet::new();
        let all: Vec<_> = rows.iter().filter_map(|(_, p)| p.map(|p| GraphValue::Scalar(CanonicalScalar::Int(p)))).collect();
        let distinct: Vec<_> = all.iter().filter(|v| seen.insert((*v).clone())).cloned().collect();
        vec![list(all), GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(bucket))),
            list(distinct), list(rows.iter().map(|(id, _)| GraphValue::Vertex(*id)).collect()), GraphAggregateValue::Count(rows.len() as u64)]
    }).collect()
}

#[test]
fn native_collection_layouts_and_pinned_lists_survive_changes_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xc011_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query(); let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap(); let view = db.read_session().unwrap();
        let prepared = PreparedNativeRead::prepare(TEXT, &args(1, 0, 2), symbols()).unwrap();
        let mut paused = prepared.stream_aggregate(&db, &cx, &args(1, 0, 2), wide()).unwrap();
        fn send(_: &impl Send) {} send(&paused);
        assert_eq!(paused.row_stats().snapshot_records, 0); assert_eq!(paused.kind(), ScanKind::Vertex);
        let mut edit = WriteBatch::new(R); edit.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(2))); edit.delete_vertex(VId(1));
        db.write(&commit, edit).await.unwrap();
        let mut add = WriteBatch::new(R); add.create_vertex(VId(3), vec![L], vec![(B, CanonicalScalar::Int(1))]); db.write(&commit, add).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(drain(&mut paused), expected(1, 0, 2)); assert_eq!(paused.snapshot_seq(), CommitSeq(1));
        for cut in 0..=3 { for (skip, limit) in [(0, 0), (0, 1), (0, 2), (1, 1), (9, 2)] {
            let params = args(cut, skip, limit);
            let QueryResult::Rows { columns, rows } = prepared.execute(&db, &cx, &params, wide()).unwrap() else { panic!("rows"); };
            let mut stream = prepared.stream_aggregate(&db, &cx, &params, wide()).unwrap();
            assert_eq!(stream.columns(), columns); assert_eq!(drain(&mut stream), rows);
            assert_eq!(rows, expected(cut, skip as usize, limit as usize));
            assert_eq!(stream.row_stats().result_rows, rows.len() as u64); assert_eq!(stream.snapshot_seq(), CommitSeq(cut));
        }}
        let mut pinned = prepared.stream_aggregate_in_view(&view, &cx, &args(1, 0, 2), wide()).unwrap();
        assert_eq!(drain(&mut pinned), expected(1, 0, 2));
        assert!(prepared.stream_aggregate_in_view(&view, &cx, &args(2, 0, 0), wide()).is_err());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_collection_errors_limits_and_empty_input_keep_atomic_result_semantics() {
    let ((), report) = run_async_under_lab(0xc011_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); db.write(&commit, seed()).await.unwrap();
        let text = "MATCH (n:L) RETURN COLLECT(n.p) AS vals,COLLECT(DISTINCT n.p) AS support,COLLECT(n) AS ids";
        let params = GqlParameters::new(); let prepared = PreparedNativeRead::prepare(text, &params, symbols()).unwrap();
        let mut baseline = prepared.stream_aggregate(&db, &cx, &params, wide()).unwrap(); let expected = drain(&mut baseline);
        let r = baseline.row_stats(); let e = baseline.evaluator_stats();
        let exact = GqlQueryPolicy::new(r.snapshot_records, 1, e.work_units, e.scratch_entries);
        assert_eq!(drain(&mut prepared.stream_aggregate(&db, &cx, &params, exact).unwrap()), expected);
        for policy in [GqlQueryPolicy::new(r.snapshot_records - 1, 1, u64::MAX, u64::MAX), GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, e.work_units - 1, u64::MAX), GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, e.scratch_entries - 1)] {
            let mut c = prepared.stream_aggregate(&db, &cx, &params, policy).unwrap();
            assert!(c.next().unwrap().is_err()); assert_eq!(c.row_stats().result_rows, 0); assert_eq!(c.state(), VertexScanState::Failed); assert!(c.next().is_none());
        }
        let mut closed = prepared.stream_aggregate(&db, &cx, &params, exact).unwrap(); closed.close(); closed.close();
        assert!(closed.next().is_none()); assert_eq!(closed.row_stats().snapshot_records, 0);
        for text in ["MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 0 RETURN COLLECT(n) AS ids",
            "MATCH (n:L) WHERE n.p>999 RETURN COLLECT(DISTINCT n.p) AS vals"] {
            assert_eq!(drain(&mut db.query_aggregate_stream(&cx, text, &params, symbols(), wide()).unwrap()), vec![vec![list(vec![])]]);
        }
        let mut edit = WriteBatch::new(R); edit.set_vertex_property(HIGH, P, Some(CanonicalScalar::ucs_basic_text("private sum operand").unwrap())); db.write(&commit, edit).await.unwrap();
        for limit in [0, 1] {
            let text = format!("MATCH (n:L) RETURN COLLECT(n.p) AS vals,SUM(n.p) AS total LIMIT {limit}");
            let mut c = db.query_aggregate_stream(&cx, &text, &params, symbols(), wide()).unwrap();
            assert!(matches!(c.next(), Some(Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { .. })))));
            assert_eq!(c.row_stats().result_rows, 0); assert!(c.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_identified_edge_collections_preserve_order_and_refuse_unproved_inputs() {
    let ((), report) = run_async_under_lab(0xc011_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); let mut b = seed();
        for (id, a, to, value) in [(1, HIGH, VId(0), Some(9)), (2, VId(0), VId(1), Some(-4)),
            (3, HIGH, VId(0), Some(9)), (4, VId(1), VId(1), None), (5, VId(1), HIGH, Some(7))] {
            b.add_edge(EId(id), a, to, value.into_iter().map(|p| (P, CanonicalScalar::Int(p))).collect());
        }
        db.write(&commit, b).await.unwrap();
        for direction in 0..3 { for hops in 1..=2 {
            let atom = |name, end| match direction { 0 => format!("-[{name}:R]->({end})"),
                1 => format!("<-[{name}:R]-({end})"), _ => format!("-[{name}:R]-({end})") };
            let mut text = format!("MATCH (a){}", atom("r", "b"));
            if hops == 2 { text += &atom("s", "c"); }
            text += " RETURN COLLECT(r) AS roots,COLLECT(a) AS starts,";
            text += if hops == 2 { "COLLECT(s) AS suffixes,COLLECT(c) AS ends," } else { "COLLECT(b) AS ends," };
            text += "COLLECT(r.p) AS vals,COLLECT(DISTINCT r.p) AS support";
            let params = GqlParameters::new(); let QueryResult::Rows { rows, .. } = db.query(&cx, &text, &params, symbols(), wide()).unwrap() else { panic!("rows"); };
            let mut c = db.query_aggregate_stream(&cx, &text, &params, symbols(), wide()).unwrap();
            assert_eq!(c.kind(), ScanKind::Edge); assert_eq!(drain(&mut c), rows); assert_eq!(c.row_stats().result_rows, 1);
            if direction == 0 && hops == 1 { assert_eq!(rows[0][rows[0].len() - 1], list(vec![9, -4, 7].into_iter().map(|n| GraphValue::Scalar(CanonicalScalar::Int(n))).collect())); }
        }}
        for text in ["MATCH (a)-[r:R]->(b) RETURN COLLECT(b) AS vals", "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p) AS vals"] {
            let p = PreparedNativeRead::prepare(text, &GqlParameters::new(), symbols()).unwrap();
            assert!(p.stream_aggregate(&db, &cx, &GqlParameters::new(), wide()).is_err());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
