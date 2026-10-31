//! Embedded CLI. Every graph operation uses the public native engine;
//! symbol IDs remain explicit until the library supplies a durable catalog.
// Commit-pack reads nest the store's futures past the default recursion
// limit when this crate proves one Send (see fgdb-strata's lib.rs).
#![recursion_limit = "256"]
#![forbid(unsafe_code)]

mod buffered;
mod diff;
mod fnx;
mod import;
mod load;
mod parameter_file;
mod query_write;
mod refusal;
mod remote;
mod scrub;
mod search;
mod spill;
mod stream;
mod transaction;
mod weighted_path;
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
    r##"{"v":1,"event":"schema","events":{"invocation":["v","event"],"columns":["v","event","columns","statement","stream","seq"],"row":["v","event","cells","statement"],"diff_columns":["v","event","before","after","semantics","columns"],"change":["v","event","weight","cells"],"statement":["v","event","index","kind","view","basis","count","statements","name"],"progress":["v","event","rows","seq"],"result":["v","event","kind","seq","count","statements","basis","stream","before","after","changed_rows","inserted","retracted","snapshot_records","work_units","scratch_entries","records","objects","repaired","block_objects","files","directories","metadata_objects"],"error":["v","event","class","code","offset","diagnostics"],"scrub":["v","event","object","kind","state","reason"],"schema":["v","event","events","exit_codes","key_file","bindings","cell_types","result_kinds","transaction","streaming","remote","spilling","buffered","diff","analytics","weighted_paths","search","errors"]},"exit_codes":{"success":0,"usage":2,"query":3,"open":4,"io":5},"key_file":"Three nonempty lines of 64 hexadecimal characters: object-id key, security namespace, encryption key; # starts a comment. Keys are never printed. On Unix the file must be a regular file with no group or other permission bits (mode 0600), at most 65536 bytes.","bindings":"Repeat --label name=u64, --relation name=u64, --property name=u64 on each invocation; IDs are decimal unsigned 64-bit integers. --write-relation u64 defaults to 1. No implicit catalog.","cell_types":["null","bool","int","text","list","map","count","wideint","average","decimal","float","timestamp","bytes","date","localtime","time","localdatetime","duration","vertex","edge","path","vertices","edges"],"result_kinds":["created","written","rows","replayed","help","schema","loaded","committed","read_closed","rolled_back","diff","compacted","imported_csv","scrubbed","searched","adopted"],"transaction":{"steps":"Ordered --write/--query/--savepoint/--rollback-to/--release; each --param belongs to its preceding --write or --query step. Savepoint names are case-sensitive, a reused name shadows the older one until released, and an unknown name is a usage error; --rollback-to discards later steps' effects and their buffered records and rows, --release keeps effects. Statement indexes are one-based. Statement/columns/row records describe intermediate transaction-local workspaces, not durable historical snapshots. Only the final result records completion; unknown completion emits an error, never rolled_back. --rollback discards effects and rows.","optional_fields":"statement on columns/row, basis on result; count on query statements and write statements with RETURN, statements only on write statements, name only on savepoint, rollback_to and release statements","max_statements":64,"max_query_rows":100000,"max_buffered_output_bytes":16777216,"execution_budgets":"per native read or write program; buffered rows/output limits are transaction-wide, not execution byte-memory bounds"},"streaming":{"flag":"query --stream; incompatible with --buffered, --spill-dir and --certify-to","profile":"native single-vertex scan with leading vertex identity, or one-edge scan with leading edge/source identities; canonical order, supported filters and SKIP/LIMIT; temporal cuts supported; no eager fallback or spill","delivery":"columns includes stream=true and the exact selected seq; each row is flushed before pulling another; result with stream=true is emitted only at successful exhaustion; error or EOF without result means an incomplete result, even after rows","memory":"one encoded row, not a collected result; the native source may retain an entire decoded generation","optional_fields":"stream and seq on columns, stream on result; absent on ordinary eager reads"},"remote":{"diff":"remote diff --before <decimal-u64> --after <decimal-u64> [--stream] <read>; both endpoints required, reverse/equal/zero valid; default buffered, --stream uses server-configured ordered comparison; typed --param and TLS options are supported; no --read-mode, argument files or subscription/write flags","diff_delivery":"finite after-minus-before bag changes: diff_columns then flushed change records then result kind=diff; exact revisions, signed i128 weights, changed_rows and inserted/retracted occurrence totals are decimal strings; no seq substitute or privileged snapshot/work/scratch counters; only matching END and successful final flush complete delivery; late error or EOF leaves the prefix incomplete","diff_execution":"one fresh execution authority covers both endpoint queries and comparison; prepared client variants rebind a connection-owned read template; unsupported profiles or absent server scratch refuse without fallback; callback output failure cancels and drains; no retry, reconnect, durable result handle, subscription or resume","flag":"remote query --read-mode buffered|source|ordered; query-only; default buffered","modes":"buffered collects the server eager answer; source uses native flow-driven row or admitted aggregate cursors; ordered uses the server operator-owned bounded external order, DISTINCT and window profile before its first result row; unsupported modes or missing server scratch refuse without retry","delivery":"source and ordered flush columns with stream=true, then flush each row before another callback; seq is absent on columns because FGP supplies the effective query cut only in END; the final rows result carries stream=true, exact seq and the fully flushed count; a schema or row output failure cancels and drains the stream; a late refusal or EOF emits no successful result and leaves any earlier rows incomplete","memory":"the CLI retains only schema width and a delivery counter between callbacks; one decoded FGP frame and one encoded row can be resident; server source storage and its private scratch have separate ownership and limits","parameters":"uint:<decimal-u64> carries a full-width top-level unsigned operand, including temporal cut and SKIP/LIMIT; int and uint retain distinct types; clients supply no scratch paths or physical limits","prepared":"remote query --rows-file <file> prepares one read template on one connection, then binds each argument set and delivers its complete columns/row/result block in file order; all three --read-mode choices are supported, including callback delivery in buffered mode; each answer has its own current or explicitly selected sequence, not a transaction-wide snapshot; the first set supplies structural parameter types, and every set supplies all actual values; any refusal, invalidated handle, EOF or output failure stops the remaining sets without retry or reprepare; earlier completed answers remain complete","prepared_limits":"a typed JSON array of argument objects, or CSV with explicit --csv-types name=type,...; 1..=1024 sets, at most 1048576 input bytes and one conservative client frame envelope, fully decoded and admitted before credentials are read; --param cannot be combined with --rows-file; arguments remain resident but answers are delivered one frame and encoded row at a time; server execution and authority limits apply independently to each read","prepared_writes":"remote write --prepared-jsonl <file> prepares one write template, then executes each physical input record with fresh typed parameters on the same connection; each record is its own atomic autocommit, unlike atomic --rows-file; --write-mode stats is the default and refuses RETURN, while --write-mode returning admits native writes with RETURN; --write-mode requires --prepared-jsonl; preparation takes types from the first record without using its values as defaults; server scope and budgets apply to every execution","prepared_write_input":"one JSON object per physical line, with typed --param string values; blank lines and empty files refuse, CRLF and a final line without LF are accepted; only regular files, no --param, --rows-file or --csv-types; at most 1048576 raw bytes per record excluding LF and including CR, bounded JSON/value nodes and an independent encoded request frame cap; the first record and prepare envelope are validated before credentials or connection; later records are decoded only after the previous result flush; no whole-file argument bag or total file-size cap","prepared_write_delivery":"one flushed result per completed record in input order, kind=written for WriteCommitted or read_closed for no-effect ReadClosed, with seq, statements and records=1; returning mode first flushes columns with stream=true and each returned row, then includes count and stream=true on the result, including zero-row answers; the CLI retains one decoded frame and encoded row, not a collected answer; the server admits the complete bounded answer before committing; output cancellation stops delivery and cannot roll back effects; a later malformed record, refusal, type drift, invalidated handle, transport loss or output failure stops remaining input without reconnect, reprepare, fallback or retry; prior completed records remain durable; unconfirmed completion must not be replayed automatically; this is not a whole-file transaction or a resumable import"},"spilling":{"flag":"query --spill-dir <existing-directory>; incompatible with --stream, --certify-to and standalone CALL fnx; combine with --buffered for authenticated cold graph sources, bounded paths and native relational stages","profile":"native vertex and fixed-edge scans with direct local field, identity and path projections and local predicates, query-owned external ORDER BY, complete-row DISTINCT then SKIP/LIMIT; projected or hidden order keys and temporal cuts; numeric COUNT/SUM/AVG/MIN/MAX grouping with native computed keys/arguments, COUNT/SUM/AVG DISTINCT arguments, complete-group HAVING, computed scalar/list/map outputs, output RETURN DISTINCT, exact typed ORDER BY including hidden aggregates and pagination; with --buffered, native RETURN/WITH, UNWIND, admitted joins/probes, set operations and aggregate relations preserve every intermediate canonicalization, DISTINCT, order and page; one finite variable-length atom supports WALK/TRAIL/ACYCLIC/SIMPLE, zero-hop isolates, parallel routes, captured paths and scalar edge property maps evaluated at every hop; COLLECT retains its admitted native source or completed relational order; raw path collections without an occurrence-order proof, shortest/unbounded/mixed path atoms, collection-valued edge predicates and unsupported native instructions refuse without eager retry","limits":"--spill-memory-bytes defaults to 67108864 for one shared scratch pool; --spill-disk-bytes defaults to 1073741824 split equally across two append-only files for ordering/stages or three for grouping, including every intermediate and metadata write; --max-spill-rows defaults to 1000000 input/intermediate occurrences independent of final --max-result-rows; --max-sort-work defaults to 1000000000 cumulative additional decode, encoding, partition, reduction, distinct-argument, sort and copy work, separate from cumulative native --max-work-units; decimal u64 including zero, exhaustion refuses","delivery":"complete source and every relational stage, input expressions, group reduction, HAVING, qualified output expressions, output DISTINCT and sorting precede columns, including LIMIT 0; each child stage finishes before parent expressions and preserves its own selected page; canonical full grouping keys order aggregate output evaluation and break ordering ties; computed outputs run once and travel in scratch beside complete ranking cells; output DISTINCT keeps the first fully ranked complete group per visible equality class, then ranks representatives before pagination; argument DISTINCT uses typed membership, ignores null, distinguishes integer 1 from floating 1.0 and does not alter ordinary bag aggregates; hidden cells are removed before delivery; stream=true and exact seq reuse row-wise delivery; scratch retirement precedes final result; error or EOF without successful result is incomplete","memory":"encoded evaluation rows and private aggregate frames are capped at 1 MiB, including complete/projected aggregate output frames; resident groups, decoded aggregate and relational-stage rows, expression workspaces, projection copies and DISTINCT comparison frames use the shared scratch pool; argument DISTINCT externally sorts each required argument column within an accepted partition and retains one previous frame rather than a membership set; final result frames are decoded/rendered one at a time; without --buffered, decoded graph storage and one native source/computed input row may remain outside scratch; --buffered separately bounds graph admission, cache and source workspaces; final sorted-row decoding and output encoding remain outside the pools; native expressions continue the source work/scratch-entry budgets","scratch":"private query-owned files only, never fsynced or adopted as database state; success, refusal and drop retire only owned names; process death may leave a private directory that later invocations never reuse or sweep"},"buffered":{"flag":"query --buffered [--spill-dir <existing-directory>] for native reads, or query --buffered [--sealed] with standalone CALL fnx; incompatible with --stream and --certify-to; standalone CALL fnx cannot use --spill-dir","profile":"Native profile: without --spill-dir: native single-vertex scan with leading vertex identity or single-edge scan with leading edge/source identities, local filters, one-edge captures, canonical order, DISTINCT/ALL and SKIP/LIMIT; with --spill-dir: local field/identity/path output, hidden ordering keys, computed RETURN/WITH and admitted relational stages with every inner DISTINCT/order/page, numeric grouping and admitted COLLECT inputs; graph sources include fixed-hop joins and one bounded variable-length atom with WALK/TRAIL/ACYCLIC/SIMPLE, zero hops, full path captures, path length/node/edge lists and endpoint fields; every route preserves native multiplicity, scalar edge property maps filter each hop using native typed equality, and terminal endpoint filters do not prune transit; all profiles retain temporal cuts; shortest/unbounded paths, additional atoms or collection-valued edge predicates in the variable-length source, and other unsupported instructions refuse before storage opens, including LIMIT 0; standalone CALL fnx uses the separate analytics.buffered profile","limits":"--buffer-memory-bytes defaults to 67108864; --buffer-source-bytes defaults to 1073741824, including birth-patch refaults; root-byte ceiling min(1048576,memory/64), metadata reserves 16 times that ceiling plus bounded cache bookkeeping; --max-work-units separately bounds initial admission and cumulative source/native stage evaluation; with --spill-dir, graph and scratch pools keep separate allowances whose sum bounds combined accounted bytes; intermediate occurrences use --max-spill-rows and only final selected rows spend --max-result-rows; all append-only disk and cumulative external-work limits retain their scopes; decimal u64 including zero","delivery":"Native delivery: same stream=true/exact-seq row-flush contract; without spill, returned native rows stay charged through encoding and flush and later failures may follow rows; with spill, source and stage guards survive encoding and scratch writes, each child completes before parent expressions, and all stages, reduction, expressions, DISTINCT, ordering and final row admission finish before columns, including LIMIT 0; scratch retirement precedes final success; opening failures emit no columns; output failure or missing terminal result is incomplete","memory":"graph admission metadata, extent cache, scan heads, decoded source/evaluation workspaces and native source copies share the graph memory pool; history metadata scales with distinct versions, properties refault; external operators share the separately bounded scratch pool; source encoding and stage decoding/evaluation/encoding reserve before allocation, with exact frame-size admission before encoding; decoding, encoding, sorting, copying and pagination continue one --max-sort-work allowance across all passes; Chronicle recovery, native preparation/parameters and final CLI row decoding/transport workspace remain outside; no resident read-view fallback","checkpoint":"requires a current authenticated checkpoint; RecoveryRequired refuses, no automatic writable recovery; immutable object directory must remain available, no cross-process retention lease"},"diff":{"command":"diff --before <seq> --after <seq> <gql>; both endpoints required, reverse/equal/zero legal","semantics":"after_minus_before_bag: complete native result net changes in one admitted history; positive weight adds occurrences, negative retracts; not write events, ordering changes, cross-branch comparison or DIFF syntax","encoding":"diff_columns then canonical change records then result kind=diff; revisions, weights and all diff counters are decimal strings; cells retain native types","delivery":"both queries and consolidation finish before diff_columns; each change is flushed; only final result plus successful exit and no error establishes complete delivery; a write/flush error may leave a partial final frame","limits":"--max-snapshot-records, --max-result-rows, --max-work-units, --max-scratch-entries, also accepted by query (eager, --stream and --certify-to) and replay; decimal u64 including zero; defaults 100000/100000/10000000/10000000; cumulative across both queries and consolidation; result rows count changed tuples","output_bytes":"--max-output-bytes is a diff-only decimal u64 transport cap, default 16777216; counts UTF-8 diff frames including newlines and final result, excludes invocation/error records; each whole frame is admitted before writing; a refused frame may follow complete changes but never implies successful completion","memory":"endpoint and consolidated results are in memory; one encoded change at a time; neither output byte cap nor execution limits are spill or an allocator-byte bound","refusals":"explicit temporal selectors, writes, --stream, --certify-to and --certificate are not supported"},"analytics":{"command":"query 'CALL fnx.<procedure>(<args>) YIELD <output> [AS <alias>], ...' runs a registered Prism procedure over an explicit projection of one committed sequence","procedures":["pagerank","single_source_shortest_path_length","single_source_dijkstra_path_length","connected_components","weakly_connected_components","strongly_connected_components","triangles","clustering_coefficient","degree_centrality","closeness_centrality","harmonic_centrality","betweenness_centrality","eigenvector_centrality","core_number","cdlp","directed_clustering_coefficient","in_degree_centrality","out_degree_centrality","load_centrality","hits_centrality","articulation_points","bridges"],"projection":"Standalone CALL: --graph-label <label> and --graph-relation <relation> select induced vertices and edges (default all); --weight <property> reads edge weights (default unit) with --missing-weight reject|unit|zero (default reject); --direction directed|reversed|undirected (default directed); --parallel-edges reject|collapse|min|max|sum (default reject); --self-loops keep|drop|reject (default keep); --as-of <seq> selects a committed sequence (default frontier)","parameters":"--param name=vertex:<id>|int:<i64>|float:<f64>|bool:true|bool:false|null binds $name; literal arguments need no parameter","output":"columns, row and result kind=rows records; result seq is the analysed sequence; vertex and component cells are vertex identities, hop distances, triangle counts and core numbers are int, scores and weighted distances are float; hits_centrality returns vertex,hub,authority with both score columns as float; articulation_points returns vertex; bridges returns vertex,neighbor as full-width vertex identities in canonical projected endpoint pairs","refusals":"a projection that violates the procedure's graph laws, a parallel edge under reject, an unknown procedure or argument, and an unbound symbol are refused, never reshaped; standalone CALL supports neither --stream nor --certify-to; composed compressed reads obey the composed_sealed restrictions","composed":"a CALL followed by WHERE, WITH, MATCH, UNWIND or RETURN is an ordinary query whose YIELD columns feed that pipeline (MATCH (n) on a yielded vertex name is that vertex; n.p on a yielded vertex reads its property with no MATCH; YIELD node names the vertex output; a non-vertex output used as a vertex is refused at its YIELD position), over its query-local projection or the whole admitted graph by default; a first literal map accepts labels and relations (empty or singleton schema-name lists), weight, missing, direction, parallel and self_loops before ordinary algorithm arguments; schema names bind at preparation and the projection applies to the pinned snapshot or effective transaction overlay; it takes native query --param values and budgets; ordinary decoded composition accepts no CLI projection flags, while composed_sealed describes explicit compressed composition","composed_sealed":"query --sealed --graph-relation <relation> supports native set and relational-aggregate composition over compressed FnX cursors; each query-local FnX projection must explicitly select that same bound relation; labels, weights, missing-weight handling, direction, parallel-edge reduction and self-loop policy remain query-local, with no CLI graph-law overrides; --as-of <seq> selects the exact committed cut (default the pinned frontier); native --max-snapshot-records, --max-result-rows, --max-work-units and --max-scratch-entries govern the complete query, alongside sealed kernel/result/image/property/row/incidence/source-incidence caps; add both --buffered and --spill-dir <dir> for independent sealed CALLs feeding supported external filtering, property reads, fixed-hop MATCH, intermediate grouping, ordering, DISTINCT and paging; this explicit profile uses buffered graph reads and spilled intermediate rows, with no resident GQL snapshot, while compressed images and kernel vectors retain separate resident bounds; terminal COUNT/SUM/AVG/MIN/MAX/COLLECT retain native exact aggregate outputs and collection order; each source executes once outside correlated bodies, and all sources share cumulative budgets; procedures inside correlated bodies refuse in the external profile; --max-spill-rows bounds private inputs and --max-result-rows the final selected result; raw selected edge histories are admitted before compaction, cache hits recharge the complete construction receipt, and each kernel/result pass runs fresh; a native temporal selector and --as-of must agree; --stream, --certify-to, --sealed-quantum, --graph-staging-bytes and --graph-workspace-bytes refuse; without the buffered/spill pair, one current authenticated checkpoint supplies both the compressed relation and the resident native snapshot used by MATCH and property reads, through two source traversals with no writer lease or recovery writes; missing or lagging checkpoints require writer publication or explicit writable recovery; this is not an external-memory or RSS bound; the complete native result succeeds before columns, and result seq is the selected cut","sealed":"standalone query --sealed explicitly runs pagerank, single_source_shortest_path_length, single_source_dijkstra_path_length, connected_components, weakly_connected_components, strongly_connected_components, triangles, clustering_coefficient, directed_clustering_coefficient, cdlp, degree_centrality, core_number, eigenvector_centrality, closeness_centrality, harmonic_centrality and betweenness_centrality, load_centrality, hits_centrality, articulation_points and bridges through compressed cursors; articulation points and bridges require an undirected simple projection, ignore weights and self-loops, and use iterative lowlink with admitted workspace and cooperative checkpoints; degree, core, eigenvector, closeness, harmonic and betweenness require an undirected loop-free projection; eigenvector keeps two score vectors with the registered 100-round, 1e-6 convergence law; closeness and harmonic retain one FIFO, one distance vector and one score vector for exact repeated hop-distance traversals, with retained-history work admitted for every pass; betweenness keeps five vertex-sized vectors with exact source-ordered Brandes dependency sums and reached-only resets, refusing nonfinite shortest-path counts; HITS keeps three score vectors, ignores edge weights and preserves canonical outgoing-row sums for hub and authority updates; it accepts all explicit graph kinds, stops at hub L1 change below 1e-8 or 100 iterations, returns the last iterate at exhaustion and normalizes both final scores; Newman load centrality keeps five vertex-sized vectors and splits accumulated flow equally among shortest-path predecessors, normalized by (V-1)(V-2); CDLP uses synchronous rounds and minimum-identity ties over separately reduced incoming/outgoing votes, requiring a loop-free projection; directed clustering ignores loops and counts directed arcs among distinct incoming/outgoing neighbors; both account a bounded compressed opposite projection in kernel workspace; --graph-relation is required; unsupported procedures and graph kinds refuse before read-only open, with no decoded fallback; one current authenticated checkpoint is sealed in a single source pass without a writer lease or graph publication; missing or lagging checkpoints require writer publication or explicit writable recovery","sealed_limits":"For standalone CALL: --sealed-kernel-bytes and --sealed-result-bytes default to 67108864 each; --sealed-image-bytes defaults to 268435456; --sealed-property-bytes defaults to 67108864; --sealed-max-rows and --sealed-max-incidences default to 1000000 each; --sealed-source-incidences defaults to the larger of 1000000 and --sealed-max-incidences, bounding all raw root incidences without --buffered or selected-relation raw histories after complete buffered admission with --buffered; size limits are decimal platform-size integers including zero, each once and only with --sealed; --sealed-quantum requires a positive platform-size integer and opts into a supported cooperative compressed kernel, refusing unsupported cooperative algorithms before open; it bounds kernel traversal/scalar steps, not synchronous image/projection construction or end-to-end poll time; independent bounds cover kernel/result backing, image size and canonical properties, not allocator/RSS or CLI encoding; source staging and projection retain separate Prism admissions, so this is not a larger-than-memory seal pipeline; --buffered uses --buffer-memory-bytes, --buffer-source-bytes and --max-work-units for source admission and scans; native --max-snapshot-records, --max-result-rows and --max-scratch-entries refuse, and --max-work-units also refuses without --buffered; all engine refusals precede columns","projection_limits":"--graph-staging-bytes defaults to 1073741824 for selected source identities and edges; --graph-workspace-bytes defaults to 4294967296 for projection construction; decimal platform-size integers including zero, each once, only with standalone CALL fnx; physical buffered graph storage, source work and algorithm execution have separate admissions","buffered":"query --buffered with standalone CALL fnx reads authenticated checkpoint cursors and builds the selected projection; --sealed retains compressed adjacency and otherwise builds decoded CSR before the registered procedure; no resident snapshot or whole-database edge-property winner map is required; labels, relations and both selected endpoints precede weight lookup, retaining selected isolates, temporal cuts and explicit graph laws; missing or lagging checkpoints refuse without resident retry; --spill-dir is incompatible","buffered_limits":"graph admission, cache and scan storage use --buffer-memory-bytes; --buffer-source-bytes bounds authenticated source admission; --max-work-units separately bounds admission and each scan; Prism cumulative source work/scratch, --graph-staging-bytes, --graph-workspace-bytes and kernel/result admissions retain separate scopes; the selected CSR and complete analytics answer remain resident, so the graph pool is not an overall memory/RSS cap or an external-memory kernel; explicit native --max-snapshot-records, --max-result-rows and --max-scratch-entries flags refuse","delivery":"analytics finish before columns; one row is encoded at a time, with a separate width pass for human tables; ordinary columns/row/result framing is unchanged and only the final result completes delivery; output failure emits no successful result"},"weighted_paths":{"flag":"query --path-source <vertex-id> --path-target <vertex-id> [--as-of <seq>]; both full decimal 128-bit identities are required, bound in endpoint declaration order","profile":"native finite MATCH p = ANY CHEAPEST or CHEAPEST k with WALK/TRAIL/ACYCLIC/SIMPLE, relation and signed Int64 COST property; default MATCH DIFFERENT EDGES, explicit REPEATABLE ELEMENTS where the mode permits; K and hop bounds accept int:/uint: parameters","output":"RETURN path name or alias, followed by <alias>_cost; path cells retain every vertex/edge identity and cost cells are exact signed wideint; ascending cost then canonical path order; unreachable targets and K=0 return zero rows, with source validation retained for K=0","execution":"read-only local host; complete syntax and parameter binding before graph open; topology and costs use the same selected sequence; native snapshot/result/work/scratch limits apply; complete evaluation before columns; native paths/results remain resident, one row is encoded at a time","refusals":"no CALL fnx, projection flags, --sealed, --stream, --buffered, --spill-dir or --certify-to; no remote grammar change; unknown identities do not create vertices"},"search":{"command":"search --text <query> --text-property <property> and/or --vector <x,y,...> with one --vector-property <property> per coordinate (or --vector-encoding packed with one --vector-property holding packed little-endian f32 bytes, as --param e=vector:<x,y,...> writes), over one committed sequence","lanes":"text only: BM25 over the text property (--text-match any|all|phrase|fuzzy1|fuzzy2|fuzzy1-all|fuzzy2-all, default any; a fuzzy mode matches within that edit distance and expands to at most --max-expansions vocabulary terms, default 64, refusing beyond the bound); vector only: nearest neighbours over the numeric properties in order or one packed little-endian f32 byte property (--vector-encoding coordinates-rounded|coordinates|packed, default coordinates-rounded: stored numbers round once to the nearest f32, coordinates refuses any value f32 cannot hold exactly; --metric l2|cosine|dot, default l2; exact unless --ann <ef_search>); both: exact reciprocal-rank fusion of --candidates hits per lane (default --k), a fusion of two candidate sets, not an exhaustive hybrid answer; graph: --expand-from <vid,...> with an explicit --max-hops <n> expands from those seeds (--expand-relation <relation>, default every relation; --expand-direction out|in|both, default out; --include-seeds true|false, default false) and fuses as a third reciprocal-rank lane (--graph-candidates, default --candidates; --graph-weight, default 1) with any requested text/vector lanes; with neither, it is graph-only retrieval over topology at the same cut, with no property projection or index build","corpus":"--k bounds the hits (default 10); --vertex-label <label> restricts the corpus; --eligible <vid,...> restricts result endpoints before each candidate limit, preserving corpus statistics, ANN routing and graph transit (unsigned u128 IDs; at most 100000 input occurrences before deduplication; omission allows every endpoint, an empty value allows none); --as-of <seq> selects a committed sequence (default frontier); the index is built for the one call from that sequence","output":"columns, row and result kind=searched records; result seq is the searched sequence; text columns vertex,score; vector columns vertex,distance; both vertex,score,vector_rank,text_rank,vector_distance,text_score where score is an exact decimal, ranks are int and an absent lane value is null; with the graph lane: vertex,score,vector_rank,text_rank,graph_rank,vector_distance,text_score,graph_hops","refusals":"no lane, a lane flag without its lane, a vector whose length differs from its --vector-property count (or, packed, from the stored coordinate count), non-finite coordinates, --candidates without a fused search (both lanes or the graph lane), --max-expansions without a fuzzy mode, graph lane flags without --expand-from, --expand-from without --max-hops, and unbound symbols are usage errors; --param and --write-relation are not accepted"},"errors":{"codes":{"usage":"exit 2: the command line is wrong","syntax":"the statement does not parse; offset is the byte to fix","unsupported":"a recognized construct that is not supported yet","not_found":"a label, relation or property name in the statement has no binding","invalid_value":"a parameter or value is missing, unexpected, mistyped or out of range","statement":"the statement parses but its definition is refused; also every remote statement refusal","budget":"an execution limit was exceeded; a larger limit may succeed","permission":"the command or capability may not do this, such as a write given to read-only query","conflict":"a concurrent commit won; nothing was committed and a retry may succeed","execution":"refused while executing or delivering output","open":"exit 4: the database or key could not be opened","io":"exit 5: storage or the operating system failed"},"class":"code refines class: usage, open and io are their own codes; every other code has class query and exit 3","offset":"UTF-8 byte offset into the statement text, the whole script for a multi-statement write, or the step's own text in a transaction","optional_fields":"offset on error, only when the refusal names a byte; code is always present","diagnostics":"human-readable messages; their wording may change, codes do not"}}"##,
    "\n"
);
const HELP: &str = "fgdb - embedded graph database
Usage: fgdb [--robot] <command>
  create --db <dir> --key-file <file>
  adopt --db <dir>
  compact --db <dir> --key-file <file>
  scrub --db <dir> --key-file <file>
  write --db <dir> --key-file <file> [bindings] [--param name=value]... [--rows json:<objects>] <gql>
  query --db <dir> --key-file <file> [bindings] [--param name=value]... [limits] [--sealed [--buffered] | --stream | --buffered [--spill-dir <dir>] | --spill-dir <dir>] <gql>
  query --db <dir> --key-file <file> [bindings] [projection] [--param name=value]...
        'CALL fnx.<procedure>(<args>) YIELD <output> [AS <alias>], ...'
  import-csv --db <dir> --key-file <file> [bindings] --input <file.csv|-> [--types-file <file>]
             [--max-input-bytes N] [--max-changes N] (--query-file <file.gql|-> | <gql>)
  diff --db <dir> --key-file <file> [bindings] [--param name=value]... --before <seq> --after <seq> <gql>
  transaction --db <dir> --key-file <file> [bindings] --write <gql> --query <gql> ... [--rollback]
    (steps may include --savepoint <name>, --rollback-to <name>, --release <name>)
  replay --db <dir> --key-file <file> [bindings] [--param name=value]... [limits] --certificate <file>
  load --db <dir> --key-file <file> [bindings] --input <file.ndjson> [--rows-per-chunk N] [--checkpoint <file>]
       [--max-source-rows N] [--max-total-key-bytes N]
  search --db <dir> --key-file <file> [bindings] [text lane] [vector lane] [--k N]
         [--candidates N] [--vertex-label <label>] [--as-of <seq>]
  remote --addr <ip:port> --token-file <file> --database <name> [--param name=value]...
         [--read-mode buffered|source|ordered] query <gql>
  remote --addr <ip:port> --token-file <file> --database <name> [--param name=value]...
         diff --before <seq> --after <seq> [--stream] <gql>
  remote --addr <ip:port> --token-file <file> --database <name> --rows-file <file>
         [--csv-types name=type,...] [--read-mode buffered|source|ordered] query <gql>
  remote --addr <ip:port> --token-file <file> --database <name> [--param name=value]... [--if-current <seq>] write <gql>
  remote --addr <ip:port> --token-file <file> --database <name> --rows-file <file>
         [--csv-types name=type,...] write <gql>
  remote --addr <ip:port> --token-file <file> --database <name> --prepared-jsonl <file> [--write-mode stats|returning] write <gql>
  remote --addr <ip:port> --token-file <file> --database <name> [--param name=value]...
         [--max-batches N] subscribe <read>
  robot schema
  help
remote runs one statement on an fgdbd server over FGP, authenticated by the hex
capability token in --token-file (owner-only; minted by `fgdbd token`). Output and
exit codes are query/write's; the server's own name bindings apply. Remote
parameters: int:, uint:, float:, text:, bool:true|false, null, json:<array|object>.
remote query --read-mode buffered (default) collects the completed eager answer.
--read-mode source uses a native cursor; ordered uses operator-configured server
scratch to complete native ordering, DISTINCT and pagination before the first row.
Both stream and flush columns and each row; no collected client result or eager retry.
The query cut is available only in the final result; columns never substitute the
SELECT frontier. Only a successful terminal result completes a delivered prefix.
A failed columns or row flush cancels the server query and drains its terminal.
Read modes apply only to query; remote clients cannot set scratch paths or limits.
remote diff compares the complete read result at two explicit revisions in one
history. Positive signed weights add occurrences; negative weights retract them.
Reverse, equal and zero revisions are legal. The default is buffered comparison;
--stream selects the server's bounded ordered comparison using operator-owned
scratch. Unsupported profiles or missing scratch refuse without a buffered retry.
Columns and each signed change are flushed; only the matching final diff result
completes delivery. The final record carries both exact revisions and changed-row,
inserted-occurrence and retracted-occurrence counts, all decimal strings in robot
output. It never substitutes the SELECT frontier or fabricates snapshot/work stats.
A failed flush cancels and drains the comparison; no retry or resumable cursor is
created. --before, --after and --stream apply only to remote diff. It accepts the
same typed --param and TLS options as remote query, not argument files or read modes.
remote query --rows-file <file> prepares once and reuses one connection for every
argument set. Use a JSON array of objects whose values have the typed --param
spellings, or CSV with explicit --csv-types name=type,... (int64, uint64, int,
text, bool, null, float, decimal, bytes). The first set supplies structural types;
every execution binds its actual values. --param cannot combine with --rows-file.
Files allow 1..=1024 sets and at most 1048576 bytes, with a separate wire-size cap.
Every answer is flushed as a complete columns/row/result block in file order,
including buffered mode. Each block reports its own selected sequence; these
reads do not share a transaction snapshot. A refusal or output failure stops
the remaining sets without retry or reprepare; earlier complete answers remain valid.
remote write --rows-file executes the complete JSON/CSV input as ONE atomic batch.
remote write --if-current <seq> accepts one text write only if the selected
database is still at that exact revision. The server checks under the writer
lock before effects; a mismatch is conflict (exit 3), with nothing committed.
The revision must come from the same database incarnation. Any intervening commit
conflicts, including unrelated writes. RETURN and matching no-effect writes keep
their ordinary outcomes. No retry occurs; transport loss can still leave an
unknown write outcome. This flag cannot combine with argument files or prepared
JSONL input. Zero and full-width decimal u64 revisions are accepted.
remote write --prepared-jsonl <file> prepares once and reads one typed JSON object
per line. Each line is an independent atomic autocommit, with a flushed completion
before the next line is decoded. Earlier completed records stay durable if a later
line is invalid or refused. No retry, reconnect or reprepare; an unconfirmed write
must not be replayed automatically. The default --write-mode stats refuses RETURN.
Use --write-mode returning for a native write with RETURN: columns and rows are
flushed as they arrive, followed by the record's completion with count. The server
admits the complete bounded answer before committing; output cancellation stops
delivery, not the committed effects. --write-mode requires --prepared-jsonl.
Use typed strings, for example {\"name\":\"text:Ada\",\"age\":\"int:37\"}. Blank lines and
empty files refuse; CRLF and a final line without LF work. Only regular files are
accepted, with at most 1048576 bytes per line (excluding LF, including CR), bounded
JSON/value decoding and a separate request-frame cap. There is no whole-file size
cap or collected argument bag. --param, --rows-file and --csv-types cannot combine
with --prepared-jsonl. Robot output has one result per record: kind=written for a
commit or read_closed for no effects, with seq, statements and records=1. Returning
mode adds count and stream=true, including when the answer contains zero rows.
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
--param-file name=path reads one UTF-8 JSON array or object as that parameter,
equivalent to --param name=json:<contents>, without putting the data on argv.
It applies wherever --param does, including each transaction query/write step.
Files must be regular files; all parameter files together allow at most 4 MiB
per invocation. Native JSON/parameter limits and the transaction's 1 MiB input
limit still apply. Inline and file parameters cannot repeat a name in one scope.
Remote --param-file cannot combine with --rows-file or --prepared-jsonl.
write --rows json:[{...},...] binds the statement's parameters once per object
and commits every row as ONE atomic program (MERGE sees earlier rows).
--tzdb-file <file> supplies a pinned transition-table artifact on every invocation.
Bindings: repeat --label name=u64, --relation name=u64, --property name=u64.
Supply the same bindings on reopen; no implicit catalog or hashed names.
--bindings-file <file> adds bindings written once, one per line:
`label <name>=<u64>`, `relation <name>=<u64>` or `property <name>=<u64>`
(# comments and blank lines ignored, at most 1 MiB); fgdbd serve accepts the
same file per database. Flags and file must not repeat a name or an ID.
query --certify-to <file> saves a portable result certificate after emitting rows.
An existing <file> is refused (exit 2) before the query runs unless --overwrite is given.
query and replay take the same four limits as diff; a refusal names the limit it hit.
query --stream pulls and flushes one native row at a time. Use a single-vertex
scan with leading vertex identity, or a one-edge scan with leading edge/source identities.
Both use canonical order, supported filters and SKIP/LIMIT, including temporal cuts.
Unsupported plans refuse; no eager fallback. --stream cannot use --certify-to.
An error can follow delivered rows; only the terminal result marks a complete stream.
The stream pins decoded source state, not out-of-core storage or a resumable cursor.
query --buffered on its own streams the native single-vertex or one-edge profile
directly
from authenticated checkpoint objects through the extent cache. Identity-leading
output, local filters, canonical order, SKIP/LIMIT and temporal cuts are supported;
probes, joins, aggregates and alternate ordering refuse before opening storage.
--buffer-memory-bytes (default 67108864) bounds graph admission, cache, scan,
evaluation and retained native rows; --buffer-source-bytes (default 1073741824)
bounds admission reads including birth-patch refaults. The root byte ceiling is
min(1048576, memory/64); root metadata reserves 16 times that ceiling plus cache
bookkeeping. --max-work-units separately bounds initial admission, cumulative
source pulls and native evaluation. Opening failures emit no columns; later
read/evaluation failures can follow delivered rows.
Chronicle recovery, query preparation/parameters and CLI row encoding are
outside the graph-memory cap. A missing/currently lagging checkpoint refuses
without resident fallback. --buffered cannot combine with --stream or
--certify-to; only a final result completes delivery. Standalone CALL fnx
uses the separately bounded analytics profile described below.
Add --spill-dir to --buffered for external ORDER BY and DISTINCT on local
field, identity and path projections, including hidden sort keys. Computed
RETURN/WITH projections, arithmetic, lists, maps and filters run in native
stages, preserving every inner order, DISTINCT and SKIP/LIMIT boundary. Numeric
grouping supports computed inputs, COUNT/SUM/AVG(DISTINCT argument), HAVING,
computed outputs (including lists and maps) and output DISTINCT. Admitted
relational aggregate inputs and COLLECT preserve their native stage and
occurrence-order rules. Vertex, single-edge, fixed-hop and one finite
variable-length graph atom support temporal cuts with --spill-dir. Bounded
WALK/TRAIL/ACYCLIC/SIMPLE paths retain every route, including parallel edges,
zero-hop isolates, captures, length, node/edge lists and endpoint properties.
Scalar relationship property maps filter every traversed hop using native
typed equality. Terminal endpoint filters do not prune transit. Shortest or
unbounded paths, additional atoms or collection-valued edge predicates in the
variable-length source, and other unsupported instructions refuse before
storage opens, even under LIMIT 0.
Complete stage evaluation, grouping and sorting precede columns.
The graph and spill pools have separate allowances; their accounted total is
bounded by --buffer-memory-bytes plus --spill-memory-bytes. Source row guards
survive encoding and scratch writes. Input rows, final rows, native work and
additional external work retain their respective limits. Buffered input encoding
reserves scratch memory before allocation and checks the frame cap first; that
encoding work shares --max-sort-work with all stage decoding, encoding, sorting,
copying and pagination. Stage rows and expression workspaces use the scratch
pool; native expression work continues the source allowance without resets.
Ordinary sorted-row decoding and transport encoding remain outside both pools.
Scratch is retired on success and failure before the final success result.
query --spill-dir <existing-dir> externally orders native vertex/fixed-edge scans,
including property-only output, hidden ORDER BY keys, DISTINCT, SKIP/LIMIT and
temporal cuts. DISTINCT requires projected sort keys. Numeric grouping
uses partitioned scratch for count/sum/avg/min/max over vertex/fixed-edge inputs;
computed keys and arguments run through the native expression evaluator first.
HAVING and computed output expressions run on every qualified complete group
before typed ORDER BY and SKIP/LIMIT. Counts, wide sums and exact averages keep
their native types. Post-aggregate RETURN DISTINCT removes equal visible rows
before pagination. COUNT/SUM/AVG(DISTINCT argument) use external typed membership
sorting; NULL is ignored and ordinary aggregates still count all occurrences.
COLLECT and relational aggregate inputs currently refuse, as do
optional/variable-length joins and unsupported native instructions; no eager retry.
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
write, import-csv and transaction also accept --max-snapshot-records,
--max-result-rows, --max-work-units and --max-scratch-entries with the same defaults.
One allowance covers the WHOLE write --rows or CSV program, not each input record.
In a plain write program, --max-result-rows bounds cumulative selected rows even
when the program has no RETURN. It is not merely a printed-row limit.
Transaction execution allowances are per native read/write program; a RETURN or
query step is also capped by the remaining transaction-wide 100000-row buffer.
These flags do not raise effect/creation caps, input limits, the 64-statement
transaction limit or the 16 MiB output bound. Zero is a real limit, not unlimited.
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
--max-source-rows (default 1000000) bounds the rows of the whole input and
--max-total-key-bytes (default 67108864) the bytes of all its distinct keys,
counting a resumed prefix; both are decimal and a resume must repeat them.
compact rewrites the storage layout durably; query results are unchanged.
adopt needs no keys. Run it once after copying a database without syncing its files,
before the first write; it syncs all regular files, directories and the parent.
scrub verifies every capsule, repairs damaged redundancy in place, and re-reads the
current generation's blocks, vertex patches, manifest, root and root segments;
each damaged object is reported, and any loss exits 5 after the reports.
CALL fnx.* runs a registered Prism procedure (pagerank, single_source_shortest_path_length,
single_source_dijkstra_path_length, connected_components, weakly_connected_components,
strongly_connected_components, triangles, clustering_coefficient, hits_centrality,
articulation_points, bridges,
directed_clustering_coefficient, cdlp; and native undirected CSR centralities with self-loops refused:
degree_centrality, closeness_centrality, harmonic_centrality, betweenness_centrality,
eigenvector_centrality, load_centrality, core_number) over an EXPLICIT projection of one committed
sequence; nothing is silently reshaped:
  directed_clustering_coefficient counts directed arcs among the union of each
  vertex's incoming/outgoing neighbors, divided by d*(d-1); self-loops are ignored.
  hits_centrality returns vertex,hub,authority; it uses topology without edge
  weights, stops at hub L1 change <1e-8 or 100 rounds, and normalizes both scores.
  articulation_points returns vertex; bridges returns vertex,neighbor. Both read
  an undirected simple projection, ignore weights and self-loops, and preserve
  full-width identities. Bridge pairs are canonical and identify projected edges.
  --graph-label <label>, --graph-relation <relation>: induced selection (default all)
  --weight <property> [--missing-weight reject|unit|zero]: edge weights (default unit)
  --direction directed|reversed|undirected (default directed)
  --parallel-edges reject|collapse|min|max|sum (default reject)
  --self-loops keep|drop|reject (default keep); --as-of <seq> (default frontier)
  --graph-staging-bytes (default 1073741824): selected source identities and edges
  --graph-workspace-bytes (default 4294967296): projection construction workspace
Analytics parameters: vertex:<id>, int:<i64>, float:<f64>, bool:true|bool:false|null.
A projection that breaks a procedure's graph laws is refused, never converted.
Weighted paths use explicit endpoint identities on the local read-only query host:
  query --path-source <vertex-id> --path-target <vertex-id> [--as-of <seq>] \
    'MATCH p = ANY CHEAPEST WALK (s)-[e:ROAD*1..8]->(t) COST e.distance RETURN p AS route'
Both endpoint flags are required, each a full decimal 128-bit vertex identity in
pattern declaration order. Bind ROAD and distance with --relation/--property.
The existing finite weighted profile supports ANY CHEAPEST or CHEAPEST <k>,
WALK/TRAIL/ACYCLIC/SIMPLE and explicit MATCH REPEATABLE ELEMENTS; otherwise MATCH
uses DIFFERENT EDGES. Bounds and K accept ordinary int:/uint: --param arguments.
Costs are exact sums of signed Int64 edge properties, including negative weights;
missing or noninteger costs refuse. Ranking is by cost then canonical path order.
Output columns are the RETURN path name/alias and <alias>_cost, a signed wideint
cell without rounding. An unreachable target or K=0 produces zero rows; K=0 still
validates the source. --as-of selects both topology and costs from that sequence.
Native --max-snapshot-records/--max-result-rows/--max-work-units/--max-scratch-entries
limits apply. Execution finishes before columns; paths and results stay in memory.
Endpoint flags cannot combine with CALL fnx, projection, --sealed, --stream,
--buffered, --spill-dir or --certify-to. They do not extend the remote grammar.
query --buffered runs standalone CALL fnx through authenticated checkpoint cursors,
then builds the selected projection and executes the registered procedure. With
--sealed it retains compressed adjacency; otherwise it builds decoded CSR. Neither
requires a resident snapshot or a whole-database edge-property winner map.
Labels, relations and both selected endpoints are checked before reading weights;
selected isolates, temporal cuts and explicit graph laws retain their meaning.
Graph admission, cache and scan storage use --buffer-memory-bytes;
--buffer-source-bytes bounds authenticated source admission, and --max-work-units
separately bounds admission and each scan. Prism's cumulative source work/scratch
allowances, --graph-staging-bytes, --graph-workspace-bytes and kernel/result
admissions are separate. The selected projection and analytics answer remain resident;
the graph pool is not an overall memory/RSS cap or an external-memory kernel.
All analytics finish before columns; output encoding retains one row at a time
(human output makes a width pass first). A final result completes delivery.
The native --max-snapshot-records, --max-result-rows and --max-scratch-entries flags
are refused for buffered analytics. --spill-dir cannot be combined with this
profile; missing or lagging checkpoints refuse without resident retry.
Standalone query --sealed runs pagerank, both single-source distance procedures, connected,
weak and strong components, triangles, clustering_coefficient,
directed_clustering_coefficient, cdlp, degree_centrality, core_number,
eigenvector_centrality, closeness_centrality, harmonic_centrality,
betweenness_centrality, load_centrality, hits_centrality, articulation_points and
bridges through compressed cursors. Cut analysis uses iterative lowlink with
admitted integer workspace and cursor backing bounded by actual traversal depth.
HITS retains three score vectors and traverses outgoing rows for both score updates;
directed, reversed and undirected projections preserve their explicit meaning.
Degree, core, eigenvector,
closeness, harmonic and betweenness require an undirected loop-free projection.
Core peeling keeps four integer vectors and opens each row once. Eigenvector
iteration keeps two score vectors
with the registered 100-round, 1e-6 convergence law. Closeness and harmonic reuse
one FIFO, one distance vector and one score vector for exact repeated hop-distance
traversals; each pass admits retained history and yields in cooperative execution.
Betweenness keeps five vertex-sized vectors and exact source-ordered Brandes
dependency sums; only reached vertices are reset between sources. Nonfinite
shortest-path counts refuse before producing scores.
CDLP uses synchronous rounds, minimum-identity tie breaking, and separately reduced
incoming/outgoing votes; it requires a loop-free projection. Directed clustering
ignores loops and counts directed arcs among distinct incoming/outgoing neighbors.
Both use vertex-sized counters plus a bounded compressed opposite projection;
its construction peak counts against --sealed-kernel-bytes.
These kernels retain no decoded adjacency. --graph-relation is required; unsupported procedures
refuse before storage opens.
This mode reads the current authenticated checkpoint without a writer lease and
builds its temporary image and vertex directory in one source pass. A missing or
lagging checkpoint requires writer publication or explicit writable recovery.
It does not publish the image or commit a graph mutation.
  --sealed-kernel-bytes and --sealed-result-bytes default to 67108864 each.
  --sealed-image-bytes defaults to 268435456; --sealed-property-bytes to 67108864.
  --sealed-max-rows and --sealed-max-incidences default to 1000000 each.
  --sealed-source-incidences bounds raw retained histories before compaction;
    its default is the larger of 1000000 and --sealed-max-incidences.
    Without --buffered it covers the complete root; with --buffered it covers
    the selected relation after the complete buffered source admission.
All are decimal platform-size integers, including zero; exhaustion refuses before
columns. These separate bounds account kernel/result backing, image bytes and
canonical property bytes, not allocator/RSS or CLI output encoding. Source
preparation may retain decoded storage; source staging and the projection directory
keep their separate Prism admissions. This is not a larger-than-memory seal pipeline.
With --buffered, source frames use --buffer-memory-bytes and source admission uses
--buffer-source-bytes; --max-work-units bounds source admission and each scan.
--sealed-quantum <positive integer> opts into a supported cooperative compressed
kernel and projection preparation, yielding between bounded traversal/scalar steps.
Unsupported cooperative procedures refuse before opening storage. Source admission,
vertex selection and image construction still contain synchronous stages; the quantum
is not an end-to-end poll-time bound.
The native --max-snapshot-records, --max-result-rows, --max-work-units and
--max-scratch-entries flags are refused with standalone --sealed; --buffered permits
only --max-work-units from that list. Use explicit --sealed-* limits for the kernel.
A CALL followed by WHERE, WITH, MATCH, UNWIND or RETURN is an ordinary query: its YIELD
columns feed the pipeline (MATCH (n) on a yielded vertex name is that vertex), over the
whole graph by default. A first literal map selects its graph, before ordinary arguments:
  {labels:['Person'], relations:['KNOWS'], weight:'weight', missing:'reject',
   direction:'directed', parallel:'sum', self_loops:'drop'}
labels/relations accept [] or one bound schema name. Defaults: procedure direction,
unit weights, parallel reject, self_loops keep. The map narrows the admitted snapshot
or staged transaction graph; algorithm parameters retain their ordinary meanings.
Composed calls take query --param values. YIELD node names
the vertex output, and n.p on
a yielded vertex reads its property with no MATCH; a non-vertex output used as a
vertex is refused at its YIELD position.
Add --sealed --graph-relation <name> to execute a composed CALL through compressed
analytics. Every CALL must select that same relation in its query-local projection;
the flag never supplies an omitted relation. RETURN, WHERE, MATCH, ORDER BY, paging,
set composition and relational aggregation retain native GQL semantics and budgets.
--as-of pins the complete query, including pattern and property reads, to one cut.
The read-only host opens one current authenticated generation beside a live writer.
It retains both a compressed relation and a resident native snapshot for GQL, with
separate source traversals; this profile is not a larger-than-memory query engine.
Native --max-snapshot-records, --max-result-rows, --max-work-units and
--max-scratch-entries limits apply alongside the --sealed-* memory/source limits.
Add BOTH --buffered and --spill-dir <dir> to compose independent sealed CALLs
with supported native external stages, including property reads, fixed-hop
MATCH, intermediate grouping, terminal COUNT/SUM/AVG/MIN/MAX/COLLECT, ordering,
DISTINCT and paging. Terminal groups retain wide sums, exact averages and native
collection order. The graph source uses authenticated buffered reads; rows spill instead of building
a resident GQL snapshot. The compressed image and algorithm vectors remain
resident under their separate --sealed-* limits. Native query budgets include
projection construction, cache hits, kernel execution and complete result work;
--max-spill-rows bounds private inputs and --max-result-rows the final result.
Each source executes once under cumulative budgets; procedures inside correlated
bodies refuse. Native temporal selectors and --as-of select one exact cut;
supplying both requires them to agree.
All evaluation completes before columns. Composed --sealed refuses --stream,
--certify-to, --sealed-quantum, --graph-staging-bytes and --graph-workspace-bytes.
Labels, weights, direction, parallel-edge and self-loop policies belong in the
query-local projection, with no command-line overrides.
search runs Beacon retrieval over an EXPLICIT projection of one committed sequence:
  text lane: --text <query> --text-property <property> [--text-match any|all|phrase]
    or --text-match fuzzy1|fuzzy2|fuzzy1-all|fuzzy2-all within that edit distance,
    expanding to at most --max-expansions vocabulary terms (default 64; beyond refuses)
  vector lane: --vector <x,y,...> with one --vector-property <property> per coordinate,
    in order [--metric l2|cosine|dot] [--ann <ef_search>] (default exact). Stored numbers
    round once to the nearest f32 (--vector-encoding coordinates-rounded, the default);
    --vector-encoding coordinates refuses any value f32 cannot hold exactly; or
    --vector-encoding packed with ONE --vector-property holding packed f32 bytes
    (as --param e=vector:<x,y,...> writes; the vector length must match)
  both lanes: exact reciprocal-rank fusion of --candidates hits per lane (default --k);
    a fusion of the two candidate sets, not an exhaustive hybrid answer
  graph lane: --expand-from <vid,...> with an explicit --max-hops <n> expands from those seeds
    [--expand-relation <relation>] [--expand-direction out|in|both]
    [--include-seeds true|false] [--graph-candidates N]
    [--graph-weight N], fused as a third lane with the text and/or vector lanes
  --k bounds the hits (default 10); --vertex-label restricts the corpus.
  --eligible <vid,...> restricts endpoints before every candidate limit; omitted means all,
    an empty string means none. Up to 100000 IDs, including duplicates; unsigned u128.
    Excluded endpoints still contribute corpus statistics and may carry graph/ANN paths.
The index is built for the one call from that sequence. Columns: text vertex,score;
vector vertex,distance; both vertex,score,vector_rank,text_rank,vector_distance,text_score;
with the graph lane also graph_rank and graph_hops.
transaction executes ordered --write/--query steps in one native transaction.
Each --param belongs to the preceding step; parameter maps do not leak between steps.
Success commits once; --rollback discards all effects and results. Errors abort before commit.
Query rows describe each transaction-local workspace, not a durable historical snapshot.
CALL hybrid.search accepts eligible => [vertex, ...] or a list parameter in transaction
queries as well as ordinary reads; filtering observes the same staged graph and properties.
Output is buffered until completion: at most 64 native statements, 100000 query rows,
and 16 MiB encoded output. Execution budgets remain per statement/program, not byte-memory bounds.
--write-relation u64 selects the native mutation coordinate (default 1).
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
write --from-query '<read RETURN ...>' '<write script>' evaluates the source
once and binds each projected row by name to the script's parameters. --param
binds the SOURCE only; project a global value to pass it into the script.
--feed-type name=type declares a SCRIPT parameter: int64, uint64, int, text,
bool, null, float, decimal, bytes, list or map. Undeclared script parameters retain
native inference (usually int64); no value-based inference or scalar coercion occurs.
Collections retain native bounds; a NULL collection stays NULL, not an empty value.
--max-batch-statements bounds the TOTAL expanded statements, not each row,
and cannot exceed the native batch ceiling. Zero permits only empty input.
One transaction and one source/work/scratch/selection allowance cover the read,
binding and all script instances. Duplicate rows remain separate invocations;
later MERGEs see earlier creates, but the source is never rerun. Empty input
closes read-only without a marker. --rows and script RETURN are not supported
in this profile; query and remote reject these flags.
Only after standalone completion does kind=written report seq, records and statements.
transaction --write '<script>' --from-query '<read>' uses the preceding write step.
Its --param values bind that step's source; --feed-type declarations do not leak.
Sources read the current staged graph, and savepoints rewind effects and output.
One source statement plus every expanded script statement counts toward the
existing 64-statement invocation cap, even after savepoint rollback. Batch limits
are intersected with remaining capacity; an oversized source refuses, not truncates.
Source text and feed flags count toward the transaction's 1 MiB input allowance.
Only the final transaction result completes delivery; source rows are not output.
This is bounded in-memory composition, not streaming input or a byte-memory cap.
Robot stdout: versioned NDJSON; an error carries a stable code (robot schema
errors.codes), the statement byte offset when one is known, and messages.
Exit codes: 0 success, 2 usage/schema, 3 query refusal, 4 open/key, 5 I/O/corruption.
";

#[derive(Debug)]
struct Failure {
    code: u8,
    class: &'static str,
    /// The robot `code`. A query failure the CLI cannot classify is an
    /// execution refusal; the other classes are their own code.
    refusal: refusal::Code,
    /// The statement byte the refusal points at, when it names one.
    offset: Option<usize>,
    message: String,
}
impl Failure {
    fn new(code: u8, class: &'static str, message: impl ToString) -> Self {
        let refusal = match class {
            "usage" => refusal::Code::Usage,
            "open" => refusal::Code::Open,
            "io" => refusal::Code::Io,
            _ => refusal::Code::Execution,
        };
        Self {
            code,
            class,
            refusal,
            offset: None,
            message: message.to_string(),
        }
    }
    fn usage(message: impl ToString) -> Self {
        Self::new(2, "usage", message)
    }
    fn query(message: impl ToString) -> Self {
        Self::new(3, "query", message)
    }
    /// A query refusal with a known code and no statement byte.
    fn coded(refusal: refusal::Code, message: impl ToString) -> Self {
        Self {
            refusal,
            ..Self::query(message)
        }
    }
    /// A parameter value its type cannot hold.
    fn invalid_value(error: impl ToString) -> Self {
        Self::coded(refusal::Code::InvalidValue, error)
    }
    /// A typed refusal of a statement before execution: its code and byte
    /// come from its source chain, or it is a refused statement definition.
    fn statement(error: impl std::error::Error + 'static) -> Self {
        let (refusal, offset) =
            refusal::classify(&error).unwrap_or((refusal::Code::Statement, None));
        Self {
            refusal,
            offset,
            ..Self::query(error)
        }
    }
    /// Name the flag that raises an execution-limit refusal. Only for a
    /// command that accepts those flags (`query`, `replay`, `diff`): the
    /// others run under fixed limits, and a `remote` refusal may be the
    /// server's limit or the capability's, neither a client flag.
    fn with_budget_hint(mut self) -> Self {
        if self.class == "query"
            && let Some(flag) = budget_flag(&self.message)
        {
            self.message.push_str(&format!(" (raise with {flag})"));
        }
        self
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
            eprintln!("fgdb: error[{}]: {}", error.refusal.name(), error.message);
            if robot {
                // Robot error events carry the message too; diagnostics never
                // include key material because messages never format keys.
                let offset = error
                    .offset
                    .map_or_else(String::new, |offset| format!(r#","offset":{offset}"#));
                let _ = emit(
                    &mut out,
                    &format!(
                        r#"{{"v":1,"event":"error","class":"{}","code":"{}"{offset},"diagnostics":[{}]}}"#,
                        error.class,
                        error.refusal.name(),
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
    param_files: Vec<(String, PathBuf)>,
    /// `write --rows json:[...]`: one parameter set per object.
    rows: Option<String>,
    /// `write --from-query`: a frozen read feeds one atomic native batch.
    query_write: query_write::QueryWriteOptions,
    tzdb_file: Option<PathBuf>,
    /// The `--tzdb-file` artifact as the statement's named-zone capability,
    /// once it is read (fgdb-txbg9).
    zones: Option<fgdb_gql::GqlTimeZones>,
    labels: BTreeMap<String, u64>,
    relations: BTreeMap<String, u64>,
    properties: BTreeMap<String, u64>,
    /// `--bindings-file`: more bindings, merged in before any name resolves.
    bindings_file: Option<PathBuf>,
    coordinate: RelationId,
    certify_to: Option<PathBuf>,
    /// `--overwrite`: `--certify-to` may replace an existing certificate.
    overwrite: bool,
    certificate: Option<PathBuf>,
    input: Option<PathBuf>,
    rows_per_chunk: Option<usize>,
    /// `load` whole-source bounds; absent ones keep the engine defaults.
    max_source_rows: Option<usize>,
    max_total_key_bytes: Option<usize>,
    checkpoint: Option<PathBuf>,
    steps: Vec<transaction::Step>,
    rollback: bool,
    stream: bool,
    buffered: buffered::BufferedOptions,
    spill: spill::SpillOptions,
    /// Native read/write execution limits; absent ones keep the CLI defaults.
    budget: QueryBudget,
    diff: diff::DiffOptions,
    csv: import::CsvOptions,
    /// `query` text is a `CALL fnx.*` Prism call, answered by `fnx::run`.
    fnx_call: bool,
    fnx: fnx::ProjectionFlags,
    /// Explicit typed endpoints for the native weighted-path text profile.
    weighted_path: weighted_path::PathFlags,
    /// `search` lanes and corpus, answered by `search::run`.
    search: search::SearchFlags,
}
impl Options {
    fn resolve(&self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match kind {
            GraphSymbolKind::Label => self
                .labels
                .get(name)
                .map(|id| GraphSymbol::Label(LabelId(*id))),
            GraphSymbolKind::Relation => self
                .relations
                .get(name)
                .map(|id| GraphSymbol::Relation(RelationId(*id))),
            GraphSymbolKind::Property => self
                .properties
                .get(name)
                .map(|id| GraphSymbol::Property(PropertyKeyId(*id))),
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
            catalog.insert_label(LabelId(id), name.clone());
        }
        for (name, &id) in &self.relations {
            catalog.insert_relation(RelationId(id), name.clone());
        }
        for (name, &id) in &self.properties {
            catalog.insert_property(PropertyKeyId(id), name.clone());
        }
        Some(catalog)
    }

    fn reverse_label(&self, id: LabelId) -> Option<String> {
        self.labels
            .iter()
            .find_map(|(name, &raw)| (raw == id.0).then(|| name.clone()))
    }

    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.relations
            .iter()
            .find_map(|(name, &raw)| (raw == id.0).then(|| name.clone()))
    }

    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        self.zones.clone()
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

    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        (*self).time_zones()
    }
}
/// The flag that raises the execution limit a refusal names, read from the
/// typed refusals' Display spelling (`GqlBudgetExceeded`, `GlaLimitExceeded`).
fn budget_flag(message: &str) -> Option<&'static str> {
    [
        (
            "GQL SnapshotRecords budget exceeded",
            "--max-snapshot-records",
        ),
        ("GQL ResultRows budget exceeded", "--max-result-rows"),
        ("GLA WorkUnits limit exceeded", "--max-work-units"),
        ("GLA ScratchEntries limit exceeded", "--max-scratch-entries"),
    ]
    .into_iter()
    .find_map(|(refusal, flag)| message.contains(refusal).then_some(flag))
}

/// One binding, from a flag or a bindings file: names and IDs are unique per kind.
fn bind_symbol(map: &mut BTreeMap<String, u64>, name: &str, id: u64) -> Result<(), Failure> {
    if name.is_empty() || map.contains_key(name) || map.values().any(|old| *old == id) {
        return Err(Failure::usage(
            "bindings must have unique names and IDs per kind",
        ));
    }
    map.insert(name.to_owned(), id);
    Ok(())
}

fn symbol_ordinal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// A bindings file's bytes, at most one past the parser's ceiling, so a
/// mistaken path to a huge file is refused without reading it whole. A FIFO
/// or device is refused BEFORE it is opened: opening a FIFO blocks until a
/// writer appears, so the handle check below would never run.
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

/// Merge `--bindings-file` into the flag bindings under the same uniqueness rule.
async fn read_bindings_file(
    options: &mut Options,
    cx: &fgdb_types::QueryCx,
) -> Result<(), Failure> {
    let Some(path) = options.bindings_file.clone() else {
        return Ok(());
    };
    cx.checkpoint().map_err(Failure::io)?;
    let bytes = read_bindings_bytes(&path).await?;
    let bindings = fgdb_gql::symbol_bindings::parse_symbol_bindings(&bytes)
        .map_err(|error| Failure::usage(error.to_string()))?;
    for binding in bindings {
        let map = match binding.kind {
            GraphSymbolKind::Label => &mut options.labels,
            GraphSymbolKind::Relation => &mut options.relations,
            GraphSymbolKind::Property => &mut options.properties,
        };
        bind_symbol(map, &binding.name, binding.id).map_err(|_| {
            Failure::usage(format!(
                "bindings file line {}: bindings must have unique names and IDs per kind",
                binding.line
            ))
        })?;
    }
    Ok(())
}

fn parse(args: &[String], command: &str) -> Result<Options, Failure> {
    let create = command == "create";
    let mut db = None;
    let mut key = None;
    let mut text = None;
    let params = GqlParameters::new();
    let mut raw_params = Vec::new();
    let mut param_files = Vec::new();
    let mut rows = None;
    let mut query_write = query_write::QueryWriteOptions::default();
    let mut tzdb_file = None;
    let mut labels = BTreeMap::new();
    let mut relations = BTreeMap::new();
    let mut properties = BTreeMap::new();
    let mut bindings_file = None;
    let mut coordinate = RelationId(1);
    let mut certify_to = None;
    let mut certificate = None;
    let mut input = None;
    let mut rows_per_chunk = None;
    let mut max_source_rows = None;
    let mut max_total_key_bytes = None;
    let mut checkpoint = None;
    let mut steps: Vec<transaction::Step> = Vec::new();
    let mut rollback = false;
    let mut stream = false;
    let mut overwrite = false;
    let mut buffered = buffered::BufferedOptions::default();
    let mut spill = spill::SpillOptions::default();
    let mut budget = QueryBudget::default();
    let mut diff = diff::DiffOptions::default();
    let mut csv = import::CsvOptions::default();
    let mut fnx = fnx::ProjectionFlags::default();
    let mut weighted_path = weighted_path::PathFlags::default();
    let mut as_of = None;
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
        if arg == "--overwrite" {
            if command != "query" || overwrite {
                return Err(Failure::usage(
                    "--overwrite is allowed once, only on query --certify-to",
                ));
            }
            overwrite = true;
            continue;
        }
        if arg == "--sealed" {
            if command != "query" {
                return Err(Failure::usage("--sealed is allowed only on query"));
            }
            fnx.enable_sealed()?;
            continue;
        }
        if arg == "--buffered" {
            if command != "query" {
                return Err(Failure::usage("--buffered is allowed only on query"));
            }
            buffered.enable()?;
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
                    if matches!(
                        command,
                        "query" | "replay" | "write" | "import-csv" | "transaction"
                    ) =>
                {
                    budget.set(arg, value)?;
                }
                flag if command == "query" && spill::SpillOptions::FLAGS.contains(&flag) => {
                    spill.set(flag, value)?;
                }
                flag if command == "query" && buffered::BufferedOptions::FLAGS.contains(&flag) => {
                    buffered.set(flag, value)?;
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
                "--max-source-rows" if command == "load" && max_source_rows.is_none() => {
                    max_source_rows = Some(decimal_usize(arg, value)?);
                }
                "--max-total-key-bytes" if command == "load" && max_total_key_bytes.is_none() => {
                    max_total_key_bytes = Some(decimal_usize(arg, value)?);
                }
                "--rows" if command == "write" && rows.is_none() => {
                    rows = Some(value.clone());
                }
                flag if matches!(command, "write" | "transaction")
                    && query_write::QueryWriteOptions::FLAGS.contains(&flag) =>
                {
                    if command == "transaction" {
                        steps
                            .last_mut()
                            .ok_or_else(|| {
                                Failure::usage("transaction query-fed flags must follow --write")
                            })?
                            .set_query_write_flag(flag, value)?;
                    } else {
                        query_write.set(flag, value)?;
                    }
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
                "--param-file"
                    if matches!(
                        command,
                        "query" | "write" | "replay" | "diff" | "transaction"
                    ) =>
                {
                    let file = parameter_file::parse(value)?;
                    let target = if command == "transaction" {
                        &mut steps
                            .last_mut()
                            .filter(|step| step.takes_params())
                            .ok_or_else(|| {
                                Failure::usage(
                                    "transaction --param-file must follow --query or --write",
                                )
                            })?
                            .param_files
                    } else {
                        &mut param_files
                    };
                    target.push(file);
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
                        .ok_or_else(|| Failure::usage("expected binding name=u64"))?;
                    let id = symbol_ordinal(raw)
                        .ok_or_else(|| Failure::usage("binding ID must be a decimal u64"))?;
                    let map = match arg.as_str() {
                        "--label" => &mut labels,
                        "--relation" => &mut relations,
                        _ => &mut properties,
                    };
                    bind_symbol(map, name, id)?;
                }
                "--bindings-file" if bindings_file.is_none() => {
                    bindings_file = Some(PathBuf::from(value));
                }
                "--write-relation"
                    if !matches!(command, "diff" | "compact" | "scrub" | "search") =>
                {
                    let id = symbol_ordinal(value)
                        .ok_or_else(|| Failure::usage("write relation must be a decimal u64"))?;
                    coordinate = RelationId(id);
                }
                "--as-of" if command == "query" => {
                    if as_of.replace(value.clone()).is_some() {
                        return Err(Failure::usage("--as-of must be supplied only once"));
                    }
                }
                flag if command == "query" && weighted_path::PathFlags::FLAGS.contains(&flag) => {
                    weighted_path.set(flag, value)?;
                }
                flag if command == "query" && fnx::ProjectionFlags::FLAGS.contains(&flag) => {
                    fnx.set(flag, value)?;
                }
                flag if command == "search" && search::SearchFlags::FLAGS.contains(&flag) => {
                    search_flags.set(flag, value)?;
                }
                _ => {
                    return Err(Failure::usage(format!(
                        "unknown, duplicate, or inapplicable flag {arg} for {command}"
                    )));
                }
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
    query_write.validate(rows.is_some())?;
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
    parameter_file::validate_names(
        &param_files,
        raw_params.iter().map(|(name, _)| name.as_str()),
    )?;
    for step in &steps {
        parameter_file::validate_names(
            &step.param_files,
            step.raw_params.iter().map(|(name, _)| name.as_str()),
        )?;
    }
    if command == "diff" {
        diff.validate()?;
    }
    if stream && certify_to.is_some() {
        return Err(Failure::usage(
            "--stream cannot be combined with --certify-to",
        ));
    }
    if overwrite && certify_to.is_none() {
        return Err(Failure::usage(
            "--overwrite only applies to query --certify-to",
        ));
    }
    let mut fnx_call = command == "query" && text.as_deref().is_some_and(fnx::is_call);
    weighted_path.validate()?;
    // Classify the shared sequence flag after every endpoint has been read;
    // --as-of has identical meaning regardless of its position on the line.
    if let Some(value) = as_of {
        if weighted_path.enabled() {
            weighted_path.set("--as-of", &value)?;
        } else {
            fnx.set("--as-of", &value)?;
        }
    }
    if weighted_path.enabled()
        && (fnx_call
            || !fnx.is_empty()
            || stream
            || buffered.enabled()
            || spill.enabled()
            || certify_to.is_some())
    {
        return Err(Failure::usage(
            "weighted paths cannot combine endpoint flags with CALL fnx, projection, \
             --sealed, --stream, --buffered, --spill-dir or --certify-to",
        ));
    }
    fnx.validate()?;
    if fnx_call
        && !buffered.enabled()
        && budget != QueryBudget::default()
        && text
            .as_deref()
            .is_some_and(fgdb_gql::starts_fnx_graph_projection)
    {
        // These named calls previously used the native query host, whose
        // limits have different domains from Prism's projection/kernel caps.
        // Keep that host when requested, or refuse an incompatible flag mix.
        // Buffered analytics instead checks its supported limits in prepare:
        // work bounds physical source admission/scans; other native caps refuse.
        if !fnx.is_empty() {
            return Err(Failure::usage(
                "native query limits cannot be combined with projection or --sealed flags \
                 on a named CALL fnx; remove those flags to retain native limit enforcement",
            ));
        }
        fnx_call = false;
    }
    buffered.validate()?;
    if buffered.enabled() && (stream || certify_to.is_some()) {
        return Err(Failure::usage(
            "--buffered cannot be combined with --stream or --certify-to",
        ));
    }
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
        fnx.validate_composed()?;
        if stream || certify_to.is_some() {
            return Err(Failure::usage(
                "composed --sealed queries cannot combine --stream or --certify-to",
            ));
        }
        if buffered.enabled() != spill.enabled() {
            return Err(Failure::usage(
                "composed buffered --sealed queries require both --buffered and --spill-dir",
            ));
        }
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
        param_files,
        rows,
        tzdb_file,
        zones: None,
        labels,
        relations,
        properties,
        bindings_file,
        coordinate,
        certify_to,
        overwrite,
        certificate,
        input,
        rows_per_chunk,
        max_source_rows,
        max_total_key_bytes,
        checkpoint,
        steps,
        rollback,
        stream,
        buffered,
        spill,
        budget,
        query_write,
        diff,
        csv,
        fnx_call,
        fnx,
        weighted_path,
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
        CanonicalScalar::ucs_basic_text(value).map_err(Failure::invalid_value)?
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
        CanonicalScalar::bytes(bytes).map_err(Failure::invalid_value)?
    } else if let Some(value) = raw.strip_prefix("vector:") {
        CanonicalScalar::bytes(remote::packed_vector(value)?).map_err(Failure::invalid_value)?
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
            .map_err(Failure::invalid_value)?,
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
        .map_err(Failure::invalid_value)
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
        | E::UnsupportedManifestFormat { .. }
        | E::SlotUnrecoverable { .. }
        | E::NotEmpty { .. } => Failure::open(error),
        other => {
            let mut source = std::error::Error::source(&other);
            while let Some(current) = source {
                if current
                    .downcast_ref::<fgdb_types::ScalarDecodeError>()
                    .is_some_and(|decode| {
                        matches!(
                            decode,
                            fgdb_types::ScalarDecodeError::Timestamp(
                                fgdb_types::TimestampDecodeError::TzdbResolverRequired
                                    | fgdb_types::TimestampDecodeError::InvalidValue(
                                        fgdb_types::TimestampConstructionError::Tzdb(_),
                                    ),
                            ) | fgdb_types::ScalarDecodeError::Text(
                                fgdb_types::CanonicalTextError::ResolverRequired
                                    | fgdb_types::CanonicalTextError::MissingArtifact { .. }
                            )
                        )
                    })
                {
                    return Failure::open(other);
                }
                source = current.source();
            }
            Failure::io(other)
        }
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
        // BufferedReadError predates error-chain exposure. Follow its two
        // storage wrappers explicitly so an extent or nested store I/O error
        // retains exit 5 even when wrapped by a Prism or native read error.
        source = match current.downcast_ref::<fgdb::BufferedReadError>() {
            Some(fgdb::BufferedReadError::Store(error)) => Some(error.as_ref()),
            Some(fgdb::BufferedReadError::Buffer(error)) => Some(error),
            _ => current.source(),
        };
    }
    let (refusal, offset) = refusal::classify(&error).unwrap_or((refusal::Code::Execution, None));
    Failure {
        refusal,
        offset,
        ..Failure::query(error)
    }
}
/// `query` opens a read view, which cannot express a write. A statement no
/// read parser accepts but the write binder does is named as a write, not
/// reported as a read parser's error at its first write keyword. Binding only
/// classifies the text; nothing executes.
fn read_failure(options: &Options, error: fgdb::QueryError) -> Failure {
    let failure = execution_failure(error);
    let parse = matches!(
        failure.refusal,
        refusal::Code::Syntax | refusal::Code::Statement
    );
    let write = || {
        matches!(
            write_returning::prepare_statement(&options.text, &options.params, options),
            Ok(Some(_))
        ) || BoundNativeGraphWrite::bind_with_resolver(
            &options.text,
            &options.params,
            options.coordinate,
            options,
        )
        .is_ok()
    };
    if parse && write() {
        Failure::coded(
            refusal::Code::Permission,
            "query is read-only: run this write statement with fgdb write",
        )
    } else {
        failure
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

/// Native execution limits, each a decimal u64 given at most once.
/// An absent limit keeps [`policy`]'s default; zero is never an omission.
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

    /// Intersect the per-operation result limit with a separate delivery cap.
    /// Do not reset snapshot/work/scratch limits or widen an explicit row limit.
    /// The transaction driver owns the remaining buffer count and savepoints.
    fn policy_with_result_cap(&self, remaining_rows: u64) -> GqlQueryPolicy {
        let allowance = self.policy();
        GqlQueryPolicy {
            rows: fgdb_gql::GqlExecutionBudget::new(
                allowance.rows.max_snapshot_records().unwrap_or(u64::MAX),
                allowance
                    .rows
                    .max_result_rows()
                    .unwrap_or(u64::MAX)
                    .min(remaining_rows),
            ),
            ..allowance
        }
    }
}

#[cfg(test)]
mod write_budget_tests;

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
/// A decimal `usize` bound: ASCII digits only, as every decimal flag.
fn decimal_usize(flag: &str, raw: &str) -> Result<usize, Failure> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Failure::usage(format!("{flag} requires a decimal integer")));
    }
    raw.parse()
        .map_err(|_| Failure::usage(format!("{flag} exceeds this platform's usize")))
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
            let builder = if options.fnx.uses_sealed_quantum() {
                // Register the cooperative root with the caller-driven
                // scheduler so intentional yields use its runnable-task path.
                // Retain the ordinary filesystem blocking pool.
                asupersync::runtime::RuntimeBuilder::current_thread()
                    .blocking_threads(0, fgdb_strata::store::BATCH_SYNCS_IN_FLIGHT)
            } else {
                fgdb::runtime_builder()
            };
            let runtime = builder.build().map_err(Failure::io)?;
            let root = runtime.request_cx_with_budget(Budget::INFINITE);
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let tunable = matches!(command, "query" | "replay" | "diff");
            let outcome = runtime.block_on(async {
                parameter_file::load_local(&mut options, &contexts.query()).await?;
                read_bindings_file(&mut options, &contexts.query()).await?;
                let mut keys = read_keys(&contexts.query(), &options.key).await?;
                let artifact = if let Some(path) = &options.tzdb_file {
                    contexts.query().checkpoint().map_err(Failure::io)?;
                    let bytes = asupersync::fs::read(path).await.map_err(Failure::io)?;
                    let artifact = std::sync::Arc::new(fgdb::PinnedTzdb::decode(&bytes).map_err(Failure::open)?);
                    keys = keys.with_scalar_resolver(artifact.clone());
                    options.zones = fgdb_gql::GqlTimeZones::new(artifact.clone(), artifact.object_id());
                    Some(artifact)
                } else { None };
                // A Prism call types its own arguments (vertex:<id> among them).
                if !options.fnx_call {
                    for (name, raw) in &options.raw_params {
                        options.params.insert(name, parameter(raw, artifact.as_deref())?).map_err(Failure::query)?;
                    }
                }
                if matches!(command, "query" | "write" | "diff" | "import-csv") {
                    refusal::unsupported_construct(&options.text)?;
                }
                // Every CSV record is read and bound before storage is opened,
                // so a refused input cannot leave any trace in the database.
                let import = if command == "import-csv" { Some(import::prepare(&options, &contexts.query())?) } else { None };
                // Likewise a Prism call's arguments and projection.
                let analytics = if options.fnx_call { Some(fnx::prepare(&options)?) } else { None };
                // A composed compressed read retains the native template and
                // its query-local projections, all bound before graph open.
                let sealed_read = if !options.fnx_call && options.fnx.is_sealed() && !options.buffered.enabled() {
                    Some(fnx::prepare_sealed_read(&options)?)
                } else { None };
                // Endpoint identities and all weighted-path arguments bind before graph open.
                let weighted = if options.weighted_path.enabled() { Some(weighted_path::prepare(&options)?) } else { None };
                // And a search's lanes, symbols and corpus.
                let retrieval = if command == "search" { Some(search::prepare(&options)?) } else { None };
                // Query-fed writes prepare both definitions before writable open.
                // Their actual input is read/bound later under the SAME transaction
                // as the dependent writes, never in a separate read invocation.
                let query_fed = if command == "write" { query_write::prepare(&options)? } else { None };
                // A --rows batch is parsed, typed and bound before storage opens,
                // like CSV, so a refused row leaves no trace in the database.
                let batch = if command == "write" && options.rows.is_some() {
                    Some(prepare_rows(&options)?)
                } else {
                    None
                };
                let returning = if command == "write" && batch.is_none() && query_fed.is_none() { write_returning::prepare(&options)? } else { None };
                // Native UNWIND mutations and ordinary scripts bind completely
                // before opening storage. Keep the bound record map through
                // completion instead of discarding it into a bare program.
                let native = if command == "write" && batch.is_none() && returning.is_none() && query_fed.is_none() {
                    Some(BoundNativeGraphWrite::bind_with_resolver(
                        &options.text,
                        &options.params,
                        options.coordinate,
                        &options,
                    ).map_err(Failure::statement)?)
                } else {
                    None
                };
                if let Some(prepared) = &sealed_read {
                    return fnx::run_sealed_read(
                        &contexts.commit(), &contexts.query(), &options, keys, prepared, robot, out,
                    ).await;
                }
                // The buffered profile binds and compiles before graph open,
                // then admits checkpoint objects through the extent cache.
                // It never constructs the resident read view below.
                if let Some(prepared) = analytics.as_ref().filter(|_| options.buffered.enabled()) {
                    let (pool, limits) = options.buffered.admission(options.budget.policy())?;
                    let mut view = Database::open_buffered_read_view(
                        &contexts.commit(), &options.db, keys, pool, limits,
                    ).await.map_err(buffered::open_failure)?;
                    return fnx::run_buffered(
                        &mut view, &contexts.query(), prepared, robot, out,
                    ).await;
                }
                if options.buffered.enabled() {
                    let prepared = buffered::prepare_query(&options)?;
                    let (pool, limits) = options.buffered.admission(options.budget.policy())?;
                    let mut view = Database::open_buffered_read_view(
                        &contexts.commit(), &options.db, keys, pool, limits,
                    ).await.map_err(buffered::open_failure)?;
                    return buffered::run_query(
                        &mut view, &contexts.query(), &options, &prepared,
                        artifact.as_deref().map(|artifact| artifact as &(dyn fgdb_types::CanonicalScalarResolver + Send + Sync)),
                        robot, out,
                    ).await;
                }
                // Binding and graph-kind checks already completed. Prepare the
                // compressed projection directly from the authenticated current
                // checkpoint, without a resident database or writer lease.
                if let Some(prepared) = analytics.as_ref().filter(|call| call.is_sealed()) {
                    return fnx::run_sealed(
                        &contexts.commit(), &contexts.query(), &options, keys, prepared, robot, out,
                    ).await;
                }
                // Reads need no writer: open only the published generation,
                // without the fold, version heads and delta history that writes
                // and certified execution use. Each read runs on that view, as
                // the database's own read entrypoints do on a fresh view.
                if matches!(command, "diff" | "search") || (command == "query" && options.certify_to.is_none()) {
                    let view = Database::open_read_view(&contexts.commit(), &options.db, keys).await.map_err(open_failure)?;
                    let cx = contexts.query();
                    let outcome = if command == "diff" {
                        diff::run(&view, &cx, &options, robot, out)
                    } else if let Some(prepared) = &weighted {
                        weighted_path::run(&view, &cx, prepared, robot, out)
                    } else if let Some(prepared) = &analytics {
                        fnx::run(&view, &cx, prepared, robot, out)
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
                        // The result names the snapshot that answered: a
                        // temporal read's bound AS OF cut, not the frontier.
                        view.query_with_snapshot(&cx, &options.text, &options.params, &options, options.budget.policy())
                            .map_err(|error| read_failure(&options, error))
                            .and_then(|(result, seq)| render(result, seq.0, "rows", robot, out))
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
                    if let Some(prepared) = query_fed {
                        return query_write::run(&mut db, &contexts, prepared, robot, out).await;
                    }
                    if let Some(prepared) = returning {
                        return write_returning::run(
                            &mut db, &contexts, prepared, options.budget.policy(), robot, out,
                        ).await;
                    }
                    let (program, records) = match batch.as_ref() {
                        Some((program, records)) => (program, Some(*records)),
                        None => {
                            let prepared = native.as_ref().expect("no-RETURN write was bound before open");
                            (prepared.program(), prepared.input_records())
                        }
                    };
                    let (receipt, completion) = db.execute_graph_write_program_returning_autocommit_engine_governed(&contexts.txn(), &contexts.query(), &contexts.commit(), program, GraphWriteProgramPolicy::new(options.budget.policy(), 100_000, 100_000, 100_000)).await.map_err(|error| {
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
                // A certificate is an audit artifact: replacing one is an
                // explicit choice (fgdb-vuyw5). Refuse before executing, and
                // create the file new, so one appearing meanwhile still wins.
                let exists = || Failure::usage(format!("certificate {} already exists; pass --overwrite to replace it", path.display()));
                if !options.overwrite && asupersync::fs::try_exists(path).await.map_err(Failure::io)? {
                    return Err(exists());
                }
                let (result, certificate) = db.execute_certified(&contexts.query(), &options.text, &options.params, &options, options.budget.policy()).map_err(execution_failure)?;
                render(result, certificate.plan.snapshot_seq.0, "rows", robot, out)?;
                out.flush().map_err(Failure::io)?;
                contexts.query().checkpoint().map_err(Failure::io)?;
                if options.overwrite {
                    return asupersync::fs::write(path, certificate.canonical_bytes()).await.map_err(Failure::io);
                }
                let mut file = match asupersync::fs::File::create_new(path).await {
                    Ok(file) => file,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Err(exists()),
                    Err(error) => return Err(Failure::io(error)),
                };
                asupersync::io::AsyncWriteExt::write_all(&mut file, &certificate.canonical_bytes()).await.map_err(Failure::io)?;
                asupersync::io::AsyncWriteExt::flush(&mut file).await.map_err(Failure::io)
            });
            outcome.map_err(|failure| {
                if tunable {
                    failure.with_budget_hint()
                } else {
                    failure
                }
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
        GraphValue::Scalar(CanonicalScalar::Date(v)) => {
            format!(r#"{{"type":"date","value":{}}}"#, quoted(&v.to_string()))
        }
        GraphValue::Scalar(CanonicalScalar::LocalTime(v)) => {
            format!(
                r#"{{"type":"localtime","value":{}}}"#,
                quoted(&v.to_string())
            )
        }
        GraphValue::Scalar(CanonicalScalar::Time(v)) => {
            format!(r#"{{"type":"time","value":{}}}"#, quoted(&v.to_string()))
        }
        GraphValue::Scalar(CanonicalScalar::LocalDateTime(v)) => {
            format!(
                r#"{{"type":"localdatetime","value":{}}}"#,
                quoted(&v.to_string())
            )
        }
        GraphValue::Scalar(CanonicalScalar::Duration(v)) => {
            format!(
                r#"{{"type":"duration","value":{}}}"#,
                quoted(&v.to_string())
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
        GraphValue::Scalar(CanonicalScalar::Date(v)) => v.to_string(),
        GraphValue::Scalar(CanonicalScalar::LocalTime(v)) => v.to_string(),
        GraphValue::Scalar(CanonicalScalar::Time(v)) => v.to_string(),
        GraphValue::Scalar(CanonicalScalar::LocalDateTime(v)) => v.to_string(),
        GraphValue::Scalar(CanonicalScalar::Duration(v)) => v.to_string(),
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
    render_rows_iter(columns, rendered.iter(), seq, kind, robot, out)
}

/// Encode a completed answer without retaining a second, rendered row set.
/// Human tables replay the iterator for widths; robot rows are encoded once.
fn render_rows_iter<Row: AsRef<[String]>>(
    columns: &[String],
    rendered: impl ExactSizeIterator<Item = Row> + Clone,
    seq: u64,
    kind: &str,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let count = rendered.len();
    render_row_body_iter(columns, rendered, robot, out)?;
    if robot {
        emit(
            out,
            &format!(r#"{{"v":1,"event":"result","kind":"{kind}","seq":{seq},"count":{count}}}"#,),
        )
    } else {
        emit(out, &format!("{count} row(s)"))
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
    render_row_body_iter(columns, rendered.iter(), robot, out)
}

fn render_row_body_iter<Row: AsRef<[String]>>(
    columns: &[String],
    rendered: impl Iterator<Item = Row> + Clone,
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
                &format!(
                    r#"{{"v":1,"event":"row","cells":[{}]}}"#,
                    row.as_ref().join(",")
                ),
            )?;
        }
        Ok(())
    } else {
        let mut widths: Vec<_> = columns.iter().map(|c| c.chars().count()).collect();
        for row in rendered.clone() {
            for (width, value) in widths.iter_mut().zip(row.as_ref()) {
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
            emit(out, &line(row.as_ref()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod buffered_error_tests {
    use super::execution_failure;
    use fgdb::BufferedReadError;
    use fgdb_prism::FnxReadError;
    use fgdb_strata::store::StoreError;
    use fgdb_strata::tiered::buffer::BufferError;
    use fgdb_types::CommitSeq;

    #[test]
    fn buffered_analytics_retains_storage_io_and_query_refusal_exit_classes() {
        type Error = FnxReadError<BufferedReadError, std::io::Error>;
        for source in [
            BufferedReadError::Store(Box::new(StoreError::Io(
                std::io::ErrorKind::NotFound.into(),
            ))),
            BufferedReadError::Store(Box::new(StoreError::RootPatchLoad {
                at: 2,
                error: Box::new(StoreError::Io(std::io::ErrorKind::UnexpectedEof.into())),
            })),
            BufferedReadError::Buffer(BufferError::Load(std::io::ErrorKind::Interrupted.into())),
        ] {
            let error = Error::Read(source);
            let diagnostic = error.to_string();
            let failure = execution_failure(error);
            assert_eq!(failure.class, "io");
            assert_eq!(failure.code, 5);
            assert_eq!(failure.message, diagnostic);
        }
        for source in [
            BufferedReadError::Limit {
                resource: "work units",
                requested: 2,
                limit: 1,
            },
            BufferedReadError::BeyondPublication {
                requested: CommitSeq(2),
                publication: CommitSeq(1),
            },
            BufferedReadError::Buffer(BufferError::CacheSlotsExhausted { limit: 0 }),
        ] {
            let failure = execution_failure(Error::Read(source));
            assert_eq!(failure.class, "query");
            assert_eq!(failure.code, 3);
        }
    }
}

#[cfg(test)]
mod completed_row_encoding_tests {
    use super::render_rows_iter;
    use std::cell::Cell;
    use std::io::{self, Write};

    #[test]
    fn robot_answer_encodes_each_row_once_and_preserves_complete_framing() {
        let encoded = Cell::new(0);
        let rows = [3, 9].iter().map(|value| {
            encoded.set(encoded.get() + 1);
            vec![format!(r#"{{"type":"int","value":"{value}"}}"#)]
        });
        let mut out = Vec::new();
        render_rows_iter(&["n".into()], rows, 17, "rows", true, &mut out).unwrap();
        assert_eq!(encoded.get(), 2);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "{\"v\":1,\"event\":\"columns\",\"columns\":[\"n\"]}\n",
                "{\"v\":1,\"event\":\"row\",\"cells\":[{\"type\":\"int\",\"value\":\"3\"}]}\n",
                "{\"v\":1,\"event\":\"row\",\"cells\":[{\"type\":\"int\",\"value\":\"9\"}]}\n",
                "{\"v\":1,\"event\":\"result\",\"kind\":\"rows\",\"seq\":17,\"count\":2}\n",
            )
        );
    }

    #[test]
    fn human_answer_replays_widths_without_retaining_rendered_rows() {
        let encoded = Cell::new(0);
        let rows = [("é", "1"), ("long", "20")].iter().map(|(name, value)| {
            encoded.set(encoded.get() + 1);
            vec![(*name).to_owned(), (*value).to_owned()]
        });
        let mut out = Vec::new();
        render_rows_iter(
            &["n".into(), "value".into()],
            rows,
            17,
            "rows",
            false,
            &mut out,
        )
        .unwrap();
        assert_eq!(encoded.get(), 4);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            concat!(
                "n    | value\n",
                "------------\n",
                "é    | 1    \n",
                "long | 20   \n",
                "2 row(s)\n",
            )
        );
    }

    #[test]
    fn failed_robot_output_stops_encoding_and_emits_no_completion() {
        struct FailSecondRow {
            bytes: Vec<u8>,
            rows: usize,
        }
        impl Write for FailSecondRow {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes
                    .windows(b"\"event\":\"row\"".len())
                    .any(|part| part == b"\"event\":\"row\"")
                {
                    self.rows += 1;
                    if self.rows == 2 {
                        return Err(io::ErrorKind::BrokenPipe.into());
                    }
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let encoded = Cell::new(0);
        let rows = (0..10).map(|value| {
            encoded.set(encoded.get() + 1);
            vec![format!(r#"{{"type":"int","value":"{value}"}}"#)]
        });
        let mut out = FailSecondRow {
            bytes: Vec::new(),
            rows: 0,
        };
        let error = render_rows_iter(&["n".into()], rows, 17, "rows", true, &mut out).unwrap_err();
        assert_eq!(error.class, "io");
        assert_eq!(encoded.get(), 2);
        let delivered = String::from_utf8(out.bytes).unwrap();
        assert_eq!(delivered.lines().count(), 2);
        assert!(!delivered.contains("\"event\":\"result\""));
    }
}

#[cfg(test)]
mod budget_hint_tests {
    use super::budget_flag;
    use fgdb_gql::{GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlBudgetExceeded};

    /// Each typed refusal's own Display text maps to the flag that raises it,
    /// so a renamed dimension or reworded refusal fails here, not in a user's
    /// terminal; an unrelated message gets no hint.
    #[test]
    fn every_execution_limit_refusal_names_its_flag() {
        let refusals = [
            (
                GqlBudgetExceeded {
                    dimension: GqlBudgetDimension::SnapshotRecords,
                    limit: 100_000,
                    observed: 100_001,
                }
                .to_string(),
                "--max-snapshot-records",
            ),
            (
                GqlBudgetExceeded {
                    dimension: GqlBudgetDimension::ResultRows,
                    limit: 1,
                    observed: 2,
                }
                .to_string(),
                "--max-result-rows",
            ),
            (
                GlaLimitExceeded {
                    dimension: GlaLimitDimension::WorkUnits,
                    limit: 10,
                    observed: 11,
                }
                .to_string(),
                "--max-work-units",
            ),
            (
                GlaLimitExceeded {
                    dimension: GlaLimitDimension::ScratchEntries,
                    limit: 10,
                    observed: 11,
                }
                .to_string(),
                "--max-scratch-entries",
            ),
        ];
        for (message, flag) in refusals {
            assert_eq!(budget_flag(&message), Some(flag), "{message}");
            let rendered = super::Failure::query(&message).with_budget_hint().message;
            assert!(
                rendered.ends_with(&format!(" (raise with {flag})")),
                "{rendered}"
            );
            // A command without the flag (write, remote, ...) is not told to
            // raise it.
            assert_eq!(super::Failure::query(&message).message, message);
            // Nor is a failure that is not a query refusal.
            assert_eq!(
                super::Failure::io(&message).with_budget_hint().message,
                message
            );
        }
        assert_eq!(budget_flag("unknown label Person"), None);
        assert_eq!(
            super::Failure::query("unknown label Person")
                .with_budget_hint()
                .message,
            "unknown label Person"
        );
    }
}

#[cfg(test)]
mod native_symbol_tests {
    use super::*;

    fn options(extra: &[&str]) -> Result<Options, Failure> {
        let args: Vec<String> = ["--db", "unused", "--key-file", "unused", "INSERT ()"]
            .into_iter()
            .chain(extra.iter().copied())
            .map(str::to_owned)
            .collect();
        parse(&args, "write")
    }

    #[test]
    fn flag_bindings_and_reverse_names_keep_the_complete_native_domain() {
        for ordinal in [0, u64::from(u32::MAX) + 1, u64::MAX] {
            let label = format!("Native={ordinal}");
            let relation = format!("NATIVE={ordinal}");
            let property = format!("wide={ordinal}");
            let raw = ordinal.to_string();
            let opts = options(&[
                "--label",
                &label,
                "--relation",
                &relation,
                "--property",
                &property,
                "--write-relation",
                &raw,
            ])
            .unwrap();
            assert_eq!(opts.coordinate, RelationId(ordinal));
            assert_eq!(
                opts.resolve(GraphSymbolKind::Label, "Native"),
                Some(GraphSymbol::Label(LabelId(ordinal)))
            );
            assert_eq!(
                opts.resolve(GraphSymbolKind::Relation, "NATIVE"),
                Some(GraphSymbol::Relation(RelationId(ordinal)))
            );
            assert_eq!(
                opts.resolve(GraphSymbolKind::Property, "wide"),
                Some(GraphSymbol::Property(PropertyKeyId(ordinal)))
            );
            assert_eq!(
                opts.reverse_label(LabelId(ordinal)).as_deref(),
                Some("Native")
            );
            assert_eq!(
                opts.reverse_relation(RelationId(ordinal)).as_deref(),
                Some("NATIVE")
            );
            let catalog = opts.reverse_catalog().unwrap();
            assert_eq!(
                catalog
                    .properties
                    .get(&PropertyKeyId(ordinal))
                    .map(String::as_str),
                Some("wide")
            );
        }
        let opts = options(&[
            "--label",
            "Low=4294967295",
            "--label",
            "Native=18446744073709551615",
        ])
        .unwrap();
        assert_eq!(opts.labels.len(), 2);
        assert_ne!(opts.labels["Low"], opts.labels["Native"]);
    }

    #[test]
    fn malformed_ordinals_and_duplicate_native_ids_refuse_before_open() {
        for raw in [
            "",
            "-1",
            "+1",
            " 1",
            "1 ",
            "1.0",
            "0x1",
            "18446744073709551616",
        ] {
            for flag in ["--label", "--relation", "--property", "--write-relation"] {
                let binding = format!("Native={raw}");
                let value = if flag == "--write-relation" {
                    raw
                } else {
                    &binding
                };
                let error = options(&[flag, value])
                    .err()
                    .expect("invalid ordinal must refuse");
                assert_eq!(error.code, 2, "{flag}={raw:?}");
                assert_eq!(error.class, "usage");
            }
        }
        for flag in ["--label", "--relation", "--property"] {
            assert!(
                options(&[
                    flag,
                    "Native=18446744073709551615",
                    flag,
                    "Alias=18446744073709551615"
                ])
                .is_err()
            );
            assert!(options(&[flag, "Native=18446744073709551615", flag, "Native=1"]).is_err());
        }
    }
}
