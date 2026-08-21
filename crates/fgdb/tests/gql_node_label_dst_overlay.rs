use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const LABELED_DST: &str = "MATCH (a)-[:R]->(b:Person) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn destination_label_overlay_sees_staged_person_destination() {
    let ((), report) = run_async_under_lab(0x38_07, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-node-label-dst-overlay-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![PERSON], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(&commit, seed).await.expect("seed commits");

        let mut txn = db.begin(&txn_cx).expect("txn begins");
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(5), vec![], vec![]);
        staged.create_vertex(VId(6), vec![PERSON], vec![]);
        staged.add_edge(EId(12), VId(5), VId(6), vec![]);
        txn.write(&mut db, staged)
            .expect("stages the labeled destination");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_label("Person", PERSON);
        let overlay = txn
            .execute_gql(&db, LABELED_DST, &bind)
            .expect("overlay destination-labeled MATCH executes");
        assert!(overlay.contains(&VId(2)));
        assert!(overlay.contains(&VId(6)));
        assert!(!overlay.contains(&VId(4)));

        let base = db
            .execute_gql(LABELED_DST, &bind)
            .expect("base destination-labeled MATCH executes");
        assert!(base.contains(&VId(2)));
        assert!(!base.contains(&VId(6)));
        assert!(!base.contains(&VId(4)));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
