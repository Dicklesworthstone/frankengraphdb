//! **A real `main()` that opens a database** (`fgdb-0b8r`).
//!
//! The spine's claim is that a person can run this database. Until this file
//! existed that claim was carried entirely by a doctest and an integration
//! suite — both of which run under `cargo test`, neither of which is a program
//! anybody could execute. This is the doctest-independent witness: a separate
//! compiled binary, with its own `main`, that creates a database, writes to it
//! through the real two-fsync commit path, reads it back, drops every handle,
//! reopens from nothing but the path and the keys, and agrees.
//!
//! ```text
//! cargo run --example open_a_database
//! ```
//!
//! # Why this is an example and not a `[[bin]]`
//!
//! `registries/workspace_topology.toml` gives the `embedded` posture
//! `binary_name = ""` — the embedded library ships no binary, and the `fgdb`
//! binary name belongs to the `cli` posture whose entry crate is `fgdb-cli`.
//! Adding a `[[bin]]` here would quietly claim a name the topology assigns
//! elsewhere. An example is a real binary with a real `main` and costs the
//! registry nothing.
//!
//! # Production runtime authority
//!
//! Pinned asupersync v0.4.6 source revision `9f7c3769` exposes
//! `Runtime::request_cx_with_budget` as its ambient-free production boundary.
//! This example uses that path with default features disabled: the `Cx` inherits
//! the runtime's drivers and capability mask, then `PurposeContexts` narrows it
//! to `CommitCx`. No `test-internals` constructor or LAB scheduler participates.
//!
//! This proves the embedded durable slice runs under the production runtime. It
//! is not a §17 performance result: the host, filesystem, and workload manifest
//! are not pinned here.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
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
    // A per-pid directory: concurrent panes run this against one /tmp, and
    // nothing is ever removed — rule 1 has no carve-out for example code.
    let path = std::env::temp_dir().join(format!("fgdb-example-{}", std::process::id()));
    let keys = DatabaseKeys {
        k_oid: [0x5a; 32],
        namespace: DatabaseSecurityNamespaceId([0x77; 32]),
        dek: [0x3c; 32],
    };

    println!("fgdb spine witness");
    println!("  database directory: {}", path.display());

    let runtime = RuntimeBuilder::new().build()?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

    runtime.block_on(async move {
        // ---- create, write, read -------------------------------------------
        let mut db = Database::create(cx, &path, keys).await?;
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(1), VId(3), vec![]);
        let seq = db.write(cx, batch).await?;

        let before = db.neighbours(VId(1), KNOWS)?;
        let root_before = db.partition_root()?;
        println!("  committed at seq {seq:?}");
        println!("  neighbours(1) before drop: {before:?}");

        // ---- drop everything ------------------------------------------------
        drop(db);
        println!("  dropped the database handle");

        // ---- reopen with nothing but the path and the keys ------------------
        let db = Database::open(cx, &path, keys).await?;
        let after = db.neighbours(VId(1), KNOWS)?;
        println!("  neighbours(1) after reopen: {after:?}");

        assert_eq!(before, after, "the reopened database must agree");
        assert_eq!(
            root_before,
            db.partition_root()?,
            "the rebuild is deterministic, so the republished root must match"
        );
        assert_eq!(
            before,
            vec![VId(2), VId(3)],
            "the fixture must be non-trivial, or agreement is cheap"
        );

        println!("OK: opened, wrote, dropped, reopened, agreed.");
        Ok(())
    })
}
