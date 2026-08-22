//! **Incoming two-hop `WHERE a.k != 1` aliases `<>` on the near end**
//! (`fgdb-w5-parsers-nje.54`).
//!
//! The C-style spelling of the near-end inequality: under
//! `(a)<-[:R]-(b)<-[:S]-(c)` the flow is `c-[:S]->b-[:R]->a`, so `a` is
//! the flow's DESTINATION and `!=` reads it there exactly like `<>`,
//! answering the same origins on the same fixture — the two spellings are
//! asserted equal against each other, not just against the literal list.
//! Only the DESTS carry `k` (`1{k:9}`, `4{k:1}`, `7{no k}`), so both
//! spellings keep the chain landing on the `k = 9` dest and answer `[3]`;
//! the keyless dest's chain stays out (missing satisfies neither
//! predicate) and the equality sibling still answers `[6]`. The far-end
//! `!=` is EMPTY (no origin carries `k` — the cells stay separated), the
//! unfiltered statement answers all three, and the direction control runs
//! on the OUTGOING equality (already grammar), which composes nothing on
//! the reversed fixture. The refusals hold: the OUTGOING hop-2 source
//! `!=` (a separate grammar slice) and the `RETURN a` projection stay
//! typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_A_BANG_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k != 1 RETURN c";
const IN_A_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <> 1 RETURN c";
const IN_A_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN c";
const IN_C_BANG_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k != 1 RETURN c";
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
fn incoming_two_hop_near_end_bang_inequality_aliases_the_angle_spelling() {
    let ((), report) = run_async_under_lab(0x54_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-src-bang-ne-{}",
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

        let bang = db
            .execute_gql(IN_A_BANG_NE, &bind)
            .expect("incoming near-end C-style inequality MATCH executes");
        assert_eq!(
            bang,
            vec![VId(3)],
            "only the chain landing on the k=9 dest answers, by its origin"
        );
        assert!(
            !bang.contains(&VId(9)),
            "the keyless dest's chain is OUT: missing satisfies NEITHER \
             predicate"
        );
        assert_eq!(
            bang,
            db.execute_gql(IN_A_NE, &bind)
                .expect("the <> sibling still executes"),
            "!= and <> are the same predicate spelled twice on this fixture"
        );

        assert_eq!(
            db.execute_gql(IN_A_EQ, &bind)
                .expect("the near-end equality sibling still executes"),
            vec![VId(6)],
            "nje.48 unmoved beside the new spelling"
        );

        // The variable control: the far-end cell reads c, which carries no
        // k on this fixture.
        assert!(
            db.execute_gql(IN_C_BANG_NE, &bind)
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

        // The OUTGOING hop-2 source != grammar slice landed after this
        // suite froze; it executes and composes nothing on this reversed
        // fixture (no :S edge leaves an :R destination). The RETURN a
        // projection on the incoming != stays typed-Parse.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k != 1 RETURN c",
                &bind,
            )
            .expect("the landed outgoing source != executes"),
            Vec::<VId>::new(),
            "the outgoing spelling composes nothing on the reversed fixture"
        );
        let off_grammar = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k != 1 RETURN a";
        let err = db.execute_gql(off_grammar, &bind).expect_err(off_grammar);
        assert!(
            matches!(err, GqlError::Parse(_)),
            "{off_grammar:?} must be the typed parse arm: {err:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
