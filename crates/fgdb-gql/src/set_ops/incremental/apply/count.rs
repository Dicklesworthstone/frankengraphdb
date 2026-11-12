//! Keyed correlated counts through source restriction and native grouping.
//! Counts are formed from RHS weights, never from an outer-multiplied join.

use super::*;
use crate::algebra::{BindingSlot, GlaPlan, GraphValueRow, PreparedGraphPattern};
use crate::row_aggregate::definition::GroupDefinition;
use crate::{
    GraphAggregate, GraphAggregateFunction, GraphIntegerExpression, GraphIntegerOp,
    PreparedGraphSetAggregate,
};
use std::collections::BTreeSet;

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

/// Owned native COUNT recipe peeled from a single-part pattern's `Collect`
/// witness scope when every enclosing reference observes only its cardinality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintainedCollectedCount {
    pub left: PreparedGraphSet,
    pub right: PreparedGraphSet,
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

    /// Decorrelate a single-part `COUNT { MATCH ... }` witness collection into
    /// the same restricted native group + Left-join zero-fill recipe as
    /// [`Self::incremental_apply_count_with_control`], without altering one-shot
    /// `PreparedGraphSet` plans or transcripts.
    pub fn incremental_collected_count_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<MaintainedCollectedCount>, E> {
        control(GlaExecutionEvent::Work)?;
        if !self.unadorned_incremental_node() {
            return Ok(None);
        }
        let SetNode::Project {
            input,
            projection,
            quantifier,
            preserve_order: false,
        } = &self.node
        else {
            return Ok(None);
        };
        if !input.unadorned_incremental_node() {
            return Ok(None);
        }
        let SetNode::Pattern(pattern) = &input.node else {
            return Ok(None);
        };
        if pattern.logical.visible_columns.is_some() {
            return Ok(None);
        }
        let [
            body @ ..,
            GlaOperator::ProjectValues {
                columns: pattern_cols,
            },
            GlaOperator::OrderByValues,
            GlaOperator::Limit {
                offset: 0,
                count: None,
            },
        ] = pattern.logical.operators()
        else {
            return Ok(None);
        };
        if pattern_cols.is_empty() || pattern_cols.len() != pattern.columns().len() {
            return Ok(None);
        }
        let mut collected_cols = Vec::new();
        for (idx, col) in pattern_cols.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            if let ValueProjection::Collected { group } = col {
                collected_cols.push((idx, *group));
            }
        }
        let Some(&(target_col, target_group)) = collected_cols.last() else {
            return Ok(None);
        };
        let mut other_collected = BTreeSet::new();
        for &(idx, group) in &collected_cols[..collected_cols.len() - 1] {
            if group == target_group {
                return Ok(None);
            }
            other_collected.insert(idx);
        }
        let mut rewritten_projection = Vec::with_capacity(projection.len());
        for item in projection {
            control(GlaExecutionEvent::Work)?;
            let Some(value) =
                rewrite_collected_size(item.value(), target_col, &other_collected, control)?
            else {
                return Ok(None);
            };
            rewritten_projection.push(GraphSetProjection::new(item.name(), value));
        }

        // Locate the top-level Collect .. CollectEnd scope for target_group.
        let mut collect_bounds = None;
        let mut skip_until = 0usize;
        for (idx, op) in body.iter().enumerate() {
            control(GlaExecutionEvent::Work)?;
            if idx < skip_until {
                continue;
            }
            match op {
                GlaOperator::Collect { group, end, slots } if *group == target_group => {
                    let end_idx = *end as usize;
                    if end_idx >= body.len() || end_idx <= idx {
                        return Ok(None);
                    }
                    let GlaOperator::CollectEnd {
                        group: end_group,
                        value: None,
                        tuple: None,
                    } = &body[end_idx]
                    else {
                        return Ok(None);
                    };
                    if *end_group != target_group {
                        return Ok(None);
                    }
                    collect_bounds = Some((idx, end_idx, *slots));
                    break;
                }
                GlaOperator::Collect { end, .. }
                | GlaOperator::Probe { end, .. }
                | GlaOperator::Optional { end, .. } => {
                    skip_until = (*end as usize).saturating_add(1);
                }
                _ => {}
            }
        }
        let Some((collect_start, collect_end, inner_slots)) = collect_bounds else {
            return Ok(None);
        };
        if inner_slots == 0 {
            return Ok(None);
        }

        // Compute outer slot width `base` and `capture_base` at `collect_start`.
        // Probe and Collect frames do not export slots or captures to `width`.
        let mut base = 0u32;
        let mut capture_base = 0u32;
        let mut cursor = 0usize;
        while cursor < collect_start {
            control(GlaExecutionEvent::Work)?;
            match &body[cursor] {
                GlaOperator::Probe { end, .. } | GlaOperator::Collect { end, .. } => {
                    cursor = (*end as usize).saturating_add(1);
                    continue;
                }
                GlaOperator::ScanEdges { .. } => base = base.saturating_add(2),
                GlaOperator::ScanVertices
                | GlaOperator::BindVertex { .. }
                | GlaOperator::BindOuterVertex { .. }
                | GlaOperator::Expand { .. }
                | GlaOperator::VarLengthExpand { .. } => base = base.saturating_add(1),
                GlaOperator::CapturePath { capture, .. } => {
                    capture_base = capture_base.max(capture.saturating_add(1));
                }
                _ => {}
            }
            cursor += 1;
        }
        if base == 0 {
            return Ok(None);
        }

        let unmap_slot = |slot: BindingSlot| -> Option<BindingSlot> {
            (slot.ordinal() >= base && slot.ordinal() < base.saturating_add(inner_slots))
                .then(|| BindingSlot(slot.ordinal() - base))
        };
        let unmap_or_correlated = |slot: BindingSlot,
                                   correlations: &[(BindingSlot, BindingSlot)]|
         -> Option<BindingSlot> {
            unmap_slot(slot).or_else(|| {
                correlations
                    .iter()
                    .find(|(_, outer)| *outer == slot)
                    .map(|(inner, _)| *inner)
            })
        };
        let add_correlation = |correlations: &mut Vec<(BindingSlot, BindingSlot)>,
                               inner: BindingSlot,
                               outer: BindingSlot|
         -> bool {
            if outer.ordinal() >= base {
                return false;
            }
            if !correlations.contains(&(inner, outer)) {
                correlations.push((inner, outer));
            }
            true
        };

        let mut correlations = Vec::new();
        let mut right_ops = Vec::new();
        let mut next_inner_slot = 0u32;
        for op in &body[collect_start + 1..collect_end] {
            control(GlaExecutionEvent::Work)?;
            match op {
                GlaOperator::BindVertex { source } => {
                    let inner = BindingSlot(next_inner_slot);
                    next_inner_slot = next_inner_slot.saturating_add(1);
                    if !add_correlation(&mut correlations, inner, *source) {
                        return Ok(None);
                    }
                    right_ops.push(GlaOperator::ScanVertices);
                }
                GlaOperator::ScanVertices => {
                    next_inner_slot = next_inner_slot.saturating_add(1);
                    right_ops.push(GlaOperator::ScanVertices);
                }
                GlaOperator::Expand {
                    source,
                    relation,
                    direction,
                } => {
                    let Some(src) = unmap_slot(*source) else {
                        return Ok(None);
                    };
                    next_inner_slot = next_inner_slot.saturating_add(1);
                    right_ops.push(GlaOperator::Expand {
                        source: src,
                        relation: *relation,
                        direction: *direction,
                    });
                }
                GlaOperator::Select { slot, predicates } => {
                    let Some(mapped) = unmap_slot(*slot) else {
                        return Ok(None);
                    };
                    right_ops.push(GlaOperator::Select {
                        slot: mapped,
                        predicates: predicates.clone(),
                    });
                }
                GlaOperator::VertexIdentity { left, right, equal } => {
                    match (unmap_slot(*left), unmap_slot(*right)) {
                        (Some(l), Some(r)) => {
                            right_ops.push(GlaOperator::VertexIdentity {
                                left: l,
                                right: r,
                                equal: *equal,
                            });
                        }
                        (None, Some(inner)) if *equal => {
                            if !add_correlation(&mut correlations, inner, *left) {
                                return Ok(None);
                            }
                        }
                        (Some(inner), None) if *equal => {
                            if !add_correlation(&mut correlations, inner, *right) {
                                return Ok(None);
                            }
                        }
                        _ => {
                            let (Some(l), Some(r)) = (
                                unmap_or_correlated(*left, &correlations),
                                unmap_or_correlated(*right, &correlations),
                            ) else {
                                return Ok(None);
                            };
                            right_ops.push(GlaOperator::VertexIdentity {
                                left: l,
                                right: r,
                                equal: *equal,
                            });
                        }
                    }
                }
                GlaOperator::CompareProperties {
                    left,
                    left_key,
                    right,
                    right_key,
                    comparison,
                } => {
                    let (Some(l), Some(r)) = (
                        unmap_or_correlated(*left, &correlations),
                        unmap_or_correlated(*right, &correlations),
                    ) else {
                        return Ok(None);
                    };
                    right_ops.push(GlaOperator::CompareProperties {
                        left: l,
                        left_key: *left_key,
                        right: r,
                        right_key: *right_key,
                        comparison: *comparison,
                    });
                }
                GlaOperator::CapturePath {
                    capture,
                    start,
                    segments,
                } => {
                    let Some(cap) = capture.checked_sub(capture_base) else {
                        return Ok(None);
                    };
                    let Some(st) = unmap_slot(*start) else {
                        return Ok(None);
                    };
                    let Some(segs): Option<Vec<_>> =
                        segments.iter().copied().map(unmap_slot).collect()
                    else {
                        return Ok(None);
                    };
                    right_ops.push(GlaOperator::CapturePath {
                        capture: cap,
                        start: st,
                        segments: segs,
                    });
                }
                GlaOperator::DifferentEdges { segments } => {
                    let Some(segs): Option<Vec<_>> =
                        segments.iter().copied().map(unmap_slot).collect()
                    else {
                        return Ok(None);
                    };
                    right_ops.push(GlaOperator::DifferentEdges { segments: segs });
                }
                GlaOperator::SelectBoolean { expression } => {
                    let Some(remapped) = expression.try_remap_elements(
                        |slot| unmap_or_correlated(slot, &correlations),
                        |cap| cap.checked_sub(capture_base),
                    ) else {
                        return Ok(None);
                    };
                    right_ops.push(GlaOperator::SelectBoolean {
                        expression: remapped,
                    });
                }
                _ => return Ok(None),
            }
        }
        if next_inner_slot != inner_slots {
            return Ok(None);
        }

        // Fold a lowered edge root (ScanVertices + Select*(0) + Expand(0)) back
        // into canonical ScanEdges so OneHop/MultiHop maintainers recognize it.
        if matches!(right_ops.first(), Some(GlaOperator::ScanVertices)) {
            let mut expand_at = 1usize;
            while matches!(
                right_ops.get(expand_at),
                Some(GlaOperator::Select {
                    slot: BindingSlot(0),
                    ..
                })
            ) {
                expand_at += 1;
            }
            if let Some(&GlaOperator::Expand {
                source: BindingSlot(0),
                relation,
                direction,
            }) = right_ops.get(expand_at)
            {
                right_ops[0] = GlaOperator::ScanEdges {
                    relation,
                    direction,
                };
                right_ops.remove(expand_at);
            }
        }

        let right_edges = right_ops
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    GlaOperator::ScanEdges { .. } | GlaOperator::Expand { .. }
                )
            })
            .count();
        let mut right_key_slots = Vec::new();
        for &(inner_slot, _) in &correlations {
            if !right_key_slots.contains(&inner_slot) {
                right_key_slots.push(inner_slot);
            }
        }
        let right_proj_cols: Vec<ValueProjection> = if right_key_slots.is_empty() {
            vec![ValueProjection::Vertex {
                slot: BindingSlot(0),
            }]
        } else {
            right_key_slots
                .iter()
                .map(|&slot| ValueProjection::Vertex { slot })
                .collect()
        };
        let right_names: Vec<String> = (0..right_proj_cols.len())
            .map(|i| format!("__fg_collect_rhs_{i}"))
            .collect();
        right_ops.push(GlaOperator::ProjectValues {
            columns: right_proj_cols,
        });
        right_ops.push(GlaOperator::OrderByValues);
        right_ops.push(GlaOperator::Limit {
            offset: 0,
            count: None,
        });
        let mut right_logical = GlaPlan::from_operators(right_ops);
        right_logical.reverse_catalog = pattern.logical.reverse_catalog.clone();
        let right_pattern: PreparedGraphPattern<GraphValueRow> = PreparedGraphPattern::from_parts(
            right_logical,
            inner_slots as usize,
            right_edges,
            right_names,
        );
        let right = PreparedGraphSet::from(right_pattern);

        // Build peeled outer `left` pattern, removing `collect_start..=collect_end`.
        let removed_len = (collect_end - collect_start + 1) as u32;
        let mut left_ops = Vec::with_capacity(body.len() - (collect_end - collect_start + 1) + 3);
        for (idx, op) in body.iter().enumerate() {
            if (collect_start..=collect_end).contains(&idx) {
                continue;
            }
            let adjusted = match op.clone() {
                GlaOperator::Probe { group, end, anti } if end as usize > collect_end => {
                    GlaOperator::Probe {
                        group,
                        end: end - removed_len,
                        anti,
                    }
                }
                GlaOperator::Optional { group, end, slots } if end as usize > collect_end => {
                    GlaOperator::Optional {
                        group,
                        end: end - removed_len,
                        slots,
                    }
                }
                GlaOperator::Collect { group, end, slots } if end as usize > collect_end => {
                    GlaOperator::Collect {
                        group,
                        end: end - removed_len,
                        slots,
                    }
                }
                other => other,
            };
            left_ops.push(adjusted);
        }
        let mut left_cols = Vec::new();
        let mut left_names = Vec::new();
        for (idx, col) in pattern_cols.iter().enumerate() {
            if idx == target_col {
                continue;
            }
            left_cols.push(*col);
            left_names.push(pattern.columns()[idx].clone());
        }
        let mut next_outer_alias = 0usize;
        let mut fresh_outer_name = |existing: &[String]| -> String {
            loop {
                let candidate = format!("__fg_collect_outer_{next_outer_alias}");
                next_outer_alias += 1;
                if !existing.contains(&candidate) {
                    return candidate;
                }
            }
        };
        let mut reversed = Vec::new();
        let mut group_keys = Vec::new();
        let mut joined = Vec::new();
        for &(inner_slot, outer_slot) in &correlations {
            charge(3 + group_keys.len(), control)?;
            let outer_col = match left_cols.iter().position(
                |col| matches!(col, ValueProjection::Vertex { slot } if *slot == outer_slot),
            ) {
                Some(pos) => pos,
                None => {
                    let pos = left_cols.len();
                    left_cols.push(ValueProjection::Vertex { slot: outer_slot });
                    let alias = fresh_outer_name(&left_names);
                    left_names.push(alias);
                    pos
                }
            };
            let right_col = right_key_slots
                .iter()
                .position(|&slot| slot == inner_slot)
                .unwrap_or(0);
            reversed.push((right_col, outer_col));
            let key = match group_keys.iter().position(|&col| col == right_col) {
                Some(at) => at,
                None => {
                    let at = group_keys.len();
                    group_keys.push(right_col);
                    at
                }
            };
            joined.push((outer_col, key));
        }
        if left_cols.is_empty() {
            left_cols.push(ValueProjection::Vertex {
                slot: BindingSlot(0),
            });
            let alias = fresh_outer_name(&left_names);
            left_names.push(alias);
        }
        let left_edges = left_ops
            .iter()
            .filter(|op| {
                matches!(
                    op,
                    GlaOperator::ScanEdges { .. }
                        | GlaOperator::Expand { .. }
                        | GlaOperator::VarLengthExpand { .. }
                )
            })
            .count();
        left_ops.push(GlaOperator::ProjectValues { columns: left_cols });
        left_ops.push(GlaOperator::OrderByValues);
        left_ops.push(GlaOperator::Limit {
            offset: 0,
            count: None,
        });
        let mut left_logical = GlaPlan::from_operators(left_ops);
        left_logical.reverse_catalog = pattern.logical.reverse_catalog.clone();
        let left_pattern: PreparedGraphPattern<GraphValueRow> = PreparedGraphPattern::from_parts(
            left_logical,
            base as usize,
            left_edges,
            left_names.clone(),
        );
        let mut left = PreparedGraphSet::from(left_pattern);
        if !other_collected.is_empty() {
            // Earlier Collect columns all sit at indices < target_col, so their
            // positions in `left` match `pattern_cols`. Wrap `left` in a Size
            // projection so recursive compilation peels each remaining Collect.
            let nested_proj: Vec<GraphSetProjection> = (0..left_names.len())
                .map(|idx| {
                    let value = if other_collected.contains(&idx) {
                        GraphSetValue::Size(Box::new(GraphSetValue::Column(idx)))
                    } else {
                        GraphSetValue::Column(idx)
                    };
                    GraphSetProjection::new(&left_names[idx], value)
                })
                .collect();
            let Ok(wrapped) = left.project(nested_proj, GraphSetQuantifier::All) else {
                return Ok(None);
            };
            left = wrapped;
        }

        let restrict = if reversed.is_empty() {
            RowJoinSpec::cross(&right.types, &left.types)
        } else {
            RowJoinSpec::new(&right.types, &left.types, &reversed)
        };
        let Ok(restrict) = restrict else {
            return Ok(None);
        };
        let restrict = restrict.with_kind(RowJoinKind::Semi);
        charge(right.types.len() + left.types.len(), control)?;
        let Ok(active) = right.clone().join(left.clone(), restrict.clone()) else {
            return Ok(None);
        };
        let mut count_name = None;
        for suffix in 0..=right.columns.len() {
            charge(1 + right.columns.len(), control)?;
            let candidate = format!("__fg_count_{suffix}");
            if !right.columns.contains(&candidate) {
                count_name = Some(candidate);
                break;
            }
        }
        let Some(count_name) = count_name else {
            return Ok(None);
        };
        let Ok(grouped) = active.group_by(&group_keys, &[GraphAggregate::count_rows(&count_name)])
        else {
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
        let join_types: Vec<_> = join.column_types().collect();
        let Ok(zero_filled_expr) = GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(left.types.len() + group_keys.len()),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Coalesce,
        ]) else {
            return Ok(None);
        };
        let mut restored_values = Vec::with_capacity(pattern_cols.len());
        for idx in 0..pattern_cols.len() {
            control(GlaExecutionEvent::Work)?;
            if idx < target_col {
                restored_values.push(GraphSetValue::Column(idx));
            } else if idx == target_col {
                restored_values.push(GraphSetValue::Integer(zero_filled_expr.clone()));
            } else {
                restored_values.push(GraphSetValue::Column(idx - 1));
            }
        }
        let Some(zero_filled) = project(&join_types, restored_values, control)? else {
            return Ok(None);
        };
        let Ok(user_proj) = RowProjectionSpec::new(
            zero_filled.column_types().to_vec(),
            rewritten_projection,
            *quantifier,
        ) else {
            return Ok(None);
        };
        if user_proj.column_types() != self.types.as_slice() {
            return Ok(None);
        }
        let Ok(named) = RowProjectionSpec::selection(
            self.types.clone(),
            self.columns.clone(),
            &[GraphSetPredicateOp::Truth(Some(true))],
        ) else {
            return Ok(None);
        };
        control(GlaExecutionEvent::Work)?;
        Ok(Some(MaintainedCollectedCount {
            left,
            right,
            restrict,
            group: *group,
            group_columns,
            group_types,
            join,
            output: vec![zero_filled, user_proj, named],
        }))
    }
}

fn rewrite_collected_size<E>(
    value: &GraphSetValue,
    target_col: usize,
    other_collected: &BTreeSet<usize>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<GraphSetValue>, E> {
    control(GlaExecutionEvent::Work)?;
    let is_collected = |col: usize| col == target_col || other_collected.contains(&col);
    Ok(Some(match value {
        GraphSetValue::Size(inner) => {
            if let GraphSetValue::Column(col) = inner.as_ref()
                && is_collected(*col)
            {
                GraphSetValue::Column(*col)
            } else {
                let Some(inner) =
                    rewrite_collected_size(inner, target_col, other_collected, control)?
                else {
                    return Ok(None);
                };
                GraphSetValue::Size(Box::new(inner))
            }
        }
        GraphSetValue::Column(col) => {
            if is_collected(*col) {
                return Ok(None);
            }
            GraphSetValue::Column(*col)
        }
        GraphSetValue::Literal(v) => GraphSetValue::Literal(v.clone()),
        GraphSetValue::TypedNull(t) => GraphSetValue::TypedNull(*t),
        GraphSetValue::Integer(expr) => GraphSetValue::Integer(expr.clone()),
        GraphSetValue::Value(v) => GraphSetValue::Value(v.clone()),
        GraphSetValue::Local(at) => GraphSetValue::Local(*at),
        GraphSetValue::Random { site, scope } => GraphSetValue::Random {
            site: *site,
            scope: *scope,
        },
        GraphSetValue::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Some(rewritten) =
                    rewrite_collected_size(item, target_col, other_collected, control)?
                else {
                    return Ok(None);
                };
                out.push(rewritten);
            }
            GraphSetValue::List(out)
        }
        GraphSetValue::Index { list, index } => {
            let Some(list) = rewrite_collected_size(list, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let Some(index) = rewrite_collected_size(index, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::Index {
                list: Box::new(list),
                index: Box::new(index),
            }
        }
        GraphSetValue::In { value: item, list } => {
            let Some(item) = rewrite_collected_size(item, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let Some(list) = rewrite_collected_size(list, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::In {
                value: Box::new(item),
                list: Box::new(list),
            }
        }
        GraphSetValue::Equal { left, right } => {
            let Some(left) = rewrite_collected_size(left, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let Some(right) = rewrite_collected_size(right, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::Equal {
                left: Box::new(left),
                right: Box::new(right),
            }
        }
        GraphSetValue::Ordered {
            comparison,
            left,
            right,
        } => {
            let Some(left) = rewrite_collected_size(left, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let Some(right) = rewrite_collected_size(right, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::Ordered {
                comparison: *comparison,
                left: Box::new(left),
                right: Box::new(right),
            }
        }
        GraphSetValue::Comprehension { list, filter, map } => {
            let Some(list) = rewrite_collected_size(list, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let filter = match filter {
                Some(f) => {
                    let Some(f) = rewrite_collected_size(f, target_col, other_collected, control)?
                    else {
                        return Ok(None);
                    };
                    Some(Box::new(f))
                }
                None => None,
            };
            let map = match map {
                Some(m) => {
                    let Some(m) = rewrite_collected_size(m, target_col, other_collected, control)?
                    else {
                        return Ok(None);
                    };
                    Some(Box::new(m))
                }
                None => None,
            };
            GraphSetValue::Comprehension {
                list: Box::new(list),
                filter,
                map,
            }
        }
        GraphSetValue::Quantifier {
            kind,
            list,
            predicate,
        } => {
            let Some(list) = rewrite_collected_size(list, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let Some(predicate) =
                rewrite_collected_size(predicate, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::Quantifier {
                kind: *kind,
                list: Box::new(list),
                predicate: Box::new(predicate),
            }
        }
        GraphSetValue::Slice { list, from, to } => {
            let Some(list) = rewrite_collected_size(list, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let from = match from {
                Some(f) => {
                    let Some(f) = rewrite_collected_size(f, target_col, other_collected, control)?
                    else {
                        return Ok(None);
                    };
                    Some(Box::new(f))
                }
                None => None,
            };
            let to = match to {
                Some(t) => {
                    let Some(t) = rewrite_collected_size(t, target_col, other_collected, control)?
                    else {
                        return Ok(None);
                    };
                    Some(Box::new(t))
                }
                None => None,
            };
            GraphSetValue::Slice {
                list: Box::new(list),
                from,
                to,
            }
        }
        GraphSetValue::Range { start, end, step } => {
            let Some(start) = rewrite_collected_size(start, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let Some(end) = rewrite_collected_size(end, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let step = match step {
                Some(s) => {
                    let Some(s) = rewrite_collected_size(s, target_col, other_collected, control)?
                    else {
                        return Ok(None);
                    };
                    Some(Box::new(s))
                }
                None => None,
            };
            GraphSetValue::Range {
                start: Box::new(start),
                end: Box::new(end),
                step,
            }
        }
        GraphSetValue::Reduce { init, list, expr } => {
            let Some(init) = rewrite_collected_size(init, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let Some(list) = rewrite_collected_size(list, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let Some(expr) = rewrite_collected_size(expr, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::Reduce {
                init: Box::new(init),
                list: Box::new(list),
                expr: Box::new(expr),
            }
        }
        GraphSetValue::MapLiteral {
            keys,
            values,
            guard,
        } => {
            let mut out = Vec::with_capacity(values.len());
            for v in values {
                let Some(v) = rewrite_collected_size(v, target_col, other_collected, control)?
                else {
                    return Ok(None);
                };
                out.push(v);
            }
            let guard = match guard {
                Some(g) => {
                    let Some(g) = rewrite_collected_size(g, target_col, other_collected, control)?
                    else {
                        return Ok(None);
                    };
                    Some(Box::new(g))
                }
                None => None,
            };
            GraphSetValue::MapLiteral {
                keys: keys.clone(),
                values: out,
                guard,
            }
        }
        GraphSetValue::MapOverlay { base, keys, values } => {
            let Some(base) = rewrite_collected_size(base, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            let mut out = Vec::with_capacity(values.len());
            for v in values {
                let Some(v) = rewrite_collected_size(v, target_col, other_collected, control)?
                else {
                    return Ok(None);
                };
                out.push(v);
            }
            GraphSetValue::MapOverlay {
                base: Box::new(base),
                keys: keys.clone(),
                values: out,
            }
        }
        GraphSetValue::MapGet { map, key, property } => {
            let Some(map) = rewrite_collected_size(map, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::MapGet {
                map: Box::new(map),
                key: key.clone(),
                property: *property,
            }
        }
        GraphSetValue::Keys(inner) => {
            let Some(inner) = rewrite_collected_size(inner, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::Keys(Box::new(inner))
        }
        GraphSetValue::ElementMetadata {
            kind,
            value: inner,
            catalog,
        } => {
            let Some(inner) = rewrite_collected_size(inner, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::ElementMetadata {
                kind: *kind,
                value: Box::new(inner),
                catalog: catalog.clone(),
            }
        }
        GraphSetValue::Let { values, body } => {
            let mut out = Vec::with_capacity(values.len());
            for v in values {
                let Some(v) = rewrite_collected_size(v, target_col, other_collected, control)?
                else {
                    return Ok(None);
                };
                out.push(v);
            }
            let Some(body) = rewrite_collected_size(body, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::Let {
                values: out,
                body: Box::new(body),
            }
        }
        GraphSetValue::Temporal {
            function,
            argument,
            zones,
        } => {
            let Some(argument) =
                rewrite_collected_size(argument, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::Temporal {
                function: *function,
                argument: Box::new(argument),
                zones: zones.clone(),
            }
        }
        GraphSetValue::Function { function, argument } => {
            let Some(argument) =
                rewrite_collected_size(argument, target_col, other_collected, control)?
            else {
                return Ok(None);
            };
            GraphSetValue::Function {
                function: *function,
                argument: Box::new(argument),
            }
        }
        GraphSetValue::Case { arms, otherwise } => {
            let mut out = Vec::with_capacity(arms.len());
            for (cond, arm) in arms {
                let Some(cond) =
                    rewrite_collected_size(cond, target_col, other_collected, control)?
                else {
                    return Ok(None);
                };
                let Some(arm) = rewrite_collected_size(arm, target_col, other_collected, control)?
                else {
                    return Ok(None);
                };
                out.push((cond, arm));
            }
            let otherwise = match otherwise {
                Some(o) => {
                    let Some(o) = rewrite_collected_size(o, target_col, other_collected, control)?
                    else {
                        return Ok(None);
                    };
                    Some(Box::new(o))
                }
                None => None,
            };
            GraphSetValue::Case {
                arms: out,
                otherwise,
            }
        }
    }))
}

#[cfg(test)]
mod tests;
