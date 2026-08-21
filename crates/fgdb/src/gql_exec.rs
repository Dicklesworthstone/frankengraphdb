use crate::{Database, ReadError};
use asupersync::fs::Vfs;
use fgdb_gql::BoundPlan;
use fgdb_types::{CommitSeq, VId};

/// Execute the pinned bound MATCH expansion over the database's live Strata view.
pub(crate) fn execute<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
) -> Result<Vec<VId>, ReadError> {
    let mut destinations: Vec<VId> = db
        .edges()?
        .into_iter()
        .filter_map(|record| {
            (record.entry.relation == plan.relation).then_some(record.entry.dst)
        })
        .collect();
    destinations.sort_unstable();
    destinations.dedup();
    Ok(destinations)
}

/// Execute the pinned bound MATCH expansion at one historical frontier.
pub(crate) fn execute_at<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
    as_of: CommitSeq,
) -> Result<Vec<VId>, ReadError> {
    let mut destinations: Vec<VId> = db
        .edges_at(as_of)?
        .into_iter()
        .filter_map(|record| {
            (record.entry.relation == plan.relation).then_some(record.entry.dst)
        })
        .collect();
    destinations.sort_unstable();
    destinations.dedup();
    Ok(destinations)
}
