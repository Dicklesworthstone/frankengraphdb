#[test]
fn gql_exec_does_not_scan_vertices_then_call_neighbours() {
    let source = include_str!("../src/gql_exec.rs");
    // A vertices()+neighbours() executor is the cheat this witness forbids.
    assert!(!source.contains("vertices("));
    assert!(!source.contains("neighbours("));
}
