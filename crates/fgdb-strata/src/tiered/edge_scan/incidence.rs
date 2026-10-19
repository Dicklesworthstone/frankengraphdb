//! Independent cold incidence positions over one admitted partition. Shared
//! authenticated descriptor routing excludes unrelated payloads before reads.
//! The existing EId merge selects history winners and property locators.

use super::*;
use fgdb_types::VId;

mod stream;
pub use stream::BufferedEdgeIncidenceScan;

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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Incidence {
    endpoint: VId,
    relation: Option<RelationId>,
    direction: BufferedEdgeDirection,
    after: Option<EId>,
}
impl Incidence {
    pub(super) fn matches(self, entry: &AdjacencyEntry) -> bool {
        self.relation.is_none_or(|r| r == entry.relation)
            && self.direction.matches(entry, self.endpoint)
    }
}

struct Driver<'source, V: Vfs> {
    root: BufferedEdgeScan<'source, V>,
}

// At most one retained successor stream. `next` is the EXACT request that may
// reuse its heads, including the last returned historical identity. A repeat,
// backwards seek or different incidence starts anew; no traversal-depth or
// caller identity is guessed. None state records proven EOF without retaining
// a decoded block or heap. This is not a neighborhood/result cache.
struct Continuation {
    next: Incidence,
    state: Option<State>,
}

/// One root stream plus independently positioned indexed incidence requests.
/// Every incidence request seeks the partition's authenticated descriptor
/// directory. It does not scan graph payloads to construct query-local routing.
/// Outgoing keys are exact; incoming ranges can admit unrelated rows within a
/// selected block, which the ordinary incidence predicate then rejects.
/// All histories in a routed block remain authoritative until native resolution.
///
/// Routing is shared resident descriptor metadata, bounded and charged during
/// authenticated partition admission, with no per-edge routing entries. Query
/// seeks and payload reads share one cumulative source allowance. This is not
/// a durable index or an optimal I/O bound: broad incoming ranges can select
/// many blocks, and each successor can refault its routed bounded blocks.
/// No full edge directory, neighborhood payload bag or result table is kept.
///
/// Root exhaustion does not disable nested reads: the final root edge may still
/// have children. Close/error/drop of a POLLED pending call drops the complete
/// driver and root heads, and makes all later calls inert.
/// Returned edge/endpoints own their charges independently. This is owner-level
/// storage access, not a Warden grant or cross-process retention lease.
pub struct BufferedEdgeJoinScan<'source, V: Vfs> {
    driver: Option<Driver<'source, V>>,
    continuation: Option<Continuation>,
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
            continuation: None,
            driver: Some(Driver { root: self }),
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
    pub const fn snapshot_seq(&self) -> CommitSeq {
        self.as_of
    }
    pub fn work_used(&self) -> usize {
        self.driver.as_ref().map_or(self.work, |d| d.root.work.used)
    }
    pub fn is_closed(&self) -> bool {
        self.driver.is_none()
    }
    pub fn close(&mut self) {
        self.work = self.work_used();
        self.continuation = None;
        self.driver = None;
    }

    pub async fn next_with_endpoints<C: Send>(
        &mut self,
        cx: &QueryCx,
        relation: Option<RelationId>,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        let Some(mut driver) = self.driver.take() else {
            return Ok(None);
        };
        // A new root no longer needs its preceding child continuation. Drop
        // the charged child workspace before the root faults another block.
        self.continuation = None;
        let used = &mut self.work;
        let result = driver
            .root
            .next_with_endpoints(cx, relation, &mut |event| {
                observe(event)?;
                if event == BufferedEdgeScanEvent::Work {
                    *used = used.saturating_add(1);
                }
                Ok(())
            })
            .await;
        self.work = driver.root.work.used;
        if result.is_ok() {
            self.driver = Some(driver);
        }
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
        let Some(mut driver) = self.driver.take() else {
            return Ok(None);
        };
        self.continuation = None;
        let request = Incidence {
            endpoint,
            relation,
            direction,
            after,
        };
        let used = &mut self.work;
        let result = driver
            .incident(cx, request, &mut |event| {
                observe(event)?;
                if event == BufferedEdgeScanEvent::Work {
                    *used = used.saturating_add(1);
                }
                Ok(())
            })
            .await;
        self.work = driver.root.work.used;
        if result.is_ok() {
            self.driver = Some(driver);
        }
        result
    }

    /// The same exclusive historical successor contract as
    /// `next_incident_with_endpoints`, retaining one continuation between pulls.
    /// Reuse occurs only for the same endpoint/relation/direction and `after`
    /// equal to the last returned candidate's EId (including invisible ones).
    /// Repeated, skipped, backwards or interleaved positions are valid requests:
    /// they release the old continuation before seeding another bounded merge.
    /// Root pulls and the independent successor method also release it.
    ///
    /// Only matching histories enter the retained permutation. Original row
    /// positions remain property locators, and every version of a matching
    /// identity participates in the ordinary same-cut winner selection. An
    /// exhausted continuation retains only its request, not heap or payload.
    /// Even repeated EOF checks observe the cumulative work/cancellation limit.
    ///
    /// At most one additional routed heap and bounded decoded block remain
    /// charged between calls, beside the independent root.
    /// A deep join may still reseed after switching incidences; this is not a
    /// per-depth cache, persistent index or optimal I/O guarantee. Errors never
    /// retry through the independent path. All state is taken before awaiting,
    /// so a dropped polled future fuses the entire source just like other pulls.
    pub async fn next_incident_retained_with_endpoints<C: Send>(
        &mut self,
        cx: &QueryCx,
        endpoint: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
        after: Option<EId>,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        let Some(mut driver) = self.driver.take() else {
            return Ok(None);
        };
        // Both ownership roots leave self before the first possible suspension.
        let mut continuation = self.continuation.take();
        let request = Incidence {
            endpoint,
            relation,
            direction,
            after,
        };
        let used = &mut self.work;
        let result = driver
            .continue_incident(cx, request, &mut continuation, &mut |event| {
                observe(event)?;
                if event == BufferedEdgeScanEvent::Work {
                    *used = used.saturating_add(1);
                }
                Ok(())
            })
            .await;
        self.work = driver.root.work.used;
        if result.is_ok() {
            self.continuation = continuation;
            self.driver = Some(driver);
        }
        result
    }
}

impl<V: Vfs> Driver<'_, V> {
    async fn continue_incident<C: Send>(
        &mut self,
        cx: &QueryCx,
        request: Incidence,
        continuation: &mut Option<Continuation>,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        self.root.work.step(cx, observe)?;
        if continuation
            .as_ref()
            .is_none_or(|saved| saved.next != request)
        {
            // Do not retain an obsolete decoded block while allocating its
            // replacement, and never attach partial heads to a continuation.
            *continuation = None;
            let state = self.prepare_incident(cx, request, true, observe).await?;
            *continuation = Some(Continuation {
                next: request,
                state: Some(state),
            });
        }
        let saved = continuation.as_mut().expect("complete continuation");
        let Some(state) = saved.state.as_mut() else {
            return Ok(None);
        };
        let candidate = self.root.advance(state, cx, observe).await?;
        let exhausted = state.heads.is_empty();
        let result = match candidate {
            Some(candidate) => Some(
                self.root
                    .resolve_endpoints(cx, candidate, request.relation, observe)
                    .await?,
            ),
            None => None,
        };
        if let Some(candidate) = &result {
            saved.next.after = Some(candidate.eid);
        }
        if exhausted {
            saved.state = None;
        }
        Ok(result)
    }

    async fn incident<C: Send>(
        &mut self,
        cx: &QueryCx,
        request: Incidence,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        let mut state = self.prepare_incident(cx, request, false, observe).await?;
        // The admission validator fixes each EId's endpoints and relation.
        // Therefore routing includes EVERY version/restatement of this EId;
        // native advance sees the same creation/publication order as root scan.
        let candidate = self.root.advance(&mut state, cx, observe).await?;
        drop(state);
        match candidate {
            Some(candidate) => self
                .root
                .resolve_endpoints(cx, candidate, request.relation, observe)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    async fn prepare_incident<C: Send>(
        &mut self,
        cx: &QueryCx,
        request: Incidence,
        retained: bool,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<State, BufferedScanError<C>> {
        self.root.work.step(cx, observe)?;
        let mut blocks = self.root.partition.edge_scan_incidence_blocks(
            request.endpoint,
            request.relation,
            request.direction,
        );
        // Only routed blocks receive heads; grow their capacity under a charge
        // before heap insertion. Root heads and their position stay unchanged.
        let mut state = State {
            heads: Vec::new(),
            active: None,
            initialized: true,
            incidence: retained.then_some(request),
            _head_charge: self
                .root
                .partition
                .reserve_scan_bytes(cx, 1024 + size_of_val(&blocks))?,
        };
        loop {
            let block = self.root.partition.edge_scan_next_incidence_block(
                &mut blocks,
                cx,
                &mut self.root.work,
                observe,
            )?;
            let Some(block) = block else {
                break;
            };
            self.root.load(&mut state, cx, block, observe).await?;
            let active = state.active.as_ref().ok_or_else(invalid_load)?;
            let mut ordinal = 0;
            if let Some(after) = request.after {
                let mut end = active.order.len();
                while ordinal < end {
                    self.root.work.step(cx, observe)?;
                    let middle = ordinal + (end - ordinal) / 2;
                    if active.order[middle].0 <= after {
                        ordinal = middle + 1;
                    } else {
                        end = middle;
                    }
                }
            }
            let mut head = None;
            for (at, &(eid, created, original)) in active.order.iter().enumerate().skip(ordinal) {
                self.root.work.step(cx, observe)?;
                let entry = active.image.0.get(original).ok_or_else(invalid_load)?;
                if request.matches(entry) {
                    head = Some(Reverse(Head {
                        eid,
                        created,
                        block,
                        ordinal: at,
                    }));
                    break;
                }
            }
            if let Some(head) = head {
                state
                    ._head_charge
                    .grow(cx, 512)
                    .map_err(BufferedReadError::Memory)?;
                state.heads.try_reserve(1).map_err(|_| {
                    BufferedReadError::Memory(MemoryError::AllocationFailed {
                        requested: state._head_charge.bytes(),
                    })
                })?;
                if state
                    .heads
                    .capacity()
                    .checked_mul(size_of::<Reverse<Head>>())
                    .is_none_or(|bytes| bytes > state._head_charge.bytes())
                {
                    return Err(BufferedReadError::SizeOverflow.into());
                }
                heap_push(&mut state.heads, head, &mut || {
                    self.root.work.step(cx, observe)
                })?;
            }
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests;
