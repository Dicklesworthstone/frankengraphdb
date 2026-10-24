//! Explicit argument declarations for the shared graph-text parser.
//! Nonnumeric types come from the caller's prepared-statement contract, never
//! from a database sample, parameter spelling, value, or text substitution.

use super::*;
use crate::set_text::{
    BoundSetTextInput, ReadProjectionTemplate, ReadStageTemplate, ReadValueTemplate,
};
use fgdb_types::CanonicalScalar;

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
        let syntax = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones())
            .parse()?;
        Self::from_syntax(statement, syntax, resolve)
    }

    pub fn prepare_with_parameter_types_and_resolver(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphPatternTextError> {
        let syntax = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones())
            .parse()?;
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
                // Both are opaque to composition delimiters. The original
                // native parser still distinguishes a text value from a key.
                TokenKind::Quoted(_) | TokenKind::DelimitedKeyword(_) => TextKind::Quoted,
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
    /// `deferred` lists the WHERE offsets read after an implicit `WITH *`.
    pub(crate) fn unresolved_for_composition<'a>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
        zones: Option<&crate::GqlTimeZones>,
        deferred: &[usize],
    ) -> Result<UnresolvedGraphText<'a>, crate::GraphSetTextError> {
        let mut parser =
            Parser::new_with_parameter_types(statement, declarations)?.with_zones(zones.cloned());
        parser.deferred_wheres = deferred.to_vec();
        parser.parse_return_for_composition(statement)
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
    pub(super) row_fields: Vec<ReadValueTemplate>,
}
impl UnresolvedGraphText<'_> {
    pub(crate) fn column_schema(&self) -> (Vec<String>, Vec<crate::GraphSetColumnType>) {
        use crate::GraphSetColumnType::{Scalar, Stored, Vertex};
        let column_type = |column: &Column<'_>| match column.path {
            // A pattern comprehension's collected list (fgdb-7nwox).
            _ if column.collect.is_some() => crate::GraphSetColumnType::List,
            Some(GraphPathFunction::Value) => crate::GraphSetColumnType::Path,
            Some(GraphPathFunction::Length) => Scalar,
            Some(GraphPathFunction::Nodes) => crate::GraphSetColumnType::Vertices,
            Some(GraphPathFunction::Edges) => crate::GraphSetColumnType::Edges,
            // Edge selects the property owner in BoundColumn::declaration;
            // an edge property itself is a stored value, not an edge identity.
            Some(GraphPathFunction::Edge) if column.property.is_some() => Stored,
            Some(GraphPathFunction::Edge) => crate::GraphSetColumnType::Edge,
            Some(GraphPathFunction::Labels) => crate::GraphSetColumnType::List,
            Some(GraphPathFunction::Type) => Scalar,
            Some(GraphPathFunction::Properties) => crate::GraphSetColumnType::Any,
            // A property is a scalar or a stored List or Map (fgdb-kr8j1).
            None if column.property.is_some() => Stored,
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
            if let ReadStageTemplate::Arguments {
                columns,
                types: arguments,
            } = stage
            {
                names.clone_from(columns);
                types.clone_from(arguments);
            }
            if let ReadStageTemplate::Aggregate { stage, .. } = stage {
                (names, types) = stage.column_schema();
            }
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
            if let ReadStageTemplate::Page {
                computed: Some(computed),
                ..
            } = stage
            {
                names.truncate(computed.visible);
                types.truncate(computed.visible);
            }
            // A CALL source appends each YIELD alias, all Any, as UNWIND does.
            if let ReadStageTemplate::Call { outputs, .. } = stage {
                for (_, alias) in outputs {
                    names.push(alias.clone());
                    types.push(crate::GraphSetColumnType::Any);
                }
            }
        }
        (names, types)
    }
    pub(crate) fn depth(&self) -> usize {
        1 + self
            .leading
            .iter()
            .map(ReadStageTemplate::depth)
            .sum::<usize>()
            + usize::from(!self.leading.is_empty() && !self.singleton)
            + usize::from(self.projection.is_some())
            + usize::from(!self.correlations.is_empty())
            + 2 * usize::from(!self.row_fields.is_empty())
            + self
                .pipeline
                .iter()
                .map(ReadStageTemplate::depth)
                .sum::<usize>()
    }
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.syntax.parameters
    }
    pub(crate) fn parameter_offsets(&self) -> &[usize] {
        &self.syntax.parameter_offsets
    }

    pub(crate) fn resolve(
        mut self,
        mut resolve: impl GraphSymbolResolver,
    ) -> Result<BoundSetTextInput, GraphPatternTextError> {
        for stage in self.leading.iter_mut().chain(self.pipeline.iter_mut()) {
            resolve_call_symbols(stage, &mut |kind, name| resolve.resolve_symbol(kind, name))?;
        }
        // Every `x.key` read binds its key now, so an element operand reads
        // that stored property at execution with no catalog (fgdb-xnd7x). A
        // key the catalog lacks reads NULL, as `n.key` on a pattern vertex does.
        for (key, slot) in &self.syntax.element_keys {
            slot.resolve(
                match resolve.resolve_symbol(GraphSymbolKind::Property, key) {
                    Some(GraphSymbol::Property(key)) => Some(key),
                    _ => None,
                },
            );
        }
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
                row_fields: self.row_fields,
                owned: None,
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
                let value = resolve.resolve_symbol(kind, name.text).ok_or_else(|| {
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
                    collect: column.collect,
                });
            }
            let clauses: Vec<_> = scopes.iter().map(BoundScope::clause).collect();
            let projected: Vec<_> = columns.iter().map(BoundColumn::declaration).collect();
            built(
                syntax.return_at,
                builder.prepare_values_with_clauses(&clauses, &projected, 0, None),
            )?;
            // `WITH labels(n) AS l` and `WITH type(r) AS t` read catalog names,
            // as a pattern RETURN does.
            let reverse_catalog = columns
                .iter()
                .any(|column| {
                    matches!(
                        column.path,
                        Some(
                            GraphPathFunction::Labels
                                | GraphPathFunction::Type
                                | GraphPathFunction::Properties
                        )
                    )
                })
                .then(|| {
                    std::sync::Arc::new(ReverseSymbolCatalog::from_resolver(
                        &mut resolve,
                        self.statement,
                    ))
                });
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
                reverse_catalog,
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
            row_fields: self.row_fields,
            projection: self.projection,
            quantifier,
            pipeline: self.pipeline,
            owned: None,
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

/// The named procedure arguments whose values are schema names: text
/// literals (or a list of them) that resolve through the catalog at prepare
/// time, exactly as a pattern's labels do, so the host receives identities
/// and a prepared call never re-resolves. Every other argument is a value.
#[must_use]
pub(crate) fn symbol_argument(
    namespace: &str,
    name: &str,
    argument: &str,
) -> Option<GraphSymbolKind> {
    match (namespace, name, argument) {
        ("hybrid", "search", "text_property" | "vector_properties" | "vector_property") => {
            Some(GraphSymbolKind::Property)
        }
        ("hybrid", "search", "label") => Some(GraphSymbolKind::Label),
        ("hybrid", "search", "relation") => Some(GraphSymbolKind::Relation),
        ("fnx", _, "label") => Some(GraphSymbolKind::Label),
        ("fnx", _, "relation") => Some(GraphSymbolKind::Relation),
        ("fnx", _, "weight") => Some(GraphSymbolKind::Property),
        _ => None,
    }
}

/// Replace each schema-name argument of a CALL stage by the integer
/// identity its name resolves to. A miss, a symbol of another kind, or a
/// value that is not a text literal refuses at that argument.
fn resolve_call_symbols(
    stage: &mut ReadStageTemplate,
    resolve: &mut impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
) -> Result<(), GraphPatternTextError> {
    let ReadStageTemplate::Call {
        at,
        namespace,
        name,
        arguments,
        names,
        offsets,
        graph_projection,
        ..
    } = stage
    else {
        return Ok(());
    };
    if namespace == "fnx"
        && names.is_empty()
        && let Some(value @ ReadValueTemplate::MapLiteral { .. }) = arguments.first()
    {
        let at = offsets.first().copied().unwrap_or(*at);
        let projection = resolve_fnx_projection(value, at, resolve)?;
        arguments.remove(0);
        offsets.remove(0);
        *graph_projection = Some(projection);
    }
    for ((argument, value), &at) in names.iter().zip(arguments.iter_mut()).zip(offsets.iter()) {
        let Some(kind) = symbol_argument(namespace, name, argument) else {
            continue;
        };
        let mut one = |value: &mut ReadValueTemplate| {
            let ReadValueTemplate::Literal(literal) = value else {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("a text literal naming a schema symbol"),
                ));
            };
            let CanonicalScalar::Text(text) = literal.value() else {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("a text literal naming a schema symbol"),
                ));
            };
            let symbol = resolve(kind, text.as_str())
                .ok_or_else(|| error(at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
            let id = match symbol {
                GraphSymbol::Label(id) if kind == GraphSymbolKind::Label => id.0,
                GraphSymbol::Relation(id) if kind == GraphSymbolKind::Relation => id.0,
                GraphSymbol::Property(id) if kind == GraphSymbolKind::Property => id.0,
                found => {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::WrongSymbolKind {
                            expected: kind,
                            found: found.kind(),
                        },
                    ));
                }
            };
            let id = i64::try_from(id)
                .ok()
                .and_then(|id| crate::GqlScalarParameter::new(CanonicalScalar::Int(id)).ok())
                .ok_or_else(|| {
                    error(
                        at,
                        GraphPatternTextErrorKind::Expected(
                            "a symbol identity in the Int64 domain",
                        ),
                    )
                })?;
            *value = ReadValueTemplate::Literal(id);
            Ok(())
        };
        match value {
            ReadValueTemplate::List(items) => {
                for item in items {
                    one(item)?;
                }
            }
            value => one(value)?,
        }
    }
    Ok(())
}

/// A fnx call's first inline map is its graph argument, not an opaque runtime
/// parameter. Check every option before consulting the catalog, then freeze
/// exact schema identities. The host still applies authority and the pinned
/// source sequence before reading this induced projection.
fn resolve_fnx_projection(
    value: &ReadValueTemplate,
    at: usize,
    resolve: &mut impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
) -> Result<crate::ProcedureGraphProjection, GraphPatternTextError> {
    use crate::{
        ProcedureGraphDirection as Direction, ProcedureGraphProjection, ProcedureGraphWeight,
        ProcedureMissingWeight as Missing, ProcedureParallelEdges as Parallel,
        ProcedureSelfLoops as Loops,
    };
    let expected = |item| error(at, GraphPatternTextErrorKind::Expected(item));
    let ReadValueTemplate::MapLiteral {
        keys,
        values,
        guard: None,
    } = value
    else {
        return Err(expected("an inline literal procedure graph projection"));
    };
    let mut label = None;
    let mut relation = None;
    let mut weight = None;
    let mut missing = None;
    let mut projection = ProcedureGraphProjection {
        vertex_label: None,
        relation: None,
        weight: ProcedureGraphWeight::Unit,
        direction: None,
        parallel_edges: Parallel::Reject,
        self_loops: Loops::Keep,
    };
    for (key, value) in keys.iter().zip(values) {
        match key.as_ref() {
            "labels" => label = projection_selection_name(value, at)?,
            "relations" => relation = projection_selection_name(value, at)?,
            "weight" => weight = Some(projection_text(value, at)?),
            "missing" => {
                missing = Some(match projection_text(value, at)? {
                    "reject" => Missing::Reject,
                    "unit" => Missing::Unit,
                    "zero" => Missing::Zero,
                    _ => return Err(expected("missing: 'reject', 'unit', or 'zero'")),
                });
            }
            "direction" => {
                projection.direction = Some(match projection_text(value, at)? {
                    "directed" => Direction::Directed,
                    "reversed" => Direction::Reversed,
                    "undirected" => Direction::Undirected,
                    _ => {
                        return Err(expected(
                            "direction: 'directed', 'reversed', or 'undirected'",
                        ));
                    }
                });
            }
            "parallel" => {
                projection.parallel_edges = match projection_text(value, at)? {
                    "reject" => Parallel::Reject,
                    "collapse" => Parallel::CollapseUnit,
                    "min" => Parallel::Minimum,
                    "max" => Parallel::Maximum,
                    "sum" => Parallel::Sum,
                    _ => {
                        return Err(expected(
                            "parallel: 'reject', 'collapse', 'min', 'max', or 'sum'",
                        ));
                    }
                };
            }
            "self_loops" => {
                projection.self_loops = match projection_text(value, at)? {
                    "keep" => Loops::Keep,
                    "drop" => Loops::Drop,
                    "reject" => Loops::Reject,
                    _ => return Err(expected("self_loops: 'keep', 'drop', or 'reject'")),
                };
            }
            _ => {
                return Err(expected(
                    "procedure graph option: labels, relations, weight, missing, direction, parallel, or self_loops",
                ));
            }
        }
    }
    if missing.is_some() && weight.is_none() {
        return Err(expected(
            "a weight property when specifying its missing policy",
        ));
    }
    let mut symbol = |kind, name| {
        let found = resolve(kind, name)
            .ok_or_else(|| error(at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
        if found.kind() != kind {
            return Err(error(
                at,
                GraphPatternTextErrorKind::WrongSymbolKind {
                    expected: kind,
                    found: found.kind(),
                },
            ));
        }
        Ok(match found {
            GraphSymbol::Label(id) => id.0,
            GraphSymbol::Relation(id) => id.0,
            GraphSymbol::Property(id) => id.0,
        })
    };
    if let Some(name) = label {
        projection.vertex_label = Some(LabelId(symbol(GraphSymbolKind::Label, name)?));
    }
    if let Some(name) = relation {
        projection.relation = Some(RelationId(symbol(GraphSymbolKind::Relation, name)?));
    }
    if let Some(name) = weight {
        projection.weight = ProcedureGraphWeight::Property {
            key: PropertyKeyId(symbol(GraphSymbolKind::Property, name)?),
            missing: missing.unwrap_or(Missing::Reject),
        };
    }
    Ok(projection)
}

fn projection_text(value: &ReadValueTemplate, at: usize) -> Result<&str, GraphPatternTextError> {
    if let ReadValueTemplate::Literal(literal) = value
        && let CanonicalScalar::Text(text) = literal.value()
    {
        return Ok(text.as_str());
    }
    Err(error(
        at,
        GraphPatternTextErrorKind::Expected("a text literal in the procedure graph projection"),
    ))
}

fn projection_selection_name(
    value: &ReadValueTemplate,
    at: usize,
) -> Result<Option<&str>, GraphPatternTextError> {
    let ReadValueTemplate::List(names) = value else {
        return Err(error(
            at,
            GraphPatternTextErrorKind::Expected("an empty or singleton list of schema names"),
        ));
    };
    match names.as_slice() {
        [] => Ok(None),
        [name] => projection_text(name, at).map(Some),
        _ => Err(error(
            at,
            GraphPatternTextErrorKind::Expected(
                "at most one schema name in the induced projection",
            ),
        )),
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

    /// A property is a stored value (fgdb-kr8j1), carried as one through WITH;
    /// arithmetic over it is a scalar.
    #[test]
    fn edge_property_output_types_match_the_bound_plan_in_every_projection_form() {
        for (terminal, kind) in [
            ("RETURN e.cost AS cost", GraphSetColumnType::Stored),
            (
                "WITH e.cost AS cost RETURN cost",
                GraphSetColumnType::Stored,
            ),
            (
                "WITH e.cost AS cost WHERE cost > 0 RETURN cost + 1 AS next",
                GraphSetColumnType::Scalar,
            ),
        ] {
            check(&format!("MATCH (a)-[e:R]->(b) {terminal}"), &[kind]);
        }
        check(
            "MATCH (a)-[e:R]->(b) RETURN e AS edge, e.cost AS cost, type(e) AS relation",
            &[
                GraphSetColumnType::Edge,
                GraphSetColumnType::Stored,
                GraphSetColumnType::Scalar,
            ],
        );
    }

    #[test]
    fn stored_set_operations_accept_edge_and_vertex_property_arms() {
        for operation in ["UNION ALL", "UNION DISTINCT", "INTERSECT", "EXCEPT"] {
            check(
                &format!(
                    "MATCH (a)-[e:R]->(b) RETURN e.cost AS value \
                     {operation} MATCH (n) RETURN n.cost AS value"
                ),
                &[GraphSetColumnType::Stored],
            );
            // A scalar arm widens to the stored domain, never narrows it.
            check(
                &format!(
                    "MATCH (a)-[e:R]->(b) RETURN e.cost AS value \
                     {operation} MATCH (n) RETURN 1 AS value"
                ),
                &[GraphSetColumnType::Stored],
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

    #[test]
    fn fnx_projection_names_resolve_once_and_determine_template_identity() {
        let statement = "CALL fnx.pagerank(label => 'Person', relation => 'R', \
                         weight => 'cost') YIELD node RETURN node";
        let mut calls = Vec::new();
        let resolve = |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Label, "Person" | "Human") => Some(GraphSymbol::Label(LabelId(7))),
            _ => symbols(kind, name),
        };
        let prepared = PreparedGraphSetText::prepare(statement, |kind, name| {
            calls.push((kind, name.to_owned()));
            resolve(kind, name)
        })
        .unwrap();
        assert_eq!(
            calls,
            vec![
                (GraphSymbolKind::Label, "Person".to_owned()),
                (GraphSymbolKind::Relation, "R".to_owned()),
                (GraphSymbolKind::Property, "cost".to_owned()),
            ]
        );
        prepared.bind_parameters(&GqlParameters::new()).unwrap();
        let alias = PreparedGraphSetText::prepare(
            "CALL fnx.pagerank(label => 'Human', relation => 'R', \
             weight => 'cost') YIELD node RETURN node",
            resolve,
        )
        .unwrap();
        assert_eq!(
            prepared.canonical_template_bytes(),
            alias.canonical_template_bytes()
        );
        let changed = PreparedGraphSetText::prepare(statement, |kind, name| {
            if kind == GraphSymbolKind::Relation {
                Some(GraphSymbol::Relation(RelationId(2)))
            } else {
                resolve(kind, name)
            }
        })
        .unwrap();
        assert_ne!(
            prepared.canonical_template_bytes(),
            changed.canonical_template_bytes()
        );
    }

    #[test]
    fn fnx_projection_schema_errors_refuse_even_for_an_empty_page() {
        for (argument, kind) in [
            ("label", GraphSymbolKind::Label),
            ("relation", GraphSymbolKind::Relation),
            ("weight", GraphSymbolKind::Property),
        ] {
            let statement = format!(
                "CALL fnx.pagerank({argument} => 'Missing') YIELD node RETURN node LIMIT 0"
            );
            let error = PreparedGraphSetText::prepare(&statement, |_, _| None).unwrap_err();
            assert_eq!(
                error.kind,
                crate::GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::UnknownSymbol(
                    kind
                ))
            );
            let found = if kind == GraphSymbolKind::Label {
                GraphSymbol::Relation(RelationId(1))
            } else {
                GraphSymbol::Label(LabelId(1))
            };
            let error = PreparedGraphSetText::prepare(&statement, |_, _| Some(found)).unwrap_err();
            assert_eq!(
                error.kind,
                crate::GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::WrongSymbolKind {
                    expected: kind,
                    found: found.kind(),
                })
            );
            for value in ["1", "NULL", "$dynamic"] {
                let statement = format!(
                    "CALL fnx.pagerank({argument} => {value}) YIELD node RETURN node LIMIT 0"
                );
                let mut calls = 0;
                assert!(
                    PreparedGraphSetText::prepare(&statement, |_, _| {
                        calls += 1;
                        None
                    })
                    .is_err()
                );
                assert_eq!(calls, 0, "{statement}");
            }
        }
    }
}
