#[test]
fn gql_exec_does_not_expand_through_per_vertex_neighbour_calls() {
    let source = include_str!("../src/gql_exec.rs");
    // A vertices()+neighbours() executor is the cheat this witness forbids:
    // pattern expansion must come from ONE pass over the admitted edge table,
    // never from per-source adjacency lookups.
    assert!(
        !source.contains("neighbours(") && !source.contains("neighbours_at("),
        "MATCH expansion reads the admitted edge table once; per-source \
         neighbour calls are the cheat"
    );
    // Node-only MATCH (`MATCH (n)` has no edge table to scan) legitimately
    // reads the vertex table exactly once per execution arm, and only as the
    // node-scan fallback. Any OTHER vertices()/vertices_at() read would be a
    // vertex-loop executor sneaking back in, so every read must be one of
    // those fallback arms — equal counts prove nothing else appeared.
    assert_eq!(
        source.matches("db.vertices").count(),
        source
            .matches("return Ok(node_scan(plan, db.vertices")
            .count(),
        "the only permitted vertex-table reads are the node-only scan \
         fallbacks in execute/execute_at"
    );
}
