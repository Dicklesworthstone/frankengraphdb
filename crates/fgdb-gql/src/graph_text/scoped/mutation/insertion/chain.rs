//! Chained CREATE statements (fgdb-cpny9): framing, preparation and binding.
//! The CREATE compiler prepares every step; this module only threads one
//! step's WITH tail row into the next and binds each step's own arguments.

use super::*;
use crate::mutation_text::PreparedGraphVertexMergeRowsText;
use crate::write_chain::ChainTextStep;
use crate::{
    GraphWriteChainStep, GraphWriteChainTextError, PreparedGraphWriteChain,
    PreparedGraphWriteChainText,
};

impl PreparedGraphWriteChainText {
    /// Classify with native token framing. A chain is a top-level write
    /// followed by another write in the same statement:
    /// - a CREATE, a WITH after it, then another CREATE;
    /// - a MERGE after a CREATE, with or without a WITH between;
    /// - a vertex MERGE first, then another CREATE or MERGE.
    ///
    /// Consecutive CREATE clauses without a WITH are one insertion. A first
    /// MERGE with ON or SET clauses is an upsert, not a chain. Quoted values,
    /// property keys, aliases, `ON CREATE` and `STARTS WITH` / `ENDS WITH`
    /// are not clauses. A second statement ends the scan. This validates
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
        // Peek the first clause without consuming it: it may be the write.
        let first = script::next_script_token(&mut lexer.clone())?;
        if !matches!(first.kind, TokenKind::Word(word) if ["CREATE", "INSERT", "UNWIND", "MATCH", "MERGE", "WITH"].iter().any(|kind| word.eq_ignore_ascii_case(kind)))
        {
            return Ok(false);
        }
        // `written`: a CREATE was seen; `merged`: the first clause is a
        // vertex MERGE; `headed`: a WITH followed the CREATE; `unwound`: the
        // first clause is an UNWIND of a non-parameter source; `matched`: the
        // first clause is a MATCH. An UNWIND of a `$param` before MERGE is
        // the parameter batch writer's (unwind_write).
        let (mut written, mut merged, mut headed) = (false, false, false);
        let (mut unwound, mut matched) = (false, false);
        // `UNWIND $rows`: one MERGE after it is the batch writer's, but a
        // vertex MERGE another write follows heads a chain (fgdb-0s2pq).
        let mut parameter_unwound = false;
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
                    // A clause starts a node pattern; a bound name does not.
                    let pattern = || -> Result<bool, GraphPatternTextError> {
                        Ok(matches!(
                            script::next_script_token(&mut lexer.clone())?.kind,
                            TokenKind::Punct(b'(')
                        ))
                    };
                    if clause
                        && (word.eq_ignore_ascii_case("CREATE")
                            || word.eq_ignore_ascii_case("INSERT"))
                    {
                        if pattern()? {
                            if headed || merged {
                                return Ok(true);
                            }
                            written = true;
                        }
                    } else if clause && written && word.eq_ignore_ascii_case("WITH") {
                        headed = true;
                    } else if previous.is_none() && word.eq_ignore_ascii_case("UNWIND") {
                        parameter_unwound = matches!(
                            script::next_script_token(&mut lexer.clone())?.kind,
                            TokenKind::Parameter(_)
                        );
                        unwound = !parameter_unwound;
                    } else if previous.is_none() && word.eq_ignore_ascii_case("WITH") {
                        // A leading source-free WITH (`WITH 42 AS v MERGE ...`)
                        // is a row prefix, as a leading UNWIND is.
                        unwound = true;
                    } else if previous.is_none() && word.eq_ignore_ascii_case("MATCH") {
                        matched = true;
                    } else if clause && word.eq_ignore_ascii_case("MERGE") && pattern()? {
                        if written || merged {
                            return Ok(true);
                        }
                        // A first-clause vertex MERGE heads a chain, and one
                        // after a leading UNWIND or MATCH is one; a
                        // relationship MERGE there is its own statement.
                        if previous.is_none() || unwound || matched || parameter_unwound {
                            let mut lookahead = lexer.clone();
                            let mut nested = 0_usize;
                            loop {
                                match script::next_script_token(&mut lookahead)?.kind {
                                    TokenKind::Punct(b'(' | b'[' | b'{') => nested += 1,
                                    TokenKind::Punct(b')' | b']' | b'}') => {
                                        nested = nested.saturating_sub(1);
                                        if nested == 0 {
                                            break;
                                        }
                                    }
                                    TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                                    _ => {}
                                }
                            }
                            let vertex = !matches!(
                                script::next_script_token(&mut lookahead)?.kind,
                                TokenKind::Punct(b'-' | b'<')
                            );
                            if previous.is_some() && !parameter_unwound && (vertex || !matched) {
                                // A vertex MERGE after the leading UNWIND or
                                // MATCH; a relationship MERGE after UNWIND is
                                // its own statement.
                                return Ok(vertex);
                            }
                            // A first vertex MERGE, a vertex MERGE after
                            // `UNWIND $rows`, or a relationship MERGE after a
                            // leading MATCH: a chain once another write
                            // follows.
                            merged = vertex || matched;
                        }
                    }
                    // A first MERGE's ON/SET clauses stay with it: a lone
                    // MERGE with them is the unique upsert's statement, and
                    // one another write follows heads a chain.
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
    ) -> Result<Self, GraphWriteChainTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphWriteChainTextError> {
        let body = query::statement_body(statement)?;
        // The first write is a MERGE (`first_merge` is where it starts) when
        // it is the first clause or follows only a leading UNWIND, or a
        // leading MATCH and its read stages; otherwise it is a CREATE.
        let first_merge = {
            let mut lexer = Lexer {
                text: body,
                at: 0,
                tokens: 0,
            };
            let (mut depth, mut first, mut unwound) = (0_usize, true, false);
            let mut matched = false;
            loop {
                let token = script::next_script_token(&mut lexer)?;
                match token.kind {
                    TokenKind::End | TokenKind::Punct(b';') => break None,
                    TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                    TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                    TokenKind::Word(word) if depth == 0 => {
                        if word.eq_ignore_ascii_case("MERGE") {
                            break (first || unwound || matched).then_some(token.at);
                        }
                        if word.eq_ignore_ascii_case("UNWIND") {
                            unwound |= first;
                        } else if first && word.eq_ignore_ascii_case("WITH") {
                            // A leading source-free WITH is a row prefix.
                            unwound = true;
                        } else if first && word.eq_ignore_ascii_case("MATCH") {
                            matched = true;
                        } else if ["CREATE", "INSERT"]
                            .iter()
                            .any(|clause| word.eq_ignore_ascii_case(clause))
                            || (!matched
                                && (["MATCH", "OPTIONAL"]
                                    .iter()
                                    .any(|clause| word.eq_ignore_ascii_case(clause))
                                    || (!unwound && word.eq_ignore_ascii_case("WITH"))))
                        {
                            break None;
                        }
                    }
                    _ => {}
                }
                first = false;
            }
        };
        // A first MERGE whose node pattern a `-` or `<` follows is a
        // relationship MERGE (after a leading MATCH, as is_chain admits).
        let relationship = match first_merge {
            Some(at) => {
                let mut lexer = Lexer {
                    text: body,
                    at,
                    tokens: 0,
                };
                script::next_script_token(&mut lexer)?;
                let mut nested = 0_usize;
                loop {
                    match script::next_script_token(&mut lexer)?.kind {
                        TokenKind::Punct(b'(' | b'[' | b'{') => nested += 1,
                        TokenKind::Punct(b')' | b']' | b'}') => {
                            nested = nested.saturating_sub(1);
                            if nested == 0 {
                                break;
                            }
                        }
                        TokenKind::End | TokenKind::Punct(b';') => break,
                        _ => {}
                    }
                }
                matches!(
                    script::next_script_token(&mut lexer)?.kind,
                    TokenKind::Punct(b'-' | b'<')
                )
            }
            None => false,
        };
        let mut steps = Vec::new();
        let mut next = if first_merge.is_some() && relationship {
            let (merge, returning, after) = crate::PreparedGraphEdgeMergeText::prepare_first_step(
                body,
                declarations,
                &mut resolve,
            )?;
            match (returning, after) {
                // A WITH head: a query step whose rows the next write reads.
                (Some(returning), Some((_, from))) => {
                    steps.push(ChainTextStep::EdgeMerge(Box::new((merge, Some(returning)))));
                    Some(from)
                }
                (None, Some((keep, from))) => {
                    steps.push(ChainTextStep::EdgeMergeRows(Box::new((merge, keep))));
                    Some(from)
                }
                (returning, None) => {
                    steps.push(ChainTextStep::EdgeMerge(Box::new((merge, returning))));
                    None
                }
            }
        } else if first_merge.is_some() {
            let (merge, after) = PreparedGraphVertexMergeRowsText::prepare_step(
                body,
                relation,
                declarations,
                &mut resolve,
                None,
            )?;
            steps.push(ChainTextStep::VertexMerge(Box::new(merge)));
            after
        } else {
            let (first, returning, after) = PreparedGraphInsertText::prepare_step(
                body,
                relation,
                declarations,
                crate::graph_text::LentResolver(&mut resolve),
                true,
                true,
            )?;
            steps.push(ChainTextStep::Insert(Box::new((first, returning))));
            after
        };
        while let Some(from) = next {
            // The continuation is at the next write's own clause.
            let mut lexer = Lexer {
                text: body,
                at: from.at,
                tokens: 0,
            };
            let merge = matches!(lexer.next()?.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("MERGE"));
            // A relationship MERGE names row vertices: `(a)-` or `(a)<-`.
            let relationship = merge
                && matches!(lexer.next()?.kind, TokenKind::Punct(b'('))
                && matches!(lexer.next()?.kind, TokenKind::Word(_))
                && matches!(lexer.next()?.kind, TokenKind::Punct(b')'))
                && matches!(lexer.next()?.kind, TokenKind::Punct(b'-' | b'<'));
            if merge && !relationship {
                let (merge, after) = PreparedGraphVertexMergeRowsText::prepare_step(
                    body,
                    relation,
                    declarations,
                    &mut resolve,
                    Some(from),
                )?;
                steps.push(ChainTextStep::VertexMerge(Box::new(merge)));
                next = after;
            } else if merge {
                let (merge, returning, after) = crate::PreparedGraphEdgeMergeText::prepare_chained(
                    body,
                    declarations,
                    &mut resolve,
                    from,
                )?;
                match (returning, after) {
                    // A WITH head: a query step whose rows the next write reads.
                    (Some(returning), Some((_, from))) => {
                        steps.push(ChainTextStep::EdgeMerge(Box::new((merge, Some(returning)))));
                        next = Some(from);
                    }
                    (None, Some((keep, from))) => {
                        steps.push(ChainTextStep::EdgeMergeRows(Box::new((merge, keep))));
                        next = Some(from);
                    }
                    (returning, None) => {
                        steps.push(ChainTextStep::EdgeMerge(Box::new((merge, returning))));
                        next = None;
                    }
                }
            } else {
                let (step, returning, after) = PreparedGraphInsertText::prepare_chained(
                    body,
                    relation,
                    declarations,
                    crate::graph_text::LentResolver(&mut resolve),
                    from,
                )?;
                steps.push(ChainTextStep::Insert(Box::new((step, returning))));
                next = after;
            }
        }
        // One write is a chain only as a vertex MERGE over a leading UNWIND
        // or MATCH (`UNWIND [1, 2] AS x MERGE (n {id: x}) RETURN n`, `MATCH
        // (p:Person) MERGE (c:City {name: p.bornIn})`).
        let unwound_merge = matches!(steps.as_slice(), [ChainTextStep::VertexMerge(merge)]
            if matches!(merge.input, crate::mutation_text::VertexMergeRowsTextInput::Relation(_)));
        if steps.len() < 2 && !unwound_merge {
            return Err(expected(body.len(), "another write after the first").into());
        }
        let chain = Self {
            statement: statement.to_owned(),
            steps,
        };
        // Each step skipped the unused-declaration check, since another step
        // may read a declaration; every one must name a parameter some step
        // reads.
        let schema = chain.parameter_schema();
        if declarations
            .iter()
            .any(|(name, _)| !schema.iter().any(|spec| spec.name == *name))
        {
            return Err(error(
                body.len(),
                GraphPatternTextErrorKind::UnusedParameterDeclaration,
            )
            .into());
        }
        Ok(chain)
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
        for step in &self.steps {
            let specs = match step {
                ChainTextStep::Insert(step) => step.0.parameter_schema(),
                ChainTextStep::EdgeMerge(step) => step.0.parameter_schema(),
                ChainTextStep::EdgeMergeRows(step) => step.0.parameter_schema(),
                ChainTextStep::VertexMerge(merge) => merge.parameter_schema(),
            };
            for spec in specs {
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
    ) -> Result<PreparedGraphWriteChain, GraphWriteChainTextError> {
        let mut named = std::collections::BTreeSet::new();
        // A step's own arguments, by its schema names: each was already
        // admitted into `arguments`, so the subset admits too.
        let mut own =
            |schema: &[GqlParameterSpec]| -> Result<GqlParameters, GraphPatternTextError> {
                let mut own = GqlParameters::new();
                for spec in schema {
                    named.insert(spec.name.clone());
                    if let Some(value) = arguments.get(&spec.name) {
                        own.insert(spec.name.clone(), value).map_err(|_| {
                            error(
                                self.statement.len(),
                                GraphPatternTextErrorKind::UnexpectedArguments,
                            )
                        })?;
                    }
                }
                Ok(own)
            };
        let mut steps = Vec::with_capacity(self.steps.len());
        let mut previous: Option<Vec<GraphSetColumnType>> = None;
        // The row a later step reads is exactly the previous step's output.
        let reads_previous = |owned: &Option<(Vec<String>, Vec<GraphSetColumnType>)>,
                              previous: &Option<Vec<GraphSetColumnType>>,
                              at: usize|
         -> Result<(), GraphInsertTextError> {
            match owned {
                Some((_, types)) if previous.as_ref() != Some(types) => {
                    Err(expected(at, "the previous write's WITH row"))
                }
                _ => Ok(()),
            }
        };
        for step in &self.steps {
            match step {
                ChainTextStep::VertexMerge(merge) => {
                    let own = own(merge.parameter_schema())?;
                    if let Some(input) = merge.owned_column_types()
                        && previous.as_deref() != Some(input)
                    {
                        return Err(expected(merge.end, "the previous write's WITH row").into());
                    }
                    let bound = merge.bind_parameters(&own)?;
                    // Its rows: the row's visible columns, then the vertex;
                    // or the rows of its WITH head (or RETURN).
                    previous = match &bound {
                        GraphWriteChainStep::VertexMerge(rows) => {
                            let mut output = rows.input_column_types().to_vec();
                            output.truncate(merge.keep);
                            output.push(GraphSetColumnType::Vertex);
                            Some(output)
                        }
                        GraphWriteChainStep::VertexMergeQuery(query) => {
                            Some(query.column_types().to_vec())
                        }
                        _ => None,
                    };
                    steps.push(bound);
                }
                ChainTextStep::Insert(step) => {
                    let (insertion, returning) = &**step;
                    let values =
                        insertion.checked_arguments(&own(insertion.parameter_schema())?)?;
                    if let InsertTextInput::Relation { input, .. } = &insertion.input {
                        reads_previous(&input.owned, &previous, insertion.create_at)?;
                    }
                    let selection = match &insertion.input {
                        InsertTextInput::Match(selection) => Some(selection.bind_values(&values)?),
                        InsertTextInput::Relation { .. } | InsertTextInput::Unit { .. } => None,
                    };
                    let bound = insertion.instantiate(selection, Some(&values))?;
                    steps.push(match returning {
                        Some(returning) => {
                            let query = returning.bind(bound, &values)?;
                            previous = Some(query.column_types().to_vec());
                            GraphWriteChainStep::Query(Box::new(query))
                        }
                        None => {
                            previous = None;
                            GraphWriteChainStep::Insert(bound)
                        }
                    });
                }
                ChainTextStep::EdgeMerge(step) => {
                    let (merge, returning) = &**step;
                    let own = own(merge.parameter_schema())?;
                    if let crate::mutation_text::EdgeMergeTextInput::Row { input, .. } =
                        &merge.input
                    {
                        reads_previous(&input.owned, &previous, merge.input.return_at())?;
                    }
                    let values = merge
                        .input
                        .checked_arguments(&own)
                        .map_err(crate::GraphEdgeMergeTextError::from)?;
                    let bound = merge.bind_parameters(&own)?;
                    // A RETURN, or a WITH head another write reads.
                    steps.push(match returning {
                        Some(returning) => {
                            let query = returning.bind(bound, &values)?;
                            previous = Some(query.column_types().to_vec());
                            GraphWriteChainStep::EdgeMergeQuery(Box::new(query))
                        }
                        None => {
                            previous = None;
                            GraphWriteChainStep::EdgeMerge(Box::new(bound))
                        }
                    });
                }
                ChainTextStep::EdgeMergeRows(step) => {
                    let (merge, keep) = &**step;
                    let own = own(merge.parameter_schema())?;
                    if let crate::mutation_text::EdgeMergeTextInput::Row { input, .. } =
                        &merge.input
                    {
                        reads_previous(&input.owned, &previous, merge.input.return_at())?;
                    }
                    let bound = merge.bind_parameters(&own)?;
                    // Its rows: the row's visible columns, then the edge. The
                    // row is the previous write's, or a MATCH-led first step's
                    // own read row, which no owned leaf carries.
                    let mut output = bound.input_column_types();
                    output.truncate(*keep);
                    output.push(GraphSetColumnType::Edge);
                    previous = Some(output);
                    steps.push(GraphWriteChainStep::EdgeMergeRows {
                        merge: Box::new(bound),
                        keep: *keep,
                    });
                }
            }
        }
        if arguments
            .parameter_types()
            .any(|(name, _)| !named.contains(name))
        {
            return Err(GraphInsertTextError::from(error(
                self.statement.len(),
                GraphPatternTextErrorKind::UnexpectedArguments,
            ))
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
            // A relationship MERGE after a CREATE, with or without WITH.
            "CREATE (a), (b) MERGE (a)-[:X]->(b) RETURN count(a)",
            "CREATE (a), (b) WITH a, b MERGE (a)<-[r:X]-(b)",
            // A vertex MERGE after a CREATE, or first and then another write
            // (TCK Merge5 [9], Create3 [11], [13], Merge1 [7]).
            "CREATE (a) MERGE (b:X)",
            "CREATE (:X) CREATE (:X) MERGE (:X)",
            "CREATE (a) WITH a MERGE () CREATE (b) CREATE (a)<-[:T]-(b)",
            "MERGE (a:A) MERGE (b:B) MERGE (a)-[:FOO]->(b)",
            "MERGE (t:T {id: 42}) CREATE (f:R) CREATE (t)-[:REL]->(f)",
            // A vertex MERGE over a leading UNWIND of a non-parameter source
            // (TCK Merge9 [1], [2]).
            "UNWIND [1] AS x MERGE (n {id: x})",
            "UNWIND [1, 2, 3, 4] AS int MERGE (n {id: int}) RETURN count(*)",
            "UNWIND ['a', 'b'] AS actor MERGE (m:Movie {name: 'M'}) MERGE (p:Person {name: actor}) \
             MERGE (p)-[:ACTED_IN]->(m)",
            // A vertex MERGE over a leading MATCH, directly or after its
            // read stages (TCK Merge1 [9], [11]).
            "MATCH (a) MERGE (b) RETURN b",
            "MATCH (person:Person) MERGE (city:City {name: person.bornIn})",
            "MATCH (foo) WITH foo.x AS x, foo.y AS y MERGE (:N {x: x, y: y}) RETURN x, y",
            "MATCH (a) UNWIND [1, 2] AS i MERGE (b {i: i})",
            // ON/SET clauses on a MERGE step (TCK Merge4 [1], [2]), and on a
            // first MERGE another write follows.
            "MATCH () MERGE (a:L) ON MATCH SET a:M1 ON CREATE SET a:M2",
            "MATCH (person:Person) MERGE (city:City) ON MATCH SET city.name = person.bornIn \
             ON CREATE SET city.name = person.bornIn RETURN person.bornIn",
            "MERGE (a:A) ON CREATE SET a.p = 1 MERGE (b:B)",
            "MERGE (a:A) SET a.p = 1 CREATE (a)-[:R]->(b)",
            // A relationship MERGE over a leading MATCH another write follows
            // (TCK Merge5 [18]).
            "MATCH (a), (b) MERGE (a)-[:X]->(b) MERGE (c)",
            "MATCH (n) MATCH (m) WITH n AS a, m AS b MERGE (a)-[:T]->(b) WITH a AS x, b AS y \
             MERGE (a) MERGE (b) MERGE (a)-[:T]->(b) RETURN x.id AS x, y.id AS y",
            // A vertex MERGE after source-free WITH stages (TCK Merge1 [8],
            // Merge9 [4]).
            "WITH 42 AS var MERGE (c:N {var: var})",
            "UNWIND [42] AS props WITH props WHERE props > 32 WITH DISTINCT props AS p \
             MERGE (a:A {num: p}) RETURN a.num AS prop",
            // Chained writes over a parameter UNWIND (fgdb-0s2pq).
            "UNWIND $pairs AS row MERGE (a:N {id: row.a}) MERGE (b:N {id: row.b}) \
             MERGE (a)-[:S]->(b)",
            "UNWIND $rows AS row MERGE (n:N {id: row.id}) CREATE (n)-[:R]->(:M)",
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
            // A lone MATCH-led or UNWIND-led relationship MERGE, a first
            // relationship MERGE, and a lone first MERGE with ON or SET
            // clauses keep their own families.
            "MERGE (a)-[:X]->(b)",
            "MERGE (a)-[:X]->(b) MERGE (c)",
            "MATCH (a), (b) WITH a, b MERGE (a)-[:X]->(b)",
            "MATCH (a), (b) MERGE (a)-[:X]->(b) RETURN a",
            "OPTIONAL MATCH (a) MERGE (b)",
            // An UNWIND of a parameter before one MERGE is the batch writer's,
            // its ON/SET clauses and a relationship MERGE after MATCH included.
            "UNWIND $rows AS row MERGE (n:Entity {id: row.id})",
            "UNWIND $rows AS row MERGE (n:Entity {id: row.id}) SET n.name = row.name",
            "UNWIND $pairs AS row MATCH (a {id: row.a}), (b {id: row.b}) MERGE (a)-[:R]->(b)",
            "UNWIND [1] AS x MERGE (a)-[:R]->(b)",
            "MERGE (a:A) ON CREATE SET a.p = 1",
            "MERGE (a:A) ON CREATE SET a.p = 1 ON MATCH SET a.q = 2 RETURN a",
            "MERGE (n:L) SET n.p = 1",
            "MERGE (n:L) RETURN n",
        ] {
            assert!(
                !PreparedGraphWriteChainText::is_chain(text).unwrap(),
                "{text}"
            );
        }
    }
}
