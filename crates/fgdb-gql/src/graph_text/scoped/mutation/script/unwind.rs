//! Structural UNWIND lowering, using the ordinary write-script lexer.
//!
//! Three-byte synthetic parameter tokens fit even the shortest `r.x` access.
//! Padding instead of removing bytes preserves every native diagnostic offset.

use super::{Lexer, Token, TokenKind, next_script_token, scan};
use crate::unwind_write::{
    GraphUnwindWriteError, GraphUnwindWriteText, MAX_UNWIND_FIELD_STEPS,
    UnwindField, UnwindFieldAccess,
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
            let Some((Token { kind: TokenKind::Word(name), .. }, last)) = tokens.get(next + 1) else {
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
            let Some((Token { kind: TokenKind::Digits(digits), .. }, _)) = tokens.get(next) else {
                return Err(syntax(offset, "a constant signed integer list index"));
            };
            let magnitude = digits.parse::<u64>()
                .map_err(|_| syntax(offset, "a list index in the Int64 range"))?;
            let signed = if negative { -i128::from(magnitude) } else { i128::from(magnitude) };
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
        {
            if is_word(token, "CREATE") || is_word(token, "INSERT") {
                return false;
            }
            if ["MERGE", "SET", "REMOVE", "DELETE", "DETACH"]
                .iter()
                .any(|word| is_word(token, word))
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
    /// MATCH ... SET/REMOVE/DELETE/MERGE ...` adapter. Ordinary CREATE forms are
    /// intentionally handled by their existing native compiler, not this path.
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
        if !is_word(&tail.0, "MERGE") && !is_word(&tail.0, "MATCH") {
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
        let mut cursor = 0;
        let mut fields: Vec<UnwindField> = Vec::new();
        let mut field_index = BTreeMap::new();
        let mut lowered = text.as_bytes().to_vec();
        blank(&mut lowered[..tokens[0].0.at]);

        // Outside a property map a ':' before a word names a label or a
        // relationship type. Inside one it introduces a value: `{id: row.id}`
        // is a row field use and must be lowered or refused like any other.
        let mut in_map = Vec::with_capacity(tokens.len());
        let mut braces = 0_usize;
        for (token, _) in &tokens {
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
            if matches!(current.kind, TokenKind::Parameter(name) if name == source_name) {
                return Err(syntax(
                    current.at,
                    "the UNWIND source parameter only in its prefix",
                ));
            }
            if !matches!(current.kind, TokenKind::Word(name) if name == alias) {
                index += 1;
                continue;
            }
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
            if !is_punct(tokens.get(index + 1), b'.') {
                return Err(syntax(
                    current.at,
                    "scalar row.field access without alias rebinding",
                ));
            }
            let (path, next, end) = field_path(&tokens, index)?;
            let field = if let Some(existing) = field_index.get(&path) {
                *existing
            } else {
                if external_parameters.len() + fields.len() + 1
                    > crate::parameters::MAX_GQL_PARAMETER_COUNT
                {
                    return Err(syntax(current.at, "at most 1000 expanded parameters"));
                }
                let parameter = fresh_parameter(&mut reserved, &mut cursor)
                    .ok_or_else(|| syntax(current.at, "an available bounded parameter name"))?;
                let field = fields.len();
                fields.push(UnwindField {
                    path: path.clone().into_boxed_slice(),
                    parameter,
                    offset: current.at,
                });
                field_index.insert(path, field);
                field
            };
            let start = current.at;
            if end - start < 3 {
                return Err(syntax(start, "a complete row.field expression"));
            }
            blank(&mut lowered[start..end]);
            lowered[start] = b'$';
            lowered[start + 1..start + 3].copy_from_slice(fields[field].parameter.as_bytes());
            index = next;
        }
        let lowered = String::from_utf8(lowered).expect("whole tokens replaced with ASCII padding");
        Ok(Some(Self {
            original: text.to_owned(),
            lowered,
            source_parameter: source_name.to_owned(),
            source_offset: source.0.at,
            external_parameters: external_parameters.into_boxed_slice(),
            fields: fields.into_boxed_slice(),
        }))
    }
}
