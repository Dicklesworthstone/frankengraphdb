//! RETURN binds the identities and fields frozen by the native CREATE
//! collector. These names are compiler scope, never a later graph lookup.

use super::*;
use crate::algebra::GraphValueOrder;
use crate::insertion_query_text::{InsertReturnTemplate, PreparedGraphInsertQueryText};
use crate::set_text::{ReadPageNumber, ReadProjectionTemplate};
use crate::{GraphInsertBinding, GraphSetProjection, GraphSetQuantifier, PreparedGraphInsertQuery};

#[derive(Clone, Copy)]
enum Binding<'a> {
    Input(usize),
    Vertex(usize),
    VertexProperty(usize, Name<'a>),
    Edge(usize),
    EdgeProperty(usize, Name<'a>),
}
impl Binding<'_> {
    fn same(self, other: Self) -> bool {
        match (self, other) {
            (Self::Input(a), Self::Input(b))
            | (Self::Vertex(a), Self::Vertex(b))
            | (Self::Edge(a), Self::Edge(b)) => a == b,
            (Self::VertexProperty(a, ak), Self::VertexProperty(b, bk))
            | (Self::EdgeProperty(a, ak), Self::EdgeProperty(b, bk)) => {
                a == b && ak.text == bk.text
            }
            _ => false,
        }
    }
}

pub(super) struct ParsedReturn<'a> {
    bindings: Vec<(Binding<'a>, Name<'a>, GraphSetColumnType)>,
    projection: Vec<ReadProjectionTemplate>,
    quantifier: GraphSetQuantifier,
    order: Vec<GraphValueOrder>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
    at: usize,
}

impl<'a> ParsedReturn<'a> {
    fn column(
        &mut self,
        binding: Binding<'a>,
        name: Name<'a>,
        kind: GraphSetColumnType,
    ) -> Result<usize, GraphPatternTextError> {
        if let Some(column) = self.bindings.iter().position(|(old, _, _)| old.same(binding)) {
            return Ok(column);
        }
        if self.bindings.len() == MAX_PATTERN_VERTICES {
            return Err(error(name.at, GraphPatternTextErrorKind::Build(
                PatternBuildError::LimitExceeded {
                    dimension: crate::algebra::PatternLimitDimension::Columns,
                    limit: MAX_PATTERN_VERTICES,
                    observed: self.bindings.len() + 1,
                },
            )));
        }
        let column = self.bindings.len();
        self.bindings.push((binding, name, kind));
        Ok(column)
    }

    fn leaf(
        &mut self,
        parser: &mut Parser<'a>,
        syntax: &InsertionSyntax<'a>,
        source: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        let vertex = syntax.vertices.iter().position(|vertex| vertex.name.is_some_and(|name| name.text == word));
        let edge = syntax.edges.iter().position(|edge| edge.name.is_some_and(|name| name.text == word));
        let input = source.iter().position(|(name, _)| name.text == word);
        if vertex.is_none() && edge.is_none() && input.is_none() {
            return Ok(None);
        }
        let name = parser.name()?;
        let property = if parser.take(b'.')? { Some(parser.name()?) } else { None };
        let (binding, alias, kind) = if let Some(vertex) = vertex {
            match property {
                Some(key) => (Binding::VertexProperty(vertex, key), key, GraphSetColumnType::Scalar),
                None => (Binding::Vertex(vertex), name, GraphSetColumnType::Vertex),
            }
        } else if let Some(edge) = edge {
            match property {
                Some(key) => (Binding::EdgeProperty(edge, key), key, GraphSetColumnType::Scalar),
                None => (Binding::Edge(edge), name, GraphSetColumnType::Edge),
            }
        } else {
            let input = input.expect("one admitted CREATE result binding");
            if property.is_some() {
                return Err(error(name.at, GraphPatternTextErrorKind::Expected("a scalar or list UNWIND binding")));
            }
            (Binding::Input(input), name, source[input].1)
        };
        self.column(binding, alias, kind).map(Some)
    }

    pub(super) fn admit(&self, parameters: &[GqlParameterSpec]) -> Result<(), GraphInsertTextError> {
        let values = shape_arguments(parameters);
        let types: Vec<_> = self.bindings.iter().map(|(_, _, kind)| *kind).collect();
        for (column, output) in self.projection.iter().enumerate() {
            let value = return_projection::bind_read_value(&output.value, &values)?;
            for result in [
                GraphSetProjection::validate_output_name(&output.name, column),
                GraphSetProjection::admit_output(&value, &types, column).map(|_| ()),
            ] {
                result.map_err(|kind| crate::GraphSetTextError {
                    offset: self.at,
                    kind: crate::GraphSetTextErrorKind::ProjectionBuild(kind),
                })?;
            }
        }
        Ok(())
    }

    pub(super) fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<InsertReturnTemplate, GraphInsertTextError> {
        let mut bindings = Vec::new();
        for (binding, _, _) in self.bindings {
            bindings.push(match binding {
                Binding::Input(column) => GraphInsertBinding::Input(column),
                Binding::Vertex(vertex) => GraphInsertBinding::CreatedVertex(vertex),
                Binding::Edge(edge) => GraphInsertBinding::CreatedEdge(edge),
                Binding::VertexProperty(vertex, name) => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    GraphInsertBinding::VertexProperty { vertex, key }
                }
                Binding::EdgeProperty(edge, name) => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    GraphInsertBinding::EdgeProperty { edge, key }
                }
            });
        }
        Ok(InsertReturnTemplate {
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
    pub(super) fn insertion_return(
        &mut self,
        syntax: &InsertionSyntax<'a>,
        source: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<ParsedReturn<'a>, GraphInsertTextError> {
        let at = self.current.at;
        self.word("RETURN")?;
        let quantifier = if self.take_word("DISTINCT")? {
            GraphSetQuantifier::Distinct
        } else {
            self.take_word("ALL")?;
            GraphSetQuantifier::All
        };
        let mut returning = ParsedReturn {
            bindings: Vec::new(),
            projection: Vec::new(),
            quantifier,
            order: Vec::new(),
            offset: ReadPageNumber::Literal(0),
            count: None,
            at,
        };
        let mut output: Vec<(Name<'a>, GraphSetColumnType)> = Vec::new();
        if self.take(b'*')? {
            for (index, &(name, kind)) in source.iter().enumerate() {
                let column = returning.column(Binding::Input(index), name, kind)?;
                returning.projection.push(ReadProjectionTemplate { name: name.text.to_owned(), value: ReadValueTemplate::Column(column) });
                output.push((name, kind));
            }
            for (index, vertex) in syntax.vertices.iter().enumerate() {
                if let Some(name) = vertex.name {
                    let column = returning.column(Binding::Vertex(index), name, GraphSetColumnType::Vertex)?;
                    returning.projection.push(ReadProjectionTemplate { name: name.text.to_owned(), value: ReadValueTemplate::Column(column) });
                    output.push((name, GraphSetColumnType::Vertex));
                }
            }
            for (index, edge) in syntax.edges.iter().enumerate() {
                if let Some(name) = edge.name {
                    let column = returning.column(Binding::Edge(index), name, GraphSetColumnType::Edge)?;
                    returning.projection.push(ReadProjectionTemplate { name: name.text.to_owned(), value: ReadValueTemplate::Column(column) });
                    output.push((name, GraphSetColumnType::Edge));
                }
            }
            if output.is_empty() {
                return Err(expected(at, "named CREATE or UNWIND bindings for RETURN *"));
            }
        } else {
            loop {
                self.capacity(output.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
                let at = self.current.at;
                let value = self.read_resolved_value(&mut |parser| returning.leaf(parser, syntax, source), 0)?;
                let name = if self.take_word("AS")? {
                    self.name()?
                } else if let ReadValueTemplate::Column(column) = &value {
                    returning.bindings[*column].1
                } else {
                    return Err(expected(at, "AS alias for a computed row value"));
                };
                if output.iter().any(|(old, _)| old.text == name.text) {
                    return Err(error(name.at, GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection)).into());
                }
                let types: Vec<_> = returning.bindings.iter().map(|(_, _, kind)| *kind).collect();
                let kind = value.column_type(&types, &self.syntax.parameters);
                returning.projection.push(ReadProjectionTemplate { name: name.text.to_owned(), value });
                output.push((name, kind));
                if !self.take(b',')? { break; }
            }
        }
        if let Some(ReadStageTemplate::Page { order, offset, count, .. }) = self.row_page(&output)? {
            returning.order = order;
            returning.offset = offset;
            returning.count = count;
        }
        Ok(returning)
    }
}

impl InsertReturnTemplate {
    fn bind(
        &self,
        insertion: PreparedGraphInsert,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphInsertQuery, GraphInsertTextError> {
        let mut projection = Vec::new();
        for output in &self.projection {
            projection.push(GraphSetProjection::new(&output.name, return_projection::bind_read_value(&output.value, values)?));
        }
        let mut query = PreparedGraphInsertQuery::prepare(insertion, self.bindings.clone(), projection, self.quantifier)
            .map_err(|kind| GraphInsertTextError { offset: self.at, kind: GraphInsertTextErrorKind::ReturnBuild(kind) })?;
        if !self.order.is_empty() {
            query = query.with_order_by(&self.order).map_err(|kind| crate::GraphSetTextError {
                offset: self.at,
                kind: crate::GraphSetTextErrorKind::OrderBuild(kind),
            })?;
        }
        Ok(query.with_page(
            return_projection::pipeline::page_value(&self.offset, values),
            self.count.as_ref().map(|count| return_projection::pipeline::page_value(count, values)),
        ))
    }
}

/// Single-statement framing uses the script lexer, so separators inside quoted
/// strings are values. One terminal semicolon is accepted, further statements
/// refuse before catalog resolution or parameter binding.
fn statement_body(statement: &str) -> Result<&str, GraphInsertTextError> {
    if statement.len() > MAX_GRAPH_TEXT_BYTES {
        return Err(error(MAX_GRAPH_TEXT_BYTES, GraphPatternTextErrorKind::DefinitionTooLarge).into());
    }
    let mut lexer = Lexer { text: statement, at: 0, tokens: 0 };
    loop {
        let token = script::next_script_token(&mut lexer)?;
        if matches!(token.kind, TokenKind::End) { return Ok(statement); }
        if matches!(token.kind, TokenKind::Punct(b';')) {
            let next = script::next_script_token(&mut lexer)?;
            if !matches!(next.kind, TokenKind::End) {
                return Err(expected(next.at, "one CREATE RETURN statement"));
            }
            return Ok(&statement[..token.at]);
        }
    }
}

impl PreparedGraphInsertQueryText {
    /// Classify write text with native token framing. Only top-level RETURN
    /// after CREATE/INSERT routes here; quoted values, property keys, parameter
    /// names and row aliases do not become clause keywords. This does not
    /// validate a statement or resolve any symbol; prepare owns those checks.
    pub fn has_return_clause(statement: &str) -> Result<bool, GraphInsertTextError> {
        if statement.len() > crate::MAX_GRAPH_WRITE_SCRIPT_BYTES {
            return Err(error(0, GraphPatternTextErrorKind::Expected("bounded native write text")).into());
        }
        let mut lexer = Lexer { text: statement, at: 0, tokens: 0 };
        let first = script::next_script_token(&mut lexer)?;
        if !matches!(first.kind, TokenKind::Word(word) if ["CREATE", "INSERT", "UNWIND"].iter().any(|kind| word.eq_ignore_ascii_case(kind))) {
            return Ok(false);
        }
        let mut created = matches!(first.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("CREATE") || word.eq_ignore_ascii_case("INSERT"));
        let mut depth = 0_usize;
        let mut previous = first.kind;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Punct(b';') => { lexer.tokens = 0; created = false; }
                TokenKind::Word(word) if depth == 0 => {
                    let alias = matches!(previous, TokenKind::Punct(b'.'))
                        || matches!(previous, TokenKind::Word(word) if word.eq_ignore_ascii_case("AS"));
                    if !alias && (word.eq_ignore_ascii_case("CREATE") || word.eq_ignore_ascii_case("INSERT")) {
                        created = true;
                    }
                    if created && !alias && word.eq_ignore_ascii_case("RETURN") {
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
    ) -> Result<Self, GraphInsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphInsertTextError> {
        let body = statement_body(statement)?;
        let (insertion, returning) = PreparedGraphInsertText::prepare_definition(body, relation, declarations, resolve, true)?;
        let returning = returning.expect("query mode prepares a RETURN definition");
        let values = shape_arguments(insertion.parameter_schema());
        returning.bind(insertion.instantiate(None, None)?, &values)?;
        Ok(Self { statement: statement.to_owned(), insertion, returning })
    }

    #[must_use]
    pub fn statement(&self) -> &str { &self.statement }

    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { self.insertion.parameter_schema() }

    pub fn bind_parameters(&self, arguments: &GqlParameters) -> Result<PreparedGraphInsertQuery, GraphInsertTextError> {
        let values = self.insertion.checked_arguments(arguments)?;
        let insertion = self.insertion.instantiate(None, Some(&values))?;
        self.returning.bind(insertion, &values)
    }
}
