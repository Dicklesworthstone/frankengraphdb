# FrankenGraphDB Implementation Status

Capability baseline: unreleased `main`. Each section carries the date it was last re-derived from the code; the latest is 2026-10-10.

This document is the compact, agent-facing map of **what executes now**, **where each live behavior is owned**, **what its evidence proves**, and **what remains target architecture**. The comprehensive plan remains normative for the finished system; this file records the present inhabitable subset.

## Product reality

FrankenGraphDB is not yet a released graph-database product. There are no tagged releases, supported installer, CLI/server distribution, Python package, or compatibility promise. The live product surface is an embedded Rust composition crate over real Chronicle durability and real Strata tier-D storage, with a deliberately bounded GQL read slice and a bounded write-transaction overlay.

The repository contains real durable mechanisms and executable cross-layer paths. It does **not** yet contain the complete W10 surface, full ISO GQL, GLA/Loom physical execution, full SSI, or the distributed and incremental system described by the plan.

## Live product verticals

### Durable embedded database

`fgdb::Database` can:

- create and reopen through the real Chronicle capsule-first, marker-last two-fsync path;
- recover the authoritative marker chain and authenticate checkpoint-selected Strata roots against it;
- commit typed `WriteBatch` mutations under the production first-committer-wins validator;
- read vertices, edges, adjacency, and complete collections at the live frontier or an exact retained `CommitSeq`;
- run over the ordinary filesystem VFS or the in-memory VFS without substituting an in-memory graph model for the durable composition path.

Canonical owner: `crates/fgdb/src/lib.rs`.

### Bounded deterministic GQL

Re-derived from the code on 2026-09-28 (fgdb-truthup-0927-6vsim). The live text surface is a bounded, deterministic subset of ISO GQL with an openCypher compatibility surface. Each item below names a test that executes it.

Reads, through `Database::query` → `PreparedNativeRead::prepare` (`crates/fgdb/src/query_explain.rs`):

- `MATCH` over labels, property maps, directed/incoming/undirected edges and multi-pattern joins (`crates/fgdb-gql/tests/disconnected_patterns.rs`); `WHERE` with `AND`/`OR`/`NOT`, comparisons, `IS [NOT] NULL`, `IN [...]`, `STARTS WITH`/`ENDS WITH`/`CONTAINS` and `=~` over an in-house bounded regex engine (`crates/fgdb-gql/tests/text_predicates_and_functions.rs`); `EXISTS { ... }` pattern predicates, also spelled as a bare pattern or `exists(<pattern>)` (`crates/fgdb-gql/tests/scoped_text.rs`).
- `OPTIONAL MATCH` (`crates/fgdb-gql/tests/multipart_optional_reads.rs`), including directly after `UNWIND` (one NULL-extended row per unmatched value; an aggregate over it needs a `WITH` boundary), and including a named relationship in an `OPTIONAL` or later required `MATCH`: `r`, `type(r)` and `r.p` are NULL where the clause has no witness, and the relationship aggregates, crosses `WITH`, feeds `SET`/`DELETE`, and is masked under a capability token (`crates/fgdb/tests/scoped_edge_variables.rs`, `crates/fgdb/tests/authorized_patterns.rs`).
- Quantified paths with a mandatory upper bound (`-[:R]->{1,4}`, `-[:R*1..3]->`), `ANY SHORTEST`/`ALL SHORTEST` (also spelled `shortestPath(...)`/`allShortestPaths(...)`, the same compiled plan: `crates/fgdb-gql/tests/any_shortest_walk_text.rs`) and `CHEAPEST` walks, and the path functions `nodes`/`relationships`/`length`/`path_length` (`crates/fgdb-gql/tests/any_shortest_walk_text.rs`, `crates/fgdb-gql/tests/shortest_walk_text.rs`, `crates/fgdb-gql/tests/restricted_path_text.rs`, `crates/fgdb/tests/weighted_path_text.rs`, `crates/fgdb-gql/tests/path_values.rs`, `crates/fgdb-gql/tests/graph_insert_return_endpoints.rs`).
- Projections with `DISTINCT`, `ORDER BY`, `SKIP` and `LIMIT`; computed integer and text expressions, `CASE`, `COALESCE`, `NULLIF`, `toUpper`/`toLower`/`trim`/`substring`/`size`/`toString`/`toInteger`, `||` and text `+`; float arithmetic with negative and exponent float literals (`-0.5`, `1e5`, `1.5e-3`; an out-of-range exponent is refused, never infinity), Int-to-Float comparison by numeric value, and `toFloat`/`floor`/`ceil`/`round`/`sqrt` rounded once by the integer-only binary64 kernel (`round` ties toward positive infinity; `toInteger` truncates a float; `toString` prints its shortest round-trip text) (`crates/fgdb-gql/tests/case_expressions.rs`, `crates/fgdb-gql/tests/computed_return_text.rs`, `crates/fgdb-gql/tests/text_predicates_and_functions.rs`); list literals, indexing and `size` (`crates/fgdb-gql/tests/list_values.rs`); list comprehensions `[x IN list WHERE p | e]`, `any`/`all`/`none`/`single`, `head`/`last`/`tail`, slices `l[a..b]`, `range` and `reduce` (`crates/fgdb/tests/list_comprehensions.rs`); map literals `{k: e, ...}`, `m.key` and `keys(m)` over values, map projection `n{.a, k: e, v}` over graph rows (NULL for a NULL `n` from `OPTIONAL MATCH`), including maps built from aggregate outputs and `UNWIND` of a list of maps, rendered as a `map` cell by the CLI (`crates/fgdb/tests/map_values.rs`, `crates/fgdb-cli/tests/cli_robot.rs`).
- `count`/`sum`/`avg`/`min`/`max`/`collect` with `DISTINCT`, grouping and `HAVING` (`crates/fgdb-gql/tests/aggregate_text.rs`, `crates/fgdb-gql/tests/having_text.rs`); `sum`/`avg` over float (or mixed integer and float) values return the correctly rounded exact sum or mean, one rounding whatever the row order, with no cancellation loss or intermediate overflow (`crates/fgdb-gql/tests/exact_averages.rs`, `fgdb_types::ExactBinary64Sum`); unaliased RETURN items are named as openCypher names them (`a.title`, `count(b)`, `size(a.title)`) when the GQL-derived name would repeat (`crates/fgdb-gql/src/graph_text.rs` tests); `WITH` pipelines, `UNWIND` and a `MATCH` after `WITH`, including grouped `WITH` (`crates/fgdb-gql/tests/multipart_reads.rs`, `crates/fgdb/tests/with_grouping.rs`).
- `UNION [ALL]`, `INTERSECT`, `EXCEPT` (`crates/fgdb-gql/tests/set_text.rs`, `crates/fgdb/tests/set_text_queries.rs`).
- `FOR SYSTEM_TIME AS OF SEQ` (`crates/fgdb/tests/temporal_graph_text.rs`); `AT BRANCH` through a host resolver (`crates/fgdb/tests/query_branches.rs`); `CALL fnx.* ... YIELD` composed into reads, including capability-authorized reads over the masked projection (`crates/fgdb/tests/gql_call_fnx.rs`, `crates/fgdb/tests/authorized_prism.rs`); `EXPLAIN (CERTIFICATE)` (`crates/fgdb/tests/embedded_query_explain.rs`).
- Single- and double-quoted text, `//` and `/* */` comments, and backtick-delimited identifiers, under one lexical rule (`crates/fgdb-gql/tests/scalar_text.rs`). Three scanners sit outside it (re-checked 2026-10-05): the `SUBSCRIBE TO` header takes `--` and nested `/* */` comments but not `//` (`crates/fgdb/src/standing_query/native/changes/statement.rs`); the standalone `CALL fnx.*` parser takes no comments and reads numbers such as `1.` and `.5`, which native text refuses (native text reads exponent literals `1e5`, `1.5e-3`, `2E+10` as floats) (`crates/fgdb-prism/src/call.rs`); and the legacy `BoundPlan` parser has its own (`crates/fgdb-gql/src/parser.rs`).

Writes, through `PreparedGraphWriteScript` (`crates/fgdb-gql/src/graph_text/scoped/mutation/script.rs`) as one atomic program per script:

- `INSERT`/`CREATE`, including `MATCH`- and `UNWIND`-driven creation and `RETURN` of what was created (`crates/fgdb-gql/tests/graph_insert_text.rs`, `crates/fgdb-gql/tests/graph_insert_query_text.rs`), and property values read from map rows (`UNWIND $rows AS row CREATE (:Person {name: row.name})`, with `$rows` a list of maps, from the CLI as `--param 'rows=json:[...]'`: `crates/fgdb-cli/tests/cli_robot.rs`), and the CLI's `write --rows json:[...]` runs any write statement (a deduplicating `MERGE` upsert included) once per object in one atomic program (`crates/fgdb-cli/tests/cli_robot.rs`) or computed from them (`row.tags[0]`, `size(row.tags)`, `IN`); a row whose value is a list or map is a typed refusal that commits nothing (`crates/fgdb/tests/map_values.rs`); `MERGE` with `ON CREATE SET`/`ON MATCH SET` and a plain trailing `SET` (both branches; it wins over an ON clause's assignment) (`crates/fgdb-gql/tests/vertex_upsert_text.rs`, `crates/fgdb-gql/tests/edge_upsert_text.rs`, `crates/fgdb-gql/tests/edge_merge_text.rs`); `SET`/`REMOVE` (`crates/fgdb-gql/tests/mutation_queries.rs`), and `MATCH ... SET/REMOVE/DETACH DELETE ... RETURN` with one row per matched occurrence whose property reads are the element's post-statement value (its own simultaneous assignment, else the old value; a deleted vertex's property is a typed error), through `PreparedGraphMutationQueryText` and `WriteTxn::execute_graph_mutation_query_governed`, and the CLI's `write` (`crates/fgdb/tests/mutation_returning.rs`); `MERGE (n ...) [ON MATCH SET ...] [ON CREATE SET ...] [SET ...] RETURN ...` returning the one chosen vertex after every clause, read from the staged transaction state, through `PreparedGraphVertexUpsertQueryText` and `WriteTxn::execute_graph_vertex_upsert_query_engine_governed`, and the CLI's `write` (a failing RETURN rolls the upsert back; `crates/fgdb/tests/mutation_returning.rs`); a write `RETURN` (CREATE, SET or MERGE) may order by an item's source spelling (`ORDER BY n.p`); aggregate write RETURN (`RETURN count(*)`) is still refused; `DELETE`/`DETACH DELETE` (`crates/fgdb-gql/tests/delete_write_scripts.rs`); multi-statement scripts end to end (`crates/fgdb/tests/write_script_execution.rs`); CSV-bound scripts (`crates/fgdb-gql/tests/csv_write_scripts.rs`).

Typed refusals today (CLI probe and a 35-query agent-style battery, 2026-09-30): untyped relationship patterns `(a)-->(b)` and `(a)-[r]-(b)` (fgdb-du0xg); a path binding in a later `MATCH`, e.g. `MATCH (a), (b) MATCH p = shortestPath((a)-[*..5]->(b))` (put the endpoint predicates inline instead) (fgdb-20foe); a list comprehension or `reduce` over an aggregate output, or reading an element's property (fgdb-20foe); maps over graph elements (`keys(n)`, `properties(n)`, `n{.*}`), map projection after `WITH` or inside an aggregate, a map-valued parameter (a list of maps binds), a map as a stored property value, and `UNWIND` rows driving `MERGE` or `SET` in the text (re-checked 2026-10-05: refused with "CREATE or INSERT after UNWIND/MATCH"; the CLI's `write --rows` runs them once per row, and library text support is fgdb-bapr6) (fgdb-2jw3z); `m.key` inside arithmetic (`m.a + 1`; project the entry with `WITH` first); `startNode`/`endNode` of an `OPTIONAL` relationship; `CALL { }` and `COUNT { }` subqueries, `size(<pattern>)` and pattern comprehensions; `MERGE` of a pattern with an unbound endpoint (fgdb-2277w); `id()`/`elementId()` (fgdb-j687q); `CREATE GRAPH` and branch DDL (`CREATE BRANCH`, `MERGE BRANCH`).

Seven statement parsers still race and the furthest-reaching one wins, so a refusal's position and wording can come from a parser the statement was not meant for (fgdb-one-lexer-one-dispatch-285i2). The legacy `crates/fgdb-gql/src/parser.rs` (`BoundPlan`) is not among them: it is the separate parser behind `Database::execute_gql`, through `RelationBind::bind` (re-checked 2026-10-05 against `crates/fgdb/src/query_explain.rs`). Live database reads, historical reads, and immutable read-view reads share one exact-sequence execution kernel in `crates/fgdb/src/gql_exec.rs`.

Typed statement parameters have landed in `crates/fgdb-gql/src/parameters.rs` as structural parser operands, and the engine execution spine consumes them (see the 2026-09-08 → 2026-09-22 reality check below).

### Pinned embedded read views

`Database::read_session()` returns an immutable `EmbeddedReadView` owning one decoded generation. A view:

- keeps its frontier and decoded graph stable after later database writes;
- can be cloned while sharing the same immutable generation;
- executes text, plan-only, or owned prepared GQL at its pinned frontier;
- executes older retained sequences at or below that frontier;
- refuses a later sequence through `ReadError::BeyondFrontier`.

This is a real embedded read-view subset. It is not authorization negotiation, lease/reattach, cursor ownership, server transport, or the synchronous facade.

### Reusable plan-only execution

`BoundPlan` is the executor-ready plan-only form. It is appropriate when statement spelling and the caller's name-to-ID map are intentionally outside the value's identity.

| Surface | Live or pinned | Exact retained sequence | Plan evidence |
|---|---:|---:|---:|
| `Database` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_certified[_at]` |
| `EmbeddedReadView` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_certified[_at]` |
| `WriteTxn` | `execute_prepared_gql` | transaction basis only | `execute_prepared_gql_certified` |

### Coherent owned preparation

`PreparedGqlQuery` owns, behind private fields:

- exact statement bytes;
- a cloned canonical `RelationBind`;
- the `BoundPlan` derived from those inputs exactly once.

Changing the original statement or bind map cannot change the prepared definition. `Debug` redacts statement, bind, and plan contents. `verifies_definition()` reparses and rebinds the retained inputs as an explicit audit; normal execution does not need to do so.

| Surface | Preparation | Execution | Historical | Exact result evidence |
|---|---|---|---|---|
| `Database` | `prepare_gql_query` | `execute_prepared_query` | `execute_prepared_query_at` | digest and artifact paths |
| `EmbeddedReadView` | `prepare_gql_query` | `execute_prepared_query` | `execute_prepared_query_at` | digest and artifact paths |
| `WriteTxn` | `prepare_gql_query` | `execute_prepared_query` | pinned basis only | staged certificate and artifact |

The owned definition and query-budget vocabulary live in `crates/fgdb-gql/src/prepared.rs`. High-level adapters live in `crates/fgdb/src/write_txn_parts/owned_prepared.rs`. They reuse the existing binder and execution bodies rather than creating another query path.

This is not yet the final parameterized prepared-statement protocol. Typed parameters have landed (`crates/fgdb-gql/src/parameters.rs`); canonical parameter evidence, catalog epochs, authorization context, physical-plan selection, cursor lifecycle, invalidation, and a released persistence contract remain open.

### Deterministic query-execution budgets

Owned prepared execution accepts `GqlExecutionBudget` on `Database`, `EmbeddedReadView`, and `WriteTxn`.

Current dimensions:

1. `SnapshotRecords`: complete immutable vertex table admitted by a node scan, or edge table admitted by an edge pattern.
2. `ResultRows`: final rows after predicates, projection, sorting, deduplication, `SKIP`, and `LIMIT`.

`observed == limit` succeeds. `observed > limit` returns `BudgetedGqlError::Budget(GqlBudgetExceeded { dimension, limit, observed })`. No partial rows escape. Successful execution returns exact `GqlExecutionStats`.

The current implementation materializes and counts the relevant table before ordinary execution reads it. The budget is a deterministic downstream-work and result guard, not wall-clock cancellation, allocation or I/O preemption, memory/spill governance, streaming backpressure, or physical runtime-cost evidence.

### Bounded write transaction

`WriteTxn` pins one basis sequence, overlays staged vertex and edge mutations for read-your-own-writes behavior, records read dependencies and MATCH expansions, and commits normalized staged effects through the production FCW seam.

Text execution binds once and delegates to the plan-only overlay executor. Owned preparation delegates to the same body. The implementation is decomposed under `crates/fgdb/src/write_txn_parts/` while retaining one private module and one `WriteTxn` state authority.

Native `CALL fnx.* ... YIELD` now composes into transaction set queries and aggregates over that same canonical overlay. Staged edges across relations, cascading deletions, changed properties and isolated vertices remain visible to analytics and subsequent `MATCH` stages. Source admission, projection, kernel execution and result conversion share the query's work and scratch allowances; `UNION` also shares its snapshot-record allowance. Full vertex/edge-table witnesses and observed element dependencies survive empty results, failed calls and savepoint rollback. `crates/fgdb/tests/gql_call_fnx.rs` includes all 14 registered procedures plus staged-topology, conflict and budget boundary regressions. This is the bounded resident, whole-graph, unit-weight native projection model; it exposes no durable snapshot certificate for uncommitted graph data.

Native `CALL hybrid.search(...) YIELD` also composes into transaction reads and aggregates, including the CLI's ordered `transaction --write ... --query ...` path. It reuses the transaction Beacon engine over the pinned basis and canonical staged effects: text, coordinate or packed vectors, labels and graph topology participate in the same retrieval, and later projections read staged properties. Canonical source records, search work and source/conversion scratch continue the enclosing query's allowance; private search hits do not consume the final aggregate-row page. Beacon's index and expansion retain their own bounded engine ceilings. Search observations survive failed calls and savepoint rollback, while ordinary transaction completion still validates conflicts. Regression sources are `crates/fgdb/tests/gql_call_hybrid.rs` and `crates/fgdb-cli/src/transaction/tests.rs`. This is per-execution resident search with the existing exact-RRF candidate fusion and ANN semantics, not a durable index or an exhaustive hybrid top-k certificate.

The CLI's ordered `transaction` driver accepts one native `CREATE`/`INSERT`, matched `SET`/`REMOVE`/deletion, or vertex `MERGE` statement with `RETURN` in each `--write` step. It shares preparation and execution with ordinary `fgdb write`, binds each step's own parameters before beginning the transaction, and freezes that statement's post-effect values. `ON CREATE`/`ON MATCH` and trailing `SET` retain native clause order. All read and write results share one transaction-wide row and encoded-output allowance and remain buffered until the sole completion boundary. Rolling back to a savepoint discards the later effects and buffered results and restores their output allowance; full rollback or any precommit expression/output refusal publishes neither effects nor rows. `LIMIT 0` still executes effects and validates return expressions. Regression sources are `crates/fgdb-cli/src/transaction/returning_tests.rs`. Aggregate write `RETURN` and relationship `MERGE RETURN` remain outside the supported native shapes.

Full SSI, merge-ladder integration and transaction ownership/session policy remain incomplete.

### Supervised served-database recovery

Added 2026-10-10 for `fgdb-t79am`. `fgdbd` owns one host-region recovery child per opened database, shared by FGP, HTTP and Bolt. A write guard observes the native handle's state on drop, including future cancellation, and fences the current generation before releasing the database lock. New requests receive `database_recovering`; retained sessions, pending results and subscriptions keep their original generation and cannot resume against a reopened handle. Each protected physical write/flush poll checks that generation as well as the existing Warden authority. Partially written frames close the connection on invalidation.

The child waits for admitted source operations, consumes the old `Database`, and calls the production `recover_authoritatively` path. A successful reopen restores admission with a new generation and an empty subscription registry. A failed or interrupted reopen remains fenced with `database_unavailable`; it does not retry automatically or serve the old handle. Trusted embedding hosts can inspect `Server::database_status`, including the retained failure reason. Recovery does not replay client statements. Post-marker uncertainty, post-D2 publication failure, and a completed write whose queued output is invalidated retain an `outcome_unknown` classification and no-replay diagnostic.

FGP subscriptions terminate with `SUBSCRIPTION_RESET` at a complete frame boundary, independently of flow credit. Its optional sequence names the last complete batch delivered, including an empty baseline. The native client validates that checkpoint, discards any incomplete batch, and returns a typed reset; the CLI emits `subscription_reset` with `resubscribe_required` and `last_delivered_seq`. A new subscription replaces the old bag with a fresh baseline. This is recovery for ephemeral served sessions, not durable subscription resume, retained result recovery, or automatic write retry.

Canonical owners are `crates/fgdb-server/src/recovery.rs`, the server execution/transport adapters, and `crates/fgdb-protocol/src/{body,client,frame}.rs`. Regression sources in `crates/fgdb-server/src/recovery/tests.rs` drive real Chronicle marker/publication failure boundaries, cancellation, generation invalidation, failed reopen and worker ownership. Standalone protocol library/mechanism tests pass on the pinned toolchain. The new server and client-transport regressions have not run in this session: compilation of the unchanged pinned asupersync dependency was killed by the environment's memory limit before reaching them.

## Reality check 2026-09-08 → 2026-09-22

### 2026-09-08 — full verification green; repairs landed

Full verification at commit `9adf484d`: **9/9 core gates and 40/40 registered gates green**. *Correction 2026-09-24: `9adf484d` is not an ancestor of `main`, so this run proves no commit `main` contains; NE-0046 rejects it as proof.* Repairs landed in the same window:

- identity-E2E pin fix;
- cursor-policy export (`fgdb-w10-embedded-54r.1.1`);
- txn bulk scans (`fgdb-w10-embedded-54r.1.2`);
- handle ownership (`fgdb-w10-embedded-54r.1.3`);
- vertex patch packing (`fgdb-w3-properties-gou.1`/`.2`/`.3` series; `.2` closed 2026-09-22).

### 2026-09-17 — robot CLI binary

`crates/fgdb/src/bin/fgdb.rs` landed (commit `79548aca`; *since moved into the `crates/fgdb-cli` crate by `4dc0b7fc`*): `create`/`write`/`query --stream`/`--certify-to`/`diff`/`transaction`/`replay`/`load`/`robot schema`, emitting the NDJSON `{"v":1,...}` envelope, with a frozen contract test (`tests/cli_robot.rs`) plus 33 process-level `cli_*` tests.

### 2026-09-2x — execution, algorithm, and kernel landings

- FreeJoin joins compiled into maintained relational circuits (`c0f0740f`); six-kind checked joins over complete relational queries (`f8c37559`).
- GQL relational WHERE fused with ordered equality probes (`ee4d964d`).
- CSV incremental decode (`6ae9747f`).
- Prism weighted shortest paths directly over compressed Strata rows (`b2d7761c`).
- Aegis payload-survival math across failure domains (`2f6abad0`, `d390c6bd`).

### 2026-09-22 — milestone closures and seven new workspace crates

Milestone beads closed: `fgdb-w4-g1-txn-core-qpmg` (W4/G1 transaction core; *reopened 2026-09-22 and still open 2026-09-24: its SSI, secure-view and merge-ladder blockers are open, and the tracker is authoritative*) and `fgdb-boundplan-gla-lowering-seam-r2kd` (BoundPlan GLA lowering seam); `fgdb-w3-properties-gou.2` closed. Seven new workspace crates materialized 2026-09-21/22:

- `fgdb-cli`;
- `fgdb-order` — Aegis deterministic Raft transition kernel;
- `fgdb-policy` — capability policy verifier IR;
- `fgdb-prism` — snapshot projections + sealed-view FNX kernels including Dijkstra;
- `fgdb-repl` — bonded replica catch-up;
- `fgdb-warden` — macaroon issuance/attenuation;
- `fgdb-beacon` — memory-resident HNSW/BM25/hybrid index. Reachable from GQL as `CALL hybrid.search(name => value, ...) YIELD node, score, ...` (text, vector and graph lanes fused by exact RRF; a vector is one property per coordinate or one packed little-endian f32 byte property, the embedding form; schema-name arguments resolve at prepare time; privileged and capability-authorized hosts, so `fgdbd` serves it). Built per call at the read's snapshot: no durable index definition yet.

Topology: `g0_topology_e2e` is green and `topology_registry_validates_clean` passes as of 2026-10-05, with all seven once-contested crates registered; bead `fgdb-topology-seven-crates-9n8ao` is still open and should be reconciled against that. The checker index grew from 99 rows (57 live) at the 2026-09-07 census to 120 rows (74 live: 16 script, 11 binary, 47 cargo-test) by 2026-09-22.

### Posture claim, limited on purpose

The embedded library posture is live. The CLI binary is real, but the robot contract is mid-consolidation. The server posture is live as a subset since 2026-10-06 (see below). No claim beyond that is made here.

## Server posture (2026-10-06)

`fgdbd` (`crates/fgdb-server`) composes the embedded engine behind `fgdb-protocol`'s FGP connection machine over asupersync TCP. Witness: `crates/fgdb-server/tests/loopback.rs` over real loopback TCP and a durable on-disk database.

- Handshake HELLO/HELLO_ACK/AUTH/AUTH_OK/SELECT_DATABASE/READY with typed canonical bodies (`crates/fgdb-protocol/src/body.rs`), a transcript-derived session binding, and one uniform refusal for a nonexistent or unauthorized database.
- Authentication by Warden capability token (`fgdbd token`); each EXECUTE builds a fresh `authorized_read_session` or `authorized_write_session`, so scope, expiry and signed budgets are rechecked per statement and masking applies before expansion.
- Results as ephemeral SNAPSHOT_RESULT chunks on a server-minted stream under exact byte-and-row flow credit, with QUERY_CANCEL, PING and DRAIN served while a stream waits; SIGINT/SIGTERM drain with GOODBYE.
- Clients: `fgdb_protocol::client::Client` and the CLI's `fgdb remote query|write`.
- An HTTP/1.1 JSON adapter (`fgdbd serve --http-listen`) over the same execution path, with a Host allow-list.
- Live subscriptions: `SUBSCRIBE TO <read>` over FGP pushes the engine's maintained-query baseline and exact per-commit deltas as SUBSCRIPTION_BATCH frames under flow credit (`fgdb remote subscribe`). They require an unrestricted-scope capability (maintained queries are not capability-masked yet) and are in-process only: no durable resume across reconnects.

- The read-only Bolt-compat subset (`BoltCompatProfileV1`, `fgdbd serve --bolt-listen`, crate `fgdb-bolt`): official Neo4j drivers run autocommit and explicit read transactions over Bolt 5.0 under a capability token. Nodes, relationships and paths include capability-visible metadata from the same session and selected historical sequence; paths preserve direction and repeated identities, also inside collections. Writes refuse with `Neo.ClientError.Statement.AccessMode` before graph access. The base driver surface was verified with the official Python driver (`execute_query`, `execute_read`, sessions, `neo4j://` routing); graph-value coverage lives in `tests/loopback.rs` and the adapter's unit tests.

Not yet: durable result retention with ACK/release/resume, PREPARE, explicit multi-statement write transactions and reattachment, capability-masked and durable subscriptions, multi-tenant admission/QoS, Bolt writes, and the HTTP/2, gRPC and WebSocket adapters. Names resolve through operator-declared bindings because there is no durable catalog.

## Query evidence tower

| Evidence | Binds | Does not bind |
|---|---|---|
| `GqlCertificate` | exact statement bytes, canonical `RelationBind`, snapshot sequence | parsed plan, rows, transaction overlay, physical plan, runtime cost |
| `GqlPlanCertificate` | every current `BoundPlan` field and snapshot sequence under transcript v2 | statement spelling, rows, transaction overlay, physical plan, runtime cost |
| durable ordered-result digest | plan digest, snapshot, exact row count, order, every returned `VId` | physical plan, cost, authorization, envelope framing |
| staged-effect digest | transaction basis and canonical staged `LogicalDeltaTemplate`, or explicit empty overlay | durable snapshot, staged bytes as payload, API-call history |
| `GqlOverlayResultCertificate` | basis, plan digest, staged-effect digest, exact row count, order, every returned `VId` | durable/staged payloads, conflict state, standalone replay |
| query-execution stats | admitted table count and final row count | cryptographic attestation, I/O/allocation cost, operator work, wall time |

The v2 plan transcript includes `BoundPlan::neq`, omitted by historical v1. New certificates use v2; legacy-v1 verification is explicit.

### Exact staged-overlay result evidence

`WriteTxn::staged_effect_digest` binds the basis and canonical semantic net effect under `fgdb:write-txn-staged-effect:v1`.

`execute_prepared_query_with_overlay_result_certificate` executes first and then issues exact rows, a plan certificate, and `GqlOverlayResultCertificate`. The result certificate binds basis, plan digest, staged-effect digest, row count, order, and every returned `VId`.

`verifies_prepared_query_overlay_result` derives the current plan and staged-effect identities from the live transaction. A later staged mutation invalidates old evidence. Equivalent transactions at the same basis with the same canonical net effect can verify the same certificate.

This is exact in-process evidence. It is not standalone replay because the certificate does not carry the durable snapshot, staged template bytes, graph rows, read-set state, or conflict state.

## Strict evidence envelopes

The live tree has two canonical **unreleased application envelopes**:

- `GqlPreparedResultArtifact` for one durable prepared-query result;
- `GqlOverlayResultArtifact` for one staged-overlay result.

The common v1 framing is owned by `crates/fgdb-gql/src/evidence_artifact.rs` and contains:

- eight-byte magic `FGQEVID1`;
- explicit major/minor version;
- a closed artifact-kind tag;
- zero-required reserved bytes;
- exact snapshot sequence or transaction basis;
- statement, canonical bind, and plan digests;
- staged-effect digest for overlay artifacts;
- exact row count and ordered `VId` bytes;
- the applicable exact result digest.

Decoding fails closed on invalid magic, unsupported versions, wrong kind, nonzero reserved bytes, row-count or length overflow, every truncated prefix, trailing bytes, and an inconsistent result transcript. Artifact fields are private and rows are redacted from `Debug` output.

### Resource-safe evidence admission

`GqlEvidenceLimits` is policy applied before untrusted artifacts allocate their row vectors. It bounds:

- total encoded bytes;
- declared row count.

`GqlEvidenceLimits::DEFAULT_UNTRUSTED` is an application default, not a format maximum or product SLO. Callers may supply a different policy. Exact limits succeed; one-below limits return `GqlEvidenceLimitExceeded` with the dimension, configured limit, and observed value.

Malformed headers retain the strict decoder's existing syntax errors. A valid header with a hostile declared row count is refused before row allocation.

Canonical owner: `crates/fgdb-gql/src/evidence_limits.rs`. Product adapters live in `crates/fgdb/src/write_txn_parts/evidence_limits.rs`.

### Durable issuance and audit

`Database` and `EmbeddedReadView` expose:

- `execute_prepared_query_artifact[_at]`;
- `audit_prepared_query_artifact`;
- `audit_untrusted_prepared_query_artifact`;
- `audit_prepared_query_artifact_with_limits`.

A resource-safe durable audit:

1. checks encoded-byte and declared-row policy;
2. strictly decodes the envelope;
3. verifies the retained statement and canonical bind;
4. recomputes the canonical plan certificate at the artifact sequence;
5. cross-checks the independently decoded result transcript against that certificate;
6. re-executes the query at the exact historical sequence;
7. requires exact ordered-row equality.

A later write may advance the live frontier without invalidating an older artifact: audit reopens the sequence named by the artifact rather than sampling current state.

### Staged-overlay issuance and audit

`WriteTxn` exposes equivalent issuance and resource-safe audit for `GqlOverlayResultArtifact`. A staged audit additionally verifies the current transaction basis and canonical staged-effect digest before re-executing the overlay. A later staged mutation returns `StagedEffectMismatch`.

## Result-bound evidence paging

The current tree supports deterministic paging over an **already materialized and audited** evidence artifact.

### Token identity and framing

`GqlEvidencePageToken` has a fixed-width v1 encoding owned by `crates/fgdb-gql/src/evidence_page.rs`:

```text
magic[8] = "FGQPAGE1"
version_major: u16be = 1
version_minor: u16be = 0
kind: u8
reserved[3] = 0
sequence_or_basis: u64be
result_digest: [u8; 32]
next_offset: u64be
checksum: [u8; 32]
```

The token binds:

- artifact kind;
- exact snapshot sequence or transaction basis;
- complete ordered-result digest;
- next row offset.

The checksum is unkeyed. It detects accidental or unsophisticated mutation but is **not** a MAC, signature, authorization decision, capability, or publisher-authenticity proof.

Strict token decoding rejects wrong length, trailing bytes, invalid magic, unsupported version, unknown kind, nonzero reserved bytes, and checksum mismatch. Unit tests walk every truncated prefix.

### Page object

`GqlEvidencePage` contains one contiguous row slice and explicit progress metadata:

- `start_offset`;
- `end_offset`;
- `total_rows`;
- `remaining_rows`;
- optional next token;
- terminal status.

Rows are redacted from ordinary `Debug` output.

### Product-level audit-and-page order

`Database`, `EmbeddedReadView`, and `WriteTxn` expose default-untrusted and caller-limited audit-and-page methods. Their order is deliberate:

1. reject zero page size;
2. strictly decode and checksum-check optional fixed-width token bytes;
3. enforce artifact byte and declared-row policy before row allocation;
4. strictly decode and verify the artifact transcript;
5. verify prepared input, plan, snapshot/basis, and staged effect where applicable;
6. re-execute the exact historical or staged query and compare ordered rows;
7. bind the token to kind, sequence/basis, result digest, and offset;
8. return the contiguous slice.

This avoids expensive replay for an intrinsically invalid page request while preventing a valid token from bypassing artifact admission or replay.

### Exact paging boundary

This is stateless resumability over materialized evidence. It is **not**:

- a database cursor;
- operator streaming;
- bounded-buffer flow control;
- backpressure;
- a session or lease;
- cancellation-aware incremental execution;
- authentication or authorization;
- a reduction in full artifact decode/replay cost.

Every product-level page call re-audits and replays the complete artifact before returning its slice. A genuine cursor requires explicit owner/session identity, lease and cancellation semantics, bounded buffers, backpressure, and an execution/storage path that can stop before materializing the full result.

## Agent-operability verticals

### Exact-SHA local context capsules

`scripts/agent_context.sh` emits a credential-free advisory package containing an exact commit/tree, one-head Git bundle, deterministic source archive, history and tracked-file inventories, stable clean/dirty worktree evidence, and truthful Beads observation modes.

The v2 verifier imports the bundle into an isolated repository and recomputes the tree, archive, paths, and recent history. `agent_context_checkout.sh` materializes a verified detached checkout with no remote.

### Exact-tree local proof bundles

`scripts/local_proof.sh` captures `bash scripts/check.sh` on one clean exact tree and preserves the raw exit, stdout/stderr, tool versions, committed tree, tracked gate-driver blob, and before/after worktree state.

Verdicts remain three-valued: stable `pass`, stable `red`, and moving-tree `void`.

The producer follows one evidence lifecycle: **observe → exclusively claim output → execute → observe → checksum → publish**. Failed state capture, a failed directory claim, incomplete inventory enumeration, or failed checksumming aborts collection with nonzero exit and retained diagnostics. Collection failure is not a fourth completed-proof verdict and cannot be relabeled as `void`: `void` records observed state movement, not a missing observation. No completed `LOCAL_PROOF_*` marker is emitted after a collection failure, and a failed directory claim must not write into another producer's evidence.

`scripts/local_proof_verify.sh` independently validates the complete inventory, checksums, manifest, and reporting contract. Shell statuses must be canonical decimal integers in `0..255`; captured after-state object IDs must be complete single-line Git IDs. Existing format-v1 reading and format-v2 production remain separate from these semantic checks.

**Verified is not green.** A consistent `red` or `void` bundle also passes independent verification. A landing needs a verified `pass` bundle bound to its intended source tree, `check_exit=0`, and `tree_stable=true`. A stable check exiting `125` remains `red`; observed tree movement produces `void` with wrapper exit `125`. Neither wrapper exit `125` nor verifier exit `0`, alone, identifies a green run.

The failure-path witness is `bash scripts/local_proof_selftest.sh`: it uses retained temporary Git repositories and fake check commands to exercise capture and verification without compiling Rust. It is not the repository-wide quality gate. Checksums establish internal consistency, not producer identity; before/after snapshots alone do not prove the absence of every transient edit between observations.

## Agent source-ownership map

| Concern | Canonical owner |
|---|---|
| durable database composition and temporal graph reads | `crates/fgdb/src/lib.rs` |
| one exact-sequence GQL read kernel | `crates/fgdb/src/gql_exec.rs` |
| parser, binder, `RelationBind`, `BoundPlan` | `crates/fgdb-gql/src/parser.rs` |
| typed statement parameters | `crates/fgdb-gql/src/parameters.rs` |
| `PreparedGqlQuery` and query-execution budgets | `crates/fgdb-gql/src/prepared.rs` |
| staged-result transcript | `crates/fgdb-gql/src/overlay_evidence.rs` |
| strict evidence framing and independent decoder | `crates/fgdb-gql/src/evidence_artifact.rs` |
| evidence byte/row admission | `crates/fgdb-gql/src/evidence_limits.rs` |
| result-bound page tokens and slices | `crates/fgdb-gql/src/evidence_page.rs` |
| owned prepared execution adapters | `crates/fgdb/src/write_txn_parts/owned_prepared.rs` |
| canonical staged-effect authority | `crates/fgdb/src/write_txn_parts/overlay_evidence.rs` |
| artifact issuance and replay audit | `crates/fgdb/src/write_txn_parts/portable_evidence.rs` |
| artifact resource admission adapters | `crates/fgdb/src/write_txn_parts/evidence_limits.rs` |
| audit-and-page adapters | `crates/fgdb/src/write_txn_parts/evidence_page.rs` |
| durable certificate authority | `crates/fgdb/src/gql_cert.rs` |
| staged transaction overlay and conflict tracking | `crates/fgdb/src/write_txn_parts/` |
| independent semantic oracle | `crates/fgdb-reference` |
| crash/differential harness | `crates/fgdb-sim` |
| exact-tree proof capture and publication | `scripts/local_proof.sh` |
| independent proof-bundle validation | `scripts/local_proof_verify.sh` |
| proof capture/validation fault witnesses | `scripts/local_proof_selftest.sh` |

Do not add a second parser, binder, execution kernel, session type, or issuing certificate authority merely to make a narrow test pass. Independent artifact decoding is allowed only while product audit cross-checks the canonical certificate transcript, as the current path does.

## Focused witnesses and tests

```bash
cargo run -p fgdb --example open_a_database
cargo run -p fgdb --example gql_owned_prepared
cargo run -p fgdb --example gql_evidence_artifact
cargo run -p fgdb --example gql_evidence_limits
cargo run -p fgdb --example gql_evidence_pages
cargo run -p fgdb --example gql_txn_overlay_result_evidence
```

Focused integration suites include:

- `embedded_read_session.rs`;
- `gql_prepared_at.rs`;
- `gql_plan_certificate_refusals.rs`;
- `gql_aligned_certificates.rs`;
- `gql_result_digest.rs`;
- `gql_owned_prepared.rs`;
- `gql_txn_overlay_result_evidence.rs`;
- `gql_evidence_artifact.rs`;
- `gql_evidence_limits.rs`;
- `gql_evidence_pages.rs`.

## Validation status

The repository-wide authority remains:

```bash
bash scripts/check.sh
```

Every cargo gate in that chain runs `--locked`, so a manifest edit landed without regenerating `Cargo.lock` reds the gate with cargo's own message instead of being rewritten silently. The verdict for a tree is the exit code of that chain executed on that exact tree, captured by `scripts/local_proof.sh` and independently checked with `scripts/local_proof_verify.sh --repository <checkout> <proof-directory>`. Per `AGENTS.md`, automatic hosted CI is not a substitute for this local evidence; do not assume a scheduled or push-triggered run supplies a missing verdict.

A paragraph in this file or in `CHANGELOG.md` describing checks that were not run is not a verdict and must not be read as one. Between 2026-08-31 and 2026-09-02, 141 commits were landed from an environment without the pinned toolchain; the tree did not compile for 33 of them, and the accompanying prose said so instead of refusing. That is recorded as NE-0045 in `docs/NEGATIVE_EVIDENCE.md`, and the repair is `b51e3232` plus the commit that lands this paragraph.

## Major remaining systems

The following remain incomplete or absent:

- canonical parameter evidence for the typed parameters that have landed;
- typed rows/columns and genuine streaming cursors with backpressure;
- full session ownership, authorization, renew/expiry/reattach, and synchronous facade;
- full SSI, predicate/range conflicts, merge ladder, and general multi-relation transactions;
- a registered, compatibility-governed evidence format and external verifier SDK;
- standalone staged-overlay replay carrying the staged template and snapshot authority;
- storage/operator-level early resource enforcement;
- full ISO GQL, GLA lowering, Loom operators, optimizer, spill, and larger-than-memory execution;
- Strata tiers I/R/A and production compaction/migration policy;
- the Ripple crate, Fabric, and maturation of the 2026-09-21/22 crates (`fgdb-order`, `fgdb-policy`, `fgdb-prism`, `fgdb-repl`, `fgdb-warden`, `fgdb-beacon`) into their full planned subsystems;
- the server's remaining surfaces (TLS, durable result machines, explicit transactions, subscriptions, adapters), Python bindings, packaging, signed releases, installer, and upgrade tooling (the robot CLI binary and the `fgdbd` FGP subset have landed — see above).

## Dependency-ordered next work

1. Establish a trustworthy evidence baseline before further feature landings: run the pinned toolchain through `scripts/local_proof.sh`, independently verify the complete bundle against the intended checkout, and distinguish a verified green result from a verified red/void result or aborted collection. Focused proof-wrapper tests do not discharge the whole-tree gate.
2. Typed parameters landed as structural parser operands (`crates/fgdb-gql/src/parameters.rs`); bind canonical parameter values into preparation identity, certificates, and evidence envelopes.
3. Decide whether the application envelope and page token graduate into registered compatibility contracts; freeze ceilings and golden vectors first.
4. Build a real cursor/session owner with cancellation, lease, bounded buffering, and backpressure.
5. Move resource enforcement into storage/operator admission before full table/result materialization.
6. Package staged template bytes and exact snapshot authority before claiming standalone transaction replay.
7. Continue into full SSI and GLA/Loom lowering.

## Recent evolution

- `92e39e59` — one exact-sequence execution kernel for database and immutable read views.
- `01f28d39` through `65d3d832` — verifiable input/plan/result evidence.
- `005a8397` — prepared transaction-overlay execution without rebinding.
- `c7a0558e` through `78137cfb` — owned preparation and deterministic query budgets.
- `80de85a6` through `3e8ff789` — exact staged-effect and staged-result evidence.
- `97d09787` through `10871100` — strict application envelopes and replay audit.
- `a3441930` through `f004bd1c` — pre-allocation evidence byte/row admission.
- `901e4ef5` through `49adac5f` — result-bound stateless paging, audit adapters, progress metadata, and request-preflight laws.
- `9adf484d` — 2026-09-08 full verification (9/9 core, 40/40 registered) with the same-window repair series. Not an ancestor of `main` (NE-0046); not proof of any `main` commit.
- `79548aca` — 2026-09-17 robot CLI binary with frozen NDJSON robot contract.
- `c0f0740f`, `f8c37559`, `ee4d964d`, `6ae9747f`, `b2d7761c`, `2f6abad0`/`d390c6bd` — FreeJoin relational circuits, six-kind checked joins, WHERE/equality-probe fusion, CSV incremental decode, Prism sealed-kernel shortest paths, Aegis payload-survival math.
- 2026-09-21/22 — seven new workspace crates (`fgdb-cli`, `fgdb-order`, `fgdb-policy`, `fgdb-prism`, `fgdb-repl`, `fgdb-warden`, `fgdb-beacon`); milestone closures `fgdb-w4-g1-txn-core-qpmg` (reopened the same day; open) and `fgdb-boundplan-gla-lowering-seam-r2kd`.

Keep this file synchronized whenever a capability crosses from plan or red-bar acceptance test into an inhabitable public path.
