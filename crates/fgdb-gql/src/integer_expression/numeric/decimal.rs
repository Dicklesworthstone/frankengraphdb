//! Decimal dispatch for the existing scalar bytecode, not a second evaluator.
//! Decimal operands stay in their own exact domain: neither ordinary Int/Float
//! nor count/wide-sum/average result cells are implicitly converted to Decimal.

use super::{ExpressionCell, GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerUnary};
use fgdb_types::{CanonicalDecimal, CanonicalScalar, DecimalError};

pub(super) fn get(cell: &ExpressionCell<'_>) -> Option<CanonicalDecimal> {
    match cell {
        ExpressionCell::Scalar(value) => match value.as_ref() {
            CanonicalScalar::Decimal(value) => Some(*value),
            _ => None,
        },
        _ => None,
    }
}

fn operand(cell: &ExpressionCell<'_>) -> Result<Option<CanonicalDecimal>, GraphIntegerErrorKind> {
    match cell {
        ExpressionCell::Scalar(value) => match value.as_ref() {
            CanonicalScalar::Null => Ok(None),
            CanonicalScalar::Decimal(value) => Ok(Some(*value)),
            _ => Err(GraphIntegerErrorKind::IncompatibleOperands),
        },
        _ => Err(GraphIntegerErrorKind::IncompatibleOperands),
    }
}

fn error(error: DecimalError) -> GraphIntegerErrorKind {
    match error {
        DecimalError::DivisionByZero => GraphIntegerErrorKind::DivisionByZero,
        // Native coefficient/range refusals are arithmetic data exceptions,
        // never an approximate fallback or a WHERE UNKNOWN result.
        _ => GraphIntegerErrorKind::Overflow,
    }
}

pub(super) fn unary(
    op: GraphIntegerUnary,
    value: CanonicalDecimal,
) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
    let result = match op {
        GraphIntegerUnary::Plus => Ok(value),
        GraphIntegerUnary::Negate => value.checked_neg(),
        GraphIntegerUnary::Abs if value.coefficient().is_negative() => value.checked_neg(),
        GraphIntegerUnary::Abs => Ok(value),
    };
    result.map(CanonicalScalar::Decimal).map_err(error)
}

pub(super) fn binary(
    op: GraphIntegerBinary,
    left: &ExpressionCell<'_>,
    right: &ExpressionCell<'_>,
) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
    let (left, right) = (operand(left)?, operand(right)?);
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(CanonicalScalar::Null);
    };
    let result = match op {
        GraphIntegerBinary::Add => left.checked_add(right),
        GraphIntegerBinary::Subtract => left.checked_sub(right),
        GraphIntegerBinary::Multiply => left.checked_mul(right),
        GraphIntegerBinary::Divide => left.checked_div(right),
        GraphIntegerBinary::Remainder => left.checked_rem(right),
        GraphIntegerBinary::NullIf => unreachable!("NULLIF compares before numeric dispatch"),
    };
    result.map(CanonicalScalar::Decimal).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphValue, IntegerComparison, ScalarPredicate};
    use crate::{GraphIntegerEvaluationError, GraphIntegerExpression, GraphIntegerOp as Op};
    use fgdb_types::{CanonicalF64, MAX_DECIMAL_COEFFICIENT};

    const UNIT: i128 = 1_000_000_000_000_000_000;

    fn d(coefficient: i128) -> CanonicalScalar {
        CanonicalScalar::Decimal(CanonicalDecimal::from_coefficient(coefficient).unwrap())
    }
    fn literal(coefficient: i128) -> Op {
        Op::Scalar(ScalarPredicate::new(d(coefficient), IntegerComparison::Equal).unwrap())
    }
    fn eval(ops: &[Op], row: &[GraphValue]) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
        GraphIntegerExpression::prepare_scalar(ops)
            .unwrap()
            .evaluate_scalar_with_control(row, &mut |_| Ok::<_, ()>(()))
            .map_err(|error| match error {
                GraphIntegerEvaluationError::Value(error) => error.kind,
                GraphIntegerEvaluationError::Control(()) => unreachable!(),
            })
    }

    #[test]
    fn literal_and_property_operands_use_the_same_decimal_operations() {
        for (op, expected) in [
            (GraphIntegerBinary::Add, 7_500_000_000_000_000_000),
            (GraphIntegerBinary::Subtract, 3_500_000_000_000_000_000),
            (GraphIntegerBinary::Multiply, 11_000_000_000_000_000_000),
            (GraphIntegerBinary::Divide, 2_750_000_000_000_000_000),
            (GraphIntegerBinary::Remainder, 1_500_000_000_000_000_000),
        ] {
            assert_eq!(
                eval(
                    &[
                        literal(5_500_000_000_000_000_000),
                        literal(2 * UNIT),
                        Op::Binary(op),
                    ],
                    &[],
                ),
                Ok(d(expected))
            );
            assert_eq!(
                eval(
                    &[Op::ScalarColumn(0), Op::ScalarColumn(1), Op::Binary(op)],
                    &[
                        GraphValue::Scalar(d(5_500_000_000_000_000_000)),
                        GraphValue::Scalar(d(2 * UNIT)),
                    ],
                ),
                Ok(d(expected))
            );
        }
        for (op, expected) in [
            (GraphIntegerUnary::Plus, -2 * UNIT),
            (GraphIntegerUnary::Negate, 2 * UNIT),
            (GraphIntegerUnary::Abs, 2 * UNIT),
        ] {
            assert_eq!(
                eval(&[literal(-2 * UNIT), Op::Unary(op)], &[]),
                Ok(d(expected))
            );
        }
        assert_eq!(
            eval(
                &[
                    literal(MAX_DECIMAL_COEFFICIENT),
                    literal(UNIT),
                    Op::Binary(GraphIntegerBinary::Multiply),
                ],
                &[],
            ),
            Ok(d(MAX_DECIMAL_COEFFICIENT))
        );
        // An operation over two dynamic properties must retain its dynamic
        // result domain; it cannot masquerade as a known Int in a comparison.
        assert_eq!(
            eval(
                &[
                    Op::ScalarColumn(0),
                    Op::ScalarColumn(1),
                    Op::Binary(GraphIntegerBinary::Divide),
                    literal(2 * UNIT),
                    Op::Compare(IntegerComparison::Greater),
                ],
                &[
                    GraphValue::Scalar(d(5 * UNIT)),
                    GraphValue::Scalar(d(2 * UNIT))
                ],
            ),
            Ok(CanonicalScalar::Bool(true))
        );
    }

    #[test]
    fn decimal_parameters_and_lazy_results_keep_their_exact_domain() {
        let expression = [
            Op::ScalarColumn(0),
            literal(10 * UNIT),
            Op::Coalesce,
            literal(3 * UNIT),
            Op::Binary(GraphIntegerBinary::Multiply),
        ];
        for (value, expected) in [(d(4 * UNIT), 12 * UNIT), (CanonicalScalar::Null, 30 * UNIT)] {
            assert_eq!(
                eval(&expression, &[GraphValue::Scalar(value)]),
                Ok(d(expected))
            );
        }
        let lazy = [
            Op::Truth(Some(true)),
            literal(7 * UNIT),
            literal(UNIT),
            literal(0),
            Op::Binary(GraphIntegerBinary::Divide),
            Op::Case,
        ];
        assert_eq!(eval(&lazy, &[]), Ok(d(7 * UNIT)));
        for (right, expected) in [(UNIT, CanonicalScalar::Null), (UNIT + 1, d(UNIT))] {
            assert_eq!(
                eval(
                    &[
                        literal(UNIT),
                        literal(right),
                        Op::Binary(GraphIntegerBinary::NullIf)
                    ],
                    &[],
                ),
                Ok(expected)
            );
        }
        for op in [
            GraphIntegerBinary::Add,
            GraphIntegerBinary::Divide,
            GraphIntegerBinary::Remainder,
        ] {
            assert_eq!(
                eval(&[Op::Literal(None), literal(0), Op::Binary(op)], &[]),
                Ok(CanonicalScalar::Null)
            );
            assert_eq!(
                eval(&[literal(0), Op::Literal(None), Op::Binary(op)], &[]),
                Ok(CanonicalScalar::Null)
            );
        }
    }

    #[test]
    fn mixed_numeric_results_do_not_cast_or_hide_a_later_nonnumeric_arm() {
        let float = CanonicalScalar::Float(CanonicalF64::new(3.5));
        for other in [CanonicalScalar::Int(3), float] {
            let operand =
                Op::Scalar(ScalarPredicate::new(other.clone(), IntegerComparison::Equal).unwrap());
            for chosen in [false, true] {
                assert_eq!(
                    eval(
                        &[
                            Op::Truth(Some(chosen)),
                            literal(UNIT + 1),
                            operand.clone(),
                            Op::Case,
                        ],
                        &[],
                    ),
                    Ok(if chosen { d(UNIT + 1) } else { other.clone() })
                );
            }
            assert_eq!(
                eval(&[literal(UNIT + 1), operand.clone(), Op::Coalesce], &[]),
                Ok(d(UNIT + 1))
            );
            assert_eq!(
                eval(&[operand, literal(UNIT + 1), Op::Coalesce], &[]),
                Ok(other)
            );
        }
        // All three result arms are statically checked, even when the first
        // one is selected. A numeric union cannot become an untyped escape.
        let bad = [
            Op::Literal(Some(1)),
            Op::Literal(Some(1)),
            literal(UNIT),
            Op::Literal(Some(2)),
            Op::Literal(Some(2)),
            Op::Truth(Some(true)),
            Op::SimpleCase { alternatives: 2 },
        ];
        assert!(GraphIntegerExpression::prepare_scalar(&bad).is_err());
        assert!(
            GraphIntegerExpression::prepare_scalar(&[
                literal(UNIT),
                Op::Truth(Some(true)),
                Op::Coalesce,
            ])
            .is_err()
        );
    }

    #[test]
    fn coefficient_rounding_and_data_exceptions_survive_scalar_bytecode() {
        for (input, expected) in [(1, 0), (3, 2), (5, 2), (-1, 0), (-3, -2), (-5, -2)] {
            assert_eq!(
                eval(
                    &[
                        literal(input),
                        literal(2 * UNIT),
                        Op::Binary(GraphIntegerBinary::Divide)
                    ],
                    &[],
                ),
                Ok(d(expected))
            );
        }
        for (left, right, op, expected) in [
            (
                UNIT,
                0,
                GraphIntegerBinary::Divide,
                GraphIntegerErrorKind::DivisionByZero,
            ),
            (
                UNIT,
                0,
                GraphIntegerBinary::Remainder,
                GraphIntegerErrorKind::DivisionByZero,
            ),
            (
                MAX_DECIMAL_COEFFICIENT,
                1,
                GraphIntegerBinary::Add,
                GraphIntegerErrorKind::Overflow,
            ),
            (
                -MAX_DECIMAL_COEFFICIENT,
                1,
                GraphIntegerBinary::Subtract,
                GraphIntegerErrorKind::Overflow,
            ),
            (
                MAX_DECIMAL_COEFFICIENT,
                2 * UNIT,
                GraphIntegerBinary::Multiply,
                GraphIntegerErrorKind::Overflow,
            ),
        ] {
            assert_eq!(
                eval(&[literal(left), literal(right), Op::Binary(op)], &[]),
                Err(expected)
            );
            assert!(expected.is_arithmetic_exception());
        }
    }

    #[test]
    fn integers_floats_and_exact_aggregates_are_not_implicit_decimal_casts() {
        let ops = [
            Op::ScalarColumn(0),
            Op::ScalarColumn(1),
            Op::Binary(GraphIntegerBinary::Multiply),
        ];
        for other in [
            CanonicalScalar::Int(2),
            CanonicalScalar::Float(CanonicalF64::new(2.0)),
            CanonicalScalar::Bool(true),
            CanonicalScalar::ucs_basic_text("2").unwrap(),
        ] {
            for reverse in [false, true] {
                let mut row = vec![
                    GraphValue::Scalar(d(UNIT)),
                    GraphValue::Scalar(other.clone()),
                ];
                if reverse {
                    row.reverse();
                }
                assert_eq!(
                    eval(&ops, &row),
                    Err(GraphIntegerErrorKind::IncompatibleOperands)
                );
            }
        }
        let expression = GraphIntegerExpression::prepare_scalar(&[
            Op::ScalarColumn(0),
            literal(UNIT),
            Op::Binary(GraphIntegerBinary::Add),
        ])
        .unwrap();
        assert!(matches!(
            expression.evaluate_loaded_with_control(
                1,
                |_| Ok(ExpressionCell::Integer(i128::MAX)),
                |_| Err(GraphIntegerErrorKind::NonScalar),
                &mut |_| Ok::<_, ()>(()),
            ),
            Err(GraphIntegerEvaluationError::Value(error))
                if error.kind == GraphIntegerErrorKind::IncompatibleOperands
        ));
        for ops in [
            vec![literal(UNIT)],
            vec![literal(UNIT), Op::Unary(GraphIntegerUnary::Negate)],
            vec![
                literal(UNIT),
                literal(UNIT),
                Op::Binary(GraphIntegerBinary::Add),
            ],
        ] {
            assert!(GraphIntegerExpression::prepare(&ops).is_err());
            assert!(GraphIntegerExpression::prepare_scalar(&ops).is_ok());
        }
        let old = [
            Op::Literal(Some(5)),
            Op::Literal(Some(2)),
            Op::Binary(GraphIntegerBinary::Divide),
        ];
        assert_eq!(eval(&old, &[]), Ok(CanonicalScalar::Int(2)));
        assert_eq!(
            GraphIntegerExpression::prepare(&old)
                .unwrap()
                .canonical_bytes(),
            GraphIntegerExpression::prepare_scalar(&old)
                .unwrap()
                .canonical_bytes()
        );
    }

    #[test]
    fn each_instruction_and_frame_charge_can_refuse_without_rewriting_the_plan() {
        let expression = GraphIntegerExpression::prepare_scalar(&[
            Op::ScalarColumn(0),
            literal(UNIT),
            Op::Binary(GraphIntegerBinary::Multiply),
            literal(2 * UNIT),
            Op::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap();
        let row = [GraphValue::Scalar(d(MAX_DECIMAL_COEFFICIENT))];
        let frozen = expression.canonical_bytes();
        let mut calls = 0;
        assert_eq!(
            expression
                .evaluate_scalar_with_control(&row, &mut |_| {
                    calls += 1;
                    Ok::<_, usize>(())
                })
                .unwrap(),
            d(5 * 10_i128.pow(33))
        );
        assert!(calls > 5);
        for stop in 1..=calls {
            let mut seen = 0;
            let result = expression.evaluate_scalar_with_control(&row, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GraphIntegerEvaluationError::Control(at)) if at == stop));
            assert_eq!(seen, stop);
            assert_eq!(expression.canonical_bytes(), frozen);
        }
    }
}
