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
        GraphMutationTextErrorKind::Build(_) => {
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
            .map(|&(variable, property)| Projection {
                variable,
                property,
                path: None,
            })
            .collect();
        // A bound vertex with a literal-looking name retains its identity.
        // Integer operators cannot silently cast it into a scalar column.
        let bare_vertex = if self.starts_integer_case()? {
            false
        } else if let TokenKind::Word(word) = self.current.kind {
            let next = self.lexer.clone().next()?;
            !matches!(next.kind, TokenKind::Punct(b'.' | b'('))
                && (self.syntax.variables.iter().any(|name| name.text == word)
                    || !(word.eq_ignore_ascii_case("TRUE")
                        || word.eq_ignore_ascii_case("FALSE")
                        || word.eq_ignore_ascii_case("NULL")))
        } else {
            false
        };
        let operand = if bare_vertex {
            let variable = self.variable()?;
            Operand::Column(self.mutation_projection(&mut sources, variable, None)?)
        } else {
            self.aggregate_value_expression(&mut sources)
                .map_err(scalar_error)?
        };
        computed.sources = sources
            .iter()
            .map(|source| (source.variable, source.property))
            .collect();
        if let Operand::Column(column) = operand {
            let source = sources[column];
            return Ok(Expression {
                variable: source.variable,
                property: source.property,
                computed: None,
            });
        }
        let value = match operand {
            Operand::Literal(value) => ReadValueTemplate::Literal(value),
            Operand::Number(Number::Literal(value)) => {
                ReadValueTemplate::Literal(scalar(value, at)?)
            }
            Operand::Number(Number::Parameter(index)) => ReadValueTemplate::Parameter { index, at },
            Operand::Integer { program, at } => ReadValueTemplate::Integer { program, at },
            Operand::Column(_) => unreachable!("plain scalar/vertex inputs returned above"),
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
            ReadValueTemplate::Size(inner) => {
                Ok(GraphSetValue::Size(Box::new(Self::bind_input_value(
                    inner, values,
                )?)))
            }
        }
    }
}
