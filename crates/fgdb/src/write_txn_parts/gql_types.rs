/// Query-local topology candidates. Storage and prepared effects remain the
/// authority; these metadata entries do not carry or clone property payloads.
#[allow(dead_code)]
type OverlayEdgeMap = std::collections::BTreeMap<EId, (VId, RelationId, VId)>;
