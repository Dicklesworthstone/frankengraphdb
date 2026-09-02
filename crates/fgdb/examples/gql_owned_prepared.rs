//! **Own one coherent prepared query and execute it under deterministic bounds.**
//!
//! ```text
//! cargo run -p fgdb --example gql_owned_prepared
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_gql::{BudgetedGqlError, GqlBudgetDimension, GqlExecutionBudget};
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
        "fgdb-owned-prepared-example-{}",
        std::process::id()
    ));
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
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        let snapshot = db.write(cx, batch).await?;

        let mut bind = RelationBind::new().with_relation("KNOWS", KNOWS);
        let prepared = db.prepare_gql_query(QUERY, &bind)?;
        bind.insert("KNOWS", RelationId(99));

        let bounded = db.execute_prepared_query_budgeted_at(
            &prepared,
            snapshot,
            GqlExecutionBudget::new(1, 1),
        )?;
        assert_eq!(bounded.value, vec![VId(2)]);
        assert_eq!(bounded.stats.snapshot_records, 1);
        assert_eq!(bounded.stats.result_rows, 1);

        let refusal = db
            .execute_prepared_query_budgeted_at(
                &prepared,
                snapshot,
                GqlExecutionBudget::snapshot_records(0),
            )
            .expect_err("one admitted edge exceeds a zero-record budget");
        assert!(matches!(
            refusal,
            BudgetedGqlError::Budget(exceeded)
                if exceeded.dimension == GqlBudgetDimension::SnapshotRecords
                    && exceeded.limit == 0
                    && exceeded.observed == 1
        ));

        let (rows, input, plan, result_digest) =
            db.execute_prepared_query_with_result_digest_at(&prepared, snapshot)?;
        assert_eq!(rows, bounded.value);
        assert!(input.verifies_at(prepared.statement(), prepared.bind(), snapshot));
        assert!(plan.verifies_at(prepared.plan(), snapshot));
        assert!(plan.verifies_result_digest(&rows, result_digest));

        println!("snapshot: {snapshot:?}");
        println!("rows: {rows:?}");
        println!("snapshot records: {}", bounded.stats.snapshot_records);
        println!("result rows: {}", bounded.stats.result_rows);
        println!("OK: owned preparation, budgets, and evidence remain aligned");
        Ok(())
    })
}
