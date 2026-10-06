//! `fgdbd`: FrankenGraphDB's server posture (plan §13 item 2, Appendix D).
//!
//! The server composes the embedded engine behind `fgdb-protocol`'s FGP
//! connection machine over asupersync TCP. It reimplements none of the
//! engine: every statement runs through the same capability-authorized
//! sessions an embedded host would use, constructed fresh per statement from
//! the connection's Warden capability, so signature, scope, expiry, signed
//! budgets and issuer retirement are rechecked for every statement and
//! security applies before expansion (FG-INV-20), never as a result filter.
//!
//! # What is served (a subset of Appendix D, never a substitute)
//!
//! - The handshake `HELLO -> HELLO_ACK -> AUTH -> AUTH_OK -> SELECT_DATABASE
//!   -> READY`, with the session binding derived from the HELLO/HELLO_ACK/AUTH
//!   transcript under a per-process server secret before AUTH_OK is encoded.
//!   A nonexistent and an unauthorized database share one failure surface.
//! - `EXECUTE` of one GQL read or write statement. A read answers at exactly
//!   one committed sequence through a read-only authorized session; a write
//!   is one autocommit program through an authorized write session.
//! - Results as the session-owned, ephemeral `SNAPSHOT_RESULT_CHUNK` /
//!   `SNAPSHOT_RESULT_END` class on a server-minted child stream, under
//!   per-stream byte-and-row flow credit replenished by `WINDOW_UPDATE`.
//! - `QUERY_CANCEL` (sender control only), `PING`/`PONG`, `DRAIN`/`GOODBYE`.
//!
//! Not served, and refused with a typed error rather than approximated: the
//! durable `PublishedResultStream` class with `RESULT_ACK`/`RESULT_RELEASE`,
//! `PREPARE`, `AUTH_REFRESH`, explicit multi-statement transactions with
//! ownership/reattach, subscriptions, TLS, and the HTTP/gRPC/Bolt adapters.
//! Because every result is ephemeral, a disconnect can lose undelivered rows
//! but never a commit: a write's outcome is decided before its first frame.
//!
//! The engine has no durable catalog yet, so each served database resolves
//! names through operator-declared [`Symbols`], exactly like the CLI flags.

#![forbid(unsafe_code)]

mod connection;
mod convert;
mod keys;
mod shutdown;
mod symbols;

pub use keys::{KeyFileError, parse_key_lines, read_database_keys, read_issuer_key};
pub use shutdown::Shutdown;
pub use symbols::{SymbolConflict, Symbols};

use asupersync::Cx;
use asupersync::fs::UnixVfs;
use asupersync::net::TcpListener;
use asupersync::security::key::AuthKey;
use asupersync::sync::RwLock;
use core::future::poll_fn;
use core::task::Poll;
use fgdb::{Database, DatabaseKeys};
use fgdb_delta_types::{RelationId, SchemaEpoch};
use fgdb_gql::{GqlQueryPolicy, GraphWriteProgramPolicy};
use fgdb_protocol::MAX_HEADER_LEN;
use fgdb_types::PurposeContexts;
use fgdb_warden::{Authority, CapabilityToken, Grant};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
}

impl DatabaseConfig {
    /// The defaults the `fgdb` CLI uses for one invocation.
    #[must_use]
    pub fn new(name: impl Into<String>, keys: DatabaseKeys, issuer_key: AuthKey) -> Self {
        let query_policy = GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000);
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
        }
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

#[derive(Debug)]
pub enum ServerError {
    InvalidLimits,
    DuplicateDatabase(String),
    InvalidDatabaseName,
    Open(fgdb::OpenError),
    Issuer(fgdb_warden::Error),
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
            Self::Spawn => f.write_str("the runtime refused to spawn a connection task"),
        }
    }
}
impl core::error::Error for ServerError {}

/// One served database and its fixed authority inputs.
pub(crate) struct Served {
    pub(crate) db: RwLock<Database<UnixVfs>>,
    pub(crate) authority: Authority,
    pub(crate) symbols: Symbols,
    pub(crate) write_relation: RelationId,
    pub(crate) query_policy: GqlQueryPolicy,
    pub(crate) write_policy: GraphWriteProgramPolicy,
    pub(crate) max_statements: usize,
    pub(crate) namespace: [u8; 32],
    pub(crate) incarnation: [u8; 32],
    pub(crate) authority_commitment: [u8; 32],
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
        })
    }

    /// Open the database at `path` and serve it under `config`.
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
        self.databases.insert(
            config.name,
            Arc::new(Served {
                db: RwLock::new(db),
                authority,
                symbols: config.symbols,
                write_relation: config.write_relation,
                query_policy: config.query_policy,
                write_policy: config.write_policy,
                max_statements: config.max_statements,
                namespace,
                incarnation,
                authority_commitment,
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

    /// Accept and serve connections until the drain signal fires or `cx` is
    /// cancelled, then wait for every connection task to finish.
    pub async fn serve(self: Arc<Self>, cx: &Cx, listener: TcpListener) -> Result<(), ServerError> {
        let waiter = self.shutdown.waiter();
        let mut connections = Vec::new();
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
            connections
                .retain(|handle: &asupersync::runtime::TaskHandle<()>| !handle.is_finished());
            if connections.len() >= self.limits.max_connections {
                drop(stream);
                continue;
            }
            let server = Arc::clone(&self);
            let handle = cx
                .spawn(move |child| async move {
                    connection::run(&child, &server, stream).await;
                })
                .map_err(|_| ServerError::Spawn)?;
            connections.push(handle);
        }
        // Admission is closed. Every connection drains at its next receive
        // point (an admitted statement finishes first), so this join is
        // bounded by in-flight statements, never by an idle client.
        self.shutdown.trigger();
        for mut handle in connections {
            let _ = handle.join(cx).await;
        }
        Ok(())
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
