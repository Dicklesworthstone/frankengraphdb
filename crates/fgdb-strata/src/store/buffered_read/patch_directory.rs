//! Immutable interval routing over fully admitted vertex-patch fences.
//!
//! The balanced tree retains one fixed-width entry per patch, never identities
//! or graph fields. A point read prunes subtrees whose identity ranges cannot
//! contain its key. Overlapping histories remain candidates, including patches
//! that only restate a retirement. Payload admission and decoding stay with the
//! ordinary reader, which resolves ties by publication order explicitly.

use super::{
    BufferedReadCx, BufferedReadError, MemoryCharge, MemoryError, MemoryPool, PatchDescriptor,
};
use fgdb_types::VId;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Range {
    first: VId,
    last: VId,
    patch: usize,
    // The maximum upper fence of the subtree rooted at this sorted midpoint.
    // Filled after sorting; subsequent operations never reorder the entries.
    maximum: VId,
}

pub(super) struct PatchDirectory {
    ranges: Vec<Range>,
    // Declaration order refunds only after the corresponding allocation drops.
    _charge: MemoryCharge,
}

impl PatchDirectory {
    pub(super) fn new(
        patches: &[PatchDescriptor],
        cx: &impl BufferedReadCx,
        pool: &MemoryPool,
        control: &mut impl FnMut() -> Result<(), BufferedReadError>,
    ) -> Result<Self, BufferedReadError> {
        let bytes = patches
            .len()
            .checked_mul(size_of::<Range>())
            .ok_or(BufferedReadError::SizeOverflow)?;
        let charge = pool.reserve(cx, bytes)?;
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(patches.len())
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        if ranges
            .capacity()
            .checked_mul(size_of::<Range>())
            .is_none_or(|allocated| allocated > bytes)
        {
            return Err(BufferedReadError::SizeOverflow);
        }
        for (patch, descriptor) in patches.iter().enumerate() {
            control()?;
            if let Some(((first, _), last)) = descriptor.first.zip(descriptor.last) {
                ranges.push(Range {
                    first,
                    last,
                    patch,
                    maximum: last,
                });
            }
        }
        crate::tiered::edge_scan::sort(&mut ranges, control)?;
        augment(&mut ranges, control)?;
        Ok(Self {
            ranges,
            _charge: charge,
        })
    }

    pub(super) fn candidates(&self, vid: VId) -> Candidates {
        Candidates::new(self.ranges.len(), vid)
    }

    pub(super) fn next<E>(
        &self,
        cursor: &mut Candidates,
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        cursor.next(&self.ranges, control)
    }
}

// Midpoint recursion has at most usize::BITS levels, including a full usize
// address space. Sorting and augmentation both checkpoint before bounded work.
fn augment<E>(
    ranges: &mut [Range],
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

// A traversal uses a fixed stack bounded by the balanced tree's height. It owns
// no vector of matching patches, so even all-overlapping histories add no query
// allocation. The usual bounded object workspace covers this small stack.
pub(super) struct Candidates {
    pending: [(usize, usize); usize::BITS as usize],
    depth: usize,
    vid: VId,
}

impl Candidates {
    fn new(length: usize, vid: VId) -> Self {
        let mut cursor = Self {
            pending: [(0, 0); usize::BITS as usize],
            depth: 0,
            vid,
        };
        cursor.push(0, length);
        cursor
    }

    fn push(&mut self, first: usize, end: usize) {
        if first < end {
            self.pending[self.depth] = (first, end);
            self.depth += 1;
        }
    }

    // No pending subtree means traversal is complete without inspecting any
    // additional node or spending another control event. A nonempty stack
    // makes no claim about whether its remaining ranges contain the identity.
    pub(super) fn is_exhausted(&self) -> bool {
        self.depth == 0
    }

    fn next<E>(
        &mut self,
        ranges: &[Range],
        control: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<usize>, E> {
        while self.depth != 0 {
            // One fixed-width node inspection, including pruning comparisons,
            // spends one work unit before inspecting its admitted metadata.
            control()?;
            self.depth -= 1;
            let (first, end) = self.pending[self.depth];
            let middle = first + (end - first) / 2;
            let node = ranges[middle];
            if ranges[first].first > self.vid || node.maximum < self.vid {
                continue;
            }
            if node.first <= self.vid {
                self.push(middle + 1, end);
            }
            self.push(first, middle);
            if node.first <= self.vid && self.vid <= node.last {
                return Ok(Some(node.patch));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
