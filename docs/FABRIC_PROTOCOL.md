# Fabric protocol mechanisms

Owner beads: `fgdb-w10-fgp-core-5b1`, `fgdb-w10-flow-send-obligations-smd`,
`fgdb-w10-fgp-frame-catalog-j1bl`. These beads remain open.

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

Run `cargo test -p fgdb-protocol`. The suite covers all header classes at every
fragmentation boundary, one-byte feeds, coalesced frames, pre-allocation binding
refusal, length/tag/flags/version checks, truncated EOF, debug redaction,
handshake ordering, refresh fencing, generation-safe drain, duplicate/foreign
send completion, credit replay, and uncertain-write accounting.

The Rust tests were added but not executed in the implementation environment:
no Rust toolchain was available. No runtime, performance or full-protocol
conformance claim is made by this change.

## Remaining integration

Typed operation bodies and authoritative catalog generation, protected-transport
authentication, Operational-root selection, fresh send guards, durable result
machines, SnapshotQuery proofs, the daemon/CLI, surface adapters, and the native
Python packaging boundary remain separate implementation work. Do not expose
raw embedded queries through this codec while bypassing those owners.
