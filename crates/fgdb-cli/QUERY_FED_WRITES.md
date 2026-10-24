# Atomic writes from a query

`fgdb write --from-query` composes a native read with a native write script in
one transaction. The read's projection names supply the script's parameters;
row values are never interpolated into statement text. This interface exposes
`PreparedGraphQueryWrite` through the CLI. It is not the single-statement
`MATCH ... MERGE` grammar.

## Example: derive cities from people

For an existing database containing `Person` vertices with `bornIn` properties:

```sh
fgdb --robot write --db ./graph --key-file ./keys \
  --label Person=1 --label City=2 \
  --property bornIn=1 --property name=2 --property seen=3 \
  --from-query 'MATCH (p:Person) RETURN p.bornIn AS city' \
  --feed-type city=text --max-batch-statements 1000 \
  'MERGE (c:City {name:$city})
   ON CREATE SET c.seen=1
   ON MATCH SET c.seen=c.seen+1'
```

Use the same symbol bindings that created the database. The numbers above are
an example, not an implicit catalog. The key file follows the ordinary CLI
contract: three key lines, owner-only permissions on Unix, and independently
generated keys rather than test-fixture keys.

The source runs once against the transaction's initial workspace. Its complete
result is bound before any script executes. Every source occurrence invokes
the script, including duplicate projection values. For input Paris, Rome,
Paris, the first Paris occurrence creates a City and the later occurrence
matches that same City and executes `ON MATCH`. A script that also creates new
Person vertices does not enlarge or rerun the already-frozen source.

All script instances finish in one transaction. A source, binding, resource or
execution refusal publishes no successful prefix. Completion failures retain
the engine's native outcome classification: an unknown or committed outcome is
not a rollback guarantee and must not be blindly retried.

## Parameters and types

`--param name=value` binds the **source read only**. To pass a global parameter
into the script, explicitly project it under the script parameter's name. For
example, source `RETURN $key AS key` with `--param key=int:7` supplies `$key` to
script `MERGE (c:City {name:$key})`. Source `RETURN $key AS other` does not.

Each script parameter must name a source projection; extra projections are
allowed. Script variables are local to each invocation. A script's own MATCH
clauses see the earlier script instances' staged effects.

`--feed-type name=type` declares the **script** parameter type, independently of
source parameter declarations. The CLI accepts `int64`, `uint64`, `int`, `text`,
`bool`, `null`, `float`, `decimal`, `bytes`, `list` and `map`. Undeclared script parameters keep
the native compiler's inference, usually `int64` for a property parameter. Types
are not inferred from the first source value. Wrongly typed values are refused,
not silently coerced. A missing or statically incompatible source projection
refuses during preparation; a dynamically incompatible value refuses when all
source rows are bound, before any script executes. Collection parameters retain
their native typed values; they are not flattened, encoded as JSON text or
interpolated into a statement. For example, source
`RETURN {key:'Paris'} AS doc,[1,2] AS items` can feed
`CREATE (:City {name:$doc.key,seen:size($items)})` with
`--feed-type doc=map --feed-type items=list` and the corresponding City/name/seen
symbol bindings.

Nested collections retain native structural, transcript, work and scratch
limits. A nullable collection argument remains NULL, not an empty collection;
`size(NULL)` therefore remains NULL instead of zero. A late wrong-shaped value
refuses the whole batch before any script runs. Collection feed declarations
are step-local inside ordered transactions, just like scalar declarations.

## Admission and budgets

`--max-batch-statements N` bounds the **total expanded statement count**, not
statements per row. Three input rows and a two-statement script require capacity
for six statements. The default is the native batch ceiling; the flag may lower
but cannot raise that ceiling. Excess input is refused, never silently truncated
with an implicit LIMIT. Zero admits only an empty source.

`--max-snapshot-records`, `--max-result-rows`, `--max-work-units` and
`--max-scratch-entries` cover the source, argument binding and the entire write
program cumulatively. No phase or input row obtains a fresh allowance. In
particular, source output rows and subsequent write selections share the
selected-row allowance even though this command prints no source rows. The
ordinary write caps for mutation effects and created vertices/edges also apply.
These are logical execution limits, not allocator-byte, process-memory or
wall-clock bounds. Source rows and bound programs are resident; this is not a
streaming or spill-backed input path.

## Output and empty input

Only after successful completion does robot mode emit the ordinary terminal
`result` with `kind: "written"`, `seq`, `records` and `statements`. `records` is
the number of source occurrences; `statements` is the number of completed
expanded script statements. No source row or tentative write receipt is printed.
The invocation frame precedes execution as on other robot commands.

An empty source closes read-only without creating a write marker: the original
sequence is reported with zero records and statements. A transport failure
while reporting an already completed write does not undo that write or cause an
automatic retry. Human mode reports the same counts and sequence in one line.

## Ordered transactions and savepoints

Attach `--from-query`, `--feed-type` and `--max-batch-statements` to the
preceding `transaction --write` step. Flags after a read or savepoint step, or
before any write, are usage errors. Each step has independent source parameters
and script type declarations. For example, this derives cities from people
created earlier in the same transaction:

```sh
fgdb --robot transaction --db ./graph --key-file ./keys \
  --label Person=1 --label City=2 \
  --property bornIn=1 --property name=2 --property seen=3 \
  --write "CREATE (:Person {bornIn:'Paris'}),(:Person {bornIn:'Rome'})" \
  --savepoint people \
  --write 'MERGE (c:City {name:$city})
           ON CREATE SET c.seen=1 ON MATCH SET c.seen=c.seen+1' \
  --from-query 'MATCH (p:Person) RETURN p.bornIn AS city' \
  --feed-type city=text --max-batch-statements 10 \
  --query 'MATCH (c:City) RETURN c.name AS city,c.seen AS seen ORDER BY city'
```

Both definitions are prepared before the transaction begins. The source executes
at its step against the current staged graph, not in a separate read session.
Every source row binds before that step's script batch stages. Later steps see
the batch's effects, but no step commits independently. A late refusal aborts
the entire invocation, including earlier successful writes and buffered rows.

`--rollback-to people` would discard the derived cities and subsequent buffered
records/rows while keeping the earlier Person creations. `--release people`
keeps the effects. `--rollback` discards everything. Conflict observations and
issued identities retain their native semantics.

The existing **64 native statement** limit is not widened: each source costs
one statement, and each expanded script statement costs one. All fixed steps
and all source reads are reserved during preparation; a dynamic batch receives
only the remaining capacity, intersected with its own `--max-batch-statements`.
Neither savepoint rollback nor release refunds that capacity. A source that
would exceed it refuses rather than running a truncated prefix. Even an empty
source costs one statement; it creates no marker by itself.

The 1 MiB transaction input cap includes source text and all query-fed option
values. Native execution allowances remain per step, with one cumulative
source/binding/program allowance inside each query-fed step. Internal source
rows are not delivered rows and do not spend the transaction's public-row
buffer. Step records still spend the 16 MiB encoded-output allowance before
completion. A query-fed step's `kind: "write"` record reports source-plus-expanded
`statements`, but emits no columns, source rows or standalone result. The sole
terminal result retains ordinary transaction completion and transport semantics.

## Scope

These flags apply to standalone `write` and ordered `transaction --write`
steps, not `query` or `remote`. `--from-query` cannot combine with `--rows`. The script is
an ordinary native write script without RETURN; unsupported source/script syntax
refuses rather than falling back to another execution family. Full resolver
metadata, including reverse names and the optional pinned time-zone artifact,
is forwarded to both native compilers.

The subprocess regression source is `tests/query_fed_write.rs`; preparation and
post-commit transport coverage is in `src/query_write/tests.rs`. Ordered-driver
coverage is in `src/transaction/query_write_tests.rs`. These tests
must be run with the repository's pinned toolchain. Their presence is not a
claim that compilation, runtime or TCK acceptance has passed.
