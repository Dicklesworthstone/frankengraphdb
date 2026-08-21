//! **Incoming two-hop `WHERE a.k < 9` keeps lesser near ends**
//! (`fgdb-w5-parsers-nje.51`).
//!
//! The strict-lesser comparator moves to the NEAR end: under
//! `(a)<-[:R]-(b)<-[:S]-(c)` the flow is `c-[:S]->b-[:R]->a`, so `a` is
//! the flow's DESTINATION and the predicate reads it there while
//! `RETURN c` still answers the origin. On this fixture only the DESTS
//! carry `k` (`1{k:9}`, `4{k:1}`, `7{no k}`), so `< 9` keeps exactly the
//! chain landing on the `k = 1` dest (`1 < 9` true — dropping it convicts
//! a kernel that misreads the bound) and answers `[6]`; the `k = 9` dest
//! sits ON the boundary (`9 < 9` false, convicting a `<=` reading) and
//! the keyless dest satisfies no ordered comparator. The landed siblings
//! stay unmoved (`= 1` answers `[6]`, `<> 1` and `> 1` answer `[3]`), the
//! far-end `<` is EMPTY (no origin carries `k`), and the direction
//! control runs on the OUTGOING equality (already grammar), which
//! composes nothing on the reversed fixture. The near-end `>=` is
//! grammar since parser 2ef32998 and answers `[3, 6]`; the refusals
//! hold: the still-unlanded near-end comparators, the C-style alias,
//! the `RETURN a` projection, and the OUTGOING hop-2 source `<` (a
//! separate grammar slice) stay typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_A_LT_9: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k < 9 RETURN c";
const IN_A_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN c";
const IN_A_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <> 1 RETURN c";
const IN_A_GT: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k > 1 RETURN c";
const IN_C_LT: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 9 RETURN c";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const OUT_A_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_two_hop_near_end_less_than_keeps_the_lesser_chain() {
    let ((), report) = run_async_under_lab(0x51_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-src-lt-{}",
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

        let lesser = db
            .execute_gql(IN_A_LT_9, &bind)
            .expect("incoming near-end less-than MATCH executes");
        assert_eq!(
            lesser,
            vec![VId(6)],
            "only the chain landing on the k=1 dest is below 9, by its origin"
        );
        assert!(
            !lesser.contains(&VId(3)),
            "the k=9 dest's chain sits ON the boundary: 9 < 9 is false — a \
             <= reading answers 3 too"
        );
        assert!(
            !lesser.contains(&VId(9)),
            "the keyless dest's chain satisfies no ordered comparator"
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
            "nje.49 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(IN_A_GT, &bind)
                .expect("the near-end greater sibling still executes"),
            vec![VId(3)],
            "nje.50 unmoved — > and < answer opposite chains here"
        );

        // The variable control: the far-end cell reads c, which carries no
        // k on this fixture.
        assert!(
            db.execute_gql(IN_C_LT, &bind)
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

        // The direction control: the OUTGOING equality (already grammar)
        // composes nothing on this reversed fixture.
        assert!(
            db.execute_gql(OUT_A_EQ, &bind)
                .expect("the outgoing equality spelling still executes")
                .is_empty(),
            "no :S edge leaves an :R destination on the reversed fixture"
        );

        // nje.51 retarget: parser 2ef32998 consumes the near-end >= into
        // dst_prop_ge, so the spelling executes — both keyed dests meet
        // >= 1 and answer by their origins. Moved, not weakened.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k >= 1 RETURN c",
                &bind
            )
            .expect("near-end >= is grammar, not a Parse"),
            vec![VId(3), VId(6)],
            "k=9 and k=1 dests both meet >= 1; the keyless dest stays OUT"
        );

        // The refusals: the still-unlanded near-end comparators, the
        // C-style alias, the RETURN a projection, and the OUTGOING hop-2
        // source < (a separate grammar slice) stay typed Parse.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <= 1 RETURN c",
                &bind
            )
            .expect("nje.53 near-end <= is grammar, not a Parse"),
            vec![VId(6)],
            "only the k=1 destination meets <= 1; the keyless destination stays OUT"
        );
        assert_eq!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k != 1 RETURN c",
                &bind,
            )
            .expect("nje.54 near-end != aliases <>"),
            vec![VId(3)],
            "only the k=9 destination differs from 1"
        );

        for off_grammar in [
            "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k < 9 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k < 9 RETURN a",
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
