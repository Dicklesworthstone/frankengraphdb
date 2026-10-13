//! Native RETURN over the single relationship selected by MERGE.
//!
//! The write owner supplies final fields of the chosen edge, never a second
//! MATCH. NoInput supplies no row, so even a constant projection stays empty.
//! Aggregation, projection, DISTINCT and pagination use the shared kernels.

use crate::algebra::{GraphOrderError, GraphValue, GraphValueOrder, GraphValueRow};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphEdgeUpsertError, GraphSetColumnType, GraphSetExecutionError, GraphSetProjection,
    GraphSetQuantifier, PreparedGraphEdgeUpsert,
};
use fgdb_delta_types::PropertyKeyId;

/// One private input column, read after all of the selected edge's SET clauses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEdgeReturnBinding {
    Edge,
    /// A missing field reads NULL. The host applies property authority first.
    Property(PropertyKeyId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeUpsertQueryBuildError {
    TooManyBindings { limit: usize, observed: usize },
    Projection(RowProjectionBuildError),
    Aggregate(crate::GraphAggregateBuildError),
}
impl core::fmt::Display for GraphEdgeUpsertQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "relationship MERGE RETURN definition: {self:?}")
    }
}
impl core::error::Error for GraphEdgeUpsertQueryBuildError {}

/// RETURN errors remain distinct from the existing non-returning upsert API.
#[derive(Debug)]
pub enum GraphEdgeUpsertQueryError<E, A> {
    Upsert(GraphEdgeUpsertError<E, A>),
    Returning(GraphSetExecutionError<E>),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphEdgeUpsertQueryError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Upsert(error) => error.fmt(f),
            Self::Returning(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphEdgeUpsertQueryError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Upsert(error) => Some(error),
            Self::Returning(error) => Some(error),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeUpsertQuery {
    upsert: PreparedGraphEdgeUpsert,
    bindings: Vec<GraphEdgeReturnBinding>,
    projection: Vec<GraphSetProjection>,
    grouping: Option<crate::set_ops::WriteReturnGroup>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
}
impl core::fmt::Debug for PreparedGraphEdgeUpsertQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeUpsertQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphEdgeUpsertQuery {
    pub fn prepare(
        upsert: PreparedGraphEdgeUpsert,
        bindings: Vec<GraphEdgeReturnBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphEdgeUpsertQueryBuildError> {
        Self::prepare_with_grouping(upsert, bindings, projection, quantifier, None)
    }

    pub(crate) fn prepare_with_grouping(
        upsert: PreparedGraphEdgeUpsert,
        bindings: Vec<GraphEdgeReturnBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        grouping: Option<crate::set_ops::WriteReturnGroupSpec>,
    ) -> Result<Self, GraphEdgeUpsertQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphEdgeUpsertQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        let input_types = bindings
            .iter()
            .map(|binding| match binding {
                GraphEdgeReturnBinding::Edge => GraphSetColumnType::Edge,
                GraphEdgeReturnBinding::Property(_) => GraphSetColumnType::Stored,
            })
            .collect::<Vec<_>>();
        let grouping = grouping
            .map(|group| group.prepare(input_types.clone()))
            .transpose()
            .map_err(GraphEdgeUpsertQueryBuildError::Aggregate)?;
        let output_input = grouping
            .as_ref()
            .map(|group| group.column_types().to_vec())
            .unwrap_or(input_types);
        let checked = RowProjectionSpec::new(output_input, projection.clone(), quantifier)
            .map_err(GraphEdgeUpsertQueryBuildError::Projection)?;
        Ok(Self {
            upsert,
            bindings,
            columns: checked.columns().map(str::to_owned).collect(),
            types: checked.column_types().to_vec(),
            projection,
            grouping,
            quantifier,
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }

    #[must_use]
    pub const fn upsert(&self) -> &PreparedGraphEdgeUpsert {
        &self.upsert
    }
    #[must_use]
    pub fn bindings(&self) -> &[GraphEdgeReturnBinding] {
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

    /// Finalize zero or one final-state input row under the statement's
    /// cumulative counters. The host must retain its rollback guard until this
    /// succeeds. LIMIT 0 discards output, never the MERGE or its field reads.
    pub fn project_governed<E, C>(
        &self,
        row: Option<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        mut evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        let mut work = || -> Result<(), GqlQueryError<GraphSetExecutionError<E>, C>> {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let observed = u128::from(evaluator.work_units) + 1;
            if observed > u128::from(policy.evaluator.max_work_units) {
                return Err(GqlQueryError::Evaluator(crate::GlaLimitExceeded {
                    dimension: crate::GlaLimitDimension::WorkUnits,
                    limit: policy.evaluator.max_work_units,
                    observed,
                }));
            }
            evaluator.work_units = observed as u64;
            Ok(())
        };
        work()?;
        if let Some(row) = &row {
            let invalid =
                || GqlQueryError::Source(GraphSetExecutionError::InputSchema { operand: 0 });
            if row.len() != self.bindings.len() {
                return Err(invalid());
            }
            for (binding, value) in self.bindings.iter().zip(row) {
                work()?;
                if !matches!(
                    (binding, value),
                    (GraphEdgeReturnBinding::Edge, GraphValue::Edge(_))
                        | (GraphEdgeReturnBinding::Property(_), GraphValue::Scalar(_))
                ) {
                    return Err(invalid());
                }
            }
        }
        drop(work);
        let mut returning = GqlQueryExecution {
            value: row
                .into_iter()
                .map(GraphValueRow::from_owned_values)
                .collect(),
            rows: selection,
            evaluator,
        };
        if let Some(grouping) = &self.grouping {
            returning = crate::set_ops::finish_owned_aggregate(
                returning,
                policy,
                grouping,
                &mut checkpoint,
            )?;
        }
        crate::set_ops::finish_owned_projection(
            returning,
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
        let mut bytes = b"fgdb:directed-edge-upsert-query:v1\0".to_vec();
        let upsert = self.upsert.canonical_bytes();
        bytes.extend_from_slice(&(upsert.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&upsert);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            match binding {
                GraphEdgeReturnBinding::Edge => bytes.push(0),
                GraphEdgeReturnBinding::Property(key) => {
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
        if let Some(grouping) = &self.grouping {
            grouping.append_canonical_bytes(&mut bytes);
        }
        bytes
    }
}
