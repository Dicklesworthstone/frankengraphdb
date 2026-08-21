use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch, WriteTxnError};
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
        "fgdb-writetxn-edge-overlay-{}-{name}",
        std::process::id()
    ))
}

fn seed_vertices() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch
}

fn staged_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch
}

#[test]
fn aborted_edge_overlay_is_absent_from_reference_replay() {
    let dir = scratch("abort-is-private");
    let ((), report) = run_async_under_lab(0x81_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_vertices())
            .await
            .expect("seed durable endpoints");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, staged_edge())
            .expect("stage private R edge");
        assert_eq!(
            transaction
                .neighbours(&database, VId(1), R)
                .expect("overlay neighbours read succeeds"),
            vec![VId(2)]
        );
        assert!(
            transaction
                .edge(&database, EId(10))
                .expect("overlay edge read succeeds")
                .is_some()
        );
        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(database.frontier().expect("abort leaves handle healthy"), frontier_before);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), frontier_before.0 as usize);
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("seeded reference coordinate exists");
        assert!(graph.neighbours(VId(1), R).is_empty());
        assert!(graph.edge(EId(10)).is_none(), "aborted staged edge is not durable");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_edge_overlay_is_present_in_reference_replay() {
    let dir = scratch("commit-is-durable");
    let ((), report) = run_async_under_lab(0x81_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_vertices())
            .await
            .expect("seed durable endpoints");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, staged_edge())
            .expect("stage private R edge");
        assert_eq!(
            transaction
                .neighbours(&database, VId(1), R)
                .expect("overlay neighbours read succeeds"),
            vec![VId(2)]
        );
        let committed = transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit staged edge");
        assert_eq!(committed.0, frontier_before.0 + 1);
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

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
        assert_eq!(graph.neighbours(VId(1), R), vec![VId(2)]);
        assert!(graph.edge(EId(10)).is_some(), "committed staged edge is durable");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn concurrent_edge_from_observed_source_aborts_read_01_and_replays_only_writer() {
    let dir = scratch("adjacency-read-conflict");
    let ((), report) = run_async_under_lab(0x81_03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_vertices())
            .await
            .expect("seed durable endpoints");

        let mut reader = database.begin(&txn_cx).expect("begin adjacency reader");
        let mut writer = database.begin(&txn_cx).expect("begin edge writer");
        assert!(
            reader
                .neighbours(&database, VId(1), R)
                .expect("transactional neighbours read succeeds")
                .is_empty()
        );
        let mut disjoint = WriteBatch::new(R);
        disjoint.create_vertex(VId(3), vec![], vec![]);
        reader
            .write(&mut database, disjoint)
            .expect("reader stages disjoint vertex");
        writer
            .write(&mut database, staged_edge())
            .expect("writer stages edge from observed source");
        writer
            .commit(&mut database, &commit_cx)
            .await
            .expect("edge writer commits first");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "adjacency conflict must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "adjacency abort must name READ-01: {rendered}"
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
        assert_eq!(graph.neighbours(VId(1), R), vec![VId(2)]);
        assert!(graph.edge(EId(10)).is_some(), "B's edge is durable");
        assert!(
            graph.vertex(VId(3)).is_none(),
            "READ-01 abort leaves none of A's disjoint write"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
