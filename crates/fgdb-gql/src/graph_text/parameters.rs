//! Explicit argument declarations for the shared graph-text parser.
//! Nonnumeric types come from the caller's prepared-statement contract, never
//! from a database sample, parameter spelling, value, or text substitution.

use super::*;

impl PreparedGraphText {
    /// Prepare with explicit types for selected argument names (without `$`).
    /// Undeclared property arguments keep the existing Int64 interpretation;
    /// SKIP/LIMIT remain UInt64. A declared Scalar(kind) property argument
    /// accepts that exact canonical kind or canonical null. All declarations
    /// must be unique, bounded, and used in the statement. Invalid or conflicting
    /// declarations are refused before catalog resolution or storage access.
    ///
    /// This is the same parser/compiler as prepare(), including scoped MATCH.
    /// Binding uses the existing GqlParameters map, which can mix numeric and
    /// canonical scalar arguments without copying scalar payloads per use.
    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphPatternTextError> {
        let syntax = Parser::new_with_parameter_types(statement, declarations)?.parse()?;
        Self::from_syntax(statement, syntax, resolve)
    }

    /// Give the relational composition parser a view of the SAME lexical
    /// tokens. Admission is for the complete statement, including every arm,
    /// grouping delimiter and final page, before splitting any leaf range.
    pub(crate) fn composition_tokens<'a>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
    ) -> Result<Vec<crate::set_text::TextToken<'a>>, GraphPatternTextError> {
        use crate::set_text::{TextKind, TextToken};
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        let mut tokens = Vec::new();
        loop {
            let token = parser.current;
            let kind = match token.kind {
                TokenKind::Word(word) => TextKind::Word(word),
                TokenKind::Digits(digits) => TextKind::Digits(digits),
                TokenKind::Parameter(name) => TextKind::Parameter(name),
                TokenKind::Quoted(_) => TextKind::Quoted,
                TokenKind::Punct(ch) => TextKind::Punct(ch),
                TokenKind::End => TextKind::End,
            };
            tokens.push(TextToken { kind, at: token.at });
            if matches!(token.kind, TokenKind::End) {
                break;
            }
            parser.advance()?;
        }
        Ok(tokens)
    }

    /// Parse once and retain syntax until ALL compound operands and their
    /// shared parameter contract have passed. Resolution cannot begin early.
    pub(crate) fn unresolved_for_composition<'a>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
    ) -> Result<UnresolvedGraphText<'a>, GraphPatternTextError> {
        let syntax = Parser::new_with_parameter_types(statement, declarations)?.parse()?;
        Ok(UnresolvedGraphText { statement, syntax })
    }
}

/// Preparation-only phase object. Its native syntax never enters execution.
/// The private module owns it; composition can inspect schema and then consume
/// it through the original graph-text catalog/compiler boundary.
pub(crate) struct UnresolvedGraphText<'a> {
    statement: &'a str,
    syntax: Syntax<'a>,
}
impl UnresolvedGraphText<'_> {
    pub(crate) fn column_schema(&self) -> (Vec<String>, Vec<crate::GraphSetColumnType>) {
        self.syntax
            .columns
            .iter()
            .map(|column| {
                (
                    column.alias.text.to_owned(),
                    if column.property.is_some() {
                        crate::GraphSetColumnType::Scalar
                    } else {
                        crate::GraphSetColumnType::Vertex
                    },
                )
            })
            .unzip()
    }
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.syntax.parameters
    }
    pub(crate) fn parameter_offsets(&self) -> &[usize] {
        &self.syntax.parameter_offsets
    }
    pub(crate) fn resolve(
        self,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<PreparedGraphText, GraphPatternTextError> {
        PreparedGraphText::from_syntax(self.statement, self.syntax, resolve)
    }
}

impl<'a> Parser<'a> {
    pub(super) fn new_with_parameter_types(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
    ) -> Result<Self, GraphPatternTextError> {
        let mut parser = Self::new(statement)?;
        if declarations.len() > MAX_GRAPH_TEXT_TOKENS {
            return Err(error(0, GraphPatternTextErrorKind::ParameterDeclaration));
        }
        let mut bytes = 0_usize;
        for &(name, kind) in declarations {
            let raw = name.as_bytes();
            if raw.is_empty()
                || raw.len() > MAX_PATTERN_NAME_BYTES
                || !(raw[0].is_ascii_alphabetic() || raw[0] == b'_')
                || !raw
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                || parser.parameter_types.contains_key(name)
            {
                return Err(error(0, GraphPatternTextErrorKind::ParameterDeclaration));
            }
            bytes = bytes
                .checked_add(raw.len())
                .ok_or_else(|| error(0, GraphPatternTextErrorKind::ParameterDeclaration))?;
            if bytes > MAX_GRAPH_TEXT_BYTES {
                return Err(error(0, GraphPatternTextErrorKind::ParameterDeclaration));
            }
            parser.parameter_types.insert(name.to_owned(), kind);
        }
        Ok(parser)
    }

    pub(super) fn check_declared_parameter(
        &self,
        name: &str,
        required: GqlParameterType,
        at: usize,
    ) -> Result<(), GraphPatternTextError> {
        if self
            .parameter_types
            .get(name)
            .is_some_and(|declared| *declared != required)
        {
            return Err(error(
                at,
                GraphPatternTextErrorKind::ConflictingParameterTypes,
            ));
        }
        Ok(())
    }

    pub(super) fn check_parameter_declarations(&self) -> Result<(), GraphPatternTextError> {
        if self.parameter_types.keys().any(|name| {
            !self
                .syntax
                .parameters
                .iter()
                .any(|spec| spec.name.as_str() == name.as_str())
        }) {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::UnusedParameterDeclaration,
            ));
        }
        Ok(())
    }
}
