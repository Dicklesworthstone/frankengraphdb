//! Lazy root scans using the existing history source and vertex cursor.
//! Only the source boundary masks records; user predicates/projection stay in
//! the native physical operator. No source table or result set is collected.

use super::*;
use fgdb_gql::algebra::{GlaOperator, PreparedGraphPattern};
use fgdb_gql::stream::{
    VertexScanBuildError, VertexScanCursor, VertexScanError, VertexScanEvent,
    VertexScanPlan, VertexScanRecord, VertexScanRow, VertexScanSource,
    VertexScanSourceError, VertexScanState,
};
use std::iter::FusedIterator;
use std::rc::Rc;

type Shared<'q> = Rc<RefCell<Execution<'q, 'q, Box<dyn FnMut() -> u64 + 'q>>>>;
type Pull<'q> = Box<dyn FnMut() -> Result<(Option<GraphValueRow>, bool), QueryError> + 'q>;

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
/// examined before user predicates; native snapshot records count candidate
/// histories, including invisible/forbidden candidates. Signed rows count only
/// delivered occurrences after SKIP/LIMIT. Private source counters are not
/// exported. At most one masked record and one projected row are needed in the
/// root path, outside the resident immutable generation and caller-owned rows.
///
/// This borrows the session and QueryCx until dropped, not the database writer,
/// token bytes or prepared template. Credential invalidation or a host callback
/// unwind closes the session; ordinary cursor errors do not widen its policy.
/// No restart token, durable lease, spill, or physical noninterference is claimed.
pub struct AuthorizedRowCursor<'q> {
    driver: Option<Pull<'q>>,
    guard: Option<PinGuard<'q>>,
    columns: Vec<String>,
    snapshot_seq: CommitSeq,
    state: VertexScanState,
}
impl AuthorizedRowCursor<'_> {
    pub fn columns(&self) -> &[String] { &self.columns }
    pub fn snapshot_seq(&self) -> CommitSeq { self.snapshot_seq }
    pub fn state(&self) -> VertexScanState { self.state }

    /// Release this cursor without draining it. A normal close preserves the
    /// session; a previously unwound poll keeps its terminal session fence.
    pub fn close(&mut self) {
        if self.state == VertexScanState::Open {
            self.state = VertexScanState::Closed;
            if let Some(guard) = &mut self.guard { guard.armed = false; }
        }
        self.driver.take();
        self.guard.take();
    }
}
impl Iterator for AuthorizedRowCursor<'_> {
    type Item = Result<GraphValueRow, QueryError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.state != VertexScanState::Open { return None; }
        self.state = VertexScanState::Failed;
        if let Some(guard) = &mut self.guard { guard.armed = true; }
        let mut driver = self.driver.take().expect("open authorized cursor owns its driver");
        match driver() {
            Ok((value, finished)) => {
                if let Some(guard) = &mut self.guard { guard.armed = false; }
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
                if let Some(guard) = &mut self.guard { guard.armed = terminal(&result); }
                self.guard.take();
                Some(result)
            }
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, (self.state != VertexScanState::Open).then_some(0))
    }
}
impl FusedIterator for AuthorizedRowCursor<'_> {}
impl core::fmt::Debug for AuthorizedRowCursor<'_> {
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
fn source_error<C>(error: VertexScanSourceError<ReadError, C>) -> VertexScanSourceError<QueryError, C> {
    match error {
        VertexScanSourceError::Source(error) => VertexScanSourceError::Source(QueryError::Read(error)),
        VertexScanSourceError::Control(error) => VertexScanSourceError::Control(error),
    }
}
impl<S: VertexScanSource<Error = ReadError>> VertexScanSource for ScopedSource<'_, S> {
    type Error = QueryError;
    fn snapshot_seq(&self) -> CommitSeq { self.inner.snapshot_seq() }
    fn next_vertex<C>(&mut self, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VId>, VertexScanSourceError<QueryError, C>>
    {
        self.inner.next_vertex(control).map_err(source_error)
    }
    fn vertex<'a, C>(&'a self, _: VId, _: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRow<'a>>, VertexScanSourceError<QueryError, C>>
    {
        // This adapter cannot lend a raw, unmasked record. Probe plans are
        // rejected at opening; optional probe access also refuses by default.
        Err(VertexScanSourceError::Source(QueryError::Unsupported {
            diagnostics: vec!["authorized root source requires owned masked records".to_owned()],
        }))
    }
    fn vertex_record<'a, C>(&'a self, vid: VId, control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>)
        -> Result<Option<VertexScanRecord<'a>>, VertexScanSourceError<QueryError, C>>
    {
        // The existing history index selects the winner BEFORE scope. A hidden
        // successor never resurrects an older, allowed label/property image.
        let Some(row) = self.inner.vertex(vid, control).map_err(source_error)? else { return Ok(None); };
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        for _ in row.labels {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        }
        if !self.execution.borrow().permit.predicates().allows_vertex(row.labels) {
            return Ok(None);
        }
        self.execution.borrow_mut().node().map_err(VertexScanSourceError::Source)?;
        VertexScanRecord::copy_masked(
            row,
            |label| self.execution.borrow().permit.predicates().allows_label(label),
            |key| self.execution.borrow().permit.predicates().allows_property(key),
            control,
        ).map(Some).map_err(VertexScanSourceError::Control)
    }
}

fn plan_error(error: VertexScanBuildError) -> QueryError {
    QueryError::Stream(GqlQueryError::Source(VertexScanError::Plan(error)))
}
fn compile(pattern: &PreparedGraphPattern<GraphValueRow>) -> Result<VertexScanPlan<GraphValueRow>, QueryError> {
    // Root record ownership does not automatically authorize probe sources.
    // Refuse before reading anything, even with LIMIT 0 or an empty graph.
    if let Some(operator) = pattern.plan().operators().iter().position(|operator| {
        matches!(operator, GlaOperator::Probe { .. })
    }) {
        return Err(plan_error(VertexScanBuildError { operator }));
    }
    VertexScanPlan::compile(pattern.plan()).map_err(plan_error)
}
fn bind(
    prepared: &PreparedNativeRead, params: &GqlParameters, default: CommitSeq,
) -> Result<(VertexScanPlan<GraphValueRow>, CommitSeq, Vec<String>), QueryError> {
    match prepared {
        PreparedNativeRead::Pattern(prepared) => {
            let query = prepared.bind_parameters(params).map_err(QueryError::PatternText)?;
            Ok((compile(&query)?, default, query.columns().to_vec()))
        }
        PreparedNativeRead::TemporalPattern(prepared) => {
            let query = prepared.bind_parameters(params).map_err(QueryError::TemporalText)?;
            Ok((compile(query.pattern())?, query.as_of(), query.pattern().columns().to_vec()))
        }
        _ => Err(QueryError::StreamingUnsupported { facade: prepared.facade_class() }),
    }
}
fn scan_error(error: GqlQueryError<VertexScanError<QueryError>, QueryError>) -> QueryError {
    match error {
        GqlQueryError::Interrupted(error) | GqlQueryError::Source(VertexScanError::Source(error)) => error,
        GqlQueryError::Source(VertexScanError::Plan(error)) => plan_error(error),
        GqlQueryError::Source(VertexScanError::NonIncreasingIdentity) => {
            QueryError::Stream(GqlQueryError::Source(VertexScanError::NonIncreasingIdentity))
        }
        GqlQueryError::Source(VertexScanError::CounterExhausted) => {
            QueryError::Stream(GqlQueryError::Source(VertexScanError::CounterExhausted))
        }
        GqlQueryError::Source(VertexScanError::Probe(_)) => QueryError::Unsupported {
            diagnostics: vec!["authorized root stream encountered an unsupported probe".to_owned()],
        },
        GqlQueryError::Rows(error) => QueryError::Stream(GqlQueryError::Rows(error)),
        GqlQueryError::Evaluator(error) => QueryError::Stream(GqlQueryError::Evaluator(error)),
        GqlQueryError::IdentifiedEdgesRequired => QueryError::Stream(GqlQueryError::IdentifiedEdgesRequired),
    }
}

#[allow(clippy::too_many_arguments)]
fn open<'q, C: FnMut() -> u64>(
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
) -> Result<AuthorizedRowCursor<'q>, QueryError> {
    let now = clock();
    if now < *last_now_ms {
        return Err(QueryError::Authorization(AuthorizationError::ClockWentBackwards));
    }
    *last_now_ms = now;
    let permit = capability.begin_read_at(branch, now).map_err(QueryError::Authorization)?;
    let tracked_clock: Box<dyn FnMut() -> u64 + 'q> = Box::new(move || {
        let now = clock();
        *last_now_ms = (*last_now_ms).max(now);
        now
    });
    let execution = Rc::new(RefCell::new(Execution { cx, permit, clock: tracked_clock }));
    execution.borrow_mut().checkpoint()?;
    let selected = (|| {
        if !Arc::ptr_eq(owner, &prepared.owner) {
            return Err(QueryError::Authorization(AuthorizationError::WrongAuthority));
        }
        let selected = prepared.selector.bind_parameters(params).map_err(selector_error)?;
        check_branch(&selected, branch)?;
        bind(&prepared.native, selected.parameters(), view.frontier())
    })();
    execution.borrow_mut().checkpoint()?;
    let (plan, at, columns) = selected?;
    let inner = view.vertex_scan_source(cx, at).map_err(QueryError::Read)?;
    let source = ScopedSource { inner, execution: Rc::clone(&execution) };
    let control = Rc::clone(&execution);
    let mut cursor = VertexScanCursor::new(source, plan, policy, move || control.borrow_mut().checkpoint());
    execution.borrow_mut().checkpoint()?;
    let driver = Box::new(move || {
        let row = cursor.next().transpose().map_err(scan_error)?;
        // A row remains private until live signed delivery admission succeeds.
        // The extra terminal check also covers a natural EOF or LIMIT 0.
        execution.borrow_mut().deliver(usize::from(row.is_some()))?;
        let finished = cursor.state() != VertexScanState::Open;
        Ok((row, finished))
    });
    Ok(AuthorizedRowCursor { driver: Some(driver), guard: None, columns, snapshot_seq: at, state: VertexScanState::Open })
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Open a prepared root-vertex stream under this session's fixed policy and
    /// capability. The projection must lead with its unique VId; following cells
    /// may be its properties/repeated identity. Ordinary local predicates and
    /// SKIP/LIMIT reuse the checked native cursor. Temporal patterns retain their
    /// exact cut, no later than the session's pin.
    ///
    /// Opening authenticates before binding/profile/source admission and scans
    /// no candidate. Unsupported probes, edges, aggregates, compound relations
    /// and alternate ordering refuse before source access; no eager fallback.
    /// Native candidate-history accounting differs from eager table admission.
    /// The writer remains independent, but this borrows the session until the
    /// cursor is dropped so its trusted clock and capability cannot be replaced.
    pub fn stream<'q>(
        &'q mut self, cx: &'q QueryCx, prepared: &AuthorizedPreparedRead, params: &GqlParameters,
    ) -> Result<AuthorizedRowCursor<'q>, QueryError> {
        let owner = &self.owner;
        let state = self.state.as_mut().ok_or(QueryError::Authorization(AuthorizationError::ExecutionStopped))?;
        let State { view, capability, branch, policy, clock, last_now_ms, .. } = state;
        let mut guard = PinGuard { pin: view, armed: true };
        let result = open(
            guard.pin.as_ref().ok_or(QueryError::Authorization(AuthorizationError::ExecutionStopped))?,
            capability, clock, last_now_ms, cx, branch, owner, prepared, params, *policy,
        );
        match result {
            Ok(mut cursor) => {
                guard.armed = false;
                cursor.guard = Some(guard);
                Ok(cursor)
            }
            Err(error) => {
                let result = Err(error);
                guard.armed = terminal(&result);
                result
            }
        }
    }
}
