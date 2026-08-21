//! **Time-travel `execute_gql_at` dests vs the reference as-of a sequence**
//! (`fgdb-w4-g1-txn-core-qpmg.19`).
//!
//! The as-of twin of `gql_exec_oracle.rs`: the pinned MATCH at sequence `S1`
//! must equal the unique `:R` dests the reference derives from the durable
//! stream's PREFIX through `S1` (`replay_through`), while the live MATCH
//! equals the full-stream derivation — and a cold reopen answers both
//! questions identically, because each is a function of the durable stream
//! at its sequence, not of the session or of later commits.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_sim::{replay, replay_through};
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
        "fgdb-gql-exec-at-oracle-{}-{name}",
        std::process::id()
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// The first epoch: `1-[:R, EId(10)]->2`. Its commit sequence is `S1`.
fn seed_first_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch
}

/// The second epoch: `3-[:R, EId(11)]->5`, invisible at `S1`.
fn seed_second_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(3), vec![], vec![]);
    batch.create_vertex(VId(5), vec![], vec![]);
    batch.add_edge(EId(11), VId(3), VId(5), vec![]);
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
fn match_at_s1_equals_the_reference_prefix_and_live_equals_the_full_stream() {
    let dir = scratch("live-session");
    let ((), report) = run_async_under_lab(0xa4_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_first_edge())
            .await
            .expect("seed first epoch");
        let s1 = database.frontier().expect("healthy S1 frontier");
        database
            .write(&commit_cx, seed_second_edge())
            .await
            .expect("seed second epoch");

        let at_s1 = database
            .execute_gql_at(PINNED, &bind_r(), s1)
            .expect("pinned MATCH executes at S1");
        let live = database
            .execute_gql(PINNED, &bind_r())
            .expect("pinned MATCH executes at the frontier");
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "both epochs are durable");

        let prefix = replay_through(&commit_cx, &coordinator, s1)
            .await
            .expect("stream prefix through S1 replays")
            .database;
        let prefix_graph = prefix
            .graph(GRAPH, BRANCH)
            .expect("prefix coordinate exists");
        assert_eq!(
            at_s1,
            reference_relation_dests(prefix_graph, R),
            "MATCH at S1 equals the reference's :R dests over the prefix"
        );
        assert_eq!(at_s1, vec![VId(2)], "the first epoch answers [2]");

        let full = replay(&commit_cx, &coordinator)
            .await
            .expect("full durable stream replays")
            .database;
        let full_graph = full.graph(GRAPH, BRANCH).expect("full coordinate exists");
        assert_eq!(
            live,
            reference_relation_dests(full_graph, R),
            "the live MATCH equals the reference's :R dests over the full stream"
        );
        assert_eq!(live, vec![VId(2), VId(5)], "both epochs answer [2, 5]");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn reopened_database_answers_the_same_as_of_and_live_match() {
    let dir = scratch("reopen");
    let ((), report) = run_async_under_lab(0xa4_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_first_edge())
            .await
            .expect("seed first epoch");
        let s1 = database.frontier().expect("healthy S1 frontier");
        database
            .write(&commit_cx, seed_second_edge())
            .await
            .expect("seed second epoch");
        drop(database);

        let reopened = Database::open(&commit_cx, &dir, engine_keys())
            .await
            .expect("cold reopen from the durable stream");
        assert_eq!(
            reopened
                .execute_gql_at(PINNED, &bind_r(), s1)
                .expect("pinned MATCH executes at S1 after reopen"),
            vec![VId(2)],
            "the as-of answer survives the reopen unchanged"
        );
        assert_eq!(
            reopened
                .execute_gql(PINNED, &bind_r())
                .expect("pinned MATCH executes at the frontier after reopen"),
            vec![VId(2), VId(5)],
            "the live answer survives the reopen unchanged"
        );
        drop(reopened);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        let prefix = replay_through(&commit_cx, &coordinator, s1)
            .await
            .expect("stream prefix through S1 replays")
            .database;
        assert_eq!(
            reference_relation_dests(
                prefix
                    .graph(GRAPH, BRANCH)
                    .expect("prefix coordinate exists"),
                R
            ),
            vec![VId(2)],
            "the reference prefix agrees with the reopened as-of answer"
        );
        let full = replay(&commit_cx, &coordinator)
            .await
            .expect("full durable stream replays")
            .database;
        assert_eq!(
            reference_relation_dests(
                full.graph(GRAPH, BRANCH).expect("full coordinate exists"),
                R
            ),
            vec![VId(2), VId(5)],
            "the full reference agrees with the reopened live answer"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
