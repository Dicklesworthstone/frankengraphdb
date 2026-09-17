//! Native bounded single-vertex MERGE and branch actions on the shared lexer.

use super::*;
use crate::insertion::{GraphInsertBuildError, GraphInsertVertex, PreparedGraphInsert};
use crate::mutation_text::VertexMergeValueTemplate;
use crate::vertex_upsert_text::{VertexUpsertActionTemplate, VertexUpsertValueTemplate};
use crate::{
    GraphMutationValue, GraphVertexMergeBuildError, GraphVertexMergeTextError,
    GraphVertexMergeTextErrorKind, GraphVertexUpsertAction, GraphVertexUpsertBranch,
    GraphVertexUpsertTextError, GraphVertexUpsertTextErrorKind, PreparedGraphVertexMerge,
    PreparedGraphVertexMergeText, PreparedGraphVertexUpsert, PreparedGraphVertexUpsertText,
};

struct MergeProperty<'a> {
    key: Name<'a>,
    filter: Number,
    value: VertexMergeValueTemplate,
}

enum ParsedUpsertAction<'a> {
    Property {
        key: Name<'a>,
        value: VertexUpsertValueTemplate,
    },
    Label {
        label: Name<'a>,
    },
}

/// Pins the action resolver's argument to one inferred source lifetime. A bare
/// closure parameter here has no type at its definition (E0282), and spelling it
/// with `'_` would make the closure higher-ranked over a lifetime the symbol
/// cache cannot outlive.
fn upsert_action_resolver<'s, F>(resolver: F) -> F
where
    F: FnMut(
        Vec<ParsedUpsertAction<'s>>,
    ) -> Result<Vec<VertexUpsertActionTemplate>, GraphPatternTextError>,
{
    resolver
}

fn insert_error(at: usize, source: GraphInsertBuildError) -> GraphVertexMergeTextError {
    GraphVertexMergeTextError {
        offset: at,
        kind: GraphVertexMergeTextErrorKind::InsertBuild(source),
    }
}
fn merge_error(at: usize, source: GraphVertexMergeBuildError) -> GraphVertexMergeTextError {
    GraphVertexMergeTextError {
        offset: at,
        kind: GraphVertexMergeTextErrorKind::MergeBuild(source),
    }
}
fn upsert_merge_error(error: GraphVertexMergeTextError) -> GraphVertexUpsertTextError {
    GraphVertexUpsertTextError {
        offset: error.offset,
        kind: GraphVertexUpsertTextErrorKind::Merge(error.kind),
    }
}

impl<'a> Parser<'a> {
    fn vertex_merge_pattern(
        &mut self,
    ) -> Result<(Name<'a>, Vec<Name<'a>>, Vec<MergeProperty<'a>>), GraphVertexMergeTextError> {
        self.word("MERGE")?;
        self.punct(b'(', "(")?;
        let variable = self.name()?;
        let mut labels = Vec::new();
        while self.take(b':')? {
            let label = self.name()?;
            if labels
                .iter()
                .any(|previous: &Name<'a>| previous.text == label.text)
            {
                return Err(insert_error(
                    label.at,
                    GraphInsertBuildError::DuplicateLabel { vertex: 0 },
                ));
            }
            labels.push(label);
        }
        let mut properties = Vec::new();
        if self.take(b'{')? {
            if self.is_punct(b'}') {
                return Err(error(
                    self.current.at,
                    GraphPatternTextErrorKind::Expected("at least one MERGE property"),
                )
                .into());
            }
            loop {
                if properties.len() >= crate::insertion::MAX_GRAPH_INSERT_FIELDS {
                    return Err(insert_error(
                        self.current.at,
                        GraphInsertBuildError::TooManyFields {
                            limit: crate::insertion::MAX_GRAPH_INSERT_FIELDS,
                            observed: properties.len() + 1,
                        },
                    ));
                }
                let key = self.name()?;
                if properties
                    .iter()
                    .any(|previous: &MergeProperty<'a>| previous.key.text == key.text)
                {
                    return Err(insert_error(
                        key.at,
                        GraphInsertBuildError::DuplicateProperty { declaration: 0 },
                    ));
                }
                self.punct(b':', ":")?;
                let at = self.current.at;
                let operand = self.mutation_operand(&mut Vec::new())?;
                let (filter, value) = match operand {
                    Operand::Literal(value) => {
                        if matches!(value.value(), CanonicalScalar::Null) {
                            return Err(error(
                                at,
                                GraphPatternTextErrorKind::Expected(
                                    "non-null MERGE property value",
                                ),
                            )
                            .into());
                        }
                        (
                            Number::Literal(GqlParameterValue::Scalar(value.clone())),
                            VertexMergeValueTemplate::Bound(value),
                        )
                    }
                    Operand::Number(Number::Literal(value)) => {
                        let scalar = scalar(value.clone(), at)?;
                        if matches!(scalar.value(), CanonicalScalar::Null) {
                            return Err(error(
                                at,
                                GraphPatternTextErrorKind::Expected(
                                    "non-null MERGE property value",
                                ),
                            )
                            .into());
                        }
                        (
                            Number::Literal(value),
                            VertexMergeValueTemplate::Bound(scalar),
                        )
                    }
                    Operand::Number(Number::Parameter(index)) => (
                        Number::Parameter(index),
                        VertexMergeValueTemplate::Parameter { index, at },
                    ),
                    Operand::Column(_) | Operand::Integer { .. } => {
                        return Err(error(
                            at,
                            GraphPatternTextErrorKind::Expected(
                                "scalar literal or parameter MERGE property value",
                            ),
                        )
                        .into());
                    }
                };
                properties.push(MergeProperty { key, filter, value });
                if !self.take(b',')? {
                    break;
                }
            }
            self.punct(b'}', "}")?;
        }
        self.punct(b')', ")")?;
        Ok((variable, labels, properties))
    }

    fn upsert_branch_actions(
        &mut self,
        variable: Name<'a>,
    ) -> Result<
        (Vec<ParsedUpsertAction<'a>>, Vec<ParsedUpsertAction<'a>>),
        GraphVertexUpsertTextError,
    > {
        let mut on_match = Vec::new();
        let mut on_create = Vec::new();
        let mut saw_match = false;
        let mut saw_create = false;
        while self.take_word("ON")? {
            let branch_at = self.current.at;
            let create = if self.take_word("MATCH")? {
                if saw_match {
                    return Err(GraphVertexUpsertTextError {
                        offset: branch_at,
                        kind: GraphVertexUpsertTextErrorKind::DuplicateBranch,
                    });
                }
                saw_match = true;
                false
            } else if self.take_word("CREATE")? {
                if saw_create {
                    return Err(GraphVertexUpsertTextError {
                        offset: branch_at,
                        kind: GraphVertexUpsertTextErrorKind::DuplicateBranch,
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
                if target.len() >= crate::MAX_GRAPH_VERTEX_UPSERT_ACTIONS {
                    return Err(GraphVertexUpsertTextError {
                        offset: self.current.at,
                        kind: GraphVertexUpsertTextErrorKind::UpsertBuild(
                            crate::GraphVertexUpsertBuildError::TooManyActions {
                                branch: if create {
                                    GraphVertexUpsertBranch::Create
                                } else {
                                    GraphVertexUpsertBranch::Match
                                },
                                limit: crate::MAX_GRAPH_VERTEX_UPSERT_ACTIONS,
                                observed: target.len() + 1,
                            },
                        ),
                    });
                }
                let actual = self.name()?;
                if actual.text != variable.text {
                    return Err(error(
                        actual.at,
                        GraphPatternTextErrorKind::Expected("the MERGE vertex variable"),
                    )
                    .into());
                }
                let action = if self.take(b':')? {
                    ParsedUpsertAction::Label {
                        label: self.name()?,
                    }
                } else {
                    self.punct(b'.', "property or label assignment")?;
                    let key = self.name()?;
                    self.punct(b'=', "=")?;
                    let at = self.current.at;
                    let value = match self.mutation_operand(&mut Vec::new())? {
                        Operand::Literal(value) => VertexUpsertValueTemplate::Bound(value),
                        Operand::Number(Number::Literal(value)) => {
                            VertexUpsertValueTemplate::Bound(scalar(value, at)?)
                        }
                        Operand::Number(Number::Parameter(index)) => {
                            VertexUpsertValueTemplate::Parameter { index, at }
                        }
                        Operand::Column(_) | Operand::Integer { .. } => {
                            return Err(error(
                                at,
                                GraphPatternTextErrorKind::Expected(
                                    "scalar literal or parameter branch assignment",
                                ),
                            )
                            .into());
                        }
                    };
                    ParsedUpsertAction::Property { key, value }
                };
                target.push(action);
                if !self.take(b',')? {
                    break;
                }
            }
        }
        Ok((on_match, on_create))
    }
}

fn resolve_merge_template<'a>(
    statement: &str,
    relation: RelationId,
    syntax: Syntax<'a>,
    variable: Name<'a>,
    parsed_labels: Vec<Name<'a>>,
    parsed_properties: Vec<MergeProperty<'a>>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<PreparedGraphVertexMergeText, GraphVertexMergeTextError> {
    let at = statement.len();
    let mut builder = GraphPatternBuilder::new();
    built(variable.at, builder.vertex(variable.text))?;
    let mut labels = Vec::new();
    for label in parsed_labels {
        let GraphSymbol::Label(label_id) = symbol(GraphSymbolKind::Label, label)? else {
            unreachable!("symbol domain checked by shared resolver")
        };
        built(
            label.at,
            builder.filter(variable.text, VertexPredicate::HasLabel(label_id)),
        )?;
        labels.push(label_id);
    }
    let mut filters = Vec::new();
    let mut properties = Vec::new();
    for property in parsed_properties {
        let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, property.key)? else {
            unreachable!("symbol domain checked by shared resolver")
        };
        filters.push(BoundFilter::Property {
            variable: variable.text.to_owned(),
            key,
            comparison: IntegerComparison::Equal,
            value: property.filter,
        });
        properties.push((key, property.value));
    }
    labels.sort_unstable();
    properties.sort_by_key(|(key, _)| *key);
    let columns = vec![BoundColumn {
        alias: "_merge_target".to_owned(),
        variable: variable.text.to_owned(),
        key: None,
        path: None,
    }];
    let projected = columns
        .iter()
        .map(BoundColumn::declaration)
        .collect::<Vec<_>>();
    built(at, builder.prepare_values(&projected, 0, None))?;
    let selection = PreparedGraphText {
        statement: statement.to_owned(),
        builder,
        filters,
        scopes: Vec::new(),
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
    Ok(PreparedGraphVertexMergeText {
        selection,
        relation,
        labels,
        properties,
    })
}

fn bind_merge_values(
    template: &PreparedGraphVertexMergeText,
    values: &[GqlParameterValue],
) -> Result<PreparedGraphVertexMerge, GraphVertexMergeTextError> {
    let mut fields = Vec::new();
    for (key, value_template) in &template.properties {
        let value = match value_template {
            VertexMergeValueTemplate::Bound(value) => value.clone(),
            VertexMergeValueTemplate::Parameter { index, at } => {
                let value = scalar(values[*index].clone(), *at)?;
                if matches!(value.value(), CanonicalScalar::Null) {
                    return Err(error(
                        *at,
                        GraphPatternTextErrorKind::Expected("non-null MERGE property value"),
                    )
                    .into());
                }
                value
            }
        };
        fields.push((*key, GraphMutationValue::Literal(value)));
    }
    let selection = template.selection.bind_values(values)?;
    let creation = PreparedGraphInsert::prepare_standalone(
        template.relation,
        vec![GraphInsertVertex {
            labels: template.labels.clone(),
            properties: fields,
        }],
        Vec::new(),
    )
    .map_err(|source| GraphVertexMergeTextError {
        offset: template.selection.return_at,
        kind: GraphVertexMergeTextErrorKind::InsertBuild(source),
    })?;
    PreparedGraphVertexMerge::prepare(selection, template.relation, 0, creation)
        .map_err(|source| merge_error(template.selection.return_at, source))
}

impl PreparedGraphVertexMergeText {
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphVertexMergeTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphVertexMergeTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        let (variable, labels, properties) = parser.vertex_merge_pattern()?;
        parser.end()?;
        let syntax = parser.syntax;
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
        resolve_merge_template(
            statement,
            relation,
            syntax,
            variable,
            labels,
            properties,
            &mut symbol,
        )
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        self.selection.statement()
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.selection.parameter_schema()
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphVertexMerge, GraphVertexMergeTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        bind_merge_values(self, &values)
    }
}

impl PreparedGraphVertexUpsertText {
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphVertexUpsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphVertexUpsertTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        let (variable, labels, properties) =
            parser.vertex_merge_pattern().map_err(upsert_merge_error)?;
        let (parsed_match, parsed_create) = parser.upsert_branch_actions(variable)?;
        if parsed_match.is_empty() && parsed_create.is_empty() {
            return Err(error(
                statement.len(),
                GraphPatternTextErrorKind::Expected("ON MATCH SET or ON CREATE SET"),
            )
            .into());
        }
        parser.end()?;
        let syntax = parser.syntax;
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
        let mut resolve_actions = upsert_action_resolver(|parsed| {
            parsed
                .into_iter()
                .map(|action| {
                    Ok(match action {
                        ParsedUpsertAction::Property { key, value } => {
                            let GraphSymbol::Property(key) =
                                symbol(GraphSymbolKind::Property, key)?
                            else {
                                unreachable!("symbol domain checked")
                            };
                            VertexUpsertActionTemplate::Property { key, value }
                        }
                        ParsedUpsertAction::Label { label } => {
                            let GraphSymbol::Label(label) = symbol(GraphSymbolKind::Label, label)?
                            else {
                                unreachable!("symbol domain checked")
                            };
                            VertexUpsertActionTemplate::Label {
                                label,
                                present: true,
                            }
                        }
                    })
                })
                .collect()
        });
        let on_match = resolve_actions(parsed_match)?;
        let on_create = resolve_actions(parsed_create)?;
        drop(resolve_actions);
        let merge = resolve_merge_template(
            statement,
            relation,
            syntax,
            variable,
            labels,
            properties,
            &mut symbol,
        )
        .map_err(upsert_merge_error)?;
        Ok(Self {
            merge,
            on_match,
            on_create,
        })
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphVertexUpsert, GraphVertexUpsertTextError> {
        let values = self.merge.selection.checked_arguments(arguments)?;
        let merge = bind_merge_values(&self.merge, &values).map_err(upsert_merge_error)?;
        let bind_actions = |templates: &[VertexUpsertActionTemplate]| -> Result<Vec<GraphVertexUpsertAction>, GraphVertexUpsertTextError> {
            templates.iter().map(|action| Ok(match action {
                VertexUpsertActionTemplate::Property { key, value } => {
                    let value = match value {
                        VertexUpsertValueTemplate::Bound(value) => value.clone(),
                        VertexUpsertValueTemplate::Parameter { index, at } =>
                            scalar(values[*index].clone(), *at)?,
                    };
                    GraphVertexUpsertAction::SetProperty { key: *key, value }
                }
                VertexUpsertActionTemplate::Label { label, present } =>
                    GraphVertexUpsertAction::SetLabel { label: *label, present: *present },
            })).collect()
        };
        let on_match = bind_actions(&self.on_match)?;
        let on_create = bind_actions(&self.on_create)?;
        PreparedGraphVertexUpsert::prepare(merge, on_match, on_create).map_err(|source| {
            GraphVertexUpsertTextError {
                offset: self.merge.selection.return_at,
                kind: GraphVertexUpsertTextErrorKind::UpsertBuild(source),
            }
        })
    }
}
