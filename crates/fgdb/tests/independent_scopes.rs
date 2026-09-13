//! Independent scoped reads over Chronicle/Strata snapshots and canonical overlays.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::BTreeMap;

const ROOT: LabelId = LabelId(1);
const FLAG: LabelId = LabelId(2);
const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
type Plain = (VId, Option<VId>, Option<i64>);
type Summary = (VId, u64, u64, Option<i128>);
fn keys() -> DatabaseKeys { DatabaseKeys::new([0xa1; 32], DatabaseSecurityNamespaceId([0xa2; 32]), [0xa3; 32]) }
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 5_000_000, 2_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Root") => Some(GraphSymbol::Label(ROOT)),
        (GraphSymbolKind::Label, "Flag") => Some(GraphSymbol::Label(FLAG)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn optional(take: u64) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare("MATCH (a:Root) OPTIONAL MATCH (f:Flag) WHERE f.p >= 0 \
        RETURN a,f,f.p AS value ORDER BY a,f LIMIT $take", symbols).unwrap()
        .bind_parameters(&GqlParameters::new().with_uint64("take", take).unwrap()).unwrap()
}
fn aggregate(take: u64) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare("MATCH (a:Root) OPTIONAL MATCH (f:Flag) WHERE f.p >= 0 \
        RETURN a,COUNT(*) AS occurrences,COUNT(f) AS flags,SUM(f.p) AS total GROUP BY a ORDER BY a LIMIT $take", symbols)
        .unwrap().bind_parameters(&GqlParameters::new().with_uint64("take", take).unwrap()).unwrap()
}
fn probe(anti: bool, take: u64) -> PreparedGraphPattern<GraphValueRow> {
    let head = if anti { "NOT EXISTS" } else { "EXISTS" };
    let floor = if anti { 99 } else { 0 };
    PreparedGraphText::prepare(&format!("MATCH (a:Root) WHERE {head} {{ MATCH (f:Flag) WHERE f.p >= {floor} }} \
        RETURN a LIMIT {take}"), symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![ROOT], vec![]);
    batch.create_vertex(VId(2), vec![ROOT], vec![]);
    batch.create_vertex(VId(10), vec![FLAG], vec![(P, CanonicalScalar::Int(7))]);
    batch.create_vertex(VId(11), vec![FLAG], vec![(P, CanonicalScalar::Int(-1))]);
    batch.create_vertex(VId(99), vec![], vec![]);
    batch.add_edge(EId(100), VId(1), VId(2), vec![]);
    batch.add_edge(EId(101), VId(10), VId(11), vec![]);
    batch.add_edge(EId(102), VId(10), VId(11), vec![]);
    db.write(cx, batch).await.unwrap()
}
fn integer(row: &VertexRow) -> Option<i64> {
    row.props.iter().find_map(|(key, value)| match value {
        CanonicalScalar::Int(value) if *key == P => Some(*value), _ => None,
    })
}
fn oracle(vertices: &[VertexRow]) -> Vec<Plain> {
    let flags: Vec<_> = vertices.iter().filter(|row| row.labels.contains(&FLAG))
        .filter_map(|row| integer(row).filter(|value| *value >= 0).map(|value| (row.vid, value))).collect();
    let mut result = Vec::new();
    for owner in vertices.iter().filter(|row| row.labels.contains(&ROOT)) {
        if flags.is_empty() { result.push((owner.vid, None, None)); }
        else { for &(flag, value) in &flags { result.push((owner.vid, Some(flag), Some(value))); } }
    }
    result.sort(); result
}
fn plain(rows: &[GraphValueRow]) -> Vec<Plain> {
    rows.iter().map(|row| {
        let value = match row.values()[2].as_scalar().unwrap() {
            CanonicalScalar::Int(value) => Some(*value), CanonicalScalar::Null => None,
            _ => panic!("unexpected fixture scalar"),
        };
        (row.values()[0].as_vertex().unwrap(), row.values()[1].as_vertex(), value)
    }).collect()
}
fn expected_summaries(rows: &[Plain]) -> Vec<Summary> {
    let mut groups = BTreeMap::<VId, (u64, u64, Option<i128>)>::new();
    for &(owner, flag, value) in rows {
        let group = groups.entry(owner).or_default(); group.0 += 1; group.1 += u64::from(flag.is_some());
        if let Some(value) = value { group.2 = Some(group.2.unwrap_or(0) + i128::from(value)); }
    }
    groups.into_iter().map(|(id, (rows, flags, total))| (id, rows, flags, total)).collect()
}
fn summaries(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(), row.get(0).unwrap().as_count().unwrap(),
        row.get(1).unwrap().as_count().unwrap(), row.get(2).unwrap().as_integer())).collect()
}

#[test]
fn independent_rows_and_aggregates_follow_staging_but_preserve_pinned_history() {
    let ((), report) = run_async_under_lab(0x1ade_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let pinned = db.read_session().unwrap();
        let plan = optional(100); let grouped = aggregate(100); let frozen = plan.canonical_bytes();
        let old = oracle(&db.vertices().unwrap());
        assert_eq!(old, vec![(VId(1), Some(VId(10)), Some(7)), (VId(2), Some(VId(10)), Some(7))]);
        let mut txn = db.begin(&txn_cx).unwrap();
        for result in [db.execute_graph_pattern_governed(&cx, &plan, wide()).unwrap(),
            db.execute_graph_pattern_governed_at(&cx, &plan, basis, wide()).unwrap(),
            pinned.execute_graph_pattern_governed(&cx, &plan, wide()).unwrap(),
            pinned.execute_graph_pattern_governed_at(&cx, &plan, basis, wide()).unwrap(),
            txn.execute_graph_pattern_governed(&db, &cx, &plan, wide()).unwrap()] {
            assert_eq!(plain(&result.value), old); assert_eq!(result.rows.snapshot_records, 5);
        }
        for result in [db.execute_graph_aggregate_governed(&cx, &grouped, wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &grouped, basis, wide()).unwrap(),
            pinned.execute_graph_aggregate_governed(&cx, &grouped, wide()).unwrap(),
            pinned.execute_graph_aggregate_governed_at(&cx, &grouped, basis, wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &grouped, wide()).unwrap()] {
            assert_eq!(summaries(&result.value), expected_summaries(&old));
        }
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(10), P, None);
        changes.set_vertex_property(VId(11), P, Some(CanonicalScalar::Int(9)));
        changes.create_vertex(VId(12), vec![FLAG], vec![(P, CanonicalScalar::Int(11))]);
        changes.delete_vertex(VId(99)); changes.delete_edge(EId(102));
        changes.ensure_edge_by_triple(EId(999), VId(10), VId(11), vec![]);
        txn.write(&mut db, changes).unwrap(); assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap());
        assert_eq!(expected_summaries(&expected), vec![(VId(1), 2, 2, Some(20)), (VId(2), 2, 2, Some(20))]);
        assert_eq!(plain(&txn.execute_graph_pattern_governed(&db, &cx, &plan, wide()).unwrap().value), expected);
        assert_eq!(summaries(&txn.execute_graph_aggregate_governed(&db, &cx, &grouped, wide()).unwrap().value), expected_summaries(&expected));
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &plan, wide()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap(); db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_pattern_governed(&cx, &plan, wide()).unwrap().value), expected);
        assert_eq!(summaries(&reopened.execute_graph_aggregate_governed(&cx, &grouped, wide()).unwrap().value), expected_summaries(&expected));
        assert_eq!(plain(&reopened.execute_graph_pattern_governed_at(&cx, &plan, basis, wide()).unwrap().value), old);
        assert_eq!(summaries(&reopened.execute_graph_aggregate_governed_at(&cx, &grouped, basis, wide()).unwrap().value), expected_summaries(&old));
        assert_eq!(plain(&pinned.execute_graph_pattern_governed(&cx, &plan, wide()).unwrap().value), old);
        assert_eq!(plan.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_children_keep_phantoms_after_empty_pages_refusals_and_grouping() {
    let ((), report) = run_async_under_lab(0x1ade_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for kind in 0..4 { for mode in 0..3 { for change in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
            let mut txn = db.begin(&txn_cx).unwrap(); let mut staged = WriteBatch::new(R);
            staged.create_vertex(VId(777), vec![], vec![]); txn.write(&mut db, staged).unwrap();
            let take = u64::from(mode != 2);
            let policy = GqlQueryPolicy::new(1000, u64::from(mode == 0), 5_000_000, 2_000_000);
            if kind == 3 {
                let result = txn.execute_graph_aggregate_governed(&db, &cx, &aggregate(take), policy);
                if mode == 1 { assert!(matches!(result, Err(GqlQueryError::Rows(_)))); }
                else { assert_eq!(result.unwrap().value.len(), usize::from(mode == 0)); }
            } else {
                let plan = if kind == 0 { optional(take) } else { probe(kind == 2, take) };
                let result = txn.execute_graph_pattern_governed(&db, &cx, &plan, policy);
                if mode == 1 { assert!(matches!(result, Err(GqlQueryError::Rows(_)))); }
                else { assert_eq!(result.unwrap().value.len(), usize::from(mode == 0)); }
            }
            if change != 0 {
                let mut winner = WriteBatch::new(R);
                match change {
                    1 => { winner.create_vertex(VId(12), vec![FLAG], vec![(P, CanonicalScalar::Int(150))]); }
                    2 => { winner.set_vertex_property(VId(11), P, Some(CanonicalScalar::Int(150))); }
                    _ => { winner.delete_vertex(VId(10)); }
                }
                db.write(&commit, winner).await.unwrap();
            }
            let frontier = db.frontier().unwrap();
            // No intervening txn read may repair a lost independent scan.
            let result = txn.commit(&mut db, &commit).await;
            if change == 0 { result.unwrap(); assert!(db.vertex(VId(777)).unwrap().is_some()); }
            else {
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", .. }))));
                assert_eq!(db.frontier().unwrap(), frontier); assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }}}
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mixed_edge_roots_and_independent_children_admit_both_tables_under_one_policy() {
    let ((), report) = run_async_under_lab(0x1ade_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
        let plan = PreparedGraphText::prepare("MATCH (a:Root)-[:R]->(b) \
            OPTIONAL MATCH (x:Flag)-[:R]->(y) RETURN a,b,x,y LIMIT 2", symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap();
        assert!(!plan.plan().scans_edges()); assert_eq!(plan.required_vertex_label(), None);
        let measured = db.execute_graph_pattern_governed(&cx, &plan, wide()).unwrap();
        assert_eq!(measured.rows.snapshot_records, 8); assert_eq!(measured.value.len(), 2);
        for row in &measured.value {
            assert_eq!(row.values().iter().map(|value| value.as_vertex()).collect::<Vec<_>>(),
                vec![Some(VId(1)), Some(VId(2)), Some(VId(10)), Some(VId(11))]);
        }
        let work = measured.evaluator.work_units; let scratch = measured.evaluator.scratch_entries;
        assert_eq!(db.execute_graph_pattern_governed(&cx, &plan, GqlQueryPolicy::new(8, 2, work, scratch)).unwrap(), measured);
        for policy in [GqlQueryPolicy::new(7, 2, u64::MAX, u64::MAX), GqlQueryPolicy::new(8, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(8, 2, work - 1, u64::MAX), GqlQueryPolicy::new(8, 2, u64::MAX, scratch - 1)] {
            assert!(db.execute_graph_pattern_governed(&cx, &plan, policy).is_err());
        }
        let absent = PreparedGraphText::prepare("MATCH (a:Root) OPTIONAL MATCH (f:Flag) WHERE f.p>99 \
            RETURN a,f,f.p AS value", symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        assert_eq!(plain(&db.execute_graph_pattern_governed(&cx, &absent, wide()).unwrap().value),
            vec![(VId(1), None, None), (VId(2), None, None)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
