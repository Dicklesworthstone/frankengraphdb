//! Numeric scalar dispatch for the existing bytecode evaluator. Integer-only
//! and exact aggregate operands continue through its checked integer path.
//! Float arithmetic is finite binary64; its bit kernel has constant bounded
//! work and no payload allocation. The enclosing instruction owns admission.

use super::{ExpressionCell, GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerUnary};
use fgdb_types::{CanonicalF64, CanonicalScalar, FloatArithmeticError};

fn error(error: FloatArithmeticError) -> GraphIntegerErrorKind {
    match error {
        FloatArithmeticError::DivisionByZero => GraphIntegerErrorKind::DivisionByZero,
        // A non-finite operand is outside this finite numeric domain, like an
        // overflowing result. Both are data exceptions, never WHERE UNKNOWN.
        FloatArithmeticError::NonFinite | FloatArithmeticError::Overflow => {
            GraphIntegerErrorKind::Overflow
        }
    }
}

fn float(cell: &ExpressionCell<'_>) -> Option<CanonicalF64> {
    match cell {
        ExpressionCell::Scalar(value) => match value.as_ref() {
            CanonicalScalar::Float(value) => Some(*value),
            _ => None,
        },
        _ => None,
    }
}

fn operand(cell: &ExpressionCell<'_>) -> Result<Option<CanonicalF64>, GraphIntegerErrorKind> {
    match cell {
        ExpressionCell::Scalar(value) => match value.as_ref() {
            CanonicalScalar::Null => Ok(None),
            CanonicalScalar::Float(value) => Ok(Some(*value)),
            CanonicalScalar::Int(value) => Ok(Some(CanonicalF64::from_i64_rounded(*value))),
            _ => Err(GraphIntegerErrorKind::IncompatibleOperands),
        },
        // Count, wide sums and exact averages are not ordinary scalar Ints.
        // Reject mixing them with Float rather than silently losing precision.
        _ => Err(GraphIntegerErrorKind::IncompatibleOperands),
    }
}

pub(super) fn check(cell: &ExpressionCell<'_>) -> Result<(), GraphIntegerErrorKind> {
    if float(cell).is_some() { Ok(()) } else { cell.integer().map(|_| ()) }
}

pub(super) fn unary(
    op: GraphIntegerUnary,
    cell: &ExpressionCell<'_>,
) -> Result<Option<CanonicalScalar>, GraphIntegerErrorKind> {
    let Some(value) = float(cell) else { return Ok(None) };
    let result = match op {
        GraphIntegerUnary::Plus => value.checked_add(CanonicalF64::from_i64_rounded(0)),
        GraphIntegerUnary::Negate => value.checked_neg(),
        GraphIntegerUnary::Abs => value.checked_abs(),
    };
    result.map(|value| Some(CanonicalScalar::Float(value))).map_err(error)
}

pub(super) fn binary(
    op: GraphIntegerBinary,
    left: &ExpressionCell<'_>,
    right: &ExpressionCell<'_>,
) -> Result<Option<CanonicalScalar>, GraphIntegerErrorKind> {
    if float(left).is_none() && float(right).is_none() {
        return Ok(None);
    }
    let (left, right) = (operand(left)?, operand(right)?);
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(Some(CanonicalScalar::Null));
    };
    let result = match op {
        GraphIntegerBinary::Add => left.checked_add(right),
        GraphIntegerBinary::Subtract => left.checked_sub(right),
        GraphIntegerBinary::Multiply => left.checked_mul(right),
        GraphIntegerBinary::Divide => left.checked_div(right),
        GraphIntegerBinary::Remainder => left.checked_rem(right),
        GraphIntegerBinary::NullIf => unreachable!("NULLIF uses exact comparison before dispatch"),
    };
    result.map(|value| Some(CanonicalScalar::Float(value))).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphValue, IntegerComparison, ScalarPredicate};
    use crate::{GraphIntegerEvaluationError, GraphIntegerExpression, GraphIntegerOp as Op};

    fn f(value: f64) -> CanonicalScalar {
        CanonicalScalar::Float(CanonicalF64::new(value))
    }
    fn literal(value: CanonicalScalar) -> Op {
        Op::Scalar(ScalarPredicate::new(value, IntegerComparison::Equal).unwrap())
    }
    fn eval(ops: &[Op], row: &[GraphValue]) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
        GraphIntegerExpression::prepare_scalar(ops).unwrap()
            .evaluate_scalar_with_control(row, &mut |_| Ok::<_, ()>(()))
            .map_err(|error| match error {
                GraphIntegerEvaluationError::Value(error) => error.kind,
                GraphIntegerEvaluationError::Control(()) => unreachable!(),
            })
    }

    #[test]
    fn literals_and_dynamic_properties_share_finite_numeric_semantics() {
        for (op, expected) in [
            (GraphIntegerBinary::Add, 7.5),
            (GraphIntegerBinary::Subtract, 3.5),
            (GraphIntegerBinary::Multiply, 11.0),
            (GraphIntegerBinary::Divide, 2.75),
            (GraphIntegerBinary::Remainder, 1.5),
        ] {
            assert_eq!(eval(&[literal(f(5.5)), Op::Literal(Some(2)), Op::Binary(op)], &[]), Ok(f(expected)));
            assert_eq!(eval(&[Op::ScalarColumn(0), Op::ScalarColumn(1), Op::Binary(op)],
                &[GraphValue::Scalar(f(5.5)), GraphValue::Scalar(CanonicalScalar::Int(2))]), Ok(f(expected)));
        }
        for (op, expected) in [
            (GraphIntegerUnary::Plus, -2.5),
            (GraphIntegerUnary::Negate, 2.5),
            (GraphIntegerUnary::Abs, 2.5),
        ] {
            assert_eq!(eval(&[Op::ScalarColumn(0), Op::Unary(op)], &[GraphValue::Scalar(f(-2.5))]), Ok(f(expected)));
        }
        // NULLIF is equality, not approximate arithmetic. Large integers do
        // not round into equality with a neighboring representable float.
        assert_eq!(eval(&[Op::Literal(Some(9_007_199_254_740_993)),
            literal(f(9_007_199_254_740_992.0)), Op::Binary(GraphIntegerBinary::NullIf)], &[]),
            Ok(CanonicalScalar::Int(9_007_199_254_740_993)));
    }

    #[test]
    fn integer_only_and_exact_aggregate_domains_do_not_become_float() {
        let ops = [Op::Literal(Some(5)), Op::Literal(Some(2)), Op::Binary(GraphIntegerBinary::Divide)];
        assert_eq!(eval(&ops, &[]), Ok(CanonicalScalar::Int(2)));
        assert_eq!(GraphIntegerExpression::prepare(&ops).unwrap().canonical_bytes(),
            GraphIntegerExpression::prepare_scalar(&ops).unwrap().canonical_bytes());
        let ops = [Op::ScalarColumn(0), Op::Literal(Some(1)), Op::Binary(GraphIntegerBinary::Add)];
        let scalar = GraphIntegerExpression::prepare_scalar(&ops).unwrap();
        assert_ne!(scalar.canonical_bytes(), GraphIntegerExpression::prepare(&ops).unwrap().canonical_bytes());
        assert_eq!(eval(&ops, &[GraphValue::Scalar(CanonicalScalar::Int(i64::MAX))]), Err(GraphIntegerErrorKind::Overflow));
        let loaded = scalar.evaluate_loaded_with_control(1,
            |_| Ok(ExpressionCell::Integer(i128::from(i64::MAX))), &mut |_| Ok::<_, ()>(())).unwrap();
        assert!(matches!(loaded, ExpressionCell::Integer(value) if value == i128::from(i64::MAX) + 1));
        let expression = GraphIntegerExpression::prepare_scalar(&[
            Op::ScalarColumn(0), literal(f(1.0)), Op::Binary(GraphIntegerBinary::Add),
        ]).unwrap();
        let refused = expression.evaluate_loaded_with_control(1,
            |_| Ok(ExpressionCell::Integer(i128::MAX)), &mut |_| Ok::<_, ()>(()));
        assert!(matches!(refused, Err(GraphIntegerEvaluationError::Value(error))
            if error.kind == GraphIntegerErrorKind::IncompatibleOperands));
    }

    #[test]
    fn lazy_branches_preserve_dynamic_float_inputs_and_skip_exceptions() {
        let ops = [Op::ScalarColumn(0), Op::Literal(Some(0)), Op::Coalesce,
            Op::Literal(Some(2)), Op::Binary(GraphIntegerBinary::Multiply)];
        assert_eq!(eval(&ops, &[GraphValue::Scalar(f(1.25))]), Ok(f(2.5)));
        assert_eq!(eval(&ops, &[GraphValue::Scalar(CanonicalScalar::Null)]), Ok(CanonicalScalar::Int(0)));
        let ops = [Op::Truth(Some(true)), literal(f(2.5)), literal(f(1.0)),
            literal(f(0.0)), Op::Binary(GraphIntegerBinary::Divide), Op::Case];
        assert_eq!(eval(&ops, &[]), Ok(f(2.5)));
        let ops = [Op::Literal(None), literal(f(0.0)), Op::Binary(GraphIntegerBinary::Divide)];
        assert_eq!(eval(&ops, &[]), Ok(CanonicalScalar::Null));
        for (left, right, expected) in [
            (1.0, 0.0, GraphIntegerErrorKind::DivisionByZero),
            (f64::MAX, 0.5, GraphIntegerErrorKind::Overflow),
            (f64::INFINITY, 1.0, GraphIntegerErrorKind::Overflow),
            (f64::NAN, 1.0, GraphIntegerErrorKind::Overflow),
        ] {
            assert_eq!(eval(&[literal(f(left)), literal(f(right)), Op::Binary(GraphIntegerBinary::Divide)], &[]), Err(expected));
            assert!(expected.is_arithmetic_exception());
        }
    }

    #[test]
    fn each_numeric_frame_and_instruction_checkpoint_remains_cancellable() {
        let expression = GraphIntegerExpression::prepare_scalar(&[
            Op::ScalarColumn(0), literal(f(2.0)), Op::Binary(GraphIntegerBinary::Remainder),
            Op::Unary(GraphIntegerUnary::Negate),
        ]).unwrap();
        let row = [GraphValue::Scalar(f(5.5))];
        let frozen = expression.canonical_bytes();
        let mut events = 0;
        assert_eq!(expression.evaluate_scalar_with_control(&row, &mut |_| {
            events += 1; Ok::<_, usize>(())
        }).unwrap(), f(-1.5));
        for stop in 1..=events {
            let mut seen = 0;
            let result = expression.evaluate_scalar_with_control(&row, &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GraphIntegerEvaluationError::Control(at)) if at == stop));
            assert_eq!(seen, stop);
            assert_eq!(expression.canonical_bytes(), frozen);
        }
    }
}
