//! **Resume an exact audited result with a result-bound continuation token.**
//!
//! ```text
//! cargo run -p fgdb --example gql_evidence_pages
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::{GqlEvidencePageAuditError, GqlEvidencePageError};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const KNOWS: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:KNOWS]->(b) RETURN b";

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let path = std::env::temp_dir().join(format!(
        "fgdb-evidence-pages-example-{}",
        std::process::id()
    ));
    let keys = DatabaseKeys::new(
        [0x3a; 32],
        DatabaseSecurityNamespaceId([0x4b; 32]),
        [0x5c; 32],
    );
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

    runtime.block_on(async move {
        let mut database = Database::create(cx, &path, keys).await?;
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        for (vid, eid) in [(VId(2), EId(10)), (VId(3), EId(11)), (VId(4), EId(12))] {
            batch.create_vertex(vid, vec![], vec![]);
            batch.add_edge(eid, VId(1), vid, vec![]);
        }
        let snapshot = database.write(cx, batch).await?;

        let query = database.prepare_gql_query(
            QUERY,
            &RelationBind::new().with_relation("KNOWS", KNOWS),
        )?;
        let artifact = database.execute_prepared_query_artifact_at(&query, snapshot)?;
        let bytes = artifact.to_bytes();

        let first = database.audit_untrusted_prepared_query_artifact_page(
            &query,
            &bytes,
            2,
            None,
        )?;
        assert_eq!(first.rows(), &[VId(2), VId(3)]);
        let token = first
            .next_token()
            .expect("one row remains")
            .to_bytes();

        let mut later = WriteBatch::new(KNOWS);
        later.create_vertex(VId(5), vec![], vec![]);
        later.add_edge(EId(13), VId(1), VId(5), vec![]);
        database.write(cx, later).await?;

        let second = database.audit_untrusted_prepared_query_artifact_page(
            &query,
            &bytes,
            2,
            Some(&token),
        )?;
        assert_eq!(second.rows(), &[VId(4)]);
        assert!(second.is_terminal());

        let current = database.execute_prepared_query_artifact(&query)?;
        let mismatch = database
            .audit_untrusted_prepared_query_artifact_page(
                &query,
                &current.to_bytes(),
                2,
                Some(&token),
            )
            .expect_err("an old token must not resume a different snapshot result");
        assert!(matches!(
            mismatch,
            GqlEvidencePageAuditError::Page(
                GqlEvidencePageError::TokenSequenceMismatch { .. }
            )
        ));

        println!("certified snapshot: {snapshot:?}");
        println!("first page: {:?}", first.rows());
        println!("second page after live advance: {:?}", second.rows());
        println!("OK: the token resumed only its exact historically replayed result");
        Ok(())
    })
}
