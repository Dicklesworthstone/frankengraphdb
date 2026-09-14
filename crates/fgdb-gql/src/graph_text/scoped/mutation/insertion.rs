//! CREATE uses the existing lexer, scoped MATCH compiler and scalar parser.
//! This module only prepares templates. Storage and identity allocation are
//! absent from parsing/binding; the ordinary insertion/write paths own both.

use super::*;
use crate::insertion::{GraphInsertBuildError, GraphInsertEdge, GraphInsertEndpoint,
    GraphInsertVertex, PreparedGraphInsert, MAX_GRAPH_INSERT_DECLARATIONS, MAX_GRAPH_INSERT_FIELDS};
use crate::insertion_text::{GraphInsertTextError, GraphInsertTextErrorKind,
    InsertEdgeTemplate, InsertVertexTemplate, PreparedGraphInsertText};
use crate::set_text::ReadValueTemplate;
use crate::{GraphIntegerExpression, GraphIntegerOp};

type ParsedFields<'a> = Vec<(Name<'a>, ReadValueTemplate)>;
struct NewVertex<'a> { name: Name<'a>, labels: Vec<Name<'a>>, properties: ParsedFields<'a> }
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

    fn insertion_endpoint(&self, name: Name<'a>, vertices: &[NewVertex<'a>], inputs: &mut Vec<Projection<'a>>)
        -> Result<GraphInsertEndpoint, GraphInsertTextError> {
        if self.syntax.variables.iter().any(|variable| variable.text == name.text) {
            return self.mutation_projection(inputs, name, None)
                .map(GraphInsertEndpoint::Column).map_err(Into::into);
        }
        vertices.iter().position(|vertex| vertex.name.text == name.text)
            .map(GraphInsertEndpoint::CreatedVertex)
            .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownVariable).into())
    }

    fn insertion_clauses(&mut self) -> Result<InsertionSyntax<'a>, GraphInsertTextError> {
        self.word("CREATE")?;
        let mut inputs = Vec::new();
        let mut vertices: Vec<NewVertex<'a>> = Vec::new();
        let mut edges = Vec::new();
        let mut fields = 0;
        loop {
            let declarations = vertices.len() + edges.len();
            if declarations >= MAX_GRAPH_INSERT_DECLARATIONS {
                return Err(insertion_build(self.current.at, GraphInsertBuildError::TooManyDeclarations {
                    limit: MAX_GRAPH_INSERT_DECLARATIONS, observed: declarations + 1,
                }));
            }
            self.punct(b'(', "(")?;
            let name = self.name()?;
            let existing = self.syntax.variables.iter().any(|variable| variable.text == name.text)
                || vertices.iter().any(|vertex| vertex.name.text == name.text);
            let mut labels: Vec<Name<'a>> = Vec::new();
            while self.take(b':')? {
                self.insertion_field_capacity(&mut fields)?;
                let label = self.name()?;
                if labels.iter().any(|old| old.text == label.text) {
                    return Err(expected(label.at, "unique CREATE label"));
                }
                labels.push(label);
            }
            let properties = self.insertion_properties(&mut inputs, &mut fields)?;
            self.punct(b')', ")")?;
            if self.take(b'-')? {
                if !labels.is_empty() || !properties.is_empty() {
                    return Err(expected(name.at, "declare new vertices separately before CREATE edges"));
                }
                let source = self.insertion_endpoint(name, &vertices, &mut inputs)?;
                self.punct(b'[', "[")?;
                self.punct(b':', ":")?;
                let relation = self.name()?;
                let properties = self.insertion_properties(&mut inputs, &mut fields)?;
                self.punct(b']', "]")?;
                self.punct(b'-', "-")?;
                self.punct(b'>', ">")?;
                self.punct(b'(', "(")?;
                let destination = self.name()?;
                self.punct(b')', ")")?;
                let destination = self.insertion_endpoint(destination, &vertices, &mut inputs)?;
                edges.push(NewEdge { source, destination, relation, properties });
            } else {
                if existing {
                    return Err(expected(name.at, "a fresh CREATE variable or a bound edge endpoint"));
                }
                if !edges.is_empty() {
                    return Err(expected(name.at, "new vertex declarations before CREATE edges"));
                }
                vertices.push(NewVertex { name, labels, properties });
            }
            if !self.take(b',')? { break; }
        }
        self.end()?;
        if inputs.is_empty() {
            // A hidden bound identity carries the occurrence bag even when all
            // new properties are constants. It is neither returned nor copied.
            let root = self.syntax.variables[0];
            self.mutation_projection(&mut inputs, root, None)?;
        }
        Ok(InsertionSyntax { projections: inputs, vertices, edges })
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
                            MutationIntegerTemplateOp::Bound(op) => *op,
                            MutationIntegerTemplateOp::Parameter { .. } => GraphIntegerOp::Literal(None),
                        }).collect();
                        GraphIntegerExpression::prepare(&shape).map_err(|kind| GraphMutationTextError {
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
    /// Prepare MATCH ... CREATE (copy:Label {p:n.p+1}), (n)-[:R]->(copy).
    /// New vertices precede edges, and edge endpoints must already be named by
    /// MATCH or those declarations. No implicit upsert or endpoint creation is
    /// performed. The explicit relation parameter is verified against every
    /// created edge, and is not inferred from a relation used only by MATCH.
    pub fn prepare(statement: &str, relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>)
        -> Result<Self, GraphInsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    /// The same parser and exact argument schema cover MATCH predicates and
    /// every created property, including CASE, arithmetic and scalar payloads.
    /// Syntax/limits are checked before the first catalog callback; every name
    /// and domain resolves once across both the read and creation clauses.
    pub fn prepare_with_parameter_types(statement: &str, relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>)
        -> Result<Self, GraphInsertTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        parser.parse_match_prefix()?;
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
        let (builder, filters) = resolve_pattern(
            &syntax.variables[..syntax.root_variables], &syntax.labels, &syntax.edges,
            syntax.filters, &mut symbol,
        )?;
        let mut scopes = Vec::new();
        for scope in syntax.scopes { scopes.push(scope.resolve(&mut symbol)?); }
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
        let mut columns = Vec::new();
        for (index, projection) in parsed.projections.into_iter().enumerate() {
            let key = if let Some(name) = projection.property {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
                    unreachable!("shared catalog resolver checked the domain")
                };
                Some(key)
            } else { None };
            columns.push(BoundColumn { alias: format!("_insert_input_{index}"),
                variable: projection.variable.text.to_owned(), key });
        }
        let clauses: Vec<_> = scopes.iter().map(BoundScope::clause).collect();
        let projected: Vec<_> = columns.iter().map(BoundColumn::declaration).collect();
        let shape = built(at, builder.prepare_values_with_clauses(&clauses, &projected, 0, None))?;
        let selection = PreparedGraphText {
            statement: statement.to_owned(), builder, filters, scopes, columns,
            ordering: Vec::new(), parameters: syntax.parameters, parameter_offsets: syntax.parameter_offsets,
            offset: Number::Literal(GqlParameterValue::UInt64(0)), count: None, distinct: false, return_at: at,
        };
        let template = Self { selection, relation, vertices, edges, create_at: at };
        // Catch catalog aliases collapsing distinct written keys, invalid
        // endpoint domains and all static expression columns during preparation.
        template.instantiate(shape, None)?;
        Ok(template)
    }

    #[must_use]
    pub fn statement(&self) -> &str { self.selection.statement() }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { self.selection.parameter_schema() }

    pub fn bind_parameters(&self, arguments: &GqlParameters) -> Result<PreparedGraphInsert, GraphInsertTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        let selection = self.selection.bind_values(&values)?;
        self.instantiate(selection, Some(&values))
    }

    fn instantiate(&self, selection: PreparedGraphPattern<GraphValueRow>, values: Option<&[GqlParameterValue]>)
        -> Result<PreparedGraphInsert, GraphInsertTextError> {
        let mut vertices = Vec::new();
        for vertex in &self.vertices {
            vertices.push(GraphInsertVertex { labels: vertex.labels.clone(), properties: bind_fields(&vertex.properties, values)? });
        }
        let mut edges = Vec::new();
        for edge in &self.edges {
            edges.push(GraphInsertEdge { source: edge.source, destination: edge.destination, properties: bind_fields(&edge.properties, values)? });
        }
        PreparedGraphInsert::prepare(selection, self.relation, vertices, edges).map_err(|kind| insertion_build(self.create_at, kind))
    }
}
