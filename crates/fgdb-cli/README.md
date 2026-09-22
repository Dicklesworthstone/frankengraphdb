# fgdb local-owner CLI

`fgdb-cli` owns the `fgdb` binary. It uses the real `fgdb::Database`, the
production Asupersync runtime and purpose-narrowed contexts. It has no alternate
in-memory engine and is not a network client or remote RBAC boundary.

Implemented commands: `init`, native read `query` (including native `EXPLAIN`),
and `compact`. Use exclusive local access to the database directory, as required
by the embedded owner; the CLI does not introduce cross-process coordination.
Do not treat this command as a sandbox for users who possess the database keys.

## Build and use

```sh
cargo build -p fgdb-cli
cargo test -p fgdb-cli
```

Keys are supplied through a **regular binary file of exactly 96 bytes**:
`k_oid[32] || DatabaseSecurityNamespaceId[32] || dek[32]`. The database's existing
key-management owner must supply the correct keys when reopening. On Unix the
file must have no group or other permission bits (normally mode `0600`). Keys
are not accepted on the command line or from environment variables. Creating a
new database requires independently generated cryptographic keys, not the test
keys in the integration suite. The CLI does not generate or persist keys.

```sh
fgdb init --db ./graph --keys-file ./graph.keys --format ndjson
printf 'relation\tKNOWS\t1\n' > symbols.tsv
printf 'MATCH (a)-[:KNOWS]->(b) RETURN b\n' > query.gql
fgdb query --db ./graph --keys-file ./graph.keys \
  --symbols-file symbols.tsv --query-file query.gql --format ndjson
fgdb compact --db ./graph --keys-file ./graph.keys --format ndjson
```

The example uses relation ID `1` only because `symbols.tsv` explicitly maps it;
there are **no default guessed schema IDs**. `query` refuses unsupported or
write syntax through the native binder. It never retries a failed read as a
write. `--query-file -` reads one bounded UTF-8 statement from stdin. Files and
stdin are capped at 65,536 bytes, including before UTF-8 decoding.

A symbols file has one `kind<TAB>name<TAB>decimal-id` per line, where kind is
`relation`, `label`, or `property`. Duplicate names in the same domain refuse.
Label and relation IDs must also have a unique canonical name within their
domain; ambiguous aliases refuse. The full catalog is supplied to native
`labels()`/`type()` reflection, including names absent from the query text.
Names are case-sensitive; no dynamic schema catalog is implied. Blank lines
are ignored. A parameters file uses `kind<TAB>name<TAB>value`; supported kinds
are `int64`, `uint64`, `text` (UCS_BASIC), `bool` (`true`/`false`) and `null`
(empty value). Text consumes the remaining line literally, including tabs and
quotes. Parameter names are passed to `GqlParameters`, never interpolated into
query text; the statement must use the native grammar's parameter declarations.

## FgdbCliResultV1

`--format ndjson` emits a columns record, zero or more row records and a required
completion record. `init` and `compact` emit one completion record. Known-format
errors emit a single error record; argument-parse errors go to stderr because a
valid format has not yet been selected. stderr contains stable codes only.

```json
{"version":1,"type":"columns","columns":["b"]}
{"version":1,"type":"row","values":[{"type":"vertex","value":"2"}]}
{"version":1,"type":"complete","rows":1}
```

Counts (`u64`), integers (`i128`), vertex/edge identities (`u128`) and exact
average numerator/denominator are decimal **strings**, never rounded JSON
numbers. Types remain disjoint. Scalars use the existing STRICT_PORTABLE codec
as `{"type":"scalar","encoding":"strict-portable-v1","hex":"..."}`.
Composite graph cells use the engine's `GraphValue::canonical_bytes` as
`{"type":"graph","encoding":"graph-value-v1","hex":"..."}`. Hex is an
explicit lossless representation, **not redaction or encryption**. Text
collation and timestamp bindings are not lost by conversion to JSON strings.
Human output shows an escaped tabular form with the same exact domains; complex
cells currently use those canonical hex representations rather than a rich
pretty-printer. This is not an FGP body encoding.

Native admission defaults are 1,000,000 snapshot records/work units/scratch
entries and 10,000 result rows. `--max-rows` and `--max-work` set finite logical
limits; these are not wall-clock or exact allocator-byte limits. The complete
encoded response defaults to at most 8 MiB (`--max-output-bytes`, maximum 64
MiB), with at most 1 MiB per record. Before allocating canonical temporary
bytes, each cell must also fit the native 4,096 logical payload-unit bound
(one per nested cell plus one per additional 64 bytes of variable payload).
This separate admission bound is intentionally not an exact allocator-byte or
query-peak-memory claim. All rows are rendered before any stdout record is
released. Input validation and success-envelope sizing precede `init`/`compact`
effects. Consumers must still require `complete`: a broken pipe or process
termination can interrupt OS output. No automatic retry follows any failure,
especially one after a mutating operation may have committed.

Exit status: `0` completed; `2` usage/input/key refusal; `3` query/output-limit
refusal; `1` database/runtime/output failure. These are local CLI results, not
Fabric durable-result ACKs or transaction-outcome tokens.

## Verification and remaining surfaces

The Rust tests cover real subprocesses and durable create/write/query/compact/
reopen, read-only refusal, keys, bounded output, typed parameters and exact
serialization. They were **authored but not run** in the implementation session:
its environment had no Rust toolchain. Compile, rustfmt and the tests still
need to run on a Rust-equipped checkout; Cargo must regenerate the workspace
lockfile for this newly added package and the protocol transport feature.

Not implemented here: remote FGP connections, sessions/transactions, mutations,
maintenance beyond compaction, interactive shell, external scalar-artifact
resolvers, or network authentication. Missing functionality is refused, not
silently approximated.
