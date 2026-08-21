//! The product MATCH executors, all riding ONE kernel
//! (fgdb-gql-one-kernel-7y17): live, as-of, and the transaction overlay
//! (`write_txn.rs`) each hand `crate::execute_bound_plan_over` their own
//! source set and expansion, so the row discipline — destinations ascending,
//! deduplicated — has exactly one owner and cannot drift between faces.

use crate::{Database, ReadError};
use asupersync::fs::Vfs;
use fgdb_gql::BoundPlan;
use fgdb_types::{CommitSeq, VId};

/// Execute the pinned bound MATCH expansion over the database's live Strata
/// view. Sources come from one pass over the admitted edge table — the
/// vertices that actually have a matching-relation edge out — never from a
/// `vertices()` outer loop; expansion is the ordinary admitted-block
/// neighbour scan.
pub(crate) fn execute<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
) -> Result<Vec<VId>, ReadError> {
    let sources: std::collections::BTreeSet<VId> = db
        .edges()?
        .into_iter()
        .filter_map(|record| (record.entry.relation == plan.relation).then_some(record.entry.src))
        .collect();
    crate::execute_bound_plan_over(plan, sources, |src, relation| db.neighbours(src, relation))
}

/// Execute the pinned bound MATCH expansion at one historical frontier —
/// the same kernel, with the source pass and the expansion both pinned to
/// `as_of`.
pub(crate) fn execute_at<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
    as_of: CommitSeq,
) -> Result<Vec<VId>, ReadError> {
    let sources: std::collections::BTreeSet<VId> = db
        .edges_at(as_of)?
        .into_iter()
        .filter_map(|record| (record.entry.relation == plan.relation).then_some(record.entry.src))
        .collect();
    crate::execute_bound_plan_over(plan, sources, |src, relation| {
        db.neighbours_at(src, relation, as_of)
    })
}
