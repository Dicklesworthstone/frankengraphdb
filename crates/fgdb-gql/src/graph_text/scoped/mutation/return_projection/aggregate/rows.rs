//! Source-free aggregate pipelines use the real singleton relation, not a
//! fabricated vertex or a graph scan. The terminal compiler and exact numeric
//! states are shared with graph-backed pipelines; only source ownership differs.

use super::*;
use crate::PreparedGraphSetAggregate;

pub(super) fn prefix<'a>(
    parser: &mut Parser<'a>,
) -> Result<
    (
        Option<GraphProjectionHead<'a>>,
        Vec<ReadStageTemplate>,
        pipeline::RowSchema<'a>,
        usize,
    ),
    Error,
> {
    if parser.is_word("MATCH") {
        parser.parse_match_prefix()?;
        if !parser.is_word("WITH") && !parser.is_word("UNWIND") {
            return Err(expected(
                parser.current.at,
                "WITH or UNWIND before a pipeline aggregate RETURN",
            ));
        }
        let head = parser.graph_projection_head()?;
        let (stages, schema, depth) =
            parser.row_pipeline_prefix(head.schema(&parser.syntax.parameters))?;
        return Ok((Some(head), stages, schema, depth));
    }
    if !parser.is_word("WITH") && !parser.is_word("UNWIND") && !parser.is_word("RETURN") {
        return Err(expected(parser.current.at, "MATCH, WITH, UNWIND or RETURN"));
    }
    let (stages, schema, depth) = parser.row_pipeline_prefix(Vec::new())?;
    parser.syntax.return_at = parser.current.at;
    // The shared prefix starts at depth two (graph leaf + first projection).
    // This input starts with only Singleton. Its Aggregate parent uses the
    // freed level, so the prefix's existing early depth refusal stays sound.
    Ok((None, stages, schema, depth - 1))
}

pub(super) fn finish<'a>(
    parser: Parser<'a>,
    statement: &'a str,
    head: Option<GraphProjectionHead<'a>>,
    stages: Vec<ReadStageTemplate>,
) -> Result<UnresolvedGraphText<'a>, GraphSetTextError> {
    match head {
        Some(head) => parser.finish_graph_projection(statement, head, stages),
        None => Ok(UnresolvedGraphText {
            statement,
            syntax: parser.syntax,
            projection: None,
            pipeline: stages,
            singleton: true,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
        }),
    }
}

impl PreparedGraphPipelineAggregateText {
    /// Bind the entire relational pipeline, including zero-source WITH/UNWIND
    /// and standalone aggregate RETURN. Execute through the set-aggregate API:
    /// source callbacks run only for actual graph leaves, never for Singleton.
    ///
    /// The initial relation contains one empty row. UNWIND may expand or remove
    /// it; a keyless aggregate over an empty relation still yields its native
    /// zero/null summaries, whereas a keyed aggregate yields no groups. Input
    /// filters, DISTINCT and pages precede terminal expression evaluation.
    ///
    /// No argument interpolation, second parse, catalog access or execution
    /// occurs during binding. Count, wide integer sum and exact average values
    /// remain in GraphAggregateRow, never narrowed into scalar row stages.
    /// All clauses and original parameter occurrences share the ordinary
    /// definition and execution limits. An error cannot return a bound prefix.
    pub fn bind_relation_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSetAggregate, Error> {
        let values = self.input.checked_arguments(arguments)?;
        let relation = self.input.bind_values(&values)?;
        let declarations = self.summaries.iter().map(declaration).collect::<Vec<_>>();
        let mut query = PreparedGraphSetAggregate::prepare(
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
