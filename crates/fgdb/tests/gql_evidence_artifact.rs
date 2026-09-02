use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::{GqlEvidenceAuditError, GqlEvidenceDecodeError};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x26; 32],
        DatabaseSecurityNamespaceId([0x37; 32]),
        [0x48; 32],
    )
}

fn test_directory() -> std::io::Result<std::path::PathBuf> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    loop {
        let ordinal = NEXT.fetch_add(1, Ordering::Relaxed);
        let parent = base.join(format!("fgdb-gql-evidence-artifact-{ordinal}"));
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
fn evidence_artifacts_audit_historical_and_staged_results_fail_closed() {
    let ((), report) = run_async_under_lab(0x51_09, |root| async move {
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
        let pinned = database.read_session().expect("view pins the first basis");

        let artifact = database
            .execute_prepared_query_artifact_at(&query, basis)
            .expect("historical artifact issues");
        assert_eq!(artifact.rows(), &[VId(2)]);
        let bytes = artifact.to_bytes();
        assert_eq!(
            database
                .audit_prepared_query_artifact(&query, &bytes)
                .expect("database audits its artifact"),
            artifact
        );
        assert_eq!(
            pinned
                .audit_prepared_query_artifact(&query, &bytes)
                .expect("pinned view audits the same artifact"),
            artifact
        );

        database
            .write(&commit, vertex_and_edge(VId(3), EId(11)))
            .await
            .expect("live frontier advances");
        assert_eq!(
            database
                .execute_prepared_query(&query)
                .expect("live query executes"),
            vec![VId(2), VId(3)]
        );
        assert_eq!(
            database
                .audit_prepared_query_artifact(&query, &bytes)
                .expect("old artifact replays at its historical sequence")
                .rows(),
            &[VId(2)]
        );

        let other_query = database
            .prepare_gql_query(
                "MATCH (a)-[:R]->(b) RETURN a",
                &RelationBind::new().with_relation("R", R),
            )
            .expect("other query prepares");
        assert!(matches!(
            database.audit_prepared_query_artifact(&other_query, &bytes),
            Err(GqlEvidenceAuditError::InputMismatch)
        ));

        let mut corrupted_row = bytes.clone();
        corrupted_row[128] ^= 1;
        assert!(matches!(
            database.audit_prepared_query_artifact(&query, &corrupted_row),
            Err(GqlEvidenceAuditError::Decode(
                GqlEvidenceDecodeError::ResultDigestMismatch
            ))
        ));

        let mut trailing = bytes;
        trailing.push(0);
        assert!(matches!(
            database.audit_prepared_query_artifact(&query, &trailing),
            Err(GqlEvidenceAuditError::Decode(
                GqlEvidenceDecodeError::TrailingBytes { count: 1 }
            ))
        ));

        let mut transaction = database.begin(&txn_cx).expect("transaction begins");
        transaction
            .write(&mut database, vertex_and_edge(VId(4), EId(12)))
            .expect("overlay stages");
        let overlay_artifact = transaction
            .execute_prepared_query_overlay_artifact(&database, &query)
            .expect("overlay artifact issues");
        assert_eq!(overlay_artifact.rows(), &[VId(2), VId(3), VId(4)]);
        let overlay_bytes = overlay_artifact.to_bytes();
        assert_eq!(
            transaction
                .audit_prepared_query_overlay_artifact(&database, &query, &overlay_bytes,)
                .expect("current overlay audits"),
            overlay_artifact
        );

        let mut corrupted_overlay_row = overlay_bytes.clone();
        corrupted_overlay_row[160] ^= 1;
        assert!(matches!(
            transaction.audit_prepared_query_overlay_artifact(
                &database,
                &query,
                &corrupted_overlay_row,
            ),
            Err(GqlEvidenceAuditError::Decode(
                GqlEvidenceDecodeError::ResultDigestMismatch
            ))
        ));

        transaction
            .write(&mut database, vertex_and_edge(VId(5), EId(13)))
            .expect("overlay advances");
        assert!(matches!(
            transaction.audit_prepared_query_overlay_artifact(&database, &query, &overlay_bytes,),
            Err(GqlEvidenceAuditError::StagedEffectMismatch)
        ));
        transaction.abort();
    });

    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
