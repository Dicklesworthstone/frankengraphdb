//! Imported map selectors become ordinary native projection expressions.
//! This module only recognizes join keys; evaluation stays in MapGet and the
//! existing three-valued row predicate kernel.

use super::*;
use crate::algebra::GraphValue;
use crate::set_text::ReadValueTemplate;

struct FieldPath<'a> {
    name: Name<'a>,
    keys: Vec<Name<'a>>,
}

/// Look ahead through one static dotted name, without advancing the parser or
/// admitting any syntax. A bounded expression may be inspected twice, never
/// copied into generated query text or interpreted during binding.
fn field_path<'a>(
    current: &mut Token<'a>,
    lexer: &mut Lexer<'a>,
) -> Result<Option<FieldPath<'a>>, GraphPatternTextError> {
    let TokenKind::Word(text) = current.kind else {
        return Ok(None);
    };
    let name = Name {
        text,
        at: current.at,
    };
    *current = lexer.next()?;
    let mut keys = Vec::new();
    while matches!(current.kind, TokenKind::Punct(b'.')) {
        if keys.len() == GraphValue::MAX_LIST_DEPTH {
            return Err(error(
                current.at,
                GraphPatternTextErrorKind::Expected("bounded imported row field path"),
            ));
        }
        *current = lexer.next()?;
        let (TokenKind::Word(text) | TokenKind::DelimitedKeyword(text)) = current.kind else {
            return Ok(None);
        };
        keys.push(Name {
            text,
            at: current.at,
        });
        *current = lexer.next()?;
    }
    Ok(Some(FieldPath { name, keys }))
}

impl<'a> Parser<'a> {
    /// Consume an imported binding and any static map selectors or integer
    /// subscripts (`row.key`, LDBC SNB IU1's `s[0]`). A bare binding keeps
    /// its original ordinal and template bytes. Computed keys occupy a
    /// private suffix of the leading row until the join completes.
    pub(super) fn imported_row_field(&mut self) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(name) = self.current.kind else {
            return Ok(None);
        };
        let Some(row) = self
            .read_row_bindings
            .iter()
            .position(|binding| binding.text == name)
        else {
            return Ok(None);
        };
        self.advance()?;
        if !self.is_punct(b'.') && !self.is_punct(b'[') {
            return Ok(Some(row));
        }
        self.capacity(
            self.read_row_bindings.len() + self.read_row_fields.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let mut value = ReadValueTemplate::Column(row);
        let mut depth = 0;
        while self.is_punct(b'.') || self.is_punct(b'[') {
            if depth == GraphValue::MAX_LIST_DEPTH {
                return Err(error(
                    self.current.at,
                    GraphPatternTextErrorKind::Expected("bounded imported row field path"),
                ));
            }
            value = if self.take(b'[')? {
                let at = self.current.at;
                let Number::Literal(GqlParameterValue::Int64(index)) =
                    self.number(GqlParameterType::Int64)?
                else {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected(
                            "an integer literal subscript of an imported value",
                        ),
                    ));
                };
                self.punct(b']', "]")?;
                let index = crate::GqlScalarParameter::new(fgdb_types::CanonicalScalar::Int(index))
                    .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
                ReadValueTemplate::Index {
                    list: Box::new(value),
                    index: Box::new(ReadValueTemplate::Literal(index)),
                }
            } else {
                self.advance()?;
                let key = self.schema_name()?;
                self.map_get(value, key.text)
            };
            depth += 1;
        }
        let column = self.read_row_bindings.len() + self.read_row_fields.len();
        self.read_row_fields.push(value);
        Ok(Some(column))
    }

    /// Whether the value ahead computes from the leading row, as in
    /// `toInteger(row.s)` or `row.a + 1` (fgdb-2npty). It must read a row
    /// binding, and call a function or apply an operator outside brackets.
    /// A bare path or subscript (`row.key`, `s[0]`) stays imported_row_field's.
    /// The value ends at `,`, `}`, `)` or `]` outside brackets, or at AND, OR,
    /// XOR or a clause keyword. Lookahead only: nothing is admitted.
    pub(super) fn row_value_is_computed(&self) -> Result<bool, GraphPatternTextError> {
        self.row_value_computes_from(self.current, self.lexer.clone())
    }

    /// [`Self::row_value_is_computed`] for a value starting at `token`.
    pub(super) fn row_value_computes_from(
        &self,
        mut token: Token<'a>,
        mut lexer: Lexer<'a>,
    ) -> Result<bool, GraphPatternTextError> {
        if self.read_row_bindings.is_empty() {
            return Ok(false);
        }
        let mut previous = None;
        let mut depth = 0_usize;
        let (mut reads_row, mut computes) = (false, false);
        loop {
            match token.kind {
                TokenKind::End => break,
                TokenKind::Punct(b',' | b'}' | b')' | b']') if depth == 0 => break,
                TokenKind::Punct(b')' | b']' | b'}') => depth -= 1,
                TokenKind::Punct(b'(') => {
                    computes |= depth == 0;
                    depth += 1;
                }
                TokenKind::Punct(b'[' | b'{') => depth += 1,
                TokenKind::Punct(b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'|') => {
                    computes |= depth == 0;
                }
                TokenKind::Word(word)
                    if depth == 0
                        && (["AND", "OR", "XOR"]
                            .iter()
                            .any(|keyword| word.eq_ignore_ascii_case(keyword))
                            || Self::ends_where(word, previous)) =>
                {
                    break;
                }
                TokenKind::Word(word)
                    if !matches!(previous, Some(TokenKind::Punct(b'.')))
                        && self
                            .read_row_bindings
                            .iter()
                            .any(|binding| binding.text == word) =>
                {
                    reads_row = true;
                }
                _ => {}
            }
            previous = Some(token.kind);
            token = lexer.next()?;
        }
        Ok(reads_row && computes)
    }

    /// A dotted imported value on one side and a root graph property/identity
    /// on the other, joined by one complete equality atom. The Boolean owner
    /// still enforces top-level AND and refuses OR/NOT scope changes.
    #[allow(clippy::type_complexity)]
    pub(super) fn row_field_correlation(
        &mut self,
    ) -> Result<Option<(Name<'a>, Option<Name<'a>>, usize, usize)>, GraphPatternTextError> {
        if self.read_row_bindings.is_empty() {
            return Ok(None);
        }
        let at = self.current.at;
        let mut lexer = self.lexer.clone();
        let mut current = self.current;
        let Some(left) = field_path(&mut current, &mut lexer)? else {
            return Ok(None);
        };
        if !matches!(current.kind, TokenKind::Punct(b'=')) {
            return Ok(None);
        }
        current = lexer.next()?;
        let Some(right) = field_path(&mut current, &mut lexer)? else {
            return Ok(None);
        };
        if !matches!(
            current.kind,
            TokenKind::End | TokenKind::Word(_) | TokenKind::Punct(b')')
        ) {
            return Ok(None);
        }
        let imported = |path: &FieldPath<'_>| {
            !path.keys.is_empty()
                && self
                    .read_row_bindings
                    .iter()
                    .any(|binding| binding.text == path.name.text)
        };
        let graph = |path: &FieldPath<'_>| {
            path.keys.len() <= 1
                && !self
                    .read_row_bindings
                    .iter()
                    .any(|binding| binding.text == path.name.text)
                && (self.syntax.variables[..self.syntax.root_variables]
                    .iter()
                    .any(|variable| variable.text == path.name.text)
                    || self
                        .syntax
                        .visible_edge(path.name.text)
                        .is_some_and(|(edge, scoped)| !scoped && edge.walk.is_none()))
        };
        let property_first = if graph(&left) && imported(&right) {
            true
        } else if imported(&left) && graph(&right) {
            false
        } else {
            return Ok(None);
        };
        self.capacity(
            self.predicates,
            MAX_PATTERN_PREDICATES,
            crate::algebra::PatternLimitDimension::Predicates,
        )?;
        let source = if property_first { &left } else { &right };
        let row = if property_first {
            None
        } else {
            let row = self
                .imported_row_field()?
                .expect("lookahead proved an imported field");
            self.punct(b'=', "=")?;
            Some(row)
        };
        // Consume through the ordinary identifier/domain validators. A
        // successful lookahead cannot bypass graph property name limits or
        // make a reserved delimited map key a graph-property identifier.
        let variable = self.property_variable()?;
        let key = if source.keys.is_empty() {
            None
        } else {
            self.punct(b'.', ".")?;
            Some(self.name()?)
        };
        let row = if let Some(row) = row {
            row
        } else {
            self.punct(b'=', "=")?;
            self.imported_row_field()?
                .expect("lookahead proved an imported field")
        };
        self.predicates += 1;
        Ok(Some((variable, key, row, at)))
    }
}
