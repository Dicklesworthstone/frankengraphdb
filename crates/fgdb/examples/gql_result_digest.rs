//! **One execution with statement, plan, and exact ordered-result evidence**.
//!
//! The result digest is chained to the plan certificate, so it changes if the
//! plan, snapshot, row count, row order, or any returned `VId` changes.
//!
//! ```text
//! cargo run --example gql_result_digest
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
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
    let path =
        std::env::temp_dir().join(format!("fgdb-result-digest-example-{}", std::process::id()));
    let keys = DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    );
    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

    runtime.block_on(async move {
        let mut db = Database::create(cx, &path, keys).await?;
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(1), VId(3), vec![]);
        let snapshot = db.write(cx, batch).await?;

        let bind = RelationBind::new().with_relation("KNOWS", KNOWS);
        let plan = db.prepare_gql_plan(QUERY, &bind)?;
        let (rows, input_certificate, plan_certificate, result_digest) =
            db.execute_gql_with_result_digest(QUERY, &bind)?;

        assert_eq!(rows, vec![VId(2), VId(3)]);
        assert!(input_certificate.verifies_at(QUERY, &bind, snapshot));
        assert!(plan_certificate.verifies_at(&plan, snapshot));
        assert!(plan_certificate.verifies_result_digest(&rows, result_digest));
        assert!(!plan_certificate.verifies_result_digest(&[VId(3), VId(2)], result_digest));

        println!("snapshot: {snapshot:?}");
        println!("rows: {rows:?}");
        println!("result digest: {result_digest:?}");
        println!("OK: statement, plan, snapshot, and ordered rows are bound");
        Ok(())
    })
}
