//! **Two-hop `WHERE c.k != 1` aliases the far-end `<>`**
//! (`fgdb-w5-parsers-nje-56-sdub`).
//!
//! The C-style spelling reaches the composed path's far end: `!=` must
//! alias `<>` exactly, so beside the literal `[6]` pin the `!=` rows are
//! asserted EQUAL to the `<>` rows themselves — one comparator, two
//! spellings. The no-`k` far end must not leak in (missing `k` satisfies
//! neither spelling), the equality sibling and the unfiltered statement
//! are re-pinned, and the direction control executes the INCOMING far-end
//! `<>` (grammar since nje.41), which composes nothing on this outgoing
//! fixture. The `RETURN a` projection under the outgoing `!=` is grammar
//! and answers the surviving chain's hop-1 origin `[4]`, while one
//! refusal holds the grammar's edge this slice: the outgoing near-end
//! `a.k != 1`.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const DST_BANG_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c";
const DST_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c";
const DST_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const IN_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn two_hop_far_end_bang_ne_aliases_the_diamond_spelling() {
    let ((), report) = run_async_under_lab(0x56_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-dst-bang-ne-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut r_seed = WriteBatch::new(R);
        r_seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
        r_seed.create_vertex(VId(2), vec![], vec![]);
        r_seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(1))]);
        r_seed.create_vertex(VId(4), vec![], vec![]);
        r_seed.create_vertex(VId(5), vec![], vec![]);
        r_seed.create_vertex(VId(6), vec![], vec![(K, CanonicalScalar::Int(9))]);
        r_seed.create_vertex(VId(7), vec![], vec![]);
        r_seed.create_vertex(VId(8), vec![], vec![]);
        r_seed.create_vertex(VId(9), vec![], vec![]);
        r_seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        r_seed.add_edge(EId(11), VId(4), VId(5), vec![]);
        r_seed.add_edge(EId(12), VId(7), VId(8), vec![]);
        db.write(&commit, r_seed).await.expect("R chains commit");
        let mut s_seed = WriteBatch::new(S);
        s_seed.add_edge(EId(20), VId(2), VId(3), vec![]);
        s_seed.add_edge(EId(21), VId(5), VId(6), vec![]);
        s_seed.add_edge(EId(22), VId(8), VId(9), vec![]);
        db.write(&commit, s_seed).await.expect("S chains commit");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_relation("S", S)
            .with_property("k", K);

        let bang = db
            .execute_gql(DST_BANG_NE, &bind)
            .expect("far-end != MATCH executes");
        assert_eq!(
            bang,
            vec![VId(6)],
            "only the k=9 far end answers — the k=1 chain fails !="
        );
        assert!(
            !bang.contains(&VId(9)),
            "the no-k far end satisfies neither spelling — a \
             complement-of-equality kernel answers it and is wrong"
        );

        // The alias law itself: != rows equal <> rows, not merely the
        // same literal list.
        assert_eq!(
            bang,
            db.execute_gql(DST_NE, &bind)
                .expect("the diamond sibling still executes"),
            "!= and <> are one comparator in two spellings"
        );

        assert_eq!(
            db.execute_gql(DST_EQ, &bind)
                .expect("the equality sibling still executes"),
            vec![VId(3)],
            "nje.33 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three chains answer"
        );

        // The direction control: the INCOMING far-end <> (grammar since
        // nje.41) composes nothing on this outgoing fixture.
        assert!(
            db.execute_gql(IN_NE, &bind)
                .expect("the incoming diamond spelling still executes")
                .is_empty(),
            "no :S edge arrives at an :R source on the outgoing fixture"
        );

        // The RETURN a projection under the outgoing != is grammar: it
        // answers the hop-1 origin of the surviving chain (moved, not
        // weakened).
        assert_eq!(
            db.execute_gql(
                "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN a",
                &bind
            )
            .expect("the RETURN a projection under the outgoing != executes"),
            vec![VId(4)],
            "the k=9 chain 4-R->5-S->6 answers by its hop-1 origin"
        );
        // The near-end a.k != filter landed after this suite froze. Every
        // near end on this fixture is keyless, and missing-is-OUT holds for
        // the near end exactly as it does for the far end: no chain
        // survives.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k != 1 RETURN c",
                &bind,
            )
            .expect("the landed near-end != executes"),
            Vec::<VId>::new(),
            "every near end lacks k, and missing-is-OUT"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
