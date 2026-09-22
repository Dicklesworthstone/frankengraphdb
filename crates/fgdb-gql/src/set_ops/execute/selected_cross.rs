//! Physical WHERE-over-product execution without the Cartesian intermediate.
//!
//! Both complete inputs are already admitted. The ordinary predicate kernel
//! sees borrowed pairs; only selected rows are copied. A necessary equality
//! key permits ordered probes into borrowed right-row ordinals. Key ties use
//! original ordinals so child order and duplicate occurrences remain exact.
//! No logical rewrite, new comparison law, hash join or graph rescan is used.

use super::*;
use crate::algebra::{IntegerComparison, MAX_PATTERN_PREDICATES};

type Key = (usize, usize);

fn equality(op: &GraphSetPredicateOp, width: usize, columns: Option<&[usize]>) -> Option<Key> {
    let GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(b),
    } = op else {
        return None;
    };
    let a = columns.map_or(*a, |columns| columns[*a]);
    let b = columns.map_or(*b, |columns| columns[*b]);
    if a < width && b >= width {
        Some((a, b - width))
    } else if b < width && a >= width {
        Some((b, a - width))
    } else {
        None
    }
}

pub(super) fn columns<E>(
    projection: Option<&[GraphSetProjection]>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<Vec<usize>>, E> {
    let Some(projection) = projection else { return Ok(None) };
    let mut columns = Vec::new();
    for column in projection {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        let GraphSetValue::Column(at) = column.value() else {
            unreachable!("the physical shape admits only column projections")
        };
        columns.push(*at);
    }
    Ok(Some(columns))
}

/// Prove necessary equalities, not a replacement Boolean expression. A TRUE
/// conjunction requires both arms; a TRUE disjunction requires either arm.
/// Therefore proof bits combine with OR for AND and AND for OR. NOT discards
/// the proof, including double negation; that conservative choice cannot turn
/// UNKNOWN into a match. Stack shape is owned by the checked predicate.
fn required_keys<E>(
    width: usize,
    code: &[GraphSetPredicateOp],
    columns: Option<&[usize]>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Vec<Key>, E> {
    let mut keys = Vec::new();
    for op in code {
        control(GlaExecutionEvent::Work)?;
        let Some(key) = equality(op, width, columns) else { continue };
        let mut duplicate = false;
        for prior in &keys {
            control(GlaExecutionEvent::Work)?;
            duplicate |= *prior == key;
        }
        if duplicate { continue; }
        let mut stack = [false; MAX_PATTERN_PREDICATES];
        let mut depth = 0;
        for op in code {
            control(GlaExecutionEvent::Work)?;
            match op {
                GraphSetPredicateOp::Not => stack[depth - 1] = false,
                GraphSetPredicateOp::And | GraphSetPredicateOp::Or => {
                    let right = stack[depth - 1];
                    depth -= 1;
                    let left = &mut stack[depth - 1];
                    *left = if matches!(op, GraphSetPredicateOp::And) {
                        *left || right
                    } else {
                        *left && right
                    };
                }
                _ => {
                    stack[depth] = equality(op, width, columns) == Some(key);
                    depth += 1;
                }
            }
        }
        debug_assert_eq!(depth, 1);
        if stack[0] {
            control(GlaExecutionEvent::ScratchEntry)?;
            keys.push(key);
        }
    }
    Ok(keys)
}

fn eligible<E>(
    row: &GraphValueRow,
    keys: &[Key],
    right: bool,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    for &(a, b) in keys {
        control(GlaExecutionEvent::Work)?;
        let cell = &row.values()[if right { b } else { a }];
        // Dynamic Any inputs are legal, but only scalar/vertex equality can
        // be TRUE in the existing WHERE law. NULL never supplies an equality.
        if cell.is_null() || !matches!(cell, GraphValue::Scalar(_) | GraphValue::Vertex(_)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn compare_keys<E>(
    left: &GraphValueRow,
    left_is_right: bool,
    right: &GraphValueRow,
    keys: &[Key],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    for &(a, b) in keys {
        let order = compare_value(
            &left.values()[if left_is_right { b } else { a }],
            &right.values()[b],
            control,
        )?;
        if order != Ordering::Equal { return Ok(order); }
    }
    Ok(Ordering::Equal)
}

fn boundary<E>(
    row: &GraphValueRow,
    right: &[GraphValueRow],
    index: &[usize],
    keys: &[Key],
    upper: bool,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<usize, E> {
    let (mut lo, mut hi) = (0, index.len());
    while lo < hi {
        control(GlaExecutionEvent::Work)?;
        let mid = lo + (hi - lo) / 2;
        let order = compare_keys(row, false, &right[index[mid]], keys, control)?;
        if order == Ordering::Greater || (upper && order == Ordering::Equal) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// The one ordered access structure used by row emission and cardinality.
struct Index {
    keys: Vec<Key>,
    ordinals: Vec<usize>,
}
impl Index {
    fn build<E>(
        width: usize,
        right: &[GraphValueRow],
        code: &[GraphSetPredicateOp],
        columns: Option<&[usize]>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        let keys = required_keys(width, code, columns, control)?;
        let mut ordinals = Vec::new();
        if !keys.is_empty() {
            for (at, row) in right.iter().enumerate() {
                control(GlaExecutionEvent::Work)?;
                if eligible(row, &keys, true, control)? {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    ordinals.push(at);
                }
            }
            merge::sort(&mut ordinals, control, &mut |a, b, control| {
                let order = compare_keys(&right[*a], true, &right[*b], &keys, control)?;
                control(GlaExecutionEvent::Work)?;
                Ok(order.then_with(|| a.cmp(b)))
            })?;
        }
        Ok(Self { keys, ordinals })
    }

    fn range<E>(
        &self,
        row: &GraphValueRow,
        right: &[GraphValueRow],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<core::ops::Range<usize>, E> {
        if self.keys.is_empty() {
            Ok(0..right.len())
        } else if eligible(row, &self.keys, false, control)? {
            Ok(boundary(row, right, &self.ordinals, &self.keys, false, control)?
                ..boundary(row, right, &self.ordinals, &self.keys, true, control)?)
        } else {
            Ok(0..0)
        }
    }

    fn row<'a>(&self, right: &'a [GraphValueRow], at: usize) -> &'a GraphValueRow {
        &right[if self.keys.is_empty() { at } else { self.ordinals[at] }]
    }
}

/// The consumer is invoked only for TRUE pairs, in the original left-major
/// sequence. Inputs remain resident; the index is O(right occurrences), with
/// no key/payload clone. For fixed predicate/key width, index work is
/// O(R log R + L log R + matching-key candidates), not O(L*R). A theta predicate
/// without a proved key still scans L*R candidates but never materializes them.
/// Comparison payload costs and every seek/sort operation are cancellable.
pub(super) fn visit<E, C>(
    left: &[GraphValueRow],
    right: &[GraphValueRow],
    code: &[GraphSetPredicateOp],
    columns: Option<&[usize]>,
    control: &mut C,
    consume: impl FnMut(&GraphValueRow, &GraphValueRow, &mut C) -> Result<(), E>,
) -> Result<(), E>
where
    C: FnMut(GlaExecutionEvent) -> Result<(), E>,
{
    visit_with_context(left, right, code, columns, control, |control, event| control(event), consume)
}

/// The same probe walk with a single caller-owned context. Folded aggregates
/// can meter probes and consume accepted pairs through their existing owner,
/// without an intermediate result bag or a second predicate/index engine.
pub(super) fn visit_with_context<E, Context>(
    left: &[GraphValueRow],
    right: &[GraphValueRow],
    code: &[GraphSetPredicateOp],
    columns: Option<&[usize]>,
    context: &mut Context,
    mut event: impl FnMut(&mut Context, GlaExecutionEvent) -> Result<(), E>,
    mut consume: impl FnMut(&GraphValueRow, &GraphValueRow, &mut Context) -> Result<(), E>,
) -> Result<(), E> {
    event(context, GlaExecutionEvent::Work)?;
    // Both source subtrees must have completed before this call. Empty bags
    // cannot hide a source failure; they only avoid unneeded local indexing.
    let Some(first) = left.first() else { return Ok(()) };
    if right.is_empty() { return Ok(()); }
    let index = Index::build(first.len(), right, code, columns, &mut |value| event(context, value))?;
    for row in left {
        event(context, GlaExecutionEvent::Work)?;
        let range = index.range(row, right, &mut |value| event(context, value))?;
        for at in range {
            event(context, GlaExecutionEvent::Work)?;
            let other = index.row(right, at);
            if GraphSetPredicateOp::evaluate_projected_pair_with_control(code, row, other, columns, &mut |value| event(context, value))? {
                consume(row, other, context)?;
            }
        }
    }
    event(context, GlaExecutionEvent::Work)?;
    Ok(())
}

/// Count selected occurrences without constructing output rows. For a pure
/// conjunction of cross-input equalities, each indexed range is already a
/// complete TRUE run: contribute its length in one checked caller-owned step.
/// Residual/OR/NOT predicates use the same borrowed-pair walk, contributing
/// one per accepted occurrence. No outer join, DISTINCT or coercion is implied.
/// The caller's exact cardinality accumulator owns overflow and final paging.
pub(super) fn count_with_context<E, Context>(
    left: &[GraphValueRow],
    right: &[GraphValueRow],
    code: &[GraphSetPredicateOp],
    columns: Option<&[usize]>,
    context: &mut Context,
    mut event: impl FnMut(&mut Context, GlaExecutionEvent) -> Result<(), E>,
    mut consume: impl FnMut(usize, &mut Context) -> Result<(), E>,
) -> Result<(), E> {
    event(context, GlaExecutionEvent::Work)?;
    let Some(first) = left.first() else { return Ok(()) };
    if right.is_empty() { return Ok(()); }
    let mut runs = true;
    for op in code {
        event(context, GlaExecutionEvent::Work)?;
        runs &= matches!(op, GraphSetPredicateOp::And | GraphSetPredicateOp::Truth(Some(true)))
            || equality(op, first.len(), columns).is_some();
    }
    if !runs {
        return visit_with_context(left, right, code, columns, context, event,
            |_, _, context| consume(1, context));
    }
    let index = Index::build(first.len(), right, code, columns, &mut |value| event(context, value))?;
    for row in left {
        event(context, GlaExecutionEvent::Work)?;
        let range = index.range(row, right, &mut |value| event(context, value))?;
        if !range.is_empty() { consume(range.len(), context)?; }
    }
    event(context, GlaExecutionEvent::Work)?;
    Ok(())
}

pub(super) fn copy_pair<E>(
    left: &GraphValueRow,
    right: &GraphValueRow,
    columns: Option<&[usize]>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<GraphValueRow, E> {
    control(GlaExecutionEvent::ScratchEntry)?;
    let mut values = Vec::new();
    if let Some(columns) = columns {
        for &column in columns {
            let value = if column < left.len() {
                &left.values()[column]
            } else {
                &right.values()[column - left.len()]
            };
            values.push(projection::copy_value(value, control)?);
        }
    } else {
        for value in left.values().iter().chain(right.values()) {
            values.push(projection::copy_value(value, control)?);
        }
    }
    Ok(GraphValueRow::from_owned_values(values))
}

pub(super) fn collect<E>(
    left: &[GraphValueRow],
    right: &[GraphValueRow],
    code: &[GraphSetPredicateOp],
    columns: Option<&[usize]>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Vec<GraphValueRow>, E> {
    let mut output = Vec::new();
    visit(left, right, code, columns, control, |left, right, control| {
        output.push(copy_pair(left, right, columns, control)?);
        Ok(())
    })?;
    Ok(output)
}

#[cfg(test)]
mod tests;
