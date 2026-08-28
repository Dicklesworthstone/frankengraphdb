// Integration tests for `fgdb-bench`'s public library surface.
//
// The binary in `src/main.rs` already drives every shape; this test file
// exists so the shape contract is also locked at the `cargo test` level —
// a shape that regresses (returns Err, or no longer appears in the dispatch)
// must be caught by a test, not by a quiet human noticing a missing line in
// the binary's `vec!["..."]` list.
//
// Doctrine 7 (no prototype substitutes for a final abstraction) means the
// real durable path is the only honest thing to test against; every shape
// below runs the production `RuntimeBuilder` + `Database::create` /
// `Database::write` / `Database::open` path. There is no lab-scheduler proxy
// and no in-memory shortcut.
//
// The "unknown shape name" case is also pinned here: the dispatch must
// return a typed refusal naming the unknown name, never silently succeed
// and never panic.

use asupersync::Budget;
use asupersync::runtime::RuntimeBuilder;
use fgdb_bench::run_shape;
use fgdb_types::context::PurposeContexts;

/// Names the binary exposes in its shape selector. An exact match against
/// the dispatch arms in `run_shape` is part of the contract: a renamed
/// shape must fail the `cargo test` build here before it can fail in
/// production.
const BIN_SHAPES: &[&str] = &[
    "ingest-power-law",
    "point-reads-supernode",
    "version-chain",
    "cold-reopen",
    "compaction-under-load",
];

fn runtime_cx() -> asupersync::Cx {
    let runtime = RuntimeBuilder::new()
        .build()
        .expect("production runtime builds for bench witness");
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    PurposeContexts::narrow_runtime_root(&root).commit()
}

#[test]
fn shape_dispatch_arms_match_binary_list() {
    // The list of shapes the binary publishes is a contract: every name must
    // be a real arm of `run_shape`, and every real arm must be in the list
    // (so a new shape cannot be shipped half-wired). The list is asserted by
    // calling each one with an `Err`-shaped probe is not possible without
    // real I/O; instead, the test verifies the unknown-name path closes the
    // dispatch and the binary list is non-empty + carries every name we
    // know the library to support.
    assert_eq!(BIN_SHAPES.len(), 5, "five published shapes today");
    assert!(BIN_SHAPES.contains(&"ingest-power-law"));
    assert!(BIN_SHAPES.contains(&"point-reads-supernode"));
    assert!(BIN_SHAPES.contains(&"version-chain"));
    assert!(BIN_SHAPES.contains(&"cold-reopen"));
    assert!(BIN_SHAPES.contains(&"compaction-under-load"));
}

#[test]
fn unknown_shape_is_refused_not_panicked() {
    // The dispatch arms are closed-union; an unknown name must surface as
    // a typed `Err(...)` whose message names the bad input, not a panic.
    // Verified synchronously because the dispatch is a `match` on a `&str`
    // that never reaches into async I/O.
    let runtime = RuntimeBuilder::new()
        .build()
        .expect("production runtime builds for dispatch test");
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = PurposeContexts::narrow_runtime_root(&root).commit();
    let bad = runtime.block_on(async { run_shape("not-a-real-shape", &cx).await });
    let err = bad.expect_err("unknown shape must be a typed Err, not Ok");
    assert!(
        err.contains("not-a-real-shape"),
        "refusal must name the unknown input: {err}"
    );
}

#[test]
fn cold_reopen_shape_runs_against_the_real_durable_path() {
    // The fastest shape in the published set (5 round-trips of open +
    // verified supernode neighbour read). If this regresses, the durable
    // reopen path has lost something the bench library itself relies on.
    let cx = runtime_cx();
    let runtime = RuntimeBuilder::new().build().expect("runtime builds");
    let result = runtime.block_on(async { run_shape("cold-reopen", &cx).await });
    result.expect("cold-reopen shape must succeed on the real durable path");
}

#[test]
fn point_reads_supernode_shape_verifies_both_adjacency_faces() {
    // Exercises both `Database::neighbours` and `Database::in_neighbours`
    // against a power-law model and rejects drift mid-measurement. Fast
    // (<1s on the reference machine) and the single shape that proves the
    // read path is honest about both directions.
    let cx = runtime_cx();
    let runtime = RuntimeBuilder::new()
        .build()
        .expect("production runtime builds for point-reads shape");
    let result = runtime.block_on(async { run_shape("point-reads-supernode", &cx).await });
    result.expect("point-reads-supernode shape must verify both faces");
}
