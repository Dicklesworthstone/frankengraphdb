//! **`WHERE a = b` is plan identity** (`fgdb-w5-parsers-nje.6`).
//!
//! The equality predicate is hashed into the plan certificate, so at ONE
//! database and ONE sequence the `WHERE a = b` spelling and the bare
//! spelling of the same MATCH mint different digests — and re-minting the
//! equality plan is byte-identical. No digest hex goldens: the laws are
//! inequality across plans and equality across re-mints, never a pinned
//! byte string.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const WHERE_EQ: &str = "MATCH (a)-[:R]->(b) WHERE a = b RETURN b";
const BARE: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn where_eq_plan_certificate_is_distinct_and_deterministic() {
    let ((), report) = run_async_under_lab(0x39_06, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!("fgdb-where-eq-cert-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("seed commits");
        let bind = RelationBind::new().with_relation("R", R);

        let with_eq = db
            .gql_plan_certificate(WHERE_EQ, &bind)
            .expect("WHERE a = b certifies");
        let bare = db
            .gql_plan_certificate(BARE, &bind)
            .expect("the bare MATCH certifies");

        assert_eq!(
            with_eq.snapshot_seq, bare.snapshot_seq,
            "one database, one sequence — only the plan separates the pair"
        );
        assert_ne!(
            with_eq.digest, bare.digest,
            "the equality predicate is hashed: WHERE a = b and the bare \
             spelling must not collide"
        );
        assert_eq!(
            db.gql_plan_certificate(WHERE_EQ, &bind)
                .expect("WHERE a = b re-certifies"),
            with_eq,
            "same equality plan at the same frontier re-mints byte-identically"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
