//! Capability-authorized asynchronous reads over one buffered publication.
//! Preparation, signed permits and lifetime policy remain the canonical session
//! owners. Only the physical source and its affine return slot differ.

use super::*;
use crate::{
    BufferedOpenError, BufferedQueryRow, BufferedReadError, BufferedReadPublication,
    BufferedReadView, MemoryPool, SymbolCatalogError,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::PropertySelection;
use fgdb_delta_types::catalog::SymbolCatalog;
use fgdb_gql::edge_stream::{
    AsyncEdgeScanCursor, AsyncEdgeScanPlan, EdgeScanPropertyDemand, EdgeScanState,
};
use fgdb_gql::stream::{AsyncVertexScanCursor, AsyncVertexScanPlan, VertexScanPropertyDemand};
use fgdb_gql::symbol_bindings::CatalogSymbolResolver;
use fgdb_strata::tiered::memory::MemoryCharge;
use fgdb_types::CommitCx;

#[path = "buffered/source.rs"]
mod source;
pub use source::OutputGuard as AuthorizedBufferedOutputGuard;
use source::{Position, Source};

/// Typed refusal at the authorized buffered boundary. Query/credential failures
/// retain the canonical QueryError; opening, catalog and cold storage failures
/// remain distinguishable and are never represented as missing graph rows.
#[derive(Debug)]
pub enum AuthorizedBufferedError {
    Query(QueryError),
    Open(BufferedOpenError),
    Catalog(SymbolCatalogError),
    /// The trusted host refused to bind this publication's exact-cut catalog.
    CatalogBinding(String),
    Read(BufferedReadError),
    /// This initial source profile binds its catalog and graph to one cut.
    DifferentCut {
        requested: CommitSeq,
        bound: CommitSeq,
    },
}

impl core::fmt::Display for AuthorizedBufferedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Query(error) => error.fmt(f),
            Self::Open(error) => error.fmt(f),
            Self::Catalog(error) => error.fmt(f),
            Self::CatalogBinding(error) => write!(f, "buffered catalog binding refused: {error}"),
            Self::Read(error) => error.fmt(f),
            Self::DifferentCut { requested, bound } => write!(
                f,
                "buffered session binds cut {}, requested {}",
                bound.0, requested.0
            ),
        }
    }
}

impl core::error::Error for AuthorizedBufferedError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Query(error) => Some(error),
            Self::Open(error) => Some(error),
            Self::Catalog(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::CatalogBinding(_) | Self::DifferentCut { .. } => None,
        }
    }
}

impl From<QueryError> for AuthorizedBufferedError {
    fn from(error: QueryError) -> Self {
        Self::Query(error)
    }
}
impl From<BufferedReadError> for AuthorizedBufferedError {
    fn from(error: BufferedReadError) -> Self {
        Self::Read(error)
    }
}

impl AuthorizedBufferedError {
    fn terminal(&self) -> bool {
        matches!(self, Self::Query(error) if terminal_error(error))
    }
}

/// Opaque backend of the canonical AuthorizedReadSession. It retains one
/// authenticated publication and one fixed catalog/graph cut. No privileged
/// view, source counters or caller-replaceable resolver can be extracted.
pub struct AuthorizedBufferedReadSource<V: Vfs> {
    view: BufferedReadView<V>,
    at: CommitSeq,
}

impl<V: Vfs> core::fmt::Debug for AuthorizedBufferedReadSource<V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedBufferedReadSource([REDACTED])")
    }
}

// Own immutable names, while forwarding every lookup/reverse lookup through
// the existing catalog resolver. This is not another name parser or authority.
struct Catalog(SymbolCatalog);
impl GraphSymbolResolver for Catalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        CatalogSymbolResolver::new(&self.0).resolve_symbol(kind, name)
    }
    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        CatalogSymbolResolver::new(&self.0).time_zones()
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        CatalogSymbolResolver::new(&self.0).reverse_catalog()
    }
    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        CatalogSymbolResolver::new(&self.0).reverse_label(id)
    }
    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        CatalogSymbolResolver::new(&self.0).reverse_relation(id)
    }
}

impl<V: Vfs + Clone> BufferedReadPublication<V> {
    /// Authenticate a live signed read capability before admitting this exact
    /// publication's catalog and graph. The database namespace comes from the
    /// trusted keys used to open this publication, never from request arguments.
    ///
    /// `at` fixes both symbol bindings and graph history. An ordinary query
    /// uses that cut; a temporal query must name the same cut. Open another
    /// session to select a different historical catalog. The publication is
    /// consumed, so no second root selection or resident fallback can occur.
    ///
    /// Use the returned canonical session's prepare and stream methods. This
    /// profile supports native ordered vertex-local and typed single-edge
    /// patterns, selected scalar properties, predicates, SKIP and LIMIT.
    /// Complete native-run images are not enabled by this constructor; its
    /// private source always uses the selected-row bounded history profile.
    /// Unsupported plans refuse before candidate I/O. Initial cold admission
    /// still validates the publication's mixed-scope physical contents and uses
    /// the host MemoryPool/BufferedReadLimits separately from signed visible
    /// budgets. This does not claim hidden-payload I/O or timing noninterference,
    /// an ordinary server routing change, or a cross-process retention lease.
    #[allow(clippy::too_many_arguments)]
    pub async fn into_authorized_read_session<'a, C: FnMut() -> u64>(
        self,
        commit_cx: &CommitCx,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        at: CommitSeq,
        policy: GqlQueryPolicy,
        clock: C,
    ) -> Result<
        AuthorizedReadSession<
            'a,
            impl GraphSymbolResolver + use<V, C>,
            C,
            AuthorizedBufferedReadSource<V>,
        >,
        AuthorizedBufferedError,
    > {
        self.into_authorized_read_session_with_resolver(
            commit_cx,
            cx,
            authority,
            token,
            branch,
            at,
            policy,
            clock,
            |catalog| Ok(Catalog(catalog)),
        )
        .await
    }

    /// Bind a trusted host resolver to the authenticated catalog at `at`.
    /// The callback runs once, after read authorization and catalog decoding,
    /// before graph admission. It can preserve host aliases or refuse a stale
    /// prepared catalog. It cannot select another graph cut or replace the
    /// capability. Fresh validity is checked even when the callback refuses.
    ///
    /// Like the default constructor, this is a trusted publication-opening
    /// boundary. A narrowed session never exposes a resolver-replacement seam.
    #[allow(clippy::too_many_arguments)]
    pub async fn into_authorized_read_session_with_resolver<
        'a,
        R: GraphSymbolResolver,
        C: FnMut() -> u64,
    >(
        self,
        commit_cx: &CommitCx,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        at: CommitSeq,
        policy: GqlQueryPolicy,
        mut clock: C,
        bind_catalog: impl FnOnce(SymbolCatalog) -> Result<R, String>,
    ) -> Result<
        AuthorizedReadSession<'a, R, C, AuthorizedBufferedReadSource<V>>,
        AuthorizedBufferedError,
    > {
        if authority.namespace() != self.security_namespace() {
            return Err(QueryError::Authorization(AuthorizationError::WrongAuthority).into());
        }
        let now = clock();
        let capability = authority
            .verify_at(token, branch, now)
            .map_err(QueryError::Authorization)?;
        let mut last_now_ms = now;
        let (view, resolver) = {
            let permit = capability
                .begin_read_at(branch, now)
                .map_err(QueryError::Authorization)?;
            let mut tracked = || {
                let now = clock();
                last_now_ms = last_now_ms.max(now);
                now
            };
            let mut execution = Execution::new(cx, permit, &mut tracked);
            execution.checkpoint()?;
            if at > self.frontier() {
                return Err(BufferedReadError::BeyondPublication {
                    requested: at,
                    publication: self.frontier(),
                }
                .into());
            }
            let catalog_result = self.symbol_catalog(commit_cx).await;
            execution.poll_fresh()?;
            let catalog = catalog_result
                .map_err(|error| AuthorizedBufferedError::Open(BufferedOpenError::Open(error)))?
                .symbol_catalog_at(at)
                .map_err(AuthorizedBufferedError::Catalog)?;
            execution.checkpoint()?;
            let resolver_result = cx.with_restriction(|| bind_catalog(catalog));
            execution.poll_fresh()?;
            let resolver = resolver_result.map_err(AuthorizedBufferedError::CatalogBinding)?;
            let view_result = self.into_read_view(commit_cx).await;
            // A failed catalog/storage callback cannot hide late credentials.
            execution.poll_fresh()?;
            let view = view_result.map_err(AuthorizedBufferedError::Open)?;
            execution.deliver(0)?;
            (view, resolver)
        };
        Ok(AuthorizedReadSession {
            state: Some(State {
                view: Some(AuthorizedBufferedReadSource { view, at }),
                capability,
                branch: branch.to_owned(),
                resolver,
                policy,
                clock,
                last_now_ms,
                sealed: None,
            }),
            owner: Arc::new(()),
        })
    }

    /// Reattach a trusted host's immutable prepared template to a freshly
    /// authorized buffered publication. `bind_catalog` must verify the frozen
    /// symbol bindings against the catalog at `at`; refusal precedes graph I/O.
    /// Map-field lowering and temporal binding retain their canonical owners.
    /// The returned handle belongs only to the returned session.
    #[allow(clippy::too_many_arguments)]
    pub async fn into_authorized_prepared_read_session_with_resolver<
        'a,
        R: GraphSymbolResolver,
        C: FnMut() -> u64,
    >(
        self,
        commit_cx: &CommitCx,
        cx: &QueryCx,
        authority: &'a Authority,
        token: &CapabilityToken,
        branch: &str,
        at: CommitSeq,
        policy: GqlQueryPolicy,
        clock: C,
        bind_catalog: impl FnOnce(SymbolCatalog) -> Result<R, String>,
        selector: Arc<PreparedGraphBranchText>,
        native: Arc<PreparedNativeRead>,
        lowered: Option<Arc<fgdb_gql::GqlMapArgumentText>>,
    ) -> Result<
        (
            AuthorizedReadSession<'a, R, C, AuthorizedBufferedReadSource<V>>,
            AuthorizedPreparedRead,
        ),
        AuthorizedBufferedError,
    > {
        let mut session = self
            .into_authorized_read_session_with_resolver(
                commit_cx,
                cx,
                authority,
                token,
                branch,
                at,
                policy,
                clock,
                bind_catalog,
            )
            .await?;
        let prepared = session.attach_prepared(cx, selector, native, lowered)?;
        Ok((session, prepared))
    }
}

/// A delivered row retains both its copy reservation and the evaluator's
/// allocation reservation. Projected payloads remain charged after their
/// source record, cursor and session are gone.
pub type AuthorizedBufferedQueryRow =
    BufferedQueryRow<GraphValueRow, AuthorizedBufferedOutputGuard>;

type Checkpoint<'q> = Box<dyn FnMut() -> Result<(), QueryError> + Send + 'q>;
type Returned<V> = Arc<Mutex<Option<AuthorizedBufferedReadSource<V>>>>;

// The return slot never calls user code or drops a backend under its lock.
// Poison recovery only retrieves the affine owner; it cannot restore authority.
fn take_returned<V: Vfs>(slot: &Returned<V>) -> Option<AuthorizedBufferedReadSource<V>> {
    slot.lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
}

struct ReturnGuard<'q, V: Vfs> {
    session: &'q mut Option<AuthorizedBufferedReadSource<V>>,
    returned: Returned<V>,
    terminal: bool,
}
impl<V: Vfs> ReturnGuard<'_, V> {
    fn fail(&mut self) {
        self.terminal = true;
        let returned = take_returned(&self.returned);
        let pinned = self.session.take();
        drop(returned);
        drop(pinned);
    }
}
impl<V: Vfs> Drop for ReturnGuard<'_, V> {
    fn drop(&mut self) {
        let returned = take_returned(&self.returned);
        if self.terminal {
            let pinned = self.session.take();
            drop(returned);
            drop(pinned);
        } else if let Some(backend) = returned {
            *self.session = Some(backend);
        }
        // Destructors run after the lock is gone. Before source construction,
        // an ordinary refusal leaves the still-populated session slot intact.
    }
}

struct PollFence<'g, 'q, V: Vfs> {
    guard: &'g mut ReturnGuard<'q, V>,
    armed: bool,
}
impl<V: Vfs> Drop for PollFence<'_, '_, V> {
    fn drop(&mut self) {
        if self.armed {
            self.guard.fail();
        }
    }
}

enum Native<'q, V: Vfs + Clone, C: FnMut() -> u64 + Send> {
    Vertex(AsyncVertexScanCursor<Source<'q, V, C>, Checkpoint<'q>, GraphValueRow>),
    Edge(AsyncEdgeScanCursor<Source<'q, V, C>, Checkpoint<'q>>),
}

impl<V: Vfs + Clone, C: FnMut() -> u64 + Send> Native<'_, V, C> {
    fn state(&self) -> VertexScanState {
        match self {
            Self::Vertex(cursor) => cursor.state(),
            Self::Edge(cursor) => match cursor.state() {
                EdgeScanState::Open => VertexScanState::Open,
                EdgeScanState::Exhausted => VertexScanState::Exhausted,
                EdgeScanState::Closed => VertexScanState::Closed,
                EdgeScanState::Failed => VertexScanState::Failed,
            },
        }
    }

    async fn next(
        &mut self,
    ) -> Option<Result<AuthorizedBufferedQueryRow, AuthorizedBufferedError>> {
        match self {
            Self::Vertex(cursor) => cursor.next().await.map(|result| {
                result
                    .map(|output| BufferedQueryRow::from_parts(output.into_parts()))
                    .map_err(source::vertex_error)
            }),
            Self::Edge(cursor) => cursor.next().await.map(|result| {
                result
                    .map(|output| BufferedQueryRow::from_parts(output.into_parts()))
                    .map_err(source::edge_error)
            }),
        }
    }
}

/// A real asynchronous native cursor under one signed permit and fixed cut.
/// Each pull drives buffered history until one accepted output; it never pages
/// an eager result. Output rows own host-pool reservations after cursor drop.
///
/// Closing, exhausting, or an ordinary query/budget error permits another
/// execution on this session. Lifetime credential failure, callback unwind, or
/// dropping a polled pending pull closes the session and releases its view.
/// Dropping an unpolled future has no effect. Earlier delivered rows remain
/// delivered when a later pull fails; the failing pull reports its error once.
pub struct AuthorizedBufferedRowCursor<'q, V: Vfs + Clone, C: FnMut() -> u64 + Send> {
    // Source must park its backend before ReturnGuard restores/discards it.
    inner: Option<Native<'q, V, C>>,
    guard: ReturnGuard<'q, V>,
    execution: Shared<'q, C>,
    cx: &'q QueryCx,
    columns: Vec<String>,
    at: CommitSeq,
    state: VertexScanState,
    _metadata: MemoryCharge,
}

impl<V: Vfs + Clone, C: FnMut() -> u64 + Send> core::fmt::Debug
    for AuthorizedBufferedRowCursor<'_, V, C>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedBufferedRowCursor([REDACTED])")
    }
}

impl<V: Vfs + Clone, C: FnMut() -> u64 + Send> AuthorizedBufferedRowCursor<'_, V, C> {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn snapshot_seq(&self) -> CommitSeq {
        self.at
    }
    pub fn state(&self) -> VertexScanState {
        self.state
    }

    pub fn close(&mut self) {
        if self.state == VertexScanState::Open {
            self.state = VertexScanState::Closed;
        }
        self.inner.take();
    }

    pub async fn next(
        &mut self,
    ) -> Option<Result<AuthorizedBufferedQueryRow, AuthorizedBufferedError>> {
        if self.state != VertexScanState::Open {
            return None;
        }
        self.state = VertexScanState::Failed;
        // Declaration order is essential: cancellation drops the native source
        // into its return slot BEFORE the fence discards that slot and pin.
        let mut fence = PollFence {
            guard: &mut self.guard,
            armed: true,
        };
        let mut inner = self
            .inner
            .take()
            .expect("open buffered cursor owns its source");
        let before = self.execution.borrow_mut().poll_fresh();
        let result = match before {
            Ok(()) => self
                .cx
                .with_restriction_async(inner.next())
                .await
                .transpose(),
            Err(error) => Err(error.into()),
        };
        let result = match self.execution.borrow_mut().poll_fresh() {
            Ok(()) => result,
            Err(error) => Err(error.into()),
        };
        let result = result.and_then(|row| {
            self.execution
                .borrow_mut()
                .deliver(usize::from(row.is_some()))?;
            Ok(row)
        });
        match result {
            Ok(row) => {
                fence.armed = false;
                self.state = if row.is_none() || inner.state() != VertexScanState::Open {
                    VertexScanState::Exhausted
                } else {
                    VertexScanState::Open
                };
                if self.state == VertexScanState::Open {
                    self.inner = Some(inner);
                }
                row.map(Ok)
            }
            Err(error) => {
                fence.armed = error.terminal();
                Some(Err(error))
            }
        }
    }
}

enum Plan {
    Vertex(AsyncVertexScanPlan<GraphValueRow>, PropertySelection),
    Edge(AsyncEdgeScanPlan, PropertySelection, RelationId),
}

fn compile(
    pattern: &PreparedGraphPattern<GraphValueRow>,
    scope: &PlannerPredicates,
    facade: crate::NativeReadClass,
) -> Result<Plan, AuthorizedBufferedError> {
    let select = |keys: PropertySelection| {
        let mut visible = PropertySelection::EMPTY;
        for &key in keys.keys() {
            if scope.allows_property(key) {
                visible
                    .insert(key)
                    .expect("a subset of a bounded selection");
            }
        }
        visible
    };
    if matches!(
        pattern.plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    ) {
        let plan = AsyncEdgeScanPlan::compile(pattern.plan()).map_err(|error| {
            QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Plan(error)))
        })?;
        let relation = plan
            .root_relation()
            .one()
            .ok_or(QueryError::StreamingUnsupported { facade })?;
        let keys = match plan.property_demand() {
            EdgeScanPropertyDemand::Topology => PropertySelection::EMPTY,
            EdgeScanPropertyDemand::Properties(keys) => select(keys),
            EdgeScanPropertyDemand::Complete => {
                return Err(QueryError::StreamingUnsupported { facade }.into());
            }
        };
        Ok(Plan::Edge(plan, keys, relation))
    } else {
        let plan = AsyncVertexScanPlan::compile(pattern.plan()).map_err(plan_error)?;
        let keys = match plan.property_demand() {
            VertexScanPropertyDemand::Topology => PropertySelection::EMPTY,
            VertexScanPropertyDemand::Properties(keys) => select(keys),
            VertexScanPropertyDemand::Complete => {
                return Err(QueryError::StreamingUnsupported { facade }.into());
            }
        };
        Ok(Plan::Vertex(plan, keys))
    }
}

fn bind(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    at: CommitSeq,
    scope: &PlannerPredicates,
) -> Result<(Plan, Vec<String>), AuthorizedBufferedError> {
    let (pattern, selected) = match prepared {
        PreparedNativeRead::Pattern(prepared) => (
            prepared
                .bind_parameters(params)
                .map_err(QueryError::PatternText)?,
            at,
        ),
        PreparedNativeRead::TemporalPattern(prepared) => {
            let query = prepared
                .bind_parameters(params)
                .map_err(QueryError::TemporalText)?;
            (query.pattern().clone(), query.as_of())
        }
        _ => {
            return Err(QueryError::StreamingUnsupported {
                facade: prepared.facade_class(),
            }
            .into());
        }
    };
    if selected != at {
        return Err(AuthorizedBufferedError::DifferentCut {
            requested: selected,
            bound: at,
        });
    }
    let plan = compile(&pattern, scope, prepared.facade_class())?;
    Ok((plan, pattern.columns().to_vec()))
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64 + Send, V: Vfs + Clone>
    AuthorizedReadSession<'_, R, C, AuthorizedBufferedReadSource<V>>
{
    /// Bind a canonical prepared template and open its buffered physical source.
    /// This authenticates before binding and refuses unsupported shapes before
    /// candidate reads. Only permitted visible identities enter native/signed
    /// budgets; labels and demanded properties are masked before evaluation.
    pub fn stream<'q>(
        &'q mut self,
        cx: &'q QueryCx,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<AuthorizedBufferedRowCursor<'q, V, C>, AuthorizedBufferedError> {
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
        let returned = Arc::new(Mutex::new(None));
        let mut guard = ReturnGuard {
            session: view,
            returned: returned.clone(),
            terminal: true,
        };
        let outcome: Result<_, AuthorizedBufferedError> = (|| {
            let backend = guard.session.as_ref().ok_or(QueryError::Authorization(
                AuthorizationError::ExecutionStopped,
            ))?;
            let now = clock();
            if now < *last_now_ms {
                return Err(
                    QueryError::Authorization(AuthorizationError::ClockWentBackwards).into(),
                );
            }
            *last_now_ms = now;
            let permit = capability
                .begin_read_at(branch, now)
                .map_err(QueryError::Authorization)?;
            let execution = Shared(Arc::new(Mutex::new(Some(Execution::tracking(
                cx,
                permit,
                clock,
                last_now_ms,
            )))));
            execution.borrow_mut().checkpoint()?;
            let selected = (|| {
                prepared.admit_input(owner, params, &mut execution.borrow_mut())?;
                let selected = prepared
                    .selector
                    .bind_parameters(params)
                    .map_err(selector_error)?;
                check_branch(&selected, branch)?;
                let params = prepared.parameters(selected.parameters())?;
                bind(
                    &prepared.native,
                    &params,
                    backend.at,
                    capability.predicates(),
                )
            })();
            execution.borrow_mut().checkpoint()?;
            let (plan, columns) = selected?;
            let at = backend.at;
            let pool = backend.view.memory_pool().clone();
            let metadata = pool
                .reserve(cx, 1024 + size_of::<Source<'q, V, C>>())
                .map_err(BufferedReadError::Memory)?;
            let mut backend = guard.session.take().expect("admitted backend is pinned");
            let position = match &plan {
                Plan::Vertex(..) => backend
                    .view
                    .partition_mut()
                    .vertex_scan(cx, at)
                    .map(|scan| Position::Vertex(Some(scan.into_state()))),
                Plan::Edge(_, _, relation)
                    if !capability.predicates().allows_relation(*relation) =>
                {
                    Ok(Position::Edge(None))
                }
                Plan::Edge(_, _, relation) => backend
                    .view
                    .partition_mut()
                    .edge_scan_relation(cx, at, *relation)
                    .map(|scan| Position::Edge(Some(scan.into_state()))),
            };
            let position = match position {
                Ok(position) => position,
                Err(error) => {
                    *guard.session = Some(backend);
                    return Err(error.into());
                }
            };
            let keys = match &plan {
                Plan::Vertex(_, keys) | Plan::Edge(_, keys, _) => *keys,
            };
            let source = Source::new(
                backend,
                returned.clone(),
                position,
                keys,
                cx,
                execution.clone(),
            );
            let control = execution.clone();
            let checkpoint: Checkpoint<'q> = Box::new(move || control.borrow_mut().checkpoint());
            let inner = match plan {
                Plan::Vertex(plan, _) => Native::Vertex(AsyncVertexScanCursor::new(
                    source, plan, *policy, checkpoint,
                )),
                Plan::Edge(plan, _, _) => {
                    Native::Edge(AsyncEdgeScanCursor::new(source, plan, *policy, checkpoint))
                }
            };
            Ok((inner, execution, columns, at, metadata))
        })();
        match outcome {
            Ok((inner, execution, columns, at, metadata)) => {
                guard.terminal = false;
                Ok(AuthorizedBufferedRowCursor {
                    inner: Some(inner),
                    guard,
                    execution,
                    cx,
                    columns,
                    at,
                    state: VertexScanState::Open,
                    _metadata: metadata,
                })
            }
            Err(error) => {
                guard.terminal = error.terminal();
                Err(error)
            }
        }
    }
}
