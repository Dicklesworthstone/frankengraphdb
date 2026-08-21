//! **`WriteTxn` MATCH overlay delete-hide vs the reference oracle**
//! (`fgdb-w4-g1-txn-core-qpmg.18`).
//!
//! The deletion mirror of `writetxn_gql_edge_scan.rs`: a staged DeleteEdge
//! hides a DURABLE edge from the transaction's own MATCH before commit, and
//! an abort restores nothing because nothing was ever taken — the durable
//! stream still answers the seed. A committed DeleteVertex of the
//! destination rides one sequence and empties the MATCH answer through its
//! cascade, judged from a cold reopen against the reference's replay of the
//! independent durable stream.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const DEK: [u8; 32] = [0x3c; 32];

fn engine_keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, DEK)
}

fn oracle_keys() -> CapsuleKeys {
    CapsuleKeys::new(
        K_OID,
        NAMESPACE,
        DEK,
        CAPSULE_OBJECT_KIND,
        CapsuleProfile::balanced(),
    )
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-writetxn-gql-delete-hide-{}-{name}",
        std::process::id()
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// The durable seed every test deletes against: `1-[:R, EId(10)]->2`, so the
/// pre-txn MATCH answer is exactly `[VId(2)]`.
fn seed_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch
}

/// Unique ascending dests of exactly `relation`, read off the reference
/// oracle's own edge table.
fn reference_relation_dests(
    graph: &fgdb_reference::ReferenceGraph,
    relation: RelationId,
) -> Vec<VId> {
    let mut dests: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == relation)
        .map(|(_, edge)| edge.dst)
        .collect();
    dests.sort_unstable();
    dests.dedup();
    dests
}

#[test]
fn aborted_delete_edge_hides_only_the_overlay_and_the_seed_survives() {
    let dir = scratch("abort-delete-edge");
    let ((), report) = run_async_under_lab(0xa3_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable edge");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut delete = WriteBatch::new(R);
        delete.delete_edge(EId(10));
        transaction
            .write(&mut database, delete)
            .expect("stage deletion of the durable edge");
        assert!(
            transaction
                .execute_gql(&database, PINNED, &bind_r())
                .expect("overlay MATCH executes")
                .is_empty(),
            "the staged deletion hides the durable edge from the overlay MATCH"
        );
        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(
            database.frontier().expect("abort leaves handle healthy"),
            frontier_before,
            "the abort consumed no sequence"
        );
        assert_eq!(
            database.execute_gql(PINNED, &bind_r()).expect("live MATCH"),
            vec![VId(2)],
            "the autocommit MATCH never saw the staged hide"
        );
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 1, "only the seed is durable");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            reference_relation_dests(graph, R),
            vec![VId(2)],
            "the durable :R dest set still answers the seed after the abort"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_delete_vertex_cascade_empties_the_reopened_match() {
    let dir = scratch("commit-delete-vertex");
    let ((), report) = run_async_under_lab(0xa3_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable edge");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut delete = WriteBatch::new(R);
        delete.delete_vertex(VId(2));
        transaction
            .write(&mut database, delete)
            .expect("stage destination-vertex deletion");
        assert!(
            transaction
                .execute_gql(&database, PINNED, &bind_r())
                .expect("overlay MATCH executes")
                .is_empty(),
            "the staged vertex deletion cascades over the overlay MATCH"
        );
        let committed = transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit the staged deletion");
        assert_eq!(committed.0, frontier_before.0 + 1, "one new sequence");
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let reopened = Database::open(&commit_cx, &dir, engine_keys())
            .await
            .expect("cold reopen from the durable stream");
        assert!(
            reopened
                .execute_gql(PINNED, &bind_r())
                .expect("pinned MATCH executes after reopen")
                .is_empty(),
            "the reopened MATCH answer is empty after the cascade"
        );
        drop(reopened);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), committed.0 as usize);
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert!(
            reference_relation_dests(graph, R).is_empty(),
            "no :R dest survives the committed cascade in the reference"
        );
        assert!(
            graph.vertex(VId(2)).is_none(),
            "the deleted destination is gone from the reference"
        );
        assert!(
            graph.vertex(VId(1)).is_some(),
            "the untouched source survives the cascade"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
