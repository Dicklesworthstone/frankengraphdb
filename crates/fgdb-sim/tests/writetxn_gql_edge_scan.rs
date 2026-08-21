//! **`WriteTxn` MATCH overlay dests vs the reference oracle**
//! (`fgdb-w4-g1-txn-core-qpmg.17`).
//!
//! The transactional twin of `gql_exec_oracle.rs`: a pinned transaction's
//! `execute_gql` sees its own staged `:R` edges before commit, an abort
//! leaves the durable stream's MATCH answer exactly at the seed (empty here),
//! and a committed overlay rides one sequence and then answers — from a cold
//! reopen — the same unique `:R` dests the reference derives from its own
//! edge table, with the isolated vertex and the foreign relation's dest
//! provably absent.

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
const OTHER: RelationId = RelationId(2);
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
        "fgdb-writetxn-gql-edge-scan-{}-{name}",
        std::process::id()
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// Edge-free `:R` seed: the endpoints-to-be plus the isolated `VId(9)` that
/// no edge of any relation ever touches.
fn seed_vertices() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.create_vertex(VId(3), vec![], vec![]);
    batch.create_vertex(VId(5), vec![], vec![]);
    batch.create_vertex(VId(9), vec![], vec![]);
    batch
}

/// The foreign-relation edge `3-[:OTHER]->7`, whose dest must never appear
/// in the pinned `:R` rows.
fn seed_other() -> WriteBatch {
    let mut batch = WriteBatch::new(OTHER);
    batch.create_vertex(VId(7), vec![], vec![]);
    batch.add_edge(EId(12), VId(3), VId(7), vec![]);
    batch
}

/// Unique ascending dests of exactly `relation`, read off the reference
/// oracle's own edge table — not an adjacency accessor, so the equality is
/// blind to how the engine indexes.
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
fn aborted_staged_edge_leaves_the_reference_match_dests_empty() {
    let dir = scratch("abort-staged-edge");
    let ((), report) = run_async_under_lab(0xa2_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_vertices())
            .await
            .expect("seed edge-free vertices");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut stage = WriteBatch::new(R);
        stage.add_edge(EId(10), VId(1), VId(2), vec![]);
        transaction
            .write(&mut database, stage)
            .expect("stage the :R edge");
        assert_eq!(
            transaction
                .execute_gql(&database, PINNED, &bind_r())
                .expect("overlay MATCH executes"),
            vec![VId(2)],
            "the staged edge is visible to the overlay MATCH before commit"
        );
        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(
            database.frontier().expect("abort leaves handle healthy"),
            frontier_before
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
        let dests = reference_relation_dests(graph, R);
        assert!(
            dests.is_empty(),
            "the aborted edge never landed, so the durable :R dest set is empty: {dests:?}"
        );
        assert!(
            graph.vertex(VId(9)).is_some() && !dests.contains(&VId(9)),
            "the isolate survives as a vertex and appears in no dest set"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_overlay_edges_reopen_to_the_reference_match_dests() {
    let dir = scratch("commit-overlay-edges");
    let ((), report) = run_async_under_lab(0xa2_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_vertices())
            .await
            .expect("seed edge-free vertices");
        database
            .write(&commit_cx, seed_other())
            .await
            .expect("seed foreign-relation edge");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut stage = WriteBatch::new(R);
        stage.add_edge(EId(10), VId(1), VId(2), vec![]);
        stage.add_edge(EId(11), VId(3), VId(5), vec![]);
        transaction
            .write(&mut database, stage)
            .expect("stage both :R edges");
        assert_eq!(
            transaction
                .execute_gql(&database, PINNED, &bind_r())
                .expect("overlay MATCH executes"),
            vec![VId(2), VId(5)],
            "both staged edges are visible to the overlay MATCH before commit"
        );
        let committed = transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit the staged edges");
        assert_eq!(committed.0, frontier_before.0 + 1, "one new sequence");
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let reopened = Database::open(&commit_cx, &dir, engine_keys())
            .await
            .expect("cold reopen from the durable stream");
        let reopened_rows = reopened
            .execute_gql(PINNED, &bind_r())
            .expect("pinned MATCH executes after reopen");
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
        let expected = reference_relation_dests(graph, R);
        assert_eq!(
            reopened_rows, expected,
            "reopened MATCH rows equal the reference's unique :R dests"
        );
        assert_eq!(expected, vec![VId(2), VId(5)], "the fixture answers [2, 5]");
        assert!(
            !reopened_rows.contains(&VId(9)),
            "the isolate appears in no expansion"
        );
        assert!(
            !reopened_rows.contains(&VId(7)),
            "the :OTHER dest must not leak into the :R rows"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
