//! **Certified time-travel MATCH vs the reference as-of a sequence**
//! (`fgdb-w4-g1-txn-core-qpmg.20`).
//!
//! The certificate twin of `gql_exec_at_oracle.rs`: `execute_gql_certified_at`
//! at `S1` must answer the reference's `:R` dests over the stream PREFIX
//! through `S1` AND carry a certificate naming exactly `S1` as its snapshot
//! sequence, while the live certified form answers the full-stream dests
//! under the live frontier's certificate. A cold reopen re-answers the
//! as-of question with the same rows and the same named sequence — the
//! certificate is the replayable claim, so it must not drift with sessions.

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
        "fgdb-gql-certified-at-oracle-{}-{name}",
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
fn certified_at_s1_names_s1_and_answers_the_reference_prefix() {
    let dir = scratch("live-session");
    let ((), report) = run_async_under_lab(0xa5_01, |root| async move {
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
        let live_frontier = database.frontier().expect("healthy live frontier");

        let (at_rows, at_cert) = database
            .execute_gql_certified_at(PINNED, &bind_r(), s1)
            .expect("certified MATCH executes at S1");
        assert_eq!(
            at_cert.snapshot_seq, s1,
            "the as-of certificate names exactly the asked sequence"
        );
        let (live_rows, live_cert) = database
            .execute_gql_certified(PINNED, &bind_r())
            .expect("certified MATCH executes at the frontier");
        assert_eq!(
            live_cert.snapshot_seq, live_frontier,
            "the live certificate names the live frontier"
        );
        assert_ne!(
            at_cert.snapshot_seq, live_cert.snapshot_seq,
            "the two certificates name different snapshots — the seq is load-bearing"
        );
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        let prefix = replay_through(&commit_cx, &coordinator, s1)
            .await
            .expect("stream prefix through S1 replays")
            .database;
        assert_eq!(
            at_rows,
            reference_relation_dests(
                prefix
                    .graph(GRAPH, BRANCH)
                    .expect("prefix coordinate exists"),
                R
            ),
            "certified rows at S1 equal the reference's :R dests over the prefix"
        );
        assert_eq!(at_rows, vec![VId(2)], "the first epoch answers [2]");

        let full = replay(&commit_cx, &coordinator)
            .await
            .expect("full durable stream replays")
            .database;
        assert_eq!(
            live_rows,
            reference_relation_dests(
                full.graph(GRAPH, BRANCH).expect("full coordinate exists"),
                R
            ),
            "live certified rows equal the reference's :R dests over the full stream"
        );
        assert_eq!(live_rows, vec![VId(2), VId(5)], "both epochs answer [2, 5]");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn reopened_certified_at_still_names_s1_with_the_same_rows() {
    let dir = scratch("reopen");
    let ((), report) = run_async_under_lab(0xa5_02, |root| async move {
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
        let (before_rows, before_cert) = database
            .execute_gql_certified_at(PINNED, &bind_r(), s1)
            .expect("certified MATCH executes at S1 before reopen");
        drop(database);

        let reopened = Database::open(&commit_cx, &dir, engine_keys())
            .await
            .expect("cold reopen from the durable stream");
        let (after_rows, after_cert) = reopened
            .execute_gql_certified_at(PINNED, &bind_r(), s1)
            .expect("certified MATCH executes at S1 after reopen");
        assert_eq!(
            after_rows, before_rows,
            "the as-of rows survive the reopen unchanged"
        );
        assert_eq!(
            after_cert.snapshot_seq, s1,
            "the reopened certificate still names S1"
        );
        assert_eq!(
            after_cert, before_cert,
            "the whole certificate is a function of (stream, statement, bind, seq), \
             not of the session that minted it"
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
            after_rows,
            reference_relation_dests(
                prefix
                    .graph(GRAPH, BRANCH)
                    .expect("prefix coordinate exists"),
                R
            ),
            "the reopened as-of rows still equal the reference prefix"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
