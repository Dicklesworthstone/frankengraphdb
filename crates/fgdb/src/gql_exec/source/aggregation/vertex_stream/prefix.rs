//! Controlled UCS_BASIC prefix seeks over immutable value buckets.
//!
//! The numeric range source supplies the same bounded, governed heap. Prefix
//! type checks and the 64-head capacity are decided before output. Histories
//! remain the complete fallback; scoped admission retains the selected prefix.

use super::{index_successor, range};
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
            CanonicalScalar::Null => false,
            CanonicalScalar::Text(value) => value.as_str().starts_with(self.prefix.as_ref()),
            // Type fallback retains the ordinary Boolean evaluator's UNKNOWN
            // and candidate charging. It never interprets a nontext value.
            _ => true,
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

/// Prefix byte order is valid only in UCS_BASIC. Two controlled boundary
/// seeks exclude other nonnull scalar types and nonbinary text bindings.
fn types_are_safe<C>(
    index: &PropertyEqualityIndex,
    range: &PropertyRange,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<bool, C> {
    let null = CanonicalScalar::Null.encode().expect("null encoding");
    if let Some((key, encoded)) = key_at_or_after(index, range.key, &null, false, control)?
        && *key == range.key
        && encoded.as_ref() < &range.lower[..2]
    {
        return Ok(false);
    }
    let after_binary = [range.lower[0], range.lower[1] + 1];
    Ok(!matches!(
        key_at_or_after(index, range.key, &after_binary, true, control)?,
        Some((key, _)) if *key == range.key
    ))
}

fn key_at_or_after<'a, C>(
    index: &'a PropertyEqualityIndex,
    key: PropertyKeyId,
    encoded: &[u8],
    inclusive: bool,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
) -> Result<Option<&'a (PropertyKeyId, Arc<[u8]>)>, C> {
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
