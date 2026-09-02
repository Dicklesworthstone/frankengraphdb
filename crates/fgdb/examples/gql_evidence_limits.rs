//! **Audit exact GQL evidence under explicit untrusted-input limits.**
//!
//! ```text
//! cargo run -p fgdb --example gql_evidence_limits
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GqlEvidenceLimitDimension, GqlEvidenceLimitedAuditError,
    GqlEvidenceLimits,
};
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
        "fgdb-evidence-limits-example-{}",
        std::process::id()
    ));
    let keys = DatabaseKeys::new(
        [0x2a; 32],
        DatabaseSecurityNamespaceId([0x3b; 32]),
        [0x4c; 32],
    );
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

    runtime.block_on(async move {
        let mut database = Database::create(cx, &path, keys).await?;
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        let snapshot = database.write(cx, batch).await?;

        let query = database.prepare_gql_query(
            QUERY,
            &RelationBind::new().with_relation("KNOWS", KNOWS),
        )?;
        let artifact =
            database.execute_prepared_query_artifact_at(&query, snapshot)?;
        let exact = GqlEvidenceLimits::new(
            artifact.canonical_encoded_len(),
            artifact.rows().len() as u64,
        );
        let bytes = artifact.to_bytes_with_limits(exact)?;

        let audited = database
            .audit_untrusted_prepared_query_artifact(&query, &bytes)?;
        assert_eq!(audited.rows(), &[VId(2)]);

        let refusal = database
            .audit_prepared_query_artifact_with_limits(
                &query,
                &bytes,
                GqlEvidenceLimits::new(bytes.len() as u64, 0),
            )
            .expect_err("a zero-row policy must refuse one declared row");
        assert!(matches!(
            refusal,
            GqlEvidenceLimitedAuditError::Limit(exceeded)
                if exceeded.dimension == GqlEvidenceLimitDimension::Rows
                    && exceeded.limit == 0
                    && exceeded.observed == 1
        ));

        println!("snapshot: {snapshot:?}");
        println!("encoded bytes: {}", bytes.len());
        println!("audited rows: {:?}", audited.rows());
        println!("OK: untrusted bytes were screened before strict replay audit");
        Ok(())
    })
}
