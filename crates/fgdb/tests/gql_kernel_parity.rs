use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn live_as_of_and_empty_transaction_match_agree() {
    let ((), report) = run_async_under_lab(0x71_17, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-kernel-parity-{}",
            std::process::id()
        ));
        let mut database = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut batch = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(3)] {
            batch.create_vertex(vid, vec![], vec![]);
        }
        batch.add_edge(EId(10), VId(1), VId(3), vec![]);
        batch.add_edge(EId(11), VId(1), VId(2), vec![]);
        database.write(&commit, batch).await.expect("seed commits");

        let frontier = database.frontier().expect("healthy frontier");
        let bind = RelationBind::new().with_relation("R", R);
        let live = database.execute_gql(PINNED, &bind).expect("live MATCH");
        let historical = database
            .execute_gql_at(PINNED, &bind, frontier)
            .expect("frontier MATCH");
        let transaction = database.begin(&txn_cx).expect("fresh transaction");
        let overlay = transaction
            .execute_gql(&database, PINNED, &bind)
            .expect("empty-overlay MATCH");

        assert_eq!(live, vec![VId(2), VId(3)]);
        assert_eq!(historical, live);
        assert_eq!(overlay, live);
        transaction.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
