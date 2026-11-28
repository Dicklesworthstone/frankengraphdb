//! `fgdbd`: serve FrankenGraphDB databases over FGP, mint capability tokens,
//! and generate owner-only key files.
// Proving `Server::serve`'s connection task Send walks through the ordered
// read futures, past the default recursion limit (a future hard error).
#![recursion_limit = "256"]
#![forbid(unsafe_code)]

use asupersync::Budget;
use asupersync::Cx;
use asupersync::io::AsyncWriteExt as _;
use asupersync::net::TcpListener;
use fgdb::BufferedReadLimits;
use fgdb_beacon::read::{Projection, ReadOptions, ReadPolicy, VectorEncoding};
use fgdb_beacon::{DistanceMetric, HnswConfig, IndexConfig};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_server::{
    BeaconIndexConfig, BufferedReadConfig, DatabaseConfig, MaintenanceEvent, MaintenanceSchedule,
    OrderedReadConfig, Server, ServerLimits, Symbols, WebSocketConfig,
};
use fgdb_strata::tiered::sealed::SealedLimits;
use fgdb_warden::{Grant, QueryLimits, Restriction, Rights, Scope};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

const HELP: &str = "\
fgdbd: the FrankenGraphDB server (FGP, HTTP, WebSocket, and Bolt with optional TLS)

USAGE:
  fgdbd serve --listen <addr:port> [--http-listen <addr:port> [--http-allow-host <host>]...]
      [--bolt-listen <addr:port>] [--websocket-listen <addr:port>]
      [--websocket-allow-host <host>]... [--websocket-allow-origin <origin>]...
      [--tls-cert-file <path> --tls-key-file <path>] DATABASE...
      DATABASE := --database <name>=<path> --key-file <path> --issuer-key-file <path>
                  [--kek-passphrase-file <path>] [--strata-ecs]
                  [--policy-epoch <n>] [--write-relation <u64>] [--query-scratch-dir <path>]
                  [--buffered-reads-memory-bytes <n>]
                  [--buffered-reads-root-bytes <n>]
                  [--buffered-reads-max-source-bytes <n>]
                  [--buffered-reads-max-work-units <n>]
                  [--buffered-reuse-live-admission]
                  [--buffered-native-run-limits <rows,incidences,image-bytes,property-bytes>]
                  [--max-snapshot-records <n>] [--max-result-rows <n>]
                  [--max-work-units <n>] [--max-scratch-entries <n>] [--bindings-file <path>]
                  [--compact-interval-seconds <n>] [--scrub-interval-seconds <n>]
                  [--beacon-checkpoint-interval-seconds <n>]
                  [--subscription-coalesce-ticks <n>]
                  [--label <name>=<u64>]... [--relation <name>=<u64>]... [--property <name>=<u64>]...
                  [--beacon-index <name> [--beacon-text <property>]
                    [--beacon-vector <property,...> | --beacon-packed-vector <property>=<dimensions>]
                    [--beacon-label <label>] [--beacon-metric l2|cosine|dot]]...
      [--max-connections <n>]
  fgdbd token --key-file <path> --issuer-key-file <path> [--policy-epoch <n>]
      [--kek-passphrase-file <path>]
      [--rights read|write|read-write|admin] [--expires-in <seconds>]
      [--allow-label <u64>]... [--allow-relation <u64>]... [--allow-property <u64>]...
      [--max-nodes <n>] [--max-work <n>] [--max-rows <n>]
  fgdbd attenuate [--rights read|write|read-write|admin] [--expires-in <seconds>]
      [--not-before-in <seconds>] [--allow-label <u64>]... [--allow-relation <u64>]...
      [--allow-property <u64>]... [--deny-property <u64>]...
      [--max-nodes <n>] [--max-work <n>] [--max-rows <n>]   < token.hex
  fgdbd keygen --database <path> [--passphrase-file <path>] | --issuer <path>
  fgdbd help

serve opens each database and accepts FGP connections. Options after a
--database apply to that database. A client authenticates with a Warden
capability token minted by `fgdbd token` for that database's key file and
issuer key; it can select exactly the databases whose issuer accepts it.
Statements run through capability-authorized sessions, so a token's label,
relation and property scope applies before expansion. Names are discovered from
the durable database catalog. Flags and --bindings-file may add process-local
names or repeat exact durable bindings; conflicting names or ids refuse startup.
The bindings file uses the fgdb CLI's format. serve prints one
NDJSON line {\"v\":1,\"event\":\"listening\",\"protocol\":\"fgp\",\"addr\":...} per
listener once bound, drains on SIGINT/SIGTERM (in-flight statements finish,
idle connections say GOODBYE), and exits 0.

--max-snapshot-records, --max-result-rows, --max-work-units and
--max-scratch-entries set that database's per-statement query limits (the
fgdb CLI's query limits; defaults 100000, 100000, 10000000 and 10000000).
They apply to every read and to the selection phase of every write. A token's
own --max-nodes/--max-work/--max-rows caveats still bound each statement too.

--beacon-index declares a maintained retrieval index for that database. The
following --beacon-* fields belong to that index until the next declaration.
Supply a text lane, a vector lane, or both. Properties and optional labels
resolve from the durable catalog and explicit flags or --bindings-file.
Coordinate properties keep the stated order and round to f32; packed vectors
contain exactly <dimensions> little-endian f32 values in one bytes property.
The default metric is l2. Each database admits at most 64 named definitions,
and vector dimensions must be 1..=65536. Names identify operator diagnostics
and derived checkpoints, not GQL catalog names: matching CALL hybrid.search
requests select the index.

Startup and authoritative recovery restore authenticated checkpoints and catch
up committed deltas before serving. Missing, corrupt or incompatible checkpoints
are rebuilt. Refreshed checkpoints are saved at that boundary; intact current
checkpoints are reused without rewriting. Graceful shutdown saves changed healthy
configured indexes; --beacon-checkpoint-interval-seconds also saves them at a fixed
positive interval. Automatic saves skip healthy indexes already checkpointed at
the current sequence when their previous save has no error.
Checkpoint work runs outside ordinary graph commits and a save failure leaves
the database and healthy resident indexes available. Initial construction and
each committed maintenance tick use the database's source/work/scratch ceilings.
A later refused maintenance tick fences that index without changing the committed
write. Matching fully visible exact reads reuse healthy generations; masked,
historical and approximate requests keep their existing admitted execution paths.
These remain resident operator-configured indexes, not a durable index catalog
or approximate-index reuse. Signed query limits still apply to every search.

--subscription-coalesce-ticks selects the maximum number of consecutive
retained commits combined into one FGP subscription delta for that database.
The default 1 delivers each retained tick. A value greater than 1 opts clients
into net-state synchronization: changes that cancel within a range disappear,
and an empty delta still advances the frontier. Each complete range shares
one signed work, scratch and result-row allowance; budget exhaustion refuses
the pull without acknowledging a partial range. Retention gaps still yield
a replacement baseline. This option does not enable durable reconnect resume.

--query-scratch-dir enables FGP ordered reads for that database using bounded
query-private files in an existing directory. The operator chooses this path;
clients request ReadOrdered or prepared OrderedStream without supplying paths
or scratch limits. Databases without this option refuse those execution modes.

--buffered-reads-memory-bytes selects bounded cold sources for FGP ReadStream
and prepared SourceStream on that database, plus ReadOrdered and prepared
OrderedStream when --query-scratch-dir is configured. The byte pool is shared
across four concurrent cold reads, including ordering and waiting for credit;
ordered execution also uses the separate scratch limits. This profile requires
durable catalog names. Cold ordered reads compose native vertex/single-edge
sources, including any or finite relation types, through external relational
and aggregate operators; unsupported shapes refuse. Difference reads and the
live writer retain their existing resident sources.

--buffered-reads-root-bytes sets the encoded root and segment byte limit for
one cold read. The default root decode reservation is one quarter of the
shared pool. This option allows reserving up to half for root decoding, with
pool admission and cache charges still enforced. It requires
--buffered-reads-memory-bytes for the same database.

--buffered-reads-max-source-bytes sets the positive encoded payload byte limit
for full buffered source admission, including history validation. The default
is 1073741824. It requires --buffered-reads-memory-bytes for the same database.
Root bytes, resident memory and later scan work have separate limits.

--buffered-reads-max-work-units sets the positive physical source work limit
for admission and for each cumulative scan. The default is 10000000.
It requires --buffered-reads-memory-bytes for the same database. Token and
query evaluation limits also apply.

--buffered-reuse-live-admission lets buffered readers reuse the live writer's
exact root validation. Metadata and every accessed object still authenticate.
Without matching proof, the full payload history is checked at open. This
requires --buffered-reads-memory-bytes for the same database.

--buffered-native-run-limits opts eligible buffered edge sources into
authenticated sealed relation runs plus their post-seal history. Supply
four comma-separated nonnegative decimal limits for rows, incidences,
image bytes and property bytes. This requires --buffered-reads-memory-bytes
for the same database. Selected run failures are terminal. The run and
suffix share the existing pool and query allowance.

--compact-interval-seconds and --scrub-interval-seconds opt that database
into fixed periodic maintenance while fgdbd retains its writer lease. Both
are disabled by default and require positive whole seconds. The first run
waits one interval; later runs wait that interval after completion (including
a failed attempt), without catching up missed ticks. Compaction and scrub
run sequentially per database through ordinary writer admission; foreground
writes can wait behind them. This is a fixed operator schedule, not adaptive
latency pacing. Existing read snapshots and the committed frontier survive
healthy maintenance. Outcomes, including lost-object counts, go to stderr;
lost data or ambiguous maintenance publication uses supervised recovery.
Drain stops new maintenance and waits for an admitted operation to finish.

--tls-cert-file and --tls-key-file must be supplied together. They enable
TLS 1.3 on EVERY configured listener, with no plaintext fallback. Supply a
PEM certificate chain and an owner-only PEM private key (chmod 600 on Unix).
The identity is validated before databases are opened or listeners are bound.
Handshakes are bounded to 10 seconds and are cancelled during server drain;
TLS early data is disabled. FGP clients require ALPN fgp/1; HTTPS uses
http/1.1. Bolt clients use bolt+s:// or neo4j+s:// with normal certificate
verification. WebSocket uses wss:// with http/1.1 ALPN. Warden capability
authentication still applies inside TLS.

--http-listen adds the HTTP/1.1 JSON adapter: POST /v1/databases/<name>/query,
/write, /write-batch, or /query-batch with `Authorization: Bearer <hex token>` and a JSON
body {\"statement\": \"<gql>\", \"parameters\": {...}} (or \"argument_sets\" for
/write-batch and /query-batch); GET /v1/databases/<name>/schema; GET /v1/health.
query-batch runs 1..=32 parameterized reads atomically over one pinned generation
and one cumulative signed allowance. It returns ordered results with each read's
actual selected sequence; a failed read refuses the whole batch. Requests must
name an allowed Host (default localhost, 127.0.0.1, [::1] and the listen IP;
--http-allow-host replaces that list).

POST /v1/databases/<name>/compact and /scrub take an empty body and require
--rights admin with unrestricted label, relation and property scope. Admin
grants physical maintenance only; it grants no query reads or writes. Query
rights cannot be attenuated into Admin rights. Query ceilings apply to query
execution; maintenance uses the server's existing engine/recovery ownership.
POST /v1/databases/<name>/seal uses the same Admin grant. Send a JSON object
with numeric relation and optional retain_from (unsigned 64-bit integers).
The response identifies the durable run and the unchanged graph sequence.

--websocket-listen serves the native FGP byte stream at /v1/fgp using
WebSocket binary messages and the required fgp.v1 subprotocol. It supports the
same authentication, queries, prepared statements, explicit transactions and
live subscriptions as the TCP listener. Each binary message may contain part
of an FGP frame or several frames; text messages refuse and compression is
not negotiated.
The per-message limit equals the server's maximum FGP frame size.

--websocket-allow-host replaces the default localhost/loopback/listen-IP Host
allowlist (names have no port; IPv6 literals have brackets).
Browser clients must supply an Origin matching an explicit
--websocket-allow-origin, for example https://console.example or
http://localhost:3000. Origins are exact, including port, without paths or
wildcards. With no allowed origins, native clients without Origin can connect
but browser upgrades refuse. Tokens are carried in FGP AUTH, never in the URL.
The HTTP upgrade and FGP setup share one 30-second establishment deadline.
Result delivery, cancellation and disconnect use the ordinary FGP contracts.

--bolt-listen adds the bounded Bolt-compat adapter for Neo4j drivers:
bolt:// or neo4j:// URIs, authenticated with the hex token as a bearer
credential (or as the basic-auth password). It serves Cypher/GQL reads,
autocommit writes with RETURN, and explicit native write transactions with
read-your-own-writes. BEGIN in write/default mode requires signed Write or
ReadWrite rights to open a writer; read-only grants and mode:r keep a read
session. PULL/DISCARD return provisional statement results; only COMMIT
publishes the explicit transaction. ROLLBACK, RESET and disconnect discard
uncommitted effects. Durable transaction reattachment remains unsupported.
The database is the driver's database argument, or the only one served.

token prints the token as hex on stdout. Scopes default to every label,
relation and property; any --allow-* flag restricts that kind to the listed
ids. Defaults: --rights read, --expires-in 86400, limits 1000000 nodes,
100000000 work units, 1000000 rows. Bump --policy-epoch on serve and token to
revoke every token minted under the old epoch.

attenuate reads one hex token on stdin and prints a narrower one as hex. Each
flag appends a caveat that every later admission must also satisfy, so it can
only take authority away: --allow-* intersects a scope, --deny-property hides
those properties, --rights and the limits can only lower, and --expires-in /
--not-before-in bound the validity window from now. It needs no key file and
verifies nothing; the server authenticates the result. At least one flag is
required. Delegate this way: mint once with token, attenuate per holder.

keygen writes a new owner-only (0600) key file. For databases it writes a
raw three-line key by default. Supply an owner-only --passphrase-file to wrap
the key and enable protected Strata storage. Then pass --kek-passphrase-file
to serve and token. With a raw database key, serve can select protected Strata
storage with --strata-ecs. Issuer keys contain one line. An existing output
file is refused.

Exit codes: 0 success, 2 usage, 4 key or open failure, 5 I/O failure.
";

struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    fn usage(message: impl ToString) -> Self {
        Self {
            code: 2,
            message: message.to_string(),
        }
    }
    fn open(message: impl ToString) -> Self {
        Self {
            code: 4,
            message: message.to_string(),
        }
    }
    fn io(message: impl ToString) -> Self {
        Self {
            code: 5,
            message: message.to_string(),
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("fgdbd: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

fn run(args: &[String]) -> Result<(), Failure> {
    let Some(command) = args.first() else {
        print!("{HELP}");
        return Err(Failure::usage("missing subcommand"));
    };
    if command == "help" || command == "--help" {
        print!("{HELP}");
        return Ok(());
    }
    let runtime = fgdb::runtime_builder().build().map_err(Failure::io)?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    match command.as_str() {
        "serve" => {
            let options = ServeOptions::parse(&args[1..])?;
            runtime.block_on(serve(&root, options))
        }
        "token" => {
            let options = TokenOptions::parse(&args[1..])?;
            runtime.block_on(token(&root, options))
        }
        "keygen" => runtime.block_on(keygen(&root, &args[1..])),
        "attenuate" => attenuate(AttenuateOptions::parse(&args[1..])?),
        _ => Err(Failure::usage("unknown subcommand; use fgdbd help")),
    }
}

struct DatabaseOptions {
    name: String,
    path: PathBuf,
    key_file: Option<PathBuf>,
    kek_passphrase_file: Option<PathBuf>,
    strata_ecs: bool,
    issuer_key_file: Option<PathBuf>,
    query_scratch_dir: Option<PathBuf>,
    buffered_reads_memory_bytes: Option<NonZeroUsize>,
    buffered_reads_root_bytes: Option<NonZeroUsize>,
    buffered_reads_source_bytes: Option<NonZeroUsize>,
    buffered_reads_work: Option<NonZeroUsize>,
    buffered_reuse_live_admission: bool,
    buffered_native_run_limits: Option<SealedLimits>,
    policy_epoch: u64,
    write_relation: u64,
    /// `--max-snapshot-records`, `--max-result-rows`, `--max-work-units` and
    /// `--max-scratch-entries`, in [`fgdb_gql::GqlQueryPolicy::new`] order.
    query_limits: [Option<u64>; 4],
    /// Fixed compaction, scrub and Beacon-checkpoint delays; absent disables one.
    maintenance_intervals: [Option<Duration>; 3],
    subscription_coalesce_ticks: Option<NonZeroUsize>,
    symbols: Symbols,
    /// `--bindings-file`: more bindings, merged in before the database opens.
    bindings_file: Option<PathBuf>,
    beacon_indexes: Vec<BeaconIndexOptions>,
}

impl DatabaseOptions {
    fn buffered_read_config(&self) -> Option<BufferedReadConfig> {
        let mut buffered = BufferedReadConfig::new(self.buffered_reads_memory_bytes?.get());
        if let Some(bytes) = self.buffered_reads_root_bytes {
            buffered.limits.max_root_bytes = bytes.get();
            buffered.limits.max_blocks = bytes.get();
            buffered.limits.max_vertex_patches = bytes.get();
        }
        if let Some(bytes) = self.buffered_reads_source_bytes {
            buffered.limits.max_source_bytes = bytes.get();
        }
        if let Some(work) = self.buffered_reads_work {
            buffered.limits.max_work = work.get();
        }
        buffered.native_run_limits = self.buffered_native_run_limits;
        buffered.reuse_live_admission = self.buffered_reuse_live_admission;
        Some(buffered)
    }
}

struct BeaconIndexOptions {
    name: String,
    text: Option<String>,
    vector: Option<BeaconVectorOptions>,
    label: Option<String>,
    metric: Option<DistanceMetric>,
}

enum BeaconVectorOptions {
    Coordinates(Vec<String>),
    Packed { property: String, dimensions: usize },
}

impl BeaconIndexOptions {
    fn new(name: &str) -> Result<Self, Failure> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        {
            return Err(Failure::usage(
                "--beacon-index needs a name of 1..=128 bytes from [A-Za-z0-9_.-]",
            ));
        }
        Ok(Self {
            name: name.to_owned(),
            text: None,
            vector: None,
            label: None,
            metric: None,
        })
    }

    fn field(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
        if raw.is_empty() {
            return Err(Failure::usage(format!("{flag} needs a nonempty value")));
        }
        match flag {
            "--beacon-text" if self.text.is_none() => self.text = Some(raw.to_owned()),
            "--beacon-label" if self.label.is_none() => self.label = Some(raw.to_owned()),
            "--beacon-vector" if self.vector.is_none() => {
                let mut properties = Vec::new();
                for name in raw.split(',') {
                    if name.is_empty() {
                        return Err(Failure::usage("--beacon-vector has an empty property name"));
                    }
                    if properties.len() == 65_536 {
                        return Err(Failure::usage("--beacon-vector exceeds 65536 dimensions"));
                    }
                    properties.push(name.to_owned());
                }
                self.vector = Some(BeaconVectorOptions::Coordinates(properties));
            }
            "--beacon-packed-vector" if self.vector.is_none() => {
                let (property, dimensions) = raw.split_once('=').ok_or_else(|| {
                    Failure::usage("--beacon-packed-vector needs <property>=<dimensions>")
                })?;
                let dimensions = number(dimensions, flag)?;
                if property.is_empty() || !(1..=65_536).contains(&dimensions) {
                    return Err(Failure::usage(
                        "--beacon-packed-vector needs a property and 1..=65536 dimensions",
                    ));
                }
                self.vector = Some(BeaconVectorOptions::Packed {
                    property: property.to_owned(),
                    dimensions,
                });
            }
            "--beacon-metric" if self.metric.is_none() => {
                self.metric = Some(match raw {
                    "l2" => DistanceMetric::SquaredEuclidean,
                    "cosine" => DistanceMetric::Cosine,
                    "dot" => DistanceMetric::NegativeDotProduct,
                    _ => return Err(Failure::usage("--beacon-metric needs l2, cosine, or dot")),
                });
            }
            _ => {
                return Err(Failure::usage(format!(
                    "duplicate or conflicting field {flag} for Beacon index {:?}",
                    self.name
                )));
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), Failure> {
        if self.text.is_none() && self.vector.is_none() {
            return Err(Failure::usage(format!(
                "Beacon index {:?} needs a text or vector lane",
                self.name
            )));
        }
        if self.metric.is_some() && self.vector.is_none() {
            return Err(Failure::usage("--beacon-metric requires a vector lane"));
        }
        Ok(())
    }

    /// Bind only after durable names and the complete operator declarations
    /// are available. Never introduce schema names through index options.
    fn bind(&self, symbols: &Symbols, limits: [u64; 4]) -> Result<BeaconIndexConfig, Failure> {
        self.validate()?;
        let property = |name: &str| match symbols.resolve(GraphSymbolKind::Property, name) {
            Some(GraphSymbol::Property(id)) => Ok(id),
            _ => Err(Failure::usage(format!(
                "Beacon index {:?} refers to unbound property {name:?}",
                self.name
            ))),
        };
        let text = self.text.as_deref().map(property).transpose()?;
        let label = self
            .label
            .as_deref()
            .map(|name| match symbols.resolve(GraphSymbolKind::Label, name) {
                Some(GraphSymbol::Label(id)) => Ok(id),
                _ => Err(Failure::usage(format!(
                    "Beacon index {:?} refers to unbound label {name:?}",
                    self.name
                ))),
            })
            .transpose()?;
        let (vector, dimensions, encoding) = match &self.vector {
            None => (Vec::new(), 0, VectorEncoding::CoordinatesRoundedF32),
            Some(BeaconVectorOptions::Coordinates(names)) => (
                names
                    .iter()
                    .map(|name| property(name))
                    .collect::<Result<_, _>>()?,
                names.len(),
                VectorEncoding::CoordinatesRoundedF32,
            ),
            Some(BeaconVectorOptions::Packed {
                property: name,
                dimensions,
            }) => (
                vec![property(name)?],
                *dimensions,
                VectorEncoding::PackedF32Le,
            ),
        };
        let mut native_limits = [0; 4];
        for (at, value) in limits.into_iter().enumerate() {
            native_limits[at] = usize::try_from(value).map_err(|_| {
                Failure::usage(format!(
                    "{} exceeds the native Beacon limit domain",
                    QUERY_LIMIT_FLAGS[at]
                ))
            })?;
        }
        let [records, rows, work, scratch] = native_limits;
        let mut index = IndexConfig::default();
        if text.is_none() {
            index.text = None;
        }
        if self.vector.is_some() {
            index.vector = Some(
                HnswConfig::new(
                    dimensions,
                    self.metric.unwrap_or(DistanceMetric::SquaredEuclidean),
                )
                .exact_only(),
            );
        }
        Ok(BeaconIndexConfig::new(
            self.name.clone(),
            ReadOptions {
                as_of: None,
                vertex_label: label,
                projection: Projection {
                    text,
                    vector,
                    encoding,
                },
                index,
                policy: ReadPolicy {
                    max_work_units: work,
                    max_source_scratch: scratch,
                    max_staging_rows: records,
                    max_result_rows: rows,
                },
            },
        ))
    }
}

const QUERY_LIMIT_FLAGS: [&str; 4] = [
    "--max-snapshot-records",
    "--max-result-rows",
    "--max-work-units",
    "--max-scratch-entries",
];

struct ServeOptions {
    listen: String,
    http_listen: Option<String>,
    http_hosts: Vec<String>,
    websocket_listen: Option<String>,
    websocket_hosts: Vec<String>,
    websocket_origins: Vec<String>,
    bolt_listen: Option<String>,
    tls_cert_file: Option<PathBuf>,
    tls_key_file: Option<PathBuf>,
    max_connections: Option<usize>,
    databases: Vec<DatabaseOptions>,
}

fn value<'a>(args: &'a [String], at: &mut usize, flag: &str) -> Result<&'a str, Failure> {
    *at += 1;
    args.get(*at)
        .map(String::as_str)
        .ok_or_else(|| Failure::usage(format!("{flag} needs a value")))
}

fn number<T: core::str::FromStr>(raw: &str, flag: &str) -> Result<T, Failure> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Failure::usage(format!("{flag} needs a decimal number")));
    }
    raw.parse()
        .map_err(|_| Failure::usage(format!("{flag} is out of range")))
}

fn native_run_limits(raw: &str, flag: &str) -> Result<SealedLimits, Failure> {
    let mut fields = raw.split(',');
    let mut next = || number(fields.next().unwrap_or_default(), flag);
    let limits = SealedLimits {
        max_rows: next()?,
        max_incidences: next()?,
        max_image_bytes: next()?,
        max_property_bytes: next()?,
    };
    if fields.next().is_some() {
        return Err(Failure::usage(format!(
            "{flag} needs four comma-separated limits"
        )));
    }
    fgdb::BufferedReadLimits::validate_native_run_limits(limits)
        .map_err(|error| Failure::usage(format!("{flag}: {error}")))?;
    Ok(limits)
}

fn binding(raw: &str, flag: &str) -> Result<(String, u64), Failure> {
    let (name, id) = raw
        .split_once('=')
        .ok_or_else(|| Failure::usage(format!("{flag} needs <name>=<u64>")))?;
    if name.is_empty() {
        return Err(Failure::usage(format!("{flag} needs a name")));
    }
    Ok((name.to_owned(), number(id, flag)?))
}

impl ServeOptions {
    fn parse(args: &[String]) -> Result<Self, Failure> {
        let mut listen = None;
        let mut http_listen = None;
        let mut http_hosts = Vec::new();
        let mut websocket_listen = None;
        let mut websocket_hosts = Vec::new();
        let mut websocket_origins = Vec::new();
        let mut bolt_listen = None;
        let mut tls_cert_file = None;
        let mut tls_key_file = None;
        let mut max_connections = None;
        let mut databases: Vec<DatabaseOptions> = Vec::new();
        let mut at = 0;
        while at < args.len() {
            let flag = args[at].as_str();
            match flag {
                "--listen" if listen.is_none() => {
                    listen = Some(value(args, &mut at, flag)?.to_owned())
                }
                "--http-listen" if http_listen.is_none() => {
                    http_listen = Some(value(args, &mut at, flag)?.to_owned());
                }
                "--http-allow-host" => http_hosts.push(value(args, &mut at, flag)?.to_owned()),
                "--websocket-listen" if websocket_listen.is_none() => {
                    websocket_listen = Some(value(args, &mut at, flag)?.to_owned());
                }
                "--websocket-allow-host" => {
                    websocket_hosts.push(value(args, &mut at, flag)?.to_owned());
                }
                "--websocket-allow-origin" => {
                    websocket_origins.push(value(args, &mut at, flag)?.to_owned());
                }
                "--bolt-listen" if bolt_listen.is_none() => {
                    bolt_listen = Some(value(args, &mut at, flag)?.to_owned());
                }
                "--tls-cert-file" if tls_cert_file.is_none() => {
                    tls_cert_file = Some(PathBuf::from(value(args, &mut at, flag)?));
                }
                "--tls-key-file" if tls_key_file.is_none() => {
                    tls_key_file = Some(PathBuf::from(value(args, &mut at, flag)?));
                }
                "--max-connections" if max_connections.is_none() => {
                    max_connections = Some(number(value(args, &mut at, flag)?, flag)?);
                }
                "--database" => {
                    let raw = value(args, &mut at, flag)?;
                    let (name, path) = raw
                        .split_once('=')
                        .ok_or_else(|| Failure::usage("--database needs <name>=<path>"))?;
                    databases.push(DatabaseOptions {
                        name: name.to_owned(),
                        path: PathBuf::from(path),
                        key_file: None,
                        kek_passphrase_file: None,
                        strata_ecs: false,
                        issuer_key_file: None,
                        query_scratch_dir: None,
                        buffered_reads_memory_bytes: None,
                        buffered_reads_root_bytes: None,
                        buffered_reads_source_bytes: None,
                        buffered_reads_work: None,
                        buffered_reuse_live_admission: false,
                        buffered_native_run_limits: None,
                        policy_epoch: 1,
                        write_relation: 1,
                        query_limits: [None; 4],
                        maintenance_intervals: [None; 3],
                        subscription_coalesce_ticks: None,
                        symbols: Symbols::new(),
                        bindings_file: None,
                        beacon_indexes: Vec::new(),
                    });
                }
                _ => {
                    let Some(db) = databases.last_mut() else {
                        return Err(Failure::usage(format!(
                            "{flag} must follow a --database or is unknown"
                        )));
                    };
                    match flag {
                        "--key-file" if db.key_file.is_none() => {
                            db.key_file = Some(PathBuf::from(value(args, &mut at, flag)?));
                        }
                        "--kek-passphrase-file" if db.kek_passphrase_file.is_none() => {
                            db.kek_passphrase_file =
                                Some(PathBuf::from(value(args, &mut at, flag)?));
                        }
                        "--strata-ecs" if !db.strata_ecs => {
                            db.strata_ecs = true;
                        }
                        "--issuer-key-file" if db.issuer_key_file.is_none() => {
                            db.issuer_key_file = Some(PathBuf::from(value(args, &mut at, flag)?));
                        }
                        "--query-scratch-dir" if db.query_scratch_dir.is_none() => {
                            db.query_scratch_dir = Some(PathBuf::from(value(args, &mut at, flag)?));
                        }
                        "--buffered-reads-memory-bytes"
                            if db.buffered_reads_memory_bytes.is_none() =>
                        {
                            db.buffered_reads_memory_bytes =
                                Some(number(value(args, &mut at, flag)?, flag)?);
                        }
                        "--buffered-reads-root-bytes" if db.buffered_reads_root_bytes.is_none() => {
                            db.buffered_reads_root_bytes =
                                Some(number(value(args, &mut at, flag)?, flag)?);
                        }
                        "--buffered-reads-max-source-bytes"
                            if db.buffered_reads_source_bytes.is_none() =>
                        {
                            db.buffered_reads_source_bytes =
                                Some(number(value(args, &mut at, flag)?, flag)?);
                        }
                        "--buffered-reads-max-work-units" if db.buffered_reads_work.is_none() => {
                            db.buffered_reads_work =
                                Some(number(value(args, &mut at, flag)?, flag)?);
                        }
                        "--buffered-reuse-live-admission" if !db.buffered_reuse_live_admission => {
                            db.buffered_reuse_live_admission = true;
                        }
                        "--buffered-native-run-limits"
                            if db.buffered_native_run_limits.is_none() =>
                        {
                            db.buffered_native_run_limits =
                                Some(native_run_limits(value(args, &mut at, flag)?, flag)?);
                        }
                        "--policy-epoch" => {
                            db.policy_epoch = number(value(args, &mut at, flag)?, flag)?
                        }
                        "--write-relation" => {
                            db.write_relation = number(value(args, &mut at, flag)?, flag)?
                        }
                        "--bindings-file" if db.bindings_file.is_none() => {
                            db.bindings_file = Some(PathBuf::from(value(args, &mut at, flag)?));
                        }
                        "--beacon-index" => {
                            if db.beacon_indexes.len() == fgdb::MAX_MAINTAINED_BEACON_INDEXES {
                                return Err(Failure::usage(
                                    "at most 64 Beacon indexes per database",
                                ));
                            }
                            let name = value(args, &mut at, flag)?;
                            if db.beacon_indexes.iter().any(|index| index.name == name) {
                                return Err(Failure::usage(format!(
                                    "duplicate Beacon index {name:?}"
                                )));
                            }
                            db.beacon_indexes.push(BeaconIndexOptions::new(name)?);
                        }
                        "--beacon-text"
                        | "--beacon-vector"
                        | "--beacon-packed-vector"
                        | "--beacon-label"
                        | "--beacon-metric" => {
                            let index = db.beacon_indexes.last_mut().ok_or_else(|| {
                                Failure::usage(format!("{flag} must follow --beacon-index"))
                            })?;
                            index.field(flag, value(args, &mut at, flag)?)?;
                        }
                        "--max-snapshot-records"
                        | "--max-result-rows"
                        | "--max-work-units"
                        | "--max-scratch-entries" => {
                            let slot = QUERY_LIMIT_FLAGS
                                .iter()
                                .position(|limit| *limit == flag)
                                .expect("listed in QUERY_LIMIT_FLAGS");
                            if db.query_limits[slot].is_some() {
                                return Err(Failure::usage(format!("duplicate flag {flag}")));
                            }
                            db.query_limits[slot] =
                                Some(number(value(args, &mut at, flag)?, flag)?);
                        }
                        "--compact-interval-seconds"
                        | "--scrub-interval-seconds"
                        | "--beacon-checkpoint-interval-seconds" => {
                            let slot = match flag {
                                "--compact-interval-seconds" => 0,
                                "--scrub-interval-seconds" => 1,
                                _ => 2,
                            };
                            if db.maintenance_intervals[slot].is_some() {
                                return Err(Failure::usage(format!("duplicate flag {flag}")));
                            }
                            let seconds = number(value(args, &mut at, flag)?, flag)?;
                            db.maintenance_intervals[slot] = Some(Duration::from_secs(seconds));
                        }
                        "--subscription-coalesce-ticks"
                            if db.subscription_coalesce_ticks.is_none() =>
                        {
                            db.subscription_coalesce_ticks =
                                Some(number(value(args, &mut at, flag)?, flag)?);
                        }
                        "--label" | "--relation" | "--property" => {
                            let kind = match flag {
                                "--label" => GraphSymbolKind::Label,
                                "--relation" => GraphSymbolKind::Relation,
                                _ => GraphSymbolKind::Property,
                            };
                            let (name, id) = binding(value(args, &mut at, flag)?, flag)?;
                            db.symbols.bind(kind, &name, id).map_err(Failure::usage)?;
                        }
                        _ => {
                            return Err(Failure::usage(format!(
                                "unknown, duplicate, or misplaced flag {flag}"
                            )));
                        }
                    }
                }
            }
            at += 1;
        }
        let listen = listen.ok_or_else(|| Failure::usage("serve needs --listen <addr:port>"))?;
        if tls_cert_file.is_some() != tls_key_file.is_some() {
            return Err(Failure::usage(
                "--tls-cert-file and --tls-key-file are required together",
            ));
        }
        if databases.is_empty() {
            return Err(Failure::usage("serve needs at least one --database"));
        }
        if websocket_listen.is_none()
            && (!websocket_hosts.is_empty() || !websocket_origins.is_empty())
        {
            return Err(Failure::usage(
                "WebSocket allowlists require --websocket-listen",
            ));
        }
        if websocket_listen.is_some() {
            let hosts = if websocket_hosts.is_empty() {
                vec!["localhost".to_owned()]
            } else {
                websocket_hosts.clone()
            };
            WebSocketConfig::new(hosts, websocket_origins.clone()).map_err(Failure::usage)?;
        }
        for db in &databases {
            if db.buffered_reads_source_bytes.is_some() && db.buffered_reads_memory_bytes.is_none()
            {
                return Err(Failure::usage(
                    "--buffered-reads-max-source-bytes requires --buffered-reads-memory-bytes for the same database",
                ));
            }
            if db.buffered_reads_work.is_some() && db.buffered_reads_memory_bytes.is_none() {
                return Err(Failure::usage(
                    "--buffered-reads-max-work-units requires --buffered-reads-memory-bytes for the same database",
                ));
            }
            if let Some(root_bytes) = db.buffered_reads_root_bytes {
                let memory = db.buffered_reads_memory_bytes.ok_or_else(|| {
                    Failure::usage(
                        "--buffered-reads-root-bytes requires --buffered-reads-memory-bytes for the same database",
                    )
                })?;
                let maximum = 2 * BufferedReadLimits::root_bytes_for_pool(memory.get());
                if root_bytes.get() > maximum {
                    return Err(Failure::usage(format!(
                        "--buffered-reads-root-bytes exceeds {maximum} for database {:?}",
                        db.name
                    )));
                }
            }
            if db.buffered_reuse_live_admission && db.buffered_reads_memory_bytes.is_none() {
                return Err(Failure::usage(
                    "--buffered-reuse-live-admission requires --buffered-reads-memory-bytes for the same database",
                ));
            }
            if db.buffered_native_run_limits.is_some() && db.buffered_reads_memory_bytes.is_none() {
                return Err(Failure::usage(
                    "--buffered-native-run-limits requires --buffered-reads-memory-bytes for the same database",
                ));
            }
            for index in &db.beacon_indexes {
                index.validate()?;
            }
            MaintenanceSchedule::new(db.maintenance_intervals[0], db.maintenance_intervals[1])
                .and_then(|schedule| schedule.with_beacon_checkpoints(db.maintenance_intervals[2]))
                .map_err(Failure::usage)?;
            if db.key_file.is_none() || db.issuer_key_file.is_none() {
                return Err(Failure::usage(format!(
                    "database {:?} needs --key-file and --issuer-key-file",
                    db.name
                )));
            }
        }
        Ok(Self {
            listen,
            http_listen,
            http_hosts,
            websocket_listen,
            websocket_hosts,
            websocket_origins,
            bolt_listen,
            tls_cert_file,
            tls_key_file,
            max_connections,
            databases,
        })
    }
}

/// A bindings file's bytes, at most one past the parser's ceiling, so a
/// mistaken path to a huge file is refused without reading it whole. A FIFO
/// or device is refused BEFORE it is opened: opening a FIFO blocks until a
/// writer appears.
async fn read_bindings_bytes(path: &std::path::Path) -> Result<Vec<u8>, Failure> {
    use asupersync::io::AsyncReadExt as _;
    let named = asupersync::fs::metadata(path).await.map_err(Failure::io)?;
    if !named.is_file() {
        return Err(Failure::usage("bindings file must be a regular file"));
    }
    let file = asupersync::fs::File::open(path)
        .await
        .map_err(Failure::io)?;
    let mut bytes = Vec::new();
    file.take(fgdb_gql::symbol_bindings::MAX_SYMBOL_BINDINGS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(Failure::io)?;
    Ok(bytes)
}

/// Index options contain names, while the native index owner consumes ids.
/// Discover only authenticated catalog metadata here, then pin its publication
/// through the ordinary full open so a concurrent writer cannot change the
/// meaning of those prebound definitions between the two opens.
async fn configured_beacon_indexes(
    cx: &Cx,
    path: &std::path::Path,
    keys: &fgdb::DatabaseKeys,
    declarations: &Symbols,
    definitions: &[BeaconIndexOptions],
    limits: [u64; 4],
) -> Result<(Vec<BeaconIndexConfig>, Option<fgdb_types::CommitSeq>), Failure> {
    if definitions.is_empty() {
        return Ok((Vec::new(), None));
    }
    let contexts = fgdb_types::PurposeContexts::narrow_runtime_root(cx);
    let catalog = fgdb::Database::open_symbol_catalog(&contexts.commit(), path, keys.clone())
        .await
        .map_err(Failure::open)?;
    let symbols = declarations
        .with_catalog(catalog.symbol_catalog())
        .map_err(Failure::usage)?;
    let mut indexes = Vec::with_capacity(definitions.len());
    for definition in definitions {
        cx.checkpoint().map_err(Failure::io)?;
        indexes.push(definition.bind(&symbols, limits)?);
    }
    Ok((indexes, Some(catalog.frontier())))
}

async fn serve(cx: &Cx, options: ServeOptions) -> Result<(), Failure> {
    let mut limits = ServerLimits::default();
    if let Some(max) = options.max_connections {
        limits.max_connections = max;
    }
    let mut maintenance_jobs = Vec::new();
    for db in &options.databases {
        let schedule =
            MaintenanceSchedule::new(db.maintenance_intervals[0], db.maintenance_intervals[1])
                .and_then(|schedule| schedule.with_beacon_checkpoints(db.maintenance_intervals[2]))
                .map_err(Failure::usage)?;
        schedule.validate(cx).map_err(Failure::usage)?;
        if schedule.is_enabled() {
            maintenance_jobs.push((db.name.clone(), schedule));
        }
    }
    let mut server = Server::new(cx, limits).map_err(Failure::usage)?;
    let tls_enabled = options.tls_cert_file.is_some();
    if let (Some(cert), Some(key)) = (&options.tls_cert_file, &options.tls_key_file) {
        let config = fgdb_server::TlsConfig::from_pem_files(cx, cert, key)
            .await
            .map_err(Failure::open)?;
        server.enable_tls(config);
    }
    for mut db in options.databases {
        if let Some(path) = &db.bindings_file {
            cx.checkpoint().map_err(Failure::io)?;
            let bytes = read_bindings_bytes(path).await?;
            let bindings =
                fgdb_gql::symbol_bindings::parse_symbol_bindings(&bytes).map_err(Failure::usage)?;
            for binding in bindings {
                db.symbols
                    .bind(binding.kind, &binding.name, binding.id)
                    .map_err(|error| {
                        Failure::usage(format!("bindings file line {}: {error}", binding.line))
                    })?;
            }
        }
        let keys = fgdb_server::read_database_keys_with_options(
            cx,
            db.key_file.as_deref().expect("checked"),
            db.kek_passphrase_file.as_deref(),
            db.strata_ecs,
        )
        .await
        .map_err(Failure::open)?;
        let issuer_key =
            fgdb_server::read_issuer_key(cx, db.issuer_key_file.as_deref().expect("checked"))
                .await
                .map_err(Failure::open)?;
        let query_limits = core::array::from_fn(|at| {
            db.query_limits[at].unwrap_or(DatabaseConfig::DEFAULT_QUERY_LIMITS[at])
        });
        let (beacon_indexes, catalog_frontier) = configured_beacon_indexes(
            cx,
            &db.path,
            &keys,
            &db.symbols,
            &db.beacon_indexes,
            query_limits,
        )
        .await?;
        let buffered_reads = db.buffered_read_config();
        let mut config = DatabaseConfig::new(db.name, keys, issuer_key);
        config.beacon_indexes = beacon_indexes;
        config.catalog_frontier = catalog_frontier;
        config.policy_epoch = db.policy_epoch;
        config.write_relation = RelationId(db.write_relation);
        config.symbols = db.symbols;
        config.ordered_reads = db.query_scratch_dir.map(OrderedReadConfig::new);
        config.buffered_reads = buffered_reads;
        if let Some(ticks) = db.subscription_coalesce_ticks {
            config.subscription_max_coalesced_ticks = ticks;
        }
        config.set_query_limits(query_limits);
        server
            .open_database(cx, &db.path, config)
            .await
            .map_err(Failure::open)?;
    }
    let addr: std::net::SocketAddr = options
        .listen
        .parse()
        .map_err(|_| Failure::usage("--listen needs <ip>:<port>"))?;
    let listener = TcpListener::bind(addr).await.map_err(Failure::io)?;
    let http = match &options.http_listen {
        None => None,
        Some(raw) => {
            let addr: std::net::SocketAddr = raw
                .parse()
                .map_err(|_| Failure::usage("--http-listen needs <ip>:<port>"))?;
            let mut hosts = options.http_hosts.clone();
            if hosts.is_empty() {
                hosts = vec!["localhost".into(), "127.0.0.1".into(), "[::1]".into()];
                hosts.push(addr.ip().to_string());
            }
            Some((TcpListener::bind(addr).await.map_err(Failure::io)?, hosts))
        }
    };
    let websocket = match &options.websocket_listen {
        None => None,
        Some(raw) => {
            let addr: std::net::SocketAddr = raw
                .parse()
                .map_err(|_| Failure::usage("--websocket-listen needs <ip>:<port>"))?;
            let mut hosts = options.websocket_hosts.clone();
            if hosts.is_empty() {
                hosts = vec!["localhost".into(), "127.0.0.1".into(), "[::1]".into()];
                hosts.push(match addr.ip() {
                    std::net::IpAddr::V4(ip) => ip.to_string(),
                    std::net::IpAddr::V6(ip) => format!("[{ip}]"),
                });
            }
            let config = WebSocketConfig::new(hosts, options.websocket_origins.clone())
                .map_err(Failure::usage)?;
            Some((TcpListener::bind(addr).await.map_err(Failure::io)?, config))
        }
    };
    let bolt = match &options.bolt_listen {
        None => None,
        Some(raw) => {
            let addr: std::net::SocketAddr = raw
                .parse()
                .map_err(|_| Failure::usage("--bolt-listen needs <ip>:<port>"))?;
            Some(TcpListener::bind(addr).await.map_err(Failure::io)?)
        }
    };
    {
        let mut stdout = std::io::stdout().lock();
        let bound = listener.local_addr().map_err(Failure::io)?;
        writeln!(
            stdout,
            r#"{{"v":1,"event":"listening","protocol":"fgp","addr":"{bound}","tls":{tls_enabled}}}"#
        )
        .map_err(Failure::io)?;
        if let Some((http, _)) = &http {
            let bound = http.local_addr().map_err(Failure::io)?;
            writeln!(
                stdout,
                r#"{{"v":1,"event":"listening","protocol":"http","addr":"{bound}","tls":{tls_enabled}}}"#
            )
            .map_err(Failure::io)?;
        }
        if let Some((websocket, _)) = &websocket {
            let bound = websocket.local_addr().map_err(Failure::io)?;
            writeln!(
                stdout,
                r#"{{"v":1,"event":"listening","protocol":"websocket","addr":"{bound}","tls":{tls_enabled}}}"#
            )
            .map_err(Failure::io)?;
        }
        if let Some(bolt) = &bolt {
            let bound = bolt.local_addr().map_err(Failure::io)?;
            writeln!(
                stdout,
                r#"{{"v":1,"event":"listening","protocol":"bolt","addr":"{bound}","tls":{tls_enabled}}}"#
            )
            .map_err(Failure::io)?;
        }
        stdout.flush().map_err(Failure::io)?;
    }
    let server = Arc::new(server);
    // Closing this owner also wakes listener/maintenance children if the
    // serving future is dropped during startup or while awaiting a join.
    let _drain = DrainOnDrop(server.shutdown());
    let mut workers: Vec<asupersync::runtime::TaskHandle<Result<(), Failure>>> = Vec::new();
    let mut result = async {
        for watch in [asupersync::signal::sigint, asupersync::signal::sigterm] {
            let shutdown = server.shutdown();
            let mut signal = watch().map_err(Failure::io)?;
            cx.spawn(move |_| async move {
                if signal.recv().await.is_some() {
                    shutdown.trigger();
                }
            })
            .map_err(|_| Failure::io("cannot install a signal watcher"))?;
        }
        if let Some((listener, hosts)) = http {
            let server = Arc::clone(&server);
            workers.push(
                cx.spawn(move |child| async move {
                    server
                        .serve_http(&child, listener, hosts)
                        .await
                        .map_err(Failure::io)
                })
                .map_err(|_| Failure::io("cannot start the HTTP listener"))?,
            );
        }
        if let Some((listener, config)) = websocket {
            let server = Arc::clone(&server);
            workers.push(
                cx.spawn(move |child| async move {
                    server
                        .serve_websocket(&child, listener, config)
                        .await
                        .map_err(Failure::io)
                })
                .map_err(|_| Failure::io("cannot start the WebSocket listener"))?,
            );
        }
        if let Some(listener) = bolt {
            let server = Arc::clone(&server);
            workers.push(
                cx.spawn(move |child| async move {
                    server
                        .serve_bolt(&child, listener)
                        .await
                        .map_err(Failure::io)
                })
                .map_err(|_| Failure::io("cannot start the Bolt listener"))?,
            );
        }
        for (name, schedule) in maintenance_jobs {
            let server = Arc::clone(&server);
            workers.push(
                cx.spawn(move |child| async move {
                    let result = server
                        .run_maintenance(&child, &name, schedule, |event| {
                            report_maintenance(&name, event);
                        })
                        .await
                        .map_err(Failure::io);
                    if result.is_err() {
                        server.shutdown().trigger();
                    }
                    result
                })
                .map_err(|_| Failure::io("cannot start database maintenance"))?,
            );
        }
        Arc::clone(&server)
            .serve(cx, listener)
            .await
            .map_err(Failure::io)
    }
    .await;
    // Retain the primary error while every already-started child drains.
    // This also covers a later listener/maintenance spawn failing at startup.
    server.shutdown().trigger();
    for mut worker in workers {
        let outcome = worker
            .join(cx)
            .await
            .map_err(|error| Failure::io(format!("server child failed: {error}")))
            .and_then(core::convert::identity);
        if result.is_ok() {
            result = outcome;
        }
    }
    server.finish_shutdown(cx).await;
    result?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, r#"{{"v":1,"event":"stopped"}}"#).map_err(Failure::io)
}

struct DrainOnDrop(fgdb_server::Shutdown);

impl Drop for DrainOnDrop {
    fn drop(&mut self) {
        self.0.trigger();
    }
}

fn report_maintenance(name: &str, event: MaintenanceEvent) {
    match event {
        MaintenanceEvent::Compaction(Ok(())) => {
            eprintln!("fgdbd: database {name:?}: compaction completed");
        }
        MaintenanceEvent::Compaction(Err(error)) => {
            eprintln!("fgdbd: database {name:?}: compaction failed: {error}");
        }
        MaintenanceEvent::Scrub(Err(error)) => {
            eprintln!("fgdbd: database {name:?}: scrub failed: {error}");
        }
        MaintenanceEvent::BeaconCheckpoint(Err(error)) => {
            eprintln!("fgdbd: database {name:?}: Beacon checkpoint failed: {error}");
        }
        MaintenanceEvent::BeaconCheckpoint(Ok(reports)) => {
            for report in reports {
                match report.result {
                    Ok(sequence) => eprintln!(
                        "fgdbd: database {name:?}: Beacon index {:?} checkpoint current at {}",
                        report.name, sequence.0,
                    ),
                    Err(error) => eprintln!(
                        "fgdbd: database {name:?}: Beacon index {:?} checkpoint failed: {error}",
                        report.name,
                    ),
                }
            }
        }
        MaintenanceEvent::Scrub(Ok(summary)) => {
            if summary.block_repaired.is_empty() && summary.metadata_repaired.is_empty() {
                eprintln!(
                    "fgdbd: database {name:?}: scrub completed: capsules={}, repaired={}, lost={}, blocks={}, block_lost={}, metadata={}, metadata_lost={}",
                    summary.objects,
                    summary.repaired.len(),
                    summary.lost.len(),
                    summary.block_objects,
                    summary.block_lost.len(),
                    summary.metadata_objects,
                    summary.metadata_lost.len(),
                );
            } else {
                eprintln!(
                    "fgdbd: database {name:?}: scrub completed: capsules={}, repaired={}, lost={}, blocks={}, block_repaired={}, block_lost={}, metadata={}, metadata_repaired={}, metadata_lost={}",
                    summary.objects,
                    summary.repaired.len(),
                    summary.lost.len(),
                    summary.block_objects,
                    summary.block_repaired.len(),
                    summary.block_lost.len(),
                    summary.metadata_objects,
                    summary.metadata_repaired.len(),
                    summary.metadata_lost.len(),
                );
            }
            if !summary.lost.is_empty()
                || !summary.block_lost.is_empty()
                || !summary.metadata_lost.is_empty()
            {
                eprintln!(
                    "fgdbd: database {name:?}: scrub found lost objects; authoritative recovery requested"
                );
            }
        }
    }
}

struct TokenOptions {
    key_file: PathBuf,
    kek_passphrase_file: Option<PathBuf>,
    issuer_key_file: PathBuf,
    policy_epoch: u64,
    rights: Rights,
    expires_in_seconds: u64,
    labels: Option<Vec<u64>>,
    relations: Option<Vec<u64>>,
    properties: Option<Vec<u64>>,
    limits: QueryLimits,
}

impl TokenOptions {
    fn parse(args: &[String]) -> Result<Self, Failure> {
        let mut key_file = None;
        let mut kek_passphrase_file = None;
        let mut issuer_key_file = None;
        let mut options = Self {
            key_file: PathBuf::new(),
            kek_passphrase_file: None,
            issuer_key_file: PathBuf::new(),
            policy_epoch: 1,
            rights: Rights::Read,
            expires_in_seconds: 86_400,
            labels: None,
            relations: None,
            properties: None,
            limits: QueryLimits {
                max_nodes: 1_000_000,
                max_work: 100_000_000,
                max_rows: 1_000_000,
            },
        };
        let mut at = 0;
        while at < args.len() {
            let flag = args[at].as_str();
            match flag {
                "--key-file" if key_file.is_none() => {
                    key_file = Some(PathBuf::from(value(args, &mut at, flag)?))
                }
                "--kek-passphrase-file" if kek_passphrase_file.is_none() => {
                    kek_passphrase_file = Some(PathBuf::from(value(args, &mut at, flag)?))
                }
                "--issuer-key-file" if issuer_key_file.is_none() => {
                    issuer_key_file = Some(PathBuf::from(value(args, &mut at, flag)?));
                }
                "--policy-epoch" => {
                    options.policy_epoch = number(value(args, &mut at, flag)?, flag)?
                }
                "--rights" => {
                    options.rights = match value(args, &mut at, flag)? {
                        "read" => Rights::Read,
                        "write" => Rights::Write,
                        "read-write" => Rights::ReadWrite,
                        "admin" => Rights::Admin,
                        _ => {
                            return Err(Failure::usage(
                                "--rights is read, write, read-write or admin",
                            ));
                        }
                    };
                }
                "--expires-in" => {
                    options.expires_in_seconds = number(value(args, &mut at, flag)?, flag)?
                }
                "--allow-label" => options
                    .labels
                    .get_or_insert_with(Vec::new)
                    .push(number(value(args, &mut at, flag)?, flag)?),
                "--allow-relation" => options
                    .relations
                    .get_or_insert_with(Vec::new)
                    .push(number(value(args, &mut at, flag)?, flag)?),
                "--allow-property" => options
                    .properties
                    .get_or_insert_with(Vec::new)
                    .push(number(value(args, &mut at, flag)?, flag)?),
                "--max-nodes" => {
                    options.limits.max_nodes = number(value(args, &mut at, flag)?, flag)?
                }
                "--max-work" => {
                    options.limits.max_work = number(value(args, &mut at, flag)?, flag)?
                }
                "--max-rows" => {
                    options.limits.max_rows = number(value(args, &mut at, flag)?, flag)?
                }
                _ => return Err(Failure::usage(format!("unknown or duplicate flag {flag}"))),
            }
            at += 1;
        }
        options.key_file = key_file.ok_or_else(|| Failure::usage("token needs --key-file"))?;
        options.kek_passphrase_file = kek_passphrase_file;
        options.issuer_key_file =
            issuer_key_file.ok_or_else(|| Failure::usage("token needs --issuer-key-file"))?;
        if options.expires_in_seconds == 0 {
            return Err(Failure::usage("--expires-in must be positive"));
        }
        Ok(options)
    }

    fn into_grant(self, expires_at_ms: u64) -> Grant {
        fn scope<T: Ord>(ids: Option<Vec<u64>>, wrap: impl Fn(u64) -> T) -> Scope<T> {
            ids.map_or(Scope::All, |ids| Scope::only(ids.into_iter().map(wrap)))
        }
        Grant {
            branch: fgdb_server::TRUNK.to_owned(),
            labels: scope(self.labels, LabelId),
            relations: scope(self.relations, RelationId),
            properties: scope(self.properties, PropertyKeyId),
            rights: self.rights,
            limits: self.limits,
            expires_at_ms,
        }
    }
}

async fn token(cx: &Cx, options: TokenOptions) -> Result<(), Failure> {
    let keys = fgdb_server::read_database_keys_with_options(
        cx,
        &options.key_file,
        options.kek_passphrase_file.as_deref(),
        false,
    )
    .await
    .map_err(Failure::open)?;
    let issuer_key = fgdb_server::read_issuer_key(cx, &options.issuer_key_file)
        .await
        .map_err(Failure::open)?;
    let authority =
        fgdb_server::issuer(issuer_key, &keys, options.policy_epoch).map_err(Failure::usage)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(Failure::io)?;
    let expires_at_ms = u64::try_from(now.as_millis())
        .map_err(Failure::io)?
        .checked_add(options.expires_in_seconds.saturating_mul(1000))
        .ok_or_else(|| Failure::usage("--expires-in is out of range"))?;
    let grant = options.into_grant(expires_at_ms);
    let token = fgdb_server::issue_token(&authority, &grant).map_err(Failure::usage)?;
    let hex: String = token.iter().map(|byte| format!("{byte:02x}")).collect();
    println!("{hex}");
    Ok(())
}

/// `fgdbd attenuate` flags. Each restriction kind may be given once (the
/// --allow-*/--deny-* lists accumulate into one caveat per kind).
#[derive(Default)]
struct AttenuateOptions {
    rights: Option<Rights>,
    expires_in_seconds: Option<u64>,
    not_before_in_seconds: Option<u64>,
    labels: Option<Vec<u64>>,
    relations: Option<Vec<u64>>,
    properties: Option<Vec<u64>>,
    deny_properties: Option<Vec<u64>>,
    max_nodes: Option<u64>,
    max_work: Option<u64>,
    max_rows: Option<u64>,
}

impl AttenuateOptions {
    fn parse(args: &[String]) -> Result<Self, Failure> {
        let mut options = Self::default();
        let mut at = 0;
        while at < args.len() {
            let flag = args[at].as_str();
            match flag {
                "--rights" if options.rights.is_none() => {
                    options.rights = Some(match value(args, &mut at, flag)? {
                        "read" => Rights::Read,
                        "write" => Rights::Write,
                        "read-write" => Rights::ReadWrite,
                        "admin" => Rights::Admin,
                        _ => {
                            return Err(Failure::usage(
                                "--rights is read, write, read-write or admin",
                            ));
                        }
                    });
                }
                "--expires-in" if options.expires_in_seconds.is_none() => {
                    let seconds = number(value(args, &mut at, flag)?, flag)?;
                    if seconds == 0 {
                        return Err(Failure::usage("--expires-in must be positive"));
                    }
                    options.expires_in_seconds = Some(seconds);
                }
                "--not-before-in" if options.not_before_in_seconds.is_none() => {
                    options.not_before_in_seconds =
                        Some(number(value(args, &mut at, flag)?, flag)?);
                }
                "--allow-label" => options
                    .labels
                    .get_or_insert_with(Vec::new)
                    .push(number(value(args, &mut at, flag)?, flag)?),
                "--allow-relation" => options
                    .relations
                    .get_or_insert_with(Vec::new)
                    .push(number(value(args, &mut at, flag)?, flag)?),
                "--allow-property" => options
                    .properties
                    .get_or_insert_with(Vec::new)
                    .push(number(value(args, &mut at, flag)?, flag)?),
                "--deny-property" => options
                    .deny_properties
                    .get_or_insert_with(Vec::new)
                    .push(number(value(args, &mut at, flag)?, flag)?),
                "--max-nodes" if options.max_nodes.is_none() => {
                    options.max_nodes = Some(number(value(args, &mut at, flag)?, flag)?);
                }
                "--max-work" if options.max_work.is_none() => {
                    options.max_work = Some(number(value(args, &mut at, flag)?, flag)?);
                }
                "--max-rows" if options.max_rows.is_none() => {
                    options.max_rows = Some(number(value(args, &mut at, flag)?, flag)?);
                }
                _ => return Err(Failure::usage(format!("unknown or duplicate flag {flag}"))),
            }
            at += 1;
        }
        if options.restrictions(0)?.is_empty() {
            return Err(Failure::usage(
                "attenuate needs at least one restriction flag",
            ));
        }
        Ok(options)
    }

    /// The caveats to append, with validity bounds measured from `now_ms`.
    fn restrictions(&self, now_ms: u64) -> Result<Vec<Restriction>, Failure> {
        fn ids<T: Ord>(ids: &[u64], wrap: impl Fn(u64) -> T) -> BTreeSet<T> {
            ids.iter().copied().map(wrap).collect()
        }
        let from_now = |seconds: u64, flag: &str| {
            seconds
                .checked_mul(1000)
                .and_then(|ms| now_ms.checked_add(ms))
                .ok_or_else(|| Failure::usage(format!("{flag} is out of range")))
        };
        let mut out = Vec::new();
        if let Some(rights) = self.rights {
            out.push(Restriction::Rights(rights));
        }
        if let Some(labels) = &self.labels {
            out.push(Restriction::Labels(Scope::Only(ids(labels, LabelId))));
        }
        if let Some(relations) = &self.relations {
            out.push(Restriction::Relations(Scope::Only(ids(
                relations, RelationId,
            ))));
        }
        if let Some(properties) = &self.properties {
            out.push(Restriction::Properties(Scope::Only(ids(
                properties,
                PropertyKeyId,
            ))));
        }
        if let Some(denied) = &self.deny_properties {
            out.push(Restriction::DenyProperties(ids(denied, PropertyKeyId)));
        }
        if let Some(limit) = self.max_nodes {
            out.push(Restriction::MaxNodes(limit));
        }
        if let Some(limit) = self.max_work {
            out.push(Restriction::MaxWork(limit));
        }
        if let Some(limit) = self.max_rows {
            out.push(Restriction::MaxRows(limit));
        }
        if let Some(seconds) = self.expires_in_seconds {
            out.push(Restriction::ExpiresBefore(from_now(
                seconds,
                "--expires-in",
            )?));
        }
        if let Some(seconds) = self.not_before_in_seconds {
            out.push(Restriction::NotBefore(from_now(
                seconds,
                "--not-before-in",
            )?));
        }
        Ok(out)
    }
}

fn attenuate(options: AttenuateOptions) -> Result<(), Failure> {
    use std::io::Read as _;
    // A token is a few KiB of hex; read a bounded prefix and refuse the rest.
    let mut input = String::new();
    std::io::stdin()
        .lock()
        .take(1 << 20)
        .read_to_string(&mut input)
        .map_err(Failure::io)?;
    let token = fgdb_protocol::json::bytes_from_hex(input.trim())
        .map_err(|_| Failure::usage("attenuate reads one hex token on stdin"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(Failure::io)?;
    let now_ms = u64::try_from(now.as_millis()).map_err(Failure::io)?;
    let narrowed = fgdb_server::attenuate_token(&token, &options.restrictions(now_ms)?)
        .map_err(|error| Failure::usage(format!("token refused: {error:?}")))?;
    let hex: String = narrowed.iter().map(|byte| format!("{byte:02x}")).collect();
    println!("{hex}");
    Ok(())
}

async fn keygen(cx: &Cx, args: &[String]) -> Result<(), Failure> {
    let (lines, path, passphrase_path) = match args {
        [flag, path] if flag == "--database" => (3, path, None),
        [flag, path, pass_flag, pass_path]
            if flag == "--database" && pass_flag == "--passphrase-file" =>
        {
            (3, path, Some(std::path::Path::new(pass_path)))
        }
        [pass_flag, pass_path, flag, path]
            if flag == "--database" && pass_flag == "--passphrase-file" =>
        {
            (3, path, Some(std::path::Path::new(pass_path)))
        }
        [flag, path] if flag == "--issuer" => (1, path, None),
        _ => {
            return Err(Failure::usage(
                "keygen needs --database <path> [--passphrase-file <path>] or --issuer <path>",
            ));
        }
    };
    let text = if let Some(passphrase_path) = passphrase_path {
        let passphrase = fgdb_server::read_passphrase_file(cx, passphrase_path)
            .await
            .map_err(Failure::open)?;
        let mut k_oid = [0u8; 32];
        let mut namespace = [0u8; 32];
        let mut dek = [0u8; 32];
        let mut salt = [0u8; 16];
        let mut nonce = [0u8; 24];
        cx.random_bytes(&mut k_oid);
        cx.random_bytes(&mut namespace);
        cx.random_bytes(&mut dek);
        cx.random_bytes(&mut salt);
        cx.random_bytes(&mut nonce);
        let keys = fgdb::DatabaseKeys::new(
            k_oid,
            fgdb_types::DatabaseSecurityNamespaceId(namespace),
            dek,
        );
        keys.seal_passphrase_envelope(passphrase.as_bytes(), salt, nonce)
            .map_err(Failure::open)?
    } else {
        let mut text = String::new();
        for _ in 0..lines {
            let mut key = [0u8; 32];
            cx.random_bytes(&mut key);
            for byte in key {
                text.push_str(&format!("{byte:02x}"));
            }
            text.push('\n');
        }
        text
    };
    let options = asupersync::fs::OpenOptions::new()
        .write(true)
        .create_new(true);
    #[cfg(unix)]
    let options = options.mode(0o600);
    let mut file = options.open(path).await.map_err(Failure::io)?;
    file.write_all(text.as_bytes()).await.map_err(Failure::io)?;
    file.sync_all().await.map_err(Failure::io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beacon_options(extra: &[&str]) -> Result<ServeOptions, Failure> {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "first=first-db",
            "--key-file",
            "first.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
        ServeOptions::parse(&args)
    }

    #[test]
    fn beacon_indexes_bind_combined_lanes_after_the_complete_catalog_is_available() {
        let mut parsed = beacon_options(&[
            "--beacon-index",
            "retrieval",
            "--beacon-text",
            "body",
            "--beacon-vector",
            "y,x,y",
            "--beacon-label",
            "Document",
            "--beacon-metric",
            "cosine",
            "--property",
            "x=4294967296",
            "--property",
            "y=18446744073709551615",
        ])
        .unwrap_or_else(|error| panic!("{}", error.message));
        let db = &mut parsed.databases[0];
        let index = &db.beacon_indexes[0];
        // Definitions cannot create symbols or reuse one from another kind.
        assert!(index.bind(&db.symbols, [11, 12, 13, 14]).is_err());
        db.symbols.bind(GraphSymbolKind::Label, "body", 20).unwrap();
        assert!(index.bind(&db.symbols, [11, 12, 13, 14]).is_err());
        // This is the same merge that serve performs after --bindings-file.
        for binding in fgdb_gql::symbol_bindings::parse_symbol_bindings(
            b"property body=7\nlabel Document=18446744073709551615\n",
        )
        .unwrap()
        {
            db.symbols
                .bind(binding.kind, &binding.name, binding.id)
                .unwrap();
        }
        let bound = index
            .bind(&db.symbols, [11, 12, 13, 14])
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(bound.name, "retrieval");
        assert_eq!(bound.options.as_of, None);
        assert_eq!(bound.options.vertex_label, Some(LabelId(u64::MAX)));
        assert_eq!(bound.options.projection.text, Some(PropertyKeyId(7)));
        assert_eq!(
            bound.options.projection.vector,
            [
                PropertyKeyId(u64::MAX),
                PropertyKeyId(4_294_967_296),
                PropertyKeyId(u64::MAX)
            ]
        );
        assert_eq!(
            bound.options.projection.encoding,
            VectorEncoding::CoordinatesRoundedF32
        );
        assert_eq!(
            bound.options.index.vector,
            Some(HnswConfig::new(3, DistanceMetric::Cosine).exact_only())
        );
        assert_eq!(bound.options.index.text, IndexConfig::default().text);
        let policy = ReadPolicy {
            max_work_units: 13,
            max_source_scratch: 14,
            max_staging_rows: 11,
            max_result_rows: 12,
        };
        assert_eq!(bound.options.policy, policy);
        assert_eq!(bound.maintenance, policy);
    }

    #[test]
    fn beacon_discovery_uses_durable_names_and_refuses_a_changed_startup_publication() {
        use asupersync::lab::run_async_under_lab;
        use asupersync::security::key::AuthKey;
        use fgdb::{Database, DatabaseKeys, WriteBatch};
        use fgdb_delta_types::catalog::{CatalogSymbol, SymbolCatalog};
        use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

        let ((), report) = run_async_under_lab(0x7a8c_0020, |root| async move {
            let path =
                std::env::temp_dir().join(format!("fgdbd-beacon-catalog-{}", std::process::id()));
            let keys = DatabaseKeys::new(
                [0x81; 32],
                DatabaseSecurityNamespaceId([0x82; 32]),
                [0x83; 32],
            );
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut database = Database::create(&contexts.commit(), &path, keys.clone())
                .await
                .unwrap();
            let mut catalog = SymbolCatalog::default();
            catalog
                .bind_all([
                    ("Document", CatalogSymbol::Label(LabelId(23))),
                    ("body", CatalogSymbol::Property(PropertyKeyId(41))),
                    ("x", CatalogSymbol::Property(PropertyKeyId(42))),
                ])
                .unwrap();
            assert_eq!(
                database
                    .register_symbols(&contexts.commit(), &catalog)
                    .await
                    .unwrap(),
                CommitSeq(1)
            );
            drop(database);
            let parsed = beacon_options(&[
                "--beacon-index",
                "retrieval",
                "--beacon-text",
                "body",
                "--beacon-vector",
                "x",
                "--beacon-label",
                "Document",
            ])
            .unwrap_or_else(|error| panic!("{}", error.message));
            let definitions = &parsed.databases[0].beacon_indexes;
            let mut declarations = Symbols::new();
            declarations
                .bind(GraphSymbolKind::Property, "ephemeral", 99)
                .unwrap();
            let (indexes, frontier) = configured_beacon_indexes(
                &root,
                &path,
                &keys,
                &declarations,
                definitions,
                [100, 100, 10_000, 10_000],
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(frontier, Some(CommitSeq(1)));
            assert_eq!(indexes.len(), 1);
            assert_eq!(indexes[0].options.vertex_label, Some(LabelId(23)));
            assert_eq!(indexes[0].options.projection.text, Some(PropertyKeyId(41)));
            assert_eq!(
                indexes[0].options.projection.vector,
                vec![PropertyKeyId(42)]
            );
            assert_eq!(
                declarations.resolve(GraphSymbolKind::Property, "body"),
                None
            );

            let mut conflicting = declarations.clone();
            conflicting
                .bind(GraphSymbolKind::Property, "body", 7)
                .unwrap();
            let error = match configured_beacon_indexes(
                &root,
                &path,
                &keys,
                &conflicting,
                definitions,
                [100, 100, 10_000, 10_000],
            )
            .await
            {
                Err(error) => error,
                Ok(_) => panic!("a conflicting durable name must refuse"),
            };
            assert_eq!(error.code, 2);
            assert!(error.message.contains("conflicting property binding"));

            // An ordinary commit between metadata discovery and full startup
            // makes the prebound configuration stale. It must not be admitted.
            let mut database = Database::open(&contexts.commit(), &path, keys.clone())
                .await
                .unwrap();
            assert_eq!(database.symbol_catalog().unwrap(), &catalog);
            let mut write = WriteBatch::new(RelationId(1));
            write.create_vertex(VId(1), vec![], vec![]);
            database.write(&contexts.commit(), write).await.unwrap();
            drop(database);
            let mut config = DatabaseConfig::new("test", keys, AuthKey::from_seed(0x7a8c_0020));
            config.beacon_indexes = indexes;
            config.catalog_frontier = frontier;
            config.symbols = declarations;
            let mut server = Server::new(&root, ServerLimits::default()).unwrap();
            assert!(matches!(
                server.open_database(&root, &path, config).await,
                Err(fgdb_server::ServerError::CatalogChanged {
                    expected: CommitSeq(1),
                    actual: CommitSeq(2),
                })
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn beacon_definitions_and_defaults_are_isolated_between_databases() {
        let parsed = beacon_options(&[
            "--beacon-index",
            "text",
            "--beacon-text",
            "body",
            "--property",
            "body=1",
            "--beacon-index",
            "embedding",
            "--beacon-packed-vector",
            "embedding=384",
            "--property",
            "embedding=2",
            "--database",
            "second=second-db",
            "--key-file",
            "second.key",
            "--issuer-key-file",
            "issuer.key",
            "--beacon-index",
            "text",
            "--beacon-vector",
            "position",
            "--beacon-metric",
            "dot",
            "--property",
            "position=5",
        ])
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(parsed.databases[0].beacon_indexes.len(), 2);
        assert_eq!(parsed.databases[1].beacon_indexes.len(), 1);
        let bind = |db: usize, index: usize| {
            let db = &parsed.databases[db];
            db.beacon_indexes[index]
                .bind(&db.symbols, DatabaseConfig::DEFAULT_QUERY_LIMITS)
                .unwrap_or_else(|error| panic!("{}", error.message))
        };
        let text = bind(0, 0);
        assert!(text.options.index.text.is_some());
        assert!(text.options.index.vector.is_none());
        let packed = bind(0, 1);
        assert_eq!(packed.options.projection.vector, [PropertyKeyId(2)]);
        assert_eq!(
            packed.options.projection.encoding,
            VectorEncoding::PackedF32Le
        );
        assert!(packed.options.index.text.is_none());
        assert_eq!(
            packed.options.index.vector,
            Some(HnswConfig::new(384, DistanceMetric::SquaredEuclidean).exact_only())
        );
        let second = bind(1, 0);
        assert_eq!(second.options.projection.vector, [PropertyKeyId(5)]);
        assert_eq!(
            second.options.index.vector,
            Some(HnswConfig::new(1, DistanceMetric::NegativeDotProduct).exact_only())
        );
        assert!(
            beacon_options(&[])
                .unwrap_or_else(|error| panic!("{}", error.message))
                .databases[0]
                .beacon_indexes
                .is_empty()
        );
    }

    #[test]
    fn beacon_options_refuse_ambiguous_incomplete_or_unbounded_definitions() {
        for extra in [
            vec!["--beacon-index"],
            vec!["--beacon-index", ""],
            vec!["--beacon-index", "bad/name", "--beacon-text", "body"],
            vec!["--beacon-index", "empty"],
            vec!["--beacon-text", "body"],
            vec!["--beacon-index", "one", "--beacon-text", ""],
            vec![
                "--beacon-index",
                "one",
                "--beacon-text",
                "body",
                "--beacon-text",
                "title",
            ],
            vec![
                "--beacon-index",
                "one",
                "--beacon-text",
                "body",
                "--beacon-index",
                "one",
                "--beacon-text",
                "title",
            ],
            vec![
                "--beacon-index",
                "one",
                "--beacon-text",
                "body",
                "--beacon-metric",
                "l2",
            ],
            vec!["--beacon-index", "one", "--beacon-vector", "x,"],
            vec!["--beacon-index", "one", "--beacon-vector", ",x"],
            vec!["--beacon-index", "one", "--beacon-vector", "x,,y"],
            vec![
                "--beacon-index",
                "one",
                "--beacon-vector",
                "x",
                "--beacon-packed-vector",
                "e=2",
            ],
            vec![
                "--beacon-index",
                "one",
                "--beacon-packed-vector",
                "e=2",
                "--beacon-vector",
                "x",
            ],
            vec![
                "--beacon-index",
                "one",
                "--beacon-vector",
                "x",
                "--beacon-metric",
                "unknown",
            ],
            vec![
                "--beacon-index",
                "one",
                "--beacon-vector",
                "x",
                "--beacon-metric",
                "l2",
                "--beacon-metric",
                "dot",
            ],
            vec![
                "--beacon-index",
                "one",
                "--beacon-vector",
                "x",
                "--beacon-label",
                "A",
                "--beacon-label",
                "B",
            ],
        ] {
            assert!(beacon_options(&extra).is_err(), "{extra:?}");
        }
        for packed in [
            "e",
            "=1",
            "e=0",
            "e=-1",
            "e=1.5",
            "e=65537",
            "e=18446744073709551616",
        ] {
            assert!(
                beacon_options(&["--beacon-index", "one", "--beacon-packed-vector", packed])
                    .is_err(),
                "{packed}"
            );
        }
        let long = "x".repeat(129);
        assert!(beacon_options(&["--beacon-index", &long, "--beacon-text", "body"]).is_err());
        let coordinates = vec!["x"; 65_537].join(",");
        assert!(
            beacon_options(&["--beacon-index", "one", "--beacon-vector", &coordinates]).is_err()
        );
        let mut arguments = Vec::new();
        for i in 0..=fgdb::MAX_MAINTAINED_BEACON_INDEXES {
            arguments.extend([
                "--beacon-index".to_owned(),
                format!("index{i}"),
                "--beacon-text".to_owned(),
                "body".to_owned(),
            ]);
        }
        let borrowed = arguments.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(beacon_options(&borrowed[..borrowed.len() - 4]).is_ok());
        assert!(beacon_options(&borrowed).is_err());
    }

    #[test]
    fn beacon_budget_conversion_never_truncates_host_limits() {
        let parsed = beacon_options(&[
            "--beacon-index",
            "one",
            "--beacon-text",
            "body",
            "--property",
            "body=1",
        ])
        .unwrap_or_else(|error| panic!("{}", error.message));
        let db = &parsed.databases[0];
        let bound = db.beacon_indexes[0].bind(&db.symbols, [u64::MAX; 4]);
        if usize::BITS == 64 {
            let bound = bound.unwrap_or_else(|error| panic!("{}", error.message));
            assert_eq!(bound.options.policy.max_work_units, usize::MAX);
            assert_eq!(bound.options.policy.max_source_scratch, usize::MAX);
            assert_eq!(bound.options.policy.max_staging_rows, usize::MAX);
            assert_eq!(bound.options.policy.max_result_rows, usize::MAX);
        } else {
            assert!(bound.is_err());
        }
    }

    #[test]
    fn serve_symbol_flags_preserve_the_full_native_ordinal_domain() {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "social=test-db",
            "--key-file",
            "database.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let parse = |extra: &[&str]| {
            let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            ServeOptions::parse(&args)
        };
        for ordinal in [0, u64::from(u32::MAX) + 1, u64::MAX] {
            let raw = ordinal.to_string();
            let named = format!("native={ordinal}");
            let options = parse(&[
                "--write-relation",
                &raw,
                "--label",
                &named,
                "--relation",
                &named,
                "--property",
                &named,
            ])
            .unwrap_or_else(|error| panic!("{}", error.message));
            let db = &options.databases[0];
            assert_eq!(db.write_relation, ordinal);
            assert_eq!(
                db.symbols.labels().collect::<Vec<_>>(),
                [("native", ordinal)]
            );
            assert_eq!(
                db.symbols.relations().collect::<Vec<_>>(),
                [("native", ordinal)]
            );
            assert_eq!(
                db.symbols.properties().collect::<Vec<_>>(),
                [("native", ordinal)]
            );
        }
        for invalid in ["", "-1", "+1", "1.5", "18446744073709551616"] {
            assert!(parse(&["--write-relation", invalid]).is_err());
            let named = format!("native={invalid}");
            for flag in ["--label", "--relation", "--property"] {
                assert!(parse(&[flag, &named]).is_err(), "{flag}={invalid}");
            }
        }
        assert!(parse(&["--label", "native=1", "--label", "native=2"]).is_err());
        assert!(parse(&["--label", "native=1", "--label", "other=1"]).is_err());
    }

    #[test]
    fn native_scope_flags_survive_token_encoding_and_attenuation_without_aliases() {
        let strings = |args: &[&str]| args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let base = [
            "--key-file",
            "database.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let native = u64::MAX.to_string();
        let options = TokenOptions::parse(&strings(&[
            "--key-file",
            "database.key",
            "--issuer-key-file",
            "issuer.key",
            "--allow-label",
            &native,
            "--allow-relation",
            &native,
            "--allow-property",
            &native,
        ]))
        .unwrap_or_else(|error| panic!("{}", error.message));
        let grant = options.into_grant(1_000);
        let keys = fgdb::DatabaseKeys::new(
            [1; 32],
            fgdb_types::DatabaseSecurityNamespaceId([2; 32]),
            [3; 32],
        );
        let authority = fgdb_server::issuer(
            asupersync::security::key::AuthKey::from_seed(8_501),
            &keys,
            1,
        )
        .unwrap();
        let token = fgdb_warden::CapabilityToken::decode(
            &authority.issue_at(&grant, 100).unwrap().encode(),
        )
        .unwrap();
        let verified = authority
            .verify_at(&token, fgdb_server::TRUNK, 100)
            .unwrap();
        let predicates = verified.predicates();
        assert!(predicates.allows_label(LabelId(u64::MAX)));
        assert!(predicates.allows_relation(RelationId(u64::MAX)));
        assert!(predicates.allows_property(PropertyKeyId(u64::MAX)));
        let low = u64::from(u32::MAX);
        assert!(!predicates.allows_label(LabelId(low)));
        assert!(!predicates.allows_relation(RelationId(low)));
        assert!(!predicates.allows_property(PropertyKeyId(low)));

        let restrict = |args: &[&str]| {
            AttenuateOptions::parse(&strings(args))
                .and_then(|options| options.restrictions(100))
                .unwrap_or_else(|error| panic!("{}", error.message))
        };
        let attenuate = |token: &fgdb_warden::CapabilityToken, args: &[&str]| {
            let bytes = fgdb_server::attenuate_token(&token.encode(), &restrict(args)).unwrap();
            fgdb_warden::CapabilityToken::decode(&bytes).unwrap()
        };
        let narrowed = attenuate(
            &token,
            &[
                "--allow-label",
                &native,
                "--allow-relation",
                &native,
                "--allow-property",
                &native,
            ],
        );
        let verified = authority
            .verify_at(&narrowed, fgdb_server::TRUNK, 100)
            .unwrap();
        assert!(verified.predicates().allows_label(LabelId(u64::MAX)));
        assert!(verified.predicates().allows_relation(RelationId(u64::MAX)));
        assert!(
            verified
                .predicates()
                .allows_property(PropertyKeyId(u64::MAX))
        );
        let narrowed = attenuate(&narrowed, &["--deny-property", &native]);
        let verified = authority
            .verify_at(&narrowed, fgdb_server::TRUNK, 100)
            .unwrap();
        assert!(
            !verified
                .predicates()
                .allows_property(PropertyKeyId(u64::MAX))
        );
        assert!(verified.predicates().allows_label(LabelId(u64::MAX)));

        let low_text = low.to_string();
        let wrong = attenuate(
            &token,
            &[
                "--allow-label",
                &low_text,
                "--allow-relation",
                &low_text,
                "--allow-property",
                &low_text,
            ],
        );
        let verified = authority
            .verify_at(&wrong, fgdb_server::TRUNK, 100)
            .unwrap();
        assert!(!verified.predicates().allows_label(LabelId(u64::MAX)));
        assert!(!verified.predicates().allows_relation(RelationId(u64::MAX)));
        assert!(
            !verified
                .predicates()
                .allows_property(PropertyKeyId(u64::MAX))
        );
        assert!(!verified.predicates().allows_label(LabelId(low)));
        for invalid in ["", "-1", "+1", "18446744073709551616"] {
            for flag in ["--allow-label", "--allow-relation", "--allow-property"] {
                let args = base
                    .iter()
                    .copied()
                    .chain([flag, invalid])
                    .collect::<Vec<_>>();
                assert!(
                    TokenOptions::parse(&strings(&args)).is_err(),
                    "{flag}={invalid}"
                );
                assert!(AttenuateOptions::parse(&strings(&[flag, invalid])).is_err());
            }
            assert!(AttenuateOptions::parse(&strings(&["--deny-property", invalid])).is_err());
        }
    }

    #[test]
    fn admin_token_and_attenuation_use_the_distinct_operation_right() {
        let args: Vec<String> = [
            "--key-file",
            "db.key",
            "--issuer-key-file",
            "issuer.key",
            "--rights",
            "admin",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let options =
            TokenOptions::parse(&args).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(options.rights, Rights::Admin);
        assert!(!options.rights.can_read());
        assert!(!options.rights.can_write());
        let args: Vec<String> = ["--rights", "admin"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let options =
            AttenuateOptions::parse(&args).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(options.rights, Some(Rights::Admin));
    }

    #[test]
    fn buffered_read_memory_is_opt_in_positive_and_per_database() {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "first=first-db",
            "--key-file",
            "first.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let parse = |extra: &[&str]| {
            let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            ServeOptions::parse(&args)
        };
        let defaults = parse(&[]).unwrap_or_else(|error| panic!("{}", error.message));
        assert!(defaults.databases[0].buffered_reads_memory_bytes.is_none());
        assert!(!defaults.databases[0].buffered_reuse_live_admission);
        assert!(defaults.databases[0].buffered_native_run_limits.is_none());
        let flag = "--buffered-reads-memory-bytes";
        let second = [
            "--database",
            "second=second-db",
            "--key-file",
            "second.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let args: Vec<&str> = [flag, "67108864"].into_iter().chain(second).collect();
        let configured = parse(&args).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            configured.databases[0]
                .buffered_reads_memory_bytes
                .unwrap()
                .get(),
            64 * 1024 * 1024
        );
        assert!(
            configured.databases[1]
                .buffered_reads_memory_bytes
                .is_none()
        );
        let args: Vec<&str> = args.into_iter().chain([flag, "134217728"]).collect();
        let configured = parse(&args).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            configured.databases[0]
                .buffered_reads_memory_bytes
                .unwrap()
                .get(),
            64 * 1024 * 1024
        );
        assert_eq!(
            configured.databases[1]
                .buffered_reads_memory_bytes
                .unwrap()
                .get(),
            128 * 1024 * 1024
        );
        assert!(parse(&[flag]).is_err());
        for invalid in ["0", "-1", "+1", "1.5", "", "18446744073709551616"] {
            assert!(parse(&[flag, invalid]).is_err(), "{flag}={invalid}");
        }
        assert!(parse(&[flag, "67108864", flag, "134217728"]).is_err());
        let reuse = "--buffered-reuse-live-admission";
        assert!(parse(&[reuse]).is_err());
        assert!(parse(&[flag, "67108864", reuse, reuse]).is_err());
        let args: Vec<&str> = [flag, "67108864", reuse]
            .into_iter()
            .chain(second)
            .collect();
        let options = parse(&args).unwrap_or_else(|error| panic!("{}", error.message));
        assert!(options.databases[0].buffered_reuse_live_admission);
        assert!(!options.databases[1].buffered_reuse_live_admission);
        let runs = "--buffered-native-run-limits";
        assert!(parse(&[runs, "32,128,65536,4096"]).is_err());
        let args: Vec<&str> = [flag, "67108864", runs, "32,0,65536,4096"]
            .into_iter()
            .chain(second)
            .collect();
        let options = parse(&args).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            options.databases[0].buffered_native_run_limits,
            Some(SealedLimits {
                max_rows: 32,
                max_incidences: 0,
                max_image_bytes: 65_536,
                max_property_bytes: 4096,
            })
        );
        assert!(options.databases[1].buffered_native_run_limits.is_none());
        for invalid in ["", "1,2,3", "1,2,3,4,5", "1,-2,3,4", "1,2,3,"] {
            assert!(
                parse(&[flag, "67108864", runs, invalid]).is_err(),
                "{invalid}"
            );
        }
        assert!(parse(&[flag, "67108864", runs, "1,2,3,4", runs, "1,2,3,4"]).is_err());
        let misplaced: Vec<String> = [flag, "67108864"]
            .iter()
            .chain(&base)
            .map(|s| (*s).to_owned())
            .collect();
        assert!(ServeOptions::parse(&misplaced).is_err());
    }

    #[test]
    fn buffered_source_allowances_are_bounded_and_per_database() {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "first=first-db",
            "--key-file",
            "first.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let parse = |extra: &[&str]| {
            let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            ServeOptions::parse(&args)
        };
        let memory = "--buffered-reads-memory-bytes";
        let root = "--buffered-reads-root-bytes";
        let source = "--buffered-reads-max-source-bytes";
        let work = "--buffered-reads-max-work-units";
        assert!(parse(&[root, "1"]).is_err());
        assert!(parse(&[source, "1"]).is_err());
        assert!(parse(&[work, "1"]).is_err());
        assert!(parse(&[memory, "67108864", root, "0"]).is_err());
        for invalid in ["0", "-1", "1.5", "18446744073709551616"] {
            assert!(parse(&[memory, "67108864", source, invalid]).is_err());
        }
        assert!(parse(&[memory, "67108864", work, "0"]).is_err());
        assert!(parse(&[memory, "67108864", root, "2097153"]).is_err());
        assert!(parse(&[memory, "67108864", root, "1", root, "2"]).is_err());
        assert!(parse(&[memory, "67108864", source, "1", source, "2"]).is_err());
        assert!(parse(&[memory, "67108864", work, "1", work, "2"]).is_err());

        let options = parse(&[
            root,
            "2097152",
            source,
            "1",
            work,
            "21000000",
            memory,
            "67108864",
            "--database",
            "second=second-db",
            "--key-file",
            "second.key",
            "--issuer-key-file",
            "issuer.key",
            memory,
            "67108864",
        ])
        .unwrap_or_else(|error| panic!("{}", error.message));
        let explicit = options.databases[0].buffered_read_config().unwrap();
        let default = options.databases[1].buffered_read_config().unwrap();
        assert_eq!(explicit.memory_bytes, default.memory_bytes);
        assert_eq!(explicit.limits.max_root_bytes, 2_097_152);
        assert_eq!(explicit.limits.max_blocks, 2_097_152);
        assert_eq!(explicit.limits.max_vertex_patches, 2_097_152);
        assert_eq!(default.limits.max_root_bytes, 1_048_576);
        assert_eq!(explicit.limits.max_work, 21_000_000);
        assert_eq!(default.limits.max_work, 10_000_000);
        assert_eq!(explicit.limits.max_source_bytes, 1);
        assert_eq!(default.limits.max_source_bytes, 1_073_741_824);
        assert_eq!(explicit.max_concurrent, default.max_concurrent);
    }

    #[test]
    fn subscription_coalescing_is_explicit_positive_and_per_database() {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "first=first-db",
            "--key-file",
            "first.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let parse = |extra: &[&str]| {
            let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            ServeOptions::parse(&args)
        };
        let defaults = parse(&[]).unwrap_or_else(|error| panic!("{}", error.message));
        assert!(defaults.databases[0].subscription_coalesce_ticks.is_none());
        let flag = "--subscription-coalesce-ticks";
        let configured = parse(&[
            flag,
            "32",
            "--database",
            "second=second-db",
            "--key-file",
            "second.key",
            "--issuer-key-file",
            "issuer.key",
            flag,
            "1",
        ])
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            configured.databases[0]
                .subscription_coalesce_ticks
                .unwrap()
                .get(),
            32
        );
        assert_eq!(
            configured.databases[1]
                .subscription_coalesce_ticks
                .unwrap()
                .get(),
            1
        );
        assert!(parse(&[flag]).is_err());
        for invalid in ["0", "-1", "1.5", "18446744073709551616"] {
            assert!(parse(&[flag, invalid]).is_err(), "{flag}={invalid}");
        }
        assert!(parse(&[flag, "2", flag, "3"]).is_err());
        let misplaced: Vec<String> = [flag, "2"]
            .iter()
            .chain(&base)
            .map(|s| (*s).to_owned())
            .collect();
        assert!(ServeOptions::parse(&misplaced).is_err());
    }

    #[test]
    fn maintenance_intervals_are_opt_in_and_per_database() {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "first=first-db",
            "--key-file",
            "first.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let parse = |extra: &[&str]| {
            let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            ServeOptions::parse(&args)
        };
        let defaults = parse(&[]).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(defaults.databases[0].maintenance_intervals, [None; 3]);
        let configured = parse(&[
            "--compact-interval-seconds",
            "300",
            "--scrub-interval-seconds",
            "3600",
            "--beacon-checkpoint-interval-seconds",
            "60",
            "--database",
            "second=second-db",
            "--key-file",
            "second.key",
            "--issuer-key-file",
            "issuer.key",
            "--scrub-interval-seconds",
            "90",
        ])
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            configured.databases[0].maintenance_intervals,
            [
                Some(Duration::from_secs(300)),
                Some(Duration::from_secs(3600)),
                Some(Duration::from_secs(60)),
            ]
        );
        assert_eq!(
            configured.databases[1].maintenance_intervals,
            [None, Some(Duration::from_secs(90)), None]
        );
        for flag in [
            "--compact-interval-seconds",
            "--scrub-interval-seconds",
            "--beacon-checkpoint-interval-seconds",
        ] {
            assert!(parse(&[flag]).is_err());
            for invalid in [
                "0",
                "-1",
                "1.5",
                "18446744073709551615",
                "18446744073709551616",
            ] {
                assert!(parse(&[flag, invalid]).is_err(), "{flag}={invalid}");
            }
            assert!(parse(&[flag, "1", flag, "2"]).is_err());
            let misplaced: Vec<String> = [flag, "1"]
                .iter()
                .chain(&base)
                .map(|s| (*s).to_owned())
                .collect();
            assert!(ServeOptions::parse(&misplaced).is_err());
        }
    }

    #[test]
    fn query_scratch_is_an_explicit_per_database_option() {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "first=first-db",
            "--key-file",
            "first.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let parse = |extra: &[&str]| {
            let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            ServeOptions::parse(&args)
        };
        let disabled = parse(&[]).unwrap_or_else(|error| panic!("{}", error.message));
        assert!(disabled.databases[0].query_scratch_dir.is_none());
        let options = parse(&[
            "--query-scratch-dir",
            "private scratch/first",
            "--database",
            "second=second-db",
            "--key-file",
            "second.key",
            "--issuer-key-file",
            "issuer.key",
        ])
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            options.databases[0].query_scratch_dir.as_deref(),
            Some(std::path::Path::new("private scratch/first"))
        );
        assert!(options.databases[1].query_scratch_dir.is_none());
        assert!(parse(&["--query-scratch-dir"]).is_err());
        assert!(
            parse(&[
                "--query-scratch-dir",
                "first",
                "--query-scratch-dir",
                "second",
            ])
            .is_err()
        );
        let misplaced: Vec<String> = ["--query-scratch-dir", "scratch"]
            .iter()
            .chain(&base)
            .map(|s| (*s).to_owned())
            .collect();
        assert!(ServeOptions::parse(&misplaced).is_err());
    }

    #[test]
    fn query_limits_are_per_database_and_default_to_the_cli_limits() {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "first=first-db",
            "--key-file",
            "first.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let parse = |extra: &[&str]| {
            let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            ServeOptions::parse(&args)
        };
        let defaults = parse(&[]).unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(defaults.databases[0].query_limits, [None; 4]);
        let options = parse(&[
            "--max-snapshot-records",
            "50000000",
            "--max-work-units",
            "0",
            "--database",
            "second=second-db",
            "--key-file",
            "second.key",
            "--issuer-key-file",
            "issuer.key",
            "--max-result-rows",
            "7",
            "--max-scratch-entries",
            "18446744073709551615",
        ])
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            options.databases[0].query_limits,
            [Some(50_000_000), None, Some(0), None]
        );
        assert_eq!(
            options.databases[1].query_limits,
            [None, Some(7), None, Some(u64::MAX)]
        );
        // Duplicates, missing or non-decimal values, overflow and a limit
        // before any --database are usage errors.
        assert!(parse(&["--max-result-rows", "1", "--max-result-rows", "2"]).is_err());
        assert!(parse(&["--max-work-units"]).is_err());
        assert!(parse(&["--max-work-units", "-1"]).is_err());
        assert!(parse(&["--max-work-units", "18446744073709551616"]).is_err());
        let misplaced: Vec<String> = ["--max-snapshot-records", "1"]
            .iter()
            .chain(&base)
            .map(|s| (*s).to_owned())
            .collect();
        assert!(ServeOptions::parse(&misplaced).is_err());
    }

    #[test]
    fn websocket_listener_requires_explicit_valid_origin_and_host_policies() {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "social=test-db",
            "--key-file",
            "database.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let parse = |extra: &[&str]| {
            let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            ServeOptions::parse(&args)
        };
        let options = parse(&[
            "--websocket-listen",
            "127.0.0.1:8789",
            "--websocket-allow-host",
            "graph.example",
            "--websocket-allow-origin",
            "https://console.example",
        ])
        .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(options.websocket_listen.as_deref(), Some("127.0.0.1:8789"));
        assert_eq!(options.websocket_hosts, ["graph.example"]);
        assert_eq!(options.websocket_origins, ["https://console.example"]);
        let defaults = parse(&["--websocket-listen", "127.0.0.1:8789"])
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert!(defaults.websocket_origins.is_empty());
        assert!(parse(&["--websocket-allow-origin", "https://console.example"]).is_err());
        assert!(
            parse(&[
                "--websocket-listen",
                "127.0.0.1:8789",
                "--websocket-allow-origin",
                "*",
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "--websocket-listen",
                "127.0.0.1:8789",
                "--websocket-allow-host",
                "host:80",
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "--websocket-listen",
                "127.0.0.1:8789",
                "--websocket-listen",
                "127.0.0.1:8790",
            ])
            .is_err()
        );
    }

    #[test]
    fn serve_tls_identity_must_be_complete_and_unique() {
        let base = [
            "--listen",
            "127.0.0.1:0",
            "--database",
            "social=test-db",
            "--key-file",
            "database.key",
            "--issuer-key-file",
            "issuer.key",
        ];
        let parse = |extra: &[&str]| {
            let args: Vec<String> = base.iter().chain(extra).map(|s| (*s).to_owned()).collect();
            ServeOptions::parse(&args)
        };
        assert!(
            parse(&[])
                .unwrap_or_else(|error| panic!("{}", error.message))
                .tls_cert_file
                .is_none()
        );
        assert!(parse(&["--tls-cert-file", "cert.pem"]).is_err());
        assert!(parse(&["--tls-key-file", "key.pem"]).is_err());
        let options = parse(&["--tls-cert-file", "cert.pem", "--tls-key-file", "key.pem"])
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            options.tls_cert_file.as_deref(),
            Some(std::path::Path::new("cert.pem"))
        );
        assert_eq!(
            options.tls_key_file.as_deref(),
            Some(std::path::Path::new("key.pem"))
        );
        assert!(
            parse(&[
                "--tls-cert-file",
                "first.pem",
                "--tls-cert-file",
                "second.pem",
                "--tls-key-file",
                "key.pem"
            ])
            .is_err()
        );
    }
}
