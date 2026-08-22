use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const FILTERED: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn source_property_equality_filters_match_destinations() {
    let ((), report) = run_async_under_lab(0x41_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!("fgdb-gql-where-prop-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.create_vertex(VId(5), vec![], vec![]);
        seed.create_vertex(VId(6), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        seed.add_edge(EId(12), VId(5), VId(6), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K);
        assert_eq!(
            db.execute_gql(FILTERED, &bind)
                .expect("property-filtered MATCH executes"),
            vec![VId(2)]
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered MATCH executes"),
            vec![VId(2), VId(4), VId(6)]
        );

        let missing = db
            .execute_gql("MATCH (a)-[:R]->(b) WHERE a.missing = 1 RETURN b", &bind)
            .expect_err("unbound property must fail");
        assert!(matches!(missing, GqlError::Bind(_)));
        let variable_rhs = db
            .execute_gql("MATCH (a)-[:R]->(b) WHERE a.k = b RETURN b", &bind)
            .expect_err("variable RHS is outside the grammar");
        assert!(matches!(variable_rhs, GqlError::Parse(_)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
