//! **Undirected `WHERE a.k = 1` keeps the matching neighbour**
//! (`fgdb-w5-parsers-nje.20`).
//!
//! On the undirected spelling every incident vertex can anchor `a`, so the
//! property predicate decides WHICH anchorings survive: only a binding
//! whose `a` carries the property answers, and its `b` rows are that
//! anchor's neighbours. Equality keeps the `k = 1` vertex's neighbour and
//! nothing across the other components; inequality keeps the `k = 9`
//! vertex's neighbour with the no-`k` component barred by the
//! missing-is-OUT law; and a WHERE on the undirected two-hop chain stays
//! off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const UN_EQ: &str = "MATCH (a)-[:R]-(b) WHERE a.k = 1 RETURN b";
const UN_NE: &str = "MATCH (a)-[:R]-(b) WHERE a.k <> 1 RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn undirected_property_predicates_filter_the_anchorings() {
    let ((), report) = run_async_under_lab(0x51_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-undirected-where-prop-{}",
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
        let equal = db
            .execute_gql(UN_EQ, &bind)
            .expect("undirected property-equality MATCH executes");
        assert!(
            equal.contains(&VId(2)),
            "the k=1 vertex's neighbour answers: {equal:?}"
        );
        assert!(
            !equal.contains(&VId(4)) && !equal.contains(&VId(6)),
            "no other component's vertices leak through the predicate: {equal:?}"
        );

        let not_equal = db
            .execute_gql(UN_NE, &bind)
            .expect("undirected property-inequality MATCH executes");
        assert!(
            not_equal.contains(&VId(4)),
            "the k=9 vertex's neighbour answers: {not_equal:?}"
        );
        assert!(
            !not_equal.contains(&VId(6)),
            "the no-k component is OUT, not trivially unequal: {not_equal:?}"
        );

        let two_hop = db
            .execute_gql(
                "MATCH (a)-[:R]-(b)-[:S]-(c) WHERE a.k = 1 RETURN c",
                &bind,
            )
            .expect_err("WHERE on the undirected two-hop chain is off-grammar");
        assert!(
            matches!(two_hop, GqlError::Parse(_)),
            "expected the typed Parse refusal, got {two_hop:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
