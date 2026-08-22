//! **Incoming two-hop `WHERE c.k < 1` keeps lesser far ends**
//! (`fgdb-w5-parsers-nje.43`).
//!
//! Strictly-less on the incoming chain's far end, with the survivor moved
//! BELOW the boundary: this fixture's origins carry `k = 1`, `k = 0`, and
//! no `k`, so `< 1` keeps exactly the `k = 0` origin — the boundary
//! origin (`1 < 1` false) convicts a `<=` reading, and the keyless origin
//! satisfies no ordered comparator. The fixture also splits the ordered
//! family visibly: `>` is EMPTY here (nothing exceeds 1) while `<` and
//! `<>` both answer `[6]` — so a flipped comparator answers nothing and
//! fails, and the `<`-vs-`<>` discrimination lives in the outgoing
//! four-chain suites. The outgoing `<` composes nothing on the reversed
//! fixture (direction control), and the remaining refusals hold: `>=`,
//! `<=`, the C-style alias, and the `RETURN a` projection stay typed
//! Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_LT: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 1 RETURN c";
const IN_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN c";
const IN_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN c";
const IN_GT: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN c";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const OUT_LT: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k < 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_two_hop_far_end_less_than_keeps_the_lesser_origin() {
    let ((), report) = run_async_under_lab(0x43_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-dst-lt-{}",
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
        r_seed.create_vertex(VId(6), vec![], vec![(K, CanonicalScalar::Int(0))]);
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

        let lesser = db
            .execute_gql(IN_LT, &bind)
            .expect("incoming far-end less-than MATCH executes");
        assert_eq!(lesser, vec![VId(6)], "only the k=0 origin is strictly less");
        assert!(
            !lesser.contains(&VId(9)),
            "the no-k origin satisfies no ordered comparator"
        );
        assert!(
            !lesser.contains(&VId(3)),
            "the boundary origin fails: 1 < 1 is false — a <= reading answers 3"
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
            "nje.41 unmoved — < and <> coincide on this fixture; their \
             discrimination lives in the outgoing four-chain suites"
        );
        assert!(
            db.execute_gql(IN_GT, &bind)
                .expect("the greater sibling still executes")
                .is_empty(),
            "nothing exceeds 1 on this fixture — a flipped comparator \
             answers nothing where < answers [6]"
        );
        assert_eq!(
            db.execute_gql(IN_UNFILTERED, &bind)
                .expect("unfiltered incoming two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three reversed chains answer"
        );

        // The direction control: the OUTGOING less-than composes nothing on
        // this reversed fixture.
        assert!(
            db.execute_gql(OUT_LT, &bind)
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

        // The >= and <= comparators landed after this suite froze. On this
        // fixture the k=0 far end sorts below 1, so >= keeps only the k=1
        // origin while <= keeps both keyed origins; missing-k stays OUT
        // under both. The RETURN a projection stays typed-Parse.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 1 RETURN c",
                &bind,
            )
            .expect("the landed ge comparator executes"),
            vec![VId(3)],
            "only the k=1 far end is at or above 1"
        );
        assert_eq!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN c",
                &bind,
            )
            .expect("the landed le comparator executes"),
            vec![VId(3), VId(6)],
            "both keyed far ends (k=1 and k=0) are at or below 1"
        );
        let off_grammar = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 1 RETURN a";
        let err = db.execute_gql(off_grammar, &bind).expect_err(off_grammar);
        assert!(
            matches!(err, GqlError::Parse(_)),
            "{off_grammar:?} must be the typed parse arm: {err:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
