use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const ONE_HOP_B: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

fn bind() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
}

#[test]
fn two_hop_certified_rows_and_digest_match_the_bound_plan() {
    let ((), report) = run_async_under_lab(0x2c_33, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-two-hop-certified-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");

        let mut r_batch = WriteBatch::new(R);
        for vid in [1u128, 2, 3, 4, 5, 7, 8, 9] {
            r_batch.create_vertex(VId(vid), vec![], vec![]);
        }
        r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        r_batch.add_edge(EId(11), VId(3), VId(2), vec![]);
        r_batch.add_edge(EId(12), VId(1), VId(7), vec![]);
        db.write(&commit, r_batch).await.expect("R edges commit");

        let mut s_batch = WriteBatch::new(S);
        s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
        s_batch.add_edge(EId(21), VId(2), VId(5), vec![]);
        s_batch.add_edge(EId(22), VId(9), VId(8), vec![]);
        db.write(&commit, s_batch).await.expect("S edges commit");

        let bind = bind();
        let txn = db.begin(&txn_cx).expect("transaction begins");
        let (two_hop_rows, two_hop_certificate) = txn
            .execute_gql_certified(&db, TWO_HOP_C, &bind)
            .expect("certified two-hop MATCH executes");
        let plan_certificate = txn
            .gql_plan_certificate(TWO_HOP_C, &bind)
            .expect("two-hop plan certifies");
        let (repeat_rows, repeat_certificate) = txn
            .execute_gql_certified(&db, TWO_HOP_C, &bind)
            .expect("certified two-hop MATCH repeats");
        let (one_hop_rows, one_hop_certificate) = txn
            .execute_gql_certified(&db, ONE_HOP_B, &bind)
            .expect("certified one-hop MATCH executes");

        assert_eq!(two_hop_rows, vec![VId(4), VId(5)]);
        assert_eq!(two_hop_certificate.digest, plan_certificate.digest);
        assert_eq!(repeat_rows, two_hop_rows);
        assert_eq!(repeat_certificate, two_hop_certificate);
        assert_eq!(one_hop_rows, vec![VId(2), VId(7)]);
        assert_ne!(one_hop_certificate.digest, two_hop_certificate.digest);
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
