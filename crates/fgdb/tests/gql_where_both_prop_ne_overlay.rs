use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

#[test]
fn both_property_inequalities_see_the_staged_pair_without_dirty_reads() {
    let ((), report) = run_async_under_lab(0x55_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-where-both-prop-ne-overlay-{}",
            std::process::id()
        ));
        let relation = RelationId(1);
        let k = PropertyKeyId(7);
        let m = PropertyKeyId(9);
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
        seed.create_vertex(VId(1), vec![], vec![(k, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![(m, CanonicalScalar::Int(9))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let mut staged = WriteBatch::new(relation);
        staged.create_vertex(VId(7), vec![], vec![(k, CanonicalScalar::Int(0))]);
        staged.create_vertex(VId(8), vec![], vec![(m, CanonicalScalar::Int(0))]);
        staged.add_edge(EId(11), VId(7), VId(8), vec![]);
        txn.write(&mut db, staged).expect("overlay stages");

        let bind = RelationBind::new()
            .with_relation("R", relation)
            .with_property("k", k)
            .with_property("m", m);
        let unfiltered = "MATCH (a)-[:R]->(b) RETURN b";
        assert_eq!(
            txn.execute_gql(&db, unfiltered, &bind)
                .expect("unfiltered overlay MATCH executes"),
            vec![VId(2), VId(8)]
        );
        assert_eq!(
            db.execute_gql(unfiltered, &bind)
                .expect("unfiltered base MATCH executes"),
            vec![VId(2)]
        );

        let filtered = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m <> 9 RETURN b";
        assert_eq!(
            txn.execute_gql(&db, filtered, &bind)
                .expect("overlay conjunction executes"),
            vec![VId(8)]
        );
        assert_eq!(
            db.execute_gql(filtered, &bind)
                .expect("base conjunction executes"),
            Vec::<VId>::new()
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
