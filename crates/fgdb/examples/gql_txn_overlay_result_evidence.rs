//! **Certify exact ordered GQL rows over one staged transaction effect.**
//!
//! ```text
//! cargo run -p fgdb --example gql_txn_overlay_result_evidence
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
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
        "fgdb-txn-overlay-evidence-example-{}",
        std::process::id()
    ));
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let txn_cx = contexts.txn();
    let keys = DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x83; 32],
    );

    runtime.block_on(async move {
        let mut database = Database::create(&commit, &path, keys).await?;
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![], vec![]);
        initial.create_vertex(VId(2), vec![], vec![]);
        initial.add_edge(EId(10), VId(1), VId(2), vec![]);
        database.write(&commit, initial).await?;

        let query = database.prepare_gql_query(
            QUERY,
            &RelationBind::new().with_relation("R", R),
        )?;
        let mut transaction = database.begin(&txn_cx)?;
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(3), vec![], vec![]);
        staged.add_edge(EId(11), VId(1), VId(3), vec![]);
        transaction.write(&mut database, staged)?;

        let (rows, plan, certificate) = transaction
            .execute_prepared_query_with_overlay_result_certificate(&database, &query)?;
        assert_eq!(rows, vec![VId(2), VId(3)]);
        assert!(plan.verifies_at(query.plan(), transaction.basis()));
        assert!(transaction.verifies_prepared_query_overlay_result(
            &query,
            &rows,
            &certificate,
        )?);

        let mut later = WriteBatch::new(R);
        later.create_vertex(VId(4), vec![], vec![]);
        later.add_edge(EId(12), VId(1), VId(4), vec![]);
        transaction.write(&mut database, later)?;
        assert!(!transaction.verifies_prepared_query_overlay_result(
            &query,
            &rows,
            &certificate,
        )?);

        println!("basis: {:?}", certificate.basis);
        println!("certified rows: {rows:?}");
        println!("OK: a later staged effect invalidates the old result evidence");
        transaction.abort();
        Ok(())
    })
}
