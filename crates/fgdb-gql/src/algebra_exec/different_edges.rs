//! Clause-local relationship identity checks over the existing path segments.
//! Membership work and copied identities are charged before execution/growth.

use super::{EId, GlaExecutionEvent, GlaOperator, GraphPath};
use crate::algebra::BindingSlot;

#[cfg(test)]
mod tests;

/// The positive compiler emits a constraint immediately after its expansion.
/// Scope lowering may insert correlation identities between the two. Stop at
/// any other instruction, particularly another producer or clause boundary.
pub(super) fn for_expansion(
    operators: &[GlaOperator],
    ordinal: usize,
    appended: usize,
) -> Option<&[BindingSlot]> {
    for operator in operators.iter().skip(ordinal + 1) {
        match operator {
            GlaOperator::VertexIdentity { .. } => {}
            GlaOperator::DifferentEdges { segments }
                if segments
                    .last()
                    .is_some_and(|slot| slot.ordinal() as usize == appended) =>
            {
                return Some(segments);
            }
            _ => return None,
        }
    }
    None
}

/// Retain prior segments' EIds for the lifetime of this expansion's cursor.
/// Its current segment is not bound yet. A missing prior segment fails closed.
pub(super) fn forbidden<E>(
    segments: &[BindingSlot],
    paths: &[Option<GraphPath>],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<Vec<EId>>, E> {
    let Some((_, previous)) = segments.split_last() else {
        return Ok(None);
    };
    let mut forbidden = Vec::new();
    for slot in previous {
        control(GlaExecutionEvent::Work)?;
        let Some(path) = paths.get(slot.ordinal() as usize).and_then(Option::as_ref) else {
            return Ok(None);
        };
        for edge in path.edges() {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            forbidden.push(edge);
        }
    }
    Ok(Some(forbidden))
}

/// Check the full constraint, including repeated EIds inside each quantified
/// segment. Orientation and endpoint equality never substitute for identity.
pub(super) fn accepts<E>(
    segments: &[BindingSlot],
    paths: &[Option<GraphPath>],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    for (at, slot) in segments.iter().enumerate() {
        control(GlaExecutionEvent::Work)?;
        let Some(path) = paths.get(slot.ordinal() as usize).and_then(Option::as_ref) else {
            return Ok(false);
        };
        for (step, &(edge, _)) in path.steps().iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            for &(previous, _) in &path.steps()[..step] {
                control(GlaExecutionEvent::Work)?;
                if previous == edge {
                    return Ok(false);
                }
            }
            for previous in &segments[..at] {
                control(GlaExecutionEvent::Work)?;
                let Some(previous) = paths
                    .get(previous.ordinal() as usize)
                    .and_then(Option::as_ref)
                else {
                    return Ok(false);
                };
                for previous in previous.edges() {
                    control(GlaExecutionEvent::Work)?;
                    if previous == edge {
                        return Ok(false);
                    }
                }
            }
        }
    }
    Ok(true)
}
