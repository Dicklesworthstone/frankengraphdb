//! Typed postfix admission and iterative control-flow compilation.
//! CASE branches merge at one scalar stack cell. The compile-time frame bound
//! is the maximum over reachable branches, not a scan through mutually
//! exclusive code. Integer preparation retains its strict root contract.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind { Null, Integer, Boolean, Text, Dynamic }

impl Kind {
    fn accepts(self, actual: Self) -> bool {
        actual == self || matches!(actual, Self::Null | Self::Dynamic)
    }
    fn merge(self, other: Self) -> Option<Self> {
        if self == other || other == Self::Null { Some(self) }
        else if self == Self::Null { Some(other) }
        else if self == Self::Dynamic || other == Self::Dynamic { Some(Self::Dynamic) }
        else { None }
    }
}
struct Node {
    op: GraphIntegerOp,
    children: Vec<usize>,
    kind: Kind,
    peak: usize,
}

pub(super) fn prepare(ops: &[GraphIntegerOp]) -> Result<GraphIntegerExpression, GraphIntegerBuildError> {
    prepare_root(ops, true)
}

pub(super) fn prepare_scalar(ops: &[GraphIntegerOp]) -> Result<GraphIntegerExpression, GraphIntegerBuildError> {
    prepare_root(ops, false)
}

fn prepare_root(ops: &[GraphIntegerOp], integer_root: bool) -> Result<GraphIntegerExpression, GraphIntegerBuildError> {
    use GraphIntegerOp as Op;
    if ops.is_empty() { return Err(GraphIntegerBuildError::Empty); }
    if ops.len() > MAX_GRAPH_INTEGER_INSTRUCTIONS {
        return Err(GraphIntegerBuildError::TooManyInstructions {
            limit: MAX_GRAPH_INTEGER_INSTRUCTIONS, observed: ops.len(),
        });
    }
    let mut nodes: Vec<Node> = Vec::with_capacity(ops.len());
    let mut roots: Vec<usize> = Vec::new();
    for (at, op) in ops.iter().enumerate() {
        let missing = || GraphIntegerBuildError::MissingOperand { instruction: at };
        let wrong = || GraphIntegerBuildError::OperandType { instruction: at };
        let arity = match op {
            Op::Column(_) | Op::Literal(_) | Op::Truth(_) | Op::Scalar(_) | Op::ScalarColumn(_) => 0,
            Op::Unary(_) | Op::IsNull(_) | Op::Not | Op::Upper | Op::Lower | Op::Trim | Op::CharLength => 1,
            Op::Binary(_) | Op::Coalesce | Op::Compare(_) | Op::And | Op::Or
            | Op::Concat | Op::StartsWith | Op::EndsWith | Op::Contains => 2,
            Op::Case | Op::Substring => 3,
            Op::InList { members } => members.checked_add(1).ok_or_else(missing)?,
            Op::SimpleCase { alternatives } => {
                if *alternatives == 0 {
                    return Err(GraphIntegerBuildError::EmptyCase { instruction: at });
                }
                alternatives.checked_mul(2).and_then(|n| n.checked_add(2)).ok_or_else(missing)?
            }
        };
        if roots.len() < arity { return Err(missing()); }
        let children = roots.split_off(roots.len() - arity);
        let child_kind = |position: usize| nodes[children[position]].kind;
        for (position, &child) in children.iter().enumerate() {
            let expected = match op {
                Op::Unary(_) | Op::Binary(_) => Some(Kind::Integer),
                Op::Not | Op::And | Op::Or => Some(Kind::Boolean),
                Op::Case if position == 0 => Some(Kind::Boolean),
                Op::Upper | Op::Lower | Op::Trim | Op::CharLength | Op::Concat
                | Op::StartsWith | Op::EndsWith | Op::Contains => Some(Kind::Text),
                Op::Substring => Some(if position == 0 { Kind::Text } else { Kind::Integer }),
                _ => None,
            };
            if expected.is_some_and(|expected| !expected.accepts(nodes[child].kind)) {
                return Err(wrong());
            }
        }
        // Keep checking exact domains even if another member is dynamic/null.
        let merge_positions = |positions: &[usize]| -> Result<Kind, GraphIntegerBuildError> {
            let mut known = Kind::Null;
            let mut dynamic = false;
            for &position in positions {
                let kind = child_kind(position);
                if kind == Kind::Dynamic { dynamic = true; }
                else { known = known.merge(kind).ok_or_else(wrong)?; }
            }
            Ok(if dynamic { Kind::Dynamic } else { known })
        };
        let kind = match op {
            Op::Literal(None) | Op::Truth(None) => Kind::Null,
            Op::Scalar(value) => match value.value() {
                CanonicalScalar::Null => Kind::Null,
                CanonicalScalar::Int(_) => Kind::Integer,
                CanonicalScalar::Bool(_) => Kind::Boolean,
                CanonicalScalar::Text(_) => Kind::Text,
                _ => return Err(wrong()),
            },
            Op::ScalarColumn(_) => Kind::Dynamic,
            Op::Coalesce => merge_positions(&[0, 1])?,
            Op::Case => merge_positions(&[1, 2])?,
            Op::Compare(_) => { merge_positions(&[0, 1])?; Kind::Boolean }
            Op::InList { .. } => {
                merge_positions(&(0..arity).collect::<Vec<_>>())?;
                Kind::Boolean
            }
            Op::SimpleCase { alternatives } => {
                let mut candidates = vec![0];
                candidates.extend((0..*alternatives).map(|arm| 1 + 2 * arm));
                merge_positions(&candidates)?;
                let mut results: Vec<_> = (0..*alternatives).map(|arm| 2 + 2 * arm).collect();
                results.push(arity - 1);
                merge_positions(&results)?
            }
            Op::Truth(_) | Op::IsNull(_) | Op::Not | Op::And | Op::Or
            | Op::StartsWith | Op::EndsWith | Op::Contains => Kind::Boolean,
            Op::Upper | Op::Lower | Op::Trim | Op::Substring | Op::Concat => Kind::Text,
            _ => Kind::Integer,
        };
        let peak = match op {
            Op::Coalesce | Op::Case => children.iter().map(|&child| nodes[child].peak).max().unwrap_or(1),
            Op::SimpleCase { alternatives } => {
                let mut peak = nodes[children[0]].peak.max(nodes[*children.last().expect("CASE default")].peak);
                for arm in 0..*alternatives {
                    peak = peak.max(1 + nodes[children[1 + 2 * arm]].peak);
                    peak = peak.max(nodes[children[2 + 2 * arm]].peak);
                }
                peak
            }
            _ => children.iter().enumerate().map(|(held, &child)| held + nodes[child].peak).max().unwrap_or(1),
        };
        roots.push(nodes.len());
        nodes.push(Node { op: op.clone(), children, kind, peak });
    }
    if roots.len() != 1 {
        return Err(GraphIntegerBuildError::ExtraOperands { remaining: roots.len() });
    }
    if integer_root && !matches!(nodes[roots[0]].kind, Kind::Integer | Kind::Null) {
        return Err(GraphIntegerBuildError::OperandType { instruction: ops.len() });
    }
    let stack_entries = nodes[roots[0]].peak;
    enum Task {
        Visit(usize), Emit(Instruction), CoalesceRight(usize), PatchPresent(usize),
        CaseCondition { then_node: usize, else_node: usize },
        CaseThen { failure: usize, else_node: usize }, PatchJump(usize),
        SwitchNext { node: usize, arm: usize, exits: Vec<usize> },
        SwitchTest { node: usize, arm: usize, exits: Vec<usize> },
        SwitchResult { node: usize, arm: usize, failure: usize, exits: Vec<usize> },
        SwitchDone(Vec<usize>),
    }
    let mut tasks = vec![Task::Visit(roots[0])];
    let mut code = Vec::with_capacity(ops.len());
    while let Some(task) = tasks.pop() {
        match task {
            Task::Visit(at) => {
                let node = &nodes[at];
                let unary = match &node.op {
                    Op::Unary(op) => Some(Instruction::Unary(*op)),
                    Op::IsNull(is_null) => Some(Instruction::IsNull(*is_null)),
                    Op::Not => Some(Instruction::Not),
                    Op::Upper => Some(Instruction::Upper),
                    Op::Lower => Some(Instruction::Lower),
                    Op::Trim => Some(Instruction::Trim),
                    Op::CharLength => Some(Instruction::CharLength),
                    _ => None,
                };
                if let Some(op) = unary {
                    tasks.push(Task::Emit(op)); tasks.push(Task::Visit(node.children[0])); continue;
                }
                let binary = match &node.op {
                    Op::Binary(op) => Some(Instruction::Binary(*op)),
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
                    tasks.push(Task::Emit(op)); tasks.push(Task::Visit(node.children[1]));
                    tasks.push(Task::Visit(node.children[0])); continue;
                }
                match &node.op {
                    Op::Column(column) => code.push(Instruction::Column(*column)),
                    Op::ScalarColumn(column) => code.push(Instruction::ScalarColumn(*column)),
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
                        tasks.push(Task::CaseCondition { then_node: node.children[1], else_node: node.children[2] });
                        tasks.push(Task::Visit(node.children[0]));
                    }
                    Op::SimpleCase { .. } => {
                        tasks.push(Task::SwitchNext { node: at, arm: 0, exits: Vec::new() });
                        tasks.push(Task::Visit(node.children[0]));
                    }
                    Op::Unary(_) | Op::IsNull(_) | Op::Not | Op::Binary(_)
                    | Op::Compare(_) | Op::And | Op::Or | Op::Upper | Op::Lower | Op::Trim
                    | Op::CharLength | Op::Concat | Op::StartsWith | Op::EndsWith | Op::Contains => unreachable!("operator emitted above"),
                }
            }
            Task::Emit(op) => code.push(op),
            Task::CoalesceRight(right) => {
                let jump = code.len(); code.push(Instruction::JumpIfPresent(0));
                tasks.push(Task::PatchPresent(jump)); tasks.push(Task::Visit(right));
            }
            Task::PatchPresent(jump) => code[jump] = Instruction::JumpIfPresent(code.len()),
            Task::CaseCondition { then_node, else_node } => {
                let failure = code.len(); code.push(Instruction::JumpUnlessTrue(0));
                tasks.push(Task::CaseThen { failure, else_node }); tasks.push(Task::Visit(then_node));
            }
            Task::CaseThen { failure, else_node } => {
                let exit = code.len(); code.push(Instruction::Jump(0));
                code[failure] = Instruction::JumpUnlessTrue(code.len());
                tasks.push(Task::PatchJump(exit)); tasks.push(Task::Visit(else_node));
            }
            Task::PatchJump(exit) => code[exit] = Instruction::Jump(code.len()),
            Task::SwitchNext { node, arm, exits } => {
                let Op::SimpleCase { alternatives } = &nodes[node].op else { unreachable!("checked switch") };
                if arm == *alternatives {
                    code.push(Instruction::Drop);
                    tasks.push(Task::SwitchDone(exits));
                    tasks.push(Task::Visit(*nodes[node].children.last().expect("checked switch default")));
                } else {
                    tasks.push(Task::SwitchTest { node, arm, exits });
                    tasks.push(Task::Visit(nodes[node].children[1 + 2 * arm]));
                }
            }
            Task::SwitchTest { node, arm, exits } => {
                let failure = code.len(); code.push(Instruction::JumpUnlessEqual(0));
                tasks.push(Task::SwitchResult { node, arm, failure, exits });
                tasks.push(Task::Visit(nodes[node].children[2 + 2 * arm]));
            }
            Task::SwitchResult { node, arm, failure, mut exits } => {
                exits.push(code.len()); code.push(Instruction::Jump(0));
                code[failure] = Instruction::JumpUnlessEqual(code.len());
                tasks.push(Task::SwitchNext { node, arm: arm + 1, exits });
            }
            Task::SwitchDone(exits) => {
                for exit in exits { code[exit] = Instruction::Jump(code.len()); }
            }
        }
    }
    // Each arm consumes at least two source nodes and emits at most two jumps.
    // This is a definition bound, not a claim that both branches execute.
    debug_assert!(code.len() <= 2 * MAX_GRAPH_INTEGER_INSTRUCTIONS);
    Ok(GraphIntegerExpression { code: code.into_boxed_slice(), stack_entries })
}

#[cfg(test)]
mod tests {
    use super::*;
    use GraphIntegerOp as Op;

    fn value(ops: &[Op]) -> Option<i64> {
        prepare(ops).unwrap().evaluate_with_control(&[], &mut |_| Ok::<_, ()>(())).unwrap()
    }

    #[test]
    fn conditional_stack_is_typed_including_unselected_branches() {
        for ops in [
            vec![Op::Literal(Some(1)), Op::Literal(Some(2)), Op::Literal(Some(3)), Op::Case],
            vec![Op::Truth(Some(true)), Op::Truth(None), Op::Literal(Some(3)), Op::Case],
            vec![Op::Truth(Some(true)), Op::Literal(Some(2)), Op::Binary(GraphIntegerBinary::Add)],
            vec![Op::Truth(None)],
        ] {
            assert!(matches!(prepare(&ops), Err(GraphIntegerBuildError::OperandType { .. })));
        }
        assert!(matches!(prepare(&[Op::SimpleCase { alternatives: 0 }]), Err(GraphIntegerBuildError::EmptyCase { .. })));
        assert!(matches!(prepare(&[Op::SimpleCase { alternatives: usize::MAX }]), Err(GraphIntegerBuildError::MissingOperand { .. })));
    }

    #[test]
    fn searched_case_has_three_valued_conditions_and_lazy_result_branches() {
        for truth in [None, Some(false), Some(true)] {
            assert_eq!(value(&[Op::Truth(truth), Op::Literal(Some(7)), Op::Literal(Some(9)), Op::Case]),
                Some(if truth == Some(true) { 7 } else { 9 }));
        }
        let ops = [Op::Truth(Some(true)), Op::Literal(Some(7)), Op::Literal(Some(1)),
            Op::Literal(Some(0)), Op::Binary(GraphIntegerBinary::Divide), Op::Case];
        assert_eq!(value(&ops), Some(7));
        let ops = [Op::Truth(None), Op::Column(usize::MAX), Op::Literal(None), Op::Case,
            Op::Literal(Some(4)), Op::Coalesce];
        assert_eq!(value(&ops), Some(4));
    }

    #[test]
    fn simple_case_evaluates_selector_once_and_null_never_matches_null() {
        for input in [None, Some(0), Some(1), Some(2)] {
            let ops = [Op::Literal(input), Op::Literal(None), Op::Literal(Some(8)),
                Op::Literal(Some(1)), Op::Literal(Some(7)), Op::Literal(Some(9)),
                Op::SimpleCase { alternatives: 2 }];
            assert_eq!(value(&ops), Some(if input == Some(1) { 7 } else { 9 }));
        }
        let ops = [Op::Literal(Some(1)), Op::Literal(Some(1)), Op::Literal(Some(7)),
            Op::Column(usize::MAX), Op::Column(usize::MAX), Op::Column(usize::MAX),
            Op::SimpleCase { alternatives: 2 }];
        assert_eq!(value(&ops), Some(7));
        let compiled = prepare(&ops).unwrap();
        assert_eq!(compiled.stack_entries, 2);
        assert_eq!(compiled.referenced_columns().count(), 3, "lazy inputs remain in static admission");
    }

    #[test]
    fn boolean_truth_tables_do_not_coerce_integer_or_unknown_cells() {
        for left in [None, Some(false), Some(true)] { for right in [None, Some(false), Some(true)] {
            for (op, truth) in [
                (Op::And, if left == Some(false) || right == Some(false) { Some(false) }
                    else if left.is_none() || right.is_none() { None } else { Some(true) }),
                (Op::Or, if left == Some(true) || right == Some(true) { Some(true) }
                    else if left.is_none() || right.is_none() { None } else { Some(false) }),
            ] {
                let ops = [Op::Truth(left), Op::Truth(right), op, Op::Literal(Some(1)), Op::Literal(Some(0)), Op::Case];
                assert_eq!(value(&ops), Some(i64::from(truth == Some(true))));
                let negated = [Op::Truth(left), Op::Truth(right), op, Op::Not,
                    Op::Literal(Some(1)), Op::Literal(Some(0)), Op::Case];
                assert_eq!(value(&negated), Some(i64::from(truth == Some(false))));
            }
        }}
    }

    #[test]
    fn deep_case_compilation_and_each_executed_checkpoint_are_bounded() {
        let mut ops = vec![Op::Literal(Some(5))];
        for _ in 0..240 {
            ops.extend([Op::Literal(Some(6)), Op::Literal(Some(7)), Op::Literal(Some(8)),
                Op::SimpleCase { alternatives: 1 }]);
        }
        let expression = prepare(&ops).unwrap();
        let mut calls = 0;
        let expected = expression.evaluate_with_control(&[], &mut |_| { calls += 1; Ok::<_, usize>(()) }).unwrap();
        assert_eq!(expected, Some(8));
        for stop in 1..=calls {
            let mut seen = 0;
            let result = expression.evaluate_with_control(&[], &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GraphIntegerEvaluationError::Control(actual)) if actual == stop));
            assert_eq!(seen, stop);
        }
        assert_eq!(expression.evaluate_with_control(&[], &mut |_| Ok::<_, ()>(())).unwrap(), expected);
        assert_eq!(prepare(&ops).unwrap().canonical_bytes(), expression.canonical_bytes());
    }
}
