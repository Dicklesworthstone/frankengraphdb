//! **`WHERE a.k > 1` is plan identity** (`fgdb-w5-parsers-nje.22`).
//!
//! Four certificates at ONE database and ONE sequence, so only the plan can
//! part any pair: the ordered comparison differs whole-struct from the
//! equality and inequality spellings of the same property literal AND from
//! the unfiltered statement — a certificate hashing the property pair
//! without the operator collides the first three — plus the re-mint control
//! proving the digest moves only when the plan does. No hex goldens.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

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
fn property_greater_than_changes_the_plan_certificate() {
    let ((), report) = run_async_under_lab(0x54_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-prop-gt-cert-{}",
            std::process::id()
        ));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("seed commits");
        let bind = RelationBind::new()
            .with_relation("R", R)
            .with_property("k", K);

        let greater = db
            .gql_plan_certificate(GREATER, &bind)
            .expect("WHERE a.k > 1 certifies");
        let equal = db
            .gql_plan_certificate(EQUAL, &bind)
            .expect("WHERE a.k = 1 certifies");
        let not_equal = db
            .gql_plan_certificate(NOT_EQUAL, &bind)
            .expect("WHERE a.k <> 1 certifies");
        let unfiltered = db
            .gql_plan_certificate(UNFILTERED, &bind)
            .expect("the unfiltered MATCH certifies");

        // One database, one sequence: only the plan separates any pair.
        assert_eq!(greater.snapshot_seq, equal.snapshot_seq);
        assert_eq!(greater.snapshot_seq, not_equal.snapshot_seq);
        assert_eq!(greater.snapshot_seq, unfiltered.snapshot_seq);

        assert_ne!(
            greater, equal,
            "the operator is plan identity: > and = on one literal must not \
             collide"
        );
        assert_ne!(
            greater, not_equal,
            "> and <> on one literal must not collide either"
        );
        assert_ne!(
            greater, unfiltered,
            "the predicate's presence is plan identity"
        );

        // The re-mint control: the digest moves only when the plan does.
        assert_eq!(
            db.gql_plan_certificate(GREATER, &bind)
                .expect("WHERE a.k > 1 re-certifies"),
            greater,
            "same plan at the same frontier re-mints byte-identically"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
