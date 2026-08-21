//! **Two-hop `WHERE c.k > 1` keeps greater far ends**
//! (`fgdb-w5-parsers-nje.36`).
//!
//! The ordered comparator reaches the composed path's far end, with FOUR
//! chains so `>` cannot cheat as `<>`: the `k = 0` far end is not-equal
//! but not greater, so it separates the two predicates ON THIS FIXTURE —
//! `> 1` answers `[6]` while `<> 1` answers `[6, 12]`, and both are
//! asserted so the divergence is visible in one test. The boundary far
//! end (`k = 1`, `1 > 1` false), the keyless far end, and the
//! below-boundary far end are each out of `>` by name; the equality and
//! unfiltered statements are re-pinned; and three refusals hold the
//! edges: `>=` and the C-style `!=` on `c` stay Parse (the nje.33/34
//! plants live on), and a WHERE on the incoming two-hop chain stays
//! off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const DST_GT: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k > 1 RETURN c";
const DST_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c";
const DST_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn two_hop_far_end_greater_than_keeps_only_the_greater_chain() {
    let ((), report) = run_async_under_lab(0x36_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-dst-gt-{}",
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
        r_seed.create_vertex(VId(10), vec![], vec![]);
        r_seed.create_vertex(VId(11), vec![], vec![]);
        r_seed.create_vertex(VId(12), vec![], vec![(K, CanonicalScalar::Int(0))]);
        r_seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        r_seed.add_edge(EId(11), VId(4), VId(5), vec![]);
        r_seed.add_edge(EId(12), VId(7), VId(8), vec![]);
        r_seed.add_edge(EId(13), VId(10), VId(11), vec![]);
        db.write(&commit, r_seed).await.expect("R chains commit");
        let mut s_seed = WriteBatch::new(S);
        s_seed.add_edge(EId(20), VId(2), VId(3), vec![]);
        s_seed.add_edge(EId(21), VId(5), VId(6), vec![]);
        s_seed.add_edge(EId(22), VId(8), VId(9), vec![]);
        s_seed.add_edge(EId(23), VId(11), VId(12), vec![]);
        db.write(&commit, s_seed).await.expect("S chains commit");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_relation("S", S)
            .with_property("k", K);

        let greater = db
            .execute_gql(DST_GT, &bind)
            .expect("far-end greater-than MATCH executes");
        assert_eq!(
            greater,
            vec![VId(6)],
            "only the k=9 far end is strictly greater"
        );
        assert!(
            !greater.contains(&VId(3)),
            "the boundary far end fails: 1 > 1 is false — a >= reading answers 3"
        );
        assert!(
            !greater.contains(&VId(12)),
            "the k=0 far end is not-equal but NOT greater — a <> cheat \
             answers 12 and fails here"
        );
        assert!(
            !greater.contains(&VId(9)),
            "the no-k far end satisfies no ordered comparator"
        );

        assert_eq!(
            db.execute_gql(DST_NE, &bind)
                .expect("the inequality sibling executes"),
            vec![VId(6), VId(12)],
            "<> answers BOTH non-equal carriers — the visible divergence \
             from > on this fixture"
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
            vec![VId(3), VId(6), VId(9), VId(12)],
            "without WHERE all four chains answer"
        );

        // Off-grammar edges: >= and the C-style alias on c (the nje.33/34
        // plants live on), and a WHERE on the incoming chain.
        for off_grammar in [
            "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k >= 1 RETURN c",
            "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c",
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
