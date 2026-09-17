//! Exact terminal summaries after the shared native WITH prefix.
//! Only this typed terminal differs from ordinary row RETURN. Matching,
//! expressions, alias scopes, filters, pages and parameter registration share
//! the existing parser; execution receives the ordinary aggregate plan.

use super::*;
use crate::pipeline_aggregate_text::{
    GraphPipelineAggregateTextError as Error, GraphPipelineAggregateTextErrorKind as Kind,
    PipelineSummary, PreparedGraphPipelineAggregateText,
};
use crate::set_text::{ReadFilterOp, ReadPageNumber};
use crate::{
    GraphAggregate, GraphAggregateBuildError, GraphAggregateColumn, GraphAggregateFunction,
    GraphAggregateOrder, GraphAggregateTextSlot, GraphHavingExpression, GraphHavingOp,
    GraphHavingOperand, GraphNullPlacement, GraphSetOperand, GraphSetPredicateOp,
    PreparedGraphAggregate,
};

fn expected(at: usize, item: &'static str) -> Error {
    GraphSetTextError {
        offset: at,
        kind: GraphSetTextErrorKind::Expected(item),
    }
    .into()
}
fn build(at: usize, kind: GraphAggregateBuildError) -> Error {
    Error {
        offset: at,
        kind: Kind::Build(kind),
    }
}

enum ReturnedValue {
    Key(usize),
    Summary(usize),
}
struct Returned<'a> {
    name: Name<'a>,
    value: ReturnedValue,
}

fn declaration(summary: &PipelineSummary) -> GraphAggregate<'_> {
    use GraphAggregateFunction as F;
    match (summary.function, summary.column) {
        (F::CountRows, None) => GraphAggregate::count_rows(&summary.name),
        (F::Count, Some(at)) => GraphAggregate::count(&summary.name, at),
        (F::CountDistinct, Some(at)) => GraphAggregate::count_distinct(&summary.name, at),
        (F::SumInt, Some(at)) => GraphAggregate::sum_int(&summary.name, at),
        (F::SumIntDistinct, Some(at)) => GraphAggregate::sum_int_distinct(&summary.name, at),
        (F::AverageInt, Some(at)) => GraphAggregate::average_int(&summary.name, at),
        (F::AverageIntDistinct, Some(at)) => {
            GraphAggregate::average_int_distinct(&summary.name, at)
        }
        (F::Min, Some(at)) => GraphAggregate::min(&summary.name, at),
        (F::Max, Some(at)) => GraphAggregate::max(&summary.name, at),
        (F::Collect, Some(at)) => GraphAggregate::collect(&summary.name, at),
        (F::CollectDistinct, Some(at)) => GraphAggregate::collect_distinct(&summary.name, at),
        _ => unreachable!("the closed native summary grammar pairs functions and arguments"),
    }
}

fn bind_having(
    code: &[ReadFilterOp],
    columns: &[GraphAggregateColumn],
    values: Option<&[GqlParameterValue]>,
    at: usize,
) -> Result<GraphHavingExpression, Error> {
    let operand = |operand: GraphSetOperand| match operand {
        GraphSetOperand::Column(column) => GraphHavingOperand::Column(columns[column]),
        GraphSetOperand::Literal(value) => match value.value() {
            CanonicalScalar::Int(value) => GraphHavingOperand::Integer(i128::from(*value)),
            _ => GraphHavingOperand::Scalar(value.predicate(IntegerComparison::Equal)),
        },
    };
    let code = pipeline::bind_filter(code, values)?
        .into_iter()
        .map(|op| match op {
            GraphSetPredicateOp::Compare {
                left,
                comparison,
                right,
            } => GraphHavingOp::Compare {
                left: operand(left),
                comparison,
                right: operand(right),
            },
            GraphSetPredicateOp::IsNull {
                operand: value,
                is_null,
            } => GraphHavingOp::IsNull {
                operand: operand(value),
                is_null,
            },
            GraphSetPredicateOp::Truth(value) => GraphHavingOp::Truth(value),
            GraphSetPredicateOp::And => GraphHavingOp::And,
            GraphSetPredicateOp::Or => GraphHavingOp::Or,
            GraphSetPredicateOp::Not => GraphHavingOp::Not,
        })
        .collect::<Vec<_>>();
    GraphHavingExpression::prepare(&code).map_err(|kind| Error {
        offset: at,
        kind: Kind::Having(kind),
    })
}

impl<'a> Parser<'a> {
    fn pipeline_column(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<usize, Error> {
        let name = self.name()?;
        schema
            .iter()
            .position(|(alias, _)| alias.text == name.text)
            .ok_or_else(|| expected(name.at, "an alias from the completed WITH stage"))
    }

    fn pipeline_summary(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<(GraphAggregateFunction, Option<usize>), Error> {
        use GraphAggregateFunction as F;
        let name = self.name()?;
        let function = if name.text.eq_ignore_ascii_case("COUNT") {
            F::Count
        } else if name.text.eq_ignore_ascii_case("SUM") || name.text.eq_ignore_ascii_case("SUM_INT")
        {
            F::SumInt
        } else if name.text.eq_ignore_ascii_case("AVG") || name.text.eq_ignore_ascii_case("AVG_INT")
        {
            F::AverageInt
        } else if name.text.eq_ignore_ascii_case("MIN") {
            F::Min
        } else if name.text.eq_ignore_ascii_case("MAX") {
            F::Max
        } else if name.text.eq_ignore_ascii_case("COLLECT") {
            F::Collect
        } else {
            return Err(expected(name.at, "COUNT, SUM, AVG, MIN, MAX or COLLECT"));
        };
        self.punct(b'(', "(")?;
        if self.take(b'*')? {
            if function != F::Count {
                return Err(expected(name.at, "COUNT(*) or a row-alias argument"));
            }
            self.punct(b')', ")")?;
            return Ok((F::CountRows, None));
        }
        let distinct = self.take_word("DISTINCT")?;
        if !distinct {
            self.take_word("ALL")?;
        }
        let at = self.current.at;
        let column = self.pipeline_column(schema)?;
        self.punct(b')', "one projected row alias as the aggregate argument")?;
        if matches!(function, F::SumInt | F::AverageInt)
            && !matches!(
                schema[column].1,
                GraphSetColumnType::Scalar | GraphSetColumnType::Any
            )
        {
            return Err(expected(at, "a scalar WITH alias for a numeric aggregate"));
        }
        let function = if distinct {
            match function {
                F::Count => F::CountDistinct,
                F::Collect => F::CollectDistinct,
                F::SumInt => F::SumIntDistinct,
                F::AverageInt => F::AverageIntDistinct,
                other => other,
            }
        } else {
            function
        };
        Ok((function, Some(column)))
    }
}

impl PreparedGraphPipelineAggregateText {
    pub fn prepare(
        statement: &str,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, Error> {
        Self::prepare_with_parameter_types(statement, &[], resolve)
    }

    /// Check all WITH stages, final grouping, output aliases and HAVING shape
    /// before entering the catalog. There is one original token/parameter table
    /// for graph predicates, row expressions, input pages and group clauses.
    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, Error> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        parser.parse_match_prefix()?;
        if !parser.is_word("WITH") && !parser.is_word("UNWIND") {
            return Err(expected(
                parser.current.at,
                "WITH or UNWIND before a pipeline aggregate RETURN",
            ));
        }
        let head = parser.graph_projection_head()?;
        let (mut stages, schema, depth) =
            parser.row_pipeline_prefix(head.schema(&parser.syntax.parameters))?;
        let aggregate_at = parser.current.at;
        if depth >= crate::MAX_GRAPH_SET_DEPTH {
            return Err(build(
                aggregate_at,
                GraphAggregateBuildError::RelationalInput(crate::GraphSetBuildError::TooDeep {
                    limit: crate::MAX_GRAPH_SET_DEPTH,
                    observed: depth + 1,
                }),
            ));
        }
        parser.word("RETURN")?;
        let output_distinct = parser.take_word("DISTINCT")?;
        if !output_distinct {
            parser.take_word("ALL")?;
        }
        let mut returned: Vec<Returned<'_>> = Vec::new();
        let mut summaries = Vec::new();
        loop {
            parser.capacity(
                returned.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let at = parser.current.at;
            let call = matches!(parser.current.kind, TokenKind::Word(_))
                && matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('));
            let (name, value) = if call {
                let (function, column) = parser.pipeline_summary(&schema)?;
                parser.word("AS")?;
                let name = parser.name()?;
                let index = summaries.len();
                summaries.push(PipelineSummary {
                    name: name.text.to_owned(),
                    function,
                    column,
                });
                (name, ReturnedValue::Summary(index))
            } else {
                let column = parser.pipeline_column(&schema)?;
                let name = if parser.take_word("AS")? {
                    parser.name()?
                } else {
                    schema[column].0
                };
                (name, ReturnedValue::Key(column))
            };
            if returned
                .iter()
                .any(|previous| previous.name.text == name.text)
            {
                return Err(build(at, GraphAggregateBuildError::DuplicateName));
            }
            returned.push(Returned { name, value });
            if !parser.take(b',')? {
                break;
            }
        }
        if summaries.is_empty() {
            return Err(build(
                aggregate_at,
                GraphAggregateBuildError::EmptyAggregates,
            ));
        }
        let mut keys = Vec::new();
        if parser.take_word("GROUP")? {
            parser.word("BY")?;
            loop {
                let at = parser.current.at;
                parser.capacity(
                    keys.len() + summaries.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let column = parser.pipeline_column(&schema)?;
                if keys.contains(&column) {
                    return Err(build(at, GraphAggregateBuildError::DuplicateKey { column }));
                }
                if summaries
                    .iter()
                    .any(|summary| summary.name == schema[column].0.text)
                {
                    return Err(build(at, GraphAggregateBuildError::DuplicateName));
                }
                keys.push(column);
                if !parser.take(b',')? {
                    break;
                }
            }
        }
        let mut names = Vec::new();
        let mut slots = Vec::new();
        let mut output_keys = Vec::new();
        let mut having_columns = Vec::new();
        let mut output_schema = Vec::new();
        for item in returned {
            let (slot, column, kind) = match item.value {
                ReturnedValue::Key(input) => {
                    let key = keys
                        .iter()
                        .position(|column| *column == input)
                        .ok_or_else(|| {
                            expected(item.name.at, "every nonaggregate RETURN alias in GROUP BY")
                        })?;
                    let slot = GraphAggregateTextSlot::GroupKey(output_keys.len());
                    output_keys.push(key);
                    (slot, GraphAggregateColumn::GroupKey(key), schema[input].1)
                }
                ReturnedValue::Summary(at) => {
                    let summary = &summaries[at];
                    let kind = if matches!(
                        summary.function,
                        GraphAggregateFunction::Min | GraphAggregateFunction::Max
                    ) {
                        schema[summary.column.expect("extrema have one argument")].1
                    } else if matches!(
                        summary.function,
                        GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct
                    ) {
                        GraphSetColumnType::List
                    } else {
                        GraphSetColumnType::Scalar
                    };
                    (
                        GraphAggregateTextSlot::Aggregate(at),
                        GraphAggregateColumn::Aggregate(at),
                        kind,
                    )
                }
            };
            names.push(item.name.text.to_owned());
            slots.push(slot);
            having_columns.push(column);
            output_schema.push((item.name, kind));
        }
        let having_at = parser.current.at;
        let mut having = Vec::new();
        if parser.take_word("HAVING")? {
            parser.row_disjunction(&output_schema, 0, &mut having)?;
            // Only static NULL placeholders are used for argument shape checks.
            // Actual values are rebound to the exact HAVING program below.
            let _ = bind_having(&having, &having_columns, None, having_at)?;
        }
        let mut ordering = Vec::new();
        let mut offset = ReadPageNumber::Literal(0);
        let mut count = None;
        if let Some(ReadStageTemplate::Page {
            order,
            offset: skip,
            count: limit,
            ..
        }) = parser.row_page(&output_schema)?
        {
            for key in order {
                let column = having_columns[key.column];
                if ordering
                    .iter()
                    .any(|prior: &GraphAggregateOrder| prior.column == column)
                {
                    return Err(build(
                        aggregate_at,
                        GraphAggregateBuildError::DuplicateOrder { column },
                    ));
                }
                ordering.push(GraphAggregateOrder {
                    column,
                    descending: key.descending,
                    nulls: if key.nulls_first {
                        GraphNullPlacement::First
                    } else {
                        GraphNullPlacement::Last
                    },
                });
            }
            offset = skip;
            count = limit;
        }
        parser.end()?;
        let input = parser
            .finish_graph_projection(statement, head, stages)?
            .resolve(resolve)?;
        Ok(Self {
            statement: statement.to_owned(),
            input,
            keys,
            output_keys,
            summaries,
            output_distinct,
            names,
            slots,
            having,
            having_columns,
            ordering,
            offset,
            count,
            aggregate_at,
            having_at,
        })
    }

    /// Lower every original parameter occurrence before any query executes.
    /// Returned group values keep their exact count/sum/average result domains.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphAggregate, Error> {
        let values = self.input.checked_arguments(arguments)?;
        let relation = self.input.bind_values(&values)?;
        let declarations = self.summaries.iter().map(declaration).collect::<Vec<_>>();
        let mut query = PreparedGraphAggregate::prepare_relation(
            relation,
            &self.keys,
            &declarations,
            pipeline::page_value(&self.offset, &values),
            self.count
                .as_ref()
                .map(|value| pipeline::page_value(value, &values)),
        )
        .map_err(|kind| build(self.aggregate_at, kind))?
        .with_key_output_columns(&self.output_keys)
        .map_err(|kind| build(self.aggregate_at, kind))?
        .with_distinct_output(self.output_distinct)
        .with_result_clauses(&[], &self.ordering)
        .map_err(|kind| build(self.aggregate_at, kind))?;
        if !self.having.is_empty() {
            let expression = bind_having(
                &self.having,
                &self.having_columns,
                Some(&values),
                self.having_at,
            )?;
            query = query
                .with_having_expression(&expression)
                .map_err(|kind| Error {
                    offset: self.having_at,
                    kind: Kind::Having(kind),
                })?;
        }
        Ok(query)
    }
}
