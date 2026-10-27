//! Ordering the terminal aggregate of a row pipeline. Computed keys share the
//! native post-group expression owner, including exact counts and averages.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Column {
    Evaluation(GraphAggregateColumn),
    Projected(usize),
}

pub(super) struct Order {
    pub(super) evaluation: Vec<GraphAggregateOrder>,
    pub(super) projected: Vec<GraphValueOrder>,
    pub(super) computed: Vec<ReadValueTemplate>,
}

/// Resolve a result expression once against the pipeline's real input and
/// grouping registries. HAVING and ORDER BY share hidden aggregate calls and
/// aliases without exposing private summary cells as result columns.
#[allow(clippy::too_many_arguments)]
fn expression<'a>(
    parser: &mut Parser<'a>,
    schema: &[(Name<'a>, GraphSetColumnType)],
    input_visible: usize,
    output: &[(Name<'a>, GraphSetColumnType)],
    keys: &[usize],
    inputs: &mut inputs::Inputs,
    summaries: &mut Vec<PipelineSummary>,
    whole: bool,
) -> Result<(ReadValueTemplate, Vec<Column>), Error> {
    let mut columns = Vec::new();
    let mut failure = None;
    let mut resolve = |parser: &mut Parser<'a>| {
        if matches!(parser.current.kind, TokenKind::Word(word) if parser.elements.contains(&word))
            && !matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            return Ok(None);
        }
        let at = parser.current.at;
        let boundary = parser.boundary_reads.replace(BoundaryReads {
            visible: input_visible,
            width: schema.len(),
            reads: Vec::new(),
            functions: Vec::new(),
            grouped: Vec::new(),
            scoped: Vec::new(),
        });
        let summary = if core::mem::take(&mut parser.percentile_leaf) {
            let outer = core::mem::replace(&mut parser.aggregate_argument, true);
            let column = inputs.read(parser, schema);
            parser.aggregate_argument = outer;
            Some(column.map(|column| (GraphAggregateFunction::Collect, Some(column))))
        } else {
            match parser.starts_pipeline_summary() {
                Ok(true) => Some(parser.pipeline_summary(schema, inputs)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            }
        };
        parser.boundary_reads = boundary;
        let column = if let Some(summary) = summary {
            let (function, column) = match summary {
                Ok(summary) => summary,
                Err(failed) => {
                    failure = Some(failed);
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected("aggregate result argument"),
                    ));
                }
            };
            let index = if let Some(index) = summaries
                .iter()
                .position(|summary| summary.function == function && summary.column == column)
            {
                index
            } else {
                parser.capacity(
                    keys.len() + summaries.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let index = summaries.len();
                let mut suffix = index;
                let name = loop {
                    let name = format!("__order_summary_{suffix}");
                    suffix += 1;
                    if !summaries.iter().any(|summary| summary.name == name)
                        && !output.iter().any(|(alias, _)| alias.text == name)
                        && !schema.iter().any(|(alias, _)| alias.text == name)
                    {
                        break name;
                    }
                };
                summaries.push(PipelineSummary {
                    name,
                    function,
                    column,
                });
                index
            };
            Column::Evaluation(GraphAggregateColumn::Aggregate(index))
        } else if let Some(column) = parser.projected_source(output.len())? {
            Column::Projected(column)
        } else if let TokenKind::Word(word) = parser.current.kind
            && !matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            if let Some(column) = output.iter().position(|(alias, _)| alias.text == word) {
                parser.advance()?;
                Column::Projected(column)
            } else if let Some(column) = schema[..input_visible]
                .iter()
                .position(|(alias, _)| alias.text == word)
                .and_then(|input| keys.iter().position(|column| *column == input))
            {
                parser.advance()?;
                Column::Evaluation(GraphAggregateColumn::GroupKey(column))
            } else {
                return Ok(None);
            }
        } else {
            return Ok(None);
        };
        let index = columns.len();
        columns.push(column);
        Ok(Some(index))
    };
    // The HAVING owner reads comparisons and Boolean precedence itself.
    // ORDER BY instead owns a complete value, which may itself be Boolean.
    let expression = if whole {
        parser.read_resolved_item(&mut resolve, 0)
    } else {
        parser.read_resolved_value(&mut resolve, 0)
    };
    if let Some(failure) = failure {
        return Err(failure);
    }
    Ok((expression?, columns))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn parse<'a>(
    parser: &mut Parser<'a>,
    schema: &[(Name<'a>, GraphSetColumnType)],
    input_visible: usize,
    output: &[(Name<'a>, GraphSetColumnType)],
    visible_columns: &[GraphAggregateColumn],
    computed_outputs: &[usize],
    keys: &[usize],
    inputs: &mut inputs::Inputs,
    summaries: &mut Vec<PipelineSummary>,
) -> Result<Order, Error> {
    let mut result = Order {
        evaluation: Vec::new(),
        projected: Vec::new(),
        computed: Vec::new(),
    };
    if !parser.take_word("ORDER")? {
        return Ok(result);
    }
    parser.word("BY")?;
    let original_summaries = summaries.len();
    let mut expressions = Vec::new();
    loop {
        parser.capacity(
            expressions.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let (expression, columns) = expression(
            parser,
            schema,
            input_visible,
            output,
            keys,
            inputs,
            summaries,
            true,
        )?;
        let descending = parser.take_word("DESC")? || parser.take_word("DESCENDING")?;
        if !descending && !parser.take_word("ASC")? {
            parser.take_word("ASCENDING")?;
        }
        let nulls_first = if parser.take_word("NULLS")? {
            if parser.take_word("FIRST")? {
                true
            } else {
                parser.word("LAST")?;
                false
            }
        } else {
            false
        };
        expressions.push((expression, columns, descending, nulls_first));
        if !parser.take(b',')? {
            break;
        }
    }
    let visible = |column| match column {
        Column::Projected(index) => Some(index),
        Column::Evaluation(column) => visible_columns
            .iter()
            .enumerate()
            .find(|(index, candidate)| !computed_outputs.contains(index) && **candidate == column)
            .map(|(index, _)| index),
    };
    let projected = expressions.iter().any(|(value, columns, _, _)| {
        matches!(value, ReadValueTemplate::Column(index)
            if matches!(columns[*index], Column::Projected(column) if computed_outputs.contains(&column)))
    });
    let private = summaries.len() != original_summaries
        || expressions
            .iter()
            .any(|(value, columns, _, _)| match value {
                ReadValueTemplate::Column(index) => projected && visible(columns[*index]).is_none(),
                _ => true,
            });
    let width = keys.len() + summaries.len();
    for (mut expression, columns, descending, nulls_first) in expressions {
        if private {
            let columns = columns
                .into_iter()
                .map(|column| match column {
                    Column::Projected(index) => width + index,
                    Column::Evaluation(GraphAggregateColumn::GroupKey(index)) => index,
                    Column::Evaluation(GraphAggregateColumn::Aggregate(index)) => {
                        keys.len() + index
                    }
                })
                .collect::<Vec<_>>();
            crate::graph_text::aggregate::remap_output_columns(&mut expression, &columns);
            result.projected.push(GraphValueOrder {
                column: result.computed.len(),
                descending,
                nulls_first,
            });
            result.computed.push(expression);
        } else {
            let ReadValueTemplate::Column(index) = expression else {
                unreachable!("computed rank selected above");
            };
            let column = columns[index];
            if projected {
                let column = visible(column).expect("all projected keys are visible");
                if !result.projected.iter().any(|key| key.column == column) {
                    result.projected.push(GraphValueOrder {
                        column,
                        descending,
                        nulls_first,
                    });
                }
            } else {
                let column = match column {
                    Column::Evaluation(column) => column,
                    Column::Projected(index) => visible_columns[index],
                };
                if !result.evaluation.iter().any(|key| key.column == column) {
                    result.evaluation.push(GraphAggregateOrder {
                        column,
                        descending,
                        nulls: if nulls_first {
                            GraphNullPlacement::First
                        } else {
                            GraphNullPlacement::Last
                        },
                    });
                }
            }
        }
    }
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn having<'a>(
    parser: &mut Parser<'a>,
    schema: &[(Name<'a>, GraphSetColumnType)],
    input_visible: usize,
    output: &[(Name<'a>, GraphSetColumnType)],
    visible_columns: &[GraphAggregateColumn],
    computed_outputs: &[usize],
    projection: Option<&[ReadProjectionTemplate]>,
    keys: &[usize],
    inputs: &mut inputs::Inputs,
    summaries: &mut Vec<PipelineSummary>,
) -> Result<Option<crate::graph_text::HavingTemplate>, Error> {
    let mut resolver = HavingOperands {
        schema,
        input_visible,
        output,
        visible_columns,
        computed_outputs,
        projection,
        keys,
        inputs,
        summaries,
        failure: None,
    };
    let parsed = crate::graph_text::aggregate::having::parse_resolved(parser, &mut resolver);
    if let Some(failure) = resolver.failure {
        return Err(failure);
    }
    Ok(parsed?)
}

struct HavingOperands<'p, 'a> {
    schema: &'p [(Name<'a>, GraphSetColumnType)],
    input_visible: usize,
    output: &'p [(Name<'a>, GraphSetColumnType)],
    visible_columns: &'p [GraphAggregateColumn],
    computed_outputs: &'p [usize],
    projection: Option<&'p [ReadProjectionTemplate]>,
    keys: &'p [usize],
    inputs: &'p mut inputs::Inputs,
    summaries: &'p mut Vec<PipelineSummary>,
    failure: Option<Error>,
}

impl<'a> crate::graph_text::aggregate::having::OperandResolver<'a> for HavingOperands<'_, 'a> {
    fn named(&self, name: &str) -> bool {
        self.output.iter().any(|(alias, _)| alias.text == name)
            || self.keys.iter().any(|&key| {
                self.schema[..self.input_visible]
                    .get(key)
                    .is_some_and(|(alias, _)| alias.text == name)
            })
    }

    fn operand(
        &mut self,
        parser: &mut Parser<'a>,
    ) -> Result<crate::graph_text::aggregate::having::Operand, GraphPatternTextError> {
        use crate::graph_text::aggregate::ResultOrderColumn as Resolved;

        let (value, columns) = match expression(
            parser,
            self.schema,
            self.input_visible,
            self.output,
            self.keys,
            self.inputs,
            self.summaries,
            false,
        ) {
            Ok(expression) => expression,
            Err(failure) => {
                let at = failure.offset;
                self.failure = Some(failure);
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("aggregate HAVING operand"),
                ));
            }
        };
        if let ReadValueTemplate::Column(leaf) = &value
            && let Column::Projected(index) = columns[*leaf]
            && !self.computed_outputs.contains(&index)
        {
            return Ok(crate::graph_text::aggregate::having::Operand::RowColumn {
                column: self.visible_columns[index],
                index,
            });
        }
        let columns = columns
            .into_iter()
            .map(|column| match column {
                Column::Evaluation(column) => Resolved::Evaluation(column),
                Column::Projected(index) if self.computed_outputs.contains(&index) => {
                    Resolved::Projected(index)
                }
                Column::Projected(index) => Resolved::Evaluation(self.visible_columns[index]),
            })
            .collect();
        crate::graph_text::aggregate::having::resolve_operand(
            parser,
            value,
            columns,
            self.keys.len(),
            |index| {
                self.projection.expect("computed output has its projection")[index]
                    .value
                    .clone()
            },
        )
    }
}
