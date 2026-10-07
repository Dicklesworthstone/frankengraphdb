//! Embedded CLI. Every graph operation uses the public native engine;
//! symbol IDs remain explicit until the library supplies a durable catalog.
#![forbid(unsafe_code)]

mod diff;
mod fnx;
mod import;
mod load;
mod remote;
mod scrub;
mod search;
mod spill;
mod stream;
mod transaction;
mod write_returning;

use asupersync::Budget;
use fgdb::{Database, DatabaseKeys, QueryResult, QueryValue};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphPath, GraphValue};
use fgdb_gql::{
    BoundNativeGraphWrite, GqlParameterType, GqlParameterValue, GqlParameters, GqlQueryPolicy,
    GqlScalarParameter, GraphSymbol, GraphSymbolKind, GraphSymbolResolver, GraphWriteProgramPolicy,
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
    r##"{"v":1,"event":"schema","events":{"invocation":["v","event"],"columns":["v","event","columns","statement","stream","seq"],"row":["v","event","cells","statement"],"diff_columns":["v","event","before","after","semantics","columns"],"change":["v","event","weight","cells"],"statement":["v","event","index","kind","view","basis","count","statements","name"],"progress":["v","event","rows","seq"],"result":["v","event","kind","seq","count","statements","basis","stream","before","after","changed_rows","inserted","retracted","snapshot_records","work_units","scratch_entries","records","objects","repaired","block_objects","files","directories","metadata_objects"],"error":["v","event","class","diagnostics"],"scrub":["v","event","object","kind","state","reason"],"schema":["v","event","events","exit_codes","key_file","bindings","cell_types","result_kinds","transaction","streaming","spilling","diff","analytics","search"]},"exit_codes":{"success":0,"usage":2,"query":3,"open":4,"io":5},"key_file":"Three nonempty lines of 64 hexadecimal characters: object-id key, security namespace, encryption key; # starts a comment. Keys are never printed. On Unix the file must be a regular file with no group or other permission bits (mode 0600), at most 65536 bytes.","bindings":"Repeat --label name=u32, --relation name=u32, --property name=u32 on each invocation; --write-relation u32 defaults to 1. No implicit catalog.","cell_types":["null","bool","int","text","list","map","count","wideint","average","decimal","float","timestamp","bytes","vertex","edge","path","vertices","edges"],"result_kinds":["created","written","rows","replayed","help","schema","loaded","committed","read_closed","rolled_back","diff","compacted","imported_csv","scrubbed","searched","adopted"],"transaction":{"steps":"Ordered --write/--query/--savepoint/--rollback-to/--release; each --param belongs to its preceding --write or --query step. Savepoint names are case-sensitive, a reused name shadows the older one until released, and an unknown name is a usage error; --rollback-to discards later steps' effects and their buffered records and rows, --release keeps effects. Statement indexes are one-based. Statement/columns/row records describe intermediate transaction-local workspaces, not durable historical snapshots. Only the final result records completion; unknown completion emits an error, never rolled_back. --rollback discards effects and rows.","optional_fields":"statement on columns/row, basis on result; count on query statements and write statements with RETURN, statements only on write statements, name only on savepoint, rollback_to and release statements","max_statements":64,"max_query_rows":100000,"max_buffered_output_bytes":16777216,"execution_budgets":"per native read or write program; buffered rows/output limits are transaction-wide, not execution byte-memory bounds"},"streaming":{"flag":"query --stream; incompatible with --certify-to","profile":"native single-vertex scan with leading vertex identity, or one-edge scan with leading edge/source identities; canonical order, supported filters and SKIP/LIMIT; temporal cuts supported; no eager fallback or spill","delivery":"columns includes stream=true and the exact selected seq; each row is flushed before pulling another; result with stream=true is emitted only at successful exhaustion; error or EOF without result means an incomplete result, even after rows","memory":"one encoded row, not a collected result; the native source may retain an entire decoded generation","optional_fields":"stream and seq on columns, stream on result; absent on ordinary eager reads"},"spilling":{"flag":"query --spill-dir <existing-directory>; incompatible with --stream, --certify-to and standalone CALL fnx","profile":"native vertex and fixed-edge scans with query-owned external ORDER BY, complete-row DISTINCT then SKIP/LIMIT; projected or hidden order keys and temporal cuts; numeric COUNT/SUM/AVG/MIN/MAX grouping with native row-local computed keys/arguments, complete-group HAVING and computed output expressions, exact typed ORDER BY including hidden aggregates, SKIP/LIMIT and hidden or repeated grouping keys; aggregate argument/output DISTINCT, COLLECT, relational inputs, optional/variable-length joins and unsupported native instructions refuse without eager retry","limits":"--spill-memory-bytes defaults to 67108864 for one shared scratch pool; --spill-disk-bytes defaults to 1073741824 split equally across two append-only files for ordering or three for grouping, including every intermediate and metadata write; --max-spill-rows defaults to 1000000 input occurrences independent of final --max-result-rows; --max-sort-work defaults to 1000000000 additional partition, reduction, sort and copy work, separate from source --max-work-units; decimal u64 including zero, exhaustion refuses","delivery":"complete source and input-expression evaluation, group reduction, HAVING, qualified output expressions and sorting precede columns, including LIMIT 0; canonical full grouping keys order expression evaluation and break ordering ties; computed outputs run once and travel in scratch beside complete ranking cells; hidden cells are removed before delivery; stream=true and exact seq reuse the row-wise stream contract; scratch retirement precedes final result; error or EOF without successful result is incomplete, including output failures","memory":"encoded evaluation rows and private aggregate frames are capped at 1 MiB, including both complete and projected rows in a computed-output frame; resident groups, decoded aggregate rows, computed-output workspaces and projection copies use the shared scratch pool; one authenticated frame is decoded and rendered at a time; decoded graph storage may retain an entire generation; one native source/computed input row, ordinary sorted-row decoding and final output encoding remain outside the scratch pool; input and output expressions share the source cursor's work and scratch-entry budgets","scratch":"private query-owned files only, never fsynced or adopted as database state; success, refusal and drop retire only owned names; process death may leave a private directory that later invocations never reuse or sweep"},"diff":{"command":"diff --before <seq> --after <seq> <gql>; both endpoints required, reverse/equal/zero legal","semantics":"after_minus_before_bag: complete native result net changes in one admitted history; positive weight adds occurrences, negative retracts; not write events, ordering changes, cross-branch comparison or DIFF syntax","encoding":"diff_columns then canonical change records then result kind=diff; revisions, weights and all diff counters are decimal strings; cells retain native types","delivery":"both queries and consolidation finish before diff_columns; each change is flushed; only final result plus successful exit and no error establishes complete delivery; a write/flush error may leave a partial final frame","limits":"--max-snapshot-records, --max-result-rows, --max-work-units, --max-scratch-entries, also accepted by query (eager, --stream and --certify-to) and replay; decimal u64 including zero; defaults 100000/100000/10000000/10000000; cumulative across both queries and consolidation; result rows count changed tuples","output_bytes":"--max-output-bytes is a diff-only decimal u64 transport cap, default 16777216; counts UTF-8 diff frames including newlines and final result, excludes invocation/error records; each whole frame is admitted before writing; a refused frame may follow complete changes but never implies successful completion","memory":"endpoint and consolidated results are in memory; one encoded change at a time; neither output byte cap nor execution limits are spill or an allocator-byte bound","refusals":"explicit temporal selectors, writes, --stream, --certify-to and --certificate are not supported"},"analytics":{"command":"query 'CALL fnx.<procedure>(<args>) YIELD <output> [AS <alias>], ...' runs a registered Prism procedure over an explicit projection of one committed sequence","procedures":["pagerank","single_source_shortest_path_length","single_source_dijkstra_path_length","connected_components","weakly_connected_components","strongly_connected_components","triangles","clustering_coefficient","degree_centrality","closeness_centrality","harmonic_centrality","betweenness_centrality","eigenvector_centrality","core_number"],"projection":"--graph-label <label> and --graph-relation <relation> select induced vertices and edges (default all); --weight <property> reads edge weights (default unit) with --missing-weight reject|unit|zero (default reject); --direction directed|reversed|undirected (default directed); --parallel-edges reject|collapse|min|max|sum (default reject); --self-loops keep|drop|reject (default keep); --as-of <seq> selects a committed sequence (default frontier)","parameters":"--param name=vertex:<id>|int:<i64>|float:<f64>|bool:true|bool:false|null binds $name; literal arguments need no parameter","output":"columns, row and result kind=rows records; result seq is the analysed sequence; vertex and component cells are vertex identities, hop distances, triangle counts and core numbers are int, scores and weighted distances are float","refusals":"a projection that violates the procedure's graph laws, a parallel edge under reject, an unknown procedure or argument, and an unbound symbol are refused, never reshaped; --stream and --certify-to are not supported; projection flags without a standalone CALL fnx statement are usage errors","composed":"a CALL followed by WHERE, WITH, MATCH, UNWIND or RETURN is an ordinary query whose YIELD columns feed that pipeline (MATCH (n) on a yielded vertex name is that vertex; n.p on a yielded vertex reads its property with no MATCH; YIELD node names the vertex output; a non-vertex output used as a vertex is refused at its YIELD position), over the whole graph read in the procedure's direction with parallel edges refused; it takes query --param values and no projection flags"},"search":{"command":"search --text <query> --text-property <property> and/or --vector <x,y,...> with one --vector-property <property> per coordinate, over one committed sequence","lanes":"text only: BM25 over the text property (--text-match any|all|phrase|fuzzy1|fuzzy2|fuzzy1-all|fuzzy2-all, default any; a fuzzy mode matches within that edit distance and expands to at most --max-expansions vocabulary terms, default 64, refusing beyond the bound); vector only: nearest neighbours over the numeric properties in order (--metric l2|cosine|dot, default l2; exact unless --ann <ef_search>); both: exact reciprocal-rank fusion of --candidates hits per lane (default --k), a fusion of two candidate sets, not an exhaustive hybrid answer; graph: --expand-from <vid,...> with an explicit --max-hops <n> expands from those seeds (--expand-relation <relation>, default every relation; --expand-direction out|in|both, default out; --include-seeds true|false, default false) and fuses as a third reciprocal-rank lane (--graph-candidates, default --candidates; --graph-weight, default 1) with the text and/or vector lanes it needs","corpus":"--k bounds the hits (default 10); --vertex-label <label> restricts the corpus; --as-of <seq> selects a committed sequence (default frontier); the index is built for the one call from that sequence","output":"columns, row and result kind=searched records; result seq is the searched sequence; text columns vertex,score; vector columns vertex,distance; both vertex,score,vector_rank,text_rank,vector_distance,text_score where score is an exact decimal, ranks are int and an absent lane value is null; with the graph lane: vertex,score,vector_rank,text_rank,graph_rank,vector_distance,text_score,graph_hops","refusals":"no lane, a lane flag without its lane, a vector whose length differs from its --vector-property count, non-finite coordinates, --candidates without a fused search (both lanes or the graph lane), --max-expansions without a fuzzy mode, graph lane flags without --expand-from, --expand-from without --max-hops or without a text or vector lane, and unbound symbols are usage errors; --param and --write-relation are not accepted"}}"##,
    "\n"
);
const HELP: &str = "fgdb - embedded graph database
Usage: fgdb [--robot] <command>
  create --db <dir> --key-file <file>
  adopt --db <dir>
  compact --db <dir> --key-file <file>
  scrub --db <dir> --key-file <file>
  write --db <dir> --key-file <file> [bindings] [--param name=value]... [--rows json:<objects>] <gql>
  query --db <dir> --key-file <file> [bindings] [--param name=value]... [limits] [--stream | --spill-dir <dir>] <gql>
  query --db <dir> --key-file <file> [bindings] [projection] [--param name=value]...
        'CALL fnx.<procedure>(<args>) YIELD <output> [AS <alias>], ...'
  import-csv --db <dir> --key-file <file> [bindings] --input <file.csv|-> [--types-file <file>]
             [--max-input-bytes N] [--max-changes N] (--query-file <file.gql|-> | <gql>)
  diff --db <dir> --key-file <file> [bindings] [--param name=value]... --before <seq> --after <seq> <gql>
  transaction --db <dir> --key-file <file> [bindings] --write <gql> --query <gql> ... [--rollback]
    (steps may include --savepoint <name>, --rollback-to <name>, --release <name>)
  replay --db <dir> --key-file <file> [bindings] [--param name=value]... [limits] --certificate <file>
  load --db <dir> --key-file <file> [bindings] --input <file.ndjson> [--rows-per-chunk N] [--checkpoint <file>]
  search --db <dir> --key-file <file> [bindings] [text lane] [vector lane] [--k N]
         [--candidates N] [--vertex-label <label>] [--as-of <seq>]
  remote --addr <ip:port> --token-file <file> --database <name> [--param name=value]... query|write <gql>
  remote --addr <ip:port> --token-file <file> --database <name> [--param name=value]...
         [--max-batches N] subscribe <read>
  robot schema
  help
remote runs one statement on an fgdbd server over FGP, authenticated by the hex
capability token in --token-file (owner-only; minted by `fgdbd token`). Output and
exit codes are query/write's; the server's own name bindings apply. Remote
parameters: int:, float:, text:, bool:true|false, null, json:<array|object>.
remote subscribe streams a live changefeed (SUBSCRIBE TO <read>): columns, then
per batch one change record per row (signed weight; the first batch is the
baseline) and a progress record with the frontier. It needs a capability with
unrestricted scope; --max-batches N cancels after N batches with a final result.
For encrypted FGP, add --tls-server-name <certificate-name> --tls-ca-file <ca.pem>.
Both flags are required together; the peer certificate and name are verified,
TLS 1.3 and fgp/1 are required, and a TLS failure never retries in plaintext.
Parameters: int:42, uint:42, float:1.5e-3, text:Ada, bool:true, bool:false, null,
timestamp:<utc-nanos>,<offset-seconds>,<zone>,<tzdb-oid-hex>,
json:<array|object> (a list or map; integers int, other numbers float).
write --rows json:[{...},...] binds the statement's parameters once per object
and commits every row as ONE atomic program (MERGE sees earlier rows).
--tzdb-file <file> supplies a pinned transition-table artifact on every invocation.
Bindings: repeat --label name=u32, --relation name=u32, --property name=u32.
Supply the same bindings on reopen; no implicit catalog or hashed names.
query --certify-to <file> saves a portable result certificate after emitting rows.
query and replay take the same four limits as diff; a refusal names the limit it hit.
query --stream pulls and flushes one native row at a time. Use a single-vertex
scan with leading vertex identity, or a one-edge scan with leading edge/source identities.
Both use canonical order, supported filters and SKIP/LIMIT, including temporal cuts.
Unsupported plans refuse; no eager fallback. --stream cannot use --certify-to.
An error can follow delivered rows; only the terminal result marks a complete stream.
The stream pins decoded source state, not out-of-core storage or a resumable cursor.
query --spill-dir <existing-dir> externally orders native vertex/fixed-edge scans,
including property-only output, hidden ORDER BY keys, DISTINCT, SKIP/LIMIT and
temporal cuts. DISTINCT requires projected sort keys. Numeric grouping
uses partitioned scratch for count/sum/avg/min/max over vertex/fixed-edge inputs;
computed keys and arguments run through the native expression evaluator first.
HAVING and computed output expressions run on every qualified complete group
before typed ORDER BY and SKIP/LIMIT. Counts, wide sums and exact averages keep
their native types. Aggregate argument/output DISTINCT, COLLECT and relational
inputs currently refuse, as do
optional/variable-length joins and unsupported instructions; no eager retry.
This flag cannot combine
with --stream, --certify-to or a standalone CALL fnx. Sorting completes before output.
--spill-memory-bytes (default 67108864) bounds the shared scratch pool;
--spill-disk-bytes (default 1073741824) is split across two append-only files for
ordering or three for grouping. Every intermediate pass spends the allowance.
--max-spill-rows (default 1000000) bounds intermediate rows independently of final
--max-result-rows; --max-sort-work (default 1000000000) bounds additional partition,
reduction and sort work. Completed results are canonically ordered before output.
All are decimal u64. Scratch files are private and retired before final success.
Each encoded row is at most 1 MiB; delivery decodes/encodes one row at a time.
Decoded graph storage, one native row and its encoding remain outside the pool.
An error or EOF without a successful terminal result means incomplete delivery.
diff compares complete results of the same native query at two committed revisions.
Positive weights add occurrences; negative weights retract them. This is a NET bag
difference, not a write log, order-change report, cross-branch diff or DIFF syntax.
Both exact endpoints are required; zero, reverse and equal cuts are legal.
Explicit historical selectors, writes, --stream and --certify-to are refused.
Optional limits for diff and query: --max-snapshot-records, --max-result-rows,
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
--types-file lines are kind<TAB>name (int64, uint64, int, text, bool, null). Types come
from the statement, never from values: an undeclared property parameter is int64, so
declare every text or bool column. --max-input-bytes bounds the CSV (default 16 MiB);
--max-changes bounds effects/new vertices/new edges for the whole file (default 100000).
load commits the NDJSON input in chunks of --rows-per-chunk rows (at most 65536).
By default it takes the largest power of two whose every chunk carries at most 4 MiB
of source transcript, chosen from the input itself, so a resume chooses the same.
compact rewrites the storage layout durably; query results are unchanged.
adopt needs no keys. Run it once after copying a database without syncing its files,
before the first write; it syncs all regular files, directories and the parent.
scrub verifies every capsule, repairs damaged redundancy in place, and re-reads the
current generation's blocks, vertex patches, manifest, root and root segments;
each damaged object is reported, and any loss exits 5 after the reports.
CALL fnx.* runs a registered Prism procedure (pagerank, single_source_shortest_path_length,
single_source_dijkstra_path_length, connected_components, weakly_connected_components,
strongly_connected_components, triangles, clustering_coefficient; and, run by the
franken_networkx catalog itself over an undirected copy with self-loops refused:
degree_centrality, closeness_centrality, harmonic_centrality, betweenness_centrality,
eigenvector_centrality, core_number) over an EXPLICIT projection of one committed
sequence; nothing is silently reshaped:
  --graph-label <label>, --graph-relation <relation>: induced selection (default all)
  --weight <property> [--missing-weight reject|unit|zero]: edge weights (default unit)
  --direction directed|reversed|undirected (default directed)
  --parallel-edges reject|collapse|min|max|sum (default reject)
  --self-loops keep|drop|reject (default keep); --as-of <seq> (default frontier)
Analytics parameters: vertex:<id>, int:<i64>, float:<f64>, bool:true|false, null.
A projection that breaks a procedure's graph laws is refused, never converted.
A CALL followed by WHERE, WITH, MATCH, UNWIND or RETURN is an ordinary query: its YIELD
columns feed the pipeline (MATCH (n) on a yielded vertex name is that vertex), over the
whole graph read in the procedure's direction, parallel edges refused; it takes query
--param values and no projection flags. YIELD node names the vertex output, and n.p on
a yielded vertex reads its property with no MATCH; a non-vertex output used as a
vertex is refused at its YIELD position.
search runs Beacon retrieval over an EXPLICIT projection of one committed sequence:
  text lane: --text <query> --text-property <property> [--text-match any|all|phrase]
    or --text-match fuzzy1|fuzzy2|fuzzy1-all|fuzzy2-all within that edit distance,
    expanding to at most --max-expansions vocabulary terms (default 64; beyond refuses)
  vector lane: --vector <x,y,...> with one --vector-property <property> per coordinate,
    in order [--metric l2|cosine|dot] [--ann <ef_search>] (default exact)
  both lanes: exact reciprocal-rank fusion of --candidates hits per lane (default --k);
    a fusion of the two candidate sets, not an exhaustive hybrid answer
  graph lane: --expand-from <vid,...> --max-hops <n> [--expand-relation <relation>]
    [--expand-direction out|in|both] [--include-seeds true|false] [--graph-candidates N]
    [--graph-weight N], fused as a third lane with the text and/or vector lanes
  --k bounds the hits (default 10); --vertex-label restricts the corpus.
The index is built for the one call from that sequence. Columns: text vertex,score;
vector vertex,distance; both vertex,score,vector_rank,text_rank,vector_distance,text_score;
with the graph lane also graph_rank and graph_hops.
transaction executes ordered --write/--query steps in one native transaction.
Each --param belongs to the preceding step; parameter maps do not leak between steps.
Success commits once; --rollback discards all effects and results. Errors abort before commit.
Query rows describe each transaction-local workspace, not a durable historical snapshot.
Output is buffered until completion: at most 64 native statements, 100000 query rows,
and 16 MiB encoded output. Execution budgets remain per statement/program, not byte-memory bounds.
--write-relation u32 selects the native mutation coordinate (default 1).
write accepts standalone or UNWIND-driven CREATE/INSERT ... RETURN and
MATCH ... SET/REMOVE/DETACH DELETE ... RETURN (one row per matched occurrence;
a property read sees the statement's own assignment) and MERGE ... RETURN (the
one chosen vertex after every ON/SET clause). It buffers the complete
result before committing, then emits typed columns and rows followed by result
kind=written at the committed sequence. DISTINCT and LIMIT affect only returned
rows, not the writes. Encoded output is bounded to 16 MiB.
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
    /// `write --rows json:[...]`: one parameter set per object.
    rows: Option<String>,
    tzdb_file: Option<PathBuf>,
    labels: BTreeMap<String, u32>,
    relations: BTreeMap<String, u32>,
    properties: BTreeMap<String, u32>,
    coordinate: RelationId,
    certify_to: Option<PathBuf>,
    certificate: Option<PathBuf>,
    input: Option<PathBuf>,
    rows_per_chunk: Option<usize>,
    checkpoint: Option<PathBuf>,
    steps: Vec<transaction::Step>,
    rollback: bool,
    stream: bool,
    spill: spill::SpillOptions,
    /// `query`/`replay` execution limits; absent ones keep the CLI defaults.
    budget: QueryBudget,
    diff: diff::DiffOptions,
    csv: import::CsvOptions,
    /// `query` text is a `CALL fnx.*` Prism call, answered by `fnx::run`.
    fnx_call: bool,
    fnx: fnx::ProjectionFlags,
    /// `search` lanes and corpus, answered by `search::run`.
    search: search::SearchFlags,
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
    let mut rows = None;
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
    let mut spill = spill::SpillOptions::default();
    let mut budget = QueryBudget::default();
    let mut diff = diff::DiffOptions::default();
    let mut csv = import::CsvOptions::default();
    let mut fnx = fnx::ProjectionFlags::default();
    let mut search_flags = search::SearchFlags::default();
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
                "--max-snapshot-records"
                | "--max-result-rows"
                | "--max-work-units"
                | "--max-scratch-entries"
                    if matches!(command, "query" | "replay") =>
                {
                    budget.set(arg, value)?;
                }
                flag if command == "query" && spill::SpillOptions::FLAGS.contains(&flag) => {
                    spill.set(flag, value)?;
                }
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
                "--rows" if command == "write" && rows.is_none() => {
                    rows = Some(value.clone());
                }
                "--certify-to" if command == "query" && certify_to.is_none() => {
                    certify_to = Some(PathBuf::from(value));
                }
                "--certificate" if command == "replay" && certificate.is_none() => {
                    certificate = Some(PathBuf::from(value));
                }
                "--param"
                    if !create
                        && !matches!(
                            command,
                            "load" | "import-csv" | "compact" | "scrub" | "search"
                        ) =>
                {
                    let (name, raw) = value
                        .split_once('=')
                        .ok_or_else(|| Failure::usage("expected --param name=value"))?;
                    let target = if command == "transaction" {
                        &mut steps
                            .last_mut()
                            .filter(|step| step.takes_params())
                            .ok_or_else(|| {
                                Failure::usage("transaction --param must follow --query or --write")
                            })?
                            .raw_params
                    } else {
                        &mut raw_params
                    };
                    target.push((name.to_owned(), raw.to_owned()));
                }
                flag if command == "transaction"
                    && transaction::StepKind::of_flag(flag).is_some() =>
                {
                    if steps.len() == transaction::MAX_STATEMENTS {
                        return Err(Failure::usage("transaction statement limit exceeded"));
                    }
                    let kind = transaction::StepKind::of_flag(flag)
                        .ok_or_else(|| Failure::usage("unknown transaction step"))?;
                    steps.push(transaction::Step::new(kind, value.clone()));
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
                "--write-relation"
                    if !matches!(command, "diff" | "compact" | "scrub" | "search") =>
                {
                    let id: u32 = value
                        .parse()
                        .map_err(|_| Failure::usage("write relation must be u32"))?;
                    coordinate = RelationId(u64::from(id));
                }
                flag if command == "query" && fnx::ProjectionFlags::FLAGS.contains(&flag) => {
                    fnx.set(flag, value)?;
                }
                flag if command == "search" && search::SearchFlags::FLAGS.contains(&flag) => {
                    search_flags.set(flag, value)?;
                }
                _ => return Err(Failure::usage("unknown, duplicate, or inapplicable flag")),
            }
        } else if create
            || matches!(command, "compact" | "scrub" | "search")
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
    spill.validate()?;
    if spill.enabled() && (stream || certify_to.is_some() || fnx_call) {
        return Err(Failure::usage(
            "--spill-dir cannot be combined with --stream, --certify-to or standalone CALL fnx",
        ));
    }
    if fnx_call && (stream || certify_to.is_some()) {
        return Err(Failure::usage(
            "CALL fnx.* supports neither --stream nor --certify-to",
        ));
    }
    if !fnx_call && !fnx.is_empty() {
        return Err(Failure::usage(
            "projection flags apply only to a standalone CALL fnx.* query; \
             a CALL that continues into a read runs over the whole graph",
        ));
    }
    Ok(Options {
        db: db.ok_or_else(|| Failure::usage("--db required"))?,
        key: key.ok_or_else(|| Failure::usage("--key-file required"))?,
        text: if create
            || matches!(command, "compact" | "scrub" | "search")
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
        rows,
        tzdb_file,
        labels,
        relations,
        properties,
        coordinate,
        certify_to,
        certificate,
        input,
        rows_per_chunk,
        checkpoint,
        steps,
        rollback,
        stream,
        spill,
        budget,
        diff,
        csv,
        fnx_call,
        fnx,
        search: search_flags,
    })
}
fn parameter(raw: &str, resolver: Option<&fgdb::PinnedTzdb>) -> Result<GqlParameterValue, Failure> {
    if let Some(value) = raw.strip_prefix("json:") {
        return json_parameter(value);
    }
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
    } else if let Some(value) = raw.strip_prefix("float:") {
        let value: f64 = value
            .parse()
            .map_err(|_| Failure::usage("invalid float parameter"))?;
        if !value.is_finite() {
            return Err(Failure::usage("a float parameter must be finite"));
        }
        CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value))
    } else if let Some(value) = raw.strip_prefix("bytes:") {
        let bytes = fgdb_protocol::json::bytes_from_hex(value).map_err(Failure::usage)?;
        CanonicalScalar::bytes(bytes).map_err(Failure::query)?
    } else if let Some(value) = raw.strip_prefix("vector:") {
        CanonicalScalar::bytes(remote::packed_vector(value)?).map_err(Failure::query)?
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
/// Bounds on one `json:` parameter document, before the collection parameter's own
/// canonical-size cap.
const MAX_JSON_PARAMETER_VALUES: usize = 65_536;
const MAX_JSON_PARAMETER_TOKEN_BYTES: usize = 65_536;

/// `json:<array|object>` binds a collection parameter, e.g. the rows of
/// `UNWIND $rows AS row CREATE (:Person {name: row.name})`. Objects are maps,
/// strings text, integers int, other finite numbers float, true/false bool,
/// null null. A top-level scalar uses the CLI's explicit scalar spellings.
fn json_parameter(text: &str) -> Result<GqlParameterValue, Failure> {
    let json = load::parse_json(
        text,
        MAX_JSON_PARAMETER_VALUES,
        MAX_JSON_PARAMETER_TOKEN_BYTES,
    )
    .map_err(|error| Failure::usage(format!("invalid json parameter: {error}")))?;
    match json_value(&json)? {
        GraphValue::List(values) => fgdb_gql::GqlListParameter::new(values.into_vec())
            .map(GqlParameterValue::List)
            .map_err(Failure::usage),
        GraphValue::Map { keys, values } => {
            fgdb_gql::GqlMapParameter::new(keys.into_vec().into_iter().zip(values).collect())
                .map(GqlParameterValue::Map)
                .map_err(Failure::usage)
        }
        _ => Err(Failure::usage(
            "invalid json parameter: expected a JSON array or object",
        )),
    }
}
fn json_value(json: &load::Json) -> Result<GraphValue, Failure> {
    let bad = |detail: &str| Failure::usage(format!("invalid json parameter: {detail}"));
    Ok(match json {
        load::Json::Null => GraphValue::Scalar(CanonicalScalar::Null),
        load::Json::Bool(value) => GraphValue::Scalar(CanonicalScalar::Bool(*value)),
        load::Json::Number(text) if text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) => {
            let value: f64 = text.parse().map_err(|_| bad("number"))?;
            if !value.is_finite() {
                return Err(bad("float out of range"));
            }
            GraphValue::Scalar(CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value)))
        }
        load::Json::Number(text) => GraphValue::Scalar(CanonicalScalar::Int(
            text.parse().map_err(|_| bad("integer out of range"))?,
        )),
        load::Json::String(text) => {
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text(text).map_err(Failure::usage)?)
        }
        load::Json::Array(items) => {
            GraphValue::List(items.iter().map(json_value).collect::<Result<_, _>>()?)
        }
        load::Json::Object(fields) => GraphValue::map(
            fields
                .iter()
                .map(|(key, value)| Ok((key.as_str().into(), json_value(value)?)))
                .collect::<Result<Vec<_>, Failure>>()?,
        )
        .ok_or_else(|| bad("duplicate key"))?,
    })
}

/// `write --rows json:[{...}, ...]` binds the statement's parameters once per
/// object and runs every row in ONE atomic program: the same record-major
/// batch import-csv builds, so a later row's MERGE sees an earlier row's
/// effects and a refused row commits nothing. A JSON integer binds like
/// `int:`, unless the key is null or absent in some row: then it binds as a
/// nullable integer scalar. A float, string or boolean binds as that scalar
/// kind, and an array as a `json:` list. A key's kind is its first non-null
/// value's, and every row must agree. A null or absent scalar key is NULL; a
/// list key cannot be NULL. A key that is not a parameter of the statement,
/// or that repeats a --param, is a usage error, and so is an empty batch.
fn prepare_rows(
    options: &Options,
) -> Result<(fgdb_gql::PreparedGraphWriteProgram, usize), Failure> {
    let bad = |detail: String| Failure::usage(format!("invalid --rows: {detail}"));
    let text = options.rows.as_deref().unwrap_or_default();
    let text = text
        .strip_prefix("json:")
        .ok_or_else(|| bad("expected json:<array of objects>".into()))?;
    let json = load::parse_json(
        text,
        MAX_JSON_PARAMETER_VALUES,
        MAX_JSON_PARAMETER_TOKEN_BYTES,
    )
    .map_err(bad)?;
    let load::Json::Array(items) = json else {
        return Err(bad("expected a JSON array of objects".into()));
    };
    if items.is_empty() {
        return Err(bad("expected at least one row".into()));
    }
    let mut rows = Vec::new();
    let mut kinds: BTreeMap<String, Option<GqlParameterType>> = BTreeMap::new();
    for (at, item) in items.iter().enumerate() {
        let load::Json::Object(fields) = item else {
            return Err(bad(format!("row {at} is not an object")));
        };
        let mut row = BTreeMap::new();
        for (key, value) in fields {
            if options.params.get(key).is_some() {
                return Err(bad(format!("key {key} repeats a --param")));
            }
            let value = match value {
                load::Json::Null => None,
                load::Json::Number(text)
                    if !text.bytes().any(|b| matches!(b, b'.' | b'e' | b'E')) =>
                {
                    Some(GqlParameterValue::Int64(text.parse().map_err(|_| {
                        bad(format!("row {at} key {key}: integer out of range"))
                    })?))
                }
                other => Some(match json_value(other)? {
                    GraphValue::List(values) => GqlParameterValue::List(
                        fgdb_gql::GqlListParameter::new(values.into_vec())
                            .map_err(Failure::usage)?,
                    ),
                    GraphValue::Map { keys, values } => GqlParameterValue::Map(
                        fgdb_gql::GqlMapParameter::new(
                            keys.into_vec().into_iter().zip(values).collect(),
                        )
                        .map_err(Failure::usage)?,
                    ),
                    GraphValue::Scalar(scalar) => GqlParameterValue::Scalar(
                        GqlScalarParameter::new(scalar).map_err(Failure::usage)?,
                    ),
                    _ => return Err(bad(format!("row {at} key {key}: unsupported value"))),
                }),
            };
            let kind = kinds.entry(key.clone()).or_insert(None);
            if let Some(value) = &value {
                match kind {
                    Some(existing) if *existing != value.parameter_type() => {
                        return Err(bad(format!("key {key} mixes kinds across rows")));
                    }
                    _ => *kind = Some(value.parameter_type()),
                }
            }
            row.insert(key.clone(), value);
        }
        rows.push(row);
    }
    // A key that is NULL in every row has no kind to learn: bind it as NULL.
    // An integer key that is NULL or absent in some row binds as a nullable
    // integer scalar in every row, since an int: parameter cannot be NULL.
    let null_kind = GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Null);
    let int_kind = GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Int);
    let kinds: BTreeMap<String, GqlParameterType> = kinds
        .into_iter()
        .map(|(key, kind)| {
            let sparse = rows
                .iter()
                .any(|row| !matches!(row.get(&key), Some(Some(_))));
            let kind = match kind {
                Some(GqlParameterType::Int64) if sparse => int_kind,
                Some(kind) => kind,
                None => null_kind,
            };
            (key, kind)
        })
        .collect();
    for row in &mut rows {
        for (key, value) in row.iter_mut() {
            if kinds.get(key) == Some(&int_kind)
                && let Some(GqlParameterValue::Int64(int)) = value
            {
                *value = Some(GqlParameterValue::Scalar(
                    GqlScalarParameter::new(CanonicalScalar::Int(*int)).map_err(Failure::usage)?,
                ));
            }
        }
    }
    // Numeric roles retain inference; collection/scalar kinds are explicit.
    let mut declarations: Vec<(&str, GqlParameterType)> = options
        .params
        .parameter_types()
        .filter(|(_, kind)| {
            matches!(
                kind,
                GqlParameterType::Scalar(_) | GqlParameterType::List | GqlParameterType::Map
            )
        })
        .collect();
    declarations.extend(
        kinds
            .iter()
            .filter(|(_, kind)| {
                matches!(
                    kind,
                    GqlParameterType::Scalar(_) | GqlParameterType::List | GqlParameterType::Map
                )
            })
            .map(|(key, kind)| (key.as_str(), *kind)),
    );
    let script = PreparedGraphWriteScript::prepare_with_parameter_types(
        &options.text,
        options.coordinate,
        &declarations,
        |kind, name| options.resolve(kind, name),
    )
    .map_err(Failure::query)?;
    if let Some(key) = kinds.keys().find(|key| {
        !script
            .parameter_schema()
            .iter()
            .any(|spec| spec.name == **key)
    }) {
        return Err(bad(format!(
            "key {key} is not a parameter of the statement"
        )));
    }
    let null = GqlParameterValue::Scalar(
        GqlScalarParameter::new(CanonicalScalar::Null).map_err(Failure::query)?,
    );
    let mut sets = Vec::new();
    for (at, row) in rows.iter().enumerate() {
        let mut set = options.params.clone();
        for (key, kind) in &kinds {
            let value = match (row.get(key), kind) {
                (Some(Some(value)), _) => value.clone(),
                (_, GqlParameterType::Scalar(_) | GqlParameterType::Map) => null.clone(),
                _ => {
                    return Err(bad(format!(
                        "row {at} key {key} is null or absent; a list parameter cannot be NULL"
                    )));
                }
            };
            set.insert(key, value).map_err(Failure::query)?;
        }
        sets.push(set);
    }
    let batch = script
        .bind_parameter_sets_with_limit(&sets, PreparedGraphWriteScript::MAX_BATCH_STATEMENTS)
        .map_err(Failure::query)?;
    Ok((batch.into_program(), rows.len()))
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
        for (byte, pair) in key.iter_mut().zip(line.as_bytes().as_chunks::<2>().0) {
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

/// The four execution limits `query` and `diff` accept, each a decimal u64
/// given at most once; an absent limit keeps [`policy`]'s default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct QueryBudget {
    records: Option<u64>,
    rows: Option<u64>,
    work: Option<u64>,
    scratch: Option<u64>,
}
impl QueryBudget {
    fn set(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
        let target = match flag {
            "--max-snapshot-records" => &mut self.records,
            "--max-result-rows" => &mut self.rows,
            "--max-work-units" => &mut self.work,
            "--max-scratch-entries" => &mut self.scratch,
            _ => return Err(Failure::usage("unknown limit flag")),
        };
        set_decimal(target, flag, raw)
    }

    fn policy(&self) -> GqlQueryPolicy {
        let defaults = policy();
        GqlQueryPolicy::new(
            self.records
                .unwrap_or(defaults.rows.max_snapshot_records().unwrap_or(u64::MAX)),
            self.rows
                .unwrap_or(defaults.rows.max_result_rows().unwrap_or(u64::MAX)),
            self.work.unwrap_or(defaults.evaluator.max_work_units),
            self.scratch
                .unwrap_or(defaults.evaluator.max_scratch_entries),
        )
    }
}

/// Store one decimal u64 flag value, refusing a repeat, signs, whitespace,
/// fractions and overflow without echoing the value.
fn set_decimal(target: &mut Option<u64>, flag: &str, raw: &str) -> Result<(), Failure> {
    if target.is_some() {
        return Err(Failure::usage(format!("{flag} must be supplied only once")));
    }
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Failure::usage(format!("{flag} requires decimal u64")));
    }
    *target = Some(
        raw.parse()
            .map_err(|_| Failure::usage(format!("{flag} exceeds u64")))?,
    );
    Ok(())
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
        Some("remote") => remote::run(&args[1..], robot, out),
        Some("adopt") => adopt(&args[1..], robot, out),
        Some(
            command @ ("create" | "query" | "write" | "replay" | "load" | "transaction" | "diff"
            | "compact" | "scrub" | "import-csv" | "search"),
        ) => {
            let mut options = parse(&args[1..], command)?;
            let runtime = fgdb::runtime_builder().build().map_err(Failure::io)?;
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
                // And a search's lanes, symbols and corpus.
                let retrieval = if command == "search" { Some(search::prepare(&options)?) } else { None };
                // A --rows batch is parsed, typed and bound before storage opens,
                // like CSV, so a refused row leaves no trace in the database.
                let batch = if command == "write" && options.rows.is_some() {
                    Some(prepare_rows(&options)?)
                } else {
                    None
                };
                let returning = if command == "write" && batch.is_none() { write_returning::prepare(&options)? } else { None };
                // Native UNWIND mutations and ordinary scripts bind completely
                // before opening storage. Keep the bound record map through
                // completion instead of discarding it into a bare program.
                let native = if command == "write" && batch.is_none() && returning.is_none() {
                    Some(BoundNativeGraphWrite::bind(
                        &options.text,
                        &options.params,
                        options.coordinate,
                        |kind, name| options.resolve(kind, name),
                    ).map_err(Failure::query)?)
                } else {
                    None
                };
                // Reads need no writer: open only the published generation,
                // without the fold, version heads and delta history that writes
                // and certified execution use. Each read runs on that view, as
                // the database's own read entrypoints do on a fresh view.
                if matches!(command, "diff" | "search") || (command == "query" && options.certify_to.is_none()) {
                    let view = Database::open_read_view(&contexts.commit(), &options.db, keys).await.map_err(open_failure)?;
                    let cx = contexts.query();
                    let outcome = if command == "diff" {
                        diff::run(&view, &cx, &options, robot, out)
                    } else if let Some(prepared) = analytics {
                        fnx::run(&view, &cx, &options.text, prepared, robot, out)
                    } else if let Some(prepared) = &retrieval {
                        search::run(&view, &cx, prepared, robot, out)
                    } else if options.spill.enabled() {
                        spill::run(
                            &view, &cx, &options,
                            artifact.as_deref().map(|artifact| artifact as &(dyn fgdb_types::CanonicalScalarResolver + Send + Sync)),
                            robot, out,
                        ).await
                    } else if options.stream {
                        stream::run(&view, &cx, &options, robot, out)
                    } else {
                        view.query(&cx, &options.text, &options.params, &options, options.budget.policy())
                            .map_err(execution_failure)
                            .and_then(|result| render(result, view.frontier().0, "rows", robot, out))
                    };
                    // The process exits next. A view owns only memory (no lease,
                    // file or writer), and freeing a large generation cell by cell
                    // is work nothing observes; the OS reclaims it whole.
                    std::mem::forget(view);
                    return outcome;
                }
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
                if command == "write" {
                    if let Some(prepared) = returning {
                        return write_returning::run(&mut db, &contexts, prepared, robot, out).await;
                    }
                    let (program, records) = match batch.as_ref() {
                        Some((program, records)) => (program, Some(*records)),
                        None => {
                            let prepared = native.as_ref().expect("no-RETURN write was bound before open");
                            (prepared.program(), prepared.input_records())
                        }
                    };
                    let (receipt, completion) = db.execute_graph_write_program_returning_autocommit_engine_governed(&contexts.txn(), &contexts.query(), &contexts.commit(), program, GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000)).await.map_err(|error| {
                        match &native {
                            Some(prepared) => execution_failure(prepared.execution_error(error)),
                            None => execution_failure(error),
                        }
                    })?;
                    let seq = match completion { EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq.0, EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => snapshot_seq.0 };
                    let statements = receipt.stats().completed_statements;
                    // The robot record is the plain write's (the caller sent the
                    // rows); only the human line counts them.
                    return match (robot, records) {
                        (true, _) => emit(out, &format!(r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"statements":{statements}}}"#)),
                        (false, Some(records)) => writeln!(out, "wrote {records} row(s) at seq {seq}").map_err(Failure::io),
                        (false, None) => writeln!(out, "completed at seq {seq}").map_err(Failure::io),
                    };
                }
                if let Some(path) = &options.certificate {
                    contexts.query().checkpoint().map_err(Failure::io)?;
                    let bytes = asupersync::fs::read(path).await.map_err(Failure::io)?;
                    let certificate = fgdb::NativeResultCertificate::decode(&bytes).map_err(Failure::query)?;
                    let result = db.replay(&contexts.query(), &certificate, &options.params, &options, options.budget.policy()).map_err(Failure::query)?;
                    return render(result, certificate.plan.snapshot_seq.0, "replayed", robot, out);
                }
                // Every other command returned above; only `query --certify-to`
                // remains.
                let Some(path) = &options.certify_to else {
                    return Err(Failure::usage("unknown or missing subcommand; use fgdb help"));
                };
                let (result, certificate) = db.execute_certified(&contexts.query(), &options.text, &options.params, &options, options.budget.policy()).map_err(execution_failure)?;
                render(result, certificate.plan.snapshot_seq.0, "rows", robot, out)?;
                out.flush().map_err(Failure::io)?;
                contexts.query().checkpoint().map_err(Failure::io)?;
                asupersync::fs::write(path, certificate.canonical_bytes()).await.map_err(Failure::io)
            })
        }
        _ => Err(Failure::usage(
            "unknown or missing subcommand; use fgdb help",
        )),
    }
}

/// Establish the durability premise for a copied directory without opening a
/// writer, reading keys, or publishing a new commit. Validate the complete
/// invocation before the first filesystem effect.
fn adopt(args: &[String], robot: bool, out: &mut impl Write) -> Result<(), Failure> {
    let [flag, directory] = args else {
        return Err(Failure::usage("adopt requires exactly --db <dir>"));
    };
    if flag != "--db" || directory.is_empty() {
        return Err(Failure::usage("adopt requires exactly --db <dir>"));
    }
    let runtime = fgdb::runtime_builder().build().map_err(Failure::io)?;
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let adopted = runtime
        .block_on(Database::adopt(&contexts.commit(), directory))
        .map_err(open_failure)?;
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"adopted","files":{},"directories":{}}}"#,
                adopted.files, adopted.directories
            ),
        )
    } else {
        writeln!(
            out,
            "adopted {directory}: {} file(s), {} director(ies) synced",
            adopted.files, adopted.directories
        )
        .map_err(Failure::io)
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
        // Keys in the value's canonical ascending order (fgdb-2jw3z).
        GraphValue::Map { keys, values } => format!(
            r#"{{"type":"map","value":{{{}}}}}"#,
            keys.iter()
                .zip(values.iter())
                .map(|(key, value)| Ok(format!("{}:{}", quoted(key), value_cell(value)?)))
                .collect::<Result<Vec<_>, Failure>>()?
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
        GraphValue::Map { keys, values } => format!(
            "{{{}}}",
            keys.iter()
                .zip(values.iter())
                .map(|(key, value)| {
                    let key: String = key.chars().flat_map(char::escape_default).collect();
                    Ok(format!("{key}: {}", human_value(value)?))
                })
                .collect::<Result<Vec<_>, Failure>>()?
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
    render_row_body(columns, &rendered, robot, out)?;
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"{kind}","seq":{seq},"count":{}}}"#,
                rendered.len()
            ),
        )
    } else {
        emit(out, &format!("{} row(s)", rendered.len()))
    }
}

/// The complete row body without a success/completion frame. Write-returning
/// uses this same encoding in a bounded private buffer before native commit.
fn render_row_body(
    columns: &[String],
    rendered: &[Vec<String>],
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
        for row in rendered {
            emit(
                out,
                &format!(r#"{{"v":1,"event":"row","cells":[{}]}}"#, row.join(",")),
            )?;
        }
        Ok(())
    } else {
        let mut widths: Vec<_> = columns.iter().map(|c| c.chars().count()).collect();
        for row in rendered {
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
        for row in rendered {
            emit(out, &line(row))?;
        }
        Ok(())
    }
}
