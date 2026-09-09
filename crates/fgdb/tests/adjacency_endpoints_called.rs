// The former `read_conflict` is now `transaction_conflict`: the same suffix
// walk validates both read and mutation footprints. This law follows that
// validator across the write_txn_parts/ decomposition; both files are included
// so a move back cannot silently turn the assertion vacuous.
#[test]
fn transaction_conflict_calls_adjacency_endpoints() {
    let source = concat!(
        include_str!("../src/write_txn.rs"),
        include_str!("../src/write_txn_parts/finish.rs"),
    );
    assert!(
        source.contains("fn transaction_conflict"),
        "transaction_conflict must live in one of the included files, or this law has no subject"
    );
    assert!(
        source.contains("adjacency_endpoints("),
        "transaction_conflict must call the shared helper; inlining src/dst is the cheat"
    );
}
