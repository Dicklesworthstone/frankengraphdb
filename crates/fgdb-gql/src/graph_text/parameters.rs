//! Explicit argument declarations for the shared graph-text parser.
//! Nonnumeric types come from the caller's prepared-statement contract, never
//! from a database sample, parameter spelling, value, or text substitution.

use super::*;
use crate::set_text::{BoundSetTextInput, ReadProjectionTemplate, ReadStageTemplate};

impl PreparedGraphText {
    /// Prepare with explicit types for selected argument names (without `$`).
    /// Undeclared property arguments keep the existing Int64 interpretation;
    /// SKIP/LIMIT remain UInt64. A declared Scalar(kind) property argument
    /// accepts that exact canonical kind or canonical null. All declarations
    /// must be unique, bounded, and used in the statement. Invalid or conflicting
    /// declarations are refused before catalog resolution or storage access.
    ///
    /// This is the same parser/compiler as prepare(), including scoped MATCH.
    /// Binding uses the existing GqlParameters map, which can mix numeric and
    /// canonical scalar arguments without copying scalar payloads per use.
    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphPatternTextError> {
        let syntax = Parser::new_with_parameter_types(statement, declarations)?.parse()?;
        Self::from_syntax(statement, syntax, resolve)
    }

    pub fn prepare_with_parameter_types_and_resolver(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphPatternTextError> {
        let syntax = Parser::new_with_parameter_types(statement, declarations)?.parse()?;
        Self::from_syntax(statement, syntax, resolve)
    }

    /// Give the relational composition parser a view of the SAME lexical
    /// tokens. Admission is for the complete statement, including every arm,
    /// grouping delimiter and final page, before splitting any leaf range.
    pub(crate) fn composition_tokens<'a>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
    ) -> Result<Vec<crate::set_text::TextToken<'a>>, GraphPatternTextError> {
        use crate::set_text::{TextKind, TextToken};
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        let mut tokens = Vec::new();
        loop {
            let token = parser.current;
            let kind = match token.kind {
                TokenKind::Word(word) => TextKind::Word(word),
                TokenKind::Digits(digits) => TextKind::Digits(digits),
                TokenKind::Parameter(name) => TextKind::Parameter(name),
                TokenKind::Quoted(_) => TextKind::Quoted,
                TokenKind::Punct(ch) => TextKind::Punct(ch),
                TokenKind::End => TextKind::End,
            };
            tokens.push(TextToken { kind, at: token.at });
            if matches!(token.kind, TokenKind::End) {
                break;
            }
            parser.advance()?;
        }
        Ok(tokens)
    }

    /// Parse once and retain native syntax until ALL compound operands and
    /// their shared parameter/output contracts have passed. Computed RETURN
    /// reuses the same scalar parser as SET, but produces only a read relation.
    pub(crate) fn unresolved_for_composition<'a>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
    ) -> Result<UnresolvedGraphText<'a>, crate::GraphSetTextError> {
        Parser::new_with_parameter_types(statement, declarations)?
            .parse_return_for_composition(statement)
    }
}

/// Preparation-only phase object. Its syntax never enters execution. For a
/// computed RETURN, syntax.columns describe private source inputs; projection
/// owns the real public output schema. Otherwise the original lowering remains.
pub(crate) struct UnresolvedGraphText<'a> {
    pub(super) statement: &'a str,
    pub(super) syntax: Syntax<'a>,
    pub(super) projection: Option<Vec<ReadProjectionTemplate>>,
    pub(super) pipeline: Vec<ReadStageTemplate>,
    pub(super) singleton: bool,
    pub(super) leading: Vec<ReadStageTemplate>,
    pub(super) leading_types: Vec<crate::GraphSetColumnType>,
    pub(super) correlations: Vec<(usize, usize)>,
}
impl UnresolvedGraphText<'_> {
    pub(crate) fn column_schema(&self) -> (Vec<String>, Vec<crate::GraphSetColumnType>) {
        use crate::GraphSetColumnType::{Scalar, Vertex};
        let column_type = |column: &Column<'_>| match column.path {
            Some(GraphPathFunction::Value) => crate::GraphSetColumnType::Path,
            Some(GraphPathFunction::Length) => Scalar,
            Some(GraphPathFunction::Nodes) => crate::GraphSetColumnType::Vertices,
            Some(GraphPathFunction::Edges) => crate::GraphSetColumnType::Edges,
            // Edge selects the property owner in BoundColumn::declaration;
            // an edge property itself is a scalar, not an edge identity.
            Some(GraphPathFunction::Edge) if column.property.is_some() => Scalar,
            Some(GraphPathFunction::Edge) => crate::GraphSetColumnType::Edge,
            Some(GraphPathFunction::Labels) => crate::GraphSetColumnType::List,
            Some(GraphPathFunction::Type) => Scalar,
            None if column.property.is_some() => Scalar,
            None => Vertex,
        };
        let input_types: Vec<_> = self
            .leading_types
            .iter()
            .copied()
            .chain(self.syntax.columns.iter().map(column_type))
            .collect();
        let (mut names, mut types): (Vec<String>, Vec<crate::GraphSetColumnType>) =
            if let Some(projection) = &self.projection {
                projection
                    .iter()
                    .map(|column| {
                        let kind = column
                            .value
                            .column_type(&input_types, &self.syntax.parameters);
                        (column.name.clone(), kind)
                    })
                    .unzip()
            } else {
                self.syntax
                    .columns
                    .iter()
                    .map(|column| (column.alias.text.to_owned(), column_type(column)))
                    .unzip()
            };
        for stage in &self.pipeline {
            if let ReadStageTemplate::Project { projection, .. } = stage {
                let next = projection
                    .iter()
                    .map(|column| column.value.column_type(&types, &self.syntax.parameters))
                    .collect();
                names = projection
                    .iter()
                    .map(|column| column.name.clone())
                    .collect();
                types = next;
            }
            if let ReadStageTemplate::Unwind { name, .. } = stage {
                names.push(name.clone());
                types.push(crate::GraphSetColumnType::Any);
            }
        }
        (names, types)
    }
    pub(crate) fn depth(&self) -> usize {
        1 + self.leading.len()
            + usize::from(!self.leading.is_empty() && !self.singleton)
            + usize::from(self.projection.is_some())
            + usize::from(!self.correlations.is_empty())
            + self
                .pipeline
                .iter()
                .filter(|stage| !matches!(stage, ReadStageTemplate::Page { .. }))
                .count()
    }
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.syntax.parameters
    }
    pub(crate) fn parameter_offsets(&self) -> &[usize] {
        &self.syntax.parameter_offsets
    }

    pub(crate) fn resolve(
        self,
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<BoundSetTextInput, GraphPatternTextError> {
        let quantifier = if self.syntax.distinct {
            crate::GraphSetQuantifier::Distinct
        } else {
            crate::GraphSetQuantifier::All
        };
        let parameters = self.syntax.parameters.clone();
        let parameter_offsets = self.syntax.parameter_offsets.clone();
        let return_at = self.syntax.return_at;
        if self.singleton {
            return Ok(BoundSetTextInput {
                selection: None,
                parameters,
                parameter_offsets,
                return_at,
                projection: self.projection,
                quantifier,
                pipeline: self.pipeline,
                singleton: true,
                leading: self.leading,
                correlations: self.correlations,
            });
        }
        let selection = if self.projection.is_none() {
            PreparedGraphText::from_syntax(self.statement, self.syntax, resolve)?
        } else {
            let syntax = self.syntax;
            let mut cache = BTreeMap::new();
            let mut symbol = |kind, name: Name<'_>| -> Result<GraphSymbol, GraphPatternTextError> {
                let key = (kind, name.text.to_owned());
                if let Some(value) = cache.get(&key) {
                    return Ok(*value);
                }
                let value = resolve(kind, name.text).ok_or_else(|| {
                    error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind))
                })?;
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
            let (builder, filters) = scoped::resolve_pattern(
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
            let mut columns = Vec::new();
            for (index, column) in syntax.columns.into_iter().enumerate() {
                let key = if let Some(name) = column.property {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)?
                    else {
                        unreachable!("the shared resolver checked the property domain")
                    };
                    Some(key)
                } else {
                    None
                };
                columns.push(BoundColumn {
                    alias: format!("_return_input_{index}"),
                    variable: column.variable.text.to_owned(),
                    key,
                    path: column.path,
                });
            }
            let clauses: Vec<_> = scopes.iter().map(BoundScope::clause).collect();
            let projected: Vec<_> = columns.iter().map(BoundColumn::declaration).collect();
            built(
                syntax.return_at,
                builder.prepare_values_with_clauses(&clauses, &projected, 0, None),
            )?;
            // Never push public DISTINCT or pagination into these hidden input
            // rows: equal output values may arise from different input tuples.
            PreparedGraphText {
                statement: self.statement.to_owned(),
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
                return_at: syntax.return_at,
                reverse_catalog: None,
            }
        };
        Ok(BoundSetTextInput {
            selection: Some(selection),
            parameters,
            parameter_offsets,
            return_at,
            singleton: false,
            leading: self.leading,
            correlations: self.correlations,
            projection: self.projection,
            quantifier,
            pipeline: self.pipeline,
        })
    }
}

impl<'a> Parser<'a> {
    pub(super) fn new_with_parameter_types(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
    ) -> Result<Self, GraphPatternTextError> {
        let mut parser = Self::new(statement)?;
        if declarations.len() > MAX_GRAPH_TEXT_TOKENS {
            return Err(error(0, GraphPatternTextErrorKind::ParameterDeclaration));
        }
        let mut bytes = 0_usize;
        for &(name, kind) in declarations {
            let raw = name.as_bytes();
            if raw.is_empty()
                || raw.len() > MAX_PATTERN_NAME_BYTES
                || !(raw[0].is_ascii_alphabetic() || raw[0] == b'_')
                || !raw
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                || parser.parameter_types.contains_key(name)
            {
                return Err(error(0, GraphPatternTextErrorKind::ParameterDeclaration));
            }
            bytes = bytes
                .checked_add(raw.len())
                .ok_or_else(|| error(0, GraphPatternTextErrorKind::ParameterDeclaration))?;
            if bytes > MAX_GRAPH_TEXT_BYTES {
                return Err(error(0, GraphPatternTextErrorKind::ParameterDeclaration));
            }
            parser.parameter_types.insert(name.to_owned(), kind);
        }
        Ok(parser)
    }

    pub(super) fn check_declared_parameter(
        &self,
        name: &str,
        required: GqlParameterType,
        at: usize,
    ) -> Result<(), GraphPatternTextError> {
        if self
            .parameter_types
            .get(name)
            .is_some_and(|declared| *declared != required)
        {
            return Err(error(
                at,
                GraphPatternTextErrorKind::ConflictingParameterTypes,
            ));
        }
        Ok(())
    }

    pub(super) fn check_parameter_declarations(&self) -> Result<(), GraphPatternTextError> {
        if self.parameter_types.keys().any(|name| {
            !self
                .syntax
                .parameters
                .iter()
                .any(|spec| spec.name.as_str() == name.as_str())
        }) {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::UnusedParameterDeclaration,
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod projection_schema_tests {
    use super::*;
    use crate::{GraphSetColumnType, PreparedGraphSetText};

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "cost") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }

    fn check(text: &str, expected: &[GraphSetColumnType]) {
        let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
        assert_eq!(prepared.column_types(), expected, "{text}");
        assert_eq!(
            prepared
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .column_types(),
            expected,
            "{text}"
        );
    }

    #[test]
    fn edge_property_output_types_match_the_bound_plan_in_every_projection_form() {
        for terminal in [
            "RETURN e.cost AS cost",
            "WITH e.cost AS cost RETURN cost",
            "WITH e.cost AS cost WHERE cost > 0 RETURN cost + 1 AS next",
        ] {
            check(
                &format!("MATCH (a)-[e:R]->(b) {terminal}"),
                &[GraphSetColumnType::Scalar],
            );
        }
        check(
            "MATCH (a)-[e:R]->(b) RETURN e AS edge, e.cost AS cost, type(e) AS relation",
            &[
                GraphSetColumnType::Edge,
                GraphSetColumnType::Scalar,
                GraphSetColumnType::Scalar,
            ],
        );
    }

    #[test]
    fn scalar_set_operations_accept_edge_and_vertex_property_arms() {
        for operation in ["UNION ALL", "UNION DISTINCT", "INTERSECT", "EXCEPT"] {
            check(
                &format!(
                    "MATCH (a)-[e:R]->(b) RETURN e.cost AS value \
                     {operation} MATCH (n) RETURN n.cost AS value"
                ),
                &[GraphSetColumnType::Scalar],
            );
        }
    }

    #[test]
    fn edge_identity_and_property_arms_refuse_before_catalog_resolution() {
        let mut calls = 0;
        let result = PreparedGraphSetText::prepare(
            "MATCH (a)-[e:R]->(b) RETURN e.cost AS value \
             UNION ALL MATCH (a)-[e:R]->(b) RETURN e AS value",
            |kind, name| {
                calls += 1;
                symbols(kind, name)
            },
        );
        let error = result.unwrap_err();
        assert!(matches!(
            error.kind,
            crate::GraphSetTextErrorKind::SetBuild(crate::GraphSetBuildError::ColumnType { .. })
        ));
        assert_eq!(calls, 0);
    }
}
