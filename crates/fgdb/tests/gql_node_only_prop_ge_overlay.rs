use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, VId};

#[test]
fn node_only_greater_or_equal_includes_boundary_without_dirty_reads() {
    let ((), report) = run_async_under_lab(0x68_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-only-prop-ge-overlay-{}",
            std::process::id()
        ));
        let relation = RelationId(1);
        let person = LabelId(3);
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
        seed.create_vertex(
            VId(1),
            vec![person],
            vec![(key, CanonicalScalar::Int(1))],
        );
        db.write(&commit, seed).await.expect("durable vertex commits");

        let mut txn = db.begin(&txn_cx).expect("transaction begins");
        let mut staged = WriteBatch::new(relation);
        staged.create_vertex(
            VId(2),
            vec![person],
            vec![(key, CanonicalScalar::Int(9))],
        );
        txn.write(&mut db, staged).expect("Person isolate stages");

        let bind = RelationBind::new()
            .with_label("Person", person)
            .with_property("k", key);
        let unfiltered = "MATCH (a:Person) RETURN a";
        assert_eq!(
            txn.execute_gql(&db, unfiltered, &bind)
                .expect("unfiltered overlay MATCH executes"),
            vec![VId(1), VId(2)]
        );
        assert_eq!(
            db.execute_gql(unfiltered, &bind)
                .expect("unfiltered base MATCH executes"),
            vec![VId(1)]
        );

        let filtered = "MATCH (a:Person) WHERE a.k >= 1 RETURN a";
        assert_eq!(
            txn.execute_gql(&db, filtered, &bind)
                .expect("overlay greater-or-equal MATCH executes"),
            vec![VId(1), VId(2)]
        );
        assert_eq!(
            db.execute_gql(filtered, &bind)
                .expect("base greater-or-equal MATCH executes"),
            vec![VId(1)]
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
