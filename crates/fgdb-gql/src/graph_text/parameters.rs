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
