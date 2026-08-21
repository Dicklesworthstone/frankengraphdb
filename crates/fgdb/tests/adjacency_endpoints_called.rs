#[test]
fn read_conflict_calls_adjacency_endpoints() {
    let source = include_str!("../src/write_txn.rs");
    assert!(
        source.contains("adjacency_endpoints("),
        "read_conflict must call the shared helper; inlining src/dst is the cheat"
    );
}
