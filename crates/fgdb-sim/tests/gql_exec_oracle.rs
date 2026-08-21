//! **Product `execute_gql` MATCH dests vs the reference oracle**
//! (`fgdb-w4-g1-txn-core-qpmg.16`).
//!
//! The pinned statement's rows must equal the unique destination vids of the
//! matched relation's edges as `fgdb-reference` replays them from the
//! independent durable stream — with an isolated vertex and a foreign-relation
//! edge in the graph so the equality can actually fail three ways: a dest
//! invented, an isolate leaked in, or a foreign relation's dest matched.
//! Reopen must answer identically: the rows are a function of the durable
//! stream, not of the session that wrote it.

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
        "fgdb-gql-exec-oracle-{}-{name}",
        std::process::id()
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// The `:R` half of the fixture: `1-[:R]->2`, `3-[:R]->5`, and the isolated
/// `VId(9)` that no edge of any relation touches.
fn seed_r() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.create_vertex(VId(3), vec![], vec![]);
    batch.create_vertex(VId(5), vec![], vec![]);
    batch.create_vertex(VId(9), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch.add_edge(EId(11), VId(3), VId(5), vec![]);
    batch
}

/// The foreign-relation half: `3-[:OTHER]->7`, whose dest must never appear
/// in the pinned `:R` rows.
fn seed_other() -> WriteBatch {
    let mut batch = WriteBatch::new(OTHER);
    batch.create_vertex(VId(7), vec![], vec![]);
    batch.add_edge(EId(12), VId(3), VId(7), vec![]);
    batch
}

/// The expected rows, derived from the reference oracle's OWN edge table —
/// unique destination vids of exactly the matched relation, ascending. Not an
/// adjacency accessor: reading the durable edges directly is the stronger
/// statement, and it is blind to how the engine indexes them.
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
fn match_dests_equal_reference_relation_dests() {
    let dir = scratch("live-session");
    let ((), report) = run_async_under_lab(0xa1_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_r())
            .await
            .expect("seed :R half");
        database
            .write(&commit_cx, seed_other())
            .await
            .expect("seed :OTHER half");

        let rows = database
            .execute_gql(PINNED, &bind_r())
            .expect("pinned MATCH executes");
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "both seed halves are durable");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");

        let expected = reference_relation_dests(graph, R);
        assert_eq!(
            rows, expected,
            "MATCH rows must equal the reference's unique :R dests"
        );
        assert_eq!(expected, vec![VId(2), VId(5)], "the fixture answers [2, 5]");
        assert!(
            !rows.contains(&VId(9)),
            "the isolated vertex must not appear in any expansion"
        );
        assert!(
            !rows.contains(&VId(7)),
            "the :OTHER dest must not leak into the :R rows"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn reopened_database_answers_the_same_match_as_the_reference() {
    let dir = scratch("reopen");
    let ((), report) = run_async_under_lab(0xa1_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_r())
            .await
            .expect("seed :R half");
        database
            .write(&commit_cx, seed_other())
            .await
            .expect("seed :OTHER half");
        let live_rows = database
            .execute_gql(PINNED, &bind_r())
            .expect("pinned MATCH executes on the writing session");
        drop(database);

        let reopened = Database::open(&commit_cx, &dir, engine_keys())
            .await
            .expect("cold reopen from the durable stream");
        let reopened_rows = reopened
            .execute_gql(PINNED, &bind_r())
            .expect("pinned MATCH executes after reopen");
        assert_eq!(
            reopened_rows, live_rows,
            "the rows are a function of the durable stream, not the session"
        );
        drop(reopened);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            reopened_rows,
            reference_relation_dests(graph, R),
            "the reopened MATCH rows equal the reference's unique :R dests"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
