//! Ordered identified-edge history merging through the admitted extent cache.
//!
//! Blocks are adjacency-ordered. A first fault sorts a bounded permutation of
//! original row positions; property locators ALWAYS use the original position.
//! One head and a compact position memo per block, one active decoded block
//! and one winning edge suffice. Refaults authenticate the same immutable
//! bytes but need not sort a block's positions again.
//! No full edge directory, result table or per-candidate partition rescan exists.

use crate::AdjacencyEntry;
use crate::edge_props::BlockProps;
use crate::store::{
    BufferedEdge, BufferedPartition, BufferedReadError, BufferedScanError, BufferedValue,
};
use crate::tiered::buffer::BufferError;
use crate::tiered::memory::{MemoryCharge, MemoryError};
use crate::vertex::VertexRow;
use asupersync::fs::Vfs;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalPropertyValue, CommitSeq, EId, QueryCx};
use std::cmp::Reverse;

mod incidence;
pub use incidence::{BufferedEdgeDirection, BufferedEdgeIncidenceScan, BufferedEdgeJoinScan};

/// Controls precede work and candidate history resolution. Identity is emitted
/// exactly once per EId, including an identity invisible at the selected cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferedEdgeScanEvent {
    Work,
    Identity(EId),
}

#[derive(Debug)]
pub struct BufferedEdgeCandidate<Row = BufferedValue<BufferedEdge>> {
    pub eid: EId,
    pub row: Option<Row>,
}

/// An authenticated visible edge without property materialization. Its source
/// reservation stays owned by this value; no empty property row is presented
/// as a complete stored record. This does not prove endpoint visibility.
pub struct BufferedEdgeTopology(BufferedValue<BufferedEdge>);

impl BufferedEdgeTopology {
    pub fn entry(&self) -> &AdjacencyEntry {
        &self.0.entry
    }
}

impl core::fmt::Debug for BufferedEdgeTopology {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedEdgeTopology([REDACTED])")
    }
}

/// One authenticated visible edge and the requested property's value, if any.
/// The owned reservation covers only this selected image; omitted properties
/// are never exposed as a complete stored edge. Endpoint visibility remains
/// the caller's responsibility, exactly as for topology candidates.
pub struct BufferedEdgeProperty(BufferedValue<BufferedEdge>);

impl BufferedEdgeProperty {
    pub fn entry(&self) -> &AdjacencyEntry {
        &self.0.entry
    }

    pub fn value(&self) -> Option<&CanonicalPropertyValue> {
        self.0.props.first().map(|(_, value)| value)
    }

    pub fn charged_bytes(&self) -> usize {
        self.0.charged_bytes()
    }
}

impl core::fmt::Debug for BufferedEdgeProperty {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedEdgeProperty([REDACTED])")
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgePropertyAccess {
    Read,
    Property(PropertyKeyId),
    Skip,
}

/// One complete edge and its visible endpoints, all resolved at the cursor's
/// cut. A self-loop retains one endpoint allocation, not two copies. This is
/// an owner-level storage record, not a Warden authorization grant.
pub struct BufferedEdgeEndpoints {
    edge: BufferedValue<BufferedEdge>,
    source: BufferedValue<VertexRow>,
    target: Option<BufferedValue<VertexRow>>,
}
impl BufferedEdgeEndpoints {
    pub fn edge(&self) -> &BufferedEdge {
        &self.edge
    }
    pub fn source_vertex(&self) -> &VertexRow {
        &self.source
    }
    pub fn target_vertex(&self) -> &VertexRow {
        self.target.as_deref().unwrap_or(&self.source)
    }
}
impl core::fmt::Debug for BufferedEdgeEndpoints {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BufferedEdgeEndpoints([REDACTED])")
    }
}

pub(crate) struct EdgeScanWork {
    used: usize,
    limit: usize,
}
impl EdgeScanWork {
    pub(crate) fn step<C>(
        &mut self,
        cx: &QueryCx,
        observe: &mut impl FnMut(BufferedEdgeScanEvent) -> Result<(), C>,
    ) -> Result<(), BufferedScanError<C>> {
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        let next = self
            .used
            .checked_add(1)
            .ok_or(BufferedReadError::SizeOverflow)?;
        if next > self.limit {
            return Err(BufferedReadError::Limit {
                resource: "buffered source work",
                requested: next,
                limit: self.limit,
            }
            .into());
        }
        observe(BufferedEdgeScanEvent::Work).map_err(BufferedScanError::Control)?;
        self.used = next;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Head {
    eid: EId,
    created: CommitSeq,
    block: usize,
    // Ordinal in the block's EId-sorted permutation, NOT a property locator.
    ordinal: usize,
    // Private memo slot. Keep this after the complete historical order key.
    slot: usize,
}

type Key = (EId, CommitSeq, usize);
struct ActiveBlock {
    block: usize,
    slot: Option<usize>,
    // Whether this state still owns an unconsumed heap head for the block.
    has_head: bool,
    image: BufferedValue<(Vec<AdjacencyEntry>, Option<BlockProps>)>,
    properties: EdgePropertyAccess,
    order: Vec<Key>,
    _order_charge: MemoryCharge,
}
struct State {
    heads: Vec<Reverse<Head>>,
    // Only original positions, never identities or properties. Allocate a
    // memo only when an unfinished block is displaced. One- and two-row
    // blocks stay on the bounded sort path without another allocation.
    orders: Vec<Option<Vec<u8>>>,
    order_bytes: usize,
    retain_orders: bool,
    active: Option<ActiveBlock>,
    initialized: bool,
    // A retained incidence merge filters each refault's position permutation,
    // never its property locators or the versions of a matching identity.
    incidence: Option<incidence::Incidence>,
    // Root heads are admitted only when the root stream is first demanded.
    // Incidence positions own their smaller, independently reserved heaps.
    _head_charge: Option<MemoryCharge>,
}

impl State {
    /// Both directory capacities and every memo capacity share the existing
    /// head reservation. No retained allocation may hide behind vector length.
    fn retained_bytes(&self) -> Option<usize> {
        self.heads
            .capacity()
            .checked_mul(size_of::<Reverse<Head>>())?
            .checked_add(
                self.orders
                    .capacity()
                    .checked_mul(size_of::<Option<Vec<u8>>>())?,
            )?
            .checked_add(self.order_bytes)
    }

    fn check_retained_bytes(&self) -> Result<(), BufferedReadError> {
        let reserved = self._head_charge.as_ref().map_or(0, MemoryCharge::bytes);
        if self.retained_bytes().is_none_or(|bytes| bytes > reserved) {
            return Err(BufferedReadError::SizeOverflow);
        }
        Ok(())
    }

    fn remember_order<C>(
        &mut self,
        slot: usize,
        control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
    ) -> Result<(), BufferedScanError<C>> {
        let cached = self.orders.get(slot).ok_or_else(invalid_load)?;
        let active = self.active.as_ref().ok_or_else(invalid_load)?;
        if cached.is_some() || active.image.0.len() <= 2 {
            return Ok(());
        }
        let available = self
            ._head_charge
            .as_ref()
            .ok_or_else(invalid_load)?
            .bytes()
            .checked_sub(
                self.retained_bytes()
                    .ok_or(BufferedReadError::SizeOverflow)?,
            )
            .ok_or(BufferedReadError::SizeOverflow)?;
        let positions = compact_positions(&active.order, available, control)?;
        self.order_bytes = self
            .order_bytes
            .checked_add(positions.capacity())
            .ok_or(BufferedReadError::SizeOverflow)?;
        self.orders[slot] = Some(positions);
        self.check_retained_bytes()?;
        Ok(())
    }
}

/// The format admits at most 256 rows: original positions fit in u8 even
/// when the complete permutation has length 256. Copy controls precede every
/// element; interruption cannot publish a partially initialized memo.
fn compact_positions<C>(
    order: &[Key],
    available: usize,
    control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
) -> Result<Vec<u8>, BufferedScanError<C>> {
    if order.len() > crate::MAX_BLOCK_ENTRIES as usize || order.len() > available {
        return Err(BufferedReadError::SizeOverflow.into());
    }
    let mut positions = Vec::new();
    positions.try_reserve_exact(order.len()).map_err(|_| {
        BufferedReadError::Memory(MemoryError::AllocationFailed {
            requested: order.len(),
        })
    })?;
    if positions.capacity() > available {
        return Err(BufferedReadError::SizeOverflow.into());
    }
    for &(_, _, original) in order {
        control()?;
        positions.push(u8::try_from(original).map_err(|_| BufferedReadError::SizeOverflow)?);
    }
    Ok(positions)
}

/// Reconstruct keys from the freshly authenticated image, retaining original
/// property locators and full identity widths. The immutable object binding
/// proved the memo's order; no cached payload substitutes for this refault.
fn restore_positions<C>(
    entries: &[AdjacencyEntry],
    positions: &[u8],
    order: &mut Vec<Key>,
    control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
) -> Result<(), BufferedScanError<C>> {
    for &position in positions {
        control()?;
        let original = usize::from(position);
        let entry = entries.get(original).ok_or_else(invalid_load)?;
        order.push((entry.eid, entry.created_at, original));
    }
    Ok(())
}

/// A canonical k-way merge of immutable edge histories. Construction creates
/// an empty position without reserving its O(block count) root heap. First root
/// pull reserves that heap and inserts each authenticated minimum key.
/// Each refault decodes at most one
/// format-bounded block. Its first load sorts original positions with controls
/// before comparisons; subsequent loads rebuild keys from a compact memo.
/// Refault frequency can still be once per historical row: this removes
/// repeated sorting, not decoding or I/O. Head and position-memo capacities
/// share the same pre-admitted 512-byte-per-block reservation.
///
/// Complete histories are resolved before publishing each candidate. The last
/// eligible creation version/publication position wins, including retirement
/// restatements. Parallel identities remain distinct. No identity+1 sentinel
/// truncates u128::MAX. Endpoint resolution shares the SAME cumulative source
/// limit; endpoint payloads are selected using authenticated patch ranges.
///
/// Dropping a polled pending pull releases its private merge state and makes
/// the cursor terminal. Errors never become EOF on that pull; subsequent pulls
/// are fused. Close/drop do not scan the unread suffix. Output reservations
/// survive cursor/view drop. Initial root admission remains separately bounded.
/// A relation-bound root reserves metadata for matching blocks only on first
/// demand and never faults another relation. It seeks the admitted relation
/// directory in logarithmic work, then visits only matching block descriptors;
/// every comparison and visit is metered. The history, property and endpoint
/// resolution laws are unchanged.
pub struct BufferedEdgeScan<'source, V: Vfs> {
    partition: &'source mut BufferedPartition<V>,
    as_of: CommitSeq,
    blocks: usize,
    // Root selection only. Independent/nested incidence requests own their
    // own relation and may traverse other relations without widening this root.
    relation: Option<RelationId>,
    state: Option<State>,
    work: EdgeScanWork,
}
impl<V: Vfs> core::fmt::Debug for BufferedEdgeScan<'_, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BufferedEdgeScan")
            .field("snapshot", &self.as_of)
            .field("finished", &self.is_finished())
            .field("work", &self.work.used)
            .field("source", &"[REDACTED]")
            .finish()
    }
}
impl<'source, V: Vfs> BufferedEdgeScan<'source, V> {
    pub(crate) fn new(
        partition: &'source mut BufferedPartition<V>,
        cx: &QueryCx,
        as_of: CommitSeq,
        blocks: usize,
        limit: usize,
        relation: Option<RelationId>,
    ) -> Result<Self, BufferedReadError> {
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        Ok(Self {
            partition,
            as_of,
            blocks,
            relation,
            state: Some(State {
                heads: Vec::new(),
                orders: Vec::new(),
                order_bytes: 0,
                retain_orders: true,
                active: None,
                initialized: false,
                incidence: None,
                _head_charge: None,
            }),
            work: EdgeScanWork { used: 0, limit },
        })
    }

    pub const fn snapshot_seq(&self) -> CommitSeq {
        self.as_of
    }
    pub const fn work_used(&self) -> usize {
        self.work.used
    }
    pub fn is_finished(&self) -> bool {
        self.state.is_none()
    }
    pub fn close(&mut self) {
        self.state = None;
    }

    async fn load<C: Send>(
        &mut self,
        state: &mut State,
        cx: &QueryCx,
        block: usize,
        slot: Option<usize>,
        properties: EdgePropertyAccess,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<(), BufferedScanError<C>> {
        let slot = slot.filter(|_| state.retain_orders);
        if state.active.as_ref().is_some_and(|active| {
            active.block == block
                && (properties == EdgePropertyAccess::Skip
                    || active.properties == EdgePropertyAccess::Read
                    || active.properties == properties)
        }) {
            return Ok(());
        }
        // An exhausted or never-displaced block pays no memo allocation or
        // copy. One-shot incidence positions also have no reuse to amortize.
        let displaced = state.active.as_ref().and_then(|active| {
            (state.retain_orders && active.has_head)
                .then_some(active.slot)
                .flatten()
        });
        if let Some(slot) = displaced {
            state.remember_order(slot, &mut || self.work.step(cx, observe))?;
        }
        // Drop old decoded storage before faulting another block.
        state.active = None;
        let image = self
            .partition
            .edge_scan_block(cx, block, properties, &mut self.work, observe)
            .await?;
        let bytes = image
            .0
            .len()
            .checked_mul(size_of::<Key>())
            .and_then(|n| n.checked_add(1024))
            .ok_or(BufferedReadError::SizeOverflow)?;
        self.work.step(cx, observe)?;
        let charge = self.partition.reserve_scan_bytes(cx, bytes)?;
        let mut order = Vec::new();
        order.try_reserve_exact(image.0.len()).map_err(|_| {
            BufferedReadError::Memory(MemoryError::AllocationFailed { requested: bytes })
        })?;
        if order
            .capacity()
            .checked_mul(size_of::<Key>())
            .is_none_or(|n| n > bytes)
        {
            return Err(BufferedReadError::SizeOverflow.into());
        }
        if let Some(positions) = slot
            .map(|slot| state.orders.get(slot).ok_or_else(invalid_load))
            .transpose()?
            .and_then(Option::as_ref)
        {
            restore_positions(&image.0, positions, &mut order, &mut || {
                self.work.step(cx, observe)
            })?;
        } else {
            for (at, row) in image.0.iter().enumerate() {
                self.work.step(cx, observe)?;
                if state.incidence.is_some_and(|request| !request.matches(row)) {
                    continue;
                }
                order.push((row.eid, row.created_at, at));
            }
            sort(&mut order, &mut || self.work.step(cx, observe))?;
        }
        state.active = Some(ActiveBlock {
            block,
            slot,
            has_head: slot.is_some(),
            image,
            properties,
            order,
            _order_charge: charge,
        });
        Ok(())
    }

    async fn advance<C: Send>(
        &mut self,
        state: &mut State,
        cx: &QueryCx,
        properties: EdgePropertyAccess,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate>, BufferedScanError<C>> {
        if !state.initialized {
            let mut selected = self
                .relation
                .map(|relation| {
                    self.partition
                        .edge_scan_relation_blocks(relation, cx, &mut self.work, observe)
                })
                .transpose()?;
            let capacity = selected.as_ref().map_or(self.blocks, |blocks| blocks.len());
            if capacity != 0 {
                // A caller may convert this position into a selective
                // incidence stream without ever using the global root heap.
                // Admit its complete backing only on actual root demand,
                // before allocation or any authenticated head is consumed.
                // The admitted directory gives the exact selected head count
                // before one cache-aware reservation.
                // Growing a MemoryCharge directly would bypass reclamation of
                // unpinned extent frames and falsely refuse under cache pressure.
                let bytes = capacity
                    .checked_mul(512)
                    .and_then(|n| n.checked_add(1024))
                    .ok_or(BufferedReadError::SizeOverflow)?;
                let charge = self.partition.reserve_scan_bytes(cx, bytes)?;
                let mut heads: Vec<Reverse<Head>> = Vec::new();
                heads.try_reserve_exact(capacity).map_err(|_| {
                    BufferedReadError::Memory(MemoryError::AllocationFailed { requested: bytes })
                })?;
                let mut orders = Vec::new();
                orders.try_reserve_exact(capacity).map_err(|_| {
                    BufferedReadError::Memory(MemoryError::AllocationFailed { requested: bytes })
                })?;
                state.heads = heads;
                state.orders = orders;
                state._head_charge = Some(charge);
                state.check_retained_bytes()?;
            }
            let mut all = 0..self.blocks;
            loop {
                let block = if let Some(selected) = selected.as_mut() {
                    self.partition.edge_scan_next_relation_block(
                        selected,
                        cx,
                        &mut self.work,
                        observe,
                    )?
                } else if let Some(block) = all.next() {
                    self.work.step(cx, observe)?;
                    Some(block)
                } else {
                    None
                };
                let Some(block) = block else {
                    break;
                };
                if let Some((eid, created)) = self.partition.edge_scan_first(block, self.relation) {
                    // Exclude unrelated relations before heap insertion or any
                    // payload/property refault. Every matching block remains
                    // present even when its history is invisible at this cut.
                    let slot = state.orders.len();
                    state.orders.push(None);
                    heap_push(
                        &mut state.heads,
                        Reverse(Head {
                            eid,
                            created,
                            block,
                            ordinal: 0,
                            slot,
                        }),
                        &mut || self.work.step(cx, observe),
                    )?;
                }
            }
            state.initialized = true;
        }
        self.work.step(cx, observe)?;
        let Some(Reverse(first)) = state.heads.first().copied() else {
            return Ok(None);
        };
        observe(BufferedEdgeScanEvent::Identity(first.eid)).map_err(BufferedScanError::Control)?;
        let mut winner: Option<(BufferedEdge, MemoryCharge)> = None;
        while let Some(Reverse(head)) = state.heads.first().copied() {
            if head.eid != first.eid {
                break;
            }
            self.work.step(cx, observe)?;
            self.load(state, cx, head.block, Some(head.slot), properties, observe)
                .await?;
            let active = state.active.as_ref().ok_or_else(invalid_load)?;
            let &(eid, created, original) =
                active.order.get(head.ordinal).ok_or_else(invalid_load)?;
            if eid != head.eid || created != head.created {
                return Err(invalid_load().into());
            }
            let entry = active.image.0.get(original).ok_or_else(invalid_load)?;
            if entry.created_at <= self.as_of {
                // Reserve before copying either a first or replacement image.
                // Retained winner and active decoder have separate workspaces.
                if winner.is_none() {
                    let charge = self.partition.reserve_scan_workspace(cx)?;
                    winner = Some((
                        BufferedEdge {
                            entry: *entry,
                            props: Vec::new(),
                        },
                        charge,
                    ));
                }
                self.work.step(cx, observe)?;
                let value = &mut winner.as_mut().expect("reserved winner").0;
                value.entry = *entry;
                value.props = match properties {
                    EdgePropertyAccess::Read => active
                        .image
                        .1
                        .as_ref()
                        .map_or_else(Vec::new, |props| props.props_of(original)),
                    EdgePropertyAccess::Property(key) => active
                        .image
                        .1
                        .as_ref()
                        .and_then(|props| {
                            let row = props.props_ref(original);
                            row.binary_search_by_key(&key, |(key, _)| *key)
                                .ok()
                                .map(|at| row[at].clone())
                        })
                        .into_iter()
                        .collect(),
                    EdgePropertyAccess::Skip => Vec::new(),
                };
            }
            let next = active
                .order
                .get(head.ordinal + 1)
                .map(|&(eid, created, _)| {
                    Reverse(Head {
                        eid,
                        created,
                        block: head.block,
                        ordinal: head.ordinal + 1,
                        slot: head.slot,
                    })
                });
            heap_pop(&mut state.heads, &mut || self.work.step(cx, observe))?;
            if let Some(next) = next {
                heap_push(&mut state.heads, next, &mut || self.work.step(cx, observe))?;
            }
            state.active.as_mut().ok_or_else(invalid_load)?.has_head = next.is_some();
        }
        self.work.step(cx, observe)?;
        let row = winner
            .filter(|(row, _)| row.entry.visible_at(self.as_of))
            .map(|(row, charge)| {
                BufferedValue::from_reserved(row, charge)
                    .retain_scan_edge(&mut || self.work.step(cx, observe))
            })
            .transpose()?;
        Ok(Some(BufferedEdgeCandidate {
            eid: first.eid,
            row,
        }))
    }

    pub async fn next_candidate_with<C: Send>(
        &mut self,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate>, BufferedScanError<C>> {
        self.next_candidate_with_access(cx, EdgePropertyAccess::Read, observe)
            .await
    }

    /// Resolve the same complete EId history without faulting edge-property
    /// sidecars. Initial root admission already proved the topology/sidecar
    /// binding; every selected topology fault still verifies its immutable
    /// extent, descriptor summary and positional locators. An invisible winner
    /// remains a candidate with no row, not EOF. Endpoint visibility and any
    /// authorization projection remain the caller's responsibility.
    ///
    /// Topology and complete-record pulls may be interleaved. A later complete
    /// pull upgrades an active topology-only block before observing properties.
    /// Both paths share history ordering, work admission and terminal cleanup.
    pub async fn next_topology_candidate_with<C: Send>(
        &mut self,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeTopology>>, BufferedScanError<C>> {
        self.next_candidate_with_access(cx, EdgePropertyAccess::Skip, observe)
            .await
            .map(|candidate| {
                candidate.map(|candidate| BufferedEdgeCandidate {
                    eid: candidate.eid,
                    row: candidate.row.map(BufferedEdgeTopology),
                })
            })
    }

    /// Resolve the same complete EId history while materializing only one
    /// property per stored sidecar row. Every selected extent and sidecar is
    /// authenticated; all framing, key order and locator checks remain active.
    /// Missing newer properties overwrite older values, and an invisible winner
    /// remains a candidate with no row. The opaque result distinguishes a
    /// missing key from a present NULL without exposing partial edge records.
    ///
    /// Property keys, full rows and topology pulls may be interleaved. A partial
    /// active image satisfies only the same key or topology; full-row and
    /// different-key requests refault through their ordinary admitted path.
    pub async fn next_property_candidate_with<C: Send>(
        &mut self,
        cx: &QueryCx,
        key: PropertyKeyId,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeProperty>>, BufferedScanError<C>> {
        self.next_candidate_with_access(cx, EdgePropertyAccess::Property(key), observe)
            .await
            .map(|candidate| {
                candidate.map(|candidate| BufferedEdgeCandidate {
                    eid: candidate.eid,
                    row: candidate.row.map(BufferedEdgeProperty),
                })
            })
    }

    async fn next_candidate_with_access<C: Send>(
        &mut self,
        cx: &QueryCx,
        properties: EdgePropertyAccess,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate>, BufferedScanError<C>> {
        let Some(mut state) = self.state.take() else {
            return Ok(None);
        };
        let result = self.advance(&mut state, cx, properties, observe).await;
        if matches!(result, Ok(Some(_))) && !state.heads.is_empty() {
            self.state = Some(state);
        }
        result
    }

    /// Resolve endpoints only for a visible edge in the requested relation.
    /// None adds no endpoint filter; a relation-bound root keeps its original
    /// selection. Mismatching/invisible root identities still emit their single
    /// candidate admission, but do not read any endpoint payload.
    pub async fn next_with_endpoints<C: Send>(
        &mut self,
        cx: &QueryCx,
        relation: Option<RelationId>,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        let Some(mut state) = self.state.take() else {
            return Ok(None);
        };
        let Some(candidate) = self
            .advance(&mut state, cx, EdgePropertyAccess::Read, observe)
            .await?
        else {
            return Ok(None);
        };
        let candidate = self
            .resolve_endpoints(cx, candidate, relation, observe)
            .await?;
        if !state.heads.is_empty() {
            self.state = Some(state);
        }
        Ok(Some(candidate))
    }

    // Root and independently positioned incidence reads share the SAME visible
    // endpoint resolution and workspace ownership, after complete edge history.
    async fn resolve_endpoints<C: Send>(
        &mut self,
        cx: &QueryCx,
        candidate: BufferedEdgeCandidate,
        relation: Option<RelationId>,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<BufferedEdgeCandidate<BufferedEdgeEndpoints>, BufferedScanError<C>> {
        let row = if let Some(edge) = candidate
            .row
            .filter(|row| relation.is_none_or(|r| r == row.entry.relation))
        {
            let source = self
                .partition
                .edge_scan_vertex(cx, edge.entry.src, self.as_of, &mut self.work, observe)
                .await?
                .ok_or(BufferedReadError::DanglingEndpoint)?;
            let target = if edge.entry.src == edge.entry.dst {
                None
            } else {
                Some(
                    self.partition
                        .edge_scan_vertex(cx, edge.entry.dst, self.as_of, &mut self.work, observe)
                        .await?
                        .ok_or(BufferedReadError::DanglingEndpoint)?,
                )
            };
            self.work.step(cx, observe)?;
            Some(BufferedEdgeEndpoints {
                edge,
                source,
                target,
            })
        } else {
            None
        };
        Ok(BufferedEdgeCandidate {
            eid: candidate.eid,
            row,
        })
    }

    // Count-only incidence still resolves both endpoint histories at the same
    // cut. Complete initial admission and each pinned patch checksum authorize
    // topology projection; no endpoint property payload is decoded or exported.
    async fn resolve_endpoint_visibility<C: Send>(
        &mut self,
        cx: &QueryCx,
        candidate: BufferedEdgeCandidate,
        relation: Option<RelationId>,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<BufferedEdgeCandidate<()>, BufferedScanError<C>> {
        let row = if let Some(edge) = candidate
            .row
            .filter(|row| relation.is_none_or(|r| r == row.entry.relation))
        {
            if self
                .partition
                .edge_scan_vertex_topology(cx, edge.entry.src, self.as_of, &mut self.work, observe)
                .await?
                .is_none()
            {
                return Err(BufferedReadError::DanglingEndpoint.into());
            }
            // Drop each projected endpoint before admitting the next. A
            // self-loop validates its endpoint exactly once, as full rows do.
            if edge.entry.src != edge.entry.dst
                && self
                    .partition
                    .edge_scan_vertex_topology(
                        cx,
                        edge.entry.dst,
                        self.as_of,
                        &mut self.work,
                        observe,
                    )
                    .await?
                    .is_none()
            {
                return Err(BufferedReadError::DanglingEndpoint.into());
            }
            self.work.step(cx, observe)?;
            Some(())
        } else {
            None
        };
        Ok(BufferedEdgeCandidate {
            eid: candidate.eid,
            row,
        })
    }

    /// Convenience visible-edge pull. Query adapters use candidate admission.
    pub async fn next(
        &mut self,
        cx: &QueryCx,
    ) -> Result<Option<BufferedValue<BufferedEdge>>, BufferedReadError> {
        loop {
            match self
                .next_candidate_with(cx, &mut |_| Ok::<_, core::convert::Infallible>(()))
                .await
            {
                Ok(Some(candidate)) => {
                    if let Some(row) = candidate.row {
                        return Ok(Some(row));
                    }
                }
                Ok(None) => return Ok(None),
                Err(BufferedScanError::Read(error)) => return Err(error),
                Err(BufferedScanError::Control(never)) => match never {},
            }
        }
    }
}

fn invalid_load() -> BufferedReadError {
    BufferedReadError::Buffer(BufferError::InvalidLoad)
}

// One controlled binary-heap kernel serves both the global merge and the
// format-bounded local permutation sort. Every comparison precedes a control;
// interruption discards the private state rather than exposing a partial sort.
fn sift<T: Ord, E>(
    values: &mut [T],
    mut root: usize,
    end: usize,
    control: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    while root < end / 2 {
        let mut child = root * 2 + 1;
        if child + 1 < end {
            control()?;
            if values[child] < values[child + 1] {
                child += 1;
            }
        }
        control()?;
        if values[root] >= values[child] {
            break;
        }
        values.swap(root, child);
        root = child;
    }
    Ok(())
}
fn heap_push<T: Ord, E>(
    values: &mut Vec<T>,
    value: T,
    control: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    control()?;
    values.push(value);
    let mut at = values.len() - 1;
    while at > 0 {
        let parent = (at - 1) / 2;
        control()?;
        if values[parent] >= values[at] {
            break;
        }
        values.swap(parent, at);
        at = parent;
    }
    Ok(())
}
fn heap_pop<T: Ord, E>(
    values: &mut Vec<T>,
    control: &mut impl FnMut() -> Result<(), E>,
) -> Result<T, E> {
    control()?;
    let first = values.swap_remove(0);
    let len = values.len();
    sift(values, 0, len, control)?;
    Ok(first)
}
pub(crate) fn sort<T: Ord, E>(
    values: &mut [T],
    control: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let len = values.len();
    for start in (0..len / 2).rev() {
        sift(values, start, len, control)?;
    }
    for end in (1..len).rev() {
        control()?;
        values.swap(0, end);
        sift(values, 0, end, control)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
