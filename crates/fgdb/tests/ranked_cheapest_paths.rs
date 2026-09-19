//! Ranked alternatives are checked against complete owned-edge enumeration,
//! not another query or the pathfinder's suffix/partition implementation.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::{GqlQueryError, GqlQueryPolicy, GraphCheapestPathError, GraphCostPath,
    GraphPathCostError, GraphWalkBounds, PreparedGraphCheapestPath};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

type Answer = (i128, Vec<(EId, VId)>);
const R: RelationId = RelationId(1);
const W: PropertyKeyId = PropertyKeyId(7);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xe1; 32], DatabaseSecurityNamespaceId([0xe2; 32]), [0xe3; 32])
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 5_000_000, 1_000_000) }
fn query(source: u128, target: u128, direction: GlaDirection, lo: u32, hi: u32) -> PreparedGraphCheapestPath {
    PreparedGraphCheapestPath::new(VId(source), VId(target), R, direction, W,
        GraphWalkBounds::new(lo, hi).unwrap()).unwrap()
}
fn plain(rows: &[GraphCostPath]) -> Vec<Answer> {
    rows.iter().map(|row| (row.cost(), row.path().steps().to_vec())).collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in [0, 1, 2, 3, 99] { batch.create_vertex(VId(id), vec![], vec![]); }
    for (id, from, to, weight) in [(9, 0, 3, 20), (4, 0, 1, 4), (5, 0, 1, 4),
        (6, 1, 2, -7), (7, 2, 3, 2), (2, 1, 1, 0)] {
        batch.add_edge(EId(id), VId(from), VId(to), vec![(W, CanonicalScalar::Int(weight))]);
    }
    let mut unrelated = WriteBatch::new(RelationId(2));
    unrelated.add_edge(EId(100), VId(99), VId(0), vec![]);
    db.write_atomic(cx, vec![batch, unrelated]).await.unwrap()
}
fn oracle(edges: &[EdgeRecord], query: &PreparedGraphCheapestPath, count: usize) -> Vec<Answer> {
    let mut layer = vec![(query.source(), 0_i128, Vec::<(EId, VId)>::new())];
    let mut answers = Vec::new();
    for depth in 0..=query.bounds().maximum() {
        if depth >= query.bounds().minimum() {
            answers.extend(layer.iter().filter(|(end, _, _)| *end == query.target())
                .map(|(_, cost, path)| (*cost, path.clone())));
        }
        if depth == query.bounds().maximum() { break; }
        let mut next = Vec::new();
        for (end, cost, path) in layer {
            for edge in edges.iter().filter(|edge| edge.entry.relation == R) {
                let weight = edge.props.iter().find_map(|(key, value)| match value {
                    CanonicalScalar::Int(value) if *key == W => Some(*value),
                    _ => None,
                }).expect("integer oracle fixture");
                let entry = &edge.entry;
                let mut destinations = Vec::new();
                if query.direction() != GlaDirection::Reverse && entry.src == end { destinations.push(entry.dst); }
                if query.direction() != GlaDirection::Forward && entry.dst == end
                    && (query.direction() != GlaDirection::Undirected || entry.src != entry.dst) {
                    destinations.push(entry.src);
                }
                for destination in destinations {
                    let mut child = path.clone(); child.push((entry.eid, destination));
                    next.push((destination, cost + i128::from(weight), child));
                }
            }
        }
        layer = next;
    }
    answers.sort(); answers.truncate(count); answers
}

#[test]
fn ranked_paths_share_live_historical_pinned_and_canonical_transaction_sources() {
    let ((), report) = run_async_under_lab(0xc0a5_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap(); let before = db.edges().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
            let (source, target) = if direction == GlaDirection::Reverse { (3, 0) } else { (0, 3) };
            let query = query(source, target, direction, 1, 4);
            let expected = oracle(&before, &query, 7);
            for result in [
                db.execute_graph_cheapest_paths_governed(&cx, &query, 7, policy()).unwrap(),
                db.execute_graph_cheapest_paths_governed_at(&cx, &query, 7, basis, policy()).unwrap(),
                pinned.execute_graph_cheapest_paths_governed(&cx, &query, 7, policy()).unwrap(),
                pinned.execute_graph_cheapest_paths_governed_at(&cx, &query, 7, basis, policy()).unwrap(),
                txn.execute_graph_cheapest_paths_governed(&db, &cx, &query, 7, policy()).unwrap(),
            ] { assert_eq!(plain(&result.value), expected); }
        }
        let query = query(0, 3, GlaDirection::Forward, 0, 4);
        let old = oracle(&before, &query, 7);
        let mut stage = WriteBatch::new(R);
        stage.set_edge_property(EId(9), W, Some(CanonicalScalar::Int(-12)));
        stage.delete_edge(EId(4));
        stage.ensure_edge_by_triple(EId(888), VId(0), VId(1), vec![]);
        stage.add_edge(EId(8), VId(0), VId(0), vec![(W, CanonicalScalar::Int(-2))]);
        txn.write(&mut db, stage).unwrap();
        let staged_edges = txn.edges(&db).unwrap();
        assert!(staged_edges.iter().all(|edge| edge.entry.eid != EId(888)));
        let staged = oracle(&staged_edges, &query, 7);
        assert_eq!(plain(&txn.execute_graph_cheapest_paths_governed(&db, &cx, &query, 7, policy()).unwrap().value), staged);
        assert_eq!(plain(&db.execute_graph_cheapest_paths_governed(&cx, &query, 7, policy()).unwrap().value), old);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(plain(&db.execute_graph_cheapest_paths_governed(&cx, &query, 7, policy()).unwrap().value), staged);
        assert_eq!(plain(&db.execute_graph_cheapest_paths_governed_at(&cx, &query, 7, basis, policy()).unwrap().value), old);
        assert_eq!(plain(&pinned.execute_graph_cheapest_paths_governed(&cx, &query, 7, policy()).unwrap().value), old);
        let mut cascade = WriteBatch::new(R); cascade.delete_vertex(VId(1));
        db.write(&commit, cascade).await.unwrap();
        let expected = oracle(&db.edges().unwrap(), &query, 7);
        assert_eq!(plain(&db.execute_graph_cheapest_paths_governed(&cx, &query, 7, policy()).unwrap().value), expected);
        assert_eq!(plain(&pinned.execute_graph_cheapest_paths_governed(&cx, &query, 7, policy()).unwrap().value), old);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ranked_prefixes_share_original_source_and_evaluator_limits() {
    let ((), report) = run_async_under_lab(0xc0a5_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let query = query(0, 3, GlaDirection::Forward, 1, 4);
        let baseline = db.execute_graph_cheapest_paths_governed(&cx, &query, 3, policy()).unwrap();
        assert_eq!(baseline.rows.result_rows, 3);
        let exact = GqlQueryPolicy::new(baseline.rows.snapshot_records, 3,
            baseline.evaluator.work_units, baseline.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_cheapest_paths_governed(&cx, &query, 3, exact).unwrap(), baseline);
        for bad in [
            GqlQueryPolicy::new(baseline.rows.snapshot_records - 1, 3, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(baseline.rows.snapshot_records, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(baseline.rows.snapshot_records, 3, baseline.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(baseline.rows.snapshot_records, 3, u64::MAX, baseline.evaluator.scratch_entries - 1),
        ] { assert!(db.execute_graph_cheapest_paths_governed(&cx, &query, 3, bad).is_err()); }
        assert_eq!(db.execute_graph_cheapest_paths_governed(&cx, &query, 0,
            GqlQueryPolicy::new(1000, 0, 5_000_000, 1_000_000)).unwrap().rows.result_rows, 0);
        assert!(matches!(db.execute_graph_cheapest_paths_governed_at(&cx, &query, 0, CommitSeq(basis.0 + 1),
            GqlQueryPolicy::new(0, 0, 0, 0)), Err(GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(_))))));
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txcx).unwrap();
        assert!(matches!(txn.execute_graph_cheapest_paths_governed(&foreign, &cx, &query, 0,
            GqlQueryPolicy::new(0, 0, 0, 0)), Err(GqlQueryError::Source(GraphCheapestPathError::Source(_)))));
        let mut invalid = WriteBatch::new(R); invalid.set_edge_property(EId(9), W, None);
        db.write(&commit, invalid).await.unwrap();
        assert!(matches!(db.execute_graph_cheapest_paths_governed(&cx, &query, 0, policy()),
            Err(GqlQueryError::Source(GraphCheapestPathError::Cost(GraphPathCostError::MissingWeight)))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ranked_success_empty_zero_and_refused_queries_keep_commit_dependencies() {
    let ((), report) = run_async_under_lab(0xc0a5_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txcx = contexts.txn();
        for mode in 0..5 {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
                if mode == 4 {
                    let mut invalid = WriteBatch::new(R); invalid.set_edge_property(EId(9), W, None);
                    db.write(&commit, invalid).await.unwrap();
                }
                let mut txn = db.begin(&txcx).unwrap();
                let mut stage = WriteBatch::new(R); stage.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let query = query(0, if mode == 2 { 99 } else { 3 }, GlaDirection::Forward, 1, 4);
                let result = txn.execute_graph_cheapest_paths_governed(&db, &cx, &query,
                    if mode == 1 { 0 } else { 3 }, GqlQueryPolicy::new(1000,
                        if mode == 3 { 0 } else { 1000 }, 5_000_000, 1_000_000));
                match mode {
                    0 => assert_eq!(result.unwrap().value.len(), 3),
                    1 | 2 => assert!(result.unwrap().value.is_empty()),
                    3 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                    _ => assert!(matches!(result, Err(GqlQueryError::Source(GraphCheapestPathError::Cost(GraphPathCostError::MissingWeight))))),
                }
                let mut winner = WriteBatch::new(R);
                match change {
                    0 => { winner.set_edge_property(EId(9), W, Some(CanonicalScalar::Int(-20))); }
                    1 => { winner.add_edge(EId(90), VId(0), VId(3), vec![(W, CanonicalScalar::Int(-30))]); }
                    _ => { winner.delete_vertex(VId(2)); }
                }
                db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
                // There is deliberately no second graph read before commit.
                assert!(matches!(txn.commit(&mut db, &commit).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(777)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn actual_database_returns_four_of_two_to_the_fortieth_routes_under_one_allowance() {
    let ((), report) = run_async_under_lab(0xc0a5_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..=40 { batch.create_vertex(VId(id), vec![], vec![]); }
        for depth in 0..40 {
            for parallel in 0..2 { batch.add_edge(EId(depth * 2 + parallel), VId(depth), VId(depth + 1), vec![(W, CanonicalScalar::Int(1))]); }
        }
        db.write(&commit, batch).await.unwrap();
        let query = query(0, 40, GlaDirection::Forward, 40, 40);
        let result = db.execute_graph_cheapest_paths_governed(&cx, &query, 4,
            GqlQueryPolicy::new(121, 4, 100_000, 50_000)).unwrap();
        assert_eq!(result.rows.snapshot_records, 121);
        assert_eq!(result.rows.result_rows, 4);
        for (ordinal, row) in result.value.iter().enumerate() {
            assert_eq!(row.cost(), 40);
            assert_eq!(row.path().edges().collect::<Vec<_>>(), (0..40_u128).map(|depth|
                EId(depth * 2 + (((ordinal as u128) >> (39 - depth)) & 1))).collect::<Vec<_>>());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[path = "ranked_cheapest_paths/modes.rs"]
mod modes;
