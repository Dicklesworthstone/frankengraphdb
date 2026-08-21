//! **Incoming two-hop `WHERE c.k = 1` keeps matching far ends**
//! (`fgdb-w5-parsers-nje.40`).
//!
//! The far-end predicate crosses to the INCOMING chain: under
//! `(a)<-[:R]-(b)<-[:S]-(c)` the flow is `c-[:S]->b-[:R]->a`, so `c` is
//! the flow's ORIGIN and the predicate reads it there. Three reversed
//! chains whose origins differ only in `k`: equality keeps exactly the
//! `k = 1` origin, the keyless origin stays out (missing-is-OUT crosses
//! the direction too), and the unfiltered statement answers all three.
//! The OUTGOING spelling on this reversed fixture composes nothing — the
//! direction control an arrow-blind kernel fails by answering `[3]` for
//! both. Equality and `<>` with `RETURN c` graduate; every ordered comparator
//! on the incoming chain, the C-style alias, and the `RETURN a` projection
//! stay typed Parse. Inequality keeps only the non-equal origin.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_EQ: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN c";
const IN_NE: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN c";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const OUT_EQ: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_two_hop_far_end_equality_keeps_the_matching_origin() {
    let ((), report) = run_async_under_lab(0x40_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-two-hop-where-dst-{}",
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

        let filtered = db
            .execute_gql(IN_EQ, &bind)
            .expect("incoming far-end equality MATCH executes");
        assert_eq!(
            filtered,
            vec![VId(3)],
            "only the k=1 origin answers the incoming composed equality"
        );
        assert!(
            !filtered.contains(&VId(9)),
            "the no-k origin is OUT: missing-is-OUT crosses the direction too"
        );
        assert!(!filtered.contains(&VId(6)), "the k=9 origin fails equality");

        assert_eq!(
            db.execute_gql(IN_NE, &bind)
                .expect("incoming far-end inequality MATCH executes"),
            vec![VId(6)],
            "only the non-equal keyed origin answers the incoming inequality"
        );

        assert_eq!(
            db.execute_gql(IN_UNFILTERED, &bind)
                .expect("unfiltered incoming two-hop executes"),
            vec![VId(3), VId(6), VId(9)],
            "without WHERE all three reversed chains answer"
        );

        // The direction control: the OUTGOING spelling composes nothing on
        // this reversed fixture — an arrow-blind kernel answers [3] for
        // both spellings and fails here.
        assert!(
            db.execute_gql(OUT_EQ, &bind)
                .expect("the outgoing spelling still executes")
                .is_empty(),
            "no :S edge leaves an :R destination on the reversed fixture"
        );

        assert_eq!(
            db.execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k != 1 RETURN c",
                &bind,
            )
            .expect("nje.55 incoming far-end != aliases <>"),
            vec![VId(6)],
            "only the far end with k=2 differs from 1"
        );

        // Equality and inequality with RETURN c graduate. Every ordered
        // incoming comparator and the RETURN a projections stay Parse.
        for off_grammar in [
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN c",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN a",
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
