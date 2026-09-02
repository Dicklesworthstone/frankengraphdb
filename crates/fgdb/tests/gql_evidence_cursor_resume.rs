use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GqlEvidenceAuditError, GqlEvidenceLimitedAuditError,
    GqlEvidencePageAuditError, GqlEvidencePageError,
    GqlEvidencePageTokenDecodeError,
};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x59; 32],
        DatabaseSecurityNamespaceId([0x6a; 32]),
        [0x7b; 32],
    )
}

fn test_directory() -> std::io::Result<std::path::PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    loop {
        let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = base.join(format!("fgdb-gql-cursor-resume-{ordinal}"));
        match std::fs::create_dir(&parent) {
            Ok(()) => return Ok(parent.join("database")),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

fn vertex_and_edge(vid: VId, eid: EId) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(vid, vec![], vec![]);
    batch.add_edge(eid, VId(1), vid, vec![]);
    batch
}

#[test]
fn result_bound_checkpoints_resume_only_the_audited_result() {
    let ((), report) = run_async_under_lab(0x71_31, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let directory = test_directory().expect("unique test directory");
        let mut database = Database::create(&commit, &directory, keys())
            .await
            .expect("database creates");

        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![], vec![]);
        for (vid, eid) in [(VId(2), EId(10)), (VId(3), EId(11)), (VId(4), EId(12))] {
            initial.create_vertex(vid, vec![], vec![]);
            initial.add_edge(eid, VId(1), vid, vec![]);
        }
        let snapshot = database
            .write(&commit, initial)
            .await
            .expect("fixture commits");

        let query = database
            .prepare_gql_query(
                QUERY,
                &RelationBind::new().with_relation("R", R),
            )
            .expect("query prepares");
        let view = database.read_session().expect("view pins");
        let artifact = database
            .execute_prepared_query_artifact_at(&query, snapshot)
            .expect("artifact issues");
        let bytes = artifact.to_bytes();

        let mut first_cursor = database
            .open_untrusted_prepared_query_artifact_cursor(&query, &bytes)
            .expect("cursor opens after audit");
        assert_eq!(
            first_cursor
                .next_page(2)
                .expect("first page succeeds")
                .rows(),
            &[VId(2), VId(3)]
        );
        let checkpoint = first_cursor
            .checkpoint_token()
            .expect("one row remains")
            .to_bytes();
        assert!(first_cursor.close());

        database
            .write(&commit, vertex_and_edge(VId(5), EId(13)))
            .await
            .expect("live database advances");

        let mut resumed = database
            .resume_untrusted_prepared_query_artifact_cursor(
                &query,
                &bytes,
                &checkpoint,
            )
            .expect("historical artifact audits once and resumes");
        assert_eq!(resumed.position(), 2);
        assert_eq!(resumed.remaining_rows(), 1);
        assert_eq!(
            resumed
                .next_page(8)
                .expect("remaining historical page succeeds")
                .rows(),
            &[VId(4)]
        );
        assert!(resumed.is_exhausted());

        let mut view_resumed = view
            .resume_untrusted_prepared_query_artifact_cursor(
                &query,
                &bytes,
                &checkpoint,
            )
            .expect("immutable view resumes the same result");
        assert_eq!(view_resumed.position(), 2);
        assert_eq!(
            view_resumed
                .next_page(8)
                .expect("view remainder succeeds")
                .rows(),
            &[VId(4)]
        );

        let current = database
            .execute_prepared_query_artifact(&query)
            .expect("current artifact issues");
        let current_bytes = current.to_bytes();
        let wrong_result = database
            .resume_untrusted_prepared_query_artifact_cursor(
                &query,
                &current_bytes,
                &checkpoint,
            )
            .expect_err("old checkpoint cannot resume the new result");
        assert!(matches!(
            wrong_result,
            GqlEvidencePageAuditError::Page(
                GqlEvidencePageError::TokenSequenceMismatch { .. }
            )
        ));

        let mut corrupt_checkpoint = checkpoint;
        corrupt_checkpoint[40] ^= 1;
        let syntax_error = database
            .resume_untrusted_prepared_query_artifact_cursor(
                &query,
                b"not an artifact",
                &corrupt_checkpoint,
            )
            .expect_err("bad token refuses before artifact replay");
        assert!(matches!(
            syntax_error,
            GqlEvidencePageAuditError::Page(
                GqlEvidencePageError::TokenDecode(
                    GqlEvidencePageTokenDecodeError::ChecksumMismatch
                )
            )
        ));

        let mut transaction = database.begin(&txn_cx).expect("transaction begins");
        transaction
            .write(&mut database, vertex_and_edge(VId(6), EId(14)))
            .expect("overlay stages");
        let overlay = transaction
            .execute_prepared_query_overlay_artifact(&database, &query)
            .expect("overlay artifact issues");
        let overlay_bytes = overlay.to_bytes();
        let mut overlay_cursor = transaction
            .open_untrusted_prepared_query_overlay_artifact_cursor(
                &database,
                &query,
                &overlay_bytes,
            )
            .expect("overlay cursor opens");
        assert_eq!(
            overlay_cursor
                .next_page(2)
                .expect("overlay first page succeeds")
                .rows(),
            &[VId(2), VId(3)]
        );
        let overlay_checkpoint = overlay_cursor
            .checkpoint_token()
            .expect("overlay rows remain")
            .to_bytes();

        let mut overlay_resumed = transaction
            .resume_untrusted_prepared_query_overlay_artifact_cursor(
                &database,
                &query,
                &overlay_bytes,
                &overlay_checkpoint,
            )
            .expect("unchanged overlay resumes");
        assert_eq!(overlay_resumed.position(), 2);
        assert_eq!(
            overlay_resumed
                .next_page(8)
                .expect("overlay remainder succeeds")
                .rows(),
            &[VId(4), VId(5), VId(6)]
        );

        transaction
            .write(&mut database, vertex_and_edge(VId(7), EId(15)))
            .expect("overlay changes");
        let stale_overlay = transaction
            .resume_untrusted_prepared_query_overlay_artifact_cursor(
                &database,
                &query,
                &overlay_bytes,
                &overlay_checkpoint,
            )
            .expect_err("stale overlay cannot reopen from its old checkpoint");
        assert!(matches!(
            stale_overlay,
            GqlEvidencePageAuditError::Audit(
                GqlEvidenceLimitedAuditError::Audit(
                    GqlEvidenceAuditError::StagedEffectMismatch
                )
            )
        ));
        transaction.abort();
    });

    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
