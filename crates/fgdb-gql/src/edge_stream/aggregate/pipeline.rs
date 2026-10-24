//! Row-local relational input between one admitted graph scan and grouping.
//!
//! Preparation walks the checked relational algebra once, retaining its exact
//! ALL projections and filters in execution order. Runtime delegates to the
//! existing projection and predicate kernels; it does not interpret text, read
//! another graph, or materialize a relation. No order-sensitive operator may
//! be stripped to get at its first graph leaf.

use crate::algebra::{GraphValueRow, PreparedGraphPattern};
use crate::{
    GlaExecutionEvent, GqlQueryError, GraphAggregateError, GraphAggregateFunction,
    GraphSetColumnType, GraphSetExecutionError, GraphSetPredicateOp, GraphSetProjection,
    GraphSetQuantifier, GraphSetValue, PreparedGraphAggregate, PreparedGraphSet,
};

#[derive(Clone)]
enum Stage {
    Project(Vec<GraphSetProjection>),
    Filter(Vec<GraphSetPredicateOp>),
}

/// Private input program for order-independent aggregate consumers. Relational
/// projections normally canonicalize their complete output. Eliding that sort
/// is legal here only because there are no pages/DISTINCT/order-sensitive
/// consumers between the source and the final exact groups. COLLECT is refused
/// by the physical aggregate compiler, including when its input is empty.
#[derive(Clone)]
pub(crate) struct InputPipeline {
    stages: Vec<Stage>,
}

impl InputPipeline {
    /// Shared admission for resident vertex and edge reducers. Source compilers
    /// still admit the complete graph leaf independently; this only admits the
    /// row-local stages and normalizes the existing completed-group clauses.
    pub(crate) fn prepare(
        aggregate: &PreparedGraphAggregate,
    ) -> Option<(PreparedGraphAggregate, Option<Self>)> {
        if aggregate.computed_order_requires_element_reads() {
            return None;
        }
        let Some(relation) = aggregate.input_relation() else {
            return Some((aggregate.prepare_streamed_output()?, None));
        };
        // Do not erase another input transform if a future constructor can
        // represent both. Each admitted relational stage already owns its VM.
        if aggregate.input_projection().is_some()
            || aggregate
                .aggregates()
                .iter()
                .any(|spec| match spec.function() {
                    GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct => {
                        true
                    }
                    // Preserve the folded reducer's open-domain extremum boundary.
                    GraphAggregateFunction::Min | GraphAggregateFunction::Max => {
                        spec.argument_column().is_some_and(|column| {
                            relation.column_types()[column] == GraphSetColumnType::Any
                        })
                    }
                    _ => false,
                })
        {
            return None;
        }
        let pipeline = Self::compile(relation, aggregate.input_pattern())?;
        Some((aggregate.prepare_complete_group_output()?, Some(pipeline)))
    }

    pub(super) fn compile(
        relation: &PreparedGraphSet,
        source: &PreparedGraphPattern<GraphValueRow>,
    ) -> Option<Self> {
        let mut current = relation;
        let mut stages = Vec::new();
        // Every accessor below preserves its complete child and refuses this
        // node's ordering/page, including LIMIT 0. PreparedGraphSet's depth
        // bound also bounds this compilation and the retained program.
        loop {
            if let Some(pattern) = current.incremental_pattern() {
                // The aggregate owns this exact, sole source. Do not accept a
                // matching schema as proof that another graph leaf is the same.
                if pattern != source {
                    return None;
                }
                stages.reverse();
                return Some(Self { stages });
            }
            if let Some(child) = current.incremental_scope() {
                current = child;
            } else if let Some((child, projection, quantifier)) = current.incremental_projection() {
                if quantifier != GraphSetQuantifier::All
                    || !projection.iter().all(|column| {
                        matches!(
                            column.value(),
                            GraphSetValue::Column(_)
                                | GraphSetValue::Literal(_)
                                | GraphSetValue::TypedNull(_)
                                | GraphSetValue::Integer(_)
                                | GraphSetValue::Value(_)
                        )
                    })
                {
                    // rand() depends on each relational stage's canonical
                    // input ordinal, not physical graph order. MapGet may need
                    // a scoped element reader. Neither is a row-only scalar
                    // program; even a dead branch must not erase its contract.
                    return None;
                }
                stages.push(Stage::Project(projection.to_vec()));
                current = child;
            } else if let Some((child, predicate)) = current.incremental_filter() {
                stages.push(Stage::Filter(predicate.to_vec()));
                current = child;
            } else {
                // No fallback for binary sets, a second source, UNWIND, CALL,
                // nested grouping, windows, or opaque future relational nodes.
                return None;
            }
        }
    }

    /// Consume one complete binding. FALSE/UNKNOWN drops only this binding;
    /// source, expression and control errors are never absence. All declared
    /// columns execute before a following filter, including unused expressions;
    /// rejected rows do not execute later stages. Predicate instructions and
    /// projection payloads use the ordinary cumulative query control callback.
    ///
    /// Failure order is physical pull order, not batch phase order. Projection
    /// diagnostics name row zero within this one-row invocation, as do the
    /// existing computed-input streaming and incremental adapters.
    pub(crate) fn evaluate<E, C>(
        &self,
        mut row: GraphValueRow,
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<Option<GraphValueRow>, GqlQueryError<GraphAggregateError<E>, C>> {
        for stage in &self.stages {
            control(GlaExecutionEvent::Work)?;
            match stage {
                Stage::Project(projection) => {
                    row = GraphSetProjection::evaluate_row_with_control(
                        &row,
                        projection,
                        control,
                        |column, error| {
                            GqlQueryError::Source(GraphAggregateError::InputRelation(
                                GraphSetExecutionError::Projection {
                                    row: 0,
                                    column,
                                    error,
                                },
                            ))
                        },
                    )?;
                }
                Stage::Filter(predicate) => {
                    if !GraphSetPredicateOp::evaluate_row_with_control(predicate, &row, control)? {
                        return Ok(None);
                    }
                }
            }
        }
        Ok(Some(row))
    }
}

#[cfg(test)]
mod tests;
