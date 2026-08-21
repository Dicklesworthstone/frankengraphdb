//! **Incoming `WHERE b.k != 1` aliases `<>` on the writers**
//! (`fgdb-w5-parsers-nje-60-li19`).
//!
//! The C-style spelling of `gql_incoming_where_prop.rs`'s inequality: on
//! the incoming spelling `(a)<-[:R]-(b)` the parser normalizes `b` to the
//! edge-flow SOURCE, so `!=` filters the writers exactly like `<>` and
//! `RETURN a` answers their targets — the two spellings are asserted
//! EQUAL against each other, not just against the literal list. The
//! inequality keeps the `k = 9` writer's target under the missing-is-OUT
//! law (the no-`k` writer's target must not leak in), the equality
//! sibling still answers `[2]`, and the unfiltered statement answers all
//! three. One refusal holds the grammar's edge this slice: `!=` on the
//! mid variable of the incoming TWO-hop chain stays typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const IN_BANG_NE: &str = "MATCH (a)<-[:R]-(b) WHERE b.k != 1 RETURN a";
const IN_NE: &str = "MATCH (a)<-[:R]-(b) WHERE b.k <> 1 RETURN a";
const IN_EQ: &str = "MATCH (a)<-[:R]-(b) WHERE b.k = 1 RETURN a";
const IN_UNFILTERED: &str = "MATCH (a)<-[:R]-(b) RETURN a";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn incoming_bang_inequality_aliases_the_angle_spelling() {
    let ((), report) = run_async_under_lab(0x60_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-incoming-where-prop-bang-ne-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
        seed.create_vertex(VId(4), vec![], vec![]);
        seed.create_vertex(VId(5), vec![], vec![]);
        seed.create_vertex(VId(6), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        seed.add_edge(EId(12), VId(5), VId(6), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K);

        let bang = db
            .execute_gql(IN_BANG_NE, &bind)
            .expect("incoming C-style inequality MATCH executes");
        assert_eq!(
            bang,
            vec![VId(4)],
            "only the k=9 writer's target answers — the no-k writer is OUT, \
             not trivially unequal, so 6 must not leak in"
        );
        assert!(
            !bang.contains(&VId(6)),
            "the no-k writer's target is OUT, not trivially unequal"
        );
        assert_eq!(
            bang,
            db.execute_gql(IN_NE, &bind)
                .expect("the <> sibling still executes"),
            "!= and <> are one comparator in two spellings on the writers"
        );

        assert_eq!(
            db.execute_gql(IN_EQ, &bind)
                .expect("the equality sibling still executes"),
            vec![VId(2)],
            "nje.19 unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(IN_UNFILTERED, &bind)
                .expect("unfiltered incoming MATCH executes"),
            vec![VId(2), VId(4), VId(6)],
            "without WHERE all three writers' targets answer"
        );

        // The refusal this slice: != on the mid variable of the incoming
        // two-hop chain stays typed Parse.
        let two_hop = db
            .execute_gql(
                "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE b.k != 1 RETURN a",
                &bind,
            )
            .expect_err("!= on the incoming two-hop mid variable is off-grammar");
        assert!(
            matches!(two_hop, GqlError::Parse(_)),
            "expected the typed Parse refusal, got {two_hop:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
