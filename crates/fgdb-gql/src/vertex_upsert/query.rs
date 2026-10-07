//! Native `MERGE (n ...) [ON MATCH SET ...] [ON CREATE SET ...] [SET ...]
//! RETURN ...`. MERGE chooses exactly one vertex, so RETURN projects one row.
//! The host reads the chosen vertex's fields after every clause is staged and
//! hands them back here; nothing is re-matched.

use super::*;
use crate::algebra::{GraphOrderError, GraphValue, GraphValueOrder, GraphValueRow};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphSetColumnType, GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier,
};

/// One private column of the single MERGE RETURN input row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphVertexReturnBinding {
    /// The chosen vertex's identity.
    Vertex,
    /// A field of the chosen vertex after every clause; missing reads NULL.
    Property(PropertyKeyId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphVertexUpsertQueryBuildError {
    TooManyBindings { limit: usize, observed: usize },
    Projection(RowProjectionBuildError),
}
impl core::fmt::Display for GraphVertexUpsertQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "MERGE RETURN definition: {self:?}")
    }
}
impl core::error::Error for GraphVertexUpsertQueryBuildError {}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphVertexUpsertQuery {
    upsert: PreparedGraphVertexUpsert,
    bindings: Vec<GraphVertexReturnBinding>,
    projection: Vec<GraphSetProjection>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
}
impl core::fmt::Debug for PreparedGraphVertexUpsertQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexUpsertQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphVertexUpsertQuery {
    pub fn prepare(
        upsert: PreparedGraphVertexUpsert,
        bindings: Vec<GraphVertexReturnBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphVertexUpsertQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphVertexUpsertQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        let input_types = bindings
            .iter()
            .map(|binding| match binding {
                GraphVertexReturnBinding::Vertex => GraphSetColumnType::Vertex,
                GraphVertexReturnBinding::Property(_) => GraphSetColumnType::Scalar,
            })
            .collect();
        let checked = RowProjectionSpec::new(input_types, projection.clone(), quantifier)
            .map_err(GraphVertexUpsertQueryBuildError::Projection)?;
        let columns = checked.columns().map(str::to_owned).collect();
        let types = checked.column_types().to_vec();
        Ok(Self {
            upsert,
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
    pub const fn upsert(&self) -> &PreparedGraphVertexUpsert {
        &self.upsert
    }
    #[must_use]
    pub fn bindings(&self) -> &[GraphVertexReturnBinding] {
        &self.bindings
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

    /// Project the one input row the host read for `bindings()` after every
    /// clause was staged. `selection` and `evaluator` are the upsert's own
    /// cumulative counters, so the result counters cover the whole statement.
    pub fn project_governed<E, C>(
        &self,
        row: Vec<GraphValue>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        crate::set_ops::finish_owned_projection(
            GqlQueryExecution {
                value: vec![GraphValueRow::from_owned_values(row)],
                rows: selection,
                evaluator,
            },
            policy,
            &self.projection,
            self.quantifier,
            &self.order,
            (self.offset, self.count),
            checkpoint,
        )
    }

    /// Definition identity, not a durable effect format.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:unique-vertex-upsert-query:v1\0".to_vec();
        let upsert = self.upsert.canonical_bytes();
        bytes.extend_from_slice(&(upsert.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&upsert);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            match binding {
                GraphVertexReturnBinding::Vertex => bytes.push(0),
                GraphVertexReturnBinding::Property(key) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                }
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
