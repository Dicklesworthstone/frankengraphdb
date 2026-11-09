//! Per-invocation DISTINCT without losing duplicate outer occurrences.
//! The native projection owner maintains each argument tuple's support. All
//! body barriers retain a private exact argument key, and a final join restores
//! the original outer bag. No ordinal or synthetic element identity is stored.

use super::*;
use crate::{GraphIntegerExpression, GraphIntegerOp};
use fgdb_types::{EId, VId};

/// A DISTINCT-bearing correlated body expressed entirely by native operators.
///
/// `outer` appends private keys to the ORIGINAL left bag. `arguments` projects
/// that bag to distinct imported tuples. Join those tuples with the complete
/// right source and apply `body` in order. Finally `restore` joins the original
/// keyed outer bag to those results, then `output` restores the public schema.
/// Every source is shared, including across simultaneous left/right changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintainedDistinctApply<'a> {
    pub left: &'a PreparedGraphSet,
    pub right: &'a PreparedGraphSet,
    pub outer: RowProjectionSpec,
    pub arguments: RowProjectionSpec,
    pub join: RowJoinSpec,
    pub body: Vec<RowProjectionSpec>,
    pub restore: RowJoinSpec,
    pub output: Vec<RowProjectionSpec>,
}

fn projection<E>(
    input: &[GraphSetColumnType],
    values: Vec<GraphSetValue>,
    quantifier: GraphSetQuantifier,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<RowProjectionSpec>, E> {
    charge(input.len() + values.len(), control)?;
    Ok(RowProjectionSpec::new(
        input.to_vec(),
        values
            .into_iter()
            .enumerate()
            .map(|(column, value)| GraphSetProjection::new(format!("__fg_apply_{column}"), value))
            .collect(),
        quantifier,
    )
    .ok())
}

/// Ordinary equality joins deliberately do not match NULL. Invocation identity
/// does: two NULL imports name the same invocation. Encode each admitted import
/// as (is_null, nonnull_value), using a same-domain filler only when NULL. The
/// flag distinguishes a real filler value, including vertex/edge zero. These
/// private values never enter a source read or escape into the public result.
fn keyed_outer<E>(
    input: &[GraphSetColumnType],
    columns: &[usize],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<RowProjectionSpec>, E> {
    charge(input.len(), control)?;
    let mut values: Vec<_> = (0..input.len()).map(GraphSetValue::Column).collect();
    let mut defaults = Vec::new();
    for &column in columns {
        charge(4, control)?;
        let filler = match input[column] {
            GraphSetColumnType::Scalar | GraphSetColumnType::Stored => {
                GraphValue::Scalar(CanonicalScalar::Bool(false))
            }
            GraphSetColumnType::Vertex => GraphValue::Vertex(VId(0)),
            GraphSetColumnType::Edge => GraphValue::Edge(EId(0)),
            _ => return Ok(None),
        };
        let Ok(presence) = GraphIntegerExpression::prepare_scalar(&[
            GraphIntegerOp::PresenceColumn(column),
            GraphIntegerOp::IsNull(true),
        ]) else {
            return Ok(None);
        };
        let is_null = GraphSetValue::Integer(presence);
        values.push(is_null);
        defaults.push((values.len(), filler));
        values.push(GraphSetValue::Column(column));
    }
    // The no-import body still activates only when the outer bag is nonempty.
    // A fixed key lets the ordinary indexed join restore each outer occurrence.
    if columns.is_empty() {
        charge(1, control)?;
        values.push(GraphSetValue::Value(GraphValue::Scalar(
            CanonicalScalar::Bool(true),
        )));
    }
    let Some(mut projected) = project(input, values, control)? else {
        return Ok(None);
    };
    for (column, fallback) in defaults {
        control(GlaExecutionEvent::Work)?;
        let Ok(normalized) = projected.with_null_default(column, fallback) else {
            return Ok(None);
        };
        projected = normalized;
    }
    Ok(Some(projected))
}

impl PreparedGraphSet {
    /// Prove an unpaged Inner/Optional Apply with per-invocation DISTINCT.
    ///
    /// The complete independent source obeys the same conditional-effect proof
    /// as the row/count recipes. The argument side can reorder/repeat imports;
    /// its scalar/stored/vertex/edge values are keyed by exact canonical value,
    /// including NULL. Native row-local projections and filters retain their
    /// execution order. Each DISTINCT includes the invocation key, so unrelated
    /// invocations cannot collapse one another. Later ALL projections retain
    /// any multiplicity they introduce. Duplicate outer occurrences rejoin only
    /// AFTER all body barriers, so they never become one occurrence themselves.
    ///
    /// Optional null extension belongs to the final restoration join, after
    /// every body filter/projection. An empty body emits one entirely NULL
    /// export per outer occurrence, without evaluating an expression on a
    /// fabricated row. A genuine all-NULL export is still a matching row.
    ///
    /// Pages/order, grouping, nested invocation, reader/ordinal effects and
    /// unsupported argument-key domains refuse. This proves canonical bags;
    /// positional windows retain their existing separate admission. The recipe
    /// is derived without changing the original query or its identity.
    pub fn incremental_apply_distinct_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<MaintainedDistinctApply<'_>>, E> {
        control(GlaExecutionEvent::Work)?;
        if !self.unadorned_incremental_node() {
            return Ok(None);
        }
        let SetNode::Apply {
            left,
            right,
            imports,
            kind,
        } = &self.node
        else {
            return Ok(None);
        };
        if matches!(kind, ApplyKind::Collect { .. }) {
            return Ok(None);
        }
        let mut stages = Vec::new();
        let Some(base) = body(right, imports.is_empty(), true, &mut stages, control)? else {
            return Ok(None);
        };
        let mut has_distinct = false;
        for stage in &stages {
            control(GlaExecutionEvent::Work)?;
            if let Stage::Project(query, projection) = stage {
                has_distinct |= matches!(
                    &query.node,
                    SetNode::Project {
                        quantifier: GraphSetQuantifier::Distinct,
                        ..
                    }
                );
                for cell in *projection {
                    control(GlaExecutionEvent::Work)?;
                    if !cell.value().is_scan_local() {
                        return Ok(None);
                    }
                }
            }
        }
        if !has_distinct || !independent(base.right, control)? {
            return Ok(None);
        }
        let arguments = match base.arguments {
            Some(input) => {
                let Some(slots) = argument_slots(input, imports, control)? else {
                    return Ok(None);
                };
                slots
            }
            None => Vec::new(),
        };
        let Some(outer) = keyed_outer(&left.types, &arguments, control)? else {
            return Ok(None);
        };
        let left_width = left.types.len();
        let argument_width = arguments.len();
        let key_width = outer.column_types().len() - left_width;
        let right_width = base.right.types.len();
        charge(argument_width + key_width, control)?;
        let mut values: Vec<_> = arguments.into_iter().map(GraphSetValue::Column).collect();
        values.extend((left_width..left_width + key_width).map(GraphSetValue::Column));
        let Some(arguments) = projection(
            outer.column_types(),
            values,
            GraphSetQuantifier::Distinct,
            control,
        )?
        else {
            return Ok(None);
        };
        let joined = if let Some(spec) = base.spec
            && !spec.keys().is_empty()
        {
            RowJoinSpec::new(arguments.column_types(), &base.right.types, spec.keys())
        } else {
            RowJoinSpec::cross(arguments.column_types(), &base.right.types)
        };
        let Ok(mut join) = joined else {
            return Ok(None);
        };
        if let Some(code) = base.spec.and_then(RowJoinSpec::predicate) {
            charge(argument_width + right_width, control)?;
            let slots: Vec<_> = (0..argument_width)
                .chain((0..right_width).map(|column| argument_width + key_width + column))
                .collect();
            let code = rebound(code, &slots, control)?;
            let Ok(selected) = join.with_predicate(&code) else {
                return Ok(None);
            };
            join = selected;
        }
        // Restore the original body frame before running its unchanged VM
        // expressions. The invocation key is carried as a private suffix.
        charge(argument_width + right_width + key_width, control)?;
        let values = (0..argument_width)
            .chain(argument_width + key_width..argument_width + key_width + right_width)
            .chain(argument_width..argument_width + key_width)
            .map(GraphSetValue::Column)
            .collect();
        let input_types: Vec<_> = join.column_types().collect();
        let Some(frame) = project(&input_types, values, control)? else {
            return Ok(None);
        };
        let mut types = frame.column_types().to_vec();
        let mut width = argument_width + right_width;
        let mut body = vec![frame];
        for stage in stages {
            control(GlaExecutionEvent::Work)?;
            let step = match stage {
                Stage::Project(query, cells) => {
                    let SetNode::Project { quantifier, .. } = &query.node else {
                        return Ok(None);
                    };
                    charge(cells.len() + key_width, control)?;
                    let mut values: Vec<_> =
                        cells.iter().map(|cell| cell.value().clone()).collect();
                    values.extend((width..width + key_width).map(GraphSetValue::Column));
                    let Some(step) = projection(&types, values, *quantifier, control)? else {
                        return Ok(None);
                    };
                    if step.column_types().get(..cells.len()) != Some(query.types.as_slice()) {
                        return Ok(None);
                    }
                    width = cells.len();
                    step
                }
                Stage::Filter(code) => {
                    charge(types.len() + code.len(), control)?;
                    let Ok(step) =
                        RowProjectionSpec::selection(types.clone(), names(types.len()), code)
                    else {
                        return Ok(None);
                    };
                    step
                }
            };
            types = step.column_types().to_vec();
            body.push(step);
        }
        charge(key_width, control)?;
        let keys: Vec<_> = (0..key_width)
            .map(|key| (left_width + key, width + key))
            .collect();
        let Ok(restore) = RowJoinSpec::new(outer.column_types(), &types, &keys) else {
            return Ok(None);
        };
        let restore = restore.with_kind(if *kind == ApplyKind::Optional {
            RowJoinKind::Left
        } else {
            RowJoinKind::Inner
        });
        let input_types: Vec<_> = restore.column_types().collect();
        charge(left_width + width, control)?;
        let values = (0..left_width)
            .chain(left_width + key_width..left_width + key_width + width)
            .map(GraphSetValue::Column)
            .collect();
        let Some(public) = project(&input_types, values, control)? else {
            return Ok(None);
        };
        if public.column_types() != self.types.as_slice() {
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
        Ok(Some(MaintainedDistinctApply {
            left,
            right: base.right,
            outer,
            arguments,
            join,
            body,
            restore,
            output: vec![public, named],
        }))
    }
}

#[cfg(test)]
mod tests;
