use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const LABELED: &str = "MATCH (a:Person)-[:R]->(b) RETURN b";
const UNLABELED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn labeled_plan_certificate_is_distinct_and_deterministic() {
    let ((), report) = run_async_under_lab(0x38_05, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!("fgdb-node-label-cert-{}", std::process::id()));
        let db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut db = db;
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![PERSON], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(&commit, seed).await.expect("seed commits");
        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_label("Person", PERSON);

        let labeled = db
            .gql_plan_certificate(LABELED, &bind)
            .expect("labeled MATCH certifies");
        let unlabeled = db
            .gql_plan_certificate(UNLABELED, &bind)
            .expect("unlabeled MATCH certifies");

        assert_eq!(labeled.snapshot_seq, unlabeled.snapshot_seq);
        assert_ne!(labeled.digest, unlabeled.digest);
        assert_eq!(
            db.execute_gql(LABELED, &bind)
                .expect("labeled MATCH executes"),
            vec![VId(2)],
            "the certificate distinction accompanies the labeled product answer"
        );
        assert_eq!(
            db.gql_plan_certificate(LABELED, &bind)
                .expect("labeled MATCH re-certifies"),
            labeled,
            "same labeled plan at the same frontier has the same certificate"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
