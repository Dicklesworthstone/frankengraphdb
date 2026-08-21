//! The first bounded GQL parser and binder slice.
//!
//! The accepted grammar is a typed edge pattern:
//! `MATCH (src)-[:Relation]->(dst) RETURN var` or
//! `MATCH (dst)<-[:Relation]-(src) RETURN var` or
//! `MATCH (left)-[:Relation]-(right) RETURN var`, plus the bounded node scan
//! `MATCH (node:Label) RETURN node`. Whitespace is optional between tokens.
//! The outgoing one-hop form may include `WHERE src <> dst` or
//! `WHERE src = dst` before `RETURN`. Unlabeled node-only scans and everything
//! else fail closed with a [`ParseError`]; this crate does not interpret a
//! partial AST or silently widen the supported language.

#![forbid(unsafe_code)]

use fgdb_delta_types::{LabelId, RelationId};
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
    UnknownLabel { name: String },
}

impl core::fmt::Display for BindError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BindError::Parse(error) => error.fmt(formatter),
            BindError::UnknownRelation { name } => {
                write!(formatter, "unknown relation {name:?}")
            }
            BindError::UnknownLabel { name } => write!(formatter, "unknown label {name:?}"),
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
    Undirected,
}

/// The executor-ready result of binding the pinned pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundPlan {
    pub relation: Option<RelationId>,
    pub src_var: String,
    pub dst_var: String,
    pub src_label: Option<LabelId>,
    pub dst_label: Option<LabelId>,
    pub via_var: String,
    pub hop2_relation: Option<RelationId>,
    pub hop2_dst_var: Option<String>,
    pub projection: ReturnProjection,
    pub direction: EdgeDirection,
    pub neq: Option<(String, String)>,
    /// `WHERE a = b` on the outgoing one-hop form, canonicalized like
    /// [`BoundPlan::neq`] (pair sorted); the parser guarantees at most one
    /// of `eq`/`neq` is `Some`.
    pub eq: Option<(String, String)>,
}

/// Deterministic relation-name binder for the supported GQL slice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelationBind {
    relations: BTreeMap<String, RelationId>,
    labels: BTreeMap<String, LabelId>,
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

    pub fn with_label(mut self, name: impl Into<String>, label: LabelId) -> Self {
        self.labels.insert(name.into(), label);
        self
    }

    /// Canonical certificate input for this relation-name binding.
    ///
    /// The transcript is big-endian and self-delimiting: entry count, then for
    /// each `(name, relation)` sorted by name and relation id, followed by the
    /// equivalently sorted label bindings. Names are length-prefixed and IDs
    /// are big-endian. Counts use `u64`, so the encoding never truncates an
    /// in-memory map or identifier length.
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
        let mut labels: Vec<_> = self.labels.iter().collect();
        labels.sort_by(|(left_name, left_label), (right_name, right_label)| {
            left_name
                .cmp(right_name)
                .then_with(|| left_label.0.cmp(&right_label.0))
        });
        bytes.extend_from_slice(&(labels.len() as u64).to_be_bytes());
        for (name, label) in labels {
            bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&label.0.to_be_bytes());
        }
        bytes
    }

    /// Parse and bind one statement without exposing the internal AST.
    pub fn bind(&self, statement: &str) -> Result<BoundPlan, BindError> {
        let ast = Parser::new(statement).parse()?;
        let relation = ast
            .relation
            .as_ref()
            .map(|name| {
                self.relations
                    .get(name)
                    .copied()
                    .ok_or_else(|| BindError::UnknownRelation { name: name.clone() })
            })
            .transpose()?;
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
        let src_label = bind_label(&self.labels, ast.src_label)?;
        let dst_label = bind_label(&self.labels, ast.dst_label)?;
        Ok(BoundPlan {
            relation,
            src_var: ast.src_var,
            dst_var: ast.dst_var,
            src_label,
            dst_label,
            via_var: ast.via_var,
            hop2_relation,
            hop2_dst_var: ast.hop2_dst_var,
            projection: ast.projection,
            direction: ast.direction,
            neq: ast.neq,
            eq: ast.eq,
        })
    }
}

fn bind_label(
    labels: &BTreeMap<String, LabelId>,
    name: Option<String>,
) -> Result<Option<LabelId>, BindError> {
    name.map(|name| {
        labels
            .get(&name)
            .copied()
            .ok_or(BindError::UnknownLabel { name })
    })
    .transpose()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MatchAst {
    src_var: String,
    relation: Option<String>,
    dst_var: String,
    src_label: Option<String>,
    dst_label: Option<String>,
    via_var: String,
    hop2_relation: Option<String>,
    hop2_dst_var: Option<String>,
    projection: ReturnProjection,
    direction: EdgeDirection,
    neq: Option<(String, String)>,
    eq: Option<(String, String)>,
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
        let left_label = self.optional_label()?;
        self.token(")")?;
        self.skip_whitespace();
        if left_label.is_some() && self.source[self.offset..].starts_with("RETURN") {
            self.keyword("RETURN")?;
            let returned = self.identifier()?;
            if returned != left_var {
                return Err(ParseError {
                    offset: self.offset.saturating_sub(returned.len()),
                    kind: ParseErrorKind::ReturnedVariableMismatch {
                        expected: left_var,
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
            return Ok(MatchAst {
                src_var: returned.clone(),
                relation: None,
                dst_var: returned.clone(),
                src_label: left_label,
                dst_label: None,
                via_var: returned,
                hop2_relation: None,
                hop2_dst_var: None,
                projection: ReturnProjection::Source,
                direction: EdgeDirection::Outgoing,
                neq: None,
                eq: None,
            });
        }
        let incoming = if self.source[self.offset..].starts_with('<') {
            self.token("<")?;
            self.token("-")?;
            true
        } else {
            self.token("-")?;
            false
        };
        self.token("[")?;
        self.token(":")?;
        let relation = self.identifier()?;
        self.token("]")?;
        self.token("-")?;
        self.skip_whitespace();
        let direction = if incoming {
            EdgeDirection::Incoming
        } else if self.source[self.offset..].starts_with('>') {
            self.token(">")?;
            EdgeDirection::Outgoing
        } else {
            EdgeDirection::Undirected
        };
        self.token("(")?;
        let right_var = self.identifier()?;
        let right_label = self.optional_label()?;
        self.token(")")?;
        self.skip_whitespace();
        let has_hop2 = match direction {
            EdgeDirection::Incoming => self.source[self.offset..].starts_with('<'),
            EdgeDirection::Outgoing | EdgeDirection::Undirected => {
                self.source[self.offset..].starts_with('-')
            }
        };
        let (hop2_relation, hop2_dst_var, hop2_label) = if has_hop2 {
            if direction == EdgeDirection::Incoming {
                self.token("<")?;
            }
            self.token("-")?;
            self.token("[")?;
            self.token(":")?;
            let relation = self.identifier()?;
            self.token("]")?;
            self.token("-")?;
            if direction == EdgeDirection::Outgoing {
                self.token(">")?;
            }
            self.token("(")?;
            let dst = self.identifier()?;
            let label = self.optional_label()?;
            self.token(")")?;
            (Some(relation), Some(dst), label)
        } else {
            (None, None, None)
        };
        if hop2_relation.is_some()
            && (left_label.is_some() || right_label.is_some() || hop2_label.is_some())
        {
            return Err(ParseError {
                offset: self.offset,
                kind: ParseErrorKind::ExpectedToken("unlabeled two-hop MATCH"),
            });
        }
        let (src_var, dst_var, src_label, dst_label) = match direction {
            EdgeDirection::Outgoing | EdgeDirection::Undirected => {
                (left_var, right_var, left_label, right_label)
            }
            EdgeDirection::Incoming if hop2_relation.is_some() => {
                (left_var, right_var, left_label, right_label)
            }
            EdgeDirection::Incoming => (right_var, left_var, right_label, left_label),
        };
        let via_var = dst_var.clone();
        self.skip_whitespace();
        let (neq, eq) = if direction == EdgeDirection::Outgoing
            && hop2_relation.is_none()
            && self.source[self.offset..].starts_with("WHERE")
        {
            self.keyword("WHERE")?;
            let left = self.identifier()?;
            self.skip_whitespace();
            // The operator decides which predicate slot fills; the parser
            // structure makes eq-and-neq-both-Some unrepresentable.
            let is_neq = self.source[self.offset..].starts_with('<');
            let operator = if is_neq {
                self.token("<")?;
                self.token(">")?;
                "<>"
            } else {
                self.token("=")?;
                "="
            };
            let right = self.identifier()?;
            let binds_endpoints = (left == src_var && right == dst_var)
                || (left == dst_var && right == src_var);
            if !binds_endpoints {
                return Err(ParseError {
                    offset: self.offset.saturating_sub(right.len()),
                    kind: ParseErrorKind::ReturnedVariableMismatch {
                        expected: format!("{src_var} {operator} {dst_var}"),
                        found: format!("{left} {operator} {right}"),
                    },
                });
            }
            let mut variables = (left, right);
            if variables.0 > variables.1 {
                variables = (variables.1, variables.0);
            }
            if is_neq {
                (Some(variables), None)
            } else {
                (None, Some(variables))
            }
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
            relation: Some(relation),
            dst_var,
            src_label,
            dst_label,
            via_var,
            hop2_relation,
            hop2_dst_var,
            projection,
            direction,
            neq,
            eq,
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

    fn optional_label(&mut self) -> Result<Option<String>, ParseError> {
        self.skip_whitespace();
        if !self.source[self.offset..].starts_with(':') {
            return Ok(None);
        }
        self.token(":")?;
        self.identifier().map(Some)
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
                relation: Some(RelationId(17)),
                src_var: "a".into(),
                dst_var: "b".into(),
                src_label: None,
                dst_label: None,
                via_var: "b".into(),
                hop2_relation: None,
                hop2_dst_var: None,
                projection: ReturnProjection::Destination,
                direction: EdgeDirection::Outgoing,
                neq: None,
                eq: None,
            }
        );
    }

    #[test]
    fn labeled_node_only_match_binds_without_a_relation() {
        let binder = RelationBind::new().with_label("Person", LabelId(7));
        let plan = binder
            .bind("MATCH (a:Person) RETURN a")
            .expect("labeled node-only MATCH binds");
        assert_eq!(plan.relation, None);
        assert_eq!(plan.src_var, "a");
        assert_eq!(plan.src_label, Some(LabelId(7)));
        assert_eq!(plan.projection, ReturnProjection::Source);

        assert!(matches!(
            binder.bind("MATCH (a) RETURN a"),
            Err(BindError::Parse(_))
        ));
        assert!(matches!(
            binder.bind("MATCH (a:Person) RETURN b"),
            Err(BindError::Parse(ParseError {
                kind: ParseErrorKind::ReturnedVariableMismatch { .. },
                ..
            }))
        ));

        let edge = RelationBind::new()
            .with_relation("R", RelationId(17))
            .bind("MATCH (a)-[:R]->(b) RETURN b")
            .expect("edge MATCH still binds");
        assert_eq!(edge.relation, Some(RelationId(17)));
    }

    #[test]
    fn one_hop_node_labels_bind_and_incoming_swaps_them() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_label("Person", LabelId(7));
        let outgoing = binder
            .bind("MATCH (a:Person)-[:R]->(b) RETURN b")
            .expect("labeled outgoing one-hop binds");
        assert_eq!(outgoing.src_label, Some(LabelId(7)));
        assert_eq!(outgoing.dst_label, None);

        let incoming = binder
            .bind("MATCH (a:Person)<-[:R]-(b) RETURN a")
            .expect("labeled incoming one-hop binds");
        assert_eq!(incoming.src_var, "b");
        assert_eq!(incoming.dst_var, "a");
        assert_eq!(incoming.src_label, None);
        assert_eq!(incoming.dst_label, Some(LabelId(7)));

        assert!(matches!(
            binder.bind("MATCH (a:Missing)-[:R]->(b) RETURN b"),
            Err(BindError::UnknownLabel { name }) if name == "Missing"
        ));
        assert!(matches!(
            binder.bind("MATCH (a:Person)-[:R]->(b)-[:S]->(c) RETURN c"),
            Err(BindError::Parse(_))
        ));
    }

    #[test]
    fn outgoing_one_hop_not_equal_predicate_binds_canonically() {
        let binder = RelationBind::new().with_relation("R", RelationId(17));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b <> a RETURN b")
            .expect("reversed endpoint inequality binds");
        assert_eq!(plan.neq, Some(("a".to_owned(), "b".to_owned())));

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE a <> c RETURN b"),
            Err(BindError::Parse(_))
        ));
    }

    #[test]
    fn where_equality_fills_eq_and_leaves_neq_empty() {
        let binder = RelationBind::new().with_relation("R", RelationId(17));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a = b RETURN b")
            .expect("endpoint equality binds");
        assert_eq!(plan.eq, Some(("a".to_owned(), "b".to_owned())));
        assert_eq!(plan.neq, None, "the two predicate slots are exclusive");

        let flipped = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b = a RETURN b")
            .expect("reversed endpoint equality binds");
        assert_eq!(
            flipped.eq,
            Some(("a".to_owned(), "b".to_owned())),
            "the pair canonicalizes exactly like neq"
        );

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE a = c RETURN b"),
            Err(BindError::Parse(_))
        ));
    }

    #[test]
    fn one_token_arrow_mutation_is_a_parse_error() {
        let binder = RelationBind::new().with_relation("R", RelationId(17));
        let plan = binder
            .bind("MATCH (a)-[:R]-(b) RETURN b")
            .expect("arrowless typed edge binds as undirected");
        assert_eq!(plan.direction, EdgeDirection::Undirected);

        let error = binder
            .bind("MATCH (a)<[:R]->(b) RETURN b")
            .expect_err("mixed arrow mutation must fail parsing");
        assert!(matches!(
            error,
            BindError::Parse(ParseError {
                kind: ParseErrorKind::ExpectedToken("-"),
                ..
            })
        ));
    }

    #[test]
    fn incoming_two_hop_binds_without_the_one_hop_swap() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23));
        let incoming = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c")
            .expect("incoming two-hop binds");
        assert_eq!(incoming.src_var, "a");
        assert_eq!(incoming.dst_var, "b");
        assert_eq!(incoming.hop2_dst_var.as_deref(), Some("c"));
        assert_eq!(incoming.hop2_relation, Some(RelationId(23)));
        assert_eq!(incoming.direction, EdgeDirection::Incoming);
        assert_eq!(incoming.projection, ReturnProjection::Hop2Destination);

        let one_hop = binder
            .bind("MATCH (a)<-[:R]-(b) RETURN a")
            .expect("incoming one-hop binds");
        assert_eq!((one_hop.src_var.as_str(), one_hop.dst_var.as_str()), ("b", "a"));

        let outgoing = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c")
            .expect("outgoing two-hop remains bound");
        assert_eq!(outgoing.direction, EdgeDirection::Outgoing);
        assert_eq!(outgoing.hop2_relation, Some(RelationId(23)));
        assert_eq!(outgoing.projection, ReturnProjection::Hop2Destination);

        assert!(matches!(
            binder.bind("MATCH (a)<[:R]->(b) RETURN b"),
            Err(BindError::Parse(_))
        ));
    }

    #[test]
    fn relation_bind_bytes_ignore_insertion_order() {
        let left = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_label("Person", LabelId(7))
            .with_label("Agent", LabelId(8));
        let right = RelationBind::new()
            .with_label("Agent", LabelId(8))
            .with_relation("S", RelationId(23))
            .with_relation("R", RelationId(17))
            .with_label("Person", LabelId(7));

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
