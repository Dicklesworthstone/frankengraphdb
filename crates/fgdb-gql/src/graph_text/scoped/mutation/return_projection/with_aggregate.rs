//! WITH grouping is a native relational stage, not a rewritten RETURN query.
//! Parsing finishes before catalog callbacks; binding lowers the retained
//! expressions to Project -> GroupAggregate -> Project under the same limits.

use super::*;
use crate::GraphAggregateFunction as Function;
use crate::set_text::aggregate::{ReadAggregateSpec, ReadAggregateStage};

mod graph;

fn function(word: &str) -> Option<Function> {
    if word.eq_ignore_ascii_case("COUNT") {
        Some(Function::Count)
    } else if word.eq_ignore_ascii_case("SUM") || word.eq_ignore_ascii_case("SUM_INT") {
        Some(Function::SumInt)
    } else if word.eq_ignore_ascii_case("AVG") || word.eq_ignore_ascii_case("AVG_INT") {
        Some(Function::AverageInt)
    } else if word.eq_ignore_ascii_case("MIN") {
        Some(Function::Min)
    } else if word.eq_ignore_ascii_case("MAX") {
        Some(Function::Max)
    } else if word.eq_ignore_ascii_case("COLLECT") {
        Some(Function::Collect)
    } else {
        None
    }
}

fn expected(at: usize, item: &'static str) -> GraphSetTextError {
    GraphSetTextError {
        offset: at,
        kind: GraphSetTextErrorKind::Expected(item),
    }
}

fn aggregate_error(at: usize, kind: crate::GraphAggregateBuildError) -> GraphSetTextError {
    GraphSetTextError {
        offset: at,
        kind: GraphSetTextErrorKind::AggregateBuild(kind),
    }
}

fn declarations(specs: &[ReadAggregateSpec]) -> Vec<crate::GraphAggregate<'_>> {
    use crate::GraphAggregate as A;
    specs
        .iter()
        .map(|spec| match (spec.function, spec.column) {
            (Function::CountRows, None) => A::count_rows(&spec.name),
            (Function::Count, Some(at)) => A::count(&spec.name, at),
            (Function::CountDistinct, Some(at)) => A::count_distinct(&spec.name, at),
            (Function::SumInt, Some(at)) => A::sum_int(&spec.name, at),
            (Function::SumIntDistinct, Some(at)) => A::sum_int_distinct(&spec.name, at),
            (Function::AverageInt, Some(at)) => A::average_int(&spec.name, at),
            (Function::AverageIntDistinct, Some(at)) => A::average_int_distinct(&spec.name, at),
            (Function::Min, Some(at)) => A::min(&spec.name, at),
            (Function::Max, Some(at)) => A::max(&spec.name, at),
            (Function::Collect, Some(at)) => A::collect(&spec.name, at),
            (Function::CollectDistinct, Some(at)) => A::collect_distinct(&spec.name, at),
            _ => unreachable!("the checked WITH grammar pairs functions and arguments"),
        })
        .collect()
}

impl ReadAggregateStage {
    pub(crate) fn bind(
        &self,
        input: PreparedGraphSet,
        values: &[GqlParameterValue],
        at: usize,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let input = bind_projection(
            input,
            &self.inputs,
            crate::GraphSetQuantifier::All,
            values,
            at,
        )?;
        let input = input
            .group_by(&self.keys, &declarations(&self.aggregates))
            .map_err(|kind| aggregate_error(at, kind))?;
        let outputs = self
            .outputs
            .iter()
            .map(|(name, column, _)| {
                GraphSetProjection::new(name.clone(), GraphSetValue::Column(*column))
            })
            .collect();
        input
            .project(outputs, self.quantifier)
            .map_err(|kind| GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::ProjectionBuild(kind),
            })
    }
}

#[derive(Clone, Copy)]
enum Output {
    Key(usize),
    Aggregate(usize),
}

/// A native-token lookahead only. The caller still parses every token normally.
/// Quoted strings, property names and aliases cannot choose an aggregate lane.
impl<'a> Parser<'a> {
    pub(super) fn with_has_aggregate(&self) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("WITH") {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        let mut previous = None;
        let mut depth = 0_usize;
        loop {
            let token = lexer.next()?;
            let named = matches!(previous, Some(TokenKind::Punct(b'.')))
                || matches!(previous, Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("AS"));
            match token.kind {
                TokenKind::End => return Ok(false),
                TokenKind::Word(word) if !named => {
                    let text_with = word.eq_ignore_ascii_case("WITH")
                        && matches!(previous, Some(TokenKind::Word(word))
                            if word.eq_ignore_ascii_case("STARTS") || word.eq_ignore_ascii_case("ENDS"));
                    if depth == 0
                        && !text_with
                        && [
                            "WHERE",
                            "ORDER",
                            "SKIP",
                            "LIMIT",
                            "RETURN",
                            "WITH",
                            "MATCH",
                            "OPTIONAL",
                            "UNWIND",
                            "UNION",
                            "EXCEPT",
                            "INTERSECT",
                            "CALL",
                        ]
                        .iter()
                        .any(|keyword| word.eq_ignore_ascii_case(keyword))
                    {
                        return Ok(false);
                    }
                    if function(word).is_some()
                        && matches!(lexer.clone().next()?.kind, TokenKind::Punct(b'('))
                    {
                        return Ok(true);
                    }
                }
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    let Some(outer) = depth.checked_sub(1) else {
                        return Ok(false);
                    };
                    depth = outer;
                }
                _ => {}
            }
            previous = Some(token.kind);
        }
    }

    fn with_input(
        &mut self,
        schema: &pipeline::RowSchema<'a>,
        inputs: &mut Vec<ReadProjectionTemplate>,
    ) -> Result<(usize, GraphSetColumnType, Option<Name<'a>>), GraphSetTextError> {
        let at = self.current.at;
        let value = self.read_row_value(schema, 0)?;
        let name = if let ReadValueTemplate::Column(column) = &value {
            Some(
                self.boundary_key(schema.len(), *column)
                    .unwrap_or(schema[*column].0),
            )
        } else {
            None
        };
        let types: Vec<_> = schema.iter().map(|(_, kind)| *kind).collect();
        let kind = value.column_type(&types, &self.syntax.parameters);
        // Admit the shared expression program before any catalog callback.
        // These typed placeholders prove structure only; they are never run.
        let values: Vec<_> = self
            .syntax
            .parameters
            .iter()
            .map(|spec| match spec.parameter_type {
                GqlParameterType::Int64 => GqlParameterValue::Int64(0),
                GqlParameterType::UInt64 => GqlParameterValue::UInt64(0),
                GqlParameterType::Scalar(_) => GqlParameterValue::Scalar(
                    GqlScalarParameter::new(CanonicalScalar::Null).expect("canonical null"),
                ),
                GqlParameterType::List => GqlParameterValue::List(
                    crate::GqlListParameter::new(Vec::new()).expect("bounded empty list"),
                ),
            })
            .collect();
        let bound = bind_read_value(&value, &values)?;
        GraphSetProjection::admit_output(&bound, &types, inputs.len()).map_err(|kind| {
            GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::ProjectionBuild(kind),
            }
        })?;
        // Equal input expressions share a slot, but parameter values never
        // participate in that decision. Repeated key aliases group only once.
        let mut bytes = Vec::new();
        value.append_template_transcript(&mut bytes);
        for (column, input) in inputs.iter().enumerate() {
            let mut prior = Vec::new();
            input.value.append_template_transcript(&mut prior);
            if prior == bytes {
                return Ok((column, kind, name));
            }
        }
        self.capacity(
            inputs.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let column = inputs.len();
        inputs.push(ReadProjectionTemplate {
            name: format!("__with_input_{column}"),
            value,
        });
        Ok((column, kind, name))
    }

    pub(super) fn with_aggregate_stage(
        &mut self,
        schema: &pipeline::RowSchema<'a>,
    ) -> Result<(ReadStageTemplate, pipeline::RowSchema<'a>), GraphSetTextError> {
        let at = self.current.at;
        self.word("WITH")?;
        let distinct = self.take_word("DISTINCT")?;
        if !distinct {
            self.take_word("ALL")?;
        }
        let mut inputs = Vec::new();
        let mut keys = Vec::new();
        let mut aggregates = Vec::new();
        let mut returned: Vec<(Name<'a>, Output, GraphSetColumnType)> = Vec::new();
        loop {
            self.capacity(
                returned.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let item_at = self.current.at;
            let summary = match self.current.kind {
                TokenKind::Word(word)
                    if matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) =>
                {
                    function(word)
                }
                _ => None,
            };
            let (output, kind, default_name) = if let Some(mut function) = summary {
                let spelling = self.name()?;
                self.punct(b'(', "(")?;
                let unique = self.take_word("DISTINCT")?;
                if !unique {
                    self.take_word("ALL")?;
                }
                let (column, input_kind) = if self.take(b'*')? {
                    if function != Function::Count || unique {
                        return Err(expected(item_at, "COUNT(*) without DISTINCT"));
                    }
                    function = Function::CountRows;
                    (None, GraphSetColumnType::Scalar)
                } else {
                    let (column, kind, _) = self.with_input(schema, &mut inputs)?;
                    (Some(column), kind)
                };
                self.punct(b')', "one expression as the aggregate argument")?;
                if matches!(function, Function::SumInt | Function::AverageInt)
                    && !matches!(
                        input_kind,
                        GraphSetColumnType::Scalar | GraphSetColumnType::Any
                    )
                {
                    return Err(expected(item_at, "scalar input for a numeric aggregate"));
                }
                if unique {
                    function = match function {
                        Function::Count => Function::CountDistinct,
                        Function::SumInt => Function::SumIntDistinct,
                        Function::AverageInt => Function::AverageIntDistinct,
                        Function::Collect => Function::CollectDistinct,
                        other => other,
                    };
                }
                let kind = match function {
                    Function::Min | Function::Max => input_kind,
                    Function::Collect | Function::CollectDistinct => GraphSetColumnType::List,
                    _ => GraphSetColumnType::Scalar,
                };
                let index = aggregates.len();
                aggregates.push(ReadAggregateSpec {
                    name: format!("__with_summary_{index}"),
                    function,
                    column,
                });
                (Output::Aggregate(index), kind, Some(spelling))
            } else {
                let (column, kind, name) = self.with_input(schema, &mut inputs)?;
                let key = match keys.iter().position(|&key| key == column) {
                    Some(key) => key,
                    None => {
                        keys.push(column);
                        keys.len() - 1
                    }
                };
                (Output::Key(key), kind, name)
            };
            let name = if self.take_word("AS")? {
                self.name()?
            } else {
                default_name.ok_or_else(|| expected(item_at, "AS alias for a computed WITH key"))?
            };
            if returned.iter().any(|(prior, _, _)| prior.text == name.text) {
                return Err(aggregate_error(
                    name.at,
                    crate::GraphAggregateBuildError::DuplicateName,
                ));
            }
            returned.push((name, output, kind));
            if !self.take(b',')? {
                break;
            }
        }
        if aggregates.is_empty() {
            return Err(aggregate_error(
                at,
                crate::GraphAggregateBuildError::EmptyAggregates,
            ));
        }
        if inputs.is_empty() {
            // COUNT(*) needs occurrences, not an invented graph source or a
            // guessed cardinality. A constant projection preserves those rows.
            inputs.push(ReadProjectionTemplate {
                name: "__with_input_0".into(),
                value: ReadValueTemplate::Literal(
                    GqlScalarParameter::new(CanonicalScalar::Null).expect("canonical null"),
                ),
            });
        }
        let output_schema = returned
            .iter()
            .map(|(name, _, kind)| (*name, *kind))
            .collect();
        let outputs = returned
            .into_iter()
            .map(|(name, output, kind)| {
                let column = match output {
                    Output::Key(at) => at,
                    Output::Aggregate(at) => keys.len() + at,
                };
                (name.text.to_owned(), column, kind)
            })
            .collect();
        self.boundary_reads = None;
        Ok((
            ReadStageTemplate::Aggregate {
                at,
                stage: ReadAggregateStage {
                    inputs,
                    keys,
                    aggregates,
                    outputs,
                    quantifier: if distinct {
                        crate::GraphSetQuantifier::Distinct
                    } else {
                        crate::GraphSetQuantifier::All
                    },
                },
            },
            output_schema,
        ))
    }
}

#[cfg(test)]
mod tests;
