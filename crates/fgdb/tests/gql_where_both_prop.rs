use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const M: PropertyKeyId = PropertyKeyId(9);
const BOTH: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 AND b.m = 9 RETURN b";
const SOURCE_ONLY: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn source_and_destination_properties_compose() {
    let ((), report) = run_async_under_lab(0x43_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-both-prop-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![(M, CanonicalScalar::Int(9))]);
        seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(4), vec![], vec![(M, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(5), vec![], vec![(K, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(6), vec![], vec![(M, CanonicalScalar::Int(9))]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        seed.add_edge(EId(12), VId(5), VId(6), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K)
            .with_property("m", M);
        assert_eq!(
            db.execute_gql(BOTH, &bind)
                .expect("composed property MATCH executes"),
            vec![VId(2)]
        );
        assert_eq!(
            db.execute_gql(SOURCE_ONLY, &bind)
                .expect("source-only property MATCH executes"),
            vec![VId(2), VId(4)]
        );

        let same_side = db
            .execute_gql(
                "MATCH (a)-[:R]->(b) WHERE a.k = 1 AND a.m = 9 RETURN b",
                &bind,
            )
            .expect_err("two source predicates are outside the bounded grammar");
        assert!(matches!(same_side, GqlError::Parse(_)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
