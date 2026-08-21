use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const UNDIRECTED_TWO_HOP: &str = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";
const UNDIRECTED_ONE_HOP: &str = "MATCH (a)-[:R]-(b) RETURN b";
const DIRECTED_TWO_HOP: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn undirected_two_hop_plan_certificate_is_shape_distinct() {
    let ((), report) = run_async_under_lab(0x36_07, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-undirected-two-hop-cert-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");

        let mut r_batch = WriteBatch::new(R);
        for vid in [1u128, 2, 3] {
            r_batch.create_vertex(VId(vid), vec![], vec![]);
        }
        r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, r_batch).await.expect("R edge commits");
        let mut s_batch = WriteBatch::new(S);
        s_batch.add_edge(EId(20), VId(2), VId(3), vec![]);
        db.write(&commit, s_batch).await.expect("S edge commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_relation("S", S);
        let undirected_two_hop = db
            .gql_plan_certificate(UNDIRECTED_TWO_HOP, &bind)
            .expect("undirected two-hop certifies");
        let undirected_two_hop_again = db
            .gql_plan_certificate(UNDIRECTED_TWO_HOP, &bind)
            .expect("undirected two-hop certifies again");
        let undirected_one_hop = db
            .gql_plan_certificate(UNDIRECTED_ONE_HOP, &bind)
            .expect("undirected one-hop certifies");
        let directed_two_hop = db
            .gql_plan_certificate(DIRECTED_TWO_HOP, &bind)
            .expect("directed two-hop certifies");

        assert_eq!(undirected_two_hop, undirected_two_hop_again);
        assert_eq!(
            undirected_two_hop.snapshot_seq,
            undirected_one_hop.snapshot_seq
        );
        assert_eq!(
            undirected_two_hop.snapshot_seq,
            directed_two_hop.snapshot_seq
        );
        assert_ne!(undirected_two_hop.digest, undirected_one_hop.digest);
        assert_ne!(undirected_two_hop.digest, directed_two_hop.digest);
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
