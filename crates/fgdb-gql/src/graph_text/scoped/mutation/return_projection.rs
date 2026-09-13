//! Read projection uses the mutation parser's existing scalar operands and
//! precedence compiler. Only terminal semantics differ: no action or write is
//! constructed. Hidden source columns retain every requested property read.

use super::*;
use crate::graph_text::parameters::UnresolvedGraphText;
use crate::set_text::{BoundSetTextInput, ReadProjectionTemplate, ReadValueTemplate};
use crate::{GraphSetProjection, GraphSetValue, GraphSetTextError, GraphSetTextErrorKind, PreparedGraphSet};

fn expression_error(source: GraphMutationTextError) -> GraphSetTextError {
    let kind = match source.kind {
        GraphMutationTextErrorKind::Query(kind) => GraphSetTextErrorKind::Pattern(kind),
        GraphMutationTextErrorKind::IntegerExpression(kind) => GraphSetTextErrorKind::IntegerExpression(kind),
        GraphMutationTextErrorKind::IntegerOperand => GraphSetTextErrorKind::IntegerOperand,
        GraphMutationTextErrorKind::IntegerNesting { limit } => GraphSetTextErrorKind::IntegerNesting { limit },
        // The scalar parser cannot construct mutation actions or their errors.
        GraphMutationTextErrorKind::Build(_) => GraphSetTextErrorKind::Expected("read scalar expression"),
    };
    GraphSetTextError { offset: source.offset, kind }
}

impl<'a> Parser<'a> {
    /// Parse a MATCH leaf exactly once. Composition owns the enclosing set
    /// delimiters and pagination; this parser owns names, expressions and the
    /// original shared parameter table. No statement substring is rewritten.
    pub(in crate::graph_text) fn parse_return_for_composition(mut self, statement: &'a str)
        -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
        self.parse_match_prefix()?;
        self.word("RETURN")?;
        self.syntax.distinct = self.take_word("DISTINCT")?;
        if !self.syntax.distinct { self.take_word("ALL")?; }
        let mut inputs = Vec::<Projection<'a>>::new();
        let mut outputs = Vec::<(Name<'a>, Operand)>::new();
        if self.take(b'*')? {
            for &variable in &self.syntax.variables {
                let at = self.mutation_projection(&mut inputs, variable, None)?;
                outputs.push((variable, Operand::Column(at)));
            }
        } else {
            loop {
                self.capacity(outputs.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
                let at = self.current.at;
                let bare_vertex = if let TokenKind::Word(word) = self.current.kind {
                    let next = self.lexer.clone().next()?;
                    !matches!(next.kind, TokenKind::Punct(b'.' | b'('))
                        && (self.syntax.variables.iter().any(|name| name.text == word)
                            || !(word.eq_ignore_ascii_case("TRUE") || word.eq_ignore_ascii_case("FALSE")
                                || word.eq_ignore_ascii_case("NULL")))
                } else { false };
                let operand = if bare_vertex {
                    let variable = self.variable()?;
                    Operand::Column(self.mutation_projection(&mut inputs, variable, None)?)
                } else {
                    self.mutation_expression(&mut inputs).map_err(expression_error)?
                };
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if let Operand::Column(index) = &operand {
                    inputs[*index].property.unwrap_or(inputs[*index].variable)
                } else {
                    return Err(GraphSetTextError { offset: at,
                        kind: GraphSetTextErrorKind::Expected("AS alias for a computed RETURN value") });
                };
                if outputs.iter().any(|(name, _)| name.text == alias.text) {
                    return Err(error(alias.at, GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection)).into());
                }
                outputs.push((alias, operand));
                if !self.take(b',')? { break; }
            }
        }
        self.end()?;
        if outputs.iter().all(|(_, operand)| matches!(operand, Operand::Column(_))) {
            // Keep the existing plan/counters/transcript for plain projections,
            // including repeated fields under different public aliases.
            self.syntax.columns = outputs.into_iter().map(|(alias, operand)| {
                let Operand::Column(index) = operand else { unreachable!("plain projection checked above") };
                let source = inputs[index];
                Column { variable: source.variable, property: source.property, alias }
            }).collect();
            return Ok(UnresolvedGraphText { statement, syntax: self.syntax, projection: None });
        }
        if inputs.is_empty() {
            // Constants still occur once for every graph match. A hidden root
            // identity carries the bag, including isolated vertices and all
            // parallel-edge/WALK occurrences; it never enters the public tuple.
            let variable = self.syntax.variables[0];
            let _ = self.mutation_projection(&mut inputs, variable, None)?;
        }
        self.syntax.columns = inputs.into_iter().map(|source| Column {
            variable: source.variable, property: source.property, alias: source.variable,
        }).collect();
        let mut projection = Vec::new();
        for (alias, operand) in outputs {
            let value = match operand {
                Operand::Column(input) => ReadValueTemplate::Column(input),
                Operand::Literal(value) => ReadValueTemplate::Literal(value),
                Operand::Number(Number::Literal(value)) => ReadValueTemplate::Literal(scalar(value, alias.at)?),
                Operand::Number(Number::Parameter(index)) => ReadValueTemplate::Parameter {
                    index, at: self.syntax.parameter_offsets[index],
                },
                Operand::Integer { program, at } => ReadValueTemplate::Integer { program, at },
            };
            projection.push(ReadProjectionTemplate { name: alias.text.to_owned(), value });
        }
        Ok(UnresolvedGraphText { statement, syntax: self.syntax, projection: Some(projection) })
    }
}

impl BoundSetTextInput {
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] { self.selection.parameter_schema() }

    pub(crate) fn bind_parameters(&self, arguments: &GqlParameters)
        -> Result<PreparedGraphSet, GraphSetTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        let input: PreparedGraphSet = self.selection.bind_values(&values)?.into();
        let Some(projection) = &self.projection else { return Ok(input); };
        let mut columns = Vec::new();
        for output in projection {
            let value = match &output.value {
                ReadValueTemplate::Column(input) => GraphSetValue::Column(*input),
                ReadValueTemplate::Literal(value) => GraphSetValue::Literal(value.clone()),
                ReadValueTemplate::Parameter { index, at } => GraphSetValue::Literal(scalar(values[*index].clone(), *at)?),
                ReadValueTemplate::Integer { program, at } => GraphSetValue::Integer(
                    integer::bind_integer(program, &values, *at).map_err(expression_error)?),
            };
            columns.push(GraphSetProjection::new(output.name.clone(), value));
        }
        input.project(columns, self.quantifier).map_err(|kind| GraphSetTextError {
            offset: self.selection.return_at, kind: GraphSetTextErrorKind::ProjectionBuild(kind),
        })
    }
}
