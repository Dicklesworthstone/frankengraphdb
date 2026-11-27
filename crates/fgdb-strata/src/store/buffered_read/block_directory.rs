//! Point and incidence routing over authenticated block descriptors, without edge rows.
//!
//! A block has one exact source/relation and a canonical destination range.
//! Outgoing seeks use the exact descriptor key; incoming seeks use a balanced
//! interval directory. A separate EId interval family covers point reads because
//! adjacency order does not imply EId order. Relation roots and seals seek their
//! publication-ordered block range without visiting unrelated histories. Ranges
//! are only exclusion evidence: selected blocks are decoded and complete matching
//! histories resolved normally.

use super::{
    BlockDescriptor, BufferedReadCx, BufferedReadError, MemoryCharge, MemoryError, MemoryPool,
};
use crate::tiered::edge_scan::BufferedEdgeDirection;
use fgdb_delta_types::RelationId;
use fgdb_types::{EId, VId};

// Subtree maxima name the descriptor owning the inclusive upper bound.
// All full-width endpoints and relations remain in the admitted descriptors.
#[derive(Clone, Copy)]
struct Interval {
    block: usize,
    maximum: usize,
}

pub(super) struct BlockDirectory {
    routes: Routes,
    _charge: MemoryCharge,
}

struct Routes {
    outgoing: Vec<usize>,
    incoming: Vec<Interval>,
    edges: Vec<Interval>,
    relations: Vec<usize>,
}

fn summary(blocks: &[BlockDescriptor], index: usize) -> &super::BlockSummary {
    blocks[index]
        .summary
        .as_ref()
        .expect("only nonempty admitted descriptors are indexed")
}

fn slots<T>(count: usize) -> Result<Vec<T>, BufferedReadError> {
    let bytes = count
        .checked_mul(size_of::<T>())
        .ok_or(BufferedReadError::SizeOverflow)?;
    let mut slots = Vec::new();
    slots
        .try_reserve_exact(count)
        .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
    if slots.capacity() > count {
        return Err(BufferedReadError::SizeOverflow);
    }
    Ok(slots)
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
            .checked_mul(2 * size_of::<usize>() + 2 * size_of::<Interval>())
            .ok_or(BufferedReadError::SizeOverflow)?;
        let charge = pool.reserve(cx, bytes)?;
        Ok(Self {
            routes: Routes::build(blocks, control)?,
            _charge: charge,
        })
    }

    pub(super) fn candidates(
        &self,
        endpoint: VId,
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
    ) -> Candidates {
        Candidates::new(self.routes.incoming.len(), endpoint, relation, direction)
    }

    pub(super) fn next<E>(
        &self,
        descriptors: &[BlockDescriptor],
        candidates: &mut Candidates,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        candidates.next(
            descriptors,
            &self.routes.outgoing,
            &self.routes.incoming,
            control,
        )
    }

    pub(super) fn root_candidates<'a>(
        &self,
        vertices: &'a [VId],
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
    ) -> RootCandidates<'a> {
        RootCandidates::new(self.routes.incoming.len(), vertices, relation, direction)
    }

    pub(super) fn next_root<E>(
        &self,
        descriptors: &[BlockDescriptor],
        candidates: &mut RootCandidates<'_>,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        candidates.next(
            descriptors,
            &self.routes.outgoing,
            &self.routes.incoming,
            control,
        )
    }

    pub(super) fn edge_candidates(&self, eid: EId) -> EdgeCandidates {
        EdgeCandidates::new(self.routes.edges.len(), eid)
    }

    pub(super) fn next_edge<E>(
        &self,
        descriptors: &[BlockDescriptor],
        candidates: &mut EdgeCandidates,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        candidates.next(descriptors, &self.routes.edges, control)
    }

    pub(super) fn relation_blocks<E>(
        &self,
        descriptors: &[BlockDescriptor],
        relation: RelationId,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<RelationBlocks, E> {
        RelationBlocks::new(descriptors, &self.routes.relations, relation, control)
    }

    pub(super) fn next_relation<E>(
        &self,
        blocks: &mut RelationBlocks,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        blocks.next(&self.routes.relations, control)
    }
}

// Bulk imports often arrive in one relation or in source order. Confirm that
// an index is already ordered with one metered comparison per adjacent pair.
// Stop probing at the first inversion and use the ordinary controlled sort.
fn sort_ordinals<T, K: Ord, E>(
    values: &mut [T],
    key: impl Fn(&T) -> K,
    control: &mut impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    for pair in values.windows(2) {
        control()?;
        if key(&pair[0]) > key(&pair[1]) {
            return crate::tiered::edge_scan::sort_by(values, |a, b| key(a).cmp(&key(b)), control);
        }
    }
    Ok(())
}

impl Routes {
    fn build(
        blocks: &[BlockDescriptor],
        control: &mut impl FnMut() -> Result<(), BufferedReadError>,
    ) -> Result<Self, BufferedReadError> {
        let mut outgoing = slots(blocks.len())?;
        let mut incoming = slots(blocks.len())?;
        let mut edges = slots(blocks.len())?;
        let mut relations = slots(blocks.len())?;
        for (block, descriptor) in blocks.iter().enumerate() {
            control()?;
            if descriptor.summary.is_some() {
                let range = Interval {
                    block,
                    maximum: block,
                };
                outgoing.push(block);
                incoming.push(range);
                edges.push(range);
                relations.push(block);
            }
        }
        let outgoing_key = |block| {
            let fields = summary(blocks, block);
            (fields.source, fields.relation, block)
        };
        let incoming_key = |block| {
            let fields = summary(blocks, block);
            (
                fields.first_destination,
                fields.last_destination,
                fields.source,
                fields.relation,
                block,
            )
        };
        let edge_key = |block| {
            let fields = summary(blocks, block);
            (fields.first_edge.0, fields.last_edge, block)
        };
        let relation_key = |block| (summary(blocks, block).relation, block);
        sort_ordinals(&mut outgoing, |block| outgoing_key(*block), control)?;
        sort_ordinals(&mut incoming, |range| incoming_key(range.block), control)?;
        sort_ordinals(&mut edges, |range| edge_key(range.block), control)?;
        sort_ordinals(&mut relations, |block| relation_key(*block), control)?;
        augment(
            &mut incoming,
            &|block| summary(blocks, block).last_destination,
            control,
        )?;
        augment(
            &mut edges,
            &|block| summary(blocks, block).last_edge,
            control,
        )?;
        Ok(Self {
            outgoing,
            incoming,
            edges,
            relations,
        })
    }
}

/// An allocation-free range in one admitted directory. Original publication
/// ordinals stay ascending within each relation, including retirement restatements.
#[derive(Clone, Copy)]
pub(crate) struct RelationBlocks {
    next: usize,
    end: usize,
}

impl RelationBlocks {
    fn new<E>(
        descriptors: &[BlockDescriptor],
        blocks: &[usize],
        relation: RelationId,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Self, E> {
        let (mut first, mut end) = (0, blocks.len());
        while first < end {
            control()?;
            let middle = first + (end - first) / 2;
            if summary(descriptors, blocks[middle]).relation < relation {
                first = middle + 1;
            } else {
                end = middle;
            }
        }
        let next = first;
        end = blocks.len();
        while first < end {
            control()?;
            let middle = first + (end - first) / 2;
            if summary(descriptors, blocks[middle]).relation <= relation {
                first = middle + 1;
            } else {
                end = middle;
            }
        }
        Ok(Self { next, end })
    }

    pub(crate) const fn len(&self) -> usize {
        self.end - self.next
    }

    fn next<E>(
        &mut self,
        blocks: &[usize],
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        if self.next == self.end {
            return Ok(None);
        }
        control()?;
        let block = blocks[self.next];
        self.next += 1;
        Ok(Some(block))
    }
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
        descriptors: &[BlockDescriptor],
        ranges: &[Interval],
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        while self.depth != 0 {
            control()?;
            self.depth -= 1;
            let (first, end) = self.pending[self.depth];
            let middle = first + (end - first) / 2;
            let range = ranges[middle];
            let entry = summary(descriptors, range.block);
            let lower = summary(descriptors, ranges[first].block).first_edge.0;
            let upper = summary(descriptors, range.maximum).last_edge;
            if lower > self.eid || upper < self.eid {
                continue;
            }
            if entry.first_edge.0 <= self.eid {
                self.push(middle + 1, end);
            }
            self.push(first, middle);
            if entry.first_edge.0 <= self.eid && self.eid <= entry.last_edge {
                return Ok(Some(range.block));
            }
        }
        Ok(None)
    }
}

fn augment<K: Ord, E>(
    ranges: &mut [Interval],
    upper: &impl Fn(usize) -> K,
    control: &mut impl FnMut() -> Result<(), E>,
) -> Result<Option<usize>, E> {
    if ranges.is_empty() {
        return Ok(None);
    }
    control()?;
    let middle = ranges.len() / 2;
    let (left, rest) = ranges.split_at_mut(middle);
    let (root, right) = rest.split_first_mut().expect("nonempty midpoint");
    let mut maximum = root.block;
    for child in [left, right] {
        if let Some(last) = augment(child, upper, control)?
            && upper(last) > upper(maximum)
        {
            maximum = last;
        }
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
        descriptors: &[BlockDescriptor],
        outgoing: &[usize],
        incoming: &[Interval],
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
                        let entry = summary(descriptors, outgoing[middle]);
                        if (entry.source, entry.relation) < key {
                            first = middle + 1;
                        } else {
                            end = middle;
                        }
                    }
                    first
                }
            };
            if let Some(&block) = outgoing.get(at) {
                control()?;
                let entry = summary(descriptors, block);
                if entry.source == self.endpoint
                    && self
                        .relation
                        .is_none_or(|relation| relation == entry.relation)
                {
                    self.outgoing_at = Some(at + 1);
                    return Ok(Some(block));
                }
            }
            self.outgoing_done = true;
        }
        while self.depth != 0 {
            control()?;
            self.depth -= 1;
            let (first, end) = self.pending[self.depth];
            let middle = first + (end - first) / 2;
            let range = incoming[middle];
            let entry = summary(descriptors, range.block);
            let lower = summary(descriptors, incoming[first].block).first_destination;
            let upper = summary(descriptors, range.maximum).last_destination;
            if lower > self.endpoint || upper < self.endpoint {
                continue;
            }
            if entry.first_destination <= self.endpoint {
                self.push(middle + 1, end);
            }
            self.push(first, middle);
            if entry.first_destination <= self.endpoint
                && self.endpoint <= entry.last_destination
                && self.relation.is_none_or(|relation| relation == entry.relation)
                // Undirected outgoing already selected this entire block,
                // including its loops and every matching historical version.
                && !(self.direction == BufferedEdgeDirection::Undirected
                    && entry.source == self.endpoint)
            {
                return Ok(Some(range.block));
            }
        }
        Ok(None)
    }
}

/// Select each descriptor at most once for a complete sorted, distinct domain.
/// Outgoing ranges have one exact source; the incoming interval tree is walked
/// once for the entire domain. Its fixed-depth stack needs no corpus bitmap or
/// per-seed candidate bag. The caller validates the domain before source work.
pub(crate) struct RootCandidates<'a> {
    vertices: &'a [VId],
    relation: Option<RelationId>,
    direction: BufferedEdgeDirection,
    outgoing_seed: usize,
    outgoing_first: usize,
    outgoing_at: Option<usize>,
    pending: [(usize, usize); usize::BITS as usize],
    depth: usize,
}

impl<'a> RootCandidates<'a> {
    fn new(
        incoming: usize,
        vertices: &'a [VId],
        relation: Option<RelationId>,
        direction: BufferedEdgeDirection,
    ) -> Self {
        let mut result = Self {
            vertices,
            relation,
            direction,
            outgoing_seed: if direction == BufferedEdgeDirection::Incoming {
                vertices.len()
            } else {
                0
            },
            outgoing_first: 0,
            outgoing_at: None,
            pending: [(0, 0); usize::BITS as usize],
            depth: 0,
        };
        if !vertices.is_empty() && direction != BufferedEdgeDirection::Outgoing {
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

    fn intersects<E>(
        &self,
        first: VId,
        last: VId,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<bool, E> {
        let (mut low, mut high) = (0, self.vertices.len());
        while low < high {
            control()?;
            let middle = low + (high - low) / 2;
            if self.vertices[middle] < first {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        control()?;
        Ok(self.vertices.get(low).is_some_and(|&vertex| vertex <= last))
    }

    fn next<E>(
        &mut self,
        descriptors: &[BlockDescriptor],
        outgoing: &[usize],
        incoming: &[Interval],
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        while let Some(&vertex) = self.vertices.get(self.outgoing_seed) {
            control()?;
            let at = match self.outgoing_at {
                Some(at) => at,
                None => {
                    let key = (vertex, self.relation.unwrap_or(RelationId(0)));
                    let (mut first, mut end) = (self.outgoing_first, outgoing.len());
                    while first < end {
                        control()?;
                        let middle = first + (end - first) / 2;
                        let entry = summary(descriptors, outgoing[middle]);
                        if (entry.source, entry.relation) < key {
                            first = middle + 1;
                        } else {
                            end = middle;
                        }
                    }
                    first
                }
            };
            let Some(&block) = outgoing.get(at) else {
                self.outgoing_seed = self.vertices.len();
                break;
            };
            control()?;
            let entry = summary(descriptors, block);
            if entry.source == vertex
                && self
                    .relation
                    .is_none_or(|relation| relation == entry.relation)
            {
                self.outgoing_at = Some(at + 1);
                return Ok(Some(block));
            }
            self.outgoing_at = None;
            self.outgoing_first = at;
            self.outgoing_seed += 1;
        }
        while self.depth != 0 {
            control()?;
            self.depth -= 1;
            let (first, end) = self.pending[self.depth];
            let middle = first + (end - first) / 2;
            let range = incoming[middle];
            let entry = summary(descriptors, range.block);
            let lower = summary(descriptors, incoming[first].block).first_destination;
            let upper = summary(descriptors, range.maximum).last_destination;
            if !self.intersects(lower, upper, control)? {
                continue;
            }
            // The last seed can exclude the right subtree without a search;
            // subtree interval exclusion handles every other absence case.
            if self
                .vertices
                .last()
                .is_some_and(|&last| entry.first_destination <= last)
            {
                self.push(middle + 1, end);
            }
            self.push(first, middle);
            if self
                .relation
                .is_none_or(|relation| relation == entry.relation)
                && !(self.direction == BufferedEdgeDirection::Undirected
                    && self.intersects(entry.source, entry.source, control)?)
                && self.intersects(entry.first_destination, entry.last_destination, control)?
            {
                return Ok(Some(range.block));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
