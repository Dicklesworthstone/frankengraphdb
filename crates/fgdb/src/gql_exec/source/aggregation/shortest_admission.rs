//! Hop-bounded source closure for the existing native shortest-walk cursor.
//! This chooses input arcs, not results: lower-bound settlement and tied-walk
//! multiplicity remain exclusively the GQL cursor's responsibility.

use crate::gql_exec::source::{AdjacencyIndex, SourceEvent};
use fgdb_delta_types::RelationId;
use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::algebra::GlaDirection;
use fgdb_strata::AdjacencyEntry;
use fgdb_types::{CommitSeq, EId, VId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Clone, Copy)]
pub(super) struct Scope {
    pub(super) source: VId,
    pub(super) relation: RelationId,
    pub(super) direction: GlaDirection,
    pub(super) as_of: CommitSeq,
    pub(super) maximum: u32,
}

/// Borrow only topology from the caller's already admitted generation.
/// Every edge used by a walk of at most `maximum` hops leaves a vertex whose
/// minimum distance from the source is less than `maximum`. Expanding each such
/// vertex once therefore supplies ALL possible walks in that interval, even
/// those revisiting a vertex before an eventual lower-bound settlement. This
/// proof does not depend on path multiplicity or the requested minimum.
pub(super) fn collect<E>(
    index: &AdjacencyIndex,
    blocks: &[Vec<AdjacencyEntry>],
    scope: Scope,
    control: &mut impl FnMut(SourceEvent) -> Result<(), E>,
) -> Result<BTreeMap<VId, Vec<VId>>, E> {
    let mut adjacency = BTreeMap::new();
    if scope.maximum == 0 {
        return Ok(adjacency);
    }
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::new();
    let mut admitted = BTreeSet::<EId>::new();
    control(SourceEvent::ScratchEntry)?;
    seen.insert(scope.source);
    control(SourceEvent::ScratchEntry)?;
    queue.push_back((scope.source, 0_u32));
    while let Some((vertex, depth)) = queue.pop_front() {
        control(SourceEvent::Work)?;
        // Queue admission excludes the terminal layer, including at u32::MAX.
        debug_assert!(depth < scope.maximum);
        let next_depth = depth + 1;
        let mut neighbors = BTreeSet::new();
        let mut after = None;
        while let Some(eid) =
            index.next_incident_edge(vertex, scope.direction, after, &mut |event| {
                control(match event {
                    GlaExecutionEvent::ScratchEntry => SourceEvent::ScratchEntry,
                    GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => SourceEvent::Work,
                })
            })?
        {
            control(SourceEvent::Work)?;
            // Strict successors advance even past retired, future or unrelated
            // candidates. Never add one to the cursor just because it is indexed.
            after = Some(eid);
            let Some((block, row)) =
                index.statement_at_controlled(blocks, eid, scope.as_of, control)?
            else {
                continue;
            };
            let entry = &blocks[block][row];
            if entry.relation != scope.relation {
                continue;
            }
            let neighbor = match scope.direction {
                GlaDirection::Forward if entry.src == vertex => entry.dst,
                GlaDirection::Reverse if entry.dst == vertex => entry.src,
                GlaDirection::Undirected if entry.src == vertex => entry.dst,
                GlaDirection::Undirected if entry.dst == vertex => entry.src,
                _ => continue,
            };
            // One source record per physical edge. Both undirected faces retain
            // their occurrences without charging the source identity twice.
            if !admitted.contains(&eid) {
                control(SourceEvent::SnapshotRecord)?;
                control(SourceEvent::ScratchEntry)?;
                admitted.insert(eid);
            }
            // EIds preserve parallel occurrences; sorting by neighbor first
            // reproduces the resident cursor's canonical adjacency order.
            control(SourceEvent::ScratchEntry)?;
            neighbors.insert((neighbor, eid));
            if next_depth < scope.maximum && !seen.contains(&neighbor) {
                control(SourceEvent::ScratchEntry)?;
                seen.insert(neighbor);
                control(SourceEvent::ScratchEntry)?;
                queue.push_back((neighbor, next_depth));
            }
        }
        if !neighbors.is_empty() {
            control(SourceEvent::ScratchEntry)?;
            let mut occurrences = Vec::new();
            for (neighbor, _) in neighbors {
                control(SourceEvent::ScratchEntry)?;
                occurrences.push(neighbor);
            }
            adjacency.insert(vertex, occurrences);
        }
    }
    Ok(adjacency)
}

#[cfg(test)]
#[path = "shortest_admission_tests.rs"]
mod tests;
