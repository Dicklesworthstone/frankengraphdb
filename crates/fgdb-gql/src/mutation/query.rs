//! Native `SET ... RETURN` over the mutation's own selection occurrences.
//! Rows and intents stay private until the mutation and the whole output
//! succeed; RETURN never rescans the graph or re-evaluates the MATCH.

use super::*;
use crate::algebra::{GraphOrderError, GraphValueOrder};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphMutationQueryBuildError {
    Binding { binding: usize },
    TooManyBindings { limit: usize, observed: usize },
    Projection(RowProjectionBuildError),
    Aggregate(crate::GraphAggregateBuildError),
    InputDepth(crate::GraphSetBuildError),
}
impl core::fmt::Display for GraphMutationQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "mutation RETURN definition: {self:?}")
    }
}
impl core::error::Error for GraphMutationQueryBuildError {}

/// A private complete proposal plus its RETURN rows, not a commit receipt.
/// Adapters stage the intents atomically and publish the rows only when the
/// transaction outcome permits.
#[derive(Debug)]
pub struct GraphMutationQueryBatch {
    mutation: GraphMutationBatch,
    returning: GqlQueryExecution<GraphValueRow>,
}
impl GraphMutationQueryBatch {
    #[must_use]
    pub const fn mutation(&self) -> &GraphMutationBatch {
        &self.mutation
    }
    /// WHOLE-operation source/work/scratch counters and final RETURN row
    /// count. They already include `mutation().stats()`: never sum them.
    #[must_use]
    pub const fn returning(&self) -> &GqlQueryExecution<GraphValueRow> {
        &self.returning
    }
    #[must_use]
    pub fn into_parts(self) -> (GraphMutationBatch, GqlQueryExecution<GraphValueRow>) {
        (self.mutation, self.returning)
    }
}

/// `MATCH ... SET/REMOVE ... RETURN ...`: one RETURN row per selection row
/// (before DISTINCT and paging), with every property read reflecting the
/// statement's own simultaneous assignments.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphMutationQuery {
    mutation: PreparedGraphMutation,
    bindings: Vec<GraphMutationBinding>,
    projection: Vec<GraphSetProjection>,
    grouping: Option<crate::set_ops::WriteReturnGroup>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
}
impl core::fmt::Debug for PreparedGraphMutationQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphMutationQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphMutationQuery {
    pub fn prepare(
        mutation: PreparedGraphMutation,
        bindings: Vec<GraphMutationBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphMutationQueryBuildError> {
        Self::prepare_with_grouping(mutation, bindings, projection, quantifier, None)
    }

    pub(crate) fn prepare_with_grouping(
        mutation: PreparedGraphMutation,
        bindings: Vec<GraphMutationBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        grouping: Option<crate::set_ops::WriteReturnGroupSpec>,
    ) -> Result<Self, GraphMutationQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphMutationQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        if let Some(input) = &mutation.input_relation {
            input
                .check_ancestor_depth(2)
                .map_err(GraphMutationQueryBuildError::InputDepth)?;
        }
        let columns = &mutation.columns;
        let mut input_types = Vec::new();
        for (at, binding) in bindings.iter().enumerate() {
            let kind = match *binding {
                GraphMutationBinding::Input(column) => columns.get(column).copied(),
                GraphMutationBinding::Property {
                    target, current, ..
                } => (matches!(
                    columns.get(target),
                    Some(GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
                ) && matches!(
                    columns.get(current),
                    Some(GraphSetColumnType::Scalar | GraphSetColumnType::Any)
                ))
                .then_some(GraphSetColumnType::Scalar),
            };
            input_types.push(kind.ok_or(GraphMutationQueryBuildError::Binding { binding: at })?);
        }
        let grouping = grouping
            .map(|group| group.prepare(input_types.clone()))
            .transpose()
            .map_err(GraphMutationQueryBuildError::Aggregate)?;
        let output_input = grouping
            .as_ref()
            .map(|group| group.column_types().to_vec())
            .unwrap_or(input_types);
        // Complete expression/name/schema admission includes lazy branches.
        // Grouped outputs address native keys then aggregates, not bindings.
        let checked = RowProjectionSpec::new(output_input, projection.clone(), quantifier)
            .map_err(GraphMutationQueryBuildError::Projection)?;
        let output = checked.columns().map(str::to_owned).collect();
        let types = checked.column_types().to_vec();
        Ok(Self {
            mutation,
            bindings,
            projection,
            grouping,
            quantifier,
            columns: output,
            types,
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }

    #[must_use]
    pub const fn mutation(&self) -> &PreparedGraphMutation {
        &self.mutation
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

    /// Selection, proposals, RETURN bindings and projection share one
    /// work/scratch allowance. The result-row allowance applies only AFTER
    /// projection, DISTINCT, ORDER BY and paging: it never limits which
    /// occurrences the mutation updates, and LIMIT 0 still proposes every
    /// assignment and evaluates every RETURN expression. No rows or intents
    /// escape if any expression, conflict check or budget fails.
    pub fn execute_governed<E, C>(
        &self,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<E>, C>> {
        let mut selection = policy;
        selection.query = GqlQueryPolicy::new(
            policy.query.rows.max_snapshot_records().unwrap_or(u64::MAX),
            u64::MAX,
            policy.query.evaluator.max_work_units,
            policy.query.evaluator.max_scratch_entries,
        );
        let (mutation, value) = collect::execute(
            &self.mutation,
            selection,
            Some(&self.bindings),
            source,
            &mut checkpoint,
        )
        .map_err(|error| error.map_source(GraphMutationQueryError::Mutation))?;
        let prefix = mutation.stats();
        let mut returning = GqlQueryExecution {
            value,
            rows: prefix.selection,
            evaluator: prefix.evaluator,
        };
        if let Some(grouping) = &self.grouping {
            returning = crate::set_ops::finish_owned_aggregate(
                returning,
                policy.query,
                grouping,
                &mut checkpoint,
            )
            .map_err(|error| error.map_source(GraphMutationQueryError::Returning))?;
        }
        let returning = crate::set_ops::finish_owned_projection(
            returning,
            policy.query,
            &self.projection,
            self.quantifier,
            &self.order,
            (self.offset, self.count),
            checkpoint,
        )
        .map_err(|error| error.map_source(GraphMutationQueryError::Returning))?;
        Ok(GraphMutationQueryBatch {
            mutation,
            returning,
        })
    }

    /// Definition identity, not a durable effect format. As in native set
    /// projection, aliases are output schema metadata.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:graph-mutation-query:v1\0".to_vec();
        let mutation = self.mutation.canonical_bytes();
        bytes.extend_from_slice(&(mutation.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&mutation);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            match *binding {
                GraphMutationBinding::Input(column) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&(column as u64).to_be_bytes());
                }
                GraphMutationBinding::Property {
                    target,
                    key,
                    current,
                } => {
                    bytes.push(1);
                    bytes.extend_from_slice(&(target as u64).to_be_bytes());
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                    bytes.extend_from_slice(&(current as u64).to_be_bytes());
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

#[derive(Debug)]
pub enum GraphMutationQueryError<E> {
    Mutation(GraphMutationError<E>),
    Returning(GraphSetExecutionError<E>),
}
impl<E: core::fmt::Display> core::fmt::Display for GraphMutationQueryError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mutation(error) => error.fmt(f),
            Self::Returning(error) => write!(f, "mutation RETURN: {error}"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphMutationQueryError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Mutation(error) => Some(error),
            Self::Returning(error) => Some(error),
        }
    }
}
