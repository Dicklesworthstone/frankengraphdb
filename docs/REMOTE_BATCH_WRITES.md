# Atomic remote write batches

`fgdb remote write --rows-file <path> <statement>` submits one finite set of
parameter maps to the server's existing native `EXECUTE_BATCH` owner. All sets
share one signed execution allowance and one commit. The CLI does not expand
query text, generate identities, retry requests, or split a batch into commits.

## Example

Save `people.json` as:

```json
[
  {"name": "text:Ada", "age": "int:37"},
  {"name": "text:Bob", "age": "int:29"}
]
```

Then, against a served database with the Person/name/age bindings:

```sh
fgdb --robot remote write \
  --addr 127.0.0.1:7688 \
  --database social \
  --token-file writer.hex \
  --rows-file people.json \
  'CREATE (:Person {name:$name, age:$age})'
```

Existing `--tls-server-name` and `--tls-ca-file` flags select the same verified
TLS path used by other remote commands. Omitting them retains the existing
plaintext behavior; use TLS for an untrusted network. `--rows-file` is write-only
and cannot be combined with `--param`. Named regular UTF-8 files are supported,
not stdin. Every argument map is independent; there are no inherited defaults.

Values are JSON strings using the normal remote parameter spellings:
`text:`, `int:`, `uint:`, `float:`, `bool:true`, `bool:false`, `null`,
`bytes:`, `vector:`, and `json:`. For example, a nested list parameter is
`"items": "json:[1,null,{\"name\":\"Ada\"}]"`. A full-width unsigned value is
`"limit": "uint:18446744073709551615"`; it never passes through a floating-point
JSON number. JSON escaping affects the file representation, not query text.
Duplicate object keys and malformed/invalid typed values refuse the entire file.

## Bounds and transaction semantics

The source file is capped at 1 MiB; its JSON tree uses the existing 65,536-value
and 65,536-byte token limits. There must be 1..=1,024 argument sets, with at most
1,024 parameters per set. The complete encoded request, including its Ready
header, must fit the client's 1 MiB frame limit. A smaller negotiated server
frame limit still applies and is enforced by the FGP client before writing.
Memory is bounded but the entire admitted request is retained; this is not a
streaming or resumable bulk-loader format.

The server separately enforces native statement counts, parameter schemas,
capability rights, effects, and execution budgets. Multiple statements in a
native script can expand one argument set into several statements. The record
count does not enlarge any server or signed limit. RETURN remains unsupported
by the stats-only batch owner. A binding/execution refusal before commit does
not publish an earlier successfully processed argument set.

## Completion and failures

Success emits one ordinary `written` terminal, with `records` for argument sets
and `statements` from the native completion. It emits no per-record progress:

```json
{"v":1,"event":"result","kind":"written","seq":7,"statements":2,"records":2}
```

File/shape/type/wire admission finishes before reading the bearer credential or
opening a connection. Such local usage failures exit 2. Server refusals keep
ordinary remote error classes, and an unknown outcome remains an I/O failure.
There is no automatic retry on any error. Connection loss after submission is
not evidence of rollback: determine the durable outcome before resubmitting.

A stdout failure after a validated completion reports that sequence and warns
against replay. Some output bytes may already have escaped. Require successful
process exit and a complete terminal together; neither a missing terminal nor
an error implies that a submitted write rolled back.
