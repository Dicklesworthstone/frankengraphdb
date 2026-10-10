//! Resolved-unbound WITH grouping. Only native row operators execute it.

use super::*;
use crate::GraphAggregateFunction;

#[derive(Clone)]
pub(crate) struct ReadAggregateSpec {
    pub(crate) name: String,
    pub(crate) function: GraphAggregateFunction,
    pub(crate) column: Option<usize>,
}

impl ReadAggregateSpec {
    pub(crate) fn declaration(&self) -> crate::GraphAggregate<'_> {
        use crate::{GraphAggregate as A, GraphAggregateFunction as F};
        match (self.function, self.column) {
            (F::CountRows, None) => A::count_rows(&self.name),
            (F::Count, Some(at)) => A::count(&self.name, at),
            (F::CountDistinct, Some(at)) => A::count_distinct(&self.name, at),
            (F::SumInt, Some(at)) => A::sum_int(&self.name, at),
            (F::SumIntDistinct, Some(at)) => A::sum_int_distinct(&self.name, at),
            (F::AverageInt, Some(at)) => A::average_int(&self.name, at),
            (F::AverageIntDistinct, Some(at)) => A::average_int_distinct(&self.name, at),
            (F::Min, Some(at)) => A::min(&self.name, at),
            (F::Max, Some(at)) => A::max(&self.name, at),
            (F::Collect, Some(at)) => A::collect(&self.name, at),
            (F::CollectDistinct, Some(at)) => A::collect_distinct(&self.name, at),
            _ => unreachable!("native aggregate grammar pairs functions and arguments"),
        }
    }
}

/// Input expressions run before grouping; output slots restore declaration
/// order after the native key-first group schema. Private input names never
/// participate in user alias resolution. All three operators are depth-counted.
#[derive(Clone)]
pub(crate) struct ReadAggregateStage {
    pub(crate) inputs: Vec<ReadProjectionTemplate>,
    pub(crate) keys: Vec<usize>,
    pub(crate) aggregates: Vec<ReadAggregateSpec>,
    pub(crate) outputs: Vec<(String, usize, GraphSetColumnType)>,
    pub(crate) quantifier: GraphSetQuantifier,
}

impl ReadAggregateStage {
    pub(crate) fn column_schema(&self) -> (Vec<String>, Vec<GraphSetColumnType>) {
        self.outputs
            .iter()
            .map(|(name, _, kind)| (name.clone(), *kind))
            .unzip()
    }

    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(b"fgdb:with-group-template:v1\0");
        append_projection_transcript(bytes, &self.inputs);
        bytes.extend_from_slice(&(self.keys.len() as u64).to_be_bytes());
        for &key in &self.keys {
            bytes.extend_from_slice(&(key as u64).to_be_bytes());
        }
        bytes.extend_from_slice(&(self.aggregates.len() as u64).to_be_bytes());
        for aggregate in &self.aggregates {
            bytes.extend_from_slice(&(aggregate.name.len() as u64).to_be_bytes());
            bytes.extend_from_slice(aggregate.name.as_bytes());
            bytes.push(match aggregate.function {
                GraphAggregateFunction::CountRows => 0,
                GraphAggregateFunction::Count => 1,
                GraphAggregateFunction::CountDistinct => 2,
                GraphAggregateFunction::SumInt => 3,
                GraphAggregateFunction::Min => 4,
                GraphAggregateFunction::Max => 5,
                GraphAggregateFunction::SumIntDistinct => 6,
                GraphAggregateFunction::AverageInt => 7,
                GraphAggregateFunction::AverageIntDistinct => 8,
                GraphAggregateFunction::Collect => 9,
                GraphAggregateFunction::CollectDistinct => 10,
            });
            bytes.push(u8::from(aggregate.column.is_some()));
            if let Some(column) = aggregate.column {
                bytes.extend_from_slice(&(column as u64).to_be_bytes());
            }
        }
        bytes.extend_from_slice(&(self.outputs.len() as u64).to_be_bytes());
        for (name, column, kind) in &self.outputs {
            bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(&(*column as u64).to_be_bytes());
            bytes.push(set_column_type_tag(*kind));
        }
        bytes.push(u8::from(self.quantifier == GraphSetQuantifier::Distinct));
    }
}
