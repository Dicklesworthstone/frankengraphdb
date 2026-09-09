//! Canonical borrowed transaction admission, independently checked against
//! ordinary point/bulk row reads rather than another GLA query result.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, RelationBind, WriteBatch, WriteError,
    WriteMismatchPolicy, WriteTxn, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GlaExecutionLimits, GlaLimitDimension, GqlBudgetDimension,
    GqlEvidenceLimits, GqlExecutionBudget, GqlQueryError, GqlQueryPolicy, PreparedGqlQuery};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, QueryCx, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(7);
const N: PropertyKeyId = PropertyKeyId(5);
const PAYLOAD: PropertyKeyId = PropertyKeyId(9);
const HIGH: VId = VId((1_u128 << 100) + 7);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xb1; 32], DatabaseSecurityNamespaceId([0xb2; 32]), [0xb3; 32])
}

fn names() -> RelationBind {
    RelationBind::new().with_relation("R", R).with_relation("S", S)
        .with_label("L", L).with_property("n", N)
}

fn query(text: &str) -> PreparedGqlQuery {
    PreparedGqlQuery::prepare(text, &names()).unwrap()
}

fn generous() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 1_000_000)
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, n) in [(VId(1), 1), (VId(2), 2), (VId(3), 3), (VId(4), 4), (VId(5), 5), (HIGH, 9)] {
        batch.create_vertex(id, if id == VId(4) { vec![] } else { vec![L] },
            vec![(N, CanonicalScalar::Int(n))]);
    }
    for (eid, src, dst) in [(10, VId(1), VId(2)), (11, VId(2), VId(3)),
        (12, VId(1), VId(2)), (13, VId(3), VId(3)), (14, VId(5), VId(1)), (15, VId(2), HIGH)]
    {
        batch.add_edge(EId(eid), src, dst, vec![]);
    }
    db.write(cx, batch).await.unwrap();
    let mut other = WriteBatch::new(S);
    other.add_edge(EId(20), VId(2), VId(1), vec![]);
    other.add_edge(EId(21), VId(3), VId(4), vec![]);
    db.write(cx, other).await.unwrap();
    db
}

fn queries() -> Vec<PreparedGqlQuery> {
    [
        "MATCH (a:L) WHERE a.n>=3 RETURN a",
        "MATCH (a)-[:R]->(b) WHERE b.n>=3 RETURN b",
        "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN c",
        "MATCH (a)-[:R]->(b)-[:S]->(a) RETURN a",
        "MATCH (a)-[:R]->(a) RETURN a",
        "MATCH (a)-[:R]-(b) RETURN a",
        "MATCH (a)-[:R]->(b) WHERE b.n>=3 RETURN b SKIP 1 LIMIT 2",
    ].into_iter().map(query).collect()
}

fn oracle(txn: &WriteTxn, db: &Database<MemVfs>) -> Vec<(Vec<VId>, u64)> {
    let vertices: BTreeMap<_, _> = txn.vertices(db).unwrap().into_iter().map(|row| (row.vid, row)).collect();
    let edges = txn.edges(db).unwrap();
    let qualifies = |vid: VId| vertices.get(&vid).is_some_and(|row| row.props.iter().any(|(key, scalar)| {
        *key == N && matches!(scalar, CanonicalScalar::Int(n) if *n >= 3)
    }));
    let nodes = vertices.values().filter(|row| row.labels.contains(&L) && qualifies(row.vid))
        .map(|row| row.vid).collect();
    let destinations: BTreeSet<_> = edges.iter().filter(|row| row.entry.relation == R && qualifies(row.entry.dst))
        .map(|row| row.entry.dst).collect();
    let mut two = BTreeSet::new();
    let mut closed = BTreeSet::new();
    for a in edges.iter().filter(|row| row.entry.relation == R) {
        for b in &edges {
            if a.entry.dst == b.entry.src {
                if b.entry.relation == R { two.insert(b.entry.dst); }
                if b.entry.relation == S && a.entry.src == b.entry.dst { closed.insert(a.entry.src); }
            }
        }
    }
    let loops: BTreeSet<_> = edges.iter().filter(|row| row.entry.relation == R && row.entry.src == row.entry.dst)
        .map(|row| row.entry.src).collect();
    let ends: BTreeSet<_> = edges.iter().filter(|row| row.entry.relation == R)
        .flat_map(|row| [row.entry.src, row.entry.dst]).collect();
    let page = destinations.iter().copied().skip(1).take(2).collect();
    let count = edges.len() as u64;
    vec![(nodes, vertices.len() as u64), (destinations.into_iter().collect(), count),
        (two.into_iter().collect(), count), (closed.into_iter().collect(), count),
        (loops.into_iter().collect(), count), (ends.into_iter().collect(), count), (page, count)]
}

fn check_overlay(txn: &WriteTxn, db: &Database<MemVfs>, cx: &QueryCx) -> Vec<(Vec<VId>, u64)> {
    let expected = oracle(txn, db);
    for (query, (rows, records)) in queries().iter().zip(&expected) {
        assert_eq!(txn.execute_prepared_query(db, query).unwrap(), *rows);
        let budgeted = txn.execute_prepared_query_budgeted(db, query,
            GqlExecutionBudget::new(*records, rows.len() as u64)).unwrap();
        assert_eq!(budgeted.value, *rows);
        assert_eq!(budgeted.stats.snapshot_records, *records);
        assert_eq!(txn.execute_prepared_query_limited(db, query,
            GlaExecutionLimits::new(1_000_000, 1_000_000)).unwrap().value, *rows);
        let run = txn.execute_prepared_query_governed(db, cx, query, generous()).unwrap();
        assert_eq!(run.value, *rows);
        assert_eq!(run.rows.snapshot_records, *records);
        assert_eq!(run.rows.result_rows, rows.len() as u64);
        let artifact = txn.execute_prepared_query_overlay_artifact(db, query).unwrap();
        assert_eq!(artifact.rows(), rows.as_slice());
        assert_eq!(txn.audit_prepared_query_overlay_artifact_governed(db, cx, query,
            &artifact.to_bytes(), GqlEvidenceLimits::DEFAULT_UNTRUSTED, generous()).unwrap().rows(), rows.as_slice());
    }
    expected
}

#[test]
fn canonical_overlays_agree_with_independent_rows_then_with_committed_history() {
    let ((), report) = run_async_under_lab(0xc0a3_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        let before = check_overlay(&txn, &db, &cx);
        let mut stage = WriteBatch::new(R);
        stage.ensure_edge_by_triple(EId(99), VId(1), VId(2), vec![(N, CanonicalScalar::Int(111))]);
        stage.delete_edge(EId(11));
        stage.set_vertex_label(VId(4), L, true);
        stage.set_vertex_label(VId(3), L, false);
        stage.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(10)));
        stage.set_vertex_property(VId(1), N, None);
        stage.create_vertex(VId(9), vec![L], vec![(N, CanonicalScalar::Int(12)),
            (PAYLOAD, CanonicalScalar::bytes(vec![0x41; 8_000]).unwrap())]);
        stage.add_edge(EId(30), VId(1), VId(9), vec![]);
        stage.add_edge(EId(31), VId(9), VId(3), vec![]);
        stage.compare_and_set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(3)),
            CanonicalScalar::Int(7), WriteMismatchPolicy::NoOp);
        stage.compare_and_set_vertex_property(VId(4), N, Some(CanonicalScalar::Int(99)),
            CanonicalScalar::Int(8), WriteMismatchPolicy::NoOp);
        stage.create_vertex(VId(88), vec![L], vec![]);
        stage.add_edge(EId(32), VId(9), VId(88), vec![]);
        stage.delete_vertex(VId(88));
        stage.delete_vertex(VId(5));
        txn.write(&mut db, stage).unwrap();
        let mut later = WriteBatch::new(R);
        later.set_vertex_property(VId(9), N, Some(CanonicalScalar::Int(14)));
        later.set_edge_property(EId(10), PAYLOAD, Some(CanonicalScalar::bytes(vec![0x42; 8_000]).unwrap()));
        txn.write(&mut db, later).unwrap();
        assert!(txn.edge(&db, EId(99)).unwrap().is_none());
        assert!(txn.vertex(&db, VId(88)).unwrap().is_none());
        let after = check_overlay(&txn, &db, &cx);
        assert_eq!(after[0], (vec![VId(2), VId(4), VId(9), HIGH], 6));
        assert_eq!(after[1], (vec![VId(2), VId(3), VId(9), HIGH], 8));
        assert_eq!(after[2].0, vec![VId(3), HIGH]);
        assert_eq!(after[3].0, vec![VId(1)]);
        assert_eq!(db.frontier().unwrap(), basis);
        txn.commit(&mut db, &commit).await.unwrap();
        for ((query, (rows, _)), (old, _)) in queries().iter().zip(&after).zip(&before) {
            assert_eq!(db.execute_prepared_query_governed(&cx, query, generous()).unwrap().value, *rows);
            assert_eq!(db.execute_prepared_query_at(query, basis).unwrap(), *old);
            assert_eq!(pinned.execute_prepared_query(query).unwrap(), *old);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_work_and_scratch_prefix_uses_the_original_shared_allowance() {
    let ((), report) = run_async_under_lab(0xc0a3_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(10)));
        txn.write(&mut db, stage).unwrap();
        let query = query("MATCH (a)-[:R]->(b) WHERE b.n>=3 RETURN b");
        let measured = txn.execute_prepared_query_governed(&db, &cx, &query, generous()).unwrap();
        let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, measured.rows.result_rows,
            measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(txn.execute_prepared_query_governed(&db, &cx, &query, exact).unwrap(), measured);
        for limit in 0..measured.evaluator.work_units {
            let policy = GqlQueryPolicy::new(100, 100, limit, 1_000_000);
            assert!(matches!(txn.execute_prepared_query_governed(&db, &cx, &query, policy),
                Err(GqlQueryError::Evaluator(e)) if e.dimension == GlaLimitDimension::WorkUnits
                    && e.limit == limit && e.observed == u128::from(limit) + 1));
        }
        for limit in 0..measured.evaluator.scratch_entries {
            let policy = GqlQueryPolicy::new(100, 100, 1_000_000, limit);
            assert!(matches!(txn.execute_prepared_query_governed(&db, &cx, &query, policy),
                Err(GqlQueryError::Evaluator(e)) if e.dimension == GlaLimitDimension::ScratchEntries
                    && e.limit == limit && e.observed == u128::from(limit) + 1));
        }
        assert!(matches!(txn.execute_prepared_query_governed(&db, &cx, &query,
            GqlQueryPolicy::new(1, 100, 1_000_000, 1_000_000)),
            Err(GqlQueryError::Rows(e)) if e.dimension == GqlBudgetDimension::SnapshotRecords
                && e.limit == 1 && e.observed == 2));
        assert_eq!(txn.execute_prepared_query_governed(&db, &cx, &query, exact).unwrap(), measured);
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn deleted_basis_rows_do_not_consume_final_overlay_record_budget() {
    let ((), report) = run_async_under_lab(0xc0a3_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(3), VId(4), VId(5), HIGH] { stage.delete_vertex(vid); }
        txn.write(&mut db, stage).unwrap();
        for query in queries() {
            let run = txn.execute_prepared_query_governed(&db, &cx, &query,
                GqlQueryPolicy::new(0, 0, 1_000_000, 1_000_000)).unwrap();
            assert!(run.value.is_empty());
            assert_eq!(run.rows.snapshot_records, 0);
            assert!(run.evaluator.work_units > 0);
            assert!(txn.execute_prepared_query_budgeted(&db, &query,
                GqlExecutionBudget::new(0, 0)).unwrap().value.is_empty());
        }
        txn.abort();
        assert_eq!(db.vertices().unwrap().len(), 6);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_payload_size_does_not_turn_borrowed_query_admission_into_scalar_copying() {
    let ((), report) = run_async_under_lab(0xc0a3_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let mut previous = None;
        for bytes in [1, 8_000] {
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(10)));
            stage.set_vertex_property(VId(2), PAYLOAD,
                Some(CanonicalScalar::bytes(vec![0x41; bytes]).unwrap()));
            stage.set_edge_property(EId(10), PAYLOAD,
                Some(CanonicalScalar::bytes(vec![0x42; bytes]).unwrap()));
            txn.write(&mut db, stage).unwrap();
            let query = query("MATCH (a)-[:R]->(b) WHERE b.n>=3 RETURN b");
            let run = txn.execute_prepared_query_governed(&db, &cx, &query, generous()).unwrap();
            if let Some(old) = previous { assert_eq!(run, old); }
            previous = Some(run);
            txn.abort();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refused_overlay_reads_keep_dependencies_but_allow_unrelated_vertex_commits() {
    let ((), report) = run_async_under_lab(0xc0a3_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        for result_limit in [false, true] {
            for winner_kind in 0..4 {
                let mut db = seeded(&commit).await;
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R);
                stage.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let query = query("MATCH (a)-[:R]->(b) RETURN b");
                let policy = if result_limit { GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000) }
                    else { GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000) };
                assert!(matches!(txn.execute_prepared_query_governed(&db, &cx, &query, policy),
                    Err(GqlQueryError::Rows(_))));
                let mut winner = WriteBatch::new(R);
                match winner_kind {
                    0 => { winner.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(20))); }
                    1 => { winner.delete_edge(EId(10)); }
                    2 => {
                        winner.create_vertex(VId(77), vec![], vec![]);
                        winner.create_vertex(VId(78), vec![], vec![]);
                        winner.add_edge(EId(80), VId(77), VId(78), vec![]);
                    }
                    _ => { winner.create_vertex(VId(77), vec![], vec![]); }
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if winner_kind == 3 {
                    result.expect("edge-query refusal is not a global vertex-insertion fence");
                    assert!(db.vertex(VId(99)).unwrap().is_some());
                } else {
                    assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(99)).unwrap().is_none());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn multi_relation_source_keeps_coordinates_and_failed_staging_leaves_it_unchanged() {
    let ((), report) = run_async_under_lab(0xc0a3_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut first = WriteBatch::new(R);
        first.add_edge(EId(100), VId(4), VId(5), vec![]);
        let mut second = WriteBatch::new(S);
        second.add_edge(EId(101), VId(5), VId(4), vec![]);
        txn.write_atomic(&mut db, vec![first, second]).unwrap();
        let query = query("MATCH (a)-[:R]->(b)-[:S]->(a) RETURN a");
        let before = txn.execute_prepared_query_governed(&db, &cx, &query, generous()).unwrap();
        assert_eq!(before.value, vec![VId(1), VId(4)]);
        assert_eq!(before.rows.snapshot_records, 10);
        let mut overlap = WriteBatch::new(R);
        overlap.set_vertex_property(VId(4), N, Some(CanonicalScalar::Int(99)));
        assert!(matches!(txn.write(&mut db, overlap), Err(WriteTxnError::AtomicRelationConflict { .. })));
        assert_eq!(txn.execute_prepared_query_governed(&db, &cx, &query, generous()).unwrap(), before);
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(db.execute_prepared_query(&query).unwrap(), vec![VId(1), VId(4)]);
        assert_eq!(pinned.execute_prepared_query(&query).unwrap(), vec![VId(1)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn interleaved_live_history_does_not_rebase_the_borrowed_overlay() {
    let ((), report) = run_async_under_lab(0xc0a3_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(10)));
        stage.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, stage).unwrap();
        let query = query("MATCH (a)-[:R]->(b) WHERE b.n>=3 RETURN b");
        let expected = vec![VId(2), VId(3), HIGH];
        assert_eq!(txn.execute_prepared_query(&db, &query).unwrap(), expected);
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(0)));
        winner.delete_edge(EId(15));
        winner.create_vertex(VId(6), vec![L], vec![(N, CanonicalScalar::Int(99))]);
        winner.add_edge(EId(80), VId(1), VId(6), vec![]);
        db.write(&commit, winner).await.unwrap();
        assert_eq!(db.execute_prepared_query(&query).unwrap(), vec![VId(6)]);
        assert_eq!(txn.execute_prepared_query_governed(&db, &cx, &query, generous()).unwrap().value, expected);
        assert_eq!(txn.execute_prepared_query_budgeted(&db, &query,
            GqlExecutionBudget::new(8, 3)).unwrap().value, expected);
        let frontier = db.frontier().unwrap();
        assert!(matches!(txn.commit(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(99)).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
