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
    AsyncEdgeJoinCursor, AsyncEdgeJoinPlan, AsyncEdgeProjectionCursor, AsyncEdgeProjectionPlan,
    AsyncEdgeScanCursor, AsyncEdgeScanPlan, AsyncGraphWalkCursor, AsyncGraphWalkPlan,
    EdgeScanPropertySelection, EdgeScanState,
};
use fgdb_gql::stream::{
    AsyncVertexProbeCursor, AsyncVertexProbePlan, AsyncVertexProjectionCursor,
    AsyncVertexProjectionPlan, AsyncVertexScanCursor, AsyncVertexScanPlan,
    VertexScanPropertyDemand,
};
use fgdb_gql::symbol_bindings::CatalogSymbolResolver;
use fgdb_strata::tiered::memory::MemoryCharge;
use fgdb_types::CommitCx;

#[path = "buffered/source.rs"]
mod source;
use fgdb_strata::store::BufferedVertexScanState;
use fgdb_strata::tiered::edge_scan::{BufferedEdgeJoinScanState, BufferedEdgeScanState};
pub use source::OutputGuard as AuthorizedBufferedOutputGuard;
use source::Source;

#[path = "buffered/ordered.rs"]
mod ordered;

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
    /// profile supports native ordered vertex-local, single-edge and fixed-hop
    /// join patterns with one, any or finite relation selectors, selected edge
    /// and endpoint properties, local predicates, paths, SKIP and LIMIT.
    /// Vertex-rooted fixed-hop EXISTS/NOT EXISTS use this same cold incidence
    /// source; a witness never multiplies the admitted outer root.
    /// Each root and child edge is authorized before selected fields decode;
    /// a final root keeps its incidence owner until traversal completes.
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

type ReturnGuard<'q, V> = PinGuard<'q, AuthorizedBufferedReadSource<V>>;

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

type VertexCursor<'q, V, C> =
    AsyncVertexScanCursor<Source<'q, V, C, BufferedVertexScanState>, Checkpoint<'q>, GraphValueRow>;
type ProjectedVertexCursor<'q, V, C> =
    AsyncVertexProjectionCursor<Source<'q, V, C, BufferedVertexScanState>, Checkpoint<'q>>;
type EdgeCursor<'q, V, C> =
    AsyncEdgeScanCursor<Source<'q, V, C, BufferedEdgeScanState>, Checkpoint<'q>>;
type ProjectedEdgeCursor<'q, V, C> =
    AsyncEdgeProjectionCursor<Source<'q, V, C, BufferedEdgeScanState>, Checkpoint<'q>>;
type JoinCursor<'q, V, C> =
    AsyncEdgeJoinCursor<Source<'q, V, C, BufferedEdgeJoinScanState>, Checkpoint<'q>>;
type VertexProbeCursor<'q, V, C> = AsyncVertexProbeCursor<
    Source<'q, V, C, BufferedEdgeJoinScanState>,
    Checkpoint<'q>,
    GraphValueRow,
>;
type WalkCursor<'q, V, C> =
    AsyncGraphWalkCursor<Source<'q, V, C, BufferedEdgeJoinScanState>, Checkpoint<'q>>;

enum Native<'q, V: Vfs + Clone, C: FnMut() -> u64 + Send> {
    Vertex(Box<VertexCursor<'q, V, C>>),
    ProjectedVertex(Box<ProjectedVertexCursor<'q, V, C>>),
    Edge(Box<EdgeCursor<'q, V, C>>),
    ProjectedEdge(Box<ProjectedEdgeCursor<'q, V, C>>),
    Join(Box<JoinCursor<'q, V, C>>),
    VertexProbe(Box<VertexProbeCursor<'q, V, C>>),
    // Walks are private input for the external ordering owner. Their DFS/BFS
    // occurrence order must never be exposed as an ordinary ordered stream.
    Walk(Box<WalkCursor<'q, V, C>>),
}

impl<V: Vfs + Clone, C: FnMut() -> u64 + Send> Native<'_, V, C> {
    fn state(&self) -> VertexScanState {
        match self {
            Self::Vertex(cursor) => cursor.state(),
            Self::ProjectedVertex(cursor) => cursor.state(),
            Self::VertexProbe(cursor) => cursor.state(),
            Self::Walk(cursor) => match cursor.state() {
                EdgeScanState::Open => VertexScanState::Open,
                EdgeScanState::Exhausted => VertexScanState::Exhausted,
                EdgeScanState::Closed => VertexScanState::Closed,
                EdgeScanState::Failed => VertexScanState::Failed,
            },
            Self::Edge(cursor) => match cursor.state() {
                EdgeScanState::Open => VertexScanState::Open,
                EdgeScanState::Exhausted => VertexScanState::Exhausted,
                EdgeScanState::Closed => VertexScanState::Closed,
                EdgeScanState::Failed => VertexScanState::Failed,
            },
            Self::ProjectedEdge(cursor) => match cursor.state() {
                EdgeScanState::Open => VertexScanState::Open,
                EdgeScanState::Exhausted => VertexScanState::Exhausted,
                EdgeScanState::Closed => VertexScanState::Closed,
                EdgeScanState::Failed => VertexScanState::Failed,
            },
            Self::Join(cursor) => match cursor.state() {
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
            Self::ProjectedVertex(cursor) => cursor.next().await.map(|result| {
                result
                    .map(|output| BufferedQueryRow::from_parts(output.into_parts()))
                    .map_err(source::projection_error)
            }),
            Self::VertexProbe(cursor) => cursor.next().await.map(|result| {
                result
                    .map(|output| BufferedQueryRow::from_parts(output.into_parts()))
                    .map_err(source::vertex_error)
            }),
            Self::Walk(cursor) => cursor.next().await.map(|result| {
                result
                    .map(|output| BufferedQueryRow::from_parts(output.into_parts()))
                    .map_err(source::edge_error)
            }),
            Self::Edge(cursor) => cursor.next().await.map(|result| {
                result
                    .map(|output| BufferedQueryRow::from_parts(output.into_parts()))
                    .map_err(source::edge_error)
            }),
            Self::ProjectedEdge(cursor) => cursor.next().await.map(|result| {
                result
                    .map(|output| BufferedQueryRow::from_parts(output.into_parts()))
                    .map_err(source::projection_error)
            }),
            Self::Join(cursor) => cursor.next().await.map(|result| {
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

enum Scan {
    Vertex(AsyncVertexScanPlan<GraphValueRow>),
    ProjectedVertex(AsyncVertexProjectionPlan),
    Edge(AsyncEdgeScanPlan),
    ProjectedEdge(AsyncEdgeProjectionPlan),
    Join(AsyncEdgeJoinPlan),
    VertexProbe(AsyncVertexProbePlan<GraphValueRow>),
    Walk(AsyncGraphWalkPlan),
}

struct Plan {
    scan: Scan,
    keys: PropertySelection,
    endpoint_keys: PropertySelection,
}

impl Plan {
    fn vertex(plan: AsyncVertexScanPlan<GraphValueRow>, keys: PropertySelection) -> Self {
        Self {
            scan: Scan::Vertex(plan),
            keys,
            endpoint_keys: PropertySelection::EMPTY,
        }
    }

    fn projected_vertex(plan: AsyncVertexProjectionPlan, keys: PropertySelection) -> Self {
        Self {
            scan: Scan::ProjectedVertex(plan),
            keys,
            endpoint_keys: PropertySelection::EMPTY,
        }
    }

    fn edge(plan: AsyncEdgeScanPlan, selection: EdgeScanPropertySelection) -> Self {
        Self {
            scan: Scan::Edge(plan),
            keys: selection.edge,
            endpoint_keys: selection.endpoints,
        }
    }

    fn projected_edge(plan: AsyncEdgeProjectionPlan, selection: EdgeScanPropertySelection) -> Self {
        Self {
            scan: Scan::ProjectedEdge(plan),
            keys: selection.edge,
            endpoint_keys: selection.endpoints,
        }
    }

    fn join(plan: AsyncEdgeJoinPlan, selection: EdgeScanPropertySelection) -> Self {
        Self {
            scan: Scan::Join(plan),
            keys: selection.edge,
            endpoint_keys: selection.endpoints,
        }
    }

    fn vertex_probe(
        plan: AsyncVertexProbePlan<GraphValueRow>,
        selection: EdgeScanPropertySelection,
    ) -> Self {
        Self {
            scan: Scan::VertexProbe(plan),
            keys: selection.edge,
            endpoint_keys: selection.endpoints,
        }
    }

    fn walk(plan: AsyncGraphWalkPlan, selection: EdgeScanPropertySelection) -> Self {
        Self {
            scan: Scan::Walk(plan),
            keys: selection.edge,
            endpoint_keys: selection.endpoints,
        }
    }

    /// Reserve the selected cursor's fixed allocation before opening any scan.
    /// On refusal the caller still owns its original backend. Successful
    /// sources park it in the return slot when they close or unwind.
    fn open<'q, V: Vfs + Clone, C: FnMut() -> u64 + Send>(
        self,
        slot: &mut Option<AuthorizedBufferedReadSource<V>>,
        returned: &Returned<V>,
        cx: &'q QueryCx,
        execution: &Shared<'q, C>,
        policy: GqlQueryPolicy,
    ) -> Result<(Native<'q, V, C>, MemoryCharge), BufferedReadError> {
        let backend = slot.as_mut().expect("opening owns its buffered backend");
        let cursor_bytes = match &self.scan {
            Scan::Vertex(_) => size_of::<VertexCursor<'q, V, C>>(),
            Scan::ProjectedVertex(_) => size_of::<ProjectedVertexCursor<'q, V, C>>(),
            Scan::Edge(_) => size_of::<EdgeCursor<'q, V, C>>(),
            Scan::ProjectedEdge(_) => size_of::<ProjectedEdgeCursor<'q, V, C>>(),
            Scan::Join(_) => size_of::<JoinCursor<'q, V, C>>(),
            Scan::VertexProbe(_) => size_of::<VertexProbeCursor<'q, V, C>>(),
            Scan::Walk(_) => size_of::<WalkCursor<'q, V, C>>(),
        };
        let metadata = backend
            .view
            .memory_pool()
            .reserve(cx, 1024 + cursor_bytes)
            .map_err(BufferedReadError::Memory)?;
        let at = backend.at;
        let control = execution.clone();
        let checkpoint: Checkpoint<'q> = Box::new(move || control.borrow_mut().checkpoint());
        let native = match self.scan {
            Scan::Vertex(plan) => {
                let state = backend
                    .view
                    .partition_mut()
                    .vertex_scan(cx, at)?
                    .into_state();
                let source = Source::new(
                    slot.take().expect("admitted backend"),
                    returned.clone(),
                    Some(state),
                    self.keys,
                    self.endpoint_keys,
                    cx,
                    execution.clone(),
                );
                Native::Vertex(Box::new(AsyncVertexScanCursor::new(
                    source, plan, policy, checkpoint,
                )))
            }
            Scan::ProjectedVertex(plan) => {
                let state = backend
                    .view
                    .partition_mut()
                    .vertex_scan(cx, at)?
                    .into_state();
                let source = Source::new(
                    slot.take().expect("admitted backend"),
                    returned.clone(),
                    Some(state),
                    self.keys,
                    self.endpoint_keys,
                    cx,
                    execution.clone(),
                );
                Native::ProjectedVertex(Box::new(AsyncVertexProjectionCursor::new(
                    source, plan, policy, checkpoint,
                )))
            }
            Scan::Edge(plan) => {
                let relation = plan.root_relation();
                let position =
                    if excludes_all_relations(relation, execution.borrow().permit.predicates()) {
                        None
                    } else {
                        let partition = backend.view.partition_mut();
                        let scan = match relation.one() {
                            Some(relation) => partition.edge_scan_relation(cx, at, relation),
                            None => partition.edge_scan(cx, at),
                        }?;
                        Some(scan.into_state())
                    };
                let source = Source::new(
                    slot.take().expect("admitted backend"),
                    returned.clone(),
                    position,
                    self.keys,
                    self.endpoint_keys,
                    cx,
                    execution.clone(),
                );
                Native::Edge(Box::new(AsyncEdgeScanCursor::new(
                    source, plan, policy, checkpoint,
                )))
            }
            Scan::ProjectedEdge(plan) => {
                let relation = plan.root_relation();
                let position =
                    if excludes_all_relations(relation, execution.borrow().permit.predicates()) {
                        None
                    } else {
                        let partition = backend.view.partition_mut();
                        let scan = match relation.one() {
                            Some(relation) => partition.edge_scan_relation(cx, at, relation),
                            None => partition.edge_scan(cx, at),
                        }?;
                        Some(scan.into_state())
                    };
                let source = Source::new(
                    slot.take().expect("admitted backend"),
                    returned.clone(),
                    position,
                    self.keys,
                    self.endpoint_keys,
                    cx,
                    execution.clone(),
                );
                Native::ProjectedEdge(Box::new(AsyncEdgeProjectionCursor::new(
                    source, plan, policy, checkpoint,
                )))
            }
            Scan::Join(plan) => {
                let relation = plan.root_relation();
                let position =
                    if excludes_all_relations(relation, execution.borrow().permit.predicates()) {
                        None
                    } else {
                        let partition = backend.view.partition_mut();
                        let scan = match relation.one() {
                            Some(relation) => partition.edge_scan_relation(cx, at, relation),
                            None => partition.edge_scan(cx, at),
                        }?;
                        Some(scan.into_join_scan().into_state())
                    };
                let source = Source::new(
                    slot.take().expect("admitted backend"),
                    returned.clone(),
                    position,
                    self.keys,
                    self.endpoint_keys,
                    cx,
                    execution.clone(),
                );
                Native::Join(Box::new(AsyncEdgeJoinCursor::new(
                    source, plan, policy, checkpoint,
                )))
            }
            Scan::VertexProbe(plan) => {
                // The joined driver keeps independent root and incidence
                // positions over the same cut and cumulative physical meter.
                // Vertex-only/empty-edge graphs still own real root rows.
                let position = backend
                    .view
                    .partition_mut()
                    .edge_scan(cx, at)?
                    .into_join_scan()
                    .into_state();
                let source = Source::new(
                    slot.take().expect("admitted backend"),
                    returned.clone(),
                    Some(position),
                    self.keys,
                    self.endpoint_keys,
                    cx,
                    execution.clone(),
                );
                Native::VertexProbe(Box::new(AsyncVertexProbeCursor::new(
                    source, plan, policy, checkpoint,
                )))
            }
            Scan::Walk(plan) => {
                // Root vertices are independent of the edge selector: even a
                // denied relation can have valid zero-hop results and isolates.
                // Every real hop is admitted by the scoped incidence source.
                let position = backend
                    .view
                    .partition_mut()
                    .edge_scan(cx, at)?
                    .into_join_scan()
                    .into_state();
                let source = Source::new(
                    slot.take().expect("admitted backend"),
                    returned.clone(),
                    Some(position),
                    self.keys,
                    self.endpoint_keys,
                    cx,
                    execution.clone(),
                );
                Native::Walk(Box::new(AsyncGraphWalkCursor::new(
                    source, plan, policy, checkpoint,
                )))
            }
        };
        Ok((native, metadata))
    }
}

// A finite selector with no allowed member needs no physical source. Any
// retains per-candidate masking under the separate host physical limits.
fn excludes_all_relations(relation: EdgeRelation, scope: &PlannerPredicates) -> bool {
    relation.relations().is_some_and(|relations| {
        relations
            .iter()
            .all(|relation| !scope.allows_relation(*relation))
    })
}

fn compile(
    pattern: &PreparedGraphPattern<GraphValueRow>,
    scope: &PlannerPredicates,
    facade: crate::NativeReadClass,
) -> Result<Plan, AuthorizedBufferedError> {
    let select = |keys| select_properties(keys, scope);
    if matches!(
        pattern.plan().operators().first(),
        Some(GlaOperator::ScanEdges { .. })
    ) {
        // Physical preparation is complete before opening either source. A
        // failed runtime read never retries through another implementation.
        if let Ok(plan) = AsyncEdgeScanPlan::compile(pattern.plan()) {
            let mut keys = plan
                .selected_property_demand()
                .ok_or(QueryError::StreamingUnsupported { facade })?;
            keys.edge = select(keys.edge);
            keys.endpoints = select(keys.endpoints);
            Ok(Plan::edge(plan, keys))
        } else {
            let plan = AsyncEdgeJoinPlan::compile(pattern.plan()).map_err(|error| {
                QueryError::EdgeStream(GqlQueryError::Source(EdgeScanError::Plan(error)))
            })?;
            let mut keys = plan
                .selected_property_demand()
                .ok_or(QueryError::StreamingUnsupported { facade })?;
            keys.edge = select(keys.edge);
            keys.endpoints = select(keys.endpoints);
            Ok(Plan::join(plan, keys))
        }
    } else if let Ok(plan) = AsyncVertexScanPlan::compile(pattern.plan()) {
        let keys = match plan.property_demand() {
            VertexScanPropertyDemand::Topology => PropertySelection::EMPTY,
            VertexScanPropertyDemand::Properties(keys) => select(keys),
            VertexScanPropertyDemand::Complete => {
                return Err(QueryError::StreamingUnsupported { facade }.into());
            }
        };
        Ok(Plan::vertex(plan, keys))
    } else {
        let plan = AsyncVertexProbePlan::compile(pattern.plan()).map_err(plan_error)?;
        let mut keys = plan
            .selected_property_demand()
            .ok_or(QueryError::StreamingUnsupported { facade })?;
        keys.edge = select(keys.edge);
        keys.endpoints = select(keys.endpoints);
        Ok(Plan::vertex_probe(plan, keys))
    }
}

fn select_properties(keys: PropertySelection, scope: &PlannerPredicates) -> PropertySelection {
    let mut visible = PropertySelection::EMPTY;
    for &key in keys.keys() {
        if scope.allows_property(key) {
            visible
                .insert(key)
                .expect("a subset of a bounded selection");
        }
    }
    visible
}

fn compile_projection(
    query: &fgdb_gql::PreparedGraphSet,
    scope: &PlannerPredicates,
    facade: crate::NativeReadClass,
) -> Result<(Plan, Vec<String>), AuthorizedBufferedError> {
    // Complete compiler demand is bounded BEFORE authorization masks it.
    // Denied keys never reach cold decoding or the retained row reader.
    if let Ok(plan) = AsyncVertexProjectionPlan::compile(query) {
        let keys = match plan.property_demand() {
            VertexScanPropertyDemand::Topology => PropertySelection::EMPTY,
            VertexScanPropertyDemand::Properties(keys) => select_properties(keys, scope),
            VertexScanPropertyDemand::Complete => {
                return Err(QueryError::StreamingUnsupported { facade }.into());
            }
        };
        return Ok((Plan::projected_vertex(plan, keys), query.columns().to_vec()));
    }
    let plan = AsyncEdgeProjectionPlan::compile(query)
        .map_err(|_| QueryError::StreamingUnsupported { facade })?;
    let mut keys = plan.selected_property_demand();
    keys.edge = select_properties(keys.edge, scope);
    keys.endpoints = select_properties(keys.endpoints, scope);
    Ok((Plan::projected_edge(plan, keys), query.columns().to_vec()))
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
        PreparedNativeRead::Set(template) => {
            let query = template
                .bind_parameters(params)
                .map_err(QueryError::SetText)?;
            return compile_projection(&query, scope, prepared.facade_class());
        }
        PreparedNativeRead::TemporalSet(template) => {
            let query = template
                .bind_parameters(params)
                .map_err(QueryError::TemporalSetText)?;
            if query.as_of() != at {
                return Err(AuthorizedBufferedError::DifferentCut {
                    requested: query.as_of(),
                    bound: at,
                });
            }
            return compile_projection(query.query(), scope, prepared.facade_class());
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
            pin: view,
            returned: Some(returned.clone()),
            armed: true,
        };
        let mut open = |clock: &'q mut C, last_now_ms: &'q mut u64| {
            let backend = guard.pin.as_ref().ok_or(QueryError::Authorization(
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
            let (inner, metadata) = plan.open(guard.pin, &returned, cx, &execution, *policy)?;
            Ok((inner, execution, columns, at, metadata))
        };
        let outcome: Result<_, AuthorizedBufferedError> = open(clock, last_now_ms);
        match outcome {
            Ok((inner, execution, columns, at, metadata)) => {
                guard.armed = false;
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
                guard.armed = error.terminal();
                Err(error)
            }
        }
    }
}
