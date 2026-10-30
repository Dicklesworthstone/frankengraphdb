//! Exact counts from a source's already admitted live cardinality.
//!
//! This is a checked physical input to the ordinary aggregate finalizer, not
//! a cached result. Source admission stays the host's responsibility. Only
//! plans with filterless input or one metadata-proven vertex label qualify.
//! The scan-summary path preserves the original evaluator event order. An
//! exact-metadata input spends only its own lookup and finalization work.

use super::*;
use crate::algebra::{EdgeRelation, GlaDirection};
use fgdb_delta_types::LabelId;

/// The live source whose exact cardinality a checked COUNT(*) can use.
/// A typed edge count examines every admitted edge, including other relations,
/// but only occurrences of the named relation contribute to its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphCountSource {
    Vertices,
    /// Every directed edge occurrence, including parallel edges and self-loops.
    /// Only exact metadata can replace this input; wildcard scans retain the
    /// ordinary evaluator's per-binding work and scratch transcript.
    Edges,
    /// Vertices with one label, counted once even when they have other labels.
    /// Only exact metadata can replace this input; ordinary label scans retain
    /// their predicate reads and interleaved work/scratch transcript.
    Label(LabelId),
    Relation(RelationId),
}

struct Shape {
    source: GraphCountSource,
    columns: usize,
}

/// A source summary bound to one checked aggregate definition. It borrows
/// that definition, including its output expressions, HAVING and pagination;
/// none can be replaced or omitted while executing the summary.
pub struct PreparedGraphCount<'query> {
    query: &'query PreparedGraphAggregate,
    shape: Shape,
    examined: u64,
    matching: u64,
    metadata: bool,
}

impl PreparedGraphAggregate {
    fn count_shape(&self) -> Option<Shape> {
        if self.computed_input.is_some()
            || self.relational_input.is_some()
            || !self.keys.is_empty()
            || self.aggregates.is_empty()
            || self
                .aggregates
                .iter()
                .any(|aggregate| aggregate.function != GraphAggregateFunction::CountRows)
        {
            return None;
        }
        let operators = self.input.plan().operators();
        let (mut source, slots) = match operators.first()? {
            GlaOperator::ScanVertices => (GraphCountSource::Vertices, 1),
            GlaOperator::ScanEdges {
                relation,
                direction: GlaDirection::Forward | GlaDirection::Reverse,
            } => (
                match relation {
                    EdgeRelation::One(relation) => GraphCountSource::Relation(*relation),
                    EdgeRelation::Any => GraphCountSource::Edges,
                    EdgeRelation::Set(_) => return None,
                },
                2,
            ),
            _ => return None,
        };
        let mut columns = None;
        for operator in &operators[1..] {
            match operator {
                GlaOperator::Select { slot, predicates }
                    if source == GraphCountSource::Vertices
                        && columns.is_none()
                        && slot.ordinal() == 0 =>
                {
                    let [crate::algebra::VertexPredicate::HasLabel(label)] = predicates.as_slice()
                    else {
                        return None;
                    };
                    source = GraphCountSource::Label(*label);
                }
                GlaOperator::ProjectValues { columns: output }
                    if columns.is_none()
                        && output.iter().all(|column| {
                            matches!(column, ValueProjection::Vertex { slot } if slot.ordinal() < slots)
                        }) =>
                {
                    columns = Some(output.len());
                }
                GlaOperator::OrderByValues if columns.is_some() => {}
                GlaOperator::Limit {
                    offset: 0,
                    count: None,
                } if columns.is_some() => {}
                // In particular, even a SelectBoolean that currently looks
                // constant keeps its normal expression/error semantics.
                _ => return None,
            }
        }
        Some(Shape {
            source,
            columns: columns?,
        })
    }

    /// Whether exact live-source counts can replace this definition's input
    /// traversal. Labels, wildcard edges, filters, grouping, computed values,
    /// undirected or multi-hop patterns and other aggregate functions return
    /// `None`. Wildcard edges have a different ordinary visitor transcript.
    #[must_use]
    pub fn count_summary_source(&self) -> Option<GraphCountSource> {
        self.count_shape().and_then(|shape| match shape.source {
            GraphCountSource::Edges | GraphCountSource::Label(_) => None,
            source => Some(source),
        })
    }

    /// The exact current-snapshot cardinality this definition may consume.
    /// Filterless directed edges may name one relation or match every type;
    /// one vertex-label predicate also qualifies. Multiple labels, property
    /// predicates and label-filtered edge counts do
    /// not: independent populations cannot prove an intersection's size.
    #[must_use]
    pub fn count_metadata_source(&self) -> Option<GraphCountSource> {
        self.count_shape().map(|shape| shape.source)
    }

    /// Bind a trusted source's exact, already admitted counts to this query.
    /// `examined` counts all live rows that the ordinary input iterator would
    /// yield; `matching` counts the named relation's occurrences, or the same
    /// live vertex count for a vertex input. Both must describe the same
    /// snapshot, cut and visible scope as source admission. This constructor
    /// checks the query shape, source kind and the counts' structural bounds
    /// before the host spends any admission work.
    #[must_use]
    pub fn prepare_count_summary(
        &self,
        source: GraphCountSource,
        examined: u64,
        matching: u64,
    ) -> Option<PreparedGraphCount<'_>> {
        let shape = self.count_shape()?;
        if shape.source != source
            || matches!(source, GraphCountSource::Edges | GraphCountSource::Label(_))
            || matching > examined
            || (source == GraphCountSource::Vertices && examined != matching)
        {
            return None;
        }
        Some(PreparedGraphCount {
            query: self,
            shape,
            examined,
            matching,
            metadata: false,
        })
    }

    /// Bind an exact cardinality read from a trusted generation's metadata.
    /// The host must prove the same cut and unrestricted visible scope, admit
    /// the metadata read, and charge its actual lookup work. This is a distinct
    /// physical input: it does not replay work for rows it never traverses.
    /// Output expressions, HAVING, ordering, pagination and errors still use
    /// the original aggregate finalizer.
    #[must_use]
    pub fn prepare_count_metadata(
        &self,
        source: GraphCountSource,
        matching: u64,
    ) -> Option<PreparedGraphCount<'_>> {
        let shape = self.count_shape()?;
        (shape.source == source).then_some(PreparedGraphCount {
            query: self,
            shape,
            examined: 0,
            matching,
            metadata: true,
        })
    }
}

impl PreparedGraphCount<'_> {
    /// Execute the bound physical input with the original aggregate finalizer.
    /// A scan summary replays the scan's evaluator transcript; exact metadata
    /// spends one cardinality update. The host must first admit its source and
    /// deduct that work/scratch from `policy`. No record allowance is reset,
    /// and LIMIT 0 still consumes source and evaluator work before output.
    pub fn execute_governed<E, C>(
        &self,
        snapshot_records: u64,
        policy: GqlQueryPolicy,
        checkpoint: impl crate::GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        self.execute_with_reads(
            snapshot_records,
            &mut crate::set_ops::no_element_reads,
            policy,
            checkpoint,
        )
    }

    /// Finalize an exact source count with the host's pinned element-property
    /// reader. Output expressions still execute after the original scan work,
    /// including for empty sources and LIMIT 0.
    pub fn execute_governed_with_element_reads<E, C>(
        &self,
        snapshot_records: u64,
        mut elements: impl crate::GraphElementReader<E>,
        policy: GqlQueryPolicy,
        checkpoint: impl crate::GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        self.execute_with_reads(
            snapshot_records,
            &mut crate::set_ops::projection::HostElements(&mut elements),
            policy,
            checkpoint,
        )
    }

    fn execute_with_reads<E, C>(
        &self,
        snapshot_records: u64,
        elements: &mut crate::set_ops::ElementRead<'_, E>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl crate::GovernedCheckpoint<C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        checkpoint.unit().map_err(GqlQueryError::Interrupted)?;
        policy
            .rows
            .check(GqlBudgetDimension::SnapshotRecords, snapshot_records)
            .map_err(GqlQueryError::Rows)?;
        let mut charges = Charges::new(checkpoint, policy, snapshot_records);
        charges.event(GlaExecutionEvent::ScratchEntry)?;
        let mut state = new_group(&self.query.aggregates, &mut |event| charges.event(event))?;

        // Identity projections spend one Work each and have no variable
        // payload. A global group spends one lookup and one update per
        // aggregate. Vertex scans do that for every vertex after their two
        // scan/projection events. The single-edge visitor first spends one
        // Work per examined edge, then visits its nonempty weighted total
        // once. Every event here is Work: no reader or data exception can
        // occur between them for a shape admitted above.
        let visit_work = self.shape.columns as u128 + 1 + self.query.aggregates.len() as u128;
        let work = if self.metadata {
            // One admitted weighted cardinality and one update per aggregate.
            1 + self.query.aggregates.len() as u128
        } else {
            match self.shape.source {
                GraphCountSource::Vertices => u128::from(self.examined) * (2 + visit_work),
                GraphCountSource::Edges | GraphCountSource::Label(_) => {
                    unreachable!("this input admits exact metadata, never scan summaries")
                }
                GraphCountSource::Relation(_) => {
                    u128::from(self.examined) + u128::from(self.matching > 0) * visit_work
                }
            }
        };
        let visited = charges.work(work).map(|()| {
            for cell in &mut state {
                let Accumulator::Count(count) = cell else {
                    unreachable!("the checked definition contains only COUNT(*)")
                };
                *count = self.matching;
            }
        });
        let finished = visited.and_then(|()| {
            let mut groups = BTreeMap::new();
            groups.insert(Vec::new(), state);
            let value =
                self.query
                    .finish_groups_with_elements(&groups, elements, &mut |event| {
                        charges.event(event)
                    })?;
            charges.event(GlaExecutionEvent::Work)?;
            Ok(value)
        });
        // Like execute_input, settle deferred host work before exposing any
        // evaluator/output error or result. A refusing host is not polled
        // again after settlement fails.
        charges.settle()?;
        Ok(GqlQueryExecution {
            value: finished?,
            rows: charges.rows,
            evaluator: charges.evaluator,
        })
    }
}

#[cfg(test)]
mod tests;
