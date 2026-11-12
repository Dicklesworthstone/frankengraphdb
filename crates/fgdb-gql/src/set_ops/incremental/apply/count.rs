//! Keyed correlated counts through source restriction and native grouping.
//! Counts are formed from RHS weights, never from an outer-multiplied join.

use super::*;
use crate::row_aggregate::definition::GroupDefinition;
use crate::{
    GraphAggregate, GraphAggregateFunction, GraphIntegerExpression, GraphIntegerOp,
    PreparedGraphSetAggregate,
};

/// A native COUNT recipe over complete original sources.
///
/// Compile the original outer and RHS once. Restrict RHS through restrict
/// (RHS Semi outer), feed those rows to group, then join the ORIGINAL outer
/// through join (Left). Apply output in order: zero-fill and restore names.
/// Five derived nodes use existing operators; no count interpreter is added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintainedCountJoin<'a> {
    pub left: &'a PreparedGraphSet,
    pub right: &'a PreparedGraphSet,
    pub restrict: RowJoinSpec,
    pub group: PreparedGraphSetAggregate,
    pub group_columns: Vec<String>,
    pub group_types: Vec<GraphSetColumnType>,
    pub join: RowJoinSpec,
    pub output: Vec<RowProjectionSpec>,
}

fn count_summary<'a, E>(
    query: &'a PreparedGraphSet,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<&'a PreparedGraphSetAggregate>, E> {
    control(GlaExecutionEvent::Work)?;
    if !query.unadorned_incremental_node() {
        return Ok(None);
    }
    match &query.node {
        SetNode::Scope(input) => count_summary(input, control),
        SetNode::Project {
            input,
            projection,
            quantifier: GraphSetQuantifier::All,
            ..
        } if projection.len() == 1 && matches!(projection[0].value(), GraphSetValue::Column(0)) => {
            count_summary(input, control)
        }
        SetNode::Aggregate(summary) => {
            if !summary.group_key_columns().is_empty()
                || !summary.having().is_empty()
                || summary.having_expression().is_some()
                || summary.has_incremental_output_transform()
                || summary.has_computed_ordering()
                || summary.incremental_result_window() != (0, None)
                || !summary.supports_incremental_maintenance_with_having()
                || query.types.as_slice() != [GraphSetColumnType::Scalar]
            {
                return Ok(None);
            }
            let mut specs = summary.aggregate_specs();
            control(GlaExecutionEvent::Work)?;
            if !matches!(
                specs.next(),
                Some((GraphAggregateFunction::CountRows, None))
                    | Some((
                        GraphAggregateFunction::Count | GraphAggregateFunction::CountDistinct,
                        Some(_),
                    ))
            ) || specs.next().is_some()
            {
                return Ok(None);
            }
            Ok(Some(summary))
        }
        _ => Ok(None),
    }
}

impl PreparedGraphSet {
    /// Prove a bounded correlated global count is a native group circuit.
    ///
    /// Only Inner Apply with one untransformed, keyless count is admitted.
    /// Transparent scopes and one-column count aliases may surround the group.
    /// Its input contains one Arguments-to-independent-graph inner join/product
    /// followed by total ALL projections. The native count-star input may have
    /// zero projected columns: ALL preserves its occurrence count. COUNT(value)
    /// and COUNT(DISTINCT value) additionally require a copied RHS column, proved
    /// through every projection. The native grouping owner retains nonnull and
    /// distinct support, including last-occurrence deletion. An outer argument
    /// cannot stand in for a RHS value shared by different outer rows. ON
    /// predicates, postfilters, other aggregates, relational DISTINCT, pages and
    /// computed effects refuse.
    ///
    /// The complete RHS is first semijoined against active outer keys. This
    /// preserves RHS multiplicity while excluding uninvoked key groups BEFORE
    /// native checked Int64 count conversion. An empty outer therefore cannot
    /// expose an overflow from a group that ordinary Apply would never execute.
    /// The same rule covers a keyless count via a Semi Cartesian product.
    ///
    /// Grouping uses distinct RHS key positions; every original typed key
    /// constraint remains in the final Left join. Duplicate outer keys never
    /// multiply counts, and NULL/unmatched keys receive scalar zero through the
    /// existing integer VM's COALESCE. No source, result or transcript is changed.
    /// Complete children still require host admission; internal schema/depth
    /// bounds may conservatively refuse an otherwise valid statement.
    pub fn incremental_apply_count_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<MaintainedCountJoin<'_>>, E> {
        control(GlaExecutionEvent::Work)?;
        if !self.unadorned_incremental_node() {
            return Ok(None);
        }
        let SetNode::Apply {
            left,
            right: body_query,
            imports,
            kind: ApplyKind::Inner,
        } = &self.node
        else {
            return Ok(None);
        };
        let Some(summary) = count_summary(body_query, control)? else {
            return Ok(None);
        };
        let mut stages = Vec::new();
        let Some(base) = body(
            summary.input(),
            imports.is_empty(),
            false,
            &mut stages,
            control,
        )?
        else {
            return Ok(None);
        };
        if base.spec.is_some_and(|spec| spec.predicate().is_some())
            || !independent(base.right, control)?
        {
            return Ok(None);
        }
        // Every original projection executes before aggregation, even when its
        // output is discarded. Prove all cells total before tracing the one
        // requested argument; a dead fallible cell must never be optimized away.
        for stage in &stages {
            control(GlaExecutionEvent::Work)?;
            let Stage::Project(_, projection) = stage else {
                return Ok(None);
            };
            for cell in *projection {
                control(GlaExecutionEvent::Work)?;
                if !matches!(
                    cell.value(),
                    GraphSetValue::Column(_)
                        | GraphSetValue::Literal(_)
                        | GraphSetValue::TypedNull(_)
                        | GraphSetValue::Value(_)
                ) {
                    return Ok(None);
                }
            }
        }
        let Some((function, mut argument)) = summary.aggregate_specs().next() else {
            return Ok(None);
        };
        if let Some(mut column) = argument {
            // Follow aliases backwards to the original Arguments + RHS frame.
            // ALL copies preserve occurrence multiplicity, even when multiple
            // complete input rows project to the same counted value. Native
            // COUNT DISTINCT, rather than this proof, owns value equivalence.
            for stage in stages.iter().rev() {
                control(GlaExecutionEvent::Work)?;
                let Stage::Project(_, projection) = stage else {
                    return Ok(None);
                };
                let Some(cell) = projection.get(column) else {
                    return Ok(None);
                };
                let GraphSetValue::Column(input) = cell.value() else {
                    return Ok(None);
                };
                column = *input;
            }
            let width = base.arguments.map_or(0, |input| input.types.len());
            let Some(right_column) = column.checked_sub(width) else {
                return Ok(None);
            };
            if right_column >= base.right.types.len() {
                return Ok(None);
            }
            argument = Some(right_column);
        }
        let arguments = match base.arguments {
            Some(arguments) => {
                let Some(slots) = argument_slots(arguments, imports, control)? else {
                    return Ok(None);
                };
                slots
            }
            None => Vec::new(),
        };
        let mut reversed = Vec::new();
        let mut group_keys = Vec::new();
        let mut joined = Vec::new();
        if let Some(spec) = base.spec {
            for &(argument, right) in spec.keys() {
                charge(3 + group_keys.len(), control)?;
                let outer = arguments[argument];
                reversed.push((right, outer));
                let key = match group_keys.iter().position(|column| *column == right) {
                    Some(at) => at,
                    None => {
                        let at = group_keys.len();
                        group_keys.push(right);
                        at
                    }
                };
                joined.push((outer, key));
            }
        }
        let restrict = if reversed.is_empty() {
            RowJoinSpec::cross(&base.right.types, &left.types)
        } else {
            RowJoinSpec::new(&base.right.types, &left.types, &reversed)
        };
        let Ok(restrict) = restrict else {
            return Ok(None);
        };
        let restrict = restrict.with_kind(RowJoinKind::Semi);
        charge(base.right.types.len() + left.types.len(), control)?;
        // The grouping owner retains its actual complete logical input. The
        // host supplies this same shared-source Semi node, not an unrelated
        // same-schema relation or an executable first-leaf substitute.
        let Ok(active) = base.right.clone().join((**left).clone(), restrict.clone()) else {
            return Ok(None);
        };
        control(GlaExecutionEvent::Work)?;
        let mut count_name = None;
        for suffix in 0..=base.right.columns.len() {
            charge(1 + base.right.columns.len(), control)?;
            let candidate = format!("__fg_count_{suffix}");
            if !base.right.columns.contains(&candidate) {
                count_name = Some(candidate);
                break;
            }
        }
        let Some(count_name) = count_name else {
            return Ok(None);
        };
        let count = match (function, argument) {
            (GraphAggregateFunction::CountRows, None) => GraphAggregate::count_rows(&count_name),
            (GraphAggregateFunction::Count, Some(column)) => {
                GraphAggregate::count(&count_name, column)
            }
            (GraphAggregateFunction::CountDistinct, Some(column)) => {
                GraphAggregate::count_distinct(&count_name, column)
            }
            _ => return Ok(None),
        };
        let Ok(grouped) = active.group_by(&group_keys, &[count]) else {
            return Ok(None);
        };
        let PreparedGraphSet {
            node: SetNode::Aggregate(group),
            columns: group_columns,
            types: group_types,
            ..
        } = grouped
        else {
            return Ok(None);
        };
        charge(group_types.len() + left.types.len(), control)?;
        let join = if joined.is_empty() {
            RowJoinSpec::cross(&left.types, &group_types)
        } else {
            RowJoinSpec::new(&left.types, &group_types, &joined)
        };
        let Ok(join) = join else {
            return Ok(None);
        };
        let join = join.with_kind(RowJoinKind::Left);
        let types: Vec<_> = join.column_types().collect();
        let Ok(zero_filled) = GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(left.types.len() + group_keys.len()),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Coalesce,
        ]) else {
            return Ok(None);
        };
        charge(left.types.len() + 4, control)?;
        let mut values: Vec<_> = (0..left.types.len()).map(GraphSetValue::Column).collect();
        values.push(GraphSetValue::Integer(zero_filled));
        let Some(projected) = project(&types, values, control)? else {
            return Ok(None);
        };
        if projected.column_types() != self.types.as_slice() {
            return Ok(None);
        }
        charge(self.types.len(), control)?;
        let Ok(named) = RowProjectionSpec::selection(
            self.types.clone(),
            self.columns.clone(),
            &[GraphSetPredicateOp::Truth(Some(true))],
        ) else {
            return Ok(None);
        };
        control(GlaExecutionEvent::Work)?;
        Ok(Some(MaintainedCountJoin {
            left,
            right: base.right,
            restrict,
            group: *group,
            group_columns,
            group_types,
            join,
            output: vec![projected, named],
        }))
    }
}

#[cfg(test)]
mod tests;
