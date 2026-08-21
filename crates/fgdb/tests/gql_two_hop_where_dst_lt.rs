//! **Two-hop `WHERE c.k < 1` keeps lesser far ends**
//! (`fgdb-w5-parsers-nje.37`).
//!
//! The strictly-less comparator at the composed path's far end, on the
//! four-chain fixture where every neighbouring cheat has its witness: the
//! boundary far end (`k = 1`, `1 < 1` false) convicts a `<=` reading, the
//! `k = 9` far end convicts a flipped comparator, the `k = 0` survivor
//! separates `<` from nothing-at-all, and the keyless far end satisfies
//! no ordered comparator. The `>`, `=`, and `<>` siblings are re-pinned
//! beside it — `<` answering `[12]` while `<>` answers `[6, 12]` and `>`
//! answers `[6]` makes all three predicates pairwise distinct ON THIS
//! FIXTURE — and three refusals hold the edges: `<=` and the C-style `!=`
//! on `c` stay Parse, and a WHERE on the incoming two-hop chain stays
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
const DST_LT: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k < 1 RETURN c";
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
fn two_hop_far_end_less_than_keeps_only_the_lesser_chain() {
    let ((), report) = run_async_under_lab(0x37_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-dst-lt-{}",
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

        let lesser = db
            .execute_gql(DST_LT, &bind)
            .expect("far-end less-than MATCH executes");
        assert_eq!(
            lesser,
            vec![VId(12)],
            "only the k=0 far end is strictly less"
        );
        assert!(
            !lesser.contains(&VId(3)),
            "the boundary far end fails: 1 < 1 is false — a <= cheat \
             answers 3 and fails here"
        );
        assert!(
            !lesser.contains(&VId(9)),
            "the no-k far end satisfies no ordered comparator"
        );
        assert!(
            !lesser.contains(&VId(6)),
            "9 is not less than 1 — a flipped comparator answers 6"
        );

        assert_eq!(
            db.execute_gql(DST_GT, &bind)
                .expect("the greater sibling still executes"),
            vec![VId(6)],
            "nje.36 unmoved: > and < answer disjoint singletons here"
        );
        assert_eq!(
            db.execute_gql(DST_EQ, &bind)
                .expect("the equality sibling still executes"),
            vec![VId(3)],
            "nje.33 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(DST_NE, &bind)
                .expect("the inequality sibling still executes"),
            vec![VId(6), VId(12)],
            "nje.34 unmoved: <> is the union of < and > on this fixture"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered two-hop executes"),
            vec![VId(3), VId(6), VId(9), VId(12)],
            "without WHERE all four chains answer"
        );

        // Retargeted by fgdb-w5-parsers-nje.39: hop-2 c.k <= graduated to
        // grammar, so it moves from the Parse list to a positive pin on
        // this same fixture — moved, not weakened.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <= 1 RETURN c",
                &bind,
            )
            .expect("hop-2 <= is grammar since nje.39"),
            vec![VId(3), VId(12)],
            "the boundary and below-boundary far ends answer <="
        );

        // Off-grammar edges: the C-style alias on c, and a WHERE on the
        // incoming chain.
        // fgdb-tdrh sibling lock: the outgoing far-end != is grammar now
        // (parser 274f4d6a) and aliases <> — on this four-chain fixture
        // both the k=9 and k=0 far ends differ from 1.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c",
                &bind
            )
            .expect("fgdb-tdrh outgoing far-end != is grammar, not a Parse"),
            vec![VId(6), VId(12)],
            "!= aliases <>: k=9 and k=0 both differ from 1; no-k stays OUT"
        );

        for off_grammar in [
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 1 RETURN a",
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
