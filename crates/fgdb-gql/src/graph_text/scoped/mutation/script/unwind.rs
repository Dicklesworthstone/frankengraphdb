//! Structural UNWIND lowering, using the ordinary write-script lexer.
//!
//! Selected values become native scalar parameter holes, never source text.
//! Long accesses preserve byte positions by padding. Short bare aliases use
//! a monotone source map for native diagnostics and executed batch locations.

use super::{Lexer, Token, TokenKind, next_script_token, scan};
use crate::unwind_write::{
    GraphUnwindWriteError, GraphUnwindWriteText, MAX_UNWIND_FIELD_STEPS, MAX_UNWIND_SOURCES,
    UnwindField, UnwindFieldAccess, UnwindSource, UnwindSourceOffset,
};
use crate::{GraphPatternTextErrorKind, GraphWriteScriptError, GraphWriteScriptErrorKind};
use std::collections::{BTreeMap, BTreeSet};

fn syntax(offset: usize, expected: &'static str) -> GraphUnwindWriteError {
    GraphUnwindWriteError::Syntax(GraphWriteScriptError {
        statement: Some(0),
        offset,
        kind: GraphWriteScriptErrorKind::Syntax(GraphPatternTextErrorKind::Expected(expected)),
    })
}

fn token<'a>(lexer: &mut Lexer<'a>) -> Result<(Token<'a>, usize), GraphUnwindWriteError> {
    let token = next_script_token(lexer).map_err(|source| {
        GraphUnwindWriteError::Syntax(GraphWriteScriptError::syntax(Some(0), 0, source))
    })?;
    Ok((token, lexer.at))
}

fn is_word(token: &Token<'_>, word: &str) -> bool {
    matches!(token.kind, TokenKind::Word(value) if value.eq_ignore_ascii_case(word))
}

fn is_punct(token: Option<&(Token<'_>, usize)>, byte: u8) -> bool {
    token.is_some_and(|(token, _)| matches!(token.kind, TokenKind::Punct(value) if value == byte))
}

fn blank(bytes: &mut [u8]) {
    for byte in bytes {
        if !matches!(*byte, b'\n' | b'\r') {
            *byte = b' ';
        }
    }
}

// Consume a complete, static input path using the SAME native tokens. The
// returned end is an original UTF-8 byte boundary, including all selectors.
// Dynamic keys/indexes, slices and arithmetic indexes cannot be partly erased.
fn field_path(
    tokens: &[(Token<'_>, usize)],
    start: usize,
) -> Result<(Vec<UnwindFieldAccess>, usize, usize), GraphUnwindWriteError> {
    let mut path = Vec::new();
    let mut next = start + 1;
    let mut end = tokens[start].1;
    while is_punct(tokens.get(next), b'.') || is_punct(tokens.get(next), b'[') {
        let offset = tokens[next].0.at;
        if path.len() == MAX_UNWIND_FIELD_STEPS {
            return Err(syntax(offset, "at most 64 row-field selectors"));
        }
        if is_punct(tokens.get(next), b'.') {
            let Some((
                Token {
                    kind: TokenKind::Word(name),
                    ..
                },
                last,
            )) = tokens.get(next + 1)
            else {
                return Err(syntax(offset, "a map field name after '.'"));
            };
            path.push(UnwindFieldAccess::Key((*name).to_owned()));
            end = *last;
            next += 2;
        } else {
            next += 1;
            let negative = is_punct(tokens.get(next), b'-');
            if negative || is_punct(tokens.get(next), b'+') {
                next += 1;
            }
            let Some((
                Token {
                    kind: TokenKind::Digits(digits),
                    ..
                },
                _,
            )) = tokens.get(next)
            else {
                return Err(syntax(offset, "a constant signed integer list index"));
            };
            let magnitude = digits
                .parse::<u64>()
                .map_err(|_| syntax(offset, "a list index in the Int64 range"))?;
            let signed = if negative {
                -i128::from(magnitude)
            } else {
                i128::from(magnitude)
            };
            let index = i64::try_from(signed)
                .map_err(|_| syntax(offset, "a list index in the Int64 range"))?;
            next += 1;
            if !is_punct(tokens.get(next), b']') {
                return Err(syntax(offset, "']' after a constant list index"));
            }
            end = tokens[next].1;
            next += 1;
            path.push(UnwindFieldAccess::Index(index));
        }
    }
    Ok((path, next, end))
}

fn fresh_parameter(reserved: &mut BTreeSet<String>, cursor: &mut usize) -> Option<String> {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    while *cursor < LETTERS.len() * LETTERS.len() {
        let index = *cursor;
        *cursor += 1;
        let name = String::from_utf8(vec![
            LETTERS[index / LETTERS.len()],
            LETTERS[index % LETTERS.len()],
        ])
        .expect("ASCII parameter name");
        if reserved.insert(name.clone()) {
            return Some(name);
        }
    }
    None
}

// Do not intercept the existing native UNWIND [MATCH] CREATE/INSERT pipeline.
// Property names and pattern-local labels are not clause keywords.
fn mutating_tail(tokens: &[(Token<'_>, usize)]) -> bool {
    let mut depth = 0usize;
    for (index, (token, _)) in tokens.iter().enumerate() {
        if depth == 0
            && !is_punct(index.checked_sub(1).and_then(|i| tokens.get(i)), b'.')
            && !is_punct(tokens.get(index + 1), b'.')
            && !index
                .checked_sub(1)
                .and_then(|i| tokens.get(i))
                .is_some_and(|(previous, _)| is_word(previous, "AS"))
        {
            let next = tokens.get(index + 1);
            let target = next.is_some_and(|(token, _)| matches!(token.kind, TokenKind::Word(_)));
            if (is_word(token, "CREATE") || is_word(token, "INSERT")) && is_punct(next, b'(') {
                return false;
            }
            if is_word(token, "MERGE") && is_punct(next, b'(')
                || (is_word(token, "SET") || is_word(token, "REMOVE"))
                    && target
                    && b".:+"
                        .iter()
                        .any(|byte| is_punct(tokens.get(index + 2), *byte))
                || is_word(token, "DETACH")
                    && next.is_some_and(|(token, _)| is_word(token, "DELETE"))
                || is_word(token, "DELETE")
                    && target
                    && (tokens.get(index + 2).is_none() || is_punct(tokens.get(index + 2), b','))
            {
                return true;
            }
        }
        match token.kind {
            TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

impl GraphUnwindWriteText {
    /// Parse the bounded `UNWIND $rows AS row MERGE ...` or `UNWIND $rows AS row
    /// MATCH ... SET/REMOVE/DELETE/MERGE ...` adapter, optionally with chained
    /// `UNWIND earlier[.static.path] AS alias` clauses before the mutation.
    /// Later clauses may also select lists from independent parameters, such
    /// as `$payload.groups[-1].members`. Missing/null selections expand to no
    /// rows; selected non-list values refuse before catalog access. Reusing a
    /// parameter under a fresh alias forms a product, not a zip. Source-only
    /// parameters are borrowed for expansion, not replicated into each native
    /// argument transcript. Every intermediate product retains the row cap.
    /// Mutation operands may use bare scalar aliases or static scalar paths.
    /// Ordinary CREATE forms keep their existing native compiler.
    pub fn parse(text: &str) -> Result<Self, GraphUnwindWriteError> {
        Self::parse_if_supported(text)?.ok_or_else(|| {
            syntax(
                0,
                "UNWIND $rows AS row followed by MERGE or a MATCH mutation",
            )
        })
    }

    /// Recognize only the additional bounded write surface. None means use the
    /// existing native compiler; malformed recognized text is a typed refusal.
    /// No values, catalog callbacks or graph state are inspected at this stage.
    pub fn parse_if_supported(text: &str) -> Result<Option<Self>, GraphUnwindWriteError> {
        let mut lexer = Lexer {
            text,
            at: 0,
            tokens: 0,
        };
        let first = token(&mut lexer)?;
        if !is_word(&first.0, "UNWIND") {
            return Ok(None);
        }
        let source = token(&mut lexer)?;
        let TokenKind::Parameter(source_name) = source.0.kind else {
            return Ok(None);
        };
        let as_token = token(&mut lexer)?;
        if !is_word(&as_token.0, "AS") {
            return Ok(None);
        }
        let alias_token = token(&mut lexer)?;
        let TokenKind::Word(alias) = alias_token.0.kind else {
            return Ok(None);
        };
        let tail = token(&mut lexer)?;
        if !is_word(&tail.0, "MERGE") && !is_word(&tail.0, "MATCH") && !is_word(&tail.0, "UNWIND") {
            return Ok(None);
        }

        let statements = scan(text).map_err(GraphUnwindWriteError::Syntax)?;
        if statements.len() != 1 {
            return Ok(None);
        }
        let mut tokens = vec![tail];
        loop {
            let next = token(&mut lexer)?;
            if matches!(next.0.kind, TokenKind::End | TokenKind::Punct(b';')) {
                break;
            }
            tokens.push(next);
        }
        if !mutating_tail(&tokens) {
            return Ok(None);
        }

        // Resolve scope coordinates, not values. A later source may refer to
        // ANY earlier alias (sibling expansions form the ordinary product).
        // Inspect the mutation family first so existing CREATE pipelines with
        // other UNWIND expression forms are never intercepted by this adapter.
        let mut aliases = BTreeMap::from([(alias, 0usize)]);
        let mut sources = Vec::new();
        let mut tail_at = 0;
        while tokens
            .get(tail_at)
            .is_some_and(|(token, _)| is_word(token, "UNWIND"))
        {
            let at = tokens[tail_at].0.at;
            if sources.len() + 1 == MAX_UNWIND_SOURCES {
                return Err(syntax(at, "at most eight UNWIND sources"));
            }
            let start = tail_at + 1;
            let Some((input, _)) = tokens.get(start) else {
                return Err(syntax(at, "a list parameter or an earlier UNWIND alias"));
            };
            let (source, parameter, path, next) = match input.kind {
                TokenKind::Parameter(name) => {
                    let (path, next, _) = field_path(&tokens, start)?;
                    (0, Some(name.to_owned()), path, next)
                }
                TokenKind::Word(parent) => {
                    let Some(&source) = aliases.get(parent) else {
                        return Err(syntax(input.at, "an earlier UNWIND alias"));
                    };
                    let (path, next, _) = field_path(&tokens, start)?;
                    (source, None, path, next)
                }
                _ => {
                    return Err(syntax(
                        input.at,
                        "a list parameter or an earlier UNWIND alias",
                    ));
                }
            };
            if !tokens
                .get(next)
                .is_some_and(|(token, _)| is_word(token, "AS"))
            {
                return Err(syntax(
                    tokens[start].0.at,
                    "a static list path followed by AS",
                ));
            }
            let Some((
                Token {
                    kind: TokenKind::Word(name),
                    at: alias_at,
                },
                _,
            )) = tokens.get(next + 1)
            else {
                return Err(syntax(at, "a fresh alias after AS"));
            };
            if aliases.contains_key(name) {
                return Err(syntax(*alias_at, "a fresh UNWIND alias without rebinding"));
            }
            sources.push(UnwindSource {
                source,
                parameter,
                path: path.into_boxed_slice(),
                offset: tokens[start].0.at,
            });
            aliases.insert(*name, sources.len());
            tail_at = next + 2;
        }
        let tokens = &tokens[tail_at..];
        if !tokens
            .first()
            .is_some_and(|(token, _)| is_word(token, "MERGE") || is_word(token, "MATCH"))
        {
            return Err(syntax(
                text.len(),
                "MERGE or MATCH after the UNWIND sources",
            ));
        }
        let external_parameters: Vec<String> = statements[0]
            .parameters
            .iter()
            .filter(|name| **name != source_name)
            .map(|name| (*name).to_owned())
            .collect();
        let mut reserved: BTreeSet<String> = statements[0]
            .parameters
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        let global_parameter_count = external_parameters
            .iter()
            .filter(|name| {
                !sources
                    .iter()
                    .any(|source| source.parameter.as_deref() == Some(name.as_str()))
            })
            .count();
        let mut cursor = 0;
        let mut fields: Vec<UnwindField> = Vec::new();
        let mut field_index = BTreeMap::new();
        let mut lowered = text.as_bytes().to_vec();
        let mut short = Vec::new();
        blank(&mut lowered[..tokens[0].0.at]);

        // Outside a property map a ':' before a word names a label or a
        // relationship type. Inside one it introduces a value: `{id: row.id}`
        // is a row field use and must be lowered or refused like any other.
        let mut in_map = Vec::with_capacity(tokens.len());
        let mut braces = 0_usize;
        for (token, _) in tokens {
            in_map.push(braces > 0);
            match token.kind {
                TokenKind::Punct(b'{') => braces += 1,
                TokenKind::Punct(b'}') => braces = braces.saturating_sub(1),
                _ => {}
            }
        }

        let mut index = 0;
        while index < tokens.len() {
            let current = &tokens[index].0;
            if matches!(current.kind, TokenKind::Parameter(name)
                if name == source_name || sources.iter().any(|source| source.parameter.as_deref() == Some(name)))
            {
                return Err(syntax(
                    current.at,
                    "UNWIND source parameters only in their prefixes",
                ));
            }
            let TokenKind::Word(name) = current.kind else {
                index += 1;
                continue;
            };
            let Some(&source) = aliases.get(name) else {
                index += 1;
                continue;
            };
            let previous = index.checked_sub(1).and_then(|i| tokens.get(i));
            if is_punct(previous, b'.') || (is_punct(previous, b':') && !in_map[index]) {
                index += 1;
                continue;
            }
            // A literal map key with the alias's spelling is not a variable use.
            if (is_punct(previous, b'{') || is_punct(previous, b','))
                && is_punct(tokens.get(index + 1), b':')
            {
                index += 1;
                continue;
            }
            let (path, next, end) = field_path(tokens, index)?;
            let identity = (source, path);
            let field = if let Some(existing) = field_index.get(&identity) {
                *existing
            } else {
                if global_parameter_count + fields.len() + 1
                    > crate::parameters::MAX_GQL_PARAMETER_COUNT
                {
                    return Err(syntax(current.at, "at most 1000 expanded parameters"));
                }
                let parameter = fresh_parameter(&mut reserved, &mut cursor)
                    .ok_or_else(|| syntax(current.at, "an available bounded parameter name"))?;
                let field = fields.len();
                fields.push(UnwindField {
                    source,
                    path: identity.1.clone().into_boxed_slice(),
                    parameter,
                    offset: current.at,
                });
                field_index.insert(identity, field);
                field
            };
            let start = current.at;
            if end - start < 3 {
                // Tokenization is still in original coordinates. Apply these
                // growing replacements only after all token ranges are sealed.
                short.push((start, end, field));
            } else {
                blank(&mut lowered[start..end]);
                lowered[start] = b'$';
                lowered[start + 1..start + 3].copy_from_slice(fields[field].parameter.as_bytes());
            }
            index = next;
        }
        let mut source_offsets = Vec::with_capacity(short.len());
        if !short.is_empty() {
            // Each short token adds at most two bytes; scan's native byte/token
            // limits bound the number. Long tokens, comments and UTF-8 literals
            // were never resized. No input VALUE is copied into this buffer.
            let extra: usize = short.iter().map(|(start, end, _)| 3 - (end - start)).sum();
            let mut expanded = Vec::with_capacity(lowered.len() + extra);
            let mut copied = 0;
            for (start, end, field) in short {
                expanded.extend_from_slice(&lowered[copied..start]);
                let generated = expanded.len();
                expanded.push(b'$');
                expanded.extend_from_slice(fields[field].parameter.as_bytes());
                source_offsets.push(UnwindSourceOffset {
                    generated: generated..expanded.len(),
                    original: start..end,
                });
                copied = end;
            }
            expanded.extend_from_slice(&lowered[copied..]);
            lowered = expanded;
        }
        let lowered = String::from_utf8(lowered).expect("whole tokens replaced with ASCII padding");
        Ok(Some(Self {
            original: text.to_owned(),
            lowered,
            source_offsets: source_offsets.into_boxed_slice(),
            source_parameter: source_name.to_owned(),
            sources: sources.into_boxed_slice(),
            external_parameters: external_parameters.into_boxed_slice(),
            fields: fields.into_boxed_slice(),
        }))
    }
}
