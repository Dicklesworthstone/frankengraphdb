//! Reusable graph/text/vector retrieval from one coherent resident generation.
//! Membership and incidence are demanded from the retained native indexes;
//! no per-query graph directory, text tokenization or vector build is needed.

use super::*;
use fgdb_beacon::expansion::{
    ExpansionDirection, ExpansionMembership, ExpansionNeighbors, ExpansionSpec,
    expand_from_membership,
};
use fgdb_beacon::{GraphHybridHit, GraphHybridQuery};
use fgdb_delta_types::RelationId;
use fgdb_gql::{GlaExecutionEvent, algebra::GlaDirection};
use fgdb_types::VId;
use std::cell::Cell;

impl<V: Vfs + Clone> Database<V> {
    /// Build reusable text/vector lanes AND retain their native graph source.
    ///
    /// Shares prepare_beacon_index's builder, projection and admission meter.
    /// The opt-in source pin enables search_graph without consulting a writer,
    /// including when options.as_of selects a historical cut. This retains an
    /// entire decoded immutable generation, not just the query neighborhood.
    /// Ordinary prepare_beacon_index keeps its existing two-lane retention.
    ///
    /// Refresh remains explicit: it advances the search lanes, graph source
    /// and sequence together after final acceptance. Edge-only commits need no
    /// text/vector rebuild. Old snapshots remain coherent after refresh,
    /// compaction, writer fencing or dropping. No durable subscription, graph
    /// authority, capability grant, spill or ANN-equivalence claim is added.
    pub fn prepare_beacon_graph_index(
        &self,
        cx: &QueryCx,
        options: &Options,
    ) -> Result<ResidentIndex, Error> {
        self.prepare_beacon_index_internal(cx, options, true)
    }
}

impl ResidentIndex {
    /// Whether this generation retained the native source needed for graph
    /// retrieval. False never causes implicit database access or index rebuild.
    #[must_use]
    pub fn has_graph_source(&self) -> bool {
        self.graph_source.is_some()
    }

    /// Fuse this generation's reused search lanes with native graph expansion.
    /// See PinnedIndex::search_graph for the source, limits and authority contract.
    pub fn search_graph(
        &self,
        cx: &QueryCx,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        self.snapshot().search_graph(cx, query, expansion, policy)
    }
}

impl PinnedIndex {
    #[must_use]
    pub fn has_graph_source(&self) -> bool {
        self.graph_source.is_some()
    }

    /// Search all three modalities at source_sequence(), with one allowance.
    /// The source and text/vector generations are paired only by the database;
    /// callers cannot inject a snapshot, document list or source sequence.
    /// Existing vertices without either indexed property can still be transit
    /// vertices and graph hits. Frozen label selection applies to seeds and
    /// BOTH edge endpoints; no property is reprojected during graph traversal.
    ///
    /// Membership uses the native per-vertex history lookup. Incidence uses
    /// the same exact-cut adjacency index as indexed Beacon reads. Complete
    /// bounded BFS and exact candidate fusion use the shared Beacon kernels.
    /// No vertex directory, edge graph, tokenization, corpus statistics or ANN
    /// topology is rebuilt. Each call has fresh work/result/visit allowances;
    /// candidate truncation never hides a later source or control refusal.
    ///
    /// For this demand-driven profile max_vertices AND max_visited_vertices
    /// bound retained visits, not the full selected population. max_input_edges
    /// counts admitted incidence visits (parallel IDs separately; undirected
    /// edges at each expanded endpoint). Source admission counts one per vertex
    /// membership probe and admitted incidence, plus native scratch events;
    /// its cap is min(policy.max_source_scratch, expansion.max_source_scratch).
    /// max_staging_rows is unused: there is no document staging on a search.
    /// These are logical limits, not byte-memory, lifetime retention or spill.
    /// Individual index-history lookups remain synchronous.
    ///
    /// A graph-enabled query on a two-lane-only preparation refuses Disabled;
    /// a disabled graph lane reads no seeds or graph source. Privileged API:
    /// never give this index to Warden clients. Approximate vector search and
    /// candidate-limited fusion keep their original approximation boundaries.
    pub fn search_graph(
        &self,
        cx: &QueryCx,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let work = RefCell::new(Meter::new(policy.max_work_units, |_| cx.checkpoint()));
            let result = self.search_graph_with_work(cx, query, expansion, policy, &work);
            complete(work.into_inner(), result)
        })
    }

    fn search_graph_with_work(
        &self,
        cx: &QueryCx,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
        work: &RefCell<impl WorkControl>,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        work.borrow_mut().charge(1)?;
        query.candidate_depths()?;
        if query.retrieval.k > policy.max_result_rows {
            return Err(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: policy.max_result_rows,
            }
            .into());
        }
        let request = Search::Hybrid(query.source_query());
        let (vector, text) = request.lanes();
        if vector && self.definition.index.vector.is_none() {
            return Err(BeaconError::Disabled("vector").into());
        }
        if text && self.definition.index.text.is_none() {
            return Err(BeaconError::Disabled("text").into());
        }
        request.validate(&self.definition.index, &mut SharedWork(work))?;
        let graph = if query.graph_enabled() {
            let view = self
                .graph_source
                .as_ref()
                .ok_or(BeaconError::Disabled("resident graph source"))?;
            // The view can contain a newer head when preparation/refresh chose
            // history. EVERY lookup uses source_sequence, never that newer head.
            let source = view
                .vertex_scan_source(cx, self.source_sequence)
                .map_err(Error::Source)?;
            let budget = SourceBudget {
                used: Cell::new(0),
                limit: policy
                    .max_source_scratch
                    .min(expansion.limits.max_source_scratch),
            };
            let membership = Membership {
                source,
                label: self.definition.vertex_label,
                budget: &budget,
                failure: RefCell::new(None),
            };
            let mut neighbors = Neighbors {
                view,
                at: self.source_sequence,
                membership: &membership,
                budget: &budget,
                relation: expansion.relation,
                direction: match expansion.direction {
                    ExpansionDirection::Outgoing => GlaDirection::Forward,
                    ExpansionDirection::Incoming => GlaDirection::Reverse,
                    ExpansionDirection::Undirected => GlaDirection::Undirected,
                },
                admitted: 0,
                limit: expansion.limits.max_input_edges,
            };
            let result = expand_from_membership(
                expansion.seeds,
                expansion.max_hops,
                expansion.include_seeds,
                query.graph_candidates,
                expansion.limits,
                &membership,
                &mut neighbors,
                &mut SharedWork(work),
            );
            // The narrow Beacon source interface never flattens a native read
            // failure into a successful omission or a permanent generic error.
            if let Some(error) = membership.failure.borrow_mut().take() {
                return Err(Error::Source(error));
            }
            result?
        } else {
            Vec::new()
        };
        let rows = self
            .index
            .hybrid_search_graph(query, &graph, &mut SharedWork(work))?;
        work.borrow_mut().charge(1)?;
        Ok(rows)
    }
}

struct SourceBudget {
    used: Cell<usize>,
    limit: usize,
}
impl SourceBudget {
    fn reserve(&self) -> Result<(), BeaconError> {
        let used = self.used.get();
        if used >= self.limit {
            return Err(BeaconError::ResourceLimit {
                resource: "resident graph source admissions",
                limit: self.limit,
            });
        }
        self.used.set(used + 1);
        Ok(())
    }
}

struct Membership<'a, S> {
    source: S,
    label: Option<LabelId>,
    budget: &'a SourceBudget,
    failure: RefCell<Option<ReadError>>,
}
impl<S: VertexScanSource<Error = ReadError>> ExpansionMembership for Membership<'_, S> {
    fn contains_vertex(&self, id: VId, work: &mut dyn WorkControl) -> Result<bool, BeaconError> {
        if self.failure.borrow().is_some() {
            return Err(BeaconError::Invariant("resident graph source refused"));
        }
        work.charge(1)?;
        self.budget.reserve()?;
        let row = self.source.vertex(id, &mut |event| {
            work.charge(1)?;
            if matches!(event, VertexScanEvent::ScratchEntry) {
                self.budget.reserve()?;
            }
            Ok::<_, BeaconError>(())
        });
        let row = match row {
            Ok(row) => row,
            Err(VertexScanSourceError::Control(error)) => return Err(error),
            Err(VertexScanSourceError::Source(error)) => {
                *self.failure.borrow_mut() = Some(error);
                return Err(BeaconError::Invariant("resident graph source refused"));
            }
        };
        let Some(row) = row else { return Ok(false) };
        work.charge(row.labels.len())?;
        Ok(self
            .label
            .is_none_or(|label| row.labels.binary_search(&label).is_ok()))
    }
}

// A thin native-index adapter for the membership-driven kernel. No edge bag,
// graph interpreter or cache of caller-asserted topology is introduced.
struct Neighbors<'a, M> {
    view: &'a EmbeddedReadView,
    at: CommitSeq,
    membership: &'a M,
    budget: &'a SourceBudget,
    relation: Option<RelationId>,
    direction: GlaDirection,
    admitted: usize,
    limit: usize,
}
impl<M: ExpansionMembership> ExpansionNeighbors for Neighbors<'_, M> {
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        let snapshot = &self.view.snapshot;
        let mut after = None;
        loop {
            let next = snapshot.adjacency_index().next_incident_edge(
                vertex,
                self.direction,
                after,
                &mut |event| {
                    work.charge(1)?;
                    if matches!(event, GlaExecutionEvent::ScratchEntry) {
                        self.budget.reserve()?;
                    }
                    Ok::<_, BeaconError>(())
                },
            )?;
            let Some(eid) = next else { break };
            after = Some(eid);
            work.charge(1)?;
            let Some((block, row)) =
                snapshot
                    .adjacency_index()
                    .statement_at(&snapshot.blocks, eid, self.at)
            else {
                continue;
            };
            let edge = &snapshot.blocks[block][row];
            let neighbor = match self.direction {
                GlaDirection::Forward if edge.src == vertex => edge.dst,
                GlaDirection::Reverse if edge.dst == vertex => edge.src,
                GlaDirection::Undirected if edge.src == vertex => edge.dst,
                GlaDirection::Undirected if edge.dst == vertex => edge.src,
                _ => continue,
            };
            if self
                .relation
                .is_some_and(|relation| relation != edge.relation)
                || !self.membership.contains_vertex(edge.src, work)?
                || !self.membership.contains_vertex(edge.dst, work)?
            {
                continue;
            }
            work.charge(1)?;
            if self.admitted >= self.limit {
                return Err(BeaconError::ResourceLimit {
                    resource: "indexed expansion incidence visits",
                    limit: self.limit,
                });
            }
            self.budget.reserve()?;
            self.admitted += 1;
            visit(neighbor, work)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "resident_graph_tests.rs"]
mod tests;
