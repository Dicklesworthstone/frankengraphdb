//! Ordered, pull-driven execution of identified fixed-edge GLA patterns.
//!
//! The leading edge identity and source vertex prove whole-row order and
//! uniqueness, including both orientations of an undirected non-self edge.
//! No adjacency/result bag, sorting, DISTINCT set or unread suffix is built.
//! This is a physical specialization of compiler-owned GLA, not a text parser.

pub mod aggregate;
mod join;
pub(crate) use join::Probe;

use crate::algebra::{
    GlaDirection, GlaOperator, GlaOutput, GlaPlan, GraphPath, GraphPathFunction, GraphValueRow,
    ValueProjection, VertexPredicate,
};
use crate::algebra_exec::{ProjectedRows, compare_element_properties};
use crate::{
    GlaExecutionEvent, GlaExecutionStats, GqlBudgetDimension, GqlExecutionStats, GqlQueryError,
    GqlQueryPolicy,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};
use std::sync::Arc;

pub use crate::stream::{VertexScanRecord, VertexScanRow, VertexScanSourceError as EdgeScanSourceError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EdgeScanBuildError {
    /// First instruction outside this physical profile; never a data identity.
    pub operator: usize,
}
impl core::fmt::Display for EdgeScanBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "operator {} is outside the ordered edge-stream profile",
            self.operator
        )
    }
}
impl core::error::Error for EdgeScanBuildError {}

/// Checked fixed-edge plan. For multiple connected edges, the output prefix
/// is root edge, root source vertex, then every appended edge in GLA traversal
/// order. This covers chains, branches and identity-constrained cycle closures;
/// other output shapes and unsupported operators fail physical preparation.
///
/// For one edge: The output must begin with the captured edge identity
/// followed by slot zero's vertex identity. Remaining columns may contain either
/// endpoint, their properties, edge properties or this one-edge path/functions.
/// This prefix permits DISTINCT and ALL without storing a seen-set. Other order,
/// hidden sort columns, optional/variable-length expansions, and catalog-name output
/// refuse before a source is driven. Boolean filters use the ordinary engine.
/// Correlated and independent fixed-hop EXISTS/NOT EXISTS use indexed,
/// short-circuit probes; nested/optional probe bodies remain unavailable.
#[derive(Clone)]
pub struct EdgeScanPlan {
    relation: RelationId,
    direction: GlaDirection,
    instructions: Arc<[GlaOperator]>,
    projection: Arc<GlaOperator>,
    offset: u64,
    count: Option<u64>,
    joined: Option<Arc<join::JoinPlan>>,
}
impl EdgeScanPlan {
    pub fn compile(plan: &GlaPlan<GraphValueRow>) -> Result<Self, EdgeScanBuildError> {
        let ops = plan.operators();
        if ops
            .iter()
            .any(|op| matches!(op, GlaOperator::Expand { .. } | GlaOperator::Probe { .. }))
        {
            return join::compile(plan);
        }
        let Some(GlaOperator::ScanEdges {
            relation,
            direction,
        }) = ops.first()
        else {
            return Err(EdgeScanBuildError { operator: 0 });
        };
        let mut at = 1;
        let mut captures = 0_usize;
        let mut instructions = Vec::new();
        loop {
            let bad = || EdgeScanBuildError { operator: at };
            let Some(op) = ops.get(at) else {
                return Err(bad());
            };
            match op {
                GlaOperator::Select { slot, .. } if slot.ordinal() < 2 => {}
                GlaOperator::VertexIdentity { left, right, .. }
                    if left.ordinal() < 2 && right.ordinal() < 2 => {}
                GlaOperator::CapturePath {
                    capture,
                    start,
                    segments,
                } if *capture as usize == captures
                    && start.ordinal() == 0
                    && segments.len() == 1
                    && segments[0].ordinal() == 1 =>
                {
                    captures += 1;
                }
                GlaOperator::SelectBoolean { expression } => {
                    let mut vertices_valid = true;
                    let mut captures_valid = true;
                    let _ = expression.remap_elements(
                        |slot| {
                            vertices_valid &= slot.ordinal() < 2;
                            slot
                        },
                        |capture| {
                            captures_valid &= (capture as usize) < captures;
                            capture
                        },
                    );
                    if !vertices_valid || !captures_valid {
                        return Err(bad());
                    }
                }
                GlaOperator::ProjectValues { columns } => {
                    let column_valid = |column: &ValueProjection| match column {
                        ValueProjection::Vertex { slot }
                        | ValueProjection::Property { slot, .. } => slot.ordinal() < 2,
                        ValueProjection::EdgeProperty { capture, .. } => {
                            (*capture as usize) < captures
                        }
                        ValueProjection::Path { capture, function } => {
                            (*capture as usize) < captures
                                && matches!(
                                    function,
                                    GraphPathFunction::Value
                                        | GraphPathFunction::Length
                                        | GraphPathFunction::Nodes
                                        | GraphPathFunction::Edges
                                        | GraphPathFunction::Edge
                                )
                        }
                        _ => false,
                    };
                    if !matches!(
                        columns.first(),
                        Some(ValueProjection::Path {
                            function: GraphPathFunction::Edge,
                            ..
                        })
                    ) || !matches!(columns.get(1), Some(ValueProjection::Vertex { slot }) if slot.ordinal() == 0)
                        || !columns.iter().all(column_valid)
                    {
                        return Err(bad());
                    }
                    break;
                }
                _ => return Err(bad()),
            }
            instructions.push(op.clone());
            at += 1;
        }
        let projection = Arc::new(ops[at].clone());
        at += 1;
        if matches!(ops.get(at), Some(GlaOperator::Distinct)) {
            at += 1;
        }
        if plan.visible_columns.is_some()
            || !matches!(ops.get(at), Some(GlaOperator::OrderByValues))
        {
            return Err(EdgeScanBuildError { operator: at });
        }
        at += 1;
        let Some(GlaOperator::Limit { offset, count }) = ops.get(at) else {
            return Err(EdgeScanBuildError { operator: at });
        };
        if at + 1 != ops.len() {
            return Err(EdgeScanBuildError { operator: at + 1 });
        }
        Ok(Self {
            relation: *relation,
            direction: *direction,
            instructions: instructions.into(),
            projection,
            offset: *offset,
            count: *count,
            joined: None,
        })
    }
}
impl core::fmt::Debug for EdgeScanPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("EdgeScanPlan([REDACTED])")
    }
}

/// One visible edge in its stored orientation. Fields are borrowed from one
/// admitted immutable generation. Properties have unique, sorted canonical keys.
#[derive(Clone, Copy)]
pub struct EdgeScanRow<'a> {
    pub source: VId,
    pub target: VId,
    pub relation: RelationId,
    pub properties: &'a [(PropertyKeyId, CanonicalScalar)],
}
impl core::fmt::Debug for EdgeScanRow<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("EdgeScanRow([REDACTED])")
    }
}

/// Immutable source seam, not an authorization grant. Candidate EIds must be
/// strictly increasing, with no omission or repetition. edge() resolves only
/// that identity at snapshot_seq(), including absence/retirement. vertex()
/// resolves endpoints at the SAME cut. Controls precede scalable source work
/// and scratch growth. Source failures and control refusals must stay distinct.
pub trait EdgeScanSource {
    type Error;
    fn snapshot_seq(&self) -> CommitSeq;
    fn next_edge<C>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>>;
    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>>;
    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>>;

    /// Endpoint/probe metadata may be owned after source masking. Indexed
    /// joins and probes keep the record only while testing this binding; they
    /// never retain a graph-wide metadata cache. The default borrows unchanged.
    /// A scoped source must enforce the same policy in vertex_property too.
    fn vertex_record<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRecord<'a>>, EdgeScanSourceError<Self::Error, C>> {
        self.vertex(vid, control)
            .map(|row| row.map(VertexScanRecord::Borrowed))
    }

    /// Borrow a source-admitted scalar without retaining a copied whole row.
    /// None is an absent vertex; Some(None) is an absent/masked property. The
    /// source's complete-record and field routes must share one immutable cut
    /// and policy. A source unable to lend masked fields must refuse, never
    /// delegate to a less restrictive source. Defaults preserve borrowed reads.
    fn vertex_property<'a, C>(
        &'a self,
        vid: VId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<Option<&'a CanonicalScalar>>, EdgeScanSourceError<Self::Error, C>> {
        let Some(row) = self.vertex(vid, control)? else {
            return Ok(None);
        };
        seek(row.properties, &key, |entry| entry.0, control)
            .map(|entry| Some(entry.map(|(_, value)| value)))
            .map_err(EdgeScanSourceError::Control)
    }

    /// Independent probe scan at snapshot_seq(). Yield candidate VIds strictly
    /// after the caller-owned position; vertex_record() resolves visibility.
    /// Do not advance a root cursor, omit isolates, or allocate a candidate bag.
    /// Each probe/local scan has its own position; no result cache suppresses
    /// later fallible source reads. An unavailable index is NOT an empty graph.
    fn next_probe_vertex<C>(
        &self,
        _after: Option<VId>,
        _control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>> {
        Err(EdgeExpansionSourceError::Unavailable)
    }

    /// Strict successor in the chosen endpoint's incident EId histories. Each
    /// invocation resumes from `after`; separate nested bindings have separate
    /// positions. Historical membership may be a superset: the cursor rechecks
    /// visible topology/relation at the SAME cut. Never scan unrelated graph
    /// edges, collect an adjacency vector, or use an identity+1 sentinel here.
    /// Existing single-edge sources need not implement this optional ability:
    /// the default explicitly refuses a demanded expansion, never returns EOF.
    fn next_incident_edge<C>(
        &self,
        _endpoint: VId,
        _direction: GlaDirection,
        _after: Option<EId>,
        _control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        Err(EdgeExpansionSourceError::Unavailable)
    }

    /// Admit the requested relation before seeking a joined/probe incidence.
    /// Defaults retain the existing index and controls, with ordinary candidate
    /// relation/visibility rechecks afterward. A scoped source can reject the
    /// relation without opening the underlying directory. Unavailable allowed
    /// indexes must still refuse. The root edge scan has its own admission.
    fn next_incident_edge_for_relation<C>(
        &self,
        endpoint: VId,
        _relation: RelationId,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.next_incident_edge(endpoint, direction, after, control)
    }
}

/// An optional indexed lookup cannot silently become an empty search domain.
#[derive(Debug)]
pub enum EdgeExpansionSourceError<E, C> {
    Unavailable,
    Read(EdgeScanSourceError<E, C>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeScanState {
    Open,
    Exhausted,
    Closed,
    Failed,
}
#[derive(Debug)]
pub enum EdgeScanError<E> {
    Source(E),
    Plan(EdgeScanBuildError),
    NonIncreasingIdentity,
    /// An admitted visible edge has no visible endpoint; never a null binding.
    DanglingEndpoint,
    CounterExhausted,
    ExpansionUnavailable,
    BoundEdgeUnavailable,
}
impl<E: core::fmt::Display> core::fmt::Display for EdgeScanError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::Plan(error) => error.fmt(f),
            Self::NonIncreasingIdentity => {
                f.write_str("edge stream source is not strictly increasing")
            }
            Self::DanglingEndpoint => f.write_str("edge stream source has a dangling endpoint"),
            Self::CounterExhausted => f.write_str("edge stream counter exhausted"),
            Self::ExpansionUnavailable => {
                f.write_str("graph source lacks a required indexed lookup")
            }
            Self::BoundEdgeUnavailable => {
                f.write_str("a bound edge disappeared from the immutable source")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for EdgeScanError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(e) => Some(e),
            Self::Plan(e) => Some(e),
            _ => None,
        }
    }
}
type ScanResult<T, E, C> = Result<T, GqlQueryError<EdgeScanError<E>, C>>;
fn flatten<T, E, C>(
    r: Result<T, EdgeScanSourceError<E, GqlQueryError<EdgeScanError<E>, C>>>,
) -> ScanResult<T, E, C> {
    r.map_err(|e| match e {
        EdgeScanSourceError::Source(e) => GqlQueryError::Source(EdgeScanError::Source(e)),
        EdgeScanSourceError::Control(e) => e,
    })
}

struct Meter<F> {
    checkpoint: F,
    policy: GqlQueryPolicy,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
}
impl<F> Meter<F> {
    fn event<E, C>(&mut self, event: GlaExecutionEvent) -> ScanResult<(), E, C>
    where
        F: FnMut() -> Result<(), C>,
    {
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        self.evaluator
            .charge_event(self.policy.evaluator, event)
            .map_err(GqlQueryError::Evaluator)
    }
    fn increment<E, C>(&self, dimension: GqlBudgetDimension, prior: u64) -> ScanResult<u64, E, C> {
        let next = prior
            .checked_add(1)
            .ok_or(GqlQueryError::Source(EdgeScanError::CounterExhausted))?;
        self.policy
            .rows
            .check(dimension, next)
            .map_err(GqlQueryError::Rows)?;
        Ok(next)
    }
}

/// Streaming one-edge results with a cumulative source/work/scratch/output
/// allowance. SnapshotRecords counts examined candidate edge histories, not an
/// eager full-table admission or each orientation/endpoint. LIMIT 0 never reads
/// a candidate. Two orientations share one record charge. At most one projected
/// row is retained; a source may still own a full decoded immutable generation.
/// Errors are yielded once and fuse/drop the source; close/drop never drain it.
/// Connected fixed-hop joins extend this lane with depth-bounded resumable
/// adjacency positions, not per-prefix neighbor/result bags. Their record meter
/// counts edge and independent-probe vertex examinations at every join level
/// (including re-examinations under different outer bindings). This is not arbitrary-order joining,
/// variable-length traversal, spilling, a byte-memory cap or a durable cursor.
pub struct EdgeScanCursor<S, F> {
    source: Option<S>,
    plan: EdgeScanPlan,
    meter: Meter<F>,
    seq: CommitSeq,
    last: Option<EId>,
    reverse_pending: Option<EId>,
    skip: u64,
    state: EdgeScanState,
    traversal: Option<join::Traversal>,
}
impl<S: EdgeScanSource, F> EdgeScanCursor<S, F> {
    /// No candidate scan or checkpoint occurs during construction.
    pub fn new(source: S, plan: EdgeScanPlan, policy: GqlQueryPolicy, checkpoint: F) -> Self {
        Self {
            seq: source.snapshot_seq(),
            skip: plan.offset,
            source: Some(source),
            plan,
            meter: Meter {
                checkpoint,
                policy,
                rows: GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: 0,
                },
                evaluator: GlaExecutionStats::default(),
            },
            last: None,
            reverse_pending: None,
            state: EdgeScanState::Open,
            traversal: None,
        }
    }
    #[must_use]
    pub fn state(&self) -> EdgeScanState {
        self.state
    }
    #[must_use]
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.seq
    }
    #[must_use]
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.meter.rows
    }
    #[must_use]
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.meter.evaluator
    }
    pub fn close(&mut self) {
        if self.state == EdgeScanState::Open {
            self.state = EdgeScanState::Closed;
        }
        self.source = None;
        self.reverse_pending = None;
        self.traversal = None;
    }
    fn advance<C>(&mut self) -> ScanResult<Option<GraphValueRow>, S::Error, C>
    where
        F: FnMut() -> Result<(), C>,
    {
        if self.plan.joined.is_some() {
            return join::advance(self);
        }
        let meter = &mut self.meter;
        meter.event(GlaExecutionEvent::Work)?;
        if self.plan.count == Some(0) {
            return Ok(None);
        }
        let source = self
            .source
            .as_mut()
            .expect("open edge cursor owns its source");
        loop {
            let (eid, second) = if let Some(eid) = self.reverse_pending.take() {
                (eid, true)
            } else {
                let Some(eid) = flatten(source.next_edge(&mut |event| meter.event(event)))? else {
                    return Ok(None);
                };
                meter.event(GlaExecutionEvent::Work)?;
                if self.last.is_some_and(|last| eid <= last) {
                    return Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity));
                }
                self.last = Some(eid);
                meter.rows.snapshot_records = meter.increment(
                    GqlBudgetDimension::SnapshotRecords,
                    meter.rows.snapshot_records,
                )?;
                (eid, false)
            };
            let Some(edge) = flatten(source.edge(eid, &mut |event| meter.event(event)))? else {
                continue;
            };
            meter.event(GlaExecutionEvent::Work)?;
            if edge.relation != self.plan.relation {
                continue;
            }
            let (from, to) = match self.plan.direction {
                GlaDirection::Forward => (edge.source, edge.target),
                GlaDirection::Reverse => (edge.target, edge.source),
                GlaDirection::Undirected => {
                    let low = edge.source.min(edge.target);
                    let high = edge.source.max(edge.target);
                    if !second && low != high {
                        self.reverse_pending = Some(eid);
                    }
                    if second { (high, low) } else { (low, high) }
                }
            };
            let left = flatten(source.vertex(from, &mut |event| meter.event(event)))?
                .ok_or(GqlQueryError::Source(EdgeScanError::DanglingEndpoint))?;
            let right = if from == to {
                left
            } else {
                flatten(source.vertex(to, &mut |event| meter.event(event)))?
                    .ok_or(GqlQueryError::Source(EdgeScanError::DanglingEndpoint))?
            };
            let image = Binding {
                eid,
                ids: [from, to],
                vertices: [left, right],
                edge: edge.properties,
            };
            let Some(paths) = self.plan.test(&image, &mut |event| meter.event(event))? else {
                continue;
            };
            if self.skip != 0 {
                self.skip -= 1;
                continue;
            }
            // Check quota BEFORE copying projected payloads, and publish only
            // after the last fallible checkpoint has accepted the complete row.
            let next = meter.increment(GqlBudgetDimension::ResultRows, meter.rows.result_rows)?;
            let row = self
                .plan
                .project(&image, &paths, &mut |event| meter.event(event))?;
            meter.event(GlaExecutionEvent::ResultRow)?;
            meter.rows.result_rows = next;
            return Ok(Some(row));
        }
    }
}
impl<S: EdgeScanSource, F: FnMut() -> Result<(), C>, C> Iterator for EdgeScanCursor<S, F> {
    type Item = ScanResult<GraphValueRow, S::Error, C>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.state != EdgeScanState::Open {
            return None;
        }
        let item = match self.advance() {
            Ok(Some(row)) => {
                if self.plan.count == Some(self.meter.rows.result_rows) {
                    self.state = EdgeScanState::Exhausted;
                }
                Some(Ok(row))
            }
            Ok(None) => {
                self.state = EdgeScanState::Exhausted;
                None
            }
            Err(error) => {
                self.state = EdgeScanState::Failed;
                Some(Err(error))
            }
        };
        if self.state != EdgeScanState::Open {
            self.close();
        }
        item
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, (self.state != EdgeScanState::Open).then_some(0))
    }
}
impl<S: EdgeScanSource, F: FnMut() -> Result<(), C>, C> std::iter::FusedIterator
    for EdgeScanCursor<S, F>
{
}
impl<S, F> core::fmt::Debug for EdgeScanCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeScanCursor")
            .field("sequence", &self.seq)
            .field("state", &self.state)
            .field("rows", &self.meter.rows)
            .field("evaluator", &self.meter.evaluator)
            .field("source_and_plan", &"[REDACTED]")
            .finish()
    }
}

struct Binding<'a> {
    eid: EId,
    ids: [VId; 2],
    vertices: [VertexScanRow<'a>; 2],
    edge: &'a [(PropertyKeyId, CanonicalScalar)],
}
fn seek<'a, T, K: Ord, E>(
    values: &'a [T],
    wanted: &K,
    key: impl Fn(&T) -> K,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Option<&'a T>, E> {
    let (mut low, mut high) = (0, values.len());
    while low < high {
        control(GlaExecutionEvent::Work)?;
        let middle = low + (high - low) / 2;
        match key(&values[middle]).cmp(wanted) {
            core::cmp::Ordering::Less => low = middle + 1,
            core::cmp::Ordering::Greater => high = middle,
            core::cmp::Ordering::Equal => return Ok(Some(&values[middle])),
        }
    }
    Ok(None)
}
impl<'a> Binding<'a> {
    fn property<E>(
        &self,
        vid: VId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<&'a CanonicalScalar>, E> {
        let slot = if vid == self.ids[0] {
            0
        } else {
            debug_assert_eq!(vid, self.ids[1]);
            1
        };
        seek(
            self.vertices[slot].properties,
            &key,
            |entry| entry.0,
            control,
        )
        .map(|entry| entry.map(|(_, v)| v))
    }
    fn edge_property<E>(
        &self,
        eid: EId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<&'a CanonicalScalar>, E> {
        debug_assert_eq!(eid, self.eid);
        seek(self.edge, &key, |entry| entry.0, control).map(|entry| entry.map(|(_, v)| v))
    }
}
impl EdgeScanPlan {
    fn test<E>(
        &self,
        row: &Binding<'_>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<Vec<Option<GraphPath>>>, E> {
        let mut paths = Vec::new();
        for op in self.instructions.iter() {
            control(GlaExecutionEvent::Work)?;
            match op {
                GlaOperator::Select { slot, predicates } => {
                    let vertex = row.vertices[slot.ordinal() as usize];
                    for predicate in predicates {
                        control(GlaExecutionEvent::Work)?;
                        for _ in 0..predicate.comparison_work_units() {
                            control(GlaExecutionEvent::Work)?;
                        }
                        let (label, property) = match predicate {
                            VertexPredicate::HasLabel(wanted) => (
                                seek(vertex.labels, wanted, |id| *id, control)?.copied(),
                                None,
                            ),
                            _ => {
                                let key = predicate.property_key().expect("property predicate");
                                (
                                    None,
                                    seek(vertex.properties, &key, |entry| entry.0, control)?
                                        .map(|(k, v)| (*k, v)),
                                )
                            }
                        };
                        if !predicate.matches_borrowed(label, property) {
                            return Ok(None);
                        }
                    }
                }
                GlaOperator::VertexIdentity { left, right, equal } => {
                    if (row.ids[left.ordinal() as usize] == row.ids[right.ordinal() as usize])
                        != *equal
                    {
                        return Ok(None);
                    }
                }
                GlaOperator::CapturePath { .. } => {
                    // One retained capture, one edge identity and one endpoint.
                    for _ in 0..3 {
                        control(GlaExecutionEvent::ScratchEntry)?;
                    }
                    paths.push(Some(GraphPath::new(
                        row.ids[0],
                        vec![(row.eid, row.ids[1])].into_boxed_slice(),
                    )));
                }
                GlaOperator::SelectBoolean { .. } => {
                    let meter = std::cell::RefCell::new(&mut *control);
                    if !compare_element_properties(
                        op,
                        &[Some(row.ids[0]), Some(row.ids[1])],
                        &paths,
                        &mut |vid, key| row.property(vid, key, &mut **meter.borrow_mut()),
                        &mut |eid, key| row.edge_property(eid, key, &mut **meter.borrow_mut()),
                        &mut |event| (**meter.borrow_mut())(event),
                    )? {
                        return Ok(None);
                    }
                }
                _ => unreachable!("checked edge-stream instruction"),
            }
        }
        Ok(Some(paths))
    }
    fn project<E>(
        &self,
        row: &Binding<'_>,
        paths: &[Option<GraphPath>],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, E> {
        collect_one::<GraphValueRow, E>(&self.projection, row, paths, control)
    }
}
fn collect_one<Row: GlaOutput, E>(
    projection: &GlaOperator,
    row: &Binding<'_>,
    paths: &[Option<GraphPath>],
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Row, E> {
    let meter = std::cell::RefCell::new(control);
    let mut projected = ProjectedRows::new(true);
    Row::collect_element_properties(
        projection,
        &[Some(row.ids[0]), Some(row.ids[1])],
        paths,
        &mut projected,
        &mut |vid, key| row.property(vid, key, &mut **meter.borrow_mut()),
        &mut |eid, key| row.edge_property(eid, key, &mut **meter.borrow_mut()),
        &mut |_| unreachable!("catalog labels outside this profile"),
        &mut |_| unreachable!("catalog types outside this profile"),
        &mut |event| (**meter.borrow_mut())(event),
    )?;
    let mut rows = projected.into_rows();
    let row = rows.next().expect("one complete binding projects one row");
    debug_assert!(rows.next().is_none());
    Ok(row)
}

#[cfg(test)]
mod tests;
