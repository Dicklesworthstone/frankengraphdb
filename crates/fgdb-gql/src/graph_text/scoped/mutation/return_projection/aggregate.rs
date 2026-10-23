//! Exact terminal summaries after the shared native WITH prefix.
//! Only this typed terminal differs from ordinary row RETURN. Matching,
//! expressions, alias scopes, filters, pages and parameter registration share
//! the existing parser; execution receives the ordinary aggregate plan.

mod inputs;
mod rows;

use super::*;
use crate::algebra::GraphValueOrder;
use crate::pipeline_aggregate_text::{
    GraphPipelineAggregateTextError as Error, GraphPipelineAggregateTextErrorKind as Kind,
    PipelineOutput, PipelineSummary, PreparedGraphPipelineAggregateText,
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

#[derive(Clone, Copy)]
enum ReturnedValue {
    Key(usize),
    Summary(usize),
    /// An item combining aggregate results (fgdb-4avot): an index into the
    /// computed items, each a template over its own leaves.
    Computed(usize),
}
/// One leaf of a computed RETURN item: a grouping key's input column, or a
/// summary the item computes privately.
#[derive(Clone, Copy)]
enum Leaf {
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

/// The computed output, bound once per execution (fgdb-4avot).
fn bind_output(
    output: &[ReadProjectionTemplate],
    values: &[GqlParameterValue],
) -> Result<Vec<GraphSetProjection>, Error> {
    output
        .iter()
        .map(|item| {
            Ok(GraphSetProjection::new(
                item.name.clone(),
                bind_read_value(&item.value, values)?,
            ))
        })
        .collect()
}

impl<'a> Parser<'a> {
    fn starts_pipeline_summary(&self) -> Result<bool, Error> {
        Ok(matches!(self.current.kind, TokenKind::Word(word)
            if ["COUNT", "SUM", "SUM_INT", "AVG", "AVG_INT", "MIN", "MAX", "COLLECT"]
                .iter().any(|name| word.eq_ignore_ascii_case(name)))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')))
    }

    /// Whether the RETURN item here combines an aggregate call with other
    /// operands (`sum(v) * 10 / count(v)`, `count(*) + 1`, fgdb-4avot): it
    /// contains a call, and it is not exactly one call. Lookahead only.
    fn starts_aggregate_expression(&self) -> Result<bool, Error> {
        const ENDS: [&str; 7] = ["AS", "GROUP", "HAVING", "ORDER", "SKIP", "OFFSET", "LIMIT"];
        let single = self.starts_pipeline_summary()?;
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        let mut depth = 0_usize;
        let mut calls = 0_usize;
        // The token count at which the leading call's parenthesis closed.
        let mut closed = None;
        let mut seen = 0_usize;
        loop {
            let next = lexer.clone().next()?;
            match token.kind {
                TokenKind::End => break,
                TokenKind::Punct(b',') if depth == 0 => break,
                TokenKind::Word(word)
                    if depth == 0 && ENDS.iter().any(|end| word.eq_ignore_ascii_case(end)) =>
                {
                    break;
                }
                TokenKind::Word(word)
                    if matches!(next.kind, TokenKind::Punct(b'('))
                        && [
                            "COUNT", "SUM", "SUM_INT", "AVG", "AVG_INT", "MIN", "MAX", "COLLECT",
                        ]
                        .iter()
                        .any(|name| word.eq_ignore_ascii_case(name)) =>
                {
                    calls += 1;
                }
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && closed.is_none() {
                        closed = Some(seen + 1);
                    }
                }
                _ => {}
            }
            seen += 1;
            token = lexer.next()?;
        }
        Ok(calls > 0 && !(single && closed == Some(seen)))
    }

    fn pipeline_summary(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        inputs: &mut inputs::Inputs,
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
        let distinct = self.take_word("DISTINCT")?;
        if !distinct {
            self.take_word("ALL")?;
        }
        if self.take(b'*')? {
            if function != F::Count || distinct {
                return Err(expected(
                    name.at,
                    "COUNT(*) without DISTINCT, or a row expression",
                ));
            }
            self.punct(b')', ")")?;
            return Ok((F::CountRows, None));
        }
        let at = self.current.at;
        // The call's own argument: rand() there is a non-constant expression
        // (Return6 [15]), as in the graph aggregate's arguments.
        let outer = core::mem::replace(&mut self.aggregate_argument, true);
        let column = inputs.read(self, schema);
        self.aggregate_argument = outer;
        let column = column?;
        self.punct(b')', "one row expression as the aggregate argument")?;
        if matches!(function, F::SumInt | F::AverageInt)
            && !matches!(
                inputs.kind(column, schema),
                GraphSetColumnType::Scalar | GraphSetColumnType::Stored | GraphSetColumnType::Any
            )
        {
            return Err(expected(
                at,
                "a scalar row expression for a numeric aggregate",
            ));
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

    /// Check graph-backed or source-free row stages, final grouping, output
    /// aliases and HAVING before catalog access. The shared parser owns one
    /// token/parameter table for the entire statement. Source-free definitions
    /// bind through bind_relation_parameters, not the single-graph binder.
    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, Error> {
        Self::prepare_with_parameter_types_and_resolver(statement, declarations, resolve)
    }

    /// As [`Self::prepare_with_parameter_types`] with a host resolver, whose
    /// reverse catalog names `type(r)` and `labels(n)` values in row stages.
    pub fn prepare_with_parameter_types_and_resolver(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, Error> {
        let zones = resolve.time_zones();
        let tokens = PreparedGraphText::composition_tokens(statement, declarations)?;
        let multipart = crate::set_text::multipart::has_continuation(&tokens)
            || Parser::deferred_where_continues(statement, declarations, &tokens, zones.as_ref());
        let leading_match =
            !multipart && crate::set_text::multipart::leading_rows_then_match(&tokens);
        let prefix = |deferred: &[usize], implicit: &[usize]| -> Result<_, Error> {
            let mut parser = Parser::new_with_parameter_types(statement, declarations)?
                .with_zones(zones.clone());
            parser.deferred_wheres = deferred.to_vec();
            parser.implicit_withs = implicit.to_vec();
            // An implicit `WITH *` makes the statement multipart.
            let multipart = multipart || !implicit.is_empty();
            let leading = leading_match && implicit.is_empty();
            let prefix = rows::prefix(&mut parser, statement, multipart, leading)?;
            Ok((parser, prefix))
        };
        // Refusals re-read as unresolved_read_input re-reads them for a plain
        // RETURN, one at a time, reporting the first error if the statement
        // stays refused:
        // - a required continuation's WHERE the graph predicate refused
        //   filters after an implicit `WITH *` (fgdb-c6hvm);
        // - a later MATCH clause that read a name its child scope cannot
        //   see (an earlier relationship or an imported column) starts a
        //   new part at an implicit `WITH *`.
        // The prefix resolves no symbol, so a re-read never repeats a
        // catalog call.
        let (mut parser, (head, mut stages, schema, depth)) = match prefix(&[], &[]) {
            Ok(read) => read,
            Err(error) => {
                let (mut deferred, mut implicit) = (Vec::new(), Vec::new());
                let mut offset = error.offset;
                let mut read = None;
                loop {
                    if let Some(at) =
                        crate::set_text::multipart::continuation_where_at(&tokens, offset)
                            .filter(|at| !deferred.contains(at))
                    {
                        deferred.push(at);
                    } else if let Some(at) =
                        crate::set_text::multipart::implicit_with_at(&tokens, offset)
                            .filter(|at| !implicit.contains(at))
                    {
                        implicit.push(at);
                    } else {
                        break;
                    }
                    match prefix(&deferred, &implicit) {
                        Ok(again) => {
                            read = Some(again);
                            break;
                        }
                        Err(next) => offset = next.offset,
                    }
                }
                read.ok_or(error)?
            }
        };
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
            parser.take_all_quantifier()?;
        }
        let mut returned: Vec<Returned<'_>> = Vec::new();
        let mut summaries = Vec::new();
        let mut inputs = inputs::Inputs::new(schema.len());
        let mut computed: Vec<(ReadValueTemplate, Vec<Leaf>)> = Vec::new();
        loop {
            parser.capacity(
                returned.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let at = parser.current.at;
            let value = if parser.starts_aggregate_expression()? {
                // Aggregate calls become private summaries; every other leaf
                // must name a row column, which must be a grouping key.
                let mut leaves = Vec::new();
                let mut failure = None;
                let template = parser.read_resolved_item(
                    &mut |parser| {
                        if failure.is_some() {
                            return Ok(None);
                        }
                        let start = parser.current.at;
                        let leaf = match parser.starts_pipeline_summary() {
                            Ok(true) => match parser.pipeline_summary(&schema, &mut inputs) {
                                Ok((function, column)) => {
                                    summaries.push(PipelineSummary {
                                        name: format!("__return_summary_{}", summaries.len()),
                                        function,
                                        column,
                                    });
                                    Leaf::Summary(summaries.len() - 1)
                                }
                                Err(failed) => {
                                    failure = Some(failed);
                                    return Err(error(
                                        start,
                                        GraphPatternTextErrorKind::Expected(
                                            "an aggregate argument",
                                        ),
                                    ));
                                }
                            },
                            Ok(false) => {
                                let TokenKind::Word(word) = parser.current.kind else {
                                    return Ok(None);
                                };
                                if matches!(
                                    parser.lexer.clone().next()?.kind,
                                    TokenKind::Punct(b'(' | b'.')
                                ) {
                                    return Ok(None);
                                }
                                let Some(column) =
                                    schema.iter().position(|(name, _)| name.text == word)
                                else {
                                    return Ok(None);
                                };
                                parser.advance()?;
                                Leaf::Key(column)
                            }
                            Err(failed) => {
                                failure = Some(failed);
                                return Err(error(
                                    start,
                                    GraphPatternTextErrorKind::Expected("an aggregate argument"),
                                ));
                            }
                        };
                        leaves.push(leaf);
                        Ok(Some(leaves.len() - 1))
                    },
                    0,
                );
                if let Some(error) = failure {
                    return Err(error);
                }
                computed.push((template?, leaves));
                ReturnedValue::Computed(computed.len() - 1)
            } else if parser.starts_pipeline_summary()? {
                let (function, column) = parser.pipeline_summary(&schema, &mut inputs)?;
                summaries.push(PipelineSummary {
                    name: String::new(),
                    function,
                    column,
                });
                ReturnedValue::Summary(summaries.len() - 1)
            } else {
                ReturnedValue::Key(inputs.read(&mut parser, &schema)?)
            };
            let end = parser.current.at;
            // An unaliased item is named by its exact text, as in openCypher
            // (`count(*)`, and `n.p` for a carried `n`, never a private name).
            let name = if parser.take_word("AS")? {
                parser.name()?
            } else {
                parser.source_name(at, end)
            };
            if let ReturnedValue::Summary(index) = value {
                summaries[index].name = name.text.to_owned();
            }
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
                // A renamed RETURN key is an alias only as a complete GROUP
                // item. In x+1 or x[0], x still belongs to the input schema.
                let standalone = match parser.lexer.clone().next()?.kind {
                    TokenKind::End | TokenKind::Punct(b',') => true,
                    TokenKind::Word(word) => ["HAVING", "ORDER", "SKIP", "OFFSET", "LIMIT"]
                        .iter()
                        .any(|keyword| word.eq_ignore_ascii_case(keyword)),
                    _ => false,
                };
                let alias = if standalone
                    && let TokenKind::Word(word) = parser.current.kind
                    && !schema.iter().any(|(alias, _)| alias.text == word)
                {
                    returned.iter().find_map(|item| match item.value {
                        ReturnedValue::Key(column) if item.name.text == word => Some(column),
                        _ => None,
                    })
                } else {
                    None
                };
                let column = if let Some(column) = alias {
                    parser.advance()?;
                    column
                } else {
                    inputs.read(&mut parser, &schema)?
                };
                if keys.contains(&column) {
                    return Err(build(at, GraphAggregateBuildError::DuplicateKey { column }));
                }
                keys.push(column);
                if !parser.take(b',')? {
                    break;
                }
            }
        } else {
            for item in &returned {
                if let ReturnedValue::Key(column) = item.value
                    && !keys.contains(&column)
                {
                    parser.capacity(
                        keys.len() + summaries.len(),
                        MAX_PATTERN_VERTICES,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    keys.push(column);
                }
            }
        }
        for &column in &keys {
            if !inputs.is_computed(column)
                && summaries
                    .iter()
                    .any(|summary| summary.name == schema[column].0.text)
            {
                return Err(build(aggregate_at, GraphAggregateBuildError::DuplicateName));
            }
        }
        // fgdb-4avot: computed items read the evaluation row, every key and
        // then every summary. A non-aggregate leaf must be a grouping key; it
        // never becomes one implicitly, as in openCypher.
        let output = if computed.is_empty() {
            None
        } else {
            let key_of = |input: usize, at: usize, item: &'static str| {
                keys.iter()
                    .position(|column| *column == input)
                    .ok_or_else(|| expected(at, item))
            };
            let mut templates = Vec::with_capacity(returned.len());
            for item in &returned {
                let value = match item.value {
                    ReturnedValue::Key(input) => ReadValueTemplate::Column(key_of(
                        input,
                        item.name.at,
                        "every nonaggregate RETURN alias in GROUP BY",
                    )?),
                    ReturnedValue::Summary(at) => ReadValueTemplate::Column(keys.len() + at),
                    ReturnedValue::Computed(at) => {
                        let (template, leaves) = &computed[at];
                        let mut columns = Vec::with_capacity(leaves.len());
                        for leaf in leaves {
                            columns.push(match *leaf {
                                Leaf::Key(input) => key_of(
                                    input,
                                    item.name.at,
                                    "a grouping key for every nonaggregate operand of an aggregate RETURN expression",
                                )?,
                                Leaf::Summary(at) => keys.len() + at,
                            });
                        }
                        let mut template = template.clone();
                        crate::graph_text::aggregate::remap_output_columns(&mut template, &columns);
                        template
                    }
                };
                templates.push(ReadProjectionTemplate {
                    name: item.name.text.to_owned(),
                    value,
                });
            }
            Some(templates)
        };
        let computed_at: Vec<usize> = (returned.iter().enumerate())
            .filter(|(_, item)| matches!(item.value, ReturnedValue::Computed(_)))
            .map(|(at, _)| at)
            .collect();
        let mut names = Vec::new();
        let mut slots = Vec::new();
        let mut output_keys = Vec::new();
        let mut key_aliases = Vec::new();
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
                    let public = if let Some(index) = output_keys.iter().position(|at| *at == key) {
                        index
                    } else {
                        let index = output_keys.len();
                        output_keys.push(key);
                        // The first alias names the key's cell, so the
                        // physical key reports the result column's name.
                        if inputs.is_computed(input) || schema[input].0.text != item.name.text {
                            key_aliases.push((key, item.name.text.to_owned()));
                        }
                        index
                    };
                    let slot = GraphAggregateTextSlot::GroupKey(public);
                    (
                        slot,
                        GraphAggregateColumn::GroupKey(key),
                        inputs.kind(input, &schema),
                    )
                }
                ReturnedValue::Summary(at) => {
                    let summary = &summaries[at];
                    let kind = if matches!(
                        summary.function,
                        GraphAggregateFunction::Min | GraphAggregateFunction::Max
                    ) {
                        inputs.kind(summary.column.expect("extrema have one argument"), &schema)
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
                // Addressed only through the output projection; HAVING and
                // ORDER BY over it refuse below.
                ReturnedValue::Computed(_) => (
                    GraphAggregateTextSlot::Aggregate(0),
                    GraphAggregateColumn::Aggregate(0),
                    GraphSetColumnType::Any,
                ),
            };
            names.push(item.name.text.to_owned());
            slots.push(slot);
            having_columns.push(column);
            output_schema.push((item.name, kind));
        }
        if output.is_some() {
            // Projected output rows hold one value per written item.
            slots = (0..names.len())
                .map(GraphAggregateTextSlot::Aggregate)
                .collect();
        }
        // HAVING and the page address the output row. A boundary still live
        // here could resolve `n.p` to an output of the same width.
        parser.boundary_reads = None;
        if !computed_at.is_empty() && parser.is_word("HAVING") {
            return Err(expected(
                parser.current.at,
                "HAVING beside an aggregate RETURN item without a combined expression",
            ));
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
        let mut projected_ordering = Vec::new();
        let mut offset = ReadPageNumber::Literal(0);
        let mut count = None;
        if let Some(ReadStageTemplate::Page {
            order,
            offset: skip,
            count: limit,
            ..
        }) = parser.row_page(&output_schema)?
        {
            // A combined item ranks the computed output tuple, as the MATCH
            // aggregate does: every key then addresses that tuple, whose
            // rows hold one value per written item. Other queries keep their
            // evaluation-column ordering and exact transcripts.
            let projected = order.iter().any(|key| computed_at.contains(&key.column));
            for key in order {
                if projected {
                    if projected_ordering
                        .iter()
                        .any(|prior: &GraphValueOrder| prior.column == key.column)
                    {
                        return Err(expected(aggregate_at, "unique ORDER BY column"));
                    }
                    projected_ordering.push(GraphValueOrder {
                        column: key.column,
                        descending: key.descending,
                        nulls_first: key.nulls_first,
                    });
                    continue;
                }
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
        inputs.append_projection(
            &parser,
            &schema,
            &mut keys,
            &mut summaries,
            &mut stages,
            depth,
            aggregate_at,
        )?;
        let input = rows::finish(parser, statement, head, stages, resolve)?;
        Ok(Self {
            statement: statement.to_owned(),
            input,
            keys,
            output_keys,
            key_aliases,
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
            output: output.map(|items| {
                Box::new(PipelineOutput {
                    items,
                    ordering: projected_ordering,
                })
            }),
        })
    }

    /// Bind exactly one graph-backed pipeline for iterator-based source APIs.
    /// Zero-source and multipart pipelines use bind_relation_parameters instead.
    /// The existing relation constructor rejects any source count other than
    /// one; a later source is never discarded to satisfy the iterator contract.
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
        .with_key_aliases(&self.key_aliases)
        .map_err(|kind| build(self.aggregate_at, kind))?
        .with_key_output_columns(&self.output_keys)
        .map_err(|kind| build(self.aggregate_at, kind))?
        .with_distinct_output(self.output_distinct)
        .with_result_clauses(&[], &self.ordering)
        .map_err(|kind| build(self.aggregate_at, kind))?;
        if let Some(output) = &self.output {
            query = query
                .with_output_projection(bind_output(&output.items, &values)?)
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
