//! Aggregate scalar arguments reuse the existing read/mutation expression
//! compiler and the enclosing MATCH parameter table. No text is synthesized,
//! no scalar is reparsed at bind, and no aggregate is evaluated by this parser.

use super::*;
use crate::graph_text::aggregate::{ComputedInputs, Expression, PreparedGraphAggregateText};
use crate::set_text::{ReadProjectionTemplate, ReadValueTemplate};
use crate::{GraphSetProjection, GraphSetValue};

fn scalar_error(source: GraphMutationTextError) -> GraphPatternTextError {
    let kind = match source.kind {
        GraphMutationTextErrorKind::Query(kind) => kind,
        GraphMutationTextErrorKind::IntegerOperand => GraphPatternTextErrorKind::ScalarLiteral,
        GraphMutationTextErrorKind::IntegerNesting { .. } => {
            GraphPatternTextErrorKind::Expected("aggregate scalar nesting at most 64")
        }
        GraphMutationTextErrorKind::IntegerExpression(_) => {
            GraphPatternTextErrorKind::Expected("valid bounded aggregate scalar expression")
        }
        GraphMutationTextErrorKind::Relation(crate::GraphSetTextErrorKind::Pattern(kind)) => kind,
        GraphMutationTextErrorKind::Build(_)
        | GraphMutationTextErrorKind::Relation(_)
        | GraphMutationTextErrorKind::ReturnBuild(_) => {
            GraphPatternTextErrorKind::Expected("aggregate scalar operand")
        }
    };
    error(source.offset, kind)
}

fn same_value(left: &ReadValueTemplate, right: &ReadValueTemplate) -> bool {
    match (left, right) {
        (ReadValueTemplate::Column(a), ReadValueTemplate::Column(b)) => a == b,
        (ReadValueTemplate::Literal(a), ReadValueTemplate::Literal(b)) => a == b,
        (
            ReadValueTemplate::Parameter { index: a, .. },
            ReadValueTemplate::Parameter { index: b, .. },
        ) => a == b,
        (
            ReadValueTemplate::Integer { program: a, .. },
            ReadValueTemplate::Integer { program: b, .. },
        ) => {
            a.len() == b.len()
                && a.iter().zip(b).all(|(a, b)| match (a, b) {
                    (MutationIntegerTemplateOp::Bound(a), MutationIntegerTemplateOp::Bound(b)) => {
                        a == b
                    }
                    (
                        MutationIntegerTemplateOp::Parameter { index: a, .. },
                        MutationIntegerTemplateOp::Parameter { index: b, .. },
                    ) => a == b,
                    (
                        MutationIntegerTemplateOp::ParameterField {
                            index: a,
                            keys: a_keys,
                            ..
                        },
                        MutationIntegerTemplateOp::ParameterField {
                            index: b,
                            keys: b_keys,
                            ..
                        },
                    ) => a == b && a_keys == b_keys,
                    _ => false,
                })
        }
        _ => false,
    }
}

impl<'a> Parser<'a> {
    pub(in crate::graph_text) fn aggregate_scalar_expression(
        &mut self,
        computed: &mut ComputedInputs<'a>,
    ) -> Result<Expression<'a>, GraphPatternTextError> {
        let at = self.current.at;
        let mut sources: Vec<_> = computed
            .sources
            .iter()
            .map(|&(variable, property, path)| Projection {
                variable,
                property,
                path,
            })
            .collect();
        // A bound vertex, edge, or path with a literal-looking name retains its identity.
        // Integer operators cannot silently cast it into a scalar column.
        let bare_variable = if self.starts_integer_case()? {
            false
        } else if let TokenKind::Word(word) = self.current.kind {
            let next = self.lexer.clone().next()?;
            !matches!(next.kind, TokenKind::Punct(b'.' | b'('))
                && (self.syntax.variables.iter().any(|name| name.text == word)
                    || self.syntax.path.is_some_and(|path| path.text == word)
                    || self.syntax.visible_edge(word).is_some()
                    || !(word.eq_ignore_ascii_case("TRUE")
                        || word.eq_ignore_ascii_case("FALSE")
                        || word.eq_ignore_ascii_case("NULL")))
        } else {
            false
        };
        // A list or map argument, as in COUNT(DISTINCT [a, b]), reads with the
        // RETURN value grammar over the same input sources. The input binder
        // already lowers every composite template, so no new evaluator exists.
        let composite = self.is_punct(b'[')
            || self.is_punct(b'{')
            || ((self.is_word("KEYS") || self.is_word("PROPERTIES"))
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')))
            || (matches!(self.current.kind, TokenKind::Word(_))
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'{')));
        let operand = if composite {
            None
        } else if bare_variable {
            let variable = self.any_variable()?;
            Some(Operand::Column(self.mutation_projection(
                &mut sources,
                variable,
                None,
            )?))
        } else {
            Some(
                self.aggregate_value_expression(&mut sources)
                    .map_err(scalar_error)?,
            )
        };
        // Err carries a plain scalar/vertex input's column.
        let value = match operand {
            None => match self.read_graph_value(&mut sources, 0).map_err(|source| {
                let kind = match source.kind {
                    crate::GraphSetTextErrorKind::Pattern(kind) => kind,
                    _ => GraphPatternTextErrorKind::Expected(
                        "valid bounded aggregate list or map argument",
                    ),
                };
                error(source.offset, kind)
            })? {
                ReadValueTemplate::Column(column) => Err(column),
                value => Ok(value),
            },
            Some(Operand::Column(column)) => Err(column),
            Some(Operand::Literal(value)) => Ok(ReadValueTemplate::Literal(value)),
            Some(Operand::Number(Number::Literal(value))) => {
                Ok(ReadValueTemplate::Literal(scalar(value, at)?))
            }
            Some(Operand::Number(Number::Parameter(index))) => {
                Ok(ReadValueTemplate::Parameter { index, at })
            }
            Some(Operand::Integer { program, at }) => {
                Ok(ReadValueTemplate::Integer { program, at })
            }
        };
        computed.sources = sources
            .iter()
            .map(|source| (source.variable, source.property, source.path))
            .collect();
        let value = match value {
            Ok(value) => value,
            Err(column) => {
                let source = sources[column];
                return Ok(Expression {
                    variable: source.variable,
                    property: source.property,
                    path: source.path,
                    computed: None,
                });
            }
        };
        let index = if let Some(index) = computed
            .operands
            .iter()
            .position(|old| same_value(old, &value))
        {
            index
        } else {
            self.capacity(
                computed.operands.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let index = computed.operands.len();
            computed.operands.push(value);
            index
        };
        Ok(Expression {
            variable: Name { text: "", at },
            property: None,
            path: None,
            computed: Some(index),
        })
    }
}

impl PreparedGraphAggregateText {
    pub(in crate::graph_text) fn bind_input_projection(
        projection: &[ReadProjectionTemplate],
        values: &[GqlParameterValue],
    ) -> Result<Vec<GraphSetProjection>, GraphPatternTextError> {
        let mut columns = Vec::new();
        for output in projection {
            let value = Self::bind_input_value(&output.value, values)?;
            columns.push(GraphSetProjection::new(output.name.clone(), value));
        }
        Ok(columns)
    }

    /// Binds one template to its typed set expression. List/index/size lower
    /// recursively so aggregate arguments admit composite list expressions
    /// under the same admission rules as the projection language.
    pub(in crate::graph_text) fn bind_input_value(
        template: &ReadValueTemplate,
        values: &[GqlParameterValue],
    ) -> Result<GraphSetValue, GraphPatternTextError> {
        match template {
            ReadValueTemplate::Column(input) => Ok(GraphSetValue::Column(*input)),
            ReadValueTemplate::Literal(value) => Ok(GraphSetValue::Literal(value.clone())),
            ReadValueTemplate::Parameter { index, at } => {
                Ok(GraphSetValue::Literal(scalar(values[*index].clone(), *at)?))
            }
            ReadValueTemplate::Integer { program, at } => Ok(GraphSetValue::Integer(
                integer::bind_integer(program, values, *at).map_err(scalar_error)?,
            )),
            ReadValueTemplate::List(items) => Ok(GraphSetValue::List(
                items
                    .iter()
                    .map(|item| Self::bind_input_value(item, values))
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            ReadValueTemplate::Index { list, index } => Ok(GraphSetValue::Index {
                list: Box::new(Self::bind_input_value(list, values)?),
                index: Box::new(Self::bind_input_value(index, values)?),
            }),
            ReadValueTemplate::Size(inner) => Ok(GraphSetValue::Size(Box::new(
                Self::bind_input_value(inner, values)?,
            ))),
            ReadValueTemplate::In { value, list } => Ok(GraphSetValue::In {
                value: Box::new(Self::bind_input_value(value, values)?),
                list: Box::new(Self::bind_input_value(list, values)?),
            }),
            ReadValueTemplate::Local(offset) => Ok(GraphSetValue::Local(*offset)),
            ReadValueTemplate::Comprehension { list, filter, map } => {
                let part = |part: &Option<Box<ReadValueTemplate>>| {
                    part.as_deref()
                        .map(|part| Self::bind_input_value(part, values).map(Box::new))
                        .transpose()
                };
                Ok(GraphSetValue::Comprehension {
                    list: Box::new(Self::bind_input_value(list, values)?),
                    filter: part(filter)?,
                    map: part(map)?,
                })
            }
            ReadValueTemplate::Quantifier {
                kind,
                list,
                predicate,
            } => Ok(GraphSetValue::Quantifier {
                kind: *kind,
                list: Box::new(Self::bind_input_value(list, values)?),
                predicate: Box::new(Self::bind_input_value(predicate, values)?),
            }),
            ReadValueTemplate::Slice { list, from, to } => {
                let part = |part: &Option<Box<ReadValueTemplate>>| {
                    part.as_deref()
                        .map(|part| Self::bind_input_value(part, values).map(Box::new))
                        .transpose()
                };
                Ok(GraphSetValue::Slice {
                    list: Box::new(Self::bind_input_value(list, values)?),
                    from: part(from)?,
                    to: part(to)?,
                })
            }
            ReadValueTemplate::Range { start, end, step } => Ok(GraphSetValue::Range {
                start: Box::new(Self::bind_input_value(start, values)?),
                end: Box::new(Self::bind_input_value(end, values)?),
                step: step
                    .as_deref()
                    .map(|step| Self::bind_input_value(step, values).map(Box::new))
                    .transpose()?,
            }),
            ReadValueTemplate::Reduce { init, list, expr } => Ok(GraphSetValue::Reduce {
                init: Box::new(Self::bind_input_value(init, values)?),
                list: Box::new(Self::bind_input_value(list, values)?),
                expr: Box::new(Self::bind_input_value(expr, values)?),
            }),
            ReadValueTemplate::MapLiteral {
                keys,
                values: entries,
                guard,
            } => Ok(GraphSetValue::MapLiteral {
                keys: keys.clone(),
                values: entries
                    .iter()
                    .map(|value| Self::bind_input_value(value, values))
                    .collect::<Result<_, _>>()?,
                guard: guard
                    .as_deref()
                    .map(|guard| Self::bind_input_value(guard, values).map(Box::new))
                    .transpose()?,
            }),
            ReadValueTemplate::MapGet { map, key } => Ok(GraphSetValue::MapGet {
                map: Box::new(Self::bind_input_value(map, values)?),
                key: key.clone(),
            }),
            ReadValueTemplate::MapOverlay {
                base,
                keys,
                values: entries,
            } => Ok(GraphSetValue::MapOverlay {
                base: Box::new(Self::bind_input_value(base, values)?),
                keys: keys.clone(),
                values: entries
                    .iter()
                    .map(|value| Self::bind_input_value(value, values))
                    .collect::<Result<_, _>>()?,
            }),
            ReadValueTemplate::Keys(map) => Ok(GraphSetValue::Keys(Box::new(
                Self::bind_input_value(map, values)?,
            ))),
        }
    }
}
