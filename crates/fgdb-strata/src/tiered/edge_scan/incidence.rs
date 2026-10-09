//! Independent cold incidence positions over one admitted partition. Routing
//! retains only endpoint/relation/block coordinates, never edge IDs or fields.
//! The existing EId merge selects history winners and property locators.

use super::*;
use fgdb_types::VId;
use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Included, Unbounded};

/// Stored incidence direction, independent of a query language's operator IR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferedEdgeDirection {
    Outgoing,
    Incoming,
    Undirected,
}

impl BufferedEdgeDirection {
    fn matches(self, entry: &AdjacencyEntry, endpoint: VId) -> bool {
        match self {
            Self::Outgoing => entry.src == endpoint,
            Self::Incoming => entry.dst == endpoint,
            Self::Undirected => entry.src == endpoint || entry.dst == endpoint,
        }
    }
}

// None is an explicit all-relation directory, not an authorization wildcard.
// A typed and an untyped request both seek ONE ordered block set. Outgoing and
// incoming sets merge by block ordinal; a self-loop's block is visited once.
type Route = (VId, bool, Option<RelationId>, usize);
struct Routes {
    entries: BTreeSet<Route>,
    // Conservative B-tree node/occupancy charge, not an allocator/RSS measure.
    // The set drops before its charge; every insertion is admitted in advance.
    charge: MemoryCharge,
}

impl Routes {
    fn lookup_work<C>(
        &self,
        cx: &QueryCx,
        work: &mut EdgeScanWork,
        observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
    ) -> Result<(), BufferedScanError<C>> {
        // Fixed-width keys: cover tree descent, within-node comparisons and
        // insertion/rebalancing conservatively before a library B-tree call.
        for _ in 0..32 * (self.entries.len().saturating_add(1).ilog2() as usize + 1) {
            work.step(cx, observe)?;
        }
        Ok(())
    }

    fn insert<C>(
        &mut self,
        route: Route,
        cx: &QueryCx,
        work: &mut EdgeScanWork,
        observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
    ) -> Result<(), BufferedScanError<C>> {
        self.lookup_work(cx, work, observe)?;
        if !self.entries.contains(&route) {
            self.charge.grow(cx, 512).map_err(BufferedReadError::Memory)?;
            self.lookup_work(cx, work, observe)?;
            self.entries.insert(route);
        }
        Ok(())
    }

    fn next_block<C>(
        &self,
        request: Incidence,
        after: Option<usize>,
        cx: &QueryCx,
        work: &mut EdgeScanWork,
        observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
    ) -> Result<Option<usize>, BufferedScanError<C>> {
        let mut found = None;
        for incoming in [false, true] {
            if (incoming && request.direction == BufferedEdgeDirection::Outgoing)
                || (!incoming && request.direction == BufferedEdgeDirection::Incoming)
            {
                continue;
            }
            self.lookup_work(cx, work, observe)?;
            let prefix = (request.endpoint, incoming, request.relation);
            let lower = after.map_or(
                Included((prefix.0, prefix.1, prefix.2, 0)),
                |block| Excluded((prefix.0, prefix.1, prefix.2, block)),
            );
            if let Some(&(endpoint, face, relation, block)) =
                self.entries.range((lower, Unbounded)).next()
                && (endpoint, face, relation) == prefix
            {
                found = Some(found.map_or(block, |old: usize| old.min(block)));
            }
        }
        Ok(found)
    }
}

#[derive(Clone, Copy)]
struct Incidence {
    endpoint: VId,
    relation: Option<RelationId>,
    direction: BufferedEdgeDirection,
    after: Option<EId>,
}
impl Incidence {
    fn matches(self, entry: &AdjacencyEntry) -> bool {
        self.relation.is_none_or(|r| r == entry.relation)
            && self.direction.matches(entry, self.endpoint)
    }
}

struct Driver<'source, V: Vfs> {
    root: BufferedEdgeScan<'source, V>,
    routes: Option<Routes>,
}

/// One root stream plus independently positioned indexed incidence requests.
/// The first incidence request builds a derived endpoint-to-block directory by
/// one governed pass over the admitted blocks. Later requests fault only routed
/// blocks; they do not rebuild the directory or scan unrelated block payloads.
/// All histories in a routed block remain authoritative until native resolution.
///
/// Routing is query-local RESIDENT METADATA, bounded by the partition pool and
/// cumulative source work. It can grow with distinct endpoint/block pairs and
/// refuse when it does not fit. This is not a persistent adjacency index or an
/// optimal I/O bound: each successor can refault its routed bounded blocks.
/// No full edge directory, neighborhood payload bag or result table is kept.
///
/// Root exhaustion does not disable nested reads: the final root edge may still
/// have children. Close/error/drop of a POLLED pending call drops the complete
/// driver, including routing and root heads, and makes all later calls inert.
/// Returned edge/endpoints own their charges independently. This is owner-level
/// storage access, not a Warden grant or cross-process retention lease.
pub struct BufferedEdgeJoinScan<'source, V: Vfs> {
    driver: Option<Driver<'source, V>>,
    as_of: CommitSeq,
    work: usize,
}

impl<'source, V: Vfs> BufferedEdgeScan<'source, V> {
    /// Add independently positioned cold incidence access without reading or
    /// allocating routing metadata. An unpolled query still does no source I/O.
    pub fn into_join_scan(self) -> BufferedEdgeJoinScan<'source, V> {
        BufferedEdgeJoinScan {
            as_of: self.as_of,
            work: self.work.used,
            driver: Some(Driver { root: self, routes: None }),
        }
    }
}

impl<V: Vfs> core::fmt::Debug for BufferedEdgeJoinScan<'_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BufferedEdgeJoinScan")
            .field("snapshot", &self.as_of)
            .field("closed", &self.is_closed())
            .field("work", &self.work_used())
            .field("source", &"[REDACTED]")
            .finish()
    }
}

impl<V: Vfs> BufferedEdgeJoinScan<'_, V> {
    pub const fn snapshot_seq(&self) -> CommitSeq { self.as_of }
    pub fn work_used(&self) -> usize {
        self.driver.as_ref().map_or(self.work, |d| d.root.work.used)
    }
    pub fn is_closed(&self) -> bool { self.driver.is_none() }
    pub fn close(&mut self) {
        self.work = self.work_used();
        self.driver = None;
    }

    pub async fn next_with_endpoints<C: Send>(
        &mut self,
        cx: &QueryCx,
        relation: Option<RelationId>,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        let Some(mut driver) = self.driver.take() else { return Ok(None); };
        let used = &mut self.work;
        let result = driver.root.next_with_endpoints(cx, relation, &mut |event| {
            observe(event)?;
            if event == BufferedEdgeScanEvent::Work {
                *used = used.saturating_add(1);
            }
            Ok(())
        }).await;
        self.work = driver.root.work.used;
        if result.is_ok() { self.driver = Some(driver); }
        result
    }

    /// The first historical incident EId strictly greater than `after`.
    /// Positions belong to the caller's traversal prefix, never to the root.
    /// Emit Identity exactly once before resolving the winning history, even
    /// if it is invisible at the selected cut. EOF emits no identity. Discovery
    /// reads only bounded block keys after routing, under the same work meter.
    pub async fn next_incident_with_endpoints<C: Send>(
        &mut self,
        cx: &QueryCx,
        endpoint: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
        after: Option<EId>,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        let Some(mut driver) = self.driver.take() else { return Ok(None); };
        let request = Incidence { endpoint, relation, direction, after };
        let used = &mut self.work;
        let result = driver.incident(cx, request, &mut |event| {
            observe(event)?;
            if event == BufferedEdgeScanEvent::Work {
                *used = used.saturating_add(1);
            }
            Ok(())
        }).await;
        self.work = driver.root.work.used;
        if result.is_ok() { self.driver = Some(driver); }
        result
    }
}

impl<V: Vfs> Driver<'_, V> {
    async fn build_routes<C: Send>(
        &mut self,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<(), BufferedScanError<C>> {
        if self.routes.is_some() { return Ok(()); }
        self.root.work.step(cx, observe)?;
        let mut routes = Routes {
            entries: BTreeSet::new(),
            charge: self.root.partition.reserve_scan_bytes(cx, 1024)?,
        };
        for block in 0..self.root.blocks {
            let image = self.root.partition
                .edge_scan_block(cx, block, &mut self.root.work, observe).await?;
            for row in &image.0 {
                for (endpoint, incoming) in [(row.src, false), (row.dst, true)] {
                    for relation in [None, Some(row.relation)] {
                        routes.insert((endpoint, incoming, relation, block), cx,
                            &mut self.root.work, observe)?;
                    }
                }
            }
        }
        self.root.work.step(cx, observe)?;
        // A failed or cancelled build never publishes a partial directory.
        self.routes = Some(routes);
        Ok(())
    }

    async fn incident<C: Send>(
        &mut self,
        cx: &QueryCx,
        request: Incidence,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        self.build_routes(cx, observe).await?;
        // Only routed blocks receive heads; grow their capacity under a charge
        // before heap insertion. Root heads and their position stay unchanged.
        let mut state = State {
            heads: Vec::new(),
            active: None,
            initialized: true,
            _head_charge: self.root.partition.reserve_scan_bytes(cx, 1024)?,
        };
        let mut after_block = None;
        loop {
            let block = self.routes.as_ref().expect("complete routing directory")
                .next_block(request, after_block, cx, &mut self.root.work, observe)?;
            let Some(block) = block else { break; };
            after_block = Some(block);
            self.root.load(&mut state, cx, block, observe).await?;
            let active = state.active.as_ref().ok_or_else(invalid_load)?;
            let mut ordinal = 0;
            if let Some(after) = request.after {
                let mut end = active.order.len();
                while ordinal < end {
                    self.root.work.step(cx, observe)?;
                    let middle = ordinal + (end - ordinal) / 2;
                    if active.order[middle].0 <= after { ordinal = middle + 1; }
                    else { end = middle; }
                }
            }
            let mut head = None;
            for (at, &(eid, created, original)) in active.order.iter().enumerate().skip(ordinal) {
                self.root.work.step(cx, observe)?;
                let entry = active.image.0.get(original).ok_or_else(invalid_load)?;
                if request.matches(entry) {
                    head = Some(Reverse(Head { eid, created, block, ordinal: at }));
                    break;
                }
            }
            if let Some(head) = head {
                state._head_charge.grow(cx, 512).map_err(BufferedReadError::Memory)?;
                state.heads.try_reserve(1).map_err(|_| BufferedReadError::Memory(
                    MemoryError::AllocationFailed { requested: state._head_charge.bytes() }
                ))?;
                if state.heads.capacity().checked_mul(size_of::<Reverse<Head>>())
                    .is_none_or(|bytes| bytes > state._head_charge.bytes())
                {
                    return Err(BufferedReadError::SizeOverflow.into());
                }
                heap_push(&mut state.heads, head, &mut || self.root.work.step(cx, observe))?;
            }
        }
        // The admission validator fixes each EId's endpoints and relation.
        // Therefore routing includes EVERY version/restatement of this EId;
        // native advance sees the same creation/publication order as root scan.
        let candidate = self.root.advance(&mut state, cx, observe).await?;
        drop(state);
        match candidate {
            Some(candidate) => self.root.resolve_endpoints(
                cx, candidate, request.relation, observe).await.map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests;
