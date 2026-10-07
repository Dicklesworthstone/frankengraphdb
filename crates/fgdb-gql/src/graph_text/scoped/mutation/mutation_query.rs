//! RETURN after SET/REMOVE/DETACH DELETE binds the statement's own selection
//! occurrences. A matched element's property is a source column plus the
//! mutation's proposal for that field; nothing is rescanned or re-matched.

use super::*;
use crate::mutation_text::{MutationReturnTemplate, PreparedGraphMutationQueryText};
use crate::set_text::{
    ReadPageNumber, ReadProjectionTemplate, ReadStageTemplate, ReadValueTemplate,
};
use crate::{
    GraphMutationBinding, GraphSetColumnType, GraphSetProjection, GraphSetQuantifier,
    GraphSetTextError, PreparedGraphMutation, PreparedGraphMutationQuery,
};

#[derive(Clone, Copy)]
enum Binding<'a> {
    Input(usize),
    Property {
        target: usize,
        key: Name<'a>,
        current: usize,
    },
}
impl Binding<'_> {
    fn same(self, other: Self) -> bool {
        match (self, other) {
            (Self::Input(a), Self::Input(b)) => a == b,
            (
                Self::Property {
                    target: a, key: ak, ..
                },
                Self::Property {
                    target: b, key: bk, ..
                },
            ) => a == b && ak.text == bk.text,
            _ => false,
        }
    }
}

/// Leaf resolution for one write statement's RETURN items: a leaf is a
/// column of the statement's private RETURN input row.
pub(super) trait ReturnLeaves<'a> {
    /// The binding column of the leaf at the current token, consuming it;
    /// `None` consumes nothing and leaves the token to the expression parser.
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>;
    /// The implicit output name of a binding column.
    fn name(&self, column: usize) -> Name<'a>;
    /// Every binding column's type, in column order.
    fn types(&self) -> Vec<GraphSetColumnType>;
}

/// The parsed items of a non-star write RETURN.
pub(super) struct ReturnItems<'a> {
    pub projection: Vec<ReadProjectionTemplate>,
    pub output: Vec<(Name<'a>, GraphSetColumnType)>,
    /// The source spelling of every item, for ORDER BY.
    pub spellings: Vec<Name<'a>>,
}

impl<'a> Parser<'a> {
    /// The comma-separated items of a write RETURN. Output names follow a
    /// read RETURN: an alias, a leaf's own name, or the item's source text,
    /// with the source text replacing a name that would otherwise repeat.
    pub(super) fn write_return_items(
        &mut self,
        leaves: &mut impl ReturnLeaves<'a>,
    ) -> Result<ReturnItems<'a>, GraphSetTextError> {
        let mut items = ReturnItems {
            projection: Vec::new(),
            output: Vec::new(),
            spellings: Vec::new(),
        };
        let mut sources = Vec::<Option<Name<'a>>>::new();
        loop {
            self.capacity(
                items.output.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let at = self.current.at;
            let value = self.read_resolved_value(&mut |parser| leaves.leaf(parser), 0)?;
            let end = self.current.at;
            items.spellings.push(self.source_name(at, end));
            let (mut name, derived) = if self.take_word("AS")? {
                (self.name()?, None)
            } else if let ReadValueTemplate::Column(column) = &value {
                (leaves.name(*column), Some(self.source_name(at, end)))
            } else {
                let derived = self.source_name(at, end);
                (derived, Some(derived))
            };
            if let Some(derived) = derived
                && let Some(previous) = items
                    .output
                    .iter()
                    .position(|(old, _)| old.text == name.text)
            {
                if let Some(earlier) = sources[previous] {
                    items.output[previous].0 = earlier;
                    items.projection[previous].name = earlier.text.to_owned();
                }
                name = derived;
            }
            sources.push(derived);
            if items.output.iter().any(|(old, _)| old.text == name.text) {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                )
                .into());
            }
            let kind = value.column_type(&leaves.types(), &self.syntax.parameters);
            items.projection.push(ReadProjectionTemplate {
                name: name.text.to_owned(),
                value,
            });
            items.output.push((name, kind));
            if !self.take(b',')? {
                break;
            }
        }
        Ok(items)
    }
}

/// Admit a write RETURN's projection over its binding types before any
/// catalog callback: names, value types and lazy-branch references.
pub(super) fn admit_return(
    projection: &[ReadProjectionTemplate],
    types: &[GraphSetColumnType],
    parameters: &[GqlParameterSpec],
    at: usize,
) -> Result<(), GraphSetTextError> {
    let values = insertion::shape_arguments(parameters);
    for (column, output) in projection.iter().enumerate() {
        let value = return_projection::bind_read_value(&output.value, &values)?;
        for result in [
            GraphSetProjection::validate_output_name(&output.name, column),
            GraphSetProjection::admit_output(&value, types, column).map(|_| ()),
        ] {
            result.map_err(|kind| GraphSetTextError {
                offset: at,
                kind: crate::GraphSetTextErrorKind::ProjectionBuild(kind),
            })?;
        }
    }
    Ok(())
}

/// The mutation leaves: the parsed RETURN plus the selection it extends.
struct MutationLeaves<'r, 'a> {
    returning: &'r mut ParsedMutationReturn<'a>,
    columns: &'r mut Vec<Projection<'a>>,
    scope: ReturnScope,
}
impl<'a> ReturnLeaves<'a> for MutationLeaves<'_, 'a> {
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        self.returning.leaf(parser, self.columns, self.scope)
    }
    fn name(&self, column: usize) -> Name<'a> {
        self.returning.bindings[column].1
    }
    fn types(&self) -> Vec<GraphSetColumnType> {
        self.returning.types()
    }
}

/// What the statement's actions make unreadable after it.
#[derive(Clone, Copy)]
pub(super) struct ReturnScope {
    /// A label action or DETACH DELETE changes `labels()`.
    pub labels_changed: bool,
    /// DETACH DELETE cascades to edges the statement cannot see.
    pub deleting: bool,
}

pub(super) struct ParsedMutationReturn<'a> {
    bindings: Vec<(Binding<'a>, Name<'a>, GraphSetColumnType)>,
    projection: Vec<ReadProjectionTemplate>,
    quantifier: GraphSetQuantifier,
    order: Vec<crate::algebra::GraphValueOrder>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
    at: usize,
}

impl<'a> ParsedMutationReturn<'a> {
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
            // Keep the current spelling for an implicit output alias.
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
        columns: &mut Vec<Projection<'a>>,
        scope: ReturnScope,
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        let name = Name {
            text: word,
            at: parser.current.at,
        };
        if matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) {
            // Graph functions are ordinary source projections, exactly as in
            // a SET right-hand side; scalar functions stay with the shared
            // expression compiler, which resolves their arguments here.
            let endpoint =
                word.eq_ignore_ascii_case("startNode") || word.eq_ignore_ascii_case("endNode");
            let kind = if endpoint {
                GraphSetColumnType::Vertex
            } else {
                match Parser::path_function(name) {
                    Ok(GraphPathFunction::Labels) if scope.labels_changed => {
                        return Err(error(
                            name.at,
                            GraphPatternTextErrorKind::Expected(
                                "labels() only in a statement without label actions or DETACH DELETE",
                            ),
                        ));
                    }
                    Ok(GraphPathFunction::Value) => GraphSetColumnType::Path,
                    Ok(GraphPathFunction::Length | GraphPathFunction::Type) => {
                        GraphSetColumnType::Scalar
                    }
                    Ok(GraphPathFunction::Nodes) => GraphSetColumnType::Vertices,
                    Ok(GraphPathFunction::Edges) => GraphSetColumnType::Edges,
                    Ok(GraphPathFunction::Edge) => GraphSetColumnType::Edge,
                    Ok(GraphPathFunction::Labels) => GraphSetColumnType::List,
                    Err(_) => return Ok(None),
                }
            };
            let Operand::Column(column) = parser.mutation_operand(columns)? else {
                unreachable!("a native graph function lowers to a source projection")
            };
            return self.column(Binding::Input(column), name, kind).map(Some);
        }
        let Some(kind) = parser.insertion_match_kind(word) else {
            return Ok(None);
        };
        let name = parser.name()?;
        if !parser.take(b'.')? {
            let column = parser.mutation_projection(columns, name, None)?;
            return self.column(Binding::Input(column), name, kind).map(Some);
        }
        let key = parser.name()?;
        match kind {
            GraphSetColumnType::Vertex => {}
            GraphSetColumnType::Edge if !scope.deleting => {}
            GraphSetColumnType::Edge => {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected(
                        "a vertex property after DETACH DELETE, which may cascade to edges",
                    ),
                ));
            }
            _ => {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("vertex or edge property"),
                ));
            }
        }
        let target = parser.mutation_projection(columns, name, None)?;
        let current = parser.mutation_projection(columns, name, Some(key))?;
        self.column(
            Binding::Property {
                target,
                key,
                current,
            },
            key,
            GraphSetColumnType::Scalar,
        )
        .map(Some)
    }

    fn types(&self) -> Vec<GraphSetColumnType> {
        self.bindings.iter().map(|(_, _, kind)| *kind).collect()
    }

    pub(super) fn admit(
        &self,
        parameters: &[GqlParameterSpec],
    ) -> Result<(), GraphMutationTextError> {
        Ok(admit_return(
            &self.projection,
            &self.types(),
            parameters,
            self.at,
        )?)
    }

    pub(super) fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<MutationReturnTemplate, GraphMutationTextError> {
        let mut bindings = Vec::new();
        for (binding, _, _) in self.bindings {
            bindings.push(match binding {
                Binding::Input(column) => GraphMutationBinding::Input(column),
                Binding::Property {
                    target,
                    key,
                    current,
                } => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    GraphMutationBinding::Property {
                        target,
                        key,
                        current,
                    }
                }
            });
        }
        Ok(MutationReturnTemplate {
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
    pub(super) fn mutation_return(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
        scope: ReturnScope,
    ) -> Result<ParsedMutationReturn<'a>, GraphMutationTextError> {
        let at = self.current.at;
        self.word("RETURN")?;
        let quantifier = if self.take_word("DISTINCT")? {
            GraphSetQuantifier::Distinct
        } else {
            self.take_all_quantifier()?;
            GraphSetQuantifier::All
        };
        let mut returning = ParsedMutationReturn {
            bindings: Vec::new(),
            projection: Vec::new(),
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
                .filter(|name| !name.text.starts_with(Self::ANONYMOUS_PREFIX))
                .collect();
            for name in matched {
                let kind = self
                    .insertion_match_kind(name.text)
                    .expect("visible MATCH binding has a domain");
                let input = self.mutation_projection(columns, name, None)?;
                let column = returning.column(Binding::Input(input), name, kind)?;
                returning.projection.push(ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(column),
                });
                output.push((name, kind));
            }
            if output.is_empty() {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("named MATCH bindings for RETURN *"),
                )
                .into());
            }
        } else {
            let items = self.write_return_items(&mut MutationLeaves {
                returning: &mut returning,
                columns: &mut *columns,
                scope,
            })?;
            returning.projection = items.projection;
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

impl MutationReturnTemplate {
    fn bind(
        &self,
        mutation: PreparedGraphMutation,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphMutationQuery, GraphMutationTextError> {
        let mut projection = Vec::new();
        for output in &self.projection {
            projection.push(GraphSetProjection::new(
                &output.name,
                return_projection::bind_read_value(&output.value, values)?,
            ));
        }
        let mut query = PreparedGraphMutationQuery::prepare(
            mutation,
            self.bindings.clone(),
            projection,
            self.quantifier,
        )
        .map_err(|kind| GraphMutationTextError {
            offset: self.at,
            kind: GraphMutationTextErrorKind::ReturnBuild(kind),
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

/// Single-statement framing on the script lexer: one terminal semicolon is
/// accepted, and a second statement refuses (naming `shape`) before any
/// catalog callback.
pub(super) fn statement_body<'s>(
    statement: &'s str,
    shape: &'static str,
) -> Result<&'s str, GraphPatternTextError> {
    if statement.len() > MAX_GRAPH_TEXT_BYTES {
        return Err(error(
            MAX_GRAPH_TEXT_BYTES,
            GraphPatternTextErrorKind::DefinitionTooLarge,
        ));
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
                return Err(error(next.at, GraphPatternTextErrorKind::Expected(shape)));
            }
            return Ok(&statement[..token.at]);
        }
    }
}

impl PreparedGraphMutationQueryText {
    /// Classify write text with native token framing: a MATCH-led statement
    /// with a top-level `SET x.`/`SET x:`/`SET x +=`, `REMOVE x`, or
    /// `DETACH DELETE x` clause followed by a top-level RETURN. A CREATE,
    /// INSERT or MERGE clause, or a second statement, is not this shape.
    /// Quoted values, property keys and aliases never become clause words.
    /// This validates nothing and resolves no symbol; prepare owns that.
    pub fn has_return_clause(statement: &str) -> Result<bool, GraphMutationTextError> {
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
        if !matches!(first.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("MATCH") || word.eq_ignore_ascii_case("OPTIONAL"))
        {
            return Ok(false);
        }
        let mut mutating = false;
        let mut depth = 0_usize;
        let mut previous = first.kind;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Word(word) if depth == 0 => {
                    let alias = matches!(previous, TokenKind::Punct(b'.'))
                        || matches!(previous, TokenKind::Word(word) if word.eq_ignore_ascii_case("AS"));
                    if !alias {
                        let mut lookahead = lexer.clone();
                        let next = script::next_script_token(&mut lookahead)?.kind;
                        let after = script::next_script_token(&mut lookahead)?.kind;
                        let clause = |name: &str| word.eq_ignore_ascii_case(name);
                        if ["CREATE", "INSERT", "MERGE"]
                            .iter()
                            .any(|name| clause(name))
                            && matches!(next, TokenKind::Punct(b'('))
                        {
                            return Ok(false);
                        }
                        let target = matches!(next, TokenKind::Word(_));
                        if (clause("SET") || clause("REMOVE"))
                            && target
                            && matches!(after, TokenKind::Punct(b'.' | b':' | b'+'))
                            || clause("DETACH")
                                && matches!(next, TokenKind::Word(word) if word.eq_ignore_ascii_case("DELETE"))
                        {
                            mutating = true;
                        }
                        if mutating && clause("RETURN") {
                            return Ok(true);
                        }
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
    ) -> Result<Self, GraphMutationTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphMutationTextError> {
        let body = statement_body(statement, "one SET/REMOVE RETURN statement")?;
        let (mutation, returning) = PreparedGraphMutationText::prepare_definition(
            body,
            relation,
            declarations,
            resolve,
            true,
        )?;
        let returning = returning.expect("query mode prepares a RETURN definition");
        Ok(Self {
            statement: statement.to_owned(),
            mutation,
            returning,
        })
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.mutation.parameter_schema()
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphMutationQuery, GraphMutationTextError> {
        let mutation = self.mutation.bind_parameters(arguments)?;
        let values = self.mutation.selection.checked_arguments(arguments)?;
        self.returning.bind(mutation, &values)
    }
}

#[cfg(test)]
mod framing_tests {
    use super::PreparedGraphMutationQueryText;

    #[test]
    fn only_a_mutation_clause_followed_by_return_routes_here() {
        for (text, expected) in [
            ("MATCH (n) SET n.p = 1 RETURN n.p", true),
            ("MATCH (n) SET n:L RETURN n", true),
            ("MATCH (n) SET n += {p: 1} RETURN n", true),
            ("MATCH (n) REMOVE n.p RETURN n", true),
            ("MATCH (n) DETACH DELETE n RETURN n", true),
            ("OPTIONAL MATCH (n) SET n.p = 1 RETURN n", true),
            ("MATCH (n) SET n.p = 1", false),
            ("MATCH (n) RETURN n", false),
            ("MATCH (set) WITH set RETURN set", false),
            ("MATCH (n) WHERE n.SET = 1 RETURN n", false),
            ("MATCH (n) SET n.p = 1 CREATE (m) RETURN m", false),
            ("MATCH (n) SET n.p = 1; MATCH (m) RETURN m", false),
            ("CREATE (n) SET n.p = 1 RETURN n", false),
            ("MATCH (n) RETURN n.p AS SET", false),
        ] {
            assert_eq!(
                PreparedGraphMutationQueryText::has_return_clause(text).unwrap(),
                expected,
                "{text}"
            );
        }
    }
}
