//! Typed numeric parameters for the existing bounded MATCH language.
//!
//! Preparation lexes parameter uses, then delegates grammar and name binding to
//! the canonical binder exactly once with inert numeric literals. Only slots
//! proven to exist in that bound plan can become parameters. Instantiation
//! checks the complete argument set and patches those numeric slots directly;
//! no caller-supplied text is ever evaluated and execution never reparses.
//!
//! A concrete statement is also rendered for the existing prepared-query and
//! evidence contracts. It contains only decimal encodings of typed numbers at
//! previously validated token spans. Existing evidence binds this concrete
//! definition, not the original template or an authenticated parameter receipt.

use crate::{BindError, BoundPlan, EdgeDirection, PreparedGqlQuery, RelationBind};
use fgdb_delta_types::PropertyKeyId;
use std::collections::BTreeMap;
use std::ops::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GqlParameterType {
    Int64,
    UInt64,
}

/// No text, floating-point, null, or implicit numeric coercion in this slice.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GqlParameterValue {
    Int64(i64),
    UInt64(u64),
}

impl GqlParameterValue {
    #[must_use]
    pub const fn parameter_type(self) -> GqlParameterType {
        match self {
            Self::Int64(_) => GqlParameterType::Int64,
            Self::UInt64(_) => GqlParameterType::UInt64,
        }
    }

    fn decimal(self) -> String {
        match self {
            Self::Int64(value) => value.to_string(),
            Self::UInt64(value) => value.to_string(),
        }
    }
}

impl core::fmt::Debug for GqlParameterValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}([REDACTED])", self.parameter_type())
    }
}

/// Exact, case-sensitive argument names without their leading `$`.
/// Duplicate insertion refuses without replacing the existing argument.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct GqlParameters {
    values: BTreeMap<String, GqlParameterValue>,
}

impl GqlParameters {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        name: impl Into<String>,
        value: GqlParameterValue,
    ) -> Result<(), GqlParameterError> {
        let name = name.into();
        if !valid_name(&name) {
            return Err(GqlParameterError::InvalidArgumentName { name });
        }
        if self.values.contains_key(&name) {
            return Err(GqlParameterError::Duplicate { name });
        }
        self.values.insert(name, value);
        Ok(())
    }

    pub fn with_int64(mut self, name: impl Into<String>, value: i64) -> Result<Self, GqlParameterError> {
        self.insert(name, GqlParameterValue::Int64(value))?;
        Ok(self)
    }

    pub fn with_uint64(mut self, name: impl Into<String>, value: u64) -> Result<Self, GqlParameterError> {
        self.insert(name, GqlParameterValue::UInt64(value))?;
        Ok(self)
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<GqlParameterValue> {
        self.values.get(name).copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Explicit plaintext export. Unlike Debug, these bytes contain values.
    /// This is a self-delimiting application transcript, not a durable format.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:gql-parameters:v1\0".to_vec();
        bytes.extend_from_slice(&(self.values.len() as u64).to_be_bytes());
        for (name, value) in &self.values {
            append_bytes(&mut bytes, name.as_bytes());
            match value {
                GqlParameterValue::Int64(value) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&value.to_be_bytes());
                }
                GqlParameterValue::UInt64(value) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&value.to_be_bytes());
                }
            }
        }
        bytes
    }
}

impl core::fmt::Debug for GqlParameters {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GqlParameters")
            .field("count", &self.len())
            .field("arguments", &"[REDACTED]")
            .finish()
    }
}

/// The template exposes these requirements before execution touches a database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GqlParameterSpec {
    pub name: String,
    pub parameter_type: GqlParameterType,
    /// True if any use of this unsigned parameter is a LIMIT.
    pub requires_positive: bool,
    pub occurrences: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GqlParameterError {
    Bind(BindError),
    InvalidParameterName { offset: usize },
    UnsupportedPosition { offset: usize },
    InvalidArgumentName { name: String },
    Duplicate { name: String },
    Missing { name: String },
    Unexpected { name: String },
    ConflictingTypes { name: String, first: GqlParameterType, second: GqlParameterType },
    TypeMismatch { name: String, expected: GqlParameterType, found: GqlParameterType },
    PositiveLimitRequired { name: String },
    /// A parameter could not be attached to a numeric slot of the canonical
    /// bound plan. Refuse instead of silently leaving an inert literal behind.
    DefinitionMismatch { offset: usize },
}

impl core::fmt::Display for GqlParameterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bind(error) => core::fmt::Display::fmt(error, f),
            Self::InvalidParameterName { offset } => write!(f, "invalid GQL parameter name at byte {offset}"),
            Self::UnsupportedPosition { offset } => write!(f, "parameter at byte {offset} is not a numeric predicate, SKIP, or LIMIT"),
            Self::InvalidArgumentName { name } => write!(f, "invalid parameter argument name {name:?}; omit the leading $"),
            Self::Duplicate { name } => write!(f, "duplicate parameter {name:?}"),
            Self::Missing { name } => write!(f, "missing parameter {name:?}"),
            Self::Unexpected { name } => write!(f, "unexpected parameter {name:?}"),
            Self::ConflictingTypes { name, first, second } => write!(f, "parameter {name:?} is used as both {first:?} and {second:?}"),
            Self::TypeMismatch { name, expected, found } => write!(f, "parameter {name:?} requires {expected:?}, found {found:?}"),
            Self::PositiveLimitRequired { name } => write!(f, "LIMIT parameter {name:?} must be positive"),
            Self::DefinitionMismatch { offset } => write!(f, "parameter at byte {offset} does not resolve to the expected bound numeric slot"),
        }
    }
}

impl core::error::Error for GqlParameterError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Bind(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comparison { Equal, NotEqual, Greater, Less, GreaterOrEqual, LessOrEqual }

impl Comparison {
    fn parse(token: &str) -> Option<Self> {
        Some(match token {
            "=" => Self::Equal,
            "<>" | "!=" => Self::NotEqual,
            ">" => Self::Greater,
            "<" => Self::Less,
            ">=" => Self::GreaterOrEqual,
            "<=" => Self::LessOrEqual,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role { Source, Destination, FarEnd }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target { Property(Role, Comparison), Skip, Limit }

impl Target {
    fn property(self, plan: &mut BoundPlan) -> Option<&mut Option<(PropertyKeyId, i64)>> {
        use Comparison::{Equal, Greater, GreaterOrEqual, Less, LessOrEqual, NotEqual};
        use Role::{Destination, FarEnd, Source};
        Some(match self {
            Self::Property(Source, Equal) => &mut plan.src_prop,
            Self::Property(Source, NotEqual) => &mut plan.src_prop_ne,
            Self::Property(Source, Greater) => &mut plan.src_prop_gt,
            Self::Property(Source, Less) => &mut plan.src_prop_lt,
            Self::Property(Source, GreaterOrEqual) => &mut plan.src_prop_ge,
            Self::Property(Source, LessOrEqual) => &mut plan.src_prop_le,
            Self::Property(Destination, Equal) => &mut plan.dst_prop,
            Self::Property(Destination, NotEqual) => &mut plan.dst_prop_ne,
            Self::Property(Destination, Greater) => &mut plan.dst_prop_gt,
            Self::Property(Destination, Less) => &mut plan.dst_prop_lt,
            Self::Property(Destination, GreaterOrEqual) => &mut plan.dst_prop_ge,
            Self::Property(Destination, LessOrEqual) => &mut plan.dst_prop_le,
            Self::Property(FarEnd, Equal) => &mut plan.hop2_dst_prop,
            Self::Property(FarEnd, NotEqual) => &mut plan.hop2_dst_prop_ne,
            Self::Property(FarEnd, Greater) => &mut plan.hop2_dst_prop_gt,
            Self::Property(FarEnd, Less) => &mut plan.hop2_dst_prop_lt,
            Self::Property(FarEnd, GreaterOrEqual) => &mut plan.hop2_dst_prop_ge,
            Self::Property(FarEnd, LessOrEqual) => &mut plan.hop2_dst_prop_le,
            Self::Skip | Self::Limit => return None,
        })
    }

    fn assign(self, plan: &mut BoundPlan, value: GqlParameterValue) -> bool {
        match (self, value) {
            (Self::Skip, GqlParameterValue::UInt64(value)) if plan.skip.is_some() => {
                plan.skip = Some(value);
                true
            }
            (Self::Limit, GqlParameterValue::UInt64(value)) if value > 0 && plan.limit.is_some() => {
                plan.limit = Some(value);
                true
            }
            (Self::Property(_, _), GqlParameterValue::Int64(value)) => {
                if let Some(Some((_, current))) = self.property(plan) {
                    *current = value;
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Use { Property { variable: String, comparison: Comparison }, Skip, Limit }

impl Use {
    fn parameter_type(&self) -> GqlParameterType {
        match self {
            Self::Property { .. } => GqlParameterType::Int64,
            Self::Skip | Self::Limit => GqlParameterType::UInt64,
        }
    }

    fn target(&self, plan: &BoundPlan) -> Option<Target> {
        match self {
            Self::Skip => Some(Target::Skip),
            Self::Limit => Some(Target::Limit),
            Self::Property { variable, comparison } => {
                // Match the canonical binder's precedence, including its
                // incoming two-hop near-end predicate normalization.
                let role = if plan.hop2_dst_var.as_ref() == Some(variable) {
                    Role::FarEnd
                } else if *variable == plan.src_var {
                    if plan.direction == EdgeDirection::Incoming && plan.hop2_relation.is_some() {
                        Role::Destination
                    } else {
                        Role::Source
                    }
                } else if *variable == plan.dst_var {
                    Role::Destination
                } else {
                    return None;
                };
                Some(Target::Property(role, *comparison))
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Occurrence {
    span: Range<usize>,
    name: String,
    usage: Use,
    target: Option<Target>,
}

/// Immutable, reusable numeric-parameter definition for the bounded grammar.
/// A successful bind returns the existing PreparedGqlQuery, usable by every
/// database, transaction, budget, certificate and evidence-replay adapter.
#[derive(Clone, PartialEq, Eq)]
#[must_use = "a prepared template has no effect until parameters are bound"]
pub struct PreparedGqlTemplate {
    statement: String,
    bind: RelationBind,
    prototype: BoundPlan,
    occurrences: Vec<Occurrence>,
    schema: Vec<GqlParameterSpec>,
}

impl PreparedGqlTemplate {
    pub fn prepare(statement: impl Into<String>, bind: &RelationBind) -> Result<Self, GqlParameterError> {
        let statement = statement.into();
        let mut occurrences = parameter_uses(&statement)?;
        let (normalized, mapping) = render(&statement, &occurrences, |_| "1".to_owned());
        let mut prototype = bind.bind(&normalized).map_err(|error| {
            let error = match error {
                BindError::Parse(mut error) => {
                    error.offset = original_offset(error.offset, &mapping);
                    BindError::Parse(error)
                }
                other => other,
            };
            GqlParameterError::Bind(error)
        })?;
        let mut schema: BTreeMap<String, GqlParameterSpec> = BTreeMap::new();
        let mut targets = Vec::new();
        for occurrence in &mut occurrences {
            let target = occurrence.usage.target(&prototype)
                .ok_or(GqlParameterError::DefinitionMismatch { offset: occurrence.span.start })?;
            let inert = match target {
                Target::Skip => prototype.skip == Some(1),
                Target::Limit => prototype.limit == Some(1),
                Target::Property(_, _) => target.property(&mut prototype)
                    .is_some_and(|slot| slot.as_ref().is_some_and(|(_, value)| *value == 1)),
            };
            if !inert || targets.contains(&target) {
                return Err(GqlParameterError::DefinitionMismatch { offset: occurrence.span.start });
            }
            targets.push(target);
            occurrence.target = Some(target);
            let kind = occurrence.usage.parameter_type();
            let spec = schema.entry(occurrence.name.clone()).or_insert_with(|| GqlParameterSpec {
                name: occurrence.name.clone(), parameter_type: kind, requires_positive: false, occurrences: 0,
            });
            if spec.parameter_type != kind {
                return Err(GqlParameterError::ConflictingTypes {
                    name: occurrence.name.clone(), first: spec.parameter_type, second: kind,
                });
            }
            spec.requires_positive |= occurrence.usage == Use::Limit;
            spec.occurrences += 1;
        }
        Ok(Self { statement, bind: bind.clone(), prototype, occurrences, schema: schema.into_values().collect() })
    }

    #[must_use]
    pub fn statement(&self) -> &str { &self.statement }

    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { &self.schema }

    /// Validate the entire argument set before cloning/changing any plan.
    pub fn validate_parameters(&self, parameters: &GqlParameters) -> Result<(), GqlParameterError> {
        for spec in &self.schema {
            let value = parameters.get(&spec.name)
                .ok_or_else(|| GqlParameterError::Missing { name: spec.name.clone() })?;
            if value.parameter_type() != spec.parameter_type {
                return Err(GqlParameterError::TypeMismatch {
                    name: spec.name.clone(), expected: spec.parameter_type, found: value.parameter_type(),
                });
            }
            if spec.requires_positive && value == GqlParameterValue::UInt64(0) {
                return Err(GqlParameterError::PositiveLimitRequired { name: spec.name.clone() });
            }
        }
        for name in parameters.values.keys() {
            if self.schema.binary_search_by(|spec| spec.name.as_str().cmp(name.as_str())).is_err() {
                return Err(GqlParameterError::Unexpected { name: name.clone() });
            }
        }
        Ok(())
    }

    /// Instantiate only validated numeric slots, without parsing or rebinding
    /// schema names. Each returned definition owns its values and remains
    /// unaffected by later argument sets or rebinding of this template.
    pub fn bind_parameters(&self, parameters: &GqlParameters) -> Result<PreparedGqlQuery, GqlParameterError> {
        self.validate_parameters(parameters)?;
        let mut plan = self.prototype.clone();
        for occurrence in &self.occurrences {
            let value = parameters.get(&occurrence.name)
                .ok_or_else(|| GqlParameterError::Missing { name: occurrence.name.clone() })?;
            if !occurrence.target.is_some_and(|target| target.assign(&mut plan, value)) {
                return Err(GqlParameterError::DefinitionMismatch { offset: occurrence.span.start });
            }
        }
        let (statement, _) = render(&self.statement, &self.occurrences, |occurrence| {
            // All occurrences were checked above against the same immutable map.
            parameters.values[&occurrence.name].decimal()
        });
        Ok(PreparedGqlQuery::from_parameter_instantiation(statement, self.bind.clone(), plan))
    }

    /// A deterministic application identity for the original template, name
    /// bindings and typed argument set. This is not an authorization token or
    /// a replacement for snapshot/result evidence.
    pub fn binding_digest(&self, parameters: &GqlParameters) -> Result<fgdb_crypto::Digest, GqlParameterError> {
        self.validate_parameters(parameters)?;
        let mut bytes = b"fgdb:gql-parameter-binding:v1\0".to_vec();
        append_bytes(&mut bytes, self.statement.as_bytes());
        append_bytes(&mut bytes, &self.bind.canonical_bytes());
        append_bytes(&mut bytes, &parameters.canonical_bytes());
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(&bytes);
        Ok(hasher.finalize())
    }
}

impl core::fmt::Debug for PreparedGqlTemplate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGqlTemplate")
            .field("parameter_count", &self.schema.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone)]
struct Token<'a> { text: &'a str }

fn identifier_start(byte: u8) -> bool { byte == b'_' || byte.is_ascii_alphabetic() }
fn identifier_continue(byte: u8) -> bool { byte == b'_' || byte.is_ascii_alphanumeric() }
fn valid_name(name: &str) -> bool {
    name.as_bytes().first().is_some_and(|byte| identifier_start(*byte))
        && name.bytes().all(identifier_continue)
}

/// Only numeric parameter positions are recognized here. The canonical parser
/// remains the authority on every pattern, comparator and conjunction rule.
fn parameter_uses(statement: &str) -> Result<Vec<Occurrence>, GqlParameterError> {
    let mut previous: Vec<Token<'_>> = Vec::new();
    let mut occurrences = Vec::new();
    let mut offset = 0;
    while offset < statement.len() {
        let character = statement[offset..].chars().next().expect("offset is inside the source");
        if character.is_whitespace() { offset += character.len_utf8(); continue; }
        let start = offset;
        let byte = statement.as_bytes()[offset];
        if byte == b'$' {
            offset += 1;
            if !statement.as_bytes().get(offset).is_some_and(|byte| identifier_start(*byte)) {
                return Err(GqlParameterError::InvalidParameterName { offset: start });
            }
            while statement.as_bytes().get(offset).is_some_and(|byte| identifier_continue(*byte)) { offset += 1; }
            let last = previous.last().map(|token| token.text);
            let usage = match last {
                Some("SKIP") => Use::Skip,
                Some("LIMIT") => Use::Limit,
                _ => {
                    let recent = previous.as_slice();
                    let Some(comparison) = last.and_then(Comparison::parse) else {
                        return Err(GqlParameterError::UnsupportedPosition { offset: start });
                    };
                    if recent.len() < 4 || recent[recent.len() - 3].text != "."
                        || !valid_name(recent[recent.len() - 4].text)
                        || !valid_name(recent[recent.len() - 2].text)
                    {
                        return Err(GqlParameterError::UnsupportedPosition { offset: start });
                    }
                    Use::Property { variable: recent[recent.len() - 4].text.to_owned(), comparison }
                }
            };
            occurrences.push(Occurrence {
                span: start..offset, name: statement[start + 1..offset].to_owned(), usage, target: None,
            });
        } else if identifier_start(byte) {
            offset += 1;
            while statement.as_bytes().get(offset).is_some_and(|byte| identifier_continue(*byte)) { offset += 1; }
        } else if byte.is_ascii_digit() {
            offset += 1;
            while statement.as_bytes().get(offset).is_some_and(u8::is_ascii_digit) { offset += 1; }
        } else {
            offset += character.len_utf8();
            if matches!(&statement[start..offset], "<" | ">" | "!")
                && statement.as_bytes().get(offset).is_some_and(|next| *next == b'=' || (byte == b'<' && *next == b'>'))
            {
                offset += 1;
            }
        }
        previous.push(Token { text: &statement[start..offset] });
        if previous.len() > 4 { previous.remove(0); }
    }
    Ok(occurrences)
}

struct RenderedSpan { original: Range<usize>, rendered: Range<usize> }

fn render(
    statement: &str,
    occurrences: &[Occurrence],
    mut decimal: impl FnMut(&Occurrence) -> String,
) -> (String, Vec<RenderedSpan>) {
    let mut result = String::new();
    let mut mapping = Vec::with_capacity(occurrences.len());
    let mut cursor = 0;
    for occurrence in occurrences {
        result.push_str(&statement[cursor..occurrence.span.start]);
        let start = result.len();
        // Preserve token boundaries even for `LIMIT$n` or `=$value` spellings.
        result.push(' ');
        result.push_str(&decimal(occurrence));
        result.push(' ');
        mapping.push(RenderedSpan { original: occurrence.span.clone(), rendered: start..result.len() });
        cursor = occurrence.span.end;
    }
    result.push_str(&statement[cursor..]);
    (result, mapping)
}

fn original_offset(offset: usize, mapping: &[RenderedSpan]) -> usize {
    let mut previous_original_end = 0;
    let mut previous_rendered_end = 0;
    for span in mapping {
        if offset < span.rendered.start { break; }
        if offset < span.rendered.end { return span.original.start; }
        previous_original_end = span.original.end;
        previous_rendered_end = span.rendered.end;
    }
    previous_original_end + offset.saturating_sub(previous_rendered_end)
}

fn append_bytes(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::{LabelId, RelationId};

    fn bindings() -> RelationBind {
        RelationBind::new().with_relation("R", RelationId(1)).with_relation("S", RelationId(2))
            .with_label("L", LabelId(3)).with_property("n", PropertyKeyId(4)).with_property("m", PropertyKeyId(5))
    }

    #[test]
    fn typed_binding_reuses_one_definition_and_preserves_concrete_coherence() {
        let mut bind = bindings();
        let source = "MATCH (a)-[:R]->(b) WHERE a.n=$min AND b.m<>$other RETURN b SKIP $offset LIMIT $cap";
        let template = PreparedGqlTemplate::prepare(source, &bind).unwrap();
        bind.insert("R", RelationId(99));
        let first = GqlParameters::new().with_int64("min", i64::MIN).unwrap()
            .with_int64("other", i64::MAX).unwrap().with_uint64("offset", 0).unwrap()
            .with_uint64("cap", u64::MAX).unwrap();
        let a = template.bind_parameters(&first).unwrap();
        assert_eq!(a.plan().src_prop, Some((PropertyKeyId(4), i64::MIN)));
        assert_eq!(a.plan().dst_prop_ne, Some((PropertyKeyId(5), i64::MAX)));
        assert_eq!(a.plan().skip, Some(0));
        assert_eq!(a.plan().limit, Some(u64::MAX));
        assert_eq!(a.plan().relation, Some(RelationId(1)));
        assert!(a.verifies_definition());
        let second = GqlParameters::new().with_int64("min", 7).unwrap()
            .with_int64("other", -8).unwrap().with_uint64("offset", 2).unwrap()
            .with_uint64("cap", 3).unwrap();
        let b = template.bind_parameters(&second).unwrap();
        assert!(b.verifies_definition());
        assert_ne!(a.plan(), b.plan());
        assert_eq!(a, template.bind_parameters(&first).unwrap());
        assert_eq!(template.statement(), source);
        assert_eq!(template.parameter_schema().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["cap", "min", "offset", "other"]);
    }

    #[test]
    fn repeated_names_share_a_type_and_intersect_count_constraints() {
        let shared = PreparedGqlTemplate::prepare(
            "MATCH (a)-[:R]->(b) WHERE a.n=$x AND b.m=$x RETURN b", &bindings()).unwrap();
        assert_eq!(shared.parameter_schema().len(), 1);
        assert_eq!(shared.parameter_schema()[0].occurrences, 2);
        let query = shared.bind_parameters(&GqlParameters::new().with_int64("x", -7).unwrap()).unwrap();
        assert_eq!(query.plan().src_prop, Some((PropertyKeyId(4), -7)));
        assert_eq!(query.plan().dst_prop, Some((PropertyKeyId(5), -7)));
        assert!(query.verifies_definition());
        let counts = PreparedGqlTemplate::prepare(
            "MATCH (a:L) RETURN a SKIP$n LIMIT$n", &bindings()).unwrap();
        assert!(counts.parameter_schema()[0].requires_positive);
        assert!(matches!(counts.bind_parameters(&GqlParameters::new().with_uint64("n", 0).unwrap()),
            Err(GqlParameterError::PositiveLimitRequired { .. })));
        assert!(counts.bind_parameters(&GqlParameters::new().with_uint64("n", 1).unwrap()).unwrap().verifies_definition());
        assert!(matches!(PreparedGqlTemplate::prepare(
            "MATCH (a:L) WHERE a.n=$x RETURN a LIMIT$x", &bindings()),
            Err(GqlParameterError::ConflictingTypes { .. })));
    }

    #[test]
    fn argument_errors_are_explicit_and_do_not_poison_future_bindings() {
        let template = PreparedGqlTemplate::prepare("MATCH (a:L) WHERE a.n=$x RETURN a LIMIT$cap", &bindings()).unwrap();
        assert!(matches!(template.bind_parameters(&GqlParameters::new()), Err(GqlParameterError::Missing { .. })));
        let wrong = GqlParameters::new().with_uint64("x", 9).unwrap().with_uint64("cap", 1).unwrap();
        assert!(matches!(template.bind_parameters(&wrong), Err(GqlParameterError::TypeMismatch { .. })));
        let valid = GqlParameters::new().with_int64("x", 9).unwrap().with_uint64("cap", 1).unwrap();
        let extra = valid.clone().with_int64("typo", 1).unwrap();
        assert!(matches!(template.bind_parameters(&extra), Err(GqlParameterError::Unexpected { .. })));
        let mut duplicate = valid.clone();
        assert!(matches!(duplicate.insert("x", GqlParameterValue::Int64(99)), Err(GqlParameterError::Duplicate { .. })));
        assert_eq!(duplicate, valid);
        assert!(template.bind_parameters(&valid).unwrap().verifies_definition());
        assert!(matches!(GqlParameters::new().with_int64("$x", 9), Err(GqlParameterError::InvalidArgumentName { .. })));
    }

    #[test]
    fn parameter_bindings_equal_literal_binding_for_every_supported_numeric_position() {
        let bind = bindings();
        let patterns = [
            ("MATCH (a:L)", "a"),
            ("MATCH (a)-[:R]->(b)", "b"),
            ("MATCH (a)<-[:R]-(b)", "b"),
            ("MATCH (a)-[:R]-(b)", "b"),
            ("MATCH (a)-[:R]->(b)-[:S]->(c)", "c"),
            ("MATCH (a)<-[:R]-(b)<-[:S]-(c)", "c"),
            ("MATCH (a)-[:R]-(b)-[:S]-(c)", "c"),
        ];
        let mut supported = 0;
        let mut refused = 0;
        for (pattern, returned) in patterns {
            for variable in ["a", "b", "c"] {
                for operator in ["=", "<>", "!=", ">", "<", ">=", "<="] {
                    let template_source = format!("{pattern} WHERE {variable}.n{operator}$value RETURN {returned} SKIP$skip LIMIT$cap");
                    let probe = format!("{pattern} WHERE {variable}.n{operator}1 RETURN {returned} SKIP 0 LIMIT 2");
                    if bind.bind(&probe).is_err() {
                        assert!(PreparedGqlTemplate::prepare(&template_source, &bind).is_err(), "{template_source}");
                        refused += 1;
                        continue;
                    }
                    let template = PreparedGqlTemplate::prepare(&template_source, &bind).unwrap();
                    for value in [i64::MIN, -1, 0, 1, i64::MAX] {
                        for (skip, cap) in [(0, 1), (1, 2), (u64::MAX, u64::MAX)] {
                            let parameters = GqlParameters::new().with_int64("value", value).unwrap()
                                .with_uint64("skip", skip).unwrap().with_uint64("cap", cap).unwrap();
                            let query = template.bind_parameters(&parameters).unwrap();
                            let literal = format!("{pattern} WHERE {variable}.n{operator}{value} RETURN {returned} SKIP {skip} LIMIT {cap}");
                            assert_eq!(query.plan(), &bind.bind(&literal).unwrap(), "{template_source}");
                            assert!(query.verifies_definition());
                        }
                    }
                    supported += 1;
                }
            }
        }
        assert!(supported > 40, "positive corpus unexpectedly shrank: {supported}");
        assert!(refused > 40, "refusal corpus unexpectedly shrank: {refused}");
    }

    #[test]
    fn parameters_cannot_supply_names_operators_clauses_or_unary_expressions() {
        for source in [
            "MATCH ($a:L) RETURN a", "MATCH (a:$label) RETURN a",
            "MATCH (a)-[:$relation]->(b) RETURN b", "MATCH (a:L) RETURN $a",
            "MATCH (a:L) WHERE a.$key=1 RETURN a", "MATCH (a:L) WHERE a.n $op 1 RETURN a",
            "MATCH (a:L) WHERE a.n=-$value RETURN a", "MATCH (a:L) WHERE a.n=1$value RETURN a",
            "MATCH (a:L) WHERE a.n=$value+1 RETURN a", "MATCH (a:L) RETURN a LIMIT $$cap",
            "MATCH (a:L) RETURN a LIMIT $", "MATCH (a:L) RETURN a LIMIT $9x",
            "MATCH (a:L) RETURN a LIMIT $雪", "MATCH (a:L) WHERE a.n='$value' RETURN a",
        ] {
            assert!(PreparedGqlTemplate::prepare(source, &bindings()).is_err(), "{source}");
        }
    }

    #[test]
    fn original_byte_offsets_survive_multiple_placeholders_and_unicode_whitespace() {
        let source = "\u{2003}MATCH (a)-[:R]->(b) WHERE a.n=$a_very_long_parameter AND b.m=$x RETURN unknown";
        let error = PreparedGqlTemplate::prepare(source, &bindings()).unwrap_err();
        assert!(matches!(error, GqlParameterError::Bind(BindError::Parse(ref error))
            if error.offset == source.rfind("unknown").unwrap()));
        let source = "MATCH (a:L) WHERE a.n=$value RETURN a LIMIT $";
        assert!(matches!(PreparedGqlTemplate::prepare(source, &bindings()),
            Err(GqlParameterError::InvalidParameterName { offset }) if offset == source.rfind('$').unwrap()));
    }

    #[test]
    fn canonical_parameter_identity_is_order_independent_but_type_and_value_sensitive() {
        let first = GqlParameters::new().with_int64("x", 3).unwrap().with_uint64("n", 2).unwrap();
        let reordered = GqlParameters::new().with_uint64("n", 2).unwrap().with_int64("x", 3).unwrap();
        let changed = GqlParameters::new().with_int64("x", 4).unwrap().with_uint64("n", 2).unwrap();
        assert_eq!(first.canonical_bytes(), reordered.canonical_bytes());
        assert_ne!(first.canonical_bytes(), changed.canonical_bytes());
        assert_ne!(GqlParameters::new().with_int64("x", 3).unwrap().canonical_bytes(),
            GqlParameters::new().with_uint64("x", 3).unwrap().canonical_bytes());
        let template = PreparedGqlTemplate::prepare("MATCH (a:L) WHERE a.n=$x RETURN a LIMIT$n", &bindings()).unwrap();
        assert_eq!(template.binding_digest(&first).unwrap(), template.binding_digest(&reordered).unwrap());
        assert_ne!(template.binding_digest(&first).unwrap(), template.binding_digest(&changed).unwrap());
        let other = PreparedGqlTemplate::prepare("MATCH (a:L) WHERE a.n<>$x RETURN a LIMIT$n", &bindings()).unwrap();
        assert_ne!(template.binding_digest(&first).unwrap(), other.binding_digest(&first).unwrap());
    }

    #[test]
    fn values_and_definitions_are_redacted_by_default_and_literals_stay_unchanged() {
        let parameters = GqlParameters::new().with_int64("private_name", 987654321).unwrap();
        assert!(!format!("{parameters:?}").contains("private_name"));
        assert!(!format!("{:?}", parameters.get("private_name").unwrap()).contains("987654321"));
        let template = PreparedGqlTemplate::prepare("MATCH (a:L) WHERE a.n=$private_name RETURN a", &bindings()).unwrap();
        assert!(!format!("{template:?}").contains("private_name"));
        let literal = "MATCH (a:L) WHERE a.n=-17 RETURN a SKIP 0 LIMIT 2";
        let template = PreparedGqlTemplate::prepare(literal, &bindings()).unwrap();
        let query = template.bind_parameters(&GqlParameters::new()).unwrap();
        assert_eq!(query.statement(), literal);
        assert!(query.verifies_definition());
        assert!(template.parameter_schema().is_empty());
    }
}
