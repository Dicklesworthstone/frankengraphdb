use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GqlEvidenceAuditError, GqlEvidenceLimitDimension,
    GqlEvidenceLimitedAuditError, GqlEvidenceLimits, GqlEvidencePageAuditError,
    GqlEvidencePageError, GqlEvidencePageTokenDecodeError,
};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x39; 32],
        DatabaseSecurityNamespaceId([0x4a; 32]),
        [0x5b; 32],
    )
}

fn test_directory() -> std::io::Result<std::path::PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    loop {
        let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = base.join(format!("fgdb-gql-evidence-pages-{ordinal}"));
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
fn audited_pages_bind_exact_durable_and_staged_results() {
    let ((), report) = run_async_under_lab(0x61_20, |root| async move {
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
        let view = database.read_session().expect("read view pins");
        let artifact = database
            .execute_prepared_query_artifact_at(&query, snapshot)
            .expect("artifact issues");
        let bytes = artifact.to_bytes();

        let first = database
            .audit_untrusted_prepared_query_artifact_page(
                &query, &bytes, 2, None,
            )
            .expect("first audited page succeeds");
        assert_eq!(first.rows(), &[VId(2), VId(3)]);
        assert_eq!(first.start_offset(), 0);
        assert_eq!(first.end_offset(), 2);
        assert_eq!(first.total_rows(), 3);
        assert_eq!(first.remaining_rows(), 1);
        let token = first
            .next_token()
            .expect("one row remains")
            .to_bytes();

        let second = view
            .audit_untrusted_prepared_query_artifact_page(
                &query,
                &bytes,
                2,
                Some(&token),
            )
            .expect("pinned view resumes the same exact result");
        assert_eq!(second.rows(), &[VId(4)]);
        assert_eq!(second.start_offset(), 2);
        assert_eq!(second.end_offset(), 3);
        assert_eq!(second.total_rows(), 3);
        assert_eq!(second.remaining_rows(), 0);
        assert!(second.is_terminal());

        database
            .write(&commit, vertex_and_edge(VId(5), EId(13)))
            .await
            .expect("live database advances");
        let historical = database
            .audit_untrusted_prepared_query_artifact_page(
                &query,
                &bytes,
                2,
                Some(&token),
            )
            .expect("old artifact still resumes by historical replay");
        assert_eq!(historical, second);

        let zero_page = database
            .audit_untrusted_prepared_query_artifact_page(
                &query, b"not an artifact", 0, None,
            )
            .expect_err("zero page size refuses before artifact decode");
        assert!(matches!(
            zero_page,
            GqlEvidencePageAuditError::Page(
                GqlEvidencePageError::ZeroPageSize
            )
        ));

        let byte_limit = database
            .audit_prepared_query_artifact_page_with_limits(
                &query,
                &bytes,
                GqlEvidenceLimits::new((bytes.len() - 1) as u64, u64::MAX),
                2,
                None,
            )
            .expect_err("artifact admission still precedes result paging");
        assert!(matches!(
            byte_limit,
            GqlEvidencePageAuditError::Audit(
                GqlEvidenceLimitedAuditError::Limit(exceeded)
            ) if exceeded.dimension == GqlEvidenceLimitDimension::EncodedBytes
        ));

        let current = database
            .execute_prepared_query_artifact(&query)
            .expect("current artifact issues");
        let current_bytes = current.to_bytes();
        let cross_snapshot = database
            .audit_untrusted_prepared_query_artifact_page(
                &query,
                &current_bytes,
                2,
                Some(&token),
            )
            .expect_err("old token cannot resume a newer snapshot");
        assert!(matches!(
            cross_snapshot,
            GqlEvidencePageAuditError::Page(
                GqlEvidencePageError::TokenSequenceMismatch { .. }
            )
        ));

        let mut corrupted_token = token;
        corrupted_token[40] ^= 1;
        let token_error = database
            .audit_untrusted_prepared_query_artifact_page(
                &query,
                b"not an artifact",
                2,
                Some(&corrupted_token),
            )
            .expect_err("token corruption refuses");
        assert!(matches!(
            token_error,
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
        let overlay_first = transaction
            .audit_untrusted_prepared_query_overlay_artifact_page(
                &database,
                &query,
                &overlay_bytes,
                2,
                None,
            )
            .expect("overlay first page succeeds");
        let overlay_token = overlay_first
            .next_token()
            .expect("overlay has more rows")
            .to_bytes();
        let overlay_second = transaction
            .audit_untrusted_prepared_query_overlay_artifact_page(
                &database,
                &query,
                &overlay_bytes,
                8,
                Some(&overlay_token),
            )
            .expect("overlay continuation succeeds");
        assert!(overlay_second.is_terminal());

        let cross_kind = transaction
            .audit_untrusted_prepared_query_overlay_artifact_page(
                &database,
                &query,
                &overlay_bytes,
                2,
                Some(&token),
            )
            .expect_err("durable token cannot resume a staged result");
        assert!(matches!(
            cross_kind,
            GqlEvidencePageAuditError::Page(
                GqlEvidencePageError::TokenKindMismatch { .. }
            )
        ));

        transaction
            .write(&mut database, vertex_and_edge(VId(7), EId(15)))
            .expect("overlay advances");
        let changed_overlay = transaction
            .audit_untrusted_prepared_query_overlay_artifact_page(
                &database,
                &query,
                &overlay_bytes,
                8,
                Some(&overlay_token),
            )
            .expect_err("changed staged effect refuses before paging");
        assert!(matches!(
            changed_overlay,
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
