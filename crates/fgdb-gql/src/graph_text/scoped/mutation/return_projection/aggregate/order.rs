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
        let mut columns = Vec::new();
        let mut failure = None;
        let expression = parser.read_resolved_item(&mut |parser| {
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
                        return Err(error(at, GraphPatternTextErrorKind::Expected("aggregate ORDER BY argument")));
                    }
                };
                let index = if let Some(index) = summaries.iter().position(|summary| summary.function == function && summary.column == column) {
                    index
                } else {
                    parser.capacity(keys.len() + summaries.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
                    let index = summaries.len();
                    let mut suffix = index;
                    let name = loop {
                        let name = format!("__order_summary_{suffix}");
                        suffix += 1;
                        if !summaries.iter().any(|summary| summary.name == name)
                            && !output.iter().any(|(alias, _)| alias.text == name)
                            && !schema.iter().any(|(alias, _)| alias.text == name)
                        { break name; }
                    };
                    summaries.push(PipelineSummary { name, function, column });
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
                } else if let Some(column) = schema[..input_visible].iter().position(|(alias, _)| alias.text == word)
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
        }, 0);
        if let Some(failure) = failure {
            return Err(failure);
        }
        let expression = expression?;
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
