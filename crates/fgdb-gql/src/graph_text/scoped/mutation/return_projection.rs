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
    outputs: Vec<(Name<'a>, ReadValueTemplate)>,
}
impl<'a> GraphProjectionHead<'a> {
    fn schema(&self, parameters: &[GqlParameterSpec]) -> pipeline::RowSchema<'a> {
        self.outputs
            .iter()
            .map(|(name, operand)| {
                let types: Vec<_> = self
                    .inputs
                    .iter()
                    .map(|input| {
                        if input.property.is_none() {
                            GraphSetColumnType::Vertex
                        } else {
                            GraphSetColumnType::Scalar
                        }
                    })
                    .collect();
                let kind = operand.column_type(&types, parameters);
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
        if self.is_word("UNWIND") || self.is_word("RETURN") || self.is_word("WITH") {
            return self.parse_leading_pipeline(statement);
        }
        self.parse_match_prefix()?;
        let head = self.graph_projection_head()?;
        let pipeline = if head.with {
            self.row_pipeline(head.schema(&self.syntax.parameters))?
        } else {
            Vec::new()
        };
        self.end()?;
        self.finish_graph_projection(statement, head, pipeline)
    }

    fn parse_leading_pipeline(
        mut self,
        statement: &'a str,
    ) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        let mut schema = Vec::new();
        let mut leading = Vec::new();
        while self.is_word("UNWIND") {
            let at = self.current.at;
            self.advance()?;
            leading.push(self.unwind_stage(&mut schema, at)?);
        }
        if self.is_word("MATCH") {
            self.read_row_bindings = schema.iter().map(|(name, _)| *name).collect();
            self.parse_match_prefix()?;
            let correlations = core::mem::take(&mut self.read_correlations);
            let width = schema.len();
            let mut inputs = Vec::new();
            for &variable in &self.syntax.variables {
                let index = self.mutation_projection(&mut inputs, variable, None)?;
                schema.push((variable, GraphSetColumnType::Vertex));
                debug_assert_eq!(index + width, schema.len() - 1);
            }
            let mut bound_correlations = Vec::new();
            for (variable, key, row) in correlations {
                let index = self.mutation_projection(&mut inputs, variable, Some(key))?;
                bound_correlations.push((row, index));
            }
            let pipeline = self.row_pipeline(schema)?;
            self.end()?;
            self.syntax.columns = inputs
                .into_iter()
                .map(|source| Column {
                    variable: source.variable,
                    property: source.property,
                    path: source.path,
                    alias: source.variable,
                })
                .collect();
            let projection = Some(
                (0..width + self.syntax.variables.len())
                    .map(|index| ReadProjectionTemplate {
                        name: if index < width {
                            self.read_row_bindings[index].text.to_owned()
                        } else {
                            self.syntax.variables[index - width].text.to_owned()
                        },
                        value: ReadValueTemplate::Column(index),
                    })
                    .collect(),
            );
            return Ok(UnresolvedGraphText {
                statement,
                syntax: self.syntax,
                projection,
                pipeline,
                singleton: false,
                leading,
                leading_types: vec![GraphSetColumnType::Any; width],
                correlations: bound_correlations,
            });
        }
        let pipeline = self.row_pipeline(schema)?;
        self.end()?;
        Ok(UnresolvedGraphText {
            statement,
            syntax: self.syntax,
            projection: None,
            pipeline: leading.into_iter().chain(pipeline).collect(),
            singleton: true,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
        })
    }
    /// Shared graph-to-row boundary. Exact grouped RETURN uses this same first
    /// WITH projection and row-stage parser, not a synthetic RETURN statement.
    fn graph_projection_head(&mut self) -> Result<GraphProjectionHead<'a>, GraphSetTextError> {
        if self.is_word("UNWIND") {
            let inputs: Vec<_> = self
                .syntax
                .variables
                .iter()
                .map(|&variable| Projection {
                    variable,
                    property: None,
                    path: None,
                })
                .collect();
            let outputs = self
                .syntax
                .variables
                .iter()
                .enumerate()
                .map(|(index, &name)| (name, ReadValueTemplate::Column(index)))
                .collect();
            return Ok(GraphProjectionHead {
                with: true,
                inputs,
                outputs,
            });
        }
        let with = self.take_word("WITH")?;
        if !with {
            self.word("RETURN")?;
        }
        self.syntax.distinct = self.take_word("DISTINCT")?;
        if !self.syntax.distinct {
            self.take_word("ALL")?;
        }
        let mut inputs = Vec::<Projection<'a>>::new();
        let mut outputs = Vec::<(Name<'a>, ReadValueTemplate)>::new();
        if self.take(b'*')? {
            for &variable in &self.syntax.variables {
                let at = self.mutation_projection(&mut inputs, variable, None)?;
                outputs.push((variable, ReadValueTemplate::Column(at)));
            }
        } else {
            loop {
                self.capacity(
                    outputs.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let at = self.current.at;
                let operand = self.read_graph_value(&mut inputs, 0)?;
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if let ReadValueTemplate::Column(index) = &operand {
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
                .all(|(_, operand)| matches!(operand, ReadValueTemplate::Column(_)))
        {
            // Keep the existing plan/counters/transcript for plain projections,
            // including repeated fields under different public aliases.
            self.syntax.columns = outputs
                .into_iter()
                .map(|(alias, operand)| {
                    let ReadValueTemplate::Column(index) = operand else {
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
                singleton: false,
                leading: Vec::new(),
                leading_types: Vec::new(),
                correlations: Vec::new(),
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
        for (alias, value) in outputs {
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
            singleton: false,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
        })
    }

    pub(in crate::graph_text) fn read_value_template(
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

    pub(in crate::graph_text) fn read_graph_value(
        &mut self,
        inputs: &mut Vec<Projection<'a>>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(Some(inputs), &[], &mut None, depth)
    }

    pub(in crate::graph_text) fn read_row_value(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(None, schema, &mut None, depth)
    }

    /// Resolve grouped keys and aggregate calls as column leaves while using
    /// the same scalar precedence, parameters and list grammar as ordinary rows.
    pub(in crate::graph_text) fn read_resolved_value(
        &mut self,
        resolve: &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        self.read_recursive_value(None, &[], &mut Some(resolve), depth)
    }

    fn read_recursive_value(
        &mut self,
        mut inputs: Option<&mut Vec<Projection<'a>>>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        resolve: &mut Option<
            &mut dyn FnMut(&mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>,
        >,
        depth: usize,
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        let at = self.current.at;
        if depth > 64 {
            return Err(GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::IntegerNesting { limit: 64 },
            });
        }
        let mut value = if self.take(b'[')? {
            let mut items = Vec::new();
            if !self.take(b']')? {
                loop {
                    self.capacity(
                        items.len(),
                        crate::MAX_GRAPH_INTEGER_INSTRUCTIONS,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    items.push(self.read_recursive_value(
                        inputs.as_deref_mut(),
                        schema,
                        resolve,
                        depth + 1,
                    )?);
                    if self.take(b']')? {
                        break;
                    }
                    self.punct(b',', ", or ]")?;
                }
            }
            ReadValueTemplate::List(items)
        } else if self.is_word("SIZE")
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            self.advance()?;
            self.punct(b'(', "(")?;
            let inner =
                self.read_recursive_value(inputs.as_deref_mut(), schema, resolve, depth + 1)?;
            self.punct(b')', ")")?;
            ReadValueTemplate::Size(Box::new(inner))
        } else if let Some(resolve) = resolve.as_deref_mut() {
            let operand = self
                .resolved_expression(resolve)
                .map_err(expression_error)?;
            self.read_value_template(operand, at)?
        } else if let Some(columns) = inputs.as_deref_mut() {
            let bare = matches!(self.current.kind, TokenKind::Word(word) if self.syntax.variables.iter().any(|name| name.text == word))
                && !matches!(
                    self.lexer.clone().next()?.kind,
                    TokenKind::Punct(b'.' | b'(')
                );
            if bare {
                let variable = self.variable()?;
                ReadValueTemplate::Column(self.mutation_projection(columns, variable, None)?)
            } else {
                let operand = self
                    .mutation_expression(columns)
                    .map_err(expression_error)?;
                self.read_value_template(operand, at)?
            }
        } else {
            let operand = self.row_expression(schema).map_err(expression_error)?;
            self.read_value_template(operand, at)?
        };
        while self.take(b'[')? {
            let index =
                self.read_recursive_value(inputs.as_deref_mut(), schema, resolve, depth + 1)?;
            self.punct(b']', "]")?;
            value = ReadValueTemplate::Index {
                list: Box::new(value),
                index: Box::new(index),
            };
        }
        Ok(value)
    }
}

impl BoundSetTextInput {
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }
    pub(crate) fn checked_arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<Vec<GqlParameterValue>, GraphPatternTextError> {
        if let Some(selection) = &self.selection {
            return selection.checked_arguments(arguments);
        }
        let mut values = Vec::new();
        for (index, spec) in self.parameters.iter().enumerate() {
            let at = self.parameter_offsets[index];
            let value = arguments
                .get(&spec.name)
                .ok_or_else(|| error(at, GraphPatternTextErrorKind::MissingParameter))?;
            if !spec.parameter_type.accepts(value.parameter_type()) {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::ParameterTypeMismatch {
                        expected: spec.parameter_type,
                        found: value.parameter_type(),
                    },
                ));
            }
            values.push(value);
        }
        if arguments.len() != values.len() {
            return Err(error(
                self.return_at,
                GraphPatternTextErrorKind::UnexpectedArguments,
            ));
        }
        Ok(values)
    }

    pub(crate) fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let values = self.checked_arguments(arguments)?;
        self.bind_values(&values)
    }

    /// Only callers that checked the COMPLETE native argument table may use
    /// this path. Shared grouped terminals need that same table for HAVING/page.
    pub(crate) fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let mut input = if self.leading.is_empty() && !self.singleton {
            self.selection
                .as_ref()
                .expect("graph source")
                .bind_values(values)?
                .into()
        } else {
            let mut leading = bind_stages(PreparedGraphSet::singleton(), &self.leading, values)?;
            if let Some(selection) = &self.selection {
                let width = leading.column_types().len();
                leading = leading
                    .cross_join(selection.bind_values(values)?.into())
                    .map_err(|kind| GraphSetTextError {
                        offset: self.return_at,
                        kind: GraphSetTextErrorKind::SetBuild(kind),
                    })?;
                let mut code = Vec::new();
                for &(left, right) in &self.correlations {
                    code.push(crate::GraphSetPredicateOp::Compare {
                        left: crate::GraphSetOperand::Column(left),
                        comparison: IntegerComparison::Equal,
                        right: crate::GraphSetOperand::Column(width + right),
                    });
                    if code.len() > 1 {
                        code.push(crate::GraphSetPredicateOp::And);
                    }
                }
                if !code.is_empty() {
                    leading = leading.filter(&code).map_err(|kind| GraphSetTextError {
                        offset: self.return_at,
                        kind: GraphSetTextErrorKind::FilterBuild(kind),
                    })?;
                }
            }
            leading
        };
        if let Some(projection) = &self.projection {
            input = bind_projection(input, projection, self.quantifier, values, self.return_at)?;
        }
        bind_stages(input, &self.pipeline, values)
    }
}

fn bind_stages(
    mut input: PreparedGraphSet,
    stages: &[ReadStageTemplate],
    values: &[GqlParameterValue],
) -> Result<PreparedGraphSet, GraphSetTextError> {
    for stage in stages {
        input = match stage {
            ReadStageTemplate::Unwind { at, name, value } => input
                .unwind(name.clone(), bind_read_value(value, values)?)
                .map_err(|kind| GraphSetTextError {
                    offset: *at,
                    kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                })?,
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

fn bind_projection(
    input: PreparedGraphSet,
    projection: &[ReadProjectionTemplate],
    quantifier: crate::GraphSetQuantifier,
    values: &[GqlParameterValue],
    at: usize,
) -> Result<PreparedGraphSet, GraphSetTextError> {
    let mut columns = Vec::new();
    for output in projection {
        let value = bind_read_value(&output.value, values)?;
        columns.push(GraphSetProjection::new(output.name.clone(), value));
    }
    input
        .project(columns, quantifier)
        .map_err(|kind| GraphSetTextError {
            offset: at,
            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
        })
}

pub(in crate::graph_text) fn bind_read_value(
    value: &ReadValueTemplate,
    values: &[GqlParameterValue],
) -> Result<GraphSetValue, GraphSetTextError> {
    Ok(match value {
        ReadValueTemplate::Column(input) => GraphSetValue::Column(*input),
        ReadValueTemplate::Literal(value) => GraphSetValue::Literal(value.clone()),
        ReadValueTemplate::Parameter { index, at } => match &values[*index] {
            GqlParameterValue::List(value) => GraphSetValue::Value(value.value().clone()),
            value => GraphSetValue::Literal(scalar(value.clone(), *at)?),
        },
        ReadValueTemplate::Integer { program, at } => GraphSetValue::Integer(
            integer::bind_integer(program, values, *at).map_err(expression_error)?,
        ),
        ReadValueTemplate::List(items) => GraphSetValue::List(
            items
                .iter()
                .map(|value| bind_read_value(value, values))
                .collect::<Result<_, _>>()?,
        ),
        ReadValueTemplate::Index { list, index } => GraphSetValue::Index {
            list: Box::new(bind_read_value(list, values)?),
            index: Box::new(bind_read_value(index, values)?),
        },
        ReadValueTemplate::Size(value) => {
            GraphSetValue::Size(Box::new(bind_read_value(value, values)?))
        }
    })
}
