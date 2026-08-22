use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const IN_TWO_HOP_C: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const IN_ONE_HOP_B: &str = "MATCH (a)<-[:R]-(b) RETURN b";
const OUT_TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_two_hop_plan_certificate_is_shape_distinct() {
    let ((), report) = run_async_under_lab(0x37_0c, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-incoming-two-hop-cert-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");

        let mut r_batch = WriteBatch::new(R);
        for vid in [1u128, 2, 4] {
            r_batch.create_vertex(VId(vid), vec![], vec![]);
        }
        r_batch.add_edge(EId(10), VId(2), VId(1), vec![]);
        db.write(&commit, r_batch).await.expect("R edge commits");
        let mut s_batch = WriteBatch::new(S);
        s_batch.add_edge(EId(20), VId(4), VId(2), vec![]);
        db.write(&commit, s_batch).await.expect("S edge commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_relation("S", S);
        let incoming_two_hop = db
            .gql_plan_certificate(IN_TWO_HOP_C, &bind)
            .expect("incoming two-hop certifies");
        let incoming_one_hop = db
            .gql_plan_certificate(IN_ONE_HOP_B, &bind)
            .expect("incoming one-hop certifies");
        let outgoing_two_hop = db
            .gql_plan_certificate(OUT_TWO_HOP_C, &bind)
            .expect("outgoing two-hop certifies");

        assert_eq!(incoming_two_hop.snapshot_seq, incoming_one_hop.snapshot_seq);
        assert_eq!(incoming_two_hop.snapshot_seq, outgoing_two_hop.snapshot_seq);
        assert_ne!(incoming_two_hop.digest, incoming_one_hop.digest);
        assert_ne!(incoming_two_hop.digest, outgoing_two_hop.digest);

        // Determinism: the same statement at the same sequence re-mints
        // byte-identically.
        assert_eq!(
            db.gql_plan_certificate(IN_TWO_HOP_C, &bind)
                .expect("incoming two-hop re-certifies"),
            incoming_two_hop,
            "same plan + same seq mint the same certificate"
        );

        // The scan cross-check: a colliding certificate cannot hide behind a
        // working scan — the incoming chain composes to exactly the durable
        // far source, matching the product overlay suite's durable answer.
        assert_eq!(
            db.execute_gql(IN_TWO_HOP_C, &bind)
                .expect("incoming two-hop executes"),
            vec![VId(4)],
            "the incoming chain composes to the far :S source"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
