// `read_conflict` moved from src/write_txn.rs into the write_txn_parts/
// decomposition (finish.rs) on 2026-09-01; this law follows the function, not
// the file that used to hold it. Both files are included so a move back cannot
// silently turn the assertion vacuous.
#[test]
fn read_conflict_calls_adjacency_endpoints() {
    let source = concat!(
        include_str!("../src/write_txn.rs"),
        include_str!("../src/write_txn_parts/finish.rs"),
    );
    assert!(
        source.contains("fn read_conflict"),
        "read_conflict must live in one of the included files, or this law has no subject"
    );
    assert!(
        source.contains("adjacency_endpoints("),
        "read_conflict must call the shared helper; inlining src/dst is the cheat"
    );
}
