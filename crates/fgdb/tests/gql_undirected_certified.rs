use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const UNDIRECTED_B: &str = "MATCH (a)-[:R]-(b) RETURN b";
const DIRECTED_B: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn undirected_certified_rows_and_digest_match_the_bound_plan() {
    let ((), report) = run_async_under_lab(0x35_03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-undirected-certified-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");

        let mut batch = WriteBatch::new(R);
        for vid in [1u128, 2, 3] {
            batch.create_vertex(VId(vid), vec![], vec![]);
        }
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(3), VId(2), vec![]);
        db.write(&commit, batch).await.expect("fixture commits");

        let bind = RelationBind::new().with_relation("R", R);
        let txn = db.begin(&txn_cx).expect("transaction begins");
        let (undirected_rows, undirected_certificate) = txn
            .execute_gql_certified(&db, UNDIRECTED_B, &bind)
            .expect("certified undirected MATCH executes");
        let plan_certificate = txn
            .gql_plan_certificate(UNDIRECTED_B, &bind)
            .expect("undirected plan certifies");
        let (repeat_rows, repeat_certificate) = txn
            .execute_gql_certified(&db, UNDIRECTED_B, &bind)
            .expect("certified undirected MATCH repeats");
        let (directed_rows, directed_certificate) = txn
            .execute_gql_certified(&db, DIRECTED_B, &bind)
            .expect("certified directed MATCH executes");

        assert_eq!(undirected_rows, vec![VId(1), VId(2), VId(3)]);
        assert_eq!(undirected_certificate.digest, plan_certificate.digest);
        assert_eq!(repeat_rows, undirected_rows);
        assert_eq!(repeat_certificate, undirected_certificate);
        assert_eq!(directed_rows, vec![VId(2)]);
        assert_ne!(directed_certificate.digest, undirected_certificate.digest);
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
