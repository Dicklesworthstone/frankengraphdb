//! Retrieval with demand-driven vertex and incidence admission.
//! No complete vertex directory, property projection or search index is built.
//! Graph-only reads need no index. Privileged mixed reads require a maintained
//! exact generation at the same pinned cut; they never rebuild on a miss.
//! Native historical indexes supply the same Beacon BFS and exact fusion used
//! by other hosts. This is an explicit physical profile, never a fallback.

use super::{BeaconQueryIndexes, Cancel, Meter, Options, Scan, SharedWork};
use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, EmbeddedReadView, ReadError, Snapshot, VertexRow};
use asupersync::fs::Vfs;
use fgdb_beacon::expansion::{
    ExpansionDirection, ExpansionMembership, ExpansionNeighbors, ExpansionSpec,
    expand_from_membership_filtered,
};
use fgdb_beacon::read::{ReadError as Error, Search};
use fgdb_beacon::{BeaconError, GraphHybridHit, GraphHybridQuery, IndexSnapshot, WorkControl};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_gql::{GlaExecutionEvent, algebra::GlaDirection};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

/// Source and traversal/fusion admissions spend one shared scratch allowance.
/// Hidden histories poll the existing live gate, without changing observable
/// work/scratch counters. No hidden or absent identity enters the cache.
struct SourceControl<'a> {
    scan: RefCell<Scan<'a>>,
    used: Cell<usize>,
    limit: usize,
}

impl SourceControl<'_> {
    fn reserve(&self) -> Result<(), BeaconError> {
        let used = self.used.get();
        if used >= self.limit {
            return Err(BeaconError::ResourceLimit {
                resource: "demand graph source admissions",
                limit: self.limit,
            });
        }
        self.used.set(used + 1);
        Ok(())
    }

    fn event(&self, event: SourceEvent, work: &mut dyn WorkControl) -> Result<(), BeaconError> {
        match &mut *self.scan.borrow_mut() {
            Scan::Metered => {
                work.charge(1)?;
                if event == SourceEvent::ScratchEntry {
                    self.reserve()?;
                }
                Ok(())
            }
            Scan::Unmetered(poll) => poll(),
        }
    }
}

/// Kernel entries describe already-admitted visible traversal state. They must
/// not use SourceControl::event: Unmetered masks hidden history work, not the
/// distance map, queue, rank maps or output buffers of an authorized query.
/// Keep this wrapper local to demand so other source profiles do not silently
/// acquire a different accounting law. Each entry is cumulative, not refunded.
struct ScratchWork<'a, 'p, W> {
    work: &'a RefCell<W>,
    source: &'a SourceControl<'p>,
}

impl<W: WorkControl> WorkControl for ScratchWork<'_, '_, W> {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        self.work.borrow_mut().charge(units)
    }

    fn reserve_scratch(&mut self, entries: usize) -> Result<(), BeaconError> {
        for _ in 0..entries {
            self.work.borrow_mut().charge(1)?;
            self.source.reserve()?;
        }
        Ok(())
    }
}

struct Membership<'s, 'c, 'p, A, L> {
    snapshot: &'s Snapshot,
    at: CommitSeq,
    label: Option<LabelId>,
    source: &'c SourceControl<'p>,
    admit: RefCell<A>,
    label_allowed: RefCell<L>,
    // Only capability-admitted identities, including an admitted vertex that
    // fails the user's label selection. Cache values are stable at this cut.
    admitted: RefCell<BTreeMap<VId, bool>>,
    limit: usize,
}

impl<A, L> ExpansionMembership for Membership<'_, '_, '_, A, L>
where
    A: FnMut(&VertexRow) -> Result<bool, BeaconError>,
    L: FnMut(LabelId) -> bool,
{
    fn contains_vertex(&self, id: VId, work: &mut dyn WorkControl) -> Result<bool, BeaconError> {
        let (cached, count) = {
            let admitted = self.admitted.borrow();
            (admitted.get(&id).copied(), admitted.len())
        };
        let lookup_work = 1 + count.checked_ilog2().unwrap_or(0) as usize;
        if let Some(selected) = cached {
            // Billing follows visibility, even for a known rejected label.
            work.charge(lookup_work)?;
            return Ok(selected);
        }
        let row = source::find_vertex(self.snapshot, id, self.at, &mut |event| {
            self.source.event(event, work)
        })?;
        let Some(row) = row else { return Ok(false) };
        // Original-label authority MUST precede native charges, label selection
        // and cache insertion. The callback charges the signed node admission
        // once per visible identity, not once per parallel edge or BFS probe.
        if !(self.admit.borrow_mut())(row)? {
            return Ok(false);
        }
        work.charge(lookup_work)?;
        let selected = {
            let mut allowed = self.label_allowed.borrow_mut();
            let visible_labels = row.labels.iter().filter(|&&label| allowed(label)).count();
            work.charge(1 + visible_labels)?;
            self.label
                .is_none_or(|label| allowed(label) && row.labels.binary_search(&label).is_ok())
        };
        if count >= self.limit {
            return Err(BeaconError::ResourceLimit {
                resource: "demand graph admitted vertices",
                limit: self.limit,
            });
        }
        self.source.reserve()?;
        self.admitted.borrow_mut().insert(id, selected);
        Ok(selected)
    }
}

struct Neighbors<'s, 'm, 'c, 'p, M, R, A> {
    snapshot: &'s Snapshot,
    at: CommitSeq,
    membership: &'m M,
    source: &'c SourceControl<'p>,
    relation: Option<RelationId>,
    direction: GlaDirection,
    relation_allowed: R,
    admit_edge: A,
    admitted: usize,
    limit: usize,
}

impl<M, R, A> ExpansionNeighbors for Neighbors<'_, '_, '_, '_, M, R, A>
where
    M: ExpansionMembership,
    R: FnMut(RelationId) -> bool,
    A: FnMut() -> Result<(), BeaconError>,
{
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        if self
            .relation
            .is_some_and(|relation| !(self.relation_allowed)(relation))
        {
            return Ok(());
        }
        let mut after = None;
        loop {
            let index = self.snapshot.adjacency_index();
            let next = index.next_incident_edge(vertex, self.direction, after, &mut |event| {
                self.source.event(
                    match event {
                        GlaExecutionEvent::ScratchEntry => SourceEvent::ScratchEntry,
                        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => SourceEvent::Work,
                    },
                    work,
                )
            })?;
            let Some(eid) = next else { break };
            after = Some(eid); // No identity arithmetic: zero and u128::MAX work.
            let Some((block, row)) = index.statement_at_controlled(
                &self.snapshot.blocks,
                eid,
                self.at,
                &mut |event| self.source.event(event, work),
            )?
            else {
                continue;
            };
            let edge = &self.snapshot.blocks[block][row];
            let next = match self.direction {
                GlaDirection::Forward if edge.src == vertex => edge.dst,
                GlaDirection::Reverse if edge.dst == vertex => edge.src,
                GlaDirection::Undirected if edge.src == vertex => edge.dst,
                GlaDirection::Undirected if edge.dst == vertex => edge.src,
                _ => continue,
            };
            if self
                .relation
                .is_some_and(|relation| relation != edge.relation)
                || !(self.relation_allowed)(edge.relation)
            {
                continue;
            }
            // The BFS has already verified `vertex` through this SAME immutable
            // membership source. Resolve ONLY the other endpoint before billing
            // an incidence. Rebilling the visible anchor before discovering a
            // hidden neighbor would disclose hidden edges through work limits.
            if !self.membership.contains_vertex(next, work)? {
                continue;
            }
            work.charge(1)?;
            if self.admitted >= self.limit {
                return Err(BeaconError::ResourceLimit {
                    resource: "demand graph incidence visits",
                    limit: self.limit,
                });
            }
            self.source.reserve()?;
            (self.admit_edge)()?;
            self.admitted += 1;
            visit(next, work)?;
        }
        Ok(())
    }
}

/// One admitted immutable cut, one source allowance and the existing BFS/RRF.
/// The caller owns health/cut checks and authority. Admitted-vertex counters
/// count unique probed visible vertices (before the user's label restriction),
/// not the complete corpus. max_vertices additionally caps that cache; visited
/// vertices remain bounded by the expansion kernel. No property is inspected.
/// Return source AND kernel admissions so native CALL reports all scratch:
/// cached membership, incidence, distances/queue, ranking and fusion buffers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_filtered(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: GraphHybridQuery<'_>,
    expansion: ExpansionSpec<'_, RelationId>,
    work: &RefCell<impl WorkControl>,
    scan: Scan<'_>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    relation_allowed: impl FnMut(RelationId) -> bool,
    admit_edge: impl FnMut() -> Result<(), BeaconError>,
    eligible: impl Fn(VId) -> bool,
) -> Result<(Vec<GraphHybridHit>, u64), BeaconError> {
    work.borrow_mut().charge(1)?;
    options.validate_graph_only(query)?;
    evaluate_prepared(
        snapshot,
        at,
        options,
        query,
        expansion,
        None,
        work,
        scan,
        admit,
        label_allowed,
        relation_allowed,
        admit_edge,
        eligible,
    )
}

/// Mixed demand reads have no query-time corpus preparation to fall back to.
/// Only the privileged owner of these immutable query pins may join their
/// exact corpus to native membership at the same cut. Capability-scoped and
/// staged hosts retain the graph-only entry point above.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_privileged(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: GraphHybridQuery<'_>,
    expansion: ExpansionSpec<'_, RelationId>,
    work: &RefCell<impl WorkControl>,
    registry: &BeaconQueryIndexes,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    eligible: impl Fn(VId) -> bool,
) -> Result<(Vec<GraphHybridHit>, u64), BeaconError> {
    if query.is_graph_only() {
        // Preserve the existing graph-only admission and checkpoint prefix,
        // including historical reads and views without any maintained index.
        return evaluate_filtered(
            snapshot,
            at,
            options,
            query,
            expansion,
            work,
            Scan::Metered,
            admit,
            |_| true,
            |_| true,
            || Ok(()),
            eligible,
        );
    }
    work.borrow_mut().charge(1)?;
    let config = options.config_for_graph(query)?;
    let source_query = Search::Hybrid(query.source_query());
    source_query.validate(&config, &mut SharedWork(work))?;
    let index = registry
        .lookup_exact(
            at,
            snapshot.frontier,
            options,
            &config,
            source_query,
            &mut SharedWork(work),
        )?
        .ok_or(BeaconError::InvalidQuery(
            "mixed demand search requires a compatible maintained exact index",
        ))?;
    evaluate_prepared(
        snapshot,
        at,
        options,
        query,
        expansion,
        Some(&index),
        work,
        Scan::Metered,
        admit,
        |_| true,
        |_| true,
        || Ok(()),
        eligible,
    )
}

/// Prepared index, if any, and graph source already share one authority/cut.
/// Every newly retained kernel entry spends the same demand scratch allowance
/// as native membership and incidence state, including text/vector fusion.
#[allow(clippy::too_many_arguments)]
fn evaluate_prepared(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: GraphHybridQuery<'_>,
    expansion: ExpansionSpec<'_, RelationId>,
    index: Option<&IndexSnapshot>,
    work: &RefCell<impl WorkControl>,
    scan: Scan<'_>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    relation_allowed: impl FnMut(RelationId) -> bool,
    admit_edge: impl FnMut() -> Result<(), BeaconError>,
    eligible: impl Fn(VId) -> bool,
) -> Result<(Vec<GraphHybridHit>, u64), BeaconError> {
    let source = SourceControl {
        scan: RefCell::new(scan),
        used: Cell::new(0),
        limit: if query.graph_enabled() {
            options
                .policy
                .max_source_scratch
                .min(expansion.limits.max_source_scratch)
        } else {
            // A disabled graph lane cannot spend seeds or graph allowances.
            options.policy.max_source_scratch
        },
    };
    let membership = Membership {
        snapshot,
        at,
        label: options.vertex_label,
        source: &source,
        admit: RefCell::new(admit),
        label_allowed: RefCell::new(label_allowed),
        admitted: RefCell::new(BTreeMap::new()),
        limit: options
            .policy
            .max_staging_rows
            .min(options.index.max_documents)
            .min(expansion.limits.max_vertices),
    };
    let mut neighbors = Neighbors {
        snapshot,
        at,
        membership: &membership,
        source: &source,
        relation: expansion.relation,
        direction: match expansion.direction {
            ExpansionDirection::Outgoing => GlaDirection::Forward,
            ExpansionDirection::Incoming => GlaDirection::Reverse,
            ExpansionDirection::Undirected => GlaDirection::Undirected,
        },
        relation_allowed,
        admit_edge,
        admitted: 0,
        limit: expansion.limits.max_input_edges,
    };
    // Complete bounded traversal even when k=0 or every endpoint is rejected.
    // Filtering can change ranks/results, never transit or refusal semantics.
    let mut execution = ScratchWork {
        work,
        source: &source,
    };
    let graph = if query.graph_enabled() {
        expand_from_membership_filtered(
            expansion.seeds,
            expansion.max_hops,
            expansion.include_seeds,
            query.graph_candidates,
            expansion.limits,
            &membership,
            &mut neighbors,
            |id, _| Ok(eligible(id)),
            &mut execution,
        )?
    } else {
        Vec::new()
    };
    let rows = match index {
        Some(index) => {
            index.hybrid_search_graph_filtered(query, &graph, eligible, &mut execution)?
        }
        None => query.fuse_graph_only(&graph, eligible, &mut execution)?,
    };
    work.borrow_mut().charge(1)?;
    let admissions = u64::try_from(source.used.get())
        .map_err(|_| BeaconError::Invariant("demand source admissions exceed u64"))?;
    Ok((rows, admissions))
}

impl<V: Vfs + Clone> Database<V> {
    /// Retrieve through demanded topology without a complete vertex directory.
    /// Enabled text/vector lanes require a compatible maintained exact index.
    /// See the pinned-view method for this explicit physical profile's limits.
    /// This is privileged, like read_session; use authorized native CALL for a
    /// capability holder. It never silently discards an enabled index modality.
    pub fn beacon_search_graph_demand_filtered(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, Error<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            self.read_session()
                .map_err(Error::Read)?
                .beacon_search_graph_demand_filtered(cx, options, query, expansion, eligible)
        })
    }
}

impl EmbeddedReadView {
    /// Demand membership and adjacency at this pinned generation.
    /// No query-time index, vertex directory, graph copy, or native property
    /// projection is constructed. Enabled text/vector lanes require a healthy
    /// maintained exact index captured at this view's frontier with matching
    /// label, projection and active index settings. A missing or incompatible
    /// generation refuses; there is no corpus build or source-profile retry.
    /// Endpoint eligibility leaves the complete index corpus and BM25 statistics
    /// intact. Approximate vector requests do not use this profile.
    ///
    /// `options.as_of` may select any retained cut for graph-only retrieval.
    /// Mixed retrieval must select this view's own frontier: use a previously
    /// pinned view to join historical index and graph generations.
    ///
    /// max_staging_rows, max_documents and max_vertices cap the unique visible
    /// membership cache; max_visited_vertices separately caps traversal. Shared
    /// source and kernel admissions share BOTH native and expansion scratch
    /// limits. Distances and queue entries, both ranking heaps, fusion maps and
    /// output buffers are admitted before retention. Counts are cumulative
    /// logical entries, not allocator capacities or bytes, and are not refunded.
    /// An incidence counts per expanded endpoint, with parallel EIds distinct.
    /// A rejected result endpoint remains a transit vertex. Empty/k=0 results
    /// retain complete bounded traversal and the final live checkpoint.
    ///
    /// The predicate is pure, bounded host computation, not authorization.
    /// Decoded native storage remains resident. This is not a spill or timing-
    /// isolation guarantee. Existing indexed/materialized profiles are unchanged.
    pub fn beacon_search_graph_demand_filtered(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, Error<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let at = options.as_of.unwrap_or(self.frontier());
            self.snapshot.check_frontier(at).map_err(Error::Read)?;
            let work = RefCell::new(Meter::new(options.policy.max_work_units, |_| {
                cx.checkpoint()
            }));
            let result = evaluate_privileged(
                &self.snapshot,
                at,
                options,
                query,
                expansion,
                &work,
                &self.beacon_indexes,
                |_| Ok(true),
                eligible,
            );
            work.into_inner().finish(result.map(|(rows, _)| rows))
        })
    }
}

#[cfg(test)]
#[path = "demand_tests.rs"]
mod tests;
