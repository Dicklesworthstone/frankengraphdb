//! **A real `main()` that opens an in-memory database**.
//!
//! The README promises `Database::open(":memory:")`; the production posture is
//! `Database::<MemVfs>::open_memory`. This example is the runnable witness:
//! it creates a private in-memory database, writes through the real two-fsync
//! commit path (with the filesystem replaced by `MemVfs`), reads it back with
//! both adjacency and GQL, drops the handle, and shows that a fresh handle is
//! an empty, independent graph.
//!
//! ```text
//! cargo run --example open_a_memory_database
//! ```
//!
//! # Why this is an example and not a `[[bin]]`
//!
//! Same reasoning as `open_a_database.rs`: `registries/workspace_topology.toml`
//! assigns the `fgdb` binary name to the `cli` posture (`fgdb-cli`). Examples
//! are real executables that cost the registry nothing.
//!
//! # Production runtime authority
//!
//! Uses `Runtime::request_cx_with_budget` with default features disabled, the
//! same production boundary as `open_a_database.rs`.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const KNOWS: RelationId = RelationId(1);

fn main() {
    if let Err(error) = run() {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn core::error::Error + Send + Sync>> {
    let keys = DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    );

    println!("fgdb :memory: witness");

    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

    runtime.block_on(async move {
        // ---- create, write, read in RAM --------------------------------------
        let mut db = Database::<MemVfs>::open_memory(cx, keys.clone()).await?;
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        let seq = db.write(cx, batch).await?;

        let neighbours = db.neighbours(VId(1), KNOWS)?;
        let gql = db.execute_gql(
            "MATCH (a)-[:KNOWS]->(b) RETURN b",
            &RelationBind::new().with_relation("KNOWS", KNOWS),
        )?;
        println!("  committed at seq {seq:?}");
        println!("  neighbours(1): {neighbours:?}");
        println!("  GQL MATCH (1)-[:KNOWS]->(b) RETURN b: {gql:?}");

        // ---- drop the handle: the database is gone --------------------------
        drop(db);
        println!("  dropped the database handle");

        // ---- a fresh open_memory is an independent, empty database ------------
        let fresh = Database::<MemVfs>::open_memory(cx, keys).await?;
        let empty = fresh.neighbours(VId(1), KNOWS)?;
        println!("  neighbours(1) in fresh handle: {empty:?}");

        assert_eq!(neighbours, vec![VId(2)], "the graph must be readable");
        assert_eq!(gql, vec![VId(2)], "GQL must agree with adjacency");
        assert!(empty.is_empty(), "a fresh :memory: handle must be empty");

        println!("OK: opened, wrote, queried, dropped, and opened a fresh empty graph.");
        Ok(())
    })
}
