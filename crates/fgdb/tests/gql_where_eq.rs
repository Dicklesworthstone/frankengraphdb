use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn equality_keeps_only_self_loop_destinations() {
    let ((), report) = run_async_under_lab(0x4e_03, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!("fgdb-where-eq-{}", std::process::id()));
        let relation = RelationId(1);
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
        for vid in [1u128, 2, 5] {
            seed.create_vertex(VId(vid), vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(5), VId(5), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new().with_relation("R", relation);
        let plain = "MATCH (a)-[:R]->(b) RETURN b";
        let equal = "MATCH (a)-[:R]->(b) WHERE a = b RETURN b";
        let unequal = "MATCH (a)-[:R]->(b) WHERE a <> b RETURN b";
        assert_eq!(db.execute_gql(equal, &bind).expect("equality executes"), vec![VId(5)]);
        assert_eq!(db.execute_gql(plain, &bind).expect("plain MATCH executes"), vec![VId(2), VId(5)]);
        assert_eq!(db.execute_gql(unequal, &bind).expect("inequality executes"), vec![VId(2)]);

        let err = db
            .execute_gql("MATCH (a)-[:R]->(b) WHERE a = c RETURN b", &bind)
            .expect_err("unbound c must be rejected");
        assert!(matches!(err, GqlError::Parse(_)), "expected Parse, got {err:?}");
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
