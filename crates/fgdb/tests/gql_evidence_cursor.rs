use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GqlEvidenceAuditError, GqlEvidenceCursorError, GqlEvidenceCursorState,
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
        [0x49; 32],
        DatabaseSecurityNamespaceId([0x5a; 32]),
        [0x6b; 32],
    )
}

fn test_directory() -> std::io::Result<std::path::PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    loop {
        let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = base.join(format!("fgdb-gql-evidence-cursor-{ordinal}"));
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
fn audited_cursors_advance_once_and_own_their_exact_result() {
    let ((), report) = run_async_under_lab(0x71_30, |root| async move {
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
        let exact_limits =
            GqlEvidenceLimits::new(bytes.len() as u64, artifact.rows().len() as u64);

        let mut cursor = database
            .open_prepared_query_artifact_cursor_with_limits(
                &query,
                &bytes,
                exact_limits,
            )
            .expect("artifact audits once and cursor opens");
        assert_eq!(cursor.state(), GqlEvidenceCursorState::Open);
        assert_eq!(cursor.sequence(), snapshot);
        assert_eq!(cursor.result_digest(), artifact.result_digest());
        assert_eq!(cursor.position(), 0);
        assert_eq!(cursor.total_rows(), 3);
        assert_eq!(cursor.remaining_rows(), 3);

        let first = cursor.next_page(2).expect("first cursor page succeeds");
        assert_eq!(first.rows(), &[VId(2), VId(3)]);
        assert_eq!(cursor.position(), 2);
        assert_eq!(cursor.remaining_rows(), 1);
        let checkpoint = cursor
            .checkpoint_token()
            .expect("one row remains after the first page");
        assert_eq!(checkpoint.next_offset(), 2);

        database
            .write(&commit, vertex_and_edge(VId(5), EId(13)))
            .await
            .expect("live database advances");

        let terminal = cursor
            .next_page(8)
            .expect("owned cursor continues without replay");
        assert_eq!(terminal.rows(), &[VId(4)]);
        assert!(terminal.is_terminal());
        assert_eq!(cursor.state(), GqlEvidenceCursorState::Exhausted);
        assert_eq!(cursor.position(), 3);
        assert_eq!(cursor.remaining_rows(), 0);
        assert!(cursor.checkpoint_token().is_none());
        assert!(matches!(
            cursor.next_page(1),
            Err(GqlEvidenceCursorError::Exhausted)
        ));

        let mut view_cursor = view
            .open_untrusted_prepared_query_artifact_cursor(&query, &bytes)
            .expect("pinned view opens the same audited result");
        assert_eq!(view_cursor.total_rows(), 3);
        assert!(view_cursor.close());
        assert_eq!(view_cursor.state(), GqlEvidenceCursorState::Closed);
        assert!(!view_cursor.close());
        assert!(matches!(
            view_cursor.next_page(1),
            Err(GqlEvidenceCursorError::Closed)
        ));

        let byte_refusal = database
            .open_prepared_query_artifact_cursor_with_limits(
                &query,
                &bytes,
                GqlEvidenceLimits::new((bytes.len() - 1) as u64, u64::MAX),
            )
            .expect_err("cursor cannot open before artifact admission");
        assert!(matches!(
            byte_refusal,
            GqlEvidenceLimitedAuditError::Limit(_)
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
            .expect("overlay audits once and cursor opens");

        let overlay_first = overlay_cursor
            .next_page(2)
            .expect("overlay first page succeeds");
        assert_eq!(overlay_first.rows(), &[VId(2), VId(3)]);
        assert_eq!(overlay_cursor.remaining_rows(), 3);

        transaction
            .write(&mut database, vertex_and_edge(VId(7), EId(15)))
            .expect("transaction advances after cursor open");

        let overlay_rest = overlay_cursor
            .next_page(8)
            .expect("open cursor retains the previously audited result");
        assert_eq!(overlay_rest.rows(), &[VId(4), VId(5), VId(6)]);
        assert!(overlay_rest.is_terminal());
        assert!(overlay_cursor.is_exhausted());

        let stale_open = transaction
            .open_untrusted_prepared_query_overlay_artifact_cursor(
                &database,
                &query,
                &overlay_bytes,
            )
            .expect_err("old bytes cannot open against a changed overlay");
        assert!(matches!(
            stale_open,
            GqlEvidenceLimitedAuditError::Audit(
                GqlEvidenceAuditError::StagedEffectMismatch
            )
        ));

        let debug = format!("{overlay_cursor:?}");
        assert!(!debug.contains("VId"));
        transaction.abort();
    });

    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
