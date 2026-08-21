use asupersync::lab::run_async_under_lab;
use fgdb::{
    CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch, WriteTxnError,
};
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
        "fgdb-writetxn-gql-readset-{}-{name}",
        std::process::id()
    ))
}

fn seed_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(
        VId(2),
        vec![],
        vec![(PROPERTY, CanonicalScalar::Int(0))],
    );
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch
}

fn create_vertex(vid: VId) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(vid, vec![], vec![]);
    batch
}

#[test]
fn changed_match_destination_aborts_read_01_and_replays_only_the_writer() {
    let dir = scratch("destination-conflict");
    let ((), report) = run_async_under_lab(0x7d_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let bind = RelationBind::new().with_relation("R", R);
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable R edge");

        let mut reader = database.begin(&txn_cx).expect("begin MATCH reader");
        let mut writer = database.begin(&txn_cx).expect("begin destination writer");
        assert_eq!(
            reader
                .execute_gql(&database, MATCH_R, &bind)
                .expect("transactional MATCH succeeds"),
            vec![VId(2)]
        );
        reader
            .write(&mut database, create_vertex(VId(3)))
            .expect("reader stages a write disjoint from MATCH elements");

        let mut destination_update = WriteBatch::new(R);
        destination_update.set_vertex_property(
            VId(2),
            PROPERTY,
            Some(CanonicalScalar::Int(22)),
        );
        writer
            .write(&mut database, destination_update)
            .expect("writer stages destination mutation");
        writer
            .commit(&mut database, &commit_cx)
            .await
            .expect("destination writer commits");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "MATCH read conflict must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "MATCH destination abort must name READ-01: {rendered}"
        );
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "seed and writer only");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(graph.neighbours(VId(1), R), vec![VId(2)]);
        assert_eq!(
            graph.vertex(VId(2)).expect("MATCH destination remains").props.get(&PROPERTY),
            Some(&CanonicalScalar::Int(22)),
            "independent replay contains only B's destination property"
        );
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

#[test]
fn disjoint_create_does_not_invalidate_match_destination_read_set() {
    let dir = scratch("disjoint-create");
    let ((), report) = run_async_under_lab(0x7d_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let bind = RelationBind::new().with_relation("R", R);
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable R edge");

        let mut reader = database.begin(&txn_cx).expect("begin MATCH reader");
        let mut creator = database.begin(&txn_cx).expect("begin disjoint creator");
        assert_eq!(
            reader
                .execute_gql(&database, MATCH_R, &bind)
                .expect("transactional MATCH succeeds"),
            vec![VId(2)]
        );
        reader
            .write(&mut database, create_vertex(VId(3)))
            .expect("reader stages its disjoint write at the pinned basis");
        creator
            .write(&mut database, create_vertex(VId(9)))
            .expect("creator stages vertex outside the MATCH read set");
        creator
            .commit(&mut database, &commit_cx)
            .await
            .expect("disjoint creator commits");
        reader
            .commit(&mut database, &commit_cx)
            .await
            .expect("VId(9) does not invalidate MATCH destination VId(2)");
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 3, "seed and both transactions");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(graph.neighbours(VId(1), R), vec![VId(2)]);
        assert!(
            graph.vertex(VId(3)).is_some() && graph.vertex(VId(9)).is_some(),
            "independent replay contains both disjoint transaction writes"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
