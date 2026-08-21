//! **`WHERE a.k > 1` keeps dests of greater sources**
//! (`fgdb-w5-parsers-nje.22`).
//!
//! The ordered comparison joins `=` and `<>` in the bounded WHERE grammar:
//! on the `{1, 9, 0, missing}` source spread, `>` keeps only the `k = 9`
//! source's dest — the `k = 0` source separates `>` from `<>`, and the
//! missing-`k` source stays OUT (a vertex with no `k` is not "greater",
//! not "unequal", not anything). Equality and inequality beside it keep
//! their landed answers. `<` is nje.23 grammar (sibling lock `[6]`);
//! `>=` stays off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const GREATER: &str = "MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b";
const EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";
const NOT_EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn source_property_greater_than_keeps_greater_sources() {
    let ((), report) = run_async_under_lab(0x53_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-prop-gt-{}",
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
        seed.create_vertex(VId(5), vec![], vec![(K, CanonicalScalar::Int(0))]);
        seed.create_vertex(VId(6), vec![], vec![]);
        seed.create_vertex(VId(7), vec![], vec![]);
        seed.create_vertex(VId(8), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(3), VId(4), vec![]);
        seed.add_edge(EId(12), VId(5), VId(6), vec![]);
        seed.add_edge(EId(13), VId(7), VId(8), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K);
        let greater = db
            .execute_gql(GREATER, &bind)
            .expect("property greater-than MATCH executes");
        assert_eq!(
            greater,
            vec![VId(4)],
            "only the k=9 source is greater — k=0 separates > from <>"
        );
        assert!(
            !greater.contains(&VId(8)),
            "the missing-k source is OUT under >, not vacuously greater"
        );
        assert_eq!(
            db.execute_gql(EQUAL, &bind)
                .expect("property-equality MATCH executes"),
            vec![VId(2)]
        );
        assert_eq!(
            db.execute_gql(NOT_EQUAL, &bind)
                .expect("property-inequality MATCH executes"),
            vec![VId(4), VId(6)],
            "<> keeps both non-1 Int sources, which > must not"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered MATCH executes"),
            vec![VId(2), VId(4), VId(6), VId(8)]
        );

        let ge = db
            // Retargeted by fgdb-w5-parsers-nje.26: the SOURCE >= spelling
            // graduated to grammar, so this planted negative now guards the
            // DEST spelling, which is still outside the bounded grammar
            // this slice — the assertion moved, it did not weaken.
            .execute_gql("MATCH (a)-[:R]->(b) WHERE b.k >= 1 RETURN a", &bind)
            .expect_err("the dest >= spelling is outside the bounded grammar");
        assert!(matches!(ge, GqlError::Parse(_)));
        assert_eq!(
            db.execute_gql("MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b", &bind)
                .expect("nje.23 < is grammar, not a Parse"),
            vec![VId(6)],
            "nje.23 sibling lock: k<1 keeps dest 6"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
