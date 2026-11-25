//! Keyed correlated summaries through source restriction and native grouping.
//! Aggregates use RHS weights, never an outer-multiplied join.

use super::*;
use crate::algebra::{BindingSlot, GlaPlan, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use crate::row_aggregate::definition::GroupDefinition;
use crate::row_window::RowWindowSpec;
use crate::{
    GraphAggregate, GraphAggregateFunction, GraphIntegerExpression, GraphIntegerOp,
    PreparedGraphSetAggregate,
};
use std::borrow::Cow;
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

/// Per-invocation aggregate tuples through the existing native row operators.
///
/// Project the original left bag through outer, then DISTINCT its arguments.
/// Join those arguments with the complete right source and apply body in
/// order, including each relational DISTINCT and UNWIND with its invocation
/// key retained. Argument-only bodies own a fixed zero-column unit source;
/// source-free expression stages also run only after invocation activation.
/// Group by that private key followed by the original grouping keys. Join
/// through complete to the DISTINCT argument bag, then apply summary and every
/// ordered post-group projection/filter. Only a global aggregate uses a Left
/// completion, COUNT's zero-fill and COLLECT's empty list; a grouped aggregate
/// has no empty group.
/// Restore the ORIGINAL keyed outer bag only
/// after the complete invocation succeeds. Optional calls null-extend only
/// after their complete post-group tail. Other aggregates retain their native
/// empty/NULL and value domains; a filtered summary is absent, never zero-filled
/// a second time. The private key survives post-group DISTINCT.
/// An optional terminal page selects each completed invocation independently,
/// after every post-group stage and before outer multiplicity restoration.
/// The host must share sources and publish all stages atomically.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintainedFilteredAggregate<'a> {
    pub left: &'a PreparedGraphSet,
    pub right: Cow<'a, PreparedGraphSet>,
    pub outer: RowProjectionSpec,
    pub arguments: RowProjectionSpec,
    pub join: RowJoinSpec,
    pub body: Vec<RowProjectionSpec>,
    pub group: PreparedGraphSetAggregate,
    pub group_columns: Vec<String>,
    pub group_types: Vec<GraphSetColumnType>,
    pub invocations: RowProjectionSpec,
    pub complete: RowJoinSpec,
    pub summary: Vec<RowProjectionSpec>,
    pub window: Option<RowWindowSpec>,
    pub restore: RowJoinSpec,
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

fn aggregate_summary<'a, E>(
    query: &'a PreparedGraphSet,
    terminal: Option<&PreparedGraphSet>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<&'a PreparedGraphSetAggregate>, E> {
    control(GlaExecutionEvent::Work)?;
    if !query.unadorned_incremental_node()
        && !terminal.is_some_and(|page| core::ptr::eq(page, query))
    {
        return Ok(None);
    }
    match &query.node {
        SetNode::Scope(input) => aggregate_summary(input, terminal, control),
        SetNode::Project {
            input,
            projection,
            quantifier: GraphSetQuantifier::All,
            ..
        } if projection.len() == input.types.len() => {
            // Only complete identity aliases are transparent. Other projections
            // need their own ordered output stage after all conversions.
            for (column, cell) in projection.iter().enumerate() {
                control(GlaExecutionEvent::Work)?;
                if !matches!(cell.value(), GraphSetValue::Column(input) if *input == column) {
                    return Ok(None);
                }
            }
            aggregate_summary(input, terminal, control)
        }
        SetNode::Aggregate(summary) => {
            if !summary.having().is_empty()
                || summary.having_expression().is_some()
                || summary.has_incremental_output_transform()
                || summary.has_computed_ordering()
                || summary.incremental_result_window() != (0, None)
                || !summary.supports_incremental_maintenance_with_having()
            {
                return Ok(None);
            }
            let specs = summary.aggregate_specs();
            if specs.len() == 0
                || summary.group_key_columns().len() + specs.len() != query.types.len()
            {
                return Ok(None);
            }
            for spec in specs {
                control(GlaExecutionEvent::Work)?;
                if !matches!(
                    spec,
                    (GraphAggregateFunction::CountRows, None)
                        | (
                            GraphAggregateFunction::Count
                                | GraphAggregateFunction::CountDistinct
                                | GraphAggregateFunction::SumInt
                                | GraphAggregateFunction::SumIntDistinct
                                | GraphAggregateFunction::AverageInt
                                | GraphAggregateFunction::AverageIntDistinct
                                | GraphAggregateFunction::Min
                                | GraphAggregateFunction::Max
                                | GraphAggregateFunction::Collect
                                | GraphAggregateFunction::CollectDistinct,
                            Some(_),
                        )
                ) {
                    return Ok(None);
                }
            }
            Ok(Some(summary))
        }
        _ => Ok(None),
    }
}

/// A collection observes occurrences, not merely the aggregate input bag.
/// Canonical complete rows need no extra state. Preserve the complete row of
/// a sort or DISTINCT when later copies and argument restoration hide its order.
enum CollectionInputOrder<'a> {
    Canonical,
    Witness(&'a PreparedGraphSet),
}

fn collection_input_order<'a, E>(
    query: &'a PreparedGraphSet,
    imports: &[usize],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<CollectionInputOrder<'a>>, E> {
    control(GlaExecutionEvent::Work)?;
    if query
        .incremental_result_order()
        .is_some_and(|(order, _)| order.is_empty())
    {
        return Ok(Some(CollectionInputOrder::Canonical));
    }
    let mut current = query;
    loop {
        control(GlaExecutionEvent::Work)?;
        if current.offset != 0 || current.count.is_some() {
            return Ok(None);
        }
        if !current.order.is_empty() {
            // A filter with its own order needs a separate staged predicate
            // proof. The ordinary filter accessor admits unadorned nodes only.
            if matches!(current.node, SetNode::Filter { .. }) {
                return Ok(None);
            }
            return Ok(Some(CollectionInputOrder::Witness(current)));
        }
        current = match &current.node {
            SetNode::Scope(input) => input,
            SetNode::Project {
                quantifier: GraphSetQuantifier::Distinct,
                ..
            } => return Ok(Some(CollectionInputOrder::Witness(current))),
            SetNode::Project {
                input,
                projection,
                quantifier: GraphSetQuantifier::All,
                preserve_order,
            } if *preserve_order || input.preserves_row_order() => {
                for cell in projection {
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
                input
            }
            SetNode::CrossJoin { left, right } => {
                if argument_slots(right, imports, control)?.is_none() {
                    return Ok(None);
                }
                left
            }
            _ => return Ok(None),
        };
    }
}

pub(super) enum SummaryStage<'a> {
    Project(
        &'a PreparedGraphSet,
        &'a [GraphSetProjection],
        GraphSetQuantifier,
    ),
    Filter(&'a [GraphSetPredicateOp]),
    Restore(&'a PreparedGraphSet, Vec<usize>),
    Unwind(&'a PreparedGraphSet, &'a GraphSetValue),
    CollectionOrder(&'a PreparedGraphSet),
}

pub(super) struct CapturedCollectionOrder<'a> {
    order: &'a [GraphValueOrder],
    width: usize,
}

type SummarySteps<'a> = (Vec<RowProjectionSpec>, Option<CapturedCollectionOrder<'a>>);

/// A scoped WITH may reintroduce an original, nullable argument after a
/// DISTINCT. Include it in the invocation key and retain its unmapped value.
pub(super) fn restored_columns<'a, 's: 'a, E>(
    stages: impl Iterator<Item = &'a SummaryStage<'s>>,
    key_slots: &mut Vec<usize>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Vec<usize>, E> {
    let mut restored = Vec::new();
    for stage in stages {
        control(GlaExecutionEvent::Work)?;
        if let SummaryStage::Restore(_, columns) = stage {
            for column in columns {
                charge(1 + restored.len() + key_slots.len(), control)?;
                if !restored.contains(column) {
                    restored.push(*column);
                }
                if !key_slots.contains(column) {
                    key_slots.push(*column);
                }
            }
        }
    }
    Ok(restored)
}

/// Peel row stages on either side of the aggregate without moving them across
/// its conversion boundary. Rejoining a projection of Arguments contributes
/// exactly one row from this invocation, even when an imported value is NULL.
/// Only the proved collection-order boundary may carry an input comparator;
/// deeper ordering and every input page still refuse.
pub(super) fn summary_stages<'a, E>(
    query: &'a PreparedGraphSet,
    imports: &[usize],
    terminal: Option<&PreparedGraphSet>,
    collection_order: Option<&PreparedGraphSet>,
    stages: &mut Vec<SummaryStage<'a>>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<&'a PreparedGraphSet>, E> {
    control(GlaExecutionEvent::Work)?;
    let capture_order = collection_order.is_some_and(|ordered| core::ptr::eq(ordered, query));
    if !query.unadorned_incremental_node()
        && !terminal.is_some_and(|page| core::ptr::eq(page, query))
        && !capture_order
    {
        return Ok(None);
    }
    let next = match &query.node {
        SetNode::Scope(input) => Some((input.as_ref(), None)),
        SetNode::Unwind { input, value } => {
            if !value.is_scan_local() {
                return Ok(None);
            }
            Some((input.as_ref(), Some(SummaryStage::Unwind(query, value))))
        }
        SetNode::Project {
            input,
            projection,
            quantifier,
            ..
        } => {
            for cell in projection {
                control(GlaExecutionEvent::Work)?;
                if !cell.value().is_scan_local() {
                    return Ok(None);
                }
            }
            Some((
                input.as_ref(),
                Some(SummaryStage::Project(query, projection, *quantifier)),
            ))
        }
        // The certified terminal page runs after its predicate. The guard
        // above still rejects intermediate pages and unconsumed ordering.
        SetNode::Filter { input, predicate } => Some((
            input.as_ref(),
            Some(SummaryStage::Filter(predicate.instructions())),
        )),
        SetNode::CrossJoin { left, right } => argument_slots(right, imports, control)?
            .map(|columns| (left.as_ref(), Some(SummaryStage::Restore(query, columns)))),
        _ => None,
    };
    let base = if let Some((input, stage)) = next {
        let Some(base) =
            summary_stages(input, imports, terminal, collection_order, stages, control)?
        else {
            return Ok(None);
        };
        if let Some(stage) = stage {
            control(GlaExecutionEvent::ScratchEntry)?;
            stages.push(stage);
        }
        base
    } else {
        query
    };
    if capture_order {
        control(GlaExecutionEvent::ScratchEntry)?;
        stages.push(SummaryStage::CollectionOrder(query));
    }
    Ok(Some(base))
}

/// The private suffix contains the non-null join key and any original nullable
/// values restored by WITH. They depend only on the invocation, so retaining
/// them across DISTINCT preserves its original per-invocation multiplicity.
/// Retain the complete sorted or DISTINCT row through later projections,
/// even when they drop every original ordering column.
pub(super) fn summary_steps<'a, E>(
    initial: RowProjectionSpec,
    stages: Vec<SummaryStage<'a>>,
    restored: &[usize],
    key_width: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<SummarySteps<'a>>, E> {
    let mut steps = vec![initial];
    let private_width = key_width + restored.len();
    let mut captured: Option<CapturedCollectionOrder<'a>> = None;
    for stage in stages {
        control(GlaExecutionEvent::Work)?;
        let types = steps.last().expect("initial frame").column_types();
        let retained_width = private_width + captured.as_ref().map_or(0, |order| order.width);
        let width = types.len() - retained_width;
        let (query, mut values, quantifier) = match stage {
            SummaryStage::CollectionOrder(query) => {
                if captured.is_some()
                    || query.offset != 0
                    || query.count.is_some()
                    || types.get(..width) != Some(query.types.as_slice())
                {
                    return Ok(None);
                }
                charge(types.len() + width, control)?;
                let values = (0..types.len())
                    .chain(0..width)
                    .map(GraphSetValue::Column)
                    .collect();
                let Some(step) = project(types, values, control)? else {
                    return Ok(None);
                };
                captured = Some(CapturedCollectionOrder {
                    order: &query.order,
                    width,
                });
                steps.push(step);
                continue;
            }
            SummaryStage::Filter(code) => {
                charge(types.len() + code.len(), control)?;
                let Ok(step) =
                    RowProjectionSpec::selection(types.to_vec(), names(types.len()), code)
                else {
                    return Ok(None);
                };
                steps.push(step);
                continue;
            }
            SummaryStage::Unwind(query, value) => {
                // Expansion appends one public cell after the retained suffix.
                // Move that cell in front of the suffix immediately, before
                // any later expression, filter, DISTINCT or aggregate reads
                // the original body's column positions. The two ordinary
                // owners preserve element multiplicity without expanding the
                // outer bag's repeated invocation occurrences.
                charge(types.len() + retained_width + 1, control)?;
                let Ok(expansion) = RowProjectionSpec::unwind(
                    types.to_vec(),
                    names(types.len()),
                    format!("__fg_apply_{}", types.len()),
                    value.clone(),
                ) else {
                    return Ok(None);
                };
                let values = (0..width)
                    .chain(core::iter::once(types.len()))
                    .chain(width..types.len())
                    .map(GraphSetValue::Column)
                    .collect();
                let Some(reordered) = project(expansion.column_types(), values, control)? else {
                    return Ok(None);
                };
                if reordered.column_types().get(..width + 1) != Some(query.types.as_slice()) {
                    return Ok(None);
                }
                steps.push(expansion);
                steps.push(reordered);
                continue;
            }
            SummaryStage::Project(query, cells, quantifier) => {
                charge(cells.len(), control)?;
                (
                    query,
                    cells
                        .iter()
                        .map(|cell| cell.value().clone())
                        .collect::<Vec<_>>(),
                    quantifier,
                )
            }
            SummaryStage::Restore(query, columns) => {
                charge(width + columns.len(), control)?;
                let mut values: Vec<_> = (0..width).map(GraphSetValue::Column).collect();
                for column in columns {
                    charge(restored.len(), control)?;
                    let Some(position) = restored.iter().position(|slot| *slot == column) else {
                        return Ok(None);
                    };
                    values.push(GraphSetValue::Column(width + key_width + position));
                }
                (query, values, GraphSetQuantifier::All)
            }
        };
        charge(retained_width, control)?;
        let public_width = values.len();
        values.extend((width..types.len()).map(GraphSetValue::Column));
        let Some(step) = distinct::projection(types, values, quantifier, control)? else {
            return Ok(None);
        };
        if step.column_types().get(..public_width) != Some(query.types.as_slice()) {
            return Ok(None);
        }
        steps.push(step);
    }
    if let Some(order) = &captured {
        // The collection kernel compares any explicit keys, then complete rows.
        // Put the original row first to preserve the full DISTINCT order or tie break.
        // Within one invocation all later cells are copies/constants of that
        // witness and its arguments; equal witnesses cannot acquire a new tie.
        let types = steps.last().expect("body frame").column_types();
        let suffix = types.len() - order.width;
        charge(types.len(), control)?;
        let values = (suffix..types.len())
            .chain(0..suffix)
            .map(GraphSetValue::Column)
            .collect();
        let Some(reordered) = project(types, values, control)? else {
            return Ok(None);
        };
        steps.push(reordered);
    }
    Ok(Some((steps, captured)))
}

// The source-restriction recipe below is specific to COUNT. In particular,
// its final integer zero-fill must never receive AVG or a typed extremum.
fn count_summary<'a, E>(
    query: &'a PreparedGraphSet,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<&'a PreparedGraphSetAggregate>, E> {
    let Some(summary) = aggregate_summary(query, None, control)? else {
        return Ok(None);
    };
    // The source-restriction fast path restores one scalar count, not the
    // original group domain. Grouped summaries use the invocation recipe.
    if !summary.group_key_columns().is_empty() {
        return Ok(None);
    }
    let mut specs = summary.aggregate_specs();
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

impl PreparedGraphSet {
    /// Prove a reader-free aggregate tuple with exact invocation identity.
    ///
    /// This fallback retains every ordered body projection and filter rather
    /// than tracing an aggregate argument through discarded computation. It
    /// admits any nonempty tuple of COUNT, integer SUM/AVG (including argument
    /// DISTINCT), typed MIN/MAX, and COLLECT with or without DISTINCT, including
    /// imported values. Collections retain canonical complete-row order or one
    /// proved explicit sort, including hidden keys dropped by later pure output
    /// projections. The complete sorted row remains a private ordering witness;
    /// unknown positional order and unproved post-sort effects still refuse.
    /// Relational
    /// DISTINCT retains the private invocation key at every body barrier.
    /// Reader-free list/range UNWIND may appear before and after the summary.
    /// Its appended member is placed before the private suffix so subsequent
    /// expressions keep their original column addresses. NULL/empty lists
    /// remove body rows; scalar/type/range failures remain conditional on an
    /// active invocation and their exact position among filters/projections.
    /// Bodies beginning at Arguments or the source-free unit require no graph
    /// source and never invent an invocation from that unit alone.
    /// The independent RHS keeps the conditional source-effect proof: no
    /// property reader or uninvoked fallible source is
    /// moved into eager maintenance.
    ///
    /// Argument tuples use the existing NULL-safe canonical key proof.
    /// DISTINCT arguments prevent duplicate outer occurrences from multiplying
    /// aggregates; the final restoration preserves those occurrences.
    /// Empty global bodies produce zero for each COUNT, an empty list for each
    /// COLLECT, and NULL for numeric/extremum aggregates. Grouped bodies instead
    /// produce only their actual groups,
    /// including a real NULL-key group when its input contains NULL values;
    /// they never fabricate a zero-count group after the last input retracts.
    /// NULL is an invocation value, not a missing invocation. Body evaluation
    /// happens only for active argument tuples, and native grouping owns
    /// NULL/distinct support, checked output conversion and retractions. A
    /// row-local post-group projection/filter runs after empty-summary completion
    /// and before outer multiplicity restoration. Each projection, including
    /// DISTINCT, retains the private invocation key. This admits native
    /// `WITH count(*) AS n WHERE n > 0 RETURN n + 1` without suppressing a
    /// discarded expression failure or inventing a result for a filtered group.
    /// Optional Apply preserves a filtered invocation as NULL exports; it does
    /// not evaluate the tail again on that fabricated row. An unfiltered empty
    /// global COUNT remains zero, including under Optional Apply.
    /// AVG and extrema retain their native output domains.
    ///
    /// One terminal finite page may follow the complete post-group tail. Its
    /// exact invocation key partitions the existing weighted row window;
    /// deleted winners refill from retained groups and outer duplicates do not
    /// consume page positions. Empty global completion still precedes the
    /// page, so SKIP can remove a zero COUNT or empty COLLECT before Optional
    /// null extension. Ordering the completed summaries does not relax the
    /// collection input's separate occurrence-order proof.
    /// Intermediate or nested pages, other unbounded ordering, internal exact-domain
    /// HAVING/output transforms,
    /// unsupported key domains and reader/ordinal effects retain their refusal.
    /// The optimized existing source-restriction recipe remains preferred.
    pub fn incremental_filtered_aggregate_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<MaintainedFilteredAggregate<'_>>, E> {
        control(GlaExecutionEvent::Work)?;
        if !self.unadorned_incremental_node() {
            return Ok(None);
        }
        let SetNode::Apply {
            left,
            right: body_query,
            imports,
            kind,
        } = &self.node
        else {
            return Ok(None);
        };
        if matches!(kind, ApplyKind::Collect { .. }) {
            return Ok(None);
        }
        let terminal = bounded_window_input(self);
        let mut tail = Vec::new();
        let Some(group_input) =
            summary_stages(body_query, imports, terminal, None, &mut tail, control)?
        else {
            return Ok(None);
        };
        let Some(summary) = aggregate_summary(group_input, terminal, control)? else {
            return Ok(None);
        };
        let original_keys = summary.group_key_columns();
        let grouped_width = original_keys.len();
        let input_order = if summary.aggregate_specs().any(|(function, _)| {
            matches!(
                function,
                GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct
            )
        }) {
            let Some(order) = collection_input_order(summary.input(), imports, control)? else {
                return Ok(None);
            };
            Some(order)
        } else {
            None
        };
        let collection_boundary = match input_order {
            Some(CollectionInputOrder::Witness(query)) => Some(query),
            _ => None,
        };
        let mut stages = Vec::new();
        let Some(source) = summary_stages(
            summary.input(),
            imports,
            None,
            collection_boundary,
            &mut stages,
            control,
        )?
        else {
            return Ok(None);
        };
        let (argument_source, right, source_join) = match &source.node {
            // Arguments have one row per invocation, including NULL imports.
            // A fixed zero-column unit supplies the ordinary RHS owner; all
            // body expressions stay AFTER the active invocation join. Owning
            // that unit in the recipe avoids borrowing a temporary definition.
            SetNode::Arguments => {
                charge(1, control)?;
                (
                    Some(source),
                    Cow::Owned(PreparedGraphSet::singleton()),
                    None,
                )
            }
            SetNode::Values => (None, Cow::Borrowed(source), None),
            _ => {
                let Some(base) = body_base(source, imports.is_empty()) else {
                    return Ok(None);
                };
                if !independent(base.right, control)? {
                    return Ok(None);
                }
                (base.arguments, Cow::Borrowed(base.right), base.spec)
            }
        };
        let slots = match argument_source {
            Some(input) => {
                let Some(slots) = argument_slots(input, imports, control)? else {
                    return Ok(None);
                };
                slots
            }
            None => Vec::new(),
        };
        let needed = if original_keys.is_empty()
            && summary
                .aggregate_specs()
                .eq([(GraphAggregateFunction::CountRows, None)])
            && let Some(join) = source_join
            && join.predicate().is_none()
            && let Some(SummaryStage::Project(_, cells, GraphSetQuantifier::Distinct)) =
                stages.first()
        {
            // A simple first DISTINCT projection discards every imported cell
            // it neither copies nor joins on. COUNT subqueries implicitly
            // capture the whole outer scope, including unrelated list values.
            charge(slots.len(), control)?;
            let mut needed = vec![false; slots.len()];
            for &(column, _) in join.keys() {
                control(GlaExecutionEvent::Work)?;
                needed[column] = true;
            }
            for cell in *cells {
                control(GlaExecutionEvent::Work)?;
                match cell.value() {
                    GraphSetValue::Column(column) => {
                        if let Some(live) = needed.get_mut(*column) {
                            *live = true;
                        }
                    }
                    GraphSetValue::Literal(_)
                    | GraphSetValue::Value(_)
                    | GraphSetValue::TypedNull(_) => {}
                    _ => {
                        needed.fill(true);
                        break;
                    }
                }
            }
            Some(needed)
        } else {
            None
        };
        charge(slots.len() * 2, control)?;
        let mut key_slots = Vec::with_capacity(slots.len());
        let mut values = Vec::with_capacity(slots.len());
        for (column, &slot) in slots.iter().enumerate() {
            if needed.as_ref().is_none_or(|needed| needed[column]) {
                key_slots.push(slot);
                values.push(GraphSetValue::Column(slot));
            } else {
                // Keep the checked frame width and type until the projection.
                values.push(GraphSetValue::TypedNull(left.types[slot]));
            }
        }
        let restored = restored_columns(stages.iter().chain(&tail), &mut key_slots, control)?;
        let Some(outer) = distinct::keyed_outer(&left.types, &key_slots, control)? else {
            return Ok(None);
        };
        let left_width = left.types.len();
        let argument_width = slots.len();
        let key_width = outer.column_types().len() - left_width;
        let private_width = key_width + restored.len();
        let right_width = right.types.len();
        charge(private_width, control)?;
        values.extend((left_width..left_width + key_width).map(GraphSetValue::Column));
        values.extend(restored.iter().copied().map(GraphSetValue::Column));
        let Some(arguments) = distinct::projection(
            outer.column_types(),
            values,
            GraphSetQuantifier::Distinct,
            control,
        )?
        else {
            return Ok(None);
        };
        let joined = if let Some(spec) = source_join
            && !spec.keys().is_empty()
        {
            RowJoinSpec::new(arguments.column_types(), &right.types, spec.keys())
        } else {
            RowJoinSpec::cross(arguments.column_types(), &right.types)
        };
        let Ok(mut join) = joined else {
            return Ok(None);
        };
        if let Some(code) = source_join.and_then(RowJoinSpec::predicate) {
            charge(argument_width + right_width, control)?;
            let positions: Vec<_> = (0..argument_width)
                .chain((0..right_width).map(|column| argument_width + private_width + column))
                .collect();
            let code = rebound(code, &positions, control)?;
            let Ok(selected) = join.with_predicate(&code) else {
                return Ok(None);
            };
            join = selected;
        }
        // Keep original body positions unchanged; append invocation identity
        // after the complete argument/source frame and after each projection.
        charge(argument_width + right_width + private_width, control)?;
        let values = (0..argument_width)
            .chain(argument_width + private_width..argument_width + private_width + right_width)
            .chain(argument_width..argument_width + private_width)
            .map(GraphSetValue::Column)
            .collect();
        let input_types: Vec<_> = join.column_types().collect();
        let Some(frame) = project(&input_types, values, control)? else {
            return Ok(None);
        };
        let Some((body, collection_order)) =
            summary_steps(frame, stages, &restored, key_width, control)?
        else {
            return Ok(None);
        };
        let collection_width = collection_order.as_ref().map_or(0, |order| order.width);
        let width = body.last().expect("body frame").column_types().len() - private_width;
        // Grouping retains the actual complete relational input. The host
        // builds this same shared-source circuit, not a same-schema surrogate.
        charge(left.types.len() + right.types.len(), control)?;
        let Some(active) = (**left).clone().project_incremental_spec(&outer) else {
            return Ok(None);
        };
        let Some(active) = active.project_incremental_spec(&arguments) else {
            return Ok(None);
        };
        let Ok(mut active) = active.join(right.as_ref().clone(), join.clone()) else {
            return Ok(None);
        };
        for step in &body {
            charge(
                step.input_types().len() + step.column_types().len(),
                control,
            )?;
            let Some(next) = active.project_incremental_spec(step) else {
                return Ok(None);
            };
            active = next;
        }
        if let Some(order) = &collection_order
            && !order.order.is_empty()
        {
            charge(order.order.len(), control)?;
            let Ok(ordered) = active.with_order_by(order.order) else {
                return Ok(None);
            };
            active = ordered;
        }
        // Build names before the borrowed aggregate descriptors. Every output
        // is retained in declaration order, including otherwise unused values.
        let mut aggregate_names = Vec::new();
        for (function, _) in summary.aggregate_specs() {
            let prefix = if matches!(
                function,
                GraphAggregateFunction::CountRows
                    | GraphAggregateFunction::Count
                    | GraphAggregateFunction::CountDistinct
            ) {
                "__fg_count"
            } else {
                "__fg_aggregate"
            };
            let occupied = active.columns.len() + aggregate_names.len();
            let mut name = None;
            for suffix in 0..=occupied {
                charge(1 + occupied, control)?;
                let candidate = format!("{prefix}_{suffix}");
                if !active.columns.contains(&candidate) && !aggregate_names.contains(&candidate) {
                    name = Some(candidate);
                    break;
                }
            }
            let Some(name) = name else {
                return Ok(None);
            };
            aggregate_names.push(name);
        }
        let mut aggregates = Vec::new();
        let mut collects = false;
        for ((function, argument), name) in summary.aggregate_specs().zip(&aggregate_names) {
            charge(1, control)?;
            let argument = argument.map(|column| collection_width + column);
            let name = name.as_str();
            let aggregate = match (function, argument) {
                (GraphAggregateFunction::CountRows, None) => GraphAggregate::count_rows(name),
                (GraphAggregateFunction::Count, Some(column)) => {
                    GraphAggregate::count(name, column)
                }
                (GraphAggregateFunction::CountDistinct, Some(column)) => {
                    GraphAggregate::count_distinct(name, column)
                }
                (GraphAggregateFunction::SumInt, Some(column)) => {
                    GraphAggregate::sum_int(name, column)
                }
                (GraphAggregateFunction::SumIntDistinct, Some(column)) => {
                    GraphAggregate::sum_int_distinct(name, column)
                }
                (GraphAggregateFunction::AverageInt, Some(column)) => {
                    GraphAggregate::average_int(name, column)
                }
                (GraphAggregateFunction::AverageIntDistinct, Some(column)) => {
                    GraphAggregate::average_int_distinct(name, column)
                }
                (GraphAggregateFunction::Min, Some(column)) => GraphAggregate::min(name, column),
                (GraphAggregateFunction::Max, Some(column)) => GraphAggregate::max(name, column),
                (GraphAggregateFunction::Collect, Some(column)) => {
                    collects = true;
                    GraphAggregate::collect(name, column)
                }
                (GraphAggregateFunction::CollectDistinct, Some(column)) => {
                    collects = true;
                    GraphAggregate::collect_distinct(name, column)
                }
                _ => return Ok(None),
            };
            aggregates.push(aggregate);
        }
        // Canonical inputs, including a retained DISTINCT row, still require
        // a canonical reconstructed relation. An explicit comparator instead
        // ranks the retained row and uses its complete tuple to break ties.
        if collects
            && collection_order
                .as_ref()
                .is_none_or(|order| order.order.is_empty())
            && !active
                .incremental_result_order()
                .is_some_and(|(order, _)| order.is_empty())
        {
            return Ok(None);
        }
        // Invocation identity partitions each original group independently.
        // Its columns live in the private suffix, so they cannot alias a user
        // grouping position even when that position copies an imported value.
        charge(key_width + grouped_width, control)?;
        let keys: Vec<_> = (width..width + key_width)
            .chain(original_keys.iter().map(|column| collection_width + column))
            .collect();
        let Ok(grouped) = active.group_by(&keys, &aggregates) else {
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
        // Only an empty GLOBAL aggregate has one row for each active invocation.
        // A grouped aggregate's last retraction removes its group. Complete
        // globally with Left, or retain only existing groups with Inner, before
        // post-group filters. Optional Apply's later restoration owns NULL
        // extension; neither empty groups nor filtered summaries are zero-filled
        // there or evaluated again on fabricated rows.
        // Retain raw values only for explicit argument reattachment. Other
        // signatures, including repeated imports, need only their join key.
        charge(private_width, control)?;
        let values = (argument_width..argument_width + private_width)
            .map(GraphSetValue::Column)
            .collect();
        let Some(invocations) = project(arguments.column_types(), values, control)? else {
            return Ok(None);
        };
        charge(key_width + private_width + group_types.len(), control)?;
        let keys: Vec<_> = (0..key_width).map(|key| (key, key)).collect();
        let Ok(complete) = RowJoinSpec::new(invocations.column_types(), &group_types, &keys) else {
            return Ok(None);
        };
        let complete = complete.with_kind(if grouped_width == 0 {
            RowJoinKind::Left
        } else {
            RowJoinKind::Inner
        });
        let group_start = private_width + key_width;
        let aggregate_start = group_start + grouped_width;
        let mut values = Vec::new();
        charge(grouped_width, control)?;
        values.extend((group_start..aggregate_start).map(GraphSetValue::Column));
        for (column, (function, _)) in summary.aggregate_specs().enumerate() {
            charge(4, control)?;
            let aggregate_column = aggregate_start + column;
            let restored = if grouped_width == 0
                && matches!(
                    function,
                    GraphAggregateFunction::CountRows
                        | GraphAggregateFunction::Count
                        | GraphAggregateFunction::CountDistinct
                ) {
                let Ok(zero_filled) = GraphIntegerExpression::prepare(&[
                    GraphIntegerOp::Column(aggregate_column),
                    GraphIntegerOp::Literal(Some(0)),
                    GraphIntegerOp::Coalesce,
                ]) else {
                    return Ok(None);
                };
                GraphSetValue::Integer(zero_filled)
            } else {
                // Preserve the native NULL, Float average, or typed extremum.
                // All group outputs are checked before restoring any of them.
                GraphSetValue::Column(aggregate_column)
            };
            values.push(restored);
        }
        charge(private_width, control)?;
        values.extend((0..private_width).map(GraphSetValue::Column));
        let types: Vec<_> = complete.column_types().collect();
        let Some(mut completed) = project(&types, values, control)? else {
            return Ok(None);
        };
        if grouped_width == 0 && collects {
            for (column, (function, _)) in summary.aggregate_specs().enumerate() {
                control(GlaExecutionEvent::Work)?;
                if matches!(
                    function,
                    GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct
                ) {
                    // A real collection result is never NULL. Only the absent
                    // global group needs completion, before the summary tail.
                    // The checked same-domain default retains List instead of
                    // widening it to Any through a general CASE expression.
                    charge(4, control)?;
                    let Ok(filled) =
                        completed.with_null_default(column, GraphValue::List(Box::new([])))
                    else {
                        return Ok(None);
                    };
                    completed = filled;
                }
            }
        }
        let Some((post_group, None)) =
            summary_steps(completed, tail, &restored, key_width, control)?
        else {
            return Ok(None);
        };
        let types = post_group.last().expect("completed group").column_types();
        let width = types.len() - private_width;
        if types.get(..width) != Some(body_query.types.as_slice()) {
            return Ok(None);
        }
        let window = if let Some(terminal) = terminal {
            let Some((order, _)) = terminal.incremental_result_order() else {
                return Ok(None);
            };
            charge(types.len() + order.len() + key_width, control)?;
            let Ok(window) = RowWindowSpec::new(
                types.to_vec(),
                order.to_vec(),
                GraphSetQuantifier::All,
                terminal.offset,
                terminal.count.expect("terminal page is finite"),
            )
            .and_then(|window| window.with_partition_columns((width..width + key_width).collect())) else {
                return Ok(None);
            };
            Some(window)
        } else {
            None
        };
        // All tail stages ran once per invocation. Inner restoration drops
        // filtered summaries; Optional restoration null-extends them without
        // evaluating a fabricated summary. Both preserve duplicate outer rows.
        charge(
            key_width + outer.column_types().len() + types.len(),
            control,
        )?;
        let keys: Vec<_> = (0..key_width)
            .map(|key| (left_width + key, width + key))
            .collect();
        let Ok(restore) = RowJoinSpec::new(outer.column_types(), types, &keys) else {
            return Ok(None);
        };
        let restore = restore.with_kind(if *kind == ApplyKind::Optional {
            RowJoinKind::Left
        } else {
            RowJoinKind::Inner
        });
        charge(left_width + width, control)?;
        let values = (0..left_width)
            .chain(outer.column_types().len()..outer.column_types().len() + width)
            .map(GraphSetValue::Column)
            .collect();
        let types: Vec<_> = restore.column_types().collect();
        let Some(public) = project(&types, values, control)? else {
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
        Ok(Some(MaintainedFilteredAggregate {
            left,
            right,
            outer,
            arguments,
            join,
            body,
            group: *group,
            group_columns,
            group_types,
            invocations,
            complete,
            summary: post_group,
            window,
            restore,
            output: vec![public, named],
        }))
    }

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
        let Some(base) = body(summary.input(), imports.is_empty(), &mut stages, control)? else {
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
