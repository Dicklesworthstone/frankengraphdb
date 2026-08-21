//! **`WHERE a.k <> 1` is plan identity** (`fgdb-w5-parsers-nje.15`).
//!
//! Three certificates at ONE database and ONE sequence, so only the plan
//! can part any pair: the inequality predicate differs whole-struct from
//! the equality spelling of the same property literal AND from the
//! unfiltered statement — a certificate that hashed only the property pair
//! without the operator would collide the first pair — plus the re-mint
//! control proving the digest moves only when the plan does. No hex
//! goldens.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

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
fn property_inequality_changes_the_plan_certificate() {
    let ((), report) = run_async_under_lab(0x4a_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-gql-where-prop-ne-cert-{}",
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

        let not_equal = db
            .gql_plan_certificate(NOT_EQUAL, &bind)
            .expect("WHERE a.k <> 1 certifies");
        let equal = db
            .gql_plan_certificate(EQUAL, &bind)
            .expect("WHERE a.k = 1 certifies");
        let unfiltered = db
            .gql_plan_certificate(UNFILTERED, &bind)
            .expect("the unfiltered MATCH certifies");

        // One database, one sequence: only the plan separates any pair.
        assert_eq!(not_equal.snapshot_seq, equal.snapshot_seq);
        assert_eq!(not_equal.snapshot_seq, unfiltered.snapshot_seq);

        assert_ne!(
            not_equal, equal,
            "the operator is plan identity: a certificate hashing only the \
             property pair would collide these"
        );
        assert_ne!(
            not_equal, unfiltered,
            "the predicate's presence is plan identity"
        );

        // The re-mint control: the digest moves only when the plan does.
        assert_eq!(
            db.gql_plan_certificate(NOT_EQUAL, &bind)
                .expect("WHERE a.k <> 1 re-certifies"),
            not_equal,
            "same plan at the same frontier re-mints byte-identically"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
