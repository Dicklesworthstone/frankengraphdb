//! Native bounded single-vertex MERGE on the shared graph-text lexer.

use super::*;
use crate::insertion::{GraphInsertBuildError, GraphInsertVertex, PreparedGraphInsert};
use crate::mutation_text::VertexMergeValueTemplate;
use crate::{
    GraphMutationValue, GraphVertexMergeBuildError, GraphVertexMergeTextError,
    GraphVertexMergeTextErrorKind, PreparedGraphVertexMerge, PreparedGraphVertexMergeText,
};

struct MergeProperty<'a> {
    key: Name<'a>,
    filter: Number,
    value: VertexMergeValueTemplate,
}

fn insert_error(at: usize, source: GraphInsertBuildError) -> GraphVertexMergeTextError {
    GraphVertexMergeTextError { offset: at, kind: GraphVertexMergeTextErrorKind::InsertBuild(source) }
}
fn merge_error(at: usize, source: GraphVertexMergeBuildError) -> GraphVertexMergeTextError {
    GraphVertexMergeTextError { offset: at, kind: GraphVertexMergeTextErrorKind::MergeBuild(source) }
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
            if labels.iter().any(|previous: &Name<'a>| previous.text == label.text) {
                return Err(insert_error(label.at, GraphInsertBuildError::DuplicateLabel { vertex: 0 }));
            }
            labels.push(label);
        }
        let mut properties = Vec::new();
        if self.take(b'{')? {
            if self.is_punct(b'}') {
                return Err(error(self.current.at,
                    GraphPatternTextErrorKind::Expected("at least one MERGE property")).into());
            }
            loop {
                if properties.len() >= crate::insertion::MAX_GRAPH_INSERT_FIELDS {
                    return Err(insert_error(self.current.at, GraphInsertBuildError::TooManyFields {
                        limit: crate::insertion::MAX_GRAPH_INSERT_FIELDS,
                        observed: properties.len() + 1,
                    }));
                }
                let key = self.name()?;
                if properties.iter().any(|previous: &MergeProperty<'a>| previous.key.text == key.text) {
                    return Err(insert_error(key.at, GraphInsertBuildError::DuplicateProperty { declaration: 0 }));
                }
                self.punct(b':', ":")?;
                let at = self.current.at;
                let operand = self.mutation_operand(&mut Vec::new())?;
                let (filter, value) = match operand {
                    Operand::Literal(value) => {
                        if matches!(value.value(), CanonicalScalar::Null) {
                            return Err(error(at, GraphPatternTextErrorKind::Expected(
                                "non-null MERGE property value",
                            )).into());
                        }
                        (Number::Literal(GqlParameterValue::Scalar(value.clone())),
                            VertexMergeValueTemplate::Bound(value))
                    }
                    Operand::Number(Number::Literal(value)) => {
                        let scalar = scalar(value.clone(), at)?;
                        if matches!(scalar.value(), CanonicalScalar::Null) {
                            return Err(error(at, GraphPatternTextErrorKind::Expected(
                                "non-null MERGE property value",
                            )).into());
                        }
                        (Number::Literal(value), VertexMergeValueTemplate::Bound(scalar))
                    }
                    Operand::Number(Number::Parameter(index)) => {
                        (Number::Parameter(index), VertexMergeValueTemplate::Parameter { index, at })
                    }
                    Operand::Column(_) | Operand::Integer { .. } => {
                        return Err(error(at, GraphPatternTextErrorKind::Expected(
                            "scalar literal or parameter MERGE property value",
                        )).into());
                    }
                };
                properties.push(MergeProperty { key, filter, value });
                if !self.take(b',')? { break; }
            }
            self.punct(b'}', "}")?;
        }
        self.punct(b')', ")")?;
        self.end()?;
        Ok((variable, labels, properties))
    }
}

impl PreparedGraphVertexMergeText {
    /// Prepare one native `MERGE (n:Label {key:value})` vertex definition. The
    /// bounded profile supports labels and exact scalar property equality. A
    /// property map is optional; null merge-key values refuse because equality
    /// against null cannot identify an existing vertex deterministically.
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
        let (variable, parsed_labels, parsed_properties) = parser.vertex_merge_pattern()?;
        let at = statement.len();
        let syntax = parser.syntax;

        // All syntax/duplicate/parameter declaration checks completed above.
        // Catalog access begins only here and shares one domain-aware cache.
        let mut cache = BTreeMap::new();
        let mut symbol = |kind, name: Name<'_>| -> Result<GraphSymbol, GraphPatternTextError> {
            let key = (kind, name.text.to_owned());
            if let Some(value) = cache.get(&key) { return Ok(*value); }
            let value = resolve(kind, name.text)
                .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
            if value.kind() != kind {
                return Err(error(name.at, GraphPatternTextErrorKind::WrongSymbolKind {
                    expected: kind,
                    found: value.kind(),
                }));
            }
            cache.insert(key, value);
            Ok(value)
        };

        let mut builder = GraphPatternBuilder::new();
        built(variable.at, builder.vertex(variable.text))?;
        let mut labels = Vec::new();
        for label in parsed_labels {
            let GraphSymbol::Label(label_id) = symbol(GraphSymbolKind::Label, label)? else {
                unreachable!("symbol domain checked by shared resolver")
            };
            built(label.at, builder.filter(variable.text, VertexPredicate::HasLabel(label_id)))?;
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
        }];
        let projected = columns.iter().map(BoundColumn::declaration).collect::<Vec<_>>();
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
            return_at: at,
        };
        Ok(Self { selection, relation, labels, properties })
    }

    #[must_use]
    pub fn statement(&self) -> &str { self.selection.statement() }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { self.selection.parameter_schema() }

    /// Bind once into the unique-match/create primitive. Matching and creation
    /// receive the exact same canonical property values.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphVertexMerge, GraphVertexMergeTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        let mut fields = Vec::new();
        for (key, template) in &self.properties {
            let value = match template {
                VertexMergeValueTemplate::Bound(value) => value.clone(),
                VertexMergeValueTemplate::Parameter { index, at } => {
                    let value = scalar(values[*index].clone(), *at)?;
                    if matches!(value.value(), CanonicalScalar::Null) {
                        return Err(error(*at, GraphPatternTextErrorKind::Expected(
                            "non-null MERGE property value",
                        )).into());
                    }
                    value
                }
            };
            fields.push((*key, GraphMutationValue::Literal(value)));
        }
        let selection = self.selection.bind_values(&values)?;
        let creation = PreparedGraphInsert::prepare_standalone(
            self.relation,
            vec![GraphInsertVertex { labels: self.labels.clone(), properties: fields }],
            Vec::new(),
        ).map_err(|source| GraphVertexMergeTextError {
            offset: self.selection.return_at,
            kind: GraphVertexMergeTextErrorKind::InsertBuild(source),
        })?;
        PreparedGraphVertexMerge::prepare(selection, self.relation, 0, creation)
            .map_err(|source| merge_error(self.selection.return_at, source))
    }
}
