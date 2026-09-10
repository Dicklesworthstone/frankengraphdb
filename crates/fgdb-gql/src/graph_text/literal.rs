//! Scalar literal operands for the common positive/scoped WHERE grammar.
//! Only doubled single quotes escape a quote; backslashes are literal bytes.
//! Decoding affects the operand only, never the surrounding statement.

use super::*;
use crate::algebra::ScalarPredicate;
use fgdb_types::CanonicalScalar;

impl<'a> Lexer<'a> {
    /// Called after the shared token counter admits one opening quote. ASCII
    /// quote boundaries are UTF-8 boundaries even though scanning uses bytes.
    pub(super) fn quoted(&mut self) -> Result<Token<'a>, GraphPatternTextError> {
        let at = self.at;
        self.at += 1;
        let start = self.at;
        let bytes = self.text.as_bytes();
        while let Some(byte) = bytes.get(self.at) {
            if *byte != b'\'' {
                self.at += 1;
                continue;
            }
            if bytes.get(self.at + 1) == Some(&b'\'') {
                self.at += 2;
                continue;
            }
            let raw = &self.text[start..self.at];
            self.at += 1;
            return Ok(Token { kind: TokenKind::Quoted(raw), at });
        }
        Err(error(at, GraphPatternTextErrorKind::Expected("closing single quote")))
    }
}

fn text_scalar(raw: &str, at: usize) -> Result<CanonicalScalar, GraphPatternTextError> {
    let refusal = || error(at, GraphPatternTextErrorKind::ScalarLiteral);
    if !raw.contains("''") {
        return CanonicalScalar::ucs_basic_text(raw).map_err(|_| refusal());
    }
    let mut decoded = String::new();
    decoded.try_reserve_exact(raw.len()).map_err(|_| refusal())?;
    let mut parts = raw.split("''");
    decoded.push_str(parts.next().unwrap_or_default());
    for part in parts {
        decoded.push('\'');
        decoded.push_str(part);
    }
    CanonicalScalar::ucs_basic_text(&decoded).map_err(|_| refusal())
}

impl<'a> Parser<'a> {
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
            return Ok(Filter::Null { variable, key, is_null: !negate });
        }
        let comparison = self.comparison()?;
        let at = self.current.at;
        let literal = match self.current.kind {
            TokenKind::Quoted(raw) => Some(text_scalar(raw, at)?),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("TRUE") => Some(CanonicalScalar::Bool(true)),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("FALSE") => Some(CanonicalScalar::Bool(false)),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("NULL") => Some(CanonicalScalar::Null),
            _ => None,
        };
        if let Some(value) = literal {
            self.advance()?;
            let predicate = ScalarPredicate::new(value, comparison)
                .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
            Ok(Filter::Scalar { variable, key, predicate })
        } else {
            let expected = match self.current.kind {
                TokenKind::Parameter(name) => self.parameter_types.get(name).copied()
                    .filter(|kind| matches!(kind, GqlParameterType::Scalar(_)))
                    .unwrap_or(GqlParameterType::Int64),
                _ => GqlParameterType::Int64,
            };
            let value = self.number(expected)?;
            Ok(Filter::Property { variable, key, comparison, value })
        }
    }
}
