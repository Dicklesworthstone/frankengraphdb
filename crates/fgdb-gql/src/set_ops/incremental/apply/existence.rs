//! Maintain the compiler's existential Apply through existing presence joins.
//! The literal witness page selects no identity: every retained body occurrence
//! becomes TRUE, so LIMIT one is exactly a presence test, including duplicates.

use super::*;

/// One existential continuation over two complete maintained children.
///
/// Compile each child once. Feed both indexes to the Semi join and the same
/// matching definition with kind Anti, project present/absent respectively,
/// UNION ALL the disjoint outer bags, then apply output to restore public names.
/// These six ordinary operators retain outer multiplicity without grouping,
/// deduplicating, dividing counts or replaying a correlated body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintainedExistenceJoin<'a> {
    pub left: &'a PreparedGraphSet,
    pub right: &'a PreparedGraphSet,
    pub join: RowJoinSpec,
    pub present: RowProjectionSpec,
    pub absent: RowProjectionSpec,
    pub output: RowProjectionSpec,
}

impl PreparedGraphSet {
    /// Prove a bounded TRUE-or-NULL existential Apply is a presence circuit.
    ///
    /// Only an Optional Apply ending in one ALL literal TRUE column with
    /// LIMIT one, zero offset and no order is recognized. The complete input
    /// beneath that literal projection must contain one Arguments-to-graph
    /// inner join/product, scopes, total ALL projections and ordinary filters.
    /// Projections before a later filter may only copy/reorder columns; their
    /// exact positions are rebound into ON. Later column/literal projections
    /// preserve nonemptiness and have no conditional expression failures.
    ///
    /// Both sources still pass ordinary host admission. The independent graph
    /// source uses the same effect proof as the row-returning Apply recipe.
    /// Computed/dead exports, DISTINCT, child ordering/pages, aggregation and
    /// nested invocation refuse even for empty outer input or an outer LIMIT 0.
    /// This is not a generic pagination rewrite or a short-circuit executor.
    ///
    /// Semi and Anti partition the ORIGINAL outer bag. A real all-NULL body
    /// row is a witness, and right multiplicity never scales the returned outer
    /// count. First/last witness changes use the existing checked derivatives.
    pub fn incremental_apply_exists_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<MaintainedExistenceJoin<'_>>, E> {
        control(GlaExecutionEvent::Work)?;
        let Some(input) = existence_input(self) else {
            return Ok(None);
        };
        let SetNode::Apply { left, imports, .. } = &self.node else {
            return Ok(None);
        };
        let mut stages = Vec::new();
        let Some(base) = body(input, imports.is_empty(), false, &mut stages, control)? else {
            return Ok(None);
        };
        if !independent(base.right, control)? {
            return Ok(None);
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
        charge(arguments.len() + base.right.types.len(), control)?;
        let mut slots = arguments.clone();
        slots.extend((0..base.right.types.len()).map(|column| left.types.len() + column));
        let mut on = match base.spec.and_then(RowJoinSpec::predicate) {
            Some(code) => rebound(code, &slots, control)?,
            None => Vec::new(),
        };
        let last_filter = stages
            .iter()
            .rposition(|stage| matches!(stage, Stage::Filter(..)));
        for (index, stage) in stages.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            match stage {
                Stage::Project(_, projection) => {
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
                    if last_filter.is_some_and(|last| index < last) {
                        let mut next = Vec::new();
                        for cell in *projection {
                            charge(1, control)?;
                            let GraphSetValue::Column(column) = cell.value() else {
                                return Ok(None);
                            };
                            next.push(slots[*column]);
                        }
                        slots = next;
                    }
                }
                Stage::Filter(code) => {
                    let next = rebound(code, &slots, control)?;
                    let conjunction = !on.is_empty();
                    charge(next.len() + usize::from(conjunction), control)?;
                    on.extend(next);
                    if conjunction {
                        on.push(GraphSetPredicateOp::And);
                    }
                }
            }
        }
        let mut keys = Vec::new();
        if let Some(spec) = base.spec {
            for &(a, b) in spec.keys() {
                charge(1, control)?;
                keys.push((arguments[a], b));
            }
        }
        let join = if keys.is_empty() {
            RowJoinSpec::cross(&left.types, &base.right.types)
        } else {
            RowJoinSpec::new(&left.types, &base.right.types, &keys)
        };
        let Ok(mut join) = join else {
            return Ok(None);
        };
        if !on.is_empty() {
            let Ok(selected) = join.with_predicate(&on) else {
                return Ok(None);
            };
            join = selected;
        }
        join = join.with_kind(RowJoinKind::Semi);
        charge(left.types.len() + 1, control)?;
        let mut values: Vec<_> = (0..left.types.len()).map(GraphSetValue::Column).collect();
        values.push(GraphSetValue::Value(GraphValue::Scalar(
            CanonicalScalar::Bool(true),
        )));
        let Some(present) = project(&left.types, values, control)? else {
            return Ok(None);
        };
        charge(left.types.len() + 1, control)?;
        let mut values: Vec<_> = (0..left.types.len()).map(GraphSetValue::Column).collect();
        values.push(GraphSetValue::TypedNull(GraphSetColumnType::Scalar));
        let Some(absent) = project(&left.types, values, control)? else {
            return Ok(None);
        };
        if present.column_types() != self.types.as_slice()
            || absent.column_types() != self.types.as_slice()
        {
            return Ok(None);
        }
        charge(self.types.len(), control)?;
        let Ok(output) = RowProjectionSpec::selection(
            self.types.clone(),
            self.columns.clone(),
            &[GraphSetPredicateOp::Truth(Some(true))],
        ) else {
            return Ok(None);
        };
        control(GlaExecutionEvent::Work)?;
        Ok(Some(MaintainedExistenceJoin {
            left,
            right: base.right,
            join,
            present,
            absent,
            output,
        }))
    }
}

#[cfg(test)]
mod tests;
