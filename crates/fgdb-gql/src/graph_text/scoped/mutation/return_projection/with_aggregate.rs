//! WITH grouping is a native relational stage, not a rewritten RETURN query.
//! Parsing finishes before catalog callbacks; binding lowers the retained
//! expressions to Project -> GroupAggregate -> Project under the same limits.

use super::*;
use crate::GraphAggregateFunction as Function;
use crate::set_text::aggregate::{ReadAggregateSpec, ReadAggregateStage};

mod graph;

pub(in crate::graph_text::scoped::mutation) fn function(word: &str) -> Option<Function> {
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

/// percentileDisc and percentileCont (fgdb-qbaex), stDev and stDevP:
/// aggregates the value reader applies after aggregation to a hidden collect
/// of their first argument, so a grouping stage reads each as a combined item.
fn percentile(word: &str) -> bool {
    ["PERCENTILEDISC", "PERCENTILECONT", "STDEV", "STDEVP"]
        .iter()
        .any(|name| word.eq_ignore_ascii_case(name))
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
    specs.iter().map(ReadAggregateSpec::declaration).collect()
}

impl ReadAggregateStage {
    pub(crate) fn bind(
        &self,
        input: PreparedGraphSet,
        values: &[GqlParameterValue],
        at: usize,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        // These are compiler-private aggregate arguments, evaluated over the
        // already selected child sequence. Canonicalizing their tuples here
        // would discard an earlier ORDER BY before COLLECT observes it.
        let mut inputs = Vec::with_capacity(self.inputs.len());
        for output in &self.inputs {
            inputs.push(GraphSetProjection::new(
                output.name.clone(),
                bind_read_value(&output.value, values)?,
            ));
        }
        let input = input
            .project_aggregate_input(inputs)
            .map_err(|kind| GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::ProjectionBuild(kind),
            })?;
        let input = input
            .group_by(&self.keys, &declarations(&self.aggregates))
            .map_err(|kind| aggregate_error(at, kind))?;
        let mut outputs = Vec::with_capacity(self.outputs.len());
        for (position, (name, column, _)) in self.outputs.iter().enumerate() {
            let value = match self.computed.iter().find(|(at, _)| *at == position) {
                Some((_, template)) => bind_read_value(template, values)?,
                None => GraphSetValue::Column(*column),
            };
            outputs.push(GraphSetProjection::new(name.clone(), value));
        }
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
    /// An item combining aggregate results; its value is a computed output.
    Computed,
}

/// One leaf of a combined WITH item (fgdb-4avot): an input slot that must
/// be a grouping key, or a summary the item computes.
#[derive(Clone, Copy)]
enum Leaf {
    Key(usize),
    Summary(usize),
}

/// A native-token lookahead only. The caller still parses every token normally.
/// Quoted strings, property names and aliases cannot choose an aggregate lane.
impl<'a> Parser<'a> {
    pub(super) fn with_has_aggregate(&self) -> Result<bool, GraphPatternTextError> {
        self.projection_has_aggregate("WITH")
    }

    pub(super) fn return_has_aggregate(&self) -> Result<bool, GraphPatternTextError> {
        self.projection_has_aggregate("RETURN")
    }

    fn projection_has_aggregate(&self, clause: &str) -> Result<bool, GraphPatternTextError> {
        if !self.is_word(clause) {
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
                    if word.eq_ignore_ascii_case("EXISTS")
                        && matches!(lexer.clone().next()?.kind, TokenKind::Punct(b'{'))
                    {
                        // Aggregation inside a returning existential body is
                        // local to that body, never an enclosing RETURN/WITH
                        // aggregate. The read-body owner admits the contents.
                        lexer.next()?;
                        let mut body_depth = 1_usize;
                        while body_depth != 0 {
                            match lexer.next()?.kind {
                                TokenKind::Punct(b'(' | b'[' | b'{') => body_depth += 1,
                                TokenKind::Punct(b')' | b']' | b'}') => body_depth -= 1,
                                TokenKind::End => return Ok(false),
                                _ => {}
                            }
                        }
                        previous = Some(TokenKind::Punct(b'}'));
                        continue;
                    }
                    let text_with = word.eq_ignore_ascii_case("WITH")
                        && matches!(previous, Some(TokenKind::Word(word))
                            if word.eq_ignore_ascii_case("STARTS") || word.eq_ignore_ascii_case("ENDS"));
                    if depth == 0
                        && !text_with
                        && [
                            "WHERE",
                            "ORDER",
                            "SKIP",
                            "OFFSET",
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
                    if (function(word).is_some() || percentile(word))
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
        let implicit = self.implicit_with_binding()?;
        let value = self.read_row_value(schema, 0)?;
        let name = implicit
            .filter(|(_, boundary)| {
                *boundary == self.current.at && matches!(value, ReadValueTemplate::Column(_))
            })
            .map(|(name, _)| name);
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
                GqlParameterType::Map => GqlParameterValue::Map(
                    crate::GqlMapParameter::new(Vec::new()).expect("bounded empty map"),
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
        Ok((self.input_slot(inputs, value)?, kind, name))
    }

    /// Equal input expressions share a slot, but parameter values never
    /// participate in that decision. Repeated key aliases group only once.
    fn input_slot(
        &self,
        inputs: &mut Vec<ReadProjectionTemplate>,
        value: ReadValueTemplate,
    ) -> Result<usize, GraphSetTextError> {
        let mut bytes = Vec::new();
        value.append_template_transcript(&mut bytes);
        for (column, input) in inputs.iter().enumerate() {
            let mut prior = Vec::new();
            input.value.append_template_transcript(&mut prior);
            if prior == bytes {
                return Ok(column);
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
        Ok(column)
    }

    /// LDBC SNB IC7: `WITH a, b, count(*) AS c RETURN NOT ((a)-[:K]-(b))`.
    /// Each pattern the scope after this grouping reads over its bare keys
    /// alone, gathered by the graph head before it (grouped_comprehensions),
    /// becomes one more hidden key: a function of keys, so the groups, their
    /// multiplicity and DISTINCT are unchanged. Returns each pattern's
    /// (source start, source end, output column), which that scope reads
    /// where the pattern stands (comprehension_read).
    fn grouped_pattern_keys(
        &mut self,
        schema: &pipeline::RowSchema<'a>,
        inputs: &mut Vec<ReadProjectionTemplate>,
        keys: &mut Vec<usize>,
        returned: &mut Vec<(Name<'a>, Output, GraphSetColumnType)>,
    ) -> Result<Vec<(usize, usize, usize)>, GraphSetTextError> {
        let Some((_, spans)) = self
            .grouped_comprehensions
            .take()
            .filter(|(width, _)| *width == schema.len())
        else {
            return Ok(Vec::new());
        };
        let mut reads = Vec::new();
        for (start, end, column) in spans {
            self.capacity(
                returned.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let input = self.input_slot(inputs, ReadValueTemplate::Column(column))?;
            let key = match keys.iter().position(|&key| key == input) {
                Some(key) => key,
                None => {
                    keys.push(input);
                    keys.len() - 1
                }
            };
            let name = graph::HIDDEN_NAMES
                .iter()
                .copied()
                .find(|name| returned.iter().all(|(output, _, _)| output.text != *name))
                .ok_or_else(|| expected(start, "bounded grouping pattern reads"))?;
            returned.push((
                Name {
                    text: name,
                    at: start,
                },
                Output::Key(key),
                schema[column].1,
            ));
            reads.push((start, end, returned.len() - 1));
        }
        Ok(reads)
    }

    /// fgdb-ezgeq: `WITH p, count(f) AS c RETURN p.name, c`. Each property the
    /// scope after this grouping reads through a kept binding (`grouped`, found
    /// by the graph head) becomes one more hidden key. The graph head keeps
    /// only items that are exactly `binding` or `binding AS alias`, so the
    /// binding is already a key, and a property is a function of its element:
    /// the groups, their multiplicity and DISTINCT are unchanged. The hidden
    /// keys follow the visible outputs, and the returned reads resolve
    /// `alias.property` to them until the next stage drops them.
    fn grouped_keys(
        &self,
        schema: &pipeline::RowSchema<'a>,
        inputs: &mut Vec<ReadProjectionTemplate>,
        keys: &mut Vec<usize>,
        returned: &mut Vec<(Name<'a>, Output, GraphSetColumnType)>,
    ) -> Result<Vec<(&'a str, Name<'a>, usize)>, GraphSetTextError> {
        let Some(boundary) = self
            .boundary_reads
            .as_ref()
            .filter(|boundary| boundary.width == schema.len())
        else {
            return Ok(Vec::new());
        };
        let mut reads = Vec::new();
        for &(alias, _, property, column) in &boundary.grouped {
            self.capacity(
                returned.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let input = self.input_slot(inputs, ReadValueTemplate::Column(column))?;
            let key = match keys.iter().position(|&key| key == input) {
                Some(key) => key,
                None => {
                    keys.push(input);
                    keys.len() - 1
                }
            };
            let name = graph::HIDDEN_NAMES
                .iter()
                .copied()
                .find(|name| returned.iter().all(|(output, _, _)| output.text != *name))
                .ok_or_else(|| expected(property.at, "bounded grouping property reads"))?;
            returned.push((
                Name {
                    text: name,
                    at: property.at,
                },
                Output::Key(key),
                schema[column].1,
            ));
            reads.push((alias, property, returned.len() - 1));
        }
        Ok(reads)
    }

    /// Whether the item spelled from `start` to `end` is one the WITH's own
    /// ORDER BY may repeat in place of its output: a name, `name.key` or one
    /// call `f(...)`. A wider expression stays unmatched, so a repeat can
    /// never regroup the operators around it.
    pub(super) fn repeatable_item(
        &self,
        start: usize,
        end: usize,
    ) -> Result<bool, GraphPatternTextError> {
        let mut lexer = crate::graph_text::Lexer {
            text: self.lexer.text,
            at: start,
            tokens: 0,
        };
        let mut kinds = Vec::new();
        loop {
            let token = lexer.next()?;
            if token.at >= end {
                break;
            }
            kinds.push(token.kind);
        }
        Ok(match kinds.as_slice() {
            [TokenKind::Word(_)] => true,
            [
                TokenKind::Word(_),
                TokenKind::Punct(b'.'),
                TokenKind::Word(_) | TokenKind::DelimitedKeyword(_),
            ] => true,
            [
                TokenKind::Word(_),
                TokenKind::Punct(b'('),
                ..,
                TokenKind::Punct(b')'),
            ] => {
                // The call's own parenthesis closes at the last token, not
                // before it (`f(a) + g(b)` is two operands).
                let last = kinds.len() - 2;
                let mut depth = 0_usize;
                kinds[1..].iter().enumerate().all(|(at, kind)| {
                    match kind {
                        TokenKind::Punct(b'(') => depth += 1,
                        TokenKind::Punct(b')') => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    depth > 0 || at == last
                })
            }
            _ => false,
        })
    }

    /// Whether the WITH item here combines an aggregate call with other
    /// operands (`count(*) + 1`, `me.age + count(you.age)`, fgdb-4avot): it
    /// contains a call, and it is not exactly one call. Lookahead only.
    fn starts_combined_aggregate(&self) -> Result<bool, GraphPatternTextError> {
        const ENDS: [&str; 15] = [
            "AS",
            "WHERE",
            "ORDER",
            "SKIP",
            "OFFSET",
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
        ];
        let single = matches!(self.current.kind, TokenKind::Word(word) if function(word).is_some())
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('));
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        let mut depth = 0_usize;
        let mut calls = 0_usize;
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
                    if (function(word).is_some() || percentile(word))
                        && matches!(next.kind, TokenKind::Punct(b'(')) =>
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

    /// A WITH item combining aggregate results (fgdb-4avot). Its aggregate
    /// calls become summaries. Every other leaf is a row column or a carried
    /// property read (`me.age` through the boundary), and must be one of this
    /// grouping's keys. Returns the item over its leaves.
    fn with_combined_item(
        &mut self,
        schema: &pipeline::RowSchema<'a>,
        inputs: &mut Vec<ReadProjectionTemplate>,
        aggregates: &mut Vec<ReadAggregateSpec>,
    ) -> Result<(ReadValueTemplate, Vec<Leaf>), GraphSetTextError> {
        let mut leaves = Vec::new();
        let mut failure = None;
        let template = self.read_resolved_item(
            &mut |parser| {
                if failure.is_some() {
                    return Ok(None);
                }
                let start = parser.current.at;
                let refuse = || {
                    error(
                        start,
                        GraphPatternTextErrorKind::Expected("a combined WITH aggregate operand"),
                    )
                };
                // A percentile's first argument is a hidden collect, which
                // the value reader applies the percentile to (fgdb-qbaex).
                if core::mem::take(&mut parser.percentile_leaf) {
                    return match parser.with_collected_argument(schema, inputs, aggregates) {
                        Ok(index) => {
                            leaves.push(Leaf::Summary(index));
                            Ok(Some(leaves.len() - 1))
                        }
                        Err(error) => {
                            failure = Some(error);
                            Err(refuse())
                        }
                    };
                }
                if let TokenKind::Word(word) = parser.current.kind
                    && let Some(call) = function(word)
                    && matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
                {
                    return match parser.with_summary_call(call, schema, inputs, aggregates, start) {
                        Ok((index, _)) => {
                            leaves.push(Leaf::Summary(index));
                            Ok(Some(leaves.len() - 1))
                        }
                        Err(error) => {
                            failure = Some(error);
                            Err(refuse())
                        }
                    };
                }
                let column = if let Some(column) = parser.boundary_read(schema.len())? {
                    column
                } else {
                    let TokenKind::Word(word) = parser.current.kind else {
                        return Ok(None);
                    };
                    if matches!(
                        parser.lexer.clone().next()?.kind,
                        TokenKind::Punct(b'(' | b'.')
                    ) {
                        return Ok(None);
                    }
                    let visible = parser.visible_width(schema);
                    let Some(column) = schema[..visible]
                        .iter()
                        .position(|(name, _)| name.text == word)
                    else {
                        return Ok(None);
                    };
                    parser.advance()?;
                    column
                };
                match parser.input_slot(inputs, ReadValueTemplate::Column(column)) {
                    Ok(slot) => {
                        leaves.push(Leaf::Key(slot));
                        Ok(Some(leaves.len() - 1))
                    }
                    Err(error) => {
                        failure = Some(error);
                        Err(refuse())
                    }
                }
            },
            0,
        );
        if let Some(error) = failure {
            return Err(error);
        }
        Ok((template?, leaves))
    }

    /// The expression at the current token, up to its delimiter, as a
    /// collect summary: a percentile's collected argument (fgdb-qbaex).
    /// Returns the summary index.
    fn with_collected_argument(
        &mut self,
        schema: &pipeline::RowSchema<'a>,
        inputs: &mut Vec<ReadProjectionTemplate>,
        aggregates: &mut Vec<ReadAggregateSpec>,
    ) -> Result<usize, GraphSetTextError> {
        // rand() in the argument is a non-constant expression.
        let outer = core::mem::replace(&mut self.aggregate_argument, true);
        let input = self.with_input(schema, inputs);
        self.aggregate_argument = outer;
        let (column, _, _) = input?;
        let index = aggregates.len();
        aggregates.push(ReadAggregateSpec {
            name: format!("__with_summary_{index}"),
            function: Function::Collect,
            column: Some(column),
        });
        Ok(index)
    }

    /// One aggregate call at the current token as a WITH summary. Its
    /// argument becomes an input slot. Returns the summary index and kind.
    fn with_summary_call(
        &mut self,
        mut function: Function,
        schema: &pipeline::RowSchema<'a>,
        inputs: &mut Vec<ReadProjectionTemplate>,
        aggregates: &mut Vec<ReadAggregateSpec>,
        item_at: usize,
    ) -> Result<(usize, GraphSetColumnType), GraphSetTextError> {
        self.advance()?;
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
            // rand() in the argument is a non-constant expression.
            let outer = core::mem::replace(&mut self.aggregate_argument, true);
            let input = self.with_input(schema, inputs);
            self.aggregate_argument = outer;
            let (column, kind, _) = input?;
            (Some(column), kind)
        };
        self.punct(b')', "one expression as the aggregate argument")?;
        if matches!(function, Function::SumInt | Function::AverageInt)
            && !input_kind.is_scalar_like()
            && input_kind != GraphSetColumnType::Any
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
        Ok((index, kind))
    }

    pub(super) fn with_aggregate_stage(
        &mut self,
        schema: &pipeline::RowSchema<'a>,
    ) -> Result<(ReadStageTemplate, pipeline::RowSchema<'a>), GraphSetTextError> {
        self.aggregate_projection_stage(schema, false)
    }

    /// Reuse native grouping for a write tail without synthesizing query text.
    /// RETURN permits source-spelled names; intermediate WITH still requires
    /// explicit aliases for computed values.
    pub(super) fn return_aggregate_stage(
        &mut self,
        schema: &pipeline::RowSchema<'a>,
    ) -> Result<(ReadStageTemplate, pipeline::RowSchema<'a>), GraphSetTextError> {
        self.aggregate_projection_stage(schema, true)
    }

    fn aggregate_projection_stage(
        &mut self,
        schema: &pipeline::RowSchema<'a>,
        returning: bool,
    ) -> Result<(ReadStageTemplate, pipeline::RowSchema<'a>), GraphSetTextError> {
        let at = self.current.at;
        self.word(if returning { "RETURN" } else { "WITH" })?;
        let distinct = self.take_word("DISTINCT")?;
        if !distinct {
            self.take_all_quantifier()?;
        }
        let mut inputs = Vec::new();
        let mut keys = Vec::new();
        let mut aggregates = Vec::new();
        let mut returned: Vec<(Name<'a>, Output, GraphSetColumnType)> = Vec::new();
        let mut sources = Vec::new();
        let mut combined = Vec::new();
        loop {
            self.capacity(
                returned.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let item_at = self.current.at;
            if self.starts_combined_aggregate()? {
                let (template, leaves) =
                    self.with_combined_item(schema, &mut inputs, &mut aggregates)?;
                let end = self.current.at;
                let name = if self.take_word("AS")? {
                    self.name()?
                } else if returning {
                    self.source_name(item_at, end)
                } else {
                    return Err(expected(item_at, "AS alias for a computed WITH value"));
                };
                if returned.iter().any(|(prior, _, _)| prior.text == name.text) {
                    return Err(aggregate_error(
                        name.at,
                        crate::GraphAggregateBuildError::DuplicateName,
                    ));
                }
                combined.push((returned.len(), item_at, template, leaves));
                returned.push((name, Output::Computed, GraphSetColumnType::Any));
                if !self.take(b',')? {
                    break;
                }
                continue;
            }
            let summary = match self.current.kind {
                TokenKind::Word(word)
                    if matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) =>
                {
                    function(word)
                }
                _ => None,
            };
            let (output, kind, default_name) = if let Some(function) = summary {
                let (index, kind) = self.with_summary_call(
                    function,
                    schema,
                    &mut inputs,
                    &mut aggregates,
                    item_at,
                )?;
                (Output::Aggregate(index), kind, None)
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
            if self.repeatable_item(item_at, self.current.at)? {
                sources.push((item_at, self.current.at, returned.len()));
            }
            let end = self.current.at;
            let name = if self.take_word("AS")? {
                self.name()?
            } else if returning {
                self.source_name(item_at, end)
            } else {
                default_name
                    .ok_or_else(|| expected(item_at, "AS alias for a computed WITH value"))?
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
        let visible = returned.len();
        // A terminal RETURN has no later element-reading scope. Its explicit
        // key and argument expressions already consumed the boundary cells.
        let reads = if returning {
            Vec::new()
        } else {
            self.grouped_keys(schema, &mut inputs, &mut keys, &mut returned)?
        };
        let spans = if returning {
            Vec::new()
        } else {
            self.grouped_pattern_keys(schema, &mut inputs, &mut keys, &mut returned)?
        };
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
        // A combined item reads the grouped row: every key, then every
        // aggregate. A non-aggregate leaf must be a key; it never becomes one
        // implicitly, as in openCypher.
        let mut computed = Vec::with_capacity(combined.len());
        for (position, item_at, mut template, leaves) in combined {
            let mut columns = Vec::with_capacity(leaves.len());
            for leaf in leaves {
                columns.push(match leaf {
                    Leaf::Key(slot) => keys.iter().position(|&key| key == slot).ok_or_else(|| {
                        expected(
                            item_at,
                            "a grouping key for every nonaggregate operand of a combined WITH item",
                        )
                    })?,
                    Leaf::Summary(index) => keys.len() + index,
                });
            }
            crate::graph_text::aggregate::remap_output_columns(&mut template, &columns);
            computed.push((position, template));
        }
        let output_schema: pipeline::RowSchema<'a> = returned
            .iter()
            .map(|(name, _, kind)| (*name, *kind))
            .collect();
        let outputs = returned
            .into_iter()
            .map(|(name, output, kind)| {
                let column = match output {
                    Output::Key(at) => at,
                    Output::Aggregate(at) => keys.len() + at,
                    Output::Computed => 0,
                };
                (name.text.to_owned(), column, kind)
            })
            .collect();
        // Hidden keys stay until the scope after this stage ends.
        self.boundary_reads = (!reads.is_empty() || !spans.is_empty()).then(|| BoundaryReads {
            visible,
            width: output_schema.len(),
            reads,
            functions: Vec::new(),
            grouped: Vec::new(),
            scoped: Vec::new(),
        });
        self.projected_sources = (!sources.is_empty()).then_some((output_schema.len(), sources));
        if !spans.is_empty() {
            self.comprehension_reads = Some((output_schema.len(), spans));
        }
        Ok((
            ReadStageTemplate::Aggregate {
                at,
                stage: ReadAggregateStage {
                    inputs,
                    keys,
                    aggregates,
                    outputs,
                    computed,
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
