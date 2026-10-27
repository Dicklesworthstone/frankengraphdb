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
    GraphSetProjection, GraphSetSource, GraphSetValue, PreparedGraphSet,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};

// One statement's text is at most MAX_GRAPH_TEXT_TOKENS (24576) tokens, about
// 6144 of the smallest declarations `(a),`. These caps stay below that, so a
// text can reach them and their refusal is the one it reports. They admit a
// whole hand-written dataset in one CREATE: TCK Create4 [1], the movie graph
// (424 declarations, about 730 fields), and Create4 [2] (1477 declarations).
// Preparation stays linear apart from a short name scan per node reference.
pub const MAX_GRAPH_INSERT_DECLARATIONS: usize = 4096;
pub const MAX_GRAPH_INSERT_FIELDS: usize = 4096;

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
        match self {
            Self::RelationalInput(error) => error.fmt(f),
            Self::Empty => f.write_str("CREATE requires at least one vertex or relationship"),
            Self::TooManyDeclarations { limit, observed } => {
                write!(f, "CREATE declares {observed} elements, limit {limit}")
            }
            Self::TooManyFields { limit, observed } => {
                write!(
                    f,
                    "CREATE sets {observed} labels and properties, limit {limit}"
                )
            }
            Self::DuplicateLabel { vertex } => {
                write!(f, "created vertex {vertex} repeats a label")
            }
            Self::DuplicateProperty { declaration } => {
                write!(f, "created element {declaration} repeats a property")
            }
            Self::ValueColumn {
                declaration,
                column,
            } => write!(
                f,
                "created element {declaration} reads column {column}, which is not a property value"
            ),
            Self::ValueExpression {
                declaration,
                property,
                kind,
            } => {
                write!(
                    f,
                    "property {property} of created element {declaration} cannot be stored"
                )?;
                match kind {
                    Some(kind) => write!(f, ": {kind}"),
                    None => Ok(()),
                }
            }
            Self::EndpointColumn { edge, column } => write!(
                f,
                "created relationship {edge} uses column {column} as an endpoint, which is not a vertex"
            ),
            Self::CreatedEndpoint { edge, vertex } => write!(
                f,
                "created relationship {edge} names endpoint vertex {vertex}, which is not created"
            ),
        }
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
    /// The effects a chain's SET, REMOVE or DELETE step may propose
    /// (fgdb-cpny9). An insertion proposes none; [`Self::new`] leaves it
    /// unbounded, so only the query allowance bounds such a step.
    pub max_mutation_effects: u64,
}
impl GraphInsertPolicy {
    #[must_use]
    pub const fn new(query: GqlQueryPolicy, max_vertices: u64, max_edges: u64) -> Self {
        Self {
            query,
            max_vertices,
            max_edges,
            max_mutation_effects: u64::MAX,
        }
    }
    #[must_use]
    pub const fn with_mutation_limit(mut self, max_mutation_effects: u64) -> Self {
        self.max_mutation_effects = max_mutation_effects;
        self
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
        /// Sorted by key. A value is a scalar or a stored List (fgdb-k3nub).
        properties: Vec<(PropertyKeyId, CanonicalPropertyValue)>,
    },
    Edge {
        edge: EId,
        relation: RelationId,
        source: VId,
        destination: VId,
        /// Sorted by key. A value is a scalar or a stored List (fgdb-k3nub).
        properties: Vec<(PropertyKeyId, CanonicalPropertyValue)>,
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
                    // A dynamically typed column (a list element, a map
                    // value) is checked per row when it runs.
                    GraphInsertEndpoint::Column(column) => {
                        if !matches!(
                            columns.get(column),
                            Some(GraphSetColumnType::Vertex | GraphSetColumnType::Any)
                        ) {
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

    /// Standalone vertex creations have one implicit input occurrence and no
    /// graph reads. Earlier vertex creations cannot change that occurrence or
    /// its frozen property values. A host may prepare such statements in
    /// independent chunks only while preserving their issued identities,
    /// storage admission, source order and ordinary all-or-nothing staging.
    ///
    /// Pattern-selected and relational inputs, including source-free UNWIND,
    /// are deliberately outside this proof, as are creations containing edges.
    #[must_use]
    pub fn independent_vertex_append_relation(&self) -> Option<RelationId> {
        (self.is_standalone() && !self.vertices.is_empty() && self.edges.is_empty())
            .then_some(self.relation)
    }

    /// One source-free creation of a complete fresh subgraph in one coordinate.
    /// The checked zero-column input makes every edge endpoint a vertex created
    /// by this same statement; properties cannot read earlier statements.
    /// Independent chunks must still preserve all issued vertex and edge IDs,
    /// storage admission, declaration order and atomic program publication.
    ///
    /// A host must not mix this profile with vertex-reading edge appends: those
    /// reads could observe the vertices created by an earlier statement.
    /// Pattern and relational inputs are outside this proof, even if a relation
    /// happens to contain no graph operand.
    #[must_use]
    pub fn independent_standalone_create_relation(&self) -> Option<RelationId> {
        (self.is_standalone()
            && !self.vertices.is_empty()
            && self.edges.iter().all(|edge| edge.relation == self.relation))
        .then_some(self.relation)
    }

    /// A native vertex-only selection followed solely by edge creations in
    /// one relation. Appending such edges cannot change this selection or its
    /// frozen property inputs. A host may reuse that fact only while no other
    /// mutations intervene and must still admit every edge and issued identity.
    ///
    /// Relational inputs, procedures and returning-query stages are not part
    /// of this proof. Native projected values name pattern-bound vertices;
    /// they cannot hide a read through a supplied edge identity.
    ///
    /// The answer is the edges' own relation, not the statement coordinate:
    /// the coordinate places created vertices, and this profile creates none.
    /// The host stages every edge group under its edge relation either way.
    #[must_use]
    pub fn independent_edge_append_relation(&self) -> Option<RelationId> {
        use crate::algebra::ValueProjection;

        let Input::Pattern(selection) = &self.input else {
            return None;
        };
        let relation = self.edges.first()?.relation;
        if !self.vertices.is_empty()
            || self.edges.iter().any(|edge| edge.relation != relation)
            || selection.plan().reads_edges()
            || !selection.value_columns().iter().all(|column| {
                matches!(
                    column,
                    ValueProjection::Vertex { .. }
                        | ValueProjection::Property { .. }
                        | ValueProjection::Labels { .. }
                        | ValueProjection::Properties { .. }
                )
            })
        {
            return None;
        }
        Some(relation)
    }

    pub(crate) fn return_property_keys(
        &self,
        binding: crate::GraphInsertBinding,
    ) -> Option<&[PropertyKeyId]> {
        match binding {
            crate::GraphInsertBinding::VertexProperties(vertex) => self
                .vertices
                .get(vertex)
                .map(|vertex| vertex.properties.keys.as_slice()),
            crate::GraphInsertBinding::EdgeProperties(edge) => self
                .edges
                .get(edge)
                .map(|edge| edge.properties.keys.as_slice()),
            _ => None,
        }
    }

    /// A created vertex's labels, in identity order (sorted at preparation).
    pub(crate) fn return_label_ids(
        &self,
        binding: crate::GraphInsertBinding,
    ) -> Option<&[LabelId]> {
        match binding {
            crate::GraphInsertBinding::VertexLabels(vertex) => self
                .vertices
                .get(vertex)
                .map(|vertex| vertex.labels.as_slice()),
            _ => None,
        }
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
        self.execute_governed_with_source(policy, source, allocate, checkpoint)
    }

    /// The same atomic collector with an explicit graph source owner. The host
    /// may supply checked factors of one admitted pattern; ordinary closures
    /// continue to use [`Self::execute_governed`] with identical semantics.
    pub fn execute_governed_with_source<E, A, C>(
        &self,
        policy: GraphInsertPolicy,
        source: impl GraphSetSource<E, C>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphInsertBatch, GqlQueryError<GraphInsertError<E, A>, C>> {
        self.execute_governed_over(None, policy, source, allocate, checkpoint)
    }

    /// The last write of a chain when it has no RETURN (`CREATE (a) WITH a
    /// CREATE (b)`): `rows` are the rows the previous write of the statement
    /// returned, read by this relational input's one owned leaf. `None` is
    /// [`Self::execute_governed_with_source`].
    pub fn execute_governed_over<E, A, C>(
        &self,
        rows: Option<Vec<GraphValueRow>>,
        policy: GraphInsertPolicy,
        source: impl GraphSetSource<E, C>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphInsertBatch, GqlQueryError<GraphInsertError<E, A>, C>> {
        collect::execute(self, policy, None, None, rows, source, allocate, checkpoint)
            .map(|(batch, _)| batch)
    }

    /// `owned` is the previous write's rows for a chained CREATE, read by the
    /// relational input's one owned leaf; `None` for every other statement.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_returning<E, A, C>(
        &self,
        policy: GraphInsertPolicy,
        bindings: &[crate::insertion_query::GraphInsertBinding],
        property_catalog: Option<&crate::ReverseSymbolCatalog>,
        owned: Option<Vec<GraphValueRow>>,
        source: impl GraphSetSource<E, C>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> ReturningInsert<E, A, C> {
        collect::execute(
            self,
            policy,
            Some(bindings),
            property_catalog,
            owned,
            source,
            allocate,
            checkpoint,
        )
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
            // A list column is a stored list field (fgdb-k3nub); execution
            // still refuses a list holding a map or a graph element.
            let check = |column: usize| {
                if columns.get(column).is_some_and(|kind| {
                    kind.is_scalar_like()
                        || matches!(kind, GraphSetColumnType::List | GraphSetColumnType::Any)
                }) {
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
                    if !kind.is_scalar_like()
                        && !matches!(kind, GraphSetColumnType::List | GraphSetColumnType::Any)
                    {
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
                GraphSetValue::TypedNull(_)
                | GraphSetValue::List(_)
                | GraphSetValue::Index { .. }
                | GraphSetValue::Size(_)
                | GraphSetValue::In { .. }
                | GraphSetValue::Equal { .. }
                | GraphSetValue::Ordered { .. }
                | GraphSetValue::Temporal { .. }
                | GraphSetValue::Function { .. }
                | GraphSetValue::Case { .. }
                | GraphSetValue::Random { .. }
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
                | GraphSetValue::ElementMetadata { .. }
                | GraphSetValue::Let { .. }
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
