//! Admit the complete storage copy before invoking the canonical converter.
//! The converter owns stored-value validation/encoding; this traversal only
//! rejects non-written domains and charges the allocation it is about to make.

use super::*;

pub(super) fn reserve<E, A, C>(
    value: &GraphValue,
    row: usize,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> ResultOf<(), E, A, C>,
) -> ResultOf<(), E, A, C> {
    fn visit<E, A, C>(
        value: &GraphValue,
        depth: usize,
        remaining: &mut usize,
        at: (usize, usize),
        control: &mut impl FnMut(GlaExecutionEvent) -> ResultOf<(), E, A, C>,
    ) -> ResultOf<(), E, A, C> {
        let invalid = || {
            GqlQueryError::Source(GraphInsertError::InputSchema {
                row: at.0,
                column: at.1,
            })
        };
        // Check before descending or walking an unbounded public value. The
        // traversal uses only the bounded call stack, never a payload clone.
        if depth > GraphValue::MAX_LIST_DEPTH || *remaining == 0 {
            return Err(invalid());
        }
        *remaining -= 1;
        match value {
            GraphValue::Scalar(value) => charge_scalar(value, control),
            GraphValue::List(values) => {
                if values.len() > *remaining {
                    return Err(invalid());
                }
                control(GlaExecutionEvent::ScratchEntry)?;
                for value in values {
                    visit(value, depth + 1, remaining, at, control)?;
                }
                Ok(())
            }
            // Stored maps still require the owner's separate language ruling.
            // Graph identities and identity sequences are not property values.
            GraphValue::Map { .. }
            | GraphValue::Vertex(_)
            | GraphValue::Edge(_)
            | GraphValue::Path(_)
            | GraphValue::Vertices(_)
            | GraphValue::Edges(_) => Err(invalid()),
        }
    }

    let mut remaining = GraphValue::MAX_LIST_NODES;
    visit(value, 0, &mut remaining, (row, column), control)
}

#[cfg(test)]
mod tests;
