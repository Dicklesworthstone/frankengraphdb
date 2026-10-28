//! Script framing and dispatch use the SAME native lexer and MATCH-prefix parser.
//! Existing statement compilers own all statement semantics and lowering.

mod unwind;

use super::*;
use crate::{
    GraphMutationProgramTemplateError, GraphWriteProgramTemplateError, GraphWriteScriptError,
    GraphWriteScriptErrorKind, GraphWriteTemplateStatement, MAX_GRAPH_MUTATION_STATEMENTS,
    MAX_GRAPH_WRITE_SCRIPT_BYTES, PreparedGraphDeleteText, PreparedGraphEdgeMergeText,
    PreparedGraphEdgeUpsertText, PreparedGraphInsertText, PreparedGraphVertexMergeText,
    PreparedGraphVertexUpsertText, PreparedGraphWriteProgramTemplate, PreparedGraphWriteScript,
};
use core::ops::Range;
use std::collections::BTreeSet;

struct Statement<'a> {
    span: Range<usize>,
    parameters: BTreeSet<&'a str>,
    last_on: Option<usize>,
    /// The last top-level SET: after MERGE it is openCypher MERGE ... SET.
    last_set: Option<usize>,
}
#[derive(Clone, Copy)]
enum Kind {
    Mutation,
    Insert,
    VertexMerge,
    VertexUpsert,
    EdgeMerge,
    EdgeUpsert,
    Delete,
}

fn refusal(
    statement: usize,
    offset: usize,
    kind: GraphWriteScriptErrorKind,
) -> GraphWriteScriptError {
    GraphWriteScriptError {
        statement: Some(statement),
        offset,
        kind,
    }
}

// A separator is recognized only BETWEEN native tokens. In particular a quoted
// scalar or a comment, including doubled quotes and semicolons, is consumed by
// the shared lexer. The single-statement lexer still rejects semicolons.
pub(in crate::graph_text) fn next_script_token<'a>(
    lexer: &mut Lexer<'a>,
) -> Result<Token<'a>, GraphPatternTextError> {
    lexer.skip_trivia()?;
    if lexer.text.as_bytes().get(lexer.at) == Some(&b';') {
        let at = lexer.at;
        lexer.at += 1;
        return Ok(Token {
            at,
            kind: TokenKind::Punct(b';'),
        });
    }
    lexer.next()
}

fn scan(script: &str) -> Result<Vec<Statement<'_>>, GraphWriteScriptError> {
    if script.len() > MAX_GRAPH_WRITE_SCRIPT_BYTES {
        return Err(GraphWriteScriptError {
            statement: None,
            offset: MAX_GRAPH_WRITE_SCRIPT_BYTES,
            kind: GraphWriteScriptErrorKind::DefinitionTooLarge {
                limit: MAX_GRAPH_WRITE_SCRIPT_BYTES,
                observed: script.len(),
            },
        });
    }
    let mut lexer = Lexer {
        text: script,
        at: 0,
        tokens: 0,
    };
    let mut result = Vec::new();
    let mut start = 0;
    let mut nonempty = false;
    let mut nesting = Vec::new();
    let mut parameters = BTreeSet::new();
    let mut last_on = None;
    let mut last_set = None;
    let mut previous_dot = false;
    loop {
        let statement = result.len();
        let token = next_script_token(&mut lexer)
            .map_err(|source| GraphWriteScriptError::syntax(Some(statement), 0, source))?;
        let end = matches!(token.kind, TokenKind::End);
        let separator = matches!(token.kind, TokenKind::Punct(b';'));
        if end || separator {
            if !nesting.is_empty() {
                return Err(refusal(
                    statement,
                    token.at,
                    GraphWriteScriptErrorKind::Syntax(GraphPatternTextErrorKind::Expected(
                        "closing delimiter before statement end",
                    )),
                ));
            }
            if !nonempty {
                if end && !result.is_empty() {
                    break;
                }
                return Err(refusal(
                    statement,
                    token.at,
                    GraphWriteScriptErrorKind::EmptyStatement,
                ));
            }
            let bytes = token.at - start;
            if bytes > MAX_GRAPH_TEXT_BYTES {
                return Err(refusal(
                    statement,
                    start + MAX_GRAPH_TEXT_BYTES,
                    GraphWriteScriptErrorKind::DefinitionTooLarge {
                        limit: MAX_GRAPH_TEXT_BYTES,
                        observed: bytes,
                    },
                ));
            }
            if result.len() == MAX_GRAPH_MUTATION_STATEMENTS {
                return Err(refusal(
                    statement,
                    start,
                    GraphWriteScriptErrorKind::TooManyStatements {
                        limit: MAX_GRAPH_MUTATION_STATEMENTS,
                        observed: statement + 1,
                    },
                ));
            }
            result.push(Statement {
                span: start..token.at,
                parameters: core::mem::take(&mut parameters),
                last_on: last_on.take(),
                last_set: last_set.take(),
            });
            if end {
                break;
            }
            start = lexer.at;
            nonempty = false;
            previous_dot = false;
            lexer.tokens = 0;
            continue;
        }
        nonempty = true;
        if lexer.at - start > MAX_GRAPH_TEXT_BYTES {
            return Err(refusal(
                statement,
                start + MAX_GRAPH_TEXT_BYTES,
                GraphWriteScriptErrorKind::DefinitionTooLarge {
                    limit: MAX_GRAPH_TEXT_BYTES,
                    observed: lexer.at - start,
                },
            ));
        }
        match token.kind {
            TokenKind::Punct(open @ (b'(' | b'[' | b'{')) => nesting.push(open),
            TokenKind::Punct(close @ (b')' | b']' | b'}')) => {
                let expected = match close {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                if nesting.pop() != Some(expected) {
                    return Err(refusal(
                        statement,
                        token.at,
                        GraphWriteScriptErrorKind::Syntax(GraphPatternTextErrorKind::Expected(
                            "matching opening delimiter",
                        )),
                    ));
                }
            }
            TokenKind::Parameter(name) => {
                parameters.insert(name);
            }
            TokenKind::Word(word)
                if nesting.is_empty() && !previous_dot && word.eq_ignore_ascii_case("ON") =>
            {
                last_on = Some(token.at);
            }
            TokenKind::Word(word)
                if nesting.is_empty() && !previous_dot && word.eq_ignore_ascii_case("SET") =>
            {
                last_set = Some(token.at);
            }
            _ => {}
        }
        previous_dot = matches!(token.kind, TokenKind::Punct(b'.'));
    }
    Ok(result)
}

fn classify(
    text: &str,
    statement: &Statement<'_>,
    declarations: &[(&str, GqlParameterType)],
) -> Result<Kind, GraphPatternTextError> {
    if let Some(kind) = PreparedGraphInsertText::multipart_source_kind(text, declarations)? {
        return Ok(match kind {
            ReadWriteKind::Insert => Kind::Insert,
            ReadWriteKind::Mutation => Kind::Mutation,
        });
    }
    let mut parser = Parser::new_with_parameter_types(text, declarations)?;
    let unwind_led = parser.is_word("UNWIND");
    if unwind_led || parser.is_word("WITH") {
        let (_, imports) = parser.insertion_unwind_prefix().map_err(|source| {
            let kind = match source.kind {
                crate::GraphSetTextErrorKind::Pattern(kind) => kind,
                _ => GraphPatternTextErrorKind::Expected("a bounded native WITH/UNWIND source"),
            };
            error(source.offset, kind)
        })?;
        // Use the insertion compiler's exact imported scope and correlation
        // rules during whole-script preflight, before any catalog callbacks.
        // A row-fed MERGE stays with its own owners.
        let matched = parser.insertion_match_prefix(&imports)?;
        return if parser.is_word("CREATE") || parser.is_word("INSERT") {
            Ok(Kind::Insert)
        } else if unwind_led
            && matched
            && ["SET", "REMOVE", "DETACH", "DELETE"]
                .iter()
                .any(|word| parser.is_word(word))
        {
            // UNWIND ... MATCH ... SET/REMOVE/DELETE: the mutation compiler
            // reads the rows of the UNWIND stages and the MATCH (fgdb-0s2pq).
            Ok(Kind::Mutation)
        } else {
            Err(error(
                parser.current.at,
                GraphPatternTextErrorKind::Expected("CREATE or INSERT after WITH/UNWIND/MATCH"),
            ))
        };
    }
    // A leading OPTIONAL MATCH is a write source only for SET, REMOVE and
    // either DELETE: the mutation compiler left-joins it with the unit row.
    if parser.is_word("OPTIONAL") {
        parser.parse_mutation_match_prefix()?;
        return if ["SET", "REMOVE", "DETACH", "DELETE"]
            .iter()
            .any(|word| parser.is_word(word))
        {
            Ok(Kind::Mutation)
        } else {
            Err(error(
                parser.current.at,
                GraphPatternTextErrorKind::Expected(
                    "SET, REMOVE, DELETE or DETACH DELETE after a leading OPTIONAL MATCH",
                ),
            ))
        };
    }
    let matched = parser.is_word("MATCH");
    // A WHERE the graph predicate cannot hold (a label test under OR or
    // NOT) reads as `MATCH ... WITH * WHERE ...`.
    let deferred = matched && parser.write_match_prefix()?;
    // MATCH ... WITH ... CREATE/SET/REMOVE/DELETE: the write reads the WITH
    // row (fgdb-sl4ym). An UNWIND right after the MATCH reads it through
    // the implicit `WITH *` (`MATCH (p) UNWIND p.tags AS t CREATE ...`).
    if matched && (deferred || parser.is_word("WITH") || parser.is_word("UNWIND")) {
        let prefix = if deferred {
            parser.implicit_write_row_prefix()
        } else {
            parser.write_row_prefix()
        };
        prefix.map_err(|source| {
            let kind = match source.kind {
                crate::GraphSetTextErrorKind::Pattern(kind) => kind,
                _ => GraphPatternTextErrorKind::Expected("a bounded WITH before a write clause"),
            };
            error(source.offset, kind)
        })?;
        // A relationship MERGE over the row (fgdb-cpny9) has no branch clause
        // here: ON MATCH/ON CREATE or SET after it keep their refusal.
        let merge_at = statement.span.start + parser.current.at;
        let plain_merge = !statement.last_on.is_some_and(|at| at > merge_at)
            && !statement.last_set.is_some_and(|at| at > merge_at)
            && parser.at_relationship_merge()?;
        return if parser.is_word("CREATE") || parser.is_word("INSERT") {
            Ok(Kind::Insert)
        } else if ["SET", "REMOVE", "DETACH", "DELETE"]
            .iter()
            .any(|word| parser.is_word(word))
        {
            Ok(Kind::Mutation)
        } else if plain_merge {
            Ok(Kind::EdgeMerge)
        } else {
            Err(error(
                parser.current.at,
                GraphPatternTextErrorKind::Expected(
                    "CREATE, SET, REMOVE, DELETE or a relationship MERGE after MATCH ... WITH",
                ),
            ))
        };
    }
    if parser.is_word("CREATE") || parser.is_word("INSERT") {
        return Ok(Kind::Insert);
    }
    if matched && parser.is_word("DELETE") {
        // DELETE p of a matched path is the mutation compiler's, which
        // expands each path into its elements (TCK Delete3).
        let mut lexer = parser.lexer.clone();
        loop {
            match lexer.next()?.kind {
                TokenKind::Word(word) if parser.syntax.names_path(word) => {
                    return Ok(Kind::Mutation);
                }
                TokenKind::End => return Ok(Kind::Delete),
                _ => {}
            }
        }
    }
    if matched && (parser.is_word("SET") || parser.is_word("REMOVE") || parser.is_word("DETACH")) {
        return Ok(Kind::Mutation);
    }
    if parser.is_word("MERGE") {
        // ON MATCH/ON CREATE, or a plain SET, after MERGE selects the upsert.
        let merge_at = statement.span.start + parser.current.at;
        let branch = statement.last_on.is_some_and(|at| at > merge_at)
            || statement.last_set.is_some_and(|at| at > merge_at);
        return Ok(match (matched, branch) {
            (false, false) => Kind::VertexMerge,
            (false, true) => Kind::VertexUpsert,
            (true, false) => Kind::EdgeMerge,
            (true, true) => Kind::EdgeUpsert,
        });
    }
    Err(error(
        parser.current.at,
        GraphPatternTextErrorKind::Expected("a supported graph write statement"),
    ))
}

/// The one cached catalog every statement of a script resolves through. It
/// forwards each defaulted resolver capability, so a statement compiler sees
/// the host's tzdb and reverse catalog, not a closure that drops them.
struct ScriptCatalog<'r, R: ?Sized> {
    resolve: &'r mut R,
    cache: BTreeMap<(GraphSymbolKind, String), GraphSymbol>,
}
impl<R: GraphSymbolResolver + ?Sized> GraphSymbolResolver for ScriptCatalog<'_, R> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        let key = (kind, name.to_owned());
        if let Some(value) = self.cache.get(&key) {
            return Some(*value);
        }
        let value = self.resolve.resolve_symbol(kind, name)?;
        self.cache.insert(key, value);
        Some(value)
    }
    fn time_zones(&self) -> Option<crate::GqlTimeZones> {
        self.resolve.time_zones()
    }
    fn reverse_catalog(&self) -> Option<crate::ReverseSymbolCatalog> {
        self.resolve.reverse_catalog()
    }
    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        self.resolve.reverse_label(id)
    }
    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.resolve.reverse_relation(id)
    }
}

impl PreparedGraphWriteScript {
    pub fn prepare(
        script: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphWriteScriptError> {
        Self::prepare_with_parameter_types(script, relation, &[], resolve)
    }

    /// Admit framing, lexical limits, declarations and statement dispatch for
    /// the WHOLE script before catalog resolution. Each native statement still
    /// validates its full grammar before its own catalog callbacks. Declarations
    /// are script-wide but each compiler receives only its local subset.
    /// Comments and transaction-control syntax are not introduced by this API.
    pub fn prepare_with_parameter_types(
        script: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphWriteScriptError> {
        Self::prepare_with_parameter_types_and_resolver(
            script,
            relation,
            declarations,
            &mut resolve,
        )
    }

    /// As [`Self::prepare_with_parameter_types`], with a host resolver whose
    /// pinned tzdb every statement compiler receives (fgdb-txbg9): a write's
    /// temporal constructors resolve named zones exactly as a read's do, and
    /// without a tzdb a named zone still refuses.
    pub fn prepare_with_parameter_types_and_resolver<R>(
        script: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
    ) -> Result<Self, GraphWriteScriptError>
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let scanned = scan(script)?;
        // Reuse the native declaration validator, without inventing a parser or
        // accepting duplicate/invalid names merely because different steps use them.
        Parser::new_with_parameter_types("", declarations)
            .map_err(|source| GraphWriteScriptError::syntax(None, 0, source))?;
        for &(name, _) in declarations {
            if !scanned
                .iter()
                .any(|statement| statement.parameters.contains(name))
            {
                return Err(GraphWriteScriptError {
                    statement: None,
                    offset: 0,
                    kind: GraphWriteScriptErrorKind::Syntax(
                        GraphPatternTextErrorKind::UnusedParameterDeclaration,
                    ),
                });
            }
        }
        let spans = scanned
            .iter()
            .map(|statement| statement.span.clone())
            .collect::<Vec<_>>();
        let mut kinds = Vec::with_capacity(scanned.len());
        let mut locals = Vec::with_capacity(scanned.len());
        for (index, statement) in scanned.iter().enumerate() {
            let local = declarations
                .iter()
                .copied()
                .filter(|(name, _)| statement.parameters.contains(*name))
                .collect::<Vec<_>>();
            let kind =
                classify(&script[statement.span.clone()], statement, &local).map_err(|source| {
                    GraphWriteScriptError::syntax(Some(index), statement.span.start, source)
                })?;
            kinds.push(kind);
            locals.push(local);
        }
        let mut catalog = ScriptCatalog {
            resolve,
            cache: BTreeMap::new(),
        };
        let mut statements = Vec::with_capacity(scanned.len());
        for (statement, definition) in scanned.iter().enumerate() {
            let text = &script[definition.span.clone()];
            let local = &locals[statement];
            let prepared: Result<GraphWriteTemplateStatement, GraphWriteProgramTemplateError> =
                match kinds[statement] {
                    Kind::Mutation => PreparedGraphMutationText::prepare_with_parameter_types(
                        text,
                        relation,
                        local,
                        crate::graph_text::LentResolver(&mut catalog),
                    )
                    .map(Into::into)
                    .map_err(|source| {
                        GraphMutationProgramTemplateError::Bind { statement, source }.into()
                    }),
                    Kind::Insert => PreparedGraphInsertText::prepare_with_parameter_types(
                        text,
                        relation,
                        local,
                        crate::graph_text::LentResolver(&mut catalog),
                    )
                    .map(Into::into)
                    .map_err(|source| {
                        GraphWriteProgramTemplateError::InsertBind { statement, source }
                    }),
                    Kind::VertexMerge => {
                        PreparedGraphVertexMergeText::prepare_with_parameter_types_and_resolver(
                            text,
                            relation,
                            local,
                            &mut catalog,
                        )
                        .map(Into::into)
                        .map_err(|source| {
                            GraphWriteProgramTemplateError::VertexMergeBind { statement, source }
                        })
                    }
                    Kind::VertexUpsert => {
                        PreparedGraphVertexUpsertText::prepare_with_parameter_types(
                            text,
                            relation,
                            local,
                            crate::graph_text::LentResolver(&mut catalog),
                        )
                        .map(Into::into)
                        .map_err(|source| {
                            GraphWriteProgramTemplateError::VertexUpsertBind { statement, source }
                        })
                    }
                    Kind::EdgeMerge => {
                        PreparedGraphEdgeMergeText::prepare_with_parameter_types_and_resolver(
                            text,
                            relation,
                            local,
                            &mut catalog,
                        )
                        .map(Into::into)
                        .map_err(|source| {
                            GraphWriteProgramTemplateError::EdgeMergeBind { statement, source }
                        })
                    }
                    Kind::EdgeUpsert => {
                        PreparedGraphEdgeUpsertText::prepare_with_parameter_types_and_resolver(
                            text,
                            relation,
                            local,
                            &mut catalog,
                        )
                        .map(Into::into)
                        .map_err(|source| {
                            GraphWriteProgramTemplateError::EdgeUpsertBind { statement, source }
                        })
                    }
                    Kind::Delete => {
                        PreparedGraphDeleteText::prepare_with_parameter_types_and_resolver(
                            text,
                            relation,
                            local,
                            &mut catalog,
                        )
                        .map(Into::into)
                        .map_err(|source| {
                            GraphWriteProgramTemplateError::DeleteBind { statement, source }
                        })
                    }
                };
            statements
                .push(prepared.map_err(|source| GraphWriteScriptError::program(&spans, source))?);
        }
        let program = PreparedGraphWriteProgramTemplate::prepare(statements)
            .map_err(|source| GraphWriteScriptError::program(&spans, source))?;
        Ok(Self {
            script: script.to_owned(),
            program,
            spans: spans.into_boxed_slice(),
        })
    }
}
