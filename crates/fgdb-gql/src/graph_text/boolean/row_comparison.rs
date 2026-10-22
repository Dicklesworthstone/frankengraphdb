//! Separable comparisons for a multipart MATCH's join ON predicate.
//!
//! Only the multipart owner enables this channel. A leading single-part
//! UNWIND, a nested graph scope, and the graph Boolean evaluator must never
//! accept a row-dependent predicate they cannot execute. Simple equality keeps
//! its existing key lowering and transcript. These atoms are top-level conjuncts.

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
    /// Lift a computed incoming-row operand into the ordinary private LEFT
    /// projection, leaving its comparison in ON. The graph operand remains a
    /// root property/identity. This is the same separable boundary as the
    /// existing dotted-key correlations, not a post-join WHERE filter.
    pub(super) fn computed_row_comparison(
        &mut self,
    ) -> Result<Option<RowComparison<'a>>, GraphPatternTextError> {
        if self.read_comparisons.is_none()
            || self.read_row_bindings.is_empty()
            || !self.starts_scalar_predicate()?
        {
            return Ok(None);
        }
        let imported = |name: &str| {
            !self.elements.contains(&name)
                && self
                    .read_row_bindings
                    .iter()
                    .any(|binding| binding.text == name)
                && !self
                    .syntax
                    .variables
                    .iter()
                    .any(|variable| variable.text == name)
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
        // Look ahead only. Names, expression programs, arguments and the
        // definition-wide budgets are admitted once by the live parser below.
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        let mut depth = 0_usize;
        let mut row_left = false;
        let mut graph_in_left = false;
        let mut after_dot = false;
        let mut previous = None;
        loop {
            match token.kind {
                TokenKind::End => return Ok(None),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    let Some(next) = depth.checked_sub(1) else {
                        return Ok(None);
                    };
                    depth = next;
                }
                TokenKind::Punct(b'=' | b'!' | b'<' | b'>') if depth == 0 => break,
                TokenKind::Word(word) if !after_dot => {
                    if depth == 0 && comparison_boundary(word, previous) {
                        return Ok(None);
                    }
                    if !matches!(lexer.clone().next()?.kind, TokenKind::Punct(b'(' | b':')) {
                        row_left |= imported(word);
                        graph_in_left |= graph(Name {
                            text: word,
                            at: token.at,
                        })
                        .is_some();
                    }
                }
                _ => {}
            }
            after_dot = token.kind == TokenKind::Punct(b'.');
            previous = Some(token.kind);
            token = lexer.next()?;
        }
        let operator = token;
        let (comparison, right) = match operator.kind {
            TokenKind::Punct(b'=') => (IntegerComparison::Equal, lexer.next()?),
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
        let mut left_lexer = self.lexer.clone();
        let left = operand(self.current, &mut left_lexer)?;
        let graph_left = left.and_then(|(name, property, end)| {
            (end.at == operator.at)
                .then(|| graph(name).map(|variable| (variable, property)))
                .flatten()
        });
        let (row_first, variable, property, normalized) = if let Some((variable, property)) =
            graph_left
        {
            let mut current = right;
            let mut depth = 0_usize;
            let mut row_right = false;
            let mut graph_in_right = false;
            let mut after_dot = false;
            let mut previous = None;
            loop {
                match current.kind {
                    TokenKind::End => break,
                    TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                    TokenKind::Punct(b')' | b']' | b'}') => {
                        let Some(next) = depth.checked_sub(1) else {
                            break;
                        };
                        depth = next;
                    }
                    TokenKind::Word(word) if !after_dot => {
                        if depth == 0 && comparison_boundary(word, previous) {
                            break;
                        }
                        if !matches!(lexer.clone().next()?.kind, TokenKind::Punct(b'(' | b':')) {
                            row_right |= imported(word);
                            graph_in_right |= graph(Name {
                                text: word,
                                at: current.at,
                            })
                            .is_some();
                        }
                    }
                    _ => {}
                }
                after_dot = current.kind == TokenKind::Punct(b'.');
                previous = Some(current.kind);
                current = lexer.next()?;
            }
            if !row_right || graph_in_right {
                return Ok(None);
            }
            (false, variable, property, reversed(comparison))
        } else if row_left && !graph_in_left {
            let Some((name, property, end)) = operand(right, &mut lexer)? else {
                return Ok(None);
            };
            if !matches!(
                end.kind,
                TokenKind::End | TokenKind::Word(_) | TokenKind::Punct(b')')
            ) {
                return Ok(None);
            }
            let Some(variable) = graph(name) else {
                return Ok(None);
            };
            (true, variable, property, comparison)
        } else {
            return Ok(None);
        };
        let at = self.current.at;
        self.capacity(
            self.predicates,
            MAX_PATTERN_PREDICATES,
            crate::algebra::PatternLimitDimension::Predicates,
        )?;
        if property.is_some() {
            self.require_property_variable(variable)?;
        }
        let row = if row_first {
            let row = self.computed_imported_operand()?;
            self.comparison()?;
            Some(row)
        } else {
            None
        };
        self.name()?;
        if property.is_some() {
            self.punct(b'.', ".")?;
            self.schema_name()?;
        }
        let row = if let Some(row) = row {
            row
        } else {
            self.comparison()?;
            self.computed_imported_operand()?
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

fn comparison_boundary(word: &str, previous: Option<TokenKind<'_>>) -> bool {
    ["AND", "OR", "XOR", "INTERSECT", "EXCEPT"]
        .iter()
        .any(|boundary| word.eq_ignore_ascii_case(boundary))
        || Parser::ends_where(word, previous)
}
