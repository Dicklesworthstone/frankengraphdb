//! Precedence-aware integer RHS lowering on the shared MATCH lexer.
//! Only preparation sees these temporary nodes. Binding substitutes typed
//! operands into checked bytecode, never into source text or an AST evaluator.

mod conditional;

use super::*;
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
}
enum ParsedOp {
    Atom(Operand, usize),
    ParameterField {
        index: usize,
        keys: Box<[Box<str>]>,
        at: usize,
    },
    Unary(GraphIntegerUnary),
    Binary(GraphIntegerBinary),
    Coalesce,
    Bound(GraphIntegerOp),
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
                | GraphIntegerOp::Concat
                | GraphIntegerOp::ToText
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

pub(in crate::graph_text) fn bind_integer(
    program: &[MutationIntegerTemplateOp],
    values: &[GqlParameterValue],
    at: usize,
) -> Result<GraphIntegerExpression, GraphMutationTextError> {
    let mut ops = Vec::with_capacity(program.len());
    for op in program {
        ops.push(match op {
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

    #[allow(clippy::type_complexity)]
    pub(in crate::graph_text) fn boolean_scalar_expression(
        &mut self,
    ) -> Result<(Vec<(Name<'a>, Name<'a>)>, Vec<MutationIntegerTemplateOp>), GraphPatternTextError>
    {
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
            .map(|column| {
                column
                    .property
                    .map(|key| (column.variable, key))
                    .ok_or_else(|| error(at, GraphPatternTextErrorKind::BooleanExpression))
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
    ) -> Result<Operand, GraphMutationTextError> {
        // HAVING owns predicates around grouping keys and aggregate arguments.
        self.checked_expression(&mut ExpressionColumns::Graph(columns), false)
    }

    pub(super) fn row_expression(
        &mut self,
        columns: &[(Name<'a>, crate::GraphSetColumnType)],
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Row(columns), true)
    }

    /// A whole Boolean predicate over resolved leaves: a list-comprehension
    /// WHERE in the aggregate-resolution scope (fgdb-20foe).
    pub(super) fn resolved_predicate(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Resolved(resolve), true)
    }

    pub(super) fn resolved_expression(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
    ) -> Result<Operand, GraphMutationTextError> {
        self.checked_expression(&mut ExpressionColumns::Resolved(resolve), false)
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
        let mut program = Vec::with_capacity(parsed.len());
        for op in parsed {
            program.push(match op {
                ParsedOp::Bound(op) => MutationIntegerTemplateOp::Bound(op),
                ParsedOp::ParameterField { index, keys, at } => {
                    MutationIntegerTemplateOp::ParameterField { index, keys, at }
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
                        && !matches!(
                            schema[column].1,
                            crate::GraphSetColumnType::Scalar | crate::GraphSetColumnType::Any
                        )
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
        self.scalar_conjunction(columns, depth, program)?;
        while self.take_word("OR")? {
            let at = self.current.at;
            self.scalar_conjunction(columns, depth, program)?;
            emit(program, ParsedOp::Bound(GraphIntegerOp::Or), at)?;
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

    fn scalar_comparison(
        &mut self,
        columns: &mut ExpressionColumns<'_, 'a>,
        depth: usize,
        program: &mut Vec<ParsedOp>,
    ) -> Result<(), GraphMutationTextError> {
        self.scalar_concat(columns, depth, program)?;
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
            if matches!(
                self.current.kind,
                TokenKind::Punct(b'=' | b'!' | b'<' | b'>')
            ) {
                let comparison = self.comparison()?;
                self.scalar_concat(columns, depth, program)?;
                emit(
                    program,
                    ParsedOp::Bound(GraphIntegerOp::Compare(comparison)),
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
        self.integer_product(columns, depth, program)?;
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
            emit(program, ParsedOp::Binary(op), at)?;
        }
        Ok(())
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
        let function = if crate::graph_text::SCALAR_FUNCTIONS
            .iter()
            .any(|name| self.is_word(name))
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
            self.scalar_concat(columns, depth + 1, program)?;
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
            } else if spelled(["CHAR_LENGTH", "SIZE"]) {
                Some(GraphIntegerOp::CharLength)
            } else {
                None
            };
            if let Some(op) = text_op {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Bound(op), at);
            }
            let numeric = [
                ("TOFLOAT", GraphNumericFunction::ToFloat),
                ("FLOOR", GraphNumericFunction::Floor),
                ("CEIL", GraphNumericFunction::Ceil),
                ("ROUND", GraphNumericFunction::Round),
                ("SQRT", GraphNumericFunction::Sqrt),
            ]
            .into_iter()
            .find_map(|(name, numeric)| function.eq_ignore_ascii_case(name).then_some(numeric));
            if let Some(numeric) = numeric {
                self.punct(b')', ")")?;
                return emit(
                    program,
                    ParsedOp::Bound(GraphIntegerOp::Numeric(numeric)),
                    at,
                );
            }
            if function.eq_ignore_ascii_case("SUBSTRING") {
                let sql = self.take_word("FROM")?;
                if !sql {
                    self.punct(b',', ", or FROM")?;
                }
                self.scalar_concat(columns, depth + 1, program)?;
                if sql {
                    self.word("FOR")?;
                } else {
                    self.punct(b',', ",")?;
                }
                self.scalar_concat(columns, depth + 1, program)?;
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Bound(GraphIntegerOp::Substring), at);
            }
            if function.eq_ignore_ascii_case("ABS") {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Unary(GraphIntegerUnary::Abs), at);
            }
            self.punct(b',', ",")?;
            self.scalar_concat(columns, depth + 1, program)?;
            if function.eq_ignore_ascii_case("NULLIF") {
                self.punct(b')', ")")?;
                return emit(program, ParsedOp::Binary(GraphIntegerBinary::NullIf), at);
            }
            emit(program, ParsedOp::Coalesce, at)?;
            while self.take(b',')? {
                self.scalar_concat(columns, depth + 1, program)?;
                emit(program, ParsedOp::Coalesce, at)?;
            }
            self.punct(b')', ")")?;
            return Ok(());
        }
        if let TokenKind::Parameter(name) = self.current.kind
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
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
                keys.push(self.name()?.text.into());
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
        // name. Reading a property of one would need storage access inside
        // the element scope, so `x.p` refuses rather than falling through to
        // a same-named pattern variable.
        if let TokenKind::Word(word) = self.current.kind
            && let Some(binding) = self.elements.iter().rposition(|name| *name == word)
        {
            match self.lexer.clone().next()?.kind {
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
        let operand = match columns {
            ExpressionColumns::Graph(columns) => self.mutation_operand(columns)?,
            ExpressionColumns::Row(schema) => self.row_operand(schema)?,
            ExpressionColumns::Resolved(resolve) => match resolve(self)? {
                Some(column) => Operand::Column(column),
                None => self.row_operand(&[])?,
            },
        };
        emit(program, ParsedOp::Atom(operand, at), at)
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
            if !(word.eq_ignore_ascii_case("TRUE")
                || word.eq_ignore_ascii_case("FALSE")
                || word.eq_ignore_ascii_case("NULL"))
                || matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
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
    use fgdb_types::VId;

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
        let values = [
            vec![(p, CanonicalScalar::Int(0)), (q, CanonicalScalar::Int(1))],
            vec![(p, CanonicalScalar::Int(1)), (q, CanonicalScalar::Int(0))],
            vec![(p, CanonicalScalar::Int(2)), (q, CanonicalScalar::Int(2))],
            vec![(p, CanonicalScalar::Null), (q, CanonicalScalar::Int(1))],
            vec![(q, CanonicalScalar::Int(1))],
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
