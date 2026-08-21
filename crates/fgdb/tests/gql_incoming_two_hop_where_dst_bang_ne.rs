//! **Incoming two-hop `WHERE c.k != 1` aliases the far-end `<>`**
//! (`fgdb-w5-parsers-nje.55`).
//!
//! The C-style spelling joins the incoming chain's far-end cell: under
//! `(a)<-[:R]-(b)<-[:S]-(c)` the predicate reads `c` at the flow's
//! ORIGIN, and `!=` must alias `<>` exactly — so beside the literal
//! `[6]` pin, the `!=` rows are asserted EQUAL to the `<>` rows, not
//! merely to the same list. The keyless origin must not leak in
//! (missing `k` satisfies neither spelling), the `k = 1` origin fails
//! the inequality, the equality sibling and the unfiltered statement
//! are re-pinned, and the direction control runs on the OUTGOING `<>`
//! (already grammar), which composes nothing on the reversed fixture.
//! The refusals hold: the outgoing `!=` (a separate grammar slice) and
//! the `RETURN a` projection on the incoming `!=` stay typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_BANG_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k != 1 RETURN c";
const IN_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN c";
const IN_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN c";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const OUT_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_two_hop_far_end_bang_ne_aliases_the_diamond_spelling() {
    let ((), report) = run_async_under_lab(0x55_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-dst-bang-ne-{}",
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

        let bang = db
            .execute_gql(IN_BANG_NE, &bind)
            .expect("incoming far-end != MATCH executes");
        assert_eq!(
            bang,
            vec![VId(6)],
            "only the k=9 origin answers the C-style inequality"
        );
        assert!(
            !bang.contains(&VId(9)),
            "the no-k origin satisfies neither spelling — a \
             complement-of-equality kernel answers it and is wrong"
        );
        assert!(
            !bang.contains(&VId(3)),
            "the k=1 origin fails the inequality"
        );

        // The alias law itself: != rows equal <> rows, not merely the
        // same literal list.
        assert_eq!(
            bang,
            db.execute_gql(IN_NE, &bind)
                .expect("the diamond sibling still executes"),
            "!= and <> are one comparator in two spellings"
        );

        assert_eq!(
            db.execute_gql(IN_EQ, &bind)
                .expect("the equality sibling still executes"),
            vec![VId(3)],
            "nje.40 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(IN_UNFILTERED, &bind)
                .expect("unfiltered incoming two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three reversed chains answer"
        );

        // The direction control: the OUTGOING <> (already grammar)
        // composes nothing on this reversed fixture.
        assert!(
            db.execute_gql(OUT_NE, &bind)
                .expect("the outgoing diamond spelling still executes")
                .is_empty(),
            "no :S edge leaves an :R destination on the reversed fixture"
        );

        // The refusals: the outgoing != is a separate grammar slice, and
        // the RETURN a projection on the incoming != stays refused.
        for off_grammar in [
            "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k != 1 RETURN a",
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
