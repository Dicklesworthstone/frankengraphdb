//! Script framing and dispatch use the SAME native lexer and MATCH-prefix parser.
//! Existing statement compilers own all statement semantics and lowering.

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
// scalar, including doubled quotes and semicolons, is consumed by Lexer::next.
// The single-statement lexer remains unchanged and still rejects semicolons.
fn next_script_token<'a>(lexer: &mut Lexer<'a>) -> Result<Token<'a>, GraphPatternTextError> {
    while let Some(ch) = lexer.text[lexer.at..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        lexer.at += ch.len_utf8();
    }
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
    let mut parser = Parser::new_with_parameter_types(text, declarations)?;
    let matched = parser.is_word("MATCH");
    if matched {
        parser.parse_match_prefix()?;
    }
    if parser.is_word("CREATE") || parser.is_word("INSERT") {
        return Ok(Kind::Insert);
    }
    if matched && parser.is_word("DELETE") {
        return Ok(Kind::Delete);
    }
    if matched && (parser.is_word("SET") || parser.is_word("REMOVE") || parser.is_word("DETACH")) {
        return Ok(Kind::Mutation);
    }
    if parser.is_word("MERGE") {
        let branch = statement
            .last_on
            .is_some_and(|at| at > statement.span.start + parser.current.at);
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
        let mut cache = BTreeMap::new();
        let mut catalog = |kind, name: &str| {
            let key = (kind, name.to_owned());
            if let Some(value) = cache.get(&key) {
                return Some(*value);
            }
            let value = resolve(kind, name)?;
            cache.insert(key, value);
            Some(value)
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
                        &mut catalog,
                    )
                    .map(Into::into)
                    .map_err(|source| {
                        GraphMutationProgramTemplateError::Bind { statement, source }.into()
                    }),
                    Kind::Insert => PreparedGraphInsertText::prepare_with_parameter_types(
                        text,
                        relation,
                        local,
                        &mut catalog,
                    )
                    .map(Into::into)
                    .map_err(|source| {
                        GraphWriteProgramTemplateError::InsertBind { statement, source }
                    }),
                    Kind::VertexMerge => {
                        PreparedGraphVertexMergeText::prepare_with_parameter_types(
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
                            &mut catalog,
                        )
                        .map(Into::into)
                        .map_err(|source| {
                            GraphWriteProgramTemplateError::VertexUpsertBind { statement, source }
                        })
                    }
                    Kind::EdgeMerge => PreparedGraphEdgeMergeText::prepare_with_parameter_types(
                        text,
                        relation,
                        local,
                        &mut catalog,
                    )
                    .map(Into::into)
                    .map_err(|source| {
                        GraphWriteProgramTemplateError::EdgeMergeBind { statement, source }
                    }),
                    Kind::EdgeUpsert => PreparedGraphEdgeUpsertText::prepare_with_parameter_types(
                        text,
                        relation,
                        local,
                        &mut catalog,
                    )
                    .map(Into::into)
                    .map_err(|source| {
                        GraphWriteProgramTemplateError::EdgeUpsertBind { statement, source }
                    }),
                    Kind::Delete => PreparedGraphDeleteText::prepare_with_parameter_types(
                        text,
                        relation,
                        local,
                        &mut catalog,
                    )
                    .map(Into::into)
                    .map_err(|source| {
                        GraphWriteProgramTemplateError::DeleteBind { statement, source }
                    }),
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
