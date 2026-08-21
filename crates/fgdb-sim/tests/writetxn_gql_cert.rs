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
const MATCH_R: &str = "MATCH (a)-[:R]->(b) RETURN b";
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
        "fgdb-writetxn-gql-cert-{}-{name}",
        std::process::id()
    ))
}

fn staged_edge(source: VId, destination: VId, edge: EId) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(source, vec![], vec![]);
    batch.create_vertex(destination, vec![], vec![]);
    batch.add_edge(edge, source, destination, vec![]);
    batch
}

fn reference_destinations(graph: &fgdb_reference::ReferenceGraph) -> Vec<VId> {
    let mut destinations: Vec<VId> = graph
        .iter_vertices()
        .flat_map(|(source, _)| graph.neighbours(source, R))
        .collect();
    destinations.sort_unstable();
    destinations.dedup();
    destinations
}

#[test]
fn certified_overlay_is_pinned_to_basis_but_abort_is_not_durable() {
    let dir = scratch("abort-is-private");
    let ((), report) = run_async_under_lab(0x7f_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let bind = RelationBind::new().with_relation("R", R);
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");

        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        database
            .write(&commit_cx, seed)
            .await
            .expect("seed durable reference coordinate");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        let basis = transaction.basis();
        transaction
            .write(&mut database, staged_edge(VId(2), VId(3), EId(1)))
            .expect("stage private R edge");
        let (rows, certificate) = transaction
            .execute_gql_certified(&database, MATCH_R, &bind)
            .expect("certified MATCH executes over the overlay");
        assert_eq!(rows, vec![VId(3)]);
        assert_eq!(
            certificate.snapshot_seq, basis,
            "overlay certificate must name the transaction's pinned basis"
        );
        assert!(
            database.execute_gql(MATCH_R, &bind).expect("base MATCH executes").is_empty(),
            "the durable view does not contain the certified staged edge"
        );

        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), basis.0 as usize);
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("seeded reference coordinate exists");
        assert!(reference_destinations(graph).is_empty());
        assert!(graph.edge(EId(1)).is_none(), "aborted staged edge is not durable");
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_overlay_replays_the_certified_destination_set() {
    let dir = scratch("commit-matches-certificate");
    let ((), report) = run_async_under_lab(0x7f_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let bind = RelationBind::new().with_relation("R", R);
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");

        let mut transaction = database.begin(&txn_cx).expect("begin pinned transaction");
        transaction
            .write(&mut database, staged_edge(VId(1), VId(2), EId(1)))
            .expect("stage private R edge");
        let (certified_rows, certificate) = transaction
            .execute_gql_certified(&database, MATCH_R, &bind)
            .expect("certified MATCH executes over the overlay");
        assert_eq!(certified_rows, vec![VId(2)]);
        assert_eq!(certificate.snapshot_seq, transaction.basis());
        transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit certified overlay");
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
            reference_destinations(graph),
            certified_rows,
            "independent replay destinations equal the certified overlay rows"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
