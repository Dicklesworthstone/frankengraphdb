//! RETURN binds the identities and fields frozen by the native CREATE
//! collector. These names are compiler scope, never a later graph lookup.

use super::*;
use crate::algebra::GraphValueOrder;
use crate::insertion_query_text::{InsertReturnTemplate, PreparedGraphInsertQueryText};
use crate::mutation_text::WriteReturnGroupTemplate;
use super::super::mutation_query::{self, ReturnLeaves};
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
    grouping: Option<WriteReturnGroupTemplate>,
    quantifier: GraphSetQuantifier,
    order: Vec<GraphValueOrder>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
    at: usize,
}

struct InsertLeaves<'r, 'a> {
    returning: &'r mut ParsedReturn<'a>,
    syntax: &'r mut InsertionSyntax<'a>,
    source: &'r [(Name<'a>, GraphSetColumnType)],
}

impl<'a> ReturnLeaves<'a> for InsertLeaves<'_, 'a> {
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        self.returning.leaf(parser, self.syntax, self.source)
    }

    fn name(&self, column: usize) -> Name<'a> {
        self.returning.bindings[column].1
    }

    fn types(&self) -> Vec<GraphSetColumnType> {
        self.returning.bindings.iter().map(|(_, _, kind)| *kind).collect()
    }
}

impl<'a> ParsedReturn<'a> {
    fn column(
        &mut self,
        binding: Binding<'a>,
        name: Name<'a>,
        kind: GraphSetColumnType,
    ) -> Result<usize, GraphPatternTextError> {
        if let Some(column) = self
            .bindings
            .iter()
            .position(|(old, _, _)| old.same(binding))
        {
            // A graph function and a bare variable can denote the same
            // vertex. Reuse its value slot, but keep the current spelling for
            // an implicit output alias (e.g. startNode(e), endNode(e) on a loop).
            self.bindings[column].1 = name;
            return Ok(column);
        }
        if self.bindings.len() == MAX_PATTERN_VERTICES {
            return Err(error(
                name.at,
                GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                    dimension: crate::algebra::PatternLimitDimension::Columns,
                    limit: MAX_PATTERN_VERTICES,
                    observed: self.bindings.len() + 1,
                }),
            ));
        }
        let column = self.bindings.len();
        self.bindings.push((binding, name, kind));
        Ok(column)
    }

    fn leaf(
        &mut self,
        parser: &mut Parser<'a>,
        syntax: &mut InsertionSyntax<'a>,
        source: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        if (word.eq_ignore_ascii_case("startNode") || word.eq_ignore_ascii_case("endNode"))
            && matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            return self.endpoint(parser, syntax, source.len()).map(Some);
        }
        // MATCH metadata and captured-path functions are ordinary source
        // projections, just as in SET and read RETURN. Keep them on the same
        // frozen source row as CREATE, including fields used only by RETURN.
        // A bare binding named like a function still takes the normal path.
        let name = Name {
            text: word,
            at: parser.current.at,
        };
        if let Ok(function) = Parser::path_function(name)
            && matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let kind = match function {
                GraphPathFunction::Value => GraphSetColumnType::Path,
                GraphPathFunction::Length | GraphPathFunction::Type => GraphSetColumnType::Scalar,
                GraphPathFunction::Nodes => GraphSetColumnType::Vertices,
                GraphPathFunction::Edges => GraphSetColumnType::Edges,
                GraphPathFunction::Edge => GraphSetColumnType::Edge,
                GraphPathFunction::Labels => GraphSetColumnType::List,
            };
            // mutation_operand owns function spelling, argument/domain
            // checks, projection deduplication and source-column limits.
            let Operand::Column(column) = parser.mutation_operand(&mut syntax.projections)? else {
                unreachable!("a native graph function lowers to a source projection")
            };
            return self
                .column(Binding::Input(source.len() + column), name, kind)
                .map(Some);
        }
        let vertex = syntax
            .vertices
            .iter()
            .position(|vertex| vertex.name.is_some_and(|name| name.text == word));
        let edge = syntax
            .edges
            .iter()
            .position(|edge| edge.name.is_some_and(|name| name.text == word));
        let input = source.iter().position(|(name, _)| name.text == word);
        let matched = parser.insertion_match_kind(word);
        if vertex.is_none() && edge.is_none() && input.is_none() && matched.is_none() {
            return Ok(None);
        }
        let name = parser.name()?;
        let property = if parser.take(b'.')? {
            Some(parser.name()?)
        } else {
            None
        };
        let (binding, alias, kind) = if let Some(vertex) = vertex {
            match property {
                Some(key) => (
                    Binding::VertexProperty(vertex, key),
                    key,
                    GraphSetColumnType::Scalar,
                ),
                None => (Binding::Vertex(vertex), name, GraphSetColumnType::Vertex),
            }
        } else if let Some(edge) = edge {
            match property {
                Some(key) => (
                    Binding::EdgeProperty(edge, key),
                    key,
                    GraphSetColumnType::Scalar,
                ),
                None => (Binding::Edge(edge), name, GraphSetColumnType::Edge),
            }
        } else if let Some(input) = input {
            if property.is_some() {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("a scalar or list UNWIND binding"),
                ));
            }
            (Binding::Input(input), name, source[input].1)
        } else {
            let kind = matched.expect("one admitted CREATE result binding");
            if property.is_some()
                && !matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
            {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("vertex or edge property"),
                ));
            }
            // RETURN-only source fields must join the same selection used by
            // CREATE. An index into the visible alias list is NOT a projection
            // column: creation may already have projected other properties.
            let input = parser.mutation_projection(&mut syntax.projections, name, property)?;
            (
                Binding::Input(source.len() + input),
                property.unwrap_or(name),
                if property.is_some() {
                    GraphSetColumnType::Scalar
                } else {
                    kind
                },
            )
        };
        self.column(binding, alias, kind).map(Some)
    }

    /// Endpoint identities are already in the native occurrence: created
    /// edges name declaration slots, matched edges name source projections.
    /// In particular, incoming syntax does not reverse the stored direction.
    fn endpoint(
        &mut self,
        parser: &mut Parser<'a>,
        syntax: &mut InsertionSyntax<'a>,
        imported_width: usize,
    ) -> Result<usize, GraphPatternTextError> {
        let function = parser.name()?;
        let start = function.text.eq_ignore_ascii_case("startNode");
        parser.punct(b'(', "(")?;
        let created = if let TokenKind::Word(word) = parser.current.kind {
            syntax
                .edges
                .iter()
                .find(|edge| edge.name.is_some_and(|name| name.text == word))
        } else {
            None
        };
        let binding = if let Some(edge) = created {
            parser.name()?;
            parser.punct(b')', ")")?;
            match if start { edge.source } else { edge.destination } {
                GraphInsertEndpoint::Column(column) => Binding::Input(column),
                GraphInsertEndpoint::CreatedVertex(vertex) => Binding::Vertex(vertex),
            }
        } else {
            // Reuse the ordinary MATCH direction/domain checks rather than
            // guessing endpoints for an undirected or quantified binding.
            let edge = parser.edge_variable()?;
            parser.punct(b')', ")")?;
            let vertex = parser.edge_endpoint(edge, start)?;
            Binding::Input(
                imported_width
                    + parser.mutation_projection(&mut syntax.projections, vertex, None)?,
            )
        };
        self.column(binding, function, GraphSetColumnType::Vertex)
    }

    pub(super) fn admit(
        &self,
        parameters: &[GqlParameterSpec],
    ) -> Result<(), GraphInsertTextError> {
        let types: Vec<_> = self.bindings.iter().map(|(_, _, kind)| *kind).collect();
        mutation_query::admit_return(
            &self.projection,
            &types,
            parameters,
            self.grouping.as_ref(),
            self.at,
        )?;
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
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)?
                    else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    GraphInsertBinding::VertexProperty { vertex, key }
                }
                Binding::EdgeProperty(edge, name) => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)?
                    else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    GraphInsertBinding::EdgeProperty { edge, key }
                }
            });
        }
        Ok(InsertReturnTemplate {
            bindings,
            projection: self.projection,
            grouping: self.grouping,
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
        syntax: &mut InsertionSyntax<'a>,
        source: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<ParsedReturn<'a>, GraphInsertTextError> {
        let at = self.current.at;
        self.word("RETURN")?;
        let quantifier = if self.take_word("DISTINCT")? {
            GraphSetQuantifier::Distinct
        } else {
            self.take_all_quantifier()?;
            GraphSetQuantifier::All
        };
        let mut returning = ParsedReturn {
            bindings: Vec::new(),
            projection: Vec::new(),
            grouping: None,
            quantifier,
            order: Vec::new(),
            offset: ReadPageNumber::Literal(0),
            count: None,
            at,
        };
        let mut output: Vec<(Name<'a>, GraphSetColumnType)> = Vec::new();
        // The source spelling of every non-star output, for ORDER BY.
        let mut spellings = Vec::new();
        if self.take(b'*')? {
            for (index, &(name, kind)) in source.iter().enumerate() {
                let column = returning.column(Binding::Input(index), name, kind)?;
                returning.projection.push(ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(column),
                });
                output.push((name, kind));
            }
            let matched: Vec<_> = self
                .syntax
                .variables
                .iter()
                .copied()
                .chain(
                    self.syntax
                        .visible_edges()
                        .filter_map(|(edge, _)| edge.variable),
                )
                .chain(self.syntax.path)
                .collect();
            for name in matched {
                if name.text.starts_with(Self::ANONYMOUS_PREFIX) {
                    continue;
                }
                let kind = self
                    .insertion_match_kind(name.text)
                    .expect("visible MATCH binding has a domain");
                let input = self.mutation_projection(&mut syntax.projections, name, None)?;
                let column = returning.column(Binding::Input(source.len() + input), name, kind)?;
                returning.projection.push(ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(column),
                });
                output.push((name, kind));
            }
            for (index, vertex) in syntax.vertices.iter().enumerate() {
                if let Some(name) = vertex.name {
                    let column = returning.column(
                        Binding::Vertex(index),
                        name,
                        GraphSetColumnType::Vertex,
                    )?;
                    returning.projection.push(ReadProjectionTemplate {
                        name: name.text.to_owned(),
                        value: ReadValueTemplate::Column(column),
                    });
                    output.push((name, GraphSetColumnType::Vertex));
                }
            }
            for (index, edge) in syntax.edges.iter().enumerate() {
                if let Some(name) = edge.name {
                    let column =
                        returning.column(Binding::Edge(index), name, GraphSetColumnType::Edge)?;
                    returning.projection.push(ReadProjectionTemplate {
                        name: name.text.to_owned(),
                        value: ReadValueTemplate::Column(column),
                    });
                    output.push((name, GraphSetColumnType::Edge));
                }
            }
            if output.is_empty() {
                return Err(expected(
                    at,
                    "named MATCH, CREATE or UNWIND bindings for RETURN *",
                ));
            }
        } else {
            let items = self.write_return_items(&mut InsertLeaves {
                returning: &mut returning,
                syntax: &mut *syntax,
                source,
            })?;
            returning.projection = items.projection;
            returning.grouping = items.grouping;
            output = items.output;
            spellings = items.spellings;
        }
        if let Some(ReadStageTemplate::Page {
            order,
            offset,
            count,
            ..
        }) = self.row_page_sourced(&output, &spellings)?
        {
            returning.order = order;
            returning.offset = offset;
            returning.count = count;
        }
        Ok(returning)
    }
}

impl InsertReturnTemplate {
    pub(super) fn bind(
        &self,
        insertion: PreparedGraphInsert,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphInsertQuery, GraphInsertTextError> {
        let mut projection = Vec::new();
        for output in &self.projection {
            projection.push(GraphSetProjection::new(
                &output.name,
                return_projection::bind_read_value(&output.value, values)?,
            ));
        }
        let mut query = PreparedGraphInsertQuery::prepare_with_grouping(
            insertion,
            self.bindings.clone(),
            projection,
            self.quantifier,
            self.grouping.as_ref().map(|group| group.bind(values)).transpose()?,
        )
        .map_err(|kind| GraphInsertTextError {
            offset: self.at,
            kind: GraphInsertTextErrorKind::ReturnBuild(kind),
        })?;
        if !self.order.is_empty() {
            query = query
                .with_order_by(&self.order)
                .map_err(|kind| crate::GraphSetTextError {
                    offset: self.at,
                    kind: crate::GraphSetTextErrorKind::OrderBuild(kind),
                })?;
        }
        Ok(query.with_page(
            return_projection::pipeline::page_value(&self.offset, values),
            self.count
                .as_ref()
                .map(|count| return_projection::pipeline::page_value(count, values)),
        ))
    }
}

/// Single-statement framing uses the script lexer, so separators inside quoted
/// strings are values. One terminal semicolon is accepted, further statements
/// refuse before catalog resolution or parameter binding.
fn statement_body(statement: &str) -> Result<&str, GraphInsertTextError> {
    if statement.len() > MAX_GRAPH_TEXT_BYTES {
        return Err(error(
            MAX_GRAPH_TEXT_BYTES,
            GraphPatternTextErrorKind::DefinitionTooLarge,
        )
        .into());
    }
    let mut lexer = Lexer {
        text: statement,
        at: 0,
        tokens: 0,
    };
    loop {
        let token = script::next_script_token(&mut lexer)?;
        if matches!(token.kind, TokenKind::End) {
            return Ok(statement);
        }
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
            return Err(error(
                0,
                GraphPatternTextErrorKind::Expected("bounded native write text"),
            )
            .into());
        }
        let mut lexer = Lexer {
            text: statement,
            at: 0,
            tokens: 0,
        };
        let first = script::next_script_token(&mut lexer)?;
        if !matches!(first.kind, TokenKind::Word(word) if ["CREATE", "INSERT", "UNWIND", "MATCH"].iter().any(|kind| word.eq_ignore_ascii_case(kind)))
        {
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
                TokenKind::Punct(b';') => {
                    lexer.tokens = 0;
                    created = false;
                }
                TokenKind::Word(word) if depth == 0 => {
                    let alias = matches!(previous, TokenKind::Punct(b'.'))
                        || matches!(previous, TokenKind::Word(word) if word.eq_ignore_ascii_case("AS"));
                    if !alias
                        && (word.eq_ignore_ascii_case("CREATE")
                            || word.eq_ignore_ascii_case("INSERT"))
                    {
                        // MATCH (CREATE) WHERE CREATE.p = 1 RETURN CREATE is
                        // a read. A clause, unlike that bound name, must start
                        // a node pattern. Peek without consuming the original
                        // stream or changing its token-budget accounting.
                        let mut lookahead = lexer.clone();
                        if matches!(
                            script::next_script_token(&mut lookahead)?.kind,
                            TokenKind::Punct(b'(')
                        ) {
                            created = true;
                        }
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
        let (insertion, returning) = PreparedGraphInsertText::prepare_definition(
            body,
            relation,
            declarations,
            resolve,
            true,
        )?;
        let returning = returning.expect("query mode prepares a RETURN definition");
        Ok(Self {
            statement: statement.to_owned(),
            insertion,
            returning,
        })
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.insertion.parameter_schema()
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphInsertQuery, GraphInsertTextError> {
        let values = self.insertion.checked_arguments(arguments)?;
        let selection = match &self.insertion.input {
            InsertTextInput::Match(selection) => Some(selection.bind_values(&values)?),
            InsertTextInput::Relation { .. } | InsertTextInput::Unit { .. } => None,
        };
        let insertion = self.insertion.instantiate(selection, Some(&values))?;
        self.returning.bind(insertion, &values)
    }
}

#[cfg(test)]
mod write_clause_framing_tests {
    use super::PreparedGraphInsertQueryText;

    #[test]
    fn matched_keyword_names_and_property_uses_do_not_turn_reads_into_writes() {
        for text in [
            "MATCH (CREATE) WHERE CREATE.p = 1 RETURN CREATE",
            "MATCH (INSERT) WHERE INSERT.p = 1 RETURN INSERT",
            "MATCH (create) WHERE create.p > 0 RETURN create.p AS p,create",
            "MATCH (insert) WITH insert AS source RETURN source",
            "MATCH (n) WHERE n.CREATE = 1 RETURN n",
            "MATCH (n) WHERE n.INSERT = 1 RETURN n",
            "UNWIND [1] AS CREATE RETURN CREATE",
            "CREATE (n); MATCH (CREATE) WHERE CREATE.p = 1 RETURN CREATE",
        ] {
            assert!(
                !PreparedGraphInsertQueryText::has_return_clause(text).unwrap(),
                "misclassified a bound keyword as a write clause: {text}"
            );
        }
    }

    #[test]
    fn real_creation_after_a_keyword_binding_keeps_the_return_route() {
        for text in [
            "MATCH (CREATE) WHERE CREATE.p = 1 CREATE (copy) RETURN CREATE,copy",
            "MATCH (INSERT) INSERT (copy) RETURN INSERT,copy",
            "UNWIND [1] AS CREATE CREATE (n {p:CREATE}) RETURN n",
            "CREATE (n) RETURN n",
            "INSERT (n) RETURN n",
            "CREATE (n); MATCH (CREATE) CREATE (copy) RETURN copy",
        ] {
            assert!(
                PreparedGraphInsertQueryText::has_return_clause(text).unwrap(),
                "lost a real CREATE/INSERT RETURN clause: {text}"
            );
        }
    }
}
