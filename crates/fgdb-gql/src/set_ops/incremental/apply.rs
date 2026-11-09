//! Decorrelate a proved row-local Apply into existing maintained operators.
//! No query is executed or rewritten. The original definition and both complete
//! source children remain the owners of admission, identity and source errors.

use super::*;
use crate::GlaExecutionEvent;
use crate::algebra::{GlaOperator, GraphPathFunction, GraphValue, ValueProjection};
use crate::row_join::{RowJoinKind, RowJoinSpec};
use crate::row_projection::RowProjectionSpec;
use fgdb_types::CanonicalScalar;

mod count;
mod existence;

/// A bag-only maintenance recipe for one correlated graph continuation.
///
/// Compile both complete children independently, optionally project the right
/// child, join, then apply every output projection in order. Publish all stages
/// atomically under the host's existing per-node policy. No stage owns a new source,
/// argument interpreter, execution clock, authorization decision or transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintainedApplyJoin<'a> {
    pub left: &'a PreparedGraphSet,
    pub right: &'a PreparedGraphSet,
    pub right_projection: Option<RowProjectionSpec>,
    pub join: RowJoinSpec,
    pub output: Vec<RowProjectionSpec>,
}

#[derive(Clone, Copy)]
enum Stage<'a> {
    Project(&'a PreparedGraphSet, &'a [GraphSetProjection]),
    Filter(&'a [GraphSetPredicateOp]),
}

struct Base<'a> {
    arguments: Option<&'a PreparedGraphSet>,
    right: &'a PreparedGraphSet,
    spec: Option<&'a RowJoinSpec>,
}

fn charge<E>(
    width: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    for _ in 0..width {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
    }
    Ok(())
}

fn names(width: usize) -> Vec<String> {
    (0..width).map(|column| format!("__fg_apply_{column}")).collect()
}

fn project<E>(
    input: &[GraphSetColumnType],
    values: Vec<GraphSetValue>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<RowProjectionSpec>, E> {
    charge(input.len() + values.len(), control)?;
    Ok(RowProjectionSpec::new(
        input.to_vec(),
        values
            .into_iter()
            .enumerate()
            .map(|(column, value)| {
                GraphSetProjection::new(format!("__fg_apply_{column}"), value)
            })
            .collect(),
        GraphSetQuantifier::All,
    )
    .ok())
}

fn body<'a, E>(
    query: &'a PreparedGraphSet,
    no_imports: bool,
    stages: &mut Vec<Stage<'a>>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<Base<'a>>, E> {
    control(GlaExecutionEvent::Work)?;
    if !query.unadorned_incremental_node() {
        return Ok(None);
    }
    match &query.node {
        SetNode::Scope(input) => body(input, no_imports, stages, control),
        SetNode::Project { input, projection, quantifier: GraphSetQuantifier::All, .. } => {
            let Some(base) = body(input, no_imports, stages, control)? else {
                return Ok(None);
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            stages.push(Stage::Project(query, projection));
            Ok(Some(base))
        }
        SetNode::Filter { input, .. } => {
            let Some((_, code)) = query.incremental_filter() else {
                return Ok(None);
            };
            let Some(base) = body(input, no_imports, stages, control)? else {
                return Ok(None);
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            stages.push(Stage::Filter(code));
            Ok(Some(base))
        }
        SetNode::Join { left, right, spec } if spec.kind() == RowJoinKind::Inner => {
            Ok(Some(Base { arguments: Some(left), right, spec: Some(spec) }))
        }
        SetNode::CrossJoin { left, right } => {
            Ok(Some(Base { arguments: Some(left), right, spec: None }))
        }
        SetNode::Pattern(_) if no_imports => {
            Ok(Some(Base { arguments: None, right: query, spec: None }))
        }
        _ => Ok(None),
    }
}

/// The compiler's literal-witness page has no observable selected identity.
/// This shape-only matcher removes exactly that page from sequence analysis;
/// it does not admit the descendant or promise that the body is effect-free.
pub(super) fn existence_input(query: &PreparedGraphSet) -> Option<&PreparedGraphSet> {
    if !query.unadorned_incremental_node() {
        return None;
    }
    let SetNode::Apply { right, kind: ApplyKind::Optional, .. } = &query.node else {
        return None;
    };
    if !right.order.is_empty() || right.offset != 0 || right.count != Some(1) {
        return None;
    }
    let SetNode::Project {
        input,
        projection,
        quantifier: GraphSetQuantifier::All,
        ..
    } = &right.node else {
        return None;
    };
    if projection.len() != 1 {
        return None;
    }
    let literal_true = match projection[0].value() {
        GraphSetValue::Literal(value) => matches!(value.value(), CanonicalScalar::Bool(true)),
        GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))) => true,
        _ => false,
    };
    literal_true.then_some(input.as_ref())
}

/// Return original outer positions, not a deduplicated relation of imports.
fn argument_slots<E>(
    query: &PreparedGraphSet,
    imports: &[usize],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<Vec<usize>>, E> {
    control(GlaExecutionEvent::Work)?;
    if !query.unadorned_incremental_node() {
        return Ok(None);
    }
    match &query.node {
        SetNode::Arguments => {
            charge(imports.len(), control)?;
            Ok(Some(imports.to_vec()))
        }
        SetNode::Scope(input) => argument_slots(input, imports, control),
        SetNode::Project { input, projection, quantifier: GraphSetQuantifier::All, .. } => {
            let Some(slots) = argument_slots(input, imports, control)? else {
                return Ok(None);
            };
            let mut output = Vec::new();
            for cell in projection {
                charge(1, control)?;
                let GraphSetValue::Column(column) = cell.value() else {
                    return Ok(None);
                };
                output.push(slots[*column]);
            }
            Ok(Some(output))
        }
        _ => Ok(None),
    }
}

/// A conditional body becomes an eagerly maintained source. Reuse the seeded
/// pattern proof to exclude fallible Boolean programs, probes and collections.
/// Also exclude catalog reads and stored-property output: a valid stored
/// collection can exceed query-value bounds and raise StoredValueBounds even
/// when the original Apply would never invoke its body for an empty outer bag.
/// A total relational wrapper cannot introduce either kind of failure.
fn independent<E>(
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
            // Check hidden projection cells too: visible-column metadata must
            // never hide a conditionally failing property or catalog read.
            for column in pattern.plan().operators().iter().flat_map(|operator| {
                match operator {
                    GlaOperator::ProjectValues { columns } => columns.as_slice(),
                    _ => &[],
                }
            }) {
                control(GlaExecutionEvent::Work)?;
                if !matches!(
                    column,
                    ValueProjection::Vertex { .. }
                        | ValueProjection::Path {
                            function: GraphPathFunction::Value
                                | GraphPathFunction::Length
                                | GraphPathFunction::Nodes
                                | GraphPathFunction::Edges
                                | GraphPathFunction::Edge,
                            ..
                        }
                ) {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        SetNode::Scope(input) | SetNode::Filter { input, .. } => {
            independent(input, control)
        }
        SetNode::Project { input, projection, quantifier: GraphSetQuantifier::All, .. } => {
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
            independent(input, control)
        }
        _ => Ok(false),
    }
}

fn rebound<E>(
    code: &[GraphSetPredicateOp],
    slots: &[usize],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Vec<GraphSetPredicateOp>, E> {
    let mut output = Vec::new();
    for op in code {
        charge(1, control)?;
        let mut op = op.clone();
        let remap = |operand: &mut GraphSetOperand| {
            if let GraphSetOperand::Column(column) = operand {
                *column = slots[*column];
            }
        };
        match &mut op {
            GraphSetPredicateOp::Compare { left, right, .. } => {
                remap(left);
                remap(right);
            }
            GraphSetPredicateOp::IsNull { operand, .. } => remap(operand),
            _ => {}
        }
        output.push(op);
    }
    Ok(output)
}

impl PreparedGraphSet {
    /// Prove one unpaged Inner/Optional Apply is a maintained relational join.
    ///
    /// The body contains one Arguments-to-independent-graph inner join or
    /// product, transparent scopes, and ALL row projections/selections. The
    /// Arguments side may only reorder/repeat columns. A no-import body may
    /// start directly at the independent pattern. Source identity/path outputs
    /// and total column/literal wrappers are admitted; fallible stored-property
    /// and catalog output, graph Boolean programs, procedures and other source
    /// effects refuse. Both original children still require host admission,
    /// including when either currently has zero rows.
    ///
    /// Inner Apply retains row-local computed stages in their original order.
    /// All expression branches must pass the existing reader/ordinal-free VM
    /// proof. Optional Apply lifts its complete selections into ON, crossing
    /// only column projections before the final selection. Every later export
    /// is guarded by a private right-hand TRUE witness: no witness makes the
    /// entire export NULL without evaluating its expression, including literals
    /// and imported values. A matched all-NULL right tuple still has a witness.
    ///
    /// DISTINCT, order/pages (even LIMIT 0), aggregation, collection, nested
    /// invocation and computed-before-selection Optional bodies refuse. Thus
    /// per-invocation COUNT is handled only by the separate count recipe.
    /// The existence recipe handles the literal-witness LIMIT-one lowering.
    /// This proves bags only, never positional enumeration or an outer window.
    /// Repeated/nonprefix imports preserve the original outer multiplicities;
    /// no projected-import bag is joined back to its parent. Internal width or
    /// predicate bounds can conservatively refuse an otherwise valid query.
    ///
    /// The callback controls bounded definition work. No rows or sources are
    /// evaluated here, and cancellation leaves the original definition intact.
    pub fn incremental_apply_join_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<MaintainedApplyJoin<'_>>, E> {
        control(GlaExecutionEvent::Work)?;
        if !self.unadorned_incremental_node() {
            return Ok(None);
        }
        let SetNode::Apply { left, right: body_query, imports, kind } = &self.node else {
            return Ok(None);
        };
        if matches!(kind, ApplyKind::Collect { .. }) {
            return Ok(None);
        }
        let optional = *kind == ApplyKind::Optional;
        let mut stages = Vec::new();
        let Some(base) = body(body_query, imports.is_empty(), &mut stages, control)? else {
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
        let left_width = left.types.len();
        let right_width = base.right.types.len();
        charge(arguments.len() + right_width, control)?;
        let mut slots = arguments.clone();
        slots.extend((0..right_width).map(|column| left_width + column));
        let base_slots = slots.clone();
        let mut on = match base.spec.and_then(RowJoinSpec::predicate) {
            Some(code) => rebound(code, &slots, control)?,
            None => Vec::new(),
        };
        let last_filter = stages.iter().rposition(|stage| matches!(stage, Stage::Filter(..)));
        for (index, stage) in stages.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            match stage {
                Stage::Project(_, projection) => {
                    for cell in *projection {
                        control(GlaExecutionEvent::Work)?;
                        if !cell.value().is_scan_local() {
                            return Ok(None);
                        }
                    }
                    if optional && last_filter.is_some_and(|last| index < last) {
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
                Stage::Filter(code) if optional => {
                    let next = rebound(code, &slots, control)?;
                    let conjunction = !on.is_empty();
                    charge(next.len() + usize::from(conjunction), control)?;
                    on.extend(next);
                    if conjunction {
                        on.push(GraphSetPredicateOp::And);
                    }
                }
                Stage::Filter(..) => {}
            }
        }
        let right_projection = if optional {
            charge(right_width + 1, control)?;
            let mut values: Vec<_> = (0..right_width).map(GraphSetValue::Column).collect();
            values.push(GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Bool(true))));
            let Some(projection) = project(&base.right.types, values, control)? else {
                return Ok(None);
            };
            Some(projection)
        } else {
            None
        };
        let right_types = right_projection
            .as_ref()
            .map_or(base.right.types.as_slice(), RowProjectionSpec::column_types);
        let mut keys = Vec::new();
        if let Some(spec) = base.spec {
            for &(a, b) in spec.keys() {
                charge(1, control)?;
                keys.push((arguments[a], b));
            }
        }
        let join = if keys.is_empty() {
            RowJoinSpec::cross(&left.types, right_types)
        } else {
            RowJoinSpec::new(&left.types, right_types, &keys)
        };
        let Ok(mut join) = join else { return Ok(None) };
        if !on.is_empty() {
            let Ok(selected) = join.with_predicate(&on) else { return Ok(None) };
            join = selected;
        }
        join = join.with_kind(if optional { RowJoinKind::Left } else { RowJoinKind::Inner });
        let mut types: Vec<_> = join.column_types().collect();
        // Keep the original body schema first. Its checked VM programs then
        // need no rewriting, including lexical locals inside CASE/list values.
        // The complete outer tuple and optional witness are a private suffix.
        charge(base_slots.len() + left_width + usize::from(optional), control)?;
        let mut values: Vec<_> = base_slots.into_iter().map(GraphSetValue::Column).collect();
        values.extend((0..left_width).map(GraphSetValue::Column));
        if optional {
            values.push(GraphSetValue::Column(left_width + right_width));
        }
        let Some(frame) = project(&types, values, control)? else {
            return Ok(None);
        };
        let mut width = arguments.len() + right_width;
        types = frame.column_types().to_vec();
        let mut output = vec![frame];
        for stage in stages {
            control(GlaExecutionEvent::Work)?;
            match stage {
                Stage::Project(query, projection) => {
                    charge(projection.len() + left_width + usize::from(optional), control)?;
                    let marker = width + left_width;
                    let mut values: Vec<_> = projection
                        .iter()
                        .map(|cell| cell.value().clone())
                        .collect();
                    values.extend((width..width + left_width).map(GraphSetValue::Column));
                    if optional {
                        values.push(GraphSetValue::Column(marker));
                    }
                    let Some(mut projected) = project(&types, values, control)? else {
                        return Ok(None);
                    };
                    if optional {
                        let Ok(guarded) = projected.with_null_guard(marker, 0..projection.len()) else {
                            return Ok(None);
                        };
                        projected = guarded;
                    }
                    if projected.column_types().get(..projection.len()) != Some(query.types.as_slice()) {
                        return Ok(None);
                    }
                    width = projection.len();
                    types = projected.column_types().to_vec();
                    output.push(projected);
                }
                Stage::Filter(_) if optional => {}
                Stage::Filter(code) => {
                    charge(types.len() + code.len(), control)?;
                    let Ok(selected) = RowProjectionSpec::selection(
                        types.clone(), names(types.len()), code,
                    ) else { return Ok(None) };
                    output.push(selected);
                }
            }
        }
        charge(left_width + width, control)?;
        let mut values: Vec<_> = (width..width + left_width).map(GraphSetValue::Column).collect();
        values.extend((0..width).map(GraphSetValue::Column));
        let Some(mut restored) = project(&types, values, control)? else {
            return Ok(None);
        };
        if optional {
            let Ok(guarded) = restored.with_null_guard(
                width + left_width,
                left_width..left_width + width,
            ) else {
                return Ok(None);
            };
            restored = guarded;
        }
        if restored.column_types() != self.types.as_slice() {
            return Ok(None);
        }
        output.push(restored);
        // Selection metadata can retain repeated/nonidentifier names, which an
        // ordinary computed projection deliberately does not permit.
        charge(self.types.len(), control)?;
        let Ok(named) = RowProjectionSpec::selection(
            self.types.clone(), self.columns.clone(), &[GraphSetPredicateOp::Truth(Some(true))],
        ) else { return Ok(None) };
        output.push(named);
        control(GlaExecutionEvent::Work)?;
        Ok(Some(MaintainedApplyJoin {
            left,
            right: base.right,
            right_projection,
            join,
            output,
        }))
    }
}

#[cfg(test)]
mod tests;
