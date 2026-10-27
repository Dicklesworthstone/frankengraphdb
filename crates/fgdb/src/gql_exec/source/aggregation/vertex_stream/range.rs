//! Ordered numeric range candidates over immutable scalar-value buckets.
//!
//! A controlled lookahead counts at most 65 buckets. Selective ranges retain a
//! governed min-heap with at most 64 roots and one VId per root. Wider ranges
//! check historical membership in identity order, one identity per cooperative
//! source step. Both paths yield the same historical candidate set, independent
//! of the read cut, and the ordinary cursor resolves the pinned MVCC winner.
//! This bounds setup and frontier space even for wide ranges with LIMIT 1.
//! It is not a byte-memory guarantee for the pinned database generation.

use super::{IndexMap, index_successor, numeric};
use crate::gql_exec::source::{
    EqualityCandidates, History, PropertyEqualityIndex, PropertyRange, SourceEvent,
    VertexHistoryScanCursor,
};
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::scan_stream::SourceStep;
use fgdb_gql::stream::VertexScanEvent;
use fgdb_strata::vertex::VertexPatchRows;
use fgdb_types::{CanonicalScalar, VId};

pub(super) const MAX_RANGE_HEADS: usize = 64;

pub(super) struct Head {
    pub(super) vid: VId,
    pub(super) root: IndexMap<VId, ()>,
}

enum Frontier {
    Pending,
    Buckets(Vec<Head>),
    Histories {
        scan: VertexHistoryScanCursor,
        // Membership checking can refuse after the history spine has advanced.
        // Retain that identity until the complete source step is admitted.
        pending: Option<VId>,
    },
}

pub(super) struct RangeCandidates {
    bounds: numeric::NumericBounds,
    frontier: Frontier,
}

impl RangeCandidates {
    pub(super) fn new(bounds: numeric::NumericBounds) -> Self {
        Self {
            bounds,
            frontier: Frontier::Pending,
        }
    }

    pub(super) fn key(&self) -> PropertyKeyId {
        self.bounds.key()
    }

    pub(super) fn accepts(&self, value: &CanonicalScalar) -> bool {
        self.bounds.accepts(value)
    }

    pub(super) fn history(&self, vid: VId) -> Option<&History> {
        match &self.frontier {
            Frontier::Histories { scan, .. } => scan.history(vid),
            Frontier::Pending | Frontier::Buckets(_) => None,
        }
    }

    pub(super) fn next<C>(
        &mut self,
        index: &PropertyEqualityIndex,
        patches: &[VertexPatchRows],
        after: Option<VId>,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<SourceStep<VId>, C> {
        control(VertexScanEvent::Work)?;
        if matches!(self.frontier, Frontier::Pending) {
            // Count before allocating: a wide unique-value range must not
            // collect its entire frontier merely to deliver its first row.
            let mut count = 0;
            let narrow = visit_ranges(
                &index.candidates,
                self.bounds.ranges(),
                control,
                &mut |root, _| {
                    count += usize::from(root.0.is_some());
                    Ok(count <= MAX_RANGE_HEADS)
                },
            )?;
            if narrow {
                let mut heads = Vec::new();
                visit_ranges(
                    &index.candidates,
                    self.bounds.ranges(),
                    control,
                    &mut |root, control| {
                        if let Some(vid) = index_successor(root, after, control)? {
                            push_head(
                                &mut heads,
                                Head {
                                    vid,
                                    root: root.clone(),
                                },
                                control,
                            )?;
                        }
                        Ok(true)
                    },
                )?;
                // Refused setup never publishes a partial range. A completed
                // empty lookup stays initialized, including contradictory bounds.
                self.frontier = Frontier::Buckets(heads);
            } else {
                self.frontier = Frontier::Histories {
                    scan: VertexHistoryScanCursor::default(),
                    pending: None,
                };
            }
        }
        match &mut self.frontier {
            Frontier::Buckets(heads) => {
                while let Some(head) = heads.first() {
                    if after.is_none_or(|after| head.vid > after) {
                        return Ok(SourceStep::Item(head.vid));
                    }
                    let next = index_successor(&head.root, after, control)?;
                    advance_head(heads, next, control)?;
                }
                Ok(SourceStep::Exhausted)
            }
            Frontier::Histories { scan, pending } => history_step(
                scan,
                pending,
                &index.histories,
                patches,
                &self.bounds,
                control,
            ),
            Frontier::Pending => unreachable!("frontier selected above"),
        }
    }
}

fn visit_ranges<C, F, Visit>(
    index: &EqualityCandidates,
    ranges: &[PropertyRange; 2],
    control: &mut F,
    visit: &mut Visit,
) -> Result<bool, C>
where
    F: FnMut(VertexScanEvent) -> Result<(), C>,
    Visit: FnMut(&IndexMap<VId, ()>, &mut F) -> Result<bool, C>,
{
    for range in ranges {
        if !range.empty && !visit_range(index, range, control, visit)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Traverse only the intersecting AVL subtrees, and stop immediately when the
/// visitor declines another bucket. The existing u32 tree cardinality bounds
/// recursion depth below 47; every visited node is governed. No iterator stack
/// or candidate-ID collection is allocated.
pub(super) fn visit_range<C, F, Visit>(
    index: &EqualityCandidates,
    range: &PropertyRange,
    control: &mut F,
    visit: &mut Visit,
) -> Result<bool, C>
where
    F: FnMut(VertexScanEvent) -> Result<(), C>,
    Visit: FnMut(&IndexMap<VId, ()>, &mut F) -> Result<bool, C>,
{
    let Some(node) = index.0.as_deref() else {
        return Ok(true);
    };
    control(VertexScanEvent::Work)?;
    let lower = node
        .key
        .0
        .cmp(&range.key)
        .then_with(|| node.key.1.as_ref().cmp(range.lower.as_slice()));
    if lower.is_lt() || (lower.is_eq() && !range.lower_inclusive) {
        return visit_range(&node.right, range, control, visit);
    }
    let upper = node
        .key
        .0
        .cmp(&range.key)
        .then_with(|| node.key.1.as_ref().cmp(range.upper.as_slice()));
    if upper.is_gt() || (upper.is_eq() && !range.upper_inclusive) {
        return visit_range(&node.left, range, control, visit);
    }
    Ok(visit_range(&node.left, range, control, visit)?
        && visit(&node.value, control)?
        && visit_range(&node.right, range, control, visit)?)
}

/// One identity per cooperative step, including non-candidates. Membership is
/// ANY historical statement, exactly as PropertyEqualityIndex::build/apply_added
/// populate the scalar buckets. It must not choose the winner or apply as_of:
/// future, retired and restated candidates still have one native admission.
fn history_step<C>(
    scan: &mut VertexHistoryScanCursor,
    pending: &mut Option<VId>,
    histories: &IndexMap<VId, History>,
    patches: &[VertexPatchRows],
    bounds: &numeric::NumericBounds,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<SourceStep<VId>, C> {
    if pending.is_none() {
        *pending = scan.next(histories, &mut |event| {
            control(match event {
                SourceEvent::Work | SourceEvent::SnapshotRecord => VertexScanEvent::Work,
                SourceEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
            })
        })?;
    }
    let Some(vid) = *pending else {
        return Ok(SourceStep::Exhausted);
    };
    let history = scan
        .history(vid)
        .expect("pending identity retains its history");
    let matches = history_matches(history, patches, bounds, control)?;
    // No fallible operation follows this publication. A refused predicate
    // leaves pending set, so a retry cannot skip the unadmitted identity.
    *pending = None;
    Ok(if matches {
        SourceStep::Item(vid)
    } else {
        SourceStep::Skipped(vid)
    })
}

fn history_matches<C>(
    history: &History,
    patches: &[VertexPatchRows],
    bounds: &numeric::NumericBounds,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<bool, C> {
    let Some(node) = history.0.as_deref() else {
        return Ok(false);
    };
    control(VertexScanEvent::Work)?;
    // Newer statements commonly answer broad ranges immediately. Older
    // statements remain candidates even when the visible value left the range.
    if history_matches(&node.right, patches, bounds, control)? {
        return Ok(true);
    }
    let (_, patch, row) = node.key;
    let properties = &patches[patch][row].props;
    let mut low = 0;
    let mut high = properties.len();
    while low < high {
        control(VertexScanEvent::Work)?;
        let mid = low + (high - low) / 2;
        match properties[mid].0.cmp(&bounds.key()) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => {
                control(VertexScanEvent::Work)?;
                if properties[mid]
                    .1
                    .as_scalar()
                    .is_some_and(|value| bounds.accepts(value))
                {
                    return Ok(true);
                }
                break;
            }
        }
    }
    history_matches(&node.left, patches, bounds, control)
}

/// Min-heap insertion with all comparisons and growth admitted before mutation.
pub(super) fn push_head<C>(
    heads: &mut Vec<Head>,
    head: Head,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<(), C> {
    let mut hole = heads.len();
    while hole > 0 {
        let parent = (hole - 1) / 2;
        control(VertexScanEvent::Work)?;
        if heads[parent].vid <= head.vid {
            break;
        }
        hole = parent;
    }
    control(VertexScanEvent::ScratchEntry)?;
    heads.push(head);
    let mut position = heads.len() - 1;
    while position > hole {
        let parent = (position - 1) / 2;
        heads.swap(position, parent);
        position = parent;
    }
    Ok(())
}

/// Replace the minimum with its next identity, or remove an exhausted bucket.
/// Determine the complete repair path before mutating so a refused comparison
/// cannot leave an invalid heap behind. The fixed path has no graph-sized growth.
pub(super) fn advance_head<C>(
    heads: &mut Vec<Head>,
    next: Option<VId>,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<(), C> {
    let len = heads.len() - usize::from(next.is_none());
    if len == 0 {
        heads.pop();
        return Ok(());
    }
    let replacement = next.unwrap_or_else(|| heads.last().expect("nonempty frontier").vid);
    let mut path = [0; usize::BITS as usize];
    let mut depth = 0;
    let mut parent = 0_usize;
    while parent < len / 2 {
        let left = parent * 2 + 1;
        let right = left + 1;
        control(VertexScanEvent::Work)?;
        let child = if right < len && heads[right].vid < heads[left].vid {
            right
        } else {
            left
        };
        control(VertexScanEvent::Work)?;
        if replacement <= heads[child].vid {
            break;
        }
        path[depth] = child;
        depth += 1;
        parent = child;
    }
    if let Some(vid) = next {
        heads[0].vid = vid;
    } else {
        heads.swap_remove(0);
    }
    parent = 0;
    for &child in &path[..depth] {
        heads.swap(parent, child);
        parent = child;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
