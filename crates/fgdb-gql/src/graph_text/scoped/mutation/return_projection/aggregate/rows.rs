//! Source-free aggregate pipelines use the real singleton relation, not a
//! fabricated vertex or a graph scan. The terminal compiler and exact numeric
//! states are shared with graph-backed pipelines; only source ownership differs.

use super::*;
use crate::PreparedGraphSetAggregate;
use crate::row_join::RowJoinSpec;
use crate::set_text::multipart::{BoundContinuation, BoundReadInput};

pub(super) enum Head<'a> {
    Single(Option<GraphProjectionHead<'a>>),
    Multipart {
        first: Box<UnresolvedGraphText<'a>>,
        continuations: Vec<(UnresolvedGraphText<'a>, RowJoinSpec)>,
    },
}

pub(super) fn prefix<'a>(
    parser: &mut Parser<'a>,
    statement: &'a str,
    multipart: bool,
) -> Result<
    (
        Head<'a>,
        Vec<ReadStageTemplate>,
        pipeline::RowSchema<'a>,
        usize,
    ),
    Error,
> {
    if multipart {
        let (first, continuations, schema, depth) = parser.multipart_aggregate_prefix(statement)?;
        return Ok((
            Head::Multipart { first: Box::new(first), continuations },
            Vec::new(),
            schema,
            depth,
        ));
    }
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
        return Ok((Head::Single(Some(head)), stages, schema, depth));
    }
    if !parser.is_word("WITH") && !parser.is_word("UNWIND") && !parser.is_word("RETURN") {
        return Err(expected(parser.current.at, "MATCH, WITH, UNWIND or RETURN"));
    }
    let (stages, schema, depth) = parser.row_pipeline_prefix(Vec::new())?;
    parser.syntax.return_at = parser.current.at;
    // The shared prefix starts at depth two (graph leaf + first projection).
    // This input starts with only Singleton. Its Aggregate parent uses the
    // freed level, so the prefix's existing early depth refusal stays sound.
    Ok((Head::Single(None), stages, schema, depth - 1))
}

pub(super) fn finish<'a>(
    parser: Parser<'a>,
    statement: &'a str,
    head: Head<'a>,
    stages: Vec<ReadStageTemplate>,
    mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
) -> Result<BoundReadInput, GraphSetTextError> {
    let (first, continuations) = match head {
        Head::Single(head) => {
            let first = match head {
                Some(head) => parser.finish_graph_projection(statement, head, stages)?,
                None => UnresolvedGraphText {
                    statement,
                    syntax: parser.syntax,
                    projection: None,
                    pipeline: stages,
                    singleton: true,
                    leading: Vec::new(),
                    leading_types: Vec::new(),
                    correlations: Vec::new(),
                },
            };
            // Keep the old resolver and source definition for a single part.
            return Ok(BoundReadInput { first: first.resolve(resolve)?, continuations: Vec::new() });
        }
        Head::Multipart { mut first, mut continuations } => {
            // Terminal argument/key expressions are evaluated on the COMPLETE
            // joined row, after every original input filter, DISTINCT and page.
            // Never attach them to a graph leaf before optional null extension.
            if let Some((last, _)) = continuations.last_mut() {
                last.pipeline.extend(stages);
            } else {
                first.pipeline.extend(stages);
            }
            first.syntax.parameters.clone_from(&parser.syntax.parameters);
            first.syntax.parameter_offsets.clone_from(&parser.syntax.parameter_offsets);
            for (part, _) in &mut continuations {
                part.syntax.parameters.clone_from(&parser.syntax.parameters);
                part.syntax.parameter_offsets.clone_from(&parser.syntax.parameter_offsets);
            }
            (*first, continuations)
        }
    };
    // All syntax, aliases, argument types and total depth were admitted before
    // touching the catalog. Every graph source shares this domain-aware cache.
    let mut cache = std::collections::BTreeMap::new();
    let mut symbols = |kind, name: &str| {
        let key = (kind, name.to_owned());
        if let Some(symbol) = cache.get(&key) {
            return Some(*symbol);
        }
        let symbol = resolve(kind, name)?;
        cache.insert(key, symbol);
        Some(symbol)
    };
    let first = first.resolve(&mut symbols)?;
    let mut bound = Vec::new();
    for (input, join) in continuations {
        bound.push(BoundContinuation { input: input.resolve(&mut symbols)?, join });
    }
    Ok(BoundReadInput { first, continuations: bound })
}

impl PreparedGraphPipelineAggregateText {
    /// Whether the admitted definition contains no graph source. This is a
    /// structural property of the prepared plan, not a probe of current data.
    /// Hosts use it to preserve the existing graph-backed execution lane.
    #[must_use]
    pub fn is_source_free(&self) -> bool {
        self.graph_source_count() == 0
    }

    /// Number of actual graph leaves; singleton row inputs do not count.
    #[must_use]
    pub fn graph_source_count(&self) -> usize {
        usize::from(self.input.first.selection.is_some()) + self.input.continuations.len()
    }

    /// Hosts must use the source-aware relation executor for zero/multiple
    /// graph leaves. Do not mistake a nonempty source count for exactly one.
    #[must_use]
    pub fn requires_relational_input(&self) -> bool {
        self.graph_source_count() != 1
    }

    /// Bind the entire relational pipeline, including zero-source WITH/UNWIND
    /// and multipart MATCH/OPTIONAL MATCH. Execute through the set-aggregate API:
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
