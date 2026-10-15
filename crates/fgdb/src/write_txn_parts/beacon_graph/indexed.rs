//! Seed-directed incidence over the pinned native history plus prepared net.
//! Only changed incidences are indexed in query scratch. Neither raw ENSURE
//! intents nor a writer's newer graph may replace the canonical overlay.

use super::*;
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionNeighbors, GraphHit};
use fgdb_gql::{GlaExecutionEvent, algebra::GlaDirection};
use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Included};

#[path = "demand.rs"]
mod demand;

impl WriteTxn {
    /// Seed-directed graph/text/vector retrieval over this transaction's
    /// pinned basis AND prepared writes. No whole edge graph is copied; only
    /// changed incidences have a temporary successor directory. Historical
    /// winners and staged replacements are merged by full-width EId before
    /// relation, direction and selected-endpoint checks.
    ///
    /// The selected vertex corpus and any requested text/vector index are
    /// still prepared by the ordinary transaction source. Graph-only requests
    /// project no properties and build no search index. Disabled graph lanes,
    /// zero hops and absent seeds demand no edge source or staged-edge index.
    /// Every enabled lane shares one allowance and the same result predicate.
    ///
    /// max_input_edges counts admitted reached INCIDENCES (parallel EIds
    /// separately, undirected edges once per expanded endpoint); graph source
    /// scratch counts prepared effects, staged successor entries, new read
    /// witnesses and incidence admissions. Source and GQL limits also apply.
    /// All limits refuse without a successful prefix; no materialized retry.
    ///
    /// Before first adjacency demand, retain the existing conservative GLOBAL
    /// edge-scan witness, not a claim of precise range predicates or full SSI.
    /// Empty results, errors and savepoint rollback cannot discard observations.
    /// This is a privileged, resident API, not a capability or spill guarantee.
    pub fn beacon_search_graph_indexed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
    ) -> Result<Vec<GraphHybridHit>, ReadError<WriteTxnError, Cancel>> {
        self.beacon_search_graph_indexed_filtered(database, cx, options, query, expansion, |_| true)
    }

    /// Filter result endpoints BEFORE all candidate limits, never graph transit
    /// or the text/vector corpus. The host predicate must be stable, bounded
    /// computation over IDs, not I/O or an authorization check. Derive it from
    /// this same pinned overlay. Native eligible lists additionally meter their
    /// own membership work. See [`Self::beacon_search_graph_indexed`].
    #[allow(clippy::too_many_arguments)]
    pub fn beacon_search_graph_indexed_filtered<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, ReadError<WriteTxnError, Cancel>> {
        self.beacon_search_graph_with_access_observed(
            database,
            cx,
            options,
            query,
            expansion,
            true,
            None,
            |_| Ok(()),
            |id, _| eligible(id),
            |result| result,
        )
    }
}

struct Changes {
    by_id: BTreeMap<EId, Option<Incidence>>,
    /// Ordered (anchor, edge) successors for the requested direction. A self
    /// loop occupies one entry even in the undirected profile.
    incidence: BTreeSet<(VId, EId)>,
}

/// Preparing changed-edge successors may prune with a complete directory,
/// but a demand source must retain them without probing unrelated vertices.
/// In both cases actual incidence admission checks effective membership.
trait Selection {
    fn retain_staged(&self, src: VId, dst: VId) -> bool;
    fn contains_vertex(&self, id: VId, work: &mut dyn WorkControl) -> Result<bool, BeaconError>;
}

impl Selection for ExpansionGraph {
    fn retain_staged(&self, src: VId, dst: VId) -> bool {
        self.contains(src) && self.contains(dst)
    }

    fn contains_vertex(&self, id: VId, _: &mut dyn WorkControl) -> Result<bool, BeaconError> {
        Ok(self.contains(id))
    }
}

struct OverlayNeighbors<'a, 'cx, S> {
    txn: &'a WriteTxn,
    snapshot: &'a Snapshot,
    selected: &'a S,
    relation: Option<RelationId>,
    direction: GlaDirection,
    edge_work: EdgeWork<'a, 'cx>,
    changes: Option<Changes>,
    admitted: usize,
    limit: usize,
}

fn tree_work(entries: usize) -> usize {
    1 + entries.checked_ilog2().unwrap_or(0) as usize
}

impl<S: Selection> OverlayNeighbors<'_, '_, S> {
    fn prepare(&mut self) -> Result<(), BeaconError> {
        if self.changes.is_some() {
            return Ok(());
        }
        self.edge_work.source(SourceEvent::Work)?;
        // Includes an empty adjacency and failures while constructing changes.
        // No predicate refinement is silently substituted for the FCW contract.
        self.txn.scanned_edges.set(true);
        let by_id = self.txn.beacon_graph_changes(&self.edge_work)?;
        let mut incidence = BTreeSet::new();
        for (&eid, &after) in &by_id {
            self.edge_work.source(SourceEvent::Work)?;
            let Some((src, relation, dst)) = after else {
                continue;
            };
            if self.relation.is_some_and(|wanted| wanted != relation)
                || !self.selected.retain_staged(src, dst)
            {
                continue;
            }
            let mut insert = |anchor| -> Result<(), BeaconError> {
                self.edge_work
                    .shared
                    .borrow_mut()
                    .charge(tree_work(incidence.len()))?;
                self.edge_work.source(SourceEvent::ScratchEntry)?;
                incidence.insert((anchor, eid));
                Ok(())
            };
            match self.direction {
                GlaDirection::Forward => insert(src)?,
                GlaDirection::Reverse => insert(dst)?,
                GlaDirection::Undirected => {
                    insert(src)?;
                    if src != dst {
                        insert(dst)?;
                    }
                }
            }
        }
        self.changes = Some(Changes { by_id, incidence });
        Ok(())
    }

    fn next_staged(&self, vertex: VId, after: Option<EId>) -> Result<Option<EId>, BeaconError> {
        let changes = self.changes.as_ref().ok_or(BeaconError::Invariant(
            "transaction incidence source was not prepared",
        ))?;
        self.edge_work
            .shared
            .borrow_mut()
            .charge(tree_work(changes.incidence.len()))?;
        let start = after.map_or(Included((vertex, EId(0))), |id| Excluded((vertex, id)));
        // Exclusive successors avoid overflow at u128::MAX and admit EId(0).
        Ok(changes
            .incidence
            .range((start, Included((vertex, EId(u128::MAX)))))
            .next()
            .map(|&(_, id)| id))
    }
}

impl<S: Selection> ExpansionNeighbors for OverlayNeighbors<'_, '_, S> {
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        self.prepare()?;
        let mut after = None;
        loop {
            let native = self.snapshot.adjacency_index().next_incident_edge(
                vertex,
                self.direction,
                after,
                &mut |event| {
                    self.edge_work.source(match event {
                        GlaExecutionEvent::ScratchEntry => SourceEvent::ScratchEntry,
                        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => SourceEvent::Work,
                    })
                },
            )?;
            let staged = self.next_staged(vertex, after)?;
            let next = match (native, staged) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let Some(eid) = next else { return Ok(()) };
            after = Some(eid);
            let changes = self.changes.as_ref().ok_or(BeaconError::Invariant(
                "transaction incidence source lost prepared changes",
            ))?;
            self.edge_work
                .shared
                .borrow_mut()
                .charge(tree_work(changes.by_id.len()))?;
            let incidence = match changes.by_id.get(&eid) {
                // Do NOT consume a replacement: undirected traversal may need
                // it again at its other endpoint. A tombstone hides all history.
                Some(after) => *after,
                None => self
                    .snapshot
                    .adjacency_index()
                    .statement_at_controlled(
                        &self.snapshot.blocks,
                        eid,
                        self.txn.basis,
                        &mut |event| self.edge_work.source(event),
                    )?
                    .map(|(block, row)| {
                        let edge = &self.snapshot.blocks[block][row];
                        (edge.src, edge.relation, edge.dst)
                    }),
            };
            let Some((src, relation, dst)) = incidence else {
                continue;
            };
            let neighbor = match self.direction {
                GlaDirection::Forward if src == vertex => dst,
                GlaDirection::Reverse if dst == vertex => src,
                GlaDirection::Undirected if src == vertex => dst,
                GlaDirection::Undirected if dst == vertex => src,
                _ => continue,
            };
            self.edge_work.observe(self.txn, ElementId::Edge(eid))?;
            self.edge_work.observe(self.txn, ElementId::Vertex(src))?;
            self.edge_work.shared.borrow_mut().record()?;
            if self.relation.is_some_and(|wanted| wanted != relation)
                || !self.selected.contains_vertex(src, work)?
                || !self.selected.contains_vertex(dst, work)?
            {
                continue;
            }
            self.edge_work.source(SourceEvent::Work)?;
            if self.admitted == self.limit {
                return Err(BeaconError::ResourceLimit {
                    resource: "transaction indexed incidence visits",
                    limit: self.limit,
                });
            }
            self.edge_work.source(SourceEvent::ScratchEntry)?;
            self.admitted += 1;
            visit(neighbor, work)?;
        }
    }
}

pub(super) fn expand(
    txn: &WriteTxn,
    snapshot: &Snapshot,
    selected: &ExpansionGraph,
    expansion: ExpansionSpec<'_, RelationId>,
    candidates: u32,
    work: &RefCell<Control<'_>>,
    eligible: impl Fn(VId) -> bool,
) -> Result<Vec<GraphHit>, BeaconError> {
    let mut neighbors = OverlayNeighbors {
        txn,
        snapshot,
        selected,
        relation: expansion.relation,
        direction: match expansion.direction {
            ExpansionDirection::Outgoing => GlaDirection::Forward,
            ExpansionDirection::Incoming => GlaDirection::Reverse,
            ExpansionDirection::Undirected => GlaDirection::Undirected,
        },
        edge_work: EdgeWork {
            shared: work,
            scratch: Cell::new(0),
            limit: expansion.limits.max_source_scratch,
        },
        changes: None,
        admitted: 0,
        limit: expansion.limits.max_input_edges,
    };
    // Staged indexing is lazy: zero-hop/absent seeds never demand adjacency.
    // The shared BFS completes bounded traversal before endpoint truncation.
    selected.expand_from_source_filtered(
        expansion.seeds,
        expansion.max_hops,
        expansion.include_seeds,
        candidates,
        &mut neighbors,
        |id, _| Ok(eligible(id)),
        &mut Shared(work),
    )
}

#[cfg(test)]
#[path = "indexed_tests.rs"]
mod tests;
