use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

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
fn labeled_node_only_property_filter_keeps_matching_isolate() {
    let ((), report) = run_async_under_lab(0x44_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-node-only-prop-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(
            VId(1),
            vec![PERSON],
            vec![(K, CanonicalScalar::Int(1))],
        );
        seed.create_vertex(
            VId(2),
            vec![PERSON],
            vec![(K, CanonicalScalar::Int(9))],
        );
        seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(4), vec![PERSON], vec![]);
        seed.create_vertex(VId(5), vec![], vec![]);
        seed.add_edge(EId(10), VId(4), VId(5), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new()
            .with_label("Person", PERSON)
            .with_property("k", K);
        assert_eq!(
            db.execute_gql(FILTERED, &bind)
                .expect("filtered node-only MATCH executes"),
            vec![VId(1)]
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered node-only MATCH executes"),
            vec![VId(1), VId(2), VId(4)]
        );

        let bare = db
            .execute_gql("MATCH (a) WHERE a.k = 1 RETURN a", &bind)
            .expect_err("bare node-only property MATCH is outside the grammar");
        assert!(matches!(bare, GqlError::Parse(_)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
