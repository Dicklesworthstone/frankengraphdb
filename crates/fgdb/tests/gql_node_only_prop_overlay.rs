use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const K: PropertyKeyId = PropertyKeyId(7);
const FILTERED: &str = "MATCH (a:Person) WHERE a.k = 1 RETURN a";
const UNFILTERED: &str = "MATCH (a:Person) RETURN a";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn node_only_property_overlay_sees_staged_matching_isolate() {
    let ((), report) = run_async_under_lab(0x44_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-only-prop-overlay-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![PERSON], vec![(K, CanonicalScalar::Int(9))]);
        db.write(&commit, seed)
            .await
            .expect("durable vertices commit");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(7), vec![PERSON], vec![(K, CanonicalScalar::Int(1))]);
        txn.write(&mut db, staged)
            .expect("stages matching Person isolate");

        let bind = RelationBind::new()
            .with_label("Person", PERSON)
            .with_property("k", K);
        assert_eq!(
            txn.execute_gql(&db, FILTERED, &bind)
                .expect("filtered overlay MATCH executes"),
            vec![VId(1), VId(7)]
        );
        assert_eq!(
            db.execute_gql(FILTERED, &bind)
                .expect("filtered base MATCH executes"),
            vec![VId(1)]
        );
        assert_eq!(
            txn.execute_gql(&db, UNFILTERED, &bind)
                .expect("unfiltered overlay MATCH executes"),
            vec![VId(1), VId(2), VId(7)]
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
