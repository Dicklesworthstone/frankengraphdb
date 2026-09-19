//! Cheapest paths keep the durable source, historical costs and transaction reads.
//! The expected routes enumerate owned edge records independently of PathFind.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::{
    GqlQueryError, GqlQueryPolicy, GraphCheapestPathError, GraphCostPath, GraphPathCostError,
    GraphWalkBounds, PreparedGraphCheapestPath,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

type Answer = Option<(i128, Vec<(EId, VId)>)>;
const R: RelationId = RelationId(1);
const W: PropertyKeyId = PropertyKeyId(7);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1, 1_000_000, 100_000)
}
fn query(
    source: u128,
    target: u128,
    direction: GlaDirection,
    lo: u32,
    hi: u32,
) -> PreparedGraphCheapestPath {
    PreparedGraphCheapestPath::new(
        VId(source),
        VId(target),
        R,
        direction,
        W,
        GraphWalkBounds::new(lo, hi).unwrap(),
    )
    .unwrap()
}
fn answer(rows: &[GraphCostPath]) -> Answer {
    assert!(rows.len() <= 1);
    rows.first()
        .map(|row| (row.cost(), row.path().steps().to_vec()))
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in [0, 1, 2, 3, 99] {
        batch.create_vertex(VId(id), vec![], vec![]);
    }
    for (id, from, to, weight) in [
        (9, 0, 3, 20),
        (4, 0, 1, 4),
        (5, 0, 1, 4),
        (6, 1, 2, -7),
        (7, 2, 3, 2),
        (2, 1, 1, 0),
    ] {
        batch.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![(W, CanonicalScalar::Int(weight))],
        );
    }
    let mut unrelated = WriteBatch::new(RelationId(2));
    unrelated.add_edge(EId(100), VId(99), VId(0), vec![]);
    db.write_atomic(cx, vec![batch, unrelated]).await.unwrap()
}
fn oracle(edges: &[EdgeRecord], query: &PreparedGraphCheapestPath) -> Answer {
    let mut layer = vec![(query.source(), 0_i128, Vec::<(EId, VId)>::new())];
    let mut candidates = Vec::new();
    for depth in 0..=query.bounds().maximum() {
        if depth >= query.bounds().minimum() {
            for (end, cost, steps) in &layer {
                if *end == query.target() {
                    candidates.push((*cost, steps.clone()));
                }
            }
        }
        if depth == query.bounds().maximum() {
            break;
        }
        let mut next = Vec::new();
        for (end, cost, steps) in layer {
            for edge in edges
                .iter()
                .filter(|edge| edge.entry.relation == query.relation())
            {
                let entry = &edge.entry;
                let weight = edge
                    .props
                    .iter()
                    .find_map(|(key, value)| match value {
                        CanonicalScalar::Int(value) if *key == query.weight_property() => {
                            Some(*value)
                        }
                        _ => None,
                    })
                    .expect("oracle fixtures have integer weights");
                let mut endpoints = Vec::new();
                if query.direction() != GlaDirection::Reverse && entry.src == end {
                    endpoints.push(entry.dst);
                }
                if query.direction() != GlaDirection::Forward
                    && entry.dst == end
                    && (query.direction() != GlaDirection::Undirected || entry.src != entry.dst)
                {
                    endpoints.push(entry.src);
                }
                for endpoint in endpoints {
                    let mut child = steps.clone();
                    child.push((entry.eid, endpoint));
                    next.push((endpoint, cost + i128::from(weight), child));
                }
            }
        }
        layer = next;
    }
    candidates.into_iter().min()
}

#[test]
fn snapshots_staged_costs_cascades_and_reopen_preserve_real_routes() {
    let ((), report) = run_async_under_lab(0xc0a5_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        let old_edges = db.edges().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for (source, target) in [(0, 3), (3, 0), (99, 99), (0, 99)] {
                let query = query(source, target, direction, 0, 4);
                let expected = oracle(&old_edges, &query);
                for result in [
                    db.execute_graph_cheapest_path_governed(&cx, &query, wide())
                        .unwrap(),
                    db.execute_graph_cheapest_path_governed_at(&cx, &query, basis, wide())
                        .unwrap(),
                    view.execute_graph_cheapest_path_governed(&cx, &query, wide())
                        .unwrap(),
                    view.execute_graph_cheapest_path_governed_at(&cx, &query, basis, wide())
                        .unwrap(),
                    txn.execute_graph_cheapest_path_governed(&db, &cx, &query, wide())
                        .unwrap(),
                ] {
                    assert_eq!(answer(&result.value), expected);
                    assert_eq!(
                        result.rows.snapshot_records, 12,
                        "retain vertices, isolates and unrelated edges in source accounting"
                    );
                }
            }
        }
        let query = query(0, 3, GlaDirection::Forward, 1, 4);
        let old = oracle(&old_edges, &query);
        assert_eq!(
            old,
            Some((
                -1,
                vec![
                    (EId(4), VId(1)),
                    (EId(2), VId(1)),
                    (EId(6), VId(2)),
                    (EId(7), VId(3))
                ]
            ))
        );
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(4));
        changes.set_edge_property(EId(5), W, Some(CanonicalScalar::Int(9)));
        changes.set_edge_property(EId(6), W, Some(CanonicalScalar::Int(-11)));
        changes.ensure_edge_by_triple(
            EId(999),
            VId(0),
            VId(1),
            vec![(W, CanonicalScalar::Int(-999))],
        );
        txn.write(&mut db, changes).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let changed = oracle(&txn.edges(&db).unwrap(), &query);
        assert_eq!(changed.as_ref().unwrap().0, 0);
        assert_eq!(
            answer(
                &txn.execute_graph_cheapest_path_governed(&db, &cx, &query, wide())
                    .unwrap()
                    .value
            ),
            changed
        );
        assert_eq!(
            answer(
                &db.execute_graph_cheapest_path_governed(&cx, &query, wide())
                    .unwrap()
                    .value
            ),
            old
        );
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(1));
        cascade.add_edge(EId(8), VId(0), VId(3), vec![(W, CanonicalScalar::Int(-2))]);
        txn.write(&mut db, cascade).unwrap();
        let expected = oracle(&txn.edges(&db).unwrap(), &query);
        assert_eq!(expected, Some((-2, vec![(EId(8), VId(3))])));
        assert_eq!(
            answer(
                &txn.execute_graph_cheapest_path_governed(&db, &cx, &query, wide())
                    .unwrap()
                    .value
            ),
            expected
        );
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            answer(
                &reopened
                    .execute_graph_cheapest_path_governed(&cx, &query, wide())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            answer(
                &reopened
                    .execute_graph_cheapest_path_governed_at(&cx, &query, basis, wide())
                    .unwrap()
                    .value
            ),
            old
        );
        assert_eq!(
            answer(
                &view
                    .execute_graph_cheapest_path_governed(&cx, &query, wide())
                    .unwrap()
                    .value
            ),
            old
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_combined_limits_and_cost_owner_and_frontier_refusals() {
    let ((), report) = run_async_under_lab(0xc0a5_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let query = query(0, 3, GlaDirection::Forward, 1, 4);
        let measured = db
            .execute_graph_cheapest_path_governed(&cx, &query, wide())
            .unwrap();
        let exact = GqlQueryPolicy::new(
            12,
            1,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_cheapest_path_governed(&cx, &query, exact)
                .unwrap(),
            measured
        );
        for (policy, evaluator_limit) in [
            (GqlQueryPolicy::new(11, 1, u64::MAX, u64::MAX), None),
            (GqlQueryPolicy::new(12, 0, u64::MAX, u64::MAX), None),
            (
                GqlQueryPolicy::new(12, 1, exact.evaluator.max_work_units - 1, u64::MAX),
                Some(exact.evaluator.max_work_units - 1),
            ),
            (
                GqlQueryPolicy::new(12, 1, u64::MAX, exact.evaluator.max_scratch_entries - 1),
                Some(exact.evaluator.max_scratch_entries - 1),
            ),
        ] {
            match db.execute_graph_cheapest_path_governed(&cx, &query, policy) {
                Err(GqlQueryError::Rows(_)) => assert!(evaluator_limit.is_none()),
                Err(GqlQueryError::Evaluator(error)) => {
                    assert_eq!(Some(error.limit), evaluator_limit)
                }
                other => panic!("expected exact refusal, got {other:?}"),
            }
        }
        let txn = db.begin(&txn_cx).unwrap();
        let tx_measured = txn
            .execute_graph_cheapest_path_governed(&db, &cx, &query, wide())
            .unwrap();
        let tx_exact = GqlQueryPolicy::new(
            12,
            1,
            tx_measured.evaluator.work_units,
            tx_measured.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_cheapest_path_governed(&db, &cx, &query, tx_exact)
                .unwrap(),
            tx_measured
        );
        let other = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            txn.execute_graph_cheapest_path_governed(
                &other,
                &cx,
                &query,
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(GqlQueryError::Source(GraphCheapestPathError::Source(_)))
        ));
        let future = CommitSeq(db.frontier().unwrap().0 + 1);
        assert!(matches!(
            db.execute_graph_cheapest_path_governed_at(
                &cx,
                &query,
                future,
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(GqlQueryError::Source(GraphCheapestPathError::Source(
                GqlError::Read(_)
            )))
        ));
        let view = db.read_session().unwrap();
        assert!(matches!(
            view.execute_graph_cheapest_path_governed_at(
                &cx,
                &query,
                future,
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(GqlQueryError::Source(GraphCheapestPathError::Source(
                GqlError::Read(_)
            )))
        ));
        let mut invalid = WriteBatch::new(R);
        invalid.set_edge_property(EId(6), W, None);
        db.write(&commit, invalid).await.unwrap();
        assert!(matches!(
            db.execute_graph_cheapest_path_governed(&cx, &query, wide()),
            Err(GqlQueryError::Source(GraphCheapestPathError::Cost(
                GraphPathCostError::MissingWeight
            )))
        ));
        // A zero-hop identity result cannot hide an invalid selected-relation cost.
        let zero = PreparedGraphCheapestPath::new(
            VId(99),
            VId(99),
            R,
            GlaDirection::Forward,
            W,
            GraphWalkBounds::new(0, 0).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            db.execute_graph_cheapest_path_governed(&cx, &zero, wide()),
            Err(GqlQueryError::Source(GraphCheapestPathError::Cost(
                GraphPathCostError::MissingWeight
            )))
        ));
        assert_eq!(
            view.execute_graph_cheapest_path_governed(&cx, &query, wide())
                .unwrap(),
            measured
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn successful_empty_and_refused_searches_retain_transaction_dependencies() {
    let ((), report) = run_async_under_lab(0xc0a5_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        for mode in 0..5 {
            for change in 0..3 {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R);
                stage.create_vertex(VId(777), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                if mode != 4 {
                    let query = PreparedGraphCheapestPath::new(
                        VId(0),
                        VId(if mode == 3 { 99 } else { 3 }),
                        R,
                        GlaDirection::Forward,
                        if mode == 2 { PropertyKeyId(999) } else { W },
                        GraphWalkBounds::new(1, 4).unwrap(),
                    )
                    .unwrap();
                    let result = txn.execute_graph_cheapest_path_governed(
                        &db,
                        &cx,
                        &query,
                        GqlQueryPolicy::new(1000, u64::from(mode != 1), 1_000_000, 100_000),
                    );
                    match mode {
                        0 => assert_eq!(result.unwrap().value.len(), 1),
                        1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                        2 => assert!(matches!(
                            result,
                            Err(GqlQueryError::Source(GraphCheapestPathError::Cost(_)))
                        )),
                        _ => assert!(result.unwrap().value.is_empty()),
                    }
                }
                let mut winner = WriteBatch::new(R);
                match change {
                    0 => {
                        winner.add_edge(
                            EId(15),
                            VId(0),
                            VId(3),
                            vec![(W, CanonicalScalar::Int(-100))],
                        );
                    }
                    1 => {
                        winner.set_edge_property(EId(9), W, Some(CanonicalScalar::Int(-100)));
                    }
                    _ => {
                        winner.delete_edge(EId(6));
                    }
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                // No subsequent read may accidentally repair a lost witness.
                let result = txn.commit(&mut db, &commit).await;
                if mode == 4 {
                    result.unwrap();
                    assert!(db.vertex(VId(777)).unwrap().is_some());
                } else {
                    assert!(matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_trillion_alternative_database_walks_do_not_materialize_candidate_paths() {
    let ((), report) = run_async_under_lab(0xc0a5_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for vertex in 0..=40 {
            batch.create_vertex(VId(vertex), vec![], vec![]);
        }
        for depth in 0..40 {
            for parallel in 0..2 {
                batch.add_edge(
                    EId(2 * depth + parallel),
                    VId(depth),
                    VId(depth + 1),
                    vec![(W, CanonicalScalar::Int(1))],
                );
            }
        }
        db.write(&commit, batch).await.unwrap();
        let query = query(0, 40, GlaDirection::Forward, 40, 40);
        let result = db
            .execute_graph_cheapest_path_governed(
                &cx,
                &query,
                GqlQueryPolicy::new(121, 1, 65_536, 8192),
            )
            .unwrap();
        assert_eq!(
            answer(&result.value),
            Some((
                40,
                (0..40)
                    .map(|depth| (EId(2 * depth), VId(depth + 1)))
                    .collect()
            ))
        );
        assert_eq!(result.rows.snapshot_records, 121);
        assert!(result.evaluator.work_units < 65_536);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
