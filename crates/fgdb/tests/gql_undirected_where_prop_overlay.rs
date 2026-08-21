use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

#[test]
fn undirected_property_inequality_sees_the_staged_origin_without_dirty_reads() {
    let ((), report) = run_async_under_lab(0x58_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-undirected-where-prop-overlay-{}",
            std::process::id()
        ));
        let relation = RelationId(1);
        let key = PropertyKeyId(7);
        let mut db = Database::create(
            &commit,
            &dir,
            DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            ),
        )
        .await
        .expect("database creates");

        let mut seed = WriteBatch::new(relation);
        seed.create_vertex(VId(1), vec![], vec![(key, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let mut staged = WriteBatch::new(relation);
        staged.create_vertex(VId(3), vec![], vec![(key, CanonicalScalar::Int(9))]);
        staged.create_vertex(VId(4), vec![], vec![]);
        staged.add_edge(EId(11), VId(3), VId(4), vec![]);
        txn.write(&mut db, staged).expect("overlay stages");

        let bind = RelationBind::new()
            .with_relation("R", relation)
            .with_property("k", key);
        let unfiltered = "MATCH (a)-[:R]-(b) RETURN b";
        assert_eq!(
            txn.execute_gql(&db, unfiltered, &bind)
                .expect("unfiltered overlay MATCH executes"),
            vec![VId(1), VId(2), VId(3), VId(4)]
        );
        assert_eq!(
            db.execute_gql(unfiltered, &bind)
                .expect("unfiltered base MATCH executes"),
            vec![VId(1), VId(2)]
        );

        let filtered = "MATCH (a)-[:R]-(b) WHERE a.k <> 1 RETURN b";
        assert_eq!(
            txn.execute_gql(&db, filtered, &bind)
                .expect("overlay inequality MATCH executes"),
            vec![VId(4)]
        );
        assert_eq!(
            db.execute_gql(filtered, &bind)
                .expect("base inequality MATCH executes"),
            Vec::<VId>::new()
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
