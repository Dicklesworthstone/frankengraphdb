//! Native numeric definitions share the complete-row maintained group owner.
//! Normalization reads no source and executes no expressions. Shape admission
//! and resource accounting remain with the returned circuit's ordinary owner.

use super::*;

impl PreparedGraphAggregate {
    /// Normalize supported numeric or collection aggregates for a maintained
    /// relational group circuit. Unlike a runtime fallback, this choice depends
    /// only on the immutable definition, before reading any graph records.
    ///
    /// Numeric sums/averages are order-independent. Keep the complete original
    /// graph source and every computed input column, then use the same exact
    /// integer/binary64 group kernel as explicit WITH/UNWIND pipelines. No
    /// expression, scope, grouping key, hidden aggregate, HAVING or result
    /// modifier may disappear. The host must admit the returned source and
    /// count every private node before registering it.
    ///
    /// COLLECT always uses its separate occurrence-order proof, even alongside
    /// numeric aggregates. Count/extremum-only definitions keep their existing
    /// direct engine. An unknown source profile or excessive complete width
    /// supplies no normalization; this is not permission to omit operators.
    #[must_use]
    pub fn incremental_group_relation(&self) -> Option<PreparedGraphSetAggregate> {
        if self.relational_input.is_some() {
            return None;
        }
        if self.aggregates.iter().any(|aggregate| {
            matches!(
                aggregate.function,
                GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct
            )
        }) {
            return self.incremental_collection_relation();
        }
        if !self.aggregates.iter().any(|aggregate| {
            matches!(
                aggregate.function,
                GraphAggregateFunction::SumInt
                    | GraphAggregateFunction::SumIntDistinct
                    | GraphAggregateFunction::AverageInt
                    | GraphAggregateFunction::AverageIntDistinct
            )
        }) {
            return None;
        }
        let source = self.input.0.as_ref()?;
        let [
            ..,
            GlaOperator::ProjectValues { columns },
            GlaOperator::OrderByValues,
            GlaOperator::Limit {
                offset: 0,
                count: None,
            },
        ] = source.plan().operators()
        else {
            return None;
        };
        // The row producer requires a private COUNT cell. Hidden source cells
        // are not recoverable from the declared schema, even for numeric input.
        if columns.is_empty()
            || columns.len() != source.columns().len()
            || columns.len() >= MAX_PATTERN_VERTICES
            || !source.preserves_duplicates()
        {
            return None;
        }
        let mut relation = PreparedGraphSet::from(source.clone());
        if let Some(projection) = &self.computed_input {
            relation = relation
                .project(projection.clone(), GraphSetQuantifier::All)
                .ok()?;
        }
        relation.check_parent_depth().ok()?;
        let mut summary = self.clone();
        summary.computed_input = None;
        summary.relational_input = Some(relation);
        // Input addresses do not move: the optional projection keeps its exact
        // schema and declaration order. Output addresses remain untouched too.
        PreparedGraphSetAggregate::from_relation(summary)
    }
}
