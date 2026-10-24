//! Preserve native parameter domains across the frozen query/write boundary.
//! All structural and transcript admission precedes copying or encoding a cell.

use super::*;
use crate::{GqlListParameter, GqlMapParameter};

pub(super) fn accepts_column(expected: GqlParameterType, source: GraphSetColumnType) -> bool {
    use GraphSetColumnType::{Any, List, Scalar, Stored};
    match expected {
        // Scalar includes canonical null. Non-null scalar rows still refuse
        // below; the declared schema cannot prove that a Scalar is non-null.
        GqlParameterType::List => matches!(source, List | Scalar | Stored | Any),
        GqlParameterType::Map => matches!(source, Scalar | Stored | Any),
        _ => matches!(source, Scalar | Stored | Any),
    }
}

// Mirror the existing parameter transcript's framing, not its encoding. The
// native constructors/map remain the sole encoders and admission authorities.
// Tests compare this forecast with their actual complete transcript lengths.
fn entry_bytes(
    value: &GraphValue,
    expected: GqlParameterType,
    name_bytes: usize,
    value_bytes: usize,
) -> Option<usize> {
    let payload = match (expected, value) {
        (GqlParameterType::Int64, GraphValue::Scalar(CanonicalScalar::Int(_))) => 8,
        (GqlParameterType::UInt64, GraphValue::Scalar(CanonicalScalar::Int(n))) if *n >= 0 => 8,
        (_, GraphValue::Scalar(value)) => {
            8_usize.checked_add(value.canonical_encoded_len().ok()?)?
        }
        _ => 8_usize.checked_add(value_bytes)?,
    };
    8_usize
        .checked_add(name_bytes)?
        .checked_add(1)?
        .checked_add(payload)
}

#[allow(clippy::too_many_arguments)] // preserve row/column coordinates and whole-prefix admission
pub(super) fn bind_cell<C>(
    cell: &GraphValue,
    expected: GqlParameterType,
    name: &str,
    row: usize,
    column: usize,
    retained_bytes: u128,
    control: &mut impl FnMut(u64) -> Result<(), GraphQueryWriteBindError<C>>,
) -> Result<GqlParameterValue, GraphQueryWriteBindError<C>> {
    let parameter_error = |source| GraphQueryWriteBindError::Parameter {
        row,
        column,
        source,
    };
    match (expected, cell) {
        (GqlParameterType::List, GraphValue::List(_))
        | (GqlParameterType::Map, GraphValue::Map { .. }) => {}
        (_, GraphValue::Scalar(CanonicalScalar::Null)) => {}
        (GqlParameterType::List | GqlParameterType::Map, _) => {
            return Err(GraphQueryWriteBindError::CollectionValue {
                row,
                column,
                expected,
            });
        }
        (_, GraphValue::Scalar(_)) => {}
        _ => return Err(GraphQueryWriteBindError::NonScalar { row, column }),
    }
    // GraphValue's public enum can represent invalid maps or excessive nesting.
    // Validate before its length walker, recursive constructors or any clone.
    if !cell.validate_bounds() {
        return Err(GraphQueryWriteBindError::InvalidValue { row, column });
    }
    let bytes = cell
        .canonical_byte_len()
        .map_err(|_| GraphQueryWriteBindError::InvalidValue { row, column })?;
    match cell {
        GraphValue::Scalar(value)
            if value.canonical_encoded_len().map_or(true, |length| {
                length > crate::algebra::MAX_SCALAR_PREDICATE_BYTES
            }) =>
        {
            return Err(parameter_error(GqlParameterError::ScalarLiteral));
        }
        GraphValue::List(_) if bytes > crate::parameters::MAX_GQL_PARAMETER_TRANSCRIPT_BYTES => {
            return Err(parameter_error(GqlParameterError::ListLiteral));
        }
        GraphValue::Map { .. } if bytes > crate::algebra::MAX_SCALAR_PREDICATE_BYTES => {
            return Err(parameter_error(GqlParameterError::MapLiteral));
        }
        _ => {}
    }
    let added = entry_bytes(cell, expected, name.len(), bytes)
        .ok_or(GraphQueryWriteBindError::InvalidValue { row, column })?;
    let limit = crate::parameters::MAX_GQL_PARAMETER_TRANSCRIPT_BYTES;
    let observed = retained_bytes.saturating_add(added as u128);
    if observed > limit as u128 {
        return Err(GraphQueryWriteBindError::ExpandedParametersTooLarge { limit, observed });
    }
    // Keep the established scalar accounting. A collection additionally charges
    // every event in the native iterative copier, including traversal storage.
    // These are conservative logical work/scratch units, not a heap-byte bound.
    control(1 + (bytes as u64).div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES as u64))?;
    if let GraphValue::Scalar(value) = cell {
        return scalar_parameter(value, expected).map_err(parameter_error);
    }
    let owned = cell.copy_with_control(&mut |_| control(1))?;
    match owned {
        GraphValue::List(values) => GqlListParameter::new(values.into_vec())
            .map(GqlParameterValue::List)
            .map_err(parameter_error),
        GraphValue::Map { keys, values } => {
            // The native map constructor takes owned pairs. Moving cells does
            // not clone payloads; reserve the pair workspace before collecting.
            control(keys.len() as u64)?;
            let entries = keys.into_vec().into_iter().zip(values.into_vec()).collect();
            GqlMapParameter::new(entries)
                .map(GqlParameterValue::Map)
                .map_err(parameter_error)
        }
        _ => Err(GraphQueryWriteBindError::InvalidValue { row, column }),
    }
}

#[cfg(test)]
mod tests;
