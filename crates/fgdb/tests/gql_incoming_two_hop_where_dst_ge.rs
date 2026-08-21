//! **Incoming two-hop `WHERE c.k >= 1` keeps inclusive greater far ends**
//! (`fgdb-w5-parsers-nje.45`).
//!
//! The inclusive comparator joins the incoming chain's far-end cell: under
//! `(a)<-[:R]-(b)<-[:S]-(c)` the predicate reads `c` at the flow's
//! ORIGIN, and `>= 1` keeps BOTH the boundary origin (`1 >= 1` true) and
//! the `k = 9` origin — a strict-`>` cheat drops the boundary and answers
//! `[6]` alone, so the two-element answer convicts it, and the keyless
//! origin satisfies no ordered comparator. The strict siblings stay
//! unmoved beside the new spelling: `>` still answers `[6]`, `=` still
//! `[3]`, `<>` still `[6]`, and the unfiltered incoming two-hop still
//! answers all three chains. The OUTGOING `>=` composes nothing on this
//! reversed fixture (the direction control), and the remaining refusals
//! hold: `<=`, the C-style alias, and the `RETURN a` projection on the
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
const IN_GE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 1 RETURN c";
const IN_GT: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN c";
const IN_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN c";
const IN_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN c";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const OUT_GE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k >= 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_two_hop_far_end_greater_equal_keeps_boundary_and_greater_origins() {
    let ((), report) = run_async_under_lab(0x45_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-dst-ge-{}",
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

        let inclusive = db
            .execute_gql(IN_GE, &bind)
            .expect("incoming far-end greater-equal MATCH executes");
        assert_eq!(
            inclusive,
            vec![VId(3), VId(6)],
            "the boundary and greater origins both satisfy >="
        );
        assert!(
            inclusive.contains(&VId(3)),
            "the boundary origin is IN: 1 >= 1 is true — a strict > cheat \
             answers [6] alone"
        );
        assert!(
            !inclusive.contains(&VId(9)),
            "the no-k origin satisfies no ordered comparator"
        );

        assert_eq!(
            db.execute_gql(IN_GT, &bind)
                .expect("the strict-greater sibling still executes"),
            vec![VId(6)],
            "nje.42 unmoved — strict > drops the boundary that >= keeps"
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
            "nje.41 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(IN_UNFILTERED, &bind)
                .expect("unfiltered incoming two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three reversed chains answer"
        );

        // The direction control: the OUTGOING greater-equal composes nothing
        // on this reversed fixture.
        assert!(
            db.execute_gql(OUT_GE, &bind)
                .expect("the outgoing spelling still executes")
                .is_empty(),
            "no :S edge leaves an :R destination on the reversed fixture"
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

        // The remaining refusals: the unlanded comparator and the RETURN a
        // projection on the incoming chain.
        for off_grammar in [
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 1 RETURN a",
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
