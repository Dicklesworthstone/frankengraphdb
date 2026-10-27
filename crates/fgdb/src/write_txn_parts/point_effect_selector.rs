// Borrow target ranges from the admitted canonical NET rows without looking
// at payloads, building another overlay, or retaining an invalidatable index.

use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId};
use fgdb_types::{EId, VId};

/// A prefix of the existing canonical byte order, not a replacement row key.
/// The first six durable tags are 01..06 in this order; their identities and
/// field IDs use unsigned big-endian encoding. Property's element discriminator
/// orders vertices before edges. Later families all follow these prefixes.
/// Equal prefixes may have different remaining bytes, so searches retain the
/// whole range in its original order. The integration oracle checks this
/// monotonicity against actual canonical_bytes, including all later families.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EffectKey {
    CreateVertex(VId),
    CreateEdge(EId),
    DeleteVertex(VId),
    DeleteEdge(EId),
    Label(VId, LabelId),
    Property(ElementId, PropertyKeyId),
    Other,
}

impl EffectKey {
    pub(crate) fn of(row: &DeltaRow) -> Self {
        match row {
            DeltaRow::CreateVertex { vid, .. } => Self::CreateVertex(*vid),
            DeltaRow::CreateEdge { eid, .. } => Self::CreateEdge(*eid),
            DeltaRow::DeleteVertex { vid, .. } => Self::DeleteVertex(*vid),
            DeltaRow::DeleteEdge { eid, .. } => Self::DeleteEdge(*eid),
            DeltaRow::LabelMembership { vid, label, .. } => Self::Label(*vid, *label),
            DeltaRow::Property { elem, property, .. } => Self::Property(*elem, *property),
            // Explicit arms make adding a family require reviewing the prefix
            // order, even when its payload is irrelevant to point projections.
            DeltaRow::ValidTime { .. }
            | DeltaRow::Counter { .. }
            | DeltaRow::Escrow { .. }
            | DeltaRow::Sketch { .. }
            | DeltaRow::Schema { .. }
            | DeltaRow::Constraint { .. } => Self::Other,
        }
    }
}

/// Every comparison polls the caller, including negative lookups. No payload
/// is inspected or copied and no allocation depends on the staged row count.
pub(crate) fn select<'a, E>(
    rows: &'a [DeltaRow],
    requested: EffectKey,
    poll: &mut impl FnMut() -> Result<(), E>,
) -> Result<&'a [DeltaRow], E> {
    select_range(rows, requested, requested, poll)
}

/// Borrow every canonical row whose prefix lies in the inclusive interval.
/// All suffixes of both endpoints remain in their original byte order. The
/// two bounds need logarithmic comparisons regardless of the selected range's
/// length; callers govern traversal of the returned rows. Empty or reversed
/// ranges still poll once, and no payload is inspected, copied or indexed.
pub(crate) fn select_range<'a, E>(
    rows: &'a [DeltaRow],
    first: EffectKey,
    last: EffectKey,
    poll: &mut impl FnMut() -> Result<(), E>,
) -> Result<&'a [DeltaRow], E> {
    poll()?;
    if first > last {
        return Ok(&rows[..0]);
    }
    let (mut low, mut high) = (0, rows.len());
    while low < high {
        poll()?;
        let middle = low + (high - low) / 2;
        if EffectKey::of(&rows[middle]) < first {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    let start = low;
    high = rows.len();
    while low < high {
        poll()?;
        let middle = low + (high - low) / 2;
        if EffectKey::of(&rows[middle]) <= last {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(&rows[start..low])
}
