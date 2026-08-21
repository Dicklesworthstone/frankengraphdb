use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

#[test]
fn two_hop_far_end_property_inequality_sees_the_staged_continuation() {
    let ((), report) = run_async_under_lab(0x71_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-two-hop-where-dst-ne-overlay-{}",
            std::process::id()
        ));
        let r = RelationId(1);
        let s = RelationId(2);
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

        // Both hop-1 edges and all vertices are durable. The transaction then
        // stages only one :S batch, preserving WriteTxn's one-relation rule.
        let mut seed_r = WriteBatch::new(r);
        seed_r.create_vertex(VId(1), vec![], vec![]);
        seed_r.create_vertex(VId(2), vec![], vec![]);
        seed_r.create_vertex(VId(3), vec![], vec![(key, CanonicalScalar::Int(1))]);
        seed_r.create_vertex(VId(4), vec![], vec![]);
        seed_r.create_vertex(VId(5), vec![], vec![]);
        seed_r.create_vertex(VId(6), vec![], vec![(key, CanonicalScalar::Int(9))]);
        seed_r.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed_r.add_edge(EId(12), VId(4), VId(5), vec![]);
        db.write(&commit, seed_r).await.expect("R fixture commits");

        let mut seed_s = WriteBatch::new(s);
        seed_s.add_edge(EId(11), VId(2), VId(3), vec![]);
        db.write(&commit, seed_s).await.expect("S fixture commits");

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let mut staged_s = WriteBatch::new(s);
        staged_s.add_edge(EId(13), VId(5), VId(6), vec![]);
        txn.write(&mut db, staged_s).expect("S overlay stages");

        let bind = RelationBind::new()
            .with_relation("R", r)
            .with_relation("S", s)
            .with_property("k", key);
        let filtered =
            "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c";
        assert_eq!(
            txn.execute_gql(&db, filtered, &bind)
                .expect("overlay far-end inequality MATCH executes"),
            vec![VId(6)]
        );
        assert_eq!(
            db.execute_gql(filtered, &bind)
                .expect("base far-end inequality MATCH executes"),
            Vec::<VId>::new()
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
