//! Checked three-valued HAVING over borrowed completed groups.
//!
//! This is a scalar result-stage program, not a second graph executor. Every
//! leaf is evaluated in order; neither Boolean decisions nor pagination hide
//! invalid numeric operands. Counts, integer sums and exact-average fractions
//! share overflow-free rational comparisons without float conversion. Other
//! scalars compare only within their canonical kind. Null and incompatible
//! nonnumeric kinds are UNKNOWN.

use super::*;
use crate::algebra::ScalarPredicate;
use std::sync::Arc;

pub const MAX_HAVING_INSTRUCTIONS: usize = 1024;

#[derive(Clone, PartialEq, Eq)]
pub enum GraphHavingOperand {
    Column(GraphAggregateColumn),
    Integer(i128),
    /// Already checked, immutable literal; its encoded payload is shared.
    Scalar(ScalarPredicate),
}

#[derive(Clone, PartialEq, Eq)]
pub enum GraphHavingOp {
    Compare {
        left: GraphHavingOperand,
        comparison: IntegerComparison,
        right: GraphHavingOperand,
    },
    IsNull { operand: GraphHavingOperand, is_null: bool },
    Truth(Option<bool>),
    And,
    Or,
    Not,
}

impl core::fmt::Debug for GraphHavingOperand {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphHavingOperand([REDACTED])")
    }
}
impl core::fmt::Debug for GraphHavingOp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphHavingOp([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphHavingError {
    Empty,
    TooManyInstructions { limit: usize, observed: usize },
    TooManyPredicates { limit: usize, observed: usize },
    InvalidStack { instruction: usize },
    UnknownColumn { column: GraphAggregateColumn },
}
impl core::fmt::Display for GraphHavingError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "invalid HAVING expression: {self:?}")
    }
}
impl core::error::Error for GraphHavingError {}

/// Immutable postfix program, validated before any group can execute it.
/// Literal encodings are already checked by ScalarPredicate and remain shared.
#[derive(Clone, PartialEq, Eq)]
pub struct GraphHavingExpression {
    program: Arc<[GraphHavingOp]>,
}
impl core::fmt::Debug for GraphHavingExpression {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphHavingExpression")
            .field("instructions", &self.program.len())
            .field("definition", &"[REDACTED]").finish()
    }
}

impl GraphHavingExpression {
    pub fn prepare(program: &[GraphHavingOp]) -> Result<Self, GraphHavingError> {
        if program.is_empty() { return Err(GraphHavingError::Empty); }
        if program.len() > MAX_HAVING_INSTRUCTIONS {
            return Err(GraphHavingError::TooManyInstructions {
                limit: MAX_HAVING_INSTRUCTIONS, observed: program.len(),
            });
        }
        let mut depth = 0;
        let mut leaves = 0;
        for (at, op) in program.iter().enumerate() {
            match op {
                GraphHavingOp::And | GraphHavingOp::Or if depth >= 2 => depth -= 1,
                GraphHavingOp::Not if depth >= 1 => {}
                GraphHavingOp::Compare { .. } | GraphHavingOp::IsNull { .. }
                | GraphHavingOp::Truth(_) => {
                    depth += 1;
                    leaves += 1;
                    if leaves > MAX_AGGREGATE_FILTERS {
                        return Err(GraphHavingError::TooManyPredicates {
                            limit: MAX_AGGREGATE_FILTERS, observed: leaves,
                        });
                    }
                }
                _ => return Err(GraphHavingError::InvalidStack { instruction: at }),
            }
        }
        if depth != 1 {
            return Err(GraphHavingError::InvalidStack { instruction: program.len() });
        }
        let normalized = program.iter().map(|op| match op {
            GraphHavingOp::Compare { left, comparison, right } => GraphHavingOp::Compare {
                left: normalize(left), comparison: *comparison, right: normalize(right),
            },
            GraphHavingOp::IsNull { operand, is_null } => GraphHavingOp::IsNull {
                operand: normalize(operand), is_null: *is_null,
            },
            op => op.clone(),
        }).collect::<Vec<_>>();
        Ok(Self { program: normalized.into() })
    }

    pub(super) fn validate_columns(&self, keys: usize, aggregates: usize) -> Result<(), GraphHavingError> {
        let validate = |operand: &GraphHavingOperand| {
            if let GraphHavingOperand::Column(column) = operand {
                let valid = match column {
                    GraphAggregateColumn::GroupKey(at) => *at < keys,
                    GraphAggregateColumn::Aggregate(at) => *at < aggregates,
                };
                if !valid { return Err(GraphHavingError::UnknownColumn { column: *column }); }
            }
            Ok(())
        };
        for op in self.program.iter() {
            match op {
                GraphHavingOp::Compare { left, right, .. } => { validate(left)?; validate(right)?; }
                GraphHavingOp::IsNull { operand, .. } => validate(operand)?,
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) fn evaluate<E, C>(
        &self,
        group: Group<'_, '_>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<bool, GqlQueryError<GraphAggregateError<E>, C>> {
        let mut stack = [None::<bool>; MAX_AGGREGATE_FILTERS];
        let mut depth = 0;
        let mut predicate = 0;
        for op in self.program.iter() {
            control(GlaExecutionEvent::Work)?;
            let value = match op {
                GraphHavingOp::Compare { left, comparison, right } => {
                    let left = resolve(left, group, control)?;
                    let right = resolve(right, group, control)?;
                    let result = compare(left, right, *comparison, predicate, control)?;
                    predicate += 1;
                    result
                }
                GraphHavingOp::IsNull { operand, is_null } => {
                    predicate += 1;
                    Some(resolve(operand, group, control)?.is_null() == *is_null)
                }
                GraphHavingOp::Truth(value) => { predicate += 1; *value }
                GraphHavingOp::Not => { stack[depth - 1] = stack[depth - 1].map(|value| !value); continue; }
                GraphHavingOp::And | GraphHavingOp::Or => {
                    let right = stack[depth - 1];
                    depth -= 1;
                    let left = stack[depth - 1];
                    stack[depth - 1] = match (op, left, right) {
                        (GraphHavingOp::And, Some(false), _) | (GraphHavingOp::And, _, Some(false)) => Some(false),
                        (GraphHavingOp::Or, Some(true), _) | (GraphHavingOp::Or, _, Some(true)) => Some(true),
                        (_, Some(left), Some(right)) => Some(if matches!(op, GraphHavingOp::And) { left && right } else { left || right }),
                        _ => None,
                    };
                    continue;
                }
            };
            stack[depth] = value;
            depth += 1;
        }
        debug_assert_eq!(depth, 1);
        Ok(stack[0] == Some(true))
    }

    pub(crate) fn append_transcript(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(b"fgdb:aggregate-having-expression:v1\0");
        bytes.extend_from_slice(&(self.program.len() as u64).to_be_bytes());
        for op in self.program.iter() {
            match op {
                GraphHavingOp::Compare { left, comparison, right } => {
                    bytes.push(0); append_operand(left, bytes);
                    bytes.push(match comparison {
                        IntegerComparison::Equal => 0, IntegerComparison::NotEqual => 1,
                        IntegerComparison::Greater => 2, IntegerComparison::Less => 3,
                        IntegerComparison::GreaterOrEqual => 4, IntegerComparison::LessOrEqual => 5,
                    });
                    append_operand(right, bytes);
                }
                GraphHavingOp::IsNull { operand, is_null } => {
                    bytes.push(1); append_operand(operand, bytes); bytes.push(u8::from(*is_null));
                }
                GraphHavingOp::Truth(value) => {
                    bytes.push(2); bytes.push(match value { None => 0, Some(false) => 1, Some(true) => 2 });
                }
                GraphHavingOp::And => bytes.push(3),
                GraphHavingOp::Or => bytes.push(4),
                GraphHavingOp::Not => bytes.push(5),
            }
        }
    }
}

fn normalize(operand: &GraphHavingOperand) -> GraphHavingOperand {
    match operand {
        GraphHavingOperand::Scalar(value) => GraphHavingOperand::Scalar(value.with_comparison(IntegerComparison::Equal)),
        value => value.clone(),
    }
}

fn append_operand(operand: &GraphHavingOperand, bytes: &mut Vec<u8>) {
    match operand {
        GraphHavingOperand::Column(column) => {
            let (tag, at) = match column {
                GraphAggregateColumn::GroupKey(at) => (0, *at),
                GraphAggregateColumn::Aggregate(at) => (1, *at),
            };
            bytes.push(tag); bytes.extend_from_slice(&(at as u64).to_be_bytes());
        }
        GraphHavingOperand::Integer(value) => { bytes.push(2); bytes.extend_from_slice(&value.to_be_bytes()); }
        GraphHavingOperand::Scalar(value) => {
            bytes.push(3);
            let encoded = value.canonical_value_bytes();
            bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
            bytes.extend_from_slice(encoded);
        }
    }
}

fn resolve<'source: 'borrow, 'borrow, E>(
    operand: &'borrow GraphHavingOperand,
    group: Group<'_, 'source>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Cell<'borrow>, E> {
    control(GlaExecutionEvent::Work)?;
    Ok(match operand {
        GraphHavingOperand::Column(column) => group.cell(*column),
        GraphHavingOperand::Integer(value) => Cell::Integer(*value),
        GraphHavingOperand::Scalar(value) => Cell::Value(ValueRef::Scalar(value.value())),
    })
}

fn compare<E, C>(
    left: Cell<'_>, right: Cell<'_>, comparison: IntegerComparison, predicate: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
) -> Result<Option<bool>, GqlQueryError<GraphAggregateError<E>, C>> {
    for cell in [left, right] {
        for _ in 0..cell.payload_units() { control(GlaExecutionEvent::Work)?; }
    }
    if left.is_null() || right.is_null() { return Ok(None); }
    let order = match (left.numeric(), right.numeric()) {
        (Some(a), Some(b)) => numeric::compare_ratios(a, b),
        (Some(_), None) | (None, Some(_)) => {
            // Preserve the existing numeric HAVING domain refusal, even under
            // OR TRUE / AND FALSE. Never make adding parentheses hide it.
            return Err(GqlQueryError::Source(GraphAggregateError::NonIntegerHaving { predicate }));
        }
        (None, None) => match (left, right) {
            (Cell::Value(ValueRef::Scalar(a)), Cell::Value(ValueRef::Scalar(b)))
                if core::mem::discriminant(a) == core::mem::discriminant(b) => a.cmp(b),
            (Cell::Value(ValueRef::Vertex(a)), Cell::Value(ValueRef::Vertex(b))) => a.cmp(&b),
            _ => return Ok(None),
        },
    };
    Ok(Some(match comparison {
        IntegerComparison::Equal => order == Ordering::Equal,
        IntegerComparison::NotEqual => order != Ordering::Equal,
        IntegerComparison::Greater => order == Ordering::Greater,
        IntegerComparison::Less => order == Ordering::Less,
        IntegerComparison::GreaterOrEqual => order != Ordering::Less,
        IntegerComparison::LessOrEqual => order != Ordering::Greater,
    }))
}

impl PreparedGraphAggregate {
    /// Replace HAVING only; preserve child, groups, output schema, ORDER BY and
    /// pagination. with_result_clauses subsequently replaces this expression.
    /// No query runs until all referenced output columns have been checked.
    pub fn with_having_expression(mut self, expression: &GraphHavingExpression) -> Result<Self, GraphHavingError> {
        expression.validate_columns(self.keys.len(), self.aggregates.len())?;
        self.having.clear();
        self.having_expression = Some(expression.clone());
        Ok(self)
    }
    #[must_use]
    pub fn having_expression(&self) -> Option<&GraphHavingExpression> { self.having_expression.as_ref() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use GraphAggregateColumn::{Aggregate as A, GroupKey as K};
    use GraphHavingOp as Op;
    use GraphHavingOperand as Arg;
    use crate::algebra::{GraphColumn, GraphPatternBuilder};

    fn cmp(left: Arg, comparison: IntegerComparison, right: Arg) -> Op {
        Op::Compare { left, comparison, right }
    }
    fn expression(ops: &[Op]) -> GraphHavingExpression { GraphHavingExpression::prepare(ops).unwrap() }
    fn evaluate(ops: &[Op], group: Group<'_, '_>) -> bool {
        expression(ops).evaluate(group, &mut |_| Ok::<_, GqlQueryError<GraphAggregateError<()>, ()>>(())).unwrap()
    }
    fn definition(offset: u64, count: Option<u64>) -> PreparedGraphAggregate {
        let mut builder = GraphPatternBuilder::new(); builder.vertex("n").unwrap();
        let input = builder.prepare_values(&[GraphColumn::vertex("owner", "n"),
            GraphColumn::property("value", "n", PropertyKeyId(1))], 0, None).unwrap().with_duplicates();
        PreparedGraphAggregate::prepare(input, &[0], &[GraphAggregate::count_rows("n"),
            GraphAggregate::sum_int("total", 1)], offset, count).unwrap()
    }

    #[test]
    fn postfix_validation_and_complete_three_valued_truth_tables() {
        let group = Group { key: &[], state: &[] };
        let values = [Some(false), None, Some(true)];
        let and = [[0,0,0],[0,1,1],[0,1,2]];
        let or = [[0,1,2],[1,1,2],[2,2,2]];
        for (i, left) in values.iter().copied().enumerate() {
            assert_eq!(evaluate(&[Op::Truth(left), Op::Not], group), left == Some(false));
            for (j, right) in values.iter().copied().enumerate() {
                for (op, expected) in [(Op::And, values[and[i][j]]), (Op::Or, values[or[i][j]])] {
                    assert_eq!(evaluate(&[Op::Truth(left), Op::Truth(right), op.clone()], group), expected == Some(true));
                    assert_eq!(evaluate(&[Op::Truth(left), Op::Truth(right), op, Op::Not], group), expected == Some(false));
                }
            }
        }
        for ops in [vec![], vec![Op::Not], vec![Op::Truth(None),Op::And],
            vec![Op::Truth(None),Op::Truth(None)]] { assert!(GraphHavingExpression::prepare(&ops).is_err()); }
        let mut too_many = vec![Op::Truth(Some(true)); MAX_AGGREGATE_FILTERS+1];
        too_many.extend(vec![Op::And; MAX_AGGREGATE_FILTERS]);
        assert!(matches!(GraphHavingExpression::prepare(&too_many), Err(GraphHavingError::TooManyPredicates { .. })));
        let mut longest = vec![Op::Truth(Some(true))]; longest.extend(vec![Op::Not; MAX_HAVING_INSTRUCTIONS-1]);
        assert!(GraphHavingExpression::prepare(&longest).is_ok());
        longest.push(Op::Not);
        assert!(matches!(GraphHavingExpression::prepare(&longest), Err(GraphHavingError::TooManyInstructions { .. })));
    }

    #[test]
    fn count_sum_and_scalar_integers_compare_without_narrowing() {
        let choices = [i128::MIN, i128::from(i64::MIN), -1, 0, i128::from(i64::MAX),
            i128::from(u64::MAX), i128::MAX];
        for left in choices { for right in choices {
            let state = [Accumulator::Sum { value: left, present: true },
                Accumulator::Sum { value: right, present: true }];
            let group = Group { key: &[], state: &state };
            for (comparison, expected) in [(IntegerComparison::Equal, left==right),
                (IntegerComparison::NotEqual,left!=right),(IntegerComparison::Less,left<right),
                (IntegerComparison::Greater,left>right),(IntegerComparison::LessOrEqual,left<=right),
                (IntegerComparison::GreaterOrEqual,left>=right)] {
                assert_eq!(evaluate(&[cmp(Arg::Column(A(0)),comparison,Arg::Column(A(1)))],group), expected);
                assert_eq!(evaluate(&[cmp(Arg::Integer(left),comparison,Arg::Column(A(1))),Op::Not],group), !expected);
            }
        }}
        let state = [Accumulator::Count(u64::MAX), Accumulator::Sum { value: i128::from(u64::MAX), present: true }];
        assert!(evaluate(&[cmp(Arg::Column(A(0)),IntegerComparison::Equal,Arg::Column(A(1)))], Group { key: &[], state: &state }));
        let scalar = CanonicalScalar::Int(i64::MIN);
        assert!(evaluate(&[cmp(Arg::Column(K(0)),IntegerComparison::Equal,Arg::Integer(i128::from(i64::MIN)))],
            Group { key: &[ValueRef::Scalar(&scalar)], state: &[] }));
    }

    #[test]
    fn null_and_incompatible_values_remain_unknown_under_not() {
        let wanted = ScalarPredicate::new(CanonicalScalar::Bool(true),IntegerComparison::Equal).unwrap();
        for actual in [CanonicalScalar::Null,CanonicalScalar::Bool(false),CanonicalScalar::Bool(true),
            CanonicalScalar::ucs_basic_text("true").unwrap()] {
            let group = Group { key: &[ValueRef::Scalar(&actual)], state: &[] };
            assert_eq!(evaluate(&[cmp(Arg::Column(K(0)),IntegerComparison::Equal,Arg::Scalar(wanted.clone())),Op::Not],group),
                matches!(actual,CanonicalScalar::Bool(false)));
        }
        let missing = [Accumulator::Sum { value: 0, present: false }];
        let group = Group { key: &[], state: &missing };
        assert!(!evaluate(&[cmp(Arg::Column(A(0)),IntegerComparison::NotEqual,Arg::Integer(0)),Op::Not],group));
        assert!(evaluate(&[Op::IsNull { operand: Arg::Column(A(0)), is_null: true }],group));
        assert!(evaluate(&[cmp(Arg::Column(A(0)),IntegerComparison::Equal,Arg::Integer(0)),
            Op::IsNull { operand: Arg::Column(A(0)),is_null:true },Op::Or],group));
    }

    #[test]
    fn schema_and_replacement_are_checked_without_changing_existing_definitions() {
        let base = definition(2,Some(3)); let original = base.canonical_bytes();
        let order = GraphAggregateOrder::descending(A(1));
        let expr = expression(&[cmp(Arg::Column(A(0)),IntegerComparison::Less,Arg::Column(A(1)))]);
        let changed = base.clone().with_result_clauses(&[],&[order]).unwrap().with_having_expression(&expr).unwrap();
        assert!(changed.having().is_empty()); assert_eq!(changed.having_expression(),Some(&expr));
        assert_eq!(changed.ordering(),&[order]); assert_eq!(base.canonical_bytes(),original);
        assert_ne!(changed.canonical_bytes(),original);
        assert_eq!(changed.with_result_clauses(&[],&[]).unwrap().canonical_bytes(),original);
        for column in [K(1),A(2)] {
            let invalid = expression(&[cmp(Arg::Integer(1),IntegerComparison::Equal,Arg::Column(column))]);
            assert!(matches!(base.clone().with_having_expression(&invalid),Err(GraphHavingError::UnknownColumn { .. })));
        }
        let secret = ScalarPredicate::new(CanonicalScalar::ucs_basic_text("private-value").unwrap(),IntegerComparison::Less).unwrap();
        let a = expression(&[Op::IsNull { operand:Arg::Scalar(secret.clone()),is_null:false }]);
        let b = expression(&[Op::IsNull { operand:Arg::Scalar(secret.with_comparison(IntegerComparison::Greater)),is_null:false }]);
        assert_eq!(a,b); assert!(!format!("{a:?}").contains("private-value"));
    }

    #[test]
    fn filtering_precedes_pages_and_every_checkpoint_without_cloning_payloads() {
        let mut groups = BTreeMap::new();
        for id in 0..8_u128 { groups.insert(vec![ValueRef::Vertex(VId(id))],vec![
            Accumulator::Count(2),Accumulator::Sum {value:id as i128,present:id!=7}]); }
        let expr = expression(&[cmp(Arg::Column(A(1)),IntegerComparison::Greater,Arg::Column(A(0))),
            Op::IsNull {operand:Arg::Column(A(1)),is_null:true},Op::Or]);
        for ordered in [false,true] {
            let mut query = definition(1,Some(2));
            if ordered {query=query.with_result_clauses(&[],&[GraphAggregateOrder::descending(A(1))]).unwrap();}
            query=query.with_having_expression(&expr).unwrap();
            let mut calls=0;
            let measured=query.finish_groups(&groups,&mut |_| {calls+=1;Ok::<_,GqlQueryError<GraphAggregateError<()>,usize>>(())}).unwrap();
            let ids:Vec<_>=measured.iter().map(|row|row.keys()[0].as_vertex().unwrap()).collect();
            assert_eq!(ids,if ordered {vec![VId(5),VId(4)]} else {vec![VId(4),VId(5)]});
            for stop in 1..=calls {
                let mut at=0;
                let result=query.finish_groups(&groups,&mut |_| {at+=1;if at==stop {Err(GqlQueryError::<GraphAggregateError<()>,_>::Interrupted(stop))}else{Ok(())}});
                assert!(matches!(result,Err(GqlQueryError::Interrupted(value)) if value==stop));assert_eq!(at,stop);
            }
        }
        let payload=CanonicalScalar::bytes(vec![7;8192]).unwrap();
        let scalar=ScalarPredicate::new(payload.clone(),IntegerComparison::Equal).unwrap();
        let expr=expression(&[cmp(Arg::Column(K(0)),IntegerComparison::Equal,Arg::Scalar(scalar))]);
        let mut work=0;
        assert!(expr.evaluate(Group {key:&[ValueRef::Scalar(&payload)],state:&[]},&mut |event| {
            assert_eq!(event,GlaExecutionEvent::Work);work+=1;Ok::<_,GqlQueryError<GraphAggregateError<()>,()>>(())
        }).unwrap());
        assert!(work>=2*8192/GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
    }

    #[test]
    fn decisive_truth_and_zero_pages_cannot_hide_invalid_numeric_domains() {
        let boolean=CanonicalScalar::Bool(true);
        let mut groups=BTreeMap::new();groups.insert(vec![ValueRef::Scalar(&boolean)],vec![Accumulator::Count(0),Accumulator::Extreme(None)]);
        for (truth,op) in [(false,Op::And),(true,Op::Or)] {
            let expr=expression(&[Op::Truth(Some(truth)),cmp(Arg::Column(K(0)),IntegerComparison::Equal,Arg::Integer(1)),op]);
            for count in [None,Some(0)] { for ordered in [false,true] {
                let query=definition(u64::MAX,count).with_result_clauses(&[],
                    &if ordered {vec![GraphAggregateOrder::ascending(A(0))]}else{vec![]}).unwrap().with_having_expression(&expr).unwrap();
                let result=query.finish_groups(&groups,&mut |_| Ok::<_,GqlQueryError<GraphAggregateError<()>,()>>(()));
                assert!(matches!(result,Err(GqlQueryError::Source(GraphAggregateError::NonIntegerHaving {predicate:1}))));
            }}
        }
    }
}
