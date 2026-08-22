use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const UNDIRECTED_B: &str = "MATCH (a)-[:R]-(b) RETURN b";
const DIRECTED_B: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn undirected_as_of_pins_incident_vertices() {
    let ((), report) = run_async_under_lab(0x34_02, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-undirected-as-of-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");

        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![], vec![]);
        initial.create_vertex(VId(2), vec![], vec![]);
        initial.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, initial)
            .await
            .expect("initial edge commits");
        let s1 = db.frontier().expect("healthy pinned frontier");

        let mut later = WriteBatch::new(R);
        later.create_vertex(VId(3), vec![], vec![]);
        later.add_edge(EId(11), VId(3), VId(2), vec![]);
        db.write(&commit, later)
            .await
            .expect("later incident edge commits");

        let bind = RelationBind::new().with_relation("R", R);
        assert_eq!(
            db.execute_gql_at(UNDIRECTED_B, &bind, s1)
                .expect("historical undirected MATCH"),
            vec![VId(1), VId(2)]
        );
        assert_eq!(
            db.execute_gql(UNDIRECTED_B, &bind)
                .expect("live undirected MATCH"),
            vec![VId(1), VId(2), VId(3)]
        );
        assert_eq!(
            db.execute_gql_at(DIRECTED_B, &bind, s1)
                .expect("historical directed MATCH"),
            vec![VId(2)]
        );
        assert_eq!(
            db.execute_gql(DIRECTED_B, &bind)
                .expect("live directed MATCH"),
            vec![VId(2)]
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
