//! Physical WHERE-over-product execution without the Cartesian intermediate.
//!
//! Both complete inputs are already admitted. The ordinary predicate kernel
//! sees borrowed pairs; only selected rows are copied. A necessary equality
//! key permits ordered probes into borrowed right-row ordinals. Key ties use
//! original ordinals so child order and duplicate occurrences remain exact.
//! No logical rewrite, new comparison law, hash join or graph rescan is used.

use super::*;
use crate::algebra::{IntegerComparison, MAX_PATTERN_PREDICATES};

mod maintained;

type Key = (usize, usize);

fn equality(op: &GraphSetPredicateOp, width: usize, columns: Option<&[usize]>) -> Option<Key> {
    let GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(b),
    } = op
    else {
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
    let Some(projection) = projection else {
        return Ok(None);
    };
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
        let Some(key) = equality(op, width, columns) else {
            continue;
        };
        let mut duplicate = false;
        for prior in &keys {
            control(GlaExecutionEvent::Work)?;
            duplicate |= *prior == key;
        }
        if duplicate {
            continue;
        }
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
        // Dynamic Any inputs may carry either graph identity. Lists/paths/maps
        // are not comparable by this predicate IR; NULL never supplies equality.
        if cell.is_null()
            || !matches!(
                cell,
                GraphValue::Scalar(_) | GraphValue::Vertex(_) | GraphValue::Edge(_)
            )
        {
            return Ok(false);
        }
    }
    Ok(true)
}

// A lookup equivalence, not canonical storage identity or general ORDER BY.
// WHERE admits Int/Float equality by exact value, so they MUST share one
// contiguous key family. Decimal remains separate: language orderability also
// ties decimals to numbers, but the native WHERE law does not. Putting only
// cross-kind numeric pairs through a different comparator would not suffice:
// an intervening scalar family could make that comparator nontransitive.
fn compare_key_value<E>(
    left: &GraphValue,
    right: &GraphValue,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    use fgdb_types::CanonicalScalar;
    let numeric = |value: &GraphValue| {
        matches!(
            value,
            GraphValue::Scalar(CanonicalScalar::Int(_) | CanonicalScalar::Float(_))
        )
    };
    control(GlaExecutionEvent::Work)?;
    match (numeric(left), numeric(right)) {
        (true, true) => left.compare_orderability_with_control(right, control),
        (true, false) => Ok(Ordering::Less),
        (false, true) => Ok(Ordering::Greater),
        (false, false) => compare_value(left, right, control),
    }
}

fn compare_keys<E>(
    left: &GraphValueRow,
    left_is_right: bool,
    right: &GraphValueRow,
    keys: &[Key],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Ordering, E> {
    for &(a, b) in keys {
        let order = compare_key_value(
            &left.values()[if left_is_right { b } else { a }],
            &right.values()[b],
            control,
        )?;
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(Ordering::Equal)
}

/// The count of indexed rows that order before `row` (or, for `upper`, before
/// or with it). `hint` is a likely answer, such as the previous left row's:
/// the probe tries it and its neighbour first, so a left side that ascends
/// through dense keys costs two comparisons a boundary, not log2(right). Any
/// other answer is found by bisecting the side of `hint` it lies on, so a
/// wrong hint costs at most two comparisons more than plain bisection. Every
/// hint yields the same boundary.
fn boundary<E>(
    row: &GraphValueRow,
    right: &[GraphValueRow],
    index: &[usize],
    keys: &[Key],
    upper: bool,
    hint: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<usize, E> {
    // Whether `row` orders after the indexed row at `at`: true on a prefix.
    let mut after = |at: usize| -> Result<bool, E> {
        control(GlaExecutionEvent::Work)?;
        let order = compare_keys(row, false, &right[index[at]], keys, control)?;
        Ok(order == Ordering::Greater || (upper && order == Ordering::Equal))
    };
    let hint = hint.min(index.len());
    let (mut lo, mut hi) = (0, index.len());
    if hint < index.len() && after(hint)? {
        lo = hint + 1;
        if lo == index.len() || !after(lo)? {
            return Ok(lo);
        }
        lo += 1;
    } else if hint > 0 {
        hi = hint - 1;
        if after(hi)? {
            return Ok(hint);
        }
    } else if hint < index.len() {
        return Ok(0);
    }
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if after(mid)? {
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

    /// The indexed rows whose keys equal `row`'s. `hint` carries the previous
    /// probe's lower boundary to the next, as canonically ordered left rows
    /// usually ascend through the same keys.
    fn range<E>(
        &self,
        row: &GraphValueRow,
        right: &[GraphValueRow],
        hint: &mut usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<core::ops::Range<usize>, E> {
        if self.keys.is_empty() {
            Ok(0..right.len())
        } else if eligible(row, &self.keys, false, control)? {
            let lower = boundary(
                row,
                right,
                &self.ordinals,
                &self.keys,
                false,
                *hint,
                control,
            )?;
            let upper = boundary(row, right, &self.ordinals, &self.keys, true, lower, control)?;
            *hint = lower;
            Ok(lower..upper)
        } else {
            Ok(0..0)
        }
    }

    fn row<'a>(&self, right: &'a [GraphValueRow], at: usize) -> &'a GraphValueRow {
        &right[if self.keys.is_empty() {
            at
        } else {
            self.ordinals[at]
        }]
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
    visit_with_context(
        left,
        right,
        code,
        columns,
        control,
        |control, event| control(event),
        consume,
    )
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
    let Some(first) = left.first() else {
        return Ok(());
    };
    if right.is_empty() {
        return Ok(());
    }
    let index = Index::build(first.len(), right, code, columns, &mut |value| {
        event(context, value)
    })?;
    let mut hint = 0;
    for row in left {
        event(context, GlaExecutionEvent::Work)?;
        let range = index.range(row, right, &mut hint, &mut |value| event(context, value))?;
        for at in range {
            event(context, GlaExecutionEvent::Work)?;
            let other = index.row(right, at);
            if GraphSetPredicateOp::evaluate_projected_pair_with_control(
                code,
                row,
                other,
                columns,
                &mut |value| event(context, value),
            )? {
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
    let Some(first) = left.first() else {
        return Ok(());
    };
    if right.is_empty() {
        return Ok(());
    }
    let mut runs = true;
    for op in code {
        event(context, GlaExecutionEvent::Work)?;
        runs &= matches!(
            op,
            GraphSetPredicateOp::And | GraphSetPredicateOp::Truth(Some(true))
        ) || equality(op, first.len(), columns).is_some();
    }
    if !runs {
        return visit_with_context(
            left,
            right,
            code,
            columns,
            context,
            event,
            |_, _, context| consume(1, context),
        );
    }
    let index = Index::build(first.len(), right, code, columns, &mut |value| {
        event(context, value)
    })?;
    let mut hint = 0;
    for row in left {
        event(context, GlaExecutionEvent::Work)?;
        let range = index.range(row, right, &mut hint, &mut |value| event(context, value))?;
        if !range.is_empty() {
            consume(range.len(), context)?;
        }
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
    visit(
        left,
        right,
        code,
        columns,
        control,
        |left, right, control| {
            output.push(copy_pair(left, right, columns, control)?);
            Ok(())
        },
    )?;
    Ok(output)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lookup_domain_tests {
    use super::*;
    use fgdb_types::{CanonicalDecimal, CanonicalF64, CanonicalScalar, EId, VId};

    fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
        Ok(())
    }
    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn float(value: f64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
    }
    fn rows(values: Vec<GraphValue>) -> Vec<GraphValueRow> {
        values
            .into_iter()
            .map(|value| GraphValueRow::from_owned_values(vec![value]))
            .collect()
    }
    fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
        GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(a),
            comparison: IntegerComparison::Equal,
            right: GraphSetOperand::Column(b),
        }
    }
    fn count(
        left: &[GraphValueRow],
        right: &[GraphValueRow],
        code: &[GraphSetPredicateOp],
        columns: Option<&[usize]>,
    ) -> usize {
        let mut total = 0;
        count_with_context(
            left,
            right,
            code,
            columns,
            &mut total,
            |_, _| Ok::<_, usize>(()),
            |amount, total| {
                *total += amount;
                Ok(())
            },
        )
        .unwrap();
        total
    }
    fn literal_pairs(
        left: &[GraphValueRow],
        right: &[GraphValueRow],
        pairs: &[(usize, usize)],
    ) -> Vec<GraphValueRow> {
        pairs
            .iter()
            .map(|&(l, r)| {
                GraphValueRow::from_owned_values(vec![
                    left[l].values()[0].clone(),
                    right[r].values()[0].clone(),
                ])
            })
            .collect()
    }

    #[test]
    fn numeric_lookup_preserves_occurrences_precision_and_count_without_coercion() {
        let left = rows(vec![
            float(7.0),
            int(7),
            int(9_007_199_254_740_993),
            int(i64::MAX),
            float(f64::NAN),
        ]);
        let right = rows(vec![
            int(7),
            float(9_007_199_254_740_992.0),
            float(7.0),
            int(9_007_199_254_740_993),
            float(9_223_372_036_854_775_808.0),
            float(f64::NAN),
        ]);
        let expected = literal_pairs(
            &left,
            &right,
            &[(0, 0), (0, 2), (1, 0), (1, 2), (2, 3), (4, 5)],
        );
        let code = [equal(0, 1)];
        assert_eq!(
            collect(&left, &right, &code, None, &mut allow).unwrap(),
            expected
        );
        assert_eq!(count(&left, &right, &code, None), 6);
        // The lookup must not rewrite values or merge their canonical identities.
        assert_ne!(left[0].values()[0], left[1].values()[0]);
    }

    #[test]
    fn edge_lookup_keeps_identity_domains_separate_and_duplicate_right_order() {
        let left = rows(vec![
            GraphValue::Edge(EId(5)),
            GraphValue::Vertex(VId(5)),
            int(5),
            GraphValue::Edge(EId(5)),
        ]);
        let right = rows(vec![
            GraphValue::Vertex(VId(5)),
            GraphValue::Edge(EId(5)),
            int(5),
            GraphValue::Edge(EId(5)),
            GraphValue::Edge(EId(6)),
        ]);
        let expected = literal_pairs(
            &left,
            &right,
            &[(0, 1), (0, 3), (1, 0), (2, 2), (3, 1), (3, 3)],
        );
        let code = [equal(0, 1)];
        assert_eq!(
            collect(&left, &right, &code, None, &mut allow).unwrap(),
            expected
        );
        assert_eq!(count(&left, &right, &code, None), 6);
    }

    #[test]
    fn lookup_ranges_are_exact_native_truth_classes_including_decimal_and_null() {
        let values = vec![
            int(7),
            float(7.0),
            int(0),
            float(-0.0),
            int(i64::MIN),
            float(-9_223_372_036_854_775_808.0),
            int(i64::MAX),
            float(9_223_372_036_854_775_808.0),
            int(9_007_199_254_740_993),
            float(9_007_199_254_740_992.0),
            float(f64::NEG_INFINITY),
            float(f64::INFINITY),
            float(f64::NAN),
            GraphValue::Scalar(CanonicalScalar::Decimal(
                CanonicalDecimal::from_scaled_half_even(7, 0).unwrap(),
            )),
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("7").unwrap()),
            GraphValue::Scalar(CanonicalScalar::bytes(vec![7]).unwrap()),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Vertex(VId(7)),
            GraphValue::Edge(EId(7)),
            GraphValue::List(vec![int(7)].into_boxed_slice()),
        ];
        let left = rows(values.clone());
        let right = rows(values.into_iter().rev().collect());
        let code = [equal(0, 1)];
        GraphSetPredicateOp::validate_schema(&[GraphSetColumnType::Any; 2], &code).unwrap();
        // The ordinary predicate is the semantic oracle, not the index comparator.
        let mut expected = Vec::new();
        for l in &left {
            for r in &right {
                if GraphSetPredicateOp::evaluate_pair_with_control(&code, l, r, &mut allow).unwrap()
                {
                    expected.push(GraphValueRow::from_owned_values(vec![
                        l.values()[0].clone(),
                        r.values()[0].clone(),
                    ]));
                }
            }
        }
        assert_eq!(
            collect(&left, &right, &code, None, &mut allow).unwrap(),
            expected
        );
        assert_eq!(count(&left, &right, &code, None), expected.len());
        let index = Index::build(1, &right, &code, None, &mut allow).unwrap();
        for l in &left {
            let literal: Vec<_> = right
                .iter()
                .enumerate()
                .filter_map(|(at, r)| {
                    GraphSetPredicateOp::evaluate_pair_with_control(&code, l, r, &mut allow)
                        .unwrap()
                        .then_some(at)
                })
                .collect();
            // Exercise all lower-bound hints, including both ends, not just sorted probes.
            for mut hint in 0..=right.len() {
                let range = index.range(l, &right, &mut hint, &mut allow).unwrap();
                assert_eq!(
                    range.map(|at| index.ordinals[at]).collect::<Vec<_>>(),
                    literal
                );
            }
        }
    }

    #[test]
    fn composite_mapped_numeric_and_edge_keys_keep_repeated_output_columns() {
        let left = [
            GraphValueRow::from_owned_values(vec![float(7.0), GraphValue::Edge(EId(2))]),
            GraphValueRow::from_owned_values(vec![int(7), GraphValue::Edge(EId(1))]),
        ];
        let right = [
            GraphValueRow::from_owned_values(vec![GraphValue::Edge(EId(1)), float(7.0)]),
            GraphValueRow::from_owned_values(vec![GraphValue::Edge(EId(2)), int(7)]),
            GraphValueRow::from_owned_values(vec![GraphValue::Edge(EId(2)), float(7.0)]),
        ];
        let mapping = [3, 1, 0, 2, 3];
        let code = [
            equal(0, 2),
            equal(1, 3),
            GraphSetPredicateOp::And,
            equal(4, 2),
            GraphSetPredicateOp::And,
        ];
        GraphSetPredicateOp::validate_schema(&[GraphSetColumnType::Any; 5], &code).unwrap();
        let expected: Vec<_> = [(0, 1), (0, 2), (1, 0)]
            .into_iter()
            .map(|(l, r)| {
                let cells = [
                    &left[l].values()[0],
                    &left[l].values()[1],
                    &right[r].values()[0],
                    &right[r].values()[1],
                ];
                GraphValueRow::from_owned_values(
                    mapping.iter().map(|&at| cells[at].clone()).collect(),
                )
            })
            .collect();
        assert_eq!(
            collect(&left, &right, &code, Some(&mapping), &mut allow).unwrap(),
            expected
        );
        assert_eq!(count(&left, &right, &code, Some(&mapping)), 3);
    }

    #[test]
    fn mixed_domain_index_refuses_every_checkpoint_without_mutating_inputs() {
        let left = rows(vec![float(7.0), GraphValue::Edge(EId(1)), int(7)]);
        let right = rows(vec![int(7), GraphValue::Edge(EId(1)), float(7.0)]);
        let before = (left.clone(), right.clone());
        let code = [equal(0, 1)];
        let mut events = 0;
        let expected = collect(&left, &right, &code, None, &mut |_| {
            events += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
        for stop in 1..=events {
            let mut seen = 0;
            assert_eq!(
                collect(&left, &right, &code, None, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(stop)
            );
            assert_eq!(seen, stop);
            assert_eq!((&left, &right), (&before.0, &before.1));
        }
        assert_eq!(
            collect(&left, &right, &code, None, &mut allow).unwrap(),
            expected
        );
    }
}
