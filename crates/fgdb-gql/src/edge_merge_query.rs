//! Native RETURN after a relationship MERGE (fgdb-at06q).
//!
//! Every selected MATCH row continues once per relationship its endpoint pair
//! resolved to: each matching edge, or the one created edge. The write owner
//! builds those rows after staging and supplies their final fields; no second
//! MATCH runs. Aggregation, projection, DISTINCT, ORDER BY and pagination use
//! the shared write-RETURN kernels.

use crate::algebra::{GraphOrderError, GraphValue, GraphValueOrder, GraphValueRow};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphSetColumnType, GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier,
    PreparedGraphEdgeMerge,
};
use fgdb_delta_types::PropertyKeyId;

/// One input column of a relationship MERGE RETURN row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEdgeMergeBinding {
    /// A MERGE selection column. MERGE changes no selected element, so its
    /// frozen value is also its final one.
    Input(usize),
    /// The relationship this row resolved to.
    Edge,
    /// A property of that relationship after the statement. A missing field
    /// reads NULL. The host applies property authority first.
    Property(PropertyKeyId),
    /// `startNode(r)` (`start`) or `endNode(r)`: the relationship's own source
    /// or destination, which an undirected match may hold either way round.
    Endpoint { start: bool },
    /// A property of that endpoint vertex. A missing field reads NULL.
    EndpointProperty { start: bool, key: PropertyKeyId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeMergeQueryBuildError {
    TooManyBindings {
        limit: usize,
        observed: usize,
    },
    /// An Input binding names no selection column.
    Binding {
        binding: usize,
    },
    Projection(RowProjectionBuildError),
    Aggregate(crate::GraphAggregateBuildError),
}
impl core::fmt::Display for GraphEdgeMergeQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "relationship MERGE RETURN definition: {self:?}")
    }
}
impl core::error::Error for GraphEdgeMergeQueryBuildError {}

/// A relationship MERGE RETURN refusal: the MERGE itself, or its RETURN.
#[derive(Debug)]
pub enum GraphEdgeMergeQueryError<E, A> {
    Merge(crate::GraphEdgeMergeError<E, A>),
    Returning(GraphSetExecutionError<E>),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphEdgeMergeQueryError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Merge(error) => error.fmt(f),
            Self::Returning(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphEdgeMergeQueryError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Merge(error) => Some(error),
            Self::Returning(error) => Some(error),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeMergeQuery {
    merge: PreparedGraphEdgeMerge,
    bindings: Vec<GraphEdgeMergeBinding>,
    input_types: Vec<GraphSetColumnType>,
    projection: Vec<GraphSetProjection>,
    grouping: Option<crate::set_ops::WriteReturnGroup>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
}
impl core::fmt::Debug for PreparedGraphEdgeMergeQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeMergeQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphEdgeMergeQuery {
    pub fn prepare(
        merge: PreparedGraphEdgeMerge,
        bindings: Vec<GraphEdgeMergeBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphEdgeMergeQueryBuildError> {
        Self::prepare_with_grouping(merge, bindings, projection, quantifier, None)
    }

    pub(crate) fn prepare_with_grouping(
        merge: PreparedGraphEdgeMerge,
        bindings: Vec<GraphEdgeMergeBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        grouping: Option<crate::set_ops::WriteReturnGroupSpec>,
    ) -> Result<Self, GraphEdgeMergeQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphEdgeMergeQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        let selection = merge.selection().value_columns();
        let mut input_types = Vec::with_capacity(bindings.len());
        for (at, binding) in bindings.iter().enumerate() {
            input_types.push(match *binding {
                GraphEdgeMergeBinding::Input(column) => selection
                    .get(column)
                    .map(GraphSetColumnType::from)
                    .ok_or(GraphEdgeMergeQueryBuildError::Binding { binding: at })?,
                GraphEdgeMergeBinding::Edge => GraphSetColumnType::Edge,
                GraphEdgeMergeBinding::Endpoint { .. } => GraphSetColumnType::Vertex,
                GraphEdgeMergeBinding::Property(_)
                | GraphEdgeMergeBinding::EndpointProperty { .. } => GraphSetColumnType::Scalar,
            });
        }
        let grouping = grouping
            .map(|group| group.prepare(input_types.clone()))
            .transpose()
            .map_err(GraphEdgeMergeQueryBuildError::Aggregate)?;
        let output_input = grouping
            .as_ref()
            .map(|group| group.column_types().to_vec())
            .unwrap_or_else(|| input_types.clone());
        let checked = RowProjectionSpec::new(output_input, projection.clone(), quantifier)
            .map_err(GraphEdgeMergeQueryBuildError::Projection)?;
        Ok(Self {
            merge,
            bindings,
            input_types,
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
    pub const fn merge(&self) -> &PreparedGraphEdgeMerge {
        &self.merge
    }
    #[must_use]
    pub fn bindings(&self) -> &[GraphEdgeMergeBinding] {
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

    /// Finalize the statement's final-state rows, one per selected row and
    /// relationship it resolved to, under its cumulative counters. Every cell
    /// is checked against its binding before any projection runs. The host
    /// must retain its rollback guard until this succeeds. LIMIT 0 discards
    /// output, never the MERGE or its field reads.
    pub fn finish_governed<E, C>(
        &self,
        rows: Vec<Vec<GraphValue>>,
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
        let invalid = || GqlQueryError::Source(GraphSetExecutionError::InputSchema { operand: 0 });
        for row in &rows {
            if row.len() != self.bindings.len() {
                return Err(invalid());
            }
            for ((binding, kind), value) in self.bindings.iter().zip(&self.input_types).zip(row) {
                work()?;
                let admitted = match binding {
                    GraphEdgeMergeBinding::Input(_) => kind.accepts(value),
                    GraphEdgeMergeBinding::Edge => matches!(value, GraphValue::Edge(_)),
                    GraphEdgeMergeBinding::Endpoint { .. } => {
                        matches!(value, GraphValue::Vertex(_))
                    }
                    GraphEdgeMergeBinding::Property(_)
                    | GraphEdgeMergeBinding::EndpointProperty { .. } => {
                        matches!(value, GraphValue::Scalar(_))
                    }
                };
                if !admitted {
                    return Err(invalid());
                }
            }
        }
        drop(work);
        let mut returning = GqlQueryExecution {
            value: rows
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
        let mut bytes = b"fgdb:edge-merge-query:v1\0".to_vec();
        let merge = self.merge.canonical_bytes();
        bytes.extend_from_slice(&(merge.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&merge);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            match *binding {
                GraphEdgeMergeBinding::Input(column) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&(column as u64).to_be_bytes());
                }
                GraphEdgeMergeBinding::Edge => bytes.push(1),
                GraphEdgeMergeBinding::Property(key) => {
                    bytes.push(2);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                }
                GraphEdgeMergeBinding::Endpoint { start } => {
                    bytes.push(3);
                    bytes.push(u8::from(start));
                }
                GraphEdgeMergeBinding::EndpointProperty { start, key } => {
                    bytes.push(4);
                    bytes.push(u8::from(start));
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
