//! Native bounded single-vertex MERGE and branch actions on the shared lexer.

use super::*;
use crate::insertion::{GraphInsertBuildError, GraphInsertVertex, PreparedGraphInsert};
use crate::mutation_text::VertexMergeValueTemplate;
use crate::set_text::{
    ReadPageNumber, ReadProjectionTemplate, ReadStageTemplate, ReadValueTemplate,
};
use crate::vertex_upsert_text::{
    PreparedGraphVertexUpsertQueryText, VertexReturnTemplate, VertexUpsertActionTemplate,
    VertexUpsertValueTemplate,
};
use crate::{
    GraphMutationValue, GraphSetColumnType, GraphSetProjection, GraphSetQuantifier,
    GraphVertexMergeBuildError, GraphVertexMergeTextError, GraphVertexMergeTextErrorKind,
    GraphVertexReturnBinding, GraphVertexUpsertAction, GraphVertexUpsertBranch,
    GraphVertexUpsertTextError, GraphVertexUpsertTextErrorKind, PreparedGraphVertexMerge,
    PreparedGraphVertexMergeText, PreparedGraphVertexUpsert, PreparedGraphVertexUpsertQuery,
    PreparedGraphVertexUpsertText,
};
use mutation_query::ReturnLeaves;

struct MergeProperty<'a> {
    key: Name<'a>,
    filter: Number,
    value: VertexMergeValueTemplate,
}

#[derive(Clone)]
enum ParsedUpsertAction<'a> {
    Property {
        key: Name<'a>,
        value: VertexUpsertValueTemplate,
    },
    Expression {
        key: Name<'a>,
        properties: Vec<Name<'a>>,
        program: Vec<MutationIntegerTemplateOp>,
        at: usize,
    },
    Label {
        label: Name<'a>,
    },
}

type ParsedUpsertClauses<'a> = (
    Vec<ParsedUpsertAction<'a>>,
    Vec<ParsedUpsertAction<'a>>,
    Vec<ParsedUpsertAction<'a>>,
);

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
    ) -> Result<ParsedUpsertClauses<'a>, GraphVertexUpsertTextError> {
        let mut on_match = Vec::new();
        let mut on_create = Vec::new();
        let mut after = Vec::new();
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
            let (target, branch) = if create {
                (&mut on_create, GraphVertexUpsertBranch::Create)
            } else {
                (&mut on_match, GraphVertexUpsertBranch::Match)
            };
            self.upsert_assignments(variable, target, branch)?;
        }
        // The common SET is a distinct clause, not a deduplicated extension
        // of both branches. It sees prior effects and cannot erase an earlier
        // failure or unauthorized write just by overwriting the same target.
        if self.take_word("SET")? {
            self.upsert_assignments(variable, &mut after, GraphVertexUpsertBranch::Match)?;
        }
        for (branch, actions) in [
            (GraphVertexUpsertBranch::Match, &on_match),
            (GraphVertexUpsertBranch::Create, &on_create),
        ] {
            let observed = actions.len() + after.len();
            if observed > crate::MAX_GRAPH_VERTEX_UPSERT_ACTIONS {
                return Err(GraphVertexUpsertTextError {
                    offset: self.current.at,
                    kind: GraphVertexUpsertTextErrorKind::UpsertBuild(
                        crate::GraphVertexUpsertBuildError::TooManyActions {
                            branch,
                            limit: crate::MAX_GRAPH_VERTEX_UPSERT_ACTIONS,
                            observed,
                        },
                    ),
                });
            }
        }
        Ok((on_match, on_create, after))
    }

    /// One simultaneous SET clause. The native scalar compiler owns operator
    /// precedence, lazy CASE/COALESCE, limits and parameter type admission.
    fn upsert_assignments(
        &mut self,
        variable: Name<'a>,
        target: &mut Vec<ParsedUpsertAction<'a>>,
        branch: GraphVertexUpsertBranch,
    ) -> Result<(), GraphVertexUpsertTextError> {
        loop {
            if target.len() >= crate::MAX_GRAPH_VERTEX_UPSERT_ACTIONS {
                return Err(GraphVertexUpsertTextError {
                    offset: self.current.at,
                    kind: GraphVertexUpsertTextErrorKind::UpsertBuild(
                        crate::GraphVertexUpsertBuildError::TooManyActions {
                            branch,
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
                let mut properties: Vec<Name<'a>> = Vec::new();
                let operand = self.resolved_predicate(&mut |parser| {
                    if !matches!(parser.current.kind, TokenKind::Word(_))
                        || !matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
                    {
                        return Ok(None);
                    }
                    let source = parser.name()?;
                    if source.text != variable.text {
                        return Err(error(
                            source.at,
                            GraphPatternTextErrorKind::Expected("the MERGE vertex variable"),
                        ));
                    }
                    parser.punct(b'.', ".")?;
                    let property = parser.name()?;
                    if let Some(column) = properties.iter().position(|p| p.text == property.text) {
                        return Ok(Some(column));
                    }
                    parser.capacity(
                        properties.len(),
                        MAX_PATTERN_VERTICES,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    let column = properties.len();
                    properties.push(property);
                    Ok(Some(column))
                })?;
                match operand {
                    Operand::Literal(value) => ParsedUpsertAction::Property {
                        key,
                        value: VertexUpsertValueTemplate::Bound(value),
                    },
                    Operand::Number(Number::Literal(value)) => ParsedUpsertAction::Property {
                        key,
                        value: VertexUpsertValueTemplate::Bound(scalar(value, at)?),
                    },
                    Operand::Number(Number::Parameter(index)) => ParsedUpsertAction::Property {
                        key,
                        value: VertexUpsertValueTemplate::Parameter { index, at },
                    },
                    Operand::Column(column) => ParsedUpsertAction::Expression {
                        key,
                        properties,
                        program: vec![MutationIntegerTemplateOp::Bound(
                            crate::GraphIntegerOp::ScalarColumn(column),
                        )],
                        at,
                    },
                    Operand::Integer { program, at } => ParsedUpsertAction::Expression {
                        key,
                        properties,
                        program,
                        at,
                    },
                }
            };
            target.push(action);
            if !self.take(b',')? {
                break;
            }
        }
        Ok(())
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
        reverse_catalog: None,
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
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphVertexUpsertTextError> {
        Self::prepare_definition(statement, relation, declarations, resolve, false)
            .map(|(upsert, _)| upsert)
    }

    /// The upsert and, in query mode, its terminal RETURN. A RETURN makes a
    /// MERGE without any SET clause a statement of this shape too.
    pub(crate) fn prepare_definition(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        returning: bool,
    ) -> Result<(Self, Option<VertexReturnTemplate>), GraphVertexUpsertTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        let (variable, labels, properties) =
            parser.vertex_merge_pattern().map_err(upsert_merge_error)?;
        let (parsed_match, parsed_create, parsed_after) = parser.upsert_branch_actions(variable)?;
        if !returning
            && parsed_match.is_empty()
            && parsed_create.is_empty()
            && parsed_after.is_empty()
        {
            return Err(error(
                statement.len(),
                GraphPatternTextErrorKind::Expected("ON MATCH SET, ON CREATE SET or SET"),
            )
            .into());
        }
        let parsed_return = if returning {
            Some(parser.vertex_return(variable)?)
        } else {
            None
        };
        parser.end()?;
        if let Some(parsed) = &parsed_return {
            mutation_query::admit_return(
                &parsed.projection,
                &parsed.types(),
                &parser.syntax.parameters,
                parsed.at,
            )?;
        }
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
                        ParsedUpsertAction::Expression {
                            key,
                            properties,
                            program,
                            at,
                        } => {
                            let GraphSymbol::Property(key) =
                                symbol(GraphSymbolKind::Property, key)?
                            else {
                                unreachable!("symbol domain checked")
                            };
                            let properties = properties
                                .into_iter()
                                .map(|name| {
                                    let GraphSymbol::Property(key) =
                                        symbol(GraphSymbolKind::Property, name)?
                                    else {
                                        unreachable!("symbol domain checked")
                                    };
                                    Ok(key)
                                })
                                .collect::<Result<Vec<_>, GraphPatternTextError>>()?;
                            VertexUpsertActionTemplate::Expression {
                                key,
                                properties,
                                program,
                                at,
                            }
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
        let after = resolve_actions(parsed_after)?;
        drop(resolve_actions);
        let returning = parsed_return
            .map(|parsed| parsed.resolve(&mut symbol))
            .transpose()?;
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
        Ok((
            Self {
                merge,
                on_match,
                on_create,
                after,
            },
            returning,
        ))
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
                VertexUpsertActionTemplate::Expression { key, properties, program, at } =>
                    GraphVertexUpsertAction::SetExpression {
                        key: *key,
                        properties: properties.clone(),
                        value: super::integer::bind_integer(program, &values, *at)?,
                    },
                VertexUpsertActionTemplate::Label { label, present } =>
                    GraphVertexUpsertAction::SetLabel { label: *label, present: *present },
            })).collect()
        };
        let on_match = bind_actions(&self.on_match)?;
        let on_create = bind_actions(&self.on_create)?;
        let after = bind_actions(&self.after)?;
        PreparedGraphVertexUpsert::prepare_with_trailing_actions(merge, on_match, on_create, after)
            .map_err(|source| GraphVertexUpsertTextError {
                offset: self.merge.selection.return_at,
                kind: GraphVertexUpsertTextErrorKind::UpsertBuild(source),
            })
    }
}

/// RETURN after MERGE: the merge variable, its properties after every
/// clause, and expressions over them. MERGE chooses one vertex, so there
/// is one input row and nothing else is in scope.
#[derive(Clone, Copy)]
enum VertexBinding<'a> {
    Vertex,
    Property(Name<'a>),
}

pub(super) struct ParsedVertexReturn<'a> {
    variable: Name<'a>,
    bindings: Vec<(VertexBinding<'a>, Name<'a>, GraphSetColumnType)>,
    projection: Vec<ReadProjectionTemplate>,
    quantifier: GraphSetQuantifier,
    order: Vec<crate::algebra::GraphValueOrder>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
    at: usize,
}

impl<'a> mutation_query::ReturnLeaves<'a> for ParsedVertexReturn<'a> {
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        if word != self.variable.text
            || matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            return Ok(None);
        }
        let name = parser.name()?;
        let (binding, alias, kind) = if parser.take(b'.')? {
            let key = parser.name()?;
            (
                VertexBinding::Property(key),
                key,
                GraphSetColumnType::Scalar,
            )
        } else {
            (VertexBinding::Vertex, name, GraphSetColumnType::Vertex)
        };
        if let Some(column) = self
            .bindings
            .iter()
            .position(|(old, _, _)| match (old, binding) {
                (VertexBinding::Vertex, VertexBinding::Vertex) => true,
                (VertexBinding::Property(old), VertexBinding::Property(key)) => {
                    old.text == key.text
                }
                _ => false,
            })
        {
            self.bindings[column].1 = alias;
            return Ok(Some(column));
        }
        if self.bindings.len() == MAX_PATTERN_VERTICES {
            return Err(error(
                alias.at,
                GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                    dimension: crate::algebra::PatternLimitDimension::Columns,
                    limit: MAX_PATTERN_VERTICES,
                    observed: self.bindings.len() + 1,
                }),
            ));
        }
        self.bindings.push((binding, alias, kind));
        Ok(Some(self.bindings.len() - 1))
    }
    fn name(&self, column: usize) -> Name<'a> {
        self.bindings[column].1
    }
    fn types(&self) -> Vec<GraphSetColumnType> {
        self.bindings.iter().map(|(_, _, kind)| *kind).collect()
    }
}

impl<'a> ParsedVertexReturn<'a> {
    fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<VertexReturnTemplate, GraphPatternTextError> {
        let mut bindings = Vec::new();
        for (binding, _, _) in self.bindings {
            bindings.push(match binding {
                VertexBinding::Vertex => GraphVertexReturnBinding::Vertex,
                VertexBinding::Property(key) => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    GraphVertexReturnBinding::Property(key)
                }
            });
        }
        Ok(VertexReturnTemplate {
            bindings,
            projection: self.projection,
            quantifier: self.quantifier,
            order: self.order,
            offset: self.offset,
            count: self.count,
            at: self.at,
        })
    }
}

impl<'a> Parser<'a> {
    fn vertex_return(
        &mut self,
        variable: Name<'a>,
    ) -> Result<ParsedVertexReturn<'a>, GraphVertexUpsertTextError> {
        let at = self.current.at;
        self.word("RETURN")?;
        let quantifier = if self.take_word("DISTINCT")? {
            GraphSetQuantifier::Distinct
        } else {
            self.take_all_quantifier()?;
            GraphSetQuantifier::All
        };
        let mut returning = ParsedVertexReturn {
            variable,
            bindings: Vec::new(),
            projection: Vec::new(),
            quantifier,
            order: Vec::new(),
            offset: ReadPageNumber::Literal(0),
            count: None,
            at,
        };
        let (output, spellings) = if self.take(b'*')? {
            returning
                .bindings
                .push((VertexBinding::Vertex, variable, GraphSetColumnType::Vertex));
            returning.projection.push(ReadProjectionTemplate {
                name: variable.text.to_owned(),
                value: ReadValueTemplate::Column(0),
            });
            (vec![(variable, GraphSetColumnType::Vertex)], Vec::new())
        } else {
            let items = self.write_return_items(&mut returning)?;
            returning.projection = items.projection;
            (items.output, items.spellings)
        };
        if let Some(ReadStageTemplate::Page {
            order,
            offset,
            count,
            ..
        }) = self.row_page_sourced(&output, &spellings)?
        {
            returning.order = order;
            returning.offset = offset;
            returning.count = count;
        }
        Ok(returning)
    }
}

impl VertexReturnTemplate {
    fn bind(
        &self,
        upsert: PreparedGraphVertexUpsert,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphVertexUpsertQuery, GraphVertexUpsertTextError> {
        let mut projection = Vec::new();
        for output in &self.projection {
            projection.push(GraphSetProjection::new(
                &output.name,
                return_projection::bind_read_value(&output.value, values)?,
            ));
        }
        let mut query = PreparedGraphVertexUpsertQuery::prepare(
            upsert,
            self.bindings.clone(),
            projection,
            self.quantifier,
        )
        .map_err(|kind| GraphVertexUpsertTextError {
            offset: self.at,
            kind: GraphVertexUpsertTextErrorKind::ReturnBuild(kind),
        })?;
        if !self.order.is_empty() {
            query = query
                .with_order_by(&self.order)
                .map_err(|kind| crate::GraphSetTextError {
                    offset: self.at,
                    kind: crate::GraphSetTextErrorKind::OrderBuild(kind),
                })?;
        }
        Ok(query.with_page(
            return_projection::pipeline::page_value(&self.offset, values),
            self.count
                .as_ref()
                .map(|count| return_projection::pipeline::page_value(count, values)),
        ))
    }
}

impl PreparedGraphVertexUpsertQueryText {
    /// Classify write text with native token framing: a statement that
    /// starts with `MERGE (` and has a top-level RETURN, in one statement.
    /// Quoted values, property keys and aliases never become clause words.
    /// This validates nothing and resolves no symbol; prepare owns that.
    pub fn has_return_clause(statement: &str) -> Result<bool, GraphVertexUpsertTextError> {
        if statement.len() > crate::MAX_GRAPH_WRITE_SCRIPT_BYTES {
            return Err(error(
                0,
                GraphPatternTextErrorKind::Expected("bounded native write text"),
            )
            .into());
        }
        let mut lexer = Lexer {
            text: statement,
            at: 0,
            tokens: 0,
        };
        let first = script::next_script_token(&mut lexer)?;
        let opening = script::next_script_token(&mut lexer.clone())?;
        if !matches!(first.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("MERGE"))
            || !matches!(opening.kind, TokenKind::Punct(b'('))
        {
            return Ok(false);
        }
        let mut depth = 0_usize;
        let mut previous = first.kind;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Word(word) if depth == 0 && word.eq_ignore_ascii_case("RETURN") => {
                    let alias = matches!(previous, TokenKind::Punct(b'.'))
                        || matches!(previous, TokenKind::Word(word) if word.eq_ignore_ascii_case("AS"));
                    if !alias {
                        return Ok(true);
                    }
                }
                _ => {}
            }
            previous = token.kind;
        }
    }

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
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphVertexUpsertTextError> {
        let body = mutation_query::statement_body(statement, "one MERGE RETURN statement")?;
        let (upsert, returning) = PreparedGraphVertexUpsertText::prepare_definition(
            body,
            relation,
            declarations,
            resolve,
            true,
        )?;
        let returning = returning.expect("query mode prepares a RETURN definition");
        Ok(Self {
            statement: statement.to_owned(),
            upsert,
            returning,
        })
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.upsert.merge.selection.parameter_schema()
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphVertexUpsertQuery, GraphVertexUpsertTextError> {
        let upsert = self.upsert.bind_parameters(arguments)?;
        let values = self.upsert.merge.selection.checked_arguments(arguments)?;
        self.returning.bind(upsert, &values)
    }
}

#[cfg(test)]
mod return_framing_tests {
    use super::PreparedGraphVertexUpsertQueryText;

    #[test]
    fn only_a_merge_with_a_top_level_return_routes_here() {
        for (text, expected) in [
            ("MERGE (n:L {k: 1}) RETURN n", true),
            (
                "MERGE (n:L {k: 1}) ON CREATE SET n.p = 1 SET n.q = 2 RETURN n.p",
                true,
            ),
            ("MERGE (n:L {k: 1}) SET n.p = 1", false),
            ("MERGE (n:L {k: 'RETURN'}) SET n.p = 1", false),
            ("MATCH (n) RETURN n", false),
            ("MERGE (n:L {k: 1}); MATCH (m) RETURN m", false),
            ("MERGE (n:L {k: 1}) SET n.RETURN = 1", false),
        ] {
            assert_eq!(
                PreparedGraphVertexUpsertQueryText::has_return_clause(text).unwrap(),
                expected,
                "{text}"
            );
        }
    }
}
