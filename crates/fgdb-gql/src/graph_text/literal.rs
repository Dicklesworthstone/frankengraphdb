//! The one lexical rule every fgdb-gql statement scanner shares
//! (fgdb-285i2), and the scalar literal operands of the common WHERE grammar.
//!
//! Whitespace, `//` line comments and `/* ... */` block comments are trivia.
//! A block comment does not nest: its first `*/` closes it. `'...'` and
//! `"..."` are text literals and `` `...` `` is a delimited identifier. Each
//! escapes its own delimiter only by doubling it; a backslash is an ordinary
//! byte. Decoding affects the operand only, never the surrounding statement.

use super::*;
use crate::algebra::ScalarPredicate;
use fgdb_types::CanonicalScalar;

/// Every word the fgdb-gql grammars compare a Word token against. Keywords
/// are plain Word tokens, so a delimited identifier spelling one would be
/// read as that keyword; the lexer refuses it instead. The plain spelling of
/// a non-reserved name (`n.type`) is unaffected.
const KEYWORDS: &[&str] = &[
    "ABS",
    "ACYCLIC",
    "ALL",
    "AND",
    "ANY",
    "AS",
    "ASC",
    "AT",
    "AVG",
    "AVG_INT",
    "BETWEEN",
    "BRANCH",
    "BY",
    "CALL",
    "CASE",
    "CEIL",
    "CHAR_LENGTH",
    "CHEAPEST",
    "COALESCE",
    "COLLECT",
    "CONTAINS",
    "COST",
    "COUNT",
    "CREATE",
    "DELETE",
    "DESC",
    "DETACH",
    "DISTINCT",
    "EDGES",
    "ELSE",
    "END",
    "ENDNODE",
    "ENDS",
    "EXCEPT",
    "EXISTS",
    "FALSE",
    "FLOOR",
    "FIRST",
    "FOR",
    "FROM",
    "GROUP",
    "GROUPS",
    "HAVING",
    "IN",
    "INSERT",
    "INTERSECT",
    "IS",
    "LABELS",
    "LAST",
    "LENGTH",
    "LIMIT",
    "LOWER",
    "MATCH",
    "MAX",
    "MERGE",
    "MIN",
    "NODES",
    "NOT",
    "NULL",
    "NULLIF",
    "NULLS",
    "OF",
    "OFFSET",
    "ON",
    "OPTIONAL",
    "OR",
    "ORDER",
    "PATH",
    "PATH_LENGTH",
    "PATHS",
    "RELATIONSHIPS",
    "REMOVE",
    "RETURN",
    "ROUND",
    "SEQ",
    "SET",
    "SHORTEST",
    "SIMPLE",
    "SIZE",
    "SKIP",
    "SQRT",
    "STARTNODE",
    "STARTS",
    "SUBSTRING",
    "SUM",
    "SUM_INT",
    "SYSTEM_TIME",
    "THEN",
    "TO",
    "TOFLOAT",
    "TOINTEGER",
    "TOLOWER",
    "TOSTRING",
    "TOUPPER",
    "TRAIL",
    "TRIM",
    "TRUE",
    "TYPE",
    "UNION",
    "UNWIND",
    "UPPER",
    "WALK",
    "WHEN",
    "WHERE",
    "WITH",
    "YIELD",
];

/// If a comment starts at `at`, the offset just past it. `Err(at)`: a block
/// comment with no closing `*/`.
pub(crate) fn comment_end(text: &str, at: usize) -> Result<Option<usize>, usize> {
    let rest = &text.as_bytes()[at..];
    if rest.starts_with(b"//") {
        Ok(Some(
            text[at..].find('\n').map_or(text.len(), |line| at + line),
        ))
    } else if rest.starts_with(b"/*") {
        text[at + 2..]
            .find("*/")
            .map(|close| Some(at + 2 + close + 2))
            .ok_or(at)
    } else {
        Ok(None)
    }
}

/// If a quoted span (`'`, `"` or a backtick) starts at `at`, the offset just
/// past its closing delimiter. `Err(at)`: the span never closes. The ASCII
/// delimiters are UTF-8 boundaries even though scanning uses bytes.
pub(crate) fn quoted_end(text: &str, at: usize) -> Result<Option<usize>, usize> {
    let bytes = text.as_bytes();
    let Some(&delimiter) = bytes.get(at).filter(|byte| b"'\"`".contains(byte)) else {
        return Ok(None);
    };
    let mut end = at + 1;
    while let Some(&byte) = bytes.get(end) {
        end += 1;
        if byte == delimiter {
            if bytes.get(end) != Some(&delimiter) {
                return Ok(Some(end));
            }
            end += 1;
        }
    }
    Err(at)
}

/// For the infallible root scanners that only locate clause keywords: if a
/// quoted span or a comment starts at `at`, the offset just past it, or the
/// end of `text` when it never closes (the native parser reports that).
pub(crate) fn opaque_end(text: &str, at: usize) -> Option<usize> {
    match quoted_end(text, at) {
        Ok(None) => comment_end(text, at).unwrap_or(Some(text.len())),
        Ok(end) => end,
        Err(_) => Some(text.len()),
    }
}

/// The offset of the first byte at or after `at` that is neither whitespace
/// nor inside a comment. `Err(at)`: an unclosed block comment starts at `at`.
pub(crate) fn trivia_end(text: &str, mut at: usize) -> Result<usize, usize> {
    loop {
        while let Some(ch) = text[at..].chars().next().filter(|ch| ch.is_whitespace()) {
            at += ch.len_utf8();
        }
        match comment_end(text, at)? {
            Some(end) => at = end,
            None => return Ok(at),
        }
    }
}

impl<'a> Lexer<'a> {
    pub(super) fn skip_trivia(&mut self) -> Result<(), GraphPatternTextError> {
        self.at = trivia_end(self.text, self.at).map_err(|at| {
            error(
                at,
                GraphPatternTextErrorKind::Expected("*/ closing the block comment"),
            )
        })?;
        Ok(())
    }

    /// Called after the shared token counter admits one opening delimiter.
    /// A text literal's token is the whole literal, delimiters included, so
    /// text_scalar knows which doubled delimiter to decode. A delimited
    /// identifier is a Word of its body; Word borrows the statement, so a
    /// doubled backtick inside one cannot be decoded and is refused.
    pub(super) fn quoted(&mut self) -> Result<Token<'a>, GraphPatternTextError> {
        let at = self.at;
        let delimiter = self.text.as_bytes()[at];
        let Ok(Some(end)) = quoted_end(self.text, at) else {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected(match delimiter {
                    b'\'' => "closing single quote",
                    b'"' => "closing double quote",
                    _ => "closing backtick",
                }),
            ));
        };
        self.at = end;
        let raw = &self.text[at..end];
        if delimiter != b'`' {
            return Ok(Token {
                kind: TokenKind::Quoted(raw),
                at,
            });
        }
        let body = &raw[1..raw.len() - 1];
        if body.is_empty() || body.contains('`') {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected(
                    "a nonempty delimited identifier without a doubled backtick",
                ),
            ));
        }
        if body.len() > MAX_PATTERN_NAME_BYTES {
            return Err(error(at, GraphPatternTextErrorKind::NameTooLong));
        }
        if KEYWORDS
            .iter()
            .any(|keyword| body.eq_ignore_ascii_case(keyword))
        {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected("a delimited identifier that is not a keyword"),
            ));
        }
        Ok(Token {
            kind: TokenKind::Word(body),
            at,
        })
    }
}

/// Decode one text literal token, delimiters included: a doubled delimiter
/// is one delimiter character, and nothing else is an escape.
pub(in crate::graph_text) fn text_scalar(
    raw: &str,
    at: usize,
) -> Result<CanonicalScalar, GraphPatternTextError> {
    let refusal = || error(at, GraphPatternTextErrorKind::ScalarLiteral);
    let (doubled, single) = if raw.starts_with('"') {
        ("\"\"", '"')
    } else {
        ("''", '\'')
    };
    let body = raw
        .len()
        .checked_sub(1)
        .and_then(|end| raw.get(1..end))
        .ok_or_else(refusal)?;
    if !body.contains(doubled) {
        return CanonicalScalar::ucs_basic_text(body).map_err(|_| refusal());
    }
    let mut decoded = String::new();
    decoded
        .try_reserve_exact(body.len())
        .map_err(|_| refusal())?;
    let mut parts = body.split(doubled);
    decoded.push_str(parts.next().unwrap_or_default());
    for part in parts {
        decoded.push(single);
        decoded.push_str(part);
    }
    CanonicalScalar::ucs_basic_text(&decoded).map_err(|_| refusal())
}

impl<'a> Parser<'a> {
    /// `0.5`, `1e5`, `1.5e-3`: a decimal literal with a fraction, an
    /// exponent or both, as a Float scalar (fgdb-qnqrj), so a Float value (a
    /// Prism score, a float property) compares with a Float of the same
    /// kind. The exponent is written without spaces (`e5`, `E-3`, `e+10`). A
    /// leading `-` makes it negative where no unary operator exists
    /// (`n.p > -0.5`); in an expression the sign is the ordinary unary
    /// operator. `None` consumes nothing, and plain digits stay an integer.
    /// The text is parsed once, to the nearest f64, and must be finite.
    pub(in crate::graph_text) fn float_literal(
        &mut self,
    ) -> Result<Option<CanonicalScalar>, GraphPatternTextError> {
        let Some((text, consumed)) = self.scan_float_literal()? else {
            return Ok(None);
        };
        let at = self.current.at;
        let value = text
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .ok_or_else(|| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
        for _ in 0..consumed {
            self.advance()?;
        }
        Ok(Some(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(
            value,
        ))))
    }

    /// The text of the float literal at the current token and how many
    /// tokens it spans, without consuming any.
    pub(in crate::graph_text) fn scan_float_literal(
        &self,
    ) -> Result<Option<(String, usize)>, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let negative = self.is_punct(b'-');
        let (whole, mut end, mut consumed) = if negative {
            let token = lexer.next()?;
            let TokenKind::Digits(whole) = token.kind else {
                return Ok(None);
            };
            (whole, token.at + whole.len(), 2)
        } else {
            let TokenKind::Digits(whole) = self.current.kind else {
                return Ok(None);
            };
            (whole, self.current.at + whole.len(), 1)
        };
        let mut text = format!("{}{whole}", if negative { "-" } else { "" });
        let mut float = false;
        let mut peek = lexer.clone();
        if matches!(peek.next()?.kind, TokenKind::Punct(b'.')) {
            let digits = peek.next()?;
            let TokenKind::Digits(fraction) = digits.kind else {
                return Ok(None);
            };
            text.push('.');
            text.push_str(fraction);
            end = digits.at + fraction.len();
            consumed += 2;
            float = true;
            lexer = peek;
        }
        let mut peek = lexer;
        let exponent = peek.next()?;
        if let TokenKind::Word(word) = exponent.kind
            && exponent.at == end
            && let Some(rest) = word.strip_prefix(['e', 'E'])
        {
            if !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()) {
                text.push('e');
                text.push_str(rest);
                consumed += 1;
                float = true;
            } else if rest.is_empty() {
                let sign = peek.next()?;
                let digits = peek.next()?;
                if let (TokenKind::Punct(sign_byte @ (b'+' | b'-')), TokenKind::Digits(power)) =
                    (sign.kind, digits.kind)
                    && sign.at == exponent.at + 1
                    && digits.at == sign.at + 1
                {
                    text.push('e');
                    text.push(char::from(sign_byte));
                    text.push_str(power);
                    consumed += 3;
                    float = true;
                }
            }
        }
        Ok(float.then_some((text, consumed)))
    }

    /// One predicate operand path used by the root and every positive child.
    /// IS NULL is not encoded as equality with NULL. Missing/stored null and
    /// scalar-kind mismatch continue to fail ordinary comparisons.
    pub(super) fn property_filter(
        &mut self,
        variable: Name<'a>,
        key: Name<'a>,
    ) -> Result<Filter<'a>, GraphPatternTextError> {
        if self.take_word("IS")? {
            let negate = self.take_word("NOT")?;
            self.word("NULL")?;
            return Ok(Filter::Null {
                variable,
                key,
                is_null: !negate,
            });
        }
        let comparison = self.comparison()?;
        self.property_operand(variable, key, comparison)
    }

    /// Parse one right operand without interpreting or synthesizing query text.
    /// Compound predicates use this same catalog, scalar and parameter path as
    /// ordinary comparisons. Each explicit parameter occurrence is admitted
    /// exactly once and retains its original byte offset.
    pub(super) fn property_operand(
        &mut self,
        variable: Name<'a>,
        key: Name<'a>,
        comparison: IntegerComparison,
    ) -> Result<Filter<'a>, GraphPatternTextError> {
        let at = self.current.at;
        // A property reference wins over literal-looking variable names such
        // as true or null. Lookahead uses the existing lexer, not text slicing.
        if matches!(self.current.kind, TokenKind::Word(_))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
        {
            let right = self.property_variable()?;
            self.punct(b'.', ".")?;
            let right_key = self.name()?;
            return Ok(Filter::Properties {
                left: variable,
                left_key: key,
                right,
                right_key,
                comparison,
            });
        }
        if let Some(value) = self.float_literal()? {
            let predicate = ScalarPredicate::new(value, comparison)
                .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
            return Ok(Filter::Scalar {
                variable,
                key,
                predicate,
            });
        }
        let literal = match self.current.kind {
            TokenKind::Quoted(raw) => Some(text_scalar(raw, at)?),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("TRUE") => {
                Some(CanonicalScalar::Bool(true))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("FALSE") => {
                Some(CanonicalScalar::Bool(false))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("NULL") => {
                Some(CanonicalScalar::Null)
            }
            _ => None,
        };
        if let Some(value) = literal {
            self.advance()?;
            let predicate = ScalarPredicate::new(value, comparison)
                .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
            Ok(Filter::Scalar {
                variable,
                key,
                predicate,
            })
        } else {
            let expected = match self.current.kind {
                TokenKind::Parameter(name) => self
                    .parameter_types
                    .get(name)
                    .copied()
                    .filter(|kind| matches!(kind, GqlParameterType::Scalar(_)))
                    .unwrap_or(GqlParameterType::Int64),
                _ => GqlParameterType::Int64,
            };
            let value = self.number(expected)?;
            Ok(Filter::Property {
                variable,
                key,
                comparison,
                value,
            })
        }
    }
}
