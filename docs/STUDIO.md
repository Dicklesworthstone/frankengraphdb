# FrankenGraph Studio

A local-first, read-only browser workspace for FrankenGraphDB. Studio is embedded
in `fgdbd`: no Node server, package install, CDN, telemetry, external fonts, or
frontend build step is required to run it. It uses the existing HTTP schema and
query endpoints and the same Warden capability checks as other clients.

## Open an existing database

Build the server with the repository's pinned toolchain and foundation checkouts:

```bash
cargo build --locked -p fgdb-server --bin fgdbd
```

Start an HTTP listener on loopback, supplying the **existing database key** and
its issuer key. Substitute your actual database directory and key paths:

```bash
target/debug/fgdbd serve \
  --listen 127.0.0.1:9000 \
  --http-listen 127.0.0.1:8080 \
  --database graph=/absolute/path/to/database \
  --key-file /absolute/path/to/database.key \
  --issuer-key-file /absolute/path/to/issuer.key \
  --max-result-rows 500
```

Preserve your usual `--bindings-file`, `--label`, `--relation`, `--property`,
policy-epoch and other database options. These names are the server's catalog;
Studio does not invent or persist database bindings. Do not generate a new key
for an existing encrypted database. A new issuer can be created separately with
`fgdbd keygen --issuer /absolute/path/to/issuer.key` when needed; keygen refuses
to overwrite an existing file.

Open **http://127.0.0.1:8080/studio/** in a modern browser. `/` and `/studio`
redirect to this directory. An existing `--http-listen` deployment needs no
additional Studio flag. The static workspace starts with a clearly labeled
sample graph so navigation and visualization can be explored without credentials.
Sample data is not a query result from your database, and editing the query does
not run a fake query against the sample.

In another terminal, mint a short-lived **read-only** capability using the same
keys and policy epoch as the server:

```bash
target/debug/fgdbd token \
  --key-file /absolute/path/to/database.key \
  --issuer-key-file /absolute/path/to/issuer.key \
  --rights read --expires-in 3600 --max-rows 500
```

Choose **Connect database**, enter `graph` (the served name, not a filesystem
path), and paste the hex capability. Studio loads the permitted schema and runs
the current editor contents. Connection uses only the page's server origin; there
is no arbitrary endpoint selector, proxy, cross-origin token forwarding or CORS
exception. Use an SSH tunnel or correctly configured HTTPS for access beyond your
machine, not plaintext credentials over a network. Existing Host validation and
server TLS configuration remain authoritative.

## Explore

The canvas supports drag/pan, wheel and touch zoom, node dragging, fit-to-view,
force, label-cluster and radial layouts, a navigation minimap, and a neighborhood
spotlight. Layout work runs in a bounded worker and stops after settling. Reduced
motion displays completed layouts without intermediate movement. Label placement
avoids collisions; selecting a node or zooming exposes more detail.

Select a node or relationship for exact identity and returned properties.
Parallel relationships and self-loops stay distinct. The inspector's searchable
node list is an alternative to pointing at the canvas. Label filters affect only
the visualization: the original table and JSON remain unchanged. A neighborhood
spotlight shows neighbors already present in the result; it does not silently
scan or fetch the database.

**Graph / Table / JSON** are synchronized views of one received result. Tables
support local search, stable sorting and 50-row pages. JSON preserves the original
response and exact typed values. CSV exports quote fields and neutralize spreadsheet
formula prefixes; JSON exports retain the exact response; PNG exports capture the
current canvas. Exports contain graph data and may be sensitive. They never add
connection credentials. Save only to destinations you trust.

The schema browser lists capability-visible label, relationship and property
names. Clicking a label loads an escaped exploration query without executing it.
The query workbench has syntax highlighting, raw JSON parameters, cancellation,
round-trip timing, snapshot metadata and a tab-local history of the last 30
requests. Loading a history entry does not execute it automatically. No write
endpoint is exposed by Studio. Even a write-capable token cannot make Studio's
read route execute a write; the native authorized read session remains the guard.

## Query result conventions

The **Explore relationships** template returns these explicit columns:

```gql
MATCH (source)-[relationship]->(target)
RETURN source, relationship, target,
       labels(source) AS source_labels,
       properties(source) AS source_properties,
       type(relationship) AS relationship_type,
       properties(relationship) AS relationship_properties,
       labels(target) AS target_labels,
       properties(target) AS target_properties
LIMIT 150
```

`source`, `relationship`, `target` establish stored edge orientation. Optional
metadata is associated with these named columns. The node template returns
`node`, `node_labels`, `node_properties`. Results without metadata still show exact
identities. Arbitrary vertex/edge co-occurrence is **not** evidence of adjacency.
A native path supplies traversal adjacency but not stored direction, so its edges
have no invented arrows unless explicit endpoint columns also supply orientation.
Graph elements nested in lists and maps are discovered too.

Identities remain decimal strings, including full-width u128 values. Typed integer,
count, wide integer, decimal and exact-average cells are not converted through
JavaScript numbers. Parameter text is validated as JSON but transmitted in its
original spelling, avoiding rounding of large integer literals. Unsafe numeric
response metadata uses the browser's JSON source-text reviver; a browser without
that facility refuses unsafe numbers rather than silently changing them. Prefer
explicit `{"$decimal":"..."}` parameters for exact fixed-scale decimal values.

## Privacy, safety and bounds

Only the color-theme preference is stored in localStorage. Tokens, parameters,
query history and results live in the current tab's memory. Disconnecting, opening
the sample workspace or leaving the page clears the session. A browser, extension,
operating system or developer tool can still inspect data already in memory;
this is not secure erasure or revocation of previously delivered results.

The embedded shell contains no live database metadata. It has an exact asset
allowlist, no filesystem resolver, no catch-all over API paths, and restrictive
same-origin CSP, no-store, nosniff and anti-framing headers. Data is rendered as
text, not HTML. Existing guarded HTTP output still checks capability expiry,
retirement and database generation at the native transport boundary.

The browser refuses responses above **8 MiB** or **10,000 rows**, and renders at
most **750 nodes / 2,000 edges** on the canvas, with a visible notice when the
visualization is capped. The complete admitted result remains available in the
table and exports. JSON display previews at most 200,000 characters; export keeps
the original admitted response. Inspector previews are bounded too. These are
browser/result bounds, **not a whole-query server-memory guarantee**. The HTTP
adapter still buffers its response; a small `LIMIT` does not bound scans, sorting
or other upstream work. Configure the existing server and signed capability
budgets independently.

Requests time out in the browser after 30 seconds. Cancel aborts the browser's
request and prevents stale results from replacing a newer workspace. It does
**not** prove that server execution has already stopped. Counts describe the
current mapped result (and active canvas filters), not the entire database;
round-trip time includes network and browser processing, not isolated engine time.
Studio is not a monitoring system, durable result cursor, administrative console,
or full Neo4j Browser compatibility layer.

## Shortcuts and testing

| Action | Shortcut |
| --- | --- |
| Execute read | Ctrl/Cmd + Enter |
| Command palette | Ctrl/Cmd + K |
| Find node | / |
| Fit graph | F |
| Pan focused canvas | Arrow keys |
| Zoom focused canvas | + / - |
| Clear selection or close dialog | Escape |

Tab keeps ordinary focus navigation, including inside the editor. Result tabs
support arrows, Home and End. The sidebar collapses and the inspector becomes a
panel on narrow displays; dark/light themes and visible focus styles are included.

Pure model, exact-value, bounded-response and layout tests need only Node:

```bash
node --test crates/fgdb-server/src/studio/tests/model.test.mjs
```

The embedded route has Rust unit tests:

```bash
cargo test --locked -p fgdb-server --lib http::studio
```

The full repository gate remains `bash scripts/check.sh`. Browser/model checks do
not substitute for compiling `fgdbd` and testing an authorized live database.

Optional client-state regressions use Python Playwright and Chromium:

```bash
CHROMIUM=/path/to/chromium python crates/fgdb-server/src/studio/tests/browser_client.py
```

These tests load the embedded assets in an isolated browser document and inject
HTTP-response fixtures; they make no network requests. Module URLs, the worker
packaging and the page origin are adapted only in this test harness. They cover
raw parameter precision, safe text rendering, cancellation/stale-response fences,
connection cleanup, schema-changing results and history behavior. They do not
validate production CSP, module-worker loading, native authorization or `fgdbd`.
Neither Playwright nor Python is needed to run Studio itself.

## WebAssembly workspace

Studio now runs genuine force integration in a WebAssembly worker: Barnes–Hut
repulsion, spring attraction, collision separation, damping and cooling. Force,
label-constellation and radial modes use the same physical kernel; the latter
modes add target forces rather than substituting a static diagram. The simulation
settles and stops. Pause, reheat, repulsion, link-length and gravity controls work
without a database request. Shift-drag, double-click, or P on a selected node pins
it; neighboring nodes continue responding. Coordinate continuity is preserved
between reads in one connection, while switching sessions clears it.

The checked-in `src/studio/wasm/force.wasm` is a 5.8 KiB freestanding module built
from the adjacent owned C11 source. It has no imports, allocator, WASI runtime,
external package or network access. It receives numeric display indices and
coordinates, not identity strings, credentials or property payloads. Its linear
memory is fixed at 4 MiB; tree depth, capacities, integration batches and settling
are bounded. Coincident points or a full spatial tree use a bounded exact leaf
fallback. Browser canvas caps remain 750 nodes / 2,000 edges. This is an isolated
presentation kernel, not native Rust code, a database algorithm, a query result,
or a deterministic plan certificate.

No build tool is needed to run the bundled module. To rebuild it as a developer:

```bash
bash crates/fgdb-server/src/studio/wasm/build.sh
node --test crates/fgdb-server/src/studio/tests/*.test.mjs
```

The build requires clang with the wasm32 target and wasm-ld (built here with
clang 17), and links no standard library. Source and binary ship together. The
server serves its exact asset path as `application/wasm`. CSP permits Wasm
compilation with `wasm-unsafe-eval`; JavaScript string evaluation and inline script
remain disallowed. The renderer does not pretend to run JS physics if Wasm fails:
an explicit failure appears and initial positions remain interactive.

### Chronicle: explore and compare returned snapshots

The native lens generates `FOR SYSTEM_TIME AS OF SEQ <u64>` after MATCH, before
RETURN. Input retains the entire unsigned 64-bit range without Number coercion.
Loading a lens never executes it, and never rewrites an arbitrary editor query.
Press Run query to invoke the existing authorized native read path.

Pin a received result as baseline A, run another read, then choose Compare.
The canvas distinguishes elements only in A, only in B, and changed returned
metadata. Comparison includes original type tags: an integer `1` is not silently
made equal to text `"1"` or decimal `1.0`. Node positions are retained by exact
identity, and baseline-only edges are drawn as ghosts. The original table, JSON
and CSV remain the current result, without comparison ghosts.

These are comparisons of **returned samples**, not complete commit-stream diffs.
A LIMIT, different query or changed projection can explain an omitted element;
omission does not establish deletion. The interface marks differing query inputs
and capped comparison canvases. One baseline is retained in tab memory only and
is cleared on session changes, permission denial, disconnect or page exit. It is
not a durable snapshot lease. Retaining a baseline and current result may retain
two individually admitted responses plus decoded presentation state; 8 MiB is a
per-response bound, not a total browser-memory ceiling.

The agent-atlas sample has four explicitly illustrative cuts (101–104). They let
you exercise additions, omitted edges and property changes without claiming to
read a database. The classic 32-node sample remains available separately.

### Prism and Beacon: use native capabilities

Query lenses include native PageRank and connected-components calls through
Prism, plus Beacon graph-neighborhood retrieval with `graph_access => 'demand'`.
They load editor contents; they do not fabricate algorithm results. Replace sample
parameters with known values, then execute with your capability. The server's
implemented procedure profiles, source projection, scope and budgets remain
independently authoritative. Loading a recipe does not prove native execution
will be admitted for every database or capability.

For a PageRank overlay, first pin an exploration result containing topology.
Run native PageRank and return a `vertex`, `id`, or `node` identity column plus a
finite float `score` or `rank` column. When the reported sequences match, Score
overlay sizes those already-observed vertices by their returned score. Exact
score text remains in the inspector and raw result; normalization is visual only.
Unmatched scored vertices are counted, not invented as new topology. Matching
sequences and connection ownership are prerequisites, not a signed source or
plan-certificate check. Re-pin topology when the database has advanced.

The inspector's Start trace / Trace here action follows only the returned
subgraph. It honors known stored direction and treats direction-unknown path
adjacency as undirected. It makes no HTTP request and is not presented as a
Prism query over the whole database.

### Verification boundary

The Node tests instantiate and execute the actual Wasm binary, including force,
collision, pinning, quadtree visits, limits and deterministic replay checks.
Browser fixtures also execute that same binary while testing interaction,
exact-value handling, client cancellation, permission cleanup, temporal lenses
and analytics overlays. Their isolated loader adapts worker packaging and origin;
they still do **not** validate production CSP/module-worker loading or native
HTTP/database integration. Rust route tests and the full native repository gates
remain separate. UI capability submission is restricted to HTTPS or explicit
loopback hosts; use an SSH tunnel for a remote plaintext server.
