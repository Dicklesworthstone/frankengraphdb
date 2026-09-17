//! Native directed relationship MERGE branch actions over bound MATCH endpoints.

use super::*;
use crate::edge_upsert_text::{EdgeUpsertActionTemplate, EdgeUpsertValueTemplate};
use crate::{
    GraphEdgeMergeTextError, GraphEdgeUpsertAction,
    GraphEdgeUpsertBranch, GraphEdgeUpsertTextError, GraphEdgeUpsertTextErrorKind,
    PreparedGraphEdgeMergeText, PreparedGraphEdgeUpsert, PreparedGraphEdgeUpsertText,
};

struct ParsedEdgeUpsert<'a> {
    source: Name<'a>,
    destination: Name<'a>,
    relation: Name<'a>,
    on_match: Vec<(Name<'a>, EdgeUpsertValueTemplate)>,
    on_create: Vec<(Name<'a>, EdgeUpsertValueTemplate)>,
}
fn merge_error(error: GraphEdgeMergeTextError) -> GraphEdgeUpsertTextError {
    GraphEdgeUpsertTextError {
        offset: error.offset,
        kind: GraphEdgeUpsertTextErrorKind::Merge(error.kind),
    }
}

impl<'a> Parser<'a> {
    fn edge_upsert_pattern(&mut self) -> Result<ParsedEdgeUpsert<'a>, GraphEdgeUpsertTextError> {
        self.word("MERGE")?;
        self.punct(b'(', "(")?;
        let left = self.variable()?;
        self.punct(b')', ")")?;
        let incoming = self.take(b'<')?;
        self.punct(b'-', "-")?;
        self.punct(b'[', "[")?;
        let relationship = self.name()?;
        if self
            .syntax
            .variables
            .iter()
            .any(|variable| variable.text == relationship.text)
        {
            return Err(error(
                relationship.at,
                GraphPatternTextErrorKind::Expected(
                    "a relationship variable distinct from vertex variables",
                ),
            )
            .into());
        }
        self.punct(b':', ":")?;
        let relation = self.name()?;
        self.punct(b']', "]")?;
        self.punct(b'-', "-")?;
        let outgoing = self.take(b'>')?;
        if incoming == outgoing {
            return Err(error(
                relation.at,
                GraphPatternTextErrorKind::Expected("exactly one relationship direction"),
            )
            .into());
        }
        self.punct(b'(', "(")?;
        let right = self.variable()?;
        self.punct(b')', ")")?;

        let mut on_match = Vec::new();
        let mut on_create = Vec::new();
        let mut saw_match = false;
        let mut saw_create = false;
        while self.take_word("ON")? {
            let branch_at = self.current.at;
            let create = if self.take_word("MATCH")? {
                if saw_match {
                    return Err(GraphEdgeUpsertTextError {
                        offset: branch_at,
                        kind: GraphEdgeUpsertTextErrorKind::DuplicateBranch,
                    });
                }
                saw_match = true;
                false
            } else if self.take_word("CREATE")? {
                if saw_create {
                    return Err(GraphEdgeUpsertTextError {
                        offset: branch_at,
                        kind: GraphEdgeUpsertTextErrorKind::DuplicateBranch,
                    });
                }
                saw_create = true;
                true
            } else {
                return Err(error(
                    branch_at,
                    GraphPatternTextErrorKind::Expected("MATCH or CREATE after ON"),
                )
                .into());
            };
            self.word("SET")?;
            let target = if create {
                &mut on_create
            } else {
                &mut on_match
            };
            loop {
                if target.len() >= crate::MAX_GRAPH_EDGE_UPSERT_ACTIONS {
                    return Err(GraphEdgeUpsertTextError {
                        offset: self.current.at,
                        kind: GraphEdgeUpsertTextErrorKind::UpsertBuild(
                            crate::GraphEdgeUpsertBuildError::TooManyActions {
                                branch: if create {
                                    GraphEdgeUpsertBranch::Create
                                } else {
                                    GraphEdgeUpsertBranch::Match
                                },
                                limit: crate::MAX_GRAPH_EDGE_UPSERT_ACTIONS,
                                observed: target.len() + 1,
                            },
                        ),
                    });
                }
                let actual = self.name()?;
                if actual.text != relationship.text {
                    return Err(error(
                        actual.at,
                        GraphPatternTextErrorKind::Expected("the MERGE relationship variable"),
                    )
                    .into());
                }
                self.punct(b'.', "relationship property assignment")?;
                let key = self.name()?;
                self.punct(b'=', "=")?;
                let at = self.current.at;
                let value = match self.mutation_operand(&mut Vec::new())? {
                    Operand::Literal(value) => EdgeUpsertValueTemplate::Bound(value),
                    Operand::Number(Number::Literal(value)) => {
                        EdgeUpsertValueTemplate::Bound(scalar(value, at)?)
                    }
                    Operand::Number(Number::Parameter(index)) => {
                        EdgeUpsertValueTemplate::Parameter { index, at }
                    }
                    Operand::Column(_) | Operand::Integer { .. } => {
                        return Err(error(
                            at,
                            GraphPatternTextErrorKind::Expected(
                                "scalar literal or parameter relationship assignment",
                            ),
                        )
                        .into());
                    }
                };
                target.push((key, value));
                if !self.take(b',')? {
                    break;
                }
            }
        }
        if on_match.is_empty() && on_create.is_empty() {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected("ON MATCH SET or ON CREATE SET"),
            )
            .into());
        }
        self.end()?;
        let (source, destination) = if incoming {
            (right, left)
        } else {
            (left, right)
        };
        Ok(ParsedEdgeUpsert {
            source,
            destination,
            relation,
            on_match,
            on_create,
        })
    }
}

impl PreparedGraphEdgeUpsertText {
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeUpsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        _relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeUpsertTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        parser.parse_match_prefix()?;
        let parsed = parser.edge_upsert_pattern()?;
        let mut projections = Vec::new();
        let source = parser.mutation_projection(&mut projections, parsed.source, None)?;
        let destination = parser.mutation_projection(&mut projections, parsed.destination, None)?;
        let syntax = parser.syntax;
        let at = syntax.return_at;

        let mut cache = BTreeMap::new();
        let mut symbol = |kind, name: Name<'_>| -> Result<GraphSymbol, GraphPatternTextError> {
            let key = (kind, name.text.to_owned());
            if let Some(value) = cache.get(&key) {
                return Ok(*value);
            }
            let value = resolve(kind, name.text)
                .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
            if value.kind() != kind {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::WrongSymbolKind {
                        expected: kind,
                        found: value.kind(),
                    },
                ));
            }
            cache.insert(key, value);
            Ok(value)
        };
        let (builder, filters) = resolve_pattern(
            &syntax.variables[..syntax.root_variables],
            &syntax.labels,
            &syntax.edges,
            syntax.filters,
            &mut symbol,
        )?;
        let mut scopes = Vec::new();
        for scope in syntax.scopes {
            scopes.push(scope.resolve(&mut symbol)?);
        }
        let GraphSymbol::Relation(found) = symbol(GraphSymbolKind::Relation, parsed.relation)?
        else {
            unreachable!("symbol domain checked")
        };
        let mut resolve_actions = |parsed: Vec<(Name<'_>, EdgeUpsertValueTemplate)>| -> Result<
            Vec<EdgeUpsertActionTemplate>,
            GraphPatternTextError,
        > {
            parsed
                .into_iter()
                .map(|(key, value)| {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                        unreachable!("symbol domain checked")
                    };
                    Ok(EdgeUpsertActionTemplate { key, value })
                })
                .collect()
        };
        let on_match = resolve_actions(parsed.on_match)?;
        let on_create = resolve_actions(parsed.on_create)?;

        let columns = projections
            .into_iter()
            .enumerate()
            .map(|(index, projection)| BoundColumn {
                alias: format!("_edge_upsert_{index}"),
                variable: projection.variable.text.to_owned(),
                key: None,
                path: None,
            })
            .collect::<Vec<_>>();
        let clauses = scopes.iter().map(BoundScope::clause).collect::<Vec<_>>();
        let projected = columns
            .iter()
            .map(BoundColumn::declaration)
            .collect::<Vec<_>>();
        built(
            at,
            builder.prepare_values_with_clauses(&clauses, &projected, 0, None),
        )?;
        let selection = PreparedGraphText {
            statement: statement.to_owned(),
            builder,
            filters,
            scopes,
            columns,
            ordering: Vec::new(),
            parameters: syntax.parameters,
            parameter_offsets: syntax.parameter_offsets,
            offset: Number::Literal(GqlParameterValue::UInt64(0)),
            count: None,
            distinct: false,
            visible_columns: None,
            return_at: at,
        };
        let merge = PreparedGraphEdgeMergeText {
            selection,
            relation: found,
            source,
            destination,
        };
        Ok(Self {
            merge,
            on_match,
            on_create,
        })
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphEdgeUpsert, GraphEdgeUpsertTextError> {
        let values = self.merge.selection.checked_arguments(arguments)?;
        let merge = self.merge.bind_parameters(arguments).map_err(merge_error)?;
        let bind = |templates: &[EdgeUpsertActionTemplate]| -> Result<Vec<GraphEdgeUpsertAction>, GraphEdgeUpsertTextError> {
            templates.iter().map(|action| {
                let value = match &action.value {
                    EdgeUpsertValueTemplate::Bound(value) => value.clone(),
                    EdgeUpsertValueTemplate::Parameter { index, at } => scalar(values[*index].clone(), *at)?,
                };
                Ok(GraphEdgeUpsertAction { key: action.key, value })
            }).collect()
        };
        let on_match = bind(&self.on_match)?;
        let on_create = bind(&self.on_create)?;
        PreparedGraphEdgeUpsert::prepare(merge, on_match, on_create).map_err(|source| {
            GraphEdgeUpsertTextError {
                offset: self.merge.selection.return_at,
                kind: GraphEdgeUpsertTextErrorKind::UpsertBuild(source),
            }
        })
    }
}
