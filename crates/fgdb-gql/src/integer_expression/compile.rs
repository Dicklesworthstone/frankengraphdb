//! Typed postfix admission and iterative control-flow compilation.
//! CASE branches merge at one scalar stack cell. The compile-time frame bound
//! is the maximum over reachable branches, not a scan through mutually
//! exclusive code. Integer preparation retains its strict root contract.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Null,
    Integer,
    Float,
    Boolean,
    Text,
    Dynamic,
}

impl Kind {
    fn accepts(self, actual: Self) -> bool {
        actual == self || matches!(actual, Self::Null | Self::Dynamic)
    }
    fn merge(self, other: Self) -> Option<Self> {
        if self == other || other == Self::Null {
            Some(self)
        } else if self == Self::Null {
            Some(other)
        } else if self == Self::Dynamic || other == Self::Dynamic {
            Some(Self::Dynamic)
        } else {
            None
        }
    }
    /// Comparison admission does not emit a cast. Result branches still use
    /// merge(); numeric arithmetic has its own admission and bytecode below.
    fn merge_comparison(self, other: Self) -> Option<Self> {
        if matches!(
            (self, other),
            (Self::Integer, Self::Float) | (Self::Float, Self::Integer)
        ) {
            Some(Self::Float)
        } else {
            self.merge(other)
        }
    }
}
struct Node {
    op: GraphIntegerOp,
    children: Vec<usize>,
    kind: Kind,
    peak: usize,
    numeric_arithmetic: bool,
    may_float: bool,
}

pub(super) fn prepare(
    ops: &[GraphIntegerOp],
) -> Result<GraphIntegerExpression, GraphIntegerBuildError> {
    prepare_root(ops, true)
}

pub(super) fn prepare_scalar(
    ops: &[GraphIntegerOp],
) -> Result<GraphIntegerExpression, GraphIntegerBuildError> {
    prepare_root(ops, false)
}

fn prepare_root(
    ops: &[GraphIntegerOp],
    integer_root: bool,
) -> Result<GraphIntegerExpression, GraphIntegerBuildError> {
    use GraphIntegerOp as Op;
    if ops.is_empty() {
        return Err(GraphIntegerBuildError::Empty);
    }
    if ops.len() > MAX_GRAPH_INTEGER_INSTRUCTIONS {
        return Err(GraphIntegerBuildError::TooManyInstructions {
            limit: MAX_GRAPH_INTEGER_INSTRUCTIONS,
            observed: ops.len(),
        });
    }
    let mut nodes: Vec<Node> = Vec::with_capacity(ops.len());
    let mut roots: Vec<usize> = Vec::new();
    for (at, op) in ops.iter().enumerate() {
        let missing = || GraphIntegerBuildError::MissingOperand { instruction: at };
        let wrong = || GraphIntegerBuildError::OperandType { instruction: at };
        let arity = match op {
            Op::Column(_)
            | Op::Literal(_)
            | Op::Truth(_)
            | Op::Scalar(_)
            | Op::ScalarColumn(_)
            | Op::Local(_) => 0,
            Op::Unary(_)
            | Op::IsNull(_)
            | Op::Not
            | Op::Upper
            | Op::Lower
            | Op::Trim
            | Op::CharLength
            | Op::ToText
            | Op::ToInteger
            | Op::Numeric(_)
            | Op::Matches(_) => 1,
            Op::Binary(_)
            | Op::Coalesce
            | Op::Compare(_)
            | Op::And
            | Op::Or
            | Op::Concat
            | Op::StartsWith
            | Op::EndsWith
            | Op::Contains => 2,
            Op::Case | Op::Substring => 3,
            Op::InList { members } => members.checked_add(1).ok_or_else(missing)?,
            Op::SimpleCase { alternatives } => {
                if *alternatives == 0 {
                    return Err(GraphIntegerBuildError::EmptyCase { instruction: at });
                }
                alternatives
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(2))
                    .ok_or_else(missing)?
            }
        };
        if roots.len() < arity {
            return Err(missing());
        }
        let children = roots.split_off(roots.len() - arity);
        let child_kind = |position: usize| nodes[children[position]].kind;
        for (position, &child) in children.iter().enumerate() {
            if !integer_root && matches!(op, Op::Unary(_) | Op::Binary(_)) {
                if !matches!(
                    nodes[child].kind,
                    Kind::Null | Kind::Integer | Kind::Float | Kind::Dynamic
                ) {
                    return Err(wrong());
                }
                continue;
            }
            if matches!(op, Op::Numeric(_)) {
                if !matches!(
                    nodes[child].kind,
                    Kind::Null | Kind::Integer | Kind::Float | Kind::Dynamic
                ) && !(matches!(op, Op::Numeric(GraphNumericFunction::ToFloat))
                    && nodes[child].kind == Kind::Text)
                {
                    return Err(wrong());
                }
                continue;
            }
            let expected = match op {
                Op::Unary(_) | Op::Binary(_) => Some(Kind::Integer),
                Op::Not | Op::And | Op::Or => Some(Kind::Boolean),
                Op::Case if position == 0 => Some(Kind::Boolean),
                Op::Upper
                | Op::Lower
                | Op::Trim
                | Op::CharLength
                | Op::Concat
                | Op::StartsWith
                | Op::EndsWith
                | Op::Contains
                | Op::Matches(_) => Some(Kind::Text),
                Op::Substring => Some(if position == 0 {
                    Kind::Text
                } else {
                    Kind::Integer
                }),
                _ => None,
            };
            if expected.is_some_and(|expected| !expected.accepts(nodes[child].kind)) {
                return Err(wrong());
            }
        }
        // Keep checking exact domains even if another member is dynamic/null.
        // Outside an integer root, result branches mixing Integer and Float
        // (CASE WHEN c THEN 1 ELSE 1.0 END, fgdb-0g2ou) merge to Dynamic, as a
        // property load does: each row keeps its own numeric type, and a
        // non-numeric member still refuses. Integer roots stay strict.
        let merge_positions = |positions: &[usize],
                               numeric_comparison: bool|
         -> Result<Kind, GraphIntegerBuildError> {
            let mut known = Kind::Null;
            let mut dynamic = false;
            let mut mixed = false;
            for &position in positions {
                let kind = child_kind(position);
                if kind == Kind::Dynamic {
                    dynamic = true;
                } else {
                    let merged = known.merge(kind);
                    known = if numeric_comparison {
                        known.merge_comparison(kind)
                    } else if merged.is_none() && !integer_root {
                        let numeric = known.merge_comparison(kind);
                        mixed |= numeric.is_some();
                        numeric
                    } else {
                        merged
                    }
                    .ok_or_else(wrong)?;
                }
            }
            Ok(if mixed || (dynamic && known == Kind::Null) {
                Kind::Dynamic
            } else {
                known
            })
        };
        let kind = match op {
            Op::Literal(None) => Kind::Null,
            Op::Scalar(value) => match value.value() {
                CanonicalScalar::Null => Kind::Null,
                CanonicalScalar::Int(_) => Kind::Integer,
                CanonicalScalar::Float(_) => Kind::Float,
                CanonicalScalar::Bool(_) => Kind::Boolean,
                CanonicalScalar::Text(_) => Kind::Text,
                _ => return Err(wrong()),
            },
            Op::ScalarColumn(_) | Op::Local(_) => Kind::Dynamic,
            Op::Unary(_) | Op::Binary(_) if !integer_root => {
                if matches!(op, Op::Binary(GraphIntegerBinary::NullIf)) {
                    child_kind(0)
                } else if children
                    .iter()
                    .any(|&child| nodes[child].kind == Kind::Float)
                {
                    Kind::Float
                } else {
                    Kind::Integer
                }
            }
            Op::Coalesce => merge_positions(&[0, 1], false)?,
            Op::Case => merge_positions(&[1, 2], false)?,
            Op::Compare(_) => {
                merge_positions(&[0, 1], true)?;
                Kind::Boolean
            }
            Op::InList { .. } => {
                merge_positions(&(0..arity).collect::<Vec<_>>(), true)?;
                Kind::Boolean
            }
            Op::SimpleCase { alternatives } => {
                let mut candidates = vec![0];
                candidates.extend((0..*alternatives).map(|arm| 1 + 2 * arm));
                merge_positions(&candidates, true)?;
                let mut results: Vec<_> = (0..*alternatives).map(|arm| 2 + 2 * arm).collect();
                results.push(arity - 1);
                merge_positions(&results, false)?
            }
            Op::Truth(_)
            | Op::IsNull(_)
            | Op::Not
            | Op::And
            | Op::Or
            | Op::StartsWith
            | Op::EndsWith
            | Op::Contains
            | Op::Matches(_) => Kind::Boolean,
            Op::Upper | Op::Lower | Op::Trim | Op::Substring | Op::Concat | Op::ToText => {
                Kind::Text
            }
            Op::Numeric(_) => Kind::Float,
            _ => Kind::Integer,
        };
        let numeric_arithmetic = !integer_root
            && matches!(op, Op::Unary(_) | Op::Binary(_))
            && children.iter().any(|&child| nodes[child].may_float);
        // Retain dynamic numeric possibilities separately from the known-kind
        // admission above. An integer fallback cannot narrow a dynamic property
        // load, but a Boolean/text fallback still refuses an arithmetic use.
        let may_float = match op {
            Op::ScalarColumn(_) | Op::Local(_) => true,
            Op::Scalar(value) => matches!(value.value(), CanonicalScalar::Float(_)),
            Op::Numeric(_) => true,
            Op::Binary(GraphIntegerBinary::NullIf) => nodes[children[0]].may_float,
            Op::Unary(_) | Op::Binary(_) => numeric_arithmetic,
            Op::Coalesce => children.iter().any(|&child| nodes[child].may_float),
            Op::Case => children[1..].iter().any(|&child| nodes[child].may_float),
            Op::SimpleCase { alternatives } => (0..*alternatives)
                .map(|arm| children[2 + 2 * arm])
                .chain(core::iter::once(children[arity - 1]))
                .any(|child| nodes[child].may_float),
            _ => false,
        };
        let peak = match op {
            Op::Coalesce | Op::Case => children
                .iter()
                .map(|&child| nodes[child].peak)
                .max()
                .unwrap_or(1),
            Op::SimpleCase { alternatives } => {
                let mut peak = nodes[children[0]]
                    .peak
                    .max(nodes[*children.last().expect("CASE default")].peak);
                for arm in 0..*alternatives {
                    peak = peak.max(1 + nodes[children[1 + 2 * arm]].peak);
                    peak = peak.max(nodes[children[2 + 2 * arm]].peak);
                }
                peak
            }
            _ => children
                .iter()
                .enumerate()
                .map(|(held, &child)| held + nodes[child].peak)
                .max()
                .unwrap_or(1),
        };
        // Scalar syntax must compile to the same integer instructions as typed
        // IR when the consuming operator requires integers. Do not infer this
        // from the result kind: CHAR_LENGTH still consumes text.
        for (position, &child) in children.iter().enumerate() {
            let integer_operand = matches!(op, Op::Unary(_) | Op::Binary(_))
                || matches!(op, Op::Substring) && position != 0;
            if integer_operand {
                let normalized = match &nodes[child].op {
                    Op::ScalarColumn(column) if !numeric_arithmetic => Some(Op::Column(*column)),
                    Op::Scalar(value) => match value.value() {
                        CanonicalScalar::Int(value) => Some(Op::Literal(Some(*value))),
                        CanonicalScalar::Null => Some(Op::Literal(None)),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(normalized) = normalized {
                    nodes[child].op = normalized;
                }
            }
        }
        roots.push(nodes.len());
        nodes.push(Node {
            op: op.clone(),
            children,
            kind,
            peak,
            numeric_arithmetic,
            may_float,
        });
    }
    if roots.len() != 1 {
        return Err(GraphIntegerBuildError::ExtraOperands {
            remaining: roots.len(),
        });
    }
    if integer_root && !matches!(nodes[roots[0]].kind, Kind::Integer | Kind::Null) {
        return Err(GraphIntegerBuildError::OperandType {
            instruction: ops.len(),
        });
    }
    let stack_entries = nodes[roots[0]].peak;
    enum Task {
        Visit(usize),
        Emit(Instruction),
        CoalesceRight(usize),
        PatchPresent(usize),
        CaseCondition {
            then_node: usize,
            else_node: usize,
        },
        CaseThen {
            failure: usize,
            else_node: usize,
        },
        PatchJump(usize),
        SwitchNext {
            node: usize,
            arm: usize,
            exits: Vec<usize>,
        },
        SwitchTest {
            node: usize,
            arm: usize,
            exits: Vec<usize>,
        },
        SwitchResult {
            node: usize,
            arm: usize,
            failure: usize,
            exits: Vec<usize>,
        },
        SwitchDone(Vec<usize>),
    }
    let mut tasks = vec![Task::Visit(roots[0])];
    let mut code = Vec::with_capacity(ops.len());
    while let Some(task) = tasks.pop() {
        match task {
            Task::Visit(at) => {
                let node = &nodes[at];
                let unary = match &node.op {
                    Op::Unary(op) => Some(if node.numeric_arithmetic {
                        Instruction::NumericUnary(*op)
                    } else {
                        Instruction::Unary(*op)
                    }),
                    Op::IsNull(is_null) => Some(Instruction::IsNull(*is_null)),
                    Op::Not => Some(Instruction::Not),
                    Op::Upper => Some(Instruction::Upper),
                    Op::Lower => Some(Instruction::Lower),
                    Op::Trim => Some(Instruction::Trim),
                    Op::CharLength => Some(Instruction::CharLength),
                    Op::ToText => Some(Instruction::ToText),
                    Op::ToInteger => Some(Instruction::ToInteger),
                    Op::Numeric(function) => Some(Instruction::Numeric(*function)),
                    Op::Matches(regex) => Some(Instruction::Matches(regex.clone())),
                    _ => None,
                };
                if let Some(op) = unary {
                    tasks.push(Task::Emit(op));
                    tasks.push(Task::Visit(node.children[0]));
                    continue;
                }
                let binary = match &node.op {
                    Op::Binary(op) => Some(if node.numeric_arithmetic {
                        Instruction::NumericBinary(*op)
                    } else {
                        Instruction::Binary(*op)
                    }),
                    Op::Compare(op) => Some(Instruction::Compare(*op)),
                    Op::And => Some(Instruction::And),
                    Op::Or => Some(Instruction::Or),
                    Op::Concat => Some(Instruction::Concat),
                    Op::StartsWith => Some(Instruction::StartsWith),
                    Op::EndsWith => Some(Instruction::EndsWith),
                    Op::Contains => Some(Instruction::Contains),
                    _ => None,
                };
                if let Some(op) = binary {
                    tasks.push(Task::Emit(op));
                    tasks.push(Task::Visit(node.children[1]));
                    tasks.push(Task::Visit(node.children[0]));
                    continue;
                }
                match &node.op {
                    Op::Column(column) => code.push(Instruction::Column(*column)),
                    Op::ScalarColumn(column) => code.push(Instruction::ScalarColumn(*column)),
                    Op::Local(offset) => code.push(Instruction::Local(*offset)),
                    Op::Scalar(value) => code.push(Instruction::Scalar(value.clone())),
                    Op::Literal(value) => code.push(Instruction::Literal(*value)),
                    Op::Truth(value) => code.push(Instruction::Truth(*value)),
                    Op::Substring | Op::InList { .. } => {
                        tasks.push(Task::Emit(match &node.op {
                            Op::InList { members } => Instruction::InList { members: *members },
                            _ => Instruction::Substring,
                        }));
                        tasks.extend(node.children.iter().rev().map(|&child| Task::Visit(child)));
                    }
                    Op::Coalesce => {
                        tasks.push(Task::CoalesceRight(node.children[1]));
                        tasks.push(Task::Visit(node.children[0]));
                    }
                    Op::Case => {
                        tasks.push(Task::CaseCondition {
                            then_node: node.children[1],
                            else_node: node.children[2],
                        });
                        tasks.push(Task::Visit(node.children[0]));
                    }
                    Op::SimpleCase { .. } => {
                        tasks.push(Task::SwitchNext {
                            node: at,
                            arm: 0,
                            exits: Vec::new(),
                        });
                        tasks.push(Task::Visit(node.children[0]));
                    }
                    Op::Unary(_)
                    | Op::IsNull(_)
                    | Op::Not
                    | Op::Binary(_)
                    | Op::Compare(_)
                    | Op::And
                    | Op::Or
                    | Op::Upper
                    | Op::Lower
                    | Op::Trim
                    | Op::CharLength
                    | Op::ToText
                    | Op::ToInteger
                    | Op::Numeric(_)
                    | Op::Matches(_)
                    | Op::Concat
                    | Op::StartsWith
                    | Op::EndsWith
                    | Op::Contains => unreachable!("operator emitted above"),
                }
            }
            Task::Emit(op) => code.push(op),
            Task::CoalesceRight(right) => {
                let jump = code.len();
                code.push(Instruction::JumpIfPresent(0));
                tasks.push(Task::PatchPresent(jump));
                tasks.push(Task::Visit(right));
            }
            Task::PatchPresent(jump) => code[jump] = Instruction::JumpIfPresent(code.len()),
            Task::CaseCondition {
                then_node,
                else_node,
            } => {
                let failure = code.len();
                code.push(Instruction::JumpUnlessTrue(0));
                tasks.push(Task::CaseThen { failure, else_node });
                tasks.push(Task::Visit(then_node));
            }
            Task::CaseThen { failure, else_node } => {
                let exit = code.len();
                code.push(Instruction::Jump(0));
                code[failure] = Instruction::JumpUnlessTrue(code.len());
                tasks.push(Task::PatchJump(exit));
                tasks.push(Task::Visit(else_node));
            }
            Task::PatchJump(exit) => code[exit] = Instruction::Jump(code.len()),
            Task::SwitchNext { node, arm, exits } => {
                let Op::SimpleCase { alternatives } = &nodes[node].op else {
                    unreachable!("checked switch")
                };
                if arm == *alternatives {
                    code.push(Instruction::Drop);
                    tasks.push(Task::SwitchDone(exits));
                    tasks.push(Task::Visit(
                        *nodes[node].children.last().expect("checked switch default"),
                    ));
                } else {
                    tasks.push(Task::SwitchTest { node, arm, exits });
                    tasks.push(Task::Visit(nodes[node].children[1 + 2 * arm]));
                }
            }
            Task::SwitchTest { node, arm, exits } => {
                let failure = code.len();
                code.push(Instruction::JumpUnlessEqual(0));
                tasks.push(Task::SwitchResult {
                    node,
                    arm,
                    failure,
                    exits,
                });
                tasks.push(Task::Visit(nodes[node].children[2 + 2 * arm]));
            }
            Task::SwitchResult {
                node,
                arm,
                failure,
                mut exits,
            } => {
                exits.push(code.len());
                code.push(Instruction::Jump(0));
                code[failure] = Instruction::JumpUnlessEqual(code.len());
                tasks.push(Task::SwitchNext {
                    node,
                    arm: arm + 1,
                    exits,
                });
            }
            Task::SwitchDone(exits) => {
                for exit in exits {
                    code[exit] = Instruction::Jump(code.len());
                }
            }
        }
    }
    // Each arm consumes at least two source nodes and emits at most two jumps.
    // This is a definition bound, not a claim that both branches execute.
    debug_assert!(code.len() <= 2 * MAX_GRAPH_INTEGER_INSTRUCTIONS);
    Ok(GraphIntegerExpression {
        code: code.into_boxed_slice(),
        stack_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use GraphIntegerOp as Op;

    fn value(ops: &[Op]) -> Option<i64> {
        prepare(ops)
            .unwrap()
            .evaluate_with_control(&[], &mut |_| Ok::<_, ()>(()))
            .unwrap()
    }

    fn floating(value: f64) -> Op {
        Op::Scalar(
            ScalarPredicate::new(
                CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value)),
                IntegerComparison::Equal,
            )
            .unwrap(),
        )
    }

    fn scalar_value(ops: &[Op], row: &[GraphValue]) -> CanonicalScalar {
        prepare_scalar(ops)
            .unwrap()
            .evaluate_scalar_with_control(row, &mut |_| Ok::<_, ()>(()))
            .unwrap()
    }

    #[test]
    fn mixed_numeric_scalar_comparisons_are_exact_for_literals_and_columns() {
        for (integer, float, order) in [
            (7, 7.0, core::cmp::Ordering::Equal),
            (-1, -1.5, core::cmp::Ordering::Greater),
            (0, f64::from_bits(1), core::cmp::Ordering::Less),
            (
                9_007_199_254_740_993,
                9_007_199_254_740_992.0,
                core::cmp::Ordering::Greater,
            ),
            (
                i64::MAX,
                9_223_372_036_854_775_808.0,
                core::cmp::Ordering::Less,
            ),
        ] {
            for reverse in [false, true] {
                let mut literals = vec![Op::Literal(Some(integer)), floating(float)];
                let mut row = vec![
                    GraphValue::Scalar(CanonicalScalar::Int(integer)),
                    GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(
                        float,
                    ))),
                ];
                if reverse {
                    literals.reverse();
                    row.reverse();
                }
                let order = if reverse { order.reverse() } else { order };
                for (comparison, expected) in [
                    (IntegerComparison::Equal, order.is_eq()),
                    (IntegerComparison::NotEqual, !order.is_eq()),
                    (IntegerComparison::Less, order.is_lt()),
                    (IntegerComparison::LessOrEqual, !order.is_gt()),
                    (IntegerComparison::Greater, order.is_gt()),
                    (IntegerComparison::GreaterOrEqual, !order.is_lt()),
                ] {
                    let mut ops = literals.clone();
                    ops.push(Op::Compare(comparison));
                    assert_eq!(scalar_value(&ops, &[]), CanonicalScalar::Bool(expected));
                    assert_eq!(
                        scalar_value(
                            &[
                                Op::ScalarColumn(0),
                                Op::ScalarColumn(1),
                                Op::Compare(comparison)
                            ],
                            &row
                        ),
                        CanonicalScalar::Bool(expected)
                    );
                }
            }
        }
    }

    #[test]
    fn numeric_membership_and_case_keep_unknown_precision_and_lazy_branches() {
        for (selector, expected) in [(7, Some(true)), (8, None)] {
            let ops = [
                Op::Literal(Some(selector)),
                floating(7.0),
                Op::Literal(None),
                Op::InList { members: 2 },
            ];
            assert_eq!(scalar_value(&ops, &[]), boolean_scalar(expected));
            let mut negated = ops.to_vec();
            negated.push(Op::Not);
            assert_eq!(
                scalar_value(&negated, &[]),
                boolean_scalar(expected.map(|value| !value))
            );
        }
        assert_eq!(
            scalar_value(
                &[
                    Op::Literal(Some(9_007_199_254_740_993)),
                    floating(9_007_199_254_740_992.0),
                    Op::InList { members: 1 }
                ],
                &[]
            ),
            CanonicalScalar::Bool(false)
        );
        // The matching mixed-numeric arm must not evaluate the invalid fallback.
        assert_eq!(
            value(&[
                Op::Literal(Some(7)),
                floating(7.0),
                Op::Literal(Some(42)),
                Op::Column(usize::MAX),
                Op::SimpleCase { alternatives: 1 }
            ]),
            Some(42)
        );
        // Rounding the selector into f64 would take the wrong arm here.
        assert_eq!(
            value(&[
                Op::Literal(Some(9_007_199_254_740_993)),
                floating(9_007_199_254_740_992.0),
                Op::Column(usize::MAX),
                Op::Literal(Some(42)),
                Op::SimpleCase { alternatives: 1 }
            ]),
            Some(42)
        );
        assert_eq!(
            scalar_value(&[Op::Literal(None), floating(1.5), Op::Coalesce], &[]),
            CanonicalScalar::Float(fgdb_types::CanonicalF64::new(1.5))
        );
    }

    #[test]
    fn float_comparison_admission_does_not_relax_integer_or_result_domains() {
        for ops in [
            vec![floating(1.0), Op::Unary(GraphIntegerUnary::Negate)],
            vec![
                floating(1.0),
                Op::Literal(Some(1)),
                Op::Binary(GraphIntegerBinary::Add),
            ],
        ] {
            assert!(matches!(
                prepare(&ops),
                Err(GraphIntegerBuildError::OperandType { .. })
            ));
            assert!(prepare_scalar(&ops).is_ok());
        }
        for ops in [
            vec![
                floating(1.0),
                Op::Truth(Some(true)),
                Op::Compare(IntegerComparison::Equal),
            ],
            vec![
                Op::ScalarColumn(0),
                Op::Truth(Some(true)),
                Op::Coalesce,
                Op::Literal(Some(1)),
                Op::Binary(GraphIntegerBinary::Add),
            ],
            vec![floating(1.0), Op::Truth(Some(true)), Op::Coalesce],
            vec![
                Op::Truth(Some(true)),
                floating(1.0),
                Op::Truth(Some(false)),
                Op::Case,
            ],
            vec![
                Op::ScalarColumn(0),
                floating(1.0),
                Op::Truth(Some(true)),
                Op::InList { members: 2 },
            ],
        ] {
            assert!(matches!(
                prepare_scalar(&ops),
                Err(GraphIntegerBuildError::OperandType { .. })
            ));
        }
        // Owner ruling 2026-10-09 (fgdb-0g2ou): an Integer/Float result mix
        // is a per-row dynamic value in a scalar root, as in openCypher, and
        // stays refused in an integer root.
        for ops in [
            vec![floating(1.0), Op::Literal(Some(1)), Op::Coalesce],
            vec![
                Op::Truth(Some(true)),
                floating(1.0),
                Op::Literal(Some(1)),
                Op::Case,
            ],
        ] {
            assert!(prepare_scalar(&ops).is_ok());
            assert!(matches!(
                prepare(&ops),
                Err(GraphIntegerBuildError::OperandType { .. })
            ));
        }
        assert!(matches!(
            prepare(&[floating(1.0)]),
            Err(GraphIntegerBuildError::OperandType { .. })
        ));
        let comparison = prepare_scalar(&[
            Op::ScalarColumn(0),
            floating(1.0),
            Op::Compare(IntegerComparison::Equal),
        ])
        .unwrap();
        assert!(matches!(
            comparison.evaluate_scalar_with_control(
                &[GraphValue::Scalar(CanonicalScalar::Bool(true))],
                &mut |_| Ok::<_, ()>(())
            ),
            Err(GraphIntegerEvaluationError::Value(GraphIntegerError {
                kind: GraphIntegerErrorKind::IncompatibleOperands,
                ..
            }))
        ));
    }

    #[test]
    fn mixed_numeric_scalar_execution_preserves_all_cancellation_checkpoints() {
        let ops = [
            Op::Literal(Some(7)),
            floating(7.0),
            Op::Compare(IntegerComparison::Equal),
        ];
        let expression = prepare_scalar(&ops).unwrap();
        let frozen = expression.canonical_bytes();
        let mut count = 0;
        assert_eq!(
            expression
                .evaluate_scalar_with_control(&[], &mut |_| {
                    count += 1;
                    Ok::<_, usize>(())
                })
                .unwrap(),
            CanonicalScalar::Bool(true)
        );
        assert!(count >= 5);
        for stop in 1..=count {
            let mut seen = 0;
            let result = expression.evaluate_scalar_with_control(&[], &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GraphIntegerEvaluationError::Control(at)) if at == stop));
            assert_eq!(seen, stop);
            assert_eq!(expression.canonical_bytes(), frozen);
        }
    }

    #[test]
    fn conditional_stack_is_typed_including_unselected_branches() {
        for ops in [
            vec![
                Op::Literal(Some(1)),
                Op::Literal(Some(2)),
                Op::Literal(Some(3)),
                Op::Case,
            ],
            vec![
                Op::Truth(Some(true)),
                Op::Truth(None),
                Op::Literal(Some(3)),
                Op::Case,
            ],
            vec![
                Op::Truth(Some(true)),
                Op::Literal(Some(2)),
                Op::Binary(GraphIntegerBinary::Add),
            ],
            vec![Op::Truth(None)],
        ] {
            assert!(matches!(
                prepare(&ops),
                Err(GraphIntegerBuildError::OperandType { .. })
            ));
        }
        assert!(matches!(
            prepare(&[Op::SimpleCase { alternatives: 0 }]),
            Err(GraphIntegerBuildError::EmptyCase { .. })
        ));
        assert!(matches!(
            prepare(&[Op::SimpleCase {
                alternatives: usize::MAX
            }]),
            Err(GraphIntegerBuildError::MissingOperand { .. })
        ));
    }

    #[test]
    fn searched_case_has_three_valued_conditions_and_lazy_result_branches() {
        for truth in [None, Some(false), Some(true)] {
            assert_eq!(
                value(&[
                    Op::Truth(truth),
                    Op::Literal(Some(7)),
                    Op::Literal(Some(9)),
                    Op::Case
                ]),
                Some(if truth == Some(true) { 7 } else { 9 })
            );
        }
        let ops = [
            Op::Truth(Some(true)),
            Op::Literal(Some(7)),
            Op::Literal(Some(1)),
            Op::Literal(Some(0)),
            Op::Binary(GraphIntegerBinary::Divide),
            Op::Case,
        ];
        assert_eq!(value(&ops), Some(7));
        let ops = [
            Op::Truth(None),
            Op::Column(usize::MAX),
            Op::Literal(None),
            Op::Case,
            Op::Literal(Some(4)),
            Op::Coalesce,
        ];
        assert_eq!(value(&ops), Some(4));
    }

    #[test]
    fn simple_case_evaluates_selector_once_and_null_never_matches_null() {
        for input in [None, Some(0), Some(1), Some(2)] {
            let ops = [
                Op::Literal(input),
                Op::Literal(None),
                Op::Literal(Some(8)),
                Op::Literal(Some(1)),
                Op::Literal(Some(7)),
                Op::Literal(Some(9)),
                Op::SimpleCase { alternatives: 2 },
            ];
            assert_eq!(value(&ops), Some(if input == Some(1) { 7 } else { 9 }));
        }
        let ops = [
            Op::Literal(Some(1)),
            Op::Literal(Some(1)),
            Op::Literal(Some(7)),
            Op::Column(usize::MAX),
            Op::Column(usize::MAX),
            Op::Column(usize::MAX),
            Op::SimpleCase { alternatives: 2 },
        ];
        assert_eq!(value(&ops), Some(7));
        let compiled = prepare(&ops).unwrap();
        assert_eq!(compiled.stack_entries, 2);
        assert_eq!(
            compiled.referenced_columns().count(),
            3,
            "lazy inputs remain in static admission"
        );
    }

    #[test]
    fn boolean_truth_tables_do_not_coerce_integer_or_unknown_cells() {
        for left in [None, Some(false), Some(true)] {
            for right in [None, Some(false), Some(true)] {
                for (op, truth) in [
                    (
                        Op::And,
                        if left == Some(false) || right == Some(false) {
                            Some(false)
                        } else if left.is_none() || right.is_none() {
                            None
                        } else {
                            Some(true)
                        },
                    ),
                    (
                        Op::Or,
                        if left == Some(true) || right == Some(true) {
                            Some(true)
                        } else if left.is_none() || right.is_none() {
                            None
                        } else {
                            Some(false)
                        },
                    ),
                ] {
                    let ops = [
                        Op::Truth(left),
                        Op::Truth(right),
                        op.clone(),
                        Op::Literal(Some(1)),
                        Op::Literal(Some(0)),
                        Op::Case,
                    ];
                    assert_eq!(value(&ops), Some(i64::from(truth == Some(true))));
                    let negated = [
                        Op::Truth(left),
                        Op::Truth(right),
                        op,
                        Op::Not,
                        Op::Literal(Some(1)),
                        Op::Literal(Some(0)),
                        Op::Case,
                    ];
                    assert_eq!(value(&negated), Some(i64::from(truth == Some(false))));
                }
            }
        }
    }

    #[test]
    fn deep_case_compilation_and_each_executed_checkpoint_are_bounded() {
        let mut ops = vec![Op::Literal(Some(5))];
        for _ in 0..240 {
            ops.extend([
                Op::Literal(Some(6)),
                Op::Literal(Some(7)),
                Op::Literal(Some(8)),
                Op::SimpleCase { alternatives: 1 },
            ]);
        }
        let expression = prepare(&ops).unwrap();
        let mut calls = 0;
        let expected = expression
            .evaluate_with_control(&[], &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        assert_eq!(expected, Some(8));
        for stop in 1..=calls {
            let mut seen = 0;
            let result = expression.evaluate_with_control(&[], &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(
                matches!(result, Err(GraphIntegerEvaluationError::Control(actual)) if actual == stop)
            );
            assert_eq!(seen, stop);
        }
        assert_eq!(
            expression
                .evaluate_with_control(&[], &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            expected
        );
        assert_eq!(
            prepare(&ops).unwrap().canonical_bytes(),
            expression.canonical_bytes()
        );
    }
}
