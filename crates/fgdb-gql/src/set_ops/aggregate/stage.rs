//! A composable native GroupAggregate, not a terminal-only summary (fgdb-ezgeq).

use super::*;
use crate::{GlaExecutionEvent, GraphAggregateFunction as Function};

impl PreparedGraphSet {
    /// Group the COMPLETE selected input relation, producing ordinary rows
    /// that can feed projection, WHERE, ORDER BY, UNWIND, joins, set operations
    /// or another grouping stage. Keys precede aggregate columns in the output.
    /// Keys and aggregate arguments address this input's final column schema.
    ///
    /// Child DISTINCT/order/page clauses remain input boundaries. This stage
    /// has no implicit final page: downstream selection runs only after native
    /// grouping and checked row conversion have both finished. Empty keyless
    /// input produces one zero/null/empty-list group; grouped empty input has
    /// no rows. Native argument DISTINCT, null and collection-order semantics
    /// are reused unchanged, not reimplemented by this relational operator.
    ///
    /// Ordinary rows have an Int64 scalar domain. Counts/sums outside it and
    /// nonnull exact averages refuse with a typed aggregate output error, never
    /// truncate or round. For unrestricted wide/exact numeric output use the
    /// terminal PreparedGraphSetAggregate interface. This stage is bounded
    /// materialization; it does not claim spill or standing-query maintenance.
    pub fn group_by(
        self,
        keys: &[usize],
        aggregates: &[GraphAggregate<'_>],
    ) -> Result<Self, GraphAggregateBuildError> {
        let summary = PreparedGraphSetAggregate::prepare(self, keys, aggregates, 0, None)?;
        let input = summary.input();
        // Native preparation checked all positions, names and the shared
        // depth/width bounds before any source or expression can execute.
        let mut types: Vec<_> = keys.iter().map(|&column| input.types[column]).collect();
        for aggregate in summary.summary.aggregates() {
            types.push(match aggregate.function() {
                Function::CountRows
                | Function::Count
                | Function::CountDistinct
                | Function::SumInt
                | Function::SumIntDistinct
                | Function::AverageInt
                | Function::AverageIntDistinct => GraphSetColumnType::Scalar,
                Function::Collect | Function::CollectDistinct => GraphSetColumnType::List,
                Function::Min | Function::Max => {
                    input.types[aggregate.argument_column().expect("native extremum argument")]
                }
            });
        }
        let columns = summary
            .key_columns()
            .iter()
            .chain(summary.aggregate_columns())
            .cloned()
            .collect();
        let operands = input.operands;
        let depth = input.depth + 1;
        Ok(Self {
            node: SetNode::Aggregate(Box::new(summary)),
            columns,
            types,
            operands,
            depth,
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }
}

impl PreparedGraphSetAggregate {
    /// Only the enclosing relational executor supplies these already-admitted
    /// rows. Reuse its control closure for accumulation, ownership and checked
    /// conversion; never call the graph source again from inside the reducer.
    pub(in crate::set_ops) fn summarize_value_rows<E, C>(
        &self,
        input: &[GraphValueRow],
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<Vec<GraphValueRow>, GqlQueryError<GraphAggregateError<E>, C>> {
        let rows = self.summary.summarize_relation_rows(input, control)?;
        super::rows::convert_rows(rows, control)
    }
}

#[cfg(test)]
mod tests;
