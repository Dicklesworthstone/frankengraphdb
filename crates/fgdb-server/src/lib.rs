//! `fgdbd`: FrankenGraphDB's server posture (plan §13 item 2, Appendix D).
//!
//! The server composes the embedded engine behind `fgdb-protocol`'s FGP
//! connection machine over asupersync TCP or TLS 1.3. It reimplements none of the
//! engine: every statement runs through the same capability-authorized
//! sessions an embedded host would use, constructed fresh per statement from
//! the connection's Warden capability, so signature, scope, expiry, signed
//! budgets and issuer retirement are rechecked for every statement and
//! security applies before expansion (FG-INV-20), never as a result filter.
//!
//! FGP also rechecks the selected issuer's cooperative retirement fence and
//! token expiry before every physical output write and flush, including READY,
//! credit-delayed results, subscription batches, and resumed partial writes.
//! These checks use the existing host clock; they do not implement the plan's
//! durable audit release, time-authority evidence, or revocation barriers.
//!
//! # What is served (a subset of Appendix D, never a substitute)
//!
//! - The handshake `HELLO -> HELLO_ACK -> AUTH -> AUTH_OK -> SELECT_DATABASE
//!   -> READY`, with the session binding derived from the HELLO/HELLO_ACK/AUTH
//!   transcript under a per-process server secret before AUTH_OK is encoded,
//!   plus live credential rotation via `AUTH_REFRESH -> AUTH_REFRESHED`.
//!   A nonexistent and an unauthorized database share one failure surface.
//! - `EXECUTE` of one GQL read (`Read`, `ReadStream`, or `ReadOrdered` when
//!   `--query-scratch-dir` is configured), one autocommit write program
//!   (`Write`), or one live changefeed subscription (`Subscribe` delivering
//!   `SUBSCRIPTION_BATCH` frames and `SUBSCRIPTION_RESET` on authoritative
//!   recovery), plus `EXECUTE_BATCH` for atomic parameterized no-`RETURN`
//!   writes and connection-scoped reusable read templates via
//!   `PREPARE`/`PREPARED`, `EXECUTE_PREPARED`, and
//!   `RELEASE_PREPARED`/`PREPARED_RELEASED`.
//!   Authorized write chains carry bindings across `WITH`, compose creation
//!   and relationship `MERGE` steps, and support terminal `SET`, `REMOVE`,
//!   and `DELETE`. Vertex `MERGE` chain steps are refused explicitly.
//! - Results as the session-owned, ephemeral `SNAPSHOT_RESULT_CHUNK` /
//!   `SNAPSHOT_RESULT_END` class on a server-minted child stream, under
//!   per-stream byte-and-row flow credit replenished by `WINDOW_UPDATE`.
//! - `QUERY_CANCEL` (sender control only), `PING`/`PONG`, `DRAIN`/`GOODBYE`,
//!   and supervised authoritative recovery per served database.
//! - HTTP/1.1 JSON (`/v1/health`, `/v1/databases/<name>/schema`, `/query`,
//!   `/write`, `/write-batch`) and the Bolt-compat profile
//!   (`BoltCompatProfileV1`: reads, explicit read-only transactions, and
//!   bounded no-`RETURN` autocommit writes) over the same authorized execution
//!   boundary; optional TLS 1.3 protects every configured listener.
//! - Trusted embedding hosts can compact and scrub a served database through
//!   [`Server::compact_database`] and [`Server::scrub_database`] while its
//!   writer lease remains held. Maintenance shares ordinary write admission,
//!   snapshot publication, and supervised recovery. These host operations are
//!   not remote endpoints or rights granted by a query capability. Operators
//!   can opt in per database to fixed compaction/scrub intervals in `fgdbd`;
//!   [`Server::run_maintenance`] drives the same operations under host ownership.
//!
//! Not served, and refused with a typed error rather than approximated: the
//! durable `PublishedResultStream` class with `RESULT_ACK`/`RESULT_RELEASE`,
//! prepared writes, explicit multi-statement write transactions with
//! ownership/reattach, durable or capability-masked subscriptions, and HTTP/2,
//! gRPC and WebSocket adapters.
//! Because every result is ephemeral, a disconnect can lose undelivered rows
//! but never a commit: a write's outcome is decided before its first frame.
//!
//! The engine has no durable catalog yet, so each served database resolves
//! names through operator-declared [`Symbols`], exactly like the CLI flags.

#![forbid(unsafe_code)]
// The authorized write path's future (execute::write -> AuthorizedWriteSession
// -> native script session -> RestrictedFuture) nests deeper than the default
// limit of 128 when proving it Send, which the recursion_depth_exceeding_limit
// future-incompatibility lint (rust-lang/rust#159228) rejects under -D warnings.
#![recursion_limit = "256"]

mod bolt;
mod commits;
mod connection;
mod convert;
mod execute;
mod http;
mod keys;
mod maintenance;
mod ordered;
mod recovery;
mod shutdown;
mod symbols;
mod tls;

pub use keys::{KeyFileError, parse_key_lines, read_database_keys, read_issuer_key};
pub use maintenance::{MaintenanceEvent, MaintenanceSchedule, MaintenanceScheduleError};
pub use ordered::OrderedReadConfig;
pub use recovery::DatabaseStatus;
pub use shutdown::Shutdown;
pub use symbols::{SymbolConflict, Symbols};
pub use tls::{TlsConfig, TlsConfigError};

use asupersync::Cx;
use asupersync::http::h1::server::{HostPolicy, Http1Config, Http1Server};
use asupersync::http::h1::types::Response;
use asupersync::net::TcpListener;
use asupersync::runtime::TaskHandle;
use asupersync::security::key::AuthKey;
use asupersync::server::shutdown::ShutdownSignal;
use core::future::poll_fn;
use core::task::Poll;
use fgdb::{Database, DatabaseKeys};
use fgdb_delta_types::{RelationId, SchemaEpoch};
use fgdb_gql::{GqlQueryPolicy, GraphWriteProgramPolicy};
use fgdb_protocol::MAX_HEADER_LEN;
use fgdb_protocol::transport::DuplexIo;
use fgdb_types::PurposeContexts;
use fgdb_warden::{Authority, CapabilityToken, Grant, Restriction};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// The branch every served capability is scoped to: the engine serves one
/// trunk coordinate per database today.
pub const TRUNK: &str = "main";
/// The graph name bound into every served issuer's signed identity.
pub const GRAPH_NAME: &str = "main";

/// The highest FGP body version this server speaks.
pub const SERVED_VERSION: u16 = fgdb_protocol::PROTOCOL_VERSION;

/// Connection-level ceilings. Every value is a real bound, never "unlimited".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerLimits {
    /// The largest frame the server accepts; it also never sends a frame
    /// larger than the smaller of this and the client's HELLO limit.
    pub max_frame_len: u32,
    /// Flow credit a result stream starts with.
    pub initial_window_bytes: u64,
    pub initial_window_rows: u64,
    /// The most credit a stream may accumulate.
    pub max_window_bytes: u64,
    pub max_window_rows: u64,
    /// Concurrent connections; further connections are closed on accept.
    pub max_connections: usize,
}

impl Default for ServerLimits {
    fn default() -> Self {
        Self {
            max_frame_len: 1 << 20,
            initial_window_bytes: 4 << 20,
            initial_window_rows: 1 << 16,
            max_window_bytes: 64 << 20,
            max_window_rows: 1 << 20,
            max_connections: 256,
        }
    }
}

impl ServerLimits {
    fn validate(&self) -> Result<(), ServerError> {
        let frame = u64::from(self.max_frame_len);
        if (self.max_frame_len as usize) < MAX_HEADER_LEN + 1024
            || self.initial_window_bytes < frame
            || self.initial_window_rows == 0
            || self.max_window_bytes < self.initial_window_bytes
            || self.max_window_rows < self.initial_window_rows
            || self.max_connections == 0
        {
            return Err(ServerError::InvalidLimits);
        }
        Ok(())
    }
}

/// Everything the operator decides about one served database.
pub struct DatabaseConfig {
    /// The selector clients name in SELECT_DATABASE.
    pub name: String,
    pub keys: DatabaseKeys,
    /// The Warden root key whose capabilities this database accepts.
    pub issuer_key: AuthKey,
    /// Bumping the policy epoch invalidates every token issued under the old one.
    pub policy_epoch: u64,
    pub symbols: Symbols,
    /// The native mutation coordinate (the CLI's `--write-relation`).
    pub write_relation: RelationId,
    pub query_policy: GqlQueryPolicy,
    pub write_policy: GraphWriteProgramPolicy,
    /// Host ceiling on expanded statements per write program.
    pub max_statements: usize,
    /// Subscription registrations this database admits over the server's
    /// lifetime: a registration lives as long as the open database.
    pub max_subscriptions: usize,
    /// External ordered FGP reads are disabled unless the host supplies a
    /// trusted scratch directory and physical limits. Request bodies cannot
    /// replace those files, limits or the database's shared resident pool.
    pub ordered_reads: Option<OrderedReadConfig>,
}

impl DatabaseConfig {
    /// The `fgdb` CLI's default query limits, in [`Self::set_query_limits`] order.
    pub const DEFAULT_QUERY_LIMITS: [u64; 4] = [100_000, 100_000, 10_000_000, 10_000_000];

    /// The defaults the `fgdb` CLI uses for one invocation.
    #[must_use]
    pub fn new(name: impl Into<String>, keys: DatabaseKeys, issuer_key: AuthKey) -> Self {
        let [records, rows, work, scratch] = Self::DEFAULT_QUERY_LIMITS;
        let query_policy = GqlQueryPolicy::new(records, rows, work, scratch);
        Self {
            name: name.into(),
            keys,
            issuer_key,
            policy_epoch: 1,
            symbols: Symbols::new(),
            write_relation: RelationId(1),
            query_policy,
            write_policy: GraphWriteProgramPolicy::new(query_policy, 100_000, 100_000, 100_000),
            max_statements: 64,
            max_subscriptions: 64,
            ordered_reads: None,
        }
    }

    /// Replace the per-statement query limits: snapshot records, result rows,
    /// work units and scratch entries, in [`GqlQueryPolicy::new`] order. The
    /// write policy's selection phase gets the same limits, so a write's MATCH
    /// admits exactly what a read can; its effect ceilings are unchanged.
    pub fn set_query_limits(&mut self, [records, rows, work, scratch]: [u64; 4]) {
        self.query_policy = GqlQueryPolicy::new(records, rows, work, scratch);
        self.write_policy.mutations.query = self.query_policy;
    }
}

/// The issuer a served database verifies capabilities with. Token minting
/// (`fgdbd token`) and serving construct it identically, so a token is valid
/// exactly for the database namespace, graph and policy epoch it names.
pub fn issuer(
    key: AuthKey,
    keys: &DatabaseKeys,
    policy_epoch: u64,
) -> Result<Authority, fgdb_warden::Error> {
    Authority::new(
        key,
        keys.namespace,
        GRAPH_NAME,
        SchemaEpoch(0),
        policy_epoch,
    )
}

/// Mint one capability token for `grant`, scoped to [`TRUNK`].
pub fn issue_token(authority: &Authority, grant: &Grant) -> Result<Vec<u8>, fgdb_warden::Error> {
    Ok(authority.issue_at(grant, unix_millis())?.encode())
}

/// Delegate a narrower token offline (`fgdbd attenuate`): append each
/// restriction as a caveat. No issuer key is needed and nothing is
/// authenticated here; a caveat can only narrow, because admission requires
/// every caveat to hold. A malformed or oversized token is refused.
pub fn attenuate_token(
    token: &[u8],
    restrictions: &[Restriction],
) -> Result<Vec<u8>, fgdb_warden::Error> {
    let mut token = CapabilityToken::decode(token)?;
    for restriction in restrictions {
        token = token.attenuate(restriction.clone())?;
    }
    Ok(token.encode())
}

#[derive(Debug)]
pub enum ServerError {
    InvalidLimits,
    DuplicateDatabase(String),
    InvalidDatabaseName,
    Open(fgdb::OpenError),
    Issuer(fgdb_warden::Error),
    OrderedScratch(std::io::Error),
    Spawn,
}

impl core::fmt::Display for ServerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidLimits => f.write_str("invalid server limits"),
            Self::DuplicateDatabase(name) => write!(f, "database {name:?} is served twice"),
            Self::InvalidDatabaseName => {
                f.write_str("a database name must be 1..=128 bytes of [A-Za-z0-9_.-]")
            }
            Self::Open(error) => write!(f, "cannot open database: {error}"),
            Self::Issuer(error) => write!(f, "invalid issuer: {error}"),
            Self::OrderedScratch(error) => write!(f, "cannot configure ordered scratch: {error}"),
            Self::Spawn => f.write_str("the runtime refused to spawn a connection task"),
        }
    }
}
impl core::error::Error for ServerError {}

/// A trusted host's maintenance request could not complete. Engine failures
/// retain their typed source; lifecycle diagnostics are never sent to a client.
#[derive(Debug)]
pub enum MaintenanceError {
    UnknownDatabase(String),
    Interrupted,
    Draining,
    DatabaseUnavailable(DatabaseStatus),
    Engine(fgdb::RebuildError),
}

impl MaintenanceError {
    fn unavailable(cx: &Cx, database: &Served) -> Self {
        if cx.checkpoint().is_err() {
            Self::Interrupted
        } else {
            Self::DatabaseUnavailable(database.db.status())
        }
    }
}

impl core::fmt::Display for MaintenanceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownDatabase(name) => write!(f, "database {name:?} is not served"),
            Self::Interrupted => f.write_str("database maintenance was interrupted"),
            Self::Draining => f.write_str("the server is draining"),
            Self::DatabaseUnavailable(status) => {
                write!(f, "database maintenance is unavailable: {status:?}")
            }
            Self::Engine(error) => write!(f, "database maintenance failed: {error}"),
        }
    }
}

impl core::error::Error for MaintenanceError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            _ => None,
        }
    }
}

/// A type-erased connection or request future: one `Send` proof per task
/// instead of one through every nested engine future (the same reason the
/// engine erases its commit boundary futures).
type ConnectionFuture<T> = core::pin::Pin<Box<dyn core::future::Future<Output = T> + Send>>;

/// A capability token from its hex text, decoded but not yet verified.
pub(crate) fn capability_from_hex(hex: &str) -> Option<CapabilityToken> {
    let hex = hex.trim();
    if hex.is_empty() || !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let digit = |b: u8| {
        if b.is_ascii_digit() {
            b - b'0'
        } else {
            b.to_ascii_lowercase() - b'a' + 10
        }
    };
    let bytes: Vec<u8> = (hex.as_bytes().as_chunks::<2>().0.iter())
        .map(|pair| (digit(pair[0]) << 4) | digit(pair[1]))
        .collect();
    CapabilityToken::decode(&bytes).ok()
}

/// One served database and its fixed authority inputs.
pub(crate) struct Served {
    pub(crate) db: recovery::DatabaseSlot,
    pub(crate) authority: Authority,
    pub(crate) symbols: Symbols,
    pub(crate) write_relation: RelationId,
    pub(crate) query_policy: GqlQueryPolicy,
    pub(crate) write_policy: GraphWriteProgramPolicy,
    pub(crate) max_statements: usize,
    pub(crate) max_subscriptions: usize,
    pub(crate) subscriptions: Arc<AtomicUsize>,
    pub(crate) commits: Arc<commits::CommitSignal>,
    pub(crate) namespace: [u8; 32],
    pub(crate) incarnation: [u8; 32],
    pub(crate) authority_commitment: [u8; 32],
    pub(crate) ordered_reads: Option<ordered::OrderedReads>,
}

impl Served {
    /// Verify a capability against this database's issuer at the host clock.
    pub(crate) fn admits(&self, token: &CapabilityToken) -> bool {
        self.authority
            .verify_at(token, TRUNK, unix_millis())
            .is_ok()
    }
}

/// The server: a set of served databases plus connection limits.
pub struct Server {
    pub(crate) databases: BTreeMap<String, Arc<Served>>,
    pub(crate) secret: [u8; 32],
    pub(crate) limits: ServerLimits,
    pub(crate) shutdown: Shutdown,
    tls: Option<TlsConfig>,
}

impl core::fmt::Debug for Server {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Server")
            .field("databases", &self.databases.keys().collect::<Vec<_>>())
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Server {
    /// A server with a fresh per-process session secret drawn from the
    /// context's entropy capability.
    pub fn new(cx: &Cx, limits: ServerLimits) -> Result<Self, ServerError> {
        limits.validate()?;
        let mut secret = [0u8; 32];
        cx.random_bytes(&mut secret);
        Ok(Self {
            databases: BTreeMap::new(),
            secret,
            limits,
            shutdown: Shutdown::new(),
            tls: None,
        })
    }

    /// Require TLS 1.3 on every subsequently served listener, using this
    /// identity. Configure before sharing the Server or accepting connections.
    /// FGP requires ALPN `fgp/1`; HTTPS requires `http/1.1`; Bolt keeps its
    /// ordinary encrypted magic/version negotiation without mandatory ALPN.
    pub fn enable_tls(&mut self, config: TlsConfig) {
        self.tls = Some(config);
    }

    /// Open the database at `path` and serve it under `config`.
    ///
    /// `cx` owns the database's recovery child and must belong to the host
    /// region that lives with this server. Ending a temporary opening region
    /// interrupts that child and fences the database; listener drain joins
    /// it before returning.
    pub async fn open_database(
        &mut self,
        cx: &Cx,
        path: &Path,
        config: DatabaseConfig,
    ) -> Result<(), ServerError> {
        let name = &config.name;
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        {
            return Err(ServerError::InvalidDatabaseName);
        }
        if self.databases.contains_key(name) {
            return Err(ServerError::DuplicateDatabase(name.clone()));
        }
        let authority = issuer(config.issuer_key, &config.keys, config.policy_epoch)
            .map_err(ServerError::Issuer)?;
        let ordered_reads = match config.ordered_reads {
            Some(config) => Some(ordered::OrderedReads::open(cx, config).await?),
            None => None,
        };
        let namespace = config.keys.namespace.0;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let db = Database::open(&contexts.commit(), path, config.keys)
            .await
            .map_err(ServerError::Open)?;
        let mut incarnation_input = b"fgdb-server:incarnation:v1".to_vec();
        incarnation_input.extend_from_slice(&namespace);
        let incarnation = fgdb_crypto::keyed_hash(&self.secret, &incarnation_input).0;
        let mut commitment_input = b"fgdb-server:authority:v1".to_vec();
        commitment_input.extend_from_slice(&namespace);
        commitment_input.extend_from_slice(&config.policy_epoch.to_be_bytes());
        let authority_commitment = fgdb_crypto::hash(&commitment_input).0;
        let commits = Arc::new(commits::CommitSignal::default());
        let subscriptions = Arc::new(AtomicUsize::new(0));
        let db =
            recovery::DatabaseSlot::new(cx, db, Arc::clone(&commits), Arc::clone(&subscriptions))?;
        self.databases.insert(
            config.name,
            Arc::new(Served {
                db,
                authority,
                symbols: config.symbols,
                write_relation: config.write_relation,
                query_policy: config.query_policy,
                write_policy: config.write_policy,
                max_statements: config.max_statements,
                max_subscriptions: config.max_subscriptions,
                subscriptions,
                commits,
                namespace,
                incarnation,
                authority_commitment,
                ordered_reads,
            }),
        );
        Ok(())
    }

    /// The drain signal. Triggering it stops admission and drains every
    /// connection at its next receive point; [`Server::serve`] then returns
    /// once every connection has closed.
    #[must_use]
    pub fn shutdown(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Inspect a served database's lifecycle from the trusted host. This is
    /// not exposed by the unauthenticated health endpoint. A failed recovery
    /// retains its diagnostic and never silently retries or serves the old pin.
    pub fn database_status(&self, cx: &Cx, name: &str) -> Option<DatabaseStatus> {
        cx.checkpoint().ok()?;
        self.databases
            .get(name)
            .map(|database| database.db.status())
    }

    /// Consolidate a served database without relinquishing its writer lease
    /// or advancing its committed frontier. Existing readers keep their
    /// immutable snapshots; later readers use the compacted publication.
    ///
    /// This is a trusted host operation, like opening a database, not a query
    /// capability or remote administration route. The caller chooses when to
    /// run it; no background pacing policy is implied. Ambiguous publication,
    /// including cancellation during root publication, fences the database and
    /// wakes its recovery worker before another request can reuse the old writer.
    pub async fn compact_database(&self, cx: &Cx, name: &str) -> Result<(), MaintenanceError> {
        let (served, _operation) = self.maintenance_admission(cx, name)?;
        let mut database = served
            .db
            .write(cx)
            .await
            .map_err(|_| MaintenanceError::unavailable(cx, served))?;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        database
            .compact(&contexts.commit())
            .await
            .map_err(MaintenanceError::Engine)
    }

    /// Verify the served database's published capsules and current Strata
    /// generation, repairing capsule redundancy where the engine can do so.
    /// This uses the same trusted host authority and writer admission as
    /// [`Server::compact_database`].
    ///
    /// A clean sweep preserves readers and the committed frontier. The
    /// returned summary can report lost objects: such a report, an I/O error,
    /// or cancellation leaves the handle fenced for supervised authoritative
    /// recovery. Call [`Server::database_status`] to inspect recovery progress.
    pub async fn scrub_database(
        &self,
        cx: &Cx,
        name: &str,
    ) -> Result<fgdb::ScrubSummary, MaintenanceError> {
        let (served, _operation) = self.maintenance_admission(cx, name)?;
        let mut database = served
            .db
            .write(cx)
            .await
            .map_err(|_| MaintenanceError::unavailable(cx, served))?;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        database
            .scrub(&contexts.commit())
            .await
            .map_err(|error| MaintenanceError::Engine(error.into()))
    }

    fn maintenance_admission(
        &self,
        cx: &Cx,
        name: &str,
    ) -> Result<(&Served, recovery::Operation), MaintenanceError> {
        cx.checkpoint().map_err(|_| MaintenanceError::Interrupted)?;
        if self.shutdown.is_triggered() {
            return Err(MaintenanceError::Draining);
        }
        let database = self
            .databases
            .get(name)
            .ok_or_else(|| MaintenanceError::UnknownDatabase(name.to_owned()))?;
        let operation = database
            .db
            .enter()
            .map_err(|_| MaintenanceError::unavailable(cx, database))?;
        // Keep this operation alive until after the writer drops, including
        // cancellation: drain/recovery must not retire the worker while a
        // maintenance operation can still publish or fence its handle.
        Ok((database, operation))
    }

    /// Finish trusted host shutdown after its listener and maintenance tasks
    /// have been joined. This also retires database workers if startup failed
    /// before any listener began serving. Repeated calls are harmless.
    pub async fn finish_shutdown(&self, cx: &Cx) {
        self.shutdown.trigger();
        self.join_database_workers(cx).await;
    }

    async fn join_database_workers(&self, cx: &Cx) {
        for database in self.databases.values() {
            database.db.stop_and_join(cx).await;
        }
    }

    /// Accept and serve FGP connections until the drain signal fires or `cx`
    /// is cancelled, then wait for every connection task to finish.
    pub async fn serve(self: Arc<Self>, cx: &Cx, listener: TcpListener) -> Result<(), ServerError> {
        let connections = self
            .accept(
                cx,
                &listener,
                tls::Protocol::Fgp,
                |server, child, stream, _local| {
                    Box::pin(async move {
                        connection::run(&child, &server, stream).await;
                    })
                },
            )
            .await;
        // Admission is closed. Every connection drains at its next receive
        // point (an admitted statement finishes first), so this join is
        // bounded by in-flight statements, never by an idle client.
        self.shutdown.trigger();
        let connections = match connections {
            Ok(connections) => connections,
            Err(error) => {
                self.join_database_workers(cx).await;
                return Err(error);
            }
        };
        for mut handle in connections {
            let _ = handle.join(cx).await;
        }
        self.join_database_workers(cx).await;
        Ok(())
    }

    /// Serve the Bolt-compat adapter (`BoltCompatProfileV1`, see the `bolt`
    /// module) until the drain signal fires or `cx` is cancelled: official
    /// Neo4j drivers run read-only statements under a capability token.
    pub async fn serve_bolt(
        self: Arc<Self>,
        cx: &Cx,
        listener: TcpListener,
    ) -> Result<(), ServerError> {
        let connections = self
            .accept(
                cx,
                &listener,
                tls::Protocol::Bolt,
                |server, child, stream, local| {
                    Box::pin(async move {
                        bolt::run(&child, &server, stream, local).await;
                    })
                },
            )
            .await;
        self.shutdown.trigger();
        let connections = match connections {
            Ok(connections) => connections,
            Err(error) => {
                self.join_database_workers(cx).await;
                return Err(error);
            }
        };
        for mut handle in connections {
            let _ = handle.join(cx).await;
        }
        self.join_database_workers(cx).await;
        Ok(())
    }

    /// Serve the HTTP/1.1 JSON adapter (see the `http` module) until the drain
    /// signal fires or `cx` is cancelled. Only requests whose `Host` names one
    /// of `allowed_hosts` (without port) are answered, which defeats DNS
    /// rebinding against a loopback listener.
    pub async fn serve_http(
        self: Arc<Self>,
        cx: &Cx,
        listener: TcpListener,
        allowed_hosts: Vec<String>,
    ) -> Result<(), ServerError> {
        let config = Http1Config::default()
            .max_body_size(8 << 20)
            .host_policy(HostPolicy::allow_list(allowed_hosts))
            .idle_timeout(Some(Duration::from_secs(30)));
        let signal = ShutdownSignal::new();
        let connections = {
            let signal = signal.clone();
            self.accept(
                cx,
                &listener,
                tls::Protocol::Http,
                move |server, child, stream, _local| {
                    let config = config.clone();
                    let signal = signal.clone();
                    Box::pin(async move {
                        // Both the handler and transport borrow this connection's
                        // server owner, so verified output authority survives every
                        // pending write without a self-referential Arc container.
                        let server = &*server;
                        let output = Arc::new(http::OutputAuthority::new());
                        let io = http::GuardedIo::new(stream, child.clone(), Arc::clone(&output));
                        let handler = move |request| {
                            let child = child.clone();
                            let output = Arc::clone(&output);
                            let answer: core::pin::Pin<
                                Box<dyn core::future::Future<Output = Response> + Send + '_>,
                            > = Box::pin(async move {
                                http::respond(&child, server, request, &output).await
                            });
                            answer
                        };
                        let _ = Http1Server::with_config(handler, config)
                            .with_shutdown_signal(signal)
                            .serve(io)
                            .await;
                    })
                },
            )
            .await
        };
        // Stop reading new requests on every keep-alive connection; a request
        // already being answered completes first.
        let _ = signal.begin_drain(Duration::from_secs(5));
        self.shutdown.trigger();
        let connections = match connections {
            Ok(connections) => connections,
            Err(error) => {
                self.join_database_workers(cx).await;
                return Err(error);
            }
        };
        for mut handle in connections {
            let _ = handle.join(cx).await;
        }
        self.join_database_workers(cx).await;
        Ok(())
    }

    /// The shared admission loop: accept until the drain signal or a
    /// cancelled context, bound the live connection count, and spawn one task
    /// per connection. Returns the live connection tasks to join.
    async fn accept<F>(
        self: &Arc<Self>,
        cx: &Cx,
        listener: &TcpListener,
        protocol: tls::Protocol,
        connect: F,
    ) -> Result<Vec<TaskHandle<()>>, ServerError>
    where
        F: Fn(
                Arc<Self>,
                Cx,
                Box<dyn DuplexIo>,
                Option<std::net::SocketAddr>,
            ) -> ConnectionFuture<()>
            + Send
            + Sync
            + 'static,
    {
        let waiter = self.shutdown.waiter();
        let connect = Arc::new(connect);
        let mut connections: Vec<TaskHandle<()>> = Vec::new();
        loop {
            let accepted = poll_fn(|task| {
                if waiter.poll_triggered(task) || cx.checkpoint().is_err() {
                    return Poll::Ready(None);
                }
                listener.poll_accept(task).map(Some)
            })
            .await;
            let Some(accepted) = accepted else { break };
            // A failed accept (a peer reset before acceptance, a transient
            // descriptor shortage) affects no admitted connection.
            let Ok((stream, _peer)) = accepted else {
                continue;
            };
            connections.retain(|handle| !handle.is_finished());
            if connections.len() >= self.limits.max_connections {
                drop(stream);
                continue;
            }
            // The concrete local address this peer reached, read before TLS
            // boxes the socket (Bolt routing advertises it).
            let local = stream.local_addr().ok();
            let server = Arc::clone(self);
            let connect = Arc::clone(&connect);
            let handle = cx.spawn(move |child| async move {
                let waiter = server.shutdown.waiter();
                let Some(stream) =
                    tls::establish(&child, &waiter, stream, server.tls.as_ref(), protocol).await
                else {
                    return;
                };
                connect(server, child, stream, local).await;
            });
            let handle = match handle {
                Ok(handle) => handle,
                Err(_) => {
                    self.shutdown.trigger();
                    for mut handle in connections {
                        let _ = handle.join(cx).await;
                    }
                    return Err(ServerError::Spawn);
                }
            };
            connections.push(handle);
        }
        Ok(connections)
    }
}

/// The host's trusted wall clock in Unix milliseconds, made monotone for the
/// life of the process: Warden refuses time that moves backwards, so a system
/// clock step back holds the last reading instead of failing every session.
pub(crate) fn unix_millis() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    LAST.fetch_max(now, Ordering::AcqRel).max(now)
}

/// Warden charges that may share one [`unix_millis`] sample (fgdb-er71y).
/// Authorized execution charges several times per admitted record, and
/// reading the wall clock on every charge was 22-55% of fgdbd's busy CPU on
/// real graphs. A few hundred charges are microseconds of work; each
/// execution still samples on its first charge and on every delivery.
const HOST_CLOCK_INTERVAL: core::num::NonZeroU32 = match core::num::NonZeroU32::new(256) {
    Some(interval) => interval,
    None => panic!("a nonzero interval"),
};

/// The query context every served statement runs under: the request's own,
/// sampling [`unix_millis`] once per [`HOST_CLOCK_INTERVAL`] charges.
pub(crate) fn query_cx(contexts: &PurposeContexts) -> fgdb_types::QueryCx {
    contexts
        .query()
        .with_host_clock_interval(HOST_CLOCK_INTERVAL)
}
