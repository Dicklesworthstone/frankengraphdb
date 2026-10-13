//! Chained CREATE statements (fgdb-cpny9): framing, preparation and binding.
//! The CREATE compiler prepares every step; this module only threads one
//! step's WITH tail row into the next and binds each step's own arguments.

use super::*;
use crate::{GraphWriteChainStep, PreparedGraphWriteChain, PreparedGraphWriteChainText};

impl PreparedGraphWriteChainText {
    /// Classify with native token framing: a top-level CREATE, a WITH after
    /// it, then another CREATE. Quoted values, property keys, aliases, `ON
    /// CREATE` and `STARTS WITH` / `ENDS WITH` are not clauses. A second
    /// statement ends the scan; a chain is one statement. This validates
    /// nothing and resolves no symbol.
    pub fn is_chain(statement: &str) -> Result<bool, GraphInsertTextError> {
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
        // Peek the first clause without consuming it: it may be the CREATE.
        let first = script::next_script_token(&mut lexer.clone())?;
        if !matches!(first.kind, TokenKind::Word(word) if ["CREATE", "INSERT", "UNWIND", "MATCH"].iter().any(|kind| word.eq_ignore_ascii_case(kind)))
        {
            return Ok(false);
        }
        let (mut written, mut headed) = (false, false);
        let mut depth = 0_usize;
        let mut previous: Option<TokenKind<'_>> = None;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Word(word) if depth == 0 => {
                    let clause = !matches!(previous, Some(TokenKind::Punct(b'.')))
                        && !matches!(previous, Some(TokenKind::Word(prior))
                            if ["AS", "ON", "STARTS", "ENDS"].iter().any(|not| prior.eq_ignore_ascii_case(not)));
                    if clause
                        && (word.eq_ignore_ascii_case("CREATE")
                            || word.eq_ignore_ascii_case("INSERT"))
                    {
                        // A clause starts a node pattern; a bound name does not.
                        let mut lookahead = lexer.clone();
                        if matches!(
                            script::next_script_token(&mut lookahead)?.kind,
                            TokenKind::Punct(b'(')
                        ) {
                            if headed {
                                return Ok(true);
                            }
                            written = true;
                        }
                    } else if clause && written && word.eq_ignore_ascii_case("WITH") {
                        headed = true;
                    }
                }
                _ => {}
            }
            previous = Some(token.kind);
        }
    }

    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphInsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphInsertTextError> {
        let body = query::statement_body(statement)?;
        let (first, returning, mut next) = PreparedGraphInsertText::prepare_step(
            body,
            relation,
            declarations,
            crate::graph_text::LentResolver(&mut resolve),
            true,
            true,
        )?;
        let mut steps = vec![(first, returning)];
        while let Some(from) = next {
            let (step, returning, after) = PreparedGraphInsertText::prepare_chained(
                body,
                relation,
                declarations,
                crate::graph_text::LentResolver(&mut resolve),
                from,
            )?;
            steps.push((step, returning));
            next = after;
        }
        if steps.len() < 2 {
            return Err(expected(
                body.len(),
                "WITH and another CREATE after the CREATE",
            ));
        }
        Ok(Self {
            statement: statement.to_owned(),
            steps,
        })
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    /// Every parameter of every step, once, in first-use order. A parameter
    /// keeps the type its first step gave it.
    #[must_use]
    pub fn parameter_schema(&self) -> Vec<GqlParameterSpec> {
        let mut schema: Vec<GqlParameterSpec> = Vec::new();
        for (step, _) in &self.steps {
            for spec in step.parameter_schema() {
                if !schema.iter().any(|known| known.name == spec.name) {
                    schema.push(spec.clone());
                }
            }
        }
        schema
    }

    /// Bind each step with exactly the arguments it names. An argument no
    /// step names refuses, as for every native statement.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphWriteChain, GraphInsertTextError> {
        let mut named = std::collections::BTreeSet::new();
        let mut steps = Vec::with_capacity(self.steps.len());
        let mut previous: Option<Vec<GraphSetColumnType>> = None;
        for (insertion, returning) in &self.steps {
            // A step's own arguments, by its schema names: each was already
            // admitted into `arguments`, so the subset admits too.
            let mut own = GqlParameters::new();
            for spec in insertion.parameter_schema() {
                named.insert(spec.name.as_str());
                if let Some(value) = arguments.get(&spec.name) {
                    own.insert(spec.name.clone(), value).map_err(|_| {
                        error(
                            self.statement.len(),
                            GraphPatternTextErrorKind::UnexpectedArguments,
                        )
                    })?;
                }
            }
            let values = insertion.checked_arguments(&own)?;
            // The row a later step reads is exactly the previous step's output.
            if let InsertTextInput::Relation { input, .. } = &insertion.input
                && let Some((_, types)) = &input.owned
                && previous.as_ref() != Some(types)
            {
                return Err(expected(
                    insertion.create_at,
                    "the previous write's WITH row",
                ));
            }
            let selection = match &insertion.input {
                InsertTextInput::Match(selection) => Some(selection.bind_values(&values)?),
                InsertTextInput::Relation { .. } | InsertTextInput::Unit { .. } => None,
            };
            let bound = insertion.instantiate(selection, Some(&values))?;
            let step = match returning {
                Some(returning) => {
                    let query = returning.bind(bound, &values)?;
                    previous = Some(query.column_types().to_vec());
                    GraphWriteChainStep::Query(Box::new(query))
                }
                None => {
                    previous = None;
                    GraphWriteChainStep::Insert(bound)
                }
            };
            steps.push(step);
        }
        if arguments
            .parameter_types()
            .any(|(name, _)| !named.contains(name))
        {
            return Err(error(
                self.statement.len(),
                GraphPatternTextErrorKind::UnexpectedArguments,
            )
            .into());
        }
        Ok(PreparedGraphWriteChain { steps })
    }
}

#[cfg(test)]
mod tests {
    use super::PreparedGraphWriteChainText;

    #[test]
    fn a_chain_is_a_create_then_with_then_another_create_at_top_level() {
        for text in [
            "CREATE (a) WITH a CREATE (b)",
            "CREATE (a) WITH a WITH * CREATE (b) CREATE (a)<-[:T]-(b)",
            "MATCH (n) WITH n AS a CREATE (a)-[:T]->() WITH a AS x CREATE (x)-[:T]->() RETURN x",
            "CREATE (a) WITH a UNWIND [0] AS i CREATE (b)",
            "create (a) with a create (b);",
        ] {
            assert!(
                PreparedGraphWriteChainText::is_chain(text).unwrap(),
                "{text}"
            );
        }
        for text in [
            "CREATE (a) WITH a RETURN a",
            "CREATE (a) CREATE (b)",
            "MATCH (n) WITH n AS a CREATE (a)-[:T]->()",
            "MATCH (create) WITH create CREATE (b)",
            "MATCH (n) WHERE n.name STARTS WITH 'x' CREATE (m) RETURN m",
            "CREATE (n {p: 'WITH CREATE (x)'}) WITH n RETURN n",
            "MERGE (a) ON CREATE SET a.p = 1 WITH a MATCH (b) RETURN b",
            "CREATE (a); MATCH (n) WITH n CREATE (b)",
            "RETURN 1",
        ] {
            assert!(
                !PreparedGraphWriteChainText::is_chain(text).unwrap(),
                "{text}"
            );
        }
    }
}
