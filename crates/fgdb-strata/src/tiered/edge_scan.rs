//! Ordered identified-edge history merging through the admitted extent cache.
//!
//! Blocks are adjacency-ordered. Each fault sorts a bounded permutation of its
//! original row positions; property locators ALWAYS use the original position.
//! One head per block, one active decoded block and one winning edge suffice.
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
use fgdb_delta_types::RelationId;
use fgdb_types::{CommitSeq, EId, QueryCx};
use std::cmp::Reverse;

mod incidence;
pub use incidence::{BufferedEdgeDirection, BufferedEdgeJoinScan};

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
}

type Key = (EId, CommitSeq, usize);
struct ActiveBlock {
    block: usize,
    image: BufferedValue<(Vec<AdjacencyEntry>, Option<BlockProps>)>,
    order: Vec<Key>,
    _order_charge: MemoryCharge,
}
struct State {
    heads: Vec<Reverse<Head>>,
    active: Option<ActiveBlock>,
    initialized: bool,
    _head_charge: MemoryCharge,
}

/// A canonical k-way merge of immutable edge histories. Construction reserves
/// O(block count) metadata, without reading payloads. First pull inserts the
/// authenticated minimum key of each block. Each refault decodes at most one
/// format-bounded block and sorts its original positions, with controls before
/// comparisons. Refault frequency can be up to once per historical row; this
/// is bounded resident execution, not an optimal sequential-I/O claim.
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
pub struct BufferedEdgeScan<'source, V: Vfs> {
    partition: &'source mut BufferedPartition<V>,
    as_of: CommitSeq,
    blocks: usize,
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
    ) -> Result<Self, BufferedReadError> {
        let bytes = blocks
            .checked_mul(512)
            .and_then(|n| n.checked_add(1024))
            .ok_or(BufferedReadError::SizeOverflow)?;
        let charge = partition.reserve_scan_bytes(cx, bytes)?;
        let mut heads: Vec<Reverse<Head>> = Vec::new();
        heads
            .try_reserve_exact(blocks)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        if heads
            .capacity()
            .checked_mul(size_of::<Reverse<Head>>())
            .is_none_or(|allocated| allocated > bytes)
        {
            return Err(BufferedReadError::SizeOverflow);
        }
        Ok(Self {
            partition,
            as_of,
            blocks,
            state: Some(State {
                heads,
                active: None,
                initialized: false,
                _head_charge: charge,
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
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<(), BufferedScanError<C>> {
        if state
            .active
            .as_ref()
            .is_some_and(|active| active.block == block)
        {
            return Ok(());
        }
        // Drop old decoded storage before faulting another block.
        state.active = None;
        let image = self
            .partition
            .edge_scan_block(cx, block, &mut self.work, observe)
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
        for (at, row) in image.0.iter().enumerate() {
            self.work.step(cx, observe)?;
            order.push((row.eid, row.created_at, at));
        }
        sort(&mut order, &mut || self.work.step(cx, observe))?;
        state.active = Some(ActiveBlock {
            block,
            image,
            order,
            _order_charge: charge,
        });
        Ok(())
    }

    async fn advance<C: Send>(
        &mut self,
        state: &mut State,
        cx: &QueryCx,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate>, BufferedScanError<C>> {
        if !state.initialized {
            for block in 0..self.blocks {
                self.work.step(cx, observe)?;
                if let Some((eid, created)) = self.partition.edge_scan_first(block) {
                    heap_push(
                        &mut state.heads,
                        Reverse(Head {
                            eid,
                            created,
                            block,
                            ordinal: 0,
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
            self.load(state, cx, head.block, observe).await?;
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
                value.props = active
                    .image
                    .1
                    .as_ref()
                    .map_or_else(Vec::new, |props| props.props_of(original));
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
                    })
                });
            heap_pop(&mut state.heads, &mut || self.work.step(cx, observe))?;
            if let Some(next) = next {
                heap_push(&mut state.heads, next, &mut || self.work.step(cx, observe))?;
            }
        }
        self.work.step(cx, observe)?;
        let row = winner
            .filter(|(row, _)| row.entry.visible_at(self.as_of))
            .map(|(row, charge)| BufferedValue::from_reserved(row, charge));
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
        let Some(mut state) = self.state.take() else {
            return Ok(None);
        };
        let result = self.advance(&mut state, cx, observe).await;
        if matches!(result, Ok(Some(_))) && !state.heads.is_empty() {
            self.state = Some(state);
        }
        result
    }

    /// Resolve endpoints only for a visible edge in the requested relation.
    /// None selects all relations. Mismatching/invisible identities still emit
    /// their single candidate admission, but do not read any endpoint payload.
    pub async fn next_with_endpoints<C: Send>(
        &mut self,
        cx: &QueryCx,
        relation: Option<RelationId>,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<BufferedEdgeCandidate<BufferedEdgeEndpoints>>, BufferedScanError<C>> {
        let Some(mut state) = self.state.take() else {
            return Ok(None);
        };
        let Some(candidate) = self.advance(&mut state, cx, observe).await? else {
            return Ok(None);
        };
        let candidate = self.resolve_endpoints(cx, candidate, relation, observe).await?;
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
fn sort<T: Ord, E>(values: &mut [T], control: &mut impl FnMut() -> Result<(), E>) -> Result<(), E> {
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
