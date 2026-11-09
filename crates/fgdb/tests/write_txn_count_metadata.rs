//! Exact transaction counts observe the pinned graph plus canonical staged
//! effects. Metadata replaces population scans, never conflict dependencies.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch, WriteError, WriteTxn, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GlaLimitDimension, GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy,
    GraphAggregateRow, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    EmbeddedTxnState, PurposeContexts, QueryCx, VId,
};
use std::sync::Arc;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const A: LabelId = LabelId(1);
const B: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const VERTICES: &str = "MATCH (n) RETURN count(*) AS n";
const A_VERTICES: &str = "MATCH (n:A) RETURN count(*) AS n";
const B_VERTICES: &str = "MATCH (n:B) RETURN count(*) AS n";
const ALL_EDGES: &str = "MATCH (a)-[r]->(b) RETURN count(*) AS n";
const R_EDGES: &str = "MATCH (a)-[r:R]->(b) RETURN count(*) AS n";
const S_EDGES: &str = "MATCH (a)-[r:S]->(b) RETURN count(*) AS n";
const POPULATIONS: [&str; 6] = [
    VERTICES, A_VERTICES, B_VERTICES, ALL_EDGES, R_EDGES, S_EDGES,
];

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd4; 32],
        DatabaseSecurityNamespaceId([0xd5; 32]),
        [0xd6; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(A)),
        (GraphSymbolKind::Label, "B") => Some(GraphSymbol::Label(B)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 1_000_000, 100_000)
}

fn one_record() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1, 100, 1_000_000, 100_000)
}

fn count(rows: &[GraphAggregateRow]) -> u64 {
    assert_eq!(rows.len(), 1);
    rows[0].get(0).unwrap().as_count().unwrap()
}

fn native_count(result: QueryResult) -> u64 {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("a count is a read");
    };
    assert_eq!(columns, ["n"]);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 1);
    rows[0][0].as_count().unwrap()
}

/// Six vertices and six edges. Parallel edges and a loop are individual
/// occurrences. Vertex 2's cascade crosses both relation families.
async fn fixture(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut first = WriteBatch::new(R);
    for id in 1..=6 {
        first.create_vertex(
            VId(id),
            vec![if id == 6 { B } else { A }],
            vec![(P, CanonicalScalar::Int(id as i64))],
        );
    }
    for (id, src, dst) in [(1, 1, 2), (2, 1, 2), (3, 2, 2), (4, 3, 4)] {
        first.add_edge(EId(id), VId(src), VId(dst), vec![]);
    }
    db.write(cx, first).await.unwrap();
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(5), VId(2), VId(3), vec![]);
    second.add_edge(EId(6), VId(5), VId(6), vec![]);
    db.write(cx, second).await.unwrap();
    db
}

fn assert_populations(
    transaction: &WriteTxn,
    db: &Database<MemVfs>,
    cx: &QueryCx,
    expected: [u64; 6],
) {
    for (text, expected) in POPULATIONS.into_iter().zip(expected) {
        let result = transaction
            .execute_graph_aggregate_governed(db, cx, &prepare(text), policy())
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert_eq!(count(&result.value), expected, "{text}");
        assert_eq!(
            native_count(
                transaction
                    .query(db, cx, text, &GqlParameters::new(), symbols, policy())
                    .unwrap()
            ),
            expected,
            "native facade: {text}"
        );
    }
}

fn assert_read_conflict<T: core::fmt::Debug>(result: Result<T, WriteTxnError>) {
    assert!(
        matches!(
            result,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                ..
            }))
        ),
        "{result:?}"
    );
}

#[test]
fn clean_transaction_counts_use_one_record_independent_of_population() {
    let ((), report) = run_async_under_lab(0xc071_0001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        let mut work = None;
        for total in [1_u64, 32, 512] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            for id in 1..=total {
                let id = u128::from(id);
                batch.create_vertex(VId(id), vec![A], vec![]);
                batch.add_edge(EId(id), VId(id), VId(id), vec![]);
            }
            db.write(&commit, batch).await.unwrap();
            let transaction = db.begin(&txcx).unwrap();
            let mut observed = Vec::new();
            for (text, expected) in [
                (VERTICES, total),
                (A_VERTICES, total),
                (B_VERTICES, 0),
                (ALL_EDGES, total),
                (R_EDGES, total),
                (S_EDGES, 0),
                ("MATCH (n) RETURN count(n) AS n", total),
                ("MATCH (n) RETURN count(DISTINCT n) AS n", total),
                ("MATCH (n:A) RETURN count(DISTINCT n) AS n", total),
                ("MATCH (a)-[r:R]->(b) RETURN count(r) AS n", total),
                ("MATCH (a)-[r:R]->(b) RETURN count(DISTINCT r) AS n", total),
                ("MATCH (a)<-[r:R]-(b) RETURN count(r) AS n", total),
                ("MATCH (a)-[r]->(b) RETURN count(DISTINCT r) AS n", total),
            ] {
                let query = prepare(text);
                let result = transaction
                    .execute_graph_aggregate_governed(&db, &cx, &query, one_record())
                    .unwrap_or_else(|error| panic!("{total}, {text}: {error:?}"));
                assert_eq!(count(&result.value), expected, "{text}");
                assert_eq!(result.rows.snapshot_records, 1, "{text}");
                observed.push((
                    result.evaluator.work_units,
                    result.evaluator.scratch_entries,
                ));
            }
            if let Some(expected) = &work {
                assert_eq!(expected, &observed, "count work grew with {total} vertices");
            } else {
                work = Some(observed);
            }
            transaction.abort();
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn counts_stay_at_the_begin_cut_after_deletes_relabels_and_insertions() {
    let ((), report) = run_async_under_lab(0xc071_0002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        let mut db = fixture(&commit).await;
        let mut reader = db.begin(&txcx).unwrap();
        let basis = reader.basis();
        assert_populations(&reader, &db, &cx, [6, 5, 1, 6, 4, 2]);

        let mut change = db.begin(&txcx).unwrap();
        let mut first = WriteBatch::new(R);
        first.delete_vertex(VId(2));
        first.set_vertex_label(VId(1), A, false);
        first.set_vertex_label(VId(1), B, true);
        first.create_vertex(VId(7), vec![A], vec![]);
        first.add_edge(EId(7), VId(7), VId(1), vec![]);
        let mut second = WriteBatch::new(S);
        second.delete_edge(EId(6));
        change.write_ordered(&mut db, vec![first, second]).unwrap();
        change.finish(&mut db, &commit).await.unwrap();

        assert_eq!(reader.basis(), basis);
        assert_populations(&reader, &db, &cx, [6, 5, 1, 6, 4, 2]);
        let current = db.begin(&txcx).unwrap();
        assert_populations(&current, &db, &cx, [6, 4, 2, 2, 2, 0]);
        current.abort();
        let frontier = db.frontier().unwrap();
        assert_read_conflict(reader.finish(&mut db, &commit).await);
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_counts_follow_net_effects_cross_relation_cascades_and_savepoints() {
    let ((), report) = run_async_under_lab(0xc071_0003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        let mut db = fixture(&commit).await;
        let basis = db.frontier().unwrap();
        let mut transaction = db.begin(&txcx).unwrap();
        transaction.savepoint(&db, "original").unwrap();
        let mut first = WriteBatch::new(R);
        first.set_vertex_label(VId(1), A, false);
        first.set_vertex_label(VId(1), B, true);
        first.create_vertex(VId(7), vec![A], vec![]);
        first.add_edge(EId(7), VId(7), VId(1), vec![]);
        let mut second = WriteBatch::new(S);
        second.add_edge(EId(8), VId(7), VId(6), vec![]);
        transaction
            .write_ordered(&mut db, vec![first, second])
            .unwrap();
        assert_populations(&transaction, &db, &cx, [7, 5, 2, 8, 5, 3]);
        transaction.savepoint(&db, "grown").unwrap();

        // One vertex deletion retires both R and S edges. Deleting a freshly
        // created vertex erases its net birth and both incident edge births.
        let mut retire = WriteBatch::new(S);
        retire.delete_vertex(VId(2));
        retire.delete_vertex(VId(7));
        retire.set_vertex_label(VId(3), A, false);
        transaction.write(&mut db, retire).unwrap();
        let mut restore_label = WriteBatch::new(S);
        restore_label.set_vertex_label(VId(3), A, true);
        restore_label.delete_edge_if_present(EId(8));
        transaction.write(&mut db, restore_label).unwrap();
        assert_populations(&transaction, &db, &cx, [5, 3, 2, 2, 1, 1]);
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.vertices().unwrap().len(), 6);

        transaction.rollback_to_savepoint(&db, "grown").unwrap();
        assert_populations(&transaction, &db, &cx, [7, 5, 2, 8, 5, 3]);
        transaction.rollback_to_savepoint(&db, "original").unwrap();
        assert_populations(&transaction, &db, &cx, [6, 5, 1, 6, 4, 2]);
        assert!(matches!(
            transaction.finish(&mut db, &commit).await.unwrap(),
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[derive(Clone, Copy, Debug)]
enum Change {
    CreateA,
    CreateB,
    DeleteA,
    DeleteB,
    EnterA,
    LeaveA,
    EnterB,
    Property,
    CreateR,
    CreateS,
    DeleteR,
    DeleteS,
    EdgeProperty,
}

fn changed(change: Change) -> WriteBatch {
    let mut batch = WriteBatch::new(match change {
        Change::CreateS | Change::DeleteS | Change::DeleteA => S,
        _ => R,
    });
    match change {
        Change::CreateA => {
            batch.create_vertex(VId(7), vec![A], vec![]);
        }
        Change::CreateB => {
            batch.create_vertex(VId(7), vec![B], vec![]);
        }
        Change::DeleteA => {
            batch.delete_vertex(VId(2));
        }
        Change::DeleteB => {
            batch.delete_vertex(VId(6));
        }
        Change::EnterA => {
            batch.set_vertex_label(VId(6), A, true);
        }
        Change::LeaveA => {
            batch.set_vertex_label(VId(1), A, false);
        }
        Change::EnterB => {
            batch.set_vertex_label(VId(1), B, true);
        }
        Change::Property => {
            batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
        }
        Change::CreateR | Change::CreateS => {
            batch.add_edge(EId(7), VId(3), VId(4), vec![]);
        }
        Change::DeleteR => {
            batch.delete_edge(EId(1));
        }
        Change::DeleteS => {
            batch.delete_edge(EId(5));
        }
        Change::EdgeProperty => {
            batch.set_edge_property(EId(1), P, Some(CanonicalScalar::Int(99)));
        }
    }
    batch
}

#[test]
fn count_conflicts_cover_negative_changes_without_locking_unrelated_domains() {
    let ((), report) = run_async_under_lab(0xc071_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        for (text, change, conflict) in [
            (VERTICES, Change::CreateA, true),
            (VERTICES, Change::DeleteA, true),
            (VERTICES, Change::LeaveA, false),
            (VERTICES, Change::Property, false),
            (VERTICES, Change::CreateR, false),
            (VERTICES, Change::DeleteS, false),
            (A_VERTICES, Change::DeleteA, true),
            (A_VERTICES, Change::DeleteB, false),
            (A_VERTICES, Change::EnterA, true),
            (A_VERTICES, Change::LeaveA, true),
            (A_VERTICES, Change::CreateB, false),
            (A_VERTICES, Change::EnterB, false),
            (A_VERTICES, Change::Property, false),
            (R_EDGES, Change::DeleteR, true),
            (R_EDGES, Change::DeleteS, false),
            (R_EDGES, Change::DeleteA, true),
            (R_EDGES, Change::DeleteB, false),
            (R_EDGES, Change::CreateR, true),
            (R_EDGES, Change::CreateS, false),
            (R_EDGES, Change::LeaveA, false),
            (R_EDGES, Change::EdgeProperty, false),
            (ALL_EDGES, Change::DeleteS, true),
            (ALL_EDGES, Change::CreateS, true),
            (ALL_EDGES, Change::DeleteA, true),
            (ALL_EDGES, Change::CreateA, false),
        ] {
            let mut db = fixture(&commit).await;
            let mut transaction = db.begin(&txcx).unwrap();
            let result = transaction
                .execute_graph_aggregate_governed(&db, &cx, &prepare(text), one_record())
                .unwrap();
            assert_eq!(result.rows.snapshot_records, 1);
            let mut sentinel = WriteBatch::new(R);
            sentinel.create_vertex(VId(99), vec![], vec![]);
            transaction.write(&mut db, sentinel).unwrap();
            db.write(&commit, changed(change)).await.unwrap();
            let frontier = db.frontier().unwrap();
            let result = transaction.finish(&mut db, &commit).await;
            if conflict {
                assert_read_conflict(result);
                assert_eq!(db.frontier().unwrap(), frontier, "{text}, {change:?}");
                assert!(db.vertex(VId(99)).unwrap().is_none());
            } else {
                assert!(
                    matches!(result, Ok(EmbeddedTxnCompletion::WriteCommitted { .. })),
                    "unrelated change {change:?} invalidated {text}: {result:?}"
                );
                assert!(db.vertex(VId(99)).unwrap().is_some());
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refresh_advances_counts_only_after_acceptance_and_keeps_rolled_back_observations() {
    let ((), report) = run_async_under_lab(0xc071_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        let mut db = fixture(&commit).await;
        let mut transaction = db.begin(&txcx).unwrap();
        let r = prepare(R_EDGES);
        assert_eq!(
            count(
                &transaction
                    .execute_graph_aggregate_governed(&db, &cx, &r, one_record())
                    .unwrap()
                    .value
            ),
            4
        );
        db.write(&commit, changed(Change::CreateB)).await.unwrap();
        let accepted = db.frontier().unwrap();
        assert_eq!(transaction.refresh_snapshot(&db, &txcx).unwrap(), accepted);
        // A domain not read before refresh sees the newly captured generation.
        assert_populations(&transaction, &db, &cx, [7, 5, 2, 6, 4, 2]);
        transaction.savepoint(&db, "before").unwrap();
        let mut own = WriteBatch::new(R);
        own.create_vertex(VId(99), vec![A], vec![]);
        transaction.write(&mut db, own).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        db.write(&commit, changed(Change::DeleteA)).await.unwrap();
        assert_read_conflict(transaction.refresh_snapshot(&db, &txcx));
        assert_eq!(transaction.basis(), accepted);
        assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
        assert_populations(&transaction, &db, &cx, [8, 6, 2, 6, 4, 2]);
        transaction.rollback_to_savepoint(&db, "before").unwrap();
        assert_populations(&transaction, &db, &cx, [7, 5, 2, 6, 4, 2]);
        assert_read_conflict(transaction.finish(&mut db, &commit).await);
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn metadata_finalization_retains_exact_limits_late_errors_and_count_dependencies() {
    let ((), report) = run_async_under_lab(0xc071_0006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        let mut db = fixture(&commit).await;
        let transaction = db.begin(&txcx).unwrap();
        let query = prepare(R_EDGES);
        let first = transaction
            .execute_graph_aggregate_governed(&db, &cx, &query, policy())
            .unwrap();
        let complete = transaction
            .execute_graph_aggregate_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(first.value, complete.value);
        assert_eq!(first.rows, complete.rows);
        assert_eq!(first.evaluator.work_units, complete.evaluator.work_units);
        assert_eq!(
            first.evaluator.scratch_entries,
            complete.evaluator.scratch_entries + 1,
            "the first relation-domain witness owns one admitted allocation"
        );
        let exact = GqlQueryPolicy::new(
            1,
            1,
            complete.evaluator.work_units,
            complete.evaluator.scratch_entries,
        );
        assert_eq!(
            transaction
                .execute_graph_aggregate_governed(&db, &cx, &query, exact)
                .unwrap(),
            complete
        );
        for (allowance, dimension) in [
            (GqlQueryPolicy::new(0, 1, 1_000_000, 100_000), 0),
            (GqlQueryPolicy::new(1, 0, 1_000_000, 100_000), 1),
            (
                GqlQueryPolicy::new(
                    1,
                    1,
                    complete.evaluator.work_units - 1,
                    complete.evaluator.scratch_entries,
                ),
                2,
            ),
            (
                GqlQueryPolicy::new(
                    1,
                    1,
                    complete.evaluator.work_units,
                    complete.evaluator.scratch_entries - 1,
                ),
                3,
            ),
        ] {
            let error = transaction
                .execute_graph_aggregate_governed(&db, &cx, &query, allowance)
                .unwrap_err();
            match (error, dimension) {
                (GqlQueryError::Rows(error), 0) => {
                    assert_eq!(error.dimension, GqlBudgetDimension::SnapshotRecords)
                }
                (GqlQueryError::Rows(error), 1) => {
                    assert_eq!(error.dimension, GqlBudgetDimension::ResultRows)
                }
                (GqlQueryError::Evaluator(error), 2) => {
                    assert_eq!(error.dimension, GlaLimitDimension::WorkUnits)
                }
                (GqlQueryError::Evaluator(error), 3) => {
                    assert_eq!(error.dimension, GlaLimitDimension::ScratchEntries)
                }
                (error, dimension) => panic!("{dimension}: {error:?}"),
            }
            assert_eq!(transaction.state(), EmbeddedTxnState::Active);
        }
        assert_eq!(
            transaction
                .execute_graph_aggregate_governed(&db, &cx, &query, exact)
                .unwrap(),
            complete
        );
        transaction.abort();

        for text in [
            "MATCH (a)-[r:R]->(b) RETURN count(*) AS n LIMIT 0",
            "MATCH (a)-[r:R]->(b) RETURN count(*) / 0 AS n LIMIT 0",
        ] {
            let mut transaction = db.begin(&txcx).unwrap();
            let query = prepare(text);
            let result = transaction.execute_graph_aggregate_governed(&db, &cx, &query, policy());
            if text.contains("/ 0") {
                assert!(
                    matches!(result, Err(GqlQueryError::Source(_))),
                    "{result:?}"
                );
            } else {
                let result = result.unwrap();
                assert!(result.value.is_empty());
                assert_eq!(result.rows.snapshot_records, 1);
            }
            // A discarded result and a failed finalizer both retain the read.
            let mut sentinel = WriteBatch::new(R);
            sentinel.create_vertex(VId(99), vec![], vec![]);
            transaction.write(&mut db, sentinel).unwrap();
            let mut retirement = WriteBatch::new(S);
            retirement.delete_edge(if text.contains("/ 0") { EId(2) } else { EId(1) });
            db.write(&commit, retirement).await.unwrap();
            assert_read_conflict(transaction.finish(&mut db, &commit).await);
            assert!(db.vertex(VId(99)).unwrap().is_none());
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn each_metadata_query_checkpoint_is_cancellable_without_changing_the_overlay() {
    let ((), report) = run_async_under_lab(0xc071_0007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        let mut db = fixture(&commit).await;
        let mut transaction = db.begin(&txcx).unwrap();
        let mut staged = WriteBatch::new(S);
        staged.delete_vertex(VId(2));
        transaction.write(&mut db, staged).unwrap();
        let digest = transaction.staged_effect_digest().unwrap();
        let basis = transaction.basis();
        let query = prepare(R_EDGES);
        // The cold witness allocation is tested above. Measure the retained
        // witness path here so every retry has exactly the same checkpoints.
        transaction
            .execute_graph_aggregate_governed(&db, &cx, &query, policy())
            .unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let measured = cx.with_checkpoint_probe(probe.clone());
        let expected = transaction
            .execute_graph_aggregate_governed(&db, &measured, &query, policy())
            .unwrap();
        assert_eq!(count(&expected.value), 1);
        let calls = probe.calls();
        assert!(calls > 0);
        for stop in 1..=calls {
            let interrupted =
                cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(stop))));
            let result =
                transaction.execute_graph_aggregate_governed(&db, &interrupted, &query, policy());
            assert!(
                matches!(result, Err(GqlQueryError::Interrupted(_))),
                "stop={stop}: {result:?}"
            );
            assert_eq!(transaction.basis(), basis);
            assert_eq!(transaction.state(), EmbeddedTxnState::Active);
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(
                transaction
                    .execute_graph_aggregate_governed(&db, &cx, &query, policy())
                    .unwrap(),
                expected
            );
        }
        transaction.abort();
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn property_filters_and_nonuniversal_endpoint_labels_keep_the_scan_path() {
    let ((), report) = run_async_under_lab(0xc071_0008, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        let db = fixture(&commit).await;
        let transaction = db.begin(&txcx).unwrap();
        for (text, expected) in [
            ("MATCH (n) RETURN count(n.p) AS n", 6),
            ("MATCH (n) WHERE n.p > 2 RETURN count(*) AS n", 4),
            ("MATCH (a)-[:S]->(b:A) RETURN count(*) AS n", 1),
        ] {
            let query = prepare(text);
            let result = transaction
                .execute_graph_aggregate_governed(&db, &cx, &query, policy())
                .unwrap();
            assert_eq!(count(&result.value), expected, "{text}");
            assert!(result.rows.snapshot_records > 1, "{text}");
            assert!(
                transaction
                    .execute_graph_aggregate_governed(&db, &cx, &query, one_record())
                    .is_err(),
                "{text}"
            );
        }
        transaction.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_population_counts_protect_first_members_but_allow_other_domains() {
    let ((), report) = run_async_under_lab(0xc071_0009, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        for (text, change, conflict) in [
            (A_VERTICES, Change::CreateA, true),
            (A_VERTICES, Change::CreateB, false),
            (R_EDGES, Change::CreateR, true),
            (R_EDGES, Change::CreateS, false),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(S);
            for id in 1..=64 {
                seed.create_vertex(VId(id), vec![B], vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let query = prepare(text);
            let result = transaction
                .execute_graph_aggregate_governed(&db, &cx, &query, one_record())
                .unwrap();
            assert_eq!(count(&result.value), 0);
            assert_eq!(result.rows.snapshot_records, 1);
            let mut own = WriteBatch::new(R);
            own.set_vertex_property(VId(64), P, Some(CanonicalScalar::Int(1)));
            transaction.write(&mut db, own).unwrap();
            let mut winner = match change {
                Change::CreateA | Change::CreateB => {
                    let mut batch = WriteBatch::new(R);
                    batch.create_vertex(
                        VId(65),
                        vec![if matches!(change, Change::CreateA) {
                            A
                        } else {
                            B
                        }],
                        vec![],
                    );
                    batch
                }
                _ => changed(change),
            };
            // Keep the original fixture identities above separate from these
            // first memberships; the property update also checks no false
            // count dependency is invented for a nonmember's field.
            winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(2)));
            db.write(&commit, winner).await.unwrap();
            let result = transaction.finish(&mut db, &commit).await;
            if conflict {
                assert_read_conflict(result);
                assert!(db.vertex(VId(64)).unwrap().unwrap().props.is_empty());
            } else {
                assert!(
                    matches!(result, Ok(EmbeddedTxnCompletion::WriteCommitted { .. })),
                    "{text}, {change:?}: {result:?}"
                );
                assert_eq!(
                    db.vertex(VId(64)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(1).into())]
                );
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn endpoint_label_metadata_proofs_remain_transaction_dependencies() {
    let ((), report) = run_async_under_lab(0xc071_0010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        let query = prepare("MATCH (a:A)-[:R]->(b:A) RETURN count(*) AS n");
        for change in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(R);
            for id in 1..=64 {
                seed.create_vertex(VId(id), vec![A], vec![]);
                seed.add_edge(EId(id), VId(id), VId(id), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let result = transaction
                .execute_graph_aggregate_governed(
                    &db,
                    &cx,
                    &query,
                    GqlQueryPolicy::new(4, 10, 1_000_000, 100_000),
                )
                .unwrap();
            assert_eq!(count(&result.value), 64);
            assert!(result.rows.snapshot_records <= 4);
            let mut own = WriteBatch::new(R);
            own.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(8)));
            transaction.write(&mut db, own).unwrap();
            let mut winner = WriteBatch::new(S);
            match change {
                0 => {
                    winner.set_vertex_label(VId(64), A, false);
                }
                1 => {
                    winner.create_vertex(VId(65), vec![], vec![]);
                }
                _ => {
                    winner.set_vertex_label(VId(64), B, true);
                }
            }
            db.write(&commit, winner).await.unwrap();
            let result = transaction.finish(&mut db, &commit).await;
            if change < 2 {
                assert_read_conflict(result);
            } else {
                assert!(
                    matches!(result, Ok(EmbeddedTxnCompletion::WriteCommitted { .. })),
                    "{result:?}"
                );
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_rebase_policy_preserves_count_observations_before_replaying_writes() {
    let ((), report) = run_async_under_lab(0xc071_0011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        for mode in 0..4 {
            for conflict in [false, true] {
                let mut db = fixture(&commit).await;
                let mut transaction = db.begin(&txcx).unwrap();
                let mut own = WriteBatch::new(R);
                match mode {
                    0 => {
                        own.add_edge(EId(99), VId(1), VId(3), vec![]);
                    }
                    1 => {
                        own.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
                    }
                    2 => {
                        own.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
                        own.create_vertex(VId(99), vec![B], vec![]);
                    }
                    _ => {
                        own.ensure_edge_by_triple(EId(99), VId(1), VId(3), vec![]);
                    }
                }
                transaction.write(&mut db, own).unwrap();
                let text = if !conflict && mode == 2 {
                    R_EDGES
                } else if !conflict {
                    VERTICES
                } else if mode == 1 || mode == 2 {
                    A_VERTICES
                } else {
                    R_EDGES
                };
                let observed = transaction
                    .execute_graph_aggregate_governed(&db, &cx, &prepare(text), policy())
                    .unwrap();
                assert_eq!(
                    count(&observed.value),
                    if !conflict && mode == 2 {
                        4
                    } else if conflict {
                        5
                    } else {
                        6
                    }
                );
                let mut winner = WriteBatch::new(R);
                if mode == 1 || mode == 2 {
                    // Same element, independent label and property fields.
                    winner.set_vertex_label(VId(1), A, false);
                } else {
                    // The idempotent policy may remove the proposed EId(99)
                    // birth, so its counted contribution cannot be forgotten.
                    winner.add_edge(EId(7), VId(1), VId(3), vec![]);
                }
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                let result = match mode {
                    0 => {
                        transaction
                            .commit_append_only_rebased(&mut db, &commit, 1000)
                            .await
                    }
                    1 => {
                        transaction
                            .commit_disjoint_fields_rebased(&mut db, &commit, 1000)
                            .await
                    }
                    2 => {
                        transaction
                            .commit_mixed_rebased(&mut db, &commit, 1000)
                            .await
                    }
                    _ => {
                        transaction
                            .commit_idempotent_rebased(&mut db, &commit, 1000)
                            .await
                    }
                };
                if conflict {
                    assert_read_conflict(result);
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.edge(EId(99)).unwrap().is_none());
                    assert!(db.vertex(VId(99)).unwrap().is_none());
                } else {
                    assert!(result.is_ok(), "mode {mode}: {result:?}");
                    let current = db.begin(&txcx).unwrap();
                    let expected = match mode {
                        0 => [6, 5, 1, 8, 6, 2],
                        1 => [6, 4, 1, 6, 4, 2],
                        2 => [7, 4, 2, 6, 4, 2],
                        _ => [6, 5, 1, 7, 5, 2],
                    };
                    assert_populations(&current, &db, &cx, expected);
                    current.abort();
                    assert_eq!(db.edge(EId(99)).unwrap().is_some(), mode == 0);
                }
                assert!(transaction.state().is_terminal());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn idempotent_replay_cannot_change_a_counted_local_ensure_outside_the_count_domain() {
    let ((), report) = run_async_under_lab(0xc071_0012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let commit = c.commit();
        let txcx = c.txn();
        let cx = c.query();
        let query = prepare(A_VERTICES);
        for existed in [false, true] {
            let mut db = fixture(&commit).await;
            if existed {
                let mut before = WriteBatch::new(R);
                before.create_vertex(VId(7), vec![B], vec![]);
                db.write(&commit, before).await.unwrap();
            }
            let mut transaction = db.begin(&txcx).unwrap();
            let mut proposal = WriteBatch::new(R);
            proposal.ensure_vertex(VId(7), vec![A], vec![]);
            transaction.write(&mut db, proposal).unwrap();
            let observed = transaction
                .execute_graph_aggregate_governed(&db, &cx, &query, policy())
                .unwrap();
            assert_eq!(count(&observed.value), if existed { 5 } else { 6 });
            let mut winner = WriteBatch::new(S);
            if existed {
                // An old no-op must not activate its proposed A birth after
                // a B-only retirement, even though committed A never changed.
                winner.delete_vertex(VId(7));
            } else {
                // A local A birth must not become a B-only winner on replay.
                winner.create_vertex(VId(7), vec![B], vec![]);
            }
            db.write(&commit, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            assert_eq!(
                count(
                    &db.execute_graph_aggregate_governed(&cx, &query, one_record())
                        .unwrap()
                        .value
                ),
                5,
                "the committed A population did not change"
            );
            assert_read_conflict(
                transaction
                    .commit_idempotent_rebased(&mut db, &commit, 1000)
                    .await,
            );
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(transaction.state(), EmbeddedTxnState::Aborted);
            if existed {
                assert!(db.vertex(VId(7)).unwrap().is_none());
            } else {
                assert_eq!(db.vertex(VId(7)).unwrap().unwrap().labels, vec![B]);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
