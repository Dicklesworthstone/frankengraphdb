//! Public weighted streams retain one source image, purpose and cumulative allowance.
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

use fgdb_gql::{GraphCheapestPathStreamIterator, GraphCheapestPathStreamState, GraphCostPath};

type Cursor<'cx> = GraphCheapestPathStreamIterator<'cx, Box<asupersync::error::Error>>;
fn drain(cursor: &mut Cursor<'_>) -> Vec<GraphCostPath> { cursor.by_ref().map(Result::unwrap).collect() }

#[test]
fn ranked_and_text_streams_keep_one_image_through_writes_compaction_drop_and_reopen() {
    let ((), report) = run_async_under_lab(0xc0a5_3011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let view = db.read_session().unwrap();
        let mut cases = Vec::new();
        for (name, _) in MODES {
            for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
                let (source, target) = if direction == GlaDirection::Reverse { (END, START) } else { (START, END) };
                for count in [None, Some(7)] {
                    let req = request(name, direction, count, source, target);
                    let k = count.unwrap_or(1);
                    let expected = db.execute_graph_cheapest_paths_governed(&cx, req.query(), k, policy()).unwrap();
                    let mut live = db.stream_graph_cheapest_path_text_governed(&cx, &req, policy()).unwrap();
                    assert_eq!(live.snapshot_seq(), basis);
                    assert_eq!(live.row_stats().result_rows, 0);
                    let first = live.next().unwrap().unwrap();
                    assert_eq!(first, expected.value[0]);
                    let mut pinned = view.stream_graph_cheapest_paths_governed(&cx, req.query(), k, policy()).unwrap();
                    assert_eq!(drain(&mut pinned), expected.value);
                    assert_eq!(pinned.row_stats(), expected.rows);
                    assert_eq!(pinned.evaluator_stats(), expected.evaluator);
                    cases.push((req, expected, first, live));
                }
            }
        }
        // No stream borrows Database or the query definition. It is legal to
        // replace source data, compact, and destroy that opened DB lifetime.
        let mut changed = WriteBatch::new(R);
        changed.set_edge_property(EId(10), W, Some(CanonicalScalar::Int(-12)));
        changed.delete_edge(EId(20));
        db.write(&commit, changed).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        for (req, expected, first, mut opened) in cases {
            let mut old = vec![first]; old.extend(drain(&mut opened));
            assert_eq!(old, expected.value);
            assert_eq!(opened.row_stats(), expected.rows);
            assert_eq!(opened.evaluator_stats(), expected.evaluator);
            assert_eq!(opened.state(), GraphCheapestPathStreamState::Exhausted);
            for mut historical in [
                db.stream_graph_cheapest_path_text_governed_at(&cx, &req, basis, policy()).unwrap(),
                view.stream_graph_cheapest_path_text_governed_at(&cx, &req, basis, policy()).unwrap(),
                view.stream_graph_cheapest_path_text_governed(&cx, &req, policy()).unwrap(),
            ] {
                assert_eq!(historical.snapshot_seq(), basis);
                assert_eq!(drain(&mut historical), expected.value);
            }
            let mut current = db.stream_graph_cheapest_paths_governed(&cx, req.query(), req.ranked_count().unwrap_or(1), policy()).unwrap();
            assert!(current.next().unwrap().unwrap().cost() <= -12);
            current.close();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn transaction_streams_freeze_canonical_staged_costs_and_identity_aliases_at_open() {
    let ((), report) = run_async_under_lab(0xc0a5_3012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.set_edge_property(EId(10), W, Some(CanonicalScalar::Int(-12)));
        stage.delete_edge(EId(20));
        stage.ensure_edge_by_triple(EId(888), START, VId(1), vec![]);
        stage.add_edge(EId(8), START, START, vec![(W, CanonicalScalar::Int(-3))]);
        txn.write(&mut db, stage).unwrap();
        let mut cases = Vec::new();
        for (name, _) in MODES {
            for direction in [GlaDirection::Forward, GlaDirection::Reverse, GlaDirection::Undirected] {
                let (source, target) = if direction == GlaDirection::Reverse { (END, START) } else { (START, END) };
                for count in [None, Some(7)] {
                    let req = request(name, direction, count, source, target);
                    let expected = txn.execute_graph_cheapest_paths_governed(&db, &cx, req.query(), count.unwrap_or(1), policy()).unwrap().value;
                    let stream = txn.stream_graph_cheapest_path_text_governed(&db, &cx, &req, policy()).unwrap();
                    assert_eq!(stream.snapshot_seq(), basis);
                    assert_eq!(stream.row_stats().result_rows, 0);
                    assert!(expected.iter().all(|row| row.path().edges().all(|eid| eid != EId(888))));
                    cases.push((expected, stream));
                }
            }
        }
        let mut later = WriteBatch::new(R);
        later.set_edge_property(EId(10), W, Some(CanonicalScalar::Int(-99)));
        txn.write(&mut db, later).unwrap();
        txn.commit(&mut db, &commit).await.unwrap();
        for (expected, mut stream) in cases {
            assert_eq!(drain(&mut stream), expected);
            assert!(expected[0].cost() > -99);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn even_unpolled_closed_empty_zero_and_refused_streams_keep_transaction_conflicts() {
    let ((), report) = run_async_under_lab(0xc0a5_3013, |root| async move {
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
                    let req = request(mode, GlaDirection::Forward, Some(if outcome == 2 { 0 } else { 7 }),
                        START, if outcome == 3 { VId(99) } else { END });
                    let allowance = if outcome == 4 { GqlQueryPolicy::new(1000, 0, 5_000_000, 1_000_000) } else { policy() };
                    let opened = txn.stream_graph_cheapest_path_text_governed(&db, &cx, &req, allowance);
                    if outcome == 5 {
                        assert!(matches!(opened, Err(GqlQueryError::Source(GraphCheapestPathError::Cost(GraphPathCostError::MissingWeight)))));
                    } else {
                        let mut stream = opened.unwrap();
                        match outcome {
                            0 => { assert_eq!(stream.row_stats().result_rows, 0); }, // Never polled.
                            1 => { stream.next().unwrap().unwrap(); assert_eq!(stream.row_stats().result_rows, 1); },
                            2 | 3 => assert!(stream.next().is_none()),
                            _ => {
                                assert!(matches!(stream.next(), Some(Err(GqlQueryError::Rows(_)))));
                                assert_eq!(stream.state(), GraphCheapestPathStreamState::Failed);
                            }
                        }
                        stream.close();
                    }
                    let mut winner = WriteBatch::new(R);
                    match change {
                        0 => { winner.set_edge_property(EId(10), W, Some(CanonicalScalar::Int(-30))); }
                        1 => { winner.add_edge(EId(60), START, END, vec![(W, CanonicalScalar::Int(-30))]); }
                        _ => { winner.delete_vertex(VId(1)); }
                    }
                    db.write(&commit, winner).await.unwrap(); let frontier = db.frontier().unwrap();
                    // Crucially, do not issue another query to restore witnesses.
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
fn exact_full_query_budgets_never_reset_between_pulls_and_source_precedence_is_preserved() {
    let ((), report) = run_async_under_lab(0xc0a5_3014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await; let pinned = db.read_session().unwrap();
        let foreign = Database::open_memory(&commit, keys()).await.unwrap(); let txn = db.begin(&txcx).unwrap();
        for (mode, _) in MODES {
            for count in [0, 1, 3] {
                let req = request(mode, GlaDirection::Forward, Some(count), START, END);
                let mut full = db.stream_graph_cheapest_path_text_governed(&cx, &req, policy()).unwrap();
                let expected = drain(&mut full); let r = full.row_stats(); let e = full.evaluator_stats();
                let exact = GqlQueryPolicy::new(r.snapshot_records, r.result_rows, e.work_units, e.scratch_entries);
                let mut repeat = db.stream_graph_cheapest_path_text_governed(&cx, &req, exact).unwrap();
                assert_eq!(drain(&mut repeat), expected);
                assert_eq!(repeat.evaluator_stats(), e);
                let mut limits = vec![
                    GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
                    GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
                    GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1),
                ];
                if r.result_rows > 0 { limits.push(GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX)); }
                for limit in limits {
                    let mut stream = match db.stream_graph_cheapest_path_text_governed(&cx, &req, limit) {
                        Ok(stream) => stream,
                        Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)) => continue,
                        Err(error) => panic!("unexpected source error: {error:?}"),
                    };
                    let mut delivered = Vec::new();
                    loop {
                        match stream.next() {
                            Some(Ok(row)) => delivered.push(row),
                            Some(Err(GqlQueryError::Evaluator(error))) => {
                                let original = match error.dimension {
                                    fgdb_gql::GlaLimitDimension::WorkUnits => limit.evaluator.max_work_units,
                                    fgdb_gql::GlaLimitDimension::ScratchEntries => limit.evaluator.max_scratch_entries,
                                };
                                assert_eq!(error.limit, original);
                                assert_eq!(error.observed, u128::from(original) + 1);
                                break;
                            }
                            Some(Err(GqlQueryError::Rows(_))) => break,
                            other => panic!("expected terminal quota refusal: {other:?}"),
                        }
                    }
                    assert_eq!(delivered, expected[..delivered.len()]);
                    assert_eq!(stream.row_stats().result_rows, delivered.len() as u64);
                    assert_eq!(stream.state(), GraphCheapestPathStreamState::Failed);
                    assert!(stream.next().is_none());
                }
                for result in [
                    db.stream_graph_cheapest_path_text_governed_at(&cx, &req, CommitSeq(basis.0 + 1), GqlQueryPolicy::new(0, 0, 0, 0)),
                    pinned.stream_graph_cheapest_path_text_governed_at(&cx, &req, CommitSeq(basis.0 + 1), GqlQueryPolicy::new(0, 0, 0, 0)),
                ] { assert!(matches!(result, Err(GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(_)))))); }
                assert!(matches!(txn.stream_graph_cheapest_path_text_governed(&foreign, &cx, &req, GqlQueryPolicy::new(0, 0, 0, 0)),
                    Err(GqlQueryError::Source(GraphCheapestPathError::Source(WriteTxnError::WrongDatabase)))));
                let mut retry = db.stream_graph_cheapest_path_text_governed(&cx, &req, exact).unwrap();
                assert_eq!(drain(&mut retry), expected);
            }
        }
        let mut invalid = WriteBatch::new(R); invalid.set_edge_property(EId(10), W, None);
        db.write(&commit, invalid).await.unwrap();
        let req = request("WALK", GlaDirection::Forward, Some(0), START, END);
        assert!(matches!(db.stream_graph_cheapest_path_text_governed(&cx, &req, policy()),
            Err(GqlQueryError::Source(GraphCheapestPathError::Cost(GraphPathCostError::MissingWeight)))));
        assert!(pinned.stream_graph_cheapest_path_text_governed(&cx, &req, policy()).unwrap().next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn database_backpressure_can_stop_after_one_of_a_trillion_paths() {
    let ((), report) = run_async_under_lab(0xc0a5_3015, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R); batch.create_vertex(START, vec![], vec![]);
        batch.add_edge(EId(1), START, START, vec![(W, CanonicalScalar::Int(-1))]);
        batch.add_edge(EId(2), START, START, vec![(W, CanonicalScalar::Int(1))]);
        db.write(&commit, batch).await.unwrap();
        let query = PreparedGraphCheapestPath::new(START, START, R, GlaDirection::Forward, W,
            GraphWalkBounds::new(40, 40).unwrap()).unwrap();
        let small = GqlQueryPolicy::new(3, 1, 10_000, 10_000);
        let expected = db.execute_graph_cheapest_paths_governed(&cx, &query, 1, small).unwrap();
        let mut stream = db.stream_graph_cheapest_paths_governed(&cx, &query, u64::MAX, small).unwrap();
        assert_eq!(stream.row_stats().result_rows, 0);
        assert_eq!(stream.next().unwrap().unwrap(), expected.value[0]);
        assert_eq!(stream.evaluator_stats(), expected.evaluator);
        stream.close(); let before = stream.evaluator_stats();
        assert_eq!(stream.state(), GraphCheapestPathStreamState::Closed);
        assert!(stream.next().is_none());
        assert_eq!(stream.evaluator_stats(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
