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
    /// A row relation input exceeds the parent depth a write admits.
    RelationalInput(crate::GraphSetBuildError),
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
    /// A row relation input (`MATCH ... WITH ... MERGE`, or a chained write's
    /// owned rows) failed before any existence scan or identity allocation.
    InputRelation(crate::GraphSetExecutionError<E>),
    IdentitySource(A),
    InvalidSourceStatistics,
    InputSchema {
        row: usize,
        column: usize,
    },
    NullEndpoint {
        row: usize,
        column: usize,
    },
    AmbiguousEndpointPairs {
        observed: u64,
    },
    AmbiguousRelationships {
        observed: u64,
    },
    CreationLimit {
        limit: u64,
        observed: u128,
    },
    IdentityKind,
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphEdgeMergeError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::InputRelation(error) => write!(f, "relationship MERGE input relation: {error}"),
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
            Self::InputRelation(error) => Some(error),
            Self::IdentitySource(error) => Some(error),
            _ => None,
        }
    }
}

/// One endpoint's selection: its labels and property equality values
/// (canonical encodings, kinds included), both sorted.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct EdgeMergeSlotKey {
    pub(crate) labels: Vec<fgdb_delta_types::LabelId>,
    pub(crate) properties: Vec<(PropertyKeyId, Vec<u8>)>,
}

/// Both endpoints' equality keys (PreparedGraphEdgeMerge::endpoint_key).
/// Two MERGEs of one shape (same relation, labels and property keys) whose
/// keys differ select disjoint endpoint pairs: a vertex holds one value per
/// property, so it can satisfy at most one of two distinct equalities.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct EdgeMergeEndpointKey {
    pub(crate) source: EdgeMergeSlotKey,
    pub(crate) destination: EdgeMergeSlotKey,
}

impl EdgeMergeEndpointKey {
    /// The labels and property keys, without values.
    pub(crate) fn shape(&self) -> [(&[fgdb_delta_types::LabelId], Vec<PropertyKeyId>); 2] {
        [&self.source, &self.destination].map(|slot| {
            (
                slot.labels.as_slice(),
                slot.properties.iter().map(|(key, _)| *key).collect(),
            )
        })
    }
}

/// Where the endpoint rows come from.
#[derive(Clone, PartialEq, Eq)]
enum Input {
    /// The MERGE's own MATCH prefix.
    Pattern(PreparedGraphPattern<GraphValueRow>),
    /// A row relation: the rows a `MATCH ... WITH` pipeline returns, or the
    /// previous write's owned rows in a chain (`SetNode::Owned`).
    Relation(Box<crate::PreparedGraphSet>),
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeMerge {
    input: Input,
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
        properties: Vec<(PropertyKeyId, GqlScalarParameter)>,
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
        Self::with_input(
            Input::Pattern(selection),
            relation,
            source,
            destination,
            properties,
        )
    }

    /// MERGE over a row relation: `MATCH ... WITH ... MERGE (a)-[:R]->(b)`,
    /// or a chained write's owned rows (`CREATE (a), (b) WITH * MERGE ...`).
    /// The endpoints are vertex columns of that row; every row's pair
    /// resolves as a MATCH-selected one does.
    pub fn prepare_relation(
        input: crate::PreparedGraphSet,
        relation: RelationId,
        source: usize,
        destination: usize,
        properties: Vec<(PropertyKeyId, GqlScalarParameter)>,
    ) -> Result<Self, GraphEdgeMergeBuildError> {
        let types = input.column_types();
        if types.get(source) != Some(&crate::GraphSetColumnType::Vertex) {
            return Err(GraphEdgeMergeBuildError::SourceColumn { column: source });
        }
        if types.get(destination) != Some(&crate::GraphSetColumnType::Vertex) {
            return Err(GraphEdgeMergeBuildError::DestinationColumn {
                column: destination,
            });
        }
        input
            .check_parent_depth()
            .map_err(GraphEdgeMergeBuildError::RelationalInput)?;
        Self::with_input(
            Input::Relation(Box::new(input)),
            relation,
            source,
            destination,
            properties,
        )
    }

    fn with_input(
        input: Input,
        relation: RelationId,
        source: usize,
        destination: usize,
        mut properties: Vec<(PropertyKeyId, GqlScalarParameter)>,
    ) -> Result<Self, GraphEdgeMergeBuildError> {
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
            input,
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

    /// The MERGE's own MATCH prefix; `None` over a row relation.
    #[must_use]
    pub fn selection(&self) -> Option<&PreparedGraphPattern<GraphValueRow>> {
        match &self.input {
            Input::Pattern(pattern) => Some(pattern),
            Input::Relation(_) => None,
        }
    }

    /// The row relation input; `None` with a MATCH prefix.
    #[must_use]
    pub fn input_relation(&self) -> Option<&crate::PreparedGraphSet> {
        match &self.input {
            Input::Pattern(_) => None,
            Input::Relation(relation) => Some(relation),
        }
    }

    /// The domain of each input row column, endpoints included.
    #[must_use]
    pub fn input_column_types(&self) -> Vec<crate::GraphSetColumnType> {
        match &self.input {
            Input::Pattern(pattern) => pattern
                .value_columns()
                .iter()
                .map(crate::GraphSetColumnType::from)
                .collect(),
            Input::Relation(relation) => relation.column_types().to_vec(),
        }
    }

    /// The endpoint rows: the MATCH prefix through `patterns`, or the row
    /// relation, whose owned leaf (a chain) reads `owned`. Rows a pattern or
    /// a leafless relation cannot read refuse rather than vanish.
    pub fn select_governed<E, A, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GqlQueryPolicy,
        mut patterns: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<
            crate::GqlQueryExecution<GraphValueRow>,
            crate::GqlQueryError<E, C>,
        >,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<
        crate::GqlQueryExecution<GraphValueRow>,
        crate::GqlQueryError<GraphEdgeMergeError<E, A>, C>,
    > {
        match &self.input {
            Input::Pattern(pattern) => {
                if owned.is_some() {
                    return Err(crate::GqlQueryError::Source(
                        GraphEdgeMergeError::InputRelation(
                            crate::GraphSetExecutionError::OwnedRowsUnavailable,
                        ),
                    ));
                }
                patterns(pattern, policy)
                    .map_err(|error| error.map_source(GraphEdgeMergeError::Source))
            }
            Input::Relation(relation) => match owned {
                None => relation.execute_governed(policy, patterns, checkpoint),
                Some(rows) => relation.execute_with_source(
                    policy,
                    &mut crate::set_ops::WithOwnedRows {
                        source: &mut patterns,
                        rows: Some(rows),
                    },
                    checkpoint,
                ),
            }
            .map_err(|error| error.map_source(GraphEdgeMergeError::InputRelation)),
        }
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

    /// The equality keys of a directed MERGE whose MATCH is two independent
    /// vertex scans, each selected only by labels and non-null property
    /// equalities, as `MATCH (a:N {id: $s}), (b:N {id: $d})` is (fgdb-2l3pt).
    /// Anything else (a relationship read, another predicate, an undirected
    /// or self-loop MERGE, a row relation input) is `None`.
    pub(crate) fn endpoint_key(&self) -> Option<EdgeMergeEndpointKey> {
        use crate::algebra::{GlaOperator, IntegerComparison, VertexPredicate};
        let Input::Pattern(selection) = &self.input else {
            return None;
        };
        if self.undirected || selection.plan().reads_edges() {
            return None;
        }
        let mut slots: Vec<EdgeMergeSlotKey> = Vec::new();
        for operator in selection.plan().operators() {
            match operator {
                GlaOperator::ScanVertices => slots.push(EdgeMergeSlotKey::default()),
                GlaOperator::Select { slot, predicates } => {
                    let key = slots.get_mut(slot.ordinal() as usize)?;
                    for predicate in predicates {
                        match predicate {
                            VertexPredicate::HasLabel(label) => key.labels.push(*label),
                            VertexPredicate::IntegerProperty {
                                key: property,
                                comparison: IntegerComparison::Equal,
                                value,
                            } => key
                                .properties
                                .push((*property, CanonicalScalar::Int(*value).encode().ok()?)),
                            VertexPredicate::ScalarProperty {
                                key: property,
                                predicate,
                            } if predicate.comparison() == IntegerComparison::Equal
                                && !matches!(predicate.value(), CanonicalScalar::Null) =>
                            {
                                key.properties
                                    .push((*property, predicate.value().encode().ok()?));
                            }
                            _ => return None,
                        }
                    }
                }
                GlaOperator::ProjectValues { .. }
                | GlaOperator::Distinct
                | GlaOperator::OrderByValues
                | GlaOperator::Limit { .. } => {}
                _ => return None,
            }
        }
        let slot = |column: usize| match selection.value_columns().get(column)? {
            ValueProjection::Vertex { slot } => Some(slot.ordinal() as usize),
            _ => None,
        };
        let (source, destination) = (slot(self.source)?, slot(self.destination)?);
        if source == destination {
            return None;
        }
        let mut key = |slot: usize| {
            let mut key = slots.get_mut(slot).map(core::mem::take)?;
            if key.properties.is_empty() {
                return None;
            }
            key.labels.sort_unstable();
            key.properties.sort_unstable();
            Some(key)
        };
        Some(EdgeMergeEndpointKey {
            source: key(source)?,
            destination: key(destination)?,
        })
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        // A directed definition keeps its original transcript; a row
        // relation input has its own domain.
        let mut bytes = match (&self.input, self.undirected) {
            (Input::Relation(_), false) => b"fgdb:relational-directed-edge-merge:v1\0".to_vec(),
            (Input::Relation(_), true) => b"fgdb:relational-undirected-edge-merge:v1\0".to_vec(),
            (Input::Pattern(_), true) => b"fgdb:undirected-edge-merge:v1\0".to_vec(),
            (Input::Pattern(_), false) => b"fgdb:directed-edge-merge:v1\0".to_vec(),
        };
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        bytes.extend_from_slice(&(self.source as u64).to_be_bytes());
        bytes.extend_from_slice(&(self.destination as u64).to_be_bytes());
        let selection = match &self.input {
            Input::Pattern(pattern) => pattern.canonical_bytes(),
            Input::Relation(relation) => relation.canonical_bytes(),
        };
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
