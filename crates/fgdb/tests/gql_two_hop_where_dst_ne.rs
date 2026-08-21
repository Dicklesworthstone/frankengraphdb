//! **Two-hop `WHERE c.k <> 1` keeps unequal far ends**
//! (`fgdb-w5-parsers-nje.34`).
//!
//! The inequality reaches the composed path's far end: on the nje.33
//! fixture, `<> 1` keeps exactly the `k = 9` chain's far end — the no-`k`
//! far end must not leak in, because missing `k` satisfies NEITHER
//! predicate even at hop 2's end (a complement-of-equality kernel answers
//! `[6, 9]` and fails). The equality sibling and the unfiltered statement
//! are re-pinned beside it, and two refusals hold the grammar's edges:
//! the C-style `!=` alias on `c` stays Parse (the sibling suite's plant,
//! honored here too), and a WHERE on the incoming two-hop chain stays
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
const DST_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c";
const DST_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn two_hop_far_end_inequality_keeps_the_unequal_chain() {
    let ((), report) = run_async_under_lab(0x34_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-dst-ne-{}",
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

        let filtered = db
            .execute_gql(DST_NE, &bind)
            .expect("far-end inequality MATCH executes");
        assert_eq!(
            filtered,
            vec![VId(6)],
            "only the k=9 far end answers — the k=1 chain fails the inequality"
        );
        assert!(
            !filtered.contains(&VId(9)),
            "the no-k far end satisfies NEITHER predicate — a \
             complement-of-equality kernel answers it and is wrong"
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

        // Off-grammar edges: the C-style alias on c (the sibling suite's
        // plant, kept alive here too) and a WHERE on the incoming chain.
        // fgdb-tdrh sibling lock: the outgoing far-end != is grammar now
        // (parser 274f4d6a) and aliases <> — only the k=9 far end differs
        // from 1 on this fixture.
        assert_eq!(
            db.execute_gql(
                "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c",
                &bind
            )
            .expect("fgdb-tdrh outgoing far-end != is grammar, not a Parse"),
            vec![VId(6)],
            "!= aliases <>: the k=1 chain fails and the no-k far end stays OUT"
        );

        for off_grammar in [
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN a",
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
