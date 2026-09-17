//! Read projection uses the mutation parser's existing scalar operands and
//! precedence compiler. Only terminal semantics differ: no action or write is
//! constructed. Hidden source columns retain every requested property read.

mod aggregate;
mod pipeline;

use super::*;
use crate::graph_text::parameters::UnresolvedGraphText;
use crate::set_text::{
    BoundSetTextInput, ReadProjectionTemplate, ReadStageTemplate, ReadValueTemplate,
};
use crate::{
    GraphSetColumnType, GraphSetProjection, GraphSetTextError, GraphSetTextErrorKind,
    GraphSetValue, PreparedGraphSet,
};

fn expression_error(source: GraphMutationTextError) -> GraphSetTextError {
    let kind = match source.kind {
        GraphMutationTextErrorKind::Query(kind) => GraphSetTextErrorKind::Pattern(kind),
        GraphMutationTextErrorKind::IntegerExpression(kind) => {
            GraphSetTextErrorKind::IntegerExpression(kind)
        }
        GraphMutationTextErrorKind::IntegerOperand => GraphSetTextErrorKind::IntegerOperand,
        GraphMutationTextErrorKind::IntegerNesting { limit } => {
            GraphSetTextErrorKind::IntegerNesting { limit }
        }
        GraphMutationTextErrorKind::Build(_) => {
            GraphSetTextErrorKind::Expected("read scalar expression")
        }
    };
    GraphSetTextError {
        offset: source.offset,
        kind,
    }
}

struct GraphProjectionHead<'a> {
    with: bool,
    inputs: Vec<Projection<'a>>,
    outputs: Vec<(Name<'a>, Operand)>,
}
impl<'a> GraphProjectionHead<'a> {
    fn schema(&self) -> pipeline::RowSchema<'a> {
        self.outputs
            .iter()
            .map(|(name, operand)| {
                let kind = match operand {
                    Operand::Column(input) if self.inputs[*input].property.is_none() => {
                        GraphSetColumnType::Vertex
                    }
                    _ => GraphSetColumnType::Scalar,
                };
                (*name, kind)
            })
            .collect()
    }
}

impl<'a> Parser<'a> {
    /// Parse a MATCH leaf exactly once. Composition owns the enclosing set
    /// delimiters and pagination; this parser owns names, expressions and the
    /// original shared parameter table. No statement substring is rewritten.
    pub(in crate::graph_text) fn parse_return_for_composition(
        mut self,
        statement: &'a str,
    ) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        self.parse_match_prefix()?;
        let head = self.graph_projection_head()?;
        let pipeline = if head.with {
            self.row_pipeline(head.schema())?
        } else {
            Vec::new()
        };
        self.end()?;
        self.finish_graph_projection(statement, head, pipeline)
    }

    /// Shared graph-to-row boundary. Exact grouped RETURN uses this same first
    /// WITH projection and row-stage parser, not a synthetic RETURN statement.
    fn graph_projection_head(&mut self) -> Result<GraphProjectionHead<'a>, GraphSetTextError> {
        let with = self.take_word("WITH")?;
        if !with {
            self.word("RETURN")?;
        }
        self.syntax.distinct = self.take_word("DISTINCT")?;
        if !self.syntax.distinct {
            self.take_word("ALL")?;
        }
        let mut inputs = Vec::<Projection<'a>>::new();
        let mut outputs = Vec::<(Name<'a>, Operand)>::new();
        if self.take(b'*')? {
            for &variable in &self.syntax.variables {
                let at = self.mutation_projection(&mut inputs, variable, None)?;
                outputs.push((variable, Operand::Column(at)));
            }
        } else {
            loop {
                self.capacity(
                    outputs.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let at = self.current.at;
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
                    Operand::Column(self.mutation_projection(&mut inputs, variable, None)?)
                } else {
                    self.mutation_expression(&mut inputs)
                        .map_err(expression_error)?
                };
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if let Operand::Column(index) = &operand {
                    inputs[*index].property.unwrap_or(inputs[*index].variable)
                } else {
                    return Err(GraphSetTextError {
                        offset: at,
                        kind: GraphSetTextErrorKind::Expected(
                            "AS alias for a computed RETURN value",
                        ),
                    });
                };
                if outputs.iter().any(|(name, _)| name.text == alias.text) {
                    return Err(error(
                        alias.at,
                        GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                    )
                    .into());
                }
                outputs.push((alias, operand));
                if !self.take(b',')? {
                    break;
                }
            }
        }
        Ok(GraphProjectionHead {
            with,
            inputs,
            outputs,
        })
    }

    fn finish_graph_projection(
        mut self,
        statement: &'a str,
        head: GraphProjectionHead<'a>,
        pipeline: Vec<ReadStageTemplate>,
    ) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        let GraphProjectionHead {
            with,
            mut inputs,
            outputs,
        } = head;
        if !with
            && outputs
                .iter()
                .all(|(_, operand)| matches!(operand, Operand::Column(_)))
        {
            // Keep the existing plan/counters/transcript for plain projections,
            // including repeated fields under different public aliases.
            self.syntax.columns = outputs
                .into_iter()
                .map(|(alias, operand)| {
                    let Operand::Column(index) = operand else {
                        unreachable!("plain projection checked above")
                    };
                    let source = inputs[index];
                    Column {
                        variable: source.variable,
                        property: source.property,
                        path: source.path,
                        alias,
                    }
                })
                .collect();
            return Ok(UnresolvedGraphText {
                statement,
                syntax: self.syntax,
                projection: None,
                pipeline,
            });
        }
        if inputs.is_empty() {
            let variable = self.syntax.variables[0];
            let _ = self.mutation_projection(&mut inputs, variable, None)?;
        }
        self.syntax.columns = inputs
            .into_iter()
            .map(|source| Column {
                variable: source.variable,
                property: source.property,
                path: source.path,
                alias: source.variable,
            })
            .collect();
        let mut projection = Vec::new();
        for (alias, operand) in outputs {
            let value = self.read_value_template(operand, alias.at)?;
            projection.push(ReadProjectionTemplate {
                name: alias.text.to_owned(),
                value,
            });
        }
        Ok(UnresolvedGraphText {
            statement,
            syntax: self.syntax,
            projection: Some(projection),
            pipeline,
        })
    }

    fn read_value_template(
        &self,
        operand: Operand,
        at: usize,
    ) -> Result<ReadValueTemplate, GraphPatternTextError> {
        Ok(match operand {
            Operand::Column(input) => ReadValueTemplate::Column(input),
            Operand::Literal(value) => ReadValueTemplate::Literal(value),
            Operand::Number(Number::Literal(value)) => {
                ReadValueTemplate::Literal(scalar(value, at)?)
            }
            Operand::Number(Number::Parameter(index)) => ReadValueTemplate::Parameter {
                index,
                at: self.syntax.parameter_offsets[index],
            },
            Operand::Integer { program, at } => ReadValueTemplate::Integer { program, at },
        })
    }
}

impl BoundSetTextInput {
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.selection.parameter_schema()
    }

    pub(crate) fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        self.bind_values(&values)
    }

    /// Only callers that checked the COMPLETE native argument table may use
    /// this path. Shared grouped terminals need that same table for HAVING/page.
    pub(crate) fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let mut input: PreparedGraphSet = self.selection.bind_values(values)?.into();
        if let Some(projection) = &self.projection {
            input = bind_projection(
                input,
                projection,
                self.quantifier,
                values,
                self.selection.return_at,
            )?;
        }
        for stage in &self.pipeline {
            input = match stage {
                ReadStageTemplate::Project {
                    at,
                    projection,
                    quantifier,
                } => bind_projection(input, projection, *quantifier, values, *at)?,
                ReadStageTemplate::Filter { at, code } => {
                    let code = pipeline::bind_filter(code, Some(values))?;
                    input.filter(&code).map_err(|kind| GraphSetTextError {
                        offset: *at,
                        kind: GraphSetTextErrorKind::FilterBuild(kind),
                    })?
                }
                ReadStageTemplate::Page {
                    at,
                    order,
                    offset,
                    count,
                } => {
                    if !order.is_empty() {
                        input = input
                            .with_order_by(order)
                            .map_err(|kind| GraphSetTextError {
                                offset: *at,
                                kind: GraphSetTextErrorKind::OrderBuild(kind),
                            })?;
                    }
                    input.with_page(
                        pipeline::page_value(offset, values),
                        count
                            .as_ref()
                            .map(|count| pipeline::page_value(count, values)),
                    )
                }
            };
        }
        Ok(input)
    }
}

fn bind_projection(
    input: PreparedGraphSet,
    projection: &[ReadProjectionTemplate],
    quantifier: crate::GraphSetQuantifier,
    values: &[GqlParameterValue],
    at: usize,
) -> Result<PreparedGraphSet, GraphSetTextError> {
    let mut columns = Vec::new();
    for output in projection {
        let value = match &output.value {
            ReadValueTemplate::Column(input) => GraphSetValue::Column(*input),
            ReadValueTemplate::Literal(value) => GraphSetValue::Literal(value.clone()),
            ReadValueTemplate::Parameter { index, at } => {
                GraphSetValue::Literal(scalar(values[*index].clone(), *at)?)
            }
            ReadValueTemplate::Integer { program, at } => GraphSetValue::Integer(
                integer::bind_integer(program, values, *at).map_err(expression_error)?,
            ),
        };
        columns.push(GraphSetProjection::new(output.name.clone(), value));
    }
    input
        .project(columns, quantifier)
        .map_err(|kind| GraphSetTextError {
            offset: at,
            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
        })
}
