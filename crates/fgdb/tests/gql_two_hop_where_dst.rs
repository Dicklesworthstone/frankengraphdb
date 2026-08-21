//! **Two-hop `WHERE c.k = 1` keeps the matching far ends**
//! (`fgdb-w5-parsers-nje.33`).
//!
//! The predicate reaches the composed path's FAR END: three disjoint
//! `R∘S` chains whose far ends differ only in `k`, so equality keeps
//! exactly the `k = 1` chain's `c`, the no-`k` far end stays out
//! (missing-is-OUT holds at hop 2's end too), and the unfiltered
//! statement answers all three. The `k = 1` chain carries the property on
//! BOTH its anchor and its far end, so the nje.21 SOURCE-side statement is
//! re-pinned at the same `[3]` — while the other two chains' anchors carry
//! no `k`, so a far-end filter mislabeled source-side (or vice versa)
//! diverges on them at hop after hop. The C-style `!=` on `c` and a WHERE
//! on the incoming two-hop chain stay off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const DST_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c";
const SRC_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn two_hop_far_end_predicate_keeps_the_matching_chain() {
    let ((), report) = run_async_under_lab(0x33_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-dst-{}",
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
            .execute_gql(DST_EQ, &bind)
            .expect("far-end equality MATCH executes");
        assert_eq!(
            filtered,
            vec![VId(3)],
            "only the k=1 far end answers — the k=9 chain fails equality"
        );
        assert!(
            !filtered.contains(&VId(9)),
            "the no-k far end is OUT: missing-is-OUT holds at hop 2's end too"
        );

        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three chains answer"
        );

        // The nje.21 SOURCE-side statement, unmoved: the k=1 chain carries
        // the property on its anchor too, and the other two anchors are
        // keyless, so the source filter also answers exactly [3] — via a
        // DIFFERENT vertex than the far-end filter read.
        assert_eq!(
            db.execute_gql(SRC_EQ, &bind)
                .expect("the nje.21 source-side statement still executes"),
            vec![VId(3)],
            "nje.21 unmoved: the anchor filter keeps the same chain"
        );

        // Off-grammar edges: the C-style alias on c, and a WHERE on the
        // incoming two-hop chain.
        for off_grammar in [
            "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN a",
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
