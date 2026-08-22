use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const TWO_HOP_A: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a";
const ONE_HOP_B: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

fn bind() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
}

#[test]
fn two_hop_as_of_pins_composed_destinations() {
    let ((), report) = run_async_under_lab(0x28_33, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-two-hop-as-of-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");

        let mut r_batch = WriteBatch::new(R);
        for vid in [1u128, 2, 4, 5, 7, 8, 9] {
            r_batch.create_vertex(VId(vid), vec![], vec![]);
        }
        r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        r_batch.add_edge(EId(11), VId(1), VId(7), vec![]);
        db.write(&commit, r_batch).await.expect("R seed commits");

        let mut initial_s = WriteBatch::new(S);
        initial_s.add_edge(EId(20), VId(2), VId(4), vec![]);
        db.write(&commit, initial_s)
            .await
            .expect("initial S edge commits");
        let s1 = db.frontier().expect("healthy pinned frontier");

        let mut later_s = WriteBatch::new(S);
        later_s.add_edge(EId(21), VId(2), VId(5), vec![]);
        later_s.add_edge(EId(22), VId(9), VId(8), vec![]);
        db.write(&commit, later_s)
            .await
            .expect("later S edges commit");

        let bind = bind();
        assert_eq!(
            db.execute_gql_at(TWO_HOP_C, &bind, s1)
                .expect("historical RETURN c"),
            vec![VId(4)]
        );
        assert_eq!(
            db.execute_gql_at(TWO_HOP_A, &bind, s1)
                .expect("historical RETURN a"),
            vec![VId(1)]
        );
        assert_eq!(
            db.execute_gql(TWO_HOP_C, &bind).expect("live RETURN c"),
            vec![VId(4), VId(5)]
        );
        assert_eq!(
            db.execute_gql(TWO_HOP_A, &bind).expect("live RETURN a"),
            vec![VId(1)]
        );
        assert_eq!(
            db.execute_gql_at(ONE_HOP_B, &bind, s1)
                .expect("historical one-hop RETURN b"),
            vec![VId(2), VId(7)]
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
