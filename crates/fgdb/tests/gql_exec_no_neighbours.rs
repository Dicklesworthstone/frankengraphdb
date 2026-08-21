#[test]
fn match_executor_does_not_call_neighbours() {
    let source = include_str!("../src/gql_exec.rs");
    assert!(
        !source.contains("neighbours(") && !source.contains("neighbours_at("),
        "live/as-of MATCH is one edge-table pass; a neighbours scan is the cheat"
    );
}
