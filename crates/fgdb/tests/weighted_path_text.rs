//! Weighted text exercises the actual Database/View/WriteTxn entrypoints.
//! Ordinary typed queries also check identical source/evaluator accounting.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError, GqlError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::{BoundGraphCheapestPathQuery, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphCheapestPathError, GraphCheapestPathMode, GraphPathCostError, GraphSymbol,
    GraphSymbolKind, GraphWalkBounds, PreparedGraphCheapestPath, PreparedGraphCheapestPathText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(7);
const W: PropertyKeyId = PropertyKeyId(9);
const START: VId = VId(1_u128 << 100);
const END: VId = VId(u128::MAX);
const MODES: [(&str, GraphCheapestPathMode); 4] = [("WALK", GraphCheapestPathMode::Walk),
    ("TRAIL", GraphCheapestPathMode::Trail), ("ACYCLIC", GraphCheapestPathMode::Acyclic), ("SIMPLE", GraphCheapestPathMode::Simple)];
fn keys() -> DatabaseKeys { DatabaseKeys::new([0xa1; 32], DatabaseSecurityNamespaceId([0xa2; 32]), [0xa3; 32]) }
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 5_000_000, 1_000_000) }
fn resolver(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "ROAD") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "cost") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}
fn request(mode: &str, direction: GlaDirection, count: Option<u64>, source: VId, target: VId) -> BoundGraphCheapestPathQuery {
    let selector = if count.is_some() { "CHEAPEST $k" } else { "ANY CHEAPEST" };
    let left = if direction == GlaDirection::Reverse { "<-" } else { "-" };
    let right = if direction == GlaDirection::Forward { "->" } else { "-" };
    let input = format!("MATCH p = {selector} {mode} (s){left}[e:ROAD*0..$max]{right}(t) COST e.cost RETURN p AS route");
    let mut args = GqlParameters::new().with_uint64("max", 3).unwrap();
    if let Some(k) = count { args = args.with_uint64("k", k).unwrap(); }
    PreparedGraphCheapestPathText::prepare(&input, resolver).unwrap().bind(source, target, &args).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for id in [START, VId(1), END, VId(99)] { batch.create_vertex(id, vec![], vec![]); }
    for (id, source, target, weight) in [(10, START, END, 9), (20, START, VId(1), 4),
        (21, START, VId(1), 4), (30, VId(1), END, -2), (40, VId(1), VId(1), -1), (50, VId(1), START, 0)] {
        batch.add_edge(EId(id), source, target, vec![(W, CanonicalScalar::Int(weight))]);
    }
    db.write(cx, batch).await.unwrap()
}

#[test]
fn weighted_text_has_native_accounting_across_live_history_pins_overlays_and_recovery() {
    let ((), report) = run_async_under_lab(0xc0a5_2011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut cases = Vec::new();
        for (name, mode) in MODES {
            for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
                let (source, target) = if direction == GlaDirection::Reverse { (END, START) } else { (START, END) };
                for count in [None, Some(7)] {
                    let req = request(name, direction, count, source, target);
                    let typed = PreparedGraphCheapestPath::new(source, target, R, direction, W,
                        GraphWalkBounds::new(0, 3).unwrap()).unwrap().with_mode(mode);
                    assert_eq!(req.query().canonical_bytes(), typed.canonical_bytes());
                    let baseline = match count {
                        Some(k) => db.execute_graph_cheapest_paths_governed(&cx, &typed, k, policy()),
                        None => db.execute_graph_cheapest_path_governed(&cx, &typed, policy()),
                    }.unwrap();
                    assert!(!baseline.value.is_empty());
                    for result in [
                        db.execute_graph_cheapest_path_text_governed(&cx, &req, policy()).unwrap(),
                        db.execute_graph_cheapest_path_text_governed_at(&cx, &req, basis, policy()).unwrap(),
                        pinned.execute_graph_cheapest_path_text_governed(&cx, &req, policy()).unwrap(),
                        pinned.execute_graph_cheapest_path_text_governed_at(&cx, &req, basis, policy()).unwrap(),
                    ] { assert_eq!(result, baseline); }
                    let expected_txn = match count {
                        Some(k) => txn.execute_graph_cheapest_paths_governed(&db, &cx, &typed, k, policy()),
                        None => txn.execute_graph_cheapest_path_governed(&db, &cx, &typed, policy()),
                    }.unwrap();
                    assert_eq!(txn.execute_graph_cheapest_path_text_governed(&db, &cx, &req, policy()).unwrap().value, expected_txn.value);
                    assert_eq!(expected_txn.value, baseline.value);
                    cases.push((req, baseline.value));
                }
            }
        }
        let mut stage = WriteBatch::new(R);
        stage.set_edge_property(EId(10), W, Some(CanonicalScalar::Int(-12)));
        stage.delete_edge(EId(20));
        stage.ensure_edge_by_triple(EId(888), START, VId(1), vec![]);
        stage.add_edge(EId(8), START, START, vec![(W, CanonicalScalar::Int(-3))]);
        txn.write(&mut db, stage).unwrap();
        let mut staged = Vec::new();
        for (req, old) in &cases {
            let result = txn.execute_graph_cheapest_path_text_governed(&db, &cx, req, policy()).unwrap();
            let typed = match req.ranked_count() {
                Some(k) => txn.execute_graph_cheapest_paths_governed(&db, &cx, req.query(), k, policy()),
                None => txn.execute_graph_cheapest_path_governed(&db, &cx, req.query(), policy()),
            }.unwrap();
            assert_eq!(result.value, typed.value);
            assert!(result.value.iter().all(|row| row.path().edges().all(|eid| eid != EId(888))));
            assert_eq!(&db.execute_graph_cheapest_path_text_governed(&cx, req, policy()).unwrap().value, old);
            assert!(result.value[0].cost() <= -12);
            staged.push(result.value);
        }
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        for ((req, old), new) in cases.iter().zip(&staged) {
            assert_eq!(&db.execute_graph_cheapest_path_text_governed(&cx, req, policy()).unwrap().value, new);
            assert_eq!(&db.execute_graph_cheapest_path_text_governed_at(&cx, req, basis, policy()).unwrap().value, old);
            assert_eq!(&pinned.execute_graph_cheapest_path_text_governed(&cx, req, policy()).unwrap().value, old);
        }
        let mut cascade = WriteBatch::new(R); cascade.delete_vertex(VId(1));
        db.write(&commit, cascade).await.unwrap();
        for (req, old) in &cases {
            let result = db.execute_graph_cheapest_path_text_governed(&cx, req, policy()).unwrap();
            assert!(result.value.iter().all(|row| row.path().steps().iter().all(|step| step.1 != VId(1))));
            assert_eq!(&pinned.execute_graph_cheapest_path_text_governed(&cx, req, policy()).unwrap().value, old);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn weighted_text_keeps_conflict_dependencies_for_success_zero_empty_and_refusals() {
    let ((), report) = run_async_under_lab(0xc0a5_2012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txcx = contexts.txn();
        for (mode, _) in MODES {
            for outcome in 0..6 {
                for change in 0..3 {
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap(); seed(&mut db, &commit).await;
                    if outcome == 5 {
                        let mut invalid = WriteBatch::new(R); invalid.set_edge_property(EId(10), W, None);
                        db.write(&commit, invalid).await.unwrap();
                    }
                    let mut txn = db.begin(&txcx).unwrap();
                    let mut stage = WriteBatch::new(R); stage.create_vertex(VId(777), vec![], vec![]);
                    txn.write(&mut db, stage).unwrap();
                    let count = if outcome == 0 { None } else { Some(if outcome == 2 { 0 } else { 3 }) };
                    let req = request(mode, GlaDirection::Forward, count, START, if outcome == 3 { VId(99) } else { END });
                    let allowance = if outcome == 4 { GqlQueryPolicy::new(1000, 0, 5_000_000, 1_000_000) } else { policy() };
                    let result = txn.execute_graph_cheapest_path_text_governed(&db, &cx, &req, allowance);
                    match outcome {
                        0 | 1 => assert!(!result.unwrap().value.is_empty()),
                        2 | 3 => assert!(result.unwrap().value.is_empty()),
                        4 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                        _ => assert!(matches!(result, Err(GqlQueryError::Source(GraphCheapestPathError::Cost(GraphPathCostError::MissingWeight))))),
                    }
                    let mut winner = WriteBatch::new(R);
                    match change {
                        0 => { winner.set_edge_property(EId(10), W, Some(CanonicalScalar::Int(-30))); }
                        1 => { winner.add_edge(EId(60), START, END, vec![(W, CanonicalScalar::Int(-30))]); }
                        _ => { winner.delete_vertex(VId(1)); }
                    }
                    db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
                    // No second graph read is permitted to repair lost query observations.
                    assert!(matches!(txn.commit(&mut db, &commit).await,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(777)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_resource_limits_and_source_error_precedence_survive_text_dispatch() {
    let ((), report) = run_async_under_lab(0xc0a5_2013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let pinned = db.read_session().unwrap();
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txcx).unwrap();
        for (mode, _) in MODES {
            for count in [None, Some(3), Some(0)] {
                let req = request(mode, GlaDirection::Forward, count, START, END);
                let result = db.execute_graph_cheapest_path_text_governed(&cx, &req, policy()).unwrap();
                let exact = GqlQueryPolicy::new(result.rows.snapshot_records, result.rows.result_rows,
                    result.evaluator.work_units, result.evaluator.scratch_entries);
                assert_eq!(db.execute_graph_cheapest_path_text_governed(&cx, &req, exact).unwrap(), result);
                for bad in [
                    GqlQueryPolicy::new(result.rows.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
                    GqlQueryPolicy::new(1000, 1000, result.evaluator.work_units - 1, u64::MAX),
                    GqlQueryPolicy::new(1000, 1000, u64::MAX, result.evaluator.scratch_entries - 1),
                ] { assert!(db.execute_graph_cheapest_path_text_governed(&cx, &req, bad).is_err()); }
                if result.rows.result_rows > 0 {
                    assert!(matches!(db.execute_graph_cheapest_path_text_governed(&cx, &req,
                        GqlQueryPolicy::new(1000, result.rows.result_rows - 1, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
                }
                assert_eq!(db.execute_graph_cheapest_path_text_governed(&cx, &req, exact).unwrap(), result);
                for future in [
                    db.execute_graph_cheapest_path_text_governed_at(&cx, &req, CommitSeq(basis.0 + 1), GqlQueryPolicy::new(0, 0, 0, 0)),
                    pinned.execute_graph_cheapest_path_text_governed_at(&cx, &req, CommitSeq(basis.0 + 1), GqlQueryPolicy::new(0, 0, 0, 0)),
                ] { assert!(matches!(future, Err(GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(_)))))); }
                assert!(matches!(txn.execute_graph_cheapest_path_text_governed(&foreign, &cx, &req,
                    GqlQueryPolicy::new(0, 0, 0, 0)), Err(GqlQueryError::Source(GraphCheapestPathError::Source(_)))));
            }
        }
        let mut invalid = WriteBatch::new(R); invalid.set_edge_property(EId(10), W, None);
        db.write(&commit, invalid).await.unwrap();
        for (mode, _) in MODES {
            let req = request(mode, GlaDirection::Forward, Some(0), START, END);
            assert!(matches!(db.execute_graph_cheapest_path_text_governed(&cx, &req, policy()),
                Err(GqlQueryError::Source(GraphCheapestPathError::Cost(GraphPathCostError::MissingWeight)))));
            assert!(pinned.execute_graph_cheapest_path_text_governed(&cx, &req, policy()).unwrap().value.is_empty());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
