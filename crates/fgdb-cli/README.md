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
| `write <gql>` | Run one native write program in one transaction. |
| `query <gql>` | Native read, including `EXPLAIN`. A write statement is refused (exit 3), never retried as a write. `--stream` flushes one row at a time on the streamable profiles; `--certify-to <file>` saves a replayable result certificate. |
| `query 'CALL fnx.<procedure>(…) YIELD …'` | A registered Prism analytics procedure over an explicit projection of one committed sequence (see Analytics). |
| `replay --certificate <file>` | Re-execute a certified query at its recorded sequence, byte for byte. |
| `diff --before <seq> --after <seq> <gql>` | Net bag difference of one query's complete results at two committed sequences. |
| `transaction --write <gql> --query <gql> …` | Ordered steps in ONE native transaction; `--rollback` discards everything. |
| `load --input <file.ndjson>` | Chunked bulk load with an optional resumable `--checkpoint`. |
| `import-csv --input <file.csv\|-> (--query-file <f> \| <gql>)` | Bind a native write script once per CSV record, and commit every record in ONE transaction or none. |
| `compact` | Publish a consolidated storage generation. Answers at every retained sequence are unchanged, and the frontier does not move. |
| `scrub` | Verify every capsule the history names and every published block. Damaged capsule redundancy is repaired in place, restoring the exact bytes. Each damaged object gets a `scrub` record; any loss exits 5 after the records, and lost objects are never overwritten. Damage beyond repair that already exists when the database opens is refused at open, naming the object. |
| `search` | Beacon text, vector or exact-fusion hybrid retrieval over one committed sequence (see Retrieval). |
| `robot schema`, `help` | Need no database or keys. |

Every database command takes `--db <dir> --key-file <file>`.

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

### Parameters: typed, never interpolated

`--param name=int:42`, `uint:42`, `text:Ada`, `bool:true`, `null`, or
`timestamp:<utc-nanos>,<offset-seconds>,<zone>,<tzdb-oid-hex>` (with
`--tzdb-file`). A parameter is a value. It is never spliced into GQL text.

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
  `uint64`, `int`, `text`, `bool` or `null`. Undeclared parameters keep native
  inference.
- `--max-input-bytes N` bounds the encoded CSV including its header. The
  default is 16 MiB, and the decoder's hard ceiling of 64 MiB still applies.
- `--max-changes N` is ONE allowance for effects, new vertices and new edges
  across the whole file, from 1 to 1,000,000 (default 100,000). It never resets
  per record.
- `-` reads stdin, and at most one input may use it.

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
  unless `--ann <ef_search>` asks for the approximate HNSW walk.
- Both lanes: exact reciprocal-rank fusion of `--candidates` hits per lane
  (default `--k`). This fuses two candidate sets. It is not an exhaustive
  hybrid answer.
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

A lane that did not rank a hit leaves its fields `null`. These are usage errors
(exit 2) before the database opens:

- no lane;
- a lane flag without its lane;
- a vector whose length differs from its `--vector-property` count;
- a non-finite coordinate;
- `--candidates` without both lanes;
- `--max-expansions` without a fuzzy mode;
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
fgdb --robot import-csv --db ./graph --key-file ./graph.keys --label Person=1 --property name=1 \
  --input people.csv 'INSERT (:Person {name: $name})'
fgdb --robot query --db ./graph --key-file ./graph.keys --label Person=1 --property name=1 \
  'MATCH (p:Person) RETURN p.name AS name ORDER BY name'
fgdb --robot compact --db ./graph --key-file ./graph.keys
```

Counts, wide integers, identities and diff counters are decimal strings, never
rounded JSON numbers. Exit codes: `0` success, `2` usage, `3` query (including
admission and execution refusals), `4` open (including key-file and wrong-key
failures), `5` io.

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

The frozen schema lives once, in `tests/support/robot_schema.rs`. To change the
schema, edit it and `ROBOT_SCHEMA` in `src/main.rs` in the same commit.
