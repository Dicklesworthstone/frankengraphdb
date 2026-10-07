# Fabric protocol mechanisms

Owner beads: `fgdb-w10-fgp-core-5b1`, `fgdb-w10-flow-send-obligations-smd`,
`fgdb-w10-fgp-frame-catalog-j1bl`, `fgdb-w10-server-rte`. These beads remain open.

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
| PING / PONG | 0x0020 / 0x0021 | client / server |

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
nonzero request IDs and control/child-stream distinction. EXECUTE and PREPARE
use the control stream; child stream IDs are minted by the server. Cancellation
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
| SELECT_DATABASE | `name text` |
| READY | `namespace [32]`, `incarnation [32]`, `service_epoch u64`, `posture u8`, `authority_commitment [32]`, `frontier u64` |
| EXECUTE | `mode u8` (0 read, 1 write, 2 subscribe), `statement text`, `parameters [(name text, value)]` (names strictly ascending) |
| SNAPSHOT_RESULT_CHUNK | `columns: none \| [text]` (first chunk only), `rows [[value]]` |
| SNAPSHOT_RESULT_END | `outcome` (`Rows{seq}`, `WriteCommitted{seq,statements}`, `ReadClosed{seq,statements}`), `rows u64` |
| SUBSCRIPTION_BATCH | `frontier u64`, `snapshot bool`, `last bool`, `columns: none \| [text]` (first frame only), `entries [(weight i128 ≠ 0, [value])]` |
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
`outcome_unknown`, `busy`, `draining` and `cancelled`.

## fgdbd: the served subset

`crates/fgdb-server` composes the embedded engine behind this machine over
asupersync TCP. The `fgdbd` binary serves one or more databases; `fgdbd
token` mints capability tokens and `fgdbd keygen` writes owner-only key files.
The CLI's `fgdb remote` and `fgdb_protocol::client::Client` are its clients.

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
  accepted and ignored instead of poisoning the connection.
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
  EphemeralCompleted` (legal for query children only: nothing durable is
  retained, so nothing is detached); a committed write reports
  `SemanticTerminalDurable`.

- **Subscriptions.** EXECUTE with mode `subscribe` and `SUBSCRIBE TO <read>`
  registers the engine's own maintained query (`Database::subscribe_native`)
  on a server-minted subscription child stream. The first batch is a
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
  registrations per server lifetime (default 64; refused `budget` beyond).
  `fgdb remote subscribe` streams `change` and `progress` records; the HTTP
  adapter refuses subscriptions (they need a flow-controlled connection).
- **HTTP/1.1 JSON adapter.** `fgdbd serve --http-listen` adds the same
  autocommit statements over plain HTTP: `POST /v1/databases/<name>/query`
  or `/write` with `Authorization: Bearer <hex token>` and a body
  `{"statement": "<gql>", "parameters": {...}}` (plain JSON arguments: an
  integer is `int`, another number `float`, an object a map; JSON has no byte
  strings, so the one-key objects `{"$bytes": "<hex>"}` and
  `{"$vector": [numbers]}` spell bytes, the latter as packed little-endian
  f32 values, the stored form of an embedding); `GET
  /v1/health`. It is a framing over the exact FGP execution path
  (`crates/fgdb-server/src/execute.rs`): the same capability check, the same
  fresh authorized session per statement, the same statement and error
  classes. A query answers `{"v":1,"columns":[...],"rows":[[cell...]],"seq":N}`
  with the CLI robot cells; a write answers `{"v":1,"seq":N,"statements":M,
  "committed":true|false}`; a refusal answers `{"v":1,"error":{"code","message"}}`
  under a status that follows its class (statement/protocol 400,
  unauthenticated 401, permission_denied 403, not_found_or_unauthorized 404,
  conflict 409, budget 422, busy 429, draining 503, otherwise 500). A missing
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
  across round trips. Vertices are returned as Bolt nodes whose labels and
  properties are read through the same session (capability masking applies);
  relationship and path values refuse with
  `Neo.ClientError.Statement.FeatureNotSupported`. The FGP error classes map
  onto Neo4j status codes, and only Busy/Draining use a retryable
  `TransientError`. Bookmarks name the generation read and are not required
  inputs. Results are the same ephemeral class as FGP's.

Not served, and refused with a typed error rather than approximated: the
durable `PublishedResultStream` class with RESULT_ACK/RESULT_RELEASE, PREPARE,
AUTH_REFRESH, explicit multi-statement transactions with ownership and
reattachment, durable subscriptions with resume across reconnects, TLS, Bolt
writes (`BoltCompatProfileV2`, post-1.0) and relationship/path values, and the
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

## Remaining integration

Authoritative frame-catalog generation, protected transport (TLS), durable
result machines with ACK/release/resume, PREPARE, explicit transactions with
ownership and reattachment, SnapshotQuery proofs, durable and capability-masked
subscriptions, the surface
adapters, multi-tenant admission/QoS, and the native Python packaging boundary
remain separate implementation work. Do not expose raw embedded queries
through this codec while bypassing the authorized session owners.
