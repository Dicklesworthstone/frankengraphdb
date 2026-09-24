//! Depth-first, resumable indexed joins in the existing edge cursor.
//! The complete traversal-identity output prefix proves canonical whole-row
//! order and uniqueness. Each level retains only its last EId and chosen edge;
//! no prefix's neighbor bag or complete match table is ever materialized.

use super::*;
use crate::algebra::{MAX_PATTERN_BINDINGS, MAX_PATTERN_EDGES};

mod probe;
pub(crate) use probe::Probe;

enum Instruction {
    Native(GlaOperator),
    Probe(probe::Probe),
}

#[derive(Clone, Copy)]
struct Expansion {
    source: usize,
    relation: RelationId,
    direction: GlaDirection,
}

pub(super) struct JoinPlan {
    expansions: Vec<Expansion>,
    stages: Vec<Vec<Instruction>>,
    // Aggregate input is private to EdgeAggregatePlan. It retains the same
    // binding/projection semantics but emits no externally delivered rows.
    emit_rows: bool,
}

// Private GLA construction is still the logical authority. Audit every operand
// here before accepting a physical execution profile, including LIMIT zero.
pub(super) fn compile(plan: &GlaPlan<GraphValueRow>) -> Result<EdgeScanPlan, EdgeScanBuildError> {
    compile_output(plan, true)
}

pub(super) fn compile_aggregate(
    plan: &GlaPlan<GraphValueRow>,
) -> Result<EdgeScanPlan, EdgeScanBuildError> {
    compile_output(plan, false)
}

fn compile_output(
    plan: &GlaPlan<GraphValueRow>,
    emit_rows: bool,
) -> Result<EdgeScanPlan, EdgeScanBuildError> {
    let ops = plan.operators();
    let Some(GlaOperator::ScanEdges {
        relation,
        direction,
    }) = ops.first()
    else {
        return Err(EdgeScanBuildError { operator: 0 });
    };
    let mut width = 2_usize;
    let mut expansions = Vec::new();
    let mut stages = vec![Vec::new()];
    let mut captures: Vec<Vec<usize>> = Vec::new();
    let mut terminal = false;
    let mut at = 1;
    loop {
        let bad = || EdgeScanBuildError { operator: at };
        let Some(op) = ops.get(at) else {
            return Err(bad());
        };
        match op {
            GlaOperator::Probe { .. } => {
                let (probe, end) = probe::Probe::compile(ops, at, width)?;
                stages
                    .last_mut()
                    .expect("root stage")
                    .push(Instruction::Probe(probe));
                terminal = true;
                at = end + 1;
                continue;
            }
            GlaOperator::Expand {
                source,
                relation,
                direction,
            } if !terminal
                && (source.ordinal() as usize) < width
                && width < MAX_PATTERN_BINDINGS
                && expansions.len() + 1 < MAX_PATTERN_EDGES =>
            {
                expansions.push(Expansion {
                    source: source.ordinal() as usize,
                    relation: *relation,
                    direction: *direction,
                });
                width += 1;
                stages.push(Vec::new());
                at += 1;
                continue;
            }
            GlaOperator::Select { slot, .. } if (slot.ordinal() as usize) < width => {}
            GlaOperator::VertexIdentity { left, right, .. }
                if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width => {}
            GlaOperator::CapturePath {
                capture,
                start,
                segments,
            } if *capture as usize == captures.len()
                && (start.ordinal() as usize) < width
                && !segments.is_empty()
                && segments.len() <= MAX_PATTERN_EDGES
                && segments
                    .iter()
                    .all(|slot| slot.ordinal() > 0 && (slot.ordinal() as usize) < width) =>
            {
                terminal = true;
                captures.push(
                    segments
                        .iter()
                        .map(|slot| slot.ordinal() as usize - 1)
                        .collect(),
                );
            }
            GlaOperator::SelectBoolean { expression } => {
                terminal = true;
                let mut vertices_valid = true;
                let mut edges_valid = true;
                let _ = expression.remap_elements(
                    |slot| {
                        vertices_valid &= (slot.ordinal() as usize) < width;
                        slot
                    },
                    |capture| {
                        edges_valid &= captures
                            .get(capture as usize)
                            .is_some_and(|steps| steps.len() == 1);
                        capture
                    },
                );
                if !vertices_valid || !edges_valid {
                    return Err(bad());
                }
            }
            GlaOperator::CompareProperties { left, right, .. }
                if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width =>
            {
                terminal = true;
            }
            GlaOperator::ProjectValues { columns } => {
                let edge_column = |column: Option<&ValueProjection>, step: usize| {
                    matches!(column, Some(ValueProjection::Path { capture, function: GraphPathFunction::Edge })
                        if captures.get(*capture as usize).is_some_and(|parts| parts.len() == 1 && parts[0] == step))
                };
                if emit_rows
                    && (!edge_column(columns.first(), 0)
                        || !matches!(columns.get(1), Some(ValueProjection::Vertex { slot }) if slot.ordinal() == 0)
                        || !(1..width - 1).all(|step| edge_column(columns.get(step + 1), step)))
                {
                    return Err(bad());
                }
                for column in columns {
                    let valid = match column {
                        ValueProjection::Vertex { slot }
                        | ValueProjection::Property { slot, .. } => {
                            (slot.ordinal() as usize) < width
                        }
                        ValueProjection::EdgeProperty { capture, .. } => captures
                            .get(*capture as usize)
                            .is_some_and(|s| s.len() == 1),
                        ValueProjection::Path { capture, function } => captures
                            .get(*capture as usize)
                            .is_some_and(|s| match function {
                                GraphPathFunction::Edge => s.len() == 1,
                                GraphPathFunction::Value
                                | GraphPathFunction::Length
                                | GraphPathFunction::Nodes
                                | GraphPathFunction::Edges => true,
                                _ => false,
                            }),
                        _ => false,
                    };
                    if !valid {
                        return Err(bad());
                    }
                }
                break;
            }
            _ => return Err(bad()),
        }
        stages
            .last_mut()
            .expect("root stage")
            .push(Instruction::Native(op.clone()));
        at += 1;
    }
    let projection = Arc::new(ops[at].clone());
    at += 1;
    if matches!(ops.get(at), Some(GlaOperator::Distinct)) {
        // A numeric reducer consumes occurrences, not projected support.
        // Its private unordered input cannot implement DISTINCT by omission.
        if !emit_rows {
            return Err(EdgeScanBuildError { operator: at });
        }
        at += 1;
    }
    if plan.visible_columns.is_some() || !matches!(ops.get(at), Some(GlaOperator::OrderByValues)) {
        return Err(EdgeScanBuildError { operator: at });
    }
    at += 1;
    let Some(GlaOperator::Limit { offset, count }) = ops.get(at) else {
        return Err(EdgeScanBuildError { operator: at });
    };
    if !emit_rows && (*offset != 0 || count.is_some()) {
        return Err(EdgeScanBuildError { operator: at });
    }
    if at + 1 != ops.len() {
        return Err(EdgeScanBuildError { operator: at + 1 });
    }
    Ok(EdgeScanPlan {
        relation: *relation,
        direction: *direction,
        instructions: Arc::from([]),
        projection,
        offset: *offset,
        count: *count,
        joined: Some(Arc::new(JoinPlan {
            expansions,
            stages,
            emit_rows,
        })),
    })
}

#[derive(Clone, Copy)]
struct Choice {
    eid: EId,
    target: VId,
}

pub(super) struct Traversal {
    choices: Vec<Choice>,
    bindings: Vec<Option<VId>>,
    after: Vec<Option<EId>>,
    resume: bool,
}
impl Traversal {
    fn new<E>(
        hops: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        // Reserve every reusable depth slot BEFORE allocating the frame. Slots
        // are reused on backtracking; no row-dependent history collection grows.
        for _ in 0..hops * 4 + 1 {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        Ok(Self {
            choices: Vec::with_capacity(hops),
            bindings: Vec::with_capacity(hops + 1),
            after: vec![None; hops],
            resume: false,
        })
    }
    fn pop(&mut self) {
        self.choices.pop();
        if self.choices.is_empty() {
            self.bindings.clear();
        } else {
            self.bindings.pop();
        }
    }
}

pub(super) fn advance<S: EdgeScanSource, F: FnMut() -> Result<(), C>, C>(
    cursor: &mut EdgeScanCursor<S, F>,
) -> ScanResult<Option<GraphValueRow>, S::Error, C> {
    let plan = cursor.plan.joined.as_ref().expect("joined plan");
    let meter = &mut cursor.meter;
    meter.event(GlaExecutionEvent::Work)?;
    if cursor.plan.count == Some(0) {
        return Ok(None);
    }
    let source = cursor.source.as_mut().expect("open cursor owns its source");
    let hops = plan.expansions.len() + 1;
    if cursor.traversal.is_none() {
        cursor.traversal = Some(Traversal::new(hops, &mut |event| meter.event(event))?);
    }
    let traversal = cursor.traversal.as_mut().expect("reserved traversal");
    if traversal.resume {
        traversal.pop();
        traversal.resume = false;
    }
    loop {
        meter.event(GlaExecutionEvent::Work)?;
        let depth = traversal.choices.len();
        let (eid, from, to) = if depth == 0 {
            let (eid, second) = if let Some(eid) = cursor.reverse_pending.take() {
                (eid, true)
            } else {
                let Some(eid) = flatten(source.next_edge(&mut |event| meter.event(event)))? else {
                    return Ok(None);
                };
                meter.event(GlaExecutionEvent::Work)?;
                if cursor.last.is_some_and(|last| eid <= last) {
                    return Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity));
                }
                cursor.last = Some(eid);
                meter.rows.snapshot_records = meter.increment(
                    GqlBudgetDimension::SnapshotRecords,
                    meter.rows.snapshot_records,
                )?;
                (eid, false)
            };
            let Some(edge) = flatten(source.edge(eid, &mut |event| meter.event(event)))? else {
                continue;
            };
            if edge.relation != cursor.plan.relation {
                continue;
            }
            let (from, to) = match cursor.plan.direction {
                GlaDirection::Forward => (edge.source, edge.target),
                GlaDirection::Reverse => (edge.target, edge.source),
                GlaDirection::Undirected => {
                    let low = edge.source.min(edge.target);
                    let high = edge.source.max(edge.target);
                    if !second && low != high {
                        cursor.reverse_pending = Some(eid);
                    }
                    if second { (high, low) } else { (low, high) }
                }
            };
            (eid, from, to)
        } else {
            let expansion = plan.expansions[depth - 1];
            let from = traversal.bindings[expansion.source].expect("positive bound endpoint");
            let next = source.next_incident_edge_for_relation(
                from,
                expansion.relation,
                expansion.direction,
                traversal.after[depth],
                &mut |event| meter.event(event),
            );
            let next = match next {
                Ok(next) => next,
                Err(EdgeExpansionSourceError::Unavailable) => {
                    return Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable));
                }
                Err(EdgeExpansionSourceError::Read(error)) => flatten(Err(error))?,
            };
            let Some(eid) = next else {
                traversal.pop();
                continue;
            };
            meter.event(GlaExecutionEvent::Work)?;
            if traversal.after[depth].is_some_and(|prior| eid <= prior) {
                return Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity));
            }
            traversal.after[depth] = Some(eid);
            meter.rows.snapshot_records = meter.increment(
                GqlBudgetDimension::SnapshotRecords,
                meter.rows.snapshot_records,
            )?;
            let Some(edge) = flatten(source.edge(eid, &mut |event| meter.event(event)))? else {
                continue;
            };
            if edge.relation != expansion.relation {
                continue;
            }
            // Incidence indexes contain historical candidates, not necessarily
            // a currently visible incidence. Recheck without resurrecting it.
            let to = match expansion.direction {
                GlaDirection::Forward if edge.source == from => edge.target,
                GlaDirection::Reverse if edge.target == from => edge.source,
                GlaDirection::Undirected if edge.source == from => edge.target,
                GlaDirection::Undirected if edge.target == from => edge.source,
                _ => continue,
            };
            (eid, from, to)
        };
        vertex(source, from, &mut |event| meter.event(event))?;
        if from != to {
            vertex(source, to, &mut |event| meter.event(event))?;
        }
        if depth == 0 {
            traversal.bindings.push(Some(from));
        }
        traversal.choices.push(Choice { eid, target: to });
        traversal.bindings.push(Some(to));
        // Probe candidates share this very meter, but never count as output.
        // The two short-lived borrows cannot overlap: source callbacks return
        // before a candidate record is admitted or the next predicate runs.
        let paths = {
            let metered = std::cell::RefCell::new(&mut *meter);
            test_stage(
                &plan.stages[depth],
                traversal,
                source,
                &mut |event| metered.borrow_mut().event(event),
                &mut || {
                    let mut meter = metered.borrow_mut();
                    let count = meter.increment(
                        GqlBudgetDimension::SnapshotRecords,
                        meter.rows.snapshot_records,
                    )?;
                    meter.rows.snapshot_records = count;
                    Ok(())
                },
            )?
        };
        let Some(paths) = paths else {
            traversal.pop();
            continue;
        };
        if depth + 1 < hops {
            traversal.after[depth + 1] = None;
            continue;
        }
        if cursor.skip != 0 {
            cursor.skip -= 1;
            traversal.pop();
            continue;
        }
        let count = if plan.emit_rows {
            Some(meter.increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows)?)
        } else {
            None
        };
        let row = project(
            &cursor.plan.projection,
            &traversal.bindings,
            &paths,
            source,
            &mut |event| meter.event(event),
        )?;
        if let Some(count) = count {
            meter.event(GlaExecutionEvent::ResultRow)?;
            meter.rows.result_rows = count;
        } else {
            meter.event(GlaExecutionEvent::Work)?;
        }
        traversal.resume = true;
        return Ok(Some(row));
    }
}

fn vertex<'a, S: EdgeScanSource, C>(
    source: &'a S,
    vid: VId,
    control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
) -> ScanResult<VertexScanRecord<'a>, S::Error, C> {
    flatten(source.vertex_record(vid, control))?
        .ok_or(GqlQueryError::Source(EdgeScanError::DanglingEndpoint))
}
fn property<'a, S: EdgeScanSource, C>(
    source: &'a S,
    vid: VId,
    key: PropertyKeyId,
    control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
) -> ScanResult<Option<&'a CanonicalScalar>, S::Error, C> {
    flatten(source.vertex_property(vid, key, control))?
        .ok_or(GqlQueryError::Source(EdgeScanError::DanglingEndpoint))
}
fn edge_property<'a, S: EdgeScanSource, C>(
    source: &'a S,
    eid: EId,
    key: PropertyKeyId,
    control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
) -> ScanResult<Option<&'a CanonicalScalar>, S::Error, C> {
    let edge = flatten(source.edge(eid, control))?
        .ok_or(GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable))?;
    seek(edge.properties, &key, |entry| entry.0, control).map(|row| row.map(|(_, value)| value))
}

fn test_stage<S: EdgeScanSource, C>(
    ops: &[Instruction],
    traversal: &Traversal,
    source: &S,
    control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
    record: &mut impl FnMut() -> ScanResult<(), S::Error, C>,
) -> ScanResult<Option<Vec<Option<GraphPath>>>, S::Error, C> {
    let ids = &traversal.bindings;
    let mut paths = Vec::new();
    for op in ops {
        control(GlaExecutionEvent::Work)?;
        match op {
            Instruction::Probe(probe) => {
                if !probe.accepts(ids, source, control, record)? {
                    return Ok(None);
                }
            }
            Instruction::Native(GlaOperator::CapturePath {
                start, segments, ..
            }) => {
                control(GlaExecutionEvent::ScratchEntry)?;
                for _ in segments {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                let steps = segments
                    .iter()
                    .map(|slot| {
                        let choice = traversal.choices[slot.ordinal() as usize - 1];
                        (choice.eid, choice.target)
                    })
                    .collect::<Vec<_>>();
                paths.push(Some(GraphPath::new(
                    ids[start.ordinal() as usize].expect("captured start"),
                    steps.into_boxed_slice(),
                )));
            }
            Instruction::Native(op) => {
                if !accepts(op, ids, &paths, source, control)? {
                    return Ok(None);
                }
            }
        }
    }
    Ok(Some(paths))
}

// Shared by outer stages and probe-local stages. In particular, NOT EXISTS
// negates existence of a TRUE witness, not the Boolean value of a nullable
// property comparison. The canonical three-valued evaluator remains in charge.
fn accepts<S: EdgeScanSource, C>(
    op: &GlaOperator,
    ids: &[Option<VId>],
    paths: &[Option<GraphPath>],
    source: &S,
    control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
) -> ScanResult<bool, S::Error, C> {
    match op {
        GlaOperator::Select { slot, predicates } => {
            let Some(vid) = ids[slot.ordinal() as usize] else {
                return Ok(false);
            };
            let record = vertex(source, vid, control)?;
            let row = record.as_row();
            for predicate in predicates {
                control(GlaExecutionEvent::Work)?;
                for _ in 0..predicate.comparison_work_units() {
                    control(GlaExecutionEvent::Work)?;
                }
                let (label, property) = match predicate {
                    VertexPredicate::HasLabel(key) => {
                        (seek(row.labels, key, |id| *id, control)?.copied(), None)
                    }
                    _ => {
                        let key = predicate.property_key().expect("property predicate");
                        (
                            None,
                            seek(row.properties, &key, |entry| entry.0, control)?
                                .map(|(key, value)| (*key, value)),
                        )
                    }
                };
                if !predicate.matches_borrowed(label, property) {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        GlaOperator::VertexIdentity { left, right, equal } => {
            let (Some(left), Some(right)) =
                (ids[left.ordinal() as usize], ids[right.ordinal() as usize])
            else {
                return Ok(false);
            };
            Ok((left == right) == *equal)
        }
        GlaOperator::SelectBoolean { .. } | GlaOperator::CompareProperties { .. } => {
            let meter = std::cell::RefCell::new(control);
            compare_element_properties(
                op,
                ids,
                paths,
                &mut |vid, key| property(source, vid, key, &mut **meter.borrow_mut()),
                &mut |eid, key| edge_property(source, eid, key, &mut **meter.borrow_mut()),
                &mut |event| (**meter.borrow_mut())(event),
            )
        }
        _ => unreachable!("checked fixed-hop predicate"),
    }
}
#[allow(clippy::too_many_arguments)]
fn collect_element_properties<'a, Row: GlaOutput, E>(
    operator: &GlaOperator,
    bindings: &[Option<VId>],
    paths: &[Option<GraphPath>],
    projected: &mut ProjectedRows<Row>,
    property: &mut impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    edge_property: &mut impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
    vertex_labels: &mut impl FnMut(VId) -> Result<Option<&'a [crate::algebra::GraphValue]>, E>,
    edge_type: &mut impl FnMut(EId) -> Result<Option<&'a CanonicalScalar>, E>,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), E> {
    Row::collect_element_properties(
        operator,
        bindings,
        paths,
        projected,
        property,
        edge_property,
        vertex_labels,
        edge_type,
        control,
    )
}

fn project<S: EdgeScanSource, C>(
    projection: &GlaOperator,
    ids: &[Option<VId>],
    paths: &[Option<GraphPath>],
    source: &S,
    control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
) -> ScanResult<GraphValueRow, S::Error, C> {
    let meter = std::cell::RefCell::new(control);
    let mut rows = ProjectedRows::new(true);
    collect_element_properties(
        projection,
        ids,
        paths,
        &mut rows,
        &mut |vid, key| property(source, vid, key, &mut **meter.borrow_mut()),
        &mut |eid, key| edge_property(source, eid, key, &mut **meter.borrow_mut()),
        &mut |_| unreachable!("catalog labels outside the checked profile"),
        &mut |_| unreachable!("catalog types outside the checked profile"),
        &mut |event| (**meter.borrow_mut())(event),
    )?;
    let mut rows = rows.into_rows();
    let row = rows.next().expect("one complete binding projects one row");
    debug_assert!(rows.next().is_none());
    Ok(row)
}

#[cfg(test)]
mod tests;
