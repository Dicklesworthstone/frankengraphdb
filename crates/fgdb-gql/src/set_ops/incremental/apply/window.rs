//! One terminal bounded page per activated correlated invocation.
//!
//! The existing keyed Apply recipe owns source restriction, pure row stages,
//! DISTINCT and optional restoration. Its private argument key partitions the
//! exact native row-window operator immediately before the restore join.

use super::*;
use crate::algebra::{BindingSlot, IntegerComparison, VertexPredicate};
use crate::row_window::RowWindowSpec;
use fgdb_delta_types::PropertyKeyId;

// This proof is deliberately local to the finite-window recipe. The existing
// unpaged Apply recipes keep their independent-source contract unchanged.
// A flat preceding integer comparison proves the stored value is an Int or
// Float. Equality and ordered comparisons reject every collection and foreign
// domain; NOT EQUAL does not, and therefore is never a scalar witness. The
// seeded-pattern proof excludes Boolean/optional/probe boundaries, so every
// returned tuple has passed this same-slot, same-property guard.
fn scalar_guard<E>(
    preceding: &[GlaOperator],
    wanted_slot: BindingSlot,
    wanted_key: PropertyKeyId,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    for operator in preceding {
        control(GlaExecutionEvent::Work)?;
        if let GlaOperator::Select { slot, predicates } = operator {
            if *slot != wanted_slot {
                continue;
            }
            for predicate in predicates {
                control(GlaExecutionEvent::Work)?;
                if matches!(predicate, VertexPredicate::IntegerProperty { key, comparison, .. }
                    if *key == wanted_key && *comparison != IntegerComparison::NotEqual)
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

pub(super) fn independent_window<E>(
    query: &PreparedGraphSet,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<bool, E> {
    control(GlaExecutionEvent::Work)?;
    if !query.unadorned_incremental_node() {
        return Ok(false);
    }
    match &query.node {
        SetNode::Pattern(pattern) => {
            if pattern.seeded_columns().is_none() {
                return Ok(false);
            }
            let operators = pattern.plan().operators();
            for (at, operator) in operators.iter().enumerate() {
                let GlaOperator::ProjectValues { columns } = operator else {
                    continue;
                };
                // Hidden cells are evaluated too; each one needs this proof.
                for column in columns {
                    control(GlaExecutionEvent::Work)?;
                    let total = match column {
                        ValueProjection::Vertex { .. }
                        | ValueProjection::Path {
                            function:
                                GraphPathFunction::Value
                                | GraphPathFunction::Length
                                | GraphPathFunction::Nodes
                                | GraphPathFunction::Edges
                                | GraphPathFunction::Edge,
                            ..
                        } => true,
                        ValueProjection::Property { slot, key } => {
                            scalar_guard(&operators[..at], *slot, *key, control)?
                        }
                        _ => false,
                    };
                    if !total {
                        return Ok(false);
                    }
                }
            }
            Ok(true)
        }
        SetNode::Scope(input) | SetNode::Filter { input, .. } => independent_window(input, control),
        SetNode::Project {
            input,
            projection,
            quantifier: GraphSetQuantifier::All,
            ..
        } => {
            for cell in projection {
                control(GlaExecutionEvent::Work)?;
                if !matches!(
                    cell.value(),
                    GraphSetValue::Column(_)
                        | GraphSetValue::Literal(_)
                        | GraphSetValue::TypedNull(_)
                        | GraphSetValue::Value(_)
                ) {
                    return Ok(false);
                }
            }
            independent_window(input, control)
        }
        _ => Ok(false),
    }
}

/// Execute `invocation` through its complete body, then apply `window`, then
/// its restoration join and public output. Duplicate outer occurrences never
/// spend the body's page: they are restored only after independent selection.
/// The host retains one source/circuit owner and publishes every stage together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintainedWindowApply<'a> {
    pub invocation: MaintainedDistinctApply<'a>,
    pub window: RowWindowSpec,
}

fn terminal<'a, E>(
    query: &'a PreparedGraphSet,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<&'a PreparedGraphSet>, E> {
    control(GlaExecutionEvent::Work)?;
    if query.unadorned_incremental_node() {
        return match &query.node {
            SetNode::Scope(input) => terminal(input, control),
            _ => Ok(None),
        };
    }
    // Unknown positional order never becomes a comparator merely because the
    // requested count is zero. Explicit or proved inherited order is required.
    Ok((query.count.is_some() && query.incremental_result_order().is_some()).then_some(query))
}

impl PreparedGraphSet {
    /// Maintain one final finite ORDER BY/SKIP/LIMIT inside Inner/Optional CALL.
    /// Exact argument tuples activate the body once; NULL imports use tagged
    /// keys, and no-import calls activate a unit key only while their outer bag
    /// is nonempty. Pure projections/filters and local DISTINCT retain their
    /// positions before the page. Deleted winners refill from the complete
    /// retained body, independently for each invocation. Optional empty pages
    /// null-extend only after selection; genuine NULL exports and original
    /// outer duplicates remain real occurrences.
    ///
    /// Only a terminal window, optionally wrapped in transparent scopes, is
    /// admitted. Intermediate/nested pages, unbounded ordering, aggregation,
    /// nested calls, reader/ordinal effects and unsupported argument domains
    /// keep their refusal. Every source still needs ordinary authorization
    /// and admission, including for an empty or LIMIT-zero body.
    /// Stored vertex fields are admitted only when a preceding flat integer
    /// comparison on that same binding and property proves a scalar value.
    /// NOT EQUAL, Boolean branches and unguarded stored fields retain refusal.
    pub fn incremental_apply_window_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<MaintainedWindowApply<'_>>, E> {
        control(GlaExecutionEvent::Work)?;
        if !self.unadorned_incremental_node() {
            return Ok(None);
        }
        let SetNode::Apply { right, kind, .. } = &self.node else {
            return Ok(None);
        };
        if matches!(kind, ApplyKind::Collect { .. }) {
            return Ok(None);
        }
        let Some(terminal) = terminal(right, control)? else {
            return Ok(None);
        };
        let Some(invocation) = self.keyed_apply_with_control(Some(terminal), control)? else {
            return Ok(None);
        };
        let Some(last) = invocation.body.last() else {
            return Ok(None);
        };
        let width = right.types.len();
        let keys = invocation.outer.column_types().len() - invocation.left.types.len();
        if last.column_types().len() != width + keys || keys == 0 {
            return Ok(None);
        }
        let Some((order, _)) = terminal.incremental_result_order() else {
            return Ok(None);
        };
        charge(last.column_types().len() + order.len() + keys, control)?;
        let Ok(window) = RowWindowSpec::new(
            last.column_types().to_vec(),
            order.to_vec(),
            GraphSetQuantifier::All,
            terminal.offset,
            terminal
                .count
                .expect("terminal admission requires a finite page"),
        )
        .and_then(|window| window.with_partition_columns((width..width + keys).collect())) else {
            return Ok(None);
        };
        control(GlaExecutionEvent::Work)?;
        Ok(Some(MaintainedWindowApply { invocation, window }))
    }
}

#[cfg(test)]
mod tests;
