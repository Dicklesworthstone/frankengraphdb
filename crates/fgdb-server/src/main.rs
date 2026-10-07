//! `fgdbd`: serve FrankenGraphDB databases over FGP, mint capability tokens,
//! and generate owner-only key files.
#![forbid(unsafe_code)]

use asupersync::Budget;
use asupersync::Cx;
use asupersync::io::AsyncWriteExt as _;
use asupersync::net::TcpListener;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::GraphSymbolKind;
use fgdb_server::{DatabaseConfig, Server, ServerLimits, Symbols};
use fgdb_warden::{Grant, QueryLimits, Restriction, Rights, Scope};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

const HELP: &str = "\
fgdbd: the FrankenGraphDB server (FGP, HTTPS, and Bolt with optional TLS)

USAGE:
  fgdbd serve --listen <addr:port> [--http-listen <addr:port> [--http-allow-host <host>]...]
      [--bolt-listen <addr:port>] [--tls-cert-file <path> --tls-key-file <path>] DATABASE...
      DATABASE := --database <name>=<path> --key-file <path> --issuer-key-file <path>
                  [--policy-epoch <n>] [--write-relation <u32>]
                  [--label <name>=<u32>]... [--relation <name>=<u32>]... [--property <name>=<u32>]...
      [--max-connections <n>]
  fgdbd token --key-file <path> --issuer-key-file <path> [--policy-epoch <n>]
      [--rights read|write|read-write] [--expires-in <seconds>]
      [--allow-label <u32>]... [--allow-relation <u32>]... [--allow-property <u32>]...
      [--max-nodes <n>] [--max-work <n>] [--max-rows <n>]
  fgdbd attenuate [--rights read|write|read-write] [--expires-in <seconds>]
      [--not-before-in <seconds>] [--allow-label <u32>]... [--allow-relation <u32>]...
      [--allow-property <u32>]... [--deny-property <u32>]...
      [--max-nodes <n>] [--max-work <n>] [--max-rows <n>]   < token.hex
  fgdbd keygen --database <path> | --issuer <path>
  fgdbd help

serve opens each database and accepts FGP connections. Options after a
--database apply to that database. A client authenticates with a Warden
capability token minted by `fgdbd token` for that database's key file and
issuer key; it can select exactly the databases whose issuer accepts it.
Statements run through capability-authorized sessions, so a token's label,
relation and property scope applies before expansion. Names resolve through
the bindings given here (there is no durable catalog yet). serve prints one
NDJSON line {\"v\":1,\"event\":\"listening\",\"protocol\":\"fgp\",\"addr\":...} per
listener once bound, drains on SIGINT/SIGTERM (in-flight statements finish,
idle connections say GOODBYE), and exits 0.

--tls-cert-file and --tls-key-file must be supplied together. They enable
TLS 1.3 on EVERY configured listener, with no plaintext fallback. Supply a
PEM certificate chain and an owner-only PEM private key (chmod 600 on Unix).
The identity is validated before databases are opened or listeners are bound.
Handshakes are bounded to 10 seconds and are cancelled during server drain;
TLS early data is disabled. FGP clients require ALPN fgp/1; HTTPS uses
http/1.1. Bolt clients use bolt+s:// or neo4j+s:// with normal certificate
verification. Warden capability authentication still applies inside TLS.

--http-listen adds the HTTP/1.1 JSON adapter: POST /v1/databases/<name>/query
or /write with `Authorization: Bearer <hex token>` and a JSON body
{\"statement\": \"<gql>\", \"parameters\": {...}}; GET /v1/health. Requests must
name an allowed Host (default localhost, 127.0.0.1, [::1] and the listen IP;
--http-allow-host replaces that list).

--bolt-listen adds the Bolt-compat adapter (BoltCompatProfileV1) for official
Neo4j drivers: bolt:// or neo4j:// URIs, authenticated with the hex token as a
bearer credential (or as the basic-auth password). It is read-only: every RUN
executes on a read session, and writes refuse with
Neo.ClientError.Statement.AccessMode. The database is the driver's database
argument, or the only one served.

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

keygen writes a fresh owner-only (0600) key file: three lines for a database
(the fgdb CLI's --key-file format) or one line for an issuer. It refuses to
overwrite an existing file.

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
    issuer_key_file: Option<PathBuf>,
    policy_epoch: u64,
    write_relation: u32,
    symbols: Symbols,
}

struct ServeOptions {
    listen: String,
    http_listen: Option<String>,
    http_hosts: Vec<String>,
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

fn binding(raw: &str, flag: &str) -> Result<(String, u32), Failure> {
    let (name, id) = raw
        .split_once('=')
        .ok_or_else(|| Failure::usage(format!("{flag} needs <name>=<u32>")))?;
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
                        issuer_key_file: None,
                        policy_epoch: 1,
                        write_relation: 1,
                        symbols: Symbols::new(),
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
                        "--issuer-key-file" if db.issuer_key_file.is_none() => {
                            db.issuer_key_file = Some(PathBuf::from(value(args, &mut at, flag)?));
                        }
                        "--policy-epoch" => {
                            db.policy_epoch = number(value(args, &mut at, flag)?, flag)?
                        }
                        "--write-relation" => {
                            db.write_relation = number(value(args, &mut at, flag)?, flag)?
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
        for db in &databases {
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
            bolt_listen,
            tls_cert_file,
            tls_key_file,
            max_connections,
            databases,
        })
    }
}

async fn serve(cx: &Cx, options: ServeOptions) -> Result<(), Failure> {
    let mut limits = ServerLimits::default();
    if let Some(max) = options.max_connections {
        limits.max_connections = max;
    }
    let mut server = Server::new(cx, limits).map_err(Failure::usage)?;
    let tls_enabled = options.tls_cert_file.is_some();
    if let (Some(cert), Some(key)) = (&options.tls_cert_file, &options.tls_key_file) {
        let config = fgdb_server::TlsConfig::from_pem_files(cx, cert, key)
            .await
            .map_err(Failure::open)?;
        server.enable_tls(config);
    }
    for db in options.databases {
        let keys = fgdb_server::read_database_keys(cx, db.key_file.as_deref().expect("checked"))
            .await
            .map_err(Failure::open)?;
        let issuer_key =
            fgdb_server::read_issuer_key(cx, db.issuer_key_file.as_deref().expect("checked"))
                .await
                .map_err(Failure::open)?;
        let mut config = DatabaseConfig::new(db.name, keys, issuer_key);
        config.policy_epoch = db.policy_epoch;
        config.write_relation = RelationId(u64::from(db.write_relation));
        config.symbols = db.symbols;
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
    let http = match http {
        None => None,
        Some((listener, hosts)) => {
            let server = Arc::clone(&server);
            Some(
                cx.spawn(move |child| async move {
                    let _ = server.serve_http(&child, listener, hosts).await;
                })
                .map_err(|_| Failure::io("cannot start the HTTP listener"))?,
            )
        }
    };
    let bolt = match bolt {
        None => None,
        Some(listener) => {
            let server = Arc::clone(&server);
            Some(
                cx.spawn(move |child| async move {
                    let _ = server.serve_bolt(&child, listener).await;
                })
                .map_err(|_| Failure::io("cannot start the Bolt listener"))?,
            )
        }
    };
    Arc::clone(&server)
        .serve(cx, listener)
        .await
        .map_err(Failure::io)?;
    if let Some(mut http) = http {
        let _ = http.join(cx).await;
    }
    if let Some(mut bolt) = bolt {
        let _ = bolt.join(cx).await;
    }
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, r#"{{"v":1,"event":"stopped"}}"#).map_err(Failure::io)
}

struct TokenOptions {
    key_file: PathBuf,
    issuer_key_file: PathBuf,
    policy_epoch: u64,
    rights: Rights,
    expires_in_seconds: u64,
    labels: Option<Vec<u32>>,
    relations: Option<Vec<u32>>,
    properties: Option<Vec<u32>>,
    limits: QueryLimits,
}

impl TokenOptions {
    fn parse(args: &[String]) -> Result<Self, Failure> {
        let mut key_file = None;
        let mut issuer_key_file = None;
        let mut options = Self {
            key_file: PathBuf::new(),
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
                        _ => return Err(Failure::usage("--rights is read, write or read-write")),
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
        options.issuer_key_file =
            issuer_key_file.ok_or_else(|| Failure::usage("token needs --issuer-key-file"))?;
        if options.expires_in_seconds == 0 {
            return Err(Failure::usage("--expires-in must be positive"));
        }
        Ok(options)
    }
}

async fn token(cx: &Cx, options: TokenOptions) -> Result<(), Failure> {
    let keys = fgdb_server::read_database_keys(cx, &options.key_file)
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
    fn scope<T: Ord>(ids: Option<Vec<u32>>, wrap: impl Fn(u64) -> T) -> Scope<T> {
        ids.map_or(Scope::All, |ids| {
            Scope::only(ids.into_iter().map(|id| wrap(u64::from(id))))
        })
    }
    let grant = Grant {
        branch: fgdb_server::TRUNK.to_owned(),
        labels: scope(options.labels, LabelId),
        relations: scope(options.relations, RelationId),
        properties: scope(options.properties, PropertyKeyId),
        rights: options.rights,
        limits: options.limits,
        expires_at_ms,
    };
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
    labels: Option<Vec<u32>>,
    relations: Option<Vec<u32>>,
    properties: Option<Vec<u32>>,
    deny_properties: Option<Vec<u32>>,
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
                        _ => return Err(Failure::usage("--rights is read, write or read-write")),
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
        fn ids<T: Ord>(ids: &[u32], wrap: impl Fn(u64) -> T) -> BTreeSet<T> {
            ids.iter().map(|id| wrap(u64::from(*id))).collect()
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
    let (lines, path) = match args {
        [flag, path] if flag == "--database" => (3, path),
        [flag, path] if flag == "--issuer" => (1, path),
        _ => {
            return Err(Failure::usage(
                "keygen needs --database <path> or --issuer <path>",
            ));
        }
    };
    let mut text = String::new();
    for _ in 0..lines {
        let mut key = [0u8; 32];
        cx.random_bytes(&mut key);
        for byte in key {
            text.push_str(&format!("{byte:02x}"));
        }
        text.push('\n');
    }
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
