use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const PROPERTY: PropertyKeyId = PropertyKeyId(7);
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
        "fgdb-writetxn-edges-overlay-{}-{name}",
        std::process::id()
    ))
}

fn seed_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for vid in [VId(1), VId(2), VId(3)] {
        batch.create_vertex(vid, vec![], vec![]);
    }
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch
}

fn edge_ids(edges: &[fgdb::EdgeRecord]) -> Vec<EId> {
    edges.iter().map(|edge| edge.entry.eid).collect()
}

#[test]
fn aborted_edges_overlay_replays_only_the_seed_edge() {
    let dir = scratch("abort-added-edge");
    let ((), report) = run_async_under_lab(0x8b_01, |root| async move {
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
        let mut add = WriteBatch::new(R);
        add.add_edge(EId(11), VId(1), VId(3), vec![]);
        transaction
            .write(&mut database, add)
            .expect("stage second edge");
        assert_eq!(
            edge_ids(&transaction.edges(&database).expect("overlay edges")),
            vec![EId(10), EId(11)]
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
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            graph.iter_edges().map(|(eid, _)| eid).collect::<Vec<_>>(),
            vec![EId(10)]
        );
        assert!(graph.edge(EId(11)).is_none(), "aborted edge is not durable");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_edges_overlay_deletion_replays_an_empty_edge_set() {
    let dir = scratch("commit-deleted-edge");
    let ((), report) = run_async_under_lab(0x8b_02, |root| async move {
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
            .expect("stage edge deletion");
        assert!(
            transaction
                .edges(&database)
                .expect("overlay edges")
                .is_empty()
        );
        let committed = transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit staged deletion");
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
        assert_eq!(graph.iter_edges().count(), 0);
        assert!(
            graph.edge(EId(10)).is_none(),
            "committed deletion is durable"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn concurrent_mutation_of_edge_observed_by_edges_aborts_and_replays_only_writer() {
    let dir = scratch("edges-read-conflict");
    let ((), report) = run_async_under_lab(0x8b_03, |root| async move {
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

        let mut reader = database.begin(&txn_cx).expect("begin edges reader");
        let mut writer = database.begin(&txn_cx).expect("begin edge writer");
        assert_eq!(
            edge_ids(&reader.edges(&database).expect("transactional edges read")),
            vec![EId(10)]
        );
        let mut disjoint = WriteBatch::new(R);
        disjoint.create_vertex(VId(4), vec![], vec![]);
        reader
            .write(&mut database, disjoint)
            .expect("reader stages disjoint vertex");
        let mut property = WriteBatch::new(R);
        property.set_edge_property(EId(10), PROPERTY, Some(CanonicalScalar::Int(33)));
        writer
            .write(&mut database, property)
            .expect("writer stages observed edge mutation");
        writer
            .commit(&mut database, &commit_cx)
            .await
            .expect("edge writer commits first");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "edges read conflict must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "edges read conflict must name READ-01: {rendered}"
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
        assert_eq!(
            graph
                .edge(EId(10))
                .expect("edge remains durable")
                .props
                .get(&PROPERTY),
            Some(&CanonicalScalar::Int(33)),
            "B's edge mutation is durable"
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
