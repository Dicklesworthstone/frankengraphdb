//! Relational input for the existing exact GroupAggregate.
//!
//! Single-source callers retain their admitted graph contract. Compound callers
//! supply every actual leaf through the set engine's source adapter. Both paths
//! feed the existing exact group/result engines, never a synthetic graph.
//! Eligible order-preserving expansions fold into the owned numeric states;
//! sorting barriers and unsupported functions retain the materialized path.

mod folded;
mod incremental;

use super::*;
use crate::{
    GraphSetProjection, GraphSetQuantifier, GraphSetValue, PreparedGraphSet,
    PreparedGraphSetAggregate,
};
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
    /// Eligible row expansions fold without an expanded intermediate bag.
    /// Leaves, sorting barriers and group state remain in memory, not spill.
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
        let source = relation
            .first_pattern_input()
            .cloned()
            .unwrap_or_else(PreparedGraphPattern::empty);
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

    /// Normalize native COLLECT with a proved occurrence order into the
    /// existing relational group owner. The retained source is the complete
    /// original graph plan, not a fabricated schema leaf. Every input expression
    /// and result clause remains in the definition.
    ///
    /// Ordinary native vertex scans visit ascending identities. Their private
    /// leading identity makes that occurrence order explicit in the row bag.
    /// Uncaptured fixed-edge scans visit ascending source/destination pairs;
    /// both identities carry this order, with exact parallel-edge multiplicity.
    /// Reverse and undirected scans use their bound endpoint orientations.
    /// Computed inputs instead retain the original complete-row ordering used
    /// by their materialized source, before evaluating argument expressions.
    /// Equal source rows have equal computed values and preserve their full
    /// multiplicity. Neither argument order nor change-arrival order can stand
    /// in for the original occurrence sequence.
    ///
    /// Computed inputs can retain multi-hop and scoped graph sources when
    /// their complete rows use canonical ordering. Unprojected inputs still
    /// require the native vertex/one-edge occurrence proof; visitation order
    /// must never be replaced with argument order. Hidden source columns,
    /// noncanonical input ordering and unsupported schema widths refuse.
    /// The consumer must independently admit and meter the COMPLETE returned
    /// relation through its ordinary maintained circuit. This normalization
    /// does not add an eager fallback for unsupported source operators.
    /// Iterator callers that supply vertices in arbitrary order have no native
    /// ordering guarantee.
    #[must_use]
    pub fn incremental_collection_relation(&self) -> Option<PreparedGraphSetAggregate> {
        if self.relational_input.is_some()
            || !self.aggregates.iter().any(|aggregate| {
                matches!(
                    aggregate.function,
                    GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct
                )
            })
        {
            return None;
        }
        let mut names: BTreeSet<_> = self.input.columns().iter().cloned().collect();
        if let Some(computed) = &self.computed_input {
            names.extend(computed.iter().map(|column| column.name().to_owned()));
        }
        let mut ordinal = 0;
        let mut private_name = || loop {
            let name = format!("_fgdb_collect_order_{ordinal}");
            ordinal += 1;
            if names.insert(name.clone()) {
                break name;
            }
        };
        let (source, projection, prefix) =
            match &self.computed_input {
                Some(computed) => {
                    // Batch computed aggregation first materializes and orders
                    // COMPLETE graph rows, then evaluates every input expression.
                    // This order does not depend on graph topology: the source
                    // may contain joins, OPTIONAL or EXISTS. Keep those rows as
                    // a private leading key, not the computed argument values.
                    let [
                        ..,
                        GlaOperator::ProjectValues { columns },
                        GlaOperator::OrderByValues,
                        GlaOperator::Limit {
                            offset: 0,
                            count: None,
                        },
                    ] = self.input.plan().operators()
                    else {
                        return None;
                    };
                    if columns.is_empty()
                        || columns.len() != self.input.columns().len()
                        || columns.len() >= MAX_PATTERN_VERTICES
                        || !self.input.preserves_duplicates()
                    {
                        // Hidden order columns cannot be reconstructed from a
                        // visible row. Reserve the counted source's private
                        // COUNT column inside its complete-schema bound.
                        return None;
                    }
                    let source = self.input_pattern().clone();
                    let prefix = source.columns().len();
                    let mut projection: Vec<_> = (0..prefix)
                        .map(|column| {
                            GraphSetProjection::new(private_name(), GraphSetValue::Column(column))
                        })
                        .collect();
                    projection.extend_from_slice(computed);
                    (source, projection, prefix)
                }
                None => {
                    let names = [private_name(), private_name()];
                    let (source, identities) =
                        self.input.incremental_collection_source(Some(&[
                            names[0].as_str(),
                            names[1].as_str(),
                        ]))?;
                    let prefix = identities.len();
                    let mut projection: Vec<_> = names
                        .into_iter()
                        .zip(identities)
                        .map(|(name, column)| {
                            GraphSetProjection::new(name, GraphSetValue::Column(column))
                        })
                        .collect();
                    projection.extend(self.input.columns().iter().enumerate().map(
                        |(column, name)| {
                            GraphSetProjection::new(name.clone(), GraphSetValue::Column(column))
                        },
                    ));
                    (source, projection, prefix)
                }
            };
        let relation = PreparedGraphSet::from(source.clone())
            .project(projection, GraphSetQuantifier::All)
            .ok()?;
        relation.check_parent_depth().ok()?;
        let mut summary = self.clone();
        summary.input = AggregateInput(Some(source));
        summary.computed_input = None;
        summary.relational_input = Some(relation);
        // Only input addresses shift. Output keys, hidden aggregate slots,
        // HAVING and scoped collection-output expressions keep their schema.
        for column in &mut summary.keys {
            *column += prefix;
        }
        for aggregate in &mut summary.aggregates {
            if let Some(column) = &mut aggregate.column {
                *column += prefix;
            }
        }
        PreparedGraphSetAggregate::from_relation(summary)
    }

    /// Reuse native grouping inside a relational operator whose COMPLETE
    /// input has already executed under the caller's meter. The caller must
    /// retain that input and its checked schema through result materialization.
    /// No source admission or fresh allowance is introduced at this seam.
    pub(crate) fn summarize_relation_rows<E, C>(
        &self,
        input: &[GraphValueRow],
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<Vec<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>> {
        debug_assert!(self.relational_input.is_some());
        self.summarize_projected_rows(input, control)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_relational_governed<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        elements: &mut crate::set_ops::ElementRead<'_, E>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        // Each borrow exists for one checkpoint, never around source execution.
        // Single-source preparation still proves that the iterators are taken
        // once; compound execution cannot enter this iterator-only interface.
        let checkpoint = RefCell::new(&mut checkpoint);
        let mut admitted = Some((vertices, edges));
        let mut source = |pattern: &PreparedGraphPattern<GraphValueRow>,
                          remaining: GqlQueryPolicy| {
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
        };
        self.execute_relational_with_source(
            policy,
            crate::set_ops::BorrowedSource {
                source: &mut source,
                elements: Some(elements),
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
        source: impl crate::GraphSetSource<E, C>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        // Select before observing a source. A runtime failure never falls
        // back, repeats a graph read, or starts a fresh allowance.
        if let Some(physical) = self.folded_definition() {
            return physical.execute_relational_folded(policy, source, checkpoint);
        }
        self.execute_relational_materialized(policy, source, checkpoint)
    }

    fn execute_relational_materialized<E, C>(
        &self,
        policy: GqlQueryPolicy,
        mut source: impl crate::GraphSetSource<E, C>,
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
        let input = relation
            .execute_with_source(source_policy, &mut source, &mut checkpoint)
            .map_err(|error| error.map_source(GraphAggregateError::InputRelation))?;
        let mut evaluator = input.evaluator;
        let mut rows = GqlExecutionStats {
            snapshot_records: input.rows.snapshot_records,
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
        let value = self.summarize_projected_rows_with_elements(
            &input.value,
            &mut |element, key| source.element_property(element, key),
            &mut control,
        )?;
        control(GlaExecutionEvent::Work)?;
        Ok(GqlQueryExecution {
            value,
            rows,
            evaluator,
        })
    }
}
