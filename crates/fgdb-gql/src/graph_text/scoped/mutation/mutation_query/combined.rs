//! Compound aggregates over the write owner's final-state rows. No source
//! text is rewritten, no graph is rescanned, and no second evaluator exists.

mod hoist;

#[cfg(test)]
mod grouped_tests;

use super::*;
use crate::GraphAggregateFunction as Function;
use crate::graph_text::aggregate::remap_output_columns;
use crate::set_text::aggregate::ReadAggregateSpec;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Leaf {
    Binding(usize),
    Summary(usize),
}

struct Item<'a> {
    at: usize,
    name: Name<'a>,
    value: ReadValueTemplate,
    leaves: Vec<Leaf>,
    kind: GraphSetColumnType,
}

/// Temporary expression columns. A nonaggregate leaf retains its raw write
/// binding; a summary retains its own aggregate index until all keys are known.
struct AggregateLeaves<'r, 'a, L> {
    raw: &'r mut L,
    inputs: &'r mut Vec<ReadProjectionTemplate>,
    summaries: &'r mut Vec<ReadAggregateSpec>,
    leaves: Vec<Leaf>,
    kinds: Vec<GraphSetColumnType>,
    failure: Option<GraphSetTextError>,
    lifetime: core::marker::PhantomData<&'a str>,
}

impl<'a, L: ReturnLeaves<'a>> AggregateLeaves<'_, 'a, L> {
    fn slot(
        &mut self,
        parser: &mut Parser<'a>,
        leaf: Leaf,
        kind: GraphSetColumnType,
    ) -> Result<usize, GraphPatternTextError> {
        if let Some(column) = self.leaves.iter().position(|&known| known == leaf) {
            return Ok(column);
        }
        parser.capacity(
            self.leaves.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        self.leaves.push(leaf);
        self.kinds.push(kind);
        Ok(self.leaves.len() - 1)
    }

    fn binding(
        &mut self,
        parser: &mut Parser<'a>,
        binding: usize,
    ) -> Result<usize, GraphPatternTextError> {
        let kind = self.raw.types()[binding];
        self.slot(parser, Leaf::Binding(binding), kind)
    }
}

impl<'a, L: ReturnLeaves<'a>> ReturnLeaves<'a> for AggregateLeaves<'_, 'a, L> {
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        if let TokenKind::Word(word) = parser.current.kind
            && let Some(function) = return_projection::with_aggregate::function(word)
            && matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let at = parser.current.at;
            let result =
                parser.compound_write_summary(self.raw, function, self.inputs, self.summaries);
            return match result {
                Ok((summary, kind)) => self.slot(parser, Leaf::Summary(summary), kind).map(Some),
                Err(failure) => {
                    self.failure = Some(failure);
                    Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected("a write aggregate operand"),
                    ))
                }
            };
        }
        self.raw
            .leaf(parser)?
            .map(|binding| self.binding(parser, binding))
            .transpose()
    }

    fn map_input(
        &mut self,
        parser: &mut Parser<'a>,
        source: Name<'a>,
        input: ReturnMapInput<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        let binding = self.raw.map_input(parser, source, input)?;
        self.binding(parser, binding)
    }

    fn types(&self) -> Vec<GraphSetColumnType> {
        self.kinds.clone()
    }
}

fn combined(kinds: &[TokenKind<'_>]) -> bool {
    let call = |kind: TokenKind<'_>| {
        matches!(kind, TokenKind::Word(word)
            if return_projection::with_aggregate::function(word).is_some())
    };
    let has_call = kinds.windows(2).enumerate().any(|(at, pair)| {
        call(pair[0])
            && matches!(pair[1], TokenKind::Punct(b'('))
            && (at == 0 || !matches!(kinds[at - 1], TokenKind::Punct(b'.')))
    });
    if !has_call {
        return false;
    }
    if kinds.len() < 3 || !call(kinds[0]) || !matches!(kinds[1], TokenKind::Punct(b'(')) {
        return true;
    }
    let mut depth = 0_usize;
    for (at, kind) in kinds.iter().enumerate().skip(1) {
        match kind {
            TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TokenKind::Punct(b')' | b']' | b'}') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return at + 1 != kinds.len();
                }
            }
            _ => {}
        }
    }
    false // Unbalanced input belongs to the real parser's diagnostic.
}

impl<'a> Parser<'a> {
    /// Only new compound forms choose this lowering. Existing plain items and
    /// top-level aggregates keep their original compiler and template identity.
    pub(super) fn write_items_have_combined_aggregate(
        &self,
    ) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        let mut kinds = Vec::new();
        let mut depth = 0_usize;
        let mut skip_alias = false;
        loop {
            if skip_alias {
                skip_alias = false;
                token = lexer.next()?;
                continue;
            }
            let previous = kinds.last().copied();
            let named = matches!(previous, Some(TokenKind::Punct(b'.')));
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(combined(&kinds)),
                TokenKind::Word(word)
                    if depth == 0 && !named && word.eq_ignore_ascii_case("AS") =>
                {
                    if combined(&kinds) {
                        return Ok(true);
                    }
                    kinds.clear();
                    skip_alias = true;
                }
                TokenKind::Punct(b',') if depth == 0 => {
                    if combined(&kinds) {
                        return Ok(true);
                    }
                    kinds.clear();
                }
                TokenKind::Word(word)
                    if depth == 0
                        && !named
                        && ([
                            "WHERE",
                            "ORDER",
                            "SKIP",
                            "LIMIT",
                            "RETURN",
                            "MATCH",
                            "OPTIONAL",
                            "UNWIND",
                            "UNION",
                            "EXCEPT",
                            "INTERSECT",
                            "CALL",
                        ]
                        .iter()
                        .any(|end| word.eq_ignore_ascii_case(end))
                            || word.eq_ignore_ascii_case("WITH")
                                && !matches!(previous,
                        Some(TokenKind::Word(prior)) if prior.eq_ignore_ascii_case("STARTS")
                            || prior.eq_ignore_ascii_case("ENDS"))) =>
                {
                    return Ok(combined(&kinds));
                }
                kind => {
                    match kind {
                        TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                        TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                    kinds.push(kind);
                }
            }
            token = lexer.next()?;
        }
    }

    fn compound_write_summary(
        &mut self,
        raw: &mut impl ReturnLeaves<'a>,
        mut function: Function,
        inputs: &mut Vec<ReadProjectionTemplate>,
        summaries: &mut Vec<ReadAggregateSpec>,
    ) -> Result<(usize, GraphSetColumnType), GraphSetTextError> {
        let at = self.current.at;
        self.advance()?;
        self.punct(b'(', "(")?;
        let distinct = self.take_word("DISTINCT")?;
        if !distinct {
            self.take_word("ALL")?;
        }
        let (column, input_kind) = if self.take(b'*')? {
            if function != Function::Count || distinct {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("COUNT(*) without DISTINCT"),
                )
                .into());
            }
            function = Function::CountRows;
            (None, GraphSetColumnType::Scalar)
        } else {
            // Raw leaves cannot resolve another summary: nested aggregates
            // refuse, and volatile-argument admission keeps its normal flag.
            let outer = core::mem::replace(&mut self.aggregate_argument, true);
            let value = self.read_write_value(raw, 0);
            self.aggregate_argument = outer;
            let value = value?;
            let kind = value.column_type(&raw.types(), &self.syntax.parameters);
            self.capacity(
                inputs.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let column = inputs.len();
            inputs.push(ReadProjectionTemplate {
                name: format!("__write_arg_{column}"),
                value,
            });
            (Some(column), kind)
        };
        self.punct(b')', "one aggregate argument")?;
        // A stored property may contain a number, NULL or a composite value.
        // Admit the domain just as WITH aggregation does; the native reducer
        // still checks each actual value, including inputs behind LIMIT 0.
        if matches!(function, Function::SumInt | Function::AverageInt)
            && !input_kind.is_scalar_like()
            && input_kind != GraphSetColumnType::Any
        {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected("scalar input for a numeric aggregate"),
            )
            .into());
        }
        if distinct {
            function = match function {
                Function::Count => Function::CountDistinct,
                Function::SumInt => Function::SumIntDistinct,
                Function::AverageInt => Function::AverageIntDistinct,
                Function::Collect => Function::CollectDistinct,
                other => other,
            };
        }
        self.capacity(
            summaries.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let summary = summaries.len();
        summaries.push(ReadAggregateSpec {
            name: format!("__write_summary_{summary}"),
            function,
            column,
        });
        let kind = match function {
            Function::Min | Function::Max => input_kind,
            Function::Collect | Function::CollectDistinct => GraphSetColumnType::List,
            _ => GraphSetColumnType::Scalar,
        };
        Ok((summary, kind))
    }

    pub(super) fn combined_write_return_items(
        &mut self,
        raw: &mut impl ReturnLeaves<'a>,
    ) -> Result<ReturnItems<'a>, GraphSetTextError> {
        let mut inputs = Vec::new();
        let mut summaries = Vec::new();
        let mut items: Vec<Item<'a>> = Vec::new();
        let mut spellings = Vec::new();
        loop {
            self.capacity(
                items.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let at = self.current.at;
            let mut leaves = AggregateLeaves {
                raw: &mut *raw,
                inputs: &mut inputs,
                summaries: &mut summaries,
                leaves: Vec::new(),
                kinds: Vec::new(),
                failure: None,
                lifetime: core::marker::PhantomData,
            };
            let value = self.read_write_value(&mut leaves, 0);
            if let Some(failure) = leaves.failure {
                return Err(failure);
            }
            let value = value?;
            let kind = value.column_type(&leaves.kinds, &self.syntax.parameters);
            let operands = leaves.leaves;
            let end = self.current.at;
            let spelling = self.source_name(at, end);
            let name = if self.take_word("AS")? {
                self.name()?
            } else {
                spelling
            };
            if items.iter().any(|item| item.name.text == name.text) {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                )
                .into());
            }
            spellings.push(spelling);
            items.push(Item {
                at,
                name,
                value,
                leaves: operands,
                kind,
            });
            if !self.take(b',')? {
                break;
            }
        }
        let mut keys = Vec::new();
        let mut key_slots = Vec::new();
        for item in &mut items {
            if item
                .leaves
                .iter()
                .any(|leaf| matches!(leaf, Leaf::Summary(_)))
            {
                continue;
            }
            let columns: Vec<_> = item
                .leaves
                .iter()
                .map(|leaf| match *leaf {
                    Leaf::Binding(binding) => binding,
                    Leaf::Summary(_) => unreachable!("nonaggregate item"),
                })
                .collect();
            remap_output_columns(&mut item.value, &columns);
            self.capacity(
                inputs.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            if let ReadValueTemplate::Column(binding) = item.value {
                key_slots.push((binding, keys.len()));
            }
            keys.push(inputs.len());
            inputs.push(ReadProjectionTemplate {
                name: format!("__write_key_{}", keys.len() - 1),
                value: item.value.clone(),
            });
            item.value = ReadValueTemplate::Column(keys.len() - 1);
        }
        // Only explicitly projected nonaggregate keys may occur outside a
        // summary. In particular, no arbitrary representative row is chosen.
        for item in &mut items {
            if !item
                .leaves
                .iter()
                .any(|leaf| matches!(leaf, Leaf::Summary(_)))
            {
                continue;
            }
            let mut columns = Vec::new();
            for leaf in &item.leaves {
                columns.push(match *leaf {
                    Leaf::Summary(summary) => keys.len() + summary,
                    Leaf::Binding(binding) => key_slots
                        .iter()
                        .find(|&&(known, _)| known == binding)
                        .map(|&(_, key)| key)
                        .ok_or_else(|| {
                            error(
                                item.at,
                                GraphPatternTextErrorKind::Expected(
                                    "an explicit grouping key for every nonaggregate write operand",
                                ),
                            )
                        })?,
                });
            }
            remap_output_columns(&mut item.value, &columns);
        }
        if inputs.is_empty() {
            inputs.push(ReadProjectionTemplate {
                name: "__write_input_0".into(),
                value: ReadValueTemplate::Literal(
                    GqlScalarParameter::new(CanonicalScalar::Null).expect("canonical null"),
                ),
            });
        }
        let output = items.iter().map(|item| (item.name, item.kind)).collect();
        let projection = items
            .into_iter()
            .map(|item| ReadProjectionTemplate {
                name: item.name.text.to_owned(),
                value: item.value,
            })
            .collect();
        Ok(ReturnItems {
            projection,
            output,
            spellings,
            grouping: Some(WriteReturnGroupTemplate {
                inputs,
                keys,
                aggregates: summaries,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::algebra::{GraphValue, GraphValueRow};
    use crate::{
        GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
        GqlQueryPolicy, GraphEdgeReturnBinding, GraphSymbol, GraphSymbolKind,
        PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertQueryText,
    };
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalScalar, EId};

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }
    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn text(tail: &str) -> String {
        format!("MATCH (a),(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.p=r.p {tail}")
    }
    fn prepare(tail: &str) -> PreparedGraphEdgeUpsertQuery {
        PreparedGraphEdgeUpsertQueryText::prepare(&text(tail), RelationId(1), symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    }
    fn policy(rows: u64) -> GqlQueryPolicy {
        GqlQueryPolicy::new(1_000, rows, 1_000_000, 1_000_000)
    }
    fn inputs(
        query: &PreparedGraphEdgeUpsertQuery,
        values: &[Option<i64>],
    ) -> Vec<Vec<GraphValue>> {
        values
            .iter()
            .map(|value| {
                query
                    .bindings()
                    .iter()
                    .map(|binding| match binding {
                        GraphEdgeReturnBinding::Edge => GraphValue::Edge(EId(1)),
                        GraphEdgeReturnBinding::Property(key) => {
                            assert_eq!(*key, PropertyKeyId(1));
                            value.map_or(GraphValue::Scalar(CanonicalScalar::Null), int)
                        }
                    })
                    .collect()
            })
            .collect()
    }
    fn execute(
        query: &PreparedGraphEdgeUpsertQuery,
        values: &[Option<i64>],
    ) -> GqlQueryExecution<GraphValueRow> {
        query
            .project_rows_governed::<(), ()>(
                inputs(query, values),
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(100),
                || Ok(()),
            )
            .unwrap()
    }
    fn values(output: GqlQueryExecution<GraphValueRow>) -> Vec<Vec<GraphValue>> {
        output
            .value
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect()
    }

    #[test]
    fn compound_write_results_use_final_rows_for_predicates_arithmetic_and_collections() {
        let query = prepare(
            "RETURN count(*) > 0 AS any, sum(r.p)+count(*) AS total, size(collect(r.p)) AS n",
        );
        assert_eq!(query.columns(), &["any", "total", "n"]);
        assert_eq!(
            values(execute(&query, &[Some(4), Some(6)])),
            vec![vec![
                GraphValue::Scalar(CanonicalScalar::Bool(true)),
                int(12),
                int(2),
            ]]
        );
    }

    #[test]
    fn compound_grouped_write_items_use_only_explicit_keys() {
        let query = prepare("RETURN r.p AS key, r.p+count(*) AS mixed ORDER BY key");
        assert_eq!(
            values(execute(&query, &[Some(4), Some(4), Some(9)])),
            vec![vec![int(4), int(6)], vec![int(9), int(10)]]
        );
    }

    #[test]
    fn unaliased_compound_write_items_and_empty_global_groups_keep_native_values() {
        let query = prepare("RETURN coalesce(sum(r.p),0)+count(*)");
        assert_eq!(query.columns(), &["coalesce(sum(r.p),0)+count(*)"]);
        assert_eq!(values(execute(&query, &[])), vec![vec![int(0)]]);
        assert_eq!(values(execute(&query, &[Some(4)])), vec![vec![int(5)]]);
    }

    #[test]
    fn compound_distinct_aggregate_arguments_do_not_count_nulls_or_duplicates() {
        let query = prepare(
            "RETURN sum(DISTINCT r.p)+count(DISTINCT r.p) AS n, size(collect(DISTINCT r.p)) AS size",
        );
        assert_eq!(
            values(execute(&query, &[Some(4), Some(4), None])),
            vec![vec![int(5), int(1)]]
        );
    }

    #[test]
    fn compound_aggregate_case_keeps_lazy_post_group_evaluation() {
        let query =
            prepare("RETURN CASE WHEN count(*) > 0 THEN sum(r.p)/count(*) ELSE 0 END AS mean");
        assert_eq!(values(execute(&query, &[])), vec![vec![int(0)]]);
        assert_eq!(
            values(execute(&query, &[Some(4), Some(6)])),
            vec![vec![int(5)]]
        );
    }

    #[test]
    fn compound_write_output_failures_are_not_hidden_by_limit_zero() {
        let query = prepare("RETURN 12/(sum(r.p)-8) AS bad LIMIT 0");
        let result = query.project_rows_governed::<(), ()>(
            inputs(&query, &[Some(4), Some(4)]),
            GqlExecutionStats::default(),
            GlaExecutionStats::default(),
            policy(0),
            || Ok(()),
        );
        assert!(matches!(result, Err(GqlQueryError::Source(_))));
    }

    #[test]
    fn invalid_compound_write_shapes_refuse_before_catalog_callbacks() {
        for tail in [
            "RETURN r.p+count(*) AS invalid",
            "RETURN sum(count(r.p))+1 AS nested",
            "RETURN count(*)+1 AS n, sum(r.p) AS n",
            "RETURN count(DISTINCT *)+1 AS invalid",
        ] {
            let mut calls = 0;
            let result = PreparedGraphEdgeUpsertQueryText::prepare(
                &text(tail),
                RelationId(1),
                |kind, name: &str| {
                    calls += 1;
                    symbols(kind, name)
                },
            );
            assert!(result.is_err(), "{tail}");
            assert_eq!(calls, 0, "{tail}");
        }
    }

    #[test]
    fn compound_write_with_feeds_the_existing_native_tail() {
        let query = prepare("WITH sum(r.p)+count(*) AS total RETURN total*2 AS doubled");
        assert_eq!(
            values(execute(&query, &[Some(4), Some(6)])),
            vec![vec![int(24)]]
        );
    }
}
