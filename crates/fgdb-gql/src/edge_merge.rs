//! Bounded relationship MERGE definition.
//!
//! A prepared MATCH produces source/destination vertex columns. Duplicate rows
//! for the same endpoint pair collapse. The singular primitive refuses multiple
//! distinct pairs; native write programs resolve every pair in canonical order
//! and stage their complete batch atomically. The write-capable host resolves
//! the relation in its canonical edge overlay (fgdb-at06q):
//! - An existing edge matches when its relation and orientation agree and
//!   every property of the MERGE map equals the edge's stored value. An
//!   undirected MERGE accepts either orientation.
//! - Zero matching edges creates one, source to destination, carrying exactly
//!   the map's properties.
//! - Every matching edge is a match. The singular primitive refuses parallel
//!   matches as ambiguous; native write programs keep each one.
//!
//! Map values are already-bound non-null canonical scalars. ON MATCH/ON CREATE
//! actions remain above this primitive.

use crate::algebra::{GraphValueRow, PreparedGraphPattern, ValueProjection};
use crate::{GlaExecutionStats, GqlExecutionStats, GqlQueryPolicy, GqlScalarParameter};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};

pub const MAX_GRAPH_EDGE_MERGE_PROPERTIES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeMergeBuildError {
    SourceColumn {
        column: usize,
    },
    DestinationColumn {
        column: usize,
    },
    TooManyProperties {
        limit: usize,
        observed: usize,
    },
    DuplicateProperty,
    /// A NULL map value can neither match nor be created (openCypher).
    NullProperty {
        key: PropertyKeyId,
    },
}
impl core::fmt::Display for GraphEdgeMergeBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph relationship MERGE definition: {self:?}")
    }
}
impl core::error::Error for GraphEdgeMergeBuildError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphEdgeMergePolicy {
    /// Cumulative MATCH + endpoint reduction + all relationship existence scans
    /// and batch receipt ownership, under one work/scratch/source allowance.
    pub query: GqlQueryPolicy,
    /// Creation proposals only. Zero permits Matched and NoInput outcomes but
    /// rejects a missing relationship before external identity allocation.
    pub max_created_edges: u64,
}
impl GraphEdgeMergePolicy {
    #[must_use]
    pub const fn new(query: GqlQueryPolicy) -> Self {
        Self {
            query,
            max_created_edges: 1,
        }
    }
    #[must_use]
    pub const fn with_creation_limit(mut self, limit: u64) -> Self {
        self.max_created_edges = limit;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphEdgeMergeStats {
    pub match_selection: GqlExecutionStats,
    /// Visible overlay edge records examined by all relationship existence
    /// scans. Repeated physical scans count again, including matched pairs.
    pub overlay_edges: u64,
    pub evaluator: GlaExecutionStats,
    pub created_edges: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphEdgeMergeOutcome {
    /// No endpoint row exists; as in a MATCH-fed MERGE pipeline, no edge is made.
    NoInput,
    Matched(EId),
    Created(EId),
}
impl GraphEdgeMergeOutcome {
    #[must_use]
    pub const fn edge(self) -> Option<EId> {
        match self {
            Self::NoInput => None,
            Self::Matched(edge) | Self::Created(edge) => Some(edge),
        }
    }
    #[must_use]
    pub const fn created(self) -> bool {
        matches!(self, Self::Created(_))
    }
}
impl core::fmt::Debug for GraphEdgeMergeOutcome {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoInput => f.write_str("NoInput"),
            Self::Matched(_) => f
                .debug_struct("Matched")
                .field("edge", &"[REDACTED]")
                .finish(),
            Self::Created(_) => f
                .debug_struct("Created")
                .field("edge", &"[REDACTED]")
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphEdgeMergeRequest;

#[derive(Debug)]
pub enum GraphEdgeMergeError<E, A> {
    Source(E),
    IdentitySource(A),
    InvalidSourceStatistics,
    InputSchema { row: usize, column: usize },
    NullEndpoint { row: usize, column: usize },
    AmbiguousEndpointPairs { observed: u64 },
    AmbiguousRelationships { observed: u64 },
    CreationLimit { limit: u64, observed: u128 },
    IdentityKind,
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphEdgeMergeError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::IdentitySource(error) => {
                write!(f, "relationship MERGE identity allocation: {error}")
            }
            Self::InvalidSourceStatistics => {
                f.write_str("relationship MERGE source returned inconsistent statistics")
            }
            Self::InputSchema { row, column } => write!(
                f,
                "relationship MERGE row {row} has incompatible column {column}"
            ),
            Self::NullEndpoint { row, column } => write!(
                f,
                "relationship MERGE row {row} has null endpoint column {column}"
            ),
            Self::AmbiguousEndpointPairs { observed } => write!(
                f,
                "relationship MERGE selected {observed} distinct endpoint pairs; one required"
            ),
            Self::AmbiguousRelationships { observed } => write!(
                f,
                "relationship MERGE found {observed} parallel relationships; one required"
            ),
            Self::CreationLimit { limit, observed } => write!(
                f,
                "relationship MERGE creation limit exceeded: {observed} > {limit}"
            ),
            Self::IdentityKind => {
                f.write_str("relationship MERGE allocator returned a non-edge identity")
            }
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphEdgeMergeError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::IdentitySource(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeMerge {
    selection: PreparedGraphPattern<GraphValueRow>,
    relation: RelationId,
    source: usize,
    destination: usize,
    properties: Vec<(PropertyKeyId, GqlScalarParameter)>,
    undirected: bool,
}
impl core::fmt::Debug for PreparedGraphEdgeMerge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeMerge")
            .field("properties", &self.properties.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphEdgeMerge {
    pub fn prepare(
        selection: PreparedGraphPattern<GraphValueRow>,
        relation: RelationId,
        source: usize,
        destination: usize,
        mut properties: Vec<(PropertyKeyId, GqlScalarParameter)>,
    ) -> Result<Self, GraphEdgeMergeBuildError> {
        let columns = selection.value_columns();
        if !matches!(columns.get(source), Some(ValueProjection::Vertex { .. })) {
            return Err(GraphEdgeMergeBuildError::SourceColumn { column: source });
        }
        if !matches!(
            columns.get(destination),
            Some(ValueProjection::Vertex { .. })
        ) {
            return Err(GraphEdgeMergeBuildError::DestinationColumn {
                column: destination,
            });
        }
        if properties.len() > MAX_GRAPH_EDGE_MERGE_PROPERTIES {
            return Err(GraphEdgeMergeBuildError::TooManyProperties {
                limit: MAX_GRAPH_EDGE_MERGE_PROPERTIES,
                observed: properties.len(),
            });
        }
        properties.sort_by_key(|(key, _)| *key);
        if properties.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(GraphEdgeMergeBuildError::DuplicateProperty);
        }
        if let Some((key, _)) = properties
            .iter()
            .find(|(_, value)| matches!(value.value(), CanonicalScalar::Null))
        {
            return Err(GraphEdgeMergeBuildError::NullProperty { key: *key });
        }
        Ok(Self {
            selection,
            relation,
            source,
            destination,
            properties,
            undirected: false,
        })
    }

    /// An undirected MERGE, `(a)-[:R]-(b)` (fgdb-at06q): an existing edge in
    /// either orientation matches, and a new edge goes source to destination.
    #[must_use]
    pub fn with_undirected(mut self) -> Self {
        self.undirected = true;
        self
    }

    #[must_use]
    pub const fn undirected(&self) -> bool {
        self.undirected
    }

    /// Whether `(src, dst)` is an orientation this MERGE accepts for the
    /// endpoint pair `(source, destination)`.
    #[must_use]
    pub fn accepts_orientation(
        &self,
        (source, destination): (VId, VId),
        (src, dst): (VId, VId),
    ) -> bool {
        (src == source && dst == destination)
            || (self.undirected && src == destination && dst == source)
    }

    /// Whether an existing edge's stored properties satisfy this MERGE's map:
    /// every map property is present and `=` its value, as a relationship
    /// pattern map filters MATCH. Properties outside the map are ignored, and
    /// an empty map accepts every edge.
    #[must_use]
    pub fn matches_properties<'s>(
        &self,
        mut stored: impl FnMut(PropertyKeyId) -> Option<&'s CanonicalScalar>,
    ) -> bool {
        self.properties.iter().all(|(key, wanted)| {
            crate::algebra::IntegerComparison::Equal
                .evaluate_scalar_pair(stored(*key), Some(wanted.value()))
                == Some(true)
        })
    }

    #[must_use]
    pub fn selection(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.selection
    }
    #[must_use]
    pub const fn relation(&self) -> RelationId {
        self.relation
    }
    #[must_use]
    pub const fn source_column(&self) -> usize {
        self.source
    }
    #[must_use]
    pub const fn destination_column(&self) -> usize {
        self.destination
    }
    #[must_use]
    pub fn properties(&self) -> &[(PropertyKeyId, GqlScalarParameter)] {
        &self.properties
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        // A directed definition keeps its original transcript.
        let mut bytes = if self.undirected {
            b"fgdb:undirected-edge-merge:v1\0".to_vec()
        } else {
            b"fgdb:directed-edge-merge:v1\0".to_vec()
        };
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        bytes.extend_from_slice(&(self.source as u64).to_be_bytes());
        bytes.extend_from_slice(&(self.destination as u64).to_be_bytes());
        let selection = self.selection.canonical_bytes();
        bytes.extend_from_slice(&(selection.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&selection);
        bytes.extend_from_slice(&(self.properties.len() as u64).to_be_bytes());
        for (key, value) in &self.properties {
            bytes.extend_from_slice(&key.0.to_be_bytes());
            bytes.extend_from_slice(&(value.canonical_bytes().len() as u64).to_be_bytes());
            bytes.extend_from_slice(value.canonical_bytes());
        }
        bytes
    }
}
