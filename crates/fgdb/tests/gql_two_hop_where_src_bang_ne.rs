//! **Two-hop `WHERE a.k != 1` aliases `<>` on the anchor**
//! (`fgdb-w5-parsers-nje.56`).
//!
//! The C-style spelling of the anchor inequality: `!=` gates the whole
//! composed path exactly like `<>`, so beside the literal `[6]` pin the
//! `!=` rows are asserted EQUAL to the `<>` rows themselves — one
//! comparator, two spellings. Three disjoint `R∘S` chains whose anchors
//! differ only in `k`: the inequality keeps the `k = 9` anchor's far end
//! under the missing-is-OUT law (the no-`k` anchor's far end must not
//! leak in), the equality sibling still answers `[3]`, and the unfiltered
//! statement answers all three. The far-end `!=` (grammar since
//! fgdb-tdrh) still executes and answers nothing here — no far end
//! carries `k` on this fixture. One refusal holds the grammar's edge:
//! the `RETURN a` projection under the anchor `!=` stays typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const SRC_BANG_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k != 1 RETURN c";
const SRC_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k <> 1 RETURN c";
const SRC_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const DST_BANG_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn two_hop_anchor_bang_inequality_aliases_the_angle_spelling() {
    let ((), report) = run_async_under_lab(0x56_02, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-src-bang-ne-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut r_seed = WriteBatch::new(R);
        r_seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
        r_seed.create_vertex(VId(2), vec![], vec![]);
        r_seed.create_vertex(VId(3), vec![], vec![]);
        r_seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(9))]);
        r_seed.create_vertex(VId(5), vec![], vec![]);
        r_seed.create_vertex(VId(6), vec![], vec![]);
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
        db.write(&commit, s_seed)
            .await
            .expect("S continuations commit");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_relation("S", S)
            .with_property("k", K);

        let bang = db
            .execute_gql(SRC_BANG_NE, &bind)
            .expect("two-hop anchor C-style inequality MATCH executes");
        assert_eq!(
            bang,
            vec![VId(6)],
            "only the k=9 anchor's chain composes to its far end"
        );
        assert!(
            !bang.contains(&VId(9)),
            "the no-k anchor's far end is OUT, not trivially unequal"
        );
        assert_eq!(
            bang,
            db.execute_gql(SRC_NE, &bind)
                .expect("the <> sibling still executes"),
            "!= and <> are one comparator in two spellings on the anchor"
        );

        assert_eq!(
            db.execute_gql(SRC_EQ, &bind)
                .expect("the anchor-equality sibling still executes"),
            vec![VId(3)],
            "nje.21 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three chains answer"
        );

        // The variable control: the far-end != (grammar since fgdb-tdrh)
        // still executes — no far end carries k on this fixture.
        assert!(
            db.execute_gql(DST_BANG_NE, &bind)
                .expect("the far-end != spelling still executes (fgdb-tdrh)")
                .is_empty(),
            "no far end carries k — the anchor and far-end cells are \
             separated"
        );

        // The refusal: the RETURN a projection under the anchor != stays
        // typed Parse.
        for off_grammar in ["MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k != 1 RETURN a"] {
            let err = db.execute_gql(off_grammar, &bind).expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm: {err:?}"
            );
        }
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
