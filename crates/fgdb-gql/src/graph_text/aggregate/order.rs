//! Post-group ordering shares RETURN's expression compiler. Its private rank
//! tuple reads the evaluation row and already computed visible aliases, and
//! never becomes a grouping key or a visible DISTINCT cell.

use super::*;

enum Leaf<'a> {
    Source(Expression<'a>),
    Column(ResultOrderColumn),
}

impl<'a> Parser<'a> {
    pub(super) fn aggregate_order_expression(
        &mut self,
        returned: &[ReturnItem<'a>],
        groups: &[Expression<'a>],
        hidden: &mut Vec<HiddenSummary<'a>>,
        computed: &mut ComputedInputs<'a>,
        expressions: &mut Vec<(ReadValueTemplate, Vec<ResultOrderColumn>)>,
    ) -> Result<ResultOrderColumn, GraphPatternTextError> {
        let at = self.current.at;
        let mut leaves = Vec::new();
        let value = self
            .read_resolved_item(
                &mut |parser| {
                    if matches!(parser.current.kind, TokenKind::Word(word)
                        if parser.elements.contains(&word))
                        && !matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
                    {
                        return Ok(None);
                    }
                    let leaf = if core::mem::take(&mut parser.percentile_leaf) {
                        let at = parser.current.at;
                        let expression = Some(parser.aggregate_argument_expression(computed)?);
                        let function = GraphAggregateFunction::Collect;
                        let visible = returned.iter().filter(|item| item.function.is_some());
                        let index = if let Some(index) = visible.clone().position(|item| {
                            item.function == Some(function)
                                && same_expression(item.expression, expression)
                        }) {
                            index
                        } else {
                            let prefix = visible.count();
                            if let Some(index) = hidden.iter().position(|item| {
                                item.function == function
                                    && same_expression(item.expression, expression)
                            }) {
                                prefix + index
                            } else {
                                parser.capacity(
                                    groups.len() + prefix + hidden.len(),
                                    MAX_PATTERN_VERTICES,
                                    crate::algebra::PatternLimitDimension::Columns,
                                )?;
                                let index = prefix + hidden.len();
                                hidden.push(HiddenSummary {
                                    expression,
                                    function,
                                    at,
                                });
                                index
                            }
                        };
                        Leaf::Column(ResultOrderColumn::Evaluation(
                            GraphAggregateColumn::Aggregate(index),
                        ))
                    } else if parser.starts_aggregate_call()? {
                        Leaf::Column(ResultOrderColumn::Evaluation(
                            parser.result_column(returned, groups, hidden, computed)?,
                        ))
                    } else if let TokenKind::Word(word) = parser.current.kind
                        && !matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
                        && !parser.aggregate_order_repeats_group_property(groups)?
                        && let Some(index) =
                            returned.iter().position(|item| item.alias.text == word)
                    {
                        let item = &returned[index];
                        // Consume only the alias: the shared value reader owns
                        // its property/subscript postfix. A repeated grouped
                        // property retains its established input meaning;
                        // other postfix reads address the visible alias.
                        parser.advance()?;
                        let column = if item.output.is_some() {
                            ResultOrderColumn::Projected(index)
                        } else if item.function.is_some() {
                            ResultOrderColumn::Evaluation(GraphAggregateColumn::Aggregate(
                                returned[..index]
                                    .iter()
                                    .filter(|item| item.function.is_some())
                                    .count(),
                            ))
                        } else {
                            let expression = item.expression.expect("plain RETURN key");
                            ResultOrderColumn::Evaluation(GraphAggregateColumn::GroupKey(
                                groups
                                    .iter()
                                    .position(|group| group.same(expression))
                                    .expect("returned grouping key checked above"),
                            ))
                        };
                        Leaf::Column(column)
                    } else if let Some(source) = parser.aggregate_order_source()? {
                        Leaf::Source(source)
                    } else {
                        return Ok(None);
                    };
                    parser.capacity(
                        leaves.len(),
                        crate::MAX_GRAPH_INTEGER_INSTRUCTIONS,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    let index = leaves.len();
                    leaves.push(leaf);
                    Ok(Some(index))
                },
                0,
            )
            .map_err(|source| {
                let kind = match source.kind {
                    crate::GraphSetTextErrorKind::Pattern(kind) => kind,
                    _ => GraphPatternTextErrorKind::Expected(
                        "valid bounded aggregate ORDER BY expression",
                    ),
                };
                error(source.offset, kind)
            })?;

        // Repeating a complete computed grouping key remains the same key,
        // including its exact existing transcript. Compare compiled programs,
        // not source spelling, so parentheses and explicit parameter uses keep
        // their existing meaning. A source occurring only within such a key
        // does not independently become an admitted grouping leaf.
        let source_columns: Option<Vec<_>> = leaves
            .iter()
            .map(|leaf| match leaf {
                Leaf::Source(source) => {
                    computed
                        .sources
                        .iter()
                        .position(|&(variable, property, path)| {
                            source.same(Expression {
                                variable,
                                property,
                                path,
                                computed: None,
                            })
                        })
                }
                Leaf::Column(_) => None,
            })
            .collect();
        if let Some(columns) = source_columns {
            let mut input = value.clone();
            remap_output_columns(&mut input, &columns);
            let mut identity = Vec::new();
            input.append_template_transcript(&mut identity);
            for (key, group) in groups.iter().enumerate() {
                if let Some(index) = group.computed {
                    let mut previous = Vec::new();
                    computed.operands[index].append_template_transcript(&mut previous);
                    if identity == previous {
                        return Ok(ResultOrderColumn::Evaluation(
                            GraphAggregateColumn::GroupKey(key),
                        ));
                    }
                }
            }
        }
        let columns = leaves
            .into_iter()
            .map(|leaf| match leaf {
                Leaf::Column(column) => Ok(column),
                Leaf::Source(source) => groups
                    .iter()
                    .position(|group| group.same(source))
                    .map(|key| ResultOrderColumn::Evaluation(GraphAggregateColumn::GroupKey(key)))
                    .ok_or_else(|| {
                        error(
                            at,
                            GraphPatternTextErrorKind::Expected(
                                "grouping key in aggregate ORDER BY expression",
                            ),
                        )
                    }),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let ReadValueTemplate::Column(index) = value {
            return Ok(columns[index]);
        }
        let index = expressions.len();
        expressions.push((value, columns));
        Ok(ResultOrderColumn::Computed(index))
    }

    fn aggregate_order_repeats_group_property(
        &self,
        groups: &[Expression<'a>],
    ) -> Result<bool, GraphPatternTextError> {
        let TokenKind::Word(variable) = self.current.kind else {
            return Ok(false);
        };
        let mut lexer = self.lexer.clone();
        if !matches!(lexer.next()?.kind, TokenKind::Punct(b'.')) {
            return Ok(false);
        }
        let (TokenKind::Word(property) | TokenKind::DelimitedKeyword(property)) =
            lexer.next()?.kind
        else {
            return Ok(false);
        };
        Ok(groups.iter().any(|group| {
            group.computed.is_none()
                && group.variable.text == variable
                && group.property.is_some_and(|name| name.text == property)
        }))
    }

    fn aggregate_order_source(&mut self) -> Result<Option<Expression<'a>>, GraphPatternTextError> {
        // Identity/path functions with a single graph binding are one source,
        // just as they are in grouping-key admission. The lookahead never
        // consumes aliases or row functions that belong to the value reader.
        if let TokenKind::Word(word) = self.current.kind
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
            && let Ok(function) = if word.eq_ignore_ascii_case("KEYS") {
                Ok(GraphPathFunction::Properties)
            } else {
                Self::path_function(Name {
                    text: word,
                    at: self.current.at,
                })
            }
        {
            let mut lookahead = self.lexer.clone();
            lookahead.next()?;
            let variable = lookahead.next()?;
            if matches!(variable.kind, TokenKind::Word(word)
                if self.syntax.variables.iter().any(|name| name.text == word)
                    || self.syntax.visible_edge(word).is_some()
                    || self.syntax.names_path(word))
                && matches!(lookahead.next()?.kind, TokenKind::Punct(b')'))
            {
                self.advance()?;
                self.punct(b'(', "(")?;
                let variable = match function {
                    GraphPathFunction::Labels => self.vertex_variable()?,
                    GraphPathFunction::Type => self.edge_variable()?,
                    GraphPathFunction::Properties => self.property_variable()?,
                    _ => self.path_variable()?,
                };
                self.punct(b')', ")")?;
                return Ok(Some(Expression {
                    variable,
                    property: None,
                    path: Some(function),
                    computed: None,
                }));
            }
            return Ok(None);
        }
        if matches!(self.current.kind, TokenKind::Word(word)
            if self.syntax.visible_edge(word).is_some_and(|(edge, _)| edge.walk.is_none()))
            && !matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let variable = self.edge_variable()?;
            let property = if self.take(b'.')? {
                Some(self.schema_name()?)
            } else {
                None
            };
            return Ok(Some(Expression {
                variable,
                property,
                path: column_path(&self.syntax, variable),
                computed: None,
            }));
        }
        if matches!(self.current.kind, TokenKind::Word(word)
            if self.syntax.variables.iter().any(|variable| variable.text == word)
                || self.syntax.names_path(word))
        {
            let variable = self.any_variable()?;
            let property = if self.take(b'.')? {
                Some(self.schema_name()?)
            } else {
                None
            };
            return Ok(Some(Expression {
                variable,
                property,
                path: column_path(&self.syntax, variable),
                computed: None,
            }));
        }
        Ok(None)
    }
}
