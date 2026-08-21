//! The first bounded GQL parser and binder slice.
//!
//! The only accepted grammar is a single directed, typed edge pattern:
//! `MATCH (src)-[:Relation]->(dst) RETURN dst`. Whitespace is optional between
//! tokens. Everything else fails closed with a [`ParseError`]; this crate does
//! not interpret a partial AST or silently widen the supported language.

#![forbid(unsafe_code)]

use fgdb_delta_types::RelationId;
use std::collections::BTreeMap;

/// A syntax error in the bounded GQL grammar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    /// Byte offset at which the parser could no longer match the grammar.
    pub offset: usize,
    pub kind: ParseErrorKind,
}

/// Closed failure vocabulary for the bounded recursive-descent parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseErrorKind {
    ExpectedKeyword(&'static str),
    ExpectedToken(&'static str),
    ExpectedIdentifier,
    ReturnedVariableMismatch { expected: String, found: String },
    TrailingInput,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            formatter,
            "GQL parse error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}

impl core::error::Error for ParseError {}

/// A relation name was not registered, or the source did not parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindError {
    Parse(ParseError),
    UnknownRelation { name: String },
}

impl core::fmt::Display for BindError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BindError::Parse(error) => error.fmt(formatter),
            BindError::UnknownRelation { name } => {
                write!(formatter, "unknown relation {name:?}")
            }
        }
    }
}

impl core::error::Error for BindError {}

impl From<ParseError> for BindError {
    fn from(error: ParseError) -> Self {
        BindError::Parse(error)
    }
}

/// The executor-ready result of binding the pinned pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundPlan {
    pub relation: RelationId,
    pub src_var: String,
    pub dst_var: String,
}

/// Deterministic relation-name binder for the supported GQL slice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelationBind {
    relations: BTreeMap<String, RelationId>,
}

impl RelationBind {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, relation: RelationId) -> Option<RelationId> {
        self.relations.insert(name.into(), relation)
    }

    pub fn with_relation(mut self, name: impl Into<String>, relation: RelationId) -> Self {
        self.insert(name, relation);
        self
    }

    /// Parse and bind one statement without exposing the internal AST.
    pub fn bind(&self, statement: &str) -> Result<BoundPlan, BindError> {
        let ast = Parser::new(statement).parse()?;
        let relation = self.relations.get(&ast.relation).copied().ok_or_else(|| {
            BindError::UnknownRelation {
                name: ast.relation.clone(),
            }
        })?;
        Ok(BoundPlan {
            relation,
            src_var: ast.src_var,
            dst_var: ast.dst_var,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MatchAst {
    src_var: String,
    relation: String,
    dst_var: String,
}

struct Parser<'a> {
    source: &'a str,
    offset: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str) -> Self {
        Self { source, offset: 0 }
    }

    fn parse(mut self) -> Result<MatchAst, ParseError> {
        self.keyword("MATCH")?;
        self.token("(")?;
        let src_var = self.identifier()?;
        self.token(")")?;
        self.token("-")?;
        self.token("[")?;
        self.token(":")?;
        let relation = self.identifier()?;
        self.token("]")?;
        self.token("-")?;
        self.token(">")?;
        self.token("(")?;
        let dst_var = self.identifier()?;
        self.token(")")?;
        self.keyword("RETURN")?;
        let returned = self.identifier()?;
        if returned != dst_var {
            return Err(ParseError {
                offset: self.offset.saturating_sub(returned.len()),
                kind: ParseErrorKind::ReturnedVariableMismatch {
                    expected: dst_var,
                    found: returned,
                },
            });
        }
        self.skip_whitespace();
        if self.offset != self.source.len() {
            return Err(ParseError {
                offset: self.offset,
                kind: ParseErrorKind::TrailingInput,
            });
        }
        Ok(MatchAst {
            src_var,
            relation,
            dst_var: returned,
        })
    }

    fn keyword(&mut self, keyword: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        let remaining = &self.source[self.offset..];
        if !remaining.starts_with(keyword)
            || remaining[keyword.len()..]
                .chars()
                .next()
                .is_some_and(is_identifier_continue)
        {
            return Err(ParseError {
                offset: self.offset,
                kind: ParseErrorKind::ExpectedKeyword(keyword),
            });
        }
        self.offset += keyword.len();
        Ok(())
    }

    fn token(&mut self, token: &'static str) -> Result<(), ParseError> {
        self.skip_whitespace();
        if !self.source[self.offset..].starts_with(token) {
            return Err(ParseError {
                offset: self.offset,
                kind: ParseErrorKind::ExpectedToken(token),
            });
        }
        self.offset += token.len();
        Ok(())
    }

    fn identifier(&mut self) -> Result<String, ParseError> {
        self.skip_whitespace();
        let start = self.offset;
        let mut chars = self.source[start..].char_indices();
        let Some((_, first)) = chars.next() else {
            return Err(ParseError {
                offset: start,
                kind: ParseErrorKind::ExpectedIdentifier,
            });
        };
        if !is_identifier_start(first) {
            return Err(ParseError {
                offset: start,
                kind: ParseErrorKind::ExpectedIdentifier,
            });
        }
        self.offset = start + first.len_utf8();
        for (relative, character) in chars {
            if !is_identifier_continue(character) {
                break;
            }
            self.offset = start + relative + character.len_utf8();
        }
        Ok(self.source[start..self.offset].to_owned())
    }

    fn skip_whitespace(&mut self) {
        while let Some(character) = self.source[self.offset..].chars().next() {
            if !character.is_whitespace() {
                break;
            }
            self.offset += character.len_utf8();
        }
    }
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_ascii_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || character.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_match_statement_binds() {
        let binder = RelationBind::new().with_relation("R", RelationId(17));
        let plan = binder
            .bind("  MATCH ( a ) - [ : R ] - > ( b ) RETURN b  ")
            .expect("pinned directed-edge statement binds");
        assert_eq!(
            plan,
            BoundPlan {
                relation: RelationId(17),
                src_var: "a".into(),
                dst_var: "b".into(),
            }
        );
    }

    #[test]
    fn one_token_arrow_mutation_is_a_parse_error() {
        let binder = RelationBind::new().with_relation("R", RelationId(17));
        let error = binder
            .bind("MATCH (a)-[:R]-(b) RETURN b")
            .expect_err("missing directed-arrow token must fail parsing");
        assert!(matches!(
            error,
            BindError::Parse(ParseError {
                kind: ParseErrorKind::ExpectedToken(">"),
                ..
            })
        ));
    }
}
