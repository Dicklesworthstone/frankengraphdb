//! Controlled UCS_BASIC prefix seeks over immutable value buckets.
//!
//! The numeric range source supplies the same bounded, governed heap. Prefix
//! type checks and the 64-head capacity are decided before output. Wide binary
//! prefixes test historical membership one identity at a time without admitting
//! unrelated histories. Alternate collations retain the complete fallback;
//! scoped admission rechecks the selected prefix after visibility masking.

use super::{EqualityKey, index_successor, range};
use crate::gql_exec::source::{
    History, PropertyEqualityIndex, PropertyRange, SourceEvent, VertexHistoryScanCursor,
    prefix_range,
};
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::scan_stream::SourceStep;
use fgdb_gql::stream::VertexScanEvent;
use fgdb_strata::vertex::VertexPatchRows;
use fgdb_types::{CanonicalScalar, VId};
use std::sync::Arc;

enum Frontier {
    Pending,
    Buckets(Vec<range::Head>),
    HistoricalMatches {
        scan: VertexHistoryScanCursor,
        // A controlled membership check can refuse after advancing the spine.
        // Keep that identity until the complete source step is admitted.
        pending: Option<VId>,
    },
    Histories(VertexHistoryScanCursor),
}

pub(super) struct PrefixCandidates {
    range: PropertyRange,
    prefix: Arc<str>,
    frontier: Frontier,
}

impl PrefixCandidates {
    pub(super) fn new(key: PropertyKeyId, prefix: &str) -> Option<Self> {
        Some(Self {
            range: prefix_range(key, prefix)?,
            prefix: Arc::from(prefix),
            frontier: Frontier::Pending,
        })
    }

    pub(super) fn key(&self) -> PropertyKeyId {
        self.range.key
    }

    pub(super) fn accepts(&self, value: &CanonicalScalar) -> bool {
        match value {
            CanonicalScalar::Text(value) => value.as_str().starts_with(self.prefix.as_ref()),
            // The admitted Boolean program contains only direct STARTS WITH
            // scalar conjuncts: nontext/NULL are UNKNOWN and no sibling can
            // raise an arithmetic exception. Label admission precedes this
            // private recheck in scoped sources.
            _ => false,
        }
    }

    pub(super) fn history(&self, vid: VId) -> Option<&History> {
        match &self.frontier {
            Frontier::Histories(scan) | Frontier::HistoricalMatches { scan, .. } => {
                scan.history(vid)
            }
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
            let mut count = 0;
            let binary = types_are_safe(index, &self.range, control)?;
            let narrow = binary
                && range::visit_range(&index.candidates, &self.range, control, &mut |root, _| {
                    count += usize::from(root.0.is_some());
                    Ok(count <= range::MAX_RANGE_HEADS)
                })?;
            if narrow {
                let mut heads = Vec::new();
                range::visit_range(
                    &index.candidates,
                    &self.range,
                    control,
                    &mut |root, control| {
                        if let Some(vid) = index_successor(root, after, control)? {
                            range::push_head(
                                &mut heads,
                                range::Head {
                                    vid,
                                    root: root.clone(),
                                },
                                control,
                            )?;
                        }
                        Ok(true)
                    },
                )?;
                // Publish only a completely admitted frontier, including an
                // empty range. Refused setup can be retried without lost roots.
                self.frontier = Frontier::Buckets(heads);
            } else if binary {
                self.frontier = Frontier::HistoricalMatches {
                    scan: VertexHistoryScanCursor::default(),
                    pending: None,
                };
            } else {
                self.frontier = Frontier::Histories(VertexHistoryScanCursor::default());
            }
        }
        let next = match &mut self.frontier {
            Frontier::Buckets(heads) => {
                while let Some(head) = heads.first() {
                    if after.is_none_or(|after| head.vid > after) {
                        return Ok(SourceStep::Item(head.vid));
                    }
                    let next = index_successor(&head.root, after, control)?;
                    range::advance_head(heads, next, control)?;
                }
                None
            }
            Frontier::Histories(scan) => scan.next(&index.histories, &mut |event| {
                control(match event {
                    SourceEvent::Work | SourceEvent::SnapshotRecord => VertexScanEvent::Work,
                    SourceEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
                })
            })?,
            Frontier::HistoricalMatches { scan, pending } => {
                return history_step(
                    scan,
                    pending,
                    index,
                    patches,
                    self.range.key,
                    &self.prefix,
                    control,
                );
            }
            Frontier::Pending => unreachable!("frontier selected above"),
        };
        Ok(next.map_or(SourceStep::Exhausted, SourceStep::Item))
    }
}

/// Keep the same candidate set as the property postings, including retired and
/// future versions. The native cursor alone resolves the winner at its pinned
/// cut. A noncandidate still yields a cooperative source step, but consumes no
/// SnapshotRecord allowance and never materializes a current vertex row.
fn history_step<C>(
    scan: &mut VertexHistoryScanCursor,
    pending: &mut Option<VId>,
    index: &PropertyEqualityIndex,
    patches: &[VertexPatchRows],
    key: PropertyKeyId,
    prefix: &str,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<SourceStep<VId>, C> {
    if pending.is_none() {
        *pending = scan.next(&index.histories, &mut |event| {
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
    let matches = history_matches(history, patches, key, prefix, control)?;
    // Publish only after the complete check. A refusal leaves pending set so
    // retry cannot skip the identity that the spine already enumerated.
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
    key: PropertyKeyId,
    prefix: &str,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<bool, C> {
    let Some(node) = history.0.as_deref() else {
        return Ok(false);
    };
    control(VertexScanEvent::Work)?;
    if history_matches(&node.right, patches, key, prefix, control)? {
        return Ok(true);
    }
    let (_, patch, row) = node.key;
    let properties = &patches[patch][row].props;
    let mut low = 0;
    let mut high = properties.len();
    while low < high {
        control(VertexScanEvent::Work)?;
        let mid = low + (high - low) / 2;
        match properties[mid].0.cmp(&key) {
            core::cmp::Ordering::Less => low = mid + 1,
            core::cmp::Ordering::Greater => high = mid,
            core::cmp::Ordering::Equal => {
                control(VertexScanEvent::Work)?;
                if matches!(properties[mid].1.as_scalar(), Some(CanonicalScalar::Text(value))
                    if value.as_str().starts_with(prefix))
                {
                    return Ok(true);
                }
                break;
            }
        }
    }
    history_matches(&node.left, patches, key, prefix, control)
}

/// Prefix byte order is valid only in UCS_BASIC. Nontext values are UNKNOWN
/// in the admitted Boolean program, including values from retired statements.
/// Only a historical nonbinary text binding requires the ordinary scan.
fn types_are_safe<C>(
    index: &PropertyEqualityIndex,
    range: &PropertyRange,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<bool, C> {
    // Every alternate binding follows UCS_BASIC inside the text tag. A
    // successor in a later scalar domain or property is not a text hazard.
    // One governed descent proves this without a history scan or allocation.
    let after_binary = [range.lower[0], range.lower[1] + 1];
    Ok(!matches!(
        key_at_or_after(index, range.key, &after_binary, true, control)?,
        Some((key, encoded))
            if *key == range.key && encoded.first() == range.lower.first()
    ))
}

fn key_at_or_after<'a, C>(
    index: &'a PropertyEqualityIndex,
    key: PropertyKeyId,
    encoded: &[u8],
    inclusive: bool,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<Option<&'a EqualityKey>, C> {
    let mut node = index.candidates.0.as_deref();
    let mut found = None;
    while let Some(current) = node {
        control(VertexScanEvent::Work)?;
        let order = current
            .key
            .0
            .cmp(&key)
            .then_with(|| current.key.1.as_ref().cmp(encoded));
        if order.is_gt() || (order.is_eq() && inclusive) {
            found = Some(&current.key);
            node = current.left.0.as_deref();
        } else {
            node = current.right.0.as_deref();
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests;
