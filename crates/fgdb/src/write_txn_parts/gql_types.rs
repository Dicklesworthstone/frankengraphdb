type OverlayVertexSet = std::collections::BTreeSet<VId>;
type OverlayEdgeMap = std::collections::BTreeMap<EId, (VId, RelationId, VId)>;

/// Transaction-owned admission state. Logical predicates and traversal live in
/// fgdb_gql::algebra; these sets retain overlay and read-dependency semantics.
struct OverlayGraph {
    observed: std::collections::BTreeSet<ElementId>,
    vertices: OverlayVertexSet,
    edges: OverlayEdgeMap,
}
