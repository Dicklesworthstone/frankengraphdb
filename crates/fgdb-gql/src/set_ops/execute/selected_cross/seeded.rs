//! A single-root pattern leaf restricted to the roots its leading bag can
//! join: by identity (fgdb-2acff), as in `CALL ... YIELD node RETURN node.p`
//! or a MATCH vertex reusing a leading vertex, or by one root property
//! (fgdb-2npty), as in `UNWIND $rows AS row MATCH (a {id: row.id})`. Either
//! otherwise admits every vertex, then keeps the few the equality joins. The
//! ordinary ordered-probe join still runs on every returned row; only the
//! right leaf's admission narrows.

use super::*;
use crate::set_ops::filter::FilteredCrossInputs;

/// The right leaf's rows of one completed WHERE-over-product, read from the
/// host's seeded source instead of a whole-graph pattern admission. `None`
/// keeps the ordinary leaf: a host without the capability (checked before
/// any charge), an ordered or paged leaf, a pattern reading anything but its
/// one root, no necessary equality on that root's identity or one of its
/// projected properties, or a host that declines the shape or seed kind. The
/// left bag is complete, so the seeds are exactly the root identities or
/// property values any pair can join on.
pub(in crate::set_ops::execute) fn collect<E, C, S, Checkpoint>(
    left: &[GraphValueRow],
    (leading, right, code, projection): FilteredCrossInputs<'_>,
    source: &mut S,
    meter: &mut Meter<Checkpoint>,
    operand: &mut usize,
) -> SetResult<Option<Vec<GraphValueRow>>, E, C>
where
    S: GraphSetSource<E, C> + ?Sized,
    Checkpoint: FnMut() -> Result<(), C>,
{
    if !source.supports_seeded_patterns()
        || !right.order.is_empty()
        || right.offset != 0
        || right.count.is_some()
    {
        return Ok(None);
    }
    let SetNode::Pattern(pattern) = &right.node else {
        return Ok(None);
    };
    let width = leading.types.len();
    let root = pattern.seeded_root_column();
    if root.is_none() {
        // A property seed is decided before any charge, so a declined one
        // leaves the ordinary join's work exactly as it was.
        if pattern.seeded_columns().is_none() {
            return Ok(None);
        }
        let mut free = |_| Ok::<(), core::convert::Infallible>(());
        let slots = columns(projection, &mut free).unwrap_or_else(|never| match never {});
        let keys = required_keys(width, code, slots.as_deref(), &mut free)
            .unwrap_or_else(|never| match never {});
        if !keys
            .iter()
            .any(|&(_, column)| pattern.seeded_root_property(column).is_some())
        {
            return Ok(None);
        }
    }
    let slots = columns(projection, &mut |event| meter.event(event))?;
    let keys = required_keys(width, code, slots.as_deref(), &mut |event| {
        meter.event(event)
    })?;
    let mut identities = Vec::new();
    let mut values = Vec::new();
    let seeds = if let Some(&(key, _)) = keys.iter().find(|&&(_, column)| Some(column) == root) {
        // A NULL or non-vertex cell never equals a vertex identity: it seeds
        // nothing, and the join rejects its pairs exactly as before.
        for row in left {
            meter.event(GlaExecutionEvent::Work)?;
            if let GraphValue::Vertex(vertex) = &row.values()[key] {
                meter.event(GlaExecutionEvent::ScratchEntry)?;
                identities.push(*vertex);
            }
        }
        merge::sort(
            &mut identities,
            &mut |event| meter.event(event),
            &mut |a, b, _| Ok(a.cmp(b)),
        )?;
        identities.dedup();
        GraphSeeds::Identities(&identities)
    } else if let Some((key, property)) = keys
        .iter()
        .find_map(|&(key, column)| Some((key, pattern.seeded_root_property(column)?)))
    {
        // The join equates a leading cell with one root property (fgdb-2npty).
        // NULL never satisfies `=`, and an element never equals a stored
        // value, so only non-NULL scalar cells seed. A list or map cell may
        // equal a stored list, which no scalar posting names: keep the
        // ordinary leaf.
        for row in left {
            meter.event(GlaExecutionEvent::Work)?;
            match &row.values()[key] {
                GraphValue::Scalar(fgdb_types::CanonicalScalar::Null) => {}
                GraphValue::Scalar(value) => {
                    meter.event(GlaExecutionEvent::ScratchEntry)?;
                    values.push(value.clone());
                }
                GraphValue::List(_) | GraphValue::Map { .. } => return Ok(None),
                _ => {}
            }
        }
        merge::sort(
            &mut values,
            &mut |event| meter.event(event),
            &mut |a, b, _| Ok(a.cmp(b)),
        )?;
        values.dedup();
        GraphSeeds::Property {
            key: property,
            values: &values,
        }
    } else {
        return Ok(None);
    };
    let at = *operand;
    let Some(execution) = source.seeded_pattern(pattern, seeds, meter.remaining()) else {
        return Ok(None);
    };
    *operand += 1;
    let execution = execution.map_err(|error| meter.source_error(error))?;
    Ok(Some(admit_source_rows(right, execution, at, meter)?))
}
