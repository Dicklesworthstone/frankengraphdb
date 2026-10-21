//! Collection assignments use the ordinary proposal reducer and canonical
//! property converter. Reserve every copied cell before ownership changes.

use super::*;
use crate::algebra::GraphValue;
use fgdb_types::CanonicalPropertyValue;

pub(super) fn written<E, C>(
    value: &GraphValue,
    row: usize,
    action: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> ResultOf<(), E, C>,
) -> ResultOf<CanonicalPropertyValue, E, C> {
    fn reserve<E, C>(
        value: &GraphValue,
        depth: usize,
        remaining: &mut usize,
        at: (usize, usize),
        control: &mut impl FnMut(GlaExecutionEvent) -> ResultOf<(), E, C>,
    ) -> ResultOf<(), E, C> {
        let invalid = || {
            GqlQueryError::Source(GraphMutationError::NonScalarValue {
                row: at.0,
                action: at.1,
            })
        };
        if depth > GraphValue::MAX_LIST_DEPTH || *remaining == 0 {
            return Err(invalid());
        }
        *remaining -= 1;
        match value {
            GraphValue::Scalar(value) => reserve_copy(value, control),
            GraphValue::List(values) => {
                if values.len() > *remaining {
                    return Err(invalid());
                }
                control(GlaExecutionEvent::ScratchEntry)?;
                for value in values {
                    reserve(value, depth + 1, remaining, at, control)?;
                }
                Ok(())
            }
            // Storage can represent maps, but the written-property language
            // still excludes them, including maps nested inside a list.
            _ => Err(invalid()),
        }
    }

    let mut remaining = GraphValue::MAX_LIST_NODES;
    reserve(value, 0, &mut remaining, (row, action), control)?;
    value
        .to_written_property()
        .ok_or(GqlQueryError::Source(GraphMutationError::NonScalarValue {
            row,
            action,
        }))
}

/// Price exact canonical equality without allocating an encoded surrogate.
/// All values here already passed the bounded native property constructor.
pub(super) fn compare_payload<E>(
    value: &CanonicalPropertyValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    control(GlaExecutionEvent::Work)?;
    match value {
        CanonicalPropertyValue::Scalar(value) => charge_payload(value, control),
        CanonicalPropertyValue::List(values) => {
            for value in values.values() {
                compare_payload(value, control)?;
            }
            Ok(())
        }
        CanonicalPropertyValue::Map(values) => {
            for entry in values.entries() {
                for _ in 0..=entry
                    .key()
                    .as_str()
                    .len()
                    .div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
                {
                    control(GlaExecutionEvent::Work)?;
                }
                compare_payload(entry.value(), control)?;
            }
            Ok(())
        }
    }
}

pub(super) fn returned<E, C>(
    value: &CanonicalPropertyValue,
    row: usize,
    binding: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> ResultOf<(), E, C>,
) -> ResultOf<GraphValue, E, C> {
    fn reserve<E>(
        value: &CanonicalPropertyValue,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        match value {
            CanonicalPropertyValue::Scalar(value) => reserve_copy(value, control),
            CanonicalPropertyValue::List(values) => {
                control(GlaExecutionEvent::ScratchEntry)?;
                for value in values.values() {
                    reserve(value, control)?;
                }
                Ok(())
            }
            CanonicalPropertyValue::Map(values) => {
                control(GlaExecutionEvent::ScratchEntry)?;
                for entry in values.entries() {
                    for _ in 0..=entry
                        .key()
                        .as_str()
                        .len()
                        .div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
                    {
                        control(GlaExecutionEvent::ScratchEntry)?;
                    }
                    reserve(entry.value(), control)?;
                }
                Ok(())
            }
        }
    }
    reserve(value, control)?;
    GraphValue::from_property_value(value).ok_or(GqlQueryError::Source(
        GraphMutationError::InputSchema {
            row,
            column: binding,
        },
    ))
}

#[cfg(test)]
mod tests;
