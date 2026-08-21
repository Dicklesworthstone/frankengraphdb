//! **Incoming two-hop `WHERE c.k > 1` keeps greater far ends**
//! (`fgdb-w5-parsers-nje.42`).
//!
//! The ordered comparator joins the incoming chain's far-end cell: under
//! `(a)<-[:R]-(b)<-[:S]-(c)` the predicate reads `c` at the flow's
//! ORIGIN, and `> 1` keeps exactly the `k = 9` origin — the boundary
//! origin (`1 > 1` false) convicts a `>=` reading, and the keyless origin
//! satisfies no ordered comparator. On THIS fixture `>` and `<>` answer
//! the same singleton, so both are asserted beside the equality: the
//! discrimination between them lives in the outgoing four-chain suites,
//! while here the point is that the incoming spelling composes through
//! the reversed chains at all — the OUTGOING `>` composes nothing on this
//! fixture (the direction control). Incoming `<` is grammar since nje.43
//! and composes nothing on this fixture; the remaining refusals hold:
//! `>=`, `<=`, the C-style alias, and the `RETURN a` projection on the
//! incoming chain all stay typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_GT: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN c";
const IN_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN c";
const IN_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN c";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const OUT_GT: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k > 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_two_hop_far_end_greater_than_keeps_the_greater_origin() {
    let ((), report) = run_async_under_lab(0x42_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-dst-gt-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut r_seed = WriteBatch::new(R);
        r_seed.create_vertex(VId(1), vec![], vec![]);
        r_seed.create_vertex(VId(2), vec![], vec![]);
        r_seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(1))]);
        r_seed.create_vertex(VId(4), vec![], vec![]);
        r_seed.create_vertex(VId(5), vec![], vec![]);
        r_seed.create_vertex(VId(6), vec![], vec![(K, CanonicalScalar::Int(9))]);
        r_seed.create_vertex(VId(7), vec![], vec![]);
        r_seed.create_vertex(VId(8), vec![], vec![]);
        r_seed.create_vertex(VId(9), vec![], vec![]);
        r_seed.add_edge(EId(10), VId(2), VId(1), vec![]);
        r_seed.add_edge(EId(11), VId(5), VId(4), vec![]);
        r_seed.add_edge(EId(12), VId(8), VId(7), vec![]);
        db.write(&commit, r_seed).await.expect("R chains commit");
        let mut s_seed = WriteBatch::new(S);
        s_seed.add_edge(EId(20), VId(3), VId(2), vec![]);
        s_seed.add_edge(EId(21), VId(6), VId(5), vec![]);
        s_seed.add_edge(EId(22), VId(9), VId(8), vec![]);
        db.write(&commit, s_seed).await.expect("S chains commit");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_relation("S", S)
            .with_property("k", K);

        let greater = db
            .execute_gql(IN_GT, &bind)
            .expect("incoming far-end greater-than MATCH executes");
        assert_eq!(
            greater,
            vec![VId(6)],
            "only the k=9 origin is strictly greater"
        );
        assert!(
            !greater.contains(&VId(9)),
            "the no-k origin satisfies no ordered comparator"
        );
        assert!(
            !greater.contains(&VId(3)),
            "the boundary origin fails: 1 > 1 is false — a >= reading answers 3"
        );

        assert_eq!(
            db.execute_gql(IN_EQ, &bind)
                .expect("the equality sibling still executes"),
            vec![VId(3)],
            "nje.40 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(IN_NE, &bind)
                .expect("the inequality sibling still executes"),
            vec![VId(6)],
            "nje.41 unmoved — > and <> coincide on this fixture; their \
             discrimination lives in the outgoing four-chain suites"
        );
        assert_eq!(
            db.execute_gql(IN_UNFILTERED, &bind)
                .expect("unfiltered incoming two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three reversed chains answer"
        );

        // The direction control: the OUTGOING greater-than composes nothing
        // on this reversed fixture.
        assert!(
            db.execute_gql(OUT_GT, &bind)
                .expect("the outgoing spelling still executes")
                .is_empty(),
            "no :S edge leaves an :R destination on the reversed fixture"
        );

        // nje.43 sibling lock: incoming < is grammar now. On THIS fixture
        // (k spread {1, 9, missing}) it composes nothing — the assertion
        // moved to a live boundary, it never weakened; the k=0 survivor
        // lives in gql_incoming_two_hop_where_dst_lt.rs.
        assert!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 1 RETURN c",
                &bind
            )
            .expect("nje.43 incoming < is grammar, not a Parse")
            .is_empty(),
            "no far end carries k < 1 on this fixture"
        );

        assert_eq!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k != 1 RETURN c",
                &bind,
            )
            .expect("nje.55 incoming far-end != aliases <>"),
            vec![VId(6)],
            "only the far end with k=2 differs from 1"
        );

        // The remaining refusals: the other ordered comparators and the
        // RETURN a projection on the incoming chain.
        for off_grammar in [
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN a",
        ] {
            let err = db.execute_gql(off_grammar, &bind).expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm: {err:?}"
            );
        }
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
