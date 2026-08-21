//! **`WHERE a.k < 1` keeps dests of lesser sources**
//! (`fgdb-w5-parsers-nje.23`).
//!
//! The less-than face completes the ordered pair on the `{1, 9, 0, missing}`
//! source spread: `<` keeps only the `k = 0` source's dest, `>` beside it
//! keeps only the `k = 9` source's — the two must partition `<>`'s answer —
//! equality keeps the literal's own, and the missing-`k` source stays OUT of
//! every comparison (no `k` is not lesser, not greater, not unequal). The
//! unlanded `<=` spelling stays off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const LESSER: &str = "MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b";
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
fn source_property_less_than_keeps_lesser_sources() {
    let ((), report) = run_async_under_lab(0x55_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-prop-lt-{}",
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
        let lesser = db
            .execute_gql(LESSER, &bind)
            .expect("property less-than MATCH executes");
        assert_eq!(
            lesser,
            vec![VId(6)],
            "only the k=0 source is lesser than the literal"
        );
        assert!(
            !lesser.contains(&VId(8)),
            "the missing-k source is OUT under <, not vacuously lesser"
        );
        assert_eq!(
            db.execute_gql(GREATER, &bind)
                .expect("property greater-than MATCH executes"),
            vec![VId(4)],
            "< and > together partition <>'s answer"
        );
        assert_eq!(
            db.execute_gql(EQUAL, &bind)
                .expect("property-equality MATCH executes"),
            vec![VId(2)]
        );
        assert_eq!(
            db.execute_gql(NOT_EQUAL, &bind)
                .expect("property-inequality MATCH executes"),
            vec![VId(4), VId(6)]
        );
        assert_eq!(
            db.execute_gql(UNFILTERED, &bind)
                .expect("unfiltered MATCH executes"),
            vec![VId(2), VId(4), VId(6), VId(8)]
        );

        // nje.58 sibling lock: the C-style != is grammar now and aliases
        // <> — on this four-source spread both the k=9 and k=0 sources
        // differ from 1, and the keyless source stays OUT.
        assert_eq!(
            db.execute_gql("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b", &bind)
                .expect("nje.58 source != is grammar, not a Parse"),
            vec![VId(4), VId(6)],
            "!= aliases <>: k=9 and k=0 both differ from 1"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
