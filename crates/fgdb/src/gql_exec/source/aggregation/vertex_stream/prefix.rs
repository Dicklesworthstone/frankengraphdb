//! Controlled UCS_BASIC prefix seeks over immutable value buckets.
//!
//! The numeric range source supplies the same bounded, governed heap. Prefix
//! type checks and the 64-head capacity are decided before output. Histories
//! remain the complete fallback; scoped admission retains the selected prefix.

use super::{EqualityKey, index_successor, range};
use crate::gql_exec::source::{PropertyEqualityIndex, PropertyRange, prefix_range};
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::scan_stream::SourceStep;
use fgdb_gql::stream::VertexScanEvent;
use fgdb_types::{CanonicalScalar, VId};
use std::sync::Arc;

enum Frontier {
    Pending,
    Buckets(Vec<range::Head>),
    Histories,
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

    pub(super) fn next<C>(
        &mut self,
        index: &PropertyEqualityIndex,
        after: Option<VId>,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<SourceStep<VId>, C> {
        control(VertexScanEvent::Work)?;
        if matches!(self.frontier, Frontier::Pending) {
            let mut count = 0;
            let narrow = types_are_safe(index, &self.range, control)?
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
            } else {
                self.frontier = Frontier::Histories;
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
            Frontier::Histories => index_successor(&index.histories, after, control)?,
            Frontier::Pending => unreachable!("frontier selected above"),
        };
        Ok(next.map_or(SourceStep::Exhausted, SourceStep::Item))
    }
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
