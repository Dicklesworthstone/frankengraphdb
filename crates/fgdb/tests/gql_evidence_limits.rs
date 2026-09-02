use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GqlEvidenceAuditError, GqlEvidenceDecodeError, GqlEvidenceLimitDimension,
    GqlEvidenceLimitedAuditError, GqlEvidenceLimits,
};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x29; 32],
        DatabaseSecurityNamespaceId([0x3a; 32]),
        [0x4b; 32],
    )
}

fn test_directory() -> std::io::Result<std::path::PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    loop {
        let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = base.join(format!("fgdb-gql-evidence-limits-{ordinal}"));
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
fn untrusted_evidence_audits_enforce_limits_before_replay() {
    let ((), report) = run_async_under_lab(0x51_10, |root| async move {
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
        let basis = database
            .write(&commit, initial)
            .await
            .expect("fixture commits");

        let query = database
            .prepare_gql_query(QUERY, &RelationBind::new().with_relation("R", R))
            .expect("query prepares");
        let view = database.read_session().expect("view pins");

        let artifact = database
            .execute_prepared_query_artifact_at(&query, basis)
            .expect("prepared artifact issues");
        let bytes = artifact.to_bytes();
        let exact = GqlEvidenceLimits::new(bytes.len() as u64, artifact.rows().len() as u64);

        assert_eq!(
            database
                .audit_prepared_query_artifact_with_limits(&query, &bytes, exact,)
                .expect("exact database limits succeed"),
            artifact
        );
        assert_eq!(
            view.audit_prepared_query_artifact_with_limits(&query, &bytes, exact,)
                .expect("exact view limits succeed"),
            artifact
        );
        assert_eq!(
            database
                .audit_untrusted_prepared_query_artifact(&query, &bytes)
                .expect("default untrusted database audit succeeds"),
            artifact
        );

        let byte_error = database
            .audit_prepared_query_artifact_with_limits(
                &query,
                &bytes,
                GqlEvidenceLimits::new((bytes.len() - 1) as u64, u64::MAX),
            )
            .expect_err("one byte below exact length refuses");
        assert!(matches!(
            byte_error,
            GqlEvidenceLimitedAuditError::Limit(exceeded)
                if exceeded.dimension
                    == GqlEvidenceLimitDimension::EncodedBytes
                    && exceeded.observed == bytes.len() as u64
        ));

        let row_error = database
            .audit_prepared_query_artifact_with_limits(
                &query,
                &bytes,
                GqlEvidenceLimits::new(u64::MAX, 0),
            )
            .expect_err("zero-row policy refuses one declared row");
        assert!(matches!(
            row_error,
            GqlEvidenceLimitedAuditError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceLimitDimension::Rows
                    && exceeded.observed == 1
        ));

        let mut hostile_count = bytes.clone();
        hostile_count[120..128].copy_from_slice(&u64::MAX.to_be_bytes());
        hostile_count.truncate(128);
        let hostile_error = database
            .audit_prepared_query_artifact_with_limits(
                &query,
                &hostile_count,
                GqlEvidenceLimits::new(1024, 100),
            )
            .expect_err("hostile count refuses before decoder allocation");
        assert!(matches!(
            hostile_error,
            GqlEvidenceLimitedAuditError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceLimitDimension::Rows
                    && exceeded.observed == u64::MAX
        ));

        let mut invalid_magic = bytes;
        invalid_magic[0] ^= 0xff;
        let format_error = database
            .audit_prepared_query_artifact_with_limits(
                &query,
                &invalid_magic,
                GqlEvidenceLimits::new(u64::MAX, u64::MAX),
            )
            .expect_err("malformed bytes preserve decoder refusal");
        assert!(matches!(
            format_error,
            GqlEvidenceLimitedAuditError::Audit(GqlEvidenceAuditError::Decode(
                GqlEvidenceDecodeError::InvalidMagic
            ))
        ));

        let mut transaction = database.begin(&txn_cx).expect("transaction begins");
        transaction
            .write(&mut database, vertex_and_edge(VId(3), EId(11)))
            .expect("overlay stages");
        let overlay = transaction
            .execute_prepared_query_overlay_artifact(&database, &query)
            .expect("overlay artifact issues");
        let overlay_bytes = overlay.to_bytes();
        let overlay_exact =
            GqlEvidenceLimits::new(overlay_bytes.len() as u64, overlay.rows().len() as u64);

        assert_eq!(
            transaction
                .audit_prepared_query_overlay_artifact_with_limits(
                    &database,
                    &query,
                    &overlay_bytes,
                    overlay_exact,
                )
                .expect("exact overlay limits succeed"),
            overlay
        );
        assert_eq!(
            transaction
                .audit_untrusted_prepared_query_overlay_artifact(&database, &query, &overlay_bytes,)
                .expect("default untrusted overlay audit succeeds"),
            overlay
        );

        let overlay_row_error = transaction
            .audit_prepared_query_overlay_artifact_with_limits(
                &database,
                &query,
                &overlay_bytes,
                GqlEvidenceLimits::new(u64::MAX, 1),
            )
            .expect_err("overlay row ceiling refuses before replay");
        assert!(matches!(
            overlay_row_error,
            GqlEvidenceLimitedAuditError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceLimitDimension::Rows
                    && exceeded.observed == 2
        ));

        transaction
            .write(&mut database, vertex_and_edge(VId(4), EId(12)))
            .expect("overlay advances");
        let staged_error = transaction
            .audit_untrusted_prepared_query_overlay_artifact(&database, &query, &overlay_bytes)
            .expect_err("changed staged effect invalidates old bytes");
        assert!(matches!(
            staged_error,
            GqlEvidenceLimitedAuditError::Audit(GqlEvidenceAuditError::StagedEffectMismatch)
        ));
        transaction.abort();
    });

    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
