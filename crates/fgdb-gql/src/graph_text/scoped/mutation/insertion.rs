//! INSERT and CREATE share the lexer, scoped MATCH compiler and scalar parser.
//! This module only prepares templates. Storage and identity allocation are
//! absent from parsing/binding; the ordinary insertion/write paths own both.

mod chain;
mod query;

use super::*;
use crate::insertion::{
    GraphInsertBuildError, GraphInsertEdge, GraphInsertEndpoint, GraphInsertVertex,
    MAX_GRAPH_INSERT_DECLARATIONS, MAX_GRAPH_INSERT_FIELDS, PreparedGraphInsert,
};
use crate::insertion_text::{
    GraphInsertTextError, GraphInsertTextErrorKind, InsertEdgeTemplate, InsertTextInput,
    InsertVertexTemplate, PreparedGraphInsertText,
};
use crate::set_text::{BoundSetTextInput, ReadStageTemplate, ReadValueTemplate};
use crate::{GraphIntegerExpression, GraphIntegerOp, GraphSetColumnType, PreparedGraphSet};

type ParsedFields<'a> = Vec<(Name<'a>, ReadValueTemplate)>;
struct NewVertex<'a> {
    name: Option<Name<'a>>,
    labels: Vec<Name<'a>>,
    properties: ParsedFields<'a>,
}
struct NewEdge<'a> {
    name: Option<Name<'a>>,
    source: GraphInsertEndpoint,
    destination: GraphInsertEndpoint,
    relation: Name<'a>,
    properties: ParsedFields<'a>,
}
struct InsertionSyntax<'a> {
    projections: Vec<Projection<'a>>,
    vertices: Vec<NewVertex<'a>>,
    edges: Vec<NewEdge<'a>>,
}

/// One statement-wide catalog cache across the read and write compilers.
/// Forward the host's metadata and time zones, not just its forward symbols.
struct InsertionResolver<R> {
    inner: R,
    symbols: BTreeMap<(GraphSymbolKind, String), Option<GraphSymbol>>,
}

impl<R: GraphSymbolResolver> GraphSymbolResolver for InsertionResolver<R> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        let key = (kind, name.to_owned());
        if let Some(value) = self.symbols.get(&key) {
            return *value;
        }
        let value = self.inner.resolve_symbol(kind, name);
        self.symbols.insert(key, value);
        value
    }

    fn time_zones(&self) -> Option<crate::GqlTimeZones> {
        self.inner.time_zones()
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        self.inner.reverse_catalog()
    }

    fn reverse_label(&self, id: LabelId) -> Option<String> {
        self.inner.reverse_label(id)
    }

    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.inner.reverse_relation(id)
    }
}

/// One prepared write of a statement, its RETURN or WITH head, and the next
/// write of a chain when its WITH tail ended at one.
pub(crate) type InsertStep<'a> = (
    PreparedGraphInsertText,
    Option<crate::insertion_query_text::InsertReturnTemplate>,
    Option<ChainContinuation<'a>>,
);

impl<'a> Parser<'a> {
    /// CREATE clauses over a WITH row, then its RETURN or WITH head. A
    /// chained tail stops at the next write, which is the continuation;
    /// otherwise the statement must end here.
    fn insertion_over_row(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        returning: bool,
    ) -> Result<
        (
            InsertionSyntax<'a>,
            Option<query::ParsedReturn<'a>>,
            Option<ChainContinuation<'a>>,
        ),
        GraphInsertTextError,
    > {
        let at = self.current.at;
        self.write_row_scope = true;
        let mut parsed = self.insertion_clauses(Some(schema))?;
        // A chain step's head follows its clauses: a WITH head, a RETURN, or
        // (directly before the next write) the implicit `WITH *`.
        let returning = returning
            || (self.chain_write
                && (self.is_word("WITH") || self.is_word("RETURN") || self.at_chained_write()?));
        let returning = if returning {
            Some(self.insertion_return(&mut parsed, schema)?)
        } else {
            None
        };
        let continuation = self.continuation();
        if continuation.is_none() {
            self.end()?;
        }
        self.write_row_scope = false;
        if !parsed.projections.is_empty() {
            return Err(expected(at, "a CREATE that reads only the WITH row"));
        }
        if let Some(returning) = &returning {
            returning.admit(&self.syntax.parameters)?;
        }
        Ok((parsed, returning, continuation))
    }
}

fn insertion_build(at: usize, kind: GraphInsertBuildError) -> GraphInsertTextError {
    GraphInsertTextError {
        offset: at,
        kind: GraphInsertTextErrorKind::Build(kind),
    }
}
fn expected(at: usize, message: &'static str) -> GraphInsertTextError {
    error(at, GraphPatternTextErrorKind::Expected(message)).into()
}

/// Parsed source-free WITH/UNWIND stages and their final imported schema.
type UnwindPrefix<'a> = (Vec<ReadStageTemplate>, Vec<(Name<'a>, GraphSetColumnType)>);
type GraphRowPrefix<'a> = (
    crate::graph_text::parameters::UnresolvedGraphText<'a>,
    Vec<(Name<'a>, GraphSetColumnType)>,
);

impl<'a> Parser<'a> {
    /// The MATCH scope is shared by CREATE endpoints and RETURN. New names
    /// must not shadow a matched edge or path by silently declaring a vertex.
    pub(super) fn insertion_match_kind(&self, name: &str) -> Option<GraphSetColumnType> {
        // After WITH only the row is in scope (fgdb-sl4ym).
        if self.write_row_scope {
            None
        } else if self.syntax.names_path(name) {
            Some(GraphSetColumnType::Path)
        } else if self.syntax.visible_edge(name).is_some() {
            Some(GraphSetColumnType::Edge)
        } else if self
            .syntax
            .variables
            .iter()
            .any(|variable| variable.text == name)
        {
            Some(GraphSetColumnType::Vertex)
        } else {
            None
        }
    }

    /// Parse source-free WITH/UNWIND through the ordinary relational grammar.
    /// The prefix ends at CREATE/INSERT or its MATCH source, without inventing
    /// a RETURN or evaluating expressions. Script preflight uses this same
    /// scope, including projection, filtering, grouping, DISTINCT and pages.
    pub(super) fn insertion_unwind_prefix(
        &mut self,
    ) -> Result<UnwindPrefix<'a>, crate::GraphSetTextError> {
        if !self.is_word("UNWIND") && !self.is_word("WITH") {
            return Ok((Vec::new(), Vec::new()));
        }
        let (stages, schema, _) = self.row_pipeline_prefix(Vec::new())?;
        Ok((stages, schema))
    }

    /// Imports feed MATCH's existing correlation compiler. A graph binding
    /// must have a new name in this write profile: never shadow an imported
    /// value or silently interpret a scalar as an existing endpoint.
    pub(super) fn insertion_match_prefix(
        &mut self,
        imports: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("MATCH") {
            return Ok(false);
        }
        self.read_row_bindings = imports.iter().map(|(name, _)| *name).collect();
        self.parse_match_prefix()?;
        for &(name, _) in imports {
            if self.insertion_match_kind(name.text).is_some() {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected(
                        "MATCH bindings distinct from imported row aliases",
                    ),
                ));
            }
        }
        Ok(true)
    }

    /// A MATCH-first UNWIND selects the native graph-to-row compiler (a
    /// MATCH-first WITH takes the write-row pipeline, `prepare_after_with`).
    /// The MATCH parser has identified the boundary; restart that same bounded
    /// statement with its row-source owner, not a rewritten RETURN or a
    /// fallback after a refused parse. Its parameter table remains live through
    /// CREATE, while take_part_syntax removes the old graph-local scope.
    pub(super) fn insertion_graph_row_prefix(
        &mut self,
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
        zones: Option<crate::GqlTimeZones>,
    ) -> Result<GraphRowPrefix<'a>, crate::GraphSetTextError> {
        *self = Self::new_with_parameter_types(statement, declarations)?.with_zones(zones);
        let (source, schema, _) = self.leading_aggregate_prefix(statement)?;
        Ok((source, schema))
    }

    /// Resolve the combined row (imports, then projected MATCH fields) while
    /// using the ordinary scalar compiler. The graph child is evaluated once;
    /// these are projection slots, not deferred reads of the created graph.
    pub(super) fn insertion_source_value(
        &mut self,
        inputs: &mut Vec<Projection<'a>>,
        imports: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<ReadValueTemplate, crate::GraphSetTextError> {
        // A read scope: an imported map's `row.key` reads its entry.
        self.read_resolved_value(
            &mut |parser| {
                // After WITH, `a.key` of an element the row carries is that
                // row's hidden read (fgdb-sl4ym).
                if let Some(column) = parser.boundary_read(imports.len())? {
                    return Ok(Some(column));
                }
                let TokenKind::Word(word) = parser.current.kind else {
                    return Ok(None);
                };
                if parser.insertion_match_kind(word).is_some() {
                    let name = parser.any_variable()?;
                    let property = if parser.take(b'.')? {
                        Some(parser.name()?)
                    } else {
                        None
                    };
                    let column = parser.mutation_projection(inputs, name, property)?;
                    return Ok(Some(imports.len() + column));
                }
                if let Some(column) = imports.iter().position(|(name, _)| name.text == word) {
                    parser.advance()?;
                    return Ok(Some(column));
                }
                Ok(None)
            },
            0,
        )
    }

    fn insertion_input_types(
        &self,
        imports: &[(Name<'a>, GraphSetColumnType)],
        inputs: &[Projection<'a>],
    ) -> Vec<GraphSetColumnType> {
        imports
            .iter()
            .map(|(_, kind)| *kind)
            .chain(inputs.iter().map(|input| {
                if input.property.is_some() {
                    GraphSetColumnType::Stored
                } else {
                    match input.path {
                        Some(GraphPathFunction::Value) => GraphSetColumnType::Path,
                        Some(GraphPathFunction::Length | GraphPathFunction::Type) => {
                            GraphSetColumnType::Scalar
                        }
                        Some(GraphPathFunction::Nodes) => GraphSetColumnType::Vertices,
                        Some(GraphPathFunction::Edges) => GraphSetColumnType::Edges,
                        Some(GraphPathFunction::Edge) => GraphSetColumnType::Edge,
                        Some(GraphPathFunction::Labels) => GraphSetColumnType::List,
                        Some(GraphPathFunction::Properties) => GraphSetColumnType::Any,
                        None => GraphSetColumnType::Vertex,
                    }
                }
            }))
            .collect()
    }

    fn insertion_field_capacity(&self, fields: &mut usize) -> Result<(), GraphInsertTextError> {
        if *fields >= MAX_GRAPH_INSERT_FIELDS {
            return Err(insertion_build(
                self.current.at,
                GraphInsertBuildError::TooManyFields {
                    limit: MAX_GRAPH_INSERT_FIELDS,
                    observed: *fields + 1,
                },
            ));
        }
        *fields += 1;
        Ok(())
    }

    fn insertion_declaration_capacity(&self, count: usize) -> Result<(), GraphInsertTextError> {
        if count >= MAX_GRAPH_INSERT_DECLARATIONS {
            return Err(insertion_build(
                self.current.at,
                GraphInsertBuildError::TooManyDeclarations {
                    limit: MAX_GRAPH_INSERT_DECLARATIONS,
                    observed: count + 1,
                },
            ));
        }
        Ok(())
    }

    /// Hold a value-language CREATE property to the written property law:
    /// a scalar or a stored List (fgdb-k3nub).
    fn scalar_property_value(
        &self,
        value: ReadValueTemplate,
        at: usize,
        imports: &[(Name<'a>, GraphSetColumnType)],
        inputs: &[Projection<'a>],
    ) -> Result<ReadValueTemplate, GraphInsertTextError> {
        let types = self.insertion_input_types(imports, inputs);
        let kind = value.column_type(&types, &self.syntax.parameters);
        let admitted = match value {
            // A parameter binds a scalar argument only; a list parameter is
            // not yet a written List.
            ReadValueTemplate::Parameter { .. } => {
                matches!(kind, GraphSetColumnType::Scalar | GraphSetColumnType::Any)
            }
            _ => matches!(
                kind,
                GraphSetColumnType::Scalar
                    | GraphSetColumnType::Stored
                    | GraphSetColumnType::List
                    | GraphSetColumnType::Any
            ),
        };
        if !admitted {
            return Err(expected(at, "scalar or list CREATE property expression"));
        }
        // A map construction is never a written property (openCypher
        // InvalidPropertyType) until the owner rules on stored maps. A list
        // that holds a map or a graph element, a map entry (`row.name`) and
        // every other composite are checked per row at execution.
        if matches!(
            value,
            ReadValueTemplate::MapLiteral { .. } | ReadValueTemplate::MapOverlay { .. }
        ) {
            return Err(expected(at, "scalar or list CREATE property expression"));
        }
        Ok(value)
    }

    fn insertion_properties(
        &mut self,
        inputs: &mut Vec<Projection<'a>>,
        fields: &mut usize,
        row_schema: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<ParsedFields<'a>, GraphInsertTextError> {
        let mut properties: ParsedFields<'a> = Vec::new();
        if !self.take(b'{')? {
            return Ok(properties);
        }
        if self.take(b'}')? {
            return Ok(properties);
        }
        loop {
            self.insertion_field_capacity(fields)?;
            self.capacity(
                properties.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let key = self.name()?;
            if properties.iter().any(|(old, _)| old.text == key.text) {
                return Err(expected(key.at, "unique CREATE property key"));
            }
            self.punct(b':', ":")?;
            let at = self.current.at;
            let value = if let Some(schema) = row_schema {
                let value = if self.syntax.variables.is_empty() {
                    self.read_row_value(schema, 0)?
                } else {
                    self.insertion_source_value(inputs, schema)?
                };
                self.scalar_property_value(value, at, schema, inputs)?
            } else if self.list_chain_follows(None)? {
                // A `+` chain with a list operand (`[1] + [2]`, `n.p + [3]`)
                // reads whole through the list sum over the MATCH columns,
                // as a SET list value does (fgdb-k3nub).
                let value = self.read_graph_value(inputs, 0)?;
                self.scalar_property_value(value, at, &[], inputs)?
            } else if self.starts_let_operand()? || self.is_punct(b'[') {
                // A temporal constructor, a quantifier, a list function,
                // keys() or size() is a value-language operand that the
                // scalar compiler reads only through a let slot, which a
                // CREATE property has none of: read the whole property in
                // the value language, as a row-fed CREATE does
                // (fgdb-txbg9, fgdb-o5o9t). So is a list literal, which a
                // stored List property may now be (fgdb-k3nub).
                let value = self.insertion_source_value(inputs, &[])?;
                self.scalar_property_value(value, at, &[], inputs)?
            } else if matches!(self.current.kind, TokenKind::Parameter(name)
                if self.parameter_types.get(name) == Some(&GqlParameterType::Map))
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
            {
                // A map argument's selected field uses the same composite
                // value compiler as UNWIND input fields. No row source is
                // needed for an immutable bound argument; the final property
                // still passes the native scalar-only write admission.
                self.insertion_source_value(inputs, &[])?
            } else {
                match self.mutation_expression(inputs)? {
                    Operand::Column(column) => ReadValueTemplate::Column(column),
                    Operand::Literal(value) => ReadValueTemplate::Literal(value),
                    Operand::Number(Number::Literal(value)) => {
                        ReadValueTemplate::Literal(scalar(value, at)?)
                    }
                    Operand::Number(Number::Parameter(index)) => ReadValueTemplate::parameter(
                        index,
                        at,
                        self.syntax.parameters[index].parameter_type,
                    ),
                    Operand::Integer { program, at } => ReadValueTemplate::Integer { program, at },
                }
            };
            properties.push((key, value));
            if self.take(b'}')? {
                break;
            }
            self.punct(b',', ", or }")?;
        }
        Ok(properties)
    }

    /// One node grammar for standalone clauses and every endpoint in a chain.
    /// Anonymous nodes are distinct declarations without invented identifiers.
    /// New names enter only the endpoint registry, never the scalar read scope.
    fn insertion_node(
        &mut self,
        parsed: &mut InsertionSyntax<'a>,
        fields: &mut usize,
        pending_edges: usize,
        row_schema: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<GraphInsertEndpoint, GraphInsertTextError> {
        self.punct(b'(', "(")?;
        let at = self.current.at;
        let name = if matches!(self.current.kind, TokenKind::Word(_)) {
            Some(self.name()?)
        } else {
            None
        };
        let existing = if let Some(name) = name {
            if parsed
                .edges
                .iter()
                .any(|edge| edge.name.is_some_and(|old| old.text == name.text))
                || matches!(
                    self.insertion_match_kind(name.text),
                    Some(GraphSetColumnType::Edge | GraphSetColumnType::Path)
                )
            {
                return Err(expected(
                    name.at,
                    "CREATE vertex name distinct from edge and path bindings",
                ));
            }
            if let Some((column, (_, kind))) = row_schema.and_then(|schema| {
                schema
                    .iter()
                    .enumerate()
                    .find(|(_, (alias, _))| alias.text == name.text)
            }) {
                // A vertex an imported row carries is that row's endpoint: the
                // WITH row of a write after WITH (fgdb-sl4ym), or a MATCH-first
                // UNWIND row that projected the matched vertex.
                if *kind != GraphSetColumnType::Vertex {
                    return Err(expected(
                        name.at,
                        if self.write_row_scope {
                            "a vertex-valued WITH binding"
                        } else {
                            "vertex-typed imported CREATE endpoint"
                        },
                    ));
                }
                Some(GraphInsertEndpoint::Column(column))
            } else if !self.write_row_scope
                && self
                    .syntax
                    .variables
                    .iter()
                    .any(|variable| variable.text == name.text)
            {
                let column = self.mutation_projection(&mut parsed.projections, name, None)?;
                Some(GraphInsertEndpoint::Column(
                    row_schema.map_or(0, |schema| schema.len()) + column,
                ))
            } else {
                parsed
                    .vertices
                    .iter()
                    .position(|vertex| vertex.name.is_some_and(|old| old.text == name.text))
                    .map(GraphInsertEndpoint::CreatedVertex)
            }
        } else {
            None
        };
        if let Some(endpoint) = existing {
            // Even an empty property map is declaration syntax, not a second
            // assignment to the already-bound node. Refuse instead of ignoring.
            if self.is_punct(b':') || self.is_punct(b'{') {
                return Err(expected(
                    at,
                    "bare reference to an already-bound CREATE node",
                ));
            }
            self.punct(b')', ")")?;
            return Ok(endpoint);
        }
        self.insertion_declaration_capacity(
            parsed.vertices.len() + parsed.edges.len() + pending_edges,
        )?;
        let mut labels: Vec<Name<'a>> = Vec::new();
        while self.take(b':')? {
            self.insertion_field_capacity(fields)?;
            let label = self.name()?;
            if labels.iter().any(|old| old.text == label.text) {
                return Err(expected(label.at, "unique CREATE label"));
            }
            labels.push(label);
        }
        let properties = self.insertion_properties(&mut parsed.projections, fields, row_schema)?;
        self.punct(b')', ")")?;
        let vertex = parsed.vertices.len();
        parsed.vertices.push(NewVertex {
            name,
            labels,
            properties,
        });
        Ok(GraphInsertEndpoint::CreatedVertex(vertex))
    }

    fn insertion_clauses(
        &mut self,
        row_schema: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<InsertionSyntax<'a>, GraphInsertTextError> {
        let create_at = self.current.at;
        if !self.take_word("INSERT")? {
            self.word("CREATE")?;
        }
        let mut parsed = InsertionSyntax {
            projections: Vec::new(),
            vertices: Vec::new(),
            edges: Vec::new(),
        };
        let mut fields = 0;
        loop {
            let mut left = self.insertion_node(&mut parsed, &mut fields, 0, row_schema)?;
            while self.is_punct(b'-') || self.is_punct(b'<') {
                self.insertion_declaration_capacity(parsed.vertices.len() + parsed.edges.len())?;
                let incoming = self.take(b'<')?;
                self.punct(b'-', "-")?;
                self.punct(b'[', "[")?;
                // A name binds the edge for RETURN; without one it is only a
                // declaration, which openCypher allows on any CREATE.
                let name = if matches!(self.current.kind, TokenKind::Word(_)) {
                    let name = self.name()?;
                    if self.insertion_match_kind(name.text).is_some()
                        || row_schema.is_some_and(|schema| {
                            schema.iter().any(|(old, _)| old.text == name.text)
                        })
                        || parsed
                            .vertices
                            .iter()
                            .any(|vertex| vertex.name.is_some_and(|old| old.text == name.text))
                        || parsed
                            .edges
                            .iter()
                            .any(|edge| edge.name.is_some_and(|old| old.text == name.text))
                    {
                        return Err(expected(name.at, "new CREATE edge binding"));
                    }
                    Some(name)
                } else {
                    None
                };
                self.punct(b':', ":")?;
                let relation = self.name()?;
                let properties =
                    self.insertion_properties(&mut parsed.projections, &mut fields, row_schema)?;
                self.punct(b']', "]")?;
                self.punct(b'-', "-")?;
                let outgoing = self.take(b'>')?;
                if incoming == outgoing {
                    return Err(expected(relation.at, "exactly one directed CREATE arrow"));
                }
                // Reserve this not-yet-pushed edge while the right node may
                // admit another vertex. A chain cannot step past the total cap.
                let right = self.insertion_node(&mut parsed, &mut fields, 1, row_schema)?;
                if let Some(name) = name
                    && parsed
                        .vertices
                        .iter()
                        .any(|vertex| vertex.name.is_some_and(|old| old.text == name.text))
                {
                    return Err(expected(
                        name.at,
                        "CREATE edge name distinct from vertex bindings",
                    ));
                }
                let (source, destination) = if incoming {
                    (right, left)
                } else {
                    (left, right)
                };
                parsed.edges.push(NewEdge {
                    name,
                    source,
                    destination,
                    relation,
                    properties,
                });
                left = right;
            }
            // Consecutive CREATE clauses with no reading clause between them
            // run on the same row, so CREATE p1 CREATE p2 is CREATE p1, p2 and
            // p2 may reuse p1's bindings.
            if !self.take(b',')? && !self.take_word("CREATE")? && !self.take_word("INSERT")? {
                break;
            }
        }
        if parsed.vertices.is_empty() && parsed.edges.is_empty() {
            return Err(insertion_build(create_at, GraphInsertBuildError::Empty));
        }
        // After WITH the row carries the bag, and no MATCH field is read.
        if parsed.projections.is_empty()
            && !self.write_row_scope
            && let Some(&root) = self.syntax.variables.first()
        {
            // A MATCH still carries its occurrence bag even for constants.
            // Standalone CREATE has no graph inputs and needs no placeholder.
            self.mutation_projection(&mut parsed.projections, root, None)?;
        }
        Ok(parsed)
    }
}

fn resolve_properties<'a>(
    properties: ParsedFields<'a>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<Vec<(PropertyKeyId, ReadValueTemplate)>, GraphInsertTextError> {
    let mut resolved = Vec::new();
    for (key, value) in properties {
        let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
            unreachable!("shared catalog resolver checked the domain")
        };
        resolved.push((key, value));
    }
    Ok(resolved)
}

// None instantiates only checked shape: parameters become nullable placeholders
// and no arithmetic is evaluated. Some binds the exact validated argument row.
// A composite value binds against `shape` (placeholder arguments) when None.
fn bind_fields(
    fields: &[(PropertyKeyId, ReadValueTemplate)],
    values: Option<&[GqlParameterValue]>,
    shape: &[GqlParameterValue],
) -> Result<Vec<(PropertyKeyId, GraphMutationValue)>, GraphInsertTextError> {
    let mut properties = Vec::new();
    for (key, value) in fields {
        let value = match value {
            ReadValueTemplate::Column(column) => GraphMutationValue::Column(*column),
            ReadValueTemplate::Literal(value) => GraphMutationValue::Literal(value.clone()),
            ReadValueTemplate::Parameter { index, at, .. } => {
                let value = match values {
                    Some(values) => scalar(values[*index].clone(), *at)?,
                    None => GqlScalarParameter::new(CanonicalScalar::Null)
                        .map_err(|_| error(*at, GraphPatternTextErrorKind::ScalarLiteral))?,
                };
                GraphMutationValue::Literal(value)
            }
            ReadValueTemplate::Integer { program, at } => {
                let expression = match values {
                    Some(values) => integer::bind_integer(program, values, *at)?,
                    None => {
                        let shape: Vec<_> = program
                            .iter()
                            .map(|op| match op {
                                MutationIntegerTemplateOp::Bound(op) => op.clone(),
                                MutationIntegerTemplateOp::Parameter { .. }
                                | MutationIntegerTemplateOp::ParameterField { .. } => {
                                    GraphIntegerOp::Literal(None)
                                }
                            })
                            .collect();
                        GraphIntegerExpression::prepare_scalar(&shape).map_err(|kind| {
                            GraphMutationTextError {
                                offset: *at,
                                kind: GraphMutationTextErrorKind::IntegerExpression(kind),
                            }
                        })?
                    }
                };
                GraphMutationValue::Expression(expression)
            }
            ReadValueTemplate::Index { .. }
            | ReadValueTemplate::Size(_)
            | ReadValueTemplate::In { .. }
            | ReadValueTemplate::Equal { .. }
            | ReadValueTemplate::Ordered { .. }
            | ReadValueTemplate::Temporal { .. }
            | ReadValueTemplate::Function { .. }
            | ReadValueTemplate::Case { .. }
            | ReadValueTemplate::Random { .. }
            | ReadValueTemplate::Quantifier { .. }
            | ReadValueTemplate::Reduce { .. }
            | ReadValueTemplate::MapGet { .. }
            | ReadValueTemplate::Let { .. }
            // A list-valued field is a stored List (fgdb-k3nub). Execution
            // refuses a list that holds a map or a graph element.
            | ReadValueTemplate::List(_)
            | ReadValueTemplate::Local(_)
            | ReadValueTemplate::Comprehension { .. }
            | ReadValueTemplate::Slice { .. }
            | ReadValueTemplate::Range { .. }
            | ReadValueTemplate::Keys(_) => GraphMutationValue::Composite(
                return_projection::bind_read_value(value, values.unwrap_or(shape))?,
            ),
            ReadValueTemplate::MapLiteral { .. } | ReadValueTemplate::MapOverlay { .. } => {
                // A map is not a written property value (openCypher
                // InvalidPropertyType) until the owner rules on stored maps.
                return Err(GraphInsertTextError {
                    offset: 0,
                    kind: GraphInsertTextErrorKind::Expression(
                        GraphMutationTextErrorKind::IntegerOperand,
                    ),
                });
            }
        };
        properties.push((*key, value));
    }
    Ok(properties)
}

// These values compile the original parameterized operator's shape only.
// They never evaluate an UNWIND list or allocate an identity.
pub(super) fn shape_arguments(parameters: &[GqlParameterSpec]) -> Vec<GqlParameterValue> {
    parameters
        .iter()
        .map(|spec| match spec.parameter_type {
            GqlParameterType::Int64 => GqlParameterValue::Int64(0),
            GqlParameterType::UInt64 => GqlParameterValue::UInt64(0),
            GqlParameterType::Scalar(_) => GqlParameterValue::Scalar(
                GqlScalarParameter::new(CanonicalScalar::Null).expect("canonical null"),
            ),
            GqlParameterType::List => GqlParameterValue::List(
                crate::GqlListParameter::new(Vec::new()).expect("bounded empty list"),
            ),
            GqlParameterType::Map => GqlParameterValue::Map(
                crate::GqlMapParameter::new(Vec::new()).expect("bounded empty map"),
            ),
        })
        .collect()
}

impl PreparedGraphInsertText {
    /// Prepare INSERT (a:Label {p:$value})-[:R]->(b), optionally after MATCH.
    /// The openCypher CREATE spelling lowers to the identical insertion program.
    /// Names are declared on first occurrence and bare repetitions share their
    /// created vertex; anonymous nodes always declare distinct vertices. Chains,
    /// cycles, self-loops and incoming arrows lower to the same typed template.
    /// All vertices are emitted before their edges for each input occurrence.
    /// The explicit relation parameter is verified against every created edge.
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphInsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    /// The original parser's parameter table covers every predicate and created
    /// property, including CASE, arithmetic and scalar payloads. Syntax/limits
    /// pass before catalog callbacks. Each name/domain resolves once, across
    /// MATCH and CREATE when both are present. Standalone creation has no graph
    /// input definition and never synthesizes one merely to validate arguments.
    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphInsertTextError> {
        Self::prepare_definition(statement, relation, declarations, resolve, false)
            .map(|(insertion, _)| insertion)
    }

    fn prepare_definition(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
        returning: bool,
    ) -> Result<
        (
            Self,
            Option<crate::insertion_query_text::InsertReturnTemplate>,
        ),
        GraphInsertTextError,
    > {
        Self::prepare_step(statement, relation, declarations, resolve, returning, false)
            .map(|(insertion, returning, _)| (insertion, returning))
    }

    /// The first write of a statement. With `chain`, a WITH tail may end at
    /// the next CREATE instead of a RETURN: that write is the continuation.
    pub(crate) fn prepare_step<'a>(
        statement: &'a str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
        returning: bool,
        chain: bool,
    ) -> Result<InsertStep<'a>, GraphInsertTextError> {
        let mut resolve = InsertionResolver {
            inner: resolve,
            symbols: BTreeMap::new(),
        };
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        parser.chain_write = chain;
        let (leading, mut row_schema) = parser.insertion_unwind_prefix()?;
        let mut matched = parser.insertion_match_prefix(&row_schema)?;
        // MATCH ... WITH ... CREATE reads the WITH row through the write-row
        // pipeline that SET, REMOVE and DELETE after WITH share (fgdb-sl4ym).
        if matched && leading.is_empty() && parser.is_word("WITH") {
            return Self::prepare_after_with(parser, statement, relation, resolve, returning);
        }
        let read_source = if leading.is_empty() && matched && parser.is_word("UNWIND") {
            let (source, schema) =
                parser.insertion_graph_row_prefix(statement, declarations, resolve.time_zones())?;
            row_schema = schema;
            matched = false;
            Some(source)
        } else {
            None
        };
        let row_input = read_source.is_some() || !leading.is_empty();
        let at = parser.current.at;
        let mut parsed = parser.insertion_clauses(row_input.then_some(row_schema.as_slice()))?;
        let returning = if returning {
            Some(parser.insertion_return(&mut parsed, &row_schema)?)
        } else {
            None
        };
        let continuation = parser.continuation();
        if continuation.is_none() {
            parser.end()?;
        }
        // Join keys may be absent from CREATE and RETURN. Append their hidden
        // fields without shifting any previously compiled expression slot.
        let mut correlations = Vec::new();
        for (variable, key, row) in core::mem::take(&mut parser.read_correlations) {
            let column = parser.mutation_projection(&mut parsed.projections, variable, key)?;
            correlations.push((row, column));
        }
        let row_fields = core::mem::take(&mut parser.read_row_fields);
        let input_types = parser.insertion_input_types(&row_schema, &parsed.projections);
        let joined_width = input_types.len() + row_fields.len();
        if row_input && joined_width > MAX_PATTERN_VERTICES {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                    dimension: crate::algebra::PatternLimitDimension::Columns,
                    limit: MAX_PATTERN_VERTICES,
                    observed: joined_width,
                }),
            )
            .into());
        }
        if let Some(returning) = &returning {
            returning.admit(&parser.syntax.parameters)?;
        }
        let syntax = parser.syntax;
        let relational = if let Some(mut source) = read_source {
            // CREATE/RETURN may add parameters after the read boundary. Every
            // stage binds the complete table, with the original indices and
            // occurrences; no source-only schema or argument re-resolution.
            source.syntax.parameters.clone_from(&syntax.parameters);
            source
                .syntax
                .parameter_offsets
                .clone_from(&syntax.parameter_offsets);
            source
                .syntax
                .element_keys
                .extend(syntax.element_keys.iter().cloned());
            let depth = source.depth();
            if depth + 1 > crate::MAX_GRAPH_SET_DEPTH {
                return Err(insertion_build(
                    at,
                    GraphInsertBuildError::RelationalInput(crate::GraphSetBuildError::TooDeep {
                        limit: crate::MAX_GRAPH_SET_DEPTH,
                        observed: depth + 1,
                    }),
                ));
            }
            if returning.is_some() && depth + 2 > crate::MAX_GRAPH_SET_DEPTH {
                return Err(GraphInsertTextError {
                    offset: at,
                    kind: GraphInsertTextErrorKind::ReturnBuild(
                        crate::GraphInsertQueryBuildError::InputDepth(
                            crate::GraphSetBuildError::TooDeep {
                                limit: crate::MAX_GRAPH_SET_DEPTH,
                                observed: depth + 2,
                            },
                        ),
                    ),
                });
            }
            let values = shape_arguments(&syntax.parameters);
            for (_, value) in parsed
                .vertices
                .iter()
                .flat_map(|v| &v.properties)
                .chain(parsed.edges.iter().flat_map(|e| &e.properties))
            {
                let value = return_projection::bind_read_value(value, &values)?;
                crate::GraphSetProjection::admit_output(&value, &input_types, 0).map_err(
                    |kind| crate::GraphSetTextError {
                        offset: at,
                        kind: crate::GraphSetTextErrorKind::ProjectionBuild(kind),
                    },
                )?;
            }
            // The selection, projection, DISTINCT, grouping, filter, UNWIND
            // and pages stay one native input. An empty MATCH is never replaced
            // with a unit row, nor is its occurrence bag deduplicated here.
            Some(source.resolve(crate::graph_text::LentResolver(&mut resolve))?)
        } else if leading.is_empty() {
            None
        } else {
            let input = BoundSetTextInput {
                selection: None,
                parameters: syntax.parameters.clone(),
                parameter_offsets: syntax.parameter_offsets.clone(),
                return_at: statement.len(),
                projection: None,
                quantifier: crate::GraphSetQuantifier::All,
                pipeline: Vec::new(),
                singleton: true,
                leading,
                correlations: Vec::new(),
                row_fields,
                owned: None,
            };
            // Compile all source and property expression shapes before any
            // catalog callback, including statically unreachable empty lists.
            let values = shape_arguments(&syntax.parameters);
            let shape = input.bind_values(&values)?;
            if !input.row_fields.is_empty() {
                input.bind_row_fields(shape.clone(), &values)?;
            }
            // A matched child adds a cross product and, when correlated, a
            // filter. Admit those ancestors BEFORE any catalog callback.
            let ancestors = 1
                + usize::from(matched)
                + usize::from(!correlations.is_empty())
                + 2 * usize::from(!input.row_fields.is_empty());
            shape.check_ancestor_depth(ancestors).map_err(|kind| {
                insertion_build(at, GraphInsertBuildError::RelationalInput(kind))
            })?;
            if returning.is_some() {
                shape
                    .check_ancestor_depth(ancestors + 1)
                    .map_err(|kind| GraphInsertTextError {
                        offset: at,
                        kind: GraphInsertTextErrorKind::ReturnBuild(
                            crate::GraphInsertQueryBuildError::InputDepth(kind),
                        ),
                    })?;
            }
            for (_, value) in parsed
                .vertices
                .iter()
                .flat_map(|v| &v.properties)
                .chain(parsed.edges.iter().flat_map(|e| &e.properties))
            {
                let value = return_projection::bind_read_value(value, &values)?;
                crate::GraphSetProjection::admit_output(&value, &input_types, 0).map_err(
                    |kind| crate::GraphSetTextError {
                        offset: at,
                        kind: crate::GraphSetTextErrorKind::ProjectionBuild(kind),
                    },
                )?;
            }
            Some(input)
        };
        if relational
            .as_ref()
            .is_some_and(|input| !input.row_fields.is_empty())
        {
            for (key, slot) in &syntax.element_keys {
                slot.resolve(
                    match resolve.resolve_symbol(GraphSymbolKind::Property, key) {
                        Some(GraphSymbol::Property(key)) => Some(key),
                        _ => None,
                    },
                );
            }
        }
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
        let matching = if matched {
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
            Some((builder, filters, scopes))
        } else {
            None
        };
        let mut vertices = Vec::new();
        for vertex in parsed.vertices {
            let mut labels = Vec::new();
            for label in vertex.labels {
                let GraphSymbol::Label(label) = symbol(GraphSymbolKind::Label, label)? else {
                    unreachable!("shared catalog resolver checked the domain")
                };
                labels.push(label);
            }
            vertices.push(InsertVertexTemplate {
                labels,
                properties: resolve_properties(vertex.properties, &mut symbol)?,
            });
        }
        let mut edges = Vec::new();
        for edge in parsed.edges {
            let GraphSymbol::Relation(relation) = symbol(GraphSymbolKind::Relation, edge.relation)?
            else {
                unreachable!("shared catalog resolver checked the domain")
            };
            edges.push(InsertEdgeTemplate {
                source: edge.source,
                destination: edge.destination,
                relation,
                properties: resolve_properties(edge.properties, &mut symbol)?,
            });
        }
        let mut returning = returning
            .map(|returning| returning.resolve(&mut symbol))
            .transpose()?;
        let (mut input, shape) = if let Some((builder, filters, scopes)) = matching {
            let mut columns = Vec::new();
            for (index, projection) in parsed.projections.into_iter().enumerate() {
                let key = if let Some(name) = projection.property {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)?
                    else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    Some(key)
                } else {
                    None
                };
                columns.push(BoundColumn {
                    alias: format!("_insert_input_{index}"),
                    variable: projection.variable.text.to_owned(),
                    key,
                    path: projection.path,
                });
            }
            let clauses: Vec<_> = scopes.iter().map(BoundScope::clause).collect();
            let projected: Vec<_> = columns.iter().map(BoundColumn::declaration).collect();
            let shape = built(
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
            if let Some(mut input) = relational {
                input.selection = Some(selection);
                input.correlations = correlations;
                (
                    InsertTextInput::Relation {
                        statement: statement.to_owned(),
                        input,
                    },
                    None,
                )
            } else {
                (InsertTextInput::Match(selection), Some(shape))
            }
        } else if let Some(input) = relational {
            debug_assert!(parsed.projections.is_empty());
            (
                InsertTextInput::Relation {
                    statement: statement.to_owned(),
                    input,
                },
                None,
            )
        } else {
            debug_assert!(parsed.projections.is_empty());
            (
                InsertTextInput::Unit {
                    statement: statement.to_owned(),
                    parameters: syntax.parameters,
                    parameter_offsets: syntax.parameter_offsets,
                },
                None,
            )
        };
        let created_maps = returning.as_ref().is_some_and(|returning| {
            returning.bindings.iter().any(|binding| {
                matches!(
                    binding,
                    crate::GraphInsertBinding::VertexProperties(_)
                        | crate::GraphInsertBinding::EdgeProperties(_)
                        | crate::GraphInsertBinding::VertexLabels(_)
                )
            })
        });
        let selection = match &mut input {
            InsertTextInput::Match(selection) => Some(selection),
            InsertTextInput::Relation { input, .. } => input.selection.as_mut(),
            InsertTextInput::Unit { .. } => None,
        };
        let source_metadata = selection.as_ref().is_some_and(|selection| {
            selection.columns.iter().any(|column| {
                matches!(
                    column.path,
                    Some(
                        GraphPathFunction::Labels
                            | GraphPathFunction::Type
                            | GraphPathFunction::Properties
                    )
                )
            })
        });
        if created_maps || source_metadata {
            let catalog =
                std::sync::Arc::new(ReverseSymbolCatalog::from_resolver(&mut resolve, statement));
            if let Some(selection) = selection {
                selection.reverse_catalog = Some(std::sync::Arc::clone(&catalog));
            }
            if created_maps {
                returning
                    .as_mut()
                    .expect("created maps belong to RETURN")
                    .property_catalog = Some(catalog);
            }
        }
        let template = Self {
            input,
            relation,
            vertices,
            edges,
            create_at: at,
        };
        // Catch catalog aliases collapsing distinct written keys, invalid
        // endpoint domains and all static expression columns during preparation.
        let prepared = template.instantiate(shape, None)?;
        if let Some(returning) = &returning {
            // Validate RETURN against the real source shape. Treating a MATCH
            // as a singleton here would lose its columns and occurrence bag.
            returning.bind(prepared, &shape_arguments(template.parameter_schema()))?;
        }
        Ok((template, returning, continuation))
    }

    /// `MATCH ... WITH ... CREATE ... [RETURN ...]` (fgdb-sl4ym): CREATE
    /// runs once per row of the read pipeline through its last WITH stage.
    /// That row is the whole scope: a vertex it carries is a bound endpoint,
    /// its values feed properties and RETURN, and a MATCH binding the WITH
    /// dropped is unknown. The row pipeline is the ordinary read relation.
    fn prepare_after_with<'a>(
        mut parser: Parser<'a>,
        statement: &'a str,
        relation: RelationId,
        mut resolve: impl GraphSymbolResolver,
        returning: bool,
    ) -> Result<InsertStep<'a>, GraphInsertTextError> {
        let (head, stages, schema) = parser.write_row_prefix()?;
        let at = parser.current.at;
        let (parsed, returning, continuation) = parser.insertion_over_row(&schema, returning)?;
        let read = parser.finish_write_row_prefix(statement, head, stages)?;
        let input = read.resolve(crate::graph_text::LentResolver(&mut resolve))?;
        let (template, returning) = Self::finish_over_row(
            statement,
            relation,
            &mut resolve,
            at,
            parsed,
            returning,
            input,
        )?;
        Ok((template, returning, continuation))
    }

    /// A later write of a chain (`CREATE (a) WITH a CREATE (b)`): CREATE runs
    /// once per row the previous write's WITH tail returned. As after `MATCH
    /// ... WITH`, that row is the whole scope, its hidden reads included; the
    /// input is the previous write's owned rows, never a second graph read.
    pub(crate) fn prepare_chained<'a>(
        statement: &'a str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl GraphSymbolResolver,
        from: ChainContinuation<'a>,
    ) -> Result<InsertStep<'a>, GraphInsertTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones())
            .resume_after(from.boundary, &from.parameters, from.at)?;
        parser.chain_write = true;
        let at = parser.current.at;
        let schema = from.row;
        // A chained write without RETURN ends the statement; with a WITH head
        // or RETURN it is a query step. insertion_over_row decides after the
        // CREATE clauses, where the head is.
        let (parsed, returning, continuation) = parser.insertion_over_row(&schema, false)?;
        let (columns, types) = schema
            .iter()
            .map(|(name, kind)| (name.text.to_owned(), *kind))
            .unzip();
        let input = BoundSetTextInput {
            selection: None,
            parameters: parser.syntax.parameters.clone(),
            parameter_offsets: parser.syntax.parameter_offsets.clone(),
            return_at: statement.len(),
            projection: None,
            quantifier: crate::GraphSetQuantifier::All,
            pipeline: Vec::new(),
            singleton: false,
            leading: Vec::new(),
            correlations: Vec::new(),
            owned: Some((columns, types)),
        };
        let (template, returning) = Self::finish_over_row(
            statement,
            relation,
            &mut resolve,
            at,
            parsed,
            returning,
            input,
        )?;
        Ok((template, returning, continuation))
    }

    /// Symbols, templates and RETURN of a CREATE over a row relation: `MATCH
    /// ... WITH ... CREATE` and every later write of a chain.
    fn finish_over_row(
        statement: &str,
        relation: RelationId,
        resolve: &mut impl GraphSymbolResolver,
        at: usize,
        parsed: InsertionSyntax<'_>,
        returning: Option<query::ParsedReturn<'_>>,
        mut input: BoundSetTextInput,
    ) -> Result<
        (
            Self,
            Option<crate::insertion_query_text::InsertReturnTemplate>,
        ),
        GraphInsertTextError,
    > {
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
        let mut vertices = Vec::new();
        for vertex in parsed.vertices {
            let mut labels = Vec::new();
            for label in vertex.labels {
                let GraphSymbol::Label(label) = symbol(GraphSymbolKind::Label, label)? else {
                    unreachable!("shared catalog resolver checked the domain")
                };
                labels.push(label);
            }
            vertices.push(InsertVertexTemplate {
                labels,
                properties: resolve_properties(vertex.properties, &mut symbol)?,
            });
        }
        let mut edges = Vec::new();
        for edge in parsed.edges {
            let GraphSymbol::Relation(relation) = symbol(GraphSymbolKind::Relation, edge.relation)?
            else {
                unreachable!("shared catalog resolver checked the domain")
            };
            edges.push(InsertEdgeTemplate {
                source: edge.source,
                destination: edge.destination,
                relation,
                properties: resolve_properties(edge.properties, &mut symbol)?,
            });
        }
        let mut returning = returning
            .map(|returning| returning.resolve(&mut symbol))
            .transpose()?;
        let created_maps = returning.as_ref().is_some_and(|returning| {
            returning.bindings.iter().any(|binding| {
                matches!(
                    binding,
                    crate::GraphInsertBinding::VertexProperties(_)
                        | crate::GraphInsertBinding::EdgeProperties(_)
                        | crate::GraphInsertBinding::VertexLabels(_)
                )
            })
        });
        let source_metadata = input.selection.as_ref().is_some_and(|selection| {
            selection.columns.iter().any(|column| {
                matches!(
                    column.path,
                    Some(
                        GraphPathFunction::Labels
                            | GraphPathFunction::Type
                            | GraphPathFunction::Properties
                    )
                )
            })
        });
        if created_maps || source_metadata {
            let catalog = std::sync::Arc::new(ReverseSymbolCatalog::from_resolver(
                &mut *resolve,
                statement,
            ));
            if let Some(selection) = input.selection.as_mut() {
                selection.reverse_catalog = Some(std::sync::Arc::clone(&catalog));
            }
            if created_maps {
                returning
                    .as_mut()
                    .expect("created maps belong to RETURN")
                    .property_catalog = Some(catalog);
            }
        }
        let template = Self {
            input: InsertTextInput::Relation {
                statement: statement.to_owned(),
                input,
            },
            relation,
            vertices,
            edges,
            create_at: at,
        };
        // Endpoint domains, written keys and value columns check here, over
        // the bound shape of the read relation.
        let prepared = template.instantiate(None, None)?;
        if let Some(returning) = &returning {
            returning.bind(prepared, &shape_arguments(template.parameter_schema()))?;
        }
        Ok((template, returning))
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        match &self.input {
            InsertTextInput::Match(selection) => selection.statement(),
            InsertTextInput::Relation { statement, .. } => statement,
            InsertTextInput::Unit { statement, .. } => statement,
        }
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        match &self.input {
            InsertTextInput::Match(selection) => selection.parameter_schema(),
            InsertTextInput::Relation { input, .. } => input.parameter_schema(),
            InsertTextInput::Unit { parameters, .. } => parameters,
        }
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphInsert, GraphInsertTextError> {
        let values = self.checked_arguments(arguments)?;
        let selection = match &self.input {
            InsertTextInput::Match(selection) => Some(selection.bind_values(&values)?),
            InsertTextInput::Relation { .. } | InsertTextInput::Unit { .. } => None,
        };
        self.instantiate(selection, Some(&values))
    }

    fn checked_arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<Vec<GqlParameterValue>, GraphInsertTextError> {
        match &self.input {
            InsertTextInput::Match(selection) => Ok(selection.checked_arguments(arguments)?),
            InsertTextInput::Relation { input, .. } => Ok(input.checked_arguments(arguments)?),
            InsertTextInput::Unit {
                statement,
                parameters,
                parameter_offsets,
            } => {
                // Use the original registered names, positions and exact-kind
                // acceptance law. There is no MATCH object to bind or execute.
                // Signed client integers for SKIP/LIMIT (fgdb-3dnpu).
                let admitted = arguments.admit_unsigned(parameters);
                let arguments: &GqlParameters = &admitted;
                let mut values = Vec::new();
                for (spec, &at) in parameters.iter().zip(parameter_offsets) {
                    let value = arguments
                        .get(&spec.name)
                        .ok_or_else(|| error(at, GraphPatternTextErrorKind::MissingParameter))?;
                    if !spec.parameter_type.accepts(value.parameter_type()) {
                        return Err(error(
                            at,
                            GraphPatternTextErrorKind::ParameterTypeMismatch {
                                expected: spec.parameter_type,
                                found: value.parameter_type(),
                            },
                        )
                        .into());
                    }
                    values.push(value);
                }
                if arguments.len() != values.len() {
                    return Err(error(
                        statement.len(),
                        GraphPatternTextErrorKind::UnexpectedArguments,
                    )
                    .into());
                }
                Ok(values)
            }
        }
    }

    fn instantiate(
        &self,
        selection: Option<PreparedGraphPattern<GraphValueRow>>,
        values: Option<&[GqlParameterValue]>,
    ) -> Result<PreparedGraphInsert, GraphInsertTextError> {
        let shape = if values.is_none() {
            shape_arguments(self.parameter_schema())
        } else {
            Vec::new()
        };
        let mut vertices = Vec::new();
        for vertex in &self.vertices {
            vertices.push(GraphInsertVertex {
                labels: vertex.labels.clone(),
                properties: bind_fields(&vertex.properties, values, &shape)?,
            });
        }
        let mut edges = Vec::new();
        for edge in &self.edges {
            edges.push(GraphInsertEdge {
                source: edge.source,
                destination: edge.destination,
                relation: edge.relation,
                properties: bind_fields(&edge.properties, values, &shape)?,
            });
        }
        if let InsertTextInput::Relation { input, .. } = &self.input {
            let shape;
            let values = match values {
                Some(values) => values,
                None => {
                    shape = shape_arguments(input.parameter_schema());
                    &shape
                }
            };
            let relation: PreparedGraphSet = input.bind_values(values)?;
            return PreparedGraphInsert::prepare_relation(relation, self.relation, vertices, edges)
                .map_err(|kind| insertion_build(self.create_at, kind));
        }
        match selection {
            Some(selection) => {
                PreparedGraphInsert::prepare(selection, self.relation, vertices, edges)
            }
            None => PreparedGraphInsert::prepare_standalone(self.relation, vertices, edges),
        }
        .map_err(|kind| insertion_build(self.create_at, kind))
    }
}

#[cfg(test)]
mod graph_row_source_tests {
    use super::*;

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "Source") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(LabelId(2))),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(7))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }

    fn prepare(text: &str) -> PreparedGraphInsertText {
        PreparedGraphInsertText::prepare(text, RelationId(7), symbols).unwrap()
    }

    #[test]
    fn with_vertex_alias_feeds_an_existing_endpoint_without_a_new_identity() {
        let template = prepare("MATCH (n:Source) WITH n AS a CREATE (a)-[:R]->(c:Copy {p:7})");
        assert_eq!(template.vertices.len(), 1);
        assert_eq!(template.edges[0].source, GraphInsertEndpoint::Column(0));
        assert_eq!(
            template.edges[0].destination,
            GraphInsertEndpoint::CreatedVertex(0)
        );
        let InsertTextInput::Relation { input, .. } = &template.input else {
            panic!("WITH must retain a native relation");
        };
        assert!(!input.singleton);
        assert!(input.selection.is_some());
        assert!(input.projection.is_some());
        let bound = template.bind_parameters(&GqlParameters::new()).unwrap();
        assert!(bound.requires_read());
        assert!(!bound.is_standalone());
        assert_eq!(bound.vertices_per_row(), 1);
        assert_eq!(bound.edges_per_row(), 1);
        assert_eq!(
            bound.relational_selection().unwrap().column_types(),
            &[GraphSetColumnType::Vertex]
        );
    }

    #[test]
    fn with_stages_keep_their_native_row_shape_before_creation() {
        for text in [
            "MATCH (n:Source) WITH n AS a,n.p AS p WHERE p>0 ORDER BY p DESC LIMIT 2 \
             CREATE (a)-[:R {p:p}]->(c:Copy {p:p+1})",
            "MATCH (n:Source) WITH DISTINCT n.p AS p CREATE (c:Copy {p:p})",
            "MATCH (n:Source) WITH count(*) AS total CREATE (c:Copy {p:total})",
            "MATCH (n:Source) WITH n AS a UNWIND [1,2] AS x \
             CREATE (a)-[:R]->(c:Copy {p:x})",
            "MATCH (n:Source) UNWIND [1,2] AS x CREATE (n)-[:R]->(c:Copy {p:x})",
            "MATCH (n:Source) WITH n.p AS p WITH p+1 AS q INSERT (c:Copy {p:q})",
        ] {
            let bound = prepare(text)
                .bind_parameters(&GqlParameters::new())
                .unwrap();
            assert!(bound.requires_read(), "{text}");
            assert!(bound.relational_selection().is_some(), "{text}");
            assert_eq!(bound.vertices_per_row(), 1, "{text}");
        }
    }

    #[test]
    fn one_parameter_table_spans_match_with_create_and_return() {
        let text = "MATCH (n:Source {p:$value}) WITH n AS a,$value AS p \
                    CREATE (a)-[:R {p:p+$extra}]->(c:Copy {p:$value}) RETURN a,c,c.p";
        let (template, returning) =
            PreparedGraphInsertText::prepare_definition(text, RelationId(7), &[], symbols, true)
                .unwrap();
        assert!(returning.is_some());
        assert_eq!(template.parameter_schema().len(), 2);
        assert_eq!(template.parameter_schema()[0].name, "value");
        assert_eq!(template.parameter_schema()[0].occurrences, 3);
        assert_eq!(template.parameter_schema()[1].name, "extra");
        for value in [-7, 0, 19] {
            let arguments = GqlParameters::new()
                .with_int64("value", value)
                .unwrap()
                .with_int64("extra", 2)
                .unwrap();
            assert!(template.bind_parameters(&arguments).is_ok());
        }
        assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    }

    #[test]
    fn dropped_bindings_and_nonvertex_endpoints_refuse_before_catalog_access() {
        for text in [
            "MATCH (n:Source) WITH 1 AS a CREATE (a)-[:R]->(c)",
            "MATCH (n:Source) WITH n.p AS a CREATE (a)-[:R]->(c)",
            "MATCH (n)-[r:R]->(m) WITH r AS a CREATE (a)-[:R]->(c)",
            "MATCH (n:Source) WITH n AS a CREATE (a:Copy)-[:R]->(c)",
            "MATCH (n:Source) WITH n AS a CREATE (a {})-[:R]->(c)",
            "MATCH (n:Source) WITH 1 AS x CREATE (c:Copy {p:n.p})",
            "MATCH (n:Source) WITH n AS a CREATE (c:Copy {p:a})",
        ] {
            let mut calls = 0;
            let result =
                PreparedGraphInsertText::prepare(text, RelationId(7), |kind, name: &str| {
                    calls += 1;
                    symbols(kind, name)
                });
            assert!(result.is_err(), "{text}");
            assert_eq!(calls, 0, "{text}");
        }
    }

    #[test]
    fn a_dropped_match_name_can_declare_a_new_vertex() {
        let template = prepare("MATCH (n:Source) WITH n AS a CREATE (n:Copy)-[:R]->(a)");
        assert_eq!(template.vertices.len(), 1);
        assert_eq!(
            template.edges[0].source,
            GraphInsertEndpoint::CreatedVertex(0)
        );
        assert_eq!(
            template.edges[0].destination,
            GraphInsertEndpoint::Column(0)
        );
    }

    #[test]
    fn graph_and_write_symbols_share_one_catalog_binding() {
        let mut calls = BTreeMap::new();
        let template = PreparedGraphInsertText::prepare(
            "MATCH (n:Source {p:$p}) WITH n AS a,n.p AS p \
             CREATE (a)-[:R {p:p}]->(c:Source {p:p})",
            RelationId(7),
            |kind, name: &str| {
                *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
                symbols(kind, name)
            },
        )
        .unwrap();
        assert_eq!(calls.len(), 3);
        assert!(calls.values().all(|&count| count == 1));
        let arguments = GqlParameters::new().with_int64("p", 3).unwrap();
        template.bind_parameters(&arguments).unwrap();
        assert!(calls.values().all(|&count| count == 1));
    }

    #[test]
    fn existing_standalone_and_match_insertions_keep_their_original_lanes() {
        let standalone = prepare("CREATE (c:Copy {p:1})")
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(standalone.is_standalone());
        assert!(!standalone.requires_read());
        let matched = prepare("MATCH (n:Source) CREATE (n)-[:R]->(c:Copy)")
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(matched.selection().is_some());
        assert!(matched.relational_selection().is_none());
        let rows = prepare("UNWIND [1,2] AS x CREATE (c:Copy {p:x})")
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        assert!(!rows.requires_read());
        assert!(!rows.is_standalone());
    }
}
