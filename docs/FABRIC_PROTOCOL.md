# Fabric protocol mechanisms

Owner beads: `fgdb-w10-fgp-core-5b1`, `fgdb-w10-flow-send-obligations-smd`,
`fgdb-w10-fgp-frame-catalog-j1bl`, `fgdb-w10-server-rte`, `fgdb-t79am`.
These beads remain open.

`fgdb-protocol` implements the transport-independent framing, connection-state,
flow-credit and drain mechanisms. It is a std-only, unsafe-forbidden crate. It
neither opens a socket nor links an alternative networking runtime; listeners
and adapters must consume the pinned asupersync implementation.

## Implemented byte profile

All integers are unsigned, fixed-width, big-endian. Length includes the header.
There is no padding, compression, implicit decompression, extension-skipping,
or FEC in this profile. Unknown versions, frame kinds and flags are fatal.

| Offset | Bytes | Field |
|---|---:|---|
| 0 | 4 | Inclusive frame length |
| 4 | 2 | Version (1) |
| 6 | 2 | Frame kind (below) |
| 8 | 2 | Header class: transport=0, session=1, ready=2 |
| 10 | 8 | Request ID |
| 18 | 16 | Opaque server-minted stream ID; zero is connection control |
| 34 | 32 | Session transcript binding (session and ready headers only) |
| 66 | 8 | Authentication-context generation (session and ready only) |
| 74 | 32 | Database security namespace (ready only) |
| 106 | 32 | Database incarnation (ready only) |
| 138 | 8 | Service epoch (ready only) |
| 146 | 1 | Posture: local=0, sharded=1 (ready only) |
| 147 | 32 | Selected authority commitment (ready only) |

Payload begins at byte 34, 74 or 179 respectively. A binding is a public
comparison value, **not** a credential, selection receipt or Operational-root
proof. The authority service must validate the underlying evidence and mint
these values; callers cannot authenticate merely by constructing the structs.

| Kind | Code | Direction |
|---|---|---|
| HELLO / HELLO_ACK | 0x0001 / 0x0002 | client / server |
| AUTH / AUTH_OK | 0x0003 / 0x0004 | client / server |
| SELECT_DATABASE / READY | 0x0005 / 0x0006 | client / server |
| AUTH_REFRESH / AUTH_REFRESHED | 0x0007 / 0x0008 | client / server |
| DRAIN / GOODBYE | 0x0009 / 0x000a | client / server |
| ERROR | 0x000b | server |
| PREPARE / PREPARED | 0x0010 / 0x0011 | client / server |
| EXECUTE / QUERY_CANCEL | 0x0012 / 0x0013 | client |
| RESULT_CHUNK / RESULT_END | 0x0014 / 0x0015 | server |
| RESULT_ACK / RESULT_RELEASE | 0x0016 / 0x0017 | client |
| SNAPSHOT_RESULT_CHUNK / SNAPSHOT_RESULT_END | 0x0018 / 0x0019 | server |
| WINDOW_UPDATE | 0x001a | client |
| SUBSCRIPTION_BATCH | 0x001b | server |
| SUBSCRIPTION_RESET | 0x001c | server |
| EXECUTE_PREPARED | 0x001d | client |
| RELEASE_PREPARED / PREPARED_RELEASED | 0x001e / 0x001f | client / server |
| PING / PONG | 0x0020 / 0x0021 | client / server |
| EXECUTE_BATCH | 0x0022 | client |

These are the implemented mechanism-profile tags, not an assertion that every
Appendix D frame body and its generated registry have been implemented.
Bodies remain opaque to the framing layer. Complete typed body codecs,
capability negotiation, the generated authoritative frame catalog and adapter
parity evidence are still required before advertising public FGP conformance.
Never serialize Rust Debug output as a body or expose these tags as an
unauthenticated query endpoint.

## Receive ownership

The decoder keeps only a fixed 179-byte header before validation. The first
four bytes enforce the encoded length cap immediately. It validates the entire
session/ready binding through the caller's validator before reserving payload
memory. Allocation is fallible. Authentication payload bytes are excluded from
Debug output. Per-connection and global budgets remain the listener's job;
a frame limit alone is not a global-memory bound.

`Decoder::decode` returns at most one frame and the exact consumed byte count.
Process that frame's state transition before presenting the unconsumed suffix:
a coalesced AUTH + SELECT cannot read ahead past authentication. Clean EOF is
valid only between frames. Any malformed/truncated frame poisons its decoder;
there is no resynchronization by scanning untrusted payload bytes.

`Connection::validate_client_header` enforces direction, phase, complete binding,
nonzero request IDs and control/child-stream distinction. EXECUTE, EXECUTE_BATCH, PREPARE,
EXECUTE_PREPARED and RELEASE_PREPARED use the control stream; child stream IDs
are minted by the server. Cancellation
and credit address admitted streams. ACK/release may address independent durable
owners, which the owning service must authenticate and validate separately.

## Send and flow ownership

Each stream has a byte-and-row window. Bytes include the encoded header.
WINDOW_UPDATE is sequential and duplicate-safe: the immediately repeated update
must exactly match, and it never grants credit twice. Out-of-order updates and
arithmetic overflow leave both counters unchanged. Reserve a separate bounded
control window so saturated results cannot starve cancellation or drain.

A reservation is linear. Dropping or explicitly cancelling before the first
write refunds its bytes/rows. After write-start, failure or cancellation does
not refund uncertain bytes. A successful send requires explicit completion;
none of these paths acknowledges a result or advances a durable cursor.

Every queued send also has an individually tracked, connection-scoped
`SendTicket`. A duplicate or foreign completion cannot discharge a different
send. Dropping a ticket does not make the connection drained.

Immediately before the first physical write, the adapter must obtain a fresh
public-frame send guard from the authority service. That service must revalidate
revocation, current root/epoch, policy and privacy state. Queue-time validation
is insufficient. This crate supplies accounting, **not that authority guard**.

## Drain ownership

Drain freezes the last admitted child generation and rejects new children.
Each child must report the matching generation and a terminus appropriate to
its owning lifecycle. The eight handoff categories correspond to Appendix D;
they are reports from owners, not fabricated durable evidence. Wrong-generation
or wrong-kind completion cannot remove an obligation.

GOODBYE is authorized only after all admitted children have handed off and all
queued sends are Sent, CancelledBeforeWrite or Failed. Socket loss, timeout and
Drop are deliberately not durable terminal categories. Retained result and
subscription owners remain outside the connection tree, so shutdown never waits
for an offline client's ACK and never releases its output implicitly.

## Validation

Run `cargo test -p fgdb-protocol --features transport`. The suite covers all
header classes at every fragmentation boundary, one-byte feeds, coalesced
frames, pre-allocation binding refusal, length/tag/flags/version checks,
truncated EOF, debug redaction, handshake ordering, refresh fencing,
generation-safe drain, duplicate/foreign send completion, credit replay, and
uncertain-write accounting. The 11 tests in `tests/transport.rs` drive the frame
I/O layer and compile only with the `transport` feature.

The tests were written without a Rust toolchain. They have since run:
`cargo test -p fgdb-protocol` passed 16 of 16 on 2026-09-28 at `61aeeb0f`
(pinned toolchain). Until 2026-10-05 no build had enabled `transport`, so its 11
tests had never run. With `--features transport`, all 27 passed on 2026-10-05,
and clippy was clean. Since then `scripts/check.sh` lints and tests with
`--all-features`. No runtime, performance or full-protocol conformance claim is
made beyond that suite.

## Typed operation bodies

`fgdb_protocol::body` gives the frames `fgdbd` serves an exact canonical body
encoding: fixed-width big-endian integers, `u32`-length-prefixed UTF-8 text
and byte strings, explicit tags on every closed union, and counts that are
bounded before any allocation. Decoding refuses trailing bytes, every
truncated prefix, unknown tags, non-UTF-8 text, a non-0/1 boolean byte, an
unsorted or duplicated map key or parameter name, a zero-denominator average,
nesting deeper than 64, and more than 2^22 value nodes. A credential is
redacted from `Debug`.

| Frame | Body |
|---|---|
| HELLO | `min_version u16`, `max_version u16`, `client_nonce [32]`, `max_frame_len u32` |
| HELLO_ACK | `version u16`, `server_nonce [32]`, `max_frame_len u32`, `initial_window_bytes u64`, `initial_window_rows u64` |
| AUTH | `mechanism u8` (1 = Warden capability), `credential bytes` |
| AUTH_OK | `session_transcript [32]`, `auth_generation u64` |
| AUTH_REFRESH | `mechanism u8` (1 = Warden capability), `replacement_credential bytes` |
| AUTH_REFRESHED | unchanged `session_transcript [32]`, successor `auth_generation u64` |
| SELECT_DATABASE | `name text` |
| READY | `namespace [32]`, `incarnation [32]`, `service_epoch u64`, `posture u8`, `authority_commitment [32]`, `frontier u64` |
| EXECUTE | `mode u8` (0 read, 1 write, 2 subscribe), `statement text`, `parameters [(name text, value)]` (names strictly ascending) |
| EXECUTE_BATCH | `statement text`, `argument_sets u32`, then that many parameter maps (each `count u32`, then strictly ascending `(name text, value)` pairs) |
| PREPARE | `statement text`, representative `parameters [(name text, value)]` (names strictly ascending) |
| PREPARED | nonzero connection-owned `handle [16]` |
| EXECUTE_PREPARED | `handle [16]`, execution `parameters [(name text, value)]` (names strictly ascending) |
| RELEASE_PREPARED / PREPARED_RELEASED | `handle [16]` |
| SNAPSHOT_RESULT_CHUNK | `columns: none \| [text]` (first chunk only), `rows [[value]]` |
| SNAPSHOT_RESULT_END | `outcome` (`Rows{seq}`, `WriteCommitted{seq,statements}`, `ReadClosed{seq,statements}`), `rows u64` |
| SUBSCRIPTION_BATCH | `frontier u64`, `snapshot bool`, `last bool`, `columns: none \| [text]` (first frame only), `entries [(weight i128 ≠ 0, [value])]` |
| SUBSCRIPTION_RESET | `has_checkpoint bool`, then `last_delivered_seq u64` only when true |
| ERROR | `code u16`, `message text` (structural diagnostics only) |
| WINDOW_UPDATE | `sequence u64`, `bytes u64`, `rows u64` |
| PING / PONG | `nonce u64` |
| DRAIN / GOODBYE / QUERY_CANCEL | empty |

A value is one tagged node of the engine's value lattice, mirroring the CLI
robot contract's cell types exactly: null, bool, int (i64), float (binary64
bits), decimal (canonical text), text, bytes, timestamp (UTC nanoseconds,
offset seconds, optional zone identifier plus tzdb object id), vertex, edge
(u128 identities), path, vertices, edges, list, map, count, wide integer
(i128) and exact average. Error codes are the closed set `protocol`,
`unsupported_version`, `unauthenticated`, `not_found_or_unauthorized`,
`statement`, `permission_denied`, `budget`, `conflict`, `execution`,
`outcome_unknown`, `busy`, `draining`, `cancelled`, `database_recovering`
(tag 14) and `database_unavailable` (tag 15).

## fgdbd: the served subset

FGP connections may narrow their authority between statements with
`Client::refresh_authority(cx, credential)`, before or after database selection.
The replacement and current tokens must both authenticate under the same served
database issuer and remain live. Every effective restriction must be equal or
narrower: branch, rights, label clauses, relations, visible properties, work/node/
row ceilings and validity window. Label clauses remain conjunctive any-of tests;
property denials participate in the comparison. Independently issued credentials
are accepted only when their effective authority satisfies these same checks.
This does not renew expiry or restore a permission removed by an earlier refresh.

Each accepted refresh preserves the transcript and all selected database fields,
advances the authentication generation exactly once, and fences old headers.
AUTH_REFRESH uses the old binding; AUTH_REFRESHED uses the successor binding in
both header and body. The client checks that exact successor before accepting the
body. A refused replacement receives `unauthenticated` under the old binding and
leaves the connection unchanged. Malformed framing remains a protocol error.
The new credential is rechecked for expiry and issuer retirement at every
physical response write/flush, including before selection. A lost response
requires reconnecting; this ephemeral operation has no durable retry record.

An active result or subscription receives `busy` for AUTH_REFRESH. Finish or
cancel that child before refreshing. Existing permits and their counters are
never replaced or reset; subsequent executions use fresh per-execution permits
under the narrowed ceilings. This quiescent profile does not claim authority
replacement for an active child, lifetime quotas or durable revocation.

`crates/fgdb-server` composes the embedded engine behind this machine over
asupersync TCP or TLS 1.3. The `fgdbd` binary serves one or more databases; `fgdbd
token` mints capability tokens and `fgdbd keygen` writes owner-only key files.
The CLI's `fgdb remote` and `fgdb_protocol::client::Client` are its clients.

TLS is configured with the paired `fgdbd serve --tls-cert-file <chain.pem>
--tls-key-file <key.pem>` options, or the embedded host's
`TlsConfig::from_pem_files` and `Server::enable_tls`. The identity is validated
at startup; key files must be owner-only on Unix. The setting protects every
configured FGP, HTTP and Bolt listener, with no plaintext retry after a failed
handshake. The foundation owns TLS 1.3, certificate parsing, record protection
and verification; early data is disabled, handshakes have a ten-second limit,
and server drain cancels unfinished handshakes. FGP requires ALPN `fgp/1`,
HTTPS requires `http/1.1`, and Bolt retains its driver-compatible encrypted
magic/version exchange without mandatory ALPN. TLS does not replace Warden.

The CLI pairs `--tls-server-name <name>` and `--tls-ca-file <ca.pem>` on
`fgdb remote`; the explicit CA bundle and hostname must verify before any FGP
credential is sent. Hosts can pass an already verified foundation TLS stream
to `Client::connect_stream`. No disabled-verification option is provided.
For other clients use HTTPS or `bolt+s://`/`neo4j+s://` with normal certificate
verification. Plain TCP remains available when the operator omits TLS config.

TLS preserves physical output fencing: each outer write/flush poll permits
at most one ciphertext write or flush, so buffered TLS records return through
the existing capability authorizer before their next physical I/O attempt.
Reads cannot flush pending protected ciphertext. This continues the existing
cooperative expiry/issuer-retirement fence; it adds no durable audit release
or time-authority evidence. The handshake is completed before protected
application output is admitted.

- **Handshake.** HELLO selects version 1 and the smaller of both frame
  limits. AUTH carries a Warden capability token; a token no served issuer
  accepts, a bad signature and a malformed token share one `unauthenticated`
  refusal, and the connection closes. The session binding is a keyed BLAKE3
  of the HELLO/HELLO_ACK transcript digest and the AUTH body digest under a
  per-process server secret, derived before AUTH_OK is encoded. A database
  that does not exist and one this token may not select share one
  `not_found_or_unauthorized` reply, and the connection stays authenticated.
  READY's incarnation is a keyed digest of the namespace under that secret,
  so every server restart presents a new incarnation; its authority
  commitment binds the namespace and the issuer's policy epoch.
- **Statements.** EXECUTE runs one GQL statement through a capability
  session constructed fresh for that statement from the connection's token,
  so signature, scope, expiry, signed budgets and issuer retirement are
  rechecked per statement, and scope applies before expansion (FG-INV-20). A
  read runs on a read-only authorized session pinned to exactly the frontier
  its END reports; a write statement presented as a read is refused, never
  reinterpreted. A write is one autocommit program through an authorized write
  session; its END reports the commit sequence. A `CREATE`/`INSERT ...
  RETURN` instead runs as one authorized insertion query (ReadWrite rights,
  matched inputs masked before selection) whose projected rows stream before
  the END that reports its commit: the rows come from the creation itself,
  never a rescan, and are released only once the commit is decided. Names
  resolve through the
  operator's bindings for that database, the CLI's `--label/--relation/
  --property` contract, because the engine has no durable catalog yet.
- **Prepared native reads.** PREPARE parses and admits one native read template
  under fresh read authority, charging the statement bytes to its signed work
  allowance before selector parsing or catalog resolution. Representative
  parameter values establish structural operand types; their values do not
  become defaults. Preparation executes no graph query and retains no snapshot
  pin. PREPARED returns a random nonzero 128-bit handle owned by that connection.
  EXECUTE_PREPARED supplies every argument again, binds it as data, and uses the
  existing authorized native executor with fresh scope, expiry and signed
  execution budgets at the current frontier (or the query's explicit historical
  selector). Branch selectors remain restricted to the selected trunk. Writes
  refuse during preparation.

  A connection retains at most 64 templates and 4 MiB of original statement
  bytes; those are template-count/source-text ceilings, not an allocator-wide
  bound on compiled structures. RELEASE_PREPARED releases one handle and
  returns PREPARED_RELEASED with the same handle; unknown or already-released
  handles have the same successful release response. Executing an unknown,
  released or foreign-connection handle produces a uniform statement refusal.
  AUTH_REFRESH discards the entire cache. Database recovery permanently fences
  every old template, including one already borrowed for execution; preparing
  again after recovery creates a new owner. Disconnect releases the cache.

  Prepared reads use ordinary SNAPSHOT_RESULT chunks, exact flow credit,
  cancellation and live send guards. PREPARE, EXECUTE_PREPARED and
  RELEASE_PREPARED refuse busy while another statement or subscription owns
  the connection. They create no transaction owner, durable prepared
  transaction, durable result, or reconnect/resume token.

  The Rust client exposes `prepare_read`, `execute_prepared`,
  `execute_prepared_streaming` and `release_prepared`. For example, after
  selecting the database:

  ```rust
  let handle = client.prepare_read(
      cx,
      "MATCH (n:Person) WHERE n.age >= $min RETURN n.name AS name",
      vec![("min".into(), WireValue::Int(0))],
  ).await?;
  let adults = client.execute_prepared(
      cx, handle, vec![("min".into(), WireValue::Int(18))],
  ).await?;
  let seniors = client.execute_prepared(
      cx, handle, vec![("min".into(), WireValue::Int(65))],
  ).await?;
  client.release_prepared(cx, handle).await?;
  ```
- **Results.** Every result is the session-owned, ephemeral
  SNAPSHOT_RESULT class on a server-minted 128-bit child stream. The first
  chunk carries the columns. Each chunk is sized to the stream's available
  credit (bytes including the header, and rows) when it is assembled, so the
  server never waits for a grant the client could only send after receiving
  that chunk; END is charged too. The client models that credit exactly and
  replenishes it whenever less than one maximal frame or zero rows remain.
  While a stream waits for credit the server keeps reading: WINDOW_UPDATE,
  QUERY_CANCEL (answered with a stream-scoped `cancelled` ERROR), PING and
  DRAIN get through, and a second EXECUTE is refused `busy`. A late
  WINDOW_UPDATE or QUERY_CANCEL for one of the last 64 finished streams is
  accepted and ignored instead of poisoning the connection. If a streaming
  client callback refuses a row, the client sends QUERY_CANCEL, suppresses
  further row callbacks, and drains the original terminal before returning
  the callback error. Draining still validates every request, stream, binding
  and terminal body; it never retries the query.
- **Send guard.** Every write attempt rechecks that the frame carries the
  binding the connection holds at that moment. The only exceptions are
  HELLO_ACK and AUTH_OK on the transport header and READY on the session
  header it completes.
- **Drain.** DRAIN, SIGINT or SIGTERM drains: admission stops, every
  connection is observed at its next receive point (an admitted statement
  finishes under its own rules, so a write commits or refuses first), the
  connection closes, and GOODBYE is its last write. A drain never waits for an
  offline client.
- **Children.** A finished read reports the new `ChildTerminus::
  EphemeralCompleted` (legal for query and subscription children: nothing durable is
  retained, so nothing is detached); a committed write reports
  `SemanticTerminalDurable`.
- **Authoritative recovery.** Each opened database has one recovery child
  owned by the host runtime, shared by all listeners. If a write leaves the
  engine requiring recovery, its write guard fences the served generation
  before releasing the database lock, including when the write future is
  cancelled. New statements refuse `database_recovering`; old sessions and
  queued results cannot become valid again after reopening. The child waits
  for admitted source operations to drain, consumes the old database handle,
  and calls `Database::recover_authoritatively`. A successful reopen serves a
  new generation. Failure or interruption keeps the database fenced with
  `database_unavailable`, without an automatic retry loop. The trusted host
  can inspect `Server::database_status`; the unauthenticated health route does
  not expose recovery diagnostics. Shutdown stops and joins the recovery child.
  Recovery never replays a client statement. A write with uncertain completion,
  or a completed write whose queued result is invalidated, retains
  `outcome_unknown` and a no-replay diagnostic. Every protected physical output
  poll checks its generation as well as its capability. If invalidation finds
  a partially written frame, the connection closes instead of appending a
  terminal control inside that frame.

- **Subscriptions.** EXECUTE with mode `subscribe` and `SUBSCRIBE TO <read>`
  prepares and atomically registers the engine's own maintained query through
  `PreparedNativeRead::prepare_subscription` and `subscribe_replaying`, on a
  server-minted subscription child stream. The first batch is a
  replacement baseline (weights are multiplicities); every later batch is the
  exact bag delta from the previous batch's frontier, as signed (weight, row)
  entries. After each committed write the server wakes caught-up
  subscriptions, which poll under the read lock; a batch is acknowledged to
  the engine only after its last frame is written, and the next delta starts
  at that acknowledged frontier, so a slow subscriber receives coalesced
  deltas, never a backlog or a gap (an unretained delta is replaced by a new
  baseline). Large batches span frames sized to the stream's credit; only the
  final frame sets `last`. An empty delta reports that the frontier moved and
  nothing the query returns changed. QUERY_CANCEL ends the stream with
  SNAPSHOT_RESULT_END at the last delivered frontier (or, if it lands while a
  batch waits for credit, a stream-scoped `cancelled` ERROR); DRAIN and
  shutdown end it the same way. The capability is rechecked before every
  batch, so expiry or issuer retirement ends a subscription. Because
  maintained queries are not masked by capability scope yet, a subscription
  requires a read capability whose scope hides nothing (refused
  `permission_denied` otherwise), and because a registration lives as long as
  the open database, each served database admits a bounded number of
  registrations per opened database generation (default 64; refused `budget`
  beyond). Failed setup, replay activation, first-baseline admission or final
  checks on initial output remove only the newly appended private circuit and restore
  this count. Successful registrations remain until reopen. A successful
  authoritative reopen resets this registration count.

  Signed limits now govern the complete subscription execution. Preparation
  reserves one work grant; its remainder is partitioned across all private
  circuit nodes, the replay sink, the first compressed baseline and wire
  conversion. Both native work and scratch events fit those partitions.
  Graph-node reservations use the circuit footprint and the admitted source
  binding width, with a conservative bound when no tighter width is available.
  Source-free circuits reserve no graph nodes; their owned intermediate rows
  still use the server's native row ceiling. The installed node and replay
  policies retain these ceilings for each later maintenance tick. This is
  conservative accounting, not exact Warden hooks on native graph reads.

  Each poll rechecks live read authority after taking the database lock and
  uses ceilings no larger than the original registration or current token.
  Two native pulls and wire conversion share one work reservation: a replay
  retention gap (`ReplayGap`, `DeltaGap` or `DeltaUnavailable`) can consume the
  reserved replacement-baseline portion but cannot acquire a fresh whole
  grant. Final row limits count the complete compressed support, not expanded
  multiplicities or individual frames, and are rechecked even when native poll
  returns an already pending batch. Values and columns are checked against
  their reserved logical copy budget before conversion. Late authority,
  cancellation or generation failures refuse before exposing a new handle or
  batch. These ceilings apply per registration, maintenance tick and poll;
  they are not a subscription-lifetime ledger or an allocator-wide RAM bound.

  Recovery terminates an old subscription with `SUBSCRIPTION_RESET`, even when
  it has no flow credit, provided the writer is at a complete frame boundary
  and the capability is still live. The optional checkpoint is the last fully
  delivered batch's sequence; it is absent if no complete baseline was sent.
  The native client verifies that checkpoint against complete wire batches,
  discards any incomplete batch, and returns `ClientError::SubscriptionReset`.
  The error's `last_delivered_seq` reports its last successful `on_change`
  callback. Batches received after that callback requested cancellation can
  advance the wire checkpoint but do not advance the caller's checkpoint;
  cancelled completion also reports the last successful callback frontier.
  Subscribe again for a replacement baseline. The checkpoint grants no durable
  resume authority and does not promise gap replay across reconnection.
  `fgdb remote subscribe` streams `change` and `progress` records. A recovery
  reset emits `subscription_reset` with `resubscribe_required: true` and
  `last_delivered_seq` (a sequence or null), then exits with failure instead of
  reporting a completed result. The HTTP
  adapter refuses subscriptions (they need a flow-controlled connection).
- **HTTP/1.1 JSON adapter.** `fgdbd serve --http-listen` adds the same
  autocommit statements over plain HTTP: `POST /v1/databases/<name>/query`
  or `/write` with `Authorization: Bearer <hex token>` and a body
  `{"statement": "<gql>", "parameters": {...}}` (plain JSON arguments: an
  integer is `int`, another number `float`, an object a map; JSON has no byte
  strings, so the one-key objects `{"$bytes": "<hex>"}` and
  `{"$vector": [numbers]}` spell bytes, the latter as packed little-endian
  f32 values, the stored form of an embedding); `GET
  /v1/health`; `GET /v1/databases/<name>/schema` (bearer token, read rights)
  answers the label, relation and property names the token's scope may see,
  so a client or agent can discover the schema without being able to learn a
  hidden name. It is a framing over the exact FGP execution path
  (`crates/fgdb-server/src/execute.rs`): the same capability check, the same
  fresh authorized session per statement, the same statement and error
  classes. A query answers `{"v":1,"columns":[...],"rows":[[cell...]],"seq":N}`
  with the CLI robot cells; a write answers `{"v":1,"seq":N,"statements":M,
  "committed":true|false}`; a refusal answers `{"v":1,"error":{"code","message"}}`
  under a status that follows its class (statement/protocol 400,
  unauthenticated 401, permission_denied 403, not_found_or_unauthorized 404,
  conflict 409, budget 422, busy 429, draining/database_recovering/
  database_unavailable 503, otherwise 500). A missing
  database and an unauthorized one share one 404. Requests must name an
  allowed `Host` (default `localhost`, `127.0.0.1`, `[::1]` and the listen IP),
  which defeats DNS rebinding against a loopback listener. Results are
  buffered whole: an HTTP response has no per-row flow control.
  `fgdb_protocol::json` holds the one strict JSON grammar and the cell
  encoding the CLI and the adapter share.
- **The Bolt-compat subset** (`fgdbd serve --bolt-listen`, crate
  `fgdb-bolt` plus `fgdb-server/src/bolt.rs`): `BoltCompatProfileV1`, the
  plan's read-only negotiated downgrade, at Bolt 5.0. HELLO carries the hex
  capability token as a `bearer` credential or a `basic` password; a token no
  served database accepts is refused (`Neo.ClientError.Security.Unauthorized`)
  and the connection closes. RUN/PULL/DISCARD, BEGIN/COMMIT/ROLLBACK, RESET,
  GOODBYE and ROUTE (a single-server table, so `neo4j://` works) are served.
  Every RUN executes on an authorized read session, which cannot express a
  write: a mutating statement refuses before graph access with
  `Neo.ClientError.Statement.AccessMode`. An explicit transaction holds one
  read session, so all its statements read one generation; no lock is held
  across round trips. Vertices, relationships and paths are returned as Bolt
  graph values, including inside lists and maps. Their labels, relationship
  types, endpoints and properties are read through the same authorized
  session at the statement's selected sequence, including historical reads.
  Paths preserve traversal direction and reuse node/relationship identities;
  missing relationship metadata or exhausted hydration allowances refuse the
  whole RUN before records are delivered. Metadata lookup batches each use
  the ordinary native and signed per-execution limits; these lookups do not
  yet share one cumulative RUN allowance. `CALL db.labels()`,
  `db.relationshipTypes()` and `db.propertyKeys()` answer the same
  scope-filtered schema names as HTTP's schema route. The FGP error classes map
  onto Neo4j status codes, and Busy/Draining/DatabaseRecovering use a retryable
  `TransientError`. Bookmarks name the generation read and are not required
  inputs. A retained transaction, delayed PULL or COMMIT from an invalidated
  generation refuses even after the database is ready again; start a new read
  transaction. Failed recovery uses a non-retryable database error. Results are
  the same ephemeral class as FGP's.

Not served, and refused with a typed error rather than approximated: the
durable `PublishedResultStream` class with RESULT_ACK/RESULT_RELEASE,
durable transaction preparation,
explicit multi-statement transactions with ownership and
reattachment, durable subscriptions with resume across reconnects, Bolt
writes (`BoltCompatProfileV2`, post-1.0), and the
HTTP/2, gRPC and WebSocket adapters. Because results are ephemeral, a disconnect can lose undelivered
rows but never a commit: a write's outcome is decided before its first frame.

Witnesses: `cargo test -p fgdb-protocol --all-features` (body round trips,
every truncated prefix, trailing bytes, noncanonical spellings, hostile
counts and depth, exact float bits) and `cargo test -p fgdb-server`
(`tests/loopback.rs`, over real loopback TCP against a durable on-disk
database: the handshake, a multi-relation CREATE, a 20-map UNWIND batch, a
22-row read through a 4-row window, parameterized pattern reads, statement
refusals that leave the connection usable, a read-only token's write refused
`permission_denied`, a Company-only token counting zero vertices, a foreign
issuer's token refused `unauthenticated`, drain delivering GOODBYE,
committed writes surviving a server restart, the HTTP adapter's health,
parameterized write and read, every refusal status and the Host allow-list,
and a subscription receiving its baseline, an insert delta, a ten-row batch
through a four-row window, a progress-only delta, an exact retraction, its
END on cancel, and a scoped token's refusal).

Prepared-read regression sources additionally cover canonical handles and
operand bodies, client reply binding, cancellation draining, live parameter
rebinding, cache ownership, recovery fencing and authority narrowing:
`crates/fgdb-protocol/src/{body,client}.rs`,
`crates/fgdb-protocol/tests/protocol.rs`,
`crates/fgdb-server/src/connection/prepared_tests.rs` and
`crates/fgdb-server/tests/loopback.rs`. These newly added tests have not run in
this session because the local compiler/process service is unavailable.

Subscription regressions in
`crates/fgdb-server/src/execute/subscription/tests.rs` and
`crates/fgdb/src/standing_query/native/changes/statement.rs` additionally cover
circuit footprint bounds, atomic setup refusal, zero signed grants,
source-free grouping, complete support limits on pending redelivery,
persistent maintenance ceilings and replacement baselines after replay
eviction. These new tests have likewise received source review only; no
runtime or formatting pass was available in this session.

## Atomic parameter batches

`EXECUTE_BATCH` and `POST /v1/databases/<name>/write-batch` prepare one native
write script, bind all argument sets, and execute one atomic program. The engine
allocates graph identities. Each record runs the complete script in input order,
so later records can match or update earlier records' checked effects. Preparation,
binding and execution share one signed work/node allowance; a bad final argument
set or a precommit execution failure leaves no committed prefix.

With the database's `Person`, `name` and `age` symbols configured, send this JSON
to `/v1/databases/social/write-batch` with `Authorization: Bearer <hex token>`:

```json
{
  "statement": "CREATE (:Person {name:$name,age:$age})",
  "argument_sets": [
    {"name": "Ann", "age": 30},
    {"name": "Bob", "age": 25}
  ]
}
```

The Rust client exposes the same operation after `client.select(cx, "social")`:

```rust
use fgdb_protocol::body::WireValue;

let outcome = client.execute_batch(
    cx,
    "CREATE (:Person {name:$name,age:$age})",
    vec![
        vec![("name".into(), WireValue::Text("Ann".into())),
             ("age".into(), WireValue::Int(30))],
        vec![("name".into(), WireValue::Text("Bob".into())),
             ("age".into(), WireValue::Int(25))],
    ],
).await?;
```

The example creates both vertices in one commit and reports two completed
statements. HTTP returns `{"v":1,"seq":N,"statements":2,"committed":true}`;
the client returns `Outcome::WriteCommitted { seq, statements: 2 }`. A program
that needs no durable write can instead return `ReadClosed` (`committed:false`).

- Supply 1–1024 argument sets. The first set establishes parameter types;
  every set must bind the same schema. Each set allows at most 1024 parameters.
  FGP's negotiated frame and shared value-node bounds, and HTTP's 8 MiB body
  and JSON bounds, still apply.
- The host's `DatabaseConfig::max_statements` defaults to **64 expanded
  statements** for the whole request. A two-statement script with 32 sets uses
  that entire allowance. The native hard maximum is 65,536; a request is never
  split into smaller commits to fit a limit.
- Write authority is required; scripts that read, including `MATCH` and
  `MERGE`, require ReadWrite. Existing label/relation/property restrictions apply.
- This is a statistics-only operation. **`RETURN` is refused before effects**,
  including `RETURN ... LIMIT 0`; no requested expression is silently skipped.
  No rows or identity receipts are delivered, so a signed `max_rows=0` is valid.
- A missing terminal response, transport loss or `outcome_unknown` can mean the
  batch already committed. The client never retries automatically. Repeating
  the call is a new write and can duplicate effects; recovery does not replay it.

## Remaining integration

Authoritative frame-catalog generation, durable
result machines with ACK/release/resume, durable transaction preparation,
explicit transactions with
ownership and reattachment, SnapshotQuery proofs, durable and capability-masked
subscriptions, the surface
adapters, multi-tenant admission/QoS, and the native Python packaging boundary
remain separate implementation work. Do not expose raw embedded queries
through this codec while bypassing the authorized session owners.
