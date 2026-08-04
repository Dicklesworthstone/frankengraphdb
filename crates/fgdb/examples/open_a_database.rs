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
//! # THE RUNTIME CAVEAT, WHICH IS THE WHOLE POINT OF `fgdb-0b8r`
//!
//! This runs under **asupersync's LAB runtime**, and that is not a stylistic
//! choice — it is the only option that exists. Measured at pinned revision
//! `3e8d08e`:
//!
//! - `fgdb::Database` needs a `&CommitCx`, which comes only from
//!   `PurposeContexts::narrow_runtime_root(&Cx<cap::All>)`.
//! - `Cx::for_testing()` and `Cx::for_request()` are
//!   `#[cfg(any(test, feature = "test-internals"))]`, and asupersync's
//!   `[features]` table **does not define `test-internals`**.
//! - `Runtime::request_cx_with_budget` — which asupersync's own doc names as the
//!   sanctioned production path, *"the only ambient-free way to mint a Cx in
//!   production"* — is declared **`pub(crate)`**. Every call site is inside
//!   `#[cfg(test)] mod tests`.
//! - No public method on `Runtime` returns a `Cx` at all.
//!
//! So the documented production path is contradicted by its own visibility
//! modifier, and `run_async_under_lab` is the only public, ungated way any
//! external crate can obtain a `Cx`. This binary therefore proves the spine is
//! **consumable from a program**, which was in doubt; it does **not** prove the
//! database runs on a production runtime, which is still blocked upstream.
//!
//! Nothing here is labelled as a performance result, and it must not be used as
//! one: the lab scheduler is deterministic and serialized, so timings taken
//! under it cannot speak to §17's across-cores throughput or p99 gates.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const KNOWS: RelationId = RelationId(1);

fn main() {
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

    let (outcome, report) = run_async_under_lab(1, move |root| async move {
        let cx = &PurposeContexts::narrow_runtime_root(&root).commit();

        // ---- create, write, read -------------------------------------------
        let mut db = Database::create(cx, &path, keys)?;
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(1), VId(3), vec![]);
        let seq = db.write(cx, batch)?;

        let before = db.neighbours(VId(1), KNOWS)?;
        let root_before = db.partition_root();
        println!("  committed at seq {seq:?}");
        println!("  neighbours(1) before drop: {before:?}");

        // ---- drop everything ------------------------------------------------
        drop(db);
        println!("  dropped the database handle");

        // ---- reopen with nothing but the path and the keys ------------------
        let db = Database::open(cx, &path, keys)?;
        let after = db.neighbours(VId(1), KNOWS)?;
        println!("  neighbours(1) after reopen: {after:?}");

        assert_eq!(before, after, "the reopened database must agree");
        assert_eq!(
            root_before,
            db.partition_root(),
            "the rebuild is deterministic, so the republished root must match"
        );
        assert_eq!(
            before,
            vec![VId(2), VId(3)],
            "the fixture must be non-trivial, or agreement is cheap"
        );

        Ok::<(), Box<dyn core::error::Error + Send + Sync>>(())
    });

    if let Err(error) = outcome {
        eprintln!("FAILED: {error}");
        std::process::exit(1);
    }
    if !report.lab_test_passed() {
        eprintln!("FAILED: lab run did not pass: {report:?}");
        std::process::exit(1);
    }
    println!("OK: opened, wrote, dropped, reopened, agreed.");
}
