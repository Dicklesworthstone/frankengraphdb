//! Typed controls for the existing explicit native transaction owner.
//! This parser admits syntax only; it neither owns nor executes a transaction.

use super::{
    GraphPatternTextError, GraphPatternTextErrorKind, Lexer, MAX_GRAPH_TEXT_BYTES,
    MAX_PATTERN_NAME_BYTES, Token, TokenKind, error,
};

/// A savepoint operation on one active native transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphSavepointOperation {
    Save,
    RollbackTo,
    Release,
}

/// A bounded, case-sensitive savepoint identifier and its native operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedGraphTransactionControl {
    operation: GraphSavepointOperation,
    name: String,
}

impl PreparedGraphTransactionControl {
    /// Construct a control without parsing text. Names use the same bound as
    /// graph identifiers. Backticks and control characters are not identifiers
    /// in this profile and cannot be interpolated into another statement.
    pub fn new(
        operation: GraphSavepointOperation,
        name: &str,
    ) -> Result<Self, GraphPatternTextError> {
        if name.len() > MAX_PATTERN_NAME_BYTES {
            return Err(error(0, GraphPatternTextErrorKind::NameTooLong));
        }
        if name.is_empty() || name.contains('`') || name.chars().any(char::is_control) {
            return Err(error(
                0,
                GraphPatternTextErrorKind::Expected("a nonempty savepoint identifier"),
            ));
        }
        Ok(Self {
            operation,
            name: name.to_owned(),
        })
    }

    #[must_use]
    pub const fn operation(&self) -> GraphSavepointOperation {
        self.operation
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Canonical statement text for the existing Write request envelope.
    #[must_use]
    pub fn statement(&self) -> String {
        let command = match self.operation {
            GraphSavepointOperation::Save => "SAVEPOINT",
            GraphSavepointOperation::RollbackTo => "ROLLBACK TO SAVEPOINT",
            GraphSavepointOperation::Release => "RELEASE SAVEPOINT",
        };
        format!("{command} `{}`", self.name)
    }

    /// Recognize SAVEPOINT name, ROLLBACK TO [SAVEPOINT] name and
    /// RELEASE [SAVEPOINT] name with the shared bounded graph-text lexer.
    /// Ordinary graph statements return None. Once a control keyword is
    /// recognized, malformed or trailing syntax is a refusal, never fallback.
    pub fn prepare(text: &str) -> Result<Option<Self>, GraphPatternTextError> {
        if text.len() > MAX_GRAPH_TEXT_BYTES {
            return Err(error(0, GraphPatternTextErrorKind::DefinitionTooLarge));
        }
        let mut lexer = Lexer {
            text,
            at: 0,
            tokens: 0,
        };
        let first = lexer.next()?;
        let operation = if keyword(text, first, "SAVEPOINT") {
            GraphSavepointOperation::Save
        } else if keyword(text, first, "ROLLBACK") {
            let to = lexer.next()?;
            if !keyword(text, to, "TO") {
                return Err(error(to.at, GraphPatternTextErrorKind::Expected("TO")));
            }
            GraphSavepointOperation::RollbackTo
        } else if keyword(text, first, "RELEASE") {
            GraphSavepointOperation::Release
        } else {
            return Ok(None);
        };
        let mut name = lexer.next()?;
        if operation != GraphSavepointOperation::Save && keyword(text, name, "SAVEPOINT") {
            name = lexer.next()?;
        }
        let identifier = match name.kind {
            TokenKind::Word(name) | TokenKind::DelimitedKeyword(name) => name,
            _ => {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("a savepoint identifier"),
                ));
            }
        };
        let prepared = Self::new(operation, identifier).map_err(|mut error| {
            error.offset = name.at;
            error
        })?;
        // Semicolons belong to statement framing, not the graph lexer. Permit
        // exactly one terminal delimiter, after any ordinary comments/trivia.
        lexer.skip_trivia()?;
        if text.as_bytes().get(lexer.at) == Some(&b';') {
            lexer.at += 1;
        }
        let end = lexer.next()?;
        if end.kind != TokenKind::End {
            return Err(error(
                end.at,
                GraphPatternTextErrorKind::Expected("the end of one transaction control"),
            ));
        }
        Ok(Some(prepared))
    }
}

fn keyword(text: &str, token: Token<'_>, expected: &str) -> bool {
    text.as_bytes().get(token.at) != Some(&b'`')
        && matches!(token.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case(expected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_share_identifier_trivia_and_exact_name_semantics() {
        for (text, operation, name) in [
            ("savepoint kept", GraphSavepointOperation::Save, "kept"),
            (
                "/* lead */ SAVEPOINT `雪 space`; // tail",
                GraphSavepointOperation::Save,
                "雪 space",
            ),
            (
                "ROLLBACK /* gap */ TO kept",
                GraphSavepointOperation::RollbackTo,
                "kept",
            ),
            (
                "rollback to savepoint `SAVEPOINT`",
                GraphSavepointOperation::RollbackTo,
                "SAVEPOINT",
            ),
            ("RELEASE kept;", GraphSavepointOperation::Release, "kept"),
            (
                "RELEASE SAVEPOINT `match`",
                GraphSavepointOperation::Release,
                "match",
            ),
        ] {
            let prepared = PreparedGraphTransactionControl::prepare(text)
                .unwrap()
                .unwrap();
            assert_eq!(prepared.operation(), operation);
            assert_eq!(prepared.name(), name);
            assert_eq!(
                PreparedGraphTransactionControl::prepare(&prepared.statement()).unwrap(),
                Some(prepared)
            );
        }
        for text in [
            "MATCH (n) RETURN n",
            "CREATE (n)",
            "RETURN 1",
            "`SAVEPOINT` kept",
        ] {
            assert!(
                PreparedGraphTransactionControl::prepare(text)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn controls_refuse_malformed_ambiguous_or_multiple_statements() {
        for text in [
            "SAVEPOINT",
            "SAVEPOINT $name",
            "SAVEPOINT 1",
            "SAVEPOINT ''",
            "SAVEPOINT ``",
            "SAVEPOINT `a``b`",
            "SAVEPOINT kept extra",
            "SAVEPOINT kept; CREATE (n)",
            "SAVEPOINT kept;;",
            "ROLLBACK kept",
            "ROLLBACK `TO` kept",
            "ROLLBACK TO SAVEPOINT",
            "RELEASE",
            "RELEASE SAVEPOINT",
            "RELEASE kept RETURN 1",
        ] {
            assert!(
                PreparedGraphTransactionControl::prepare(text).is_err(),
                "{text}"
            );
        }
        for name in ["", "a`b", "line\nfeed", "nul\0byte"] {
            assert!(
                PreparedGraphTransactionControl::new(GraphSavepointOperation::Save, name).is_err()
            );
        }
        let bounded = "é".repeat(MAX_PATTERN_NAME_BYTES / 2);
        let control =
            PreparedGraphTransactionControl::new(GraphSavepointOperation::Save, &bounded).unwrap();
        assert_eq!(
            PreparedGraphTransactionControl::prepare(&control.statement()).unwrap(),
            Some(control)
        );
        assert!(
            PreparedGraphTransactionControl::new(GraphSavepointOperation::Save, &(bounded + "x"))
                .is_err()
        );
    }
}
