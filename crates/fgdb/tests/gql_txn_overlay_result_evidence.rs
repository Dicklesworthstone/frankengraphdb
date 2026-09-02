use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x83; 32],
    )
}

fn test_directory() -> std::io::Result<std::path::PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    loop {
        let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = base.join(format!("fgdb-txn-overlay-evidence-{ordinal}"));
        match std::fs::create_dir(&parent) {
            Ok(()) => return Ok(parent.join("database")),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn staged_vertex_and_edge(vid: VId, eid: EId) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(vid, vec![], vec![]);
    batch.add_edge(eid, VId(1), vid, vec![]);
    batch
}

#[test]
fn exact_overlay_result_evidence_binds_canonical_effect_and_ordered_rows() {
    let ((), report) = run_async_under_lab(0x44_17, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let directory = test_directory().expect("unique test directory");
        let mut database = Database::create(&commit, &directory, keys())
            .await
            .expect("database creates");

        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![], vec![]);
        initial.create_vertex(VId(2), vec![], vec![]);
        initial.add_edge(EId(10), VId(1), VId(2), vec![]);
        database
            .write(&commit, initial)
            .await
            .expect("fixture commits");

        let query = database
            .prepare_gql_query(QUERY, &RelationBind::new().with_relation("R", R))
            .expect("query prepares");
        let mut first = database.begin(&txn_cx).expect("first transaction begins");
        let mut equivalent = database
            .begin(&txn_cx)
            .expect("equivalent transaction begins");

        let staged = staged_vertex_and_edge(VId(3), EId(11));
        first
            .write(&mut database, staged.clone())
            .expect("first overlay stages");
        equivalent
            .write(&mut database, staged)
            .expect("equivalent overlay stages");

        let (rows, plan_certificate, certificate) = first
            .execute_prepared_query_with_overlay_result_certificate(&database, &query)
            .expect("exact overlay evidence issues");
        assert_eq!(rows, vec![VId(2), VId(3)]);
        assert!(plan_certificate.verifies_at(query.plan(), first.basis()));
        assert!(
            first
                .verifies_prepared_query_overlay_result(&query, &rows, &certificate)
                .expect("first transaction verifies")
        );
        assert!(
            equivalent
                .verifies_prepared_query_overlay_result(&query, &rows, &certificate)
                .expect("equal canonical overlay verifies")
        );

        assert!(
            !first
                .verifies_prepared_query_overlay_result(&query, &[VId(3), VId(2)], &certificate,)
                .expect("row reorder is a clean mismatch")
        );
        assert!(
            !first
                .verifies_prepared_query_overlay_result(&query, &[VId(2), VId(4)], &certificate,)
                .expect("row replacement is a clean mismatch")
        );
        assert!(
            !first
                .verifies_prepared_query_overlay_result(&query, &[VId(2)], &certificate)
                .expect("row truncation is a clean mismatch")
        );

        equivalent
            .write(&mut database, staged_vertex_and_edge(VId(4), EId(12)))
            .expect("later staged mutation succeeds");
        assert!(
            !equivalent
                .verifies_prepared_query_overlay_result(&query, &rows, &certificate)
                .expect("changed overlay is a clean mismatch")
        );
        assert!(
            first
                .verifies_prepared_query_overlay_result(&query, &rows, &certificate)
                .expect("unchanged overlay still verifies")
        );

        let (advanced_rows, advanced_plan, advanced_certificate) = equivalent
            .execute_prepared_query_with_overlay_result_certificate(&database, &query)
            .expect("advanced overlay evidence issues");
        assert_eq!(advanced_rows, vec![VId(2), VId(3), VId(4)]);
        assert!(advanced_plan.verifies_at(query.plan(), equivalent.basis()));
        assert!(
            equivalent
                .verifies_prepared_query_overlay_result(
                    &query,
                    &advanced_rows,
                    &advanced_certificate,
                )
                .expect("advanced certificate verifies")
        );
        assert!(
            !first
                .verifies_prepared_query_overlay_result(
                    &query,
                    &advanced_rows,
                    &advanced_certificate,
                )
                .expect("older overlay rejects advanced evidence")
        );

        first.abort();
        equivalent.abort();
    });

    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
