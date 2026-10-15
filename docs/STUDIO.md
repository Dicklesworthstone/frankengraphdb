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
| Explorer | Ctrl/Cmd + 1 |
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
