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
}
fn build_error(at: usize, source: crate::GraphEdgeMergeBuildError) -> GraphEdgeMergeTextError {
    GraphEdgeMergeTextError { offset: at, kind: GraphEdgeMergeTextErrorKind::Build(source) }
}

impl<'a> Parser<'a> {
    fn edge_merge_pattern(&mut self) -> Result<ParsedEdgeMerge<'a>, GraphEdgeMergeTextError> {
        self.word("MERGE")?;
        self.punct(b'(', "(")?;
        let left = self.variable()?;
        self.punct(b')', ")")?;

        let incoming = self.take(b'<')?;
        self.punct(b'-', "-")?;
        self.punct(b'[', "[")?;
        self.punct(b':', ":")?;
        let relation = self.name()?;
        self.punct(b']', "]")?;
        self.punct(b'-', "-")?;
        let outgoing = self.take(b'>')?;
        if incoming == outgoing {
            return Err(error(relation.at,
                GraphPatternTextErrorKind::Expected("exactly one relationship direction")).into());
        }

        self.punct(b'(', "(")?;
        let right = self.variable()?;
        self.punct(b')', ")")?;
        self.end()?;
        Ok(if incoming {
            ParsedEdgeMerge { source: right, destination: left, relation }
        } else {
            ParsedEdgeMerge { source: left, destination: right, relation }
        })
    }
}

impl PreparedGraphEdgeMergeText {
    /// Prepare `MATCH ... MERGE (a)-[:R]->(b)` or its reverse-arrow spelling.
    /// Both endpoints must already be visible MATCH variables. Relationship
    /// variables, property maps, undirected relationships, endpoint declarations
    /// and ON MATCH/ON CREATE clauses refuse rather than being partially ignored.
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeMergeTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeMergeTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
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
        let (builder, filters) = resolve_pattern(
            &syntax.variables[..syntax.root_variables],
            &syntax.labels,
            &syntax.edges,
            syntax.filters,
            &mut symbol,
        )?;
        let mut scopes = Vec::new();
        for scope in syntax.scopes { scopes.push(scope.resolve(&mut symbol)?); }
        let GraphSymbol::Relation(found) = symbol(GraphSymbolKind::Relation, parsed.relation)? else {
            unreachable!("symbol domain checked by shared resolver")
        };
        if found != relation {
            return Err(GraphEdgeMergeTextError {
                offset: parsed.relation.at,
                kind: GraphEdgeMergeTextErrorKind::RelationMismatch,
            });
        }

        let columns = projections.into_iter().enumerate().map(|(index, projection)| BoundColumn {
            alias: format!("_edge_merge_{index}"),
            variable: projection.variable.text.to_owned(),
            key: None,
        }).collect::<Vec<_>>();
        let clauses = scopes.iter().map(BoundScope::clause).collect::<Vec<_>>();
        let projected = columns.iter().map(BoundColumn::declaration).collect::<Vec<_>>();
        built(at, builder.prepare_values_with_clauses(&clauses, &projected, 0, None))?;
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
            return_at: at,
        };
        Ok(Self { selection, relation, source, destination })
    }

    #[must_use]
    pub fn statement(&self) -> &str { self.selection.statement() }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { self.selection.parameter_schema() }

    /// Bind without lexing, catalog access or storage observation.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphEdgeMerge, GraphEdgeMergeTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        let selection = self.selection.bind_values(&values)?;
        PreparedGraphEdgeMerge::prepare(
            selection,
            self.relation,
            self.source,
            self.destination,
            Vec::new(),
        ).map_err(|source| build_error(self.selection.return_at, source))
    }
}
