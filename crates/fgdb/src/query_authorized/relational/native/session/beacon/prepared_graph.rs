//! Reuse an authorized index's text/vector lanes with demand-driven topology
//! from the SAME session and historical cut. There is no per-read corpus,
//! vertex-directory or edge-graph rebuild, and no new graph authority.

use super::*;
use crate::{EmbeddedReadView, ReadError, Snapshot};
use fgdb_beacon::expansion::{
    ExpansionCursor, ExpansionDirection, ExpansionMembership, ExpansionNeighbors, ExpansionRequest,
    ExpansionSpec, GraphHit,
};
use fgdb_beacon::{GraphHybridHit, GraphHybridQuery};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::GqlQueryPolicy;
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::stream::{VertexScanSource, VertexScanSourceError};
use fgdb_warden::PlannerPredicates;
use std::cell::Cell;

/// Forward kernel scratch reservations into the host's already-selected
/// cumulative admission callback without creating a second work allowance.
pub(super) struct ScratchWork<'a, W, A> {
    pub(super) work: &'a RefCell<W>,
    pub(super) admit: &'a A,
}

impl<W: WorkControl, A: Fn(usize) -> Result<(), BeaconError>> WorkControl
    for ScratchWork<'_, W, A>
{
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        self.work.borrow_mut().charge(units)
    }

    fn reserve_scratch(&mut self, entries: usize) -> Result<(), BeaconError> {
        (self.admit)(entries)
    }
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Fuse this session's prepared text/vector generation with seed-directed
    /// graph expansion. Use an existing `prepare_beacon_index` handle; no
    /// second preparation or graph copy is needed. Graph-only queries can use
    /// the same handle without searching either indexed lane.
    ///
    /// Every topology lookup uses the prepared source sequence, not the
    /// session's potentially newer frontier or the writer's current head.
    /// Original-label authorization, the frozen user label and allowed
    /// relations govern seeds and BOTH edge endpoints before admission. A
    /// vertex without text/vector properties can still be a transit node/hit.
    /// Properties are never reprojected; BM25 statistics and ANN topology stay
    /// exactly those of the prepared generation.
    ///
    /// A fresh live permit rechecks the owner, authority and admitted vertex
    /// population. The host's snapshot-record cap covers that population PLUS
    /// admitted incidence visits (parallel EIds separately, undirected edges
    /// at each expanded endpoint). Signed nodes count the saved population
    /// once. One cumulative source-scratch allowance covers admitted incidences,
    /// retained BFS distance/queue state, graph ranks, fusion evidence, heaps and
    /// result buffers; graph visit/seed bounds remain additional count ceilings.
    /// Staging rows are unused: neither documents nor a graph directory
    /// are staged. These are admission bounds, not a combined byte-memory cap.
    ///
    /// Hidden history is poll-only; no observable work/admission charge for an
    /// edge occurs until both endpoints and its relation are permitted. Native
    /// index lookups remain synchronous and the pin remains resident. This is
    /// not descriptor/timing isolation, spill, an advancing index or an
    /// exhaustive-ANN certificate. Final delivery and errors use the ordinary
    /// session gate; no prefix, stale fallback or standalone index escapes.
    pub fn search_beacon_graph_index(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedBeaconIndex,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        request: ReadPolicy,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        self.search_beacon_graph_index_with_eligibility(
            cx, prepared, query, expansion, request, None,
        )
    }

    /// Filter result endpoints before every candidate limit, never graph
    /// transit, corpus statistics or ANN routing. Supply at most 100,000
    /// strictly ascending unique IDs; empty allows none. Hidden/unknown IDs
    /// are ordinary nonmatches. No callback can inspect candidates before
    /// signed row delivery. Reject-all keeps every source and live check.
    pub fn search_beacon_graph_index_filtered(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedBeaconIndex,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        request: ReadPolicy,
        eligible: &[VId],
    ) -> Result<Vec<GraphHybridHit>, Error> {
        self.search_beacon_graph_index_with_eligibility(
            cx,
            prepared,
            query,
            expansion,
            request,
            Some(eligible),
        )
    }

    fn search_beacon_graph_index_with_eligibility(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedBeaconIndex,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        request: ReadPolicy,
        eligible: Option<&[VId]>,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        let owner = Arc::clone(&self.owner);
        finish(self.run(cx, |view, _, scope, _, host, execution| {
            // source_query has k=0. Admission must check the FINAL requested k.
            admit_prepared(
                &owner,
                prepared,
                view,
                scope,
                host,
                execution,
                Search::Hybrid(query.retrieval),
            )?;
            let allowance = prepared_policy(prepared, request, host);
            let work = RefCell::new(Meter::new(allowance.max_work_units, |units| {
                charge(execution, units)
            }));
            let result = (|| {
                work.borrow_mut().charge(1)?;
                query.candidate_depths()?;
                if query.retrieval.k > allowance.max_result_rows {
                    return Err(BeaconError::ResourceLimit {
                        resource: "result rows",
                        limit: allowance.max_result_rows,
                    });
                }
                if !query.is_graph_only() {
                    let source = Search::Hybrid(query.source_query());
                    let config = &prepared.definition.index;
                    let (vector, text) = source.lanes();
                    if vector && config.vector.is_none() {
                        return Err(BeaconError::Disabled("vector"));
                    }
                    if text && config.text.is_none() {
                        return Err(BeaconError::Disabled("text"));
                    }
                    source.validate(config, &mut SharedWork(&work))?;
                }
                validate_eligibility(eligible, &mut SharedWork(&work))?;
                let scratch_limit = expansion
                    .limits
                    .max_source_scratch
                    .min(allowance.max_source_scratch)
                    .min(prepared.definition.policy.max_source_scratch);
                let scratch = Cell::new(0usize);
                let admit_scratch =
                    |entries: usize| {
                        let next = scratch.get().checked_add(entries).ok_or(
                            BeaconError::ResourceLimit {
                                resource: "prepared graph scratch entries",
                                limit: scratch_limit,
                            },
                        )?;
                        if next > scratch_limit {
                            return Err(BeaconError::ResourceLimit {
                                resource: "prepared graph scratch entries",
                                limit: scratch_limit,
                            });
                        }
                        scratch.set(next);
                        Ok(())
                    };
                let graph = if query.graph_enabled() {
                    expand_at(
                        cx,
                        prepared,
                        view,
                        scope,
                        host,
                        execution,
                        expansion,
                        query.graph_candidates,
                        allowance,
                        &work,
                        |id, _| Ok(allows(eligible, id)),
                        &admit_scratch,
                    )?
                    .0
                } else {
                    // Disabled graph lanes never inspect seeds or graph limits.
                    Vec::new()
                };
                // Fusion state joins the graph's one allowance only while the
                // lane runs; a disabled lane never meets a graph limit.
                let enabled = query.graph_enabled();
                let fusion_scratch = |entries: usize| {
                    if enabled {
                        admit_scratch(entries)
                    } else {
                        Ok(())
                    }
                };
                let mut kernel = ScratchWork {
                    work: &work,
                    admit: &fusion_scratch,
                };
                let rows = if query.is_graph_only() {
                    query.fuse_graph_only(&graph, |id| allows(eligible, id), &mut kernel)?
                } else {
                    prepared.index.hybrid_search_graph_filtered(
                        query,
                        &graph,
                        |id| allows(eligible, id),
                        &mut kernel,
                    )?
                };
                work.borrow_mut().charge(1)?;
                Ok(rows)
            })();
            let result = settle(work, result)?;
            let rows = result.as_ref().map_or(0, Vec::len);
            Ok((result, rows))
        }))
    }
}

/// The typed and native CALL hosts share source selection, the original live
/// meter and the same ExpansionCursor. The optional logical scratch admission
/// runs BEFORE retaining each new map/queue visit, admitted incidence, or rank
/// workspace. Typed calls keep their established independent engine ceilings;
/// native calls additionally spend/report the enclosing statement's allowance.
/// No permit, source corpus or binding can be replaced inside this helper.
#[allow(clippy::too_many_arguments)]
pub(super) fn expand_at<F>(
    cx: &QueryCx,
    prepared: &AuthorizedBeaconIndex,
    view: &EmbeddedReadView,
    scope: &PlannerPredicates,
    host: GqlQueryPolicy,
    execution: &super::super::Live<'_, '_, '_>,
    mut expansion: ExpansionSpec<'_, RelationId>,
    candidates: u32,
    allowance: ReadPolicy,
    work: &RefCell<Meter<QueryError, F>>,
    eligible: impl FnMut(VId, &mut dyn WorkControl) -> Result<bool, BeaconError>,
    admit_scratch: &impl Fn(usize) -> Result<(), BeaconError>,
) -> Result<(Vec<GraphHit>, u64), BeaconError>
where
    F: FnMut(usize) -> Result<(), QueryError>,
{
    let cap = allowance
        .max_source_scratch
        .min(prepared.definition.policy.max_source_scratch);
    expansion.limits.max_vertices = expansion.limits.max_vertices.min(cap);
    expansion.limits.max_visited_vertices = expansion.limits.max_visited_vertices.min(cap);
    expansion.limits.max_seed_ids = expansion.limits.max_seed_ids.min(cap);
    expansion.limits.max_input_edges = expansion.limits.max_input_edges.min(cap);
    expansion.limits.max_source_scratch = expansion.limits.max_source_scratch.min(cap);
    let refuse = |error| work.borrow_mut().refuse(error);
    let source = view
        .vertex_scan_source(cx, prepared.sequence)
        .map_err(|error| refuse(QueryError::Read(error)))?;
    let poll = RefCell::new(|| execution.borrow_mut().poll().map_err(&refuse));
    let membership = Membership {
        source,
        scope,
        label: prepared.definition.vertex_label,
        poll: &poll,
        refuse: &refuse,
    };
    let mut neighbors = Neighbors {
        snapshot: &view.snapshot,
        at: prepared.sequence,
        membership: &membership,
        relation: expansion.relation,
        direction: match expansion.direction {
            ExpansionDirection::Outgoing => GlaDirection::Forward,
            ExpansionDirection::Incoming => GlaDirection::Reverse,
            ExpansionDirection::Undirected => GlaDirection::Undirected,
        },
        host,
        population: prepared.admitted,
        admitted: 0,
        limit: expansion.limits.max_input_edges,
        scratch_limit: expansion.limits.max_source_scratch,
        admit_scratch,
    };
    let mut control = SharedWork(work);
    let mut cursor = ExpansionCursor::new(
        expansion.seeds,
        expansion.max_hops,
        expansion.include_seeds,
        candidates,
        expansion.limits,
        &mut control,
    )?;
    loop {
        match cursor.request(&mut control)? {
            ExpansionRequest::Seed(id) => {
                let present = membership.contains_vertex(id, &mut control)?;
                cursor.resolve_membership(present, || admit_scratch(2))?;
            }
            ExpansionRequest::Neighbors(id) => {
                let result = neighbors.visit_neighbors(id, &mut control, &mut |next, work| {
                    cursor.prepare_neighbor(next, work)?;
                    let present = membership
                        .contains_vertex(next, work)
                        .map_err(|error| cursor.refuse(error))?;
                    cursor.resolve_membership(present, || admit_scratch(2))
                });
                if let Err(error) = result {
                    return Err(cursor.refuse(error));
                }
                // A swallowed visitor refusal cannot become successful EOF.
                cursor.finish_neighbors()?;
            }
            ExpansionRequest::Complete => break,
        }
    }
    // Reserve the maximum live rank heap and returned GraphHit vector BEFORE
    // finish allocates them. Rejected endpoints cannot waive traversal costs.
    let capacity = cursor.visited_vertices().min(
        usize::try_from(candidates)
            .map_err(|_| BeaconError::InvalidQuery("graph candidate depth exceeds usize"))?,
    );
    admit_scratch(capacity.checked_mul(2).ok_or(BeaconError::ResourceLimit {
        resource: "prepared graph rank workspace",
        limit: cap,
    })?)?;
    let hits = cursor.finish(eligible, &mut control)?;
    let incidences = u64::try_from(neighbors.admitted)
        .map_err(|_| BeaconError::Invariant("prepared incidence count exceeds u64"))?;
    Ok((hits, incidences))
}

// A point probe borrows a native historical winner. It does not cache hidden
// IDs, copy properties, or expose history shape through charged work/scratch.
struct Membership<'a, S, P, F> {
    source: S,
    scope: &'a PlannerPredicates,
    label: Option<LabelId>,
    poll: &'a RefCell<P>,
    refuse: &'a F,
}

impl<S, P, F> Membership<'_, S, P, F>
where
    S: VertexScanSource<Error = ReadError>,
    P: FnMut() -> Result<(), BeaconError>,
    F: Fn(QueryError) -> BeaconError,
{
    /// Resolve authority and user selection WITHOUT observable charges. An
    /// edge caller must establish BOTH endpoints before charging either one.
    fn admitted(&self, id: VId) -> Result<Option<usize>, BeaconError> {
        (self.poll.borrow_mut())()?;
        let row = self.source.vertex(id, &mut |_| (self.poll.borrow_mut())());
        let row = match row {
            Ok(row) => row,
            Err(VertexScanSourceError::Control(error)) => return Err(error),
            Err(VertexScanSourceError::Source(error)) => {
                return Err((self.refuse)(QueryError::Read(error)));
            }
        };
        let Some(row) = row else { return Ok(None) };
        if !self.scope.allows_vertex(row.labels)
            || self.label.is_some_and(|label| {
                !self.scope.allows_label(label) || row.labels.binary_search(&label).is_err()
            })
        {
            return Ok(None);
        }
        Ok(Some(
            1 + row
                .labels
                .iter()
                .filter(|&&label| self.scope.allows_label(label))
                .count(),
        ))
    }
}

impl<S, P, F> ExpansionMembership for Membership<'_, S, P, F>
where
    S: VertexScanSource<Error = ReadError>,
    P: FnMut() -> Result<(), BeaconError>,
    F: Fn(QueryError) -> BeaconError,
{
    fn contains_vertex(&self, id: VId, work: &mut dyn WorkControl) -> Result<bool, BeaconError> {
        let Some(units) = self.admitted(id)? else {
            return Ok(false);
        };
        work.charge(units)?;
        Ok(true)
    }
}

struct Neighbors<'a, 'm, S, P, F> {
    snapshot: &'a Snapshot,
    at: CommitSeq,
    membership: &'a Membership<'m, S, P, F>,
    relation: Option<RelationId>,
    direction: GlaDirection,
    host: GqlQueryPolicy,
    population: u64,
    admitted: usize,
    limit: usize,
    scratch_limit: usize,
    admit_scratch: &'a dyn Fn(usize) -> Result<(), BeaconError>,
}

impl<S, P, F> ExpansionNeighbors for Neighbors<'_, '_, S, P, F>
where
    S: VertexScanSource<Error = ReadError>,
    P: FnMut() -> Result<(), BeaconError>,
    F: Fn(QueryError) -> BeaconError,
{
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        let membership = self.membership;
        if self
            .relation
            .is_some_and(|relation| !membership.scope.allows_relation(relation))
        {
            return Ok(());
        }
        let mut after = None;
        loop {
            (membership.poll.borrow_mut())()?;
            let next = self.snapshot.adjacency_index().next_incident_edge(
                vertex,
                self.direction,
                after,
                &mut |_| (membership.poll.borrow_mut())(),
            )?;
            let Some(eid) = next else { break };
            after = Some(eid); // ordered successor, including EId(u128::MAX)
            let Some((block, row)) = self.snapshot.adjacency_index().statement_at_controlled(
                &self.snapshot.blocks,
                eid,
                self.at,
                &mut |_| (membership.poll.borrow_mut())(),
            )?
            else {
                continue;
            };
            let edge = &self.snapshot.blocks[block][row];
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
                || !membership.scope.allows_relation(edge.relation)
            {
                continue;
            }
            // BFS admitted the anchor through this SAME immutable membership
            // source before asking for its adjacency. Check the other endpoint
            // before charging anything for the edge; rebilling a visible anchor
            // before finding a hidden neighbor would expose hidden incidence.
            let Some(target_units) = membership.admitted(neighbor)? else {
                continue;
            };
            work.charge(1)?;
            work.charge(target_units)?;
            if self.admitted >= self.limit {
                return Err(BeaconError::ResourceLimit {
                    resource: "prepared graph incidence visits",
                    limit: self.limit,
                });
            }
            if self.admitted >= self.scratch_limit {
                return Err(BeaconError::ResourceLimit {
                    resource: "prepared graph source admissions",
                    limit: self.scratch_limit,
                });
            }
            let next = self
                .admitted
                .checked_add(1)
                .ok_or(BeaconError::ResourceLimit {
                    resource: "prepared graph incidence visits",
                    limit: self.limit,
                })?;
            let records = u64::try_from(next)
                .ok()
                .and_then(|next| self.population.checked_add(next))
                .ok_or(BeaconError::ResourceLimit {
                    resource: "prepared graph records",
                    limit: usize::MAX,
                })?;
            self.host
                .rows
                .check(GqlBudgetDimension::SnapshotRecords, records)
                .map_err(|error| {
                    (membership.refuse)(QueryError::Pattern(GqlQueryError::Rows(error)))
                })?;
            (self.admit_scratch)(1)?;
            self.admitted = next;
            visit(neighbor, work)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "prepared_graph_tests.rs"]
mod tests;
