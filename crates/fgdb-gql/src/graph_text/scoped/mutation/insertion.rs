//! CREATE uses the existing lexer, scoped MATCH compiler and scalar parser.
//! This module only prepares templates. Storage and identity allocation are
//! absent from parsing/binding; the ordinary insertion/write paths own both.

use super::*;
use crate::insertion::{GraphInsertBuildError, GraphInsertEdge, GraphInsertEndpoint,
    GraphInsertVertex, PreparedGraphInsert, MAX_GRAPH_INSERT_DECLARATIONS, MAX_GRAPH_INSERT_FIELDS};
use crate::insertion_text::{GraphInsertTextError, GraphInsertTextErrorKind, InsertTextInput,
    InsertEdgeTemplate, InsertVertexTemplate, PreparedGraphInsertText};
use crate::set_text::ReadValueTemplate;
use crate::{GraphIntegerExpression, GraphIntegerOp};

type ParsedFields<'a> = Vec<(Name<'a>, ReadValueTemplate)>;
struct NewVertex<'a> { name: Option<Name<'a>>, labels: Vec<Name<'a>>, properties: ParsedFields<'a> }
struct NewEdge<'a> { source: GraphInsertEndpoint, destination: GraphInsertEndpoint,
    relation: Name<'a>, properties: ParsedFields<'a> }
struct InsertionSyntax<'a> {
    projections: Vec<Projection<'a>>,
    vertices: Vec<NewVertex<'a>>,
    edges: Vec<NewEdge<'a>>,
}

fn insertion_build(at: usize, kind: GraphInsertBuildError) -> GraphInsertTextError {
    GraphInsertTextError { offset: at, kind: GraphInsertTextErrorKind::Build(kind) }
}
fn expected(at: usize, message: &'static str) -> GraphInsertTextError {
    error(at, GraphPatternTextErrorKind::Expected(message)).into()
}

impl<'a> Parser<'a> {
    fn insertion_field_capacity(&self, fields: &mut usize) -> Result<(), GraphInsertTextError> {
        if *fields >= MAX_GRAPH_INSERT_FIELDS {
            return Err(insertion_build(self.current.at, GraphInsertBuildError::TooManyFields {
                limit: MAX_GRAPH_INSERT_FIELDS, observed: *fields + 1,
            }));
        }
        *fields += 1;
        Ok(())
    }

    fn insertion_declaration_capacity(&self, count: usize) -> Result<(), GraphInsertTextError> {
        if count >= MAX_GRAPH_INSERT_DECLARATIONS {
            return Err(insertion_build(self.current.at, GraphInsertBuildError::TooManyDeclarations {
                limit: MAX_GRAPH_INSERT_DECLARATIONS, observed: count + 1,
            }));
        }
        Ok(())
    }

    fn insertion_properties(&mut self, inputs: &mut Vec<Projection<'a>>, fields: &mut usize)
        -> Result<ParsedFields<'a>, GraphInsertTextError> {
        let mut properties: ParsedFields<'a> = Vec::new();
        if !self.take(b'{')? { return Ok(properties); }
        if self.take(b'}')? { return Ok(properties); }
        loop {
            self.insertion_field_capacity(fields)?;
            self.capacity(properties.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
            let key = self.name()?;
            if properties.iter().any(|(old, _)| old.text == key.text) {
                return Err(expected(key.at, "unique CREATE property key"));
            }
            self.punct(b':', ":")?;
            let at = self.current.at;
            let value = match self.mutation_expression(inputs)? {
                Operand::Column(column) => ReadValueTemplate::Column(column),
                Operand::Literal(value) => ReadValueTemplate::Literal(value),
                Operand::Number(Number::Literal(value)) => ReadValueTemplate::Literal(scalar(value, at)?),
                Operand::Number(Number::Parameter(index)) => ReadValueTemplate::Parameter { index, at },
                Operand::Integer { program, at } => ReadValueTemplate::Integer { program, at },
            };
            properties.push((key, value));
            if self.take(b'}')? { break; }
            self.punct(b',', ", or }")?;
        }
        Ok(properties)
    }

    /// One node grammar for standalone clauses and every endpoint in a chain.
    /// Anonymous nodes are distinct declarations without invented identifiers.
    /// New names enter only the endpoint registry, never the scalar read scope.
    fn insertion_node(
        &mut self, parsed: &mut InsertionSyntax<'a>, fields: &mut usize, pending_edges: usize,
    ) -> Result<GraphInsertEndpoint, GraphInsertTextError> {
        self.punct(b'(', "(")?;
        let at = self.current.at;
        let name = if matches!(self.current.kind, TokenKind::Word(_)) {
            Some(self.name()?)
        } else { None };
        let existing = if let Some(name) = name {
            if self.syntax.variables.iter().any(|variable| variable.text == name.text) {
                Some(GraphInsertEndpoint::Column(self.mutation_projection(&mut parsed.projections, name, None)?))
            } else {
                parsed.vertices.iter().position(|vertex| vertex.name.is_some_and(|old| old.text == name.text))
                    .map(GraphInsertEndpoint::CreatedVertex)
            }
        } else { None };
        if let Some(endpoint) = existing {
            // Even an empty property map is declaration syntax, not a second
            // assignment to the already-bound node. Refuse instead of ignoring.
            if self.is_punct(b':') || self.is_punct(b'{') {
                return Err(expected(at, "bare reference to an already-bound CREATE node"));
            }
            self.punct(b')', ")")?;
            return Ok(endpoint);
        }
        self.insertion_declaration_capacity(parsed.vertices.len() + parsed.edges.len() + pending_edges)?;
        let mut labels: Vec<Name<'a>> = Vec::new();
        while self.take(b':')? {
            self.insertion_field_capacity(fields)?;
            let label = self.name()?;
            if labels.iter().any(|old| old.text == label.text) {
                return Err(expected(label.at, "unique CREATE label"));
            }
            labels.push(label);
        }
        let properties = self.insertion_properties(&mut parsed.projections, fields)?;
        self.punct(b')', ")")?;
        let vertex = parsed.vertices.len();
        parsed.vertices.push(NewVertex { name, labels, properties });
        Ok(GraphInsertEndpoint::CreatedVertex(vertex))
    }

    fn insertion_clauses(&mut self) -> Result<InsertionSyntax<'a>, GraphInsertTextError> {
        let create_at = self.current.at;
        self.word("CREATE")?;
        let mut parsed = InsertionSyntax { projections: Vec::new(), vertices: Vec::new(), edges: Vec::new() };
        let mut fields = 0;
        loop {
            let mut left = self.insertion_node(&mut parsed, &mut fields, 0)?;
            while self.is_punct(b'-') || self.is_punct(b'<') {
                self.insertion_declaration_capacity(parsed.vertices.len() + parsed.edges.len())?;
                let incoming = self.take(b'<')?;
                self.punct(b'-', "-")?;
                self.punct(b'[', "[")?;
                self.punct(b':', ":")?;
                let relation = self.name()?;
                let properties = self.insertion_properties(&mut parsed.projections, &mut fields)?;
                self.punct(b']', "]")?;
                self.punct(b'-', "-")?;
                let outgoing = self.take(b'>')?;
                if incoming == outgoing {
                    return Err(expected(relation.at, "exactly one directed CREATE arrow"));
                }
                // Reserve this not-yet-pushed edge while the right node may
                // admit another vertex. A chain cannot step past the total cap.
                let right = self.insertion_node(&mut parsed, &mut fields, 1)?;
                let (source, destination) = if incoming { (right, left) } else { (left, right) };
                parsed.edges.push(NewEdge { source, destination, relation, properties });
                left = right;
            }
            if !self.take(b',')? { break; }
        }
        self.end()?;
        if parsed.vertices.is_empty() && parsed.edges.is_empty() {
            return Err(insertion_build(create_at, GraphInsertBuildError::Empty));
        }
        if parsed.projections.is_empty()
            && let Some(&root) = self.syntax.variables.first()
        {
            // A MATCH still carries its occurrence bag even for constants.
            // Standalone CREATE has no graph inputs and needs no placeholder.
            self.mutation_projection(&mut parsed.projections, root, None)?;
        }
        Ok(parsed)
    }
}

fn resolve_properties<'a>(properties: ParsedFields<'a>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>)
    -> Result<Vec<(PropertyKeyId, ReadValueTemplate)>, GraphInsertTextError> {
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
fn bind_fields(fields: &[(PropertyKeyId, ReadValueTemplate)], values: Option<&[GqlParameterValue]>)
    -> Result<Vec<(PropertyKeyId, GraphMutationValue)>, GraphInsertTextError> {
    let mut properties = Vec::new();
    for (key, value) in fields {
        let value = match value {
            ReadValueTemplate::Column(column) => GraphMutationValue::Column(*column),
            ReadValueTemplate::Literal(value) => GraphMutationValue::Literal(value.clone()),
            ReadValueTemplate::Parameter { index, at } => {
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
                        let shape: Vec<_> = program.iter().map(|op| match op {
                            MutationIntegerTemplateOp::Bound(op) => op.clone(),
                            MutationIntegerTemplateOp::Parameter { .. } => GraphIntegerOp::Literal(None),
                        }).collect();
                        GraphIntegerExpression::prepare_scalar(&shape).map_err(|kind| GraphMutationTextError {
                            offset: *at, kind: GraphMutationTextErrorKind::IntegerExpression(kind),
                        })?
                    }
                };
                GraphMutationValue::Expression(expression)
            }
        };
        properties.push((*key, value));
    }
    Ok(properties)
}

impl PreparedGraphInsertText {
    /// Prepare CREATE (a:Label {p:$value})-[:R]->(b), optionally after MATCH.
    /// Names are declared on first occurrence and bare repetitions share their
    /// created vertex; anonymous nodes always declare distinct vertices. Chains,
    /// cycles, self-loops and incoming arrows lower to the same typed template.
    /// All vertices are emitted before their edges for each input occurrence.
    /// The explicit relation parameter is verified against every created edge.
    pub fn prepare(statement: &str, relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>)
        -> Result<Self, GraphInsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    /// The original parser's parameter table covers every predicate and created
    /// property, including CASE, arithmetic and scalar payloads. Syntax/limits
    /// pass before catalog callbacks. Each name/domain resolves once, across
    /// MATCH and CREATE when both are present. Standalone creation has no graph
    /// input definition and never synthesizes one merely to validate arguments.
    pub fn prepare_with_parameter_types(statement: &str, relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>)
        -> Result<Self, GraphInsertTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        let matched = parser.is_word("MATCH");
        if matched { parser.parse_match_prefix()?; }
        let at = parser.current.at;
        let parsed = parser.insertion_clauses()?;
        let syntax = parser.syntax;
        let mut cache = BTreeMap::new();
        let mut symbol = |kind, name: Name<'_>| -> Result<GraphSymbol, GraphPatternTextError> {
            let key = (kind, name.text.to_owned());
            if let Some(value) = cache.get(&key) { return Ok(*value); }
            let value = resolve(kind, name.text)
                .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
            if value.kind() != kind {
                return Err(error(name.at, GraphPatternTextErrorKind::WrongSymbolKind { expected: kind, found: value.kind() }));
            }
            cache.insert(key, value);
            Ok(value)
        };
        let matching = if matched {
            let (builder, filters) = resolve_pattern(
                &syntax.variables[..syntax.root_variables], &syntax.labels, &syntax.edges,
                syntax.filters, &mut symbol,
            )?;
            let mut scopes = Vec::new();
            for scope in syntax.scopes { scopes.push(scope.resolve(&mut symbol)?); }
            Some((builder, filters, scopes))
        } else { None };
        let mut vertices = Vec::new();
        for vertex in parsed.vertices {
            let mut labels = Vec::new();
            for label in vertex.labels {
                let GraphSymbol::Label(label) = symbol(GraphSymbolKind::Label, label)? else {
                    unreachable!("shared catalog resolver checked the domain")
                };
                labels.push(label);
            }
            vertices.push(InsertVertexTemplate { labels, properties: resolve_properties(vertex.properties, &mut symbol)? });
        }
        let mut edges = Vec::new();
        for edge in parsed.edges {
            let GraphSymbol::Relation(found) = symbol(GraphSymbolKind::Relation, edge.relation)? else {
                unreachable!("shared catalog resolver checked the domain")
            };
            if found != relation {
                return Err(GraphInsertTextError { offset: edge.relation.at,
                    kind: GraphInsertTextErrorKind::RelationCoordinate { expected: relation, found } });
            }
            edges.push(InsertEdgeTemplate { source: edge.source, destination: edge.destination,
                properties: resolve_properties(edge.properties, &mut symbol)? });
        }
        let (input, shape) = if let Some((builder, filters, scopes)) = matching {
            let mut columns = Vec::new();
            for (index, projection) in parsed.projections.into_iter().enumerate() {
                let key = if let Some(name) = projection.property {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    Some(key)
                } else { None };
                columns.push(BoundColumn { alias: format!("_insert_input_{index}"),
                    variable: projection.variable.text.to_owned(), key, path: None });
            }
            let clauses: Vec<_> = scopes.iter().map(BoundScope::clause).collect();
            let projected: Vec<_> = columns.iter().map(BoundColumn::declaration).collect();
            let shape = built(at, builder.prepare_values_with_clauses(&clauses, &projected, 0, None))?;
            let selection = PreparedGraphText {
                statement: statement.to_owned(), builder, filters, scopes, columns,
                ordering: Vec::new(), parameters: syntax.parameters, parameter_offsets: syntax.parameter_offsets,
                offset: Number::Literal(GqlParameterValue::UInt64(0)), count: None, distinct: false, return_at: at,
            };
            (InsertTextInput::Match(selection), Some(shape))
        } else {
            debug_assert!(parsed.projections.is_empty());
            (InsertTextInput::Unit { statement: statement.to_owned(),
                parameters: syntax.parameters, parameter_offsets: syntax.parameter_offsets }, None)
        };
        let template = Self { input, relation, vertices, edges, create_at: at };
        // Catch catalog aliases collapsing distinct written keys, invalid
        // endpoint domains and all static expression columns during preparation.
        template.instantiate(shape, None)?;
        Ok(template)
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        match &self.input {
            InsertTextInput::Match(selection) => selection.statement(),
            InsertTextInput::Unit { statement, .. } => statement,
        }
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        match &self.input {
            InsertTextInput::Match(selection) => selection.parameter_schema(),
            InsertTextInput::Unit { parameters, .. } => parameters,
        }
    }

    pub fn bind_parameters(&self, arguments: &GqlParameters) -> Result<PreparedGraphInsert, GraphInsertTextError> {
        match &self.input {
            InsertTextInput::Match(selection) => {
                let values = selection.checked_arguments(arguments)?;
                self.instantiate(Some(selection.bind_values(&values)?), Some(&values))
            }
            InsertTextInput::Unit { statement, parameters, parameter_offsets } => {
                // Use the original registered names, positions and exact-kind
                // acceptance law. There is no MATCH object to bind or execute.
                let mut values = Vec::new();
                for (spec, &at) in parameters.iter().zip(parameter_offsets) {
                    let value = arguments.get(&spec.name)
                        .ok_or_else(|| error(at, GraphPatternTextErrorKind::MissingParameter))?;
                    if !spec.parameter_type.accepts(value.parameter_type()) {
                        return Err(error(at, GraphPatternTextErrorKind::ParameterTypeMismatch {
                            expected: spec.parameter_type, found: value.parameter_type(),
                        }).into());
                    }
                    values.push(value);
                }
                if arguments.len() != values.len() {
                    return Err(error(statement.len(), GraphPatternTextErrorKind::UnexpectedArguments).into());
                }
                self.instantiate(None, Some(&values))
            }
        }
    }

    fn instantiate(&self, selection: Option<PreparedGraphPattern<GraphValueRow>>, values: Option<&[GqlParameterValue]>)
        -> Result<PreparedGraphInsert, GraphInsertTextError> {
        let mut vertices = Vec::new();
        for vertex in &self.vertices {
            vertices.push(GraphInsertVertex { labels: vertex.labels.clone(), properties: bind_fields(&vertex.properties, values)? });
        }
        let mut edges = Vec::new();
        for edge in &self.edges {
            edges.push(GraphInsertEdge { source: edge.source, destination: edge.destination, properties: bind_fields(&edge.properties, values)? });
        }
        match selection {
            Some(selection) => PreparedGraphInsert::prepare(selection, self.relation, vertices, edges),
            None => PreparedGraphInsert::prepare_standalone(self.relation, vertices, edges),
        }.map_err(|kind| insertion_build(self.create_at, kind))
    }
}
