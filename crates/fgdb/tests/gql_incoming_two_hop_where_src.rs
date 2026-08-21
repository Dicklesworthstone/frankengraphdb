//! **Incoming two-hop `WHERE a.k = 1` keeps matching near ends**
//! (`fgdb-w5-parsers-nje.48`).
//!
//! The predicate moves to the NEAR end: under `(a)<-[:R]-(b)<-[:S]-(c)`
//! the flow is `c-[:S]->b-[:R]->a`, so `a` is the flow's DESTINATION and
//! the predicate reads it there while `RETURN c` still answers the
//! origin. On this fixture only the DESTS carry `k` (`1{k:9}`, `4{k:1}`,
//! `7{no k}`) and the origins carry none, so `WHERE a.k = 1` keeps
//! exactly the chain landing on the `k = 1` dest and answers its origin
//! `[6]` — while `WHERE c.k = 1` is EMPTY, separating the near-end cell
//! from the far-end cell (a variable-blind kernel answers `[6]` for
//! both). The keyless dest's chain stays out (missing-is-OUT holds at the
//! near end too), the `k = 9` dest fails equality, and the unfiltered
//! statement answers all three. The OUTGOING spelling composes nothing on
//! this reversed fixture (the direction control), and the refusals hold:
//! `<=` and `!=` on `a.k`, plus the `RETURN a` projection, stay typed
//! Parse. The `<>` spelling keeps the `k = 9` destination's chain and
//! answers its origin `[3]`; `< 1` executes but answers nothing, while
//! `>= 1` answers both keyed chains `[3, 6]`.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_A_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN c";
const IN_A_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <> 1 RETURN c";
const IN_A_LT_1: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k < 1 RETURN c";
const IN_A_GE_1: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k >= 1 RETURN c";
const IN_C_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN c";
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
fn incoming_two_hop_near_end_equality_keeps_the_matching_chain() {
    let ((), report) = run_async_under_lab(0x48_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-src-{}",
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

        let filtered = db
            .execute_gql(IN_A_EQ, &bind)
            .expect("incoming near-end equality MATCH executes");
        assert_eq!(
            filtered,
            vec![VId(6)],
            "only the chain landing on the k=1 dest answers, by its origin"
        );
        assert!(
            !filtered.contains(&VId(9)),
            "the keyless dest's chain is OUT: missing-is-OUT holds at the \
             near end too"
        );
        assert!(
            !filtered.contains(&VId(3)),
            "the k=9 dest's chain fails equality"
        );

        assert_eq!(
            db.execute_gql(IN_A_NE, &bind)
                .expect("incoming near-end inequality MATCH executes"),
            vec![VId(3)],
            "only the chain landing on the k=9 dest answers inequality"
        );

        // The variable control: the far-end cell reads c, which carries no
        // k on this fixture — a variable-blind kernel answers [6] for both.
        assert!(
            db.execute_gql(IN_C_EQ, &bind)
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
            db.execute_gql(OUT_A_EQ, &bind)
                .expect("the outgoing spelling still executes")
                .is_empty(),
            "no :S edge leaves an :R destination on the reversed fixture"
        );

        // The refusals: ordered comparators and != on the near end, plus
        // the RETURN a projection, stay typed Parse.
        // nje.50 sibling lock: incoming near-end > is grammar now. Only
        // the k=9 dest exceeds 1, so its chain's origin answers.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k > 1 RETURN c",
                &bind
            )
            .expect("nje.50 near-end > is grammar, not a Parse"),
            vec![VId(3)],
            "only the k=9 dest's chain is strictly greater"
        );

        assert_eq!(
            db.execute_gql(IN_A_LT_1, &bind)
                .expect("nje.51 near-end < is grammar, not a Parse"),
            Vec::<VId>::new(),
            "k=9 and k=1 both fail < 1; the keyless destination stays OUT"
        );

        assert_eq!(
            db.execute_gql(IN_A_GE_1, &bind)
                .expect("nje.52 near-end >= is grammar, not a Parse"),
            vec![VId(3), VId(6)],
            "both keyed destinations meet >= 1; the keyless destination stays OUT"
        );

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

        for off_grammar in ["MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN a"] {
            let err = db.execute_gql(off_grammar, &bind).expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm: {err:?}"
            );
        }
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
