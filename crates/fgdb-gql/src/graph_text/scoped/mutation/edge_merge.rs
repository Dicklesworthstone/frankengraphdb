//! Native directed relationship MERGE over endpoints bound by the shared MATCH prefix.

use super::*;
use crate::{
    GraphEdgeMergeTextError, GraphEdgeMergeTextErrorKind, PreparedGraphEdgeMerge,
    PreparedGraphEdgeMergeText,
};

struct ParsedEdgeMerge<'a> {
    source: Name<'a>,
    destination: Name<'a>,
    relation: Name<'a>,
    properties: Vec<super::merge::MergeProperty<'a>>,
    undirected: bool,
}
fn build_error(at: usize, source: crate::GraphEdgeMergeBuildError) -> GraphEdgeMergeTextError {
    GraphEdgeMergeTextError {
        offset: at,
        kind: GraphEdgeMergeTextErrorKind::Build(source),
    }
}

impl<'a> Parser<'a> {
    /// `MERGE (a)-[r:R {k: v}]->(b)`, its reverse arrow, or no arrow for an
    /// undirected MERGE. The relationship variable and the property map are
    /// optional; a named variable must be distinct from every vertex
    /// variable (fgdb-at06q).
    fn edge_merge_pattern(&mut self) -> Result<ParsedEdgeMerge<'a>, GraphEdgeMergeTextError> {
        self.word("MERGE")?;
        self.punct(b'(', "(")?;
        let left = self.variable()?;
        self.punct(b')', ")")?;

        let incoming = self.take(b'<')?;
        self.punct(b'-', "-")?;
        self.punct(b'[', "[")?;
        if !self.is_punct(b':') {
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
        }
        self.punct(b':', ":")?;
        let relation = self.name()?;
        let properties = self.merge_property_map().map_err(|refusal| match refusal {
            super::merge::MergeMapError::Pattern(error) => error.into(),
            super::merge::MergeMapError::TooManyFields { at, observed } => build_error(
                at,
                crate::GraphEdgeMergeBuildError::TooManyProperties {
                    limit: crate::MAX_GRAPH_EDGE_MERGE_PROPERTIES,
                    observed,
                },
            ),
            super::merge::MergeMapError::DuplicateProperty { at } => {
                build_error(at, crate::GraphEdgeMergeBuildError::DuplicateProperty)
            }
        })?;
        self.punct(b']', "]")?;
        self.punct(b'-', "-")?;
        let outgoing = self.take(b'>')?;
        if incoming && outgoing {
            return Err(error(
                relation.at,
                GraphPatternTextErrorKind::Expected("at most one relationship direction"),
            )
            .into());
        }

        self.punct(b'(', "(")?;
        let right = self.variable()?;
        self.punct(b')', ")")?;
        self.end()?;
        // An undirected MERGE creates left to right, as openCypher does.
        let (source, destination) = if incoming {
            (right, left)
        } else {
            (left, right)
        };
        Ok(ParsedEdgeMerge {
            source,
            destination,
            relation,
            properties,
            undirected: !incoming && !outgoing,
        })
    }
}

impl PreparedGraphEdgeMergeText {
    /// Prepare `MATCH ... MERGE (a)-[r:R {k: v}]->(b)`, its reverse arrow,
    /// or the undirected `(a)-[:R]-(b)` (fgdb-at06q). Both endpoints must
    /// already be visible MATCH variables. Map values are non-null literals
    /// or parameters; they filter existing edges and fill a created one.
    /// Endpoint declarations, ON MATCH/ON CREATE clauses and RETURN refuse
    /// here rather than being partially ignored.
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeMergeTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        _relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeMergeTextError> {
        Self::prepare_with_parameter_types_and_resolver(
            statement,
            _relation,
            declarations,
            &mut resolve,
        )
    }

    /// As [`Self::prepare_with_parameter_types`], with a host resolver whose
    /// pinned tzdb the statement's temporal constructors use (fgdb-txbg9).
    pub(crate) fn prepare_with_parameter_types_and_resolver<R>(
        statement: &str,
        _relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
    ) -> Result<Self, GraphEdgeMergeTextError>
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        parser.parse_match_prefix()?;
        let parsed = parser.edge_merge_pattern()?;
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
            let value = resolve
                .resolve_symbol(kind, name.text)
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
            syntax.match_mode,
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
            unreachable!("symbol domain checked by shared resolver")
        };
        let mut properties = Vec::new();
        for property in parsed.properties {
            let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, property.key)?
            else {
                unreachable!("symbol domain checked by shared resolver")
            };
            properties.push((key, property.value));
        }

        let columns = projections
            .into_iter()
            .enumerate()
            .map(|(index, projection)| BoundColumn {
                alias: format!("_edge_merge_{index}"),
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
            reverse_catalog: None,
        };
        Ok(Self {
            selection,
            relation: found,
            source,
            destination,
            properties,
            undirected: parsed.undirected,
        })
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        self.selection.statement()
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.selection.parameter_schema()
    }

    /// Bind without lexing, catalog access or storage observation.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphEdgeMerge, GraphEdgeMergeTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        let selection = self.selection.bind_values(&values)?;
        let mut properties = Vec::with_capacity(self.properties.len());
        for (key, template) in &self.properties {
            let value = match template {
                crate::mutation_text::VertexMergeValueTemplate::Bound(value) => value.clone(),
                crate::mutation_text::VertexMergeValueTemplate::Parameter { index, at } => {
                    scalar(values[*index].clone(), *at)?
                }
            };
            properties.push((*key, value));
        }
        // A NULL argument refuses as NullProperty: it can neither match nor
        // be created.
        let merge = PreparedGraphEdgeMerge::prepare(
            selection,
            self.relation,
            self.source,
            self.destination,
            properties,
        )
        .map_err(|source| build_error(self.selection.return_at, source))?;
        Ok(if self.undirected {
            merge.with_undirected()
        } else {
            merge
        })
    }
}
