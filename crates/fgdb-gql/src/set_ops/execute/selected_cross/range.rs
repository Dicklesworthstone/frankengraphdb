//! Ordered scalar candidates for a necessary inequality over completed inputs.
//!
//! This is a resident access path, not a predicate evaluator or a spill index.
//! Right rows remain in place. Only their ordinals and scalar-family intervals
//! are indexed. Row emission still runs the complete native WHERE; a proved
//! complete band can instead contribute its exact interval cardinality.

use super::*;
use core::ops::Range;
use fgdb_types::CanonicalScalar;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Constraint {
    key: Key,
    comparison: IntegerComparison,
}

fn inequality(
    op: &GraphSetPredicateOp,
    width: usize,
    columns: Option<&[usize]>,
) -> Option<Constraint> {
    let GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison,
        right: GraphSetOperand::Column(b),
    } = op
    else {
        return None;
    };
    let reversed = match comparison {
        IntegerComparison::Less => IntegerComparison::Greater,
        IntegerComparison::LessOrEqual => IntegerComparison::GreaterOrEqual,
        IntegerComparison::Greater => IntegerComparison::Less,
        IntegerComparison::GreaterOrEqual => IntegerComparison::LessOrEqual,
        _ => return None,
    };
    let a = columns.map_or(*a, |columns| columns[*a]);
    let b = columns.map_or(*b, |columns| columns[*b]);
    if a < width && b >= width {
        Some(Constraint {
            key: (a, b - width),
            comparison: *comparison,
        })
    } else if b < width && a >= width {
        Some(Constraint {
            key: (b, a - width),
            comparison: reversed,
        })
    } else {
        None
    }
}

fn required<E>(
    width: usize,
    code: &[GraphSetPredicateOp],
    columns: Option<&[usize]>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<Constraint>, E> {
    for op in code {
        control(GlaExecutionEvent::Work)?;
        let Some(candidate) = inequality(op, width, columns) else {
            continue;
        };
        if requires_atom(
            code,
            |op| inequality(op, width, columns) == Some(candidate),
            control,
        )? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

// The existing predicate admits order comparisons only between compatible
// scalars. Duration ordering, graph identities and composites yield UNKNOWN.
fn scalar(value: &GraphValue) -> Option<&CanonicalScalar> {
    match value {
        GraphValue::Scalar(CanonicalScalar::Null | CanonicalScalar::Duration(_)) => None,
        GraphValue::Scalar(value) => Some(value),
        _ => None,
    }
}

fn same_family(left: &GraphValue, right: &GraphValue) -> bool {
    let (Some(left), Some(right)) = (scalar(left), scalar(right)) else {
        return false;
    };
    matches!(
        (left, right),
        (
            CanonicalScalar::Int(_) | CanonicalScalar::Float(_),
            CanonicalScalar::Int(_) | CanonicalScalar::Float(_)
        )
    ) || core::mem::discriminant(left) == core::mem::discriminant(right)
}

pub(super) struct Index {
    constraint: Constraint,
    ordinals: Vec<usize>,
    families: Vec<Range<usize>>,
}

impl Index {
    pub(super) fn build<E>(
        width: usize,
        right: &[GraphValueRow],
        code: &[GraphSetPredicateOp],
        columns: Option<&[usize]>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<Self>, E> {
        let Some(constraint) = required(width, code, columns, control)? else {
            return Ok(None);
        };
        Self::with_constraint(constraint, right, control).map(Some)
    }

    fn with_constraint<E>(
        constraint: Constraint,
        right: &[GraphValueRow],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        let at = constraint.key.1;
        let mut ordinals = Vec::new();
        for (ordinal, row) in right.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            if scalar(&row.values()[at]).is_some() {
                control(GlaExecutionEvent::ScratchEntry)?;
                ordinals.push(ordinal);
            }
        }
        // The existing equality lookup comparator already groups Int/Float
        // by exact numeric value, separately from Decimal and all other kinds.
        merge::sort(&mut ordinals, control, &mut |a, b, control| {
            let order = compare_key_value(&right[*a].values()[at], &right[*b].values()[at], control)?;
            control(GlaExecutionEvent::Work)?;
            Ok(order.then_with(|| a.cmp(b)))
        })?;
        let mut families: Vec<Range<usize>> = Vec::new();
        for (position, &ordinal) in ordinals.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            if let Some(last) = families.last_mut()
                && same_family(
                    &right[ordinals[last.start]].values()[at],
                    &right[ordinal].values()[at],
                )
            {
                last.end = position + 1;
                continue;
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            families.push(position..position + 1);
        }
        Ok(Self {
            constraint,
            ordinals,
            families,
        })
    }

    fn range<E>(
        &self,
        row: &GraphValueRow,
        right: &[GraphValueRow],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Range<usize>, E> {
        self.range_for(self.constraint, row, right, control)
    }

    fn range_for<E>(
        &self,
        constraint: Constraint,
        row: &GraphValueRow,
        right: &[GraphValueRow],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Range<usize>, E> {
        control(GlaExecutionEvent::Work)?;
        let (left_column, right_column) = constraint.key;
        debug_assert_eq!(right_column, self.constraint.key.1);
        let value = &row.values()[left_column];
        if scalar(value).is_none() {
            return Ok(0..0);
        }
        // There are at most as many intervals as scalar kinds, not right rows.
        // Restrict BEFORE probing: total storage rank is not WHERE truth across
        // unrelated types, nor may Decimal share Int/Float's predicate family.
        for family in &self.families {
            control(GlaExecutionEvent::Work)?;
            if !same_family(value, &right[self.ordinals[family.start]].values()[right_column]) {
                continue;
            }
            let upper = matches!(
                constraint.comparison,
                IntegerComparison::Less | IntegerComparison::GreaterOrEqual
            );
            let bound = boundary(
                row,
                right,
                &self.ordinals[family.clone()],
                &[constraint.key],
                upper,
                0,
                control,
            )? + family.start;
            return Ok(match constraint.comparison {
                IntegerComparison::Less | IntegerComparison::LessOrEqual => bound..family.end,
                IntegerComparison::Greater | IntegerComparison::GreaterOrEqual => family.start..bound,
                _ => unreachable!("only checked inequalities are admitted"),
            });
        }
        Ok(0..0)
    }

    /// Restore original right order, never key order, before the caller runs
    /// its complete eager predicate and observes output occurrences. Reuse one
    /// ordinal buffer; no row or scalar payload is cloned. Retained length is
    /// at most the right input size; allocator capacity may exceed that length.
    /// A K-candidate interval adds O(K log K) ordinal ordering, so this path can
    /// cost more than a full scan for broad predicates. Logical work/scratch
    /// admissions remain shared and cumulative, not a byte cap or spill claim.
    pub(super) fn select<E>(
        &self,
        row: &GraphValueRow,
        right: &[GraphValueRow],
        selected: &mut Vec<usize>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        selected.clear();
        for at in self.range(row, right, control)? {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            selected.push(self.ordinals[at]);
        }
        merge::sort(selected, control, &mut |a, b, control| {
            control(GlaExecutionEvent::Work)?;
            Ok(a.cmp(b))
        })
    }
}

// Unlike a necessary constraint, this recognizes the COMPLETE predicate as a
// conjunction of scalar bounds on one right column. It must refuse residuals,
// OR and NOT before inspecting inputs or allocating any index. Column mapping
// and operand reversal are identical to row emission's constraint binding.
fn complete_band<E>(
    width: usize,
    code: &[GraphSetPredicateOp],
    columns: Option<&[usize]>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<Vec<Constraint>>, E> {
    let mut right_column = None;
    for op in code {
        control(GlaExecutionEvent::Work)?;
        if matches!(op, GraphSetPredicateOp::And | GraphSetPredicateOp::Truth(Some(true))) {
            continue;
        }
        let Some(bound) = inequality(op, width, columns) else {
            return Ok(None);
        };
        if right_column.is_some_and(|column| column != bound.key.1) {
            return Ok(None);
        }
        right_column = Some(bound.key.1);
    }
    if right_column.is_none() {
        return Ok(None);
    }
    let mut bounds = Vec::new();
    for op in code {
        control(GlaExecutionEvent::Work)?;
        if let Some(bound) = inequality(op, width, columns) {
            control(GlaExecutionEvent::ScratchEntry)?;
            bounds.push(bound);
        }
    }
    Ok(Some(bounds))
}

/// Count exact scalar intervals without a selected-ordinal buffer or per-pair
/// evaluation/copy. Only the caller's existing checked accumulator accepts a
/// contribution; no product of usize counts is computed here. False means the
/// immutable predicate shape is not exact, never recovery from a failed build.
/// For P bounds this uses O(R log R + L*P*log R) scalar-key work and O(R + P)
/// logical retained metadata. Payload comparison costs remain governed.
pub(super) fn try_count<E, Context>(
    left: &[GraphValueRow],
    right: &[GraphValueRow],
    code: &[GraphSetPredicateOp],
    columns: Option<&[usize]>,
    context: &mut Context,
    event: &mut impl FnMut(&mut Context, GlaExecutionEvent) -> Result<(), E>,
    consume: &mut impl FnMut(usize, &mut Context) -> Result<(), E>,
) -> Result<bool, E> {
    let Some(first) = left.first() else {
        return Ok(true);
    };
    let Some(bounds) = complete_band(first.len(), code, columns, &mut |value| {
        event(context, value)
    })? else {
        return Ok(false);
    };
    let index = Index::with_constraint(bounds[0], right, &mut |value| event(context, value))?;
    for row in left {
        event(context, GlaExecutionEvent::Work)?;
        let (mut start, mut end) = (0, index.ordinals.len());
        for &bound in &bounds {
            let range = index.range_for(bound, row, right, &mut |value| event(context, value))?;
            start = start.max(range.start);
            end = end.min(range.end);
        }
        // Different scalar families, NULL bounds and contradictory bands can
        // produce disjoint intervals. They are UNKNOWN/FALSE, never underflow.
        if end > start {
            consume(end - start, context)?;
        }
    }
    event(context, GlaExecutionEvent::Work)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_types::{CanonicalDecimal, CanonicalDuration, CanonicalF64, EId, VId};

    const OPS: [IntegerComparison; 4] = [
        IntegerComparison::Less,
        IntegerComparison::LessOrEqual,
        IntegerComparison::Greater,
        IntegerComparison::GreaterOrEqual,
    ];
    fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
        Ok(())
    }
    fn int(n: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(n))
    }
    fn float(n: f64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(n)))
    }
    fn row(values: Vec<GraphValue>) -> GraphValueRow {
        GraphValueRow::from_owned_values(values)
    }
    fn comparison(a: usize, op: IntegerComparison, b: usize) -> GraphSetPredicateOp {
        GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(a),
            comparison: op,
            right: GraphSetOperand::Column(b),
        }
    }
    fn validate(width: usize, code: &[GraphSetPredicateOp]) {
        GraphSetPredicateOp::validate_schema(&vec![GraphSetColumnType::Any; width], code).unwrap();
    }
    fn ordinary(
        left: &[GraphValueRow],
        right: &[GraphValueRow],
        code: &[GraphSetPredicateOp],
        mapping: Option<&[usize]>,
    ) -> Vec<GraphValueRow> {
        let mut output = Vec::new();
        for left in left {
            for right in right {
                let cells: Vec<_> = left.values().iter().chain(right.values()).cloned().collect();
                let projected = row(match mapping {
                    Some(mapping) => mapping.iter().map(|&at| cells[at].clone()).collect(),
                    None => cells,
                });
                if GraphSetPredicateOp::evaluate_row_with_control(code, &projected, &mut allow)
                    .unwrap()
                {
                    output.push(projected);
                }
            }
        }
        output
    }

    #[test]
    fn all_four_ranges_preserve_literal_bags_in_original_child_order() {
        let left_data = [Some(2), None, Some(1), Some(2)];
        let right_data = [Some(3), Some(1), None, Some(2)];
        let value = |n: Option<i64>| {
            n.map_or(GraphValue::Scalar(CanonicalScalar::Null), int)
        };
        for mask in 0..256 {
            let left: Vec<_> = left_data.iter().enumerate()
                .filter(|(at, _)| mask & (1 << at) != 0)
                .map(|(_, n)| row(vec![value(*n)])).collect();
            let right: Vec<_> = right_data.iter().enumerate()
                .filter(|(at, _)| mask & (1 << (at + 4)) != 0)
                .map(|(_, n)| row(vec![value(*n)])).collect();
            for op in OPS {
                let code = [comparison(0, op, 1)];
                validate(2, &code);
                let mut expected = Vec::new();
                for (a, l) in left_data.iter().enumerate() {
                    for (b, r) in right_data.iter().enumerate() {
                        if mask & (1 << a) == 0 || mask & (1 << (b + 4)) == 0 {
                            continue;
                        }
                        let (Some(l), Some(r)) = (l, r) else { continue };
                        let keep = match op {
                            IntegerComparison::Less => l < r,
                            IntegerComparison::LessOrEqual => l <= r,
                            IntegerComparison::Greater => l > r,
                            IntegerComparison::GreaterOrEqual => l >= r,
                            _ => unreachable!(),
                        };
                        if keep { expected.push(row(vec![int(*l), int(*r)])); }
                    }
                }
                assert_eq!(collect(&left, &right, &code, None, &mut allow).unwrap(), expected);
            }
        }
    }

    #[test]
    fn scalar_families_bound_exact_native_truth_without_lossy_numeric_coercion() {
        let values = vec![
            int(7), float(7.0), int(i64::MIN), int(i64::MAX),
            float(-9_223_372_036_854_775_808.0), float(9_223_372_036_854_775_808.0),
            int(9_007_199_254_740_993), float(9_007_199_254_740_992.0),
            float(-0.0), int(0), float(f64::NEG_INFINITY), float(f64::INFINITY), float(f64::NAN),
            GraphValue::Scalar(CanonicalScalar::Decimal(CanonicalDecimal::from_integer(7).unwrap())),
            GraphValue::Scalar(CanonicalScalar::Decimal(CanonicalDecimal::from_integer(8).unwrap())),
            GraphValue::Scalar(CanonicalScalar::Bool(false)),
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("a").unwrap()),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("z").unwrap()),
            GraphValue::Scalar(CanonicalScalar::bytes(vec![1]).unwrap()),
            GraphValue::Scalar(CanonicalScalar::bytes(vec![2]).unwrap()),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Duration(CanonicalDuration::new(1, 0, 0).unwrap())),
            GraphValue::Vertex(VId(1)), GraphValue::Edge(EId(1)),
            GraphValue::List(vec![int(1)].into_boxed_slice()),
        ];
        let left: Vec<_> = values.iter().cloned().map(|value| row(vec![value])).collect();
        let right: Vec<_> = values.iter().rev().cloned().map(|value| row(vec![value])).collect();
        for op in OPS {
            for reversed in [false, true] {
                let code = [if reversed { comparison(1, op, 0) } else { comparison(0, op, 1) }];
                validate(2, &code);
                let expected = ordinary(&left, &right, &code, None);
                assert_eq!(collect(&left, &right, &code, None, &mut allow).unwrap(), expected);
                let index = Index::build(1, &right, &code, None, &mut allow).unwrap().unwrap();
                for l in &left {
                    let mut selected = Vec::new();
                    index.select(l, &right, &mut selected, &mut allow).unwrap();
                    let literal: Vec<_> = right.iter().enumerate().filter_map(|(at, r)| {
                        GraphSetPredicateOp::evaluate_pair_with_control(&code, l, r, &mut allow)
                            .unwrap().then_some(at)
                    }).collect();
                    assert_eq!(selected, literal);
                }
            }
        }
    }

    #[test]
    fn shared_boolean_proof_keeps_unsafe_or_not_and_projection_boundaries() {
        let less = comparison(0, IntegerComparison::Less, 1);
        let reversed = comparison(1, IntegerComparison::Greater, 0);
        let cases = [
            (vec![less.clone()], true),
            (vec![less.clone(), GraphSetPredicateOp::Truth(Some(true)), GraphSetPredicateOp::And], true),
            (vec![less.clone(), reversed, GraphSetPredicateOp::Or], true),
            (vec![less.clone(), GraphSetPredicateOp::Truth(Some(true)), GraphSetPredicateOp::Or], false),
            (vec![less.clone(), GraphSetPredicateOp::Truth(None), GraphSetPredicateOp::Or], false),
            (vec![less.clone(), GraphSetPredicateOp::Not], false),
            (vec![less, GraphSetPredicateOp::Not, GraphSetPredicateOp::Not], false),
        ];
        let left = [row(vec![int(2)]), row(vec![GraphValue::Scalar(CanonicalScalar::Null)])];
        let right = [row(vec![int(3)]), row(vec![int(1)]), row(vec![int(3)])];
        for (code, indexed) in cases {
            validate(2, &code);
            assert_eq!(Index::build(1, &right, &code, None, &mut allow).unwrap().is_some(), indexed);
            assert_eq!(collect(&left, &right, &code, None, &mut allow).unwrap(), ordinary(&left, &right, &code, None));
        }
        let left = [row(vec![int(2), int(10)]), row(vec![int(1), int(20)])];
        let right = [row(vec![int(3), int(30)]), row(vec![int(1), int(40)]), row(vec![int(3), int(5)])];
        let mapping = [3, 0, 2, 0, 1];
        let code = [comparison(1, IntegerComparison::Less, 2), comparison(4, IntegerComparison::Less, 0), GraphSetPredicateOp::And];
        validate(mapping.len(), &code);
        assert_eq!(collect(&left, &right, &code, Some(&mapping), &mut allow).unwrap(), ordinary(&left, &right, &code, Some(&mapping)));
        assert_eq!(required(2, &code, Some(&mapping), &mut allow).unwrap().unwrap().key, (0, 0));
    }

    #[test]
    fn every_index_ordinal_sort_predicate_and_copy_refusal_is_terminal() {
        let left = [row(vec![int(2)]), row(vec![float(1.0)]), row(vec![int(2)])];
        let right = [row(vec![int(3)]), row(vec![float(1.0)]), row(vec![int(2)]), row(vec![float(3.0)])];
        let before = (left.clone(), right.clone());
        let code = [comparison(0, IntegerComparison::LessOrEqual, 1)];
        let mut events = 0;
        let expected = collect(&left, &right, &code, None, &mut |_| {
            events += 1;
            Ok::<_, usize>(())
        }).unwrap();
        for stop in 1..=events {
            let mut seen = 0;
            assert_eq!(collect(&left, &right, &code, None, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            }), Err(stop));
            assert_eq!(seen, stop);
            assert_eq!((&left, &right), (&before.0, &before.1));
        }
        assert_eq!(collect(&left, &right, &code, None, &mut allow).unwrap(), expected);
    }

    #[test]
    fn selective_ranges_compare_against_the_live_unindexed_predicate() {
        for n in [128_i64, 512] {
            let left: Vec<_> = (0..n).map(|_| row(vec![int(n - 2)])).collect();
            let right: Vec<_> = (0..n).rev().map(|key| row(vec![int(key)])).collect();
            let code = [comparison(0, IntegerComparison::Less, 1)];
            let unindexed = [code[0].clone(), GraphSetPredicateOp::Not, GraphSetPredicateOp::Not];
            let measure = |code: &[GraphSetPredicateOp]| {
                let mut work = 0;
                let rows = collect(&left, &right, code, None, &mut |_| {
                    work += 1;
                    Ok::<_, usize>(())
                }).unwrap();
                (rows, work)
            };
            let (indexed, indexed_work) = measure(&code);
            let (ordinary, ordinary_work) = measure(&unindexed);
            assert_eq!(indexed, vec![row(vec![int(n - 2), int(n - 1)]); n as usize]);
            assert_eq!(indexed, ordinary);
            assert!(indexed_work < ordinary_work, "{n}: indexed={indexed_work}, ordinary={ordinary_work}");
        }
    }

    #[test]
    fn public_selected_range_keeps_child_sequence_and_parent_page() {
        let sequence = |name: &str, data: &[i64]| {
            PreparedGraphSet::singleton().unwind(
                name.into(),
                GraphSetValue::List(data.iter().map(|&value| GraphSetValue::Value(int(value))).collect()),
            ).unwrap()
        };
        let query = sequence("l", &[3, 1, 3]).cross_join(sequence("r", &[2, 4, 2])).unwrap()
            .filter(&[comparison(0, IntegerComparison::Less, 1)]).unwrap().with_page(1, Some(3));
        let definition = query.canonical_bytes();
        let result = query.execute_governed(
            GqlQueryPolicy::new(0, 3, 100_000, 100_000),
            |_, _| -> Result<_, GqlQueryError<usize, usize>> { panic!("source-free range") },
            || Ok::<_, usize>(()),
        ).unwrap();
        assert_eq!(result.value, vec![row(vec![int(1), int(2)]), row(vec![int(1), int(4)]), row(vec![int(1), int(2)])]);
        assert_eq!(query.canonical_bytes(), definition);
    }

    fn counted(
        left: &[GraphValueRow],
        right: &[GraphValueRow],
        code: &[GraphSetPredicateOp],
        mapping: Option<&[usize]>,
    ) -> u128 {
        let mut total = 0_u128;
        super::super::count_with_context(
            left, right, code, mapping, &mut total,
            |_, _| Ok::<_, usize>(()),
            |amount, total| { *total += amount as u128; Ok(()) },
        ).unwrap();
        total
    }

    #[test]
    fn complete_bands_count_intersections_and_leave_residuals_to_the_native_predicate() {
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        let decimal = GraphValue::Scalar(CanonicalScalar::Decimal(
            CanonicalDecimal::from_integer(2).unwrap(),
        ));
        let left = [
            row(vec![int(1), float(3.0)]), row(vec![float(2.0), int(2)]),
            row(vec![int(3), int(1)]), row(vec![null.clone(), int(3)]),
            row(vec![decimal.clone(), int(3)]), row(vec![decimal.clone(), decimal.clone()]),
        ];
        let right = [
            row(vec![int(3)]), row(vec![float(2.0)]), row(vec![int(1)]),
            row(vec![int(2)]), row(vec![null]), row(vec![decimal]),
        ];
        // r, upper, lower, r: repeated projected cells and reversed operands.
        let mapping = [2, 1, 0, 2];
        for first in OPS {
            for second in OPS {
                let code = [comparison(0, first, 2), comparison(1, second, 3), GraphSetPredicateOp::And];
                validate(mapping.len(), &code);
                assert!(complete_band(2, &code, Some(&mapping), &mut allow).unwrap().is_some());
                assert_eq!(counted(&left, &right, &code, Some(&mapping)), ordinary(&left, &right, &code, Some(&mapping)).len() as u128);
            }
        }
        let less = comparison(0, IntegerComparison::Less, 2);
        for code in [
            vec![less.clone(), GraphSetPredicateOp::Truth(None), GraphSetPredicateOp::And],
            vec![less.clone(), GraphSetPredicateOp::Truth(Some(true)), GraphSetPredicateOp::Or],
            vec![less.clone(), GraphSetPredicateOp::Not],
            vec![less, comparison(0, IntegerComparison::Less, 1), GraphSetPredicateOp::And],
        ] {
            validate(3, &code);
            let mut deliveries = 0;
            assert!(!try_count(
                &left, &right, &code, None, &mut deliveries,
                &mut |_, _| Ok::<_, usize>(()),
                &mut |_, deliveries| { *deliveries += 1; Ok(()) },
            ).unwrap());
            assert_eq!(deliveries, 0, "ineligible counting cannot publish a partial count");
            assert_eq!(counted(&left, &right, &code, None), ordinary(&left, &right, &code, None).len() as u128);
        }
        let different_right_columns = [
            comparison(0, IntegerComparison::Less, 2),
            comparison(1, IntegerComparison::Greater, 3),
            GraphSetPredicateOp::And,
        ];
        validate(4, &different_right_columns);
        assert!(complete_band(2, &different_right_columns, None, &mut allow).unwrap().is_none());
    }

    #[test]
    fn exact_range_count_contributes_once_per_left_row_not_per_pair() {
        let left = vec![row(vec![int(0)]); 10_000];
        let right = vec![row(vec![float(1.0)]); 10_000];
        let mut context = (0_usize, 0_usize, 0_usize, 0_u128);
        super::super::count_with_context(
            &left, &right, &[comparison(0, IntegerComparison::Less, 1)], None,
            &mut context,
            |context, event| {
                context.0 += 1;
                context.1 += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, usize>(())
            },
            |amount, context| {
                context.2 += 1;
                context.3 += amount as u128;
                Ok(())
            },
        ).unwrap();
        assert_eq!(context.3, 100_000_000);
        assert_eq!(context.2, left.len());
        assert!(context.1 < 3 * right.len(), "no per-pair scratch: {context:?}");
        assert!(context.0 < 200 * (left.len() + right.len()), "{context:?}");
    }

    #[test]
    fn public_count_aggregate_and_zero_page_preserve_completed_children() {
        use crate::{GraphAggregate, PreparedGraphSetAggregate};
        let sequence = |name: &str, data: &[i64]| {
            PreparedGraphSet::singleton().unwind(
                name.into(),
                GraphSetValue::List(data.iter().map(|&value| GraphSetValue::Value(int(value))).collect()),
            ).unwrap()
        };
        let no_source = |_: &crate::algebra::PreparedGraphPattern<GraphValueRow>, _: GqlQueryPolicy|
            -> Result<crate::GqlQueryExecution<GraphValueRow>, GqlQueryError<usize, usize>> {
            panic!("source-free range aggregate")
        };
        let allowance = GqlQueryPolicy::new(0, 1, 100_000, 5_000);
        let input = sequence("l", &[0; 128]).cross_join(sequence("r", &[1; 128])).unwrap()
            .filter(&[comparison(0, IntegerComparison::Less, 1)]).unwrap();
        let summary = PreparedGraphSetAggregate::prepare(
            input.clone(), &[], &[GraphAggregate::count_rows("n")], 0, None,
        ).unwrap();
        let result = summary.execute_governed(allowance, no_source, || Ok::<_, usize>(())).unwrap();
        assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(16_384));
        assert_eq!(result.rows.result_rows, 1);
        assert!(result.evaluator.scratch_entries < 5_000);
        assert_eq!(input.with_page(7, Some(5)).count_governed(
            allowance, no_source, || Ok::<_, usize>(()),
        ).unwrap().0, Some(5));
        // No empty result may hide the right child's data exception.
        let bad = sequence("r", &[1]).project(
            vec![GraphSetProjection::new("bad", GraphSetValue::Size(Box::new(GraphSetValue::Column(0))))],
            GraphSetQuantifier::All,
        ).unwrap();
        let empty = sequence("l", &[0]).with_page(0, Some(0)).cross_join(bad).unwrap()
            .filter(&[comparison(0, IntegerComparison::Less, 1)]).unwrap().with_page(0, Some(0));
        assert!(matches!(
            empty.count_governed(allowance, no_source, || Ok::<_, usize>(())),
            Err(GqlQueryError::Source(GraphSetExecutionError::Projection { .. }))
        ));
    }

    #[test]
    fn band_count_refusals_and_consumer_errors_never_restart_pair_enumeration() {
        let left = [row(vec![int(0), int(3)]), row(vec![int(1), int(2)])];
        let right = [row(vec![int(1)]), row(vec![int(2)]), row(vec![int(3)]), row(vec![int(2)])];
        let before = (left.clone(), right.clone());
        let code = [comparison(0, IntegerComparison::LessOrEqual, 2), comparison(1, IntegerComparison::Greater, 2), GraphSetPredicateOp::And];
        let attempt = |stop: Option<usize>, reject: bool| -> Result<(usize, u128), usize> {
            let mut state = (0, 0);
            super::super::count_with_context(
                &left, &right, &code, None, &mut state,
                |state, _| {
                    state.0 += 1;
                    if stop == Some(state.0) { Err(state.0) } else { Ok(()) }
                },
                |amount, state| {
                    if reject { return Err(usize::MAX); }
                    state.1 += amount as u128;
                    Ok(())
                },
            )?;
            Ok(state)
        };
        let expected = attempt(None, false).unwrap();
        assert_eq!(expected.1, 4);
        for stop in 1..=expected.0 {
            assert_eq!(attempt(Some(stop), false), Err(stop));
            assert_eq!((&left, &right), (&before.0, &before.1));
        }
        assert_eq!(attempt(None, true), Err(usize::MAX));
        assert_eq!(attempt(None, false).unwrap(), expected);
    }
}
