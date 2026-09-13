//! Group retirement must not retire transaction observations or historical data.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x81; 32], DatabaseSecurityNamespaceId([0x82; 32]), [0x83; 32]) }
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 2_000_000, 1_000_000) }
fn template() -> PreparedGraphAggregateText {
    PreparedGraphAggregateText::prepare("MATCH (a:L)-[:R]->(b) \
        RETURN a,COUNT(*) AS n,AVG(b.p) AS mean,SUM(DISTINCT b.p) AS unique_total GROUP BY a \
        HAVING COUNT(b.p) >= $minimum OR mean IS NULL ORDER BY mean DESC NULLS LAST,a SKIP $off LIMIT $take",
        |kind, name| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }).unwrap()
}
fn arguments(minimum: i64, offset: u64, count: u64) -> GqlParameters {
    GqlParameters::new().with_int64("minimum", minimum).unwrap().with_uint64("off", offset).unwrap()
        .with_uint64("take", count).unwrap()
}
fn query(count: u64) -> PreparedGraphAggregate { template().bind_parameters(&arguments(2, 0, count)).unwrap() }
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4 { batch.create_vertex(VId(id), vec![L], vec![]); }
    for (id, value) in [(10, Some(1)), (11, Some(7)), (12, Some(3)), (13, Some(9)), (14, None), (15, Some(5))] {
        batch.create_vertex(VId(id), vec![], value.map(|value| (P, CanonicalScalar::Int(value))).into_iter().collect());
    }
    for (id, src, dst) in [(1,1,10), (2,1,11), (3,1,11), (4,2,12), (5,2,13), (6,3,14), (7,4,15)] {
        batch.add_edge(EId(id), VId(src), VId(dst), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
#[derive(Clone)]
struct Expected { root: VId, count: u64, mean: Option<(i128, u64)>, unique: Option<i128> }
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord], minimum: i64) -> Vec<Expected> {
    let mut groups: BTreeMap<VId, Vec<Option<i64>>> = BTreeMap::new();
    for edge in edges.iter().filter(|edge| edge.entry.relation == R) {
        let source = vertices.iter().find(|row| row.vid == edge.entry.src).unwrap();
        if !source.labels.contains(&L) { continue; }
        let destination = vertices.iter().find(|row| row.vid == edge.entry.dst).unwrap();
        let value = destination.props.iter().find_map(|(key, value)| match value {
            CanonicalScalar::Int(value) if *key == P => Some(*value), _ => None,
        });
        groups.entry(source.vid).or_default().push(value);
    }
    let mut result = Vec::new();
    for (root, bag) in groups {
        let values: Vec<_> = bag.iter().flatten().copied().collect();
        if !values.is_empty() && (values.len() as i128) < i128::from(minimum) { continue; }
        let mean = (!values.is_empty()).then(|| (values.iter().map(|n| i128::from(*n)).sum(), values.len() as u64));
        let unique = (!values.is_empty()).then(|| values.into_iter().collect::<BTreeSet<_>>().into_iter().map(i128::from).sum());
        result.push(Expected { root, count: bag.len() as u64, mean, unique });
    }
    result.sort_by(|a, b| match (a.mean, b.mean) {
        (None, None) => Ordering::Equal, (None, Some(_)) => Ordering::Greater, (Some(_), None) => Ordering::Less,
        (Some((a, na)), Some((b, nb))) => (b * i128::from(na)).cmp(&(a * i128::from(nb))),
    }.then_with(|| a.root.cmp(&b.root)));
    result
}
fn check(rows: &[GraphAggregateRow], expected: &[Expected]) {
    assert_eq!(rows.len(), expected.len());
    for (row, expected) in rows.iter().zip(expected) {
        assert_eq!(row.keys()[0].as_vertex(), Some(expected.root));
        assert_eq!(row.values().len(), 3);
        assert_eq!(row.get(0).unwrap().as_count(), Some(expected.count));
        assert_eq!(row.get(2).unwrap().as_integer(), expected.unique);
        match expected.mean {
            None => assert!(row.get(1).unwrap().is_null()),
            Some((sum, count)) => {
                let mean = row.get(1).unwrap().as_average().unwrap();
                assert_eq!(mean.numerator() * i128::from(count), sum * i128::from(mean.denominator()));
            }
        }
    }
}

#[test]
fn retired_groups_use_the_same_canonical_overlay_and_all_historical_entrypoints() {
    let ((), report) = run_async_under_lab(0x817e_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let pinned = db.read_session().unwrap();
        let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), 2);
        assert_eq!(expected.iter().map(|row| row.root).collect::<Vec<_>>(), vec![VId(2), VId(1), VId(3)]);
        let plan = query(3); let frozen = plan.canonical_bytes(); let mut txn = db.begin(&txn_cx).unwrap();
        for result in [db.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap(),
            db.execute_graph_aggregate_governed_at(&cx, &plan, basis, wide()).unwrap(),
            pinned.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap(),
            pinned.execute_graph_aggregate_governed_at(&cx, &plan, basis, wide()).unwrap(),
            txn.execute_graph_aggregate_governed(&db, &cx, &plan, wide()).unwrap()] { check(&result.value, &expected); }
        let mut staged = WriteBatch::new(R);
        staged.delete_edge(EId(1)); staged.ensure_edge_by_triple(EId(999), VId(1), VId(11), vec![]);
        staged.set_vertex_property(VId(13), P, None);
        staged.set_vertex_property(VId(15), P, Some(CanonicalScalar::Int(9)));
        staged.add_edge(EId(90), VId(4), VId(15), vec![]); staged.delete_vertex(VId(10));
        txn.write(&mut db, staged).unwrap(); assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let changed = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap(), 2);
        assert_eq!(changed.iter().map(|row| row.root).collect::<Vec<_>>(), vec![VId(4), VId(1), VId(3)]);
        check(&txn.execute_graph_aggregate_governed(&db, &cx, &plan, wide()).unwrap().value, &changed);
        check(&db.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap().value, &expected);
        txn.commit(&mut db, &commit).await.unwrap(); db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        check(&reopened.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap().value, &changed);
        check(&reopened.execute_graph_aggregate_governed_at(&cx, &plan, basis, wide()).unwrap().value, &expected);
        check(&pinned.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap().value, &expected);
        let relaxed = template().bind_parameters(&arguments(1, 1, 2)).unwrap();
        let all = oracle(&reopened.vertices().unwrap(), &reopened.edges().unwrap(), 1);
        check(&reopened.execute_graph_aggregate_governed(&cx, &relaxed, wide()).unwrap().value, &all[1..3]);
        assert_eq!(plan.canonical_bytes(), frozen); assert_ne!(relaxed.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rejected_or_evicted_groups_keep_conflict_dependencies_after_refusal_or_zero_limit() {
    let ((), report) = run_async_under_lab(0x817e_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..3 { for change in 0..5 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
            let mut txn = db.begin(&txn_cx).unwrap(); let mut staged = WriteBatch::new(R);
            staged.create_vertex(VId(777), vec![], vec![]); txn.write(&mut db, staged).unwrap();
            let plan = query(u64::from(mode != 2));
            let result = txn.execute_graph_aggregate_governed(&db, &cx, &plan,
                GqlQueryPolicy::new(1000, u64::from(mode == 0), 2_000_000, 1_000_000));
            match mode {
                0 => assert_eq!(result.unwrap().value[0].keys()[0].as_vertex(), Some(VId(2))),
                1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                _ => assert!(result.unwrap().value.is_empty()),
            }
            let mut winner = WriteBatch::new(R);
            match change {
                0 => { winner.create_vertex(VId(888), vec![], vec![]); }
                1 => { winner.set_vertex_property(VId(10), P, Some(CanonicalScalar::Int(100))); }
                2 => { winner.set_vertex_property(VId(15), P, Some(CanonicalScalar::Int(9)));
                    winner.add_edge(EId(90), VId(4), VId(15), vec![]); }
                3 => { winner.delete_edge(EId(1)); }
                _ => { winner.delete_edge(EId(5)); }
            }
            db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
            // No intervening transaction read may repair a dependency lost by
            // group retirement, HAVING rejection, or top-page eviction.
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
fn database_group_retirement_preserves_each_exact_governed_limit() {
    let ((), report) = run_async_under_lab(0x817e_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root); let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
        let plan = query(2); let measured = db.execute_graph_aggregate_governed(&cx, &plan, wide()).unwrap();
        let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, 2, measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_aggregate_governed(&cx, &plan, exact).unwrap(), measured);
        for policy in [GqlQueryPolicy::new(measured.rows.snapshot_records - 1, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 1, u64::MAX, u64::MAX), GqlQueryPolicy::new(1000, 2, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 2, u64::MAX, measured.evaluator.scratch_entries - 1)] {
            assert!(db.execute_graph_aggregate_governed(&cx, &plan, policy).is_err());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
