//! The first bounded GQL parser and binder slice.
//!
//! The only accepted grammar is a single directed, typed edge pattern:
//! `MATCH (src)-[:Relation]->(dst) RETURN var` or
//! `MATCH (dst)<-[:Relation]-(src) RETURN var`. Whitespace is optional between
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReturnProjection {
    Source,
    Destination,
    Hop2Destination,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeDirection {
    Outgoing,
    Incoming,
}

/// The executor-ready result of binding the pinned pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundPlan {
    pub relation: RelationId,
    pub src_var: String,
    pub dst_var: String,
    pub via_var: String,
    pub hop2_relation: Option<RelationId>,
    pub hop2_dst_var: Option<String>,
    pub projection: ReturnProjection,
    pub direction: EdgeDirection,
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

    /// Canonical certificate input for this relation-name binding.
    ///
    /// The transcript is big-endian and self-delimiting: entry count, then for
    /// each `(name, relation)` sorted by name and relation id, the UTF-8 name
    /// length, name bytes, and the `RelationId` value. Counts use `u64`, so the
    /// encoding never truncates an in-memory map or identifier length.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut entries: Vec<_> = self.relations.iter().collect();
        entries.sort_by(|(left_name, left_relation), (right_name, right_relation)| {
            left_name
                .cmp(right_name)
                .then_with(|| left_relation.0.cmp(&right_relation.0))
        });

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(entries.len() as u64).to_be_bytes());
        for (name, relation) in entries {
            bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&relation.0.to_be_bytes());
        }
        bytes
    }

    /// Parse and bind one statement without exposing the internal AST.
    pub fn bind(&self, statement: &str) -> Result<BoundPlan, BindError> {
        let ast = Parser::new(statement).parse()?;
        let relation = self.relations.get(&ast.relation).copied().ok_or_else(|| {
            BindError::UnknownRelation {
                name: ast.relation.clone(),
            }
        })?;
        let hop2_relation = ast
            .hop2_relation
            .as_ref()
            .map(|name| {
                self.relations
                    .get(name)
                    .copied()
                    .ok_or_else(|| BindError::UnknownRelation { name: name.clone() })
            })
            .transpose()?;
        Ok(BoundPlan {
            relation,
            src_var: ast.src_var,
            dst_var: ast.dst_var,
            via_var: ast.via_var,
            hop2_relation,
            hop2_dst_var: ast.hop2_dst_var,
            projection: ast.projection,
            direction: ast.direction,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MatchAst {
    src_var: String,
    relation: String,
    dst_var: String,
    via_var: String,
    hop2_relation: Option<String>,
    hop2_dst_var: Option<String>,
    projection: ReturnProjection,
    direction: EdgeDirection,
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
        let left_var = self.identifier()?;
        self.token(")")?;
        self.skip_whitespace();
        let direction = if self.source[self.offset..].starts_with('<') {
            self.token("<")?;
            self.token("-")?;
            EdgeDirection::Incoming
        } else {
            self.token("-")?;
            EdgeDirection::Outgoing
        };
        self.token("[")?;
        self.token(":")?;
        let relation = self.identifier()?;
        self.token("]")?;
        self.token("-")?;
        if direction == EdgeDirection::Outgoing {
            self.token(">")?;
        }
        self.token("(")?;
        let right_var = self.identifier()?;
        self.token(")")?;
        let (src_var, dst_var) = match direction {
            EdgeDirection::Outgoing => (left_var, right_var),
            EdgeDirection::Incoming => (right_var, left_var),
        };
        let via_var = dst_var.clone();
        self.skip_whitespace();
        let (hop2_relation, hop2_dst_var) = if direction == EdgeDirection::Outgoing
            && self.source[self.offset..].starts_with('-')
        {
            self.token("-")?;
            self.token("[")?;
            self.token(":")?;
            let relation = self.identifier()?;
            self.token("]")?;
            self.token("-")?;
            self.token(">")?;
            self.token("(")?;
            let dst = self.identifier()?;
            self.token(")")?;
            (Some(relation), Some(dst))
        } else {
            (None, None)
        };
        self.keyword("RETURN")?;
        let returned = self.identifier()?;
        let projection = if hop2_dst_var.as_ref() == Some(&returned) {
            ReturnProjection::Hop2Destination
        } else if returned == dst_var {
            ReturnProjection::Destination
        } else if returned == src_var {
            ReturnProjection::Source
        } else {
            return Err(ParseError {
                offset: self.offset.saturating_sub(returned.len()),
                kind: ParseErrorKind::ReturnedVariableMismatch {
                    expected: dst_var.clone(),
                    found: returned,
                },
            });
        };
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
            dst_var,
            via_var,
            hop2_relation,
            hop2_dst_var,
            projection,
            direction,
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
                via_var: "b".into(),
                hop2_relation: None,
                hop2_dst_var: None,
                projection: ReturnProjection::Destination,
                direction: EdgeDirection::Outgoing,
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

    #[test]
    fn relation_bind_bytes_ignore_insertion_order() {
        let left = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23));
        let right = RelationBind::new()
            .with_relation("S", RelationId(23))
            .with_relation("R", RelationId(17));

        assert_eq!(left.canonical_bytes(), right.canonical_bytes());
    }

    #[test]
    fn different_relation_maps_have_different_bytes() {
        let left = RelationBind::new().with_relation("R", RelationId(17));
        let different_id = RelationBind::new().with_relation("R", RelationId(18));
        let different_name = RelationBind::new().with_relation("S", RelationId(17));

        assert_ne!(left.canonical_bytes(), different_id.canonical_bytes());
        assert_ne!(left.canonical_bytes(), different_name.canonical_bytes());
    }
}
