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
                    input.types[aggregate
                        .argument_column()
                        .expect("native extremum argument")]
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

/// Bound input expressions and native aggregate declarations for a write
/// RETURN. Preparation below checks them against the write collector's actual
/// binding schema before any source or effect can execute.
pub(crate) struct WriteReturnGroupSpec {
    pub(crate) inputs: Vec<crate::GraphSetProjection>,
    pub(crate) keys: Vec<usize>,
    pub(crate) aggregates: Vec<crate::set_text::aggregate::ReadAggregateSpec>,
}

/// One checked grouping stage over frozen write occurrences. No graph source
/// is retained or callable; the owned-row reducer uses the same exact native
/// accumulators and checked ordinary-row conversion as a read WITH grouping.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct WriteReturnGroup {
    pub(in crate::set_ops) inputs: Vec<crate::GraphSetProjection>,
    summary: crate::aggregation::OwnedRowAggregate,
    types: Vec<GraphSetColumnType>,
}

impl WriteReturnGroupSpec {
    pub(crate) fn prepare(
        self,
        input: Vec<GraphSetColumnType>,
    ) -> Result<WriteReturnGroup, GraphAggregateBuildError> {
        let checked = crate::row_projection::RowProjectionSpec::new(
            input,
            self.inputs.clone(),
            GraphSetQuantifier::All,
        )
        .map_err(GraphAggregateBuildError::InputRows)?;
        let columns = checked.columns().map(str::to_owned).collect::<Vec<_>>();
        let declarations = self
            .aggregates
            .iter()
            .map(crate::set_text::aggregate::ReadAggregateSpec::declaration)
            .collect::<Vec<_>>();
        let summary =
            crate::aggregation::OwnedRowAggregate::prepare(&columns, &self.keys, &declarations)?;
        let input_types = checked.column_types();
        let mut types = self
            .keys
            .iter()
            .map(|&column| input_types[column])
            .collect::<Vec<_>>();
        for aggregate in &self.aggregates {
            types.push(match aggregate.function {
                Function::CountRows
                | Function::Count
                | Function::CountDistinct
                | Function::SumInt
                | Function::SumIntDistinct
                | Function::AverageInt
                | Function::AverageIntDistinct => GraphSetColumnType::Scalar,
                Function::Collect | Function::CollectDistinct => GraphSetColumnType::List,
                Function::Min | Function::Max => {
                    input_types[aggregate.column.expect("native extremum argument")]
                }
            });
        }
        Ok(WriteReturnGroup {
            inputs: self.inputs,
            summary,
            types,
        })
    }
}

impl WriteReturnGroup {
    pub(crate) fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }

    pub(in crate::set_ops) fn summarize_value_rows<E, C>(
        &self,
        input: &[GraphValueRow],
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<Vec<GraphValueRow>, GqlQueryError<GraphAggregateError<E>, C>> {
        let rows = self.summary.summarize(input, control)?;
        super::rows::convert_rows(rows, control)
    }

    pub(crate) fn append_canonical_bytes(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(b"fgdb:write-return-group:v1\0");
        bytes.extend_from_slice(&(self.inputs.len() as u64).to_be_bytes());
        for input in &self.inputs {
            input.value().append_canonical_bytes(bytes);
        }
        let summary = self.summary.canonical_bytes();
        bytes.extend_from_slice(&(summary.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&summary);
    }
}

#[cfg(test)]
mod tests;
