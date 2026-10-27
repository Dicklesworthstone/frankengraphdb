//! HAVING precedence and immutable operands over the shared aggregate parser.
//! No extra lexer, source query, catalog lookup or text interpolation at bind.
//! Output references and private aggregate calls use the SAME resolver and
//! bounded registry as ORDER BY. Hidden grouping keys keep evaluation indices.
//! IN/NOT IN list constructors and BETWEEN/NOT BETWEEN inclusive ranges lower
//! to the existing eager three-valued comparisons. An operand can be a public
//! alias, grouping expression, hidden aggregate, scalar literal or parameter,
//! composed by the ordinary typed scalar and value expression compiler.
use super::super::boolean::comparison_tag;
use super::*;
use crate::algebra::ScalarPredicate;
use crate::{
    GqlScalarParameter, GraphHavingExpression, GraphHavingOp, GraphHavingOperand,
    MAX_HAVING_INSTRUCTIONS,
};
use fgdb_types::CanonicalScalar;

#[derive(Clone)]
pub(in crate::graph_text) enum Operand {
    Column(GraphAggregateColumn),
    /// Preserve a pipeline's written alias position for the original row
    /// filter transcript, even when two aliases name the same grouping key.
    RowColumn {
        column: GraphAggregateColumn,
        index: usize,
    },
    Number {
        value: Number,
        legacy_int64: bool,
        at: usize,
    },
    Scalar(ScalarPredicate),
    Expression {
        value: ReadValueTemplate,
        column: usize,
    },
    ResolvedExpression {
        inputs: Vec<ReadValueTemplate>,
        value: ReadValueTemplate,
        column: usize,
    },
}
impl Operand {
    fn bind(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<GraphHavingOperand, GraphPatternTextError> {
        Ok(match self {
            Self::Column(column) | Self::RowColumn { column, .. } => {
                GraphHavingOperand::Column(*column)
            }
            Self::Scalar(value) => GraphHavingOperand::Scalar(value.clone()),
            Self::Expression { value, column } => GraphHavingOperand::Expression {
                value: std::sync::Arc::new(PreparedGraphAggregateText::bind_input_value(
                    value, values,
                )?),
                column: *column,
            },
            Self::ResolvedExpression {
                inputs,
                value,
                column,
            } => GraphHavingOperand::ResolvedExpression {
                inputs: inputs
                    .iter()
                    .map(|input| PreparedGraphAggregateText::bind_input_value(input, values))
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
                value: std::sync::Arc::new(PreparedGraphAggregateText::bind_input_value(
                    value, values,
                )?),
                column: *column,
            },
            Self::Number { value, .. } => match value.value(values) {
                GqlParameterValue::Int64(value) => GraphHavingOperand::Integer(i128::from(value)),
                GqlParameterValue::UInt64(value) => GraphHavingOperand::Integer(i128::from(value)),
                GqlParameterValue::Scalar(value) => {
                    GraphHavingOperand::Scalar(value.predicate(IntegerComparison::Equal))
                }
                // List declarations refuse at the typed number parser; bind is
                // infallible over the admitted operand domain.
                GqlParameterValue::List(_) | GqlParameterValue::Map(_) => {
                    unreachable!("collection parameters never become HAVING operands")
                }
            },
        })
    }
}

#[derive(Clone)]
enum Op {
    Compare {
        left: Operand,
        comparison: IntegerComparison,
        right: Operand,
    },
    IsNull {
        operand: Operand,
        is_null: bool,
    },
    Truth(Option<bool>),
    And,
    Or,
    Not,
}

#[derive(Clone)]
pub(crate) struct HavingTemplate {
    program: Vec<Op>,
    at: usize,
}
impl HavingTemplate {
    /// Keep the existing relational HAVING transcript and native program for
    /// its original column/scalar domain. Newly supported expressions and
    /// hidden summaries retain this resolved template instead of reparsing.
    pub(in crate::graph_text) fn row_filter(
        &self,
        columns: &[GraphAggregateColumn],
        parameters: &[GqlParameterSpec],
    ) -> Option<Vec<crate::set_text::ReadFilterOp>> {
        use crate::set_text::{ReadFilterOp as Filter, ReadFilterOperand as Value};

        let operand = |operand: &Operand| -> Option<Value> {
            Some(match operand {
                Operand::Column(column) => {
                    Value::Column(columns.iter().position(|candidate| candidate == column)?)
                }
                Operand::RowColumn { index, .. } => Value::Column(*index),
                Operand::Scalar(value) => {
                    Value::Literal(GqlScalarParameter::new(value.value().clone()).ok()?)
                }
                Operand::Number { value, at, .. } => match value {
                    Number::Literal(GqlParameterValue::Int64(value)) => {
                        Value::Literal(GqlScalarParameter::new(CanonicalScalar::Int(*value)).ok()?)
                    }
                    Number::Literal(GqlParameterValue::Scalar(value)) => {
                        Value::Literal(value.clone())
                    }
                    Number::Parameter(index)
                        if parameters[*index].parameter_type != GqlParameterType::UInt64 =>
                    {
                        Value::Parameter {
                            index: *index,
                            at: *at,
                        }
                    }
                    _ => return None,
                },
                Operand::Expression { .. } | Operand::ResolvedExpression { .. } => return None,
            })
        };
        self.program
            .iter()
            .map(|op| {
                Some(match op {
                    Op::Compare {
                        left,
                        comparison,
                        right,
                    } => Filter::Compare {
                        left: operand(left)?,
                        comparison: *comparison,
                        right: operand(right)?,
                    },
                    Op::IsNull {
                        operand: value,
                        is_null,
                    } => Filter::IsNull {
                        operand: operand(value)?,
                        is_null: *is_null,
                    },
                    Op::Truth(value) => Filter::Truth(*value),
                    Op::And => Filter::And,
                    Op::Or => Filter::Or,
                    Op::Not => Filter::Not,
                })
            })
            .collect()
    }

    pub(in crate::graph_text) fn attach(
        &self,
        aggregate: PreparedGraphAggregate,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphAggregate, GraphPatternTextError> {
        let program = self
            .program
            .iter()
            .map(|op| {
                Ok(match op {
                    Op::Compare {
                        left,
                        comparison,
                        right,
                    } => GraphHavingOp::Compare {
                        left: left.bind(values)?,
                        comparison: *comparison,
                        right: right.bind(values)?,
                    },
                    Op::IsNull { operand, is_null } => GraphHavingOp::IsNull {
                        operand: operand.bind(values)?,
                        is_null: *is_null,
                    },
                    Op::Truth(value) => GraphHavingOp::Truth(*value),
                    Op::And => GraphHavingOp::And,
                    Op::Or => GraphHavingOp::Or,
                    Op::Not => GraphHavingOp::Not,
                })
            })
            .collect::<Result<Vec<_>, GraphPatternTextError>>()?;
        let failure = |_| {
            error(
                self.at,
                GraphPatternTextErrorKind::Expected("valid bounded HAVING expression"),
            )
        };
        let expression = GraphHavingExpression::prepare(&program).map_err(failure)?;
        aggregate
            .with_having_expression(&expression)
            .map_err(failure)
    }

    /// Resolved unbound HAVING program: column identities, comparisons,
    /// literal/parameter operands and structure. Values never enter.
    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        fn operand(bytes: &mut Vec<u8>, operand: &Operand) {
            match operand {
                Operand::Column(column) | Operand::RowColumn { column, .. } => {
                    bytes.push(0);
                    match column {
                        GraphAggregateColumn::GroupKey(at) => {
                            bytes.push(0);
                            bytes.extend_from_slice(&(*at as u64).to_be_bytes());
                        }
                        GraphAggregateColumn::Aggregate(at) => {
                            bytes.push(1);
                            bytes.extend_from_slice(&(*at as u64).to_be_bytes());
                        }
                    }
                }
                Operand::Number { value, .. } => {
                    bytes.push(1);
                    super::append_number(bytes, value);
                }
                Operand::Scalar(value) => {
                    bytes.push(2);
                    let encoded = value.canonical_value_bytes();
                    bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(encoded);
                }
                Operand::Expression { value, column } => {
                    bytes.push(3);
                    bytes.extend_from_slice(&(*column as u64).to_be_bytes());
                    value.append_template_transcript(bytes);
                }
                Operand::ResolvedExpression {
                    inputs,
                    value,
                    column,
                } => {
                    bytes.push(4);
                    bytes.extend_from_slice(&(*column as u64).to_be_bytes());
                    bytes.extend_from_slice(&(inputs.len() as u64).to_be_bytes());
                    for input in inputs {
                        input.append_template_transcript(bytes);
                    }
                    value.append_template_transcript(bytes);
                }
            }
        }
        bytes.extend_from_slice(&(self.program.len() as u64).to_be_bytes());
        for op in &self.program {
            match op {
                Op::Compare {
                    left,
                    comparison,
                    right,
                } => {
                    bytes.push(0);
                    operand(bytes, left);
                    bytes.push(comparison_tag(*comparison));
                    operand(bytes, right);
                }
                Op::IsNull {
                    operand: inner,
                    is_null,
                } => {
                    bytes.push(1);
                    operand(bytes, inner);
                    bytes.push(u8::from(*is_null));
                }
                Op::Truth(value) => {
                    bytes.push(2);
                    bytes.push(match value {
                        None => 0,
                        Some(false) => 1,
                        Some(true) => 2,
                    });
                }
                Op::And => bytes.push(3),
                Op::Or => bytes.push(4),
                Op::Not => bytes.push(5),
            }
        }
    }
}

/// Length-framed facade view of one resolved HAVING template.
pub(super) fn template_program_bytes(template: &HavingTemplate) -> Vec<u8> {
    let mut bytes = Vec::new();
    template.append_template_transcript(&mut bytes);
    bytes
}

/// Each source owner resolves the same post-group operand grammar against its
/// own grouping and input-expression registry. Boolean syntax and exact native
/// HAVING evaluation are shared by graph heads and relational pipelines.
pub(in crate::graph_text) trait OperandResolver<'a> {
    fn named(&self, name: &str) -> bool;
    fn operand(&mut self, parser: &mut Parser<'a>) -> Result<Operand, GraphPatternTextError>;
}

struct GraphOperands<'p, 'a> {
    returned: &'p [ReturnItem<'a>],
    groups: &'p [Expression<'a>],
    output_columns: &'p [usize],
    hidden: &'p mut Vec<HiddenSummary<'a>>,
    computed: &'p mut ComputedInputs<'a>,
}

pub(in crate::graph_text) fn parse_resolved<'a>(
    parser: &mut Parser<'a>,
    resolve: &mut impl OperandResolver<'a>,
) -> Result<Option<HavingTemplate>, GraphPatternTextError> {
    if !parser.take_word("HAVING")? {
        return Ok(None);
    }
    let at = parser.current.at;
    let mut input = HavingParser {
        parser,
        resolve,
        program: Vec::new(),
        leaves: 0,
        compound: false,
    };
    input.disjunction(0)?;
    Ok(Some(HavingTemplate {
        program: input.program,
        at,
    }))
}

pub(super) fn parse<'a>(
    parser: &mut Parser<'a>,
    returned: &[ReturnItem<'a>],
    groups: &[Expression<'a>],
    output_columns: &[usize],
    hidden: &mut Vec<HiddenSummary<'a>>,
    computed: &mut ComputedInputs<'a>,
) -> Result<(Vec<Having>, Option<HavingTemplate>), GraphPatternTextError> {
    if !parser.take_word("HAVING")? {
        return Ok((Vec::new(), None));
    }
    let at = parser.current.at;
    let mut resolve = GraphOperands {
        returned,
        groups,
        output_columns,
        hidden,
        computed,
    };
    let mut input = HavingParser {
        parser,
        resolve: &mut resolve,
        program: Vec::new(),
        leaves: 0,
        compound: false,
    };
    input.disjunction(0)?;
    // One grammar, not retry-after-error. Preserve the old flat conjunction's
    // lowering, eager numeric-domain checks, counters and transcript bytes.
    if !input.compound {
        let mut flat = Vec::new();
        let mut eligible = true;
        for op in &input.program {
            match op {
                Op::Compare {
                    left: Operand::Column(column),
                    comparison,
                    right:
                        Operand::Number {
                            value,
                            legacy_int64: true,
                            ..
                        },
                } => flat.push(Having {
                    column: *column,
                    test: HavingTest::Integer {
                        comparison: *comparison,
                        value: value.clone(),
                    },
                }),
                Op::IsNull {
                    operand: Operand::Column(column),
                    is_null,
                } => flat.push(Having {
                    column: *column,
                    test: if *is_null {
                        HavingTest::IsNull
                    } else {
                        HavingTest::IsNotNull
                    },
                }),
                Op::And => {}
                _ => {
                    eligible = false;
                    break;
                }
            }
        }
        if eligible {
            return Ok((flat, None));
        }
    }
    Ok((
        Vec::new(),
        Some(HavingTemplate {
            program: input.program,
            at,
        }),
    ))
}

struct HavingParser<'p, 'a> {
    parser: &'p mut Parser<'a>,
    resolve: &'p mut dyn OperandResolver<'a>,
    program: Vec<Op>,
    leaves: usize,
    compound: bool,
}
impl HavingParser<'_, '_> {
    fn emit(&mut self, op: Op) -> Result<(), GraphPatternTextError> {
        if self.program.len() == MAX_HAVING_INSTRUCTIONS {
            return Err(error(
                self.parser.current.at,
                GraphPatternTextErrorKind::Expected("at most 1024 HAVING instructions"),
            ));
        }
        self.program.push(op);
        Ok(())
    }
    fn leaf(&mut self, op: Op) -> Result<(), GraphPatternTextError> {
        self.parser.capacity(
            self.leaves,
            MAX_AGGREGATE_FILTERS,
            crate::algebra::PatternLimitDimension::Predicates,
        )?;
        self.leaves += 1;
        self.emit(op)
    }
    fn disjunction(&mut self, depth: usize) -> Result<(), GraphPatternTextError> {
        self.conjunction(depth)?;
        while self.parser.take_word("OR")? {
            self.compound = true;
            self.conjunction(depth)?;
            self.emit(Op::Or)?;
        }
        Ok(())
    }
    fn conjunction(&mut self, depth: usize) -> Result<(), GraphPatternTextError> {
        self.unary(depth)?;
        while self.parser.take_word("AND")? {
            self.unary(depth)?;
            self.emit(Op::And)?;
        }
        Ok(())
    }
    fn unary(&mut self, depth: usize) -> Result<(), GraphPatternTextError> {
        if depth > 64 {
            return Err(error(
                self.parser.current.at,
                GraphPatternTextErrorKind::Expected("HAVING nesting at most 64"),
            ));
        }
        // Preserve an output named `not` when followed by an operand suffix.
        // NOT (...) or NOT <condition> is unary syntax, not a function call.
        let not_reference = if self.parser.is_word("NOT") {
            let mut lexer = self.parser.lexer.clone();
            let next = lexer.next()?;
            let named = matches!(self.parser.current.kind, TokenKind::Word(word)
                if self.resolve.named(word));
            matches!(
                next.kind,
                TokenKind::Punct(b'.' | b'=' | b'<' | b'>' | b'!')
            ) || matches!(next.kind, TokenKind::Word(word)
                if word.eq_ignore_ascii_case("IS")
                    || word.eq_ignore_ascii_case("IN")
                    || word.eq_ignore_ascii_case("BETWEEN"))
                || (named
                    && matches!(
                        next.kind,
                        TokenKind::Punct(b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'|' | b'[')
                    ))
                || (named
                    && matches!(next.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("NOT"))
                    && matches!(lexer.next()?.kind, TokenKind::Word(word)
                        if word.eq_ignore_ascii_case("IN")
                            || word.eq_ignore_ascii_case("BETWEEN")))
        } else {
            false
        };
        if self.parser.is_word("NOT") && !not_reference {
            self.parser.advance()?;
            self.compound = true;
            self.unary(depth + 1)?;
            return self.emit(Op::Not);
        }
        if self.parser.is_punct(b'(') && !self.parenthesized_operand()? {
            self.parser.advance()?;
            self.compound = true;
            self.disjunction(depth + 1)?;
            return self.parser.punct(b')', ")");
        }
        self.atom()
    }
    /// `(c + 1) > 2` starts an operand; `(c > 1 OR c IS NULL)` starts a
    /// condition. One bounded token lookahead selects the grammar without
    /// reparsing, registering parameters twice or touching the catalog.
    fn parenthesized_operand(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.parser.lexer.clone();
        let mut depth = 1;
        loop {
            match lexer.next()?.kind {
                TokenKind::Punct(b'(') => {
                    depth += 1;
                    if depth > 64 {
                        return Err(error(
                            self.parser.current.at,
                            GraphPatternTextErrorKind::Expected("HAVING nesting at most 64"),
                        ));
                    }
                }
                TokenKind::Punct(b')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(match lexer.next()?.kind {
                            TokenKind::Punct(
                                b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'|' | b'.' | b'[' | b'='
                                | b'<' | b'>' | b'!',
                            ) => true,
                            TokenKind::Word(word) => ["IS", "IN", "BETWEEN", "NOT"]
                                .iter()
                                .any(|keyword| word.eq_ignore_ascii_case(keyword)),
                            _ => false,
                        });
                    }
                }
                TokenKind::End => return Ok(false),
                _ => {}
            }
        }
    }

    fn operand(&mut self) -> Result<Operand, GraphPatternTextError> {
        // A standalone finite decimal, including its old signed-literal
        // spelling, retains the scalar operand and its transcript. A following
        // arithmetic operator selects the shared expression grammar instead.
        if let Some((_, consumed)) = self.parser.scan_float_literal()? {
            let mut lexer = self.parser.lexer.clone();
            for _ in 1..consumed {
                lexer.next()?;
            }
            if !matches!(
                lexer.next()?.kind,
                TokenKind::Punct(b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'|' | b'.' | b'[')
            ) {
                let at = self.parser.current.at;
                let value = self.parser.float_literal()?.expect("scanned float literal");
                return ScalarPredicate::new(value, IntegerComparison::Equal)
                    .map(Operand::Scalar)
                    .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral));
            }
        }
        // The pre-existing bare UInt64 HAVING domain includes values above
        // i64::MAX. Keep it outside the shared scalar language's Int64-only
        // parameter domain; arithmetic never silently narrows this argument.
        if let TokenKind::Parameter(name) = self.parser.current.kind
            && self.parser.parameter_types.get(name) == Some(&GqlParameterType::UInt64)
            && !matches!(
                self.parser.lexer.clone().next()?.kind,
                TokenKind::Punct(b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'|' | b'.' | b'[')
            )
        {
            return Ok(Operand::Number {
                at: self.parser.current.at,
                value: self.parser.number(GqlParameterType::UInt64)?,
                legacy_int64: false,
            });
        }
        let outer = core::mem::replace(&mut self.parser.having_operand, true);
        let operand = self.resolve.operand(self.parser);
        self.parser.having_operand = outer;
        operand
    }
}

impl<'a> OperandResolver<'a> for GraphOperands<'_, 'a> {
    fn named(&self, name: &str) -> bool {
        self.returned.iter().any(|item| item.alias.text == name)
            || self
                .groups
                .iter()
                .any(|group| group.property.is_none() && group.variable.text == name)
    }

    fn operand(&mut self, parser: &mut Parser<'a>) -> Result<Operand, GraphPatternTextError> {
        let (value, columns) = parser.aggregate_result_expression(
            self.returned,
            self.groups,
            self.hidden,
            self.computed,
            false,
        )?;
        resolve_operand(parser, value, columns, self.groups.len(), |column| {
            let mut value = self.returned[column]
                .output
                .clone()
                .expect("computed alias");
            remap_output_columns(&mut value, self.output_columns);
            value
        })
    }
}

/// Convert source-resolved leaves to exact HAVING operands. A computed alias
/// is supplied over the immutable evaluation row, never the public projection.
pub(in crate::graph_text) fn resolve_operand(
    parser: &Parser<'_>,
    mut value: ReadValueTemplate,
    columns: Vec<ResultOrderColumn>,
    key_width: usize,
    alias: impl Fn(usize) -> ReadValueTemplate,
) -> Result<Operand, GraphPatternTextError> {
    let evaluation_column = |column| match column {
        GraphAggregateColumn::GroupKey(index) => index,
        GraphAggregateColumn::Aggregate(index) => key_width + index,
    };
    // Preserve existing simple operands, transcripts and flat numeric
    // lowering. Only an operator or function selects an expression plan.
    match &value {
        ReadValueTemplate::Column(index) => match columns[*index] {
            ResultOrderColumn::Evaluation(column) => return Ok(Operand::Column(column)),
            ResultOrderColumn::Projected(column) => {
                let value = alias(column);
                return Ok(Operand::Expression { value, column });
            }
            ResultOrderColumn::Computed(_) => {
                unreachable!("resolved leaves are not order ranks")
            }
        },
        ReadValueTemplate::Literal(value) => {
            return Ok(match value.value() {
                CanonicalScalar::Int(value) => Operand::Number {
                    value: Number::Literal(GqlParameterValue::Int64(*value)),
                    legacy_int64: true,
                    at: parser.current.at,
                },
                _ => Operand::Scalar(value.predicate(IntegerComparison::Equal)),
            });
        }
        ReadValueTemplate::Parameter { index, at, .. }
            if !matches!(
                parser.syntax.parameters[*index].parameter_type,
                GqlParameterType::List | GqlParameterType::Map
            ) =>
        {
            return Ok(Operand::Number {
                value: Number::Parameter(*index),
                legacy_int64: parser.syntax.parameters[*index].parameter_type
                    == GqlParameterType::Int64,
                at: *at,
            });
        }
        _ => {}
    }
    let column = columns.iter().find_map(|column| match column {
        ResultOrderColumn::Projected(index) => Some(*index),
        _ => None,
    });
    if let Some(column) = column {
        // Scalar aliases compose before the shared VM compiles its lazy
        // branches. Inlining postfix programs preserves CASE/COALESCE,
        // including a skipped alias whose own expression would fail.
        if inline_scalar_aliases(&mut value, &columns, key_width, &alias)? {
            return Ok(Operand::Expression { value, column });
        }
        // A composite alias needs a private resolved row. Until that row
        // supports deferred cells, refuse conditional uses explicitly;
        // never turn an unevaluated branch into an eager alias error.
        if has_deferred_values(&value) {
            return Err(error(
                parser.current.at,
                GraphPatternTextErrorKind::Expected(
                    "scalar computed aliases in conditional HAVING operands",
                ),
            ));
        }
        // A private operand row contains only its referenced inputs. Alias
        // values evaluate once per distinct reference, over the frozen
        // group; their native numeric domains and element identities stay
        // intact. No visible output or grouping key is added.
        let mut distinct = Vec::new();
        let mut inputs = Vec::new();
        let mut remap = Vec::new();
        for source in columns {
            let index = if let Some(index) = distinct.iter().position(|old| *old == source) {
                index
            } else {
                let input = match source {
                    ResultOrderColumn::Evaluation(column) => {
                        ReadValueTemplate::Column(evaluation_column(column))
                    }
                    ResultOrderColumn::Projected(index) => alias(index),
                    ResultOrderColumn::Computed(_) => {
                        unreachable!("resolved leaves are not order ranks")
                    }
                };
                distinct.push(source);
                inputs.push(input);
                inputs.len() - 1
            };
            remap.push(index);
        }
        remap_output_columns(&mut value, &remap);
        Ok(Operand::ResolvedExpression {
            inputs,
            value,
            column,
        })
    } else {
        let columns = columns
            .into_iter()
            .map(|column| match column {
                ResultOrderColumn::Evaluation(column) => evaluation_column(column),
                _ => unreachable!("computed aliases are resolved above"),
            })
            .collect::<Vec<_>>();
        remap_output_columns(&mut value, &columns);
        Ok(Operand::Expression { value, column: 0 })
    }
}

fn inline_scalar_aliases(
    value: &mut ReadValueTemplate,
    columns: &[ResultOrderColumn],
    key_width: usize,
    alias: &impl Fn(usize) -> ReadValueTemplate,
) -> Result<bool, GraphPatternTextError> {
    use crate::GraphIntegerOp;
    use crate::mutation_text::MutationIntegerTemplateOp as Template;

    let ReadValueTemplate::Integer { program, at } = value else {
        return Ok(false);
    };
    let mut expanded = Vec::new();
    for op in program.iter() {
        let source = match op {
            Template::Bound(
                GraphIntegerOp::Column(index)
                | GraphIntegerOp::ScalarColumn(index)
                | GraphIntegerOp::IdentityColumn { column: index, .. },
            ) => Some(*index),
            _ => None,
        };
        let Some(source) = source else {
            expanded.push(op.clone());
            continue;
        };
        let mapped = match columns[source] {
            ResultOrderColumn::Evaluation(column) => Some(match column {
                GraphAggregateColumn::GroupKey(index) => index,
                GraphAggregateColumn::Aggregate(index) => key_width + index,
            }),
            ResultOrderColumn::Projected(index) => {
                let alias = alias(index);
                if let ReadValueTemplate::Column(index) = alias {
                    Some(index)
                } else {
                    if matches!(op, Template::Bound(GraphIntegerOp::IdentityColumn { .. })) {
                        return Ok(false);
                    }
                    match alias {
                        ReadValueTemplate::Integer { program, .. } => expanded.extend(program),
                        ReadValueTemplate::Literal(value) => expanded.push(Template::Bound(
                            GraphIntegerOp::Scalar(value.predicate(IntegerComparison::Equal)),
                        )),
                        ReadValueTemplate::Parameter { index, at, .. } => {
                            expanded.push(Template::Parameter { index, at });
                        }
                        _ => return Ok(false),
                    }
                    None
                }
            }
            ResultOrderColumn::Computed(_) => {
                unreachable!("resolved leaves are not order ranks")
            }
        };
        if let Some(column) = mapped {
            expanded.push(Template::Bound(match op {
                Template::Bound(GraphIntegerOp::Column(_)) => GraphIntegerOp::Column(column),
                Template::Bound(GraphIntegerOp::ScalarColumn(_)) => {
                    GraphIntegerOp::ScalarColumn(column)
                }
                Template::Bound(GraphIntegerOp::IdentityColumn { text, .. }) => {
                    GraphIntegerOp::IdentityColumn {
                        column,
                        text: *text,
                    }
                }
                _ => unreachable!("column instruction checked above"),
            }));
        }
        if expanded.len() > crate::MAX_GRAPH_INTEGER_INSTRUCTIONS {
            return Err(error(
                *at,
                GraphPatternTextErrorKind::Expected("bounded computed HAVING operand"),
            ));
        }
    }
    if expanded.len() > crate::MAX_GRAPH_INTEGER_INSTRUCTIONS {
        return Err(error(
            *at,
            GraphPatternTextErrorKind::Expected("bounded computed HAVING operand"),
        ));
    }
    *program = expanded;
    Ok(true)
}

impl HavingParser<'_, '_> {
    fn atom(&mut self) -> Result<(), GraphPatternTextError> {
        let at = self.parser.current.at;
        let left = self.operand()?;
        if self.parser.take_word("IS")? {
            let negate = self.parser.take_word("NOT")?;
            self.parser.word("NULL")?;
            return self.leaf(Op::IsNull {
                operand: left,
                is_null: !negate,
            });
        }
        if self.parser.is_word("IN") || self.parser.is_word("BETWEEN") || self.parser.is_word("NOT")
        {
            return self.membership_or_range(left);
        }
        if matches!(
            self.parser.current.kind,
            TokenKind::Punct(b'=' | b'<' | b'>' | b'!')
        ) {
            let mut left = left;
            let mut chained = false;
            loop {
                let comparison = self.parser.comparison()?;
                let right = self.operand()?;
                self.leaf(Op::Compare {
                    left,
                    comparison,
                    right: right.clone(),
                })?;
                if chained {
                    self.emit(Op::And)?;
                }
                if !matches!(
                    self.parser.current.kind,
                    TokenKind::Punct(b'=' | b'<' | b'>' | b'!')
                ) {
                    return Ok(());
                }
                // Resolve each middle operand once, then reuse its immutable
                // program in the neighboring comparisons, as row filters do.
                left = right;
                chained = true;
                self.compound = true;
            }
        }
        if let Operand::Scalar(value) = left {
            let truth = match value.value() {
                CanonicalScalar::Bool(value) => Some(*value),
                CanonicalScalar::Null => None,
                _ => {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected("HAVING comparison or null test"),
                    ));
                }
            };
            return self.leaf(Op::Truth(truth));
        }
        Err(error(
            at,
            GraphPatternTextErrorKind::Expected("HAVING comparison or null test"),
        ))
    }

    /// Reuse resolved operands, not text. A left parameter or hidden aggregate
    /// is registered once, even when its immutable value is compared with many
    /// members. Cloning a reference does not create another aggregate state.
    fn membership_or_range(&mut self, left: Operand) -> Result<(), GraphPatternTextError> {
        self.compound = true;
        let negate = self.parser.take_word("NOT")?;
        if self.parser.take_word("IN")? {
            self.parser.punct(b'[', "[")?;
            if self.parser.take(b']')? {
                // Do not prune the left operand's hidden aggregate or its source
                // errors merely because membership in an empty list is false.
                self.leaf(Op::IsNull {
                    operand: left,
                    is_null: true,
                })?;
                self.leaf(Op::Truth(Some(false)))?;
                self.emit(Op::And)?;
            } else {
                let mut first = true;
                loop {
                    self.parser.capacity(
                        self.leaves,
                        MAX_AGGREGATE_FILTERS,
                        crate::algebra::PatternLimitDimension::Predicates,
                    )?;
                    let right = self.operand()?;
                    self.leaf(Op::Compare {
                        left: left.clone(),
                        comparison: IntegerComparison::Equal,
                        right,
                    })?;
                    if !first {
                        self.emit(Op::Or)?;
                    }
                    first = false;
                    if self.parser.take(b']')? {
                        break;
                    }
                    self.parser.punct(b',', ", or ]")?;
                }
            }
        } else {
            self.parser.word("BETWEEN")?;
            let lower = self.operand()?;
            self.leaf(Op::Compare {
                left: left.clone(),
                comparison: IntegerComparison::GreaterOrEqual,
                right: lower,
            })?;
            // Consume only the range delimiter; the outer conjunction remains
            // the owner of any subsequent AND. No symmetric-bound reordering.
            self.parser.word("AND")?;
            let upper = self.operand()?;
            self.leaf(Op::Compare {
                left,
                comparison: IntegerComparison::LessOrEqual,
                right: upper,
            })?;
            self.emit(Op::And)?;
        }
        if negate {
            self.emit(Op::Not)?;
        }
        Ok(())
    }
}

/// Value programs with a conditional or per-element child cannot eagerly
/// resolve composite aliases. Scalar postfix aliases inline before this gate.
fn has_deferred_values(value: &ReadValueTemplate) -> bool {
    use crate::GraphIntegerOp;
    use crate::mutation_text::MutationIntegerTemplateOp;

    match value {
        ReadValueTemplate::Integer { program, .. } => program.iter().any(|op| {
            matches!(
                op,
                MutationIntegerTemplateOp::Bound(
                    GraphIntegerOp::Case
                        | GraphIntegerOp::SimpleCase { .. }
                        | GraphIntegerOp::Coalesce
                )
            )
        }),
        ReadValueTemplate::Case { .. }
        | ReadValueTemplate::Comprehension { .. }
        | ReadValueTemplate::Quantifier { .. }
        | ReadValueTemplate::Reduce { .. }
        | ReadValueTemplate::MapOverlay { .. }
        | ReadValueTemplate::MapLiteral { guard: Some(_), .. }
        | ReadValueTemplate::Function {
            function: crate::GraphValueFunction::Coalesce,
            ..
        } => true,
        ReadValueTemplate::List(values)
        | ReadValueTemplate::MapLiteral {
            values,
            guard: None,
            ..
        } => values.iter().any(has_deferred_values),
        ReadValueTemplate::Index { list, index } => {
            has_deferred_values(list) || has_deferred_values(index)
        }
        ReadValueTemplate::In { value, list } => {
            has_deferred_values(value) || has_deferred_values(list)
        }
        ReadValueTemplate::Equal { left, right }
        | ReadValueTemplate::Ordered { left, right, .. } => {
            has_deferred_values(left) || has_deferred_values(right)
        }
        ReadValueTemplate::Size(value)
        | ReadValueTemplate::MapGet { map: value, .. }
        | ReadValueTemplate::Keys(value)
        | ReadValueTemplate::ElementMetadata { value, .. } => has_deferred_values(value),
        ReadValueTemplate::Temporal { argument, .. }
        | ReadValueTemplate::Function { argument, .. } => has_deferred_values(argument),
        ReadValueTemplate::Slice { list, from, to } => {
            has_deferred_values(list)
                || from.as_deref().is_some_and(has_deferred_values)
                || to.as_deref().is_some_and(has_deferred_values)
        }
        ReadValueTemplate::Range { start, end, step } => {
            has_deferred_values(start)
                || has_deferred_values(end)
                || step.as_deref().is_some_and(has_deferred_values)
        }
        ReadValueTemplate::Let { values, body } => {
            values.iter().any(has_deferred_values) || has_deferred_values(body)
        }
        ReadValueTemplate::Column(_)
        | ReadValueTemplate::Literal(_)
        | ReadValueTemplate::Parameter { .. }
        | ReadValueTemplate::Local(_)
        | ReadValueTemplate::Random { .. } => false,
    }
}

#[cfg(test)]
mod compound_tests {
    use super::*;
    use std::cell::Cell;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }

    fn prepare(predicate: &str) -> PreparedGraphAggregate {
        PreparedGraphAggregateText::prepare(
            &format!("MATCH (n) RETURN COUNT(*) AS c HAVING {predicate}"),
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    }

    #[test]
    fn membership_and_ranges_reuse_the_exact_having_program() {
        for (compound, expanded) in [
            ("c IN [0, 1, NULL]", "(c = 0 OR c = 1 OR c = NULL)"),
            ("c NOT IN [1, 2]", "NOT (c = 1 OR c = 2)"),
            ("c BETWEEN -1 AND 3", "(c >= -1 AND c <= 3)"),
            ("c NOT BETWEEN 1 AND 2", "NOT (c >= 1 AND c <= 2)"),
            ("c IN []", "(c IS NULL AND FALSE)"),
            ("c NOT IN []", "NOT (c IS NULL AND FALSE)"),
        ] {
            assert_eq!(prepare(compound), prepare(expanded), "{compound}");
        }
    }

    #[test]
    fn hidden_aggregate_operands_share_one_summary_and_remain_hidden() {
        for (compound, expanded) in [
            ("MIN(n.p) IN [1, 2]", "(MIN(n.p) = 1 OR MIN(n.p) = 2)"),
            (
                "c IN [MIN(n.p), MAX(n.p)]",
                "(c = MIN(n.p) OR c = MAX(n.p))",
            ),
            (
                "c BETWEEN MIN(n.p) AND MAX(n.p)",
                "(c >= MIN(n.p) AND c <= MAX(n.p))",
            ),
            ("MIN(n.p) NOT IN []", "NOT (MIN(n.p) IS NULL AND FALSE)"),
        ] {
            assert_eq!(prepare(compound), prepare(expanded), "{compound}");
        }
        let template = PreparedGraphAggregateText::prepare(
            "MATCH (n) RETURN COUNT(*) AS c HAVING MIN(n.p) IN [1, 2, 3]",
            symbols,
        )
        .unwrap();
        assert_eq!(template.columns(), &["c"]);
        assert_eq!(template.output_slots().len(), 1);
    }

    #[test]
    fn left_parameter_occurrences_are_not_multiplied_by_lowering() {
        let calls = Cell::new(0);
        let template = PreparedGraphAggregateText::prepare(
            "MATCH (n) RETURN COUNT(*) AS c HAVING $x IN [c, $x] AND MIN(n.p) BETWEEN $lo AND $hi",
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert_eq!(template.parameter_schema()[0].occurrences, 2);
        let arguments = GqlParameters::new()
            .with_int64("x", 1)
            .unwrap()
            .with_int64("lo", i64::MIN)
            .unwrap()
            .with_int64("hi", i64::MAX)
            .unwrap();
        let bound = template.bind_parameters(&arguments).unwrap();
        assert_eq!(bound, template.bind_parameters(&arguments).unwrap());
        assert_eq!(calls.get(), 1);
        assert!(matches!(
            template
                .bind_parameters(&GqlParameters::new())
                .unwrap_err()
                .kind,
            GraphPatternTextErrorKind::MissingParameter
        ));
    }

    #[test]
    fn not_alias_and_prefix_negation_keep_their_distinct_roles() {
        for predicate in [
            "not IN [1]",
            "not NOT IN [1]",
            "NOT not IN [1]",
            "not BETWEEN 0 AND 2",
            "not NOT BETWEEN 0 AND 2",
            "NOT not BETWEEN 0 AND 2",
        ] {
            PreparedGraphAggregateText::prepare(
                &format!("MATCH (n) RETURN COUNT(*) AS not HAVING {predicate}"),
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        }
    }

    #[test]
    fn malformed_and_excessive_membership_refuses_before_catalog_access() {
        for predicate in [
            "MIN(n.p) IN [1,]",
            "MIN(n.p) IN [1 2]",
            "MIN(n.p) IN $list",
            "MIN(n.p) BETWEEN 1",
            "MIN(n.p) NOT BETWEEN 1 OR 2",
        ] {
            let calls = Cell::new(0);
            assert!(
                PreparedGraphAggregateText::prepare(
                    &format!("MATCH (n) RETURN COUNT(*) AS c HAVING {predicate}"),
                    |kind, name| {
                        calls.set(calls.get() + 1);
                        symbols(kind, name)
                    },
                )
                .is_err(),
                "{predicate}"
            );
            assert_eq!(calls.get(), 0, "{predicate}");
        }
        let members = vec!["1"; MAX_AGGREGATE_FILTERS + 1].join(",");
        let calls = Cell::new(0);
        assert!(
            PreparedGraphAggregateText::prepare(
                &format!("MATCH (n) RETURN COUNT(*) AS c HAVING MIN(n.p) IN [{members}]"),
                |kind, name| {
                    calls.set(calls.get() + 1);
                    symbols(kind, name)
                },
            )
            .is_err()
        );
        assert_eq!(calls.get(), 0);
    }
}
