//! **`MATCH … RETURN a` source vids vs the reference oracle**
//! (`fgdb-gql-return-src-g7g5`).
//!
//! The source-projection twin of `gql_exec_oracle.rs`: `RETURN a` must answer
//! the unique SOURCE vids of the matched relation's edges as the reference
//! derives them from its own edge table, while `RETURN b` keeps answering the
//! destinations — and the three product faces (live, as-of, and an empty
//! transaction overlay) must agree on the source projection, because an
//! overlay with nothing staged IS the pinned basis. The epoch law pins the
//! projection to the sequence: after a later edge, `RETURN a` at the first
//! sequence still answers the first epoch's sources while the live answer
//! includes the new one.

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
const RETURN_A: &str = "MATCH (a)-[:R]->(b) RETURN a";
const RETURN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
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
        "fgdb-gql-return-src-oracle-{}-{name}",
        std::process::id()
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// Two sources into ONE shared destination: `1-[:R]->2` and `3-[:R]->2`, so
/// the two projections genuinely differ — `RETURN b` collapses to `[2]`
/// while `RETURN a` answers `[1, 3]`.
fn seed_shared_dest() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.create_vertex(VId(3), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch.add_edge(EId(11), VId(3), VId(2), vec![]);
    batch
}

/// The later epoch: `9-[:R]->5`, adding a new source past the first sequence.
fn later_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(9), vec![], vec![]);
    batch.create_vertex(VId(5), vec![], vec![]);
    batch.add_edge(EId(12), VId(9), VId(5), vec![]);
    batch
}

/// Unique ascending SOURCE vids of exactly `relation`, read off the
/// reference oracle's own edge table.
fn reference_relation_sources(
    graph: &fgdb_reference::ReferenceGraph,
    relation: RelationId,
) -> Vec<VId> {
    let mut sources: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == relation)
        .map(|(_, edge)| edge.src)
        .collect();
    sources.sort_unstable();
    sources.dedup();
    sources
}

#[test]
fn return_a_answers_the_reference_sources_across_all_three_faces() {
    let dir = scratch("three-faces");
    let ((), report) = run_async_under_lab(0xa9_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_shared_dest())
            .await
            .expect("seed shared-destination edges");
        let frontier = database.frontier().expect("healthy seed frontier");

        let dest_rows = database
            .execute_gql(RETURN_B, &bind_r())
            .expect("RETURN b executes");
        assert_eq!(dest_rows, vec![VId(2)], "the shared dest collapses to [2]");

        let live_sources = database
            .execute_gql(RETURN_A, &bind_r())
            .expect("RETURN a executes live");
        assert_eq!(live_sources, vec![VId(1), VId(3)], "both sources, sorted");

        let as_of_sources = database
            .execute_gql_at(RETURN_A, &bind_r(), frontier)
            .expect("RETURN a executes at the seed frontier");
        assert_eq!(
            as_of_sources, live_sources,
            "as-of at the live frontier is the live answer"
        );

        // An overlay with nothing staged IS the pinned basis: the empty
        // transaction's MATCH must agree with the live and as-of faces.
        let transaction = database.begin(&txn_cx).expect("begin empty transaction");
        let overlay_sources = transaction
            .execute_gql(&database, RETURN_A, &bind_r())
            .expect("RETURN a executes over the empty overlay");
        assert_eq!(
            overlay_sources, live_sources,
            "the empty overlay agrees with the live and as-of faces"
        );
        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

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
            live_sources,
            reference_relation_sources(graph, R),
            "RETURN a equals the reference's unique :R sources"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn return_a_at_the_first_sequence_ignores_the_later_source() {
    let dir = scratch("epoch-pinning");
    let ((), report) = run_async_under_lab(0xa9_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_shared_dest())
            .await
            .expect("seed shared-destination edges");
        let s1 = database.frontier().expect("healthy S1 frontier");
        database
            .write(&commit_cx, later_edge())
            .await
            .expect("seed the later source");

        assert_eq!(
            database
                .execute_gql_at(RETURN_A, &bind_r(), s1)
                .expect("RETURN a executes at S1"),
            vec![VId(1), VId(3)],
            "the first epoch's sources are unchanged by the later edge"
        );
        let live_sources = database
            .execute_gql(RETURN_A, &bind_r())
            .expect("RETURN a executes live");
        assert_eq!(
            live_sources,
            vec![VId(1), VId(3), VId(9)],
            "the live answer includes the later source"
        );
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        assert_eq!(
            live_sources,
            reference_relation_sources(
                reference
                    .graph(GRAPH, BRANCH)
                    .expect("reference coordinate exists"),
                R
            ),
            "the live RETURN a equals the reference's full-stream sources"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
