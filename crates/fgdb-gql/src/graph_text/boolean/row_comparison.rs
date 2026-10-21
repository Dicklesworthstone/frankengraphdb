//! Non-equality correlations for a multipart MATCH's join ON predicate.
//!
//! Only the multipart owner enables this channel. A leading single-part
//! UNWIND, a nested graph scope, and the graph Boolean evaluator must never
//! accept a row-dependent predicate they cannot execute. Equality keeps its
//! existing key lowering and transcript. These atoms are top-level conjuncts.

use super::super::*;

pub(in crate::graph_text) struct RowComparison<'a> {
    pub(in crate::graph_text) variable: Name<'a>,
    pub(in crate::graph_text) property: Option<Name<'a>>,
    pub(in crate::graph_text) row: usize,
    /// Normalized to imported LEFT row compared with RIGHT graph projection.
    pub(in crate::graph_text) comparison: IntegerComparison,
    pub(in crate::graph_text) at: usize,
}

/// Look ahead with the shared lexer. Successful admission is replayed through
/// name()/comparison(), preserving reserved-name checks and token accounting.
#[allow(clippy::type_complexity)]
fn operand<'a>(
    token: Token<'a>,
    lexer: &mut Lexer<'a>,
) -> Result<Option<(Name<'a>, Option<Name<'a>>, Token<'a>)>, GraphPatternTextError> {
    let TokenKind::Word(text) = token.kind else {
        return Ok(None);
    };
    let name = Name { text, at: token.at };
    let mut next = lexer.next()?;
    let property = if next.kind == TokenKind::Punct(b'.') {
        let key = lexer.next()?;
        let TokenKind::Word(text) = key.kind else {
            return Ok(None);
        };
        next = lexer.next()?;
        Some(Name { text, at: key.at })
    } else {
        None
    };
    Ok(Some((name, property, next)))
}

fn reversed(comparison: IntegerComparison) -> IntegerComparison {
    use IntegerComparison::*;
    match comparison {
        Equal => Equal,
        NotEqual => NotEqual,
        Greater => Less,
        Less => Greater,
        GreaterOrEqual => LessOrEqual,
        LessOrEqual => GreaterOrEqual,
    }
}

impl<'a> Parser<'a> {
    pub(super) fn row_comparison(
        &mut self,
    ) -> Result<Option<RowComparison<'a>>, GraphPatternTextError> {
        if self.read_comparisons.is_none() || self.read_row_bindings.is_empty() {
            return Ok(None);
        }
        let at = self.current.at;
        let mut lexer = self.lexer.clone();
        let Some((left, left_property, operator)) = operand(self.current, &mut lexer)? else {
            return Ok(None);
        };
        // Leave '=' entirely to row_correlation(), including its pinned map
        // equivalence. No speculative parse may consume the live parser.
        let (comparison, right_token) = match operator.kind {
            TokenKind::Punct(b'!') => {
                if lexer.next()?.kind != TokenKind::Punct(b'=') {
                    return Ok(None);
                }
                (IntegerComparison::NotEqual, lexer.next()?)
            }
            TokenKind::Punct(b'<') => {
                let next = lexer.next()?;
                match next.kind {
                    TokenKind::Punct(b'>') => (IntegerComparison::NotEqual, lexer.next()?),
                    TokenKind::Punct(b'=') => (IntegerComparison::LessOrEqual, lexer.next()?),
                    _ => (IntegerComparison::Less, next),
                }
            }
            TokenKind::Punct(b'>') => {
                let next = lexer.next()?;
                if next.kind == TokenKind::Punct(b'=') {
                    (IntegerComparison::GreaterOrEqual, lexer.next()?)
                } else {
                    (IntegerComparison::Greater, next)
                }
            }
            _ => return Ok(None),
        };
        let Some((right, right_property, end)) = operand(right_token, &mut lexer)? else {
            return Ok(None);
        };
        if !matches!(
            end.kind,
            TokenKind::End | TokenKind::Word(_) | TokenKind::Punct(b')')
        ) {
            return Ok(None);
        }
        let row = |name: Name<'a>| {
            if self.elements.contains(&name.text) {
                None
            } else {
                self.read_row_bindings
                    .iter()
                    .position(|binding| binding.text == name.text)
            }
        };
        let graph = |name: Name<'a>| {
            if self.elements.contains(&name.text) {
                return None;
            }
            self.syntax.variables[..self.syntax.root_variables]
                .iter()
                .copied()
                .find(|variable| variable.text == name.text)
                .or_else(|| {
                    self.syntax
                        .visible_edge(name.text)
                        .filter(|(edge, scoped)| !*scoped && edge.walk.is_none())
                        .and_then(|(edge, _)| edge.variable)
                })
        };
        let (row_first, variable, property, normalized) =
            match (row(left), graph(left), row(right), graph(right)) {
                (Some(_), None, None, Some(variable)) => {
                    (true, variable, right_property, comparison)
                }
                (None, Some(variable), Some(_), None) => {
                    (false, variable, left_property, reversed(comparison))
                }
                _ => return Ok(None),
            };
        self.capacity(
            self.predicates,
            MAX_PATTERN_PREDICATES,
            crate::algebra::PatternLimitDimension::Predicates,
        )?;
        if property.is_some() {
            self.require_property_variable(variable)?;
        }
        // Dotted imported operands are evaluated once per LEFT occurrence by
        // the same native MapGet projection used for equality/map keys. They
        // must never read the optional join's nullable RIGHT copy.
        let row = if row_first {
            let row = self
                .imported_row_field()?
                .expect("lookahead proved an imported comparison operand");
            self.comparison()?;
            Some(row)
        } else {
            None
        };
        self.name()?;
        if property.is_some() {
            self.punct(b'.', ".")?;
            self.name()?;
        }
        let row = if let Some(row) = row {
            row
        } else {
            self.comparison()?;
            self.imported_row_field()?
                .expect("lookahead proved an imported comparison operand")
        };
        self.predicates += 1;
        Ok(Some(RowComparison {
            variable,
            property,
            row,
            comparison: normalized,
            at,
        }))
    }
}
