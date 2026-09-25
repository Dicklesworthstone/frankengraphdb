//! Lazy root scans and probes over one scoped immutable history source.
//! Only the source boundary masks records; user predicates/projection stay in
//! the native physical operator. No source table or result set is collected.

use super::*;
use fgdb_gql::GlaExecutionEvent;
use fgdb_gql::algebra::{GlaOperator, PreparedGraphPattern};
use fgdb_gql::edge_stream::{EdgeExpansionSourceError, EdgeScanError, EdgeScanRow};
use fgdb_gql::stream::{
    VertexScanBuildError, VertexScanCursor, VertexScanError, VertexScanEvent, VertexScanPlan,
    VertexScanRecord, VertexScanRow, VertexScanSource, VertexScanSourceError, VertexScanState,
};
use fgdb_types::EId;
use std::iter::FusedIterator;
use std::rc::Rc;

#[path = "stream/aggregate.rs"]
mod aggregate;

type Shared<'q> = Rc<RefCell<Execution<'q, 'q, Box<dyn FnMut() -> u64 + 'q>>>>;
type Pull<'q, Row> = Box<dyn FnMut() -> Result<(Option<Row>, bool), QueryError> + 'q>;
type Opened<'q, Row, Metadata> = Result<(AuthorizedRowCursor<'q, Row>, Metadata), QueryError>;

// The pin slot is disjoint from the borrowed capability/clock. Arming before
// callbacks makes unwind close this session even if a caller catches the panic
// and tries to poll or explicitly close the still-borrowing cursor afterward.
struct PinGuard<'q> {
    pin: &'q mut Option<EmbeddedReadView>,
    armed: bool,
}
impl Drop for PinGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.pin.take();
        }
    }
}

/// A fused, capability-checked pull cursor over the session's pinned generation.
/// `next()` drives the actual source only until another accepted row. Closing
/// or dropping does not scan the suffix. Earlier successful pulls stay delivered
/// after a later error; that error occurs once and is never reported as EOF.
///
/// One signed allowance and one native meter span opening and all pulls (native
/// work/scratch begin at physical execution). Nodes count permitted vertices
/// examined before user predicates, including repeated endpoint/property
/// admissions inside probes; native snapshot records count candidate
/// histories, including invisible/forbidden candidates. Signed rows count only
/// delivered occurrences after SKIP/LIMIT. Private source counters are not
/// exported. At most one masked record and one projected row are needed in the
/// root path, outside the resident immutable generation and caller-owned rows.
/// Fixed probes add definition-bounded frames and temporary masked records;
/// finite variable-length probes retain governed traversal/support state.
/// The aggregate factory uses this same lifecycle with completed group rows;
/// its accumulation/support/collection storage is not bounded by the root-row
/// profile above. See stream_aggregate for its first-pull and storage contract.
///
/// Single-task by design: the cursor is `!Send`, because it shares its
/// session's live permit through `Rc<RefCell<..>>`, and authorized sessions do
/// not require a `Send` clock. Drive it on the task that opened it (a runtime's
/// `block_on` accepts it; a `Send`-only spawner does not).
///
/// This borrows the session and QueryCx until dropped, not the database writer,
/// token bytes or prepared template. Credential invalidation or a host callback
/// unwind closes the session; ordinary cursor errors do not widen its policy.
/// No restart token, durable lease, spill, or physical noninterference is claimed.
pub struct AuthorizedRowCursor<'q, Row = GraphValueRow> {
    driver: Option<Pull<'q, Row>>,
    guard: Option<PinGuard<'q>>,
    columns: Vec<String>,
    snapshot_seq: CommitSeq,
    state: VertexScanState,
}
impl<Row> AuthorizedRowCursor<'_, Row> {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.snapshot_seq
    }
    pub fn state(&self) -> VertexScanState {
        self.state
    }

    /// Release this cursor without draining it. A normal close preserves the
    /// session; a previously unwound poll keeps its terminal session fence.
    pub fn close(&mut self) {
        if self.state == VertexScanState::Open {
            self.state = VertexScanState::Closed;
            if let Some(guard) = &mut self.guard {
                guard.armed = false;
            }
        }
        self.driver.take();
        self.guard.take();
    }
}
impl<Row> Iterator for AuthorizedRowCursor<'_, Row> {
    type Item = Result<Row, QueryError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.state != VertexScanState::Open {
            return None;
        }
        self.state = VertexScanState::Failed;
        if let Some(guard) = &mut self.guard {
            guard.armed = true;
        }
        let mut driver = self
            .driver
            .take()
            .expect("open authorized cursor owns its driver");
        match driver() {
            Ok((value, finished)) => {
                if let Some(guard) = &mut self.guard {
                    guard.armed = false;
                }
                if finished || value.is_none() {
                    self.state = VertexScanState::Exhausted;
                    self.guard.take();
                } else {
                    self.state = VertexScanState::Open;
                    self.driver = Some(driver);
                }
                value.map(Ok)
            }
            Err(error) => {
                let result = Err(error);
                if let Some(guard) = &mut self.guard {
                    guard.armed = terminal(&result);
                }
                self.guard.take();
                Some(result)
            }
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, (self.state != VertexScanState::Open).then_some(0))
    }
}
impl<Row> FusedIterator for AuthorizedRowCursor<'_, Row> {}
impl<Row> core::fmt::Debug for AuthorizedRowCursor<'_, Row> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedRowCursor")
            .field("state", &self.state)
            .field("authority_source_and_position", &"[REDACTED]")
            .finish()
    }
}

struct ScopedSource<'q, S> {
    inner: S,
    execution: Shared<'q>,
}
// Every history lookup here runs under a poll-only control (FG-INV-20), whose
// own refusal/cancellation is carried as a source error because the executor's
// control type is opaque at this seam (scan_error reports both alike).
fn scoped_error<C>(
    error: VertexScanSourceError<ReadError, QueryError>,
) -> VertexScanSourceError<QueryError, C> {
    match error {
        VertexScanSourceError::Source(error) => {
            VertexScanSourceError::Source(QueryError::Read(error))
        }
        VertexScanSourceError::Control(error) => VertexScanSourceError::Source(error),
    }
}

fn scoped_expansion_error<C>(
    error: EdgeExpansionSourceError<ReadError, QueryError>,
) -> EdgeExpansionSourceError<QueryError, C> {
    match error {
        EdgeExpansionSourceError::Unavailable => EdgeExpansionSourceError::Unavailable,
        EdgeExpansionSourceError::Read(error) => {
            EdgeExpansionSourceError::Read(scoped_error(error))
        }
    }
}

fn edge_event(event: VertexScanEvent) -> GlaExecutionEvent {
    match event {
        VertexScanEvent::Work => GlaExecutionEvent::Work,
        VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
    }
}

impl<S: VertexScanSource<Error = ReadError>> ScopedSource<'_, S> {
    /// Whether this capability may see `vid` at the cut, decided with a
    /// poll-only lookup: skipping a candidate is never charged (FG-INV-20).
    fn visible<C>(&self, vid: VId) -> Result<bool, VertexScanSourceError<QueryError, C>> {
        let execution = Rc::clone(&self.execution);
        let mut poll = |_: VertexScanEvent| execution.borrow_mut().poll();
        let Some(row) = self.inner.vertex(vid, &mut poll).map_err(scoped_error)? else {
            return Ok(false);
        };
        Ok(self
            .execution
            .borrow()
            .permit
            .predicates()
            .allows_vertex(row.labels))
    }

    // This is private source admission, not a caller-visible raw record route.
    // Historical winner selection precedes scope on EVERY lookup. No per-probe
    // table, cache, permit or rescan from the first candidate is introduced.
    fn admitted_vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<QueryError, C>> {
        // FG-INV-20: the history lookup polls cancellation only, and nothing is
        // charged until the vertex is admitted; then one unit plus one per label
        // the capability may see. A forbidden or absent vertex costs nothing.
        let execution = Rc::clone(&self.execution);
        let mut poll = |_: VertexScanEvent| execution.borrow_mut().poll();
        let Some(row) = self.inner.vertex(vid, &mut poll).map_err(scoped_error)? else {
            return Ok(None);
        };
        let admitted = self
            .execution
            .borrow()
            .permit
            .predicates()
            .allows_vertex(row.labels);
        if !admitted {
            return Ok(None);
        }
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        for &label in row.labels {
            let visible = self
                .execution
                .borrow()
                .permit
                .predicates()
                .allows_label(label);
            if visible {
                control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            }
        }
        self.execution
            .borrow_mut()
            .node()
            .map_err(VertexScanSourceError::Source)?;
        Ok(Some(row))
    }
}
impl<S: VertexScanSource<Error = ReadError>> VertexScanSource for ScopedSource<'_, S> {
    type Error = QueryError;
    fn snapshot_seq(&self) -> CommitSeq {
        self.inner.snapshot_seq()
    }
    fn next_vertex<C>(
        &mut self,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, VertexScanSourceError<QueryError, C>> {
        // FG-INV-20: candidates this capability cannot see (forbidden, or not
        // live at the cut) are skipped HERE, walked with a poll-only control,
        // so neither the cursor's native meter nor the signed allowance ever
        // counts them. A refusal or cancellation seen while skipping surfaces
        // as a source error, which the stream reports exactly as a control one.
        let execution = Rc::clone(&self.execution);
        let mut poll = |_: VertexScanEvent| execution.borrow_mut().poll();
        loop {
            let Some(vid) = self.inner.next_vertex(&mut poll).map_err(scoped_error)? else {
                return Ok(None);
            };
            if self.visible(vid)? {
                return Ok(Some(vid));
            }
        }
    }
    fn vertex<'a, C>(
        &'a self,
        _: VId,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<QueryError, C>> {
        // Both root and probe kernels use owned masked records or individually
        // admitted scalar fields. Never lend raw metadata through this route.
        Err(VertexScanSourceError::Source(QueryError::Unsupported {
            diagnostics: vec!["authorized root source requires owned masked records".to_owned()],
        }))
    }
    fn vertex_record<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRecord<'a>>, VertexScanSourceError<QueryError, C>> {
        let Some(row) = self.admitted_vertex(vid, control)? else {
            return Ok(None);
        };
        VertexScanRecord::copy_masked(
            row,
            |label| {
                self.execution
                    .borrow()
                    .permit
                    .predicates()
                    .allows_label(label)
            },
            |key| {
                self.execution
                    .borrow()
                    .permit
                    .predicates()
                    .allows_property(key)
            },
            control,
        )
        .map(Some)
        .map_err(VertexScanSourceError::Control)
    }

    fn vertex_property<'a, C>(
        &'a self,
        vid: VId,
        key: PropertyKeyId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<Option<&'a CanonicalScalar>>, VertexScanSourceError<QueryError, C>> {
        let Some(row) = self.admitted_vertex(vid, control)? else {
            return Ok(None); // Missing vertex is NOT a present vertex with SQL NULL.
        };
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        if !self
            .execution
            .borrow()
            .permit
            .predicates()
            .allows_property(key)
        {
            return Ok(Some(None)); // Do not inspect or copy a forbidden payload.
        }
        // A binary search over the RAW property vector exposes its hidden
        // length/layout through both native and signed work ceilings. Inspect
        // only the admitted key sequence for logical accounting, as the scoped
        // edge-property source already does. Hidden traversal polls cancellation
        // without sampling the issuer clock or spending either allowance.
        for (candidate, value) in row.properties {
            self.execution
                .borrow_mut()
                .poll()
                .map_err(VertexScanSourceError::Source)?;
            if !self
                .execution
                .borrow()
                .permit
                .predicates()
                .allows_property(*candidate)
            {
                continue;
            }
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            match candidate.cmp(&key) {
                core::cmp::Ordering::Less => {}
                core::cmp::Ordering::Greater => break,
                core::cmp::Ordering::Equal => return Ok(Some(Some(value))),
            }
        }
        Ok(Some(None))
    }

    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        _: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<QueryError, C>> {
        // Independent scopes get their own caller-owned position and the same
        // immutable generation. Forbidden and absent candidates are skipped
        // here, unmetered, exactly as next_vertex does (FG-INV-20).
        let execution = Rc::clone(&self.execution);
        let mut poll = |_: GlaExecutionEvent| execution.borrow_mut().poll();
        let mut after = after;
        loop {
            let Some(vid) = self
                .inner
                .next_probe_vertex(after, &mut poll)
                .map_err(scoped_expansion_error)?
            else {
                return Ok(None);
            };
            if self.visible(vid).map_err(EdgeExpansionSourceError::Read)? {
                return Ok(Some(vid));
            }
            after = Some(vid);
        }
    }

    fn next_probe_edge_for_relation<C>(
        &self,
        endpoint: VId,
        relation: RelationId,
        direction: fgdb_gql::algebra::GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<QueryError, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        // This check precedes both endpoint resolution and incidence access.
        // The outer control is the SAME live permit even for a denied relation.
        if !self
            .execution
            .borrow()
            .permit
            .predicates()
            .allows_relation(relation)
        {
            return Ok(None);
        }
        let vertex = self
            .admitted_vertex(endpoint, &mut |event| control(edge_event(event)))
            .map_err(EdgeExpansionSourceError::Read)?;
        if vertex.is_none() {
            return Err(EdgeExpansionSourceError::Read(
                VertexScanSourceError::Source(probe_error(EdgeScanError::DanglingEndpoint)),
            ));
        }
        // The inner lookup may return a historical superset of incident edges
        // of ANY relation. An edge of another relation, or one whose far
        // endpoint this capability cannot see, is not part of its graph: skip
        // it here, unmetered, so no count of such edges reaches a charge
        // (FG-INV-20). probe_edge still re-admits every edge returned.
        let execution = Rc::clone(&self.execution);
        let mut poll = |_: GlaExecutionEvent| execution.borrow_mut().poll();
        let mut after = after;
        loop {
            let Some(eid) = self
                .inner
                .next_probe_edge_for_relation(endpoint, relation, direction, after, &mut poll)
                .map_err(scoped_expansion_error)?
            else {
                return Ok(None);
            };
            after = Some(eid);
            let Some(edge) = self
                .inner
                .probe_edge(eid, &mut poll)
                .map_err(scoped_expansion_error)?
            else {
                continue;
            };
            if edge.relation != relation {
                continue;
            }
            let far = if edge.source == endpoint {
                edge.target
            } else {
                edge.source
            };
            if self.visible(far).map_err(EdgeExpansionSourceError::Read)? {
                return Ok(Some(eid));
            }
        }
    }

    fn probe_edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<QueryError, C>> {
        // The edge's history lookup costs more as history grows, including
        // history this capability cannot see: resolve it unmetered, and charge
        // one unit only once the relation is admitted (FG-INV-20).
        let execution = Rc::clone(&self.execution);
        let mut poll = |_: GlaExecutionEvent| execution.borrow_mut().poll();
        let Some(edge) = self
            .inner
            .probe_edge(eid, &mut poll)
            .map_err(scoped_expansion_error)?
        else {
            return Ok(None);
        };
        if !self
            .execution
            .borrow()
            .permit
            .predicates()
            .allows_relation(edge.relation)
        {
            return Ok(None);
        }
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        for endpoint in [
            Some(edge.source),
            (edge.target != edge.source).then_some(edge.target),
        ]
        .into_iter()
        .flatten()
        {
            if self
                .admitted_vertex(endpoint, &mut |event| control(edge_event(event)))
                .map_err(EdgeExpansionSourceError::Read)?
                .is_none()
            {
                return Ok(None); // A hidden transit vertex removes the edge itself.
            }
        }
        // The checked profile has no captured probe edges or edge-property
        // operands. Give it only admitted topology, not unused raw properties.
        // compile() explicitly refuses captures even if a future kernel grows.
        Ok(Some(EdgeScanRow {
            source: edge.source,
            target: edge.target,
            relation: edge.relation,
            properties: &[],
        }))
    }
}

fn plan_error(error: VertexScanBuildError) -> QueryError {
    QueryError::Stream(GqlQueryError::Source(VertexScanError::Plan(error)))
}
fn compile(
    pattern: &PreparedGraphPattern<GraphValueRow>,
) -> Result<VertexScanPlan<GraphValueRow>, QueryError> {
    admit_source_profile(pattern)?;
    VertexScanPlan::compile(pattern.plan()).map_err(plan_error)
}

fn admit_source_profile(pattern: &PreparedGraphPattern<GraphValueRow>) -> Result<(), QueryError> {
    // This source exposes admitted probe topology, not edge payloads. Captured
    // edge/path operands remain outside its profile even if the native probe
    // compiler later grows them. Other admission belongs to that compiler.
    if let Some(operator) = pattern
        .plan()
        .operators()
        .iter()
        .position(|operator| matches!(operator, GlaOperator::CapturePath { .. }))
    {
        return Err(plan_error(VertexScanBuildError { operator }));
    }
    Ok(())
}
fn bind(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    default: CommitSeq,
) -> Result<(VertexScanPlan<GraphValueRow>, CommitSeq, Vec<String>), QueryError> {
    match prepared {
        PreparedNativeRead::Pattern(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::PatternText)?;
            Ok((compile(&query)?, default, query.columns().to_vec()))
        }
        PreparedNativeRead::TemporalPattern(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalText)?;
            Ok((
                compile(query.pattern())?,
                query.as_of(),
                query.pattern().columns().to_vec(),
            ))
        }
        _ => Err(QueryError::StreamingUnsupported {
            facade: prepared.facade_class(),
        }),
    }
}
fn scan_error(error: GqlQueryError<VertexScanError<QueryError>, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error)
        | GqlQueryError::Source(VertexScanError::Source(error)) => error,
        GqlQueryError::Source(VertexScanError::Plan(error)) => plan_error(error),
        GqlQueryError::Source(VertexScanError::NonIncreasingIdentity) => QueryError::Stream(
            GqlQueryError::Source(VertexScanError::NonIncreasingIdentity),
        ),
        GqlQueryError::Source(VertexScanError::CounterExhausted) => {
            QueryError::Stream(GqlQueryError::Source(VertexScanError::CounterExhausted))
        }
        GqlQueryError::Source(VertexScanError::Probe(error)) => probe_error(error),
        GqlQueryError::Rows(error) => QueryError::Stream(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::Stream(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => {
            QueryError::Stream(GqlQueryError::IdentifiedEdgesRequired)
        }
    }
}

fn probe_error(error: EdgeScanError<QueryError>) -> QueryError {
    let error = match error {
        EdgeScanError::Source(error) => return error,
        EdgeScanError::Plan(error) => EdgeScanError::Plan(error),
        EdgeScanError::NonIncreasingIdentity => EdgeScanError::NonIncreasingIdentity,
        EdgeScanError::DanglingEndpoint => EdgeScanError::DanglingEndpoint,
        EdgeScanError::CounterExhausted => EdgeScanError::CounterExhausted,
        EdgeScanError::ExpansionUnavailable => EdgeScanError::ExpansionUnavailable,
        EdgeScanError::BoundEdgeUnavailable => EdgeScanError::BoundEdgeUnavailable,
    };
    QueryError::Stream(GqlQueryError::Source(VertexScanError::Probe(error)))
}

#[allow(clippy::too_many_arguments)]
fn open<'q, C: FnMut() -> u64, Plan, Row: 'q, Metadata>(
    view: &EmbeddedReadView,
    capability: &'q VerifiedCapability<'_>,
    clock: &'q mut C,
    last_now_ms: &'q mut u64,
    cx: &'q QueryCx,
    branch: &str,
    owner: &Arc<()>,
    prepared: &AuthorizedPreparedRead,
    params: &GqlParameters,
    policy: GqlQueryPolicy,
    bind_plan: impl FnOnce(&PreparedNativeRead, &GqlParameters, CommitSeq) -> Result<Plan, QueryError>,
    build: impl FnOnce(
        Plan,
        &EmbeddedReadView,
        &'q QueryCx,
        Shared<'q>,
        GqlQueryPolicy,
    ) -> Opened<'q, Row, Metadata>,
) -> Opened<'q, Row, Metadata> {
    let now = clock();
    if now < *last_now_ms {
        return Err(QueryError::Authorization(
            AuthorizationError::ClockWentBackwards,
        ));
    }
    *last_now_ms = now;
    let permit = capability
        .begin_read_at(branch, now)
        .map_err(QueryError::Authorization)?;
    let tracked_clock: Box<dyn FnMut() -> u64 + 'q> = Box::new(move || {
        let now = clock();
        *last_now_ms = (*last_now_ms).max(now);
        now
    });
    let execution = Rc::new(RefCell::new(Execution::new(cx, permit, tracked_clock)));
    execution.borrow_mut().checkpoint()?;
    let selected = (|| {
        if !Arc::ptr_eq(owner, &prepared.owner) {
            return Err(QueryError::Authorization(
                AuthorizationError::WrongAuthority,
            ));
        }
        let selected = prepared
            .selector
            .bind_parameters(params)
            .map_err(selector_error)?;
        check_branch(&selected, branch)?;
        bind_plan(&prepared.native, selected.parameters(), view.frontier())
    })();
    execution.borrow_mut().checkpoint()?;
    build(selected?, view, cx, execution, policy)
}

fn build_rows<'q>(
    selected: (VertexScanPlan<GraphValueRow>, CommitSeq, Vec<String>),
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    execution: Shared<'q>,
    policy: GqlQueryPolicy,
) -> Opened<'q, GraphValueRow, ()> {
    let (plan, at, columns) = selected;
    let inner = view.vertex_scan_source(cx, at).map_err(QueryError::Read)?;
    let source = ScopedSource {
        inner,
        execution: Rc::clone(&execution),
    };
    let control = Rc::clone(&execution);
    let mut cursor = VertexScanCursor::new(source, plan, policy, move || {
        control.borrow_mut().checkpoint()
    });
    execution.borrow_mut().checkpoint()?;
    let driver = Box::new(move || {
        let row = cursor.next().transpose().map_err(scan_error)?;
        // A row remains private until live signed delivery admission succeeds.
        // The extra terminal check also covers a natural EOF or LIMIT 0.
        execution.borrow_mut().deliver(usize::from(row.is_some()))?;
        let finished = cursor.state() != VertexScanState::Open;
        Ok((row, finished))
    });
    Ok((
        AuthorizedRowCursor {
            driver: Some(driver),
            guard: None,
            columns,
            snapshot_seq: at,
            state: VertexScanState::Open,
        },
        (),
    ))
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Open a prepared root-vertex stream under this session's fixed policy and
    /// capability. The projection must lead with its unique VId; following cells
    /// may be its properties/repeated identity. Ordinary local predicates and
    /// SKIP/LIMIT reuse the checked native cursor. Temporal patterns retain their
    /// exact cut, no later than the session's pin.
    ///
    /// Opening authenticates before binding/profile/source admission and scans
    /// no candidate. Correlated/independent EXISTS and NOT EXISTS, with fixed
    /// or finite variable-length anonymous hops, use the existing indexed probe
    /// engine. Both historical endpoints must be visible at every hop, including
    /// transit vertices; labels and properties are masked before predicates.
    /// Denied relation types do not open incidence directories. Signed node
    /// usage includes repeated admitted endpoint/property reads, without a new
    /// permit per probe. Work and candidate records remain cumulative.
    ///
    /// Captured/nested/optional probes, root edges, aggregates, compound relations
    /// and alternate ordering refuse before source access; no eager fallback.
    /// Native candidate-history accounting differs from eager table admission.
    /// The writer remains independent, but this borrows the session until the
    /// cursor is dropped so its trusted clock and capability cannot be replaced.
    pub fn stream<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedRowCursor<'q>, QueryError> {
        self.open_cursor(cx, prepared, params, bind, build_rows)
            .map(|(cursor, ())| cursor)
    }

    // The same authenticated open/pin guard serves rows and aggregates. Each
    // factory receives only the fixed view and live permit; none can obtain a
    // Database, refresh a generation or independently authorize a source.
    fn open_cursor<'q, Plan, Row: 'q, Metadata>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
        bind_plan: impl FnOnce(
            &PreparedNativeRead,
            &GqlParameters,
            CommitSeq,
        ) -> Result<Plan, QueryError>,
        build: impl FnOnce(
            Plan,
            &EmbeddedReadView,
            &'q QueryCx,
            Shared<'q>,
            GqlQueryPolicy,
        ) -> Opened<'q, Row, Metadata>,
    ) -> Opened<'q, Row, Metadata> {
        let owner = &self.owner;
        let state = self.state.as_mut().ok_or(QueryError::Authorization(
            AuthorizationError::ExecutionStopped,
        ))?;
        let State {
            view,
            capability,
            branch,
            policy,
            clock,
            last_now_ms,
            ..
        } = state;
        let mut guard = PinGuard {
            pin: view,
            armed: true,
        };
        let result = open(
            guard.pin.as_ref().ok_or(QueryError::Authorization(
                AuthorizationError::ExecutionStopped,
            ))?,
            capability,
            clock,
            last_now_ms,
            cx,
            branch,
            owner,
            prepared,
            params,
            *policy,
            bind_plan,
            build,
        );
        match result {
            Ok((mut cursor, metadata)) => {
                guard.armed = false;
                cursor.guard = Some(guard);
                Ok((cursor, metadata))
            }
            Err(error) => {
                let result = Err(error);
                guard.armed = terminal(&result);
                result
            }
        }
    }
}

// This file is itself loaded through `#[path]`, so an undecorated child would
// resolve beside it (session/probe_tests.rs), not under stream/.
#[cfg(test)]
#[path = "stream/probe_tests.rs"]
mod probe_tests;
