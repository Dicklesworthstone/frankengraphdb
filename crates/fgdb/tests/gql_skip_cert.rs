//! **`SKIP` is plan identity** (`fgdb-w5-parsers-nje.13`).
//!
//! Three separations at ONE database and ONE sequence, so only the plan can
//! part any pair: `SKIP 1` differs whole-struct from the bare statement,
//! `SKIP 0` differs from no-`SKIP` (an explicit zero offset is `Some(0)`,
//! not the absent clause), and `SKIP 1 LIMIT 1` differs from `SKIP 1` — plus
//! the re-mint control proving the digest moves only when the plan does.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const PLAIN: &str = "MATCH (a)-[:R]->(b) RETURN b";
const SKIP_ONE: &str = "MATCH (a)-[:R]->(b) RETURN b SKIP 1";
const SKIP_ZERO: &str = "MATCH (a)-[:R]->(b) RETURN b SKIP 0";
const SKIP_ONE_LIMIT_ONE: &str = "MATCH (a)-[:R]->(b) RETURN b SKIP 1 LIMIT 1";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn skip_changes_the_plan_certificate() {
    let ((), report) = run_async_under_lab(0x47_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!("fgdb-gql-skip-cert-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("seed commits");
        let bind = RelationBind::new().with_relation("R", R);

        let plain = db
            .gql_plan_certificate(PLAIN, &bind)
            .expect("the bare MATCH certifies");
        let skip_one = db
            .gql_plan_certificate(SKIP_ONE, &bind)
            .expect("SKIP 1 certifies");
        let skip_zero = db
            .gql_plan_certificate(SKIP_ZERO, &bind)
            .expect("SKIP 0 certifies");
        let skip_one_limit_one = db
            .gql_plan_certificate(SKIP_ONE_LIMIT_ONE, &bind)
            .expect("SKIP 1 LIMIT 1 certifies");

        // One database, one sequence: only the plan separates any pair.
        assert_eq!(plain.snapshot_seq, skip_one.snapshot_seq);
        assert_eq!(plain.snapshot_seq, skip_zero.snapshot_seq);
        assert_eq!(plain.snapshot_seq, skip_one_limit_one.snapshot_seq);

        assert_ne!(
            skip_one, plain,
            "SKIP 1 and the bare statement must not collide"
        );
        assert_ne!(
            skip_zero, plain,
            "an explicit SKIP 0 is Some(0), not the absent clause — the \
             certificates must differ"
        );
        assert_ne!(
            skip_one_limit_one, skip_one,
            "adding LIMIT to a skipped plan changes its identity"
        );

        // The re-mint control: the digest moves only when the plan does.
        assert_eq!(
            db.gql_plan_certificate(SKIP_ONE, &bind)
                .expect("SKIP 1 re-certifies"),
            skip_one,
            "same plan at the same frontier re-mints byte-identically"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
