//! Bounded, conjunctive read-policy IR (plan §12.2; fgdb-w1-authz-policy-10y).
//!
//! This is the read arm of the verifier language, not a grant of authority.
//! Compiling caller-supplied restrictions never authenticates a caller. A host
//! must verify the complete macaroon and its current security/time binding
//! before using a compiled policy. Mutations, third-party discharge protocols,
//! durable permits and policy administration are not implemented by this IR.
//!
//! Scopes intersect; predicates conjoin; resource ceilings take the minimum.
//! Vertex predicates run on the unprojected row. Property masks apply only to
//! the authorized projection. Both endpoints of an edge must be visible before
//! the edge may enter an adjacency index, path search, degree or aggregate.

#![forbid(unsafe_code)]

mod codec;

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, GraphId, VId};

pub const MAX_READ_CAVEATS: usize = 64;
pub const MAX_SCOPE_ITEMS: usize = 128;
pub const MAX_CAVEAT_BYTES: usize = 4096;
pub const MAX_POLICY_BYTES: usize = 32768;
pub const MAX_POLICY_SCALAR_BYTES: usize = 1024;

/// Diagnostics deliberately contain no identifiers, scalar values or tokens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyError {
    Limit,
    NonCanonical,
    Unsupported,
    InvalidWindow,
    InvalidScalar,
}
impl core::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "read policy rejected: {self:?}")
    }
}
impl core::error::Error for PolicyError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}
impl Comparison {
    /// Only same-kind, non-null canonical scalars compare. These are policy
    /// comparisons, not SQL coercions. Canonical ordering is version-bound.
    #[must_use]
    pub fn matches(self, actual: &CanonicalScalar, expected: &CanonicalScalar) -> bool {
        use core::cmp::Ordering::{Equal, Greater, Less};
        if matches!(actual, CanonicalScalar::Null)
            || matches!(expected, CanonicalScalar::Null)
            || core::mem::discriminant(actual) != core::mem::discriminant(expected)
        {
            return false;
        }
        let order = actual.cmp(expected);
        match self {
            Self::Equal => order == Equal,
            Self::NotEqual => order != Equal,
            Self::Less => order == Less,
            Self::LessOrEqual => order != Greater,
            Self::Greater => order == Greater,
            Self::GreaterOrEqual => order != Less,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PropertyPredicate {
    pub key: PropertyKeyId,
    pub comparison: Comparison,
    pub value: CanonicalScalar,
}
impl core::fmt::Debug for PropertyPredicate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("PropertyPredicate([REDACTED])")
    }
}
impl PropertyPredicate {
    #[must_use]
    pub fn matches(&self, properties: &[(PropertyKeyId, CanonicalScalar)]) -> bool {
        properties
            .iter()
            .find(|(key, _)| *key == self.key)
            .is_some_and(|(_, value)| self.comparison.matches(value, &self.value))
    }
}

/// Logical evaluator ceilings, not allocator-byte or I/O accounting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadLimits {
    pub rows: u64,
    pub work: u64,
    pub scratch: u64,
}
impl ReadLimits {
    pub const UNBOUNDED: Self = Self {
        rows: u64::MAX,
        work: u64::MAX,
        scratch: u64::MAX,
    };
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        Self {
            rows: self.rows.min(other.rows),
            work: self.work.min(other.work),
            scratch: self.scratch.min(other.scratch),
        }
    }
}

/// A first-party restriction. Lists are strictly increasing, duplicate-free
/// catalog identities, not names interpreted against an ambient catalog.
/// Empty lists deny that scope; they never mean unrestricted.
#[derive(Clone, PartialEq, Eq)]
pub enum ReadCaveat {
    Graphs(Vec<GraphId>),
    Branches(Vec<BranchId>),
    /// A finite label scope excludes unlabeled vertices and vertices carrying
    /// ANY label outside the scope. It never reveals a hidden secondary label.
    Labels(Vec<LabelId>),
    EdgeTypes(Vec<RelationId>),
    Properties(Vec<PropertyKeyId>),
    Vertices(Vec<VId>),
    HasLabel(LabelId),
    VertexProperty(PropertyPredicate),
    EdgeProperty(PropertyPredicate),
    /// Closed-open authority-profile ticks. No ambient clock is consulted.
    TimeWindow { not_before: u128, expires_at: u128 },
    /// Closed interval of permitted committed snapshots.
    Snapshots { first: CommitSeq, last: CommitSeq },
    Limits(ReadLimits),
}
impl core::fmt::Debug for ReadCaveat {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ReadCaveat([REDACTED])")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Scope<T>(Option<Vec<T>>);
impl<T: Ord + Clone> Scope<T> {
    #[must_use]
    pub fn contains(&self, value: &T) -> bool {
        self.0.as_ref().is_none_or(|items| items.binary_search(value).is_ok())
    }
    /// None is unrestricted; Some(empty) is deny-all.
    #[must_use]
    pub fn items(&self) -> Option<&[T]> {
        self.0.as_deref()
    }
    fn intersect(&mut self, items: &[T]) {
        match &mut self.0 {
            None => self.0 = Some(items.to_vec()),
            Some(current) => current.retain(|item| items.binary_search(item).is_ok()),
        }
    }
}
impl<T> core::fmt::Debug for Scope<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Scope([REDACTED])")
    }
}

/// Immutable compiled restrictions. Not a permit and deliberately not Default.
/// The ordered source is retained for exact authenticated attenuation, while
/// sets and ceilings are compiled once rather than per row.
#[derive(Clone, PartialEq, Eq)]
pub struct ReadPolicy {
    caveats: Vec<ReadCaveat>,
    graphs: Scope<GraphId>,
    branches: Scope<BranchId>,
    labels: Scope<LabelId>,
    edge_types: Scope<RelationId>,
    properties: Scope<PropertyKeyId>,
    vertices: Scope<VId>,
    required_labels: Vec<LabelId>,
    vertex_predicates: Vec<PropertyPredicate>,
    edge_predicates: Vec<PropertyPredicate>,
    not_before: u128,
    expires_at: u128,
    first_snapshot: CommitSeq,
    last_snapshot: CommitSeq,
    limits: ReadLimits,
}
impl core::fmt::Debug for ReadPolicy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ReadPolicy")
            .field("caveat_count", &self.caveats.len())
            .finish_non_exhaustive()
    }
}
impl ReadPolicy {
    pub fn compile(caveats: &[ReadCaveat]) -> Result<Self, PolicyError> {
        if caveats.len() > MAX_READ_CAVEATS {
            return Err(PolicyError::Limit);
        }
        // Bound every scalar/list and the aggregate BEFORE cloning inputs.
        let mut bytes = 0usize;
        for caveat in caveats {
            let encoded = caveat.to_bytes()?;
            bytes = bytes.checked_add(encoded.len() + 2).ok_or(PolicyError::Limit)?;
            if bytes > MAX_POLICY_BYTES {
                return Err(PolicyError::Limit);
            }
        }
        let mut policy = Self {
            caveats: caveats.to_vec(),
            graphs: Scope(None),
            branches: Scope(None),
            labels: Scope(None),
            edge_types: Scope(None),
            properties: Scope(None),
            vertices: Scope(None),
            required_labels: Vec::new(),
            vertex_predicates: Vec::new(),
            edge_predicates: Vec::new(),
            not_before: 0,
            expires_at: u128::MAX,
            first_snapshot: CommitSeq(0),
            last_snapshot: CommitSeq(u64::MAX),
            limits: ReadLimits::UNBOUNDED,
        };
        for caveat in caveats {
            match caveat {
                ReadCaveat::Graphs(items) => policy.graphs.intersect(items),
                ReadCaveat::Branches(items) => policy.branches.intersect(items),
                ReadCaveat::Labels(items) => policy.labels.intersect(items),
                ReadCaveat::EdgeTypes(items) => policy.edge_types.intersect(items),
                ReadCaveat::Properties(items) => policy.properties.intersect(items),
                ReadCaveat::Vertices(items) => policy.vertices.intersect(items),
                ReadCaveat::HasLabel(label) => policy.required_labels.push(*label),
                ReadCaveat::VertexProperty(predicate) => policy.vertex_predicates.push(predicate.clone()),
                ReadCaveat::EdgeProperty(predicate) => policy.edge_predicates.push(predicate.clone()),
                ReadCaveat::TimeWindow { not_before, expires_at } => {
                    policy.not_before = policy.not_before.max(*not_before);
                    policy.expires_at = policy.expires_at.min(*expires_at);
                }
                ReadCaveat::Snapshots { first, last } => {
                    policy.first_snapshot = policy.first_snapshot.max(*first);
                    policy.last_snapshot = policy.last_snapshot.min(*last);
                }
                ReadCaveat::Limits(limits) => policy.limits = policy.limits.intersect(*limits),
            }
        }
        policy.required_labels.sort_unstable();
        policy.required_labels.dedup();
        Ok(policy)
    }

    /// Recompile the authenticated ordered prefix plus the new conjunction.
    /// Even an apparently broader child mask cannot restore a removed item.
    pub fn attenuate(&self, added: &[ReadCaveat]) -> Result<Self, PolicyError> {
        if added.len() > MAX_READ_CAVEATS.saturating_sub(self.caveats.len()) {
            return Err(PolicyError::Limit);
        }
        Self::compile(added)?;
        let mut combined = self.caveats.clone();
        combined.extend_from_slice(added);
        Self::compile(&combined)
    }

    #[must_use]
    pub fn caveats(&self) -> &[ReadCaveat] { &self.caveats }
    #[must_use]
    pub fn graph_scope(&self) -> &Scope<GraphId> { &self.graphs }
    #[must_use]
    pub fn branch_scope(&self) -> &Scope<BranchId> { &self.branches }
    #[must_use]
    pub fn label_scope(&self) -> &Scope<LabelId> { &self.labels }
    #[must_use]
    pub fn edge_type_scope(&self) -> &Scope<RelationId> { &self.edge_types }
    #[must_use]
    pub fn property_scope(&self) -> &Scope<PropertyKeyId> { &self.properties }
    #[must_use]
    pub fn vertex_scope(&self) -> &Scope<VId> { &self.vertices }
    #[must_use]
    pub fn required_labels(&self) -> &[LabelId] { &self.required_labels }
    #[must_use]
    pub fn vertex_predicates(&self) -> &[PropertyPredicate] { &self.vertex_predicates }
    #[must_use]
    pub fn edge_predicates(&self) -> &[PropertyPredicate] { &self.edge_predicates }
    #[must_use]
    pub fn limits(&self) -> ReadLimits { self.limits }

    #[must_use]
    pub fn allows_snapshot(&self, graph: GraphId, branch: BranchId, at: CommitSeq) -> bool {
        self.graphs.contains(&graph) && self.branches.contains(&branch)
            && at >= self.first_snapshot && at <= self.last_snapshot
    }

    /// Every instant in the trusted observation interval must be usable.
    /// Boundary overlap, reversed intervals and empty intersections deny.
    #[must_use]
    pub fn allows_time_interval(&self, earliest: u128, latest: u128) -> bool {
        earliest <= latest && earliest >= self.not_before && latest < self.expires_at
    }

    #[must_use]
    pub fn allows_vertex(&self, id: VId, labels: &[LabelId], properties: &[(PropertyKeyId, CanonicalScalar)]) -> bool {
        self.vertices.contains(&id)
            && (self.labels.items().is_none() || (!labels.is_empty()
                && labels.iter().all(|label| self.labels.contains(label))))
            && self.required_labels.iter().all(|label| labels.contains(label))
            && self.vertex_predicates.iter().all(|predicate| predicate.matches(properties))
    }

    /// Only the edge-local part. The secure-view caller MUST also authorize
    /// both endpoint vertices before exposing the edge, including self-loops.
    #[must_use]
    pub fn allows_edge(&self, relation: RelationId, properties: &[(PropertyKeyId, CanonicalScalar)]) -> bool {
        self.edge_types.contains(&relation)
            && self.edge_predicates.iter().all(|predicate| predicate.matches(properties))
    }

    /// Filter only after evaluating the row's authorization predicates.
    #[must_use]
    pub fn project_properties(&self, properties: &[(PropertyKeyId, CanonicalScalar)]) -> Vec<(PropertyKeyId, CanonicalScalar)> {
        properties.iter().filter(|(key, _)| self.properties.contains(key)).cloned().collect()
    }
}

#[cfg(test)]
mod tests;
