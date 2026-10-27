//! Host-owned external ordering over the existing scoped native sources.
//! Private input frames, scratch owners and completed spool handles never
//! cross the narrowed session boundary; only live-admitted final rows do.

use super::*;
use crate::query::view::{SpoolInput, SpoolRow, ordered_input, ordered_stages};
use crate::{NativeResultSpool, NativeSpoolCursor, NativeSpoolError};
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite};
use fgdb_delta_types::ElementId;
use fgdb_gql::edge_stream::{EdgeScanCursor, EdgeScanSource, EdgeScanState};
use fgdb_gql::scan_stream::{ScanKind, ScanSortTail, ScanState};
use fgdb_gql::spill_set::{AsyncSpillElementSource, AsyncSpillSetPlan};
use fgdb_gql::{GlaExecutionStats, GqlBudgetDimension, GqlExecutionBudget, GqlExecutionStats};
use fgdb_strata::tiered::memory::{SpillError, SpillFile, TrackedBytes};
use fgdb_types::CanonicalPropertyValue;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::{Context, Poll};

type Result<T> = core::result::Result<T, NativeSpoolError>;

/// Physical limits fixed by the trusted host when it creates an ordered
/// session. Execution arguments cannot replace these limits or scratch files.
/// Both files should use children of one memory pool for a combined bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorizedSortLimits {
    pub run_rows: usize,
    pub max_runs: usize,
    pub page_bytes: usize,
    pub max_row_bytes: usize,
    pub max_input_rows: u64,
    /// Reserved in full, without refunds, from each execution's signed work
    /// allowance before encoding or scratch access. Source work is additional.
    pub max_sort_work: u64,
}
impl AuthorizedSortLimits {
    fn validate(self) -> Result<()> {
        if self.run_rows == 0
            || self.page_bytes == 0
            || self.page_bytes > 64 * 1024
            || self.max_row_bytes == 0
        {
            return Err(SpillError::InvalidLimits.into());
        }
        if self.max_runs == 0 {
            return Err(NativeSpoolError::SortRunLimit {
                required: 1,
                limit: 0,
            });
        }
        if self.max_sort_work == 0 {
            return Err(NativeSpoolError::SortWorkLimit {
                attempted: 1,
                limit: 0,
            });
        }
        Ok(())
    }
}

/// A narrowed session with two privately owned, trusted scratch backends.
/// Obtain it only through `Database::authorized_ordered_read_session`.
///
/// The host fixes the database generation, capability, catalog, clock, native
/// policy, spill backends and physical ceilings together. Request code can
/// prepare text and rebind exact-session templates, but cannot install a file
/// that observes private pre-window rows or recover a raw completed spool.
/// Files keep the existing append-only quotas and poisoning laws across calls.
/// Close/drop never evaluates pending work; backend/path retirement belongs to
/// the host's query region. No durable result, resume token or spill of the
/// underlying resident graph is implied.
pub struct AuthorizedOrderedReadSession<'a, R, C, A, B> {
    session: AuthorizedReadSession<'a, R, C>,
    scratch: SpillFile<A>,
    destination: SpillFile<B>,
    limits: AuthorizedSortLimits,
}
impl<R, C, A, B> core::fmt::Debug for AuthorizedOrderedReadSession<'_, R, C, A, B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedOrderedReadSession")
            .field("closed", &self.session.is_closed())
            .field("authority_generation_and_scratch", &"[REDACTED]")
            .finish()
    }
}
impl<R, C, A, B> AuthorizedOrderedReadSession<'_, R, C, A, B> {
    pub fn close(&mut self) {
        self.session.close();
    }
    pub fn is_closed(&self) -> bool {
        self.session.is_closed()
    }
}
impl<R: GraphSymbolResolver, C: FnMut() -> u64, A, B> AuthorizedOrderedReadSession<'_, R, C, A, B> {
    /// Retain only immutable syntax/catalog bindings owned by this session.
    /// No query execution, scratch admission or result-row reservation occurs.
    pub fn prepare(
        &mut self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
    ) -> core::result::Result<AuthorizedPreparedRead, QueryError> {
        self.session.prepare(cx, text, params)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Bind a trusted host-prepared template to a fresh ordered read session.
    ///
    /// This preserves the catalog and source-coherence contract of
    /// `authorized_prepared_read_session`: the host must validate the shared
    /// selector/native template's database, branch and catalog, plus any
    /// connection, authentication-generation and recovery fences. Importing
    /// shares those immutable bindings without parsing or catalog callbacks.
    /// The returned handle belongs only to this new session and pins no graph
    /// independently; execution rebinds every argument under its live permit.
    ///
    /// Authority and template admission precede physical-limit inspection.
    /// Both backends and their fixed ceilings move into the private wrapper;
    /// neither a raw template nor a scratch attachment API is exposed to
    /// narrowed request code. A failed constructor releases its new read pin.
    #[allow(clippy::too_many_arguments)]
    pub fn authorized_prepared_ordered_read_session<'a, R, C, A, B>(
        &self,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
        selector: Arc<PreparedGraphBranchText>,
        native: Arc<PreparedNativeRead>,
        scratch: SpillFile<A>,
        destination: SpillFile<B>,
        limits: AuthorizedSortLimits,
    ) -> Result<(
        AuthorizedOrderedReadSession<'a, R, C, A, B>,
        AuthorizedPreparedRead,
    )>
    where
        R: GraphSymbolResolver,
        C: FnMut() -> u64,
    {
        let (session, prepared) = self
            .authorized_prepared_read_session(
                cx, authority, token, branch, resolver, policy, clock, selector, native,
            )
            .map_err(authorized)?;
        limits.validate()?;
        Ok((
            AuthorizedOrderedReadSession {
                session,
                scratch,
                destination,
                limits,
            },
            prepared,
        ))
    }

    /// Create a pinned authorized session with host-owned external ordering.
    ///
    /// The backend types and files are trusted inputs, like the issuer clock:
    /// they must not expose private scratch bytes to request code. Ownership
    /// moves into the narrowed wrapper and no backend/spool getter is provided.
    /// Authentication precedes physical-limit inspection. A failed constructor
    /// releases its read pin; the host retains its ordinary path cleanup duty.
    #[allow(clippy::too_many_arguments)]
    pub fn authorized_ordered_read_session<'a, R, C, A, B>(
        &self,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
        scratch: SpillFile<A>,
        destination: SpillFile<B>,
        limits: AuthorizedSortLimits,
    ) -> Result<AuthorizedOrderedReadSession<'a, R, C, A, B>>
    where
        R: GraphSymbolResolver,
        C: FnMut() -> u64,
    {
        let session = self
            .authorized_read_session(cx, authority, token, branch, resolver, policy, clock)
            .map_err(authorized)?;
        limits.validate()?;
        Ok(AuthorizedOrderedReadSession {
            session,
            scratch,
            destination,
            limits,
        })
    }
}

impl EmbeddedReadView {
    /// Bind trusted templates and external-sort files to this pinned view.
    /// Authority, source coherence and physical limits follow
    /// [`Database::authorized_prepared_ordered_read_session`]. The host must
    /// retain its recovery fence while this immutable view is in use.
    #[allow(clippy::too_many_arguments)]
    pub fn authorized_prepared_ordered_read_session<'a, R, C, A, B>(
        &self,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
        selector: Arc<PreparedGraphBranchText>,
        native: Arc<PreparedNativeRead>,
        scratch: SpillFile<A>,
        destination: SpillFile<B>,
        limits: AuthorizedSortLimits,
    ) -> Result<(
        AuthorizedOrderedReadSession<'a, R, C, A, B>,
        AuthorizedPreparedRead,
    )>
    where
        R: GraphSymbolResolver,
        C: FnMut() -> u64,
    {
        let (session, prepared) = self
            .authorized_prepared_read_session(
                cx, authority, token, branch, resolver, policy, clock, selector, native,
            )
            .map_err(authorized)?;
        limits.validate()?;
        Ok((
            AuthorizedOrderedReadSession {
                session,
                scratch,
                destination,
                limits,
            },
            prepared,
        ))
    }

    /// Bind trusted external-sort files to this exact authorized generation.
    /// The namespace and live permit checks precede physical-limit admission,
    /// as in [`Database::authorized_ordered_read_session`].
    #[allow(clippy::too_many_arguments)]
    pub fn authorized_ordered_read_session<'a, R, C, A, B>(
        &self,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        resolver: R,
        policy: GqlQueryPolicy,
        clock: C,
        scratch: SpillFile<A>,
        destination: SpillFile<B>,
        limits: AuthorizedSortLimits,
    ) -> Result<AuthorizedOrderedReadSession<'a, R, C, A, B>>
    where
        R: GraphSymbolResolver,
        C: FnMut() -> u64,
    {
        let session = self
            .authorized_read_session(cx, authority, token, branch, resolver, policy, clock)
            .map_err(authorized)?;
        limits.validate()?;
        Ok(AuthorizedOrderedReadSession {
            session,
            scratch,
            destination,
            limits,
        })
    }
}

fn authorized(error: QueryError) -> NativeSpoolError {
    NativeSpoolError::Authorized(Box::new(error))
}
fn terminal_spool(error: &NativeSpoolError) -> bool {
    match error {
        NativeSpoolError::Authorized(error) | NativeSpoolError::Prepare(error) => {
            terminal_error(error)
        }
        _ => false,
    }
}

// Input adapters preserve the native cursor's actual counters and state. The
// rows are private occurrences, never delivered results or fabricated stats.
trait InputCursor: Send {
    fn pull(&mut self) -> Option<Result<SpoolRow>>;
    fn state(&self) -> ScanState;
    fn stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats);
}
struct VertexInput<S, F>(VertexScanCursor<S, F, GraphValueRow>);
impl<S, F> InputCursor for VertexInput<S, F>
where
    S: VertexScanSource<Error = QueryError> + Send,
    F: FnMut() -> core::result::Result<(), QueryError> + Send,
{
    fn pull(&mut self) -> Option<Result<SpoolRow>> {
        self.0.next().map(|row| {
            row.map(SpoolRow::governed)
                .map_err(|e| authorized(scan_error(e)))
        })
    }
    fn state(&self) -> ScanState {
        match self.0.state() {
            VertexScanState::Open => ScanState::Open,
            VertexScanState::Exhausted => ScanState::Exhausted,
            VertexScanState::Closed => ScanState::Closed,
            VertexScanState::Failed => ScanState::Failed,
        }
    }
    fn stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (
            self.0.snapshot_seq(),
            ScanKind::Vertex,
            self.0.row_stats(),
            self.0.evaluator_stats(),
        )
    }
}
/// A pulled set (fgdb-7xqxb): scan and projection share the original native
/// work/scratch meter and signed checkpoint. Only completed projected rows
/// spend the private sort-input allowance; final delivery has its own quota.
/// Projection errors keep their source ordinal and fuse the input cursor,
/// including when an outer LIMIT would discard every result.
struct SetInput<S, F> {
    rows: VertexInput<S, F>,
    pull: GraphSetRowPull,
}
impl<S, F> InputCursor for SetInput<S, F>
where
    S: VertexScanSource<Error = QueryError> + Send,
    F: FnMut() -> core::result::Result<(), QueryError> + Send,
{
    fn pull(&mut self) -> Option<Result<SpoolRow>> {
        self.rows
            .0
            .next_projected(&self.pull, scan_error, |ordinal, column, error| {
                QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Projection {
                    row: usize::try_from(ordinal).unwrap_or(usize::MAX),
                    column,
                    error,
                }))
            })
            .map(|row| row.map(SpoolRow::governed).map_err(authorized))
    }
    fn state(&self) -> ScanState {
        self.rows.state()
    }
    fn stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        self.rows.stats()
    }
}
struct EdgeInput<S, F>(EdgeScanCursor<S, F>);
impl<S, F> InputCursor for EdgeInput<S, F>
where
    S: EdgeScanSource<Error = QueryError> + Send,
    F: FnMut() -> core::result::Result<(), QueryError> + Send,
{
    fn pull(&mut self) -> Option<Result<SpoolRow>> {
        self.0.next().map(|row| {
            row.map(SpoolRow::governed)
                .map_err(|error| authorized(aggregate::edge::row_error(error)))
        })
    }
    fn state(&self) -> ScanState {
        match self.0.state() {
            EdgeScanState::Open => ScanState::Open,
            EdgeScanState::Exhausted => ScanState::Exhausted,
            EdgeScanState::Closed => ScanState::Closed,
            EdgeScanState::Failed => ScanState::Failed,
        }
    }
    fn stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        (
            self.0.snapshot_seq(),
            ScanKind::Edge,
            self.0.row_stats(),
            self.0.evaluator_stats(),
        )
    }
}
struct OrderedInput<'q>(Box<dyn InputCursor + 'q>);
impl SpoolInput for OrderedInput<'_> {
    async fn pull(&mut self) -> Option<Result<SpoolRow>> {
        self.0.pull()
    }
    fn spool_state(&self) -> ScanState {
        self.0.state()
    }
    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        self.0.stats()
    }
}

fn compile_ordered(
    query: &PreparedGraphPattern<GraphValueRow>,
) -> core::result::Result<(RowPlan, ScanSortTail), QueryError> {
    if matches!(
        query.plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    ) {
        EdgeScanPlan::compile_sort_input(query.plan())
            .map(|(plan, tail)| (RowPlan::Edge(plan), tail))
            .map_err(|error| {
                QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Plan(error)))
            })
    } else {
        admit_source_profile(query)?;
        VertexScanPlan::compile_sort_input(query.plan())
            .map(|(plan, tail)| (RowPlan::Vertex(plan), tail))
            .map_err(plan_error)
    }
}
/// A bound ordered plan: the physical input, its sort tail, its cut, the
/// source columns and either the proven pull projection or the unary stages
/// that execute only after the graph leaf's complete native order and page.
type BoundOrder = (
    RowPlan,
    ScanSortTail,
    CommitSeq,
    Vec<String>,
    Option<GraphSetRowPull>,
    Option<AsyncSpillSetPlan>,
);

fn bind_projected_order(
    query: &fgdb_gql::PreparedGraphSet,
    at: CommitSeq,
    facade: crate::NativeReadClass,
) -> core::result::Result<BoundOrder, QueryError> {
    if let Some((pull, tail)) = query.ordered_row_pull() {
        let RowPlan::Vertex(plan) = compile(pull.pattern())? else {
            unreachable!("an ordered row pull is rooted at its one vertex scan")
        };
        return Ok((
            RowPlan::Vertex(plan),
            tail,
            at,
            query.columns().to_vec(),
            Some(pull),
            None,
        ));
    }
    let unsupported = || QueryError::StreamingUnsupported { facade };
    let stages = AsyncSpillSetPlan::compile(query).map_err(|_| unsupported())?;
    // A unary program retains one ordinary scoped graph source. Its canonical
    // order/page must finish before the first stage evaluates a row, including
    // property-only projections whose failure ordinals differ from VId order.
    // Binary, source-free, aggregate and procedure roots have separate owners.
    let pattern = stages.source_pattern().ok_or_else(unsupported)?;
    let (plan, tail) = compile_ordered(pattern)?;
    let columns = pattern.columns().to_vec();
    Ok((plan, tail, at, columns, None, Some(stages)))
}

fn bind_ordered(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    default: CommitSeq,
) -> core::result::Result<BoundOrder, QueryError> {
    let (query, at) = match prepared {
        PreparedNativeRead::Pattern(prepared) => (
            prepared
                .bind_parameters(params)
                .map_err(QueryError::PatternText)?,
            default,
        ),
        PreparedNativeRead::TemporalPattern(prepared) => {
            let bound = prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalText)?;
            (bound.pattern().clone(), bound.as_of())
        }
        // `MATCH (n) RETURN 1 AS value ORDER BY value` (fgdb-7xqxb): a set
        // over one vertex scan projects each row, then the external
        // sort applies the set's own ORDER BY and page.
        PreparedNativeRead::Set(set) => {
            let query = set.bind_parameters(params).map_err(QueryError::SetText)?;
            return bind_projected_order(&query, default, prepared.facade_class());
        }
        // Bind the historical row relation before selecting its physical
        // profile; sorting and delivery must retain that exact source cut.
        PreparedNativeRead::TemporalSet(set) => {
            let query = set
                .bind_parameters(params)
                .map_err(QueryError::TemporalSetText)?;
            return bind_projected_order(query.query(), query.as_of(), prepared.facade_class());
        }
        _ => {
            return Err(QueryError::StreamingUnsupported {
                facade: prepared.facade_class(),
            });
        }
    };
    let (plan, tail) = compile_ordered(&query)?;
    Ok((plan, tail, at, query.columns().to_vec(), None, None))
}

struct OpenedOrder<'q, C> {
    input: OrderedInput<'q>,
    tail: ScanSortTail,
    columns: Vec<String>,
    stages: Option<(AsyncSpillSetPlan, EmbeddedReadView)>,
    at: CommitSeq,
    policy: GqlQueryPolicy,
    execution: Shared<'q, C>,
    guard: PinGuard<'q>,
}
impl<R: GraphSymbolResolver, C: FnMut() -> u64 + Send> AuthorizedReadSession<'_, R, C> {
    fn open_ordered<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
        limits: AuthorizedSortLimits,
    ) -> Result<OpenedOrder<'q, C>> {
        let state = self.state.as_mut().ok_or_else(|| {
            authorized(QueryError::Authorization(
                AuthorizationError::ExecutionStopped,
            ))
        })?;
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
            guard.pin.as_ref().ok_or_else(|| {
                authorized(QueryError::Authorization(
                    AuthorizationError::ExecutionStopped,
                ))
            })?,
            capability,
            clock,
            last_now_ms,
            cx,
            branch,
            &self.owner,
            prepared,
            params,
            *policy,
            bind_ordered,
            |(plan, tail, at, columns, pull, stages), view, cx, execution, policy| {
                execution.borrow_mut().reserve_work(limits.max_sort_work)?;
                let input_policy = GqlQueryPolicy {
                    rows: match policy.rows.max_snapshot_records() {
                        Some(records) => GqlExecutionBudget::new(records, limits.max_input_rows),
                        None => GqlExecutionBudget::result_rows(limits.max_input_rows),
                    },
                    evaluator: policy.evaluator,
                };
                let control = execution.clone();
                let input: Box<dyn InputCursor + Send + 'q> = match plan {
                    RowPlan::Vertex(plan) => {
                        let source = ScopedSource {
                            inner: view
                                .vertex_scan_source(cx, at)
                                .map_err(QueryError::Read)?
                                .with_root_equality(&plan, |key| {
                                    execution.borrow().permit.predicates().allows_property(key)
                                }),
                            execution: execution.clone(),
                        };
                        let rows = VertexInput(VertexScanCursor::new(
                            source,
                            plan,
                            input_policy,
                            move || control.borrow_mut().checkpoint(),
                        ));
                        match pull {
                            Some(pull) => Box::new(SetInput { rows, pull }),
                            None => Box::new(rows),
                        }
                    }
                    RowPlan::Edge(plan) => {
                        let source =
                            aggregate::edge::ordered_source(at, view, cx, execution.clone())?;
                        Box::new(EdgeInput(EdgeScanCursor::new(
                            source,
                            plan,
                            input_policy,
                            move || control.borrow_mut().checkpoint(),
                        )))
                    }
                };
                execution.borrow_mut().checkpoint()?;
                Ok((
                    OrderedInput(input),
                    tail,
                    columns,
                    stages.map(|stages| (stages, view.clone())),
                    at,
                    policy,
                    execution,
                ))
            },
        );
        match result {
            Ok((input, tail, columns, stages, at, policy, execution)) => {
                guard.armed = false;
                Ok(OpenedOrder {
                    input,
                    tail,
                    columns,
                    stages,
                    at,
                    policy,
                    execution,
                    guard,
                })
            }
            Err(error) => {
                guard.armed = terminal_error(&error);
                Err(authorized(error))
            }
        }
    }
}

// A carried element is an identity, never cached authority. Resolve its winner
// at the source cut, admit the whole visibility predicate, then lend only the
// requested field until the evaluator admits its copy. Private history and
// hidden keys poll without spending native or signed allowances.
struct ScopedElements<'s, 'q, C> {
    view: &'s EmbeddedReadView,
    cx: &'q QueryCx,
    at: CommitSeq,
    execution: Shared<'q, C>,
    budget: GqlExecutionBudget,
    records: u64,
}

impl<'s, C: FnMut() -> u64> ScopedElements<'s, '_, C> {
    fn poll(&self) -> Result<()> {
        self.execution.borrow_mut().poll().map_err(authorized)
    }

    fn visible_vertex(&self, vid: VId) -> Result<Option<&'s VertexRow>> {
        let snapshot: &'s crate::Snapshot = &self.view.snapshot;
        let row = source::find_vertex(snapshot, vid, self.at, &mut |_| self.poll())?;
        Ok(row.filter(|row| {
            self.execution
                .borrow()
                .permit
                .predicates()
                .allows_vertex(&row.labels)
        }))
    }

    fn admit_vertex(
        &self,
        row: &VertexRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<()>,
    ) -> Result<()> {
        control(GlaExecutionEvent::Work)?;
        for &label in &row.labels {
            self.poll()?;
            if self
                .execution
                .borrow()
                .permit
                .predicates()
                .allows_label(label)
            {
                control(GlaExecutionEvent::Work)?;
            }
        }
        // Source traversal has finished. No previous scan root can exempt a
        // newly reached vertex from this lookup's signed node admission.
        self.execution.borrow_mut().node().map_err(authorized)
    }

    fn read(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<()>,
    ) -> Result<Option<&'s CanonicalPropertyValue>> {
        self.poll()?;
        if !self
            .execution
            .borrow()
            .permit
            .predicates()
            .allows_property(key)
        {
            return Ok(None);
        }
        let snapshot: &'s crate::Snapshot = &self.view.snapshot;
        let props = match element {
            ElementId::Vertex(vid) => {
                let Some(row) = self.visible_vertex(vid)? else {
                    return Ok(None);
                };
                self.admit_vertex(row, control)?;
                row.props.as_slice()
            }
            ElementId::Edge(eid) => {
                let Some((block, row)) = snapshot.adjacency_index().statement_at_controlled(
                    &snapshot.blocks,
                    eid,
                    self.at,
                    &mut |_| self.poll(),
                )?
                else {
                    return Ok(None);
                };
                let edge = &snapshot.blocks[block][row];
                if !self
                    .execution
                    .borrow()
                    .permit
                    .predicates()
                    .allows_relation(edge.relation)
                {
                    return Ok(None);
                }
                let Some(source) = self.visible_vertex(edge.src)? else {
                    return Ok(None);
                };
                let target = if edge.dst == edge.src {
                    None
                } else {
                    let Some(target) = self.visible_vertex(edge.dst)? else {
                        return Ok(None);
                    };
                    Some(target)
                };
                // Both endpoint predicates precede every logical charge and
                // the payload lookup, including a repeated carried edge.
                control(GlaExecutionEvent::Work)?;
                self.admit_vertex(source, control)?;
                if let Some(target) = target {
                    self.admit_vertex(target, control)?;
                }
                snapshot
                    .block_props
                    .get(block)
                    .and_then(Option::as_ref)
                    .map_or(&[][..], |props| props.props_ref(row))
            }
        };
        let records = self
            .records
            .checked_add(1)
            .ok_or(SpillError::SizeOverflow)?;
        self.budget
            .check(GqlBudgetDimension::SnapshotRecords, records)
            .map_err(|error| authorized(QueryError::Set(GqlQueryError::Rows(error))))?;
        self.records = records;
        control(GlaExecutionEvent::Work)?;
        for (candidate, value) in props {
            self.poll()?;
            if !self
                .execution
                .borrow()
                .permit
                .predicates()
                .allows_property(*candidate)
            {
                continue;
            }
            control(GlaExecutionEvent::Work)?;
            match candidate.cmp(&key) {
                core::cmp::Ordering::Less => {}
                core::cmp::Ordering::Equal => return Ok(Some(value)),
                core::cmp::Ordering::Greater => break,
            }
        }
        Ok(None)
    }

    fn metadata(
        &mut self,
        element: ElementId,
        kind: fgdb_gql::GraphElementMetadata,
        catalog: &fgdb_gql::ReverseSymbolCatalog,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<()>,
    ) -> Result<Option<fgdb_gql::algebra::GraphValue>> {
        use crate::gql_exec::property_maps::{self, MetadataRecord};

        self.poll()?;
        let snapshot: &'s crate::Snapshot = &self.view.snapshot;
        let record = match element {
            ElementId::Vertex(id) => {
                let Some(row) = self.visible_vertex(id)? else {
                    return Ok(None);
                };
                self.admit_vertex(row, control)?;
                MetadataRecord::Vertex(row)
            }
            ElementId::Edge(id) => {
                let Some((block, row)) = snapshot.adjacency_index().statement_at_controlled(
                    &snapshot.blocks,
                    id,
                    self.at,
                    &mut |_| self.poll(),
                )?
                else {
                    return Ok(None);
                };
                let edge = &snapshot.blocks[block][row];
                if !self
                    .execution
                    .borrow()
                    .permit
                    .predicates()
                    .allows_relation(edge.relation)
                {
                    return Ok(None);
                }
                let Some(src) = self.visible_vertex(edge.src)? else {
                    return Ok(None);
                };
                let dst = if edge.dst == edge.src {
                    None
                } else {
                    let Some(dst) = self.visible_vertex(edge.dst)? else {
                        return Ok(None);
                    };
                    Some(dst)
                };
                // As for a scalar property read, no visible counter is spent
                // until the edge and both endpoint predicates have passed.
                control(GlaExecutionEvent::Work)?;
                self.admit_vertex(src, control)?;
                if let Some(dst) = dst {
                    self.admit_vertex(dst, control)?;
                }
                MetadataRecord::Edge(
                    edge.relation,
                    source::edge_properties_at(&snapshot.block_props, block, row),
                )
            }
        };
        let records = self
            .records
            .checked_add(1)
            .ok_or(SpillError::SizeOverflow)?;
        self.budget
            .check(GqlBudgetDimension::SnapshotRecords, records)
            .map_err(|error| authorized(QueryError::Set(GqlQueryError::Rows(error))))?;
        self.records = records;
        property_maps::metadata(
            record,
            kind,
            catalog,
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
            &mut |event| {
                control(match event {
                    source::SourceEvent::Work | source::SourceEvent::SnapshotRecord => {
                        GlaExecutionEvent::Work
                    }
                    source::SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                })
            },
            &|error| {
                authorized(QueryError::Set(GqlQueryError::Source(
                    fgdb_gql::GraphSetExecutionError::Source(GqlError::Read(error)),
                )))
            },
        )
        .map(Some)
    }
}

impl<'s, C: FnMut() -> u64 + Send> AsyncSpillElementSource for ScopedElements<'s, '_, C> {
    type Error = NativeSpoolError;
    type Property = &'s CanonicalPropertyValue;

    async fn element_property(
        &mut self,
        _: ElementId,
        _: PropertyKeyId,
    ) -> Option<Result<Option<Self::Property>>> {
        // The native callback owns both meters. A call that omits it cannot
        // gain an unmetered read route through this private adapter.
        None
    }

    async fn element_property_with_control(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
        control: &mut (impl FnMut(GlaExecutionEvent) -> Result<()> + Send),
    ) -> Option<Result<Option<Self::Property>>> {
        let cx = self.cx;
        Some(cx.with_restriction(|| self.read(element, key, control)))
    }

    async fn element_metadata_with_control(
        &mut self,
        element: ElementId,
        kind: fgdb_gql::GraphElementMetadata,
        catalog: &fgdb_gql::ReverseSymbolCatalog,
        control: &mut (impl FnMut(GlaExecutionEvent) -> Result<()> + Send),
    ) -> Option<Result<Option<fgdb_gql::algebra::GraphValue>>> {
        let cx = self.cx;
        Some(cx.with_restriction(|| self.metadata(element, kind, catalog, control)))
    }
}

// These stages have no buffered graph source. Preserve the authorized set
// error surface for data and controls, including its original row ordinal.
fn stage_error(error: NativeSpoolError) -> NativeSpoolError {
    let error = match error {
        NativeSpoolError::SetExecution(error) => {
            GqlQueryError::Source(error.map_source(|never| match never {}))
        }
        NativeSpoolError::Execute(error) => match *error {
            GqlQueryError::Source(error) => {
                return NativeSpoolError::Execute(Box::new(GqlQueryError::Source(error)));
            }
            GqlQueryError::Rows(error) => GqlQueryError::Rows(error),
            GqlQueryError::Evaluator(error) => GqlQueryError::Evaluator(error),
            GqlQueryError::Interrupted(error) => GqlQueryError::Interrupted(error),
            GqlQueryError::IdentifiedEdgesRequired => GqlQueryError::IdentifiedEdgesRequired,
            GqlQueryError::Data(error) => GqlQueryError::Data(error),
        },
        NativeSpoolError::BufferedExecute(error) => match *error {
            GqlQueryError::Source(error) => {
                return NativeSpoolError::BufferedExecute(Box::new(GqlQueryError::Source(error)));
            }
            GqlQueryError::Rows(error) => GqlQueryError::Rows(error),
            GqlQueryError::Evaluator(error) => GqlQueryError::Evaluator(error),
            GqlQueryError::Interrupted(error) => GqlQueryError::Interrupted(error),
            GqlQueryError::IdentifiedEdgesRequired => GqlQueryError::IdentifiedEdgesRequired,
            GqlQueryError::Data(error) => GqlQueryError::Data(error),
        },
        error => return error,
    };
    authorized(QueryError::Set(error))
}

// Ordinary delivery and each difference endpoint complete the same native
// source and unary stages. The host supplies the remaining cumulative budget;
// neither an endpoint nor a stage creates another signed permit.
#[allow(clippy::too_many_arguments)]
async fn complete_ordered_input<'q, C, A, B>(
    cx: &'q QueryCx,
    input: OrderedInput<'q>,
    tail: ScanSortTail,
    columns: Vec<String>,
    stages: Option<(AsyncSpillSetPlan, EmbeddedReadView)>,
    policy: GqlQueryPolicy,
    execution: Shared<'q, C>,
    scratch: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    limits: AuthorizedSortLimits,
) -> Result<(NativeResultSpool, u64)>
where
    C: FnMut() -> u64 + Send,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    let source_policy = if stages.is_some() {
        GqlQueryPolicy {
            rows: match policy.rows.max_snapshot_records() {
                Some(records) => GqlExecutionBudget::new(records, limits.max_input_rows),
                None => GqlExecutionBudget::result_rows(limits.max_input_rows),
            },
            evaluator: policy.evaluator,
        }
    } else {
        policy
    };
    let (spool, work) = ordered_input(
        cx,
        columns,
        input,
        tail,
        source_policy,
        scratch,
        destination,
        limits.run_rows,
        limits.max_runs,
        limits.page_bytes,
        limits.max_row_bytes,
        limits.max_sort_work,
    )
    .await?;
    let Some((stages, view)) = stages else {
        return Ok((spool, work));
    };
    let mut elements = ScopedElements {
        view: &view,
        cx,
        at: spool.snapshot_seq(),
        execution: execution.clone(),
        budget: policy.rows,
        records: spool.row_stats().snapshot_records,
    };
    let (spool, work) = ordered_stages(
        stages,
        spool,
        &mut elements,
        &mut || execution.borrow_mut().checkpoint().map_err(authorized),
        cx,
        policy,
        scratch,
        destination,
        limits.run_rows,
        limits.max_runs,
        limits.page_bytes,
        limits.max_row_bytes,
        limits.max_input_rows,
        limits.max_sort_work,
        work,
        None,
    )
    .await
    .map_err(stage_error)?;
    Ok((spool.with_snapshot_records(elements.records)?, work))
}

// Physical sort/encoding work is pre-reserved; native stage evaluation charges
// the same live permit per event. Fresh checks surround EVERY poll, including
// Pending and errors. A bounded synchronous section is not preempted midway.
async fn checked_future<T>(
    guard: &mut PinGuard<'_>,
    mut checkpoint: impl FnMut() -> core::result::Result<(), QueryError>,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    let mut future = std::pin::pin!(future);
    poll_fn(|context| {
        guard.armed = true;
        if let Err(error) = checkpoint() {
            guard.armed = terminal_error(&error);
            return Poll::Ready(Err(authorized(error)));
        }
        let outcome = future.as_mut().poll(context);
        if let Err(error) = checkpoint() {
            guard.armed = terminal_error(&error);
            return Poll::Ready(Err(authorized(error)));
        }
        guard.armed = matches!(&outcome, Poll::Ready(Err(error)) if terminal_spool(error));
        outcome
    })
    .await
}

enum Control {
    Checkpoint,
    Deliver(usize),
}
type LiveControl<'q> = Box<dyn FnMut(Control) -> core::result::Result<(), QueryError> + Send + 'q>;

impl<'a, R, C, A, B> AuthorizedOrderedReadSession<'a, R, C, A, B>
where
    R: GraphSymbolResolver,
    C: FnMut() -> u64 + Send,
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send,
{
    /// Execute the query's own external ORDER BY, DISTINCT and final window.
    /// Property-only and hidden-key projections use the ordinary sort compiler;
    /// vertices, fixed-edge joins and admitted probes use the same masked
    /// native sources as authorized streams. Unsupported profiles refuse before
    /// input demand, including under LIMIT 0; no eager fallback is attempted.
    /// Unary Set/TemporalSet projections, scopes, filters and UNWIND complete
    /// each native order/DISTINCT/window barrier before the next stage runs.
    /// Stages consume the same signed work permit and cumulative native budget;
    /// property captures and carried-element lookups share the exact cut and
    /// live capability. Each reached field is scoped before value inspection;
    /// the evaluator reserves its destination copy before cloning any payload.
    ///
    /// Opening binds and admits the exact snapshot synchronously; a refusal
    /// returns immediately without creating a future. The successful future
    /// exposes its column schema and effective snapshot before input demand,
    /// so a host can announce a cancellable stream before starting the sort.
    /// An unpolled future reserves no scratch. The complete private input,
    /// merge sort and final window finish before its cursor can deliver a row. One
    /// permit spans all phases. The fixed maximum sort/encoding allowance is
    /// reserved in full, source work continues on that permit, and the final
    /// encoded byte length is reserved before delivery reads. Neither reserve
    /// is refunded after failure or an early close. Signed rows are charged
    /// only by successful final pulls, never by input or window construction.
    ///
    /// Authority is checked before/after every future poll and final delivery.
    /// A synchronous bounded sort section may finish before observing expiry;
    /// its result/error cannot escape the subsequent live check. This is not
    /// per-comparison time preemption or an exact physical-I/O work bound.
    pub fn execute_ordered<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedOrderExecution<'q, B>> {
        let Self {
            session,
            scratch,
            destination,
            limits,
        } = self;
        let limits = *limits;
        let opened = session.open_ordered(cx, prepared, params, limits)?;
        let columns = opened.stages.as_ref().map_or_else(
            || opened.columns.clone(),
            |(stages, _)| stages.columns().to_vec(),
        );
        let at = opened.at;
        let future = Box::pin(async move {
            let OpenedOrder {
                input,
                tail,
                columns,
                stages,
                at,
                policy,
                execution,
                mut guard,
            } = opened;
            let future = complete_ordered_input(
                cx,
                input,
                tail,
                columns,
                stages,
                policy,
                execution.clone(),
                scratch,
                destination,
                limits,
            );
            let (spool, _) =
                checked_future(&mut guard, || execution.borrow_mut().checkpoint(), future).await?;
            guard.armed = true;
            let admitted = (|| {
                let bytes =
                    u64::try_from(spool.encoded_len()).map_err(|_| SpillError::SizeOverflow)?;
                execution
                    .borrow_mut()
                    .reserve_work(bytes)
                    .map_err(authorized)?;
                execution.borrow_mut().checkpoint().map_err(authorized)
            })();
            if let Err(error) = admitted {
                guard.armed = terminal_spool(&error);
                return Err(error);
            }
            guard.armed = false;
            let columns = spool.columns().to_vec();
            let reader = spool.reader(destination);
            let control: LiveControl<'q> = Box::new(move |action| {
                let mut execution = execution.borrow_mut();
                match action {
                    Control::Checkpoint => execution.checkpoint(),
                    Control::Deliver(rows) => execution.deliver(rows),
                }
            });
            Ok(AuthorizedSpoolCursor {
                reader: Some(reader),
                control: Some(control),
                guard: Some(guard),
                cx,
                columns,
                at,
                state: ScanState::Open,
            })
        });
        Ok(AuthorizedOrderExecution {
            future,
            columns,
            at,
        })
    }
}

/// One admitted external sort, retaining its private input and scratch owners.
///
/// The schema and selected snapshot are available before the first poll, so
/// an async host can announce the stream and accept cancellation while sorting.
/// They disclose no input rows, cardinality, scratch contents or progress.
/// Public transports still need their ordinary live output guards before
/// sending metadata. Awaiting runs the same live-checked sort and produces
/// one final cursor; dropping never finishes the query or exposes a spool.
/// A host must retain this exact future across control-frame pauses, because
/// dropping an in-flight scratch operation preserves its poisoning rules.
pub struct AuthorizedOrderExecution<'q, F> {
    future: Pin<Box<dyn Future<Output = Result<AuthorizedSpoolCursor<'q, F>>> + Send + 'q>>,
    columns: Vec<String>,
    at: CommitSeq,
}
impl<F> core::fmt::Debug for AuthorizedOrderExecution<'_, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedOrderExecution([REDACTED])")
    }
}
impl<F> AuthorizedOrderExecution<'_, F> {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.at
    }
}
impl<'q, F> Future for AuthorizedOrderExecution<'q, F> {
    type Output = Result<AuthorizedSpoolCursor<'q, F>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

/// A single fused position in a completed, privately owned ordered result.
/// Only authenticated canonical frames whose live signed delivery succeeds
/// leave `next_row`. Returned buffers retain their spill-pool reservations.
/// Earlier delivered rows remain valid after a later refusal. Close/drop does
/// not read the suffix, and there is no independent reader or raw spool getter.
pub struct AuthorizedSpoolCursor<'q, F> {
    reader: Option<NativeSpoolCursor<'q, F>>,
    control: Option<LiveControl<'q>>,
    guard: Option<PinGuard<'q>>,
    cx: &'q QueryCx,
    columns: Vec<String>,
    at: CommitSeq,
    state: ScanState,
}
impl<F> core::fmt::Debug for AuthorizedSpoolCursor<'_, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AuthorizedSpoolCursor")
            .field("state", &self.state)
            .field("authority_rows_and_scratch", &"[REDACTED]")
            .finish()
    }
}
impl<F> AuthorizedSpoolCursor<'_, F> {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.at
    }
    pub fn state(&self) -> ScanState {
        self.state
    }
    pub fn close(&mut self) {
        if self.state == ScanState::Open {
            self.state = ScanState::Closed;
            if let Some(guard) = &mut self.guard {
                guard.armed = false;
            }
        }
        self.reader.take();
        self.control.take();
        self.guard.take();
    }
}
impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin> AuthorizedSpoolCursor<'_, F> {
    /// Drive one authenticated frame read under the original QueryCx. A dropped
    /// in-flight call fuses this cursor; an unwound host callback additionally
    /// closes the session. No caller-selected clock or policy is accepted.
    pub async fn next_row(&mut self) -> Result<Option<TrackedBytes>> {
        if self.state != ScanState::Open {
            return Ok(None);
        }
        self.state = ScanState::Failed;
        let mut reader = self
            .reader
            .take()
            .expect("open ordered cursor owns its reader");
        let mut control = self
            .control
            .take()
            .expect("open ordered cursor owns its permit");
        let guard = self
            .guard
            .as_mut()
            .expect("open ordered cursor guards its pin");
        let outcome = checked_future(guard, || control(Control::Checkpoint), async {
            reader
                .next_row(self.cx)
                .await
                .map_err(NativeSpoolError::from)
        })
        .await;
        let outcome = match outcome {
            Ok(row) => {
                guard.armed = true;
                control(Control::Deliver(usize::from(row.is_some())))
                    .map(|()| row)
                    .map_err(authorized)
            }
            Err(error) => Err(error),
        };
        match outcome {
            Ok(row) => {
                guard.armed = false;
                if row.is_none() || reader.state() == ScanState::Exhausted {
                    self.state = ScanState::Exhausted;
                    self.guard.take();
                } else {
                    self.state = ScanState::Open;
                    self.reader = Some(reader);
                    self.control = Some(control);
                }
                Ok(row)
            }
            Err(error) => {
                guard.armed = terminal_spool(&error);
                self.guard.take();
                Err(error)
            }
        }
    }
}

#[cfg(test)]
#[path = "ordered/tests.rs"]
mod tests;

#[path = "ordered/difference.rs"]
mod difference;
pub use difference::{AuthorizedDifferenceCursor, AuthorizedDifferenceExecution};
