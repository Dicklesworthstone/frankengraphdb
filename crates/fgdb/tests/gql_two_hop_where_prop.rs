//! **Two-hop `WHERE a.k = 1` keeps the matching hop-2 dest**
//! (`fgdb-w5-parsers-nje.21`).
//!
//! The anchor predicate gates the whole composed path: three disjoint
//! `R∘S` chains whose anchors differ only in `k`, so equality keeps exactly
//! the `k = 1` chain's far end, inequality keeps the `k = 9` chain's under
//! the missing-is-OUT law (the no-`k` chain's far end must not leak in),
//! the unfiltered statement answers all three, and a WHERE on the incoming
//! two-hop chain stays off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const TWO_HOP_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c";
const TWO_HOP_NE: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k <> 1 RETURN c";
const TWO_HOP_UNFILTERED: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn two_hop_anchor_predicate_gates_the_composed_path() {
    let ((), report) = run_async_under_lab(0x52_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-two-hop-where-prop-{}",
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
        assert_eq!(
            db.execute_gql(TWO_HOP_EQ, &bind)
                .expect("two-hop anchor-equality MATCH executes"),
            vec![VId(3)],
            "only the k=1 anchor's chain composes to its far end"
        );
        assert_eq!(
            db.execute_gql(TWO_HOP_NE, &bind)
                .expect("two-hop anchor-inequality MATCH executes"),
            vec![VId(6)],
            "only the k=9 anchor's chain composes — the no-k anchor is OUT, \
             not trivially unequal, so 9 must not leak in"
        );
        assert_eq!(
            db.execute_gql(TWO_HOP_UNFILTERED, &bind)
                .expect("unfiltered two-hop MATCH executes"),
            vec![VId(3), VId(6), VId(9)]
        );

        // WHERE on the incoming two-hop chain landed after this suite
        // froze. It is grammar now, and on this OUTGOING fixture the
        // incoming spelling composes nothing (no :S edge arrives at an :R
        // source), so it answers empty.
        let incoming = db
            .execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN c",
                &bind,
            )
            .expect("WHERE on the incoming two-hop chain executes");
        assert!(
            incoming.is_empty(),
            "the incoming spelling composes nothing here: {incoming:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
