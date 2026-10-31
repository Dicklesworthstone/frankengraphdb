//! Source-free aggregate pipelines use the real singleton relation, not a
//! fabricated vertex or a graph scan. The terminal compiler and exact numeric
//! states are shared with graph-backed pipelines; only source ownership differs.

use super::*;
use crate::PreparedGraphSetAggregate;
use crate::set_text::multipart::BoundReadInput;
use pipeline::multipart::UnresolvedReadInput;

pub(super) enum Head<'a> {
    Single(Option<GraphProjectionHead<'a>>),
    Multipart(Box<UnresolvedReadInput<'a>>),
}

/// Whether the RETURN at the current token has both a pattern comprehension
/// or count subquery and an aggregate call among its items (`MATCH (n)
/// RETURN count([p = (n)-->() | p])`, TCK Pattern2 [6]; `sum(COUNT { ... })`).
/// The graph aggregate facade reads neither, so such a RETURN reads its MATCH
/// through an implicit `WITH *`, whose head gathers it (hoist_boundary_reads).
fn return_gathers_comprehension(parser: &mut Parser<'_>) -> Result<bool, Error> {
    const AGGREGATES: [&str; 12] = [
        "COUNT",
        "SUM",
        "SUM_INT",
        "AVG",
        "AVG_INT",
        "MIN",
        "MAX",
        "COLLECT",
        "PERCENTILEDISC",
        "PERCENTILECONT",
        "STDEV",
        "STDEVP",
    ];
    let saved = (parser.lexer.clone(), parser.current);
    let mut lexer = parser.lexer.clone();
    let (mut comprehension, mut aggregate) = (false, false);
    let mut depth = 0_usize;
    let found = loop {
        let token = match lexer.next() {
            Ok(token) => token,
            Err(error) => break Err(error.into()),
        };
        match token.kind {
            TokenKind::End => break Ok(comprehension && aggregate),
            TokenKind::Word(word)
                if depth == 0
                    && ["ORDER", "SKIP", "LIMIT", "UNION"]
                        .iter()
                        .any(|end| word.eq_ignore_ascii_case(end)) =>
            {
                break Ok(comprehension && aggregate);
            }
            // `COUNT { ... }` and `EXISTS { ... }` are pattern counts, which
            // the head gathers as it gathers a comprehension; `COUNT(` is the
            // aggregate.
            TokenKind::Word(word)
                if word.eq_ignore_ascii_case("COUNT") || word.eq_ignore_ascii_case("EXISTS") =>
            {
                parser.lexer = lexer.clone();
                parser.current = token;
                match parser.starts_pattern_count() {
                    Ok(starts) => comprehension |= starts,
                    Err(error) => break Err(error.into()),
                }
                aggregate |= word.eq_ignore_ascii_case("COUNT")
                    && matches!(
                        lexer.clone().next().map(|next| next.kind),
                        Ok(TokenKind::Punct(b'('))
                    );
            }
            TokenKind::Word(word)
                if AGGREGATES
                    .iter()
                    .any(|name| word.eq_ignore_ascii_case(name))
                    && matches!(
                        lexer.clone().next().map(|next| next.kind),
                        Ok(TokenKind::Punct(b'('))
                    ) =>
            {
                aggregate = true;
            }
            TokenKind::Punct(b'[') => {
                parser.lexer = lexer.clone();
                parser.current = token;
                match parser.starts_pattern_comprehension() {
                    Ok(starts) => comprehension |= starts,
                    Err(error) => break Err(error.into()),
                }
                depth += 1;
            }
            TokenKind::Punct(b'(' | b'{') => depth += 1,
            TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ => {}
        }
    };
    (parser.lexer, parser.current) = saved;
    found
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
        let (input, schema, depth) = parser.multipart_aggregate_prefix(statement)?;
        return Ok((Head::Multipart(Box::new(input)), Vec::new(), schema, depth));
    }
    // `UNWIND ... MATCH ... WITH ...`: the leading pipeline's graph part, the
    // CrossJoin a read builds for it, as the one source of the aggregate.
    if leading_match {
        let (first, schema, depth) = parser.leading_aggregate_prefix(statement)?;
        return Ok((
            Head::Multipart(Box::new(UnresolvedReadInput::from_first(first))),
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
        } else if parser.is_word("RETURN") && return_gathers_comprehension(parser)? {
            parser.implicit_with_head()?
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
    // A leading procedure shares the read parser's identity binding when a
    // following MATCH or output expression needs its yielded vertex. The
    // aggregate must retain that real graph source and its correlations.
    let (schema, leading) = parser.leading_row_prefix()?;
    let implied = if parser.is_word("MATCH") {
        Vec::new()
    } else {
        parser.implied_call_vertices(&schema, &leading)?
    };
    if parser.is_word("MATCH") || !implied.is_empty() {
        let at = parser.current.at;
        let (part, tail) = parser.leading_graph_part(statement, schema, leading, implied, true)?;
        let schema = tail.ok_or_else(|| expected(at, "aggregate input row schema"))?;
        let depth = part.depth();
        return Ok((
            Head::Multipart(Box::new(UnresolvedReadInput::from_first(part))),
            Vec::new(),
            schema,
            depth,
        ));
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
    // Every leading CALL or UNWIND contributes its unchanged native node.
    let depth = depth - 1 + leading.iter().map(ReadStageTemplate::depth).sum::<usize>();
    Ok((
        Head::Single(None),
        leading.into_iter().chain(stages).collect(),
        schema,
        depth,
    ))
}

pub(super) fn finish<'a>(
    mut parser: Parser<'a>,
    statement: &'a str,
    head: Head<'a>,
    stages: Vec<ReadStageTemplate>,
    mut resolve: impl GraphSymbolResolver,
) -> Result<BoundReadInput, GraphSetTextError> {
    let mut input = match head {
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
        Head::Multipart(mut input) => {
            // Terminal argument/key expressions are evaluated on the COMPLETE
            // joined row, after every original input filter, DISTINCT and page.
            // Never attach them to a graph leaf before optional null extension.
            input.append_stages(stages);
            // `WITH a MATCH (b) WITH a, b RETURN a.id, count(b)`: the key reads
            // the carried vertex's property through an element slot, which
            // binds only with a part (fgdb-x87sw).
            input.append_element_keys(&mut parser.syntax.element_keys);
            input
        }
    };
    input.share_parameters(&parser.syntax.parameters, &parser.syntax.parameter_offsets);
    // All syntax, aliases, argument types and total depth were admitted before
    // touching the catalog. Every graph source shares this domain-aware cache.
    let mut cache = std::collections::BTreeMap::new();
    Ok(input.resolve(&mut Cached {
        inner: &mut resolve,
        cache: &mut cache,
    })?)
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
        self.input.graph_source_count()
    }

    /// Hosts must preserve every continuation and procedure stage, including
    /// an Apply whose body owns the sole graph leaf. A source count is not an
    /// execution shape.
    #[must_use]
    pub fn requires_relational_input(&self) -> bool {
        self.input.first.selection.is_none()
            || !self.input.first.leading.is_empty()
            || self
                .input
                .first
                .pipeline
                .iter()
                .any(|stage| matches!(stage, ReadStageTemplate::Call { .. }))
            || !self.input.continuations.is_empty()
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
            if !output.computed_ordering.is_empty() {
                let expressions = output
                    .computed_ordering
                    .iter()
                    .map(|expression| bind_read_value(expression, &values).map_err(Error::from))
                    .collect::<Result<Vec<_>, _>>()?;
                query = query
                    .with_computed_order_by(expressions, &output.ordering)
                    .map_err(|kind| build(self.aggregate_at, kind))?;
            } else if !output.ordering.is_empty() {
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
        if let Some(expression) = &self.having_expression {
            query = expression.attach_relation(query, &values)?;
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
