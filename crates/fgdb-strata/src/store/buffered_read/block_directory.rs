//! Point and incidence routing over authenticated block descriptors, without edge rows.
//!
//! A block has one exact source/relation and a canonical destination range.
//! Outgoing seeks use the exact descriptor key; incoming seeks use a balanced
//! interval directory. A separate EId interval family covers point reads because
//! adjacency order does not imply EId order. Ranges are only exclusion evidence:
//! selected blocks are decoded and complete matching histories resolved normally.

use super::{
    BlockDescriptor, BufferedReadCx, BufferedReadError, MemoryCharge, MemoryError, MemoryPool,
};
use crate::tiered::edge_scan::BufferedEdgeDirection;
use fgdb_delta_types::RelationId;
use fgdb_types::{EId, VId};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Outgoing {
    source: VId,
    relation: RelationId,
    block: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Incoming {
    first: VId,
    last: VId,
    source: VId,
    relation: RelationId,
    block: usize,
    maximum: VId,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct EdgeRange {
    first: EId,
    last: EId,
    block: usize,
    maximum: EId,
}

pub(super) struct BlockDirectory {
    outgoing: Vec<Outgoing>,
    incoming: Vec<Incoming>,
    edges: Vec<EdgeRange>,
    // All descriptor vectors drop before their shared reservation.
    _charge: MemoryCharge,
}

impl BlockDirectory {
    pub(super) fn new(
        blocks: &[BlockDescriptor],
        cx: &impl BufferedReadCx,
        pool: &MemoryPool,
        control: &mut impl FnMut() -> Result<(), BufferedReadError>,
    ) -> Result<Self, BufferedReadError> {
        let bytes = blocks
            .len()
            .checked_mul(size_of::<Outgoing>() + size_of::<Incoming>() + size_of::<EdgeRange>())
            .ok_or(BufferedReadError::SizeOverflow)?;
        let charge = pool.reserve(cx, bytes)?;
        let mut outgoing = Vec::new();
        let mut incoming = Vec::new();
        let mut edges = Vec::new();
        outgoing
            .try_reserve_exact(blocks.len())
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        incoming
            .try_reserve_exact(blocks.len())
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        edges
            .try_reserve_exact(blocks.len())
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        if outgoing
            .capacity()
            .checked_mul(size_of::<Outgoing>())
            .and_then(|outgoing| {
                incoming
                    .capacity()
                    .checked_mul(size_of::<Incoming>())
                    .and_then(|incoming| outgoing.checked_add(incoming))
            })
            .and_then(|allocated| {
                edges
                    .capacity()
                    .checked_mul(size_of::<EdgeRange>())
                    .and_then(|edges| allocated.checked_add(edges))
            })
            .is_none_or(|allocated| allocated > bytes)
        {
            return Err(BufferedReadError::SizeOverflow);
        }
        for (block, descriptor) in blocks.iter().enumerate() {
            control()?;
            if let Some(summary) = descriptor.summary {
                outgoing.push(Outgoing {
                    source: summary.source,
                    relation: summary.relation,
                    block,
                });
                incoming.push(Incoming {
                    first: summary.first_destination,
                    last: summary.last_destination,
                    source: summary.source,
                    relation: summary.relation,
                    block,
                    maximum: summary.last_destination,
                });
                edges.push(EdgeRange {
                    first: summary.first_edge.0,
                    last: summary.last_edge,
                    block,
                    maximum: summary.last_edge,
                });
            }
        }
        crate::tiered::edge_scan::sort(&mut outgoing, control)?;
        crate::tiered::edge_scan::sort(&mut incoming, control)?;
        crate::tiered::edge_scan::sort(&mut edges, control)?;
        augment(&mut incoming, control)?;
        augment_edges(&mut edges, control)?;
        Ok(Self {
            outgoing,
            incoming,
            edges,
            _charge: charge,
        })
    }

    pub(super) fn candidates(
        &self,
        endpoint: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
    ) -> Candidates {
        Candidates::new(self.incoming.len(), endpoint, relation, direction)
    }

    pub(super) fn next<E>(
        &self,
        candidates: &mut Candidates,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        candidates.next(&self.outgoing, &self.incoming, control)
    }

    pub(super) fn edge_candidates(&self, eid: EId) -> EdgeCandidates {
        EdgeCandidates::new(self.edges.len(), eid)
    }

    pub(super) fn next_edge<E>(
        &self,
        candidates: &mut EdgeCandidates,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        candidates.next(&self.edges, control)
    }
}

fn augment_edges<E>(
    ranges: &mut [EdgeRange],
    control: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<EId>, E> {
    if ranges.is_empty() {
        return Ok(None);
    }
    control()?;
    let middle = ranges.len() / 2;
    let (left, rest) = ranges.split_at_mut(middle);
    let (root, right) = rest.split_first_mut().expect("nonempty midpoint");
    let mut maximum = root.last;
    if let Some(last) = augment_edges(left, control)? {
        maximum = maximum.max(last);
    }
    if let Some(last) = augment_edges(right, control)? {
        maximum = maximum.max(last);
    }
    root.maximum = maximum;
    Ok(Some(maximum))
}

/// A bounded position over all possible histories of one full-width EId.
pub(super) struct EdgeCandidates {
    eid: EId,
    pending: [(usize, usize); usize::BITS as usize],
    depth: usize,
}

impl EdgeCandidates {
    fn new(edges: usize, eid: EId) -> Self {
        let mut result = Self {
            eid,
            pending: [(0, 0); usize::BITS as usize],
            depth: 0,
        };
        result.push(0, edges);
        result
    }

    fn push(&mut self, first: usize, end: usize) {
        if first < end {
            self.pending[self.depth] = (first, end);
            self.depth += 1;
        }
    }

    fn next<E>(
        &mut self,
        ranges: &[EdgeRange],
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        while self.depth != 0 {
            control()?;
            self.depth -= 1;
            let (first, end) = self.pending[self.depth];
            let middle = first + (end - first) / 2;
            let entry = ranges[middle];
            if ranges[first].first > self.eid || entry.maximum < self.eid {
                continue;
            }
            if entry.first <= self.eid {
                self.push(middle + 1, end);
            }
            self.push(first, middle);
            if entry.first <= self.eid && self.eid <= entry.last {
                return Ok(Some(entry.block));
            }
        }
        Ok(None)
    }
}

fn augment<E>(
    ranges: &mut [Incoming],
    control: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<VId>, E> {
    if ranges.is_empty() {
        return Ok(None);
    }
    control()?;
    let middle = ranges.len() / 2;
    let (left, rest) = ranges.split_at_mut(middle);
    let (root, right) = rest.split_first_mut().expect("nonempty midpoint");
    let mut maximum = root.last;
    if let Some(last) = augment(left, control)? {
        maximum = maximum.max(last);
    }
    if let Some(last) = augment(right, control)? {
        maximum = maximum.max(last);
    }
    root.maximum = maximum;
    Ok(Some(maximum))
}

/// An owned metadata position, with no borrow or allocated candidate bag.
/// The balanced incoming tree bounds its stack by the address-space width.
pub(crate) struct Candidates {
    endpoint: VId,
    relation: Option<RelationId>,
    direction: BufferedEdgeDirection,
    outgoing_at: Option<usize>,
    outgoing_done: bool,
    pending: [(usize, usize); usize::BITS as usize],
    depth: usize,
}

impl Candidates {
    fn new(
        incoming: usize,
        endpoint: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
    ) -> Self {
        let mut result = Self {
            endpoint,
            relation,
            direction,
            outgoing_at: None,
            outgoing_done: direction == BufferedEdgeDirection::Incoming,
            pending: [(0, 0); usize::BITS as usize],
            depth: 0,
        };
        if direction != BufferedEdgeDirection::Outgoing {
            result.push(0, incoming);
        }
        result
    }

    fn push(&mut self, first: usize, end: usize) {
        if first < end {
            self.pending[self.depth] = (first, end);
            self.depth += 1;
        }
    }

    fn next<E>(
        &mut self,
        outgoing: &[Outgoing],
        incoming: &[Incoming],
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        if !self.outgoing_done {
            let at = match self.outgoing_at {
                Some(at) => at,
                None => {
                    let key = (self.endpoint, self.relation.unwrap_or(RelationId(0)));
                    let (mut first, mut end) = (0, outgoing.len());
                    while first < end {
                        control()?;
                        let middle = first + (end - first) / 2;
                        let entry = outgoing[middle];
                        if (entry.source, entry.relation) < key {
                            first = middle + 1;
                        } else {
                            end = middle;
                        }
                    }
                    first
                }
            };
            if let Some(entry) = outgoing.get(at) {
                control()?;
                if entry.source == self.endpoint
                    && self
                        .relation
                        .is_none_or(|relation| relation == entry.relation)
                {
                    self.outgoing_at = Some(at + 1);
                    return Ok(Some(entry.block));
                }
            }
            self.outgoing_done = true;
        }
        while self.depth != 0 {
            control()?;
            self.depth -= 1;
            let (first, end) = self.pending[self.depth];
            let middle = first + (end - first) / 2;
            let entry = incoming[middle];
            if incoming[first].first > self.endpoint || entry.maximum < self.endpoint {
                continue;
            }
            if entry.first <= self.endpoint {
                self.push(middle + 1, end);
            }
            self.push(first, middle);
            if entry.first <= self.endpoint
                && self.endpoint <= entry.last
                && self.relation.is_none_or(|relation| relation == entry.relation)
                // Undirected outgoing already selected this entire block,
                // including its loops and every matching historical version.
                && !(self.direction == BufferedEdgeDirection::Undirected
                    && entry.source == self.endpoint)
            {
                return Ok(Some(entry.block));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
