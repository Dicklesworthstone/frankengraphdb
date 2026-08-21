//! **`WHERE b.k <> 1` keeps sources of non-equal dests**
//! (`fgdb-w5-parsers-nje.16`).
//!
//! The destination twin of `gql_where_prop_ne.rs`: the inequality binds the
//! DEST variable, so `RETURN a` answers the sources whose destination
//! carries `k` as an Int not equal to the literal. The planted negative is
//! the propertyless dest `6`: an executor that treats a missing `k` as
//! "trivially unequal" answers `[3, 5]` and fails the exact-equality here —
//! missing is OUT. Equality beside it, the unfiltered statement, and the
//! off-grammar C-style `!=` complete the frame.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const NOT_EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE b.k <> 1 RETURN a";
const EQUAL: &str = "MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a";
const UNFILTERED: &str = "MATCH (a)-[:R]->(b) RETURN a";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn destination_property_inequality_keeps_non_matching_sources() {
    let ((), report) = run_async_under_lab(0x4d_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-dst-prop-ne-{}",
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
        let filtered = db
            .execute_gql(NOT_EQUAL, &bind)
            .expect("destination-inequality MATCH executes");
        assert_eq!(
            filtered,
            vec![VId(3)],
            "only the k=9 dest's source passes — an executor treating the \
             missing-k dest as trivially unequal answers [3, 5] and fails here"
        );
        assert!(
            !filtered.contains(&VId(5)),
            "the propertyless dest's source is OUT, not trivially unequal"
        );
        assert_eq!(
            db.execute_gql(EQUAL, &bind)
                .expect("destination-equality MATCH executes"),
            vec![VId(1)],
            "equality beside it still answers its own source"
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered MATCH executes"),
            vec![VId(1), VId(3), VId(5)]
        );

        let c_style = db
            .execute_gql("MATCH (a)-[:R]->(b) WHERE b.k != 1 RETURN a", &bind)
            .expect_err("the C-style != spelling is outside the grammar");
        assert!(
            matches!(c_style, GqlError::Parse(_)),
            "expected the typed Parse refusal, got {c_style:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
