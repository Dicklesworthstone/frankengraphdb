//! **A real `main()` that queries a historical commit sequence**.
//!
//! Chronicle's MVCC commit stream lets a handle read the graph as it was at any
//! published commit sequence. This example writes two batches, keeps the first
//! commit sequence, and then shows that `execute_gql_at` returns the earlier
//! adjacency even after the second batch has been published.
//!
//! ```text
//! cargo run --example gql_time_travel
//! ```

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CommitSeq, EId, VId};

const KNOWS: RelationId = RelationId(1);

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let path =
        std::env::temp_dir().join(format!("fgdb-time-travel-example-{}", std::process::id()));
    let keys = DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    );

    println!("fgdb GQL time-travel witness");
    println!("  database directory: {}", path.display());

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
        println!("  committed first batch at seq {first_seq:?}");

        let mut second = WriteBatch::new(KNOWS);
        second.create_vertex(VId(3), vec![], vec![]);
        second.add_edge(EId(11), VId(1), VId(3), vec![]);
        let second_seq = db.write(cx, second).await?;
        println!("  committed second batch at seq {second_seq:?}");

        let bind = RelationBind::new().with_relation("KNOWS", KNOWS);
        let query = "MATCH (a)-[:KNOWS]->(b) RETURN b";

        let current = db.execute_gql(query, &bind)?;
        println!("  current neighbors of (1): {current:?}");

        let at_first = db.execute_gql_at(query, &bind, first_seq)?;
        println!("  neighbors of (1) at {first_seq:?}: {at_first:?}");

        assert_eq!(
            current,
            vec![VId(2), VId(3)],
            "current view sees both edges"
        );
        assert_eq!(
            at_first,
            vec![VId(2)],
            "historical view sees only the first edge"
        );

        let certified = db.execute_gql_certified_at(query, &bind, first_seq)?;
        println!(
            "  certified historical query at snapshot {snapshot:?}: {rows:?}",
            snapshot = certified.1.snapshot_seq,
            rows = certified.0
        );
        assert_eq!(
            certified.1.snapshot_seq, first_seq,
            "certificate pins the requested seq"
        );

        let before_any_commit = CommitSeq(0);
        let empty = db.execute_gql_at(query, &bind, before_any_commit)?;
        println!("  neighbors of (1) at {before_any_commit:?}: {empty:?}");
        assert!(
            empty.is_empty(),
            "seq 0 is the empty database before any commit"
        );

        println!(
            "OK: time-travel reads return the exact adjacency at the requested commit sequence."
        );
        Ok(())
    })
}
