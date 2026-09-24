//! Embedded CLI. Every graph operation uses the public native engine;
//! symbol IDs remain explicit until the library supplies a durable catalog.
#![forbid(unsafe_code)]

mod diff;
mod fnx;
mod import;
mod load;
mod scrub;
mod stream;
mod transaction;

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphPath, GraphValue};
use fgdb_gql::{
    GqlParameterType, GqlParameterValue, GqlParameters, GqlQueryPolicy, GqlScalarParameter,
    GraphSymbol, GraphSymbolKind, GraphSymbolResolver, GraphWriteProgramPolicy,
    PreparedGraphWriteScript, ReverseSymbolCatalog,
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
    r##"{"v":1,"event":"schema","events":{"invocation":["v","event"],"columns":["v","event","columns","statement","stream","seq"],"row":["v","event","cells","statement"],"diff_columns":["v","event","before","after","semantics","columns"],"change":["v","event","weight","cells"],"statement":["v","event","index","kind","view","basis","count","statements"],"progress":["v","event","rows","seq"],"result":["v","event","kind","seq","count","statements","basis","stream","before","after","changed_rows","inserted","retracted","snapshot_records","work_units","scratch_entries","records","objects","repaired","block_objects"],"error":["v","event","class","diagnostics"],"scrub":["v","event","object","kind","state","reason"],"schema":["v","event","events","exit_codes","key_file","bindings","cell_types","result_kinds","transaction","streaming","diff","analytics"]},"exit_codes":{"success":0,"usage":2,"query":3,"open":4,"io":5},"key_file":"Three nonempty lines of 64 hexadecimal characters: object-id key, security namespace, encryption key; # starts a comment. Keys are never printed. On Unix the file must be a regular file with no group or other permission bits (mode 0600), at most 65536 bytes.","bindings":"Repeat --label name=u32, --relation name=u32, --property name=u32 on each invocation; --write-relation u32 defaults to 1. No implicit catalog.","cell_types":["null","bool","int","text","list","count","wideint","average","decimal","float","timestamp","bytes","vertex","edge","path","vertices","edges"],"result_kinds":["created","written","rows","replayed","help","schema","loaded","committed","read_closed","rolled_back","diff","compacted","imported_csv","scrubbed"],"transaction":{"steps":"Ordered --write/--query; each --param belongs to its preceding step. Statement indexes are one-based. Statement/columns/row records describe intermediate transaction-local workspaces, not durable historical snapshots. Only the final result records completion; unknown completion emits an error, never rolled_back. --rollback discards effects and rows.","optional_fields":"statement on columns/row, basis on result; count only on query statements, statements only on write statements","max_statements":64,"max_query_rows":100000,"max_buffered_output_bytes":16777216,"execution_budgets":"per native read or write program; buffered rows/output limits are transaction-wide, not execution byte-memory bounds"},"streaming":{"flag":"query --stream; incompatible with --certify-to","profile":"native single-vertex scan with leading vertex identity, or one-edge scan with leading edge/source identities; canonical order, supported filters and SKIP/LIMIT; temporal cuts supported; no eager fallback or spill","delivery":"columns includes stream=true and the exact selected seq; each row is flushed before pulling another; result with stream=true is emitted only at successful exhaustion; error or EOF without result means an incomplete result, even after rows","memory":"one encoded row, not a collected result; the native source may retain an entire decoded generation","optional_fields":"stream and seq on columns, stream on result; absent on ordinary eager reads"},"diff":{"command":"diff --before <seq> --after <seq> <gql>; both endpoints required, reverse/equal/zero legal","semantics":"after_minus_before_bag: complete native result net changes in one admitted history; positive weight adds occurrences, negative retracts; not write events, ordering changes, cross-branch comparison or DIFF syntax","encoding":"diff_columns then canonical change records then result kind=diff; revisions, weights and all diff counters are decimal strings; cells retain native types","delivery":"both queries and consolidation finish before diff_columns; each change is flushed; only final result plus successful exit and no error establishes complete delivery; a write/flush error may leave a partial final frame","limits":"diff-only --max-snapshot-records, --max-result-rows, --max-work-units, --max-scratch-entries; decimal u64 including zero; defaults 100000/100000/10000000/10000000; cumulative across both queries and consolidation; result rows count changed tuples","output_bytes":"--max-output-bytes is a diff-only decimal u64 transport cap, default 16777216; counts UTF-8 diff frames including newlines and final result, excludes invocation/error records; each whole frame is admitted before writing; a refused frame may follow complete changes but never implies successful completion","memory":"endpoint and consolidated results are in memory; one encoded change at a time; neither output byte cap nor execution limits are spill or an allocator-byte bound","refusals":"explicit temporal selectors, writes, --stream, --certify-to and --certificate are not supported"},"analytics":{"command":"query 'CALL fnx.<procedure>(<args>) YIELD <output> [AS <alias>], ...' runs a registered Prism procedure over an explicit projection of one committed sequence","procedures":["pagerank","single_source_shortest_path_length","single_source_dijkstra_path_length","connected_components","weakly_connected_components","strongly_connected_components","triangles","clustering_coefficient"],"projection":"--graph-label <label> and --graph-relation <relation> select induced vertices and edges (default all); --weight <property> reads edge weights (default unit) with --missing-weight reject|unit|zero (default reject); --direction directed|reversed|undirected (default directed); --parallel-edges reject|collapse|min|max|sum (default reject); --self-loops keep|drop|reject (default keep); --as-of <seq> selects a committed sequence (default frontier)","parameters":"--param name=vertex:<id>|int:<i64>|float:<f64>|bool:true|bool:false|null binds $name; literal arguments need no parameter","output":"columns, row and result kind=rows records; result seq is the analysed sequence; vertex and component cells are vertex identities, hop distances and triangle counts are int, scores and weighted distances are float","refusals":"a projection that violates the procedure's graph laws, a parallel edge under reject, an unknown procedure or argument, and an unbound symbol are refused, never reshaped; --stream and --certify-to are not supported; projection flags without a CALL fnx statement are usage errors"}}"##,
    "\n"
);
const HELP: &str = "fgdb - embedded graph database
Usage: fgdb [--robot] <command>
  create --db <dir> --key-file <file>
  compact --db <dir> --key-file <file>
  scrub --db <dir> --key-file <file>
  write --db <dir> --key-file <file> [bindings] [--param name=value]... <gql>
  query --db <dir> --key-file <file> [bindings] [--param name=value]... [--stream] <gql>
  query --db <dir> --key-file <file> [bindings] [projection] [--param name=value]...
        'CALL fnx.<procedure>(<args>) YIELD <output> [AS <alias>], ...'
  import-csv --db <dir> --key-file <file> [bindings] --input <file.csv|-> [--types-file <file>]
             [--max-input-bytes N] [--max-changes N] (--query-file <file.gql|-> | <gql>)
  diff --db <dir> --key-file <file> [bindings] [--param name=value]... --before <seq> --after <seq> <gql>
  transaction --db <dir> --key-file <file> [bindings] --write <gql> --query <gql> ... [--rollback]
  replay --db <dir> --key-file <file> [bindings] [--param name=value]... --certificate <file>
  load --db <dir> --key-file <file> [bindings] --input <file.ndjson> [--rows-per-chunk N] [--checkpoint <file>]
  robot schema
  help
Parameters: int:42, uint:42, text:Ada, bool:true, bool:false, null,
timestamp:<utc-nanos>,<offset-seconds>,<zone>,<tzdb-oid-hex>.
--tzdb-file <file> supplies a pinned transition-table artifact on every invocation.
Bindings: repeat --label name=u32, --relation name=u32, --property name=u32.
Supply the same bindings on reopen; no implicit catalog or hashed names.
query --certify-to <file> saves a portable result certificate after emitting rows.
query --stream pulls and flushes one native row at a time. Use a single-vertex
scan with leading vertex identity, or a one-edge scan with leading edge/source identities.
Both use canonical order, supported filters and SKIP/LIMIT, including temporal cuts.
Unsupported plans refuse; no eager fallback. --stream cannot use --certify-to.
An error can follow delivered rows; only the terminal result marks a complete stream.
The stream pins decoded source state, not out-of-core storage or a resumable cursor.
diff compares complete results of the same native query at two committed revisions.
Positive weights add occurrences; negative weights retract them. This is a NET bag
difference, not a write log, order-change report, cross-branch diff or DIFF syntax.
Both exact endpoints are required; zero, reverse and equal cuts are legal.
Explicit historical selectors, writes, --stream and --certify-to are refused.
Optional diff-only limits: --max-snapshot-records, --max-result-rows,
--max-work-units, --max-scratch-entries (decimal u64, including zero).
Defaults: 100000 source records, 100000 changed tuples, 10000000 work/scratch units.
One budget covers both endpoint queries and consolidation; results are in memory.
--max-output-bytes bounds UTF-8 diff frames, including the terminal result (default 16 MiB).
It excludes invocation/error records and does not bound encoding scratch or source memory.
Robot diff output uses diff_columns, signed change records, then result kind=diff.
Revisions, weights and diff counters are decimal strings, never lossy JSON numbers.
Only a final result AND successful exit confirm complete delivery; EOF/error is incomplete.
import-csv binds every CSV record (header = parameter names) to the statement and
runs the whole file as ONE atomic native program: any failure commits nothing.
All input is read and bound before the database opens. - reads stdin (one input only).
--types-file lines are kind<TAB>name (int64, uint64, int, text, bool, null); undeclared
parameters keep native inference. --max-input-bytes bounds the CSV (default 16 MiB);
--max-changes bounds effects/new vertices/new edges for the whole file (default 100000).
compact rewrites the storage layout durably; query results are unchanged.
scrub verifies every capsule, repairs damaged redundancy in place, and re-reads every
block; each damaged object is reported, and any loss exits 5 after the reports.
CALL fnx.* runs a registered Prism procedure (pagerank, single_source_shortest_path_length,
single_source_dijkstra_path_length, connected_components, weakly_connected_components,
strongly_connected_components, triangles, clustering_coefficient) over an EXPLICIT
projection of one committed sequence; nothing is silently reshaped:
  --graph-label <label>, --graph-relation <relation>: induced selection (default all)
  --weight <property> [--missing-weight reject|unit|zero]: edge weights (default unit)
  --direction directed|reversed|undirected (default directed)
  --parallel-edges reject|collapse|min|max|sum (default reject)
  --self-loops keep|drop|reject (default keep); --as-of <seq> (default frontier)
Analytics parameters: vertex:<id>, int:<i64>, float:<f64>, bool:true|false, null.
A projection that breaks a procedure's graph laws is refused, never converted.
transaction executes ordered --write/--query steps in one native transaction.
Each --param belongs to the preceding step; parameter maps do not leak between steps.
Success commits once; --rollback discards all effects and results. Errors abort before commit.
Query rows describe each transaction-local workspace, not a durable historical snapshot.
Output is buffered until completion: at most 64 native statements, 100000 query rows,
and 16 MiB encoded output. Execution budgets remain per statement/program, not byte-memory bounds.
--write-relation u32 selects the native mutation coordinate (default 1).
Key file: three nonempty lines of 64 hex characters: object-id key,
security namespace, encryption key. # starts a comment. Keys never printed.
On Unix the key file must be a regular file with mode 0600 (no group/other bits).
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
    raw_params: Vec<(String, String)>,
    tzdb_file: Option<PathBuf>,
    labels: BTreeMap<String, u32>,
    relations: BTreeMap<String, u32>,
    properties: BTreeMap<String, u32>,
    coordinate: RelationId,
    certify_to: Option<PathBuf>,
    certificate: Option<PathBuf>,
    input: Option<PathBuf>,
    rows_per_chunk: usize,
    checkpoint: Option<PathBuf>,
    steps: Vec<transaction::Step>,
    rollback: bool,
    stream: bool,
    diff: diff::DiffOptions,
    csv: import::CsvOptions,
    /// `query` text is a `CALL fnx.*` Prism call, answered by `fnx::run`.
    fnx_call: bool,
    fnx: fnx::ProjectionFlags,
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

impl GraphSymbolResolver for Options {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.resolve(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        for (name, &id) in &self.labels {
            catalog.insert_label(LabelId(u64::from(id)), name.clone());
        }
        for (name, &id) in &self.relations {
            catalog.insert_relation(RelationId(u64::from(id)), name.clone());
        }
        Some(catalog)
    }

    fn reverse_label(&self, id: LabelId) -> Option<String> {
        self.labels
            .iter()
            .find_map(|(name, &raw)| (u64::from(raw) == id.0).then(|| name.clone()))
    }

    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.relations
            .iter()
            .find_map(|(name, &raw)| (u64::from(raw) == id.0).then(|| name.clone()))
    }
}

impl GraphSymbolResolver for &Options {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.resolve(kind, name)
    }

    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        (*self).reverse_catalog()
    }

    fn reverse_label(&self, id: LabelId) -> Option<String> {
        (*self).reverse_label(id)
    }

    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        (*self).reverse_relation(id)
    }
}
fn parse(args: &[String], command: &str) -> Result<Options, Failure> {
    let create = command == "create";
    let mut db = None;
    let mut key = None;
    let mut text = None;
    let params = GqlParameters::new();
    let mut raw_params = Vec::new();
    let mut tzdb_file = None;
    let mut labels = BTreeMap::new();
    let mut relations = BTreeMap::new();
    let mut properties = BTreeMap::new();
    let mut coordinate = RelationId(1);
    let mut certify_to = None;
    let mut certificate = None;
    let mut input = None;
    let mut rows_per_chunk = None;
    let mut checkpoint = None;
    let mut steps: Vec<transaction::Step> = Vec::new();
    let mut rollback = false;
    let mut stream = false;
    let mut diff = diff::DiffOptions::default();
    let mut csv = import::CsvOptions::default();
    let mut fnx = fnx::ProjectionFlags::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--stream" {
            if command != "query" || stream {
                return Err(Failure::usage("--stream is allowed once, only on query"));
            }
            stream = true;
            continue;
        }
        if command == "transaction" && arg == "--rollback" && !rollback {
            rollback = true;
            continue;
        }
        if arg.starts_with("--") {
            let value = iter
                .next()
                .ok_or_else(|| Failure::usage("flag requires a value"))?;
            match arg.as_str() {
                "--db" if db.is_none() => db = Some(PathBuf::from(value)),
                "--key-file" if key.is_none() => key = Some(PathBuf::from(value)),
                "--tzdb-file" if tzdb_file.is_none() => tzdb_file = Some(PathBuf::from(value)),
                "--before"
                | "--after"
                | "--max-snapshot-records"
                | "--max-result-rows"
                | "--max-work-units"
                | "--max-scratch-entries"
                | "--max-output-bytes"
                    if command == "diff" =>
                {
                    diff.set(arg, value)?;
                }
                "--input" if matches!(command, "load" | "import-csv") && input.is_none() => {
                    input = Some(PathBuf::from(value))
                }
                "--types-file" | "--query-file" | "--max-input-bytes" | "--max-changes"
                    if command == "import-csv" =>
                {
                    csv.set(arg, value)?;
                }
                "--checkpoint" if command == "load" && checkpoint.is_none() => {
                    checkpoint = Some(PathBuf::from(value))
                }
                "--rows-per-chunk" if command == "load" && rows_per_chunk.is_none() => {
                    let rows: usize = value
                        .parse()
                        .map_err(|_| Failure::usage("rows-per-chunk must be a positive integer"))?;
                    if rows == 0 {
                        return Err(Failure::usage("rows-per-chunk must be positive"));
                    }
                    rows_per_chunk = Some(rows);
                }
                "--certify-to" if command == "query" && certify_to.is_none() => {
                    certify_to = Some(PathBuf::from(value));
                }
                "--certificate" if command == "replay" && certificate.is_none() => {
                    certificate = Some(PathBuf::from(value));
                }
                "--param"
                    if !create
                        && !matches!(command, "load" | "import-csv" | "compact" | "scrub") =>
                {
                    let (name, raw) = value
                        .split_once('=')
                        .ok_or_else(|| Failure::usage("expected --param name=value"))?;
                    let target = if command == "transaction" {
                        &mut steps
                            .last_mut()
                            .ok_or_else(|| {
                                Failure::usage("transaction --param must follow --query or --write")
                            })?
                            .raw_params
                    } else {
                        &mut raw_params
                    };
                    target.push((name.to_owned(), raw.to_owned()));
                }
                "--write" | "--query" if command == "transaction" => {
                    if steps.len() == transaction::MAX_STATEMENTS {
                        return Err(Failure::usage("transaction statement limit exceeded"));
                    }
                    steps.push(transaction::Step::new(arg == "--write", value.clone()));
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
                "--write-relation" if !matches!(command, "diff" | "compact" | "scrub") => {
                    let id: u32 = value
                        .parse()
                        .map_err(|_| Failure::usage("write relation must be u32"))?;
                    coordinate = RelationId(u64::from(id));
                }
                flag if command == "query" && fnx::ProjectionFlags::FLAGS.contains(&flag) => {
                    fnx.set(flag, value)?;
                }
                _ => return Err(Failure::usage("unknown, duplicate, or inapplicable flag")),
            }
        } else if create
            || matches!(command, "compact" | "scrub")
            || command == "transaction"
            || command == "replay"
            || command == "load"
            || text.replace(arg.clone()).is_some()
        {
            return Err(Failure::usage(
                "expected exactly one GQL argument for query/write/diff, none for create",
            ));
        }
    }
    if command == "replay" && certificate.is_none() {
        return Err(Failure::usage("--certificate required"));
    }
    if matches!(command, "load" | "import-csv") && input.is_none() {
        return Err(Failure::usage("--input required"));
    }
    if command == "import-csv" && text.is_some() == csv.query_file().is_some() {
        return Err(Failure::usage(
            "import-csv takes exactly one statement: a GQL argument or --query-file",
        ));
    }
    if command == "transaction" {
        transaction::validate_input(&steps)?;
    }
    if command == "diff" {
        diff.validate()?;
    }
    if stream && certify_to.is_some() {
        return Err(Failure::usage(
            "--stream cannot be combined with --certify-to",
        ));
    }
    let fnx_call = command == "query" && text.as_deref().is_some_and(fnx::is_call);
    if fnx_call && (stream || certify_to.is_some()) {
        return Err(Failure::usage(
            "CALL fnx.* supports neither --stream nor --certify-to",
        ));
    }
    if !fnx_call && !fnx.is_empty() {
        return Err(Failure::usage(
            "projection flags apply only to a CALL fnx.* query",
        ));
    }
    Ok(Options {
        db: db.ok_or_else(|| Failure::usage("--db required"))?,
        key: key.ok_or_else(|| Failure::usage("--key-file required"))?,
        text: if create
            || matches!(command, "compact" | "scrub")
            || command == "replay"
            || command == "load"
            || command == "transaction"
            || (command == "import-csv" && text.is_none())
        {
            String::new()
        } else {
            text.ok_or_else(|| Failure::usage("GQL argument required"))?
        },
        params,
        raw_params,
        tzdb_file,
        labels,
        relations,
        properties,
        coordinate,
        certify_to,
        certificate,
        input,
        rows_per_chunk: rows_per_chunk.unwrap_or(1000),
        checkpoint,
        steps,
        rollback,
        stream,
        diff,
        csv,
        fnx_call,
        fnx,
    })
}
fn parameter(raw: &str, resolver: Option<&fgdb::PinnedTzdb>) -> Result<GqlParameterValue, Failure> {
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
    } else if let Some(value) = raw.strip_prefix("timestamp:") {
        let fields: Vec<_> = value.split(',').collect();
        if fields.len() != 4 {
            return Err(Failure::usage(
                "timestamp requires nanos,offset,zone,tzdb-oid",
            ));
        }
        let instant = fields[0]
            .parse()
            .map_err(|_| Failure::usage("invalid timestamp nanos"))?;
        let offset = fields[1]
            .parse()
            .map_err(|_| Failure::usage("invalid timestamp offset"))?;
        let oid = fields[3];
        if oid.len() != 64 || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Failure::usage(
                "tzdb OID requires 64 hexadecimal characters",
            ));
        }
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte =
                u8::from_str_radix(&oid[index * 2..index * 2 + 2], 16).map_err(Failure::usage)?;
        }
        let resolver = resolver.ok_or_else(|| Failure::usage("timestamp requires --tzdb-file"))?;
        CanonicalScalar::Timestamp(
            fgdb_types::CanonicalTimestamp::zoned(
                instant,
                offset,
                fields[2],
                fgdb_types::ObjectId(bytes),
                resolver,
            )
            .map_err(Failure::query)?,
        )
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
/// Three 64-hex lines plus comments; anything larger is not a key file.
const MAX_KEY_FILE_BYTES: u64 = 64 * 1024;
async fn read_keys(
    cx: &fgdb_types::QueryCx,
    path: &std::path::Path,
) -> Result<DatabaseKeys, Failure> {
    use asupersync::io::AsyncReadExt as _;
    cx.checkpoint().map_err(Failure::io)?;
    let unreadable = |_| Failure::open("cannot read key file");
    let not_regular = || Failure::open("key file must be a regular file");
    // Refuse a FIFO or device BEFORE opening it: opening a FIFO blocks until a
    // writer appears, so the handle check below would never run.
    let named = asupersync::fs::metadata(path).await.map_err(unreadable)?;
    if !named.is_file() {
        return Err(not_regular());
    }
    let file = asupersync::fs::File::open(path).await.map_err(unreadable)?;
    // Validate the OPENED handle, not a path that could be swapped between a
    // check and the read. Whoever can read this file can read the database.
    let metadata = file.metadata().await.map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(not_regular());
    }
    #[cfg(unix)]
    {
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Failure::open(
                "key file must not be accessible to group or others (chmod 600)",
            ));
        }
    }
    let mut text = String::new();
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .await
        .map_err(unreadable)?;
    if text.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(Failure::open("key file exceeds 65536 bytes"));
    }
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
        | E::WrongDek { .. }
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
        Some(
            command @ ("create" | "query" | "write" | "replay" | "load" | "transaction" | "diff"
            | "compact" | "scrub" | "import-csv"),
        ) => {
            let mut options = parse(&args[1..], command)?;
            let runtime = RuntimeBuilder::new().build().map_err(Failure::io)?;
            let root = runtime.request_cx_with_budget(Budget::INFINITE);
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            runtime.block_on(async {
                let mut keys = read_keys(&contexts.query(), &options.key).await?;
                let artifact = if let Some(path) = &options.tzdb_file {
                    contexts.query().checkpoint().map_err(Failure::io)?;
                    let bytes = asupersync::fs::read(path).await.map_err(Failure::io)?;
                    let artifact = std::sync::Arc::new(fgdb::PinnedTzdb::decode(&bytes).map_err(Failure::open)?);
                    keys = keys.with_scalar_resolver(artifact.clone());
                    Some(artifact)
                } else { None };
                // A Prism call types its own arguments (vertex:<id> among them).
                if !options.fnx_call {
                    for (name, raw) in &options.raw_params {
                        options.params.insert(name, parameter(raw, artifact.as_deref())?).map_err(Failure::query)?;
                    }
                }
                // Every CSV record is read and bound before storage is opened,
                // so a refused input cannot leave any trace in the database.
                let import = if command == "import-csv" { Some(import::prepare(&options, &contexts.query())?) } else { None };
                // Likewise a Prism call's arguments and projection.
                let analytics = if options.fnx_call { Some(fnx::prepare(&options)?) } else { None };
                let mut db = if command == "create" { Database::create(&contexts.commit(), &options.db, keys).await } else { Database::open(&contexts.commit(), &options.db, keys).await }.map_err(open_failure)?;
                if command == "create" {
                    let seq = db.frontier().map_err(Failure::io)?.0;
                    return if robot { emit(out, &format!(r#"{{"v":1,"event":"result","kind":"created","seq":{seq}}}"#)) } else { writeln!(out, "created (seq {seq})").map_err(Failure::io) };
                }
                if command == "compact" {
                    db.compact(&contexts.commit()).await.map_err(execution_failure)?;
                    let seq = db.frontier().map_err(Failure::io)?.0;
                    return if robot { emit(out, &format!(r#"{{"v":1,"event":"result","kind":"compacted","seq":{seq}}}"#)) } else { writeln!(out, "compacted (seq {seq})").map_err(Failure::io) };
                }
                if command == "scrub" {
                    return scrub::run(&mut db, &contexts.commit(), robot, out).await;
                }
                if let Some(prepared) = import {
                    return import::run(&mut db, &contexts, prepared, robot, out).await;
                }
                if command == "load" {
                    return load::run(&mut db, &contexts, &options, artifact.as_deref(), robot, out).await;
                }
                if command == "transaction" {
                    return transaction::run(&mut db, &contexts, &options, artifact.as_deref(), robot, out).await;
                }
                if command == "diff" {
                    return diff::run(&db, &contexts.query(), &options, robot, out);
                }
                if command == "write" {
                    let declarations: Vec<_> = options.params.parameter_types().filter(|(_, kind)| matches!(kind, GqlParameterType::Scalar(_))).collect();
                    let script = PreparedGraphWriteScript::prepare_with_parameter_types(&options.text, options.coordinate, &declarations, |kind, name| options.resolve(kind, name)).map_err(Failure::query)?;
                    let program = script.bind_parameters(&options.params).map_err(Failure::query)?;
                    let (receipt, completion) = db.execute_graph_write_program_returning_autocommit_engine_governed(&contexts.txn(), &contexts.query(), &contexts.commit(), &program, GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000)).await.map_err(execution_failure)?;
                    let seq = match completion { EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq.0, EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => snapshot_seq.0 };
                    return if robot { emit(out, &format!(r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"statements":{}}}"#, receipt.stats().completed_statements)) } else { writeln!(out, "completed at seq {seq}").map_err(Failure::io) };
                }
                if let Some(prepared) = analytics {
                    return fnx::run(&db, &contexts.query(), &options.text, prepared, robot, out);
                }
                if let Some(path) = &options.certificate {
                    contexts.query().checkpoint().map_err(Failure::io)?;
                    let bytes = asupersync::fs::read(path).await.map_err(Failure::io)?;
                    let certificate = fgdb::NativeResultCertificate::decode(&bytes).map_err(Failure::query)?;
                    let result = db.replay(&contexts.query(), &certificate, &options.params, &options, policy()).map_err(Failure::query)?;
                    return render(result, certificate.plan.snapshot_seq.0, "replayed", robot, out);
                }
                if let Some(path) = &options.certify_to {
                    let (result, certificate) = db.execute_certified(&contexts.query(), &options.text, &options.params, &options, policy()).map_err(execution_failure)?;
                    render(result, certificate.plan.snapshot_seq.0, "rows", robot, out)?;
                    out.flush().map_err(Failure::io)?;
                    contexts.query().checkpoint().map_err(Failure::io)?;
                    return asupersync::fs::write(path, certificate.canonical_bytes()).await.map_err(Failure::io);
                }
                if options.stream {
                    return stream::run(&db, &contexts.query(), &options, robot, out);
                }
                let result = db.query(&contexts.query(), &options.text, &options.params, &options, policy()).map_err(execution_failure)?;
                let seq = db.frontier().map_err(Failure::io)?.0;
                render(result, seq, "rows", robot, out)
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
fn render(
    result: QueryResult,
    seq: u64,
    kind: &str,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
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
    render_rows(&columns, rendered, seq, kind, robot, out)
}
/// Emit one complete, already-rendered row set: robot columns, row and result
/// records, or the human table. Shared by native reads and Prism calls.
fn render_rows(
    columns: &[String],
    rendered: Vec<Vec<String>>,
    seq: u64,
    kind: &str,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
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
                r#"{{"v":1,"event":"result","kind":"{kind}","seq":{seq},"count":{}}}"#,
                rendered.len()
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
        emit(out, &line(columns))?;
        emit(out, &"-".repeat(line(columns).chars().count()))?;
        for row in &rendered {
            emit(out, &line(row))?;
        }
        emit(out, &format!("{} row(s)", rendered.len()))
    }
}
