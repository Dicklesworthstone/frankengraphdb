//! Completed-group filtering/projection for physical pull reducers.
//!
//! This adapter neither reads graph data nor reconstructs accumulators. It
//! borrows the exact finalized cells through the existing HAVING interpreter
//! and the same result-projection evaluator as incremental/batch consumers.

use super::super::super::having::GroupCells;
use super::*;

#[derive(Clone, Copy)]
struct CompleteGroup<'a>(&'a GraphAggregateRow);
impl<'a> GroupCells<'a> for CompleteGroup<'a> {
    fn cell(self, column: GraphAggregateColumn) -> Cell<'a> {
        match column {
            GraphAggregateColumn::GroupKey(at) => Cell::Value(value_ref(&self.0.keys[at])),
            GraphAggregateColumn::Aggregate(at) => result_cell(&self.0.values[at]),
        }
    }
}

impl PreparedGraphAggregate {
    /// Normalize legacy conjunctive HAVING filters into the SAME bounded
    /// program as textual HAVING, once at physical-plan admission. The original
    /// logical definition/canonical transcript is never changed. All operands,
    /// including hidden keys and summaries, address the full evaluation schema.
    /// This private gate does not widen any incremental-maintenance contract.
    pub(crate) fn prepare_streamed_output(&self) -> Option<Self> {
        if self.supports_row_local_aggregate_stream() {
            return Some(self.clone());
        }
        if self.relational_input.is_some() || self.output_distinct || !self.ordering.is_empty() {
            return None;
        }
        let mut physical = self.clone();
        if !self.having.is_empty() {
            let mut program = Vec::new();
            for (at, filter) in self.having.iter().enumerate() {
                let operand = GraphHavingOperand::Column(filter.column);
                program.push(match filter.test {
                    GraphAggregateTest::IsNull => GraphHavingOp::IsNull {
                        operand,
                        is_null: true,
                    },
                    GraphAggregateTest::IsNotNull => GraphHavingOp::IsNull {
                        operand,
                        is_null: false,
                    },
                    GraphAggregateTest::Integer { comparison, value } => GraphHavingOp::Compare {
                        left: operand,
                        comparison,
                        right: GraphHavingOperand::Integer(value),
                    },
                });
                if at != 0 {
                    program.push(GraphHavingOp::And);
                }
            }
            physical.having_expression = Some(GraphHavingExpression::prepare(&program).ok()?);
            physical.having.clear();
        }
        Some(physical)
    }

    /// Plain reductions keep their original event sequence and move-only
    /// delivery. Clauses require a complete pre-delivery validation pass, even
    /// when the result window is empty or earlier groups fill the requested page.
    pub(crate) fn has_streamed_output_stage(&self) -> bool {
        self.having_expression.is_some()
            || self.output_projection.is_some()
            || self.key_output.is_some()
            || self.output_aggregates != self.aggregates.len()
            || self.offset != 0
            || self.count.is_some()
    }

    /// The owning reducer must supply every complete group in canonical key
    /// order. None is only HAVING FALSE/UNKNOWN, never a schema or source error.
    /// Apply pagination after this call: every qualified output expression is
    /// evaluated even for skipped/off-page groups. No ResultRow event is emitted
    /// here; only actual delivery consumes that counter.
    pub(crate) fn evaluate_streamed_output<E, C>(
        &self,
        row: GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), QueryError<E, C>>,
    ) -> Result<Option<GraphAggregateRow>, QueryError<E, C>> {
        debug_assert_eq!(row.keys.len(), self.keys.len());
        debug_assert_eq!(row.values.len(), self.aggregates.len());
        control(GlaExecutionEvent::Work)?;
        if let Some(having) = &self.having_expression {
            if !having.evaluate(CompleteGroup(&row), control)? {
                return Ok(None);
            }
        }
        if self.output_projection.is_none()
            && self.key_output.is_none()
            && self.output_aggregates == self.aggregates.len()
        {
            return Ok(Some(row));
        }
        self.project_complete_output(&row, control).map(Some)
    }
}
