//! The first bounded GQL parser and binder slice.
//!
//! The accepted grammar is a typed edge pattern:
//! `MATCH (src)-[:Relation]->(dst) RETURN var` or
//! `MATCH (dst)<-[:Relation]-(src) RETURN var` or
//! `MATCH (left)-[:Relation]-(right) RETURN var`, plus the bounded node scan
//! `MATCH (node:Label) RETURN node`. Whitespace is optional between tokens.
//! The outgoing one-hop form may include endpoint equality/inequality or
//! integer property equality, including one source and one destination
//! property predicate joined by `AND`, before `RETURN`. Unlabeled node-only
//! scans and everything else fail closed with a [`ParseError`]; this crate does
//! not interpret a partial AST or silently widen the supported language.

#![forbid(unsafe_code)]

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
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

/// A relation, label, or property name was not registered, or parsing failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindError {
    Parse(ParseError),
    UnknownRelation { name: String },
    UnknownLabel { name: String },
    UnknownProperty { name: String },
}

impl core::fmt::Display for BindError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BindError::Parse(error) => error.fmt(formatter),
            BindError::UnknownRelation { name } => {
                write!(formatter, "unknown relation {name:?}")
            }
            BindError::UnknownLabel { name } => write!(formatter, "unknown label {name:?}"),
            BindError::UnknownProperty { name } => {
                write!(formatter, "unknown property {name:?}")
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
    /// Source-property integer equality on an outgoing one-hop or labeled
    /// node-only form.
    pub src_prop: Option<(PropertyKeyId, i64)>,
    /// Source-property integer inequality using the GQL `<>` spelling.
    pub src_prop_ne: Option<(PropertyKeyId, i64)>,
    /// Source-property strict integer greater-than predicate.
    pub src_prop_gt: Option<(PropertyKeyId, i64)>,
    /// Source-property strict integer less-than predicate.
    pub src_prop_lt: Option<(PropertyKeyId, i64)>,
    /// Source-property inclusive integer greater-than predicate.
    pub src_prop_ge: Option<(PropertyKeyId, i64)>,
    /// Source-property inclusive integer less-than predicate.
    pub src_prop_le: Option<(PropertyKeyId, i64)>,
    /// Destination-property integer equality on the outgoing one-hop form.
    pub dst_prop: Option<(PropertyKeyId, i64)>,
    /// Destination-property integer inequality using GQL `<>` or its `!=` alias.
    pub dst_prop_ne: Option<(PropertyKeyId, i64)>,
    /// Destination-property strict integer greater-than predicate.
    pub dst_prop_gt: Option<(PropertyKeyId, i64)>,
    /// Destination-property strict integer less-than predicate.
    pub dst_prop_lt: Option<(PropertyKeyId, i64)>,
    /// Destination-property inclusive integer greater-than predicate.
    pub dst_prop_ge: Option<(PropertyKeyId, i64)>,
    /// Destination-property inclusive integer less-than predicate.
    pub dst_prop_le: Option<(PropertyKeyId, i64)>,
    /// Positive result-row bound applied after projection.
    pub limit: Option<u64>,
    /// Result rows dropped from the front after projection, before `limit`.
    /// `SKIP 0` is legal and binds `Some(0)` — the identity, the kernel
    /// drops nothing — unlike `LIMIT`, whose zero is a parse error.
    pub skip: Option<u64>,
    /// Hop-2 far-end property equality on the outgoing two-hop form.
    pub hop2_dst_prop: Option<(PropertyKeyId, i64)>,
    /// Hop-2 far-end property inequality on a two-hop form.
    pub hop2_dst_prop_ne: Option<(PropertyKeyId, i64)>,
    /// Hop-2 far-end strict greater-than on a two-hop form.
    pub hop2_dst_prop_gt: Option<(PropertyKeyId, i64)>,
    /// Hop-2 far-end strict less-than on a two-hop form.
    pub hop2_dst_prop_lt: Option<(PropertyKeyId, i64)>,
    /// Hop-2 far-end inclusive greater-than on a two-hop form.
    pub hop2_dst_prop_ge: Option<(PropertyKeyId, i64)>,
    /// Hop-2 far-end inclusive less-than on a two-hop form.
    pub hop2_dst_prop_le: Option<(PropertyKeyId, i64)>,
}

/// Deterministic relation-name binder for the supported GQL slice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelationBind {
    relations: BTreeMap<String, RelationId>,
    labels: BTreeMap<String, LabelId>,
    properties: BTreeMap<String, PropertyKeyId>,
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

    pub fn with_property(mut self, name: impl Into<String>, property: PropertyKeyId) -> Self {
        self.properties.insert(name.into(), property);
        self
    }

    /// Canonical certificate input for this relation-name binding.
    ///
    /// The transcript is big-endian and self-delimiting: entry count, then for
    /// each `(name, relation)` sorted by name and relation id, followed by the
    /// equivalently sorted label and property bindings. Names are
    /// length-prefixed and IDs are big-endian. Counts use `u64`, so the
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
        let mut properties: Vec<_> = self.properties.iter().collect();
        properties.sort_by(|(left_name, left_property), (right_name, right_property)| {
            left_name
                .cmp(right_name)
                .then_with(|| left_property.0.cmp(&right_property.0))
        });
        bytes.extend_from_slice(&(properties.len() as u64).to_be_bytes());
        for (name, property) in properties {
            bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&property.0.to_be_bytes());
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
        let src_prop = bind_property(&self.properties, ast.src_prop)?;
        let src_prop_ne = bind_property(&self.properties, ast.src_prop_ne)?;
        let src_prop_gt = bind_property(&self.properties, ast.src_prop_gt)?;
        let src_prop_lt = bind_property(&self.properties, ast.src_prop_lt)?;
        let src_prop_ge = bind_property(&self.properties, ast.src_prop_ge)?;
        let src_prop_le = bind_property(&self.properties, ast.src_prop_le)?;
        let dst_prop = bind_property(&self.properties, ast.dst_prop)?;
        let dst_prop_ne = bind_property(&self.properties, ast.dst_prop_ne)?;
        let dst_prop_gt = bind_property(&self.properties, ast.dst_prop_gt)?;
        let dst_prop_lt = bind_property(&self.properties, ast.dst_prop_lt)?;
        let dst_prop_ge = bind_property(&self.properties, ast.dst_prop_ge)?;
        let dst_prop_le = bind_property(&self.properties, ast.dst_prop_le)?;
        let hop2_dst_prop = bind_property(&self.properties, ast.hop2_dst_prop)?;
        let hop2_dst_prop_ne = bind_property(&self.properties, ast.hop2_dst_prop_ne)?;
        let hop2_dst_prop_gt = bind_property(&self.properties, ast.hop2_dst_prop_gt)?;
        let hop2_dst_prop_lt = bind_property(&self.properties, ast.hop2_dst_prop_lt)?;
        let hop2_dst_prop_ge = bind_property(&self.properties, ast.hop2_dst_prop_ge)?;
        let hop2_dst_prop_le = bind_property(&self.properties, ast.hop2_dst_prop_le)?;
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
            src_prop,
            src_prop_ne,
            src_prop_gt,
            src_prop_lt,
            src_prop_ge,
            src_prop_le,
            dst_prop,
            dst_prop_ne,
            dst_prop_gt,
            dst_prop_lt,
            dst_prop_ge,
            dst_prop_le,
            limit: ast.limit,
            skip: ast.skip,
            hop2_dst_prop,
            hop2_dst_prop_ne,
            hop2_dst_prop_gt,
            hop2_dst_prop_lt,
            hop2_dst_prop_ge,
            hop2_dst_prop_le,
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

fn bind_property(
    properties: &BTreeMap<String, PropertyKeyId>,
    predicate: Option<(String, i64)>,
) -> Result<Option<(PropertyKeyId, i64)>, BindError> {
    predicate
        .map(|(name, value)| {
            properties
                .get(&name)
                .copied()
                .map(|property| (property, value))
                .ok_or(BindError::UnknownProperty { name })
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
    src_prop: Option<(String, i64)>,
    src_prop_ne: Option<(String, i64)>,
    src_prop_gt: Option<(String, i64)>,
    src_prop_lt: Option<(String, i64)>,
    src_prop_ge: Option<(String, i64)>,
    src_prop_le: Option<(String, i64)>,
    dst_prop: Option<(String, i64)>,
    dst_prop_ne: Option<(String, i64)>,
    dst_prop_gt: Option<(String, i64)>,
    dst_prop_lt: Option<(String, i64)>,
    dst_prop_ge: Option<(String, i64)>,
    dst_prop_le: Option<(String, i64)>,
    limit: Option<u64>,
    skip: Option<u64>,
    hop2_dst_prop: Option<(String, i64)>,
    hop2_dst_prop_ne: Option<(String, i64)>,
    hop2_dst_prop_gt: Option<(String, i64)>,
    hop2_dst_prop_lt: Option<(String, i64)>,
    hop2_dst_prop_ge: Option<(String, i64)>,
    hop2_dst_prop_le: Option<(String, i64)>,
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
        if left_label.is_some()
            && (self.source[self.offset..].starts_with("RETURN")
                || self.source[self.offset..].starts_with("WHERE"))
        {
            let (src_prop, src_prop_ne, src_prop_gt, src_prop_lt, src_prop_ge, src_prop_le) =
                if self.source[self.offset..].starts_with("WHERE") {
                    self.keyword("WHERE")?;
                    let predicate_var = self.identifier()?;
                    if predicate_var != left_var {
                        return Err(ParseError {
                            offset: self.offset.saturating_sub(predicate_var.len()),
                            kind: ParseErrorKind::ReturnedVariableMismatch {
                                expected: left_var,
                                found: predicate_var,
                            },
                        });
                    }
                    self.token(".")?;
                    let property = self.identifier()?;
                    let remaining = self.source[self.offset..].trim_start();
                    let is_angle_ne = remaining.starts_with("<>");
                    let is_bang_ne = remaining.starts_with("!=");
                    let is_ne = is_angle_ne || is_bang_ne;
                    let is_le = remaining.starts_with("<=");
                    let is_lt = remaining.starts_with('<') && !is_ne && !is_le;
                    let is_ge = remaining.starts_with(">=");
                    let is_gt = remaining.starts_with('>') && !is_ge;
                    if is_angle_ne {
                        self.token("<")?;
                        self.token(">")?;
                    } else if is_bang_ne {
                        self.token("!")?;
                        self.token("=")?;
                    } else if is_le {
                        self.token("<")?;
                        self.token("=")?;
                    } else if is_lt {
                        self.token("<")?;
                    } else if is_ge {
                        self.token(">")?;
                        self.token("=")?;
                    } else if is_gt {
                        self.token(">")?;
                    } else {
                        self.token("=")?;
                    }
                    let predicate = Some((property, self.integer()?));
                    if is_ne {
                        (None, predicate, None, None, None, None)
                    } else if is_le {
                        (None, None, None, None, None, predicate)
                    } else if is_lt {
                        (None, None, None, predicate, None, None)
                    } else if is_ge {
                        (None, None, None, None, predicate, None)
                    } else if is_gt {
                        (None, None, predicate, None, None, None)
                    } else {
                        (predicate, None, None, None, None, None)
                    }
                } else {
                    (None, None, None, None, None, None)
                };
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
            let skip = self.optional_skip()?;
            let limit = self.optional_limit()?;
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
                src_prop,
                src_prop_ne,
                src_prop_gt,
                src_prop_lt,
                src_prop_ge,
                src_prop_le,
                dst_prop: None,
                dst_prop_ne: None,
                dst_prop_gt: None,
                dst_prop_lt: None,
                dst_prop_ge: None,
                dst_prop_le: None,
                limit,
                skip,
                hop2_dst_prop: None,
                hop2_dst_prop_ne: None,
                hop2_dst_prop_gt: None,
                hop2_dst_prop_lt: None,
                hop2_dst_prop_ge: None,
                hop2_dst_prop_le: None,
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
        let mut hop2_dst_prop = None;
        let mut hop2_dst_prop_ne = None;
        let mut hop2_dst_prop_gt = None;
        let mut hop2_dst_prop_lt = None;
        let mut hop2_dst_prop_ge = None;
        let mut hop2_dst_prop_le = None;
        let (
            neq,
            eq,
            src_prop,
            src_prop_ne,
            src_prop_gt,
            src_prop_lt,
            src_prop_ge,
            src_prop_le,
            dst_prop,
            dst_prop_ne,
            dst_prop_gt,
            dst_prop_lt,
            dst_prop_ge,
            dst_prop_le,
        ) = if (hop2_relation.is_none() || direction != EdgeDirection::Undirected)
            && self.source[self.offset..].starts_with("WHERE")
        {
            self.keyword("WHERE")?;
            let left = self.identifier()?;
            self.skip_whitespace();
            let is_incoming_two_hop =
                direction == EdgeDirection::Incoming && hop2_relation.is_some();
            let is_incoming_two_hop_near_end = is_incoming_two_hop && left == src_var;
            if is_incoming_two_hop
                && ((hop2_dst_var.as_ref() != Some(&left) && !is_incoming_two_hop_near_end)
                    || !self.source[self.offset..].starts_with('.'))
            {
                return Err(ParseError {
                    offset: self.offset.saturating_sub(left.len()),
                    kind: ParseErrorKind::ExpectedToken(
                        "incoming hop-2 destination property equality before RETURN",
                    ),
                });
            }
            if self.source[self.offset..].starts_with('.') {
                let is_hop2_destination =
                    hop2_relation.is_some() && hop2_dst_var.as_ref() == Some(&left);
                if left != src_var && left != dst_var && !is_hop2_destination {
                    return Err(ParseError {
                        offset: self.offset.saturating_sub(left.len()),
                        kind: ParseErrorKind::ReturnedVariableMismatch {
                            expected: hop2_dst_var.as_ref().map_or_else(
                                || format!("{src_var} or {dst_var}"),
                                |hop2_dst| format!("{src_var}, {dst_var}, or {hop2_dst}"),
                            ),
                            found: left,
                        },
                    });
                }
                self.token(".")?;
                let property = self.identifier()?;
                let remaining = self.source[self.offset..].trim_start();
                let is_prop_angle_ne = remaining.starts_with("<>");
                let is_prop_bang_ne = remaining.starts_with("!=");
                let is_prop_ne = is_prop_angle_ne || is_prop_bang_ne;
                let is_prop_le = remaining.starts_with("<=");
                let is_prop_lt = remaining.starts_with('<') && !is_prop_angle_ne && !is_prop_le;
                let is_prop_ge = remaining.starts_with(">=");
                let is_prop_gt = remaining.starts_with('>') && !is_prop_ge;
                if is_prop_bang_ne
                    && !is_incoming_two_hop_near_end
                    && !is_hop2_destination
                    && !(direction == EdgeDirection::Outgoing
                        && hop2_relation.is_some()
                        && left == src_var)
                    && !(direction == EdgeDirection::Outgoing
                        && hop2_relation.is_none()
                        && left == src_var)
                    && !(direction == EdgeDirection::Incoming
                        && hop2_relation.is_none()
                        && left == src_var)
                    && !(direction == EdgeDirection::Incoming
                        && hop2_relation.is_none()
                        && left == dst_var)
                    && !(direction == EdgeDirection::Undirected
                        && hop2_relation.is_none()
                        && left == src_var)
                    && !(direction == EdgeDirection::Undirected
                        && hop2_relation.is_none()
                        && left == dst_var)
                    && (direction != EdgeDirection::Outgoing
                        || hop2_relation.is_some()
                        || left != dst_var)
                {
                    return Err(ParseError {
                        offset: self.offset,
                        kind: ParseErrorKind::ExpectedToken(
                            "outgoing one-hop destination property before !=",
                        ),
                    });
                }
                if is_prop_angle_ne {
                    self.token("<")?;
                    self.token(">")?;
                } else if is_prop_bang_ne {
                    self.token("!")?;
                    self.token("=")?;
                } else if is_prop_le {
                    if !is_hop2_destination
                        && !is_incoming_two_hop_near_end
                        && (direction != EdgeDirection::Outgoing || hop2_relation.is_some())
                    {
                        return Err(ParseError {
                            offset: self.offset,
                            kind: ParseErrorKind::ExpectedToken(
                                "outgoing one-hop property before <=",
                            ),
                        });
                    }
                    self.token("<")?;
                    self.token("=")?;
                } else if is_prop_lt {
                    if !is_hop2_destination
                        && !is_incoming_two_hop_near_end
                        && (direction != EdgeDirection::Outgoing || hop2_relation.is_some())
                    {
                        return Err(ParseError {
                            offset: self.offset,
                            kind: ParseErrorKind::ExpectedToken(
                                "outgoing one-hop property before <",
                            ),
                        });
                    }
                    self.token("<")?;
                } else if is_prop_ge {
                    if !is_hop2_destination
                        && !is_incoming_two_hop_near_end
                        && (direction != EdgeDirection::Outgoing || hop2_relation.is_some())
                    {
                        return Err(ParseError {
                            offset: self.offset,
                            kind: ParseErrorKind::ExpectedToken(
                                "outgoing one-hop property before >=",
                            ),
                        });
                    }
                    self.token(">")?;
                    self.token("=")?;
                } else if is_prop_gt {
                    if !is_hop2_destination
                        && !is_incoming_two_hop_near_end
                        && (direction != EdgeDirection::Outgoing || hop2_relation.is_some())
                    {
                        return Err(ParseError {
                            offset: self.offset,
                            kind: ParseErrorKind::ExpectedToken(
                                "outgoing one-hop property before >",
                            ),
                        });
                    }
                    self.token(">")?;
                } else {
                    self.token("=")?;
                }
                let value = self.integer()?;
                let first_is_source = left == src_var;
                self.skip_whitespace();
                if is_hop2_destination {
                    if is_prop_ne {
                        hop2_dst_prop_ne = Some((property, value));
                    } else if is_prop_gt {
                        hop2_dst_prop_gt = Some((property, value));
                    } else if is_prop_lt {
                        hop2_dst_prop_lt = Some((property, value));
                    } else if is_prop_ge {
                        hop2_dst_prop_ge = Some((property, value));
                    } else if is_prop_le {
                        hop2_dst_prop_le = Some((property, value));
                    } else {
                        hop2_dst_prop = Some((property, value));
                    }
                    if self.source[self.offset..].starts_with("AND") {
                        return Err(ParseError {
                            offset: self.offset,
                            kind: ParseErrorKind::ExpectedToken(
                                "RETURN after hop-2 destination property predicate",
                            ),
                        });
                    }
                    (
                        None, None, None, None, None, None, None, None, None, None, None, None,
                        None, None,
                    )
                } else if is_incoming_two_hop_near_end {
                    if self.source[self.offset..].starts_with("AND") {
                        return Err(ParseError {
                            offset: self.offset,
                            kind: ParseErrorKind::ExpectedToken(
                                "RETURN after incoming two-hop near-end property equality",
                            ),
                        });
                    }
                    if is_prop_ne {
                        (
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some((property, value)),
                            None,
                            None,
                            None,
                            None,
                        )
                    } else if is_prop_gt {
                        (
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some((property, value)),
                            None,
                            None,
                            None,
                        )
                    } else if is_prop_lt {
                        (
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some((property, value)),
                            None,
                            None,
                        )
                    } else if is_prop_ge {
                        (
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some((property, value)),
                            None,
                        )
                    } else if is_prop_le {
                        (
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some((property, value)),
                        )
                    } else {
                        (
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                            Some((property, value)),
                            None,
                            None,
                            None,
                            None,
                            None,
                        )
                    }
                } else if self.source[self.offset..].starts_with("AND") {
                    if is_prop_gt || is_prop_lt || is_prop_ge || is_prop_le {
                        return Err(ParseError {
                            offset: self.offset,
                            kind: ParseErrorKind::ExpectedToken(
                                "RETURN after strict source property predicate",
                            ),
                        });
                    }
                    self.keyword("AND")?;
                    let second_var = self.identifier()?;
                    if second_var != src_var && second_var != dst_var {
                        return Err(ParseError {
                            offset: self.offset.saturating_sub(second_var.len()),
                            kind: ParseErrorKind::ReturnedVariableMismatch {
                                expected: format!("{src_var} or {dst_var}"),
                                found: second_var,
                            },
                        });
                    }
                    let second_is_source = second_var == src_var;
                    if first_is_source == second_is_source {
                        return Err(ParseError {
                            offset: self.offset.saturating_sub(second_var.len()),
                            kind: ParseErrorKind::ExpectedToken(
                                "one source and one destination property predicate",
                            ),
                        });
                    }
                    self.token(".")?;
                    let second_property = self.identifier()?;
                    let second_is_ne = self.source[self.offset..].trim_start().starts_with('<');
                    if second_is_ne {
                        self.token("<")?;
                        self.token(">")?;
                    } else {
                        self.token("=")?;
                    }
                    let second_value = self.integer()?;
                    let first = (property, value);
                    let second = (second_property, second_value);
                    let (mut src_prop, mut src_prop_ne, mut dst_prop, mut dst_prop_ne) =
                        (None, None, None, None);
                    if first_is_source {
                        if is_prop_ne {
                            src_prop_ne = Some(first);
                        } else {
                            src_prop = Some(first);
                        }
                        if second_is_ne {
                            dst_prop_ne = Some(second);
                        } else {
                            dst_prop = Some(second);
                        }
                    } else {
                        if is_prop_ne {
                            dst_prop_ne = Some(first);
                        } else {
                            dst_prop = Some(first);
                        }
                        if second_is_ne {
                            src_prop_ne = Some(second);
                        } else {
                            src_prop = Some(second);
                        }
                    }
                    (
                        None,
                        None,
                        src_prop,
                        src_prop_ne,
                        None,
                        None,
                        None,
                        None,
                        dst_prop,
                        dst_prop_ne,
                        None,
                        None,
                        None,
                        None,
                    )
                } else if is_prop_ne && first_is_source {
                    (
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                } else if is_prop_ne {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                        None,
                        None,
                    )
                } else if is_prop_ge && first_is_source {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                } else if is_prop_ge {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                    )
                } else if is_prop_le && first_is_source {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                } else if is_prop_le {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                    )
                } else if is_prop_gt && first_is_source {
                    (
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                } else if is_prop_gt {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                        None,
                    )
                } else if is_prop_lt && first_is_source {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                } else if is_prop_lt {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                    )
                } else if first_is_source {
                    (
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                } else {
                    (
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some((property, value)),
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                }
            } else {
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
                let binds_endpoints =
                    (left == src_var && right == dst_var) || (left == dst_var && right == src_var);
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
                    (
                        Some(variables),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                } else {
                    (
                        None,
                        Some(variables),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                }
            }
        } else {
            (
                None, None, None, None, None, None, None, None, None, None, None, None, None, None,
            )
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
        if direction == EdgeDirection::Incoming
            && hop2_relation.is_some()
            && (dst_prop.is_some()
                || dst_prop_ne.is_some()
                || dst_prop_gt.is_some()
                || dst_prop_lt.is_some()
                || dst_prop_ge.is_some()
                || dst_prop_le.is_some()
                || hop2_dst_prop.is_some()
                || hop2_dst_prop_ne.is_some()
                || hop2_dst_prop_gt.is_some()
                || hop2_dst_prop_lt.is_some()
                || hop2_dst_prop_ge.is_some()
                || hop2_dst_prop_le.is_some())
            && projection != ReturnProjection::Hop2Destination
        {
            return Err(ParseError {
                offset: self.offset.saturating_sub(returned.len()),
                kind: ParseErrorKind::ExpectedToken("incoming hop-2 destination after WHERE"),
            });
        }
        let skip = self.optional_skip()?;
        let limit = self.optional_limit()?;
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
            src_prop,
            src_prop_ne,
            src_prop_gt,
            src_prop_lt,
            src_prop_ge,
            src_prop_le,
            dst_prop,
            dst_prop_ne,
            dst_prop_gt,
            dst_prop_lt,
            dst_prop_ge,
            dst_prop_le,
            limit,
            skip,
            hop2_dst_prop,
            hop2_dst_prop_ne,
            hop2_dst_prop_gt,
            hop2_dst_prop_lt,
            hop2_dst_prop_ge,
            hop2_dst_prop_le,
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

    fn integer(&mut self) -> Result<i64, ParseError> {
        self.skip_whitespace();
        let start = self.offset;
        if self.source[self.offset..].starts_with('-') {
            self.offset += 1;
        }
        let digits = self.offset;
        while self.source[self.offset..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.offset += 1;
        }
        if self.offset == digits {
            return Err(ParseError {
                offset: start,
                kind: ParseErrorKind::ExpectedToken("integer"),
            });
        }
        self.source[start..self.offset]
            .parse()
            .map_err(|_| ParseError {
                offset: start,
                kind: ParseErrorKind::ExpectedToken("i64 integer"),
            })
    }

    /// Optional `SKIP <unsigned>` after the RETURN variable and before an
    /// optional `LIMIT` (fgdb-w5-parsers-nje.13). Unlike `LIMIT`, zero is
    /// legal — `SKIP 0` is the identity and binds `Some(0)`. `OFFSET` is
    /// deliberately not a spelling this slice accepts.
    fn optional_skip(&mut self) -> Result<Option<u64>, ParseError> {
        self.skip_whitespace();
        if !self.source[self.offset..].starts_with("SKIP") {
            return Ok(None);
        }
        self.keyword("SKIP")?;
        self.skip_whitespace();
        let start = self.offset;
        while self.source[self.offset..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.offset += 1;
        }
        let value = self.source[start..self.offset]
            .parse::<u64>()
            .map_err(|_| ParseError {
                offset: start,
                kind: ParseErrorKind::ExpectedToken("unsigned SKIP"),
            })?;
        Ok(Some(value))
    }

    fn optional_limit(&mut self) -> Result<Option<u64>, ParseError> {
        self.skip_whitespace();
        if !self.source[self.offset..].starts_with("LIMIT") {
            return Ok(None);
        }
        self.keyword("LIMIT")?;
        self.skip_whitespace();
        let start = self.offset;
        while self.source[self.offset..]
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.offset += 1;
        }
        let value = self.source[start..self.offset]
            .parse::<u64>()
            .map_err(|_| ParseError {
                offset: start,
                kind: ParseErrorKind::ExpectedToken("unsigned LIMIT"),
            })?;
        if value == 0 {
            return Err(ParseError {
                offset: start,
                kind: ParseErrorKind::ExpectedToken("positive LIMIT"),
            });
        }
        Ok(Some(value))
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
                src_prop: None,
                src_prop_ne: None,
                src_prop_gt: None,
                src_prop_lt: None,
                src_prop_ge: None,
                src_prop_le: None,
                dst_prop: None,
                dst_prop_ne: None,
                dst_prop_gt: None,
                dst_prop_lt: None,
                dst_prop_ge: None,
                dst_prop_le: None,
                limit: None,
                skip: None,
                hop2_dst_prop: None,
                hop2_dst_prop_ne: None,
                hop2_dst_prop_gt: None,
                hop2_dst_prop_lt: None,
                hop2_dst_prop_ge: None,
                hop2_dst_prop_le: None,
            }
        );
    }

    #[test]
    fn positive_limit_binds_and_zero_is_parse() {
        let binder = RelationBind::new().with_relation("R", RelationId(17));
        let limited = binder
            .bind("MATCH (a)-[:R]->(b) RETURN b LIMIT 1")
            .expect("positive LIMIT binds");
        assert_eq!(limited.limit, Some(1));

        let unlimited = binder
            .bind("MATCH (a)-[:R]->(b) RETURN b")
            .expect("MATCH without LIMIT remains grammar");
        assert_eq!(unlimited.limit, None);

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) RETURN b LIMIT 0"),
            Err(BindError::Parse(_))
        ));
    }

    #[test]
    fn skip_binds_before_limit_and_zero_is_identity() {
        let binder = RelationBind::new().with_relation("R", RelationId(17));
        let skipped = binder
            .bind("MATCH (a)-[:R]->(b) RETURN b SKIP 1")
            .expect("SKIP binds");
        assert_eq!(skipped.skip, Some(1));
        assert_eq!(skipped.limit, None);

        let paged = binder
            .bind("MATCH (a)-[:R]->(b) RETURN b SKIP 1 LIMIT 1")
            .expect("SKIP before LIMIT binds");
        assert_eq!(paged.skip, Some(1));
        assert_eq!(paged.limit, Some(1));

        // Zero is the identity, not an error — the LIMIT asymmetry is
        // deliberate: an empty page bound is meaningless, an empty drop is
        // not.
        let identity = binder
            .bind("MATCH (a)-[:R]->(b) RETURN b SKIP 0")
            .expect("SKIP 0 binds");
        assert_eq!(identity.skip, Some(0));

        let plain = binder
            .bind("MATCH (a)-[:R]->(b) RETURN b LIMIT 1")
            .expect("LIMIT-only stays grammar");
        assert_eq!(plain.skip, None);

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) RETURN b SKIP"),
            Err(BindError::Parse(_))
        ));
        // LIMIT-first is not the pinned order: the leftover SKIP is
        // trailing input, and OFFSET is not a spelling this slice accepts.
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) RETURN b LIMIT 1 SKIP 1"),
            Err(BindError::Parse(_))
        ));
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) RETURN b OFFSET 1"),
            Err(BindError::Parse(_))
        ));
    }

    #[test]
    fn node_only_skip_and_limit_bind_in_the_pinned_order() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_label("Person", LabelId(7));

        let skipped = binder
            .bind("MATCH (a:Person) RETURN a SKIP 1")
            .expect("node-only SKIP binds");
        assert_eq!(skipped.skip, Some(1));
        assert_eq!(skipped.limit, None);
        assert_eq!(skipped.relation, None, "node-only stays relationless");
        assert_eq!(skipped.src_label, Some(LabelId(7)));

        let paged = binder
            .bind("MATCH (a:Person) RETURN a SKIP 1 LIMIT 1")
            .expect("node-only SKIP then LIMIT binds");
        assert_eq!(paged.skip, Some(1));
        assert_eq!(paged.limit, Some(1));

        let limited = binder
            .bind("MATCH (a:Person) RETURN a LIMIT 1")
            .expect("node-only LIMIT alone binds");
        assert_eq!(limited.skip, None);
        assert_eq!(limited.limit, Some(1));

        // Pagination does not legalize the bare vertex scan: the pattern is
        // refused before SKIP is ever reached.
        assert!(matches!(
            binder.bind("MATCH (a) RETURN a SKIP 1"),
            Err(BindError::Parse(_))
        ));

        // The edge statement beside it is unmoved by the node-only path.
        let edge = binder
            .bind("MATCH (a)-[:R]->(b) RETURN b SKIP 1")
            .expect("edge SKIP still binds");
        assert_eq!(edge.skip, Some(1));
        assert_eq!(edge.relation, Some(RelationId(17)));
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
        assert_eq!(plan.src_prop, None);
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
    fn labeled_node_only_source_property_integer_equality_binds() {
        let binder = RelationBind::new()
            .with_label("Person", LabelId(7))
            .with_property("k", PropertyKeyId(9));
        let plan = binder
            .bind("MATCH (a:Person) WHERE a.k = 1 RETURN a")
            .expect("labeled node-only property predicate binds");
        assert_eq!(plan.relation, None);
        assert_eq!(plan.src_label, Some(LabelId(7)));
        assert_eq!(plan.src_prop, Some((PropertyKeyId(9), 1)));
        assert_eq!(plan.src_prop_ne, None);
        assert_eq!(plan.dst_prop, None);

        let unequal = binder
            .bind("MATCH (a:Person) WHERE a.k <> 1 RETURN a")
            .expect("labeled node-only property inequality binds");
        assert_eq!(unequal.src_prop, None);
        assert_eq!(unequal.src_prop_ne, Some((PropertyKeyId(9), 1)));

        assert!(matches!(
            binder.bind("MATCH (a) WHERE a.k = 1 RETURN a"),
            Err(BindError::Parse(_))
        ));
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
    fn source_property_integer_equality_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
            .expect("source property equality binds");
        assert_eq!(plan.src_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.src_prop_ne, None);
        assert_eq!(plan.eq, None);
        assert_eq!(plan.neq, None);

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE a.missing = 1 RETURN b"),
            Err(BindError::UnknownProperty { name }) if name == "missing"
        ));
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE a.k = b RETURN b"),
            Err(BindError::Parse(_))
        ));
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE a.k = b RETURN b"),
            Err(BindError::Parse(_))
        ));

        let inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a <> b RETURN b")
            .expect("endpoint inequality remains grammar");
        assert_eq!(inequality.neq, Some(("a".into(), "b".into())));
    }

    #[test]
    fn source_property_integer_inequality_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b SKIP 1 LIMIT 1")
            .expect("source property inequality binds");
        assert_eq!(plan.src_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.src_prop, None);
        assert_eq!(plan.skip, Some(1));
        assert_eq!(plan.limit, Some(1));

        let bang_inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b")
            .expect("source property bang inequality binds");
        assert_eq!(bang_inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(bang_inequality.src_prop, None);

        let equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
            .expect("source property equality remains grammar");
        assert_eq!(equality.src_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.src_prop_ne, None);
    }

    #[test]
    fn destination_property_integer_equality_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a")
            .expect("destination property equality binds");
        assert_eq!(plan.dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.dst_prop_ne, None);
        assert_eq!(plan.src_prop, None);

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE b.k = a RETURN a"),
            Err(BindError::Parse(_))
        ));
        let source = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
            .expect("source property equality remains grammar");
        assert_eq!(source.src_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(source.dst_prop, None);
    }

    #[test]
    fn destination_property_integer_inequality_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k <> 1 RETURN a SKIP 1 LIMIT 1")
            .expect("destination property inequality binds");
        assert_eq!(plan.dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.dst_prop, None);
        assert_eq!(plan.skip, Some(1));
        assert_eq!(plan.limit, Some(1));

        let alias = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k != 1 RETURN a")
            .expect("destination property != alias binds");
        assert_eq!(alias.dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(alias.dst_prop, None);

        let equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a")
            .expect("destination property equality remains grammar");
        assert_eq!(equality.dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.dst_prop_ne, None);

        let source_ne = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b")
            .expect("source property inequality remains grammar");
        assert_eq!(source_ne.src_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(source_ne.dst_prop_ne, None);
    }

    #[test]
    fn destination_property_strict_greater_than_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k > 1 RETURN a")
            .expect("destination property strict greater-than binds");
        assert_eq!(plan.dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.dst_prop, None);
        assert_eq!(plan.dst_prop_ne, None);

        let equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a")
            .expect("destination equality remains grammar");
        assert_eq!(equality.dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.dst_prop_ne, None);
        assert_eq!(equality.dst_prop_gt, None);

        let inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k <> 1 RETURN a")
            .expect("destination inequality remains grammar");
        assert_eq!(inequality.dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.dst_prop, None);
        assert_eq!(inequality.dst_prop_gt, None);

        let source = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b")
            .expect("source greater-than remains grammar");
        assert_eq!(source.src_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(source.dst_prop_gt, None);
    }

    #[test]
    fn destination_property_strict_less_than_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k < 1 RETURN a")
            .expect("destination property strict less-than binds");
        assert_eq!(plan.dst_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.dst_prop, None);
        assert_eq!(plan.dst_prop_ne, None);
        assert_eq!(plan.dst_prop_gt, None);

        let inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k <> 1 RETURN a")
            .expect("destination inequality remains grammar");
        assert_eq!(inequality.dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.dst_prop_lt, None);

        let greater = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k > 1 RETURN a")
            .expect("destination greater-than remains grammar");
        assert_eq!(greater.dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.dst_prop_lt, None);
    }

    #[test]
    fn destination_property_greater_than_or_equal_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k >= 1 RETURN a")
            .expect("destination property greater-than-or-equal binds");
        assert_eq!(plan.dst_prop_ge, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.dst_prop, None);
        assert_eq!(plan.dst_prop_ne, None);
        assert_eq!(plan.dst_prop_gt, None);
        assert_eq!(plan.dst_prop_lt, None);

        let greater = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k > 1 RETURN a")
            .expect("destination greater-than remains grammar");
        assert_eq!(greater.dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.dst_prop_ge, None);
        let less = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k < 1 RETURN a")
            .expect("destination less-than remains grammar");
        assert_eq!(less.dst_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(less.dst_prop_ge, None);
        let equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a")
            .expect("destination equality remains grammar");
        assert_eq!(equality.dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.dst_prop_ge, None);
        let inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k <> 1 RETURN a")
            .expect("destination inequality remains grammar");
        assert_eq!(inequality.dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.dst_prop_ge, None);

        let source = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN b")
            .expect("source greater-than-or-equal remains grammar");
        assert_eq!(source.src_prop_ge, Some((PropertyKeyId(7), 1)));
        assert_eq!(source.dst_prop_ge, None);

        let alias = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k != 1 RETURN a")
            .expect("destination != alias remains grammar");
        assert_eq!(alias.dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(alias.dst_prop_ge, None);
    }

    #[test]
    fn destination_property_less_than_or_equal_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k <= 1 RETURN a")
            .expect("destination property less-than-or-equal binds");
        assert_eq!(plan.dst_prop_le, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.dst_prop, None);
        assert_eq!(plan.dst_prop_ne, None);
        assert_eq!(plan.dst_prop_gt, None);
        assert_eq!(plan.dst_prop_lt, None);
        assert_eq!(plan.dst_prop_ge, None);

        let less = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k < 1 RETURN a")
            .expect("destination less-than remains grammar");
        assert_eq!(less.dst_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(less.dst_prop_le, None);
        let greater = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k > 1 RETURN a")
            .expect("destination greater-than remains grammar");
        assert_eq!(greater.dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.dst_prop_le, None);
        let equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a")
            .expect("destination equality remains grammar");
        assert_eq!(equality.dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.dst_prop_le, None);
        let inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE b.k <> 1 RETURN a")
            .expect("destination inequality remains grammar");
        assert_eq!(inequality.dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.dst_prop_le, None);

        let source = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k <= 1 RETURN b")
            .expect("source less-than-or-equal remains grammar");
        assert_eq!(source.src_prop_le, Some((PropertyKeyId(7), 1)));
        assert_eq!(source.dst_prop_le, None);

        let bang_source = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b")
            .expect("source bang inequality remains grammar");
        assert_eq!(bang_source.src_prop_ne, Some((PropertyKeyId(7), 1)));
    }

    #[test]
    fn source_and_destination_property_equalities_bind_in_either_order() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7))
            .with_property("m", PropertyKeyId(9));
        for statement in [
            "MATCH (a)-[:R]->(b) WHERE a.k = 1 AND b.m = 9 RETURN b",
            "MATCH (a)-[:R]->(b) WHERE b.m = 9 AND a.k = 1 RETURN b",
        ] {
            let plan = binder
                .bind(statement)
                .expect("both property predicates bind");
            assert_eq!(plan.src_prop, Some((PropertyKeyId(7), 1)));
            assert_eq!(plan.dst_prop, Some((PropertyKeyId(9), 9)));
        }

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 AND a.m = 9 RETURN b"),
            Err(BindError::Parse(_))
        ));
    }

    #[test]
    fn source_property_strict_greater_than_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_label("Person", LabelId(11))
            .with_property("k", PropertyKeyId(7));

        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b")
            .expect("source property strict greater-than binds");
        assert_eq!(plan.src_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.src_prop, None);
        assert_eq!(plan.src_prop_ne, None);

        let node_only = binder
            .bind("MATCH (a:Person) WHERE a.k > 1 RETURN a")
            .expect("labeled node-only strict greater-than binds");
        assert_eq!(node_only.src_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(node_only.src_prop, None);
        assert_eq!(node_only.src_prop_ne, None);

        let equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
            .expect("source equality remains grammar");
        assert_eq!(equality.src_prop, Some((PropertyKeyId(7), 1)));
        let inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b")
            .expect("source inequality remains grammar");
        assert_eq!(inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));

        let bang_inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b")
            .expect("source bang inequality remains grammar");
        assert_eq!(bang_inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
    }

    #[test]
    fn source_property_strict_less_than_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_label("Person", LabelId(11))
            .with_property("k", PropertyKeyId(7));

        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b")
            .expect("source property strict less-than binds");
        assert_eq!(plan.src_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.src_prop, None);
        assert_eq!(plan.src_prop_ne, None);
        assert_eq!(plan.src_prop_gt, None);

        let node_only = binder
            .bind("MATCH (a:Person) WHERE a.k < 1 RETURN a")
            .expect("labeled node-only strict less-than binds");
        assert_eq!(node_only.src_prop_lt, Some((PropertyKeyId(7), 1)));

        let equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
            .expect("source equality remains grammar");
        assert_eq!(equality.src_prop, Some((PropertyKeyId(7), 1)));
        let inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b")
            .expect("source inequality remains grammar");
        assert_eq!(inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
        let greater = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b")
            .expect("source greater-than remains grammar");
        assert_eq!(greater.src_prop_gt, Some((PropertyKeyId(7), 1)));
    }

    #[test]
    fn source_property_greater_than_or_equal_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN b")
            .expect("source property greater-than-or-equal binds");
        assert_eq!(plan.src_prop_ge, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.src_prop, None);
        assert_eq!(plan.src_prop_ne, None);
        assert_eq!(plan.src_prop_gt, None);
        assert_eq!(plan.src_prop_lt, None);

        let greater = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b")
            .expect("bare greater-than remains grammar");
        assert_eq!(greater.src_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.src_prop_ge, None);
        let equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
            .expect("equality remains grammar");
        assert_eq!(equality.src_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.src_prop_ge, None);
        let inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b")
            .expect("inequality remains grammar");
        assert_eq!(inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.src_prop_ge, None);
        let less = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b")
            .expect("less-than remains grammar");
        assert_eq!(less.src_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(less.src_prop_ge, None);

        let bang_inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b")
            .expect("source bang inequality remains grammar");
        assert_eq!(bang_inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
    }

    #[test]
    fn node_only_property_greater_than_or_equal_binds() {
        let binder = RelationBind::new()
            .with_label("Person", LabelId(11))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a:Person) WHERE a.k >= 1 RETURN a")
            .expect("labeled node-only greater-than-or-equal binds");
        assert_eq!(plan.relation, None);
        assert_eq!(plan.src_prop_ge, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.src_prop_gt, None);

        let greater = binder
            .bind("MATCH (a:Person) WHERE a.k > 1 RETURN a")
            .expect("labeled node-only greater-than remains grammar");
        assert_eq!(greater.src_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.src_prop_ge, None);

        let bang_inequality = binder
            .bind("MATCH (a:Person) WHERE a.k != 1 RETURN a")
            .expect("labeled node-only bang inequality binds");
        assert_eq!(bang_inequality.relation, None);
        assert_eq!(bang_inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));

        for statement in [
            "MATCH (a) WHERE a.k >= 1 RETURN a",
            "MATCH (a) WHERE a.k != 1 RETURN a",
        ] {
            assert!(matches!(binder.bind(statement), Err(BindError::Parse(_))));
        }
    }

    #[test]
    fn node_only_property_less_than_or_equal_binds() {
        let binder = RelationBind::new()
            .with_label("Person", LabelId(11))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a:Person) WHERE a.k <= 1 RETURN a")
            .expect("labeled node-only less-than-or-equal binds");
        assert_eq!(plan.relation, None);
        assert_eq!(plan.src_prop_le, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.src_prop, None);
        assert_eq!(plan.src_prop_ne, None);
        assert_eq!(plan.src_prop_gt, None);
        assert_eq!(plan.src_prop_lt, None);
        assert_eq!(plan.src_prop_ge, None);

        let less = binder
            .bind("MATCH (a:Person) WHERE a.k < 1 RETURN a")
            .expect("labeled node-only less-than remains grammar");
        assert_eq!(less.src_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(less.src_prop_le, None);

        let greater_equal = binder
            .bind("MATCH (a:Person) WHERE a.k >= 1 RETURN a")
            .expect("labeled node-only greater-than-or-equal remains grammar");
        assert_eq!(greater_equal.src_prop_ge, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater_equal.src_prop_le, None);
    }

    #[test]
    fn source_property_less_than_or_equal_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7));
        let plan = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k <= 1 RETURN b")
            .expect("source property less-than-or-equal binds");
        assert_eq!(plan.src_prop_le, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.src_prop, None);
        assert_eq!(plan.src_prop_ne, None);
        assert_eq!(plan.src_prop_gt, None);
        assert_eq!(plan.src_prop_lt, None);
        assert_eq!(plan.src_prop_ge, None);

        let less = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b")
            .expect("source less-than remains grammar");
        assert_eq!(less.src_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(less.src_prop_le, None);
        let greater = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b")
            .expect("source greater-than remains grammar");
        assert_eq!(greater.src_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.src_prop_le, None);
        let greater_equal = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN b")
            .expect("source greater-than-or-equal remains grammar");
        assert_eq!(greater_equal.src_prop_ge, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater_equal.src_prop_le, None);
        let equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
            .expect("source equality remains grammar");
        assert_eq!(equality.src_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.src_prop_le, None);
        let inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b")
            .expect("source inequality remains grammar");
        assert_eq!(inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.src_prop_le, None);

        let bang_inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b")
            .expect("source bang inequality remains grammar");
        assert_eq!(bang_inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
    }

    #[test]
    fn source_and_destination_property_inequalities_bind_in_either_order() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7))
            .with_property("m", PropertyKeyId(9));
        for statement in [
            "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m <> 9 RETURN b",
            "MATCH (a)-[:R]->(b) WHERE b.m <> 9 AND a.k <> 1 RETURN b",
        ] {
            let plan = binder
                .bind(statement)
                .expect("both property inequalities bind");
            assert_eq!(plan.src_prop_ne, Some((PropertyKeyId(7), 1)));
            assert_eq!(plan.dst_prop_ne, Some((PropertyKeyId(9), 9)));
            assert_eq!(plan.src_prop, None);
            assert_eq!(plan.dst_prop, None);
        }

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND a.m <> 9 RETURN b"),
            Err(BindError::Parse(_))
        ));

        assert!(
            binder
                .bind("MATCH (a)-[:R]->(b) WHERE a.k <> 1 RETURN b")
                .is_ok()
        );
        assert!(
            binder
                .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 AND b.m = 9 RETURN b")
                .is_ok()
        );
        let bang_inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b")
            .expect("source bang inequality remains grammar");
        assert_eq!(bang_inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
    }

    #[test]
    fn mixed_property_equality_and_inequality_bind_in_either_order() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_property("k", PropertyKeyId(7))
            .with_property("m", PropertyKeyId(9));
        for statement in [
            "MATCH (a)-[:R]->(b) WHERE a.k = 1 AND b.m <> 9 RETURN b",
            "MATCH (a)-[:R]->(b) WHERE b.m <> 9 AND a.k = 1 RETURN b",
        ] {
            let plan = binder
                .bind(statement)
                .expect("source equality and destination inequality bind");
            assert_eq!(plan.src_prop, Some((PropertyKeyId(7), 1)));
            assert_eq!(plan.dst_prop_ne, Some((PropertyKeyId(9), 9)));
            assert_eq!(plan.src_prop_ne, None);
            assert_eq!(plan.dst_prop, None);
        }
        for statement in [
            "MATCH (a)-[:R]->(b) WHERE a.k <> 1 AND b.m = 9 RETURN b",
            "MATCH (a)-[:R]->(b) WHERE b.m = 9 AND a.k <> 1 RETURN b",
        ] {
            let plan = binder
                .bind(statement)
                .expect("source inequality and destination equality bind");
            assert_eq!(plan.src_prop_ne, Some((PropertyKeyId(7), 1)));
            assert_eq!(plan.dst_prop, Some((PropertyKeyId(9), 9)));
            assert_eq!(plan.src_prop, None);
            assert_eq!(plan.dst_prop_ne, None);
        }

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 AND a.m <> 9 RETURN b"),
            Err(BindError::Parse(_))
        ));

        let bang_with_equality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k != 1 AND b.m = 9 RETURN b")
            .expect("source bang inequality and destination equality bind");
        assert_eq!(bang_with_equality.src_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(bang_with_equality.dst_prop, Some((PropertyKeyId(9), 9)));

        let bang_with_inequality = binder
            .bind("MATCH (a)-[:R]->(b) WHERE a.k != 1 AND b.m <> 9 RETURN b")
            .expect("source bang inequality and destination inequality bind");
        assert_eq!(
            bang_with_inequality.src_prop_ne,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(
            bang_with_inequality.dst_prop_ne,
            Some((PropertyKeyId(9), 9))
        );

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b) WHERE a.k != 1 AND a.m <> 9 RETURN b"),
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
        assert_eq!(
            (one_hop.src_var.as_str(), one_hop.dst_var.as_str()),
            ("b", "a")
        );

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
    fn incoming_one_hop_where_binds_after_source_destination_swap() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7));

        let source_eq = binder
            .bind("MATCH (a)<-[:R]-(b) WHERE b.k = 1 RETURN a")
            .expect("incoming source property equality binds");
        assert_eq!(source_eq.src_var, "b");
        assert_eq!(source_eq.dst_var, "a");
        assert_eq!(source_eq.src_prop, Some((PropertyKeyId(7), 1)));

        let source_ne = binder
            .bind("MATCH (a)<-[:R]-(b) WHERE b.k <> 1 RETURN a")
            .expect("incoming source property inequality binds");
        assert_eq!(source_ne.src_prop_ne, Some((PropertyKeyId(7), 1)));

        let destination_eq = binder
            .bind("MATCH (a)<-[:R]-(b) WHERE a.k = 1 RETURN a")
            .expect("incoming destination property equality binds");
        assert_eq!(destination_eq.dst_prop, Some((PropertyKeyId(7), 1)));

        let destination_ne = binder
            .bind("MATCH (a)<-[:R]-(b) WHERE a.k <> 1 RETURN a")
            .expect("incoming destination property inequality binds");
        assert_eq!(destination_ne.dst_prop_ne, Some((PropertyKeyId(7), 1)));

        let destination_bang_ne = binder
            .bind("MATCH (a)<-[:R]-(b) WHERE a.k != 1 RETURN a")
            .expect("incoming destination bang inequality binds");
        assert_eq!(destination_bang_ne.src_var, "b");
        assert_eq!(destination_bang_ne.dst_var, "a");
        assert_eq!(destination_bang_ne.dst_prop_ne, Some((PropertyKeyId(7), 1)));

        assert!(
            binder
                .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
                .is_ok()
        );
        assert!(matches!(
            binder.bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE b.k = 1 RETURN c"),
            Err(BindError::Parse(_))
        ));
        let source_bang_ne = binder
            .bind("MATCH (a)<-[:R]-(b) WHERE b.k != 1 RETURN a")
            .expect("incoming source bang inequality binds");
        assert_eq!(source_bang_ne.src_var, "b");
        assert_eq!(source_bang_ne.dst_var, "a");
        assert_eq!(source_bang_ne.src_prop_ne, Some((PropertyKeyId(7), 1)));
    }

    #[test]
    fn undirected_one_hop_where_binds_left_as_source() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7));

        let source_eq = binder
            .bind("MATCH (a)-[:R]-(b) WHERE a.k = 1 RETURN b")
            .expect("undirected source property equality binds");
        assert_eq!(source_eq.direction, EdgeDirection::Undirected);
        assert_eq!(source_eq.src_var, "a");
        assert_eq!(source_eq.dst_var, "b");
        assert_eq!(source_eq.src_prop, Some((PropertyKeyId(7), 1)));

        let source_ne = binder
            .bind("MATCH (a)-[:R]-(b) WHERE a.k <> 1 RETURN b")
            .expect("undirected source property inequality binds");
        assert_eq!(source_ne.src_prop_ne, Some((PropertyKeyId(7), 1)));

        let source_bang_ne = binder
            .bind("MATCH (a)-[:R]-(b) WHERE a.k != 1 RETURN b")
            .expect("undirected source bang inequality binds");
        assert_eq!(source_bang_ne.direction, EdgeDirection::Undirected);
        assert_eq!(source_bang_ne.src_prop_ne, Some((PropertyKeyId(7), 1)));

        let destination_eq = binder
            .bind("MATCH (a)-[:R]-(b) WHERE b.k = 1 RETURN b")
            .expect("undirected destination property equality binds");
        assert_eq!(destination_eq.dst_prop, Some((PropertyKeyId(7), 1)));

        let destination_ne = binder
            .bind("MATCH (a)-[:R]-(b) WHERE b.k <> 1 RETURN b")
            .expect("undirected destination property inequality binds");
        assert_eq!(destination_ne.dst_prop_ne, Some((PropertyKeyId(7), 1)));

        let destination_bang_ne = binder
            .bind("MATCH (a)-[:R]-(b) WHERE b.k != 1 RETURN b")
            .expect("undirected destination bang inequality binds");
        assert_eq!(destination_bang_ne.direction, EdgeDirection::Undirected);
        assert_eq!(destination_bang_ne.dst_prop_ne, Some((PropertyKeyId(7), 1)));

        assert!(
            binder
                .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
                .is_ok()
        );
        assert!(
            binder
                .bind("MATCH (a)<-[:R]-(b) WHERE b.k = 1 RETURN a")
                .is_ok()
        );
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]-(b)-[:S]-(c) WHERE a.k = 1 RETURN c"),
            Err(BindError::Parse(_))
        ));
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]-(b)-[:S]-(c) WHERE a.k != 1 RETURN c"),
            Err(BindError::Parse(_))
        ));
    }

    #[test]
    fn outgoing_two_hop_where_binds_the_hop_one_source() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7))
            .with_label("Person", LabelId(11));

        let equality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c")
            .expect("outgoing two-hop source property equality binds");
        assert_eq!(equality.src_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.hop2_relation, Some(RelationId(23)));
        assert_eq!(equality.projection, ReturnProjection::Hop2Destination);

        let inequality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k <> 1 RETURN c")
            .expect("outgoing two-hop source property inequality binds");
        assert_eq!(inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
        let bang_inequality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k != 1 RETURN c")
            .expect("outgoing two-hop source bang inequality binds");
        assert_eq!(bang_inequality.src_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(bang_inequality.hop2_dst_prop_ne, None);
        let projected_source = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k != 1 RETURN a")
            .expect("outgoing two-hop source bang inequality projects the source");
        assert_eq!(projected_source.src_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(projected_source.hop2_dst_prop_ne, None);

        assert!(
            binder
                .bind("MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b")
                .is_ok()
        );
        let incoming_near_end = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN c")
            .expect("incoming two-hop near-end property equality binds");
        assert_eq!(incoming_near_end.dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(incoming_near_end.src_prop, None);
        assert_eq!(incoming_near_end.hop2_dst_prop, None);
        let incoming_near_end_inequality = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <> 1 RETURN c")
            .expect("incoming two-hop near-end property inequality binds");
        assert_eq!(
            incoming_near_end_inequality.dst_prop_ne,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(incoming_near_end_inequality.src_prop_ne, None);
        assert_eq!(incoming_near_end_inequality.hop2_dst_prop_ne, None);
        let incoming_near_end_greater = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k > 1 RETURN c")
            .expect("incoming two-hop near-end property greater-than binds");
        assert_eq!(
            incoming_near_end_greater.dst_prop_gt,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(incoming_near_end_greater.src_prop_gt, None);
        assert_eq!(incoming_near_end_greater.hop2_dst_prop_gt, None);
        let incoming_near_end_less = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k < 9 RETURN c")
            .expect("incoming two-hop near-end property less-than binds");
        assert_eq!(
            incoming_near_end_less.dst_prop_lt,
            Some((PropertyKeyId(7), 9))
        );
        assert_eq!(incoming_near_end_less.src_prop_lt, None);
        assert_eq!(incoming_near_end_less.hop2_dst_prop_lt, None);
        let incoming_near_end_greater_or_equal = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k >= 9 RETURN c")
            .expect("incoming two-hop near-end property greater-or-equal binds");
        assert_eq!(
            incoming_near_end_greater_or_equal.dst_prop_ge,
            Some((PropertyKeyId(7), 9))
        );
        assert_eq!(incoming_near_end_greater_or_equal.src_prop_ge, None);
        assert_eq!(incoming_near_end_greater_or_equal.hop2_dst_prop_ge, None);
        let incoming_near_end_less_or_equal = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <= 1 RETURN c")
            .expect("incoming two-hop near-end property less-or-equal binds");
        assert_eq!(
            incoming_near_end_less_or_equal.dst_prop_le,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(incoming_near_end_less_or_equal.src_prop_le, None);
        assert_eq!(incoming_near_end_less_or_equal.hop2_dst_prop_le, None);
        let incoming_near_end_bang_inequality = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k != 1 RETURN c")
            .expect("incoming two-hop near-end C-style inequality binds");
        assert_eq!(
            incoming_near_end_bang_inequality.dst_prop_ne,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(incoming_near_end_bang_inequality.src_prop_ne, None);
        assert_eq!(incoming_near_end_bang_inequality.hop2_dst_prop_ne, None);
        for statement in [
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k = 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <> 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k > 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k < 9 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k >= 9 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k <= 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE a.k != 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE b.k = 1 RETURN c",
        ] {
            assert!(matches!(binder.bind(statement), Err(BindError::Parse(_))));
        }
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]-(b)-[:S]-(c) WHERE a.k = 1 RETURN c"),
            Err(BindError::Parse(_))
        ));
        assert!(matches!(
            binder.bind("MATCH (a:Person)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c"),
            Err(BindError::Parse(_))
        ));
    }

    #[test]
    fn two_hop_destination_property_equality_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7));

        let destination = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c")
            .expect("outgoing two-hop far-end property equality binds");
        assert_eq!(destination.hop2_dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(destination.hop2_dst_prop_ne, None);
        assert_eq!(destination.src_prop, None);
        assert_eq!(destination.dst_prop, None);

        let source = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE a.k = 1 RETURN c")
            .expect("outgoing two-hop source property equality remains bound");
        assert_eq!(source.src_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(source.dst_prop, None);
        assert_eq!(source.hop2_dst_prop, None);
        assert_eq!(source.hop2_dst_prop_ne, None);

        let bang_inequality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c")
            .expect("outgoing two-hop far-end bang inequality binds");
        assert_eq!(
            bang_inequality.hop2_dst_prop_ne,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(bang_inequality.hop2_dst_prop, None);
        let incoming = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN c")
            .expect("incoming two-hop far-end property equality binds");
        assert_eq!(incoming.hop2_dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(incoming.hop2_dst_prop_ne, None);
        assert_eq!(incoming.hop2_dst_prop_gt, None);
        assert_eq!(incoming.hop2_dst_prop_lt, None);
        assert_eq!(incoming.hop2_dst_prop_ge, None);
        assert_eq!(incoming.hop2_dst_prop_le, None);
    }

    #[test]
    fn incoming_two_hop_destination_property_equality_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7));

        let plan = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN c")
            .expect("incoming two-hop far-end property equality binds");
        assert_eq!(plan.hop2_dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(plan.hop2_dst_prop_ne, None);
        assert_eq!(plan.hop2_dst_prop_gt, None);
        assert_eq!(plan.hop2_dst_prop_lt, None);
        assert_eq!(plan.hop2_dst_prop_ge, None);
        assert_eq!(plan.hop2_dst_prop_le, None);

        let inequality = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN c")
            .expect("incoming two-hop far-end property inequality binds");
        assert_eq!(inequality.hop2_dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.hop2_dst_prop, None);
        assert_eq!(inequality.hop2_dst_prop_gt, None);
        assert_eq!(inequality.hop2_dst_prop_lt, None);
        assert_eq!(inequality.hop2_dst_prop_ge, None);
        assert_eq!(inequality.hop2_dst_prop_le, None);

        let bang_inequality = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k != 1 RETURN c")
            .expect("incoming two-hop far-end bang inequality binds");
        assert_eq!(
            bang_inequality.hop2_dst_prop_ne,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(bang_inequality.hop2_dst_prop, None);

        let greater = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN c")
            .expect("incoming two-hop far-end property greater-than binds");
        assert_eq!(greater.hop2_dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.hop2_dst_prop, None);
        assert_eq!(greater.hop2_dst_prop_ne, None);
        assert_eq!(greater.hop2_dst_prop_lt, None);
        assert_eq!(greater.hop2_dst_prop_ge, None);
        assert_eq!(greater.hop2_dst_prop_le, None);

        let less = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 1 RETURN c")
            .expect("incoming two-hop far-end property less-than binds");
        assert_eq!(less.hop2_dst_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(less.hop2_dst_prop, None);
        assert_eq!(less.hop2_dst_prop_ne, None);
        assert_eq!(less.hop2_dst_prop_gt, None);
        assert_eq!(less.hop2_dst_prop_ge, None);
        assert_eq!(less.hop2_dst_prop_le, None);

        let greater_or_equal = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 1 RETURN c")
            .expect("incoming two-hop far-end property greater-than-or-equal binds");
        assert_eq!(
            greater_or_equal.hop2_dst_prop_ge,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(greater_or_equal.hop2_dst_prop, None);
        assert_eq!(greater_or_equal.hop2_dst_prop_ne, None);
        assert_eq!(greater_or_equal.hop2_dst_prop_gt, None);
        assert_eq!(greater_or_equal.hop2_dst_prop_lt, None);
        assert_eq!(greater_or_equal.hop2_dst_prop_le, None);

        let less_or_equal = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN c")
            .expect("incoming two-hop far-end property less-than-or-equal binds");
        assert_eq!(less_or_equal.hop2_dst_prop_le, Some((PropertyKeyId(7), 1)));
        assert_eq!(less_or_equal.hop2_dst_prop, None);
        assert_eq!(less_or_equal.hop2_dst_prop_ne, None);
        assert_eq!(less_or_equal.hop2_dst_prop_gt, None);
        assert_eq!(less_or_equal.hop2_dst_prop_lt, None);
        assert_eq!(less_or_equal.hop2_dst_prop_ge, None);

        for statement in [
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k = 1 RETURN b",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN b",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN b",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 1 RETURN b",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 1 RETURN b",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN b",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k != 1 RETURN a",
            "MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE b.k != 1 RETURN c",
        ] {
            assert!(matches!(binder.bind(statement), Err(BindError::Parse(_))));
        }
    }

    #[test]
    fn two_hop_destination_property_inequality_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7));

        let inequality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c")
            .expect("outgoing two-hop far-end property inequality binds");
        assert_eq!(inequality.hop2_dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.hop2_dst_prop, None);
        assert_eq!(inequality.dst_prop_ne, None);

        let equality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c")
            .expect("outgoing two-hop far-end property equality remains bound");
        assert_eq!(equality.hop2_dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.hop2_dst_prop_ne, None);

        let bang_inequality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c")
            .expect("outgoing two-hop far-end bang inequality binds");
        assert_eq!(
            bang_inequality.hop2_dst_prop_ne,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(bang_inequality.hop2_dst_prop, None);
        assert_eq!(bang_inequality.dst_prop_ne, None);
        let incoming = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <> 1 RETURN c")
            .expect("incoming two-hop far-end property inequality binds");
        assert_eq!(incoming.hop2_dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(incoming.hop2_dst_prop, None);
        assert_eq!(incoming.dst_prop_ne, None);
    }

    #[test]
    fn two_hop_destination_property_gt_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7));

        let greater = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k > 1 RETURN c")
            .expect("outgoing two-hop far-end greater-than binds");
        assert_eq!(greater.hop2_dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.hop2_dst_prop, None);
        assert_eq!(greater.hop2_dst_prop_ne, None);
        assert_eq!(greater.dst_prop_gt, None);

        let equality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c")
            .expect("outgoing two-hop far-end equality remains bound");
        assert_eq!(equality.hop2_dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.hop2_dst_prop_ne, None);
        assert_eq!(equality.hop2_dst_prop_gt, None);

        let inequality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c")
            .expect("outgoing two-hop far-end inequality remains bound");
        assert_eq!(inequality.hop2_dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.hop2_dst_prop, None);
        assert_eq!(inequality.hop2_dst_prop_gt, None);

        let greater_or_equal = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k >= 1 RETURN c")
            .expect("outgoing two-hop far-end greater-than-or-equal remains bound");
        assert_eq!(
            greater_or_equal.hop2_dst_prop_ge,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(greater_or_equal.hop2_dst_prop_gt, None);
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c"),
            Err(BindError::Parse(_))
        ));
        let incoming = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k > 1 RETURN c")
            .expect("incoming two-hop far-end greater-than binds");
        assert_eq!(incoming.hop2_dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(incoming.hop2_dst_prop, None);
        assert_eq!(incoming.hop2_dst_prop_ne, None);
        assert_eq!(incoming.dst_prop_gt, None);
    }

    #[test]
    fn two_hop_destination_property_lt_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7));

        let less = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k < 1 RETURN c")
            .expect("outgoing two-hop far-end less-than binds");
        assert_eq!(less.hop2_dst_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(less.hop2_dst_prop, None);
        assert_eq!(less.hop2_dst_prop_ne, None);
        assert_eq!(less.hop2_dst_prop_gt, None);
        assert_eq!(less.dst_prop_lt, None);

        let equality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c")
            .expect("outgoing two-hop far-end equality remains bound");
        assert_eq!(equality.hop2_dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.hop2_dst_prop_lt, None);

        let inequality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c")
            .expect("outgoing two-hop far-end inequality remains bound");
        assert_eq!(inequality.hop2_dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.hop2_dst_prop_lt, None);

        let greater = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k > 1 RETURN c")
            .expect("outgoing two-hop far-end greater-than remains bound");
        assert_eq!(greater.hop2_dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.hop2_dst_prop_lt, None);

        let less_or_equal = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <= 1 RETURN c")
            .expect("outgoing two-hop far-end less-or-equal remains bound");
        assert_eq!(less_or_equal.hop2_dst_prop_le, Some((PropertyKeyId(7), 1)));
        assert_eq!(less_or_equal.hop2_dst_prop, None);
        assert_eq!(less_or_equal.hop2_dst_prop_ne, None);
        assert_eq!(less_or_equal.hop2_dst_prop_gt, None);
        assert_eq!(less_or_equal.hop2_dst_prop_lt, None);
        assert_eq!(less_or_equal.hop2_dst_prop_ge, None);
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c"),
            Err(BindError::Parse(_))
        ));
        let incoming = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k < 1 RETURN c")
            .expect("incoming two-hop far-end less-than binds");
        assert_eq!(incoming.hop2_dst_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(incoming.hop2_dst_prop, None);
        assert_eq!(incoming.hop2_dst_prop_ne, None);
        assert_eq!(incoming.hop2_dst_prop_gt, None);
        assert_eq!(incoming.dst_prop_lt, None);
    }

    #[test]
    fn two_hop_destination_property_ge_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7));

        let greater_or_equal = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k >= 1 RETURN c")
            .expect("outgoing two-hop far-end greater-or-equal binds");
        assert_eq!(
            greater_or_equal.hop2_dst_prop_ge,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(greater_or_equal.hop2_dst_prop, None);
        assert_eq!(greater_or_equal.hop2_dst_prop_ne, None);
        assert_eq!(greater_or_equal.hop2_dst_prop_gt, None);
        assert_eq!(greater_or_equal.hop2_dst_prop_lt, None);
        assert_eq!(greater_or_equal.dst_prop_ge, None);

        let equality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c")
            .expect("outgoing two-hop far-end equality remains bound");
        assert_eq!(equality.hop2_dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.hop2_dst_prop_ge, None);

        let inequality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c")
            .expect("outgoing two-hop far-end inequality remains bound");
        assert_eq!(inequality.hop2_dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.hop2_dst_prop_ge, None);

        let greater = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k > 1 RETURN c")
            .expect("outgoing two-hop far-end greater-than remains bound");
        assert_eq!(greater.hop2_dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.hop2_dst_prop_ge, None);

        let less = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k < 1 RETURN c")
            .expect("outgoing two-hop far-end less-than remains bound");
        assert_eq!(less.hop2_dst_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(less.hop2_dst_prop_ge, None);

        let less_or_equal = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <= 1 RETURN c")
            .expect("outgoing two-hop far-end less-or-equal remains bound");
        assert_eq!(less_or_equal.hop2_dst_prop_le, Some((PropertyKeyId(7), 1)));
        assert_eq!(less_or_equal.hop2_dst_prop, None);
        assert_eq!(less_or_equal.hop2_dst_prop_ne, None);
        assert_eq!(less_or_equal.hop2_dst_prop_gt, None);
        assert_eq!(less_or_equal.hop2_dst_prop_lt, None);
        assert_eq!(less_or_equal.hop2_dst_prop_ge, None);
        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c"),
            Err(BindError::Parse(_))
        ));
        let incoming = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k >= 1 RETURN c")
            .expect("incoming two-hop far-end greater-than-or-equal binds");
        assert_eq!(incoming.hop2_dst_prop_ge, Some((PropertyKeyId(7), 1)));
        assert_eq!(incoming.hop2_dst_prop, None);
        assert_eq!(incoming.hop2_dst_prop_ne, None);
        assert_eq!(incoming.hop2_dst_prop_gt, None);
        assert_eq!(incoming.hop2_dst_prop_lt, None);
        assert_eq!(incoming.dst_prop_ge, None);
    }

    #[test]
    fn two_hop_destination_property_le_binds() {
        let binder = RelationBind::new()
            .with_relation("R", RelationId(17))
            .with_relation("S", RelationId(23))
            .with_property("k", PropertyKeyId(7));

        let less_or_equal = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <= 1 RETURN c")
            .expect("outgoing two-hop far-end less-or-equal binds");
        assert_eq!(less_or_equal.hop2_dst_prop_le, Some((PropertyKeyId(7), 1)));
        assert_eq!(less_or_equal.hop2_dst_prop, None);
        assert_eq!(less_or_equal.hop2_dst_prop_ne, None);
        assert_eq!(less_or_equal.hop2_dst_prop_gt, None);
        assert_eq!(less_or_equal.hop2_dst_prop_lt, None);
        assert_eq!(less_or_equal.hop2_dst_prop_ge, None);
        assert_eq!(less_or_equal.dst_prop_le, None);

        let equality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k = 1 RETURN c")
            .expect("outgoing two-hop far-end equality remains bound");
        assert_eq!(equality.hop2_dst_prop, Some((PropertyKeyId(7), 1)));
        assert_eq!(equality.hop2_dst_prop_le, None);

        let inequality = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k <> 1 RETURN c")
            .expect("outgoing two-hop far-end inequality remains bound");
        assert_eq!(inequality.hop2_dst_prop_ne, Some((PropertyKeyId(7), 1)));
        assert_eq!(inequality.hop2_dst_prop_le, None);

        let greater = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k > 1 RETURN c")
            .expect("outgoing two-hop far-end greater-than remains bound");
        assert_eq!(greater.hop2_dst_prop_gt, Some((PropertyKeyId(7), 1)));
        assert_eq!(greater.hop2_dst_prop_le, None);

        let less = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k < 1 RETURN c")
            .expect("outgoing two-hop far-end less-than remains bound");
        assert_eq!(less.hop2_dst_prop_lt, Some((PropertyKeyId(7), 1)));
        assert_eq!(less.hop2_dst_prop_le, None);

        let greater_or_equal = binder
            .bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k >= 1 RETURN c")
            .expect("outgoing two-hop far-end greater-or-equal remains bound");
        assert_eq!(
            greater_or_equal.hop2_dst_prop_ge,
            Some((PropertyKeyId(7), 1))
        );
        assert_eq!(greater_or_equal.hop2_dst_prop_le, None);

        assert!(matches!(
            binder.bind("MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.k != 1 RETURN c"),
            Err(BindError::Parse(_))
        ));
        let incoming = binder
            .bind("MATCH (a)<-[:R]-(b)<-[:S]-(c) WHERE c.k <= 1 RETURN c")
            .expect("incoming two-hop far-end less-than-or-equal binds");
        assert_eq!(incoming.hop2_dst_prop_le, Some((PropertyKeyId(7), 1)));
        assert_eq!(incoming.hop2_dst_prop, None);
        assert_eq!(incoming.hop2_dst_prop_ne, None);
        assert_eq!(incoming.hop2_dst_prop_gt, None);
        assert_eq!(incoming.hop2_dst_prop_lt, None);
        assert_eq!(incoming.hop2_dst_prop_ge, None);
        assert_eq!(incoming.dst_prop_le, None);
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
