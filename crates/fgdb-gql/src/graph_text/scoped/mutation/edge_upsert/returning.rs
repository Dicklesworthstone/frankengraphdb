//! The shared RETURN compiler, bound only to the MERGE-selected relationship.
//! Endpoint selection remains private. Final properties come from native reads
//! after ON/SET, not from a second MATCH or a source-text interpreter.

use super::*;
use crate::edge_upsert_text::PreparedGraphEdgeUpsertQueryText;
use crate::mutation_text::WriteReturnGroupTemplate;
use crate::set_text::{ReadPageNumber, ReadProjectionTemplate, ReadStageTemplate};
use crate::{
    GraphEdgeReturnBinding, GraphSetColumnType, GraphSetProjection, GraphSetQuantifier,
    PreparedGraphEdgeUpsertQuery,
};
use mutation_query::{ReturnLeaves, ReturnMapInput};

#[derive(Clone, Copy)]
enum EdgeBinding<'a> {
    Edge,
    Property(Name<'a>),
}

pub(super) struct ParsedEdgeReturn<'a> {
    variable: Name<'a>,
    bindings: Vec<(EdgeBinding<'a>, GraphSetColumnType)>,
    pub(super) projection: Vec<ReadProjectionTemplate>,
    pub(super) grouping: Option<WriteReturnGroupTemplate>,
    quantifier: GraphSetQuantifier,
    order: Vec<crate::algebra::GraphValueOrder>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
    pub(super) at: usize,
    tail: Option<Vec<ReadStageTemplate>>,
}
impl<'a> ReturnLeaves<'a> for ParsedEdgeReturn<'a> {
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        if word != self.variable.text
            || matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            return Ok(None);
        }
        let source = parser.name()?;
        let input = if parser.take(b'.')? {
            ReturnMapInput::Property(parser.schema_name()?)
        } else {
            ReturnMapInput::Identity
        };
        self.map_input(parser, source, input).map(Some)
    }
    fn map_input(
        &mut self,
        parser: &mut Parser<'a>,
        source: Name<'a>,
        input: ReturnMapInput<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        if source.text != self.variable.text {
            return Err(error(
                source.at,
                GraphPatternTextErrorKind::Expected("the MERGE relationship variable"),
            ));
        }
        let (binding, kind) = match input {
            ReturnMapInput::Identity | ReturnMapInput::Guard => {
                (EdgeBinding::Edge, GraphSetColumnType::Edge)
            }
            ReturnMapInput::Property(key) => {
                (EdgeBinding::Property(key), GraphSetColumnType::Scalar)
            }
            ReturnMapInput::Properties => {
                return Err(error(
                    source.at,
                    GraphPatternTextErrorKind::Expected(
                        "an explicit MERGE relationship property, not a complete property map",
                    ),
                ));
            }
        };
        if let Some(column) = self
            .bindings
            .iter()
            .position(|(old, _)| match (old, binding) {
                (EdgeBinding::Edge, EdgeBinding::Edge) => true,
                (EdgeBinding::Property(old), EdgeBinding::Property(key)) => old.text == key.text,
                _ => false,
            })
        {
            return Ok(column);
        }
        parser.capacity(
            self.bindings.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        self.bindings.push((binding, kind));
        Ok(self.bindings.len() - 1)
    }
    fn types(&self) -> Vec<GraphSetColumnType> {
        self.bindings.iter().map(|(_, kind)| *kind).collect()
    }
}
impl<'a> ParsedEdgeReturn<'a> {
    pub(super) fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<EdgeReturnTemplate, GraphPatternTextError> {
        let mut bindings = Vec::new();
        for (binding, _) in self.bindings {
            bindings.push(match binding {
                EdgeBinding::Edge => GraphEdgeReturnBinding::Edge,
                EdgeBinding::Property(name) => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)?
                    else {
                        unreachable!("shared resolver checked the symbol domain")
                    };
                    GraphEdgeReturnBinding::Property(key)
                }
            });
        }
        Ok(EdgeReturnTemplate {
            bindings,
            projection: self.projection,
            grouping: self.grouping,
            quantifier: self.quantifier,
            order: self.order,
            offset: self.offset,
            count: self.count,
            at: self.at,
            tail: self.tail,
        })
    }
}
impl<'a> Parser<'a> {
    pub(super) fn edge_upsert_return(
        &mut self,
        variable: Name<'a>,
    ) -> Result<ParsedEdgeReturn<'a>, GraphEdgeUpsertTextError> {
        let at = self.current.at;
        let head = self.take_word("WITH")?;
        if !head {
            self.word("RETURN")?;
        }
        let quantifier = if self.take_word("DISTINCT")? {
            GraphSetQuantifier::Distinct
        } else {
            self.take_all_quantifier()?;
            GraphSetQuantifier::All
        };
        // RETURN * would also include bound endpoints. Do not silently drop
        // them from this deliberately singular selected-relationship profile.
        if matches!(self.current.kind, TokenKind::Punct(b'*')) {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected("explicit MERGE relationship RETURN columns"),
            )
            .into());
        }
        let mut returning = ParsedEdgeReturn {
            variable,
            bindings: Vec::new(),
            projection: Vec::new(),
            grouping: None,
            quantifier,
            order: Vec::new(),
            offset: ReadPageNumber::Literal(0),
            count: None,
            at,
            tail: None,
        };
        let items = self.write_return_items(&mut returning)?;
        returning.projection = items.projection;
        returning.grouping = items.grouping;
        if let Some(ReadStageTemplate::Page {
            order,
            offset,
            count,
            ..
        }) = self.row_page_sourced(&items.output, &items.spellings)?
        {
            returning.order = order;
            returning.offset = offset;
            returning.count = count;
        }
        if head {
            returning.tail = Some(self.edge_upsert_tail(&mut returning, items.output)?);
        }
        Ok(returning)
    }

    /// Carry the selected relationship under a direct alias or identity key.
    /// Its hidden property cells are read after all ON/SET actions by the
    /// existing owner, with the same authority and rollback boundary.
    fn edge_upsert_tail(
        &mut self,
        returning: &mut ParsedEdgeReturn<'a>,
        mut schema: Vec<(Name<'a>, GraphSetColumnType)>,
    ) -> Result<Vec<ReadStageTemplate>, GraphEdgeUpsertTextError> {
        let visible = schema.len();
        let mut reads = Vec::new();
        for (alias, key) in self.write_tail_reads(&schema)? {
            let Some(at) = schema[..visible]
                .iter()
                .position(|(name, _)| name.text == alias.text)
            else {
                continue;
            };
            let Some(binding) = WriteReturnGroupTemplate::carried_binding(
                returning.grouping.as_ref(),
                &returning.projection[at].value,
            ) else {
                continue;
            };
            if !matches!(
                returning.bindings[binding],
                (EdgeBinding::Edge, GraphSetColumnType::Edge)
            ) {
                continue;
            }
            let variable = returning.variable;
            let column = returning.map_input(self, variable, ReturnMapInput::Property(key))?;
            let column = match &mut returning.grouping {
                Some(group) => group.hoist_property(&mut returning.projection, column, key.at)?,
                None => column,
            };
            mutation_query::push_tail_read(
                &mut returning.projection,
                &mut schema,
                &mut reads,
                (alias, key),
                column,
            )?;
        }
        Ok(self.finish_write_tail(schema, visible, reads)?)
    }
}
impl EdgeReturnTemplate {
    fn bind(
        &self,
        upsert: PreparedGraphEdgeUpsert,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphEdgeUpsertQuery, GraphEdgeUpsertTextError> {
        let mut projection = Vec::new();
        for output in &self.projection {
            projection.push(GraphSetProjection::new(
                &output.name,
                return_projection::bind_read_value(&output.value, values)?,
            ));
        }
        let mut query = PreparedGraphEdgeUpsertQuery::prepare_with_grouping(
            upsert,
            self.bindings.clone(),
            projection,
            self.quantifier,
            self.grouping
                .as_ref()
                .map(|group| group.bind(values))
                .transpose()?,
        )
        .map_err(|kind| GraphEdgeUpsertTextError {
            offset: self.at,
            kind: GraphEdgeUpsertTextErrorKind::ReturnBuild(kind),
        })?;
        if !self.order.is_empty() {
            query = query
                .with_order_by(&self.order)
                .map_err(|kind| crate::GraphSetTextError {
                    offset: self.at,
                    kind: crate::GraphSetTextErrorKind::OrderBuild(kind),
                })?;
        }
        let query = query.with_page(
            return_projection::pipeline::page_value(&self.offset, values),
            self.count
                .as_ref()
                .map(|count| return_projection::pipeline::page_value(count, values)),
        );
        let Some(stages) = &self.tail else {
            return Ok(query);
        };
        let owned =
            crate::PreparedGraphSet::owned(query.columns().to_vec(), query.column_types().to_vec());
        let tail = return_projection::bind_stages(owned, stages, values)?;
        Ok(query.with_tail(tail))
    }
}
impl PreparedGraphEdgeUpsertQueryText {
    /// Classify a MATCH-fed relationship MERGE with ON/SET and RETURN using
    /// native token framing, without binding symbols or inspecting storage.
    /// Plain MERGE RETURN retains its multi-row owner; vertex MERGE and other
    /// writes retain theirs. Preparation, not this probe, validates grammar.
    pub fn has_return_clause(statement: &str) -> Result<bool, GraphEdgeUpsertTextError> {
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
        if !matches!(first.kind, TokenKind::Word(word)
                if word.eq_ignore_ascii_case("MATCH") || word.eq_ignore_ascii_case("OPTIONAL"))
        {
            return Ok(false);
        }
        const PREFIX_ENDS: [&str; 10] = [
            "WITH", "UNWIND", "CREATE", "INSERT", "DELETE", "DETACH", "SET", "REMOVE", "CALL",
            "RETURN",
        ];
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
                        if PREFIX_ENDS
                            .iter()
                            .any(|name| word.eq_ignore_ascii_case(name))
                        {
                            return Ok(false);
                        }
                        if word.eq_ignore_ascii_case("MERGE")
                            && matches!(
                                script::next_script_token(&mut lexer.clone())?.kind,
                                TokenKind::Punct(b'(')
                            )
                        {
                            return Self::actions_then_return(&mut lexer);
                        }
                    }
                }
                _ => {}
            }
            previous = token.kind;
        }
    }

    fn actions_then_return(lexer: &mut Lexer<'_>) -> Result<bool, GraphEdgeUpsertTextError> {
        let mut depth = 0_usize;
        let mut nodes = 0_u8;
        loop {
            let token = script::next_script_token(lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(close @ (b')' | b']' | b'}')) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && close == b')' {
                        nodes += 1;
                        let next = script::next_script_token(&mut lexer.clone())?.kind;
                        if nodes > 1 {
                            if matches!(next, TokenKind::Word(word)
                                if word.eq_ignore_ascii_case("ON") || word.eq_ignore_ascii_case("SET"))
                            {
                                break;
                            }
                            return Ok(false);
                        }
                        if !matches!(next, TokenKind::Punct(b'-' | b'<')) {
                            return Ok(false);
                        }
                    }
                }
                _ => {}
            }
        }
        let mut previous = TokenKind::End;
        loop {
            let token = script::next_script_token(lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Word(word) if depth == 0 && word.eq_ignore_ascii_case("RETURN") => {
                    let alias = matches!(previous, TokenKind::Punct(b'.'))
                        || matches!(previous, TokenKind::Word(word) if word.eq_ignore_ascii_case("AS"));
                    if !alias {
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
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphEdgeUpsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }
    pub fn prepare_with_parameter_types(
        statement: &str,
        _relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphEdgeUpsertTextError> {
        let body =
            mutation_query::statement_body(statement, "one relationship MERGE RETURN statement")?;
        let (upsert, returning) = PreparedGraphEdgeUpsertText::prepare_definition(
            body,
            declarations,
            &mut resolve,
            true,
        )?;
        Ok(Self {
            statement: statement.to_owned(),
            upsert,
            returning: returning.expect("query mode prepares a RETURN definition"),
        })
    }
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.upsert.parameter_schema()
    }
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphEdgeUpsertQuery, GraphEdgeUpsertTextError> {
        let upsert = self.upsert.bind_parameters(arguments)?;
        let values = self.upsert.merge.input.checked_arguments(arguments)?;
        self.returning.bind(upsert, &values)
    }
}

#[cfg(test)]
mod classification_tests {
    use super::*;

    #[test]
    fn branch_return_classifier_preserves_native_statement_ownership() {
        for text in [
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) ON MATCH SET r.p=1 RETURN r",
            "MATCH (a),(b) MERGE (a)-[r:R]-(b) SET r.p=1 RETURN r.p",
            "/* MERGE RETURN */ MATCH (a),(b) MERGE (a)-[r:R {p:'RETURN'}]-(b) \
             ON CREATE SET r.p='SET' ON MATCH SET r.p='ON' RETURN r",
            "MATCH (a),(b) WHERE a.RETURN=1 MERGE (a)<-[r:R]-(b) \
             ON CREATE SET r.RETURN=2 RETURN r.RETURN AS value;",
        ] {
            assert!(
                PreparedGraphEdgeUpsertQueryText::has_return_clause(text).unwrap(),
                "{text}"
            );
            assert!(
                !crate::PreparedGraphEdgeMergeQueryText::has_return_clause(text).unwrap(),
                "{text}"
            );
        }
        for text in [
            "MERGE (a) ON CREATE SET a.p=1 RETURN a",
            "MATCH (a) CREATE (b) RETURN b",
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) RETURN r",
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) SET r.p='RETURN'",
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) SET r.RETURN=1",
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) SET r.p='; RETURN r'",
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) SET r.p=1; MATCH (n) RETURN n",
        ] {
            assert!(
                !PreparedGraphEdgeUpsertQueryText::has_return_clause(text).unwrap(),
                "{text}"
            );
        }
    }
}
