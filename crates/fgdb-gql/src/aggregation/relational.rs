//! Relational input for the existing exact GroupAggregate.
//!
//! Single-source callers retain their admitted graph contract. Compound callers
//! supply every actual leaf through the set engine's source adapter. Both paths
//! feed the SAME borrowed-row group/result engine, never a synthetic graph.

use super::*;
use crate::PreparedGraphSet;
use std::cell::RefCell;

impl PreparedGraphAggregate {
    /// Aggregate the completed rows of a single-source relational pipeline.
    /// Projection, filters, DISTINCT and local pages remain in their original
    /// order. Group keys and arguments address the FINAL pipeline schema.
    ///
    /// Unlike prepare(), an explicit input page/DISTINCT is meaningful here:
    /// it changes the relation being summarized, not the final group page.
    /// All nine existing aggregate functions, exact i128 sums/rational averages,
    /// HAVING, hidden output columns and deterministic ranking remain available.
    /// Existing snapshot and WriteTxn aggregate entrypoints execute this plan.
    ///
    /// Binary set inputs still refuse at this single-source boundary. Use
    /// PreparedGraphSetAggregate for a relation with multiple graph operands.
    /// This path materializes bounded rows, not spill storage.
    pub fn prepare_relation(
        relation: PreparedGraphSet,
        keys: &[usize],
        aggregates: &[GraphAggregate<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<Self, GraphAggregateBuildError> {
        relation
            .check_parent_depth()
            .map_err(GraphAggregateBuildError::RelationalInput)?;
        let source = relation
            .single_pattern_input()
            .ok_or(GraphAggregateBuildError::RequiresSingleGraphSource)?
            .clone();
        Self::prepare_input(
            source,
            None,
            Some(relation),
            keys,
            aggregates,
            offset,
            count,
        )
    }

    /// Private construction for the compound owner. The retained primary leaf
    /// is a REAL input, not a generated schema-carrier pattern. It is metadata
    /// only on this path: the public compound type never exposes this object's
    /// single-source admission/execution interface. prepare_input validates
    /// keys and arguments against the completed relation, not that first leaf.
    pub(crate) fn prepare_set_relation(
        relation: PreparedGraphSet,
        keys: &[usize],
        aggregates: &[GraphAggregate<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<Self, GraphAggregateBuildError> {
        relation
            .check_parent_depth()
            .map_err(GraphAggregateBuildError::RelationalInput)?;
        let source = relation.first_pattern_input().clone();
        Self::prepare_input(
            source,
            None,
            Some(relation),
            keys,
            aggregates,
            offset,
            count,
        )
    }

    /// The actual immutable row-stage definition, distinct from input_pattern's
    /// graph admission leaf. None preserves ordinary streaming/projected input.
    #[must_use]
    pub fn input_relation(&self) -> Option<&PreparedGraphSet> {
        self.relational_input.as_ref()
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_relational_governed<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        // Each borrow exists for one checkpoint, never around source execution.
        // Single-source preparation still proves that the iterators are taken
        // once; compound execution cannot enter this iterator-only interface.
        let checkpoint = RefCell::new(&mut checkpoint);
        let mut admitted = Some((vertices, edges));
        self.execute_relational_with_source(
            policy,
            |pattern, remaining| {
                let (vertices, edges) = admitted
                    .take()
                    .expect("preparation admitted exactly one immutable graph leaf");
                pattern.plan().execute_governed_with_properties(
                    snapshot_records,
                    vertices,
                    edges,
                    &mut test_vertex,
                    &mut property,
                    remaining,
                    || (*checkpoint.borrow_mut())(),
                )
            },
            || (*checkpoint.borrow_mut())(),
        )
    }

    /// One owner for source visits, set stages, grouping and result release.
    /// Called only by the single-source entrypoint above or the compound type.
    /// The trusted host adapter pins one snapshot/overlay for ALL leaf calls.
    pub(crate) fn execute_relational_with_source<E, C>(
        &self,
        policy: GqlQueryPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        let relation = self
            .relational_input
            .as_ref()
            .expect("relational input dispatch");
        // Intermediate rows are not public aggregate results. The set engine
        // still charges their traversal, retained cells, payloads and release.
        // Its work/scratch totals continue into grouping without a fresh quota.
        let source_policy = GqlQueryPolicy {
            rows: crate::GqlExecutionBudget::snapshot_records(
                policy.rows.max_snapshot_records().unwrap_or(u64::MAX),
            ),
            evaluator: policy.evaluator,
        };
        let source = relation
            .execute_governed(source_policy, source, &mut checkpoint)
            .map_err(|error| error.map_source(GraphAggregateError::InputRelation))?;
        let mut evaluator = source.evaluator;
        let mut rows = GqlExecutionStats {
            snapshot_records: source.rows.snapshot_records,
            result_rows: 0,
        };
        let mut control = |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let result_rows = if event == GlaExecutionEvent::ResultRow {
                let next = rows.result_rows.checked_add(1).ok_or_else(|| {
                    GqlQueryError::Source(GraphAggregateError::ResultCountOverflow)
                })?;
                policy
                    .rows
                    .check(GqlBudgetDimension::ResultRows, next)
                    .map_err(GqlQueryError::Rows)?;
                next
            } else {
                rows.result_rows
            };
            evaluator
                .charge_event(policy.evaluator, event)
                .map_err(GqlQueryError::Evaluator)?;
            rows.result_rows = result_rows;
            Ok(())
        };
        // Borrow the completed input through final result copying. Empty input
        // still makes one keyless zero/null group; LIMIT 0 never skips input
        // failures or aggregation. No row payload clone is needed at this seam.
        let value = self.summarize_projected_rows(&source.value, &mut control)?;
        control(GlaExecutionEvent::Work)?;
        Ok(GqlQueryExecution {
            value,
            rows,
            evaluator,
        })
    }
}
