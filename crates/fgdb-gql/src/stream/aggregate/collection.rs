//! Ordered, owned COLLECT state for admitted physical aggregate sources.
//!
//! The physical compiler proves visitation order. This leaf never sorts or
//! deduplicates; DISTINCT uses the existing canonical membership owner before
//! entering it. NULL is omitted, including a missing property. An empty input
//! finishes as an empty list, not NULL. Lists are governed result payloads and
//! are not constrained by the separate literal/parameter admission limits.

use super::*;

pub(super) fn push<E>(
    values: &mut Vec<GraphValue>,
    input: Input<'_>,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
) -> Result<(), E> {
    let input = input.normalized();
    control(VertexScanEvent::Work)?;
    if matches!(input, Input::Scalar(None | Some(CanonicalScalar::Null))) {
        return Ok(());
    }
    // Reserve the occurrence and its whole logical payload before cloning or
    // growing the list. A callback error/unwind cannot publish half an entry.
    // The DISTINCT owner separately charges its retained membership copy.
    control(VertexScanEvent::ScratchEntry)?;
    for _ in 0..input.payload_units() {
        control(VertexScanEvent::Work)?;
        control(VertexScanEvent::ScratchEntry)?;
    }
    let value = match input {
        Input::Vertex(vid) => GraphValue::Vertex(vid),
        Input::Scalar(Some(value)) => GraphValue::Scalar(value.clone()),
        Input::Value(value) => value.clone(),
        _ => unreachable!("COLLECT has a checked nonnull argument column"),
    };
    values.push(value);
    Ok(())
}

#[cfg(test)]
mod tests;
