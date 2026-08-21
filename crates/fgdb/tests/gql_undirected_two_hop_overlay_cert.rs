use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const UNDIRECTED_TWO_HOP: &str = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";

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
fn staged_second_hop_changes_rows_but_not_the_basis_certificate() {
    let ((), report) = run_async_under_lab(0x36_0c, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-undirected-two-hop-overlay-cert-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");

        let mut r_batch = WriteBatch::new(R);
        for vid in [1u128, 2, 4] {
            r_batch.create_vertex(VId(vid), vec![], vec![]);
        }
        r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, r_batch).await.expect("R edge commits");
        let mut s_batch = WriteBatch::new(S);
        s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
        db.write(&commit, s_batch).await.expect("S edge commits");

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let basis = txn.basis();
        let mut staged = WriteBatch::new(S);
        staged.create_vertex(VId(5), vec![], vec![]);
        staged.add_edge(EId(21), VId(2), VId(5), vec![]);
        txn.write(&mut db, staged).expect("second-hop edge stages");

        let bind = bind();
        assert_eq!(
            txn.execute_gql(&db, UNDIRECTED_TWO_HOP, &bind)
                .expect("overlay two-hop MATCH"),
            vec![VId(4), VId(5)]
        );
        assert_eq!(
            db.execute_gql(UNDIRECTED_TWO_HOP, &bind)
                .expect("durable two-hop MATCH"),
            vec![VId(4)]
        );

        let txn_certificate = txn
            .gql_plan_certificate(UNDIRECTED_TWO_HOP, &bind)
            .expect("transaction plan certifies");
        let basis_certificate = db
            .gql_plan_certificate_at(UNDIRECTED_TWO_HOP, &bind, basis)
            .expect("basis plan certifies");
        assert_eq!(txn_certificate, basis_certificate);
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
