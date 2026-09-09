/// Query-local topology candidates. Storage and prepared effects remain the
/// authority; these metadata entries do not carry or clone property payloads.
type OverlayEdgeMap = std::collections::BTreeMap<EId, (VId, RelationId, VId)>;
