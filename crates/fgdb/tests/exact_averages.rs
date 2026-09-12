//! Exact means and DISTINCT sums compose with canonical database sources.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::cmp::Ordering;
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
const OWNER: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const HEAD: &str = "MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b) RETURN a,COUNT(b.p) AS n,SUM(DISTINCT b.p) AS total,AVG(b.p) AS mean,AVG(DISTINCT b.p) AS unique_mean GROUP BY a ORDER BY mean DESC NULLS LAST,a";
type Plain = (VId, u64, Option<i128>, Option<(i128, u64)>, Option<(i128, u64)>);
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32]) }
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 2_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn query(count: Option<u64>) -> PreparedGraphAggregate {
    let suffix = count.map_or(String::new(), |count| format!(" LIMIT {count}"));
    PreparedGraphAggregateText::prepare(&format!("{HEAD}{suffix}"), symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in 0..4 { batch.create_vertex(VId(id), vec![OWNER], vec![]); }
    for (id, value) in [(10, i64::MAX - 1), (11, i64::MAX), (12, 3), (13, 5)] {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
    }
    for (id, owner, destination) in [(1, 0, 10), (2, 0, 11), (3, 0, 11),
        (4, 1, 11), (5, 3, 12), (6, 3, 12), (7, 3, 13)] {
        batch.add_edge(EId(id), VId(owner), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn mean(values: &[i64]) -> Option<(i128, u64)> {
    if values.is_empty() { return None; }
    let sum = values.iter().map(|v| i128::from(*v)).sum::<i128>();
    let count = values.len() as u64;
    let (mut a, mut b) = (sum.unsigned_abs(), u128::from(count));
    while b != 0 { let remainder = a % b; a = b; b = remainder; }
    Some((sum / a as i128, count / a as u64))
}
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Plain> {
    let mut output = Vec::new();
    for owner in vertices.iter().filter(|row| row.labels.contains(&OWNER)) {
        let mut values = Vec::new();
        for edge in edges.iter().filter(|edge| edge.entry.relation == R && edge.entry.src == owner.vid) {
            let destination = vertices.iter().find(|row| row.vid == edge.entry.dst).unwrap();
            if let Some((_, CanonicalScalar::Int(value))) = destination.props.iter().find(|(key, _)| *key == P) {
                values.push(*value);
            }
        }
        let distinct: Vec<_> = values.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
        output.push((owner.vid, values.len() as u64,
            (!distinct.is_empty()).then(|| distinct.iter().map(|v| i128::from(*v)).sum()), mean(&values), mean(&distinct)));
    }
    output.sort_by(|a, b| {
        // This fixture's counts are <= 3: independent cross products fit i128.
        let order = match (a.3, b.3) {
            (None, None) => Ordering::Equal, (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some((a, da)), Some((b, db))) => (b * i128::from(da)).cmp(&(a * i128::from(db))),
        };
        order.then_with(|| a.0.cmp(&b.0))
    });
    output
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Plain> {
    rows.iter().map(|row| {
        let average = |at| row.get(at).unwrap().as_average().map(|v| (v.numerator(), v.denominator()));
        (row.keys()[0].as_vertex().unwrap(), row.get(0).unwrap().as_count().unwrap(),
            row.get(1).unwrap().as_integer(), average(2), average(3))
    }).collect()
}

#[test]
fn exact_means_cover_all_reads_canonical_staging_and_reopened_history() {
    let ((), report) = run_async_under_lab(0xa6e0_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap(); let query = query(None); let frozen = query.canonical_bytes();
        let old = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(old.iter().map(|row| row.0).collect::<Vec<_>>(), vec![VId(1), VId(0), VId(3), VId(2)]);
        assert_eq!(old[1].3, Some((3 * i128::from(i64::MAX) - 1, 3)));
        assert_eq!(old[1].4, Some((2 * i128::from(i64::MAX) - 1, 2)));
        for result in [db.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &query, basis, wide()).unwrap(),
            pinned.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap(),
            pinned.execute_graph_aggregate_governed_at(&cx, &query, basis, wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &query, wide()).unwrap()] { assert_eq!(plain(&result.value), old); }
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(1)); changes.ensure_edge_by_triple(EId(999), VId(0), VId(11), vec![]);
        changes.set_vertex_property(VId(11), P, Some(CanonicalScalar::Int(7)));
        changes.set_vertex_property(VId(13), P, None);
        changes.add_edge(EId(90), VId(2), VId(12), vec![]); changes.delete_vertex(VId(10));
        txn.write(&mut db, changes).unwrap(); assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        assert_eq!(expected.iter().map(|row| row.0).collect::<Vec<_>>(), vec![VId(0), VId(1), VId(2), VId(3)]);
        assert_eq!(plain(&txn.execute_graph_aggregate_governed(&db, &cx, &query, wide()).unwrap().value), expected);
        assert_eq!(plain(&db.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap(); db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), expected);
        assert_eq!(plain(&reopened.execute_graph_aggregate_governed_at(&cx, &query, basis, wide()).unwrap().value), old);
        assert_eq!(plain(&pinned.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), old);
        assert_eq!(query.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changed_denominators_and_discarded_means_keep_conflicts_after_refusal() {
    let ((), report) = run_async_under_lab(0xa6e0_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..3 { for change in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
            let mut txn = db.begin(&txn_cx).unwrap(); let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(777), vec![], vec![]); txn.write(&mut db, stage).unwrap();
            let query = query(Some(u64::from(mode != 2)));
            let result = txn.execute_graph_aggregate_governed(&db, &cx, &query,
                GqlQueryPolicy::new(1000, u64::from(mode == 0), 2_000_000, 1_000_000));
            match mode { 0 => assert_eq!(plain(&result.unwrap().value)[0].0, VId(1)),
                1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))), _ => assert!(result.unwrap().value.is_empty()) }
            let mut winner = WriteBatch::new(R);
            match change {
                0 => { winner.create_vertex(VId(888), vec![], vec![]); }
                1 => { winner.set_vertex_property(VId(10), P, Some(CanonicalScalar::Int(i64::MAX))); }
                2 => { winner.delete_edge(EId(4)); }
                3 => { winner.add_edge(EId(90), VId(1), VId(11), vec![]); }
                _ => { winner.set_vertex_property(VId(11), P, None); }
            }
            db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
            // No subsequent transaction read can restore a missing observation.
            let result = txn.commit(&mut db, &commit).await;
            if change == 0 { result.unwrap(); assert!(db.vertex(VId(777)).unwrap().is_some()); }
            else {
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                assert_eq!(db.frontier().unwrap(), frontier); assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }}
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_average_database_policies_share_source_and_result_allowances() {
    let ((), report) = run_async_under_lab(0xa6e0_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
        let query = query(Some(2)); let measured = db.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap();
        let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, 2,
            measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_aggregate_governed(&cx, &query, exact).unwrap(), measured);
        for policy in [GqlQueryPolicy::new(measured.rows.snapshot_records - 1, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 2, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 2, u64::MAX, measured.evaluator.scratch_entries - 1)] {
            assert!(db.execute_graph_aggregate_governed(&cx, &query, policy).is_err());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
