//! Lazy root scans and probes over one scoped immutable history source.
//! Only the source boundary masks records; user predicates/projection stay in
//! the native physical operator. No source table or result set is collected.

use super::*;
use fgdb_gql::algebra::{EdgeRelation, GlaOperator, PreparedGraphPattern};
use fgdb_gql::edge_stream::{EdgeExpansionSourceError, EdgeScanError, EdgeScanPlan, EdgeScanRow};
use fgdb_gql::stream::{
    VertexScanBuildError, VertexScanCursor, VertexScanError, VertexScanEvent, VertexScanPlan,
    VertexScanRecord, VertexScanRow, VertexScanSource, VertexScanSourceError, VertexScanState,
};
use fgdb_gql::{GlaExecutionEvent, GraphSetExecutionError, GraphSetRowPull};
use fgdb_types::EId;
use std::iter::FusedIterator;
use std::marker::PhantomData;
use std::sync::{Mutex, MutexGuard};

#[path = "stream/aggregate.rs"]
mod aggregate;
pub use aggregate::{
    AuthorizedAggregateCursor, AuthorizedAsyncAggregateCursor, AuthorizedSendAggregateCursor,
};

#[path = "stream/ordered.rs"]
mod ordered;
pub use ordered::{
    AuthorizedOrderExecution, AuthorizedOrderedReadSession, AuthorizedSortLimits,
    AuthorizedSpoolCursor,
};

// Synchronization is local to ONE cursor's sequential callbacks. No guard is
// held across a source call or suspension. Keeping the actual borrowed clock
// permits both existing thread-local clocks and Send task-owned cursors.
struct Shared<'q, Clock>(Arc<Mutex<Execution<'q, 'q, &'q mut Clock>>>);
impl<Clock> Clone for Shared<'_, Clock> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}
impl<'q, Clock> Shared<'q, Clock> {
    fn borrow(&self) -> MutexGuard<'_, Execution<'q, 'q, &'q mut Clock>> {
        self.0.lock().expect("authorized cursor callback unwound")
    }
    fn borrow_mut(&self) -> MutexGuard<'_, Execution<'q, 'q, &'q mut Clock>> {
        self.borrow()
    }
}
type Pull<'q, Row> = Box<dyn FnMut() -> Result<(Option<Row>, bool), QueryError> + 'q>;
type SendPull<'q, Row> = Box<dyn FnMut() -> Result<(Option<Row>, bool), QueryError> + Send + 'q>;
type Opened<'q, Row, Metadata, Driver = Pull<'q, Row>> =
    Result<(AuthorizedRowCursor<'q, Row, Driver>, Metadata), QueryError>;

/// The same native row cursor with a Send driver. It remains exclusively
/// borrowed from its session; moving between runtime workers does not create
/// a second permit, snapshot, source position, or concurrent reader.
pub type AuthorizedSendRowCursor<'q> =
    AuthorizedRowCursor<'q, GraphValueRow, SendPull<'q, GraphValueRow>>;

// The bound GLA root chooses one physical source before any candidate read.
// Failure in that profile cannot fall back to a different or eager executor.
enum RowPlan {
    Vertex(VertexScanPlan<GraphValueRow>),
    Edge(EdgeScanPlan),
}

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
/// admissions inside probes; native snapshot records count admitted candidate
/// examinations, including re-examinations under distinct bindings. Signed rows
/// count only delivered occurrences after SKIP/LIMIT. Private source counters are not
/// exported. Root vertex scans retain one masked record; root edge scans retain
/// the admitted edge and its endpoints. Each keeps one projected row outside
/// the resident immutable generation and caller-owned rows.
/// Fixed probes add definition-bounded frames and temporary masked records;
/// finite variable-length probes retain governed traversal/support state.
/// The aggregate factory uses this same lifecycle with completed group rows;
/// its accumulation/support/collection storage is not bounded by the root-row
/// profile above. See stream_aggregate for its first-pull and storage contract.
///
/// The ordinary constructor preserves support for thread-local host clocks.
/// `stream_send` produces the same native cursor with a Send driver when the
/// session clock is Send, for a task that may move between runtime workers.
///
/// This borrows the session and QueryCx until dropped, not the database writer,
/// token bytes or prepared template. Credential invalidation or a host callback
/// unwind closes the session; ordinary cursor errors do not widen its policy.
/// No restart token, durable lease, spill, or physical noninterference is claimed.
pub struct AuthorizedRowCursor<'q, Row = GraphValueRow, Driver = Pull<'q, Row>> {
    driver: Option<Driver>,
    guard: Option<PinGuard<'q>>,
    columns: Vec<String>,
    snapshot_seq: CommitSeq,
    state: VertexScanState,
    row: PhantomData<fn() -> Row>,
}
impl<Row, Driver> AuthorizedRowCursor<'_, Row, Driver> {
    fn new(driver: Driver, columns: Vec<String>, at: CommitSeq) -> Self {
        Self {
            driver: Some(driver),
            guard: None,
            columns,
            snapshot_seq: at,
            state: VertexScanState::Open,
            row: PhantomData,
        }
    }
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
impl<Row, Driver> Iterator for AuthorizedRowCursor<'_, Row, Driver>
where
    Driver: FnMut() -> Result<(Option<Row>, bool), QueryError>,
{
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
impl<Row, Driver> FusedIterator for AuthorizedRowCursor<'_, Row, Driver> where
    Driver: FnMut() -> Result<(Option<Row>, bool), QueryError>
{
}
impl<Row, Driver> core::fmt::Debug for AuthorizedRowCursor<'_, Row, Driver> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedRowCursor")
            .field("state", &self.state)
            .field("authority_source_and_position", &"[REDACTED]")
            .finish()
    }
}

struct ScopedSource<'q, S, Clock> {
    inner: S,
    execution: Shared<'q, Clock>,
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

impl<S: VertexScanSource<Error = ReadError>, Clock: FnMut() -> u64> ScopedSource<'_, S, Clock> {
    /// Whether this capability may see `vid` at the cut, decided with a
    /// poll-only lookup: skipping a candidate is never charged (FG-INV-20).
    fn visible<C>(&self, vid: VId) -> Result<bool, VertexScanSourceError<QueryError, C>> {
        let execution = self.execution.clone();
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

    // Resolve the historical winner once, admit its scope, and recheck the
    // selected equality before charging a root. Historical index membership is
    // only a candidate superset; stale or private rows spend no allowance.
    fn visible_root<C>(&self, vid: VId) -> Result<bool, VertexScanSourceError<QueryError, C>> {
        let execution = self.execution.clone();
        let mut poll = |_: VertexScanEvent| execution.borrow_mut().poll();
        let Some(row) = self.inner.vertex(vid, &mut poll).map_err(scoped_error)? else {
            return Ok(false);
        };
        let admitted = self
            .execution
            .borrow()
            .permit
            .predicates()
            .allows_vertex(row.labels);
        if !admitted {
            return Ok(false);
        }
        self.inner
            .root_is_candidate(&row, &mut poll)
            .map_err(scoped_error)
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
        let execution = self.execution.clone();
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
            .vertex_node(vid)
            .map_err(VertexScanSourceError::Source)?;
        Ok(Some(row))
    }

    // Private common admission for topology and scalar reads. The selected
    // historical edge and BOTH endpoints must be in scope before any logical
    // charge or payload inspection. A capture is not cached authorization.
    fn admitted_probe_edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeExpansionSourceError<QueryError, C>> {
        let execution = self.execution.clone();
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
        let endpoints = [
            Some(edge.source),
            (edge.target != edge.source).then_some(edge.target),
        ];
        for endpoint in endpoints.into_iter().flatten() {
            if !self
                .visible(endpoint)
                .map_err(EdgeExpansionSourceError::Read)?
            {
                return Ok(None);
            }
        }
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        for endpoint in endpoints.into_iter().flatten() {
            if self
                .admitted_vertex(endpoint, &mut |event| control(edge_event(event)))
                .map_err(EdgeExpansionSourceError::Read)?
                .is_none()
            {
                return Ok(None);
            }
        }
        Ok(Some(edge))
    }
}
impl<S: VertexScanSource<Error = ReadError>, Clock: FnMut() -> u64> VertexScanSource
    for ScopedSource<'_, S, Clock>
{
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
        let execution = self.execution.clone();
        let mut poll = |_: VertexScanEvent| execution.borrow_mut().poll();
        loop {
            let Some(vid) = self.inner.next_vertex(&mut poll).map_err(scoped_error)? else {
                return Ok(None);
            };
            if self.visible_root(vid)? {
                // An admitted root costs its node before any row can use it.
                self.execution
                    .borrow_mut()
                    .root_node(vid)
                    .map_err(VertexScanSourceError::Source)?;
                return Ok(Some(vid));
            }
        }
    }

    fn next_vertex_step<C>(
        &mut self,
        _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<fgdb_gql::scan_stream::SourceStep<VId>, VertexScanSourceError<QueryError, C>> {
        use fgdb_gql::scan_stream::SourceStep;
        let execution = self.execution.clone();
        let mut poll = |_: VertexScanEvent| execution.borrow_mut().poll();
        match self
            .inner
            .next_vertex_step(&mut poll)
            .map_err(scoped_error)?
        {
            SourceStep::Item(vid) if self.visible_root(vid)? => {
                // The cooperative step admits a root exactly as next_vertex
                // does, so its node is charged before any row can use it.
                self.execution
                    .borrow_mut()
                    .root_node(vid)
                    .map_err(VertexScanSourceError::Source)?;
                Ok(SourceStep::Item(vid))
            }
            SourceStep::Item(vid) | SourceStep::Skipped(vid) => Ok(SourceStep::Skipped(vid)),
            SourceStep::Exhausted => Ok(SourceStep::Exhausted),
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
    ) -> Result<
        Option<Option<&'a fgdb_types::CanonicalPropertyValue>>,
        VertexScanSourceError<QueryError, C>,
    > {
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
        let execution = self.execution.clone();
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
        relation: EdgeRelation,
        direction: fgdb_gql::algebra::GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<QueryError, C>> {
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        // This check precedes both endpoint resolution and incidence access.
        // The outer control is the SAME live permit even for a denied relation.
        if relation.relations().is_some_and(|members| {
            let execution = self.execution.borrow();
            let predicates = execution.permit.predicates();
            members
                .iter()
                .all(|member| !predicates.allows_relation(*member))
        }) {
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
        let execution = self.execution.clone();
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
            if !relation.matches(edge.relation)
                || !self
                    .execution
                    .borrow()
                    .permit
                    .predicates()
                    .allows_relation(edge.relation)
            {
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
        let Some(edge) = self.admitted_probe_edge(eid, control)? else {
            return Ok(None);
        };
        // Captures retain identities, never raw payloads. Their predicates use
        // the separately admitted scalar route below, on the same permit/cut.
        Ok(Some(EdgeScanRow {
            source: edge.source,
            target: edge.target,
            relation: edge.relation,
            properties: &[],
        }))
    }

    fn probe_edge_property<'a, C>(
        &'a self,
        eid: EId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<
        Option<Option<&'a fgdb_types::CanonicalPropertyValue>>,
        EdgeExpansionSourceError<QueryError, C>,
    > {
        let Some(edge) = self.admitted_probe_edge(eid, control)? else {
            return Ok(None);
        };
        control(GlaExecutionEvent::Work)
            .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
        if !self
            .execution
            .borrow()
            .permit
            .predicates()
            .allows_property(key)
        {
            return Ok(Some(None));
        }
        // Never binary-search raw layout: hidden keys would change the quota
        // threshold. Hidden traversal polls cancellation only, without sampling
        // the issuer clock; admitted keys alone spend native and signed work.
        for (candidate, value) in edge.properties {
            self.execution.borrow_mut().poll().map_err(|error| {
                EdgeExpansionSourceError::Read(VertexScanSourceError::Source(error))
            })?;
            if !self
                .execution
                .borrow()
                .permit
                .predicates()
                .allows_property(*candidate)
            {
                continue;
            }
            control(GlaExecutionEvent::Work)
                .map_err(|e| EdgeExpansionSourceError::Read(VertexScanSourceError::Control(e)))?;
            match candidate.cmp(&key) {
                core::cmp::Ordering::Less => {}
                core::cmp::Ordering::Greater => break,
                core::cmp::Ordering::Equal => return Ok(Some(Some(value))),
            }
        }
        Ok(Some(None))
    }
}

fn plan_error(error: VertexScanBuildError) -> QueryError {
    QueryError::Stream(GqlQueryError::Source(VertexScanError::Plan(error)))
}
fn compile(pattern: &PreparedGraphPattern<GraphValueRow>) -> Result<RowPlan, QueryError> {
    if matches!(
        pattern.plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    ) {
        return EdgeScanPlan::compile(pattern.plan())
            .map(RowPlan::Edge)
            .map_err(|error| {
                QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Plan(error)))
            });
    }
    admit_source_profile(pattern)?;
    VertexScanPlan::compile(pattern.plan())
        .map(RowPlan::Vertex)
        .map_err(plan_error)
}

fn admit_source_profile(pattern: &PreparedGraphPattern<GraphValueRow>) -> Result<(), QueryError> {
    // Admit private single-edge probe captures, not arbitrary captured paths
    // or outer capture export. The native compiler still proves fixed-edge
    // producers, operand domains, scope-local IDs and the full output profile.
    // Keep this source fence shared by row and aggregate factories.
    let operators = pattern.plan().operators();
    let mut probe_end = None;
    for (operator, op) in operators.iter().enumerate() {
        if probe_end == Some(operator) {
            probe_end = None;
        }
        match op {
            GlaOperator::Probe { group, end, .. } => {
                let end = *end as usize;
                if probe_end.is_some()
                    || end <= operator
                    || !matches!(operators.get(end), Some(GlaOperator::ProbeEnd { group: close }) if close == group)
                {
                    return Err(plan_error(VertexScanBuildError { operator }));
                }
                probe_end = Some(end);
            }
            GlaOperator::CapturePath { segments, .. }
                if probe_end.is_none() || segments.len() != 1 =>
            {
                return Err(plan_error(VertexScanBuildError { operator }));
            }
            _ => {}
        }
    }
    Ok(())
}
/// A bound row plan: the physical plan, its cut, the output columns and,
/// for a set that pulls row by row, the projection each source row takes.
type BoundRows = (RowPlan, CommitSeq, Vec<String>, Option<GraphSetRowPull>);

fn bind(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    default: CommitSeq,
) -> Result<BoundRows, QueryError> {
    match prepared {
        PreparedNativeRead::Pattern(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::PatternText)?;
            Ok((compile(&query)?, default, query.columns().to_vec(), None))
        }
        PreparedNativeRead::TemporalPattern(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalText)?;
            Ok((
                compile(query.pattern())?,
                query.as_of(),
                query.pattern().columns().to_vec(),
                None,
            ))
        }
        // `MATCH (n) RETURN n AS id, $value AS value` (fgdb-7xqxb): a set
        // whose canonical order is its one vertex scan's order pulls through
        // that scan's native cursor, projecting each row as the eager path.
        PreparedNativeRead::Set(set) => {
            let query = set.bind_parameters(params).map_err(QueryError::SetText)?;
            let pull = query.row_pull().ok_or(QueryError::StreamingUnsupported {
                facade: prepared.facade_class(),
            })?;
            let RowPlan::Vertex(plan) = compile(pull.pattern())? else {
                unreachable!("a row pull is rooted at its one vertex scan")
            };
            Ok((
                RowPlan::Vertex(plan),
                default,
                query.columns().to_vec(),
                Some(pull),
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
        GqlQueryError::Source(VertexScanError::InvalidCandidateAdmission) => QueryError::Stream(
            GqlQueryError::Source(VertexScanError::InvalidCandidateAdmission),
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
        GqlQueryError::Data(error) => QueryError::Stream(GqlQueryError::Data(error)),
    }
}

fn probe_error(error: EdgeScanError<QueryError>) -> QueryError {
    let error = match error {
        EdgeScanError::Source(error) => return error,
        EdgeScanError::Plan(error) => EdgeScanError::Plan(error),
        EdgeScanError::NonIncreasingIdentity => EdgeScanError::NonIncreasingIdentity,
        EdgeScanError::InvalidCandidateAdmission => EdgeScanError::InvalidCandidateAdmission,
        EdgeScanError::DanglingEndpoint => EdgeScanError::DanglingEndpoint,
        EdgeScanError::CounterExhausted => EdgeScanError::CounterExhausted,
        EdgeScanError::ExpansionUnavailable => EdgeScanError::ExpansionUnavailable,
        EdgeScanError::BoundEdgeUnavailable => EdgeScanError::BoundEdgeUnavailable,
    };
    QueryError::Stream(GqlQueryError::Source(VertexScanError::Probe(error)))
}

#[allow(clippy::too_many_arguments)]
fn open<'q, C: FnMut() -> u64, Plan, Output>(
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
        Shared<'q, C>,
        GqlQueryPolicy,
    ) -> Result<Output, QueryError>,
) -> Result<Output, QueryError> {
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
    let execution = Shared(Arc::new(Mutex::new(Execution::tracking(
        cx,
        permit,
        clock,
        last_now_ms,
    ))));
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

fn build_rows<'q, C: FnMut() -> u64>(
    selected: BoundRows,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    policy: GqlQueryPolicy,
) -> Opened<'q, GraphValueRow, ()> {
    let (plan, at, columns, pull) = selected;
    let driver: Pull<'q, GraphValueRow> = match plan {
        RowPlan::Edge(plan) => Box::new(aggregate::edge::row_driver(
            plan, at, view, cx, execution, policy,
        )?),
        RowPlan::Vertex(plan) => {
            Box::new(vertex_driver(plan, pull, at, view, cx, execution, policy)?)
        }
    };
    Ok((AuthorizedRowCursor::new(driver, columns, at), ()))
}

fn build_send_rows<'q, C: FnMut() -> u64 + Send>(
    selected: BoundRows,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    policy: GqlQueryPolicy,
) -> Opened<'q, GraphValueRow, (), SendPull<'q, GraphValueRow>> {
    let (plan, at, columns, pull) = selected;
    // Only erasure differs from the local factory. Both use the exact same
    // native physical driver, source masks, signed permit and terminal guard.
    let driver: SendPull<'q, GraphValueRow> = match plan {
        RowPlan::Edge(plan) => Box::new(aggregate::edge::row_driver(
            plan, at, view, cx, execution, policy,
        )?),
        RowPlan::Vertex(plan) => {
            Box::new(vertex_driver(plan, pull, at, view, cx, execution, policy)?)
        }
    };
    Ok((AuthorizedRowCursor::new(driver, columns, at), ()))
}

/// One step of a native row driver: the row it delivered, if any, and
/// whether its cursor has finished.
type DriverStep = Result<(Option<GraphValueRow>, bool), QueryError>;

fn vertex_driver<'q, C: FnMut() -> u64>(
    plan: VertexScanPlan<GraphValueRow>,
    pull: Option<GraphSetRowPull>,
    at: CommitSeq,
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    policy: GqlQueryPolicy,
) -> Result<impl FnMut() -> DriverStep + use<'q, C>, QueryError> {
    let inner = view
        .vertex_scan_source(cx, at)
        .map_err(QueryError::Read)?
        .with_root_equality(&plan, |key| {
            execution.borrow().permit.predicates().allows_property(key)
        });
    let source = ScopedSource {
        inner,
        execution: execution.clone(),
    };
    let control = execution.clone();
    let mut cursor = VertexScanCursor::new(source, plan, policy, move || {
        control.borrow_mut().checkpoint()
    });
    execution.borrow_mut().checkpoint()?;
    Ok(move || {
        // The projection runs inside the same native meter as its source.
        // Every work/scratch event also reaches the existing signed checkpoint;
        // no private source row counts as delivered before projection succeeds.
        let row = if let Some(pull) = &pull {
            cursor.next_projected(pull, scan_error, |ordinal, column, error| {
                QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Projection {
                    row: usize::try_from(ordinal).unwrap_or(usize::MAX),
                    column,
                    error,
                }))
            })
        } else {
            cursor.next().map(|row| row.map_err(scan_error))
        }
        .transpose()?;
        // A row remains private until live signed delivery admission succeeds.
        // The extra terminal check also covers a natural EOF or LIMIT 0.
        execution.borrow_mut().deliver(usize::from(row.is_some()))?;
        let finished = cursor.state() != VertexScanState::Open;
        Ok((row, finished))
    })
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Open a prepared vertex or fixed-edge stream under this session's policy
    /// and capability. Vertex projection must lead with its unique VId. Directed
    /// edge projection must lead with the captured root EId; undirected roots
    /// also require the root-source VId to distinguish orientations. Connected
    /// fixed joins then name each appended EId in traversal order.
    /// These prefixes prove native canonical ordering and ALL/DISTINCT semantics
    /// without a result bag or seen-set. Remaining edge columns may project
    /// endpoints, masked vertex/edge properties and admitted fixed-path values.
    /// Both orientations of an undirected edge remain separate occurrences;
    /// a self-loop is emitted once. SKIP/LIMIT act on accepted occurrences.
    /// Temporal patterns keep their exact cut, no later than the session's pin.
    ///
    /// Opening authenticates before binding/profile/source admission and scans
    /// no candidate. Correlated/independent EXISTS and NOT EXISTS, with fixed
    /// or finite variable-length anonymous hops, use the existing indexed probe
    /// engine. Fixed probe relationships may bind private property operands;
    /// each field lookup re-admits the edge, its endpoints and selected key.
    /// Both historical endpoints must be visible at every hop, including
    /// transit vertices; labels and properties are masked before predicates.
    /// Denied relation types do not open incidence directories. Signed node
    /// usage includes repeated admitted endpoint/property reads, without a new
    /// permit per probe. Work and candidate records remain cumulative.
    ///
    /// The selected native compiler refuses unsupported projections, optional
    /// or nested scopes, variable-length outer joins, catalog-name outputs and
    /// alternate ordering before source access; no eager or privileged fallback.
    /// Probe captures cannot escape into output or correlate to outer edges.
    /// Aggregates use stream_aggregate; compound relations are not admitted here.
    /// Native candidate accounting differs from eager full-table admission.
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

    /// Open the same source-native row plan with a Send driver, suitable for
    /// a task that suspends between pulls and moves between runtime workers.
    /// Only the clock gains a Send bound; non-Send clocks can still use stream.
    /// No background task, row queue, extra permit or eager fallback is created.
    /// Admission, ordering, source profile and error semantics equal stream().
    pub fn stream_send<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedSendRowCursor<'q>, QueryError>
    where
        C: Send,
    {
        self.open_cursor(cx, prepared, params, bind, build_send_rows)
            .map(|(cursor, ())| cursor)
    }

    // The same authenticated open/pin guard serves rows and aggregates. Each
    // factory receives only the fixed view and live permit; none can obtain a
    // Database, refresh a generation or independently authorize a source.
    fn open_cursor<'q, Plan, Row: 'q, Metadata, Driver>(
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
            Shared<'q, C>,
            GqlQueryPolicy,
        ) -> Opened<'q, Row, Metadata, Driver>,
    ) -> Opened<'q, Row, Metadata, Driver> {
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

#[cfg(test)]
#[path = "stream/edge_property_tests.rs"]
mod edge_property_tests;

#[cfg(test)]
mod projected_budget_tests {
    use super::*;
    use crate::{Database, DatabaseKeys, MemVfs, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use asupersync::security::key::AuthKey;
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GlaLimitDimension, GraphIntegerErrorKind, GraphSymbol, GraphSymbolKind};
    use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
    use fgdb_warden::{Authority, Grant, QueryLimits, Rights, Scope};

    const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x8b; 32]);
    const LABEL: LabelId = LabelId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const R: RelationId = RelationId(1);

    async fn database(cx: &CommitCx) -> Database<MemVfs> {
        let mut db = Database::open_memory(cx, DatabaseKeys::new([0x8a; 32], NS, [0x8c; 32]))
            .await
            .unwrap();
        let mut batch = WriteBatch::new(R);
        for (id, label, value) in [(1, LABEL, 2), (2, LabelId(2), 0), (3, LABEL, 0)] {
            batch.create_vertex(VId(id), vec![label], vec![(P, CanonicalScalar::Int(value))]);
        }
        db.write(cx, batch).await.unwrap();
        db
    }

    fn authority() -> Authority {
        Authority::new(AuthKey::from_seed(0x8b00), NS, "graph", SchemaEpoch(1), 1).unwrap()
    }

    fn grant() -> Grant {
        Grant {
            branch: "main".into(),
            labels: Scope::only([LABEL]),
            relations: Scope::only([R]),
            properties: Scope::only([P]),
            rights: Rights::Read,
            limits: QueryLimits {
                max_nodes: 1000,
                max_work: 100_000_000,
                max_rows: 1000,
            },
            expires_at_ms: 10_000,
        }
    }

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }
    }

    fn denied<Driver>(
        mut cursor: AuthorizedRowCursor<'_, GraphValueRow, Driver>,
        dimension: GlaLimitDimension,
    ) where
        Driver: FnMut() -> DriverStep,
    {
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Stream(GqlQueryError::Evaluator(error))))
                if error.dimension == dimension
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(cursor.size_hint(), (0, Some(0)));
        assert!(cursor.next().is_none());
        cursor.close();
        assert_eq!(cursor.state(), VertexScanState::Failed);
    }

    #[test]
    fn authorized_local_and_send_projections_cannot_bypass_native_limits() {
        let ((), report) = run_async_under_lab(0x8b00_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let db = database(&contexts.commit()).await;
            let issuer = authority();
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let args = GqlParameters::new()
                .with_scalar(
                    "value",
                    CanonicalScalar::ucs_basic_text(&"x".repeat(32_768)).unwrap(),
                )
                .unwrap();
            for (text, policy, dimension) in [
                (
                    "MATCH (n) RETURN n AS id, $value AS value",
                    GqlQueryPolicy::new(1000, 1000, u64::MAX, 128),
                    GlaLimitDimension::ScratchEntries,
                ),
                (
                    "MATCH (n) RETURN n AS id, toUpper($value) AS value",
                    GqlQueryPolicy::new(1000, 1000, 256, u64::MAX),
                    GlaLimitDimension::WorkUnits,
                ),
            ] {
                let mut session = db
                    .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy, || 100)
                    .unwrap();
                // The entire native source fits this policy. Only the actual
                // projection can exhaust it; signed allowances are generous.
                let empty = GqlParameters::new();
                let scan = session
                    .prepare(&cx, "MATCH (n) RETURN n AS id", &empty)
                    .unwrap();
                let rows = session
                    .stream(&cx, &scan, &empty)
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].values(), &[GraphValue::Vertex(VId(1))]);
                assert_eq!(rows[1].values(), &[GraphValue::Vertex(VId(3))]);
                let prepared = session.prepare(&cx, text, &args).unwrap();
                // Both open lazily. The first credited pull must refuse with
                // the native evaluator error, not a successful unmetered row.
                denied(session.stream(&cx, &prepared, &args).unwrap(), dimension);
                denied(
                    session.stream_send(&cx, &prepared, &args).unwrap(),
                    dimension,
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    fn late_error<Driver>(mut cursor: AuthorizedRowCursor<'_, GraphValueRow, Driver>)
    where
        Driver: FnMut() -> DriverStep,
    {
        let first = cursor.next().unwrap().unwrap();
        assert_eq!(
            first.values(),
            &[
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Int(5))
            ]
        );
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Set(GqlQueryError::Source(
                GraphSetExecutionError::Projection { row: 1, column: 1, error }
            )))) if error.kind == GraphIntegerErrorKind::DivisionByZero
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    }

    #[test]
    fn authorized_projection_errors_keep_the_visible_row_ordinal_and_native_cause() {
        let ((), report) = run_async_under_lab(0x8b00_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let db = database(&contexts.commit()).await;
            let issuer = authority();
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let policy = GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000);
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy, || 100)
                .unwrap();
            let args = GqlParameters::new();
            let prepared = session
                .prepare(&cx, "MATCH (n) RETURN n AS id, 10 / n.p AS value", &args)
                .unwrap();
            // Hidden VId(2) has the same bad divisor but is not a source row.
            // The failure remains row one of the visible execution in both
            // driver variants; an earlier successful row remains delivered.
            late_error(session.stream(&cx, &prepared, &args).unwrap());
            late_error(session.stream_send(&cx, &prepared, &args).unwrap());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn authorized_equality_streams_hide_private_candidates_and_recheck_historical_membership() {
        let ((), report) = run_async_under_lab(0x8b00_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let issuer = authority();
            let empty = GqlParameters::new();
            let policy = GqlQueryPolicy::new(2, 2, 100_000, 100_000);
            let mut allowance = grant();
            allowance.limits.max_nodes = 2;
            allowance.limits.max_rows = 2;
            let token = issuer.issue_at(&allowance, 100).unwrap();
            let mut answers = Vec::new();
            for private_history in [false, true] {
                let mut db =
                    Database::open_memory(&commit, DatabaseKeys::new([0x8a; 32], NS, [0x8c; 32]))
                        .await
                        .unwrap();
                let mut batch = WriteBatch::new(R);
                batch.create_vertex(VId(1), vec![LABEL], vec![(P, CanonicalScalar::Int(7))]);
                batch.create_vertex(
                    VId(2),
                    vec![LABEL],
                    vec![(P, CanonicalScalar::Int(if private_history { 7 } else { 9 }))],
                );
                batch.create_vertex(VId(3), vec![LABEL], vec![(P, CanonicalScalar::Int(7))]);
                for id in 10..=138 {
                    batch.create_vertex(
                        VId(id),
                        vec![LABEL],
                        vec![(P, CanonicalScalar::Int(id as i64 + 100))],
                    );
                    if private_history {
                        batch.create_vertex(
                            VId(1000 + id),
                            vec![LabelId(2)],
                            vec![(P, CanonicalScalar::Int(7))],
                        );
                    }
                }
                db.write(&commit, batch).await.unwrap();
                let mut edits = WriteBatch::new(R);
                edits.set_vertex_property(
                    VId(1),
                    P,
                    Some(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(7.0))),
                );
                edits.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(9)));
                db.write(&commit, edits).await.unwrap();

                let mut session = db
                    .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy, || 100)
                    .unwrap();
                let prepared = session
                    .prepare(&cx, "MATCH (n {p: 7.0}) RETURN n AS id, n.p AS p", &empty)
                    .unwrap();
                let rows = session
                    .stream(&cx, &prepared, &empty)
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0].values()[0], GraphValue::Vertex(VId(1)));
                assert_eq!(rows[1].values()[0], GraphValue::Vertex(VId(3)));
                assert_eq!(
                    session
                        .stream_send(&cx, &prepared, &empty)
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                    rows
                );
                answers.push(rows);
                drop(session);

                // A denied property cannot steer candidate seeks. Its masked
                // predicate is still evaluated on the ordinary visible domain,
                // whose third row exceeds this native record allowance.
                let mut masked = grant();
                masked.properties = Scope::only(std::iter::empty::<PropertyKeyId>());
                let masked = issuer.issue_at(&masked, 100).unwrap();
                let mut session = db
                    .authorized_read_session(&cx, &issuer, &masked, "main", symbols, policy, || 100)
                    .unwrap();
                let prepared = session
                    .prepare(&cx, "MATCH (n {p: 7.0}) RETURN n AS id", &empty)
                    .unwrap();
                assert!(
                    session
                        .stream(&cx, &prepared, &empty)
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .is_err()
                );
            }
            assert_eq!(answers[0], answers[1]);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
