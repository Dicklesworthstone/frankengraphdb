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
    leading_match: bool,
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
            Head::Multipart {
                first: Box::new(first),
                continuations,
            },
            Vec::new(),
            schema,
            depth,
        ));
    }
    // `UNWIND ... MATCH ... WITH ...`: the leading pipeline's graph part, the
    // CrossJoin a read builds for it, as the one source of the aggregate.
    if leading_match {
        let (first, schema, depth) = parser.leading_aggregate_prefix(statement)?;
        return Ok((
            Head::Multipart {
                first: Box::new(first),
                continuations: Vec::new(),
            },
            Vec::new(),
            schema,
            depth,
        ));
    }
    if parser.is_word("MATCH") {
        // MATCH ... WHERE <over values> RETURN count(*) is MATCH ... WITH *
        // WHERE ... RETURN count(*) (fgdb-1c29d), as for a plain RETURN: the
        // row pipeline reads the predicate after an implicit boundary that
        // projects every visible binding.
        parser.defer_where = true;
        let prefix = parser.parse_match_prefix();
        parser.defer_where = false;
        prefix?;
        let mut head = if let Some((lexer, at_where)) = parser.deferred_where.take() {
            if !parser.syntax.scopes.is_empty()
                || !(parser.is_word("RETURN") || parser.is_word("WITH"))
            {
                return Err(expected(
                    parser.current.at,
                    "RETURN or WITH right after a MATCH WHERE over values",
                ));
            }
            let head = parser.implicit_with_head()?;
            parser.lexer = lexer;
            parser.current = at_where;
            head
        } else {
            if !parser.is_word("WITH") && !parser.is_word("UNWIND") {
                return Err(expected(
                    parser.current.at,
                    "WITH or UNWIND before a pipeline aggregate RETURN",
                ));
            }
            parser.graph_projection_head()?
        };
        // `WITH n WHERE n.p > 1 RETURN n.q, count(*)`: the WITH's WHERE and
        // pages and the aggregate RETURN read carried properties.
        parser.hoist_boundary_reads(&mut head, 0)?;
        let (stages, schema, depth) =
            parser.row_pipeline_prefix(head.schema(&parser.syntax.parameters))?;
        return Ok((Head::Single(Some(head)), stages, schema, depth));
    }
    // An initial procedure supplies the source, exactly as in a non-aggregate
    // read. Subsequent calls are independent sources joined by the row prefix.
    let mut schema = Vec::new();
    let mut leading = Vec::new();
    if parser.is_word("CALL") {
        let at = parser.current.at;
        parser.advance()?;
        leading.push(parser.call_stage(&mut schema, at)?);
        while parser.is_word("UNWIND") {
            let at = parser.current.at;
            parser.advance()?;
            leading.push(parser.unwind_stage(&mut schema, at)?);
        }
        let implied = if parser.is_word("MATCH") {
            Vec::new()
        } else {
            parser.implied_call_vertices(&schema, &leading)?
        };
        if parser.is_word("MATCH") || !implied.is_empty() {
            // A yielded vertex's first-scope property reads use the same
            // identity-correlated graph source as an ordinary CALL read.
            // Expression-produced UNWIND elements remain row values.
            let at = parser.current.at;
            let (first, tail) =
                parser.leading_graph_part(statement, schema, leading, implied, true)?;
            let schema = tail
                .ok_or_else(|| expected(at, "a row schema before a pipeline aggregate RETURN"))?;
            let depth = first.depth();
            return Ok((
                Head::Multipart {
                    first: Box::new(first),
                    continuations: Vec::new(),
                },
                Vec::new(),
                schema,
                depth,
            ));
        }
    }
    if leading.is_empty()
        && !parser.is_word("WITH")
        && !parser.is_word("UNWIND")
        && !parser.is_word("RETURN")
    {
        return Err(expected(
            parser.current.at,
            "MATCH, CALL, WITH, UNWIND or RETURN",
        ));
    }
    let (stages, schema, depth) = parser.row_pipeline_prefix(schema)?;
    parser.syntax.return_at = parser.current.at;
    // The shared prefix starts at depth two (graph leaf + first projection).
    // This input starts with only Singleton. Its Aggregate parent uses the
    // freed level, so the prefix's existing early depth refusal stays sound.
    // Each initial CALL/UNWIND contributes its ordinary relational node.
    let depth = depth - 1 + leading.len();
    Ok((
        Head::Single(None),
        leading.into_iter().chain(stages).collect(),
        schema,
        depth,
    ))
}

pub(super) fn finish<'a>(
    parser: Parser<'a>,
    statement: &'a str,
    head: Head<'a>,
    stages: Vec<ReadStageTemplate>,
    mut resolve: impl GraphSymbolResolver,
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
                    row_fields: Vec::new(),
                },
            };
            // Keep the old resolver and source definition for a single part.
            return Ok(BoundReadInput {
                first: first.resolve(resolve)?,
                continuations: Vec::new(),
            });
        }
        Head::Multipart {
            mut first,
            mut continuations,
        } => {
            // Terminal argument/key expressions are evaluated on the COMPLETE
            // joined row, after every original input filter, DISTINCT and page.
            // Never attach them to a graph leaf before optional null extension.
            if let Some((last, _)) = continuations.last_mut() {
                last.pipeline.extend(stages);
            } else {
                first.pipeline.extend(stages);
            }
            first
                .syntax
                .parameters
                .clone_from(&parser.syntax.parameters);
            first
                .syntax
                .parameter_offsets
                .clone_from(&parser.syntax.parameter_offsets);
            // Terminal arguments and output expressions can register element
            // keys after the multipart input was captured. Bind their shared
            // slots through the same catalog as the complete input.
            first
                .syntax
                .element_keys
                .extend(parser.syntax.element_keys.iter().cloned());
            for (part, _) in &mut continuations {
                part.syntax.parameters.clone_from(&parser.syntax.parameters);
                part.syntax
                    .parameter_offsets
                    .clone_from(&parser.syntax.parameter_offsets);
            }
            (*first, continuations)
        }
    };
    // All syntax, aliases, argument types and total depth were admitted before
    // touching the catalog. Every graph source shares this domain-aware cache.
    let mut cache = std::collections::BTreeMap::new();
    let first = first.resolve(Cached {
        inner: &mut resolve,
        cache: &mut cache,
    })?;
    let mut bound = Vec::new();
    for (input, join) in continuations {
        bound.push(BoundContinuation {
            input: input.resolve(Cached {
                inner: &mut resolve,
                cache: &mut cache,
            })?,
            join,
        });
    }
    Ok(BoundReadInput {
        first,
        continuations: bound,
    })
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
        .with_key_aliases(&self.key_aliases)
        .map_err(|kind| build(self.aggregate_at, kind))?
        .with_key_output_columns(&self.output_keys)
        .map_err(|kind| build(self.aggregate_at, kind))?
        .with_distinct_output(self.output_distinct)
        .with_result_clauses(&[], &self.ordering)
        .map_err(|kind| build(self.aggregate_at, kind))?;
        if let Some(output) = &self.output {
            query = query
                .with_output_projection(super::bind_output(&output.items, &values)?)
                .map_err(|kind| build(self.aggregate_at, kind))?;
            if !output.ordering.is_empty() {
                query = query
                    .with_projected_order_by(&output.ordering)
                    .map_err(|kind| build(self.aggregate_at, kind))?;
            }
        }
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

/// One resolver cache shared by every graph source of a statement; the host's
/// reverse catalog passes through for `labels(n)` and `type(r)` names.
struct Cached<'c, R> {
    inner: &'c mut R,
    cache: &'c mut std::collections::BTreeMap<(GraphSymbolKind, String), GraphSymbol>,
}

impl<R: GraphSymbolResolver> GraphSymbolResolver for Cached<'_, R> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        let key = (kind, name.to_owned());
        if let Some(symbol) = self.cache.get(&key) {
            return Some(*symbol);
        }
        let symbol = self.inner.resolve_symbol(kind, name)?;
        self.cache.insert(key, symbol);
        Some(symbol)
    }

    fn time_zones(&self) -> Option<crate::GqlTimeZones> {
        self.inner.time_zones()
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        self.inner.reverse_catalog()
    }
}
