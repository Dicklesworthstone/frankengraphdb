# Bolt writes and explicit transactions

The Bolt adapter extends its declared read profile with bounded native
autocommit writes and explicit write transactions. HELLO advertises
`fgdb_autocommit_writes: true` and
`fgdb_explicit_write_transactions: true`. These flags describe the implemented
subset; they do not claim complete Neo4j or Bolt V2 conformance.

## Autocommit execution

An authenticated client with signed Write or ReadWrite rights can issue native
CREATE, INSERT, MERGE, MATCH mutations, write chains and parameterized UNWIND
programs outside BEGIN. Omitting `mode` uses Bolt's write routing default;
`mode: "w"` also permits writes. `mode: "r"` never permits a write.

RUN admits and retains one bounded request. It does not publish mutations.
The first valid PULL or DISCARD consumes the request and executes its native
atomic program. RETURN values, including nodes, relationships, paths and nested
collections, are authorized, hydrated from the unpublished final workspace,
and admitted against output limits before the sole native commit. Metadata
is retained with the result; later pages do not query a newer committed state.

For example, with Person/name/age symbols:

```cypher
UNWIND $payload.members AS row
MERGE (n:Person {name:row.name})
SET n.age=row.age
RETURN n
```

DISCARD executes an autocommit write even when the client does not need its
rows. RESET, GOODBYE or disconnect before consumption abandons the queued
request. After consumption, RESET cannot undo a published commit. A retained
request cannot be executed twice.

Terminal success carries the native completion bookmark, database, statement
count and available creation/update statistics. No-RETURN writes emit no
records. Partial PULL retains the exact admitted result and does not repeat
the write.

## Explicit native transactions

BEGIN in write/default mode with signed Write or ReadWrite authority creates
one `AuthorizedWriteTransaction`. A read-only capability, or BEGIN with
`mode: "r"`, retains the existing pinned read session. A routing option never
confers additional authority.

Each RUN in a writable transaction prepares and stages one native statement.
Successful statements share the original snapshot, earlier unpublished
effects, conflict observations and cumulative resource allowance. Ordinary
single CREATE, INSERT, MERGE, SET, REMOVE and DELETE statements use the same
chain compiler as compound native writes. Standalone reads use the existing
authorized graph/set reader over that private workspace.

A transaction can therefore perform the following separate RUN statements:

```cypher
CREATE (n:Person {name:$name, age:$age}) RETURN n
```

```cypher
MATCH (n:Person {name:$name}) SET n.age=n.age+1 RETURN n
```

```cypher
MATCH (n:Person {name:$name}) RETURN n.name, n.age
```

The transaction sees its own changes. Other connections continue to see only
committed state until COMMIT succeeds.

RUN/PULL/DISCARD results are **provisional**. Statement summaries carry
`fgdb_transaction_local: true` and no commit bookmark. Their counters describe
that statement, not a durable commit receipt. PULL and DISCARD consume output;
neither publishes the workspace. Consume or discard every outstanding result
before COMMIT. The existing query-ID cursor owner supports multiple pending
results with bounded cursor count and retained memory.

Only COMMIT invokes native completion and returns its bookmark. Subscription
notifications follow release of the writer guard. No database lock is retained
between protocol messages. ROLLBACK, RESET, GOODBYE and disconnect discard an
uncommitted workspace. Any failed statement, output admission, cancellation,
authority refusal or stale generation aborts the complete unpublished prefix.

An explicit `mode: "r"` on RUN narrows that request to a read. A write under
that ceiling fails and aborts the transaction. An absent RUN mode inherits the
owner selected by BEGIN; `mode: "w"` cannot widen a read-only owner.

## Authority, resources and recovery

The native owner retains one live capability permit. Source work, evaluator
work, result rows, creation, mutation and statement limits apply cumulatively
across statements. Field masking, graph metadata and output conversion reuse
the existing authorized collectors. A final query-cancellation and live-permit
check follows each successful output-admission callback, before provisional
rows are released or the active owner is restored.

Every active request enters the retained database generation and rechecks it
after waiting for a writer guard. An idle transaction holds no active-operation
permit or database lock. Recovery or reopen fences its old generation; a later
request cannot silently attach the old workspace to the replacement database.

Bookmarks use `fgdb:<database>:<sequence>`. BEGIN checks the maximum requested
sequence against the native owner's actual basis. Future bookmarks refuse
promptly; the adapter has no causal wait queue. A RUN cannot switch databases
or use a newer bookmark to advance an existing transaction snapshot.

The current native owner conservatively refuses a subsequent statement if
another commit has advanced its basis. Final COMMIT uses the existing native
conflict/completion validator. This is the implemented bounded transaction
model, not a full SSI or concurrent-snapshot qualification.

Issuer and generation fences also protect physical socket writes. A lost
COMMIT response does not prove rollback. Unknown native outcomes retain their
non-retryable classification; the server never automatically replays COMMIT or
the statement prefix.

## Current boundaries

Explicit writer reads use the supported native graph/set subset. Standalone
terminal aggregate reads such as `RETURN count(*)` or `RETURN sum(n.p)`,
procedure hosts such as `CALL fnx.*`, hybrid retrieval and catalog calls,
historical or branch selectors unsupported by that owner, and other unsupported
syntax refuse and discard the entire staged prefix. They do not fall back to
a committed-state reader or autocommit path. Explicit `mode: "r"` transactions
retain the existing broader read-session behavior, including aggregate and
procedure reads over committed snapshots.

Durable ownership leases, reconnect/reattach, durable result delivery,
impersonation and non-null `tx_timeout` are outside this extension. The
bounded write-chain compiler and existing authorization/resource contracts
continue to determine which statements are accepted.

## Regression sources and qualification

The existing native explicit-transaction target covers shared unpublished
state, one final commit, rollback/drop, authority, quotas, output failures,
foreign handles and stale bases. Added output-boundary regressions invalidate
authority or interrupt execution immediately after successful metadata
hydration and require the entire staged prefix to be discarded.

Bolt state-machine regressions cover provisional output, own-writes,
single-commit publication, rollback/reset/GOODBYE/drop, read and write rights,
mode narrowing, cursor budgets, generation fencing and cumulative limits.
The real Bolt TCP tests use an independent FGP observer to distinguish
provisional state from committed state.

These changes have undergone source and API review. The execution environment
became unavailable during dependency setup. Formatting, compilation, runtime
tests, Clippy, UBS and the full exact-tree proof are therefore **UNRUN** for
this source cut. No broader roadmap bead is closed by this implementation.

Focused verification targets, in the repository's pinned execution lane:

```text
cargo test -p fgdb --test authorized_explicit_transactions
cargo test -p fgdb-server --lib bolt::
cargo test -p fgdb-server --test bolt_autocommit
```

These focused targets do not replace the repository's complete mandatory
checks and exact-tree proof.
