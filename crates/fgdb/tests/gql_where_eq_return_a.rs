//! **`WHERE a = b RETURN a` answers the loop's source**
//! (`fgdb-w5-parsers-nje.6`).
//!
//! The source-projection twin of `gql_where_eq.rs`: on a fixture pairing an
//! ordinary edge with a self-loop, the equality predicate keeps only the
//! self-loop — so `RETURN a` answers the looping source and not the
//! ordinary edge's — while the bare statement beside it still answers both
//! sources. The pairing is what proves the predicate filters the MATCH and
//! not merely the projected column.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

#[test]
fn where_eq_return_a_is_the_loop_source() {
    let ((), report) = run_async_under_lab(0x4e_04, |root| async move {
        let commit = PurposeContexts::narrow_runtime_root(&root).commit();
        let dir = std::env::temp_dir().join(format!(
            "fgdb-where-eq-return-a-{}",
            std::process::id()
        ));
        let relation = RelationId(1);
        let mut db = Database::create(
            &commit,
            &dir,
            DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            ),
        )
        .await
        .expect("database creates");
        let mut seed = WriteBatch::new(relation);
        for vid in [1u128, 2, 5] {
            seed.create_vertex(VId(vid), vec![], vec![]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(5), VId(5), vec![]);
        db.write(&commit, seed).await.expect("fixture commits");

        let bind = RelationBind::new().with_relation("R", relation);
        let equal = db
            .execute_gql("MATCH (a)-[:R]->(b) WHERE a = b RETURN a", &bind)
            .expect("equality source projection executes");
        assert!(
            equal.contains(&VId(5)),
            "the self-loop's source answers: {equal:?}"
        );
        assert!(
            !equal.contains(&VId(1)),
            "the ordinary edge's source is filtered out: {equal:?}"
        );

        let bare = db
            .execute_gql("MATCH (a)-[:R]->(b) RETURN a", &bind)
            .expect("bare source projection executes");
        assert!(
            bare.contains(&VId(1)) && bare.contains(&VId(5)),
            "without WHERE both sources answer: {bare:?}"
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
