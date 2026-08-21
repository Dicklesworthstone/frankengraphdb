//! **Incoming two-hop `WHERE a.k > 1` keeps greater near ends**
//! (`fgdb-w5-parsers-nje.50`).
//!
//! The strict-greater comparator moves to the NEAR end: under
//! `(a)<-[:R]-(b)<-[:S]-(c)` the flow is `c-[:S]->b-[:R]->a`, so `a` is
//! the flow's DESTINATION and the predicate reads it there while
//! `RETURN c` still answers the origin. On this fixture only the DESTS
//! carry `k` (`1{k:9}`, `4{k:1}`, `7{no k}`), so `> 1` keeps exactly the
//! chain landing on the `k = 9` dest and answers `[3]` — and the literal
//! sweep separates `>` from `<>`, which coincides at literal 1: `> 0`
//! answers BOTH keyed chains `[3, 6]` where `<> 0` would answer the same
//! two, but `<> 1` answers `[3]` alone while `> 0` answers two — and
//! `> 9` answers nothing (nothing exceeds the top key). The keyless
//! dest's chain satisfies no ordered comparator, the equality and
//! inequality siblings stay unmoved, the far-end `>` is EMPTY (no origin
//! carries `k`), and the OUTGOING spelling composes nothing on this
//! reversed fixture (the direction control). The refusals hold: the
//! still-unlanded ordered comparators on `a.k`, the C-style alias, and
//! the `RETURN a` projection stay typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_A_GT_1: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k > 1 RETURN c";
const IN_A_GT_0: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k > 0 RETURN c";
const IN_A_GT_9: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k > 9 RETURN c";
const IN_A_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN c";
const IN_A_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <> 1 RETURN c";
const IN_C_GT: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN c";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const OUT_A_GT: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k > 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_two_hop_near_end_greater_than_keeps_the_greater_chain() {
    let ((), report) = run_async_under_lab(0x50_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-src-gt-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut r_seed = WriteBatch::new(R);
        r_seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(9))]);
        r_seed.create_vertex(VId(2), vec![], vec![]);
        r_seed.create_vertex(VId(3), vec![], vec![]);
        r_seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(1))]);
        r_seed.create_vertex(VId(5), vec![], vec![]);
        r_seed.create_vertex(VId(6), vec![], vec![]);
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
            .execute_gql(IN_A_GT_1, &bind)
            .expect("incoming near-end greater-than MATCH executes");
        assert_eq!(
            greater,
            vec![VId(3)],
            "only the chain landing on the k=9 dest exceeds 1, by its origin"
        );
        assert!(
            !greater.contains(&VId(9)),
            "the keyless dest's chain satisfies no ordered comparator"
        );
        assert!(
            !greater.contains(&VId(6)),
            "the k=1 dest's chain fails: 1 > 1 is false"
        );

        // The literal sweep separates > from <>: at literal 0 both keyed
        // chains exceed it, where <> 1 answers [3] alone.
        assert_eq!(
            db.execute_gql(IN_A_GT_0, &bind)
                .expect("the lower-literal spelling executes"),
            vec![VId(3), VId(6)],
            "both keyed dests exceed 0 — an <>-aliased kernel answers [3]"
        );
        assert!(
            db.execute_gql(IN_A_GT_9, &bind)
                .expect("the top-literal spelling executes")
                .is_empty(),
            "nothing exceeds the top key 9"
        );

        assert_eq!(
            db.execute_gql(IN_A_EQ, &bind)
                .expect("the near-end equality sibling still executes"),
            vec![VId(6)],
            "nje.48 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(IN_A_NE, &bind)
                .expect("the near-end inequality sibling still executes"),
            vec![VId(3)],
            "nje.49 unmoved — <> stays grammar this slice"
        );

        // The variable control: the far-end cell reads c, which carries no
        // k on this fixture.
        assert!(
            db.execute_gql(IN_C_GT, &bind)
                .expect("the far-end spelling still executes")
                .is_empty(),
            "no origin carries k on this fixture — the near-end and \
             far-end cells are separated"
        );

        assert_eq!(
            db.execute_gql(IN_UNFILTERED, &bind)
                .expect("unfiltered incoming two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three reversed chains answer"
        );

        // The direction control: the OUTGOING spelling composes nothing on
        // this reversed fixture.
        assert!(
            db.execute_gql(OUT_A_GT, &bind)
                .expect("the outgoing spelling still executes")
                .is_empty(),
            "no :S edge leaves an :R destination on the reversed fixture"
        );

        // The refusals: the still-unlanded ordered comparators on the near
        // end, the C-style alias, and the RETURN a projection stay typed
        // Parse.
        for off_grammar in [
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k < 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k >= 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <= 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k != 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k > 1 RETURN a",
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
