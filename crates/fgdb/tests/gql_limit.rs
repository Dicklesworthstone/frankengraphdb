use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const LIMITED: &str = "MATCH (a)-[:R]->(b) RETURN b LIMIT 1";
const UNLIMITED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn limit_keeps_the_smallest_cgse_destination() {
    let ((), report) = run_async_under_lab(0x45_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!("fgdb-gql-limit-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(3), VId(4), VId(6)] {
            seed.create_vertex(vid, vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(1), VId(4), vec![]);
        seed.add_edge(EId(12), VId(3), VId(6), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new().with_relation("R", R);
        assert_eq!(
            db.execute_gql(LIMITED, &bind)
                .expect("limited MATCH executes"),
            vec![VId(2)]
        );
        assert_eq!(
            db.execute_gql(UNLIMITED, &bind)
                .expect("unlimited MATCH executes"),
            vec![VId(2), VId(4), VId(6)]
        );

        let zero = db
            .execute_gql("MATCH (a)-[:R]->(b) RETURN b LIMIT 0", &bind)
            .expect_err("LIMIT 0 is outside the bounded grammar");
        assert!(matches!(zero, GqlError::Parse(_)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
