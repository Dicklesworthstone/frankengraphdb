use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[path = "../src/gql_cert.rs"]
#[allow(dead_code)]
mod gql_cert;

#[test]
fn write_txn_plan_certificate_equals_certify_at_its_basis() {
    let ((), report) = run_async_under_lab(0x95_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-writetxn-plan-certify-{}",
            std::process::id()
        ));
        let keys = DatabaseKeys::new(
            [0x5a; 32],
            DatabaseSecurityNamespaceId([0x77; 32]),
            [0x3c; 32],
        );
        let relation = RelationId(1);
        let mut database = Database::create(&commit_cx, &dir, keys)
            .await
            .expect("create database");
        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        database.write(&commit_cx, seed).await.expect("seed edge");

        let statement = "MATCH (a)-[:R]->(b) RETURN b";
        let bind = RelationBind::new().with_relation("R", relation);
        let transaction = database.begin(&txn_cx).expect("begin transaction");
        let plan = bind.bind(statement).expect("bind plan");
        let expected = gql_cert::certify(&plan, transaction.basis());
        let actual = transaction
            .gql_plan_certificate(statement, &bind)
            .expect("transaction plan certificate");
        assert_eq!(actual.snapshot_seq, expected.snapshot_seq);
        assert_eq!(actual.digest, expected.digest);

        let mut later = WriteBatch::new(relation);
        later.create_vertex(VId(9), vec![], vec![]);
        database
            .write(&commit_cx, later)
            .await
            .expect("advance live frontier");
        let live = database
            .gql_plan_certificate(statement, &bind)
            .expect("live plan certificate");
        assert_eq!(
            live.snapshot_seq,
            database.frontier().expect("read advanced frontier")
        );
        assert_ne!(live.snapshot_seq, actual.snapshot_seq);
        assert_ne!(live.digest, actual.digest);
        transaction.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
