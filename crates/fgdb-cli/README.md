# fgdb — the embedded-database CLI

`fgdb-cli` owns the one `fgdb` binary (`src/main.rs`). Every command opens the
real `fgdb::Database` directory through the production Asupersync runtime and
purpose-narrowed contexts. There is no alternate engine, no network client and
no daemon. Use one process at a time per database directory: the embedded owner
does not add cross-process coordination. Anyone holding the key file has full
access to the data. The CLI is not a sandbox.

`fgdb help` prints the complete usage. `fgdb robot schema` prints the
machine-readable contract that the tests freeze.

## Commands

| Command | What it does |
|---|---|
| `create` | Create a new database directory (refuses an existing one). |
| `adopt --db <dir>` | Sync a copied database before its first write. Needs no keys; does not change data or advance the frontier. |
| `write <gql>` | Run one native write program in one transaction. |
| `query <gql>` | Native read, including `EXPLAIN`. A write statement is refused (exit 3), never retried as a write. `--stream` flushes one row at a time on the streamable profiles; `--certify-to <file>` saves a replayable result certificate. |
| `query 'CALL fnx.<procedure>(…) YIELD …'` | A registered Prism analytics procedure over an explicit projection of one committed sequence (see Analytics). |
| `replay --certificate <file>` | Re-execute a certified query at its recorded sequence, byte for byte. |
| `diff --before <seq> --after <seq> <gql>` | Net bag difference of one query's complete results at two committed sequences. |
| `transaction --write <gql> --query <gql> …` | Ordered steps in ONE native transaction; `--rollback` discards everything. `--savepoint <name>`, `--rollback-to <name>` and `--release <name>` steps rewind or keep the effects staged after a savepoint; rolling back also drops the rows and records those later steps buffered. |
| `load --input <file.ndjson>` | Chunked bulk load with an optional resumable `--checkpoint`. `--max-source-rows N` and `--max-total-key-bytes N` raise the whole-input bounds (defaults 1,000,000 rows and 64 MiB of keys; a resume repeats them). |
| `import-csv --input <file.csv\|-> (--query-file <f> \| <gql>)` | Bind a native write script once per CSV record, and commit every record in ONE transaction or none. |
| `compact` | Publish a consolidated storage generation. Answers at every retained sequence are unchanged, and the frontier does not move. |
| `scrub` | Verify every capsule the history names and the current admitted generation's blocks, vertex patches, manifest, partition root and root segments. Damaged capsule redundancy is repaired in place, restoring the exact bytes. Each damaged object gets a `scrub` record; any loss exits 5 after the records, and lost objects are never overwritten. Damage that prevents admission is refused at open, naming the object; scrub also verifies capsule history deferred by checkpoint opens. |
| `search` | Beacon text, vector or exact-fusion hybrid retrieval over one committed sequence (see Retrieval). |
| `robot schema`, `help` | Need no database or keys. |

Every database command except `adopt` takes `--db <dir> --key-file <file>`.

### Adopting a copied database

After copying a database directory with a tool that did not sync its files
(for example, `cp -r` or an archive extractor), run `fgdb adopt --db <copy>`
once before the first write. Writable opens trust that the published root's
objects are already durable; adoption establishes that premise for a copy by
syncing every regular file, then the directories from deepest to shallowest,
then the database directory's parent. It follows no symlinks and needs no key
file. Use one process at a time, as for other database operations.

Adoption refuses a non-database directory with exit 4. It does not validate or
repair object contents; subsequent opens perform their normal authentication.
Robot mode emits a terminal `result` with `kind: "adopted"` and integer
`files` and `directories` counts. The directory count covers the database
directory and its descendants; its parent is synced in addition.

### Key file

The key file has three non-empty lines of 64 hexadecimal characters: the
object-id key, the security namespace and the encryption key. `#` starts a
comment. Keys are never accepted on the command line and never printed. The CLI
neither generates nor stores keys. A new database needs independently generated
keys, not the test keys. On Unix the key file must be a regular file with no
group or other permission bits (`chmod 600`). Anything else is refused with
exit 4 before the database is opened or created. The check reads the opened
handle, not the path, and the file may be at most 64 KiB.

### Bindings: no implicit catalog

Label, relation and property IDs are explicit on every invocation:
`--label Person=1 --relation KNOWS=1 --property name=1`. Supply the same
bindings when you reopen. The CLI never guesses or hashes names, and `labels()`
/ `type()` reflection answers from the supplied bindings.

`--bindings-file <file>` reads the same mapping once from a file, one binding
per line (`label Person=1`, `relation KNOWS=1`, `property name=1`; `#` comments
and blank lines are ignored; at most 1 MiB). File and flags combine, and must
not repeat a name or an ID within a kind. `fgdbd serve` accepts the same file
per database. It is an explicit mapping you keep, not a catalog in the database.

### Loading edges between vertices that are already in the database

An NDJSON edge endpoint is either the caller key of a vertex row earlier in
the same input (`"source":"z1"`), or an object naming one vertex that was
committed before this load began, by an optional label and one property value:

```json
{"kind":"edge","key":"t1","source":{"label":"Zone","property":"id","value":"int:132"},
 "destination":{"label":"Zone","property":"id","value":"int:7"},"relation":"TRIP","props":{}}
```

So vertices can load once and edges arrive in later inputs (monthly trip
files, `edges.csv` after `nodes.csv`). The match is resolved through the
property index as of the load's start, so a vertex the same load creates never
changes it, and a resumed load resolves identically. A match that names no
committed vertex (`UnresolvedEndpoint`) or several (`AmbiguousEndpoint`) refuses
at its line before anything commits; a null match value is a malformed row.

### Parameters: typed, never interpolated

`--param name=int:42`, `uint:42`, `text:Ada`, `bool:true`, `null`,
`timestamp:<utc-nanos>,<offset-seconds>,<zone>,<tzdb-oid-hex>` (with
`--tzdb-file`), or `json:<array>` for a list: objects are maps, strings
text, integers int, other numbers float, so
`--param 'rows=json:[{"name":"Ada"}]'` feeds
`UNWIND $rows AS row CREATE (:Person {name: row.name})`. A parameter is a
value. It is never spliced into GQL text.

`write --rows 'json:[{"id":1,"name":"Ada"},...]' 'MERGE (p:Person {id: $id})
ON CREATE SET p.name = $name ON MATCH SET p.name = $name'` binds the
statement's parameters once per object and commits every row as ONE atomic
program, as `import-csv` does for CSV records. A later row's `MERGE` sees an
earlier row's node, so a repeated key upserts instead of duplicating. A
failing row commits nothing. Integers bind like `int:` (nullable when a row
omits them), and other scalars and arrays as in `json:`. A key that is not a
parameter of the statement is a usage error, never silently ignored.

## `import-csv`

The CSV header names the statement's parameters, and each record binds the
statement once. The whole file runs as ONE atomic native write program: a
binding or execution failure in any record commits nothing. A division by zero
in record *n*, for example, rolls back records 1..*n*-1 too. Every input (the
statement, the type declarations and the CSV) is read under a byte bound and
bound by the native compiler before the database opens, so a refused input
leaves no trace. That covers quoting, multi-line fields, record and field
limits, and malformed records. Refusals locate the bad field (record, column,
offset) without echoing its value.

- The statement comes from exactly one of `--query-file <file.gql|->` or a GQL
  argument.
- `--types-file` holds one `kind<TAB>name` per line with kinds `int64`,
  `uint64`, `int`, `text`, `bool`, `null`, `float`, `decimal` or `bytes`.
  Undeclared parameters keep native inference (stored-property operands default
  to `int64`); values never choose a type.
- `--max-input-bytes N` bounds the encoded CSV including its header. The
  default is 16 MiB, and the decoder's hard ceiling of 64 MiB still applies.
- `--max-changes N` is ONE allowance for effects, new vertices and new edges
  across the whole file, from 1 to 1,000,000 (default 100,000). It never resets
  per record.
- `-` reads stdin, and at most one input may use it.

### Chained statements

A chained write such as
`MERGE (a:Person {id: $src}) MERGE (b:Person {id: $dst}) MERGE (a)-[:KNOWS]->(b)`
is not a write script, so it is not bound once per record. It runs as one
chain over every record: `UNWIND` over the records, with each `$name` read
from the current record. As in openCypher's `LOAD CSV`, each clause applies
to every record before the next clause runs. A chain of MERGEs (with at most
ON CREATE clauses, and at most one final CREATE) stages its creations once,
so its cost grows linearly with the record count. The equivalent per-record
script (`MERGE (a ...); MERGE (b ...); MATCH (a ...), (b ...) MERGE
(a)-[:KNOWS]->(b)`) re-reads everything staged before each record, and in a
2026-10-22 measurement it refused after about 190 records.

- Statements the script grammar accepts keep the per-record path above.
- Parameter types come from `--types-file`; an undeclared parameter is int64.
  `--types-file` must not declare a parameter the statement does not read.
- The import stays one transaction. Preparation streams the CSV through the
  native framer and typed decoder into one bounded row relation, retaining
  only that relation plus the header/current record. Execution retains the
  relation and its writes in memory; this is not a constant-memory bulk loader.
- Each record is admitted as its own map, so column count no longer divides
  a single list parameter's 65,536-value allowance. The unchanged CSV ceiling
  is 65,536 records, also bounded by source bytes, field sizes and cumulative
  parameter bytes. Execution still spends the selected work, scratch and
  change allowances; a larger import may require explicit budget flags.
- The text must not already use the names `__csv_rows` or `__csv_row`.

### Floating-point, decimal and binary CSV columns

`float` parses finite binary64 values, including decimal exponents; nonfinite
values, leading `+`, surrounding whitespace and overflow refuse. `decimal`
is exact fixed scale 18, never a conversion through binary floating point:
`0.1` and `9007199254740993.000000000000000001` retain every digit. Exponents
are not decimal syntax; extra fractional places must all be trailing zeros.
The native coefficient/range limits still apply.

`bytes` accepts pairs of hexadecimal digits (either case), with no `0x` or
`bytes:` prefix. This carries arbitrary binary data, including packed embedding
vectors. A quoted empty field `""` is empty bytes, not NULL. Unquoted `\N`
means NULL in every type; quoted `"\N"` is data and is invalid for these three
types. Declaring `bytes` does not validate an embedding's shape: vector length
and finiteness are checked when the existing Beacon projection reads it.

For an existing database and key file, import two-dimensional packed f32
embeddings (little-endian, four bytes per coordinate) and search them:

```sh
printf 'text\tname\nbytes\tembedding\n' > embedding.types
printf 'name,embedding\nfirst,0000803f00000000\nsecond,000000000000803f\n' > embeddings.csv
fgdb --robot import-csv --db ./graph --key-file ./graph.keys \
  --label Doc=1 --property name=1 --property embedding=2 \
  --input embeddings.csv --types-file embedding.types \
  'CREATE (n:Doc {name:$name, embedding:$embedding})'
fgdb --robot search --db ./graph --key-file ./graph.keys \
  --label Doc=1 --property embedding=2 --vertex-label Doc \
  --vector 1,0 --vector-property embedding --vector-encoding packed --k 2
```

All CSV records still form one atomic program. A malformed value in a late
record refuses the complete input before database open; a late execution
failure discards all earlier records' staged effects. The existing source,
parameter, execution and change limits are neither bypassed nor reset per row.

## Analytics: `CALL fnx.*`

`query` runs a registered Prism procedure when the statement is `CALL
fnx.<procedure>(<args>) YIELD <output> [AS <alias>], ...`. The registered
procedures are `pagerank`, `single_source_shortest_path_length`,
`single_source_dijkstra_path_length`, `connected_components`,
`weakly_connected_components`, `strongly_connected_components`, `triangles`
and `clustering_coefficient`. The graph a procedure sees is an explicit
projection that you choose. It is never an implicit collapse of the stored
multigraph:

- `--graph-label <label>` and `--graph-relation <relation>` select the induced
  vertices and edges. The default is every vertex and every relation.
- `--weight <property>` reads edge weights, with `--missing-weight
  reject|unit|zero`. Without it, every edge weighs one.
- `--direction directed|reversed|undirected` (default `directed`).
- `--parallel-edges reject|collapse|min|max|sum` (default `reject`): a
  multigraph is refused until you choose a law.
- `--self-loops keep|drop|reject` (default `keep`).
- `--as-of <seq>` analyses a committed sequence (time travel). The default is
  the frontier.

Arguments are literals or `$name` parameters: `--param name=vertex:<id>`,
`int:`, `float:`, `bool:true|false` or `null`. A procedure whose graph laws
the projection breaks, such as `connected_components` over a directed
projection, is refused, never converted. Output uses the ordinary `columns`,
`row` and `result kind=rows` records. The result's `seq` is the analysed
sequence. Vertices and component labels are vertex identities, hop distances
and triangle counts are exact `int`s, and scores and weighted distances are
`float`s. `--stream` and `--certify-to` are not offered for analytics.

```sh
fgdb --robot query --db ./graph --key-file ./graph.keys --relation KNOWS=1 \
  --graph-relation KNOWS --direction undirected \
  'CALL fnx.connected_components() YIELD vertex, component'
```

## Retrieval: `search`

`fgdb search` runs Beacon text, vector, or hybrid retrieval over one committed
sequence, through the same `Database::beacon_search` the library exposes. The
corpus is an explicit projection chosen by flags:

- Text lane: `--text <query> --text-property <property>`, BM25 over that text
  property. `--text-match any|all|phrase` defaults to `any`. The typo-tolerant
  modes `fuzzy1`, `fuzzy2`, `fuzzy1-all` and `fuzzy2-all` match within that
  edit distance, any term or every term. They expand to at most
  `--max-expansions` vocabulary terms (default 64), and a query beyond the
  bound is refused rather than truncated.
- Vector lane: `--vector <x,y,...>` with one `--vector-property <property>` per
  coordinate, in order. It finds nearest neighbours over those numeric
  properties. `--metric l2|cosine|dot` defaults to `l2`. The search is exact
  unless `--ann <ef_search>` asks for the approximate HNSW walk. With
  `--vector-encoding packed`, name exactly one bytes property containing all
  coordinates as little-endian f32 values (see CSV example above). The default
  encoding is `coordinates-rounded`: each stored integer, float or decimal
  coordinate rounds once to the nearest f32. `--vector-encoding coordinates`
  opts into exact mode, which refuses any value f32 cannot hold exactly. No
  stored-value type is used to guess the encoding.
- Both lanes: exact reciprocal-rank fusion of `--candidates` hits per lane
  (default `--k`). This fuses two candidate sets. It is not an exhaustive
  hybrid answer.
- Graph lane (GraphRAG): `--expand-from <vid,...>` with an explicit
  `--max-hops <n>` expands from those seed vertices and fuses the reached
  vertices as a third reciprocal-rank lane with the text and/or vector lanes.
  The expansion is bounded:
  - `--expand-relation <relation>` (default every relation);
  - `--expand-direction out|in|both` (default out);
  - `--include-seeds true|false` (default false);
  - `--graph-candidates` (default `--candidates`) and `--graph-weight`
    (default 1).

  Label selection applies to transit vertices too, and missing seeds are
  simply absent.
- `--k` bounds the hits (default 10).
- `--vertex-label <label>` restricts the corpus.
- `--as-of <seq>` searches a committed sequence (time travel). The default is
  the frontier.

The index is built for the one call from that sequence, so the result's `seq`
is exactly the sequence searched. Output uses the ordinary `columns`, `row` and
`result kind=searched` records:

| Lanes | Columns |
|---|---|
| Text | `vertex`, `score` |
| Vector | `vertex`, `distance` |
| Both | `vertex`, `score` (an exact decimal), `vector_rank`, `text_rank`, `vector_distance`, `text_score` |
| With the graph lane | `vertex`, `score`, `vector_rank`, `text_rank`, `graph_rank`, `vector_distance`, `text_score`, `graph_hops` |

A lane that did not rank a hit leaves its fields `null`. These are usage errors
(exit 2) before the database opens:

- no lane;
- a lane flag without its lane;
- a coordinate-encoded vector whose length differs from its property count,
  or packed encoding without exactly one vector property;
- a non-finite coordinate;
- `--candidates` without a fused search (both lanes, or the graph lane);
- `--max-expansions` without a fuzzy mode;
- a graph lane flag without `--expand-from`, or `--expand-from` without
  `--max-hops` or without a text or vector lane;
- an unbound symbol.


```sh
fgdb --robot search --db ./graph --key-file ./graph.keys \
  --property title=1 --property x=2 --property y=3 \
  --text 'graph memory' --text-property title \
  --vector 0.1,0.9 --vector-property x --vector-property y --k 5
```

## Robot mode

`fgdb --robot <command>` writes versioned NDJSON on stdout: an `invocation`
record, then the command's records, then exactly one terminal `result`, or an
`error` with a `class` and `diagnostics`. Human text never appears in robot
stdout. Only a terminal `result` plus exit `0` means complete delivery.

```sh
fgdb --robot create --db ./graph --key-file ./graph.keys
fgdb --robot write --db ./graph --key-file ./graph.keys --label Person=1 --property name=1 \
  "INSERT (:Person {name: 'Ada'})"
printf 'name\nGrace\nLinus\n' > people.csv
printf 'text\tname\n' > people.types
fgdb --robot import-csv --db ./graph --key-file ./graph.keys --label Person=1 --property name=1 \
  --input people.csv --types-file people.types 'INSERT (:Person {name: $name})'
fgdb --robot query --db ./graph --key-file ./graph.keys --label Person=1 --property name=1 \
  'MATCH (p:Person) RETURN p.name AS name ORDER BY name'
fgdb --robot compact --db ./graph --key-file ./graph.keys
```

Counts, wide integers, identities and diff counters are decimal strings, never
rounded JSON numbers. Exit codes: `0` success, `2` usage, `3` query (including
admission and execution refusals), `4` open (including key-file and wrong-key
failures), `5` io.

An `error` also carries a stable `code` that says what to do next, and an
`offset` when the refusal points at a statement byte. Branch on `code`, not on
the `diagnostics` wording, which may change:

```text
{"v":1,"event":"error","class":"query","code":"unsupported","offset":0,"diagnostics":["byte 0: CREATE GRAPH is not supported yet (graph DDL)"]}
{"v":1,"event":"error","class":"query","code":"syntax","offset":26,"diagnostics":["statement 0, byte 26: expected end of statement"]}
```

| code | class (exit) | meaning |
|---|---|---|
| `usage` | usage (2) | the command line is wrong |
| `syntax` | query (3) | the statement does not parse; `offset` is the byte to fix |
| `unsupported` | query (3) | a recognized construct that is not supported yet: graph or branch DDL, `AT BRANCH`, `CALL {...}` |
| `not_found` | query (3) | a label, relation or property name in the statement has no binding |
| `invalid_value` | query (3) | a parameter or value is missing, unexpected, mistyped or out of range |
| `statement` | query (3) | the statement parses but its definition is refused; also every remote statement refusal |
| `budget` | query (3) | an execution limit was exceeded; a larger limit may succeed |
| `permission` | query (3) | the command or capability may not do this, such as a write given to read-only `query` |
| `conflict` | query (3) | a concurrent commit won; nothing was committed |
| `execution` | query (3) | refused while executing or delivering output |
| `open` | open (4) | the database or key could not be opened |
| `io` | io (5) | storage or the operating system failed |

`offset` is a UTF-8 byte offset into the statement, into the whole script for
a multi-statement `write`, or into the step's own text in a `transaction`.
Statement indexes in messages are zero-based, like the native program's.
Human mode prints the same code: `fgdb: error[unsupported]: byte 0: ...`.
`fgdb robot schema` documents the codes under `errors`.

## Verification

```sh
cargo test -p fgdb-cli
```

Every test drives the built binary in fresh subprocesses against the durable
engine.

- `cli_robot.rs`: lifecycle, certificates, typed parameters, exact scalar
  output, timestamps, typed failures and key privacy, all validated against the
  frozen schema.
- `cli_fuzz_contract.rs`: a seeded process fuzz campaign over the same
  contract.
- `cli_load.rs`, `cli_streamed_load.rs`: bulk load and streaming.
- `cli.rs`: `compact` preserves results across reopens and really publishes a
  new generation; library-written data is read by CLI processes; catalog
  reflection; refusals that create nothing (bad key files, missing database,
  write text on `query`); the `diff` output cap.
- `write_import.rs`: CSV atomicity, admission before mutation, the
  whole-program budget, spent identities, and stdin bounds.
- `csv_scalar_import.rs`: exact numeric/binary round-trips, atomic late refusals,
  independent limits, and CSV-loaded 384-dimensional vectors searched at current
  and historical sequences.

The frozen schema lives once, in `tests/support/robot_schema.rs`. To change the
schema, edit it and `ROBOT_SCHEMA` in `src/main.rs` in the same commit.
