//! **MATCH dest phantom vs the reference oracle**
//! (`fgdb-w4-g1-txn-core-qpmg.21`).
//!
//! The phantom this suite pins: a transaction whose MATCH observed a
//! destination vertex must abort when a first committer lands a NEW
//! qualifying edge INTO that destination — the fresh edge's own id can never
//! be in any reader's read-set, so the conflict is visible only through the
//! edge's adjacency endpoints. The control law keeps the abort honest: a
//! first committer that only creates an isolated vertex — touching nothing
//! the reader observed and changing no observed adjacency — must NOT abort
//! the reader, and both effects land. `fgdb-reference` replay of the
//! independent durable stream judges both outcomes.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch, WriteTxnError};
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
        "fgdb-writetxn-gql-dest-phantom-{}-{name}",
        std::process::id()
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// The durable seed: `1-[:R, EId(10)]->2`, so the reader's MATCH observes
/// destination `VId(2)`.
fn seed_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch
}

#[test]
fn new_edge_into_observed_dest_aborts_the_reader_and_replays_only_the_writer() {
    let dir = scratch("edge-into-observed-dest");
    let ((), report) = run_async_under_lab(0xa6_01, |root| async move {
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

        let mut reader = database.begin(&txn_cx).expect("begin MATCH reader");
        let mut writer = database.begin(&txn_cx).expect("begin edge writer");
        assert_eq!(
            reader
                .execute_gql(&database, PINNED, &bind_r())
                .expect("transactional MATCH observes the dest"),
            vec![VId(2)],
            "the reader's MATCH observed destination VId(2)"
        );
        let mut disjoint = WriteBatch::new(R);
        disjoint.create_vertex(VId(4), vec![], vec![]);
        reader
            .write(&mut database, disjoint)
            .expect("reader stages disjoint vertex");
        let mut incoming = WriteBatch::new(R);
        incoming.create_vertex(VId(9), vec![], vec![]);
        incoming.add_edge(EId(20), VId(9), VId(2), vec![]);
        writer
            .write(&mut database, incoming)
            .expect("writer stages the new edge into the observed dest");
        writer
            .commit(&mut database, &commit_cx)
            .await
            .expect("edge writer commits first");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "the dest phantom must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "the dest phantom must name READ-01: {rendered}"
        );
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "seed and edge writer only");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert!(
            graph.iter_edges().any(|(eid, edge)| eid == EId(20)
                && edge.src == VId(9)
                && edge.relation == R
                && edge.dst == VId(2)),
            "B's new edge into the observed dest is durable"
        );
        assert!(
            graph.vertex(VId(4)).is_none(),
            "READ-01 abort leaves none of A's disjoint write"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn isolated_vertex_commit_does_not_abort_the_reader_and_both_land() {
    let dir = scratch("isolate-control");
    let ((), report) = run_async_under_lab(0xa6_02, |root| async move {
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

        let mut reader = database.begin(&txn_cx).expect("begin MATCH reader");
        let mut writer = database.begin(&txn_cx).expect("begin isolate writer");
        assert_eq!(
            reader
                .execute_gql(&database, PINNED, &bind_r())
                .expect("transactional MATCH observes the dest"),
            vec![VId(2)],
            "the reader's MATCH observed destination VId(2)"
        );
        let mut disjoint = WriteBatch::new(R);
        disjoint.create_vertex(VId(4), vec![], vec![]);
        reader
            .write(&mut database, disjoint)
            .expect("reader stages disjoint vertex");
        let mut isolate = WriteBatch::new(R);
        isolate.create_vertex(VId(9), vec![], vec![]);
        writer
            .write(&mut database, isolate)
            .expect("writer stages the isolated vertex");
        writer
            .commit(&mut database, &commit_cx)
            .await
            .expect("isolate writer commits first");

        let committed = reader
            .commit(&mut database, &commit_cx)
            .await
            .expect("an isolate touching nothing observed must not abort the reader");
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(
            coordinator.chain().len(),
            committed.0 as usize,
            "seed, isolate writer, and reader all landed"
        );
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert!(
            graph.vertex(VId(9)).is_some(),
            "the isolate writer's vertex is durable"
        );
        assert!(
            graph.vertex(VId(4)).is_some(),
            "the reader's disjoint write is durable too"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
