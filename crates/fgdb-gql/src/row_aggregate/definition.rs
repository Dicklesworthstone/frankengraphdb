//! Source-independent access to checked native aggregate definitions.
//!
//! Both single-source and compound owners expose only grouping/presentation
//! here. In particular, a compound owner never hands out its first graph leaf
//! as an executable substitute for its complete relational input.

use crate::algebra::GraphValue;
use crate::{GlaExecutionEvent, GqlQueryError, GraphAggregateError, GraphAggregateFilter,
    GraphAggregateFunction, GraphAggregateOrder, GraphAggregateRow, GraphAggregateValue,
    GraphHavingExpression, GraphSetColumnType};
use core::convert::Infallible;

pub(crate) mod sealed {
    pub trait Sealed {}
}

/// A checked complete-group and result-stage definition, not a graph source.
/// Implementations are sealed to the two ordinary prepared aggregate owners.
/// Row producers must separately admit the COMPLETE input and every aggregate
/// function, validate changed input counts, and publish their state atomically.
/// None from a helper is a definition/schema refusal, never an empty SQL value.
pub trait GroupDefinition: sealed::Sealed + Clone {
    fn group_key_columns(&self) -> &[usize];
    fn aggregate_specs(&self)
        -> impl ExactSizeIterator<Item = (GraphAggregateFunction, Option<usize>)> + '_;
    fn incremental_input_column_type(&self, column: usize) -> Option<GraphSetColumnType>;
    fn supports_incremental_maintenance_with_having(&self) -> bool;
    fn having(&self) -> &[GraphAggregateFilter];
    fn having_expression(&self) -> Option<&GraphHavingExpression>;
    fn ordering(&self) -> &[GraphAggregateOrder];
    fn has_incremental_output_transform(&self) -> bool;
    fn has_incremental_ranking(&self) -> bool;
    fn incremental_output_is_distinct(&self) -> bool;
    fn incremental_result_window(&self) -> (u64, Option<u64>);

    /// Remove only output transformations. Matching, input row stages,
    /// grouping and HAVING remain in this SAME source-aware owner type.
    fn complete_groups(&self) -> Option<Self>;
    fn materialize_incremental_row(&self, keys: Vec<GraphValue>,
        values: Vec<GraphAggregateValue>) -> Option<GraphAggregateRow>;
    fn evaluate_incremental_having<C>(&self, row: &GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<bool>, GqlQueryError<GraphAggregateError<Infallible>, C>>;
    fn project_incremental_output<C>(&self, row: &GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>)
        -> Result<Option<GraphAggregateRow>, GqlQueryError<GraphAggregateError<Infallible>, C>>;
}

// Forward to the ONE native implementation. No expression, comparison,
// aggregate or column-domain evaluator is duplicated by this interface.
macro_rules! delegate_group_definition {
    ($owner:ty, $this:ident => $inner:expr, $wrap:expr) => {
        impl $crate::row_aggregate::definition::sealed::Sealed for $owner {}
        impl $crate::row_aggregate::definition::GroupDefinition for $owner {
            fn group_key_columns(&self) -> &[usize] {
                let $this = self; ($inner).group_key_columns()
            }
            fn aggregate_specs(&self) -> impl ExactSizeIterator<
                Item = ($crate::GraphAggregateFunction, Option<usize>)> + '_ {
                let $this = self;
                ($inner).aggregates().iter().map(|a| (a.function(), a.argument_column()))
            }
            fn incremental_input_column_type(&self, column: usize) -> Option<$crate::GraphSetColumnType> {
                let $this = self; ($inner).incremental_input_column_type(column)
            }
            fn supports_incremental_maintenance_with_having(&self) -> bool {
                let $this = self; ($inner).supports_incremental_maintenance_with_having()
            }
            fn having(&self) -> &[$crate::GraphAggregateFilter] {
                let $this = self; ($inner).having()
            }
            fn having_expression(&self) -> Option<&$crate::GraphHavingExpression> {
                let $this = self; ($inner).having_expression()
            }
            fn ordering(&self) -> &[$crate::GraphAggregateOrder] {
                let $this = self; ($inner).ordering()
            }
            fn has_incremental_output_transform(&self) -> bool {
                let $this = self; ($inner).has_incremental_output_transform()
            }
            fn has_incremental_ranking(&self) -> bool {
                let $this = self; ($inner).has_incremental_ranking()
            }
            fn incremental_output_is_distinct(&self) -> bool {
                let $this = self; ($inner).incremental_output_is_distinct()
            }
            fn incremental_result_window(&self) -> (u64, Option<u64>) {
                let $this = self; ($inner).incremental_result_window()
            }
            fn complete_groups(&self) -> Option<Self> {
                let $this = self;
                ($inner).incremental_source_definition().map($wrap)
            }
            fn materialize_incremental_row(&self, keys: Vec<$crate::algebra::GraphValue>,
                values: Vec<$crate::GraphAggregateValue>) -> Option<$crate::GraphAggregateRow> {
                let $this = self; ($inner).materialize_incremental_row(keys, values)
            }
            fn evaluate_incremental_having<C>(&self, row: &$crate::GraphAggregateRow,
                control: &mut impl FnMut($crate::GlaExecutionEvent) -> Result<(), C>)
                -> Result<Option<bool>, $crate::GqlQueryError<
                    $crate::GraphAggregateError<core::convert::Infallible>, C>> {
                let $this = self; ($inner).evaluate_incremental_having(row, control)
            }
            fn project_incremental_output<C>(&self, row: &$crate::GraphAggregateRow,
                control: &mut impl FnMut($crate::GlaExecutionEvent) -> Result<(), C>)
                -> Result<Option<$crate::GraphAggregateRow>, $crate::GqlQueryError<
                    $crate::GraphAggregateError<core::convert::Infallible>, C>> {
                let $this = self; ($inner).project_incremental_output(row, control)
            }
        }
    };
}
pub(crate) use delegate_group_definition;

delegate_group_definition!(crate::PreparedGraphAggregate, owner => owner, |definition| definition);

#[cfg(test)]
mod tests;
