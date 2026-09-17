//! Embedded CLI. Every graph operation uses the public native engine;
//! symbol IDs remain explicit until the library supplies a durable catalog.
#![forbid(unsafe_code)]

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphPath, GraphValue};
use fgdb_gql::{
    GqlParameterType, GqlParameterValue, GqlParameters, GqlQueryPolicy, GqlScalarParameter,
    GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy, PreparedGraphWriteScript,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

const ROBOT_SCHEMA: &str = concat!(
    r##"{"v":1,"event":"schema","events":{"invocation":["v","event"],"columns":["v","event","columns"],"row":["v","event","cells"],"result":["v","event","kind","seq","count","statements"],"error":["v","event","class","diagnostics"],"schema":["v","event","events","exit_codes","key_file","bindings","cell_types"]},"exit_codes":{"success":0,"usage":2,"query":3,"open":4,"io":5},"key_file":"Three nonempty lines of 64 hexadecimal characters: object-id key, security namespace, encryption key; # starts a comment. Keys are never printed.","bindings":"Repeat --label name=u32, --relation name=u32, --property name=u32 on each invocation; --write-relation u32 defaults to 1. No implicit catalog.","cell_types":["null","bool","int","text","list","count","wideint","average","decimal","float","timestamp","bytes","vertex","edge","path","vertices","edges"]}"##,
    "\n"
);
const HELP: &str = "fgdb - embedded graph database
Usage: fgdb [--robot] <command>
  create --db <dir> --key-file <file>
  write --db <dir> --key-file <file> [bindings] [--param name=value]... <gql>
  query --db <dir> --key-file <file> [bindings] [--param name=value]... <gql>
  robot schema
  help
Parameters: int:42, uint:42, text:Ada, bool:true, bool:false, null.
Bindings: repeat --label name=u32, --relation name=u32, --property name=u32.
Supply the same bindings on reopen; no implicit catalog or hashed names.
--write-relation u32 selects the native mutation coordinate (default 1).
Key file: three nonempty lines of 64 hex characters: object-id key,
security namespace, encryption key. # starts a comment. Keys never printed.
Robot stdout: versioned NDJSON; errors include message strings in diagnostics.
Exit codes: 0 success, 2 usage/schema, 3 query refusal, 4 open/key, 5 I/O/corruption.
";

struct Failure {
    code: u8,
    class: &'static str,
    message: String,
}
impl Failure {
    fn new(code: u8, class: &'static str, message: impl ToString) -> Self {
        Self {
            code,
            class,
            message: message.to_string(),
        }
    }
    fn usage(message: impl ToString) -> Self {
        Self::new(2, "usage", message)
    }
    fn query(message: impl ToString) -> Self {
        Self::new(3, "query", message)
    }
    fn open(message: impl ToString) -> Self {
        Self::new(4, "open", message)
    }
    fn io(message: impl ToString) -> Self {
        Self::new(5, "io", message)
    }
}
fn emit(out: &mut impl Write, line: &str) -> Result<(), Failure> {
    writeln!(out, "{line}").map_err(Failure::io)
}
fn main() -> ExitCode {
    let raw: Vec<_> = std::env::args_os().skip(1).collect();
    let robot = raw.first().is_some_and(|arg| arg == "--robot");
    let args = raw
        .into_iter()
        .skip(usize::from(robot))
        .map(|arg| {
            arg.into_string()
                .map_err(|_| Failure::usage("arguments must be UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>();
    let mut out = io::stdout().lock();
    let result = (|| {
        if robot {
            emit(&mut out, r#"{"v":1,"event":"invocation"}"#)?;
        }
        dispatch(&args?, robot, &mut out)?;
        out.flush().map_err(Failure::io)
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fgdb: {}", error.message);
            if robot {
                // Robot error events carry the message too; diagnostics never
                // include key material because messages never format keys.
                let _ = emit(
                    &mut out,
                    &format!(
                        r#"{{"v":1,"event":"error","class":"{}","diagnostics":[{}]}}"#,
                        error.class,
                        quoted(&error.message)
                    ),
                );
                let _ = out.flush();
            }
            ExitCode::from(error.code)
        }
    }
}

struct Options {
    db: PathBuf,
    key: PathBuf,
    text: String,
    params: GqlParameters,
    labels: BTreeMap<String, u32>,
    relations: BTreeMap<String, u32>,
    properties: BTreeMap<String, u32>,
    coordinate: RelationId,
}
impl Options {
    fn resolve(&self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match kind {
            GraphSymbolKind::Label => self
                .labels
                .get(name)
                .map(|id| GraphSymbol::Label(LabelId(u64::from(*id)))),
            GraphSymbolKind::Relation => self
                .relations
                .get(name)
                .map(|id| GraphSymbol::Relation(RelationId(u64::from(*id)))),
            GraphSymbolKind::Property => self
                .properties
                .get(name)
                .map(|id| GraphSymbol::Property(PropertyKeyId(u64::from(*id)))),
        }
    }
}
fn parse(args: &[String], create: bool) -> Result<Options, Failure> {
    let mut db = None;
    let mut key = None;
    let mut text = None;
    let mut params = GqlParameters::new();
    let mut labels = BTreeMap::new();
    let mut relations = BTreeMap::new();
    let mut properties = BTreeMap::new();
    let mut coordinate = RelationId(1);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg.starts_with("--") {
            let value = iter
                .next()
                .ok_or_else(|| Failure::usage("flag requires a value"))?;
            match arg.as_str() {
                "--db" if db.is_none() => db = Some(PathBuf::from(value)),
                "--key-file" if key.is_none() => key = Some(PathBuf::from(value)),
                "--param" if !create => {
                    let (name, raw) = value
                        .split_once('=')
                        .ok_or_else(|| Failure::usage("expected --param name=value"))?;
                    let parsed = parameter(raw)?;
                    params.insert(name, parsed).map_err(Failure::query)?;
                }
                "--label" | "--relation" | "--property" => {
                    let (name, raw) = value
                        .split_once('=')
                        .ok_or_else(|| Failure::usage("expected binding name=u32"))?;
                    let id: u32 = raw
                        .parse()
                        .map_err(|_| Failure::usage("binding ID must be u32"))?;
                    let map = match arg.as_str() {
                        "--label" => &mut labels,
                        "--relation" => &mut relations,
                        _ => &mut properties,
                    };
                    if name.is_empty()
                        || map.contains_key(name)
                        || map.values().any(|old| *old == id)
                    {
                        return Err(Failure::usage(
                            "bindings must have unique names and IDs per kind",
                        ));
                    }
                    map.insert(name.to_owned(), id);
                }
                "--write-relation" => {
                    coordinate = RelationId(
                        value
                            .parse()
                            .map_err(|_| Failure::usage("write relation must be u32"))?,
                    )
                }
                _ => return Err(Failure::usage("unknown, duplicate, or inapplicable flag")),
            }
        } else if create || text.replace(arg.clone()).is_some() {
            return Err(Failure::usage(
                "expected exactly one GQL argument for query/write, none for create",
            ));
        }
    }
    Ok(Options {
        db: db.ok_or_else(|| Failure::usage("--db required"))?,
        key: key.ok_or_else(|| Failure::usage("--key-file required"))?,
        text: if create {
            String::new()
        } else {
            text.ok_or_else(|| Failure::usage("GQL argument required"))?
        },
        params,
        labels,
        relations,
        properties,
        coordinate,
    })
}
fn parameter(raw: &str) -> Result<GqlParameterValue, Failure> {
    if let Some(value) = raw.strip_prefix("int:") {
        return value
            .parse()
            .map(GqlParameterValue::Int64)
            .map_err(|_| Failure::usage("invalid int parameter"));
    }
    if let Some(value) = raw.strip_prefix("uint:") {
        return value
            .parse()
            .map(GqlParameterValue::UInt64)
            .map_err(|_| Failure::usage("invalid uint parameter"));
    }
    let scalar = if let Some(value) = raw.strip_prefix("text:") {
        CanonicalScalar::ucs_basic_text(value).map_err(Failure::query)?
    } else {
        match raw {
            "bool:true" => CanonicalScalar::Bool(true),
            "bool:false" => CanonicalScalar::Bool(false),
            "null" => CanonicalScalar::Null,
            _ => return Err(Failure::usage("invalid parameter type or value")),
        }
    };
    GqlScalarParameter::new(scalar)
        .map(GqlParameterValue::Scalar)
        .map_err(Failure::query)
}
async fn read_keys(
    cx: &fgdb_types::QueryCx,
    path: &std::path::Path,
) -> Result<DatabaseKeys, Failure> {
    cx.checkpoint().map_err(Failure::io)?;
    let text = asupersync::fs::read_to_string(path)
        .await
        .map_err(|_| Failure::open("cannot read key file"))?;
    let lines: Vec<_> = text
        .lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() != 3 {
        return Err(Failure::open("key file requires three key lines"));
    }
    let mut keys = [[0u8; 32]; 3];
    for (key, line) in keys.iter_mut().zip(lines) {
        if line.len() != 64 || !line.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Failure::open("key lines require 64 hexadecimal characters"));
        }
        for (byte, pair) in key.iter_mut().zip(line.as_bytes().chunks_exact(2)) {
            let digit = |b: u8| {
                if b.is_ascii_digit() {
                    b - b'0'
                } else {
                    b.to_ascii_lowercase() - b'a' + 10
                }
            };
            *byte = digit(pair[0]) * 16 + digit(pair[1]);
        }
    }
    Ok(DatabaseKeys::new(
        keys[0],
        DatabaseSecurityNamespaceId(keys[1]),
        keys[2],
    ))
}
fn open_failure(error: fgdb::OpenError) -> Failure {
    use fgdb::OpenError as E;
    match error {
        // Capsule recovery authenticates symbols with the supplied DEK before
        // decoding. A wrong key leaves too few authenticated symbols. The
        // public error cannot distinguish that from total symbol loss, so
        // this ambiguous open-time failure belongs to the open/key class.
        E::Rebuild(fgdb::RebuildError::Commit(fgdb_chronicle::CommitError::Capsule(
            fgdb_chronicle::capsule::CapsuleError::Recovery(
                fgdb_chronicle::SymbolizeError::InsufficientSymbols
                | fgdb_chronicle::SymbolizeError::AuthenticationFailed,
            ),
        ))) => Failure::open(error),
        // Key failures: the path is fine, the identity in hand is not. A
        // foreign slot (namespace/opener disagreement) and a slot the
        // K_oid-authenticated stream disowns (wrong K_oid, SlotDisagreesWith
        // Stream) are both key failures, not I/O failures.
        E::NotADirectory { .. }
        | E::NotADatabase { .. }
        | E::AlreadyADatabase { .. }
        | E::ForeignSlot { .. }
        | E::SlotDisagreesWithStream { .. }
        | E::SlotUnrecoverable { .. }
        | E::NotEmpty { .. } => Failure::open(error),
        _ => Failure::io(error),
    }
}
fn execution_failure(error: impl std::error::Error + 'static) -> Failure {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(current) = source {
        if current.is::<std::io::Error>()
            || current.is::<fgdb::RebuildError>()
            || current.is::<fgdb_chronicle::CommitError>()
            || current.downcast_ref::<fgdb::WriteError>().is_some_and(|e| {
                matches!(
                    e,
                    fgdb::WriteError::Commit(_)
                        | fgdb::WriteError::CommitOutcomeUnknown { .. }
                        | fgdb::WriteError::RecoveryRequired(_)
                )
            })
        {
            return Failure::io(error);
        }
        source = current.source();
    }
    Failure::query(error)
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn dispatch(args: &[String], robot: bool, out: &mut impl Write) -> Result<(), Failure> {
    match args.first().map(String::as_str) {
        Some("help") if args.len() == 1 => {
            if robot {
                eprint!("{HELP}");
                emit(out, r#"{"v":1,"event":"result","kind":"help"}"#)
            } else {
                write!(out, "{HELP}").map_err(Failure::io)
            }
        }
        Some("robot") if args.len() == 2 && args[1] == "schema" => {
            write!(out, "{ROBOT_SCHEMA}").map_err(Failure::io)?;
            if robot {
                emit(out, r#"{"v":1,"event":"result","kind":"schema"}"#)?;
            }
            Ok(())
        }
        Some(command @ ("create" | "query" | "write")) => {
            let options = parse(&args[1..], command == "create")?;
            let runtime = RuntimeBuilder::new().build().map_err(Failure::io)?;
            let root = runtime.request_cx_with_budget(Budget::INFINITE);
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            runtime.block_on(async {
                let keys = read_keys(&contexts.query(), &options.key).await?;
                let mut db = if command == "create" { Database::create(&contexts.commit(), &options.db, keys).await } else { Database::open(&contexts.commit(), &options.db, keys).await }.map_err(open_failure)?;
                if command == "create" {
                    let seq = db.frontier().map_err(Failure::io)?.0;
                    return if robot { emit(out, &format!(r#"{{"v":1,"event":"result","kind":"created","seq":{seq}}}"#)) } else { writeln!(out, "created (seq {seq})").map_err(Failure::io) };
                }
                if command == "write" {
                    let declarations: Vec<_> = options.params.parameter_types().filter(|(_, kind)| matches!(kind, GqlParameterType::Scalar(_))).collect();
                    let script = PreparedGraphWriteScript::prepare_with_parameter_types(&options.text, options.coordinate, &declarations, |kind, name| options.resolve(kind, name)).map_err(Failure::query)?;
                    let program = script.bind_parameters(&options.params).map_err(Failure::query)?;
                    let (receipt, completion) = db.execute_graph_write_program_returning_autocommit_engine_governed(&contexts.txn(), &contexts.query(), &contexts.commit(), &program, GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000)).await.map_err(execution_failure)?;
                    let seq = match completion { EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq.0, EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => snapshot_seq.0 };
                    return if robot { emit(out, &format!(r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"statements":{}}}"#, receipt.stats().completed_statements)) } else { writeln!(out, "completed at seq {seq}").map_err(Failure::io) };
                }
                let result = db.query(&contexts.query(), &options.text, &options.params, |kind, name| options.resolve(kind, name), policy()).map_err(execution_failure)?;
                let seq = db.frontier().map_err(Failure::io)?.0;
                render(result, seq, robot, out)
            })
        }
        _ => Err(Failure::usage(
            "unknown or missing subcommand; use fgdb help",
        )),
    }
}
fn quoted(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
fn value_cell(value: &GraphValue) -> Result<String, Failure> {
    Ok(match value {
        GraphValue::Scalar(CanonicalScalar::Null) => r#"{"type":"null"}"#.to_owned(),
        GraphValue::Scalar(CanonicalScalar::Bool(v)) => format!(r#"{{"type":"bool","value":{v}}}"#),
        GraphValue::Scalar(CanonicalScalar::Int(v)) => format!(r#"{{"type":"int","value":"{v}"}}"#),
        GraphValue::Scalar(CanonicalScalar::Text(v)) => {
            format!(r#"{{"type":"text","value":{}}}"#, quoted(v.as_str()))
        }
        GraphValue::Scalar(CanonicalScalar::Decimal(v)) => {
            format!(r#"{{"type":"decimal","value":"{v}"}}"#)
        }
        GraphValue::Scalar(CanonicalScalar::Float(v)) => {
            format!(
                r#"{{"type":"float","value":{}}}"#,
                quoted(&float_text(v.get()))
            )
        }
        GraphValue::Scalar(CanonicalScalar::Timestamp(v)) => {
            format!(r#"{{"type":"timestamp","value":{}}}"#, timestamp_cell(v))
        }
        GraphValue::Scalar(CanonicalScalar::Bytes(v)) => {
            format!(
                r#"{{"type":"bytes","value":{}}}"#,
                quoted(&hex(v.as_slice()))
            )
        }
        GraphValue::Vertex(v) => format!(r#"{{"type":"vertex","value":"{}"}}"#, v.0),
        GraphValue::Edge(v) => format!(r#"{{"type":"edge","value":"{}"}}"#, v.0),
        GraphValue::Path(v) => format!(r#"{{"type":"path","value":{}}}"#, path_cell(v)),
        GraphValue::Vertices(v) => format!(
            r#"{{"type":"vertices","value":[{}]}}"#,
            v.iter()
                .map(|id| quoted(&id.0.to_string()))
                .collect::<Vec<_>>()
                .join(",")
        ),
        GraphValue::Edges(v) => format!(
            r#"{{"type":"edges","value":[{}]}}"#,
            v.iter()
                .map(|id| quoted(&id.0.to_string()))
                .collect::<Vec<_>>()
                .join(",")
        ),
        GraphValue::List(values) => format!(
            r#"{{"type":"list","value":[{}]}}"#,
            values
                .iter()
                .map(value_cell)
                .collect::<Result<Vec<_>, _>>()?
                .join(",")
        ),
    })
}
fn path_cell(value: &GraphPath) -> String {
    let mut nodes = vec![quoted(&value.start().0.to_string())];
    let mut edges = Vec::new();
    for (edge, vertex) in value.steps() {
        edges.push(quoted(&edge.0.to_string()));
        nodes.push(quoted(&vertex.0.to_string()));
    }
    format!(
        r#"{{"nodes":[{}],"edges":[{}]}}"#,
        nodes.join(","),
        edges.join(",")
    )
}
/// Shortest round-trip float text; non-finite values are quoted tokens
/// because JSON has no numeric spelling for them.
fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        value.to_string()
    }
}
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}
/// Preserve every timestamp component, including second-resolution offsets
/// and the exact timezone database identity. JSON integers use decimal text
/// where their range exceeds the interoperable numeric domain.
fn timestamp_cell(value: &fgdb_types::CanonicalTimestamp) -> String {
    let zone = value.zone().map_or_else(
        || "null".to_owned(),
        |zone| {
            format!(
                r#"{{"identifier":{},"tzdb_oid":"{}"}}"#,
                quoted(zone.identifier()),
                hex(&zone.tzdb_oid().0)
            )
        },
    );
    format!(
        r#"{{"instant_utc_nanos":"{}","utc_offset_seconds":{},"zone":{zone}}}"#,
        value.instant_utc_nanos(),
        value.utc_offset_seconds()
    )
}
fn cell(value: &QueryValue) -> Result<String, Failure> {
    match value {
        QueryValue::Value(v) => value_cell(v),
        QueryValue::Count(v) => Ok(format!(r#"{{"type":"count","value":"{v}"}}"#)),
        QueryValue::Integer(v) => Ok(format!(r#"{{"type":"wideint","value":"{v}"}}"#)),
        QueryValue::Average(v) => Ok(format!(
            r#"{{"type":"average","value":"{}/{}"}}"#,
            v.numerator(),
            v.denominator()
        )),
    }
}
fn human_value(value: &GraphValue) -> Result<String, Failure> {
    Ok(match value {
        GraphValue::Scalar(CanonicalScalar::Null) => "NULL".into(),
        GraphValue::Scalar(CanonicalScalar::Bool(v)) => v.to_string(),
        GraphValue::Scalar(CanonicalScalar::Int(v)) => v.to_string(),
        GraphValue::Scalar(CanonicalScalar::Text(v)) => {
            v.as_str().chars().flat_map(char::escape_default).collect()
        }
        GraphValue::List(values) => format!(
            "[{}]",
            values
                .iter()
                .map(human_value)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        ),
        GraphValue::Scalar(CanonicalScalar::Decimal(v)) => v.to_string(),
        GraphValue::Scalar(CanonicalScalar::Float(v)) => float_text(v.get()),
        GraphValue::Scalar(CanonicalScalar::Timestamp(v)) => timestamp_cell(v),
        GraphValue::Scalar(CanonicalScalar::Bytes(v)) => format!("0x{}", hex(v.as_slice())),
        GraphValue::Vertex(v) => format!("vertex {}", v.0),
        GraphValue::Edge(v) => format!("edge {}", v.0),
        GraphValue::Path(v) => format!("path({})", path_cell(v)),
        GraphValue::Vertices(v) => format!(
            "vertices({})",
            v.iter()
                .map(|id| id.0.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
        GraphValue::Edges(v) => format!(
            "edges({})",
            v.iter()
                .map(|id| id.0.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
    })
}
fn render(result: QueryResult, seq: u64, robot: bool, out: &mut impl Write) -> Result<(), Failure> {
    let QueryResult::Rows { columns, rows } = result else {
        return Err(Failure::query("read returned a write receipt"));
    };
    // Validate the entire output domain before emitting a partial result.
    let rendered: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| {
                    if robot {
                        cell(v)
                    } else {
                        match v {
                            QueryValue::Value(v) => human_value(v),
                            QueryValue::Count(v) => Ok(v.to_string()),
                            QueryValue::Integer(v) => Ok(v.to_string()),
                            QueryValue::Average(v) => Ok(v.to_string()),
                        }
                    }
                })
                .collect()
        })
        .collect::<Result<_, _>>()?;
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"columns","columns":[{}]}}"#,
                columns
                    .iter()
                    .map(|c| quoted(c))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        )?;
        for row in &rendered {
            emit(
                out,
                &format!(r#"{{"v":1,"event":"row","cells":[{}]}}"#, row.join(",")),
            )?;
        }
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"rows","seq":{seq},"count":{}}}"#,
                rows.len()
            ),
        )
    } else {
        let mut widths: Vec<_> = columns.iter().map(|c| c.chars().count()).collect();
        for row in &rendered {
            for (width, value) in widths.iter_mut().zip(row) {
                *width = (*width).max(value.chars().count());
            }
        }
        let line = |row: &[String]| {
            row.iter()
                .zip(&widths)
                .map(|(s, width)| format!("{s:width$}"))
                .collect::<Vec<_>>()
                .join(" | ")
        };
        emit(out, &line(&columns))?;
        emit(out, &"-".repeat(line(&columns).chars().count()))?;
        for row in &rendered {
            emit(out, &line(row))?;
        }
        emit(out, &format!("{} row(s)", rows.len()))
    }
}
