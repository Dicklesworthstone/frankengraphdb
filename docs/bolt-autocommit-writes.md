# Bolt no-RETURN autocommit writes

This is a bounded extension to the existing `BoltCompatProfileV1` adapter.
The HELLO success metadata advertises `fgdb_autocommit_writes: true`. It is
not a claim of full Neo4j compatibility or explicit write-transaction support.

## Supported execution boundary

An authenticated client with signed Write or ReadWrite rights can issue a
native no-RETURN write with RUN outside BEGIN. Omitting `mode` uses the Bolt
default; explicit `mode: "w"` also permits this path. `mode: "r"` never does.
Existing explicit transactions remain pinned read-only sessions, even when
BEGIN contains write mode. A driver-managed explicit `execute_write` callback
is therefore not supported by this extension.

The ordinary native write compiler owns syntax. Its supported CREATE, INSERT,
MERGE, MATCH mutations, no-RETURN scripts and parameterized UNWIND batches use
the same capability-authorized writer as FGP/HTTP, without a second executor.
For example, with operator-declared Person/name/age bindings:

```cypher
UNWIND $payload.members AS row
MERGE (n:Person {name:row.name})
SET n.age=row.age
```

RUN returns an empty `fields` list and retains the bounded request. It does
not expand argument rows, allocate graph identities, open a transaction or
publish mutations. Definition admission is not full binding: argument shapes,
values and downstream UNWIND types can still be refused at consumption.
The first valid PULL or DISCARD consumes the request and executes one atomic
native program. There are no result records. Terminal success reports
`type: "w"`, the native completion bookmark, the database name and
`fgdb_statements`; it does not invent Neo4j property/node counters.

DISCARD **executes** the write. RESET, GOODBYE or disconnect **before**
consumption abandons it. The retained request cannot be consumed twice, and an
execution failure enters the existing Bolt failure state until RESET. This is
not durable exactly-once delivery or reconnect deduplication. Native argument
admission, statement limits, signed work limits, field scopes, cancellation,
commit outcome classification and subscription notifications remain owned by
the shared executor.

## Bookmarks, recovery and authority

Local bookmarks of the existing form `fgdb:<database>:<sequence>` are accepted.
Their maximum sequence is checked under the write lock, before any mutation.
A local bookmark ahead of the current frontier is refused without executing;
this adapter does not wait for a future sequence. Foreign, non-canonical and
malformed bookmarks are refused rather than ignored.

A queued write captures its original recovery generation. Consumption enters
that generation and checks it again after acquiring the write lock. A recovery
or reopen cannot cause the same queued request to be applied to the replacement
generation. Idle queues retain no active-operation permit or database lock.

Write authority is checked at admission and again by the native executor at
consumption. Existing issuer and generation fences still protect physical
socket writes and flushes. Losing the terminal response does **not** prove
rollback: a native unknown commit outcome remains a non-retryable database
error, and the adapter never automatically replays the request. RESET after
consumption cannot undo an already-published commit.

## Explicit refusals

Write RETURN is not supported: the deferred path never invokes the returning
executor or attempts to hydrate graph rows after committing. Explicit write
transactions, impersonation and non-null `tx_timeout` options are not enabled.
Write statements in explicit transactions refuse rather than silently becoming
autocommit operations. Native read queries retain priority over write-like
words in aliases, literals, labels and comments; syntax admission is not based
on a keyword match alone.

## Regression sources and validation status

The three definition-probe tests live in
`crates/fgdb-gql/src/native_write_text.rs`.
The eight tests in `crates/fgdb-server/src/bolt/autocommit/tests.rs` cover
metadata admission, signed budgets, retired issuers, one-time consumption,
durable recovery, a stale queued request and recovery while PULL waits for
the writer lock. They use the existing real Chronicle/UnixVfs fixture under
the deterministic host scheduler, not a substitute graph model.

The five tests in `crates/fgdb-server/tests/bolt_autocommit.rs` use real Bolt TCP
framing and a separate FGP observer to cover document-root upserts, bookmarks,
DISCARD, RESET, raw disconnect, write-only authority with zero returned-row
allowance, read boundaries, native read dispatch and whole-script rollback
after a later arithmetic failure. Cargo discovers this standalone integration
target without changing the existing loopback suite or any gate.

These source changes and regression tests have **not** been compiled or run
in the authoring environment. There is no Rust toolchain or dependency
checkout there. Source review and blob/whitespace checks are not runtime
evidence. Run the repository's pinned formatting, compilation and test gates
before release. The focused test targets are:

```text
cargo test -p fgdb-gql --lib native_write_text::tests
cargo test -p fgdb-server --lib bolt::autocommit::tests
cargo test -p fgdb-server --test bolt_autocommit
```

Use the repository's configured execution lane and dependency checkout for
these commands; they are not a replacement for the full exact-tree proof.
