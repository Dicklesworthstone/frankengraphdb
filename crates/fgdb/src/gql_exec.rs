//! The product MATCH executors, all riding ONE kernel
//! (fgdb-gql-one-kernel-7y17): live, as-of, and the transaction overlay
//! (`write_txn.rs`) each hand `crate::execute_bound_plan_over` their own
//! source set and expansion, so the row discipline — projected vertex IDs
//! ascending, deduplicated — has exactly one owner and cannot drift between
//! faces. The parser normalizes either arrow spelling into edge source and
//! destination variables, so execution always builds source-to-destinations
//! adjacency in one pass over the admitted edge table.

use crate::{Database, ReadError};
use asupersync::fs::Vfs;
use fgdb_gql::BoundPlan;
use fgdb_types::{CommitSeq, VId};

/// Execute the pinned bound MATCH expansion over the database's live Strata
/// view.
pub(crate) fn execute<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
) -> Result<Vec<VId>, ReadError> {
    let mut adjacency = std::collections::BTreeMap::<VId, Vec<VId>>::new();
    for record in db.edges()? {
        if record.entry.relation == plan.relation {
            adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
        }
    }
    let sources: Vec<_> = adjacency.keys().copied().collect();
    crate::execute_bound_plan_over(plan, sources, |src, _| {
        Ok(adjacency.get(&src).cloned().unwrap_or_default())
    })
}

/// Execute the pinned bound MATCH expansion at one historical frontier —
/// the same kernel, with the anchor pass and the expansion both pinned to
/// `as_of`.
pub(crate) fn execute_at<V: Vfs + Clone>(
    plan: &BoundPlan,
    db: &Database<V>,
    as_of: CommitSeq,
) -> Result<Vec<VId>, ReadError> {
    let mut adjacency = std::collections::BTreeMap::<VId, Vec<VId>>::new();
    for record in db.edges_at(as_of)? {
        if record.entry.relation == plan.relation {
            adjacency
                .entry(record.entry.src)
                .or_default()
                .push(record.entry.dst);
        }
    }
    let sources: Vec<_> = adjacency.keys().copied().collect();
    crate::execute_bound_plan_over(plan, sources, |src, _| {
        Ok(adjacency.get(&src).cloned().unwrap_or_default())
    })
}
