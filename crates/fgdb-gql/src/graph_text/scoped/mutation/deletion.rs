//! Native non-detaching DELETE on the shared MATCH lexer/compiler.

use super::*;
use crate::{
    GraphDeleteBuildError, GraphDeleteTextError, GraphDeleteTextErrorKind, PreparedGraphDelete,
    PreparedGraphDeleteText,
};

fn build_error(at: usize, source: GraphDeleteBuildError) -> GraphDeleteTextError {
    GraphDeleteTextError { offset: at, kind: GraphDeleteTextErrorKind::Build(source) }
}

impl<'a> Parser<'a> {
    fn plain_delete_targets(
        &mut self,
    ) -> Result<(Vec<Projection<'a>>, Vec<usize>), GraphDeleteTextError> {
        self.word("DELETE")?;
        let mut columns = Vec::new();
        let mut targets = Vec::new();
        loop {
            if targets.len() >= crate::MAX_GRAPH_DELETE_TARGETS {
                return Err(build_error(
                    self.current.at,
                    GraphDeleteBuildError::TooManyTargets {
                        limit: crate::MAX_GRAPH_DELETE_TARGETS,
                        observed: targets.len() + 1,
                    },
                ));
            }
            let variable = self.variable()?;
            let target = self.mutation_projection(&mut columns, variable, None)?;
            if targets.contains(&target) {
                return Err(build_error(
                    variable.at,
                    GraphDeleteBuildError::TargetColumn {
                        target: targets.len(),
                        column: target,
                    },
                ));
            }
            targets.push(target);
            if !self.take(b',')? { break; }
        }
        self.end()?;
        Ok((columns, targets))
    }
}

impl PreparedGraphDeleteText {
    /// Prepare `MATCH [WALK] ... [WHERE ...] [OPTIONAL MATCH ...] DELETE a[,b]`
    /// through the existing graph-text parser. DELETE is non-detaching; the
    /// write-capable host later proves every target has no incident relationship.
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphDeleteTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    /// One parameter schema spans MATCH/WHERE/OPTIONAL clauses. DELETE itself
    /// accepts bound vertex variables only. Syntax and target shape are fully
    /// validated before any catalog callback.
    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphDeleteTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        parser.parse_match_prefix()?;
        let (projections, targets) = parser.plain_delete_targets()?;
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

        let columns = projections.into_iter().enumerate().map(|(index, projection)| BoundColumn {
            alias: format!("_delete_{index}"),
            variable: projection.variable.text.to_owned(),
            key: None,
            path: None,
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
        Ok(Self { selection, relation, targets })
    }

    #[must_use]
    pub fn statement(&self) -> &str { self.selection.statement() }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { self.selection.parameter_schema() }

    /// Bind without lexing, catalog access or storage observation.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphDelete, GraphDeleteTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        let selection = self.selection.bind_values(&values)?;
        PreparedGraphDelete::prepare(selection, self.relation, self.targets.clone())
            .map_err(|source| build_error(self.selection.return_at, source))
    }
}
