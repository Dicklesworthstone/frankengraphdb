//! Row-local relational input between one admitted graph scan and grouping.
//!
//! Preparation walks the checked relational algebra once, retaining its exact
//! ALL projections, filters and scan-local UNWINDs in execution order. Runtime
//! delegates to the existing projection, predicate and expansion kernels; it
//! does not interpret text, read another graph, or materialize a relation. No
//! order-sensitive operator may be stripped to get at its first graph leaf.

use crate::algebra::{GraphValueRow, PreparedGraphPattern};
use crate::set_ops::{ProjectionFailure, UnwindRows, no_element_reads};
use crate::{
    GlaExecutionEvent, GqlQueryError, GraphAggregateError, GraphAggregateFunction,
    GraphSetColumnType, GraphSetExecutionError, GraphSetPredicateOp, GraphSetProjection,
    GraphSetQuantifier, GraphSetValue, PreparedGraphAggregate, PreparedGraphSet,
};

#[derive(Clone)]
enum Stage {
    Project(Vec<GraphSetProjection>),
    Filter(Vec<GraphSetPredicateOp>),
    Unwind(GraphSetValue),
}

/// Private input program for order-independent aggregate consumers. Relational
/// projections normally canonicalize their complete output. Eliding that sort
/// is legal here only because there are no pages/DISTINCT/order-sensitive
/// consumers between the source and the final exact groups. COLLECT is refused
/// by the physical aggregate compiler, including when its input is empty.
#[derive(Clone)]
pub(crate) struct InputPipeline {
    stages: Vec<Stage>,
    expands: bool,
}

/// One admitted source binding and a depth-first expansion spine. Each UNWIND
/// owns its original prefix and one evaluated list, or just range bounds. An
/// emitted occurrence is consumed by the reducer before another is generated;
/// nested expansions never retain their Cartesian output bag. All allocation
/// and expression work uses the enclosing cursor's original cumulative meter.
pub(crate) struct InputRows {
    current: Option<(usize, GraphValueRow)>,
    expansions: Vec<(usize, UnwindRows)>,
}

impl InputPipeline {
    /// Shared admission for resident vertex and edge reducers. Source compilers
    /// still admit the complete graph leaf independently; this only admits the
    /// row-local stages and normalizes the existing completed-group clauses.
    pub(crate) fn prepare(
        aggregate: &PreparedGraphAggregate,
    ) -> Option<(PreparedGraphAggregate, Option<Self>)> {
        Self::prepare_output(aggregate, false, false)
    }

    /// Resident reducers own a resumable expansion spine. The synchronous
    /// external owner still consumes a one-row transformation and therefore
    /// keeps its old fanout refusal; its complete relational spill path owns
    /// UNWIND separately. Never hand that one-row consumer an expanding plan.
    pub(crate) fn prepare_expanding(
        aggregate: &PreparedGraphAggregate,
    ) -> Option<(PreparedGraphAggregate, Option<Self>)> {
        Self::prepare_output(aggregate, false, true)
    }

    /// The resident external owner retains a pinned element reader after input
    /// exhaustion. Only completed-group output gains that capability; every
    /// input-stage admission below remains unchanged.
    pub(crate) fn prepare_with_element_reads(
        aggregate: &PreparedGraphAggregate,
    ) -> Option<(PreparedGraphAggregate, Option<Self>)> {
        Self::prepare_output(aggregate, true, false)
    }

    fn prepare_output(
        aggregate: &PreparedGraphAggregate,
        element_reads: bool,
        expanding: bool,
    ) -> Option<(PreparedGraphAggregate, Option<Self>)> {
        if !element_reads && aggregate.computed_order_requires_element_reads() {
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
        if pipeline.expands && !expanding {
            return None;
        }
        Some((aggregate.prepare_complete_group_output()?, Some(pipeline)))
    }

    pub(super) fn compile(
        relation: &PreparedGraphSet,
        source: &PreparedGraphPattern<GraphValueRow>,
    ) -> Option<Self> {
        let mut current = relation;
        let mut stages = Vec::new();
        let mut expands = false;
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
                return Some(Self { stages, expands });
            }
            if let Some(child) = current.incremental_scope() {
                current = child;
            } else if let Some((child, projection, quantifier)) = current.incremental_projection() {
                if quantifier != GraphSetQuantifier::All
                    || !projection
                        .iter()
                        .map(GraphSetProjection::value)
                        .all(GraphSetValue::is_scan_local)
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
            } else if let Some((child, _, value)) = current.incremental_unwind() {
                if !value.is_scan_local() {
                    return None;
                }
                stages.push(Stage::Unwind(value.clone()));
                expands = true;
                current = child;
            } else {
                // No fallback for binary sets, a second source, CALL,
                // nested grouping, windows, or opaque future relational nodes.
                return None;
            }
        }
    }

    pub(crate) fn expands(&self) -> bool {
        self.expands
    }

    /// Move the already admitted source projection into the private driver.
    /// Its first step performs every new allocation through the caller's meter.
    pub(crate) fn start(&self, row: GraphValueRow) -> InputRows {
        InputRows {
            current: Some((0, row)),
            expansions: Vec::new(),
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
        // Only prepare()/prepare_with_element_reads() feed this one-row API.
        // Refuse defensively instead of dropping expanded output if a future
        // caller bypasses that physical admission.
        if self.expands {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
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
                Stage::Unwind(_) => unreachable!("nonexpanding pipeline checked above"),
            }
        }
        Ok(Some(row))
    }
}

impl InputRows {
    /// Advance bounded pipeline transitions, independently of source-record
    /// count. One source can expand to arbitrarily many governed occurrences;
    /// even fully filtered expansions yield without publishing partial groups.
    /// A refusal is terminal in the enclosing aggregate cursor, which drops
    /// this entire private spine together with its accumulated groups.
    pub(crate) fn advance_bounded<E, C>(
        &mut self,
        pipeline: &InputPipeline,
        remaining: &mut usize,
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<core::task::Poll<Option<GraphValueRow>>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        loop {
            if let Some((at, row)) = self.current.take() {
                if at == pipeline.stages.len() {
                    return Ok(core::task::Poll::Ready(Some(row)));
                }
                if *remaining == 0 {
                    self.current = Some((at, row));
                    return Ok(core::task::Poll::Pending);
                }
                *remaining -= 1;
                control(GlaExecutionEvent::Work)?;
                match &pipeline.stages[at] {
                    Stage::Project(projection) => {
                        let row = GraphSetProjection::evaluate_row_with_control(
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
                        self.current = Some((at + 1, row));
                    }
                    Stage::Filter(predicate) => {
                        if GraphSetPredicateOp::evaluate_row_with_control(predicate, &row, control)?
                        {
                            self.current = Some((at + 1, row));
                        }
                    }
                    Stage::Unwind(value) => {
                        let rows = UnwindRows::new(row, value, control, &mut no_element_reads)
                            .map_err(|error| match error {
                                ProjectionFailure::Control(error) => error,
                                ProjectionFailure::Arithmetic { column, error } => {
                                    GqlQueryError::Source(GraphAggregateError::InputRelation(
                                        GraphSetExecutionError::Projection {
                                            row: 0,
                                            column,
                                            error,
                                        },
                                    ))
                                }
                            })?;
                        control(GlaExecutionEvent::ScratchEntry)?;
                        self.expansions.push((at + 1, rows));
                    }
                }
                continue;
            }
            if self.expansions.is_empty() {
                return Ok(core::task::Poll::Ready(None));
            }
            if *remaining == 0 {
                return Ok(core::task::Poll::Pending);
            }
            *remaining -= 1;
            // The shared kernel charges successful member production. This
            // transition also governs an exhausted or NULL expansion.
            control(GlaExecutionEvent::Work)?;
            let (at, rows) = self.expansions.pop().expect("nonempty expansion spine");
            if let Some((row, rows)) = rows.next_with_control(control)? {
                // Pop/push reuses the same already admitted stack slot.
                self.expansions.push((at, rows));
                self.current = Some((at, row));
            }
        }
    }
}

#[cfg(test)]
mod tests;
