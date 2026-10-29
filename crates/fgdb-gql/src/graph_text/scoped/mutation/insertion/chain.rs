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
use std::collections::BTreeSet;

enum MatchedWrite {
    Insert,
    MultipartInsert,
    Mutation,
    MultipartMutation,
    VertexMerge,
    EdgeMerge,
}

impl PreparedGraphWriteChainText {
    /// Classify with native token framing. A chain is a top-level write
    /// followed by another write in the same statement:
    /// - a CREATE, a WITH after it, then another CREATE;
    /// - a MERGE after a CREATE, with or without a WITH between;
    /// - a vertex MERGE first, then another CREATE or MERGE;
    /// - a relationship MERGE introducing an endpoint, or after a WITH.
    ///
    /// Consecutive CREATE clauses without a WITH are one insertion. A first
    /// vertex MERGE with ON or SET clauses is an upsert, not a chain. Quoted values,
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
        if !matches!(first.kind, TokenKind::Word(word) if ["CREATE", "INSERT", "UNWIND", "MATCH", "OPTIONAL", "CALL", "MERGE", "WITH"].iter().any(|kind| word.eq_ignore_ascii_case(kind)))
        {
            return Ok(false);
        }
        // `written`: a CREATE was seen; `merged`: the first clause is a
        // vertex MERGE; `headed`: a WITH followed the CREATE; `unwound`: the
        // first clause is an UNWIND of a non-parameter source; `matched`: the
        // first clause is a MATCH. An UNWIND of a `$param` before MERGE is
        // the parameter batch writer's (unwind_write).
        let (mut written, mut merged, mut headed) = (false, false, false);
        // Whether that MERGE was a vertex MERGE: only one returns rows as a
        // chain; a relationship MERGE stays the batch writer's.
        let mut merged_vertex = false;
        // The read/write framing lexes one statement: a `;` ends its scan
        // exactly as it ends the loop below.
        let first_statement = {
            let mut scan = lexer.clone();
            loop {
                let token = script::next_script_token(&mut scan)?;
                if matches!(token.kind, TokenKind::End | TokenKind::Punct(b';')) {
                    break &statement[..token.at];
                }
            }
        };
        let multipart_mutation =
            PreparedGraphInsertText::multipart_source_kind(first_statement, &[])?
                == Some(ReadWriteKind::Mutation);
        let (mut unwound, mut matched) = (false, multipart_mutation);
        // `UNWIND $rows`: one MERGE after it is the batch writer's, but a
        // vertex MERGE another write follows heads a chain (fgdb-0s2pq).
        let mut parameter_unwound = false;
        // A MATCH after `UNWIND $rows`: the batch writer lowers no vertex
        // MERGE over its bindings, so one heads a chain.
        let mut parameter_matched = false;
        // A MATCH after a leading UNWIND of a non-parameter source: it binds
        // a relationship MERGE's endpoints, which the chain reads per row.
        let mut unwound_matched = false;
        // A SET, REMOVE or DELETE after a leading MATCH: a chain once a
        // CREATE or MERGE follows it.
        let mut mutated = false;
        // A WITH or UNWIND stage after the leading MATCH, before any write:
        // the unique upsert reads a MATCH only, so a relationship MERGE's
        // ON/SET clauses there are the chain's row actions.
        let mut staged = false;
        // Known bindings suffice to retain the established bound-endpoint
        // owner. A WITH may drop, rename or introduce them, so its following
        // relationship MERGE uses the chain's complete native row prefix.
        // This is framing only: preparation checks actual scopes and types.
        let mut bindings = BTreeSet::new();
        let (mut match_pattern, mut unwind_source, mut projected) = (false, false, false);
        let mut depth = 0_usize;
        let mut previous: Option<TokenKind<'_>> = None;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(open @ (b'(' | b'[' | b'{')) => {
                    if depth == 0
                        && match_pattern
                        && matches!(open, b'(' | b'[')
                        && let TokenKind::Word(name) =
                            script::next_script_token(&mut lexer.clone())?.kind
                    {
                        bindings.insert(name);
                    }
                    depth += 1;
                }
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
                    if clause {
                        if word.eq_ignore_ascii_case("MATCH")
                            && (pattern()? || Self::named_merge_pattern(lexer.clone())?)
                        {
                            match_pattern = true;
                            unwind_source = false;
                        } else if [
                            "WHERE", "WITH", "UNWIND", "CALL", "RETURN", "CREATE", "INSERT",
                            "MERGE", "SET", "REMOVE", "DELETE", "DETACH",
                        ]
                        .iter()
                        .any(|end| word.eq_ignore_ascii_case(end))
                        {
                            match_pattern = false;
                            unwind_source = word.eq_ignore_ascii_case("UNWIND");
                            projected |= word.eq_ignore_ascii_case("WITH");
                        }
                    }
                    if unwind_source && word.eq_ignore_ascii_case("AS") {
                        if let TokenKind::Word(name) =
                            script::next_script_token(&mut lexer.clone())?.kind
                        {
                            bindings.insert(name);
                        }
                        unwind_source = false;
                    }
                    let mutation = || -> Result<bool, GraphPatternTextError> {
                        if word.eq_ignore_ascii_case("SET") || word.eq_ignore_ascii_case("REMOVE") {
                            Self::mutation_clause_target(lexer.clone())
                        } else if word.eq_ignore_ascii_case("DELETE")
                            || word.eq_ignore_ascii_case("DETACH")
                        {
                            Self::deletion_clause_target(
                                lexer.clone(),
                                previous,
                                word.eq_ignore_ascii_case("DETACH"),
                            )
                        } else {
                            Ok(false)
                        }
                    };
                    if clause
                        && (written || merged || mutated)
                        && (word.eq_ignore_ascii_case("MATCH")
                            || (word.eq_ignore_ascii_case("OPTIONAL")
                                && matches!(script::next_script_token(&mut lexer.clone())?.kind,
                                    TokenKind::Word(next) if next.eq_ignore_ascii_case("MATCH")))
                            || (word.eq_ignore_ascii_case("CALL")
                                && matches!(
                                    script::next_script_token(&mut lexer.clone())?.kind,
                                    TokenKind::Punct(b'{' | b'(')
                                )))
                    {
                        // A graph read after the write consumes its private
                        // rows and complete staged overlay in this statement.
                        return Ok(true);
                    } else if clause
                        && (word.eq_ignore_ascii_case("CREATE")
                            || word.eq_ignore_ascii_case("INSERT"))
                    {
                        if pattern()? {
                            if headed || merged || mutated {
                                return Ok(true);
                            }
                            written = true;
                        }
                    } else if clause && written && word.eq_ignore_ascii_case("WITH") {
                        headed = true;
                    } else if clause && written && mutation()? {
                        // A SET, REMOVE or DELETE after a CREATE writes its
                        // rows (`CREATE (n) SET n.p = 1`, fgdb-cpny9).
                        return Ok(true);
                    } else if clause && matched && mutation()? {
                        mutated = true;
                    } else if clause
                        && matched
                        && !written
                        && (word.eq_ignore_ascii_case("WITH")
                            || word.eq_ignore_ascii_case("UNWIND"))
                    {
                        staged = true;
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
                    } else if clause && parameter_unwound && word.eq_ignore_ascii_case("MATCH") {
                        parameter_matched = true;
                    } else if clause && unwound && word.eq_ignore_ascii_case("MATCH") {
                        unwound_matched = true;
                    } else if clause
                        && word.eq_ignore_ascii_case("MERGE")
                        && (pattern()? || Self::named_merge_pattern(lexer.clone())?)
                    {
                        if written || merged || mutated {
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
                            if !vertex
                                && (projected
                                    || Self::edge_merge_has_unbound_endpoint(
                                        lexer.clone(),
                                        &bindings,
                                    )?)
                            {
                                return Ok(true);
                            }
                            if parameter_matched && vertex {
                                return Ok(true);
                            }
                            if !vertex
                                && matched
                                && !parameter_unwound
                                && Self::edge_actions_need_rows(
                                    lookahead.clone(),
                                    Self::named_merge_pattern(lexer.clone())?,
                                    staged,
                                )?
                            {
                                return Ok(true);
                            }
                            if previous.is_some() && !parameter_unwound && (vertex || !matched) {
                                // A vertex MERGE after the leading UNWIND or
                                // MATCH. A relationship MERGE after UNWIND is
                                // its own statement, unless a MATCH after the
                                // UNWIND binds its endpoints: the chain reads
                                // those rows (fgdb-at06q).
                                return Ok(vertex || unwound_matched);
                            }
                            // A first vertex MERGE, a vertex MERGE after
                            // `UNWIND $rows`, or a relationship MERGE after a
                            // leading MATCH: a chain once another write
                            // follows.
                            merged = vertex || matched || parameter_matched;
                            merged_vertex = vertex;
                        }
                    } else if clause
                        && parameter_unwound
                        && (merged_vertex || (merged && parameter_matched))
                        && word.eq_ignore_ascii_case("RETURN")
                    {
                        // The batch writer has no RETURN: a vertex MERGE over
                        // `UNWIND $rows` that returns rows is the chain's, as
                        // over a literal UNWIND (fgdb-0s2pq), and so is a
                        // relationship MERGE between the MATCH's vertices
                        // there (fgdb-at06q).
                        return Ok(true);
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

    /// Inspect endpoint names without mistaking labels, map values, quoted
    /// text or a named path for bindings. An anonymous endpoint is new too.
    /// The caller already framed a relationship after the first node; the
    /// actual pattern compiler owns malformed or multi-edge patterns.
    fn edge_merge_has_unbound_endpoint(
        mut lexer: Lexer<'_>,
        bindings: &BTreeSet<&str>,
    ) -> Result<bool, GraphPatternTextError> {
        let (mut depth, mut endpoints) = (0_usize, 0_usize);
        loop {
            match script::next_script_token(&mut lexer)?.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(open @ (b'(' | b'[' | b'{')) => {
                    if depth == 0 && open == b'(' {
                        endpoints += 1;
                        if !matches!(
                            script::next_script_token(&mut lexer.clone())?.kind,
                            TokenKind::Word(name) if bindings.contains(name)
                        ) {
                            return Ok(true);
                        }
                    }
                    depth += 1;
                }
                TokenKind::Punct(b')' | b']' | b'}') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && endpoints == 2 {
                        return Ok(false);
                    }
                }
                _ => {}
            }
        }
    }

    /// Whether the ON/SET clauses of a relationship MERGE after a leading
    /// MATCH need the chain's row actions (fgdb-at06q): the relationship is
    /// anonymous, or an assignment writes or reads (`a.key`) an element
    /// other than it. The unique upsert writes only its named relationship.
    /// `lexer` is just past the `-` or `<` after the first node pattern.
    /// `any`: every ON/SET action needs the chain's rows, as after a WITH or
    /// UNWIND stage, which the unique MATCH-only upsert cannot read.
    fn edge_actions_need_rows(
        mut lexer: Lexer<'_>,
        named_path: bool,
        any: bool,
    ) -> Result<bool, GraphPatternTextError> {
        const ENDS: [&str; 12] = [
            "RETURN", "WITH", "CREATE", "INSERT", "MERGE", "MATCH", "OPTIONAL", "UNWIND", "DELETE",
            "DETACH", "REMOVE", "FOREACH",
        ];
        let mut token = script::next_script_token(&mut lexer)?;
        if matches!(token.kind, TokenKind::Punct(b'-')) {
            token = script::next_script_token(&mut lexer)?;
        }
        if !matches!(token.kind, TokenKind::Punct(b'[')) {
            return Ok(false);
        }
        let relationship = match script::next_script_token(&mut lexer)?.kind {
            TokenKind::Word(word) => Some(word),
            _ => None,
        };
        let (mut depth, mut actions) = (1_usize, false);
        let (mut previous, mut before): (Option<TokenKind<'_>>, Option<TokenKind<'_>>) =
            (None, None);
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                // `ON CREATE` and `ON MATCH` name a branch, not a clause.
                TokenKind::Word(word)
                    if depth == 0
                        && ENDS.iter().any(|end| word.eq_ignore_ascii_case(end))
                        && !matches!(previous, Some(TokenKind::Word(on))
                            if on.eq_ignore_ascii_case("ON")) =>
                {
                    return Ok(false);
                }
                TokenKind::Word(word)
                    if depth == 0
                        && (word.eq_ignore_ascii_case("ON")
                            || word.eq_ignore_ascii_case("SET")) =>
                {
                    if named_path || relationship.is_none() || any {
                        return Ok(true);
                    }
                    actions = true;
                }
                TokenKind::Punct(b'.') if actions => {
                    if let Some(TokenKind::Word(element)) = previous
                        && !matches!(before, Some(TokenKind::Punct(b'.')))
                        && Some(element) != relationship
                    {
                        return Ok(true);
                    }
                }
                _ => {}
            }
            before = previous;
            previous = Some(token.kind);
        }
    }

    /// The write family owning a read prefix after an earlier write. CREATE
    /// keeps its established direct MATCH prefix and uses the complete native
    /// read relation for multipart, OPTIONAL and CALL prefixes. Mutations
    /// share that relation while MERGE retains its mandatory MATCH profile. Everything else
    /// remains a terminal read. This is framing only: the selected compiler
    /// still admits the complete relation and write.
    /// Property keys, labels, aliases, map entries and STARTS/ENDS WITH are
    /// never clause boundaries.
    fn matched_write(mut lexer: Lexer<'_>) -> Result<Option<MatchedWrite>, GraphPatternTextError> {
        const TERMINAL: [&str; 3] = ["RETURN", "UNION", "FOREACH"];
        let mut depth = 0_usize;
        let mut row_stages = false;
        let mut multipart = false;
        let mut matched = false;
        let mut previous: Option<TokenKind<'_>> = None;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(None),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Word(word)
                    if depth == 0
                        && !matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
                        && !matches!(previous, Some(TokenKind::Word(prior))
                            if ["AS", "STARTS", "ENDS"].iter().any(|not| prior.eq_ignore_ascii_case(not))) =>
                {
                    if (word.eq_ignore_ascii_case("CREATE") || word.eq_ignore_ascii_case("INSERT"))
                        && (matches!(
                            script::next_script_token(&mut lexer.clone())?.kind,
                            TokenKind::Punct(b'(')
                        ) || Self::named_merge_pattern(lexer.clone())?)
                    {
                        return Ok(Some(if row_stages || multipart {
                            MatchedWrite::MultipartInsert
                        } else {
                            MatchedWrite::Insert
                        }));
                    }
                    if word.eq_ignore_ascii_case("MERGE")
                        && (matches!(
                            script::next_script_token(&mut lexer.clone())?.kind,
                            TokenKind::Punct(b'(')
                        ) || Self::named_merge_pattern(lexer.clone())?)
                    {
                        if multipart {
                            return Ok(None);
                        }
                        // Both MERGE families read the complete MATCH
                        // relation. Inspect past the whole node pattern,
                        // including nested property values, before choosing
                        // its native owner.
                        let mut pattern = lexer.clone();
                        let mut nested = 0_usize;
                        loop {
                            match script::next_script_token(&mut pattern)?.kind {
                                TokenKind::Punct(b'(' | b'[' | b'{') => nested += 1,
                                TokenKind::Punct(b')' | b']' | b'}') => {
                                    nested = nested.saturating_sub(1);
                                    if nested == 0 {
                                        break;
                                    }
                                }
                                TokenKind::End | TokenKind::Punct(b';') => return Ok(None),
                                _ => {}
                            }
                        }
                        let vertex = !matches!(
                            script::next_script_token(&mut pattern)?.kind,
                            TokenKind::Punct(b'-' | b'<')
                        );
                        return Ok(Some(if vertex {
                            MatchedWrite::VertexMerge
                        } else {
                            MatchedWrite::EdgeMerge
                        }));
                    }
                    if (word.eq_ignore_ascii_case("SET") || word.eq_ignore_ascii_case("REMOVE"))
                        && Self::mutation_clause_target(lexer.clone())?
                    {
                        return Ok(Some(if multipart {
                            MatchedWrite::MultipartMutation
                        } else {
                            MatchedWrite::Mutation
                        }));
                    }
                    if (word.eq_ignore_ascii_case("DELETE") || word.eq_ignore_ascii_case("DETACH"))
                        && Self::deletion_clause_target(
                            lexer.clone(),
                            previous,
                            word.eq_ignore_ascii_case("DETACH"),
                        )?
                    {
                        return Ok(Some(if multipart {
                            MatchedWrite::MultipartMutation
                        } else {
                            MatchedWrite::Mutation
                        }));
                    }
                    if TERMINAL.iter().any(|read| word.eq_ignore_ascii_case(read)) {
                        return Ok(None);
                    }
                    if word.eq_ignore_ascii_case("MATCH") {
                        multipart |= matched;
                        matched = true;
                    }
                    if word.eq_ignore_ascii_case("OPTIONAL") || word.eq_ignore_ascii_case("CALL") {
                        multipart = true;
                    }
                    if ["WITH", "UNWIND", "ORDER", "SKIP", "LIMIT"]
                        .iter()
                        .any(|stage| word.eq_ignore_ascii_case(stage))
                    {
                        row_stages = true;
                    }
                }
                _ => {}
            }
            previous = Some(token.kind);
        }
    }

    /// A named MERGE path has the same native pattern owner as an unnamed
    /// one. This is token framing only; the pattern compiler checks the name,
    /// complete pattern, scopes and endpoint types.
    pub(in crate::graph_text::scoped::mutation) fn named_merge_pattern(
        mut lexer: Lexer<'_>,
    ) -> Result<bool, GraphPatternTextError> {
        Ok(matches!(
            script::next_script_token(&mut lexer)?.kind,
            TokenKind::Word(_)
        ) && matches!(
            script::next_script_token(&mut lexer)?.kind,
            TokenKind::Punct(b'=')
        ) && matches!(
            script::next_script_token(&mut lexer)?.kind,
            TokenKind::Punct(b'(')
        ))
    }

    /// DELETE and DETACH are also legal identifiers. A deletion clause
    /// follows a complete read expression and starts a new target expression;
    /// `WITH delete`, `ORDER BY detach` and `n.p = delete` remain row uses.
    /// The mutation compiler owns the target's actual type and scope, including
    /// list/map/path targets and the runtime refusal of a non-element value.
    pub(in crate::graph_text::scoped::mutation) fn deletion_clause_target(
        mut lexer: Lexer<'_>,
        previous: Option<TokenKind<'_>>,
        detach: bool,
    ) -> Result<bool, GraphPatternTextError> {
        if matches!(
            previous,
            Some(TokenKind::Punct(
                b',' | b'.'
                    | b':'
                    | b'+'
                    | b'-'
                    | b'*'
                    | b'/'
                    | b'%'
                    | b'^'
                    | b'|'
                    | b'~'
                    | b'!'
                    | b'='
                    | b'<'
                    | b'>'
            ))
        ) || matches!(previous, Some(TokenKind::Word(word))
                if ["WITH", "BY", "WHERE", "AS", "AND", "OR", "XOR", "NOT", "WHEN", "THEN",
                    "ELSE", "IN", "IS", "UNWIND", "SKIP", "LIMIT", "STARTS", "ENDS", "CONTAINS"]
                    .iter().any(|prefix| word.eq_ignore_ascii_case(prefix)))
        {
            return Ok(false);
        }
        if detach
            && !matches!(script::next_script_token(&mut lexer)?.kind,
                TokenKind::Word(word) if word.eq_ignore_ascii_case("DELETE"))
        {
            return Ok(false);
        }
        Ok(match script::next_script_token(&mut lexer)?.kind {
            TokenKind::Word(word) => !RESERVED_NAMES
                .iter()
                .any(|reserved| word.eq_ignore_ascii_case(reserved)),
            TokenKind::Digits(_) | TokenKind::Parameter(_) | TokenKind::Quoted(_) => true,
            TokenKind::Punct(b'(' | b'[' | b'{' | b'+' | b'-') => true,
            _ => false,
        })
    }

    /// SET and REMOVE are legal row aliases. Only the native target prefix
    /// distinguishes a mutation clause from `WITH set`, `ORDER BY remove`,
    /// or a property of either name. The mutation compiler validates the
    /// target's scope, domain and complete action after this framing check.
    pub(in crate::graph_text::scoped::mutation) fn mutation_clause_target(
        mut lexer: Lexer<'_>,
    ) -> Result<bool, GraphPatternTextError> {
        match script::next_script_token(&mut lexer)?.kind {
            TokenKind::Word(_) => match script::next_script_token(&mut lexer)?.kind {
                TokenKind::Punct(b'.' | b':' | b'=') => Ok(true),
                TokenKind::Punct(b'+') => Ok(matches!(
                    script::next_script_token(&mut lexer)?.kind,
                    TokenKind::Punct(b'=')
                )),
                _ => Ok(false),
            },
            TokenKind::Punct(b'(') => Ok(matches!(
                script::next_script_token(&mut lexer)?.kind,
                TokenKind::Word(_)
            ) && matches!(
                script::next_script_token(&mut lexer)?.kind,
                TokenKind::Punct(b')')
            ) && matches!(
                script::next_script_token(&mut lexer)?.kind,
                TokenKind::Punct(b'.')
            )),
            _ => Ok(false),
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
        // leading MATCH and its read stages; otherwise it is a CREATE. A SET,
        // REMOVE or DELETE after a leading MATCH is a first mutation step
        // (`first_mutation`), which is_chain admits only before another write.
        let mut first_mutation =
            PreparedGraphInsertText::multipart_source_kind(body, declarations)?
                == Some(ReadWriteKind::Mutation);
        let first_merge = {
            let mut lexer = Lexer {
                text: body,
                at: 0,
                tokens: 0,
            };
            let (mut depth, mut first, mut unwound) = (0_usize, true, false);
            let mut matched = first_mutation;
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
                        if matched
                            && ["SET", "REMOVE", "DELETE", "DETACH"]
                                .iter()
                                .any(|clause| word.eq_ignore_ascii_case(clause))
                        {
                            first_mutation = true;
                            break None;
                        }
                        if word.eq_ignore_ascii_case("UNWIND") {
                            unwound |= first;
                        } else if first && word.eq_ignore_ascii_case("WITH") {
                            // A leading source-free WITH is a row prefix.
                            unwound = true;
                        } else if (first || (unwound && !matched))
                            && word.eq_ignore_ascii_case("MATCH")
                        {
                            // A MATCH after the leading UNWIND stages reads
                            // their rows (`UNWIND $rows AS row MATCH (y {k:
                            // row.k}) MERGE ...`), as a leading MATCH does.
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
        let mut next = if first_mutation {
            let (step, returning, after) =
                crate::mutation_text::PreparedGraphMutationRowsText::prepare_first_step(
                    body,
                    relation,
                    declarations,
                    &mut resolve,
                )?;
            steps.push(ChainTextStep::Mutation(Box::new((step, returning))));
            after
        } else if first_merge.is_some() && relationship {
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
        let mut read = None;
        // The property keys the terminal read names on carried elements.
        let mut read_keys = Vec::new();
        while let Some(from) = next {
            // The continuation is at the next write's or terminal read's clause.
            let mut lexer = Lexer {
                text: body,
                at: from.at,
                tokens: 0,
            };
            // The native write family owns a supported read prefix joined
            // to these rows. CREATE may consume a complete multipart read;
            // a terminal read keeps its existing compiler and result owner.
            let clause = lexer.clone().next()?.kind;
            let reading = matches!(clause, TokenKind::Word(word)
                if ["MATCH", "OPTIONAL", "CALL"].iter().any(|read| word.eq_ignore_ascii_case(read)));
            let matched = if reading {
                Self::matched_write(lexer.clone())?
            } else {
                None
            };
            if reading && matched.is_none() {
                let (columns, types) = from.owned_columns();
                let parser = Parser::new_with_parameter_types(body, declarations)?
                    .with_zones(resolve.time_zones())
                    .resume_after(from.boundary, &from.parameters, from.at)?;
                let unresolved = parser.write_read_continuation(body, from.row)?;
                let slots = unresolved.element_key_slots();
                let mut input = unresolved.resolve(&mut resolve)?;
                input.first.owned = Some(Box::new((columns, types)));
                read = Some(input);
                read_keys = slots
                    .iter()
                    .filter_map(|slot| match slot.bind() {
                        crate::GraphElementKey::Property(key) => Some(key),
                        crate::GraphElementKey::Unbound | crate::GraphElementKey::Missing => None,
                    })
                    .collect();
                break;
            }
            // A SET, REMOVE or DELETE: a RETURN or the end of the statement
            // follows it, or a WITH head or another write, which reads its
            // post-write rows.
            if matches!(
                matched,
                Some(MatchedWrite::Mutation | MatchedWrite::MultipartMutation)
            ) || matches!(lexer.clone().next()?.kind, TokenKind::Word(word)
                    if ["SET", "REMOVE", "DELETE", "DETACH"]
                        .iter()
                        .any(|kind| word.eq_ignore_ascii_case(kind)))
            {
                let (step, returning, after) =
                    crate::mutation_text::PreparedGraphMutationRowsText::prepare_chained(
                        body,
                        relation,
                        declarations,
                        &mut resolve,
                        from,
                        matches!(matched, Some(MatchedWrite::MultipartMutation)),
                    )?;
                steps.push(ChainTextStep::Mutation(Box::new((step, returning))));
                next = after;
                continue;
            }
            let matched_vertex_merge = matches!(matched, Some(MatchedWrite::VertexMerge));
            let matched_edge_merge = matches!(matched, Some(MatchedWrite::EdgeMerge));
            let merge = matched_vertex_merge
                || matched_edge_merge
                || matches!(lexer.next()?.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("MERGE"));
            if merge
                && !matched_vertex_merge
                && !matched_edge_merge
                && Self::named_merge_pattern(lexer.clone())?
            {
                lexer.next()?; // path name
                lexer.next()?; // equals
            }
            // A relationship MERGE names row vertices: `(a)-` or `(a)<-`.
            let relationship = matched_edge_merge
                || (merge
                    && !matched_vertex_merge
                    && matches!(lexer.next()?.kind, TokenKind::Punct(b'('))
                    && matches!(lexer.next()?.kind, TokenKind::Word(_))
                    && matches!(lexer.next()?.kind, TokenKind::Punct(b')'))
                    && matches!(lexer.next()?.kind, TokenKind::Punct(b'-' | b'<')));
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
                    matches!(matched, Some(MatchedWrite::MultipartInsert)),
                )?;
                steps.push(ChainTextStep::Insert(Box::new((step, returning))));
                next = after;
            }
        }
        // One write is a chain only as a vertex MERGE over a leading UNWIND
        // or MATCH (`UNWIND [1, 2] AS x MERGE (n {id: x}) RETURN n`, `MATCH
        // (p:Person) MERGE (c:City {name: p.bornIn})`), or as a relationship
        // MERGE whose ON/SET clauses read or write the MATCH row, or over the
        // rows of a leading UNWIND or WITH and a MATCH (fgdb-at06q).
        let unwound_merge = matches!(steps.as_slice(), [ChainTextStep::VertexMerge(merge)]
            if matches!(merge.input, crate::mutation_text::VertexMergeRowsTextInput::Relation(_)));
        let leading_rows = matches!(
            script::next_script_token(&mut Lexer {
                text: body,
                at: 0,
                tokens: 0,
            })?
            .kind,
            TokenKind::Word(word)
                if word.eq_ignore_ascii_case("UNWIND") || word.eq_ignore_ascii_case("WITH")
        );
        let row_actions = matches!(steps.as_slice(), [ChainTextStep::EdgeMerge(merge)]
            if merge.0.row_actions.is_some()
                || (leading_rows
                    && matches!(merge.0.input, crate::mutation_text::EdgeMergeTextInput::Row { .. })));
        if steps.len() < 2 && !unwound_merge && !row_actions && read.is_none() {
            return Err(expected(body.len(), "another write after the first").into());
        }
        let chain = Self {
            statement: statement.to_owned(),
            steps,
            read,
            read_keys,
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
                ChainTextStep::Mutation(step) => step.0.parameter_schema(),
            };
            for spec in specs {
                if !schema.iter().any(|known| known.name == spec.name) {
                    schema.push(spec.clone());
                }
            }
        }
        if let Some(read) = &self.read {
            for spec in read.parameter_schema() {
                if !schema.iter().any(|known| known.name == spec.name) {
                    schema.push(spec.clone());
                }
            }
        }
        schema
    }

    /// Replace exactly one leading list-parameter expansion with a typed
    /// external row relation. The native CSV adapter supplies one map cell per
    /// record; every later read stage and write keeps its compiled row schema.
    /// The parameter remains as an empty, admitted shape argument only: no
    /// source record is ever collected into that scalar container.
    pub(crate) fn bind_csv_row_source(
        &self,
        parameter: &str,
    ) -> Result<PreparedGraphWriteChain, GraphWriteChainTextError> {
        use crate::set_text::{ReadStageTemplate, ReadValueTemplate};
        let refused = || expected(0, "one leading UNWIND of the CSV row source");
        let schema = self.parameter_schema();
        if schema.len() != 1
            || schema[0].name != parameter
            || schema[0].parameter_type != GqlParameterType::List
            || schema[0].occurrences != 1
        {
            return Err(refused().into());
        }
        // parameter_schema() keeps the first step's entry for a shared name.
        // Count actual native tokens too: a later clause must not observe the
        // empty shape argument in place of the caller's complete row source.
        let mut lexer = Lexer {
            text: &self.statement,
            at: 0,
            tokens: 0,
        };
        let mut occurrences = 0;
        loop {
            match script::next_script_token(&mut lexer)?.kind {
                TokenKind::Parameter(name) => {
                    occurrences += 1;
                    if name != parameter || occurrences > 1 {
                        return Err(refused().into());
                    }
                }
                TokenKind::End => break,
                _ => {}
            }
        }
        if occurrences != 1 {
            return Err(refused().into());
        }
        let mut template = self.clone();
        let input = match template.steps.first_mut() {
            Some(ChainTextStep::VertexMerge(merge)) => match &mut merge.input {
                crate::mutation_text::VertexMergeRowsTextInput::Relation(input) => &mut **input,
                _ => return Err(refused().into()),
            },
            Some(ChainTextStep::Insert(step)) => match &mut step.0.input {
                InsertTextInput::Relation { input, .. } => input,
                _ => return Err(refused().into()),
            },
            Some(ChainTextStep::EdgeMerge(step)) => match &mut step.0.input {
                crate::mutation_text::EdgeMergeTextInput::Row { input, .. } => &mut **input,
                _ => return Err(refused().into()),
            },
            Some(ChainTextStep::EdgeMergeRows(step)) => match &mut step.0.input {
                crate::mutation_text::EdgeMergeTextInput::Row { input, .. } => &mut **input,
                _ => return Err(refused().into()),
            },
            _ => return Err(refused().into()),
        };
        let Some(ReadStageTemplate::Unwind {
            name,
            value: ReadValueTemplate::Parameter { index, .. },
            ..
        }) = input.leading.first()
        else {
            return Err(refused().into());
        };
        if input.owned.is_some()
            || !input.parameters.get(*index).is_some_and(|spec| {
                spec.name == parameter
                    && spec.parameter_type == GqlParameterType::List
                    && spec.occurrences == 1
            })
        {
            return Err(refused().into());
        }
        input.leading[0] = ReadStageTemplate::InputRows { name: name.clone() };
        let arguments = GqlParameters::new()
            .with_list(parameter, Vec::new())
            .map_err(|_| refused())?;
        template.bind_parameters(&arguments)
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
        let reads_previous = |owned: Option<&(Vec<String>, Vec<GraphSetColumnType>)>,
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
                    let owned = match &insertion.input {
                        InsertTextInput::Relation { input, .. } => input.owned.as_deref(),
                        InsertTextInput::Multipart { input, .. } => input.first.owned.as_deref(),
                        InsertTextInput::Match(_) | InsertTextInput::Unit { .. } => None,
                    };
                    reads_previous(owned, &previous, insertion.create_at)?;
                    let selection = match &insertion.input {
                        InsertTextInput::Match(selection) => Some(selection.bind_values(&values)?),
                        InsertTextInput::Relation { .. }
                        | InsertTextInput::Multipart { .. }
                        | InsertTextInput::Unit { .. } => None,
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
                        reads_previous(input.owned.as_deref(), &previous, merge.input.return_at())?;
                    }
                    let values = merge
                        .input
                        .checked_arguments(&own)
                        .map_err(crate::GraphEdgeMergeTextError::from)?;
                    let bound = merge.bind_values(&values)?;
                    let upsert = merge.bind_row_upsert(&bound, &values)?;
                    // A RETURN, or a WITH head another write reads.
                    steps.push(match returning {
                        Some(returning) => {
                            let query = returning.bind(bound, &values)?;
                            previous = Some(query.column_types().to_vec());
                            match upsert {
                                Some(upsert) => GraphWriteChainStep::EdgeUpsertQuery {
                                    upsert: Box::new(upsert),
                                    query: Box::new(query),
                                },
                                None => GraphWriteChainStep::EdgeMergeQuery(Box::new(query)),
                            }
                        }
                        None => {
                            previous = None;
                            match upsert {
                                Some(upsert) => GraphWriteChainStep::EdgeUpsert(Box::new(upsert)),
                                None => GraphWriteChainStep::EdgeMerge(Box::new(bound)),
                            }
                        }
                    });
                }
                ChainTextStep::EdgeMergeRows(step) => {
                    let (merge, keep) = &**step;
                    let own = own(merge.parameter_schema())?;
                    if let crate::mutation_text::EdgeMergeTextInput::Row { input, .. } =
                        &merge.input
                    {
                        reads_previous(input.owned.as_deref(), &previous, merge.input.return_at())?;
                    }
                    let values = merge
                        .input
                        .checked_arguments(&own)
                        .map_err(crate::GraphEdgeMergeTextError::from)?;
                    let bound = merge.bind_values(&values)?;
                    let upsert = merge.bind_row_upsert(&bound, &values)?;
                    // Its rows: the row's visible columns, then the edge. The
                    // row is the previous write's, or a MATCH-led first step's
                    // own read row, which no owned leaf carries.
                    let mut output = bound.input_column_types();
                    output.truncate(*keep);
                    output.push(GraphSetColumnType::Edge);
                    previous = Some(output);
                    steps.push(match upsert {
                        Some(upsert) => GraphWriteChainStep::EdgeUpsertRows {
                            upsert: Box::new(upsert),
                            keep: *keep,
                        },
                        None => GraphWriteChainStep::EdgeMergeRows {
                            merge: Box::new(bound),
                            keep: *keep,
                        },
                    });
                }
                ChainTextStep::Mutation(step) => {
                    let (mutation, returning) = &**step;
                    let own = own(mutation.parameter_schema())?;
                    if mutation.owned_column_types() != previous.as_deref() {
                        return Err(expected(mutation.at, "the previous write's WITH row").into());
                    }
                    steps.push(match returning {
                        Some(returning) => {
                            let query = mutation.bind_query(returning, &own)?;
                            previous = Some(query.column_types().to_vec());
                            GraphWriteChainStep::MutationQuery(Box::new(query))
                        }
                        None => {
                            previous = None;
                            GraphWriteChainStep::Mutation(Box::new(mutation.bind_parameters(&own)?))
                        }
                    });
                }
            }
        }
        let read = self
            .read
            .as_ref()
            .map(|read| {
                reads_previous(read.first.owned.as_deref(), &previous, self.statement.len())?;
                read.bind_parameters(&own(read.parameter_schema())?)
                    .map_err(GraphWriteChainTextError::from)
            })
            .transpose()?;
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
        let chain = PreparedGraphWriteChain {
            steps,
            initial_rows: None,
            read,
            read_keys: self.read_keys.clone(),
        };
        if let Some(read) = chain.read_continuation()
            && !chain.accepts_read_continuation(read)
        {
            return Err(expected(
                self.statement.len(),
                "one complete write-row input to the read",
            )
            .into());
        }
        Ok(chain)
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
            // A relationship MERGE over a leading MATCH whose ON/SET clauses
            // write a carried vertex, read one, or follow an anonymous
            // relationship: the unique upsert writes only its named
            // relationship (TCK Merge6 [1], Merge7 [1], fgdb-at06q).
            "MATCH (a:A), (b:B) MERGE (a)-[:KNOWS]->(b) ON CREATE SET b.created = 1",
            "MATCH (a:A), (b:B) MERGE (a)<-[:KNOWS]-(b) ON MATCH SET b.created = 1",
            "MATCH (a), (b) MERGE (a)-[r:R]->(b) ON CREATE SET b.p = 1",
            "MATCH (a), (b) MERGE (a)-[r:R]->(b) ON MATCH SET r.p = a.p",
            "MATCH (a), (b) WITH a, b MERGE (a)-[r:R {k: 1}]->(b) SET r.p = 1, b.q = 2",
            "MATCH (a), (b) MERGE (a)-[r:R]->(b) ON CREATE SET r += {p: a.p} RETURN r",
            // A vertex MERGE after source-free WITH stages (TCK Merge1 [8],
            // Merge9 [4]).
            "WITH 42 AS var MERGE (c:N {var: var})",
            "UNWIND [42] AS props WITH props WHERE props > 32 WITH DISTINCT props AS p \
             MERGE (a:A {num: p}) RETURN a.num AS prop",
            // Chained writes over a parameter UNWIND (fgdb-0s2pq).
            "UNWIND $pairs AS row MERGE (a:N {id: row.a}) MERGE (b:N {id: row.b}) \
             MERGE (a)-[:S]->(b)",
            "UNWIND $rows AS row MERGE (n:N {id: row.id}) CREATE (n)-[:R]->(:M)",
            // A RETURN after one vertex MERGE over a parameter UNWIND, with
            // or without its ON/SET clauses: the batch writer has no RETURN.
            "UNWIND $rows AS row MERGE (n:N {id: row.id}) RETURN count(*) AS c",
            "UNWIND $rows AS row MERGE (n:N {id: row.id}) ON CREATE SET n.name = row.name \
             RETURN n.id AS id, n.name AS name",
            "UNWIND $rows AS row MERGE (n:N {id: row.id}) SET n.name = row.name RETURN n",
            // A vertex MERGE over the bindings of a MATCH after the UNWIND
            // (TCK Unwind1 [6]), over a parameter or a literal list.
            "UNWIND $events AS event MATCH (y:Year {year: event.year}) \
             MERGE (e:Event {id: event.id}) MERGE (y)<-[:IN]-(e) RETURN e.id AS x ORDER BY x",
            "UNWIND $events AS event MATCH (y:Year {year: event.year}) MERGE (e:Event {id: event.id})",
            "UNWIND [1, 2] AS v MATCH (y:Year {year: v}) MERGE (e:Event {id: v})",
            // A relationship MERGE between the vertices of a MATCH after a
            // literal UNWIND, and one over `UNWIND $pairs` that returns rows:
            // the batch writer has no RETURN, and refused it at RETURN
            // (fgdb-at06q).
            "UNWIND [1, 2] AS i MATCH (n {id: i}), (y {id: 9}) MERGE (n)-[:R]->(y)",
            "UNWIND [1, 2] AS x MATCH (a:A), (b:B) MERGE (a)-[r:R {k: x}]->(b) RETURN r.k",
            "UNWIND $pairs AS row MATCH (a {id: row.a}), (b {id: row.b}) MERGE (a)-[:R]->(b) \
             RETURN count(*) AS c",
            // A SET, REMOVE or DELETE after a CREATE writes its rows.
            "CREATE (n) SET n.p = 1",
            "CREATE (n:P {p: 1}) SET n.p = n.p + 1 RETURN n.p",
            "UNWIND [1, 2] AS i CREATE (n) SET n.i = i",
            "MATCH (a) CREATE (b) SET b.k = a.k RETURN b.k",
            "CREATE (n) WITH n SET n.p = 1 RETURN n",
            "CREATE (n {p: 1}) REMOVE n.p",
            "MATCH () CREATE (n) DELETE n",
            "CREATE (n) DETACH DELETE n",
            // A write after a SET, REMOVE or DELETE reads its post-write rows
            // (TCK Match5 [26], Merge1 [14], Merge5 [21]).
            "MATCH (a)-[r:OLD]->(b) DELETE r CREATE (a)-[:NEW]->(b)",
            "MATCH (a)-[r:OLD]->(b) WITH a, b, r DELETE r CREATE (a)-[:NEW]->(b)",
            "MATCH (n:A) SET n.x = 1 WITH n CREATE (n)-[:T]->(:C)",
            "MATCH (n) REMOVE n.x MERGE (m:M)",
            "MATCH (a:A) DELETE a MERGE (a2:A) RETURN a2.num",
            "MATCH (a)-[t:T]->(b) DELETE t MERGE (a)-[t2:T {name: 'rel3'}]->(b) RETURN t2.name",
            "CREATE (n) SET n.p = 1 CREATE (m)",
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
            "CREATE (a); MATCH (n) WITH n CREATE (b)",
            "MATCH (a) SET a.p = 1; MATCH (b) CREATE (c)",
            "RETURN 1",
            // A lone relationship MERGE with already bound endpoints and a
            // lone first vertex MERGE keep their established families.
            "MATCH (a), (b) MERGE (a)-[:X]->(b) RETURN a",
            "OPTIONAL MATCH (a) MERGE (b)",
            // ON/SET clauses that write and read only the named relationship
            // stay with the unique upsert; reads in a RETURN do not count.
            "MATCH (a), (b) MERGE (a)-[r:R]->(b) ON CREATE SET r.p = 1 ON MATCH SET r.p = r.p + 1",
            "MATCH (a), (b) MERGE (a)-[r:R]->(b) SET r += {p: 1, q: 'a.b'} RETURN a.p, r.p",
            "MATCH (a), (b) MERGE (a)-[r:R]->(b) ON CREATE SET r.p = $x.p",
            // An UNWIND of a parameter before one MERGE is the batch writer's,
            // its ON/SET clauses and a relationship MERGE after MATCH included,
            // unless a RETURN follows.
            "UNWIND $rows AS row MERGE (n:Entity {id: row.id})",
            "UNWIND $rows AS row MERGE (n:Entity {id: row.id}) SET n.name = row.name",
            "UNWIND $pairs AS row MATCH (a {id: row.a}), (b {id: row.b}) MERGE (a)-[:R]->(b)",
            "UNWIND $rows AS row MERGE (n:Entity {p: 'RETURN'})",
            "MERGE (a:A) ON CREATE SET a.p = 1",
            "MERGE (a:A) ON CREATE SET a.p = 1 ON MATCH SET a.q = 2 RETURN a",
            "MERGE (n:L) SET n.p = 1",
            "MERGE (n:L) RETURN n",
            // A SET without a CREATE before it is the mutation's own.
            "MATCH (n) SET n.p = 1",
            "MATCH (n) SET n.p = 1 RETURN n",
            "MATCH (n) SET n.p = 1 WITH n RETURN n.p",
            "MATCH (n) WITH n SET n.p = 1 RETURN n",
            "MATCH (n {p: 'CREATE (x)'}) DELETE n",
        ] {
            assert!(
                !PreparedGraphWriteChainText::is_chain(text).unwrap(),
                "{text}"
            );
        }
    }

    #[test]
    fn whole_pattern_relationship_merges_use_native_chain_rows() {
        for text in [
            "MERGE (a)-[:R]->(b)",
            "MERGE (a)-[:R]->(b) MERGE (c)",
            "MERGE ()-[:R]->()",
            "MERGE (a:Visible {p:1})<-[r:R {p:2}]-(b:Visible {p:3}) RETURN a,b,r",
            "MERGE route=(a:Visible {p:1})-[r:R]-(a) RETURN route",
            "MATCH (a) MERGE (a)-[:R]->(b)",
            "MATCH (b) MERGE (a)<-[:R]-(b)",
            "MATCH (a) MERGE (a)-[:R]->()",
            "MATCH (a {b:1}) MERGE (a)-[:R]->(b)",
            "MATCH (a) WHERE coalesce(b,1)=1 MERGE (a)-[:R]->(b)",
            "MATCH (a) MERGE route=(a)-[r:R {p:[1,(2+3)]}]->(b:Visible {p:2})",
            "UNWIND [1] AS x MERGE (a {p:x})-[:R]->(b)",
            "UNWIND $rows AS row MERGE (a {p:row.a})-[:R]->(b {p:row.b})",
            "UNWIND $rows AS row MATCH (a {p:row.a}) MERGE (a)-[:R]->(b {p:row.b})",
            "UNWIND $rows AS row MATCH route=(a)-[:R]->(b) MERGE (b)-[:R]->(c)",
            "MATCH (a), (b) WITH a,b MERGE (a)-[:R]->(b)",
            "MATCH (a), (b) WITH a MERGE (a)-[:R]->(b)",
            "MATCH (a) WITH a AS b MERGE (a)-[:R]->(b)",
            "WITH 1 AS value MERGE (a {p:value})-[:R]->(b)",
        ] {
            assert!(
                PreparedGraphWriteChainText::is_chain(text).unwrap(),
                "{text}"
            );
        }
        for text in [
            "MATCH (a), (b) MERGE (a)-[:R]->(b)",
            "MATCH (a) MATCH (b) MERGE (a)-[r:R]->(b) ON CREATE SET r.p=1",
            "MATCH route=(a)-[edge:R]->(b) MERGE (a)-[:R]->(b)",
            "MATCH (a) MERGE (a)-[:R]->(a)",
            "MATCH (a),(b) WHERE a.p STARTS WITH 'WITH' MERGE (a)-[:R]->(b)",
            "MATCH (a {p:'MERGE (new)-[:R]->()'}),(b) MERGE (a)-[:R]->(b)",
            "UNWIND $pairs AS row MATCH (a {p:row.a}),(b {p:row.b}) MERGE (a)-[:R]->(b)",
            "UNWIND $left AS a UNWIND $right AS b MERGE (a)-[:R]->(b)",
            "MATCH (a),(b) MERGE (a)-[:R]->(b); MERGE (new)-[:R]->()",
            "MATCH (a) RETURN a; MERGE (new)-[:R]->()",
        ] {
            assert!(
                !PreparedGraphWriteChainText::is_chain(text).unwrap(),
                "{text}"
            );
        }
    }
}
