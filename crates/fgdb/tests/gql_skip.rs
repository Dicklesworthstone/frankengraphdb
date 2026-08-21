//! **`SKIP n` drops the smallest CGSE rows** (`fgdb-w5-parsers-nje.13`).
//!
//! The offset twin of `gql_limit.rs`, on the same fixture: SKIP applies to
//! the CGSE-ordered row set (destinations ascending, deduplicated), so
//! `SKIP 1` drops exactly the smallest destination, composes with `LIMIT`
//! as offset-then-truncate, and `SKIP 0` answers precisely the un-skipped
//! rows — while a `SKIP` with no integer stays off-grammar.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const SKIP_ONE: &str = "MATCH (a)-[:R]->(b) RETURN b SKIP 1";
const SKIP_ONE_LIMIT_ONE: &str = "MATCH (a)-[:R]->(b) RETURN b SKIP 1 LIMIT 1";
const SKIP_ZERO: &str = "MATCH (a)-[:R]->(b) RETURN b SKIP 0";
const PLAIN: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

#[test]
fn skip_drops_the_smallest_cgse_destination() {
    let ((), report) = run_async_under_lab(0x46_01, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!("fgdb-gql-skip-{}", std::process::id()));
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        for vid in [VId(1), VId(2), VId(3), VId(4), VId(6)] {
            seed.create_vertex(vid, vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(1), VId(4), vec![]);
        seed.add_edge(EId(12), VId(3), VId(6), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new().with_relation("R", R);
        assert_eq!(
            db.execute_gql(SKIP_ONE, &bind)
                .expect("SKIP 1 MATCH executes"),
            vec![VId(4), VId(6)],
            "SKIP 1 drops exactly the CGSE-smallest destination"
        );
        assert_eq!(
            db.execute_gql(SKIP_ONE_LIMIT_ONE, &bind)
                .expect("SKIP 1 LIMIT 1 MATCH executes"),
            vec![VId(4)],
            "SKIP then LIMIT is offset-then-truncate over the same ordering"
        );
        assert_eq!(
            db.execute_gql(SKIP_ZERO, &bind)
                .expect("SKIP 0 MATCH executes"),
            vec![VId(2), VId(4), VId(6)],
            "SKIP 0 answers the un-skipped rows"
        );
        assert_eq!(
            db.execute_gql(PLAIN, &bind).expect("plain MATCH executes"),
            vec![VId(2), VId(4), VId(6)],
            "no SKIP is the same answer"
        );

        let missing = db
            .execute_gql("MATCH (a)-[:R]->(b) RETURN b SKIP", &bind)
            .expect_err("SKIP with no integer is off-grammar");
        assert!(matches!(missing, GqlError::Parse(_)));
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
