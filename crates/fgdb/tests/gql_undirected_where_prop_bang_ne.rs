//! **Undirected `WHERE a.k != 1` aliases `<>` on the anchorings**
//! (`fgdb-w5-parsers-nje-61-56vl`).
//!
//! The C-style spelling of `gql_undirected_where_prop.rs`'s inequality:
//! on the undirected spelling every incident vertex can anchor `a`, so
//! `!=` decides WHICH anchorings survive exactly like `<>` — the two
//! spellings are asserted EQUAL against each other, not just against the
//! contained/not-contained pins. The inequality keeps the `k = 9`
//! vertex's neighbour with the no-`k` component barred by the
//! missing-is-OUT law, the equality sibling still keeps the `k = 1`
//! vertex's neighbour, and one refusal holds the grammar's edge this
//! slice: `!=` on the undirected TWO-hop chain stays typed Parse.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const UN_BANG_NE: &str = "MATCH (a)-[:R]-(b) WHERE a.k != 1 RETURN b";
const UN_NE: &str = "MATCH (a)-[:R]-(b) WHERE a.k <> 1 RETURN b";
const UN_EQ: &str = "MATCH (a)-[:R]-(b) WHERE a.k = 1 RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn undirected_bang_inequality_aliases_the_angle_spelling() {
    let ((), report) = run_async_under_lab(0x61_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-where-prop-bang-ne-{}",
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
            .execute_gql(UN_BANG_NE, &bind)
            .expect("undirected C-style inequality MATCH executes");
        assert!(
            bang.contains(&VId(4)),
            "the k=9 vertex's neighbour answers: {bang:?}"
        );
        assert!(
            !bang.contains(&VId(6)),
            "the no-k component is OUT, not trivially unequal: {bang:?}"
        );
        assert_eq!(
            bang,
            db.execute_gql(UN_NE, &bind)
                .expect("the <> sibling still executes"),
            "!= and <> are one comparator in two spellings on the anchorings"
        );

        let equal = db
            .execute_gql(UN_EQ, &bind)
            .expect("the equality sibling still executes");
        assert!(
            equal.contains(&VId(2)),
            "nje.20 unmoved: the k=1 vertex's neighbour answers: {equal:?}"
        );

        // The refusal this slice: != on the undirected two-hop chain
        // stays typed Parse.
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
