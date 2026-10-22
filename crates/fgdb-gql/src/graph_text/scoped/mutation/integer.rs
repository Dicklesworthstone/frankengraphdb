//! Precedence-aware integer RHS lowering on the shared MATCH lexer.
//! Only preparation sees these temporary nodes. Binding substitutes typed
//! operands into checked bytecode, never into source text or an AST evaluator.

mod conditional;

use super::*;
use crate::set_text::ReadValueTemplate;
use crate::{
    GraphIntegerBinary, GraphIntegerBuildError, GraphIntegerExpression, GraphIntegerOp,
    GraphIntegerUnary, GraphNumericFunction, MAX_GRAPH_INTEGER_INSTRUCTIONS,
};
use fgdb_types::CanonicalScalarKind;

const MAX_INTEGER_NESTING: usize = 64;
// The precedence/CASE compiler is shared by graph assignments and relational
// stages. A row scope resolves only admitted aliases, never ambient graph names.
enum ExpressionColumns<'columns, 'text> {
    Graph(&'columns mut Vec<Projection<'text>>),
    Row(&'columns [(Name<'text>, crate::GraphSetColumnType)]),
    Resolved(
        &'columns mut dyn FnMut(&mut Parser<'text>) -> Result<Option<usize>, GraphPatternTextError>,
    ),
}
enum ExpressionBoundary {
    Whole,
    Predicate,
    Operand,
    /// One operand of a `+`/`-` chain: a product, stopping at `+` and `-`.
    Product,
}
enum ParsedOp {
    Atom(Operand, usize),
    ParameterField {
        index: usize,
        keys: Box<[Box<str>]>,
        at: usize,
    },
    /// `IN $list` (fgdb-dn9q3): the list argument's membership test.
    ListMembers {
        index: usize,
        at: usize,
    },
    Unary(GraphIntegerUnary),
    Binary(GraphIntegerBinary),
    Coalesce,
    Bound(GraphIntegerOp),
    /// Let slot `i` of the expression being parsed (fgdb-o5o9t). It becomes
    /// a `Local` once the expression's slot count is known.
    LetRef(usize),
}

/// Whether a postfix operand's root is text before binding: a text literal
/// or a text-producing function or concatenation.
fn static_text(root: Option<&ParsedOp>) -> bool {
    match root {
        Some(ParsedOp::Atom(Operand::Literal(value), _)) => {
            matches!(value.value(), CanonicalScalar::Text(_))
        }
        Some(ParsedOp::Bound(op)) => matches!(
            op,
            GraphIntegerOp::Upper
                | GraphIntegerOp::Lower
                | GraphIntegerOp::Trim
                | GraphIntegerOp::Substring
                | GraphIntegerOp::SubstringFrom
                | GraphIntegerOp::CypherSubstring
                | GraphIntegerOp::CypherSubstringToEnd
                | GraphIntegerOp::Left
                | GraphIntegerOp::Right
                | GraphIntegerOp::Replace
                | GraphIntegerOp::Concat
                | GraphIntegerOp::ToText
                | GraphIntegerOp::IdentityColumn { text: true, .. }
        ),
        _ => false,
    }
}

fn failure(at: usize, kind: GraphMutationTextErrorKind) -> GraphMutationTextError {
    GraphMutationTextError { offset: at, kind }
}
fn emit(
    program: &mut Vec<ParsedOp>,
    op: ParsedOp,
    at: usize,
) -> Result<(), GraphMutationTextError> {
    if program.len() >= MAX_GRAPH_INTEGER_INSTRUCTIONS {
        return Err(failure(
            at,
            GraphMutationTextErrorKind::IntegerExpression(
                GraphIntegerBuildError::TooManyInstructions {
                    limit: MAX_GRAPH_INTEGER_INSTRUCTIONS,
                    observed: program.len() + 1,
                },
            ),
        ));
    }
    program.push(op);
    Ok(())
}

fn parameter_field(
    values: &[GqlParameterValue],
    index: usize,
    keys: &[Box<str>],
    at: usize,
) -> Result<GqlScalarParameter, GraphMutationTextError> {
    let refused = || failure(at, GraphMutationTextErrorKind::IntegerOperand);
    let mut value = match values.get(index) {
        Some(GqlParameterValue::Map(map)) => Some(map.value()),
        Some(GqlParameterValue::Scalar(value)) if value.kind() == CanonicalScalarKind::Null => None,
        _ => return Err(refused()),
    };
    for key in keys {
        value = match value {
            None => None,
            Some(value) if value.is_null() => None,
            Some(crate::algebra::GraphValue::Map { keys, values }) => keys
                .binary_search_by(|candidate| candidate.as_bytes().cmp(key.as_bytes()))
                .ok()
                .map(|at| &values[at]),
            _ => return Err(refused()),
        };
    }
    let value = match value {
        None => CanonicalScalar::Null,
        Some(crate::algebra::GraphValue::Scalar(value)) => value.clone(),
        _ => return Err(refused()),
    };
    GqlScalarParameter::new(value).map_err(|_| refused())
}

/// `IN $list` (fgdb-dn9q3): each member becomes a scalar operand, then one
/// InList over them, so the membership is exactly the literal list's (three
/// valued, an empty list FALSE). A NULL argument compares `= NULL`, UNKNOWN for
/// every operand. A nested list or map member, or a list beyond the scalar
/// predicate byte bound, refuses; prepare_scalar still bounds the instruction
/// count.
fn list_members(
    ops: &mut Vec<GraphIntegerOp>,
    values: &[GqlParameterValue],
    index: usize,
    at: usize,
) -> Result<(), GraphMutationTextError> {
    let refused = || failure(at, GraphMutationTextErrorKind::IntegerOperand);
    let list = match values.get(index) {
        Some(GqlParameterValue::List(list)) => list,
        Some(GqlParameterValue::Scalar(value)) if value.kind() == CanonicalScalarKind::Null => {
            ops.push(GraphIntegerOp::Literal(None));
            ops.push(GraphIntegerOp::Compare(IntegerComparison::Equal));
            return Ok(());
        }
        _ => return Err(refused()),
    };
    if list.canonical_bytes().len() > crate::algebra::MAX_SCALAR_PREDICATE_BYTES {
        return Err(refused());
    }
    for member in list.values() {
        let crate::algebra::GraphValue::Scalar(member) = member else {
            return Err(refused());
        };
        let member = GqlScalarParameter::new(member.clone()).map_err(|_| refused())?;
        ops.push(GraphIntegerOp::Scalar(
            member.predicate(IntegerComparison::Equal),
        ));
    }
    ops.push(GraphIntegerOp::InList {
        members: list.values().len(),
    });
    Ok(())
}

pub(in crate::graph_text) fn bind_integer(
    program: &[MutationIntegerTemplateOp],
    values: &[GqlParameterValue],
    at: usize,
) -> Result<GraphIntegerExpression, GraphMutationTextError> {
    let mut ops = Vec::with_capacity(program.len());
    for op in program {
        if let MutationIntegerTemplateOp::ListMembers { index, at } = op {
            list_members(&mut ops, values, *index, *at)?;
            continue;
        }
        ops.push(match op {
            MutationIntegerTemplateOp::ListMembers { .. } => unreachable!("expanded above"),
            MutationIntegerTemplateOp::Bound(op) => op.clone(),
            MutationIntegerTemplateOp::Parameter { index, at } => {
                let value = values
                    .get(*index)
                    .ok_or_else(|| failure(*at, GraphMutationTextErrorKind::IntegerOperand))?;
                GraphIntegerOp::Scalar(
                    scalar(value.clone(), *at)?.predicate(IntegerComparison::Equal),
                )
            }
            MutationIntegerTemplateOp::ParameterField { index, keys, at } => {
                GraphIntegerOp::Scalar(
                    parameter_field(values, *index, keys, *at)?.predicate(IntegerComparison::Equal),
                )
            }
        });
    }
    GraphIntegerExpression::prepare_scalar(&ops)
        .map_err(|error| failure(at, GraphMutationTextErrorKind::IntegerExpression(error)))
}

impl<'a> Parser<'a> {
    pub(in crate::graph_text) fn bind_boolean_scalar(
        program: &[MutationIntegerTemplateOp],
        values: &[GqlParameterValue],
        at: usize,
    ) -> Result<GraphIntegerExpression, GraphPatternTextError> {
        bind_integer(program, values, at)
            .map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))
    }

    /// A pattern WHERE scalar predicate: its program and its columns, each a
    /// variable with a property key or, for id()/elementId() (fgdb-j687q,
    /// fgdb-cdbie), the vertex or captured relationship itself (no key). A
    /// path column has no Boolean operand and refuses.
    #[allow(clippy::type_complexity)]
    pub(in crate::graph_text) fn boolean_scalar_expression(
        &mut self,
    ) -> Result<
        (
            Vec<(Name<'a>, Option<Name<'a>>)>,
            Vec<MutationIntegerTemplateOp>,
        ),
        GraphPatternTextError,
    > {
        let at = self.current.at;
        let mut columns = Vec::new();
        let operand = self
            .checked_expression_with_boundary(
                &mut ExpressionColumns::Graph(&mut columns),
                ExpressionBoundary::Predicate,
            )
            .map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))?;
        let program = match operand {
            Operand::Integer { program, .. } => program,
            Operand::Column(column) => vec![MutationIntegerTemplateOp::Bound(
                GraphIntegerOp::ScalarColumn(column),
            )],
            Operand::Literal(value) => vec![MutationIntegerTemplateOp::Bound(
                GraphIntegerOp::Scalar(value.predicate(IntegerComparison::Equal)),
            )],
            Operand::Number(Number::Literal(value)) => vec![MutationIntegerTemplateOp::Bound(
                GraphIntegerOp::Scalar(scalar(value, at)?.predicate(IntegerComparison::Equal)),
            )],
            Operand::Number(Number::Parameter(index)) => {
                vec![MutationIntegerTemplateOp::Parameter { index, at }]
            }
        };
        let columns = columns
            .into_iter()
            .map(|column| match (column.property, column.path) {
                (Some(key), _) => Ok((column.variable, Some(key))),
                // The element itself: a vertex, or a pattern relationship
                // (bound later as the captured edge).
                (None, None | Some(crate::graph_text::GraphPathFunction::Edge)) => {
                    Ok((column.variable, None))
                }
                (None, Some(_)) => Err(error(at, GraphPatternTextErrorKind::BooleanExpression)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((columns, program))
    }

    pub(super) fn mutation_expression(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Graph(columns), true)
    }

    pub(super) fn aggregate_value_expression(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
        predicates: bool,
    ) -> Result<Operand, GraphMutationTextError> {
        // HAVING owns predicates around GROUP BY keys and aggregate
        // arguments; a RETURN item's key is a whole expression (`predicates`).
        self.checked_expression(&mut ExpressionColumns::Graph(columns), predicates)
    }

    /// A whole expression over graph columns whose composite operands
    /// (list quantifiers, comprehensions, list functions, keys(), size())
    /// return beside it as let slots (fgdb-o5o9t).
    pub(super) fn mutation_expression_with_lets(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
    ) -> Result<(Operand, Vec<ReadValueTemplate>), GraphMutationTextError> {
        self.checked_expression_scoped(
            &mut ExpressionColumns::Graph(columns),
            ExpressionBoundary::Whole,
            true,
        )
    }

    /// One operand at the concatenation level in a graph scope, with its let
    /// slots: where `mutation_expression_with_lets` would read on through a
    /// comparison, AND or OR.
    pub(super) fn mutation_operand_with_lets(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
    ) -> Result<(Operand, Vec<ReadValueTemplate>), GraphMutationTextError> {
        self.checked_expression_scoped(
            &mut ExpressionColumns::Graph(columns),
            ExpressionBoundary::Operand,
            true,
        )
    }

    /// Add `value` as the next let slot of the expression being parsed and
    /// read it as an operand.
    fn let_slot(
        &mut self,
        program: &mut Vec<ParsedOp>,
        value: ReadValueTemplate,
        at: usize,
    ) -> Result<(), GraphMutationTextError> {
        let slots = self.lets.as_mut().expect("callers check the let sink");
        slots.push(value);
        let slot = slots.len() - 1;
        emit(program, ParsedOp::LetRef(slot), at)
    }

    /// One composite operand read in the value language, as a let slot
    /// carries it (fgdb-o5o9t).
    fn composite_value(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        nesting: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        Ok(match columns {
            ExpressionColumns::Graph(columns) => {
                self.read_composite_operand(Some(&mut **columns), &[], nesting)?
            }
            ExpressionColumns::Row(schema) => self.read_composite_operand(None, schema, nesting)?,
            ExpressionColumns::Resolved(resolve) if self.named_subscript_follows()? => {
                self.read_resolved_composite_operand(&mut **resolve, nesting)?
            }
            ExpressionColumns::Resolved(_) => {
                return Err(failure(at, GraphMutationTextErrorKind::IntegerOperand));
            }
        })
    }

    // The two let-slot readers below are out of line on purpose: their
    // templates would otherwise sit in `integer_unary`'s frame, which repeats
    // once per nested parenthesis up to MAX_INTEGER_NESTING, and a wider frame
    // overflows a test thread's stack at that depth (fgdb-9cawv).

    /// One composite operand read through a let slot (fgdb-o5o9t).
    #[inline(never)]
    fn composite_slot(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
        at: usize,
    ) -> Result<(), GraphMutationTextError> {
        let nesting = self.let_base + depth + 1;
        let value = self.composite_value(columns, nesting, at)?;
        self.let_slot(program, value, at)
    }

    /// A `+`/`-` chain with a list operand (sum_has_list), read whole by
    /// list_sum; false, with nothing consumed, for any other chain. Out of
    /// line: integer_sum repeats once per nested parenthesis.
    #[inline(never)]
    fn list_sum_here(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<bool, GraphMutationTextError> {
        if !self.sum_has_list(columns)? {
            return Ok(false);
        }
        self.list_sum(columns, depth, program)?;
        Ok(true)
    }

    /// A searched CASE with a list result (list_case_follows), read in the
    /// value language as one let slot: `CASE WHEN c THEN reverse(l) ELSE l
    /// END` (fgdb-13l85). False, with nothing consumed, for any other CASE,
    /// which keeps the scalar program. Out of line: integer_unary repeats
    /// once per nested parenthesis.
    #[inline(never)]
    fn list_case_here(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
        at: usize,
    ) -> Result<bool, GraphMutationTextError> {
        let row = match columns {
            ExpressionColumns::Row(schema) => Some(*schema),
            _ => None,
        };
        if !self.list_case_follows(row)? {
            return Ok(false);
        }
        let nesting = self.let_base + depth + 1;
        let value = self.list_case_value(columns, nesting, at)?;
        self.let_slot(program, value, at)?;
        Ok(true)
    }

    /// The Case template of a CASE with a list result, from its `CASE` to
    /// its `END`, read without handing the CASE back to the scalar compiler.
    fn list_case_value(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        nesting: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        self.advance()?;
        // A simple CASE compares its subject with each WHEN value: equality
        // is three-valued, so a NULL subject or value never selects.
        let subject = if self.is_word("WHEN") {
            None
        } else {
            Some(self.case_result(columns, nesting, at)?)
        };
        let mut arms = Vec::new();
        while self.take_word("WHEN")? {
            let mut condition = self.case_result(columns, nesting, at)?;
            if let Some(subject) = &subject {
                condition = ReadValueTemplate::Equal {
                    left: Box::new(subject.clone()),
                    right: Box::new(condition),
                };
            }
            self.word("THEN")?;
            arms.push((condition, self.case_result(columns, nesting, at)?));
        }
        if arms.is_empty() {
            self.word("WHEN")?;
        }
        let otherwise = if self.take_word("ELSE")? {
            Some(Box::new(self.case_result(columns, nesting, at)?))
        } else {
            None
        };
        self.word("END")?;
        Ok(ReadValueTemplate::Case { arms, otherwise })
    }

    /// One list operand of a list sum or CASE, read as a value: a list-typed
    /// row column or a list CASE directly, since the value reader would hand
    /// either back to the scalar compiler and re-read the whole chain, and
    /// any other list operand through the value reader.
    fn list_operand_value(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        nesting: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        let row = match columns {
            ExpressionColumns::Row(schema) => Some(*schema),
            _ => None,
        };
        if let Some(column) = self.row_list_column(row)? {
            self.advance()?;
            return Ok(ReadValueTemplate::Column(column));
        }
        // A vertex, edge or path column is a whole CASE result too.
        if let Some(column) = self.row_element_column(row)? {
            self.advance()?;
            return Ok(ReadValueTemplate::Column(column));
        }
        if self.list_case_follows(row)? {
            return self.list_case_value(columns, nesting, at);
        }
        // A list- or map-typed parameter is that argument's value. Read it
        // here: the composite reader would hand a bare parameter back to the
        // scalar compiler, which sees the same comparison again.
        if let TokenKind::Parameter(name) = self.current.kind
            && let Some(kind @ (GqlParameterType::List | GqlParameterType::Map)) =
                self.parameter_types.get(name).copied()
        {
            let at = self.current.at;
            let Number::Parameter(index) = self.number(kind)? else {
                unreachable!("the current native token is a parameter");
            };
            return Ok(ReadValueTemplate::parameter(index, at, kind));
        }
        self.composite_value(columns, nesting, at)
    }

    /// The list operand of `IN`. openCypher binds IN tighter than comparisons,
    /// AND, OR and XOR, but looser than additive list concatenation. Consume a
    /// complete list-valued +/- chain before bounding an ordinary value at the
    /// concatenation level (fgdb-klah2).
    fn membership_list(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        nesting: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        if self.sum_has_list(columns)? {
            return self.list_sum_value(columns, nesting, at);
        }
        let row = match columns {
            ExpressionColumns::Row(schema) => Some(*schema),
            _ => None,
        };
        if let Some(column) = self.row_list_column(row)? {
            self.advance()?;
            return Ok(ReadValueTemplate::Column(column));
        }
        if self.list_case_follows(row)? {
            return self.list_case_value(columns, nesting, at);
        }
        // Only the value read that starts here is bounded: it takes the flag
        // on entry, so nothing nested inside the operand inherits it.
        self.membership_list = true;
        let list = self.composite_value(columns, nesting, at);
        self.membership_list = false;
        list
    }

    /// One condition or result of a list CASE: a lone list operand read as
    /// a value, or a whole expression with its own let slots.
    fn case_result(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        nesting: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        let row = match columns {
            ExpressionColumns::Row(schema) => Some(*schema),
            _ => None,
        };
        if self.lone_case_result(row)? {
            return self.list_operand_value(columns, nesting, at);
        }
        let base = core::mem::replace(&mut self.let_base, nesting);
        let parsed = self.checked_expression_scoped(columns, ExpressionBoundary::Whole, true);
        self.let_base = base;
        let (operand, slots) = parsed?;
        Ok(self.scalar_let_value(operand, slots, at)?)
    }

    /// Whether a list or map literal (starts_composite_comparison) or a
    /// list-typed row column (list_column_compared) is compared here. Out of
    /// line: integer_unary repeats once per nested parenthesis.
    #[inline(never)]
    fn composite_comparison_follows(
        &self,
        columns: &ExpressionColumns<'_, 'a>,
    ) -> Result<bool, GraphMutationTextError> {
        if self.starts_composite_comparison()? {
            return Ok(true);
        }
        let row = match columns {
            ExpressionColumns::Row(schema) => Some(*schema),
            _ => None,
        };
        Ok(self.list_column_compared(row)?)
    }

    /// Whether the `+`/`-` chain here has a list operand, which the value
    /// language reads whole (fgdb-c4i0d).
    fn sum_has_list(
        &self,
        columns: &ExpressionColumns<'_, 'a>,
    ) -> Result<bool, GraphMutationTextError> {
        let row = match columns {
            ExpressionColumns::Row(schema) => Some(*schema),
            _ => None,
        };
        Ok(self.list_chain_follows(row)?)
    }

    /// openCypher list concatenation: a `+`/`-` chain with a list operand
    /// (sum_has_list), read whole in the value language as one let slot.
    /// Each operand is a list value or a scalar product, left-associative,
    /// so `+` keeps binding looser than `*` and tighter than comparisons.
    #[inline(never)]
    fn list_sum(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        let at = self.current.at;
        let nesting = self.let_base + depth + 1;
        let value = self.list_sum_value(columns, nesting, at)?;
        self.let_slot(program, value, at)
    }

    /// Value form of list_sum. Membership uses the same additive operand
    /// without first materializing it as a scalar let slot.
    fn list_sum_value(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        nesting: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        let mut value = self.sum_operand(columns, nesting, at)?;
        loop {
            let function = if self.take(b'+')? {
                crate::GraphValueFunction::Add
            } else if self.take(b'-')? {
                crate::GraphValueFunction::Subtract
            } else {
                break;
            };
            let right = self.sum_operand(columns, nesting, at)?;
            value = ReadValueTemplate::Function {
                function,
                argument: Box::new(ReadValueTemplate::List(vec![value, right])),
            };
        }
        Ok(value)
    }

    /// One operand of a list sum: a list value with its postfix reads, or a
    /// scalar product with its own let slots.
    fn sum_operand(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        nesting: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        let row = match columns {
            ExpressionColumns::Row(schema) => Some(*schema),
            _ => None,
        };
        if self.list_operand_here(row)? {
            return self.list_operand_value(columns, nesting, at);
        }
        let base = core::mem::replace(&mut self.let_base, nesting);
        let parsed = self.checked_expression_scoped(columns, ExpressionBoundary::Product, true);
        self.let_base = base;
        let (operand, slots) = parsed?;
        Ok(self.scalar_let_value(operand, slots, at)?)
    }

    /// Whether a lone operand (a number or text literal, a parameter, a
    /// name with optional `.key` reads, TRUE/FALSE/NULL, or a parenthesized
    /// expression such as `(a = b)`) stands here directly before IN, and the
    /// list after IN is not a `[` literal.
    fn starts_operand_membership(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let after = match self.current.kind {
            TokenKind::Digits(_) | TokenKind::Quoted(_) | TokenKind::Parameter(_) => {
                lexer.next()?
            }
            // TCK Precedence1: `(a = b) IN c` over a list column.
            TokenKind::Punct(b'(') => {
                super::return_projection::skip_group(&mut lexer)?;
                lexer.next()?
            }
            TokenKind::Word(_) => {
                let mut token = lexer.next()?;
                while matches!(token.kind, TokenKind::Punct(b'.')) {
                    if !matches!(
                        lexer.next()?.kind,
                        TokenKind::Word(_) | TokenKind::DelimitedKeyword(_)
                    ) {
                        return Ok(false);
                    }
                    token = lexer.next()?;
                }
                token
            }
            _ => return Ok(false),
        };
        if !matches!(after.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("IN")) {
            return Ok(false);
        }
        if !matches!(lexer.next()?.kind, TokenKind::Punct(b'[')) {
            return Ok(true);
        }
        // A list literal keeps the scalar IN, unless a slice, subscript or
        // additive list concatenation reads it (`3 IN [1, 2, 3][0..1]`,
        // `2 IN [1] + 2`): then the list is a value.
        super::return_projection::skip_group(&mut lexer)?;
        Ok(matches!(
            lexer.next()?.kind,
            TokenKind::Punct(b'[' | b'+' | b'-')
        ))
    }

    /// `operand IN value` whose list is not a literal (a name, a parameter,
    /// keys(n), list[0], list[0..1]) is one membership in the value language,
    /// as `[..] IN list` is (fgdb-9cawv). Only a lone operand directly before
    /// IN qualifies, so `1 + x IN l` keeps `1 + x` as its left side; a list
    /// literal keeps the scalar IN. False when this is not such a membership.
    /// Out of line: scalar_predicate repeats once per nested parenthesis up to
    /// MAX_INTEGER_NESTING, and these temporaries would widen every frame.
    #[inline(never)]
    fn operand_membership(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<bool, GraphMutationTextError> {
        if self.lets.is_none() || !self.starts_operand_membership()? {
            return Ok(false);
        }
        let at = self.current.at;
        self.composite_comparison(columns, depth, program, at, true)?;
        Ok(true)
    }

    /// The left side of `operand IN value` (starts_operand_membership): one
    /// operand at the concatenation level, with its own let slots, as a value.
    fn membership_operand(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        nesting: usize,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        let base = core::mem::replace(&mut self.let_base, nesting);
        let parsed = self.checked_expression_scoped(columns, ExpressionBoundary::Operand, true);
        self.let_base = base;
        let (operand, slots) = parsed?;
        Ok(self.scalar_let_value(operand, slots, at)?)
    }

    /// `[..] = rhs`, `{..} <> rhs` or `[..] IN list`, read whole in the
    /// value language as one let slot (fgdb-9cawv); `<>` is NOT of the
    /// equality. `operand IN value` with a non-literal list takes the same
    /// path (starts_operand_membership).
    #[inline(never)]
    fn composite_comparison(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
        at: usize,
        lone_operand: bool,
    ) -> Result<(), GraphMutationTextError> {
        let nesting = self.let_base + depth + 1;
        // A lone scalar operand is read up to the IN and no further: read as a
        // whole expression it would rediscover this same membership.
        let left = if lone_operand {
            self.membership_operand(columns, nesting, at)?
        } else {
            // A literal, or a list-typed row column read as its value.
            self.list_operand_value(columns, nesting, at)?
        };
        let (value, negate) = if self.take_word("IN")? {
            let list = self.membership_list(columns, nesting, at)?;
            let value = ReadValueTemplate::In {
                value: Box::new(left),
                list: Box::new(list),
            };
            (value, false)
        } else {
            let comparison = self.comparison()?;
            let mut right = self.list_operand_value(columns, nesting, at)?;
            // IN binds tighter than a comparison, as in the scalar grammar:
            // `[1, 2] = [3, 4] IN l` compares with the membership
            // (openCypher Precedence3 [6]).
            while self.take_word("IN")? {
                let list = self.membership_list(columns, nesting, at)?;
                right = ReadValueTemplate::In {
                    value: Box::new(right),
                    list: Box::new(list),
                };
            }
            let (left, right) = (Box::new(left), Box::new(right));
            match comparison {
                IntegerComparison::Equal => (ReadValueTemplate::Equal { left, right }, false),
                IntegerComparison::NotEqual => (ReadValueTemplate::Equal { left, right }, true),
                // A list literal's `<`, `<=`, `>` or `>=`.
                comparison => (
                    ReadValueTemplate::Ordered {
                        comparison,
                        left,
                        right,
                    },
                    false,
                ),
            }
        };
        self.let_slot(program, value, at)?;
        if negate {
            emit(program, ParsedOp::Bound(GraphIntegerOp::Not), at)?;
        }
        Ok(())
    }

    /// `m.key` of a dynamically typed row column, such as a map from WITH or
    /// UNWIND, reads the entry through a let slot (fgdb-o5o9t), and so does a
    /// temporal field of a scalar column (a stored date's `.year`). A carried
    /// vertex's `n.p` is a Vertex column read at the boundary. False, with
    /// nothing consumed, when the operand here is not such a read. Out of line
    /// so its template and results never widen integer_unary's frame, which
    /// repeats once per nested parenthesis.
    #[inline(never)]
    fn row_entry_slot(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        program: &mut Vec<ParsedOp>,
        at: usize,
    ) -> Result<bool, GraphMutationTextError> {
        if self.lets.is_some()
            && let ExpressionColumns::Row(schema) = columns
            && let TokenKind::Word(word) = self.current.kind
            && !self.elements.contains(&word)
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
            && let Some(column) = schema[..self.visible_width(schema)]
                .iter()
                .position(|(name, _)| name.text == word)
            && match schema[column].1 {
                crate::GraphSetColumnType::Any => true,
                crate::GraphSetColumnType::Scalar | crate::GraphSetColumnType::Stored => {
                    self.temporal_field_follows()?
                }
                _ => false,
            }
        {
            self.advance()?;
            let value = self.map_entries(ReadValueTemplate::Column(column))?;
            self.let_slot(program, value, at)?;
            return Ok(true);
        }
        Ok(false)
    }

    /// A graph function of literal NULL (`type(null)`, `nodes(null)`) is NULL
    /// (TCK Graph4 [3], Path1 [1], Path2 [3]); false, with nothing consumed,
    /// otherwise. Out of line: integer_unary repeats once per nesting level.
    #[inline(never)]
    fn null_graph_function(
        &mut self,
        program: &mut Vec<ParsedOp>,
        at: usize,
    ) -> Result<bool, GraphMutationTextError> {
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(false);
        };
        let mut lexer = self.lexer.clone();
        if Parser::path_function(Name { text: word, at }).is_err()
            || !matches!(lexer.next()?.kind, TokenKind::Punct(b'('))
            || !matches!(lexer.next()?.kind, TokenKind::Word(null) if null.eq_ignore_ascii_case("NULL"))
            || !matches!(lexer.next()?.kind, TokenKind::Punct(b')'))
        {
            return Ok(false);
        }
        for _ in 0..4 {
            self.advance()?;
        }
        let null = GqlScalarParameter::new(CanonicalScalar::Null)
            .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
        emit(program, ParsedOp::Atom(Operand::Literal(null), at), at)?;
        Ok(true)
    }

    /// Whether `.field` follows the current name and names a temporal field,
    /// the only read a scalar column can answer with a value.
    fn temporal_field_follows(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        lexer.next()?;
        Ok(matches!(
            lexer.next()?.kind,
            TokenKind::Word(key) | TokenKind::DelimitedKeyword(key)
                if crate::set_ops::temporal_field_name(key)
        ))
    }

    /// `.key` reads after `value`, each one a map entry.
    fn map_entries(
        &mut self,
        mut value: ReadValueTemplate,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        while self.is_punct(b'.')
            && matches!(
                self.lexer.clone().next()?.kind,
                TokenKind::Word(_) | TokenKind::DelimitedKeyword(_)
            )
        {
            self.advance()?;
            let key = self.schema_name()?;
            value = self.map_get(value, key.text);
        }
        Ok(value)
    }

    /// The row-scope entry the compiler laws drive: the production path,
    /// with no composite operands to return.
    #[cfg(test)]
    fn row_expression(
        &mut self,
        columns: &[(Name<'a>, crate::GraphSetColumnType)],
    ) -> Result<Operand, GraphMutationTextError> {
        self.row_expression_with_lets(columns)
            .map(|(operand, _)| operand)
    }

    /// [`Self::mutation_expression_with_lets`] over a row scope.
    pub(super) fn row_expression_with_lets(
        &mut self,
        columns: &[(Name<'a>, crate::GraphSetColumnType)],
    ) -> Result<(Operand, Vec<ReadValueTemplate>), GraphMutationTextError> {
        self.checked_expression_scoped(
            &mut ExpressionColumns::Row(columns),
            ExpressionBoundary::Whole,
            true,
        )
    }

    /// One operand at the concatenation level in a row scope, with its let
    /// slots (mutation_operand_with_lets).
    pub(super) fn row_operand_with_lets(
        &mut self,
        columns: &[(Name<'a>, crate::GraphSetColumnType)],
    ) -> Result<(Operand, Vec<ReadValueTemplate>), GraphMutationTextError> {
        self.checked_expression_scoped(
            &mut ExpressionColumns::Row(columns),
            ExpressionBoundary::Operand,
            true,
        )
    }

    /// A whole Boolean predicate over resolved leaves: a list-comprehension
    /// WHERE in the aggregate-resolution scope (fgdb-20foe).
    pub(super) fn resolved_predicate(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Resolved(resolve), true)
    }

    /// [`Self::resolved_predicate`] with its own let sink: a composite operand
    /// or a list element's entry (`x.key`) reads as a let slot, as it does in
    /// a graph or row scope. A quantifier or comprehension WHERE over row
    /// aliases (a MATCH WHERE deferred past an implicit WITH *, fgdb-1c29d)
    /// reads element properties this way.
    pub(super) fn resolved_predicate_with_lets(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
    ) -> Result<(Operand, Vec<ReadValueTemplate>), GraphMutationTextError> {
        self.checked_expression_scoped(
            &mut ExpressionColumns::Resolved(resolve),
            ExpressionBoundary::Whole,
            true,
        )
    }

    /// One operand over resolved leaves, with its own let sink for element
    /// and map entries (`x.p`, `row.key`).
    pub(super) fn resolved_expression(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
    ) -> Result<(Operand, Vec<ReadValueTemplate>), GraphMutationTextError> {
        self.checked_expression_scoped(
            &mut ExpressionColumns::Resolved(resolve),
            ExpressionBoundary::Operand,
            true,
        )
    }

    fn checked_expression(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        predicates: bool,
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression_with_boundary(
            columns,
            if predicates {
                ExpressionBoundary::Whole
            } else {
                ExpressionBoundary::Operand
            },
        )
    }

    fn checked_expression_with_boundary(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        boundary: ExpressionBoundary,
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression_scoped(columns, boundary, false)
            .map(|(operand, _)| operand)
    }

    /// One expression with its own let sink: `lets` admits composite
    /// operands, and a nested expression never sees or fills the enclosing
    /// expression's slots.
    fn checked_expression_scoped(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        boundary: ExpressionBoundary,
        lets: bool,
    ) -> Result<(Operand, Vec<ReadValueTemplate>), GraphMutationTextError> {
        let outer = core::mem::replace(&mut self.lets, lets.then(Vec::new));
        let operand = self.checked_expression_body(columns, boundary);
        let slots = core::mem::replace(&mut self.lets, outer).unwrap_or_default();
        operand.map(|operand| (operand, slots))
    }

    fn checked_expression_body(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        boundary: ExpressionBoundary,
    ) -> Result<Operand, GraphMutationTextError> {
        let at = self.current.at;
        let mut parsed = Vec::new();
        match boundary {
            ExpressionBoundary::Whole => self.scalar_boolean(columns, 0, &mut parsed)?,
            // The graph Boolean parser owns AND/OR between its leaves. Consuming
            // them here would regroup A AND scalar(B) OR C as A AND (B OR C).
            // Parentheses still parse their complete internal Boolean expression.
            ExpressionBoundary::Predicate => self.scalar_negation(columns, 0, &mut parsed)?,
            // HAVING owns comparisons and Boolean operators around each operand.
            ExpressionBoundary::Operand => self.scalar_concat(columns, 0, &mut parsed)?,
            ExpressionBoundary::Product => self.integer_product(columns, 0, &mut parsed)?,
        }
        // A lone list element is a one-instruction program, not an atom.
        if parsed.len() == 1 && matches!(parsed[0], ParsedOp::Atom(..)) {
            let ParsedOp::Atom(value, _) = parsed.pop().expect("one parsed operand") else {
                unreachable!("operators and CASE also contain their operands")
            };
            // Preserve all prior scalar assignments and their transcript bytes.
            // An arithmetic operator/function, not parentheses, selects i64.
            return Ok(value);
        }
        // The body of a Let reads the enclosing row extended by its slots, so
        // an enclosing element sits `slots` further from the end.
        let slots = self.lets.as_ref().map_or(0, Vec::len);
        let mut program = Vec::with_capacity(parsed.len());
        for op in parsed {
            program.push(match op {
                ParsedOp::Bound(GraphIntegerOp::Local(offset)) => {
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Local(offset + slots))
                }
                ParsedOp::LetRef(slot) => {
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Local(slots - 1 - slot))
                }
                ParsedOp::Bound(op) => MutationIntegerTemplateOp::Bound(op),
                ParsedOp::ParameterField { index, keys, at } => {
                    MutationIntegerTemplateOp::ParameterField { index, keys, at }
                }
                ParsedOp::ListMembers { index, at } => {
                    MutationIntegerTemplateOp::ListMembers { index, at }
                }
                ParsedOp::Unary(op) => MutationIntegerTemplateOp::Bound(GraphIntegerOp::Unary(op)),
                ParsedOp::Binary(op) => {
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Binary(op))
                }
                ParsedOp::Coalesce => MutationIntegerTemplateOp::Bound(GraphIntegerOp::Coalesce),
                ParsedOp::Atom(Operand::Column(column), at) => {
                    // Refuse only columns that can never hold a scalar. A
                    // dynamically typed column (an UNWIND element, an Any
                    // projection) is checked per value by the evaluator, which
                    // answers a typed NonScalar/NonInteger error, never a panic.
                    if let ExpressionColumns::Row(schema) = columns
                        && !schema[column].1.is_scalar_like()
                        && schema[column].1 != crate::GraphSetColumnType::Any
                    {
                        return Err(failure(at, GraphMutationTextErrorKind::IntegerOperand));
                    }
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::ScalarColumn(column))
                }
                ParsedOp::Atom(Operand::Literal(value), _) => MutationIntegerTemplateOp::Bound(
                    GraphIntegerOp::Scalar(value.predicate(IntegerComparison::Equal)),
                ),
                ParsedOp::Atom(Operand::Number(Number::Literal(value)), at) => {
                    MutationIntegerTemplateOp::Bound(GraphIntegerOp::Scalar(
                        scalar(value, at)?.predicate(IntegerComparison::Equal),
                    ))
                }
                ParsedOp::Atom(Operand::Number(Number::Parameter(index)), at) => {
                    // This compiler also serves text and Boolean expressions.
                    // Binding substitutes the declared scalar and validates its
                    // operator context with GraphIntegerExpression::prepare_scalar.
                    let parameter_type = self.syntax.parameters[index].parameter_type;
                    if !matches!(
                        parameter_type,
                        GqlParameterType::Int64 | GqlParameterType::Scalar(_)
                    ) {
                        return Err(failure(at, GraphMutationTextErrorKind::IntegerOperand));
                    }
                    MutationIntegerTemplateOp::Parameter { index, at }
                }
                ParsedOp::Atom(Operand::Integer { .. }, _) => {
                    unreachable!("atoms never recurse into expression preparation")
                }
            });
        }
        // Validate declared scalar kinds before catalog access. These values
        // are type witnesses only; preparation never executes the program.
        let mut noninteger_parameter = None;
        let shape: Vec<_> = program
            .iter()
            .map(|op| match op {
                MutationIntegerTemplateOp::Bound(op) => Ok(op.clone()),
                MutationIntegerTemplateOp::ParameterField { .. } => {
                    // A field's kind is known only after the Map argument is
                    // bound. NULL checks structure without assuming an integer
                    // or text domain; bind_integer validates the actual scalar.
                    Ok(GraphIntegerOp::Literal(None))
                }
                // The members arrive at binding; an empty membership has the
                // same shape (one operand in, one Boolean out).
                MutationIntegerTemplateOp::ListMembers { .. } => {
                    Ok(GraphIntegerOp::InList { members: 0 })
                }
                MutationIntegerTemplateOp::Parameter { index, at } => {
                    let kind = self.syntax.parameters[*index].parameter_type;
                    let witness = match kind {
                        GqlParameterType::Int64
                        | GqlParameterType::Scalar(CanonicalScalarKind::Int) => {
                            GraphIntegerOp::Literal(Some(0))
                        }
                        GqlParameterType::Scalar(CanonicalScalarKind::Null) => {
                            GraphIntegerOp::Literal(None)
                        }
                        GqlParameterType::Scalar(CanonicalScalarKind::Bool) => {
                            noninteger_parameter.get_or_insert(*at);
                            GraphIntegerOp::Truth(Some(false))
                        }
                        GqlParameterType::Scalar(CanonicalScalarKind::Float) => {
                            noninteger_parameter.get_or_insert(*at);
                            // Keep the declared domain: an integer witness would
                            // admit integer-only uses and change numeric lowering.
                            // This value is never executed, even in a denominator.
                            GraphIntegerOp::Scalar(
                                crate::GqlScalarParameter::new(CanonicalScalar::Float(
                                    fgdb_types::CanonicalF64::new(0.0),
                                ))
                                .expect("zero float is an admitted scalar")
                                .predicate(IntegerComparison::Equal),
                            )
                        }
                        GqlParameterType::Scalar(CanonicalScalarKind::Decimal) => {
                            noninteger_parameter.get_or_insert(*at);
                            // Preserve the declared exact domain during shape
                            // validation. This zero is never evaluated: binding
                            // still substitutes the caller's decimal or NULL.
                            GraphIntegerOp::Scalar(
                                crate::GqlScalarParameter::new(CanonicalScalar::Decimal(
                                    fgdb_types::CanonicalDecimal::default(),
                                ))
                                .expect("zero decimal is an admitted scalar")
                                .predicate(IntegerComparison::Equal),
                            )
                        }
                        GqlParameterType::Scalar(CanonicalScalarKind::Text) => {
                            noninteger_parameter.get_or_insert(*at);
                            GraphIntegerOp::Scalar(
                                crate::GqlScalarParameter::new(
                                    CanonicalScalar::ucs_basic_text("")
                                        .expect("empty text is canonical"),
                                )
                                .expect("empty text is an admitted scalar")
                                .predicate(IntegerComparison::Equal),
                            )
                        }
                        _ => return Err(failure(*at, GraphMutationTextErrorKind::IntegerOperand)),
                    };
                    Ok(witness)
                }
            })
            .collect::<Result<_, _>>()?;
        GraphIntegerExpression::prepare_scalar(&shape).map_err(|error| {
            if matches!(error, GraphIntegerBuildError::OperandType { .. })
                && let Some(parameter_at) = noninteger_parameter
            {
                failure(parameter_at, GraphMutationTextErrorKind::IntegerOperand)
            } else {
                failure(at, GraphMutationTextErrorKind::IntegerExpression(error))
            }
        })?;
        Ok(Operand::Integer { program, at })
    }

    fn scalar_boolean(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.scalar_exclusive(columns, depth, program)?;
        while self.take_word("OR")? {
            let at = self.current.at;
            self.scalar_exclusive(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::Or), at)?;
        }
        Ok(())
    }

    /// `XOR` binds tighter than `OR` and looser than `AND`, as in openCypher.
    fn scalar_exclusive(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.scalar_conjunction(columns, depth, program)?;
        while self.take_word("XOR")? {
            let at = self.current.at;
            self.scalar_conjunction(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::Xor), at)?;
        }
        Ok(())
    }

    fn scalar_conjunction(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.scalar_negation(columns, depth, program)?;
        while self.is_word("AND") {
            // Scoped EXISTS is owned by the graph clause parser.
            let mut lexer = self.lexer.clone();
            let mut next = lexer.next()?;
            if matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT")) {
                next = lexer.next()?;
            }
            if matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("EXISTS")) {
                break;
            }
            self.advance()?;
            let at = self.current.at;
            self.scalar_negation(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::And), at)?;
        }
        Ok(())
    }

    fn scalar_negation(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        if depth > MAX_INTEGER_NESTING {
            return Err(failure(
                self.current.at,
                GraphMutationTextErrorKind::IntegerNesting {
                    limit: MAX_INTEGER_NESTING,
                },
            ));
        }
        if self.is_word("NOT") && !matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
        {
            let at = self.current.at;
            self.advance()?;
            self.scalar_negation(columns, depth + 1, program)?;
            return emit(program, ParsedOp::Bound(GraphIntegerOp::Not), at);
        }
        self.scalar_comparison(columns, depth, program)
    }

    /// openCypher's comparison level: `=`, `<>`, `<`, `>`, `<=`, `>=` between
    /// predicate-level operands. A chain compares neighbours: `a < b <= c` is
    /// `a < b AND b <= c` (TCK Comparison3, Comparison4 [1]). The string, list
    /// and null predicates bind tighter, so `a = b IS NULL` is `a = (b IS
    /// NULL)` and `a = b IN [..]` is `a = (b IN [..])` (TCK Precedence1 [8],
    /// [11]).
    fn scalar_comparison(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.scalar_predicate(columns, depth, program)?;
        if matches!(
            self.current.kind,
            TokenKind::Punct(b'=' | b'!' | b'<' | b'>')
        ) {
            self.comparison_tail(columns, depth, program)?;
        }
        Ok(())
    }

    // The operator loops of the levels below are out of line, each entered
    // only at its operator: these levels repeat once per nested parenthesis
    // up to MAX_INTEGER_NESTING, and a loop's locals and temporaries would
    // widen every frame of a deeply parenthesized operand.

    /// The comparisons after a comparison level's first operand.
    #[inline(never)]
    fn comparison_tail(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        // Where the previous comparison's right operand starts, once one has
        // been read: a chained comparison reads it again as its left operand.
        let mut previous = None;
        while matches!(
            self.current.kind,
            TokenKind::Punct(b'=' | b'!' | b'<' | b'>')
        ) {
            let at = self.current.at;
            let chained = previous.is_some();
            if let Some(start) = previous {
                self.chained_operand(columns, depth, program, start)?;
            }
            let comparison = self.comparison()?;
            previous = Some(self.current.at);
            self.scalar_predicate(columns, depth, program)?;
            emit(
                program,
                ParsedOp::Bound(GraphIntegerOp::Compare(comparison)),
                at,
            )?;
            if chained {
                emit(program, ParsedOp::Bound(GraphIntegerOp::And), at)?;
            }
        }
        Ok(())
    }

    /// The left operand of a chained comparison: the previous comparison's
    /// right operand, read again from `start`, its first token. The text is
    /// the same, so the reading ends where it ended before, at this
    /// comparison's operator, and the token count is left as it was. Out of
    /// line: scalar_comparison repeats once per nested parenthesis.
    #[inline(never)]
    fn chained_operand(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
        start: usize,
    ) -> Result<(), GraphMutationTextError> {
        let (resume, tokens) = (self.current.at, self.lexer.tokens);
        self.lexer = crate::graph_text::Lexer {
            text: self.lexer.text,
            at: start,
            tokens,
        };
        self.current = self.lexer.next()?;
        self.scalar_predicate(columns, depth, program)?;
        debug_assert_eq!(
            self.current.at, resume,
            "a reread operand ends where it did"
        );
        self.lexer.tokens = tokens;
        Ok(())
    }

    /// openCypher's string, list and null predicate level: STARTS WITH,
    /// ENDS WITH, CONTAINS, `=~`, [NOT] IN and IS [NOT] NULL, applied left to
    /// right to one concatenation-level operand.
    fn scalar_predicate(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        if self.lets.is_some() && self.sum_has_list(columns)? {
            // IN binds below additive list concatenation (Precedence3):
            // parse the complete left list sum before deciding membership.
            let at = self.current.at;
            let nesting = self.let_base + depth + 1;
            let left = self.list_sum_value(columns, nesting, at)?;
            let membership = self.is_word("IN")
                || (self.is_word("NOT")
                    && matches!(
                        self.lexer.clone().next()?.kind,
                        TokenKind::Word(word) if word.eq_ignore_ascii_case("IN")
                    ));
            if membership {
                let negate = self.take_word("NOT")?;
                self.word("IN")?;
                let list = self.membership_list(columns, nesting, at)?;
                self.let_slot(
                    program,
                    ReadValueTemplate::In {
                        value: Box::new(left),
                        list: Box::new(list),
                    },
                    at,
                )?;
                if negate {
                    emit(program, ParsedOp::Bound(GraphIntegerOp::Not), at)?;
                }
            } else {
                self.let_slot(program, left, at)?;
                if self.is_punct(b'|') {
                    self.concat_tail(columns, depth, program)?;
                }
            }
        } else if !self.operand_membership(columns, depth, program)? {
            self.scalar_concat(columns, depth, program)?;
        }
        if matches!(
            self.current.kind,
            TokenKind::Word(_) | TokenKind::Punct(b'=')
        ) {
            self.predicate_tail(columns, depth, program)?;
        }
        Ok(())
    }

    /// The string, list and null predicates after a predicate level's operand.
    #[inline(never)]
    fn predicate_tail(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        loop {
            let at = self.current.at;
            let op = if self.take_word("STARTS")? {
                self.word("WITH")?;
                Some(GraphIntegerOp::StartsWith)
            } else if self.take_word("ENDS")? {
                self.word("WITH")?;
                Some(GraphIntegerOp::EndsWith)
            } else if self.take_word("CONTAINS")? {
                Some(GraphIntegerOp::Contains)
            } else {
                None
            };
            if let Some(op) = op {
                self.scalar_concat(columns, depth, program)?;
                emit(program, ParsedOp::Bound(op), at)?;
                continue;
            }
            if self.is_word("IN")
                || (self.is_word("NOT")
                    && matches!(self.lexer.clone().next()?.kind,
                TokenKind::Word(word) if word.eq_ignore_ascii_case("IN")))
            {
                let negate = self.take_word("NOT")?;
                self.word("IN")?;
                if let TokenKind::Parameter(name) = self.current.kind
                    && matches!(self.parameter_types.get(name), Some(GqlParameterType::List))
                {
                    // `n.p IN $ids`: binding pushes the argument's members as
                    // the literal list's operands, so the same three-valued
                    // InList decides (fgdb-dn9q3).
                    let list_at = self.current.at;
                    let Number::Parameter(index) = self.number(GqlParameterType::List)? else {
                        unreachable!("the current native token is a parameter");
                    };
                    emit(program, ParsedOp::ListMembers { index, at: list_at }, at)?;
                } else {
                    self.punct(b'[', "[")?;
                    let mut members = 0;
                    if !self.take(b']')? {
                        loop {
                            self.scalar_concat(columns, depth + 1, program)?;
                            members += 1;
                            if self.take(b']')? {
                                break;
                            }
                            self.punct(b',', ",")?;
                        }
                    }
                    emit(
                        program,
                        ParsedOp::Bound(GraphIntegerOp::InList { members }),
                        at,
                    )?;
                }
                if negate {
                    emit(program, ParsedOp::Bound(GraphIntegerOp::Not), at)?;
                }
                continue;
            }
            if self.take_word("IS")? {
                let negate = self.take_word("NOT")?;
                self.word("NULL")?;
                emit(
                    program,
                    ParsedOp::Bound(GraphIntegerOp::IsNull(!negate)),
                    at,
                )?;
                continue;
            }
            // openCypher `text =~ 'pattern'`: the pattern is a text literal,
            // compiled once here into fgdb regex profile 1 (fgdb-20foe).
            if self.is_punct(b'=')
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'~'))
            {
                self.advance()?;
                self.advance()?;
                let pattern_at = self.current.at;
                let expected = |item| {
                    failure(
                        pattern_at,
                        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
                            item,
                        )),
                    )
                };
                let TokenKind::Quoted(raw) = self.current.kind else {
                    return Err(expected("a regular-expression text literal after =~"));
                };
                let CanonicalScalar::Text(pattern) =
                    crate::graph_text::literal::text_scalar(raw, pattern_at)?
                else {
                    return Err(expected("a regular-expression text literal after =~"));
                };
                let regex = crate::regex::CompiledRegex::compile(pattern.as_str())
                    .map_err(|_| expected("a regular expression in fgdb regex profile 1"))?;
                self.advance()?;
                emit(
                    program,
                    ParsedOp::Bound(GraphIntegerOp::Matches(Box::new(regex))),
                    at,
                )?;
                continue;
            }
            break;
        }
        Ok(())
    }

    fn scalar_concat(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.integer_sum(columns, depth, program)?;
        if self.is_punct(b'|') {
            self.concat_tail(columns, depth, program)?;
        }
        Ok(())
    }

    /// The `||` concatenations after a concatenation level's operand.
    #[inline(never)]
    fn concat_tail(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        // Only `||` continues an expression. A lone `|` ends it: it separates
        // a list comprehension's predicate from its projection (fgdb-20foe).
        while self.is_punct(b'|')
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'|'))
        {
            self.advance()?;
            let at = self.current.at;
            self.punct(b'|', "||")?;
            self.integer_sum(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::Concat), at)?;
        }
        Ok(())
    }

    fn integer_sum(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        if self.lets.is_some() && self.list_sum_here(columns, depth, program)? {
            return Ok(());
        }
        self.integer_product(columns, depth, program)?;
        if matches!(self.current.kind, TokenKind::Punct(b'+' | b'-')) {
            self.sum_tail(columns, depth, program)?;
        }
        Ok(())
    }

    /// The `+` and `-` operands after a sum level's first product.
    #[inline(never)]
    fn sum_tail(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        loop {
            let at = self.current.at;
            let op = if self.take(b'+')? {
                GraphIntegerBinary::Add
            } else if self.take(b'-')? {
                GraphIntegerBinary::Subtract
            } else {
                break;
            };
            let right = program.len();
            self.integer_product(columns, depth, program)?;
            // openCypher `+` concatenates when either operand is statically
            // text (fgdb-j687q), compiling to exactly the program `||` does.
            // An operand of unknown type keeps integer addition, and a
            // text/integer mix is the typed Concat operand refusal.
            let concat = matches!(op, GraphIntegerBinary::Add)
                && (static_text(program[..right].last()) || static_text(program.last()));
            if concat {
                emit(program, ParsedOp::Bound(GraphIntegerOp::Concat), at)?;
            } else {
                emit(program, ParsedOp::Binary(op), at)?;
            }
        }
        Ok(())
    }
    fn integer_product(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.integer_unary(columns, depth, program)?;
        if self.is_punct(b'^') {
            self.power_tail(columns, depth, program)?;
        }
        if matches!(self.current.kind, TokenKind::Punct(b'*' | b'/' | b'%')) {
            self.product_tail(columns, depth, program)?;
        }
        Ok(())
    }

    /// The `*`, `/` and `%` operands after a product level's first factor.
    #[inline(never)]
    fn product_tail(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        loop {
            let at = self.current.at;
            let op = if self.take(b'*')? {
                GraphIntegerBinary::Multiply
            } else if self.take(b'/')? {
                GraphIntegerBinary::Divide
            } else if self.take(b'%')? {
                GraphIntegerBinary::Remainder
            } else {
                break;
            };
            self.integer_unary(columns, depth, program)?;
            if self.is_punct(b'^') {
                self.power_tail(columns, depth, program)?;
            }
            emit(program, ParsedOp::Binary(op), at)?;
        }
        Ok(())
    }

    /// openCypher `^` after a unary operand, left-associative and binding
    /// tighter than `*`, `/` and `%`, looser than a sign: `-3 ^ 2` is
    /// `(-3) ^ 2`, `4 ^ 3 ^ 2` is `(4 ^ 3) ^ 2` (TCK Precedence2). Out of line,
    /// and entered only at a `^`, so the per-parenthesis frames are unchanged.
    #[inline(never)]
    fn power_tail(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        while self.is_punct(b'^') {
            let at = self.current.at;
            self.advance()?;
            self.integer_unary(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::Power), at)?;
        }
        Ok(())
    }

    /// Whether an openCypher `length(...)` call at the current token counts
    /// the characters of text, as CHAR_LENGTH (fgdb-20foe). Any argument but
    /// one lone name does, and so does a list element. Any other lone name
    /// does unless it names a path: a MATCH path, a path a WITH carried for
    /// a length read, or a row column of paths. Those stay with the
    /// graph-function readers, which answer the path's hop count. Nothing is
    /// consumed. Out of line: integer_unary repeats once per nested
    /// parenthesis.
    #[inline(never)]
    fn text_length_call(
        &self,
        columns: &ExpressionColumns<'_, 'a>,
    ) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("LENGTH") {
            return Ok(false);
        }
        let Some(argument) = Self::length_argument(self.lexer.clone())? else {
            return Ok(false);
        };
        let Some(name) = argument else {
            return Ok(true);
        };
        if self.elements.contains(&name) {
            return Ok(true);
        }
        let carried = self.boundary_reads.as_ref().is_some_and(|boundary| {
            (boundary.functions.iter())
                .any(|&(read, function, _)| read == name && function == GraphPathFunction::Length)
        });
        let row_path = match columns {
            ExpressionColumns::Row(schema) => schema.iter().any(|(column, kind)| {
                column.text == name && *kind == crate::GraphSetColumnType::Path
            }),
            _ => false,
        };
        Ok(!self.syntax.names_path(name) && !carried && !row_path)
    }

    /// openCypher pi() and e(), just after their `(`: the binary64 nearest
    /// each constant. False, with nothing consumed, for any other function.
    /// Out of line: integer_unary repeats once per nested parenthesis.
    #[inline(never)]
    fn constant_function(
        &mut self,
        function: &str,
        program: &mut Vec<ParsedOp>,
        at: usize,
    ) -> Result<bool, GraphMutationTextError> {
        let constant = if function.eq_ignore_ascii_case("PI") {
            core::f64::consts::PI
        } else if function.eq_ignore_ascii_case("E") {
            core::f64::consts::E
        } else {
            return Ok(false);
        };
        self.punct(b')', ")")?;
        let value = crate::GqlScalarParameter::new(CanonicalScalar::Float(
            fgdb_types::CanonicalF64::new(constant),
        ))
        .map_err(|_| failure(at, GraphMutationTextErrorKind::IntegerOperand))?;
        emit(
            program,
            ParsedOp::Bound(GraphIntegerOp::Scalar(
                value.predicate(IntegerComparison::Equal),
            )),
            at,
        )?;
        Ok(true)
    }

    fn integer_unary(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        let at = self.current.at;
        if depth > MAX_INTEGER_NESTING {
            return Err(failure(
                at,
                GraphMutationTextErrorKind::IntegerNesting {
                    limit: MAX_INTEGER_NESTING,
                },
            ));
        }
        if self.starts_integer_case()? {
            if self.lets.is_some() && self.list_case_here(columns, depth, program, at)? {
                return Ok(());
            }
            self.advance()?;
            return self.integer_case(columns, depth + 1, program, at);
        }
        // -9223372036854775808 is one legal signed literal, not negation of
        // an unrepresentable positive integer. Other signs are checked unary
        // IR, including `-2.5`: negation of a float literal.
        let signed_literal = self.is_punct(b'-')
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Digits(_))
            && self.scan_float_literal()?.is_none();
        if (self.is_punct(b'+') || self.is_punct(b'-')) && !signed_literal {
            let op = if self.is_punct(b'+') {
                GraphIntegerUnary::Plus
            } else {
                GraphIntegerUnary::Negate
            };
            self.advance()?;
            self.integer_unary(columns, depth + 1, program)?;
            return emit(program, ParsedOp::Unary(op), at);
        }
        if self.take(b'(')? {
            self.scalar_boolean(columns, depth + 1, program)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        // A list or map literal compared with `=`, `<>`, `!=` or IN is one
        // three-valued comparison in the value language (fgdb-9cawv).
        if self.lets.is_some() && self.composite_comparison_follows(columns)? {
            return self.composite_comparison(columns, depth, program, at, false);
        }
        // A composite operand evaluates as a let slot of this expression
        // (fgdb-o5o9t), read like a list element.
        if self.lets.is_some() && self.let_operand_here()? {
            return self.composite_slot(columns, depth, program, at);
        }
        if self.row_entry_slot(columns, program, at)? {
            return Ok(());
        }
        // `n[key]` of a MATCH element reads through a let slot too (Graph7).
        if self.lets.is_some()
            && matches!(columns, ExpressionColumns::Graph(_))
            && self.element_subscript_follows()?
        {
            return self.composite_slot(columns, depth, program, at);
        }
        if self.null_graph_function(program, at)? {
            return Ok(());
        }
        let function = if (crate::graph_text::SCALAR_FUNCTIONS
            .iter()
            .any(|name| self.is_word(name))
            || self.text_length_call(columns)?)
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let TokenKind::Word(name) = self.current.kind else {
                unreachable!("checked function word")
            };
            Some(name)
        } else {
            None
        };
        if let Some(function) = function {
            self.advance()?;
            self.punct(b'(', "(")?;
            if self.constant_function(function, program, at)? {
                return Ok(());
            }
            // An argument is a whole expression, comparisons and Boolean
            // operators included (`toString(1 < 0)`, TCK TypeConversion4 [3]),
            // exactly as inside parentheses.
            self.scalar_boolean(columns, depth + 1, program)?;
            let spelled =
                |names: [&str; 2]| names.iter().any(|name| function.eq_ignore_ascii_case(name));
            let text_op = if spelled(["UPPER", "TOUPPER"]) {
                Some(GraphIntegerOp::Upper)
            } else if spelled(["LOWER", "TOLOWER"]) {
                Some(GraphIntegerOp::Lower)
            } else if function.eq_ignore_ascii_case("TRIM") {
                Some(GraphIntegerOp::Trim)
            } else if function.eq_ignore_ascii_case("TOSTRING") {
                Some(GraphIntegerOp::ToText)
            } else if function.eq_ignore_ascii_case("TOINTEGER") {
                Some(GraphIntegerOp::ToInteger)
            } else if function.eq_ignore_ascii_case("TOBOOLEAN") {
                Some(GraphIntegerOp::ToBoolean)
            } else if spelled(["CHAR_LENGTH", "SIZE"]) || function.eq_ignore_ascii_case("LENGTH") {
                Some(GraphIntegerOp::CharLength)
            } else {
                None
            };
            if let Some(op) = text_op {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Bound(op), at);
            }
            if let Some(numeric) = GraphNumericFunction::named(function) {
                self.punct(b')', ")")?;
                return emit(
                    program,
                    ParsedOp::Bound(GraphIntegerOp::Numeric(numeric)),
                    at,
                );
            }
            // GQL/SQL SUBSTRING(s FROM start [FOR length]) counts from 1;
            // the openCypher comma form substring(s, start [, length]) counts
            // from 0 (fgdb-qkeq2). Without a length, the rest of the text.
            if function.eq_ignore_ascii_case("SUBSTRING") {
                let sql = self.take_word("FROM")?;
                if !sql {
                    self.punct(b',', ", or FROM")?;
                }
                self.scalar_boolean(columns, depth + 1, program)?;
                if self.take(b')')? {
                    let op = if sql {
                        GraphIntegerOp::SubstringFrom
                    } else {
                        GraphIntegerOp::CypherSubstringToEnd
                    };
                    return emit(program, ParsedOp::Bound(op), at);
                }
                if sql {
                    self.word("FOR")?;
                } else {
                    self.punct(b',', ",")?;
                }
                self.scalar_boolean(columns, depth + 1, program)?;
                self.punct(b')', ")")?;
                let op = if sql {
                    GraphIntegerOp::Substring
                } else {
                    GraphIntegerOp::CypherSubstring
                };
                return emit(program, ParsedOp::Bound(op), at);
            }
            if function.eq_ignore_ascii_case("ABS") {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Unary(GraphIntegerUnary::Abs), at);
            }
            self.punct(b',', ",")?;
            self.scalar_boolean(columns, depth + 1, program)?;
            if function.eq_ignore_ascii_case("NULLIF") {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Binary(GraphIntegerBinary::NullIf), at);
            }
            if function.eq_ignore_ascii_case("ATAN2") {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Bound(GraphIntegerOp::Atan2), at);
            }
            // openCypher left/right(text, length), replace(text, search,
            // replacement) (fgdb-20foe).
            if spelled(["LEFT", "RIGHT"]) {
                self.punct(b')', ")")?;
                let op = if function.eq_ignore_ascii_case("LEFT") {
                    GraphIntegerOp::Left
                } else {
                    GraphIntegerOp::Right
                };
                return emit(program, ParsedOp::Bound(op), at);
            }
            if function.eq_ignore_ascii_case("REPLACE") {
                self.punct(b',', ",")?;
                self.scalar_boolean(columns, depth + 1, program)?;
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Bound(GraphIntegerOp::Replace), at);
            }
            emit(program, ParsedOp::Coalesce, at)?;
            while self.take(b',')? {
                self.scalar_boolean(columns, depth + 1, program)?;
                emit(program, ParsedOp::Coalesce, at)?;
            }
            self.punct(b')', ")")?;
            return Ok(());
        }
        // `$doc.key` reads a map argument's field; `$from..` is a slice bound
        // before the range dots (TCK List2 [10]), not a field read.
        if let TokenKind::Parameter(name) = self.current.kind
            && self.property_dot_follows()?
        {
            self.parameter_types
                .entry(name.to_owned())
                .or_insert(GqlParameterType::Map);
            let Number::Parameter(index) = self.number(GqlParameterType::Map)? else {
                unreachable!("the current native token is a parameter");
            };
            let mut keys = Vec::new();
            while self.take(b'.')? {
                if keys.len() == MAX_INTEGER_NESTING {
                    return Err(failure(
                        at,
                        GraphMutationTextErrorKind::IntegerNesting {
                            limit: MAX_INTEGER_NESTING,
                        },
                    ));
                }
                keys.push(self.schema_name()?.text.into());
            }
            return emit(
                program,
                ParsedOp::ParameterField {
                    index,
                    keys: keys.into_boxed_slice(),
                    at,
                },
                at,
            );
        }
        // A list-comprehension element (fgdb-20foe) shadows every row and graph
        // name. Reading a vertex element's property would need storage access
        // inside the element scope, so `x.p` never falls through to a
        // same-named pattern variable. With a let sink, `x.key` reads a map
        // element's entry as a let slot (fgdb-o5o9t); any other element is
        // the evaluator's typed non-map failure. Without one it refuses.
        if let TokenKind::Word(word) = self.current.kind
            && let Some(binding) = self.elements.iter().rposition(|name| *name == word)
        {
            match self.lexer.clone().next()?.kind {
                TokenKind::Punct(b'.') if self.lets.is_some() => {
                    let element = ReadValueTemplate::Local(self.elements.len() - 1 - binding);
                    self.advance()?;
                    let value = self.map_entries(element)?;
                    return self.let_slot(program, value, at);
                }
                TokenKind::Punct(b'.') => {
                    return Err(failure(
                        at,
                        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
                            "a list element used as a value, not a property read",
                        )),
                    ));
                }
                TokenKind::Punct(b'(') => {}
                _ => {
                    self.advance()?;
                    let offset = self.elements.len() - 1 - binding;
                    return emit(program, ParsedOp::Bound(GraphIntegerOp::Local(offset)), at);
                }
            }
        }
        // openCypher id(x)/elementId(x) (fgdb-j687q): the column naming the
        // vertex or edge x holds the element itself, and IdentityColumn reads
        // its identity, so no element value ever enters the scalar stack.
        if (self.is_word("ID") || self.is_word("ELEMENTID"))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let text = self.is_word("ELEMENTID");
            self.advance()?;
            self.punct(b'(', "(")?;
            let element = self.current.at;
            let not_element = || {
                failure(
                    element,
                    GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
                        "a vertex or edge for id()/elementId()",
                    )),
                )
            };
            let column = match columns {
                ExpressionColumns::Graph(columns) => {
                    let variable = self.any_variable()?;
                    self.mutation_projection(columns, variable, None)?
                }
                ExpressionColumns::Row(schema) => {
                    let name = self.name()?;
                    schema
                        .iter()
                        .position(|(column, kind)| {
                            column.text == name.text
                                && matches!(
                                    kind,
                                    crate::GraphSetColumnType::Vertex
                                        | crate::GraphSetColumnType::Edge
                                        | crate::GraphSetColumnType::Any
                                )
                        })
                        .ok_or_else(not_element)?
                }
                ExpressionColumns::Resolved(resolve) => resolve(self)?.ok_or_else(not_element)?,
            };
            self.punct(b')', ")")?;
            return emit(
                program,
                ParsedOp::Bound(GraphIntegerOp::IdentityColumn { column, text }),
                at,
            );
        }
        if self.element_null_operand(columns, program, at)? {
            return Ok(());
        }
        if self.label_test_operand(columns, program, at)? {
            return Ok(());
        }
        let operand = match columns {
            ExpressionColumns::Graph(columns) => self.mutation_operand(columns)?,
            ExpressionColumns::Row(schema) => self.row_operand(schema)?,
            ExpressionColumns::Resolved(resolve) => match resolve(self)? {
                // `.key` after a resolved value reads its entry through a let
                // slot, as a row scope's row_entry_slot does: an imported
                // map's `row.key` beside MATCH reads (fgdb-0s2pq). Other
                // resolver scopes keep their own entry rules.
                Some(column) if self.lets.is_some() && self.resolved_entries => {
                    match self.map_entries(ReadValueTemplate::Column(column))? {
                        ReadValueTemplate::Column(column) => Operand::Column(column),
                        entry => return self.let_slot(program, entry, at),
                    }
                }
                Some(column) => Operand::Column(column),
                None => self.row_operand(&[])?,
            },
        };
        emit(program, ParsedOp::Atom(operand, at), at)
    }

    /// The operand of `x IS [NOT] NULL` when `x` names a vertex or edge
    /// (openCypher `RETURN a IS NOT NULL`, `s IS NULL` for an OPTIONAL edge).
    /// The element itself never enters the scalar stack: its identity column
    /// stands in, as text so a wide identity cannot overflow, and the IS NULL
    /// test the caller applies next consumes it, so only NULL-ness is ever
    /// observed. False, with nothing consumed, when the operand here is not
    /// such a binding. Out of line: integer_unary repeats once per nested
    /// parenthesis.
    #[inline(never)]
    fn element_null_operand(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        program: &mut Vec<ParsedOp>,
        at: usize,
    ) -> Result<bool, GraphMutationTextError> {
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(false);
        };
        if self.elements.contains(&word)
            || !matches!(self.lexer.clone().next()?.kind,
                TokenKind::Word(next) if next.eq_ignore_ascii_case("IS"))
        {
            return Ok(false);
        }
        let graph = self.syntax.variables.iter().any(|name| name.text == word)
            || self.syntax.visible_edge(word).is_some();
        let column = match columns {
            ExpressionColumns::Graph(columns) if graph => {
                let variable = self.any_variable()?;
                self.mutation_projection(columns, variable, None)?
            }
            ExpressionColumns::Row(schema) => {
                let visible = self.visible_width(schema);
                let Some(column) = schema[..visible].iter().position(|(name, kind)| {
                    name.text == word
                        && matches!(
                            kind,
                            crate::GraphSetColumnType::Vertex | crate::GraphSetColumnType::Edge
                        )
                }) else {
                    return Ok(false);
                };
                self.advance()?;
                column
            }
            ExpressionColumns::Resolved(resolve) if graph => match resolve(self)? {
                Some(column) => column,
                None => return Ok(false),
            },
            _ => return Ok(false),
        };
        emit(
            program,
            ParsedOp::Bound(GraphIntegerOp::IdentityColumn { column, text: true }),
            at,
        )?;
        Ok(true)
    }

    /// openCypher `v:L1:L2` as a value (Graph5, Return2 [8], fgdb-qo3jv).
    /// A vertex test is `'L1' IN labels(v) AND 'L2' IN labels(v)` over the
    /// labels() column, each membership a let slot. A relationship test is
    /// `type(r) = 'T'`. Either is NULL for a NULL element, and a label no
    /// catalog name matches is simply absent. Over a graph-to-row boundary (a
    /// WITH's WHERE, a MATCH WHERE deferred past an implicit `WITH *`) a
    /// carried element reads the labels() or type() column the boundary
    /// hoisted for it, as `labels(v)` there does. False, with nothing
    /// consumed, unless a MATCH vertex or relationship, or such a carried
    /// element, is followed by `:`. Out of line: integer_unary repeats once
    /// per nested parenthesis.
    #[inline(never)]
    fn label_test_operand(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        program: &mut Vec<ParsedOp>,
        at: usize,
    ) -> Result<bool, GraphMutationTextError> {
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(false);
        };
        if self.elements.contains(&word)
            || !matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b':'))
        {
            return Ok(false);
        }
        let (column, function) = match columns {
            ExpressionColumns::Graph(columns) => {
                let (variable, function) = match self.insertion_match_kind(word) {
                    Some(crate::GraphSetColumnType::Vertex) => {
                        (self.vertex_variable()?, GraphPathFunction::Labels)
                    }
                    Some(crate::GraphSetColumnType::Edge) => {
                        (self.edge_variable()?, GraphPathFunction::Type)
                    }
                    _ => return Ok(false),
                };
                if function == GraphPathFunction::Labels && self.lets.is_none() {
                    return Err(failure(
                        at,
                        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
                            "a label test where values are read (RETURN, WITH or ORDER BY)",
                        )),
                    ));
                }
                let column = self.projection_slot(
                    columns,
                    Projection {
                        variable,
                        property: None,
                        path: Some(function),
                        collect: None,
                    },
                )?;
                (column, function)
            }
            ExpressionColumns::Resolved(resolve) => {
                let hoisted = self.boundary_reads.as_ref().is_some_and(|boundary| {
                    boundary.functions.iter().any(|&(alias, function, _)| {
                        alias == word
                            && matches!(
                                function,
                                GraphPathFunction::Labels | GraphPathFunction::Type
                            )
                    })
                });
                if !hoisted || self.lets.is_none() {
                    return Ok(false);
                }
                // Only the boundary row's own resolver reads the hoisted
                // column; a nested row of another width resolves the name
                // as something else, which is no label test.
                self.label_test = Some(None);
                let resolved = resolve(self);
                let function = self.label_test.take().flatten();
                match (resolved?, function) {
                    (Some(column), Some(function)) => (column, function),
                    (None, None) => return Ok(false),
                    _ => {
                        return Err(failure(
                            at,
                            GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::Expected(
                                "a label test over a MATCH element carried unchanged",
                            )),
                        ));
                    }
                }
            }
            ExpressionColumns::Row(_) => return Ok(false),
        };
        let mut first = true;
        while self.take(b':')? {
            let label = self.schema_name()?;
            let name = CanonicalScalar::ucs_basic_text(label.text)
                .ok()
                .and_then(|text| GqlScalarParameter::new(text).ok())
                .ok_or_else(|| {
                    failure(
                        label.at,
                        GraphMutationTextErrorKind::Query(GraphPatternTextErrorKind::ScalarLiteral),
                    )
                })?;
            if function == GraphPathFunction::Labels {
                let membership = ReadValueTemplate::In {
                    value: Box::new(ReadValueTemplate::Literal(name)),
                    list: Box::new(ReadValueTemplate::Column(column)),
                };
                self.let_slot(program, membership, label.at)?;
            } else {
                emit(
                    program,
                    ParsedOp::Atom(Operand::Column(column), at),
                    label.at,
                )?;
                emit(
                    program,
                    ParsedOp::Atom(Operand::Literal(name), at),
                    label.at,
                )?;
                emit(
                    program,
                    ParsedOp::Bound(GraphIntegerOp::Compare(IntegerComparison::Equal)),
                    label.at,
                )?;
            }
            if !first {
                emit(program, ParsedOp::Bound(GraphIntegerOp::And), label.at)?;
            }
            first = false;
        }
        Ok(true)
    }

    pub(super) fn row_operand(
        &mut self,
        schema: &[(Name<'a>, crate::GraphSetColumnType)],
    ) -> Result<Operand, GraphPatternTextError> {
        // `n.p` of a carried MATCH vertex inside a graph-to-row WITH's scope
        // reads its hidden boundary column (fgdb-1tgko); the private names of
        // those columns never resolve as text.
        if let Some(column) = self.boundary_read(schema.len())? {
            return Ok(Operand::Column(column));
        }
        if let TokenKind::Word(word) = self.current.kind {
            let visible = self.visible_width(schema);
            if let Some(column) = schema[..visible]
                .iter()
                .position(|(name, _)| name.text == word)
            {
                self.advance()?;
                return Ok(Operand::Column(column));
            }
            // An alias wins; then a grouping WITH's own item expression.
            if let Some(column) = self.projected_source(schema.len())? {
                return Ok(Operand::Column(column));
            }
            if !(word.eq_ignore_ascii_case("TRUE")
                || word.eq_ignore_ascii_case("FALSE")
                || word.eq_ignore_ascii_case("NULL"))
                || self.property_dot_follows()?
            {
                return Err(error(
                    self.current.at,
                    GraphPatternTextErrorKind::UnknownVariable,
                ));
            }
        }
        // Only literals/parameters remain. In particular, no root variable or
        // property lookup can reach the graph operand branch of this helper.
        self.mutation_operand(&mut Vec::new())
    }
}

#[cfg(test)]
mod predicate_boundary_tests {
    use super::*;
    use crate::GqlQueryPolicy;
    use fgdb_types::{CanonicalPropertyValue, VId};

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }

    fn rows(predicate: &str) -> Vec<VId> {
        let p = PropertyKeyId(1);
        let q = PropertyKeyId(2);
        let values: [Vec<(PropertyKeyId, CanonicalPropertyValue)>; 5] = [
            vec![
                (p, CanonicalScalar::Int(0).into()),
                (q, CanonicalScalar::Int(1).into()),
            ],
            vec![
                (p, CanonicalScalar::Int(1).into()),
                (q, CanonicalScalar::Int(0).into()),
            ],
            vec![
                (p, CanonicalScalar::Int(2).into()),
                (q, CanonicalScalar::Int(2).into()),
            ],
            vec![
                (p, CanonicalScalar::Null.into()),
                (q, CanonicalScalar::Int(1).into()),
            ],
            vec![(q, CanonicalScalar::Int(1).into())],
        ];
        let pattern =
            PreparedGraphText::prepare(&format!("MATCH (n) WHERE {predicate} RETURN n"), symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        pattern
            .plan()
            .execute_governed_with_properties(
                5,
                (1..=5).map(VId),
                [],
                |vid, predicate| {
                    Ok::<_, ()>(
                        predicate
                            .iter()
                            .all(|p| p.matches(&[], &values[vid.0 as usize - 1])),
                    )
                },
                |vid, key| {
                    Ok(values[vid.0 as usize - 1]
                        .iter()
                        .find(|(property, _)| *property == key)
                        .map(|(_, value)| value))
                },
                GqlQueryPolicy::new(100, 100, 100_000, 100_000),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value
            .iter()
            .map(|row| row.get(0).unwrap().as_vertex().unwrap())
            .collect()
    }

    #[test]
    fn scalar_where_leaves_do_not_swallow_outer_conjunctions_or_disjunctions() {
        for (predicate, expected) in [
            ("n.p=1 AND n.q+1=1 OR n.p=0", vec![VId(1), VId(2)]),
            ("n.p=1 AND (n.q+1=1 OR n.p=0)", vec![VId(2)]),
            ("NOT n.p=1 AND n.q+1=1 OR n.p=0", vec![VId(1)]),
            ("n.p=1 AND NOT n.q+1=1 OR n.p=0", vec![VId(1)]),
            (
                "n.q+1=1 AND n.p BETWEEN 0 AND 1 OR n.p=2",
                vec![VId(2), VId(3)],
            ),
            (
                "n.p=1 AND n.q+1=1 OR n.p IS NULL",
                vec![VId(2), VId(4), VId(5)],
            ),
        ] {
            assert_eq!(rows(predicate), expected, "{predicate}");
        }
    }

    #[test]
    fn graph_boolean_program_keeps_separate_scalar_leaves_and_operator_order() {
        use crate::graph_text::boolean::SyntaxItem;
        let syntax = Parser::new("MATCH (n) WHERE n.p+1=1 AND n.q+1=1 OR n.p=2 RETURN n")
            .unwrap()
            .parse()
            .unwrap();
        let [Filter::Boolean { program, .. }] = syntax.filters.as_slice() else {
            panic!("one graph Boolean program");
        };
        assert!(matches!(
            program.as_slice(),
            [
                SyntaxItem::Expression { .. },
                SyntaxItem::Expression { .. },
                SyntaxItem::And,
                SyntaxItem::Atom(Filter::Property { .. }),
                SyntaxItem::Or,
            ]
        ));
    }

    #[test]
    fn assignment_and_row_expressions_still_parse_the_whole_boolean_expression() {
        let text = "1=1 AND 2=2 OR 3=4";
        let mut predicate = Parser::new(text).unwrap();
        predicate.boolean_scalar_expression().unwrap();
        assert!(predicate.is_word("AND"));
        for row_scope in [false, true] {
            let mut parser = Parser::new(text).unwrap();
            let operand = if row_scope {
                parser.row_expression(&[]).unwrap()
            } else {
                parser.mutation_expression(&mut Vec::new()).unwrap()
            };
            assert!(matches!(parser.current.kind, TokenKind::End));
            let Operand::Integer { program, .. } = operand else {
                panic!("whole Boolean expression bytecode");
            };
            assert!(matches!(
                program.last(),
                Some(MutationIntegerTemplateOp::Bound(GraphIntegerOp::Or))
            ));
        }
    }
}

#[cfg(test)]
mod float_parameter_tests {
    use super::*;
    use crate::algebra::GraphValue;
    use crate::{GraphIntegerErrorKind, GraphIntegerEvaluationError};
    use fgdb_types::CanonicalF64;

    fn float(value: f64) -> CanonicalScalar {
        CanonicalScalar::Float(CanonicalF64::new(value))
    }

    fn template(text: &str, kind: CanonicalScalarKind) -> (Vec<MutationIntegerTemplateOp>, usize) {
        let mut parser =
            Parser::new_with_parameter_types(text, &[("delta", GqlParameterType::Scalar(kind))])
                .unwrap();
        let columns = [(Name { text: "x", at: 0 }, crate::GraphSetColumnType::Scalar)];
        let operand = parser.row_expression(&columns).unwrap();
        assert!(matches!(parser.current.kind, TokenKind::End));
        let Operand::Integer { program, at } = operand else {
            panic!("an expression must retain its native parameter holes")
        };
        (program, at)
    }

    fn evaluate(
        template: &(Vec<MutationIntegerTemplateOp>, usize),
        parameter: CanonicalScalar,
        property: CanonicalScalar,
    ) -> Result<CanonicalScalar, GraphIntegerErrorKind> {
        let value = GqlParameterValue::Scalar(crate::GqlScalarParameter::new(parameter).unwrap());
        bind_integer(&template.0, &[value], template.1)
            .unwrap()
            .evaluate_scalar_with_control(&[GraphValue::Scalar(property)], &mut |_| Ok::<_, ()>(()))
            .map_err(|error| match error {
                GraphIntegerEvaluationError::Value(error) => error.kind,
                GraphIntegerEvaluationError::Control(()) => unreachable!(),
            })
    }

    fn decimal(coefficient: i128) -> CanonicalScalar {
        CanonicalScalar::Decimal(
            fgdb_types::CanonicalDecimal::from_coefficient(coefficient).unwrap(),
        )
    }

    #[test]
    fn declared_decimal_parameters_reach_exact_arithmetic_and_conversions() {
        const UNIT: i128 = 1_000_000_000_000_000_000;
        for (text, expected) in [
            ("x + $delta", 15 * UNIT / 2),
            ("x - $delta", 7 * UNIT / 2),
            ("x * $delta", 11 * UNIT),
            ("x / $delta", 11 * UNIT / 4),
            ("x % $delta", 3 * UNIT / 2),
            ("ABS(-$delta)", 2 * UNIT),
            ("+$delta", 2 * UNIT),
        ] {
            let prepared = template(text, CanonicalScalarKind::Decimal);
            assert_eq!(
                evaluate(&prepared, decimal(2 * UNIT), decimal(11 * UNIT / 2)),
                Ok(decimal(expected)),
                "{text}"
            );
        }
        let prepared = template("x * $delta", CanonicalScalarKind::Decimal);
        // Literal coefficients pin half-even rounding on opposite sides of
        // a tie. Reuse the SAME template with new arguments, not new syntax.
        for (input, factor, expected) in [
            (10 * UNIT + 1, UNIT / 2, 5 * UNIT),
            (20 * UNIT + 3, UNIT / 2, 10 * UNIT + 2),
            (10 * UNIT + 1, UNIT / 4, 5 * UNIT / 2),
            (20 * UNIT + 3, UNIT / 4, 5 * UNIT + 1),
            (-20 * UNIT - 3, UNIT / 2, -10 * UNIT - 2),
        ] {
            assert_eq!(
                evaluate(&prepared, decimal(factor), decimal(input)),
                Ok(decimal(expected))
            );
        }
        let wide = decimal(9_007_199_254_740_993 * UNIT + 1);
        assert_eq!(
            evaluate(
                &template("toInteger($delta)", CanonicalScalarKind::Decimal),
                wide.clone(),
                CanonicalScalar::Null,
            ),
            Ok(CanonicalScalar::Int(9_007_199_254_740_993))
        );
        assert_eq!(
            evaluate(
                &template("toString($delta)", CanonicalScalarKind::Decimal),
                wide,
                CanonicalScalar::Null,
            ),
            Ok(CanonicalScalar::ucs_basic_text("9007199254740993.000000000000000001").unwrap())
        );
    }

    #[test]
    fn decimal_witnesses_preserve_parameter_holes_nulls_and_lazy_branches() {
        const UNIT: i128 = 1_000_000_000_000_000_000;
        let prepared = template("x / $delta", CanonicalScalarKind::Decimal);
        assert!(
            prepared
                .0
                .iter()
                .any(|op| matches!(op, MutationIntegerTemplateOp::Parameter { index: 0, .. }))
        );
        let transcript = |program: &[MutationIntegerTemplateOp]| {
            let mut bytes = Vec::new();
            for op in program {
                op.append_template_transcript(&mut bytes);
            }
            bytes
        };
        let before = transcript(&prepared.0);
        assert_eq!(
            evaluate(&prepared, decimal(2 * UNIT), decimal(7 * UNIT)),
            Ok(decimal(7 * UNIT / 2))
        );
        assert_eq!(
            evaluate(&prepared, CanonicalScalar::Null, decimal(7 * UNIT)),
            Ok(CanonicalScalar::Null)
        );
        assert_eq!(transcript(&prepared.0), before);
        // The type witness is zero even in a denominator, but neither it nor
        // an unselected branch may execute during preparation or binding.
        for text in [
            "COALESCE($delta, x / x)",
            "CASE WHEN TRUE THEN $delta ELSE x / x END",
        ] {
            assert_eq!(
                evaluate(
                    &template(text, CanonicalScalarKind::Decimal),
                    decimal(3 * UNIT + 1),
                    decimal(0),
                ),
                Ok(decimal(3 * UNIT + 1)),
                "{text}"
            );
        }
        assert_eq!(
            evaluate(
                &template("NULLIF($delta, $delta)", CanonicalScalarKind::Decimal),
                decimal(1),
                CanonicalScalar::Null,
            ),
            Ok(CanonicalScalar::Null)
        );
    }

    #[test]
    fn decimal_parameter_failures_reach_the_native_typed_error_boundary() {
        const UNIT: i128 = 1_000_000_000_000_000_000;
        for (text, delta, input, expected) in [
            (
                "x / $delta",
                decimal(0),
                decimal(UNIT),
                GraphIntegerErrorKind::DivisionByZero,
            ),
            (
                "x % $delta",
                decimal(0),
                decimal(UNIT),
                GraphIntegerErrorKind::DivisionByZero,
            ),
            (
                "x * $delta",
                decimal(fgdb_types::MAX_DECIMAL_COEFFICIENT),
                decimal(2 * UNIT),
                GraphIntegerErrorKind::Overflow,
            ),
            (
                "x + $delta",
                decimal(1),
                decimal(fgdb_types::MAX_DECIMAL_COEFFICIENT),
                GraphIntegerErrorKind::Overflow,
            ),
            (
                "x * $delta",
                decimal(UNIT),
                CanonicalScalar::Int(2),
                GraphIntegerErrorKind::IncompatibleOperands,
            ),
            (
                "x * $delta",
                decimal(UNIT),
                float(0.5),
                GraphIntegerErrorKind::IncompatibleOperands,
            ),
        ] {
            assert_eq!(
                evaluate(&template(text, CanonicalScalarKind::Decimal), delta, input),
                Err(expected),
                "{text}"
            );
        }
    }

    #[test]
    fn decimal_parameters_do_not_weaken_nonnumeric_or_integer_only_admission() {
        for text in [
            "UPPER($delta)",
            "$delta AND TRUE",
            "SUBSTRING('abc', $delta, 1)",
            "SUBSTRING('abc', 1, $delta)",
            "SUBSTRING('abc' FROM $delta FOR 1)",
            "SUBSTRING('abc' FROM 1 FOR $delta)",
            "FLOOR($delta)",
        ] {
            let mut parser = Parser::new_with_parameter_types(
                text,
                &[(
                    "delta",
                    GqlParameterType::Scalar(CanonicalScalarKind::Decimal),
                )],
            )
            .unwrap();
            let error = parser
                .row_expression(&[])
                .err()
                .expect("invalid decimal context");
            assert_eq!(error.offset, text.find('$').unwrap(), "{text}");
            assert!(matches!(
                error.kind,
                GraphMutationTextErrorKind::IntegerOperand
            ));
        }
    }

    #[test]
    fn bound_decimal_arithmetic_keeps_every_evaluator_checkpoint() {
        const UNIT: i128 = 1_000_000_000_000_000_000;
        for text in ["x + $delta", "x * $delta", "x / $delta", "x % $delta"] {
            let prepared = template(text, CanonicalScalarKind::Decimal);
            let value =
                GqlParameterValue::Scalar(GqlScalarParameter::new(decimal(2 * UNIT)).unwrap());
            let bound = bind_integer(&prepared.0, &[value], prepared.1).unwrap();
            let row = [GraphValue::Scalar(decimal(7 * UNIT))];
            let mut calls = 0;
            bound
                .evaluate_scalar_with_control(&row, &mut |_| {
                    calls += 1;
                    Ok::<_, usize>(())
                })
                .unwrap();
            assert!(calls > 0);
            for stop in 1..=calls {
                let mut seen = 0;
                let result = bound.evaluate_scalar_with_control(&row, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                });
                assert!(
                    matches!(
                        result,
                        Err(GraphIntegerEvaluationError::Control(at)) if at == stop
                    ),
                    "{text}: checkpoint {stop}"
                );
            }
        }
    }

    #[test]
    fn declared_float_parameters_reach_every_numeric_operator() {
        for (text, expected) in [
            ("x + $delta", 7.5),
            ("x - $delta", 3.5),
            ("x * $delta", 11.0),
            ("x / $delta", 2.75),
            ("x % $delta", 1.5),
            ("ABS(-$delta)", 2.0),
            ("+$delta", 2.0),
        ] {
            let prepared = template(text, CanonicalScalarKind::Float);
            assert_eq!(
                evaluate(&prepared, float(2.0), float(5.5)),
                Ok(float(expected)),
                "{text}"
            );
        }
        let prepared = template("x + $delta", CanonicalScalarKind::Float);
        for delta in [0.5, 1.25, -2.0] {
            assert_eq!(
                evaluate(&prepared, float(delta), CanonicalScalar::Int(2)),
                Ok(float(2.0 + delta)),
            );
        }
    }

    #[test]
    fn nullable_parameters_and_lazy_branches_do_not_execute_type_witnesses() {
        for (text, delta, expected) in [
            ("x / $delta", float(2.0), float(2.75)),
            ("x / $delta", CanonicalScalar::Null, CanonicalScalar::Null),
            (
                "COALESCE($delta, 2.0) * 2",
                CanonicalScalar::Null,
                float(4.0),
            ),
            ("COALESCE($delta, 1.0 / 0.0)", float(1.25), float(1.25)),
            (
                "CASE WHEN x > 0 THEN $delta ELSE 1.0 / 0.0 END",
                float(3.5),
                float(3.5),
            ),
            ("NULLIF($delta, 2.0)", float(2.0), CanonicalScalar::Null),
        ] {
            let prepared = template(text, CanonicalScalarKind::Float);
            assert_eq!(
                evaluate(&prepared, delta, float(5.5)),
                Ok(expected),
                "{text}"
            );
        }
    }

    #[test]
    fn float_parameters_do_not_weaken_text_boolean_or_integer_only_admission() {
        for text in [
            "UPPER($delta)",
            "$delta AND TRUE",
            "SUBSTRING('abc', $delta, 1)",
            "SUBSTRING('abc', 1, $delta)",
            "SUBSTRING('abc' FROM $delta FOR 1)",
            "SUBSTRING('abc' FROM 1 FOR $delta)",
        ] {
            let mut parser = Parser::new_with_parameter_types(
                text,
                &[(
                    "delta",
                    GqlParameterType::Scalar(CanonicalScalarKind::Float),
                )],
            )
            .unwrap();
            let error = parser
                .row_expression(&[])
                .err()
                .expect("invalid operand kind");
            assert_eq!(error.offset, text.find('$').unwrap(), "{text}");
            assert!(matches!(
                error.kind,
                GraphMutationTextErrorKind::IntegerOperand
            ));
        }
        for kind in [CanonicalScalarKind::Bool, CanonicalScalarKind::Text] {
            let text = "$delta * 2";
            let mut parser = Parser::new_with_parameter_types(
                text,
                &[("delta", GqlParameterType::Scalar(kind))],
            )
            .unwrap();
            assert!(parser.row_expression(&[]).is_err());
        }
    }

    #[test]
    fn float_parameter_failures_remain_typed_arithmetic_exceptions() {
        for (text, delta, expected) in [
            ("x / $delta", 0.0, GraphIntegerErrorKind::DivisionByZero),
            ("$delta * 2.0", f64::MAX, GraphIntegerErrorKind::Overflow),
            (
                "$delta + 1.0",
                f64::INFINITY,
                GraphIntegerErrorKind::Overflow,
            ),
            ("$delta + 1.0", f64::NAN, GraphIntegerErrorKind::Overflow),
        ] {
            let prepared = template(text, CanonicalScalarKind::Float);
            assert_eq!(
                evaluate(&prepared, float(delta), float(5.5)),
                Err(expected),
                "{text}"
            );
        }
    }

    #[test]
    fn integer_parameter_division_and_overflow_are_unchanged() {
        let prepared = template("$delta / 2", CanonicalScalarKind::Int);
        assert_eq!(
            evaluate(&prepared, CanonicalScalar::Int(5), CanonicalScalar::Null),
            Ok(CanonicalScalar::Int(2)),
        );
        let prepared = template("$delta + 1", CanonicalScalarKind::Int);
        assert_eq!(
            evaluate(
                &prepared,
                CanonicalScalar::Int(i64::MAX),
                CanonicalScalar::Null
            ),
            Err(GraphIntegerErrorKind::Overflow),
        );
    }

    #[test]
    fn exact_mixed_numeric_comparison_is_not_rounded_by_parameter_admission() {
        let prepared = template("x = $delta", CanonicalScalarKind::Float);
        assert_eq!(
            evaluate(
                &prepared,
                float(9_007_199_254_740_992.0),
                CanonicalScalar::Int(9_007_199_254_740_993)
            ),
            Ok(CanonicalScalar::Bool(false)),
        );
        assert_eq!(
            evaluate(
                &prepared,
                float(9_007_199_254_740_992.0),
                CanonicalScalar::Int(9_007_199_254_740_992)
            ),
            Ok(CanonicalScalar::Bool(true)),
        );
    }
}

#[cfg(test)]
mod map_parameter_field_tests {
    use super::*;
    use crate::algebra::GraphValue;

    fn template(text: &str) -> Vec<MutationIntegerTemplateOp> {
        let mut parser =
            Parser::new_with_parameter_types(text, &[("m", GqlParameterType::Map)]).unwrap();
        let Operand::Integer { program, .. } = parser.row_expression(&[]).unwrap() else {
            panic!("map fields use a native parameterized scalar program");
        };
        assert!(matches!(parser.current.kind, TokenKind::End));
        program
    }

    fn arguments(value: GraphValue) -> Vec<GqlParameterValue> {
        vec![GqlParameterValue::Map(
            crate::GqlMapParameter::new(vec![("value".into(), value)]).unwrap(),
        )]
    }

    fn evaluate(program: &[MutationIntegerTemplateOp], value: GraphValue) -> CanonicalScalar {
        bind_integer(program, &arguments(value), 0)
            .unwrap()
            .evaluate_scalar_with_control(&[], &mut |_| Ok::<_, ()>(()))
            .unwrap()
    }

    #[test]
    fn map_field_binding_retains_actual_numeric_text_and_null_domains() {
        let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
        let float = |value| {
            GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value)))
        };
        let arithmetic = template("$m.value * 2 + 1");
        assert_eq!(evaluate(&arithmetic, int(4)), CanonicalScalar::Int(9));
        assert_eq!(
            evaluate(&arithmetic, float(0.25)),
            CanonicalScalar::Float(fgdb_types::CanonicalF64::new(1.5))
        );
        assert_eq!(
            evaluate(
                &template("upper($m.value)"),
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("aBc").unwrap())
            ),
            CanonicalScalar::ucs_basic_text("ABC").unwrap()
        );
        assert_eq!(
            evaluate(
                &template("$m.value"),
                GraphValue::Scalar(CanonicalScalar::Bool(true))
            ),
            CanonicalScalar::Bool(true)
        );
        let missing = template("coalesce($m.value.absent, 7)");
        assert_eq!(
            evaluate(&missing, GraphValue::map(Vec::new()).unwrap()),
            CanonicalScalar::Int(7)
        );
        assert_eq!(
            evaluate(&missing, GraphValue::Scalar(CanonicalScalar::Null)),
            CanonicalScalar::Int(7)
        );
        let null =
            GqlParameterValue::Scalar(GqlScalarParameter::new(CanonicalScalar::Null).unwrap());
        assert_eq!(
            bind_integer(&arithmetic, &[null], 0)
                .unwrap()
                .evaluate_scalar_with_control(&[], &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            CanonicalScalar::Null
        );
        for value in [
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
            GraphValue::map(Vec::new()).unwrap(),
            GraphValue::List(Vec::new().into()),
        ] {
            assert!(bind_integer(&arithmetic, &arguments(value), 0).is_err());
        }
        assert!(bind_integer(&missing, &arguments(int(1)), 0).is_err());
    }

    #[test]
    fn parameter_field_transcripts_pin_path_order_and_parser_bounds() {
        let program = template("$m.outer.value + 1");
        let mut transcript = Vec::new();
        for op in &program {
            op.append_template_transcript(&mut transcript);
        }
        let mut expected = vec![2];
        expected.extend_from_slice(&0_u64.to_be_bytes());
        expected.extend_from_slice(&2_u64.to_be_bytes());
        for key in ["outer", "value"] {
            expected.extend_from_slice(&(key.len() as u64).to_be_bytes());
            expected.extend_from_slice(key.as_bytes());
        }
        let mut field = Vec::new();
        program[0].append_template_transcript(&mut field);
        assert_eq!(field, expected);
        let other = template("$m.value.outer + 1");
        let mut other_bytes = Vec::new();
        for op in &other {
            op.append_template_transcript(&mut other_bytes);
        }
        assert_ne!(transcript, other_bytes);
        let mut scalar =
            Parser::new_with_parameter_types("$m.value + 1", &[("m", GqlParameterType::Int64)])
                .unwrap();
        assert!(scalar.row_expression(&[]).is_err());
        let deep = format!("$m{} + 1", ".field".repeat(MAX_INTEGER_NESTING + 1));
        let mut parser =
            Parser::new_with_parameter_types(&deep, &[("m", GqlParameterType::Map)]).unwrap();
        assert!(matches!(
            parser.row_expression(&[]),
            Err(GraphMutationTextError {
                kind: GraphMutationTextErrorKind::IntegerNesting {
                    limit: MAX_INTEGER_NESTING
                },
                ..
            })
        ));
    }
}
