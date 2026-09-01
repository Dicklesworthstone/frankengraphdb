//! **A real `main()` with pinned read sessions and one reusable GQL plan**.
//!
//! One `BoundPlan` is executed against two immutable `EmbeddedReadView`
//! sessions and one explicit historical sequence. The first session keeps
//! answering from its original frontier after the writer commits a successor
//! generation; the same prepared plan executes against the new session without
//! reparsing or rebinding, and its `_at` face reproduces the old answer with a
//! certificate naming the exact historical sequence.
//!
//! ```text
//! cargo run --example gql_prepared_read_session
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
        std::env::temp_dir().join(format!("fgdb-read-session-example-{}", std::process::id()));
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
        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        let first_seq = db.write(cx, first).await?;

        let bind = RelationBind::new().with_relation("KNOWS", KNOWS);
        let old = db.read_session()?;
        let prepared = old.prepare_gql_plan(QUERY, &bind)?;
        let (old_rows, old_certificate) = old.execute_prepared_gql_certified(&prepared)?;
        assert_eq!(old.frontier(), first_seq);
        assert_eq!(old_rows, vec![VId(2)]);
        assert!(old_certificate.verifies_at(&prepared, first_seq));

        let mut successor = WriteBatch::new(KNOWS);
        successor.create_vertex(VId(3), vec![], vec![]);
        successor.add_edge(EId(11), VId(1), VId(3), vec![]);
        let successor_seq = db.write(cx, successor).await?;
        let current = db.read_session()?;
        let (current_rows, current_certificate) =
            current.execute_prepared_gql_certified(&prepared)?;
        let (replayed_rows, replayed_certificate) =
            current.execute_prepared_gql_certified_at(&prepared, first_seq)?;
        let (input_rows, input_certificate) = old.execute_gql_certified(QUERY, &bind)?;

        assert_eq!(old.execute_prepared_gql(&prepared)?, vec![VId(2)]);
        assert_eq!(current_rows, vec![VId(2), VId(3)]);
        assert_eq!(replayed_rows, old_rows);
        assert_eq!(replayed_certificate, old_certificate);
        assert_eq!(input_rows, old_rows);
        assert!(input_certificate.verifies_at(QUERY, &bind, first_seq));
        assert_eq!(old_certificate.snapshot_seq, first_seq);
        assert_eq!(current_certificate.snapshot_seq, successor_seq);
        assert!(current_certificate.verifies_at(&prepared, successor_seq));
        assert_ne!(old_certificate.digest, current_certificate.digest);

        println!("old session {first_seq:?}: {old_rows:?}");
        println!("new session {successor_seq:?}: {current_rows:?}");
        println!("historical replay {first_seq:?}: {replayed_rows:?}");
        println!("OK: one prepared plan crossed live, pinned, and historical reads");
        Ok(())
    })
}
