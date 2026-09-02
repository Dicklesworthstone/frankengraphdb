//! **Persist and audit exact prepared-query evidence locally.**
//!
//! ```text
//! cargo run -p fgdb --example gql_evidence_artifact
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::GqlEvidenceAuditError;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let path = std::env::temp_dir().join(format!(
        "fgdb-evidence-artifact-example-{}",
        std::process::id()
    ));
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let txn_cx = contexts.txn();
    let keys = DatabaseKeys::new(
        [0x26; 32],
        DatabaseSecurityNamespaceId([0x37; 32]),
        [0x48; 32],
    );

    runtime.block_on(async move {
        let mut database = Database::create(&commit, &path, keys).await?;
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![], vec![]);
        initial.create_vertex(VId(2), vec![], vec![]);
        initial.add_edge(EId(10), VId(1), VId(2), vec![]);
        let basis = database.write(&commit, initial).await?;

        let query =
            database.prepare_gql_query(QUERY, &RelationBind::new().with_relation("R", R))?;
        let durable = database.execute_prepared_query_artifact_at(&query, basis)?;
        let durable_bytes = durable.to_bytes();

        let mut later = WriteBatch::new(R);
        later.create_vertex(VId(3), vec![], vec![]);
        later.add_edge(EId(11), VId(1), VId(3), vec![]);
        database.write(&commit, later).await?;
        assert_eq!(
            database.audit_prepared_query_artifact(&query, &durable_bytes)?,
            durable
        );
        assert_eq!(durable.rows(), &[VId(2)]);

        let mut transaction = database.begin(&txn_cx)?;
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(4), vec![], vec![]);
        staged.add_edge(EId(12), VId(1), VId(4), vec![]);
        transaction.write(&mut database, staged)?;
        let overlay = transaction.execute_prepared_query_overlay_artifact(&database, &query)?;
        let overlay_bytes = overlay.to_bytes();
        assert_eq!(
            transaction.audit_prepared_query_overlay_artifact(&database, &query, &overlay_bytes,)?,
            overlay
        );

        let mut changed = WriteBatch::new(R);
        changed.create_vertex(VId(5), vec![], vec![]);
        changed.add_edge(EId(13), VId(1), VId(5), vec![]);
        transaction.write(&mut database, changed)?;
        assert!(matches!(
            transaction.audit_prepared_query_overlay_artifact(&database, &query, &overlay_bytes,),
            Err(GqlEvidenceAuditError::StagedEffectMismatch)
        ));

        println!("historical artifact bytes: {}", durable_bytes.len());
        println!("historical rows: {:?}", durable.rows());
        println!("staged artifact bytes: {}", overlay_bytes.len());
        println!("staged rows: {:?}", overlay.rows());
        println!("OK: exact historical replay and staged-effect invalidation");
        transaction.abort();
        Ok(())
    })
}
