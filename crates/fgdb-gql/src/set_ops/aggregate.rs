//! Exact GroupAggregate over an arbitrary bounded set/row relation.
//! The source-aware type cannot accidentally execute only the first graph leaf.

use super::*;
use crate::{
    GraphAggregate, GraphAggregateBuildError, GraphAggregateError, GraphAggregateFilter,
    GraphAggregateOrder, GraphAggregateRow, GraphHavingError, GraphHavingExpression,
    PreparedGraphAggregate,
};

/// A terminal exact summary of a complete relational input, including UNION,
/// INTERSECT and EXCEPT with either bag or distinct semantics. Grouping is AFTER
/// all input-local projections, filters, ordering and pages. It never distributes
/// aggregates into the operands, which would change DISTINCT and difference.
///
/// This owns the existing aggregate definition/accumulators, not a parallel
/// aggregate implementation. It deliberately has no input_pattern() or raw
/// graph-iterator executor: all real operands must enter through one source
/// authority. Construction and drop share the existing bounded relation depth.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphSetAggregate {
    summary: PreparedGraphAggregate,
}

crate::row_aggregate::definition::delegate_group_definition!(
    PreparedGraphSetAggregate, owner => &owner.summary, |summary| Self { summary }
);

impl core::fmt::Debug for PreparedGraphSetAggregate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphSetAggregate")
            .field("operands", &self.input().operand_count())
            .field("key_columns", &self.key_columns().len())
            .field("aggregate_columns", &self.aggregate_columns().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl PreparedGraphSetAggregate {
    /// Keep an already checked relational aggregate in its source-aware owner.
    /// The complete input and all result clauses are retained unchanged. Plain
    /// graph/computed-binding definitions refuse; no schema carrier is invented.
    pub fn from_relation(summary: PreparedGraphAggregate) -> Option<Self> {
        summary.input_relation().is_some().then_some(Self { summary })
    }

    /// Reuse native post-HAVING expressions without exposing a graph executor.
    pub fn with_output_projection(
        mut self, projection: Vec<crate::GraphSetProjection>,
    ) -> Result<Self, GraphAggregateBuildError> {
        self.summary = self.summary.with_output_projection(projection)?;
        Ok(self)
    }

    /// Keys and arguments address the completed relation's column schema.
    /// The usual nine exact aggregate functions and group-result clauses are
    /// unchanged; counts/sums/averages never narrow into CanonicalScalar.
    /// offset/count here select GROUP results, independently of the input page.
    /// No source, catalog, cancellation context or input values execute here.
    pub fn prepare(
        input: PreparedGraphSet,
        keys: &[usize],
        aggregates: &[GraphAggregate<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<Self, GraphAggregateBuildError> {
        Ok(Self {
            summary: PreparedGraphAggregate::prepare_set_relation(
                input, keys, aggregates, offset, count,
            )?,
        })
    }

    #[must_use]
    pub fn input(&self) -> &PreparedGraphSet {
        self.summary
            .input_relation()
            .expect("compound owner always has a relation")
    }
    #[must_use]
    pub fn key_columns(&self) -> &[String] {
        self.summary.key_columns()
    }
    #[must_use]
    pub fn aggregate_columns(&self) -> &[String] {
        self.summary.aggregate_columns()
    }
    #[must_use]
    pub fn evaluation_key_columns(&self) -> &[String] {
        self.summary.evaluation_key_columns()
    }
    #[must_use]
    pub fn evaluation_aggregate_columns(&self) -> &[String] {
        self.summary.evaluation_aggregate_columns()
    }

    pub fn with_result_clauses(
        mut self,
        having: &[GraphAggregateFilter],
        ordering: &[GraphAggregateOrder],
    ) -> Result<Self, GraphAggregateBuildError> {
        self.summary = self.summary.with_result_clauses(having, ordering)?;
        Ok(self)
    }
    pub fn with_having_expression(
        mut self,
        expression: &GraphHavingExpression,
    ) -> Result<Self, GraphHavingError> {
        self.summary = self.summary.with_having_expression(expression)?;
        Ok(self)
    }
    pub fn with_key_output_columns(
        mut self,
        columns: &[usize],
    ) -> Result<Self, GraphAggregateBuildError> {
        self.summary = self.summary.with_key_output_columns(columns)?;
        Ok(self)
    }
    pub fn with_aggregate_output_prefix(
        mut self,
        count: usize,
    ) -> Result<Self, GraphAggregateBuildError> {
        self.summary = self.summary.with_aggregate_output_prefix(count)?;
        Ok(self)
    }
    #[must_use]
    pub fn with_distinct_output(mut self, distinct: bool) -> Self {
        self.summary = self.summary.with_distinct_output(distinct);
        self
    }

    /// Reuse the exact aggregate transcript, including the COMPLETE input set.
    /// Equivalent single-source relational aggregates retain identical bytes;
    /// changing any operand, quantifier or local page changes the definition.
    /// These are application bytes, not a durable format or result certificate.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        self.summary.canonical_bytes()
    }

    /// Execute every real input through a trusted immutable-snapshot adapter.
    /// The host must pin the SAME database sequence/transaction overlay and
    /// authorization/catalog context for every leaf call. Built-in database
    /// adapters enforce that lifetime with immutable borrows.
    ///
    /// Operands execute once in left-to-right order, even after empty inputs or
    /// beneath final LIMIT 0. Their record VISITS sum across sources. Work and
    /// scratch continue through set operations and grouping without a refreshed
    /// allowance. Only final group rows consume the public result-row limit.
    /// Late source, schema, arithmetic and cancellation errors release no rows.
    /// This is bounded materialization, not spill-backed or streaming grouping.
    pub fn execute_governed<E, C>(
        &self,
        policy: GqlQueryPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        self.summary
            .execute_relational_with_source(policy, source, checkpoint)
    }
}

impl PreparedGraphSet {
    /// First real graph leaf, if any. Values-only relations need no graph
    /// metadata and never manufacture a source merely to describe their schema.
    pub(crate) fn first_pattern_input(&self) -> Option<&PreparedGraphPattern<GraphValueRow>> {
        match &self.node {
            SetNode::Pattern(pattern) => Some(pattern),
            SetNode::Values => None,
            SetNode::Scope(input)
            | SetNode::Project { input, .. }
            | SetNode::Unwind { input, .. }
            | SetNode::Filter { input, .. } => input.first_pattern_input(),
            SetNode::Binary { left, right, .. } | SetNode::CrossJoin { left, right } => left
                .first_pattern_input()
                .or_else(|| right.first_pattern_input()),
        }
    }
}
