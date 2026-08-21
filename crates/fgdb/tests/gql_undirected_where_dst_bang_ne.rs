//! **Undirected `WHERE b.k != 1` keeps the unequal neighbour**
//! (`fgdb-w5-parsers-nje-63-lpg5`).
//!
//! The C-style inequality moves to the undirected NEIGHBOUR variable: on
//! `(a)-[:R]-(b)` every incident vertex can anchor `a`, so a predicate on
//! `b` filters which neighbours survive and `RETURN b` answers them.
//! Only the stored dests carry `k` (`2{k:1}`, `4{k:9}`, `6{no k}`), so
//! `b.k != 1` keeps the `k = 9` neighbour and bars the keyless one
//! (missing satisfies neither predicate — the complement-of-equality
//! cheat), while the equality sibling still keeps the `k = 1` neighbour.
//! The variable control runs on the SOURCE spelling (`a.k != 1`, grammar
//! since nje.61), and one refusal holds the grammar's edge: `!=` on the
//! undirected TWO-hop chain stays typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const UN_B_BANG_NE: &str = "MATCH (a)-[:R]-(b) WHERE b.k != 1 RETURN b";
const UN_B_EQ: &str = "MATCH (a)-[:R]-(b) WHERE b.k = 1 RETURN b";
const UN_A_BANG_NE: &str = "MATCH (a)-[:R]-(b) WHERE a.k != 1 RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn undirected_neighbour_bang_inequality_keeps_the_unequal_neighbour() {
    let ((), report) = run_async_under_lab(0x63_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-where-dst-bang-ne-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![(K, CanonicalScalar::Int(1))]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(9))]);
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
            .execute_gql(UN_B_BANG_NE, &bind)
            .expect("undirected neighbour C-style inequality MATCH executes");
        assert!(
            bang.contains(&VId(4)),
            "the k=9 neighbour answers: {bang:?}"
        );
        assert!(
            !bang.contains(&VId(6)),
            "the keyless neighbour is OUT, not trivially unequal: {bang:?}"
        );

        let equal = db
            .execute_gql(UN_B_EQ, &bind)
            .expect("the neighbour-equality sibling still executes");
        assert!(
            equal.contains(&VId(2)),
            "equality still keeps the k=1 neighbour: {equal:?}"
        );

        // The variable control: the SOURCE spelling (grammar since nje.61)
        // still executes — the k=9 anchor's neighbour answers there.
        let anchored = db
            .execute_gql(UN_A_BANG_NE, &bind)
            .expect("the anchor != spelling still executes (nje.61)");
        assert!(
            anchored.contains(&VId(3)),
            "the k=9 anchor's neighbour answers under a.k != 1: {anchored:?}"
        );
        assert!(
            !anchored.contains(&VId(5)),
            "the keyless component stays OUT under a.k != 1: {anchored:?}"
        );

        // The refusal: != on the undirected two-hop chain stays typed
        // Parse.
        let two_hop = db
            .execute_gql("MATCH (a)-[:R]-(b)-[:S]-(c) WHERE a.k != 1 RETURN c", &bind)
            .expect_err("!= on the undirected two-hop chain is off-grammar");
        assert!(
            matches!(two_hop, GqlError::Parse(_)),
            "expected the typed Parse refusal, got {two_hop:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
