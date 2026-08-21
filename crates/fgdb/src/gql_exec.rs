use crate::{Database, ReadError};
use asupersync::fs::Vfs;
use fgdb_gql::BoundPlan;
use fgdb_types::VId;

/// Execute the pinned bound MATCH expansion over the database's live Strata view.
pub(crate) fn execute<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
) -> Result<Vec<VId>, ReadError> {
    let mut destinations = Vec::new();
    for vertex in db.vertices()? {
        destinations.extend(db.neighbours(vertex.vid, plan.relation)?);
    }
    destinations.sort_unstable();
    destinations.dedup();
    Ok(destinations)
}
