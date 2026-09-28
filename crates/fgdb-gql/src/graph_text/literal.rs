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
            return Ok(Token {
                kind: TokenKind::Quoted(raw),
                at,
            });
        }
        Err(error(
            at,
            GraphPatternTextErrorKind::Expected("closing single quote"),
        ))
    }
}

pub(in crate::graph_text) fn text_scalar(
    raw: &str,
    at: usize,
) -> Result<CanonicalScalar, GraphPatternTextError> {
    let refusal = || error(at, GraphPatternTextErrorKind::ScalarLiteral);
    if !raw.contains("''") {
        return CanonicalScalar::ucs_basic_text(raw).map_err(|_| refusal());
    }
    let mut decoded = String::new();
    decoded
        .try_reserve_exact(raw.len())
        .map_err(|_| refusal())?;
    let mut parts = raw.split("''");
    decoded.push_str(parts.next().unwrap_or_default());
    for part in parts {
        decoded.push('\'');
        decoded.push_str(part);
    }
    CanonicalScalar::ucs_basic_text(&decoded).map_err(|_| refusal())
}

impl<'a> Parser<'a> {
    /// `0.5`: a non-negative decimal literal, `digits . digits`, as a Float
    /// scalar (fgdb-qnqrj), so a Float value (a Prism score, a float
    /// property) compares with a Float of the same kind. `None` consumes
    /// nothing. The text is parsed once, to the nearest f64, and must be
    /// finite. A sign is the ordinary unary operator, and an exponent is not
    /// accepted here.
    pub(in crate::graph_text) fn float_literal(
        &mut self,
    ) -> Result<Option<CanonicalScalar>, GraphPatternTextError> {
        let TokenKind::Digits(whole) = self.current.kind else {
            return Ok(None);
        };
        let mut lexer = self.lexer.clone();
        if !matches!(lexer.next()?.kind, TokenKind::Punct(b'.')) {
            return Ok(None);
        }
        let TokenKind::Digits(fraction) = lexer.next()?.kind else {
            return Ok(None);
        };
        let at = self.current.at;
        let value = format!("{whole}.{fraction}")
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .ok_or_else(|| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
        for _ in 0..3 {
            self.advance()?;
        }
        Ok(Some(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(
            value,
        ))))
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
