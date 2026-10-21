//! A single-root pattern leaf restricted to the root identities its leading
//! bag carries (fgdb-2acff). `CALL ... YIELD node RETURN node.p` and a MATCH
//! vertex reusing a leading vertex otherwise admit every vertex, then keep
//! the few the identity equality joins. The ordinary ordered-probe join still
//! runs on every returned row; only the right leaf's admission narrows.

use super::*;
use crate::set_ops::filter::FilteredCrossInputs;

/// The right leaf's rows of one completed WHERE-over-product, read from the
/// host's seeded source instead of a whole-graph pattern admission. `None`
/// keeps the ordinary leaf: a host without the capability (checked before
/// any charge), an ordered or paged leaf, a pattern reading anything but its
/// one root, no necessary identity equality on that root, or a host that
/// declines the shape. The left bag is complete, so the seeds are exactly the
/// root identities any pair can join on.
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
    let Some(root) = pattern.seeded_root_column() else {
        return Ok(None);
    };
    let width = leading.types.len();
    let slots = columns(projection, &mut |event| meter.event(event))?;
    let keys = required_keys(width, code, slots.as_deref(), &mut |event| {
        meter.event(event)
    })?;
    let Some(&(key, _)) = keys.iter().find(|&&(_, column)| column == root) else {
        return Ok(None);
    };
    // A NULL or non-vertex cell never equals a vertex identity: it seeds
    // nothing, and the join rejects its pairs exactly as before.
    let mut seeds = Vec::new();
    for row in left {
        meter.event(GlaExecutionEvent::Work)?;
        if let GraphValue::Vertex(vertex) = &row.values()[key] {
            meter.event(GlaExecutionEvent::ScratchEntry)?;
            seeds.push(*vertex);
        }
    }
    merge::sort(
        &mut seeds,
        &mut |event| meter.event(event),
        &mut |a, b, _| Ok(a.cmp(b)),
    )?;
    seeds.dedup();
    let at = *operand;
    let Some(execution) = source.seeded_pattern(pattern, &seeds, meter.remaining()) else {
        return Ok(None);
    };
    *operand += 1;
    let execution = execution.map_err(|error| meter.source_error(error))?;
    Ok(Some(admit_source_rows(right, execution, at, meter)?))
}
