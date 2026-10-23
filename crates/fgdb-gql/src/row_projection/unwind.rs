//! Checked list expansion over one evaluated row, using the ordinary sink.
//! The expression interpreter and complete input-count admission stay in the
//! parent projection engine. This module introduces no graph/source authority.

use super::*;
use crate::GraphIntegerErrorKind;
use crate::algebra::GraphValue;

impl RowProjectionSpec {
    /// Append each element of a native list expression to the original row.
    /// The alias and expression use ordinary relational UNWIND admission;
    /// inherited names (including duplicate/qualified names) stay unchanged.
    /// All expressions address the original input, never the appended alias.
    ///
    /// NULL and empty lists emit nothing. A scalar or other non-list value is
    /// an IncompatibleOperands expression failure at the appended column. List
    /// elements may be heterogeneous, NULL, lists or full-width graph identities;
    /// the appended type is Any. Element duplicates multiply the input's exact
    /// count. The expression executes once per changed tuple, including deletes,
    /// not once per occurrence. Hidden input counts remain validated and owned.
    ///
    /// Output is an ALL bag, not an element-order trace. Compose an ordinary
    /// DISTINCT projection downstream when needed. with_filter selects ORIGINAL
    /// inputs before evaluation/expansion; later filters must be separate stages.
    /// Work/scratch bound evaluated and copied payloads and element fanout, not
    /// allocator bytes. Final result limits count consolidated occurrences.
    pub fn unwind(
        input: Vec<GraphSetColumnType>,
        columns: Vec<String>,
        name: String,
        value: GraphSetValue,
    ) -> Result<Self, RowProjectionBuildError> {
        let column = input.len();
        if column >= MAX_PATTERN_VERTICES {
            return Err(GraphSetProjectionError::TooManyColumns {
                limit: MAX_PATTERN_VERTICES,
                observed: column.saturating_add(1),
            }
            .into());
        }
        GraphSetProjection::validate_binding_name(&name, column)?;
        if columns.contains(&name) {
            return Err(GraphSetProjectionError::DuplicateName { column }.into());
        }
        // Reuse the selection schema/name checks rather than imposing new
        // expression-name rules on inherited metadata. The temporary constant
        // predicate is removed; it does not run during an ordinary UNWIND tick.
        let mut spec = Self::selection(input, columns, &[GraphSetPredicateOp::Truth(Some(true))])?;
        // range() streams its members (fgdb-85aal): the cell holds only its
        // bounds, `[start, end]` or `[start, end, step]`, and append
        // generates the members, so the list bound never caps them.
        let (value, range) = match value {
            GraphSetValue::Range { start, end, step } => {
                let mut bounds = vec![*start, *end];
                bounds.extend(step.map(|step| *step));
                (GraphSetValue::List(bounds), true)
            }
            value => (value, false),
        };
        GraphSetProjection::admit_output(&value, &spec.input, column)?;
        let mut projection = spec.projection.into_vec();
        projection.push(GraphSetProjection::new(name, value));
        spec.projection = projection.into_boxed_slice();
        let mut types = spec.types.into_vec();
        types.push(GraphSetColumnType::Any);
        spec.types = types.into_boxed_slice();
        spec.filter = Box::new([]);
        spec.expand_last = true;
        spec.expand_range = range;
        Ok(spec)
    }
}

pub(super) fn append<E>(
    evaluated: &GraphValueRow,
    weight: &ZWeight,
    limbs: LimbLimit,
    range: bool,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    output: &mut Vec<(GraphValueRow, ZWeight)>,
) -> Result<(), RowProjectionError<E>> {
    charge(control, ZSetEvent::Work)?;
    let (list, prefix) = evaluated
        .values()
        .split_last()
        .ok_or(RowProjectionError::InvalidResult)?;
    let failure = |kind| RowProjectionError::Expression {
        column: prefix.len(),
        error: GraphIntegerError {
            instruction: 0,
            kind,
        },
    };
    if range {
        return append_range(list, prefix, weight, limbs, &failure, control, output);
    }
    if list.is_null() {
        return Ok(());
    }
    let GraphValue::List(elements) = list else {
        return Err(failure(GraphIntegerErrorKind::IncompatibleOperands));
    };
    // Nested values may be assembled from several valid input cells. Check the
    // evaluated list before its descendants are copied or traversed repeatedly.
    if !list.validate_bounds() {
        return Err(failure(GraphIntegerErrorKind::Overflow));
    }
    for element in elements.iter() {
        charge(control, ZSetEvent::Work)?;
        charge(control, ZSetEvent::ScratchEntry)?;
        let mut cells = Vec::new();
        for cell in prefix.iter().chain(core::iter::once(element)) {
            charge(control, ZSetEvent::Work)?;
            for _ in 0..=cell.payload_units() {
                charge(control, ZSetEvent::ScratchEntry)?;
            }
            cells.push(cell.clone());
        }
        let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
        charge(control, ZSetEvent::ScratchEntry)?;
        output.push((GraphValueRow::from_owned_values(cells), weight));
    }
    Ok(())
}

/// The members of `range(start, end[, step])` from its evaluated bounds, as
/// evaluate_value_at's Range arm reads them: a non-integer bound is a typed
/// NonInteger error, any NULL bound makes range() NULL and unwinds nothing,
/// and only then is a zero step InvalidRangeStep. Each member charges the
/// scratch entry its materialized list element would have, then the same
/// output row as a listed element (fgdb-85aal).
fn append_range<E>(
    bounds: &GraphValue,
    prefix: &[GraphValue],
    weight: &ZWeight,
    limbs: LimbLimit,
    failure: &impl Fn(GraphIntegerErrorKind) -> RowProjectionError<E>,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    output: &mut Vec<(GraphValueRow, ZWeight)>,
) -> Result<(), RowProjectionError<E>> {
    let GraphValue::List(bounds) = bounds else {
        return Err(RowProjectionError::InvalidResult);
    };
    let mut values = [None, None, Some(1)];
    for (at, bound) in bounds.iter().enumerate() {
        values[at] = match bound {
            GraphValue::Scalar(fgdb_types::CanonicalScalar::Int(value)) => Some(*value),
            bound if bound.is_null() => None,
            _ => return Err(failure(GraphIntegerErrorKind::NonInteger)),
        };
    }
    let [Some(start), Some(end), Some(step)] = values else {
        return Ok(());
    };
    let count = crate::set_ops::range_member_count(start, end, step).map_err(failure)?;
    let (start, step) = (i128::from(start), i128::from(step));
    for at in 0..count {
        charge(control, ZSetEvent::ScratchEntry)?;
        let member =
            i64::try_from(start + at * step).expect("a range member lies within its bounds");
        let element = GraphValue::Scalar(fgdb_types::CanonicalScalar::Int(member));
        charge(control, ZSetEvent::Work)?;
        charge(control, ZSetEvent::ScratchEntry)?;
        let mut cells = Vec::new();
        for cell in prefix.iter().chain(core::iter::once(&element)) {
            charge(control, ZSetEvent::Work)?;
            for _ in 0..=cell.payload_units() {
                charge(control, ZSetEvent::ScratchEntry)?;
            }
            cells.push(cell.clone());
        }
        let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
        charge(control, ZSetEvent::ScratchEntry)?;
        output.push((GraphValueRow::from_owned_values(cells), weight));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
