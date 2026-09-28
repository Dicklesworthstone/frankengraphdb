//! Native CREATE RETURN over the collector's exact occurrence bindings.
//! Rows and intents stay private until creation and the entire output succeed.

use crate::algebra::{GraphOrderError, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use crate::insertion::{
    GraphInsertBatch, GraphInsertError, GraphInsertPolicy, GraphInsertRequest, PreparedGraphInsert,
};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetBuildError, GraphSetColumnType,
    GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier,
};
use fgdb_delta_types::{ElementId, PropertyKeyId};

/// One private column of the per-occurrence RETURN input. Property bindings
/// read the already-checked CREATE values, and missing keys yield scalar NULL.
/// They do not issue graph reads or evaluate CREATE expressions again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphInsertBinding {
    Input(usize),
    CreatedVertex(usize),
    VertexProperty { vertex: usize, key: PropertyKeyId },
    CreatedEdge(usize),
    EdgeProperty { edge: usize, key: PropertyKeyId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphInsertQueryBuildError {
    Binding { binding: usize },
    TooManyBindings { limit: usize, observed: usize },
    Projection(RowProjectionBuildError),
    InputDepth(GraphSetBuildError),
}
impl core::fmt::Display for GraphInsertQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "insertion RETURN definition: {self:?}")
    }
}
impl core::error::Error for GraphInsertQueryBuildError {}

#[derive(Debug)]
pub enum GraphInsertQueryError<E, A> {
    Insertion(GraphInsertError<E, A>),
    Returning(GraphSetExecutionError<E>),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphInsertQueryError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Insertion(error) => error.fmt(f),
            Self::Returning(error) => write!(f, "insertion RETURN: {error}"),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphInsertQueryError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Insertion(error) => Some(error),
            Self::Returning(error) => Some(error),
        }
    }
}

/// A private complete proposal, not a commit receipt. Adapters must stage the
/// intents atomically and retain rows until their transaction outcome permits
/// publication. No partial result exists on expression, budget or cancellation
/// failure, although the allocator may already have spent identities.
#[derive(Debug, PartialEq, Eq)]
pub struct GraphInsertQueryBatch {
    insertion: GraphInsertBatch,
    returning: GqlQueryExecution<GraphValueRow>,
}
impl GraphInsertQueryBatch {
    /// Creation counts and the cumulative prefix through binding collection.
    #[must_use]
    pub const fn insertion(&self) -> &GraphInsertBatch {
        &self.insertion
    }
    /// WHOLE-operation source/work/scratch counters and final RETURN row count.
    /// These counters already include `insertion().stats()`: never sum them.
    #[must_use]
    pub const fn returning(&self) -> &GqlQueryExecution<GraphValueRow> {
        &self.returning
    }
    #[must_use]
    pub fn into_parts(self) -> (GraphInsertBatch, GqlQueryExecution<GraphValueRow>) {
        (self.insertion, self.returning)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphInsertQuery {
    insertion: PreparedGraphInsert,
    bindings: Vec<GraphInsertBinding>,
    projection: Vec<GraphSetProjection>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
}
impl core::fmt::Debug for PreparedGraphInsertQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphInsertQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphInsertQuery {
    pub fn prepare(
        insertion: PreparedGraphInsert,
        bindings: Vec<GraphInsertBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphInsertQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphInsertQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        if let Some(input) = insertion.relational_selection() {
            input
                .check_ancestor_depth(2)
                .map_err(GraphInsertQueryBuildError::InputDepth)?;
        }
        let mut input_types = Vec::new();
        for (at, binding) in bindings.iter().enumerate() {
            let kind = match *binding {
                GraphInsertBinding::Input(column) => {
                    insertion.input_column_types().get(column).copied()
                }
                GraphInsertBinding::CreatedVertex(vertex) => {
                    (vertex < insertion.vertices_per_row()).then_some(GraphSetColumnType::Vertex)
                }
                GraphInsertBinding::VertexProperty { vertex, .. } => {
                    (vertex < insertion.vertices_per_row()).then_some(GraphSetColumnType::Scalar)
                }
                GraphInsertBinding::CreatedEdge(edge) => {
                    (edge < insertion.edges_per_row()).then_some(GraphSetColumnType::Edge)
                }
                GraphInsertBinding::EdgeProperty { edge, .. } => {
                    (edge < insertion.edges_per_row()).then_some(GraphSetColumnType::Scalar)
                }
            };
            input_types.push(kind.ok_or(GraphInsertQueryBuildError::Binding { binding: at })?);
        }
        // Use the ordinary complete expression/name/schema admission, including
        // references hidden in lazy branches. The snapshot VM is shared below.
        let checked = RowProjectionSpec::new(input_types, projection.clone(), quantifier)
            .map_err(GraphInsertQueryBuildError::Projection)?;
        let columns = checked.columns().map(str::to_owned).collect();
        let types = checked.column_types().to_vec();
        Ok(Self {
            insertion,
            bindings,
            projection,
            quantifier,
            columns,
            types,
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }

    #[must_use]
    pub const fn insertion(&self) -> &PreparedGraphInsert {
        &self.insertion
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    #[must_use]
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }

    pub fn with_order_by(mut self, order: &[GraphValueOrder]) -> Result<Self, GraphOrderError> {
        if order.is_empty() {
            return Err(GraphOrderError::EmptyOrder);
        }
        if order.len() > self.columns.len() {
            return Err(GraphOrderError::TooManyColumns {
                limit: self.columns.len(),
                observed: order.len(),
            });
        }
        for (at, key) in order.iter().enumerate() {
            if key.column >= self.columns.len() {
                return Err(GraphOrderError::UnknownColumn { column: key.column });
            }
            if order[..at].iter().any(|prior| prior.column == key.column) {
                return Err(GraphOrderError::DuplicateColumn { column: key.column });
            }
        }
        self.order = order.to_vec();
        Ok(self)
    }
    #[must_use]
    pub fn with_page(mut self, offset: u64, count: Option<u64>) -> Self {
        self.offset = offset;
        self.count = count;
        self
    }

    /// Source selection, frozen properties, all identities and RETURN share one
    /// work/scratch allowance. The result-row allowance applies only AFTER
    /// projection, DISTINCT, ORDER BY and paging; it never limits creation.
    /// `max_vertices`/`max_edges` still admit all creation counts before any ID.
    /// LIMIT 0 evaluates every source and RETURN expression and creates the same
    /// structures. No rows/intents escape if any later expression fails.
    pub fn execute_governed<E, A, C>(
        &self,
        policy: GraphInsertPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<E, A>, C>> {
        let mut creation_policy = policy;
        creation_policy.query = GqlQueryPolicy::new(
            policy.query.rows.max_snapshot_records().unwrap_or(u64::MAX),
            u64::MAX,
            policy.query.evaluator.max_work_units,
            policy.query.evaluator.max_scratch_entries,
        );
        let (insertion, value) = self
            .insertion
            .execute_returning(
                creation_policy,
                &self.bindings,
                source,
                allocate,
                &mut checkpoint,
            )
            .map_err(|error| error.map_source(GraphInsertQueryError::Insertion))?;
        let prefix = insertion.stats();
        let returning = crate::set_ops::finish_owned_projection(
            GqlQueryExecution {
                value,
                rows: prefix.selection,
                evaluator: prefix.evaluator,
            },
            policy.query,
            &self.projection,
            self.quantifier,
            &self.order,
            (self.offset, self.count),
            checkpoint,
        )
        .map_err(|error| error.map_source(GraphInsertQueryError::Returning))?;
        Ok(GraphInsertQueryBatch {
            insertion,
            returning,
        })
    }

    /// Definition identity, not an allocation record or durable effect format.
    /// As in native set projection, aliases are output schema metadata.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:graph-insert-query:v1\0".to_vec();
        let insertion = self.insertion.canonical_bytes();
        bytes.extend_from_slice(&(insertion.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&insertion);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            let (tag, index, key) = match *binding {
                GraphInsertBinding::Input(column) => (0, column, None),
                GraphInsertBinding::CreatedVertex(vertex) => (1, vertex, None),
                GraphInsertBinding::VertexProperty { vertex, key } => (2, vertex, Some(key)),
                GraphInsertBinding::CreatedEdge(edge) => (3, edge, None),
                GraphInsertBinding::EdgeProperty { edge, key } => (4, edge, Some(key)),
            };
            bytes.push(tag);
            bytes.extend_from_slice(&(index as u64).to_be_bytes());
            if let Some(key) = key {
                bytes.extend_from_slice(&key.0.to_be_bytes());
            }
        }
        bytes.extend_from_slice(&(self.projection.len() as u64).to_be_bytes());
        for column in &self.projection {
            column.value().append_canonical_bytes(&mut bytes);
        }
        bytes.push(u8::from(self.quantifier == GraphSetQuantifier::Distinct));
        bytes.extend_from_slice(&(self.order.len() as u64).to_be_bytes());
        for key in &self.order {
            bytes.extend_from_slice(&(key.column as u64).to_be_bytes());
            bytes.extend_from_slice(&[u8::from(key.descending), u8::from(key.nulls_first)]);
        }
        bytes.extend_from_slice(&self.offset.to_be_bytes());
        bytes.push(u8::from(self.count.is_some()));
        if let Some(count) = self.count {
            bytes.extend_from_slice(&count.to_be_bytes());
        }
        bytes
    }
}

#[cfg(test)]
mod tests;
