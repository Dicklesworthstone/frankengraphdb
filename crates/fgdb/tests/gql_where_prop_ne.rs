//! **`WHERE a.k <> 1` keeps dests of non-matching sources**
//! (`fgdb-w5-parsers-nje.15`).
//!
//! The inequality twin of `gql_where_prop.rs`, on the same fixture: the
//! predicate keeps sources whose `k` is an Int AND not the literal — a
//! source with no `k` at all is OUT, not "trivially unequal", which is why
//! the propertyless `5→6` edge separates the two readings. Equality beside
//! it still answers its own dest, the unfiltered statement answers all
//! three, and the C-style `!=` spelling stays off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const NOT_EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b";
const EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn source_property_inequality_keeps_non_matching_sources() {
    let ((), report) = run_async_under_lab(0x49_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir =
            std::env::temp_dir().join(format!("fgdb-gql-where-prop-ne-{}", std::process::id()));
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
        assert_eq!(
            db.execute_gql(NOT_EQUAL, &bind)
                .expect("property-inequality MATCH executes"),
            vec![VId(4)],
            "only the k=9 source passes: k=1 fails the predicate and the \
             propertyless source is out, not trivially unequal"
        );
        assert_eq!(
            db.execute_gql(EQUAL, &bind)
                .expect("property-equality MATCH executes"),
            vec![VId(2)],
            "equality beside it still answers its own dest"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered MATCH executes"),
            vec![VId(2), VId(4), VId(6)]
        );

        // nje.58 sibling lock: the C-style != is grammar now and aliases
        // <> — the same [4] on this fixture, keyless source still OUT.
        assert_eq!(
            db.execute_gql("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b", &bind)
                .expect("nje.58 source != is grammar, not a Parse"),
            vec![VId(4)],
            "!= aliases <>: only the k=9 source passes"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
