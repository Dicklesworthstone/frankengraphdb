//! Graph creation through the existing GLA and write pipeline.
//!
//! Every selected occurrence creates its own vertices and edges. A declaration
//! can connect a matched vertex to a vertex created for that SAME occurrence.
//! Standalone creation consumes the relational unit: one zero-column occurrence,
//! not a synthetic graph vertex, a scan, or an empty match that creates nothing.
//! Properties are frozen before identity allocation. The caller supplies fresh
//! typed identities; no identity is inferred from graph size, time or row data.
//! Proposals are private staging inputs, not durable effects or commit receipts.

mod collect;

use crate::algebra::{GraphValueRow, MAX_PATTERN_VERTICES, PreparedGraphPattern};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphIntegerError, GraphMutationValue, GraphSetColumnType, GraphSetExecutionError,
    GraphSetProjection, GraphSetValue, PreparedGraphSet,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, EId, VId};

pub const MAX_GRAPH_INSERT_DECLARATIONS: usize = 256;
pub const MAX_GRAPH_INSERT_FIELDS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphInsertEndpoint {
    /// A vertex column of the frozen selection. A null endpoint is an error.
    Column(usize),
    /// Zero-based vertex declaration, instantiated separately for each row.
    CreatedVertex(usize),
}

#[derive(Clone, PartialEq, Eq)]
pub struct GraphInsertVertex {
    pub labels: Vec<LabelId>,
    pub properties: Vec<(PropertyKeyId, GraphMutationValue)>,
}
impl core::fmt::Debug for GraphInsertVertex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphInsertVertex([REDACTED])")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct GraphInsertEdge {
    pub source: GraphInsertEndpoint,
    pub destination: GraphInsertEndpoint,
    pub relation: RelationId,
    pub properties: Vec<(PropertyKeyId, GraphMutationValue)>,
}
impl core::fmt::Debug for GraphInsertEdge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphInsertEdge([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphInsertBuildError {
    RelationalInput(crate::GraphSetBuildError),
    Empty,
    TooManyDeclarations {
        limit: usize,
        observed: usize,
    },
    TooManyFields {
        limit: usize,
        observed: usize,
    },
    DuplicateLabel {
        vertex: usize,
    },
    DuplicateProperty {
        declaration: usize,
    },
    ValueColumn {
        declaration: usize,
        column: usize,
    },
    /// A composite value that does not fit the input schema or can never be a
    /// scalar property value.
    ValueExpression {
        declaration: usize,
        property: usize,
        kind: Option<crate::GraphSetProjectionError>,
    },
    EndpointColumn {
        edge: usize,
        column: usize,
    },
    CreatedEndpoint {
        edge: usize,
        vertex: usize,
    },
}
impl core::fmt::Display for GraphInsertBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph insertion definition: {self:?}")
    }
}
impl core::error::Error for GraphInsertBuildError {}

/// Calls occur in canonical selected-row order: vertices, then edges per row.
/// Standalone creation has exactly row zero. A request is not an ID and grants
/// no allocation authority by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphInsertRequest {
    Vertex { row: usize, vertex: usize },
    Edge { row: usize, edge: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphInsertLimitDimension {
    Vertices,
    Edges,
}

#[derive(Debug)]
pub enum GraphInsertError<E, A> {
    Source(E),
    /// The complete relational source failed before identity allocation.
    InputRelation(GraphSetExecutionError<E>),
    IdentitySource(A),
    InvalidSourceStatistics,
    InputSchema {
        row: usize,
        column: usize,
    },
    NullEndpoint {
        row: usize,
        edge: usize,
        column: usize,
    },
    Arithmetic {
        row: usize,
        declaration: usize,
        property: usize,
        error: GraphIntegerError,
    },
    IdentityKind {
        request: GraphInsertRequest,
    },
    DuplicateIdentity {
        request: GraphInsertRequest,
    },
    Limit {
        dimension: GraphInsertLimitDimension,
        limit: u64,
        observed: u128,
    },
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display for GraphInsertError<E, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::InputRelation(error) => write!(f, "insertion input relation: {error}"),
            Self::IdentitySource(error) => write!(f, "insertion identity allocation: {error}"),
            Self::InvalidSourceStatistics => {
                f.write_str("insertion source returned inconsistent statistics")
            }
            Self::InputSchema { row, column } => {
                write!(f, "insertion row {row} has incompatible column {column}")
            }
            Self::NullEndpoint { row, edge, column } => write!(
                f,
                "insertion row {row} edge {edge} has null endpoint column {column}"
            ),
            Self::Arithmetic {
                row,
                declaration,
                property,
                error,
            } => write!(
                f,
                "insertion row {row} declaration {declaration} property {property}: {error}"
            ),
            Self::IdentityKind { request } => write!(
                f,
                "insertion allocator returned wrong identity kind for {request:?}"
            ),
            Self::DuplicateIdentity { request } => {
                write!(f, "insertion allocator repeated an identity at {request:?}")
            }
            Self::Limit {
                dimension,
                limit,
                observed,
            } => write!(f, "insertion {dimension:?} limit: {observed} > {limit}"),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphInsertError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::InputRelation(error) => Some(error),
            Self::IdentitySource(error) => Some(error),
            Self::Arithmetic { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Bounds matching, computed values, identity bookkeeping and private proposals.
/// These are not storage-preparation, allocation-service, or commit cost limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphInsertPolicy {
    pub query: GqlQueryPolicy,
    pub max_vertices: u64,
    pub max_edges: u64,
}
impl GraphInsertPolicy {
    #[must_use]
    pub const fn new(query: GqlQueryPolicy, max_vertices: u64, max_edges: u64) -> Self {
        Self {
            query,
            max_vertices,
            max_edges,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphInsertStats {
    /// Standalone creation admits zero graph records and one unit occurrence.
    pub selection: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
    pub created_vertices: u64,
    pub created_edges: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub enum GraphInsertIntent {
    Vertex {
        vertex: VId,
        labels: Vec<LabelId>,
        properties: Vec<(PropertyKeyId, CanonicalScalar)>,
    },
    Edge {
        edge: EId,
        relation: RelationId,
        source: VId,
        destination: VId,
        properties: Vec<(PropertyKeyId, CanonicalScalar)>,
    },
}
impl core::fmt::Debug for GraphInsertIntent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphInsertIntent([REDACTED])")
    }
}

#[derive(PartialEq, Eq)]
pub struct GraphInsertBatch {
    intents: Vec<GraphInsertIntent>,
    stats: GraphInsertStats,
}
impl GraphInsertBatch {
    #[must_use]
    pub fn intents(&self) -> &[GraphInsertIntent] {
        &self.intents
    }
    #[must_use]
    pub const fn stats(&self) -> GraphInsertStats {
        self.stats
    }
    #[must_use]
    pub fn into_intents(self) -> Vec<GraphInsertIntent> {
        self.intents
    }
}
impl core::fmt::Debug for GraphInsertBatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphInsertBatch")
            .field("stats", &self.stats)
            .field("intents", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Properties {
    keys: Vec<PropertyKeyId>,
    projection: Vec<GraphSetProjection>,
}
#[derive(Clone, PartialEq, Eq)]
struct Vertex {
    labels: Vec<LabelId>,
    properties: Properties,
}
#[derive(Clone, PartialEq, Eq)]
struct Edge {
    relation: RelationId,
    source: GraphInsertEndpoint,
    destination: GraphInsertEndpoint,
    properties: Properties,
}

#[derive(Clone, PartialEq, Eq)]
enum Input {
    Unit,
    Pattern(PreparedGraphPattern<GraphValueRow>),
    // Boxed: a prepared relation is several times the size of the other arms.
    Relation(Box<PreparedGraphSet>),
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphInsert {
    input: Input,
    column_types: Vec<GraphSetColumnType>,
    relation: RelationId,
    vertices: Vec<Vertex>,
    edges: Vec<Edge>,
}
impl core::fmt::Debug for PreparedGraphInsert {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphInsert")
            .field("vertices_per_row", &self.vertices.len())
            .field("edges_per_row", &self.edges.len())
            .field("standalone", &self.is_standalone())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
/// A returning insertion's batch and its RETURN rows, or the insertion error.
type ReturningInsert<E, A, C> =
    Result<(GraphInsertBatch, Vec<GraphValueRow>), GqlQueryError<GraphInsertError<E, A>, C>>;
impl PreparedGraphInsert {
    /// Freeze a creation template. Properties read only the original selection;
    /// created vertices may be edge endpoints but are not RHS temporaries.
    /// Duplicate label/property declarations refuse instead of choosing a value.
    /// Each edge carries its relation; the supplied coordinate places vertices.
    pub fn prepare(
        selection: PreparedGraphPattern<GraphValueRow>,
        relation: RelationId,
        vertices: Vec<GraphInsertVertex>,
        edges: Vec<GraphInsertEdge>,
    ) -> Result<Self, GraphInsertBuildError> {
        Self::prepare_input(Input::Pattern(selection), relation, vertices, edges)
    }

    /// Create one structure for every occurrence of a typed relation, including
    /// source-free UNWIND, projection, filtering and set composition. The whole
    /// relation is evaluated before properties or identities are collected.
    /// Its row order and duplicates are preserved. Dynamic scalar columns are
    /// checked against their actual values before requesting any identity.
    pub fn prepare_relation(
        selection: PreparedGraphSet,
        relation: RelationId,
        vertices: Vec<GraphInsertVertex>,
        edges: Vec<GraphInsertEdge>,
    ) -> Result<Self, GraphInsertBuildError> {
        selection
            .check_parent_depth()
            .map_err(GraphInsertBuildError::RelationalInput)?;
        Self::prepare_input(
            Input::Relation(Box::new(selection)),
            relation,
            vertices,
            edges,
        )
    }

    /// Create one structure without scanning the graph. The source schema is
    /// empty, so every column reference (even in an unselected CASE branch)
    /// refuses during preparation. Edges may refer to the created vertices.
    /// Constants and already-bound scalar programs use the ordinary collector.
    pub fn prepare_standalone(
        relation: RelationId,
        vertices: Vec<GraphInsertVertex>,
        edges: Vec<GraphInsertEdge>,
    ) -> Result<Self, GraphInsertBuildError> {
        Self::prepare_input(Input::Unit, relation, vertices, edges)
    }

    fn prepare_input(
        input: Input,
        relation: RelationId,
        vertices: Vec<GraphInsertVertex>,
        edges: Vec<GraphInsertEdge>,
    ) -> Result<Self, GraphInsertBuildError> {
        let declarations = vertices.len().saturating_add(edges.len());
        if declarations == 0 {
            return Err(GraphInsertBuildError::Empty);
        }
        if declarations > MAX_GRAPH_INSERT_DECLARATIONS {
            return Err(GraphInsertBuildError::TooManyDeclarations {
                limit: MAX_GRAPH_INSERT_DECLARATIONS,
                observed: declarations,
            });
        }
        let fields = vertices.iter().fold(0_usize, |sum, v| {
            sum.saturating_add(v.labels.len())
                .saturating_add(v.properties.len())
        });
        let fields = edges
            .iter()
            .fold(fields, |sum, e| sum.saturating_add(e.properties.len()));
        if fields > MAX_GRAPH_INSERT_FIELDS {
            return Err(GraphInsertBuildError::TooManyFields {
                limit: MAX_GRAPH_INSERT_FIELDS,
                observed: fields,
            });
        }
        let column_types = match &input {
            Input::Unit => Vec::new(),
            Input::Pattern(pattern) => pattern
                .value_columns()
                .iter()
                .map(GraphSetColumnType::from)
                .collect(),
            Input::Relation(relation) => relation.column_types().to_vec(),
        };
        let columns = &column_types;
        for (edge, declaration) in edges.iter().enumerate() {
            for endpoint in [declaration.source, declaration.destination] {
                match endpoint {
                    GraphInsertEndpoint::Column(column) => {
                        if !matches!(columns.get(column), Some(GraphSetColumnType::Vertex)) {
                            return Err(GraphInsertBuildError::EndpointColumn { edge, column });
                        }
                    }
                    GraphInsertEndpoint::CreatedVertex(vertex) if vertex >= vertices.len() => {
                        return Err(GraphInsertBuildError::CreatedEndpoint { edge, vertex });
                    }
                    GraphInsertEndpoint::CreatedVertex(_) => {}
                }
            }
        }
        let vertex_count = vertices.len();
        let mut bound_vertices = Vec::new();
        for (vertex, mut declaration) in vertices.into_iter().enumerate() {
            declaration.labels.sort_unstable();
            if declaration.labels.windows(2).any(|pair| pair[0] == pair[1]) {
                return Err(GraphInsertBuildError::DuplicateLabel { vertex });
            }
            bound_vertices.push(Vertex {
                labels: declaration.labels,
                properties: Properties::prepare(declaration.properties, columns, vertex)?,
            });
        }
        let mut bound_edges = Vec::new();
        for (edge, declaration) in edges.into_iter().enumerate() {
            bound_edges.push(Edge {
                relation: declaration.relation,
                source: declaration.source,
                destination: declaration.destination,
                properties: Properties::prepare(
                    declaration.properties,
                    columns,
                    vertex_count + edge,
                )?,
            });
        }
        Ok(Self {
            input,
            column_types,
            relation,
            vertices: bound_vertices,
            edges: bound_edges,
        })
    }

    /// The original graph pattern for pattern-selected creation. Returns None
    /// for both standalone creation and relational input; use `is_standalone`
    /// or `relational_selection` to distinguish those cases.
    #[must_use]
    pub fn selection(&self) -> Option<&PreparedGraphPattern<GraphValueRow>> {
        match &self.input {
            Input::Pattern(pattern) => Some(pattern),
            Input::Unit | Input::Relation(_) => None,
        }
    }
    #[must_use]
    pub fn relational_selection(&self) -> Option<&PreparedGraphSet> {
        match &self.input {
            Input::Relation(relation) => Some(relation.as_ref()),
            Input::Unit | Input::Pattern(_) => None,
        }
    }
    /// True only for one implicit zero-column occurrence. A source-free
    /// relation may produce zero, one or many occurrences instead.
    #[must_use]
    pub const fn is_standalone(&self) -> bool {
        matches!(self.input, Input::Unit)
    }
    /// Whether evaluating this input requires access to a graph source.
    #[must_use]
    pub const fn requires_read(&self) -> bool {
        match &self.input {
            Input::Unit => false,
            Input::Pattern(_) => true,
            Input::Relation(relation) => relation.operand_count() != 0,
        }
    }
    #[must_use]
    pub const fn relation(&self) -> RelationId {
        self.relation
    }
    pub(crate) fn input_column_types(&self) -> &[GraphSetColumnType] {
        &self.column_types
    }
    #[must_use]
    pub fn vertices_per_row(&self) -> usize {
        self.vertices.len()
    }
    #[must_use]
    pub fn edges_per_row(&self) -> usize {
        self.edges.len()
    }

    /// Execute the frozen selection once; validate every row/endpoint/property;
    /// then allocate typed IDs and assemble one complete private proposal.
    /// The source callback is not called for standalone or source-free relational
    /// creation. Each selected occurrence consumes the selected-row allowance.
    /// When a relation has multiple graph leaves, the host must supply one pinned
    /// snapshot/transaction overlay and authorization context for every call.
    /// No allocator call precedes data validation and creation-count admission.
    /// Empty MATCH selections allocate no IDs. Duplicate occurrences are NOT merged.
    ///
    /// Allocators must supply fresh identities under the host's identity policy.
    /// Already issued IDs are not reclaimed on failure or cancellation. Duplicate
    /// IDs within this batch and wrong kinds refuse here; collision with live or
    /// retired storage identities is checked by ordinary write preparation.
    /// The definition transcript does not pin nondeterministic allocator output;
    /// replay must retain the actual request-to-identity mapping separately.
    pub fn execute_governed<E, A, C>(
        &self,
        policy: GraphInsertPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphInsertBatch, GqlQueryError<GraphInsertError<E, A>, C>> {
        collect::execute(self, policy, None, source, allocate, checkpoint).map(|(batch, _)| batch)
    }

    pub(crate) fn execute_returning<E, A, C>(
        &self,
        policy: GraphInsertPolicy,
        bindings: &[crate::insertion_query::GraphInsertBinding],
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> ReturningInsert<E, A, C> {
        collect::execute(self, policy, Some(bindings), source, allocate, checkpoint)
    }

    /// Application definition, not a durable effect encoding or allocation log.
    /// Edge relations are included alongside their endpoints and properties.
    /// Unit, graph-pattern and relational inputs have distinct domains.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = match &self.input {
            Input::Pattern(_) => b"fgdb:query-graph-insert:v1\0".to_vec(),
            Input::Unit => b"fgdb:standalone-graph-insert:v1\0".to_vec(),
            Input::Relation(_) => b"fgdb:relational-graph-insert:v1\0".to_vec(),
        };
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        let selection = match &self.input {
            Input::Unit => Vec::new(),
            Input::Pattern(pattern) => pattern.canonical_bytes(),
            Input::Relation(relation) => relation.canonical_bytes(),
        };
        bytes.extend_from_slice(&(selection.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&selection);
        bytes.extend_from_slice(&(self.vertices.len() as u64).to_be_bytes());
        for vertex in &self.vertices {
            bytes.extend_from_slice(&(vertex.labels.len() as u64).to_be_bytes());
            for label in &vertex.labels {
                bytes.extend_from_slice(&label.0.to_be_bytes());
            }
            vertex.properties.append(&mut bytes);
        }
        bytes.extend_from_slice(&(self.edges.len() as u64).to_be_bytes());
        for edge in &self.edges {
            bytes.extend_from_slice(&edge.relation.0.to_be_bytes());
            for endpoint in [edge.source, edge.destination] {
                let (tag, index) = match endpoint {
                    GraphInsertEndpoint::Column(index) => (0, index),
                    GraphInsertEndpoint::CreatedVertex(index) => (1, index),
                };
                bytes.push(tag);
                bytes.extend_from_slice(&(index as u64).to_be_bytes());
            }
            edge.properties.append(&mut bytes);
        }
        bytes
    }
}

impl Properties {
    fn prepare(
        mut fields: Vec<(PropertyKeyId, GraphMutationValue)>,
        columns: &[GraphSetColumnType],
        declaration: usize,
    ) -> Result<Self, GraphInsertBuildError> {
        if fields.len() > MAX_PATTERN_VERTICES {
            return Err(GraphInsertBuildError::TooManyFields {
                limit: MAX_PATTERN_VERTICES,
                observed: fields.len(),
            });
        }
        fields.sort_by_key(|(key, _)| *key);
        if fields.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(GraphInsertBuildError::DuplicateProperty { declaration });
        }
        let mut keys = Vec::new();
        let mut projection = Vec::new();
        for (at, (key, value)) in fields.into_iter().enumerate() {
            let check = |column| {
                if matches!(
                    columns.get(column),
                    Some(GraphSetColumnType::Scalar | GraphSetColumnType::Any)
                ) {
                    Ok(())
                } else {
                    Err(GraphInsertBuildError::ValueColumn {
                        declaration,
                        column,
                    })
                }
            };
            let value = match value {
                GraphMutationValue::Column(column) => {
                    check(column)?;
                    GraphSetValue::Column(column)
                }
                GraphMutationValue::Literal(value) => GraphSetValue::Literal(value),
                GraphMutationValue::Expression(expression) => {
                    for column in expression.referenced_columns() {
                        check(column)?;
                    }
                    GraphSetValue::Integer(expression)
                }
                GraphMutationValue::Composite(value) => {
                    let refused = |kind| GraphInsertBuildError::ValueExpression {
                        declaration,
                        property: at,
                        kind,
                    };
                    let kind = crate::GraphSetProjection::admit_output(&value, columns, at)
                        .map_err(|kind| refused(Some(kind)))?;
                    if !matches!(kind, GraphSetColumnType::Scalar | GraphSetColumnType::Any) {
                        return Err(refused(None));
                    }
                    value
                }
            };
            keys.push(key);
            projection.push(GraphSetProjection::new(format!("_insert_{at}"), value));
        }
        Ok(Self { keys, projection })
    }
    fn append(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&(self.keys.len() as u64).to_be_bytes());
        for (key, expression) in self.keys.iter().zip(&self.projection) {
            bytes.extend_from_slice(&key.0.to_be_bytes());
            match expression.value() {
                GraphSetValue::Column(column) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&(*column as u64).to_be_bytes());
                }
                GraphSetValue::Literal(value) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&(value.canonical_bytes().len() as u64).to_be_bytes());
                    bytes.extend_from_slice(value.canonical_bytes());
                }
                GraphSetValue::Integer(expression) => {
                    let expression = expression.canonical_bytes();
                    bytes.push(2);
                    bytes.extend_from_slice(&(expression.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(&expression);
                }
                // Composite projections use the enum's own canonical
                // transcript; the discriminant byte keeps wire framing.
                GraphSetValue::List(_)
                | GraphSetValue::Index { .. }
                | GraphSetValue::Size(_)
                | GraphSetValue::In { .. }
                | GraphSetValue::Local(_)
                | GraphSetValue::Comprehension { .. }
                | GraphSetValue::Quantifier { .. }
                | GraphSetValue::Slice { .. }
                | GraphSetValue::Range { .. }
                | GraphSetValue::Reduce { .. }
                | GraphSetValue::MapLiteral { .. }
                | GraphSetValue::MapOverlay { .. }
                | GraphSetValue::MapGet { .. }
                | GraphSetValue::Keys(_)
                | GraphSetValue::Value(_) => {
                    bytes.push(3);
                    let mut encoded = Vec::new();
                    expression.value().append_canonical_bytes(&mut encoded);
                    bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(&encoded);
                }
            }
        }
    }
}

#[cfg(test)]
mod relational_tests;
