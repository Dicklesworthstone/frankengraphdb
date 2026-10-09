# frankengraphdb

<div align="center">

[![License: MIT + Rider](https://img.shields.io/badge/License-MIT_+_OpenAI/Anthropic_Rider-blue.svg)](./LICENSE)
[![Rust Edition](https://img.shields.io/badge/Rust-2024_Edition-orange.svg)](https://doc.rust-lang.org/edition-guide/rust-2024/)
[![toolchain: nightly](https://img.shields.io/badge/toolchain-nightly-purple.svg)](./rust-toolchain.toml)
[![unsafe: forbidden*](https://img.shields.io/badge/unsafe-forbidden*-success.svg)](https://github.com/rust-secure-code/safety-dance/)
[![language: GQL ISO/IEC 39075:2024](https://img.shields.io/badge/language-GQL_ISO%2FIEC_39075%3A2024-teal.svg)](https://www.iso.org/standard/76120.html)
[![deps: closed universe](https://img.shields.io/badge/deps-closed_universe-black.svg)](./COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md)

**A blank-slate, memory-safe, ultra-high-performance property-graph database in Rust, built on the Franken/asupersync ecosystem. It unifies MVCC, time-travel history, git-style branches, replication, and change subscriptions into a single fountain-coded commit stream, runs transactional writes and static-CSR analytics on one temperature-tiered store, and makes every query result deterministic, auditable, and replayable.**

</div>

```bash
# What runs today, from a source checkout (there are no releases or installer yet — see Installation):
git clone https://github.com/Dicklesworthstone/frankengraphdb
cd frankengraphdb
cargo run -p fgdb --example open_a_database
```

> **A note on tense (read this first).** This README is written in the **present tense, as if the entire design in [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md`](./COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md) is fully realized**: the 1.0 target state where every performance gate is green and every subsystem is live. This is a deliberate choice. It lets the document describe the *finished* system so it gets **trued-up in place as milestones land** (§19's gates G1→G4) rather than rewritten from scratch later. Where the plan itself stages something as genuinely future work (horizontal sharding, covered under [Limitations](#limitations)), the README says so plainly. Everything else below is the spec of the system this repository builds.

---

## TL;DR

**The problem.** Every graph database on the market is a compromise fossilized around one old decision. Neo4j chose pointer-chasing on the JVM (great ergonomics, painful memory, a runtime that took 15 years to vectorize). TigerGraph chose MPP with a proprietary language and a platform-sized footprint. Kùzu got the *query* story right (columnar CSR + vectorization + factorization + worst-case-optimal joins), and then it left the commons: Apple acquired Kùzu Inc. (agreed October 2025, disclosed February 2026 via Apple's EU DMA filing) and the upstream repository was archived read-only — community forks carry the architecture forward while inheriting its gaps, and no engine in the space has the durability, temporal, multi-writer, or verification story. Memgraph/FalkorDB are fast in-memory engines with thin durability. JanusGraph/NebulaGraph pay a permanent impedance tax on a generic KV underlay. The academic frontier has solved essentially every hard subproblem *in isolation*, yet **no shipping system has ever composed them**.

**The solution.** `frankengraphdb` composes them. One codebase, three postures: an embedded library (`fgdb`), a server (`fgdbd`), and a CLI (`fgdb`), all speaking **GQL** (ISO/IEC 39075:2024) with an openCypher on-ramp. Larger-than-memory is first-class everywhere. It is, in a precise sense, *a database written in the asupersync programming model*, the way FoundationDB is a database written in Flow: structured concurrency, capability contexts, fountain coding, and a deterministic lab runtime are the substrate, not add-ons.

**Why `frankengraphdb`:**

| | `frankengraphdb` |
|---|---|
| Durability | Content-addressed, RaptorQ-erasure-coded commit stream. **No double-write journaling anywhere**; bit-rot is a maintenance event, not an outage. |
| Time | Unbounded, queryable history (`FOR SYSTEM_TIME AS OF/BETWEEN`) as a *corollary* of how MVCC works, not a bolt-on temporal engine. |
| Branches | `git`-style database branches: O(1), zero-copy, 10k+ concurrent. Fork, mutate, run analytics, merge or discard. |
| Storage | Three temperature tiers per vertex: inline micro-adjacency → sorted delta blocks → sealed compressed CSR runs. Hot minority pays delta cost; cold majority sits *below* raw CSR. |
| Execution | One Free-Join operator family: binary hash joins, worst-case-optimal multiway joins, and factorized intermediates in one continuum, over runs that *are already tries*. |
| Incremental | One DBSP-style Z-set engine drives recursion, materialized views, subscriptions, and incremental analytics; the commit stream *is* the delta stream. |
| Determinism | Same state + same query + same policy ⇒ byte-identical results, *including order*. Every result ships an auditable, replayable **plan certificate**. |
| Verification | The whole database runs under a deterministic lab runtime: virtual time, DPOR schedule exploration, chaos injection, seed-replayable failures. FoundationDB-class, largely by inheritance. |
| Governance | Capability tokens (macaroons) with graph caveats compile to **planner-enforced** row/subgraph security, applied before expansion, never as a post-filter. |
| Retrieval | `hybrid.search(text, vector, seeds, expand)` fuses ANN + BM25 + graph expansion *inside one planner*: GraphRAG's retrieval step as one optimized operator, transactional and time-travelable. |
| Safety | `unsafe_code = "forbid"` workspace-wide, with a ledgered boundary for the few SIMD/arena/VFS islands, each carrying a bit-identical scalar fallback. |
| Dependencies | **Closed universe.** `std` + the pinned nightly + three owned foundations. No serde, no tokio, no rocksdb, no arrow, no tantivy, no hnswlib in first-party code or on any new dependency path. Ever. (The foundations' own transitive dependencies, which do include serde, are enumerated and pinned in `deny.toml`.) |

---

## Quick example

```gql
-- Create a graph and write some data (serializable by default)
CREATE GRAPH social;

INSERT (:Person {name: 'Ada',  born: 1815})
       -[:KNOWS {since: 1833}]->
       (:Person {name: 'Charles', born: 1791});

-- Pattern match with a quantified path and a shortest-path selector
MATCH p = SHORTEST (a:Person {name: 'Ada'})-[:KNOWS]->{1,4}(b:Person)
RETURN b.name, path_length(p);

-- Time travel: the graph as it was at a past commit; no separate temporal engine
MATCH (p:Person) FOR SYSTEM_TIME AS OF SEQ 41999 RETURN p.name;

-- Branch like git: fork, experiment, keep or throw away; O(1), zero-copy
CREATE BRANCH what_if FROM social@trunk;
MATCH (p:Person {name: 'Ada'}) AT BRANCH what_if SET p.born = 1816;
MERGE BRANCH what_if INTO trunk;          -- semantic intent-log merge; conflicts are a queryable report

-- In-database analytics over the live snapshot, zero-copy, under full isolation
CALL fnx.pagerank(GRAPH social) YIELD node, score
RETURN node.name, score ORDER BY score DESC LIMIT 10;

-- Standing query: a live changefeed maintained incrementally by Ripple
SUBSCRIBE TO MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE b.born < 1800;

-- GraphRAG retrieval in one fused operator (ANN + BM25 + graph expansion, exact RRF)
CALL hybrid.search(
  text   => 'computing pioneers',  text_property => 'bio',
  vector => $query_embedding,      vector_properties => ['e0', 'e1', 'e2'],
  seeds  => $ada, relation => 'KNOWS', max_hops => 2,
  k => 20, fusion => 'RRF'
) YIELD node, score RETURN node.name, score ORDER BY score DESC;

-- Every result is auditable: get its plan certificate
EXPLAIN (CERTIFICATE) MATCH (a:Person)-[:KNOWS]->(b) RETURN count(*);
```

> **Target state.** Checked 2026-09-28, and again 2026-10-05, by running each statement through the `fgdb` CLI on a fresh database: 6 of these 11 statements run as written: the `INSERT`, the `SHORTEST` quantified-path match, `FOR SYSTEM_TIME AS OF SEQ` (given a sequence number the database has retained), `EXPLAIN (CERTIFICATE)`, `CALL hybrid.search` (named arguments; the corpus is the projection the arguments name, built per call at the read's snapshot, and `$ada` is a list of seed vertices) and `SUBSCRIBE TO` (served by `fgdbd` and streamed by `fgdb remote subscribe`, or registered in process with `Database::subscribe_native`; a read with no `RETURN` subscribes to its bound variables). `CALL fnx.pagerank() YIELD node, score ...` also runs without the `GRAPH social` argument. Not yet: `CREATE GRAPH` (there is no graph catalog), the three branch statements (there is no branch catalog, fork, merge or branch write; `AT BRANCH` works on reads only, against a pinned snapshot the host resolves through `Database::query_with_branch_resolver`), and naming a graph in `CALL` (fgdb-luq0b).

---

## The six bets

No single trick makes this a leapfrog. The **composition** of six bets does, each at or beyond the current frontier, each feasible only because the foundation libraries already exist.

| Bet | One-line statement |
|---|---|
| **B1 · One Version Universe** | MVCC versions, time-travel history, replication stream, change subscriptions, and git-style branches are *the same mechanism*: an append-only, content-addressed, RaptorQ-coded commit stream (**Chronicle**). |
| **B2 · Graph-Structured LSM ("Strata")** | Adjacency lives in three temperature tiers (versioned delta blocks → sealed compressed CSR runs → archived anchors), so one store targets high transactional ingest *and* static-CSR-class scans; the actual rates are CI-enforced benchmark gates, not assumptions. |
| **B3 · Unified Factorized/WCO Execution ("Loom")** | One Free-Join operator family subsumes binary hash joins, worst-case-optimal multiway joins, and factorized intermediates, running vectorized and morsel-parallel over Strata runs that *are already tries*. |
| **B4 · Incremental Everything ("Ripple")** | A DBSP-style Z-set delta algebra is the single engine for recursive queries, materialized views, standing queries, and incremental analytics, fed by the commit stream, which is *already* a Z-set stream. |
| **B5 · Determinism as a Product Feature** | CGSE tie-break policies, complexity witnesses, and plan certificates make every eligible **STRICT** result byte-reproducible and auditable; every adaptive decision emits a replayable **decision card**; the whole database runs under the lab runtime for DPOR-explored, seed-replayable testing. |
| **B6 · Agent-Native by Construction** | Branch-per-agent isolation with semantic merge, capability-scoped subgraph authorization, provenance as first-class edges, hybrid vector+text+graph retrieval in one planner, and deterministic replay of any agent's reads. |

---

## Design philosophy

These are the constitutional, non-negotiable constraints the whole system is built under. They read like restrictions; they are the moat.

1. **The dependency universe is closed.** Allowed: `core`/`alloc`/`std`, the pinned Rust nightly, and three foundations: [`asupersync`](https://github.com/Dicklesworthstone) (runtime, RaptorQ, networking, lab runtime, macaroons), the `fnx-*` crates of [`franken_networkx`](https://github.com/Dicklesworthstone) (550+ graph algorithms, the CGSE determinism doctrine), and design-level reuse of [`frankensqlite`](https://github.com/Dicklesworthstone). Every codec, sketch, index, parser, and wire format is built in-house. The entire dependency surface is auditable, deterministic under lab, and owned.
2. **Memory safety is structural.** `unsafe_code = "forbid"` at the workspace level, with an *unsafe boundary ledger* for the handful of crates that need raw pointers (buffer arenas, SIMD kernels, mmap in the VFS). Every `unsafe` block gets a ledger row and a bit-identical scalar fallback.
3. **`Cx` everywhere.** Every function that does I/O, takes a lock, allocates from a shared arena, or can block accepts `&Cx`, asupersync's capability context. Swap the `Cx` and the entire database runs under the lab runtime. It also makes read-only connections *structurally* unable to express writes, and cancellation-correct query timeouts *structural* rather than conventional.
4. **Deterministic by default.** Same state + same query + same policy ⇒ byte-identical results, always, including result order. Nondeterminism is opt-in and *declared in the certificate*.
5. **The commit stream is the source of truth.** There is no mutable primary file. The only mutable object in a database directory is `manifest.root`. Everything else is immutable, content-addressed, and erasure-coded. Derived structures (indexes, views, statistics) are never more authoritative than the commit stream; recovery discards and rebuilds them.
6. **Prohibited shortcuts are constitutional.** No global-lock "interim" transaction model; no `HashMap<VId, Vec<EId>>` presented as storage; no snapshot isolation quietly labeled "ACID"; no parser-interprets-AST engine; no non-durable benchmark mode reported as a result; no serde-derived enum as a durable format; no detached background thread.

---

## How it works

`frankengraphdb` is nine named subsystems over one commit stream. Each maps to a crate family (§18 of the plan) and a supervised region in the process tree.

```
┌──────────────────────────────────────────────────────────────────────────┐
│  FABRIC + WARDEN   sessions · macaroon caveats · admission control       │
│  GQL · openCypher · Datalog   over   FGP · HTTP/2 · gRPC · WS · Bolt     │
└──────────────────────────────────────────────────────────────────────────┘
                                     ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  LOOM   parse → GLA algebra → cost-based optimizer →                     │
│         vectorized, morsel-parallel FreeJoin / factorized / path ops     │
└──────────────────────────────────────────────────────────────────────────┘
                  ▼                                       ▼
┌──────────────────────────────────┐    ┌──────────────────────────────────┐
│  PRISM   fnx algorithms over     │    │  RIPPLE   DBSP Z-set circuits    │
│  SnapshotGraphView (zero-copy)   │    │  views · subscriptions · stats   │
└──────────────────────────────────┘    └──────────────────────────────────┘
                                     ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  STRATA   Tier I inline · Tier D delta blocks (block-MVCC) ·             │
│           Tier R sealed Elias-Fano/varint CSR runs · Tier A anchors      │
│  BEACON   B-tree/hash · adjacency views · FTS/BM25 · HNSW · path idx     │
└──────────────────────────────────────────────────────────────────────────┘
                                     ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  CHRONICLE (B1)   content-addressed ECS objects · RaptorQ symbols ·      │
│    CommitCapsule + two-fsync CommitMarker chain · WriteCoordinator ·     │
│    retention tiers = temporal DB · branches · scrub / self-heal ·        │
│    the only mutable file: manifest.root                                  │
└──────────────────────────────────────────────────────────────────────────┘
                                     ▼
┌──────────────────────────────────────────────────────────────────────────┐
│  AEGIS   Raft-sequenced markers · RaptorQ bulk plane ·                   │
│          bit-identical replicas · bonded multi-donor seeding · PITR      │
└──────────────────────────────────────────────────────────────────────────┘

All of it runs under FGDB-SIM (asupersync lab runtime): virtual time,
DPOR, chaos, seed-replayable failures. Same code, a different Cx.
```

- **Chronicle (B1):** the ECS-native durability substrate. Every durable thing is an immutable, content-addressed object (`ObjectId = Trunc128(BLAKE3(...))`) stored as RaptorQ symbols. A commit is the two-fsync protocol: build a deterministic `CommitCapsule` (snapshot basis + graph intent log + block deltas + SSI witnesses) off the critical path, then a single-actor `WriteCoordinator` validates, allocates a gap-free `CommitSeq`, fsyncs the capsule, appends a ~100-byte `CommitMarker`, and fsyncs again. Retired versions don't die; they *cool* through retention tiers, which *are* the temporal database (AeonG's anchor+delta, where the deltas are commit capsules we already stored). Branches are just a `BranchManifest` over shared content-addressed state.
- **Strata (B2):** the storage engine that refuses to pick a point on the write/scan/space trilemma. Adjacency for each `(vertex, direction, edge_type)` triple migrates by temperature: inline in the vertex directory row (the long tail of power-law graphs lives here, *below* raw CSR), then sorted 256-edge delta blocks with per-block MVCC, then sealed Elias-Fano + delta-varint CSR runs (2.5–5 bits/edge typical), then cold anchors. Most vertices read as pure CSR with zero delta presence.
- **Loom (B3):** one Graph-Logical Algebra, one cost model, no "graph engine bolted to SQL engine" seam. The `FreeJoin` operator is a continuum from binary hash joins to worst-case-optimal Generic Join; for graph atoms, sealed runs *already are* the tries, so intersections run as SIMD galloping over compressed neighbor lists. Factorization is a *type* in the algebra, not an executor trick: a 3-hop friends-of-friends result that would be 10⁸ flat rows stays ~10⁴ run slices, even over the wire.
- **Ripple (B4):** a from-scratch DBSP-style Z-set circuit engine. Recursion is incremental fixpoint (semi-naïve for free). `CREATE MATERIALIZED VIEW … REFRESH INCREMENTAL` installs a circuit whose output is snapshot-consistent at a published watermark. `SUBSCRIBE TO MATCH …` is the same circuit fanned out over broadcast channels. Incremental analytics maintainers keep PageRank, connected components, and statistics warm under updates, each with a declared staleness contract.
- **Beacon:** the index fabric. It holds property B-trees/hash, A+-style adjacency views, segment-based FTS/BM25, and a **transactional HNSW** vector index (the Strata pattern applied to the index itself, so vector search respects your snapshot, honors `AS OF`, is branch-scoped, and is fresh in commit-latency, not reindex-hours). All indexes share one lifecycle and one optimizer registration surface.
- **Prism:** the entire `franken_networkx` catalog (`CALL fnx.pagerank`, `fnx.louvain`, …) exposed inside queries via an authorized `SnapshotGraphView`: algorithms traverse borrowed neighbor cursors without whole-graph materialization (an honest, explicit copy boundary where a trait contract requires one), under full snapshot isolation, with CGSE witnesses folded into the query certificate.
- **Warden:** capability tokens (macaroons) with graph caveats (`labels⊆{…}`, `subgraph=MATCH-predicate`, `asof≤seq`, `ops⊆{…}`) that compile to mandatory planner predicates. Row/subgraph security with index-aware pushdown, encrypt-then-code at rest, TLS 1.3 in flight, per-branch DEK derivation for cryptographic branch hand-off.
- **Fabric:** the surface. Embedded API, the native **FGP** wire protocol (with optional factorized frames and RaptorQ FEC for lossy links), HTTP/2 + gRPC + WebSocket, a Bolt-compat subset for Neo4j drivers/tools, Python bindings, and format import/export for the whole legacy graph ecosystem plus a Parquet-lite reader/writer.
- **Aegis:** replication as *Chronicle over the network*. Raft sequences the ~100-byte marker stream while capsule bytes ride the fountain-coded plane; deterministic apply makes replicas **bit-identical** (divergence is detectable by `ObjectId` comparison). New replicas seed via bonded multi-donor ATP pulls. Sharding is designed-in but sequenced last (see [Limitations](#limitations)).

The full census lives in [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md`](./COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md): every asset reused from each foundation, every SOTA system's adopt/adapt/reject verdict, and the normative on-disk formats.

## How it compares

Honest framing. `frankengraphdb` is the only one of these that composes durability, unbounded time-travel, git-style branches, unified WCO/factorized execution, incremental everything, and deterministic verification in a single engine.

| | `frankengraphdb` | Neo4j | Kùzu | Memgraph | TigerGraph |
|---|---|---|---|---|---|
| Language / runtime | Pure Rust, no JVM/GC | JVM | C++ (embedded) | C++ | C++ (MPP) |
| Native language | GQL + openCypher | Cypher | Cypher | Cypher | GSQL (proprietary) |
| Storage model | Temperature-tiered CSR LSM | Pointer-chasing | Columnar CSR (static-leaning) | In-memory | MPP |
| Execution | FreeJoin + factorized + WCOJ | Vectorized (late-arriving) | Vectorized + factorized + WCOJ | Vectorized | MPP |
| Durability | RaptorQ commit stream, no double-write | WAL + store files | Single-writer WAL | Thin durability | Distributed |
| Time travel | Unbounded, queryable (`AS OF`/`BETWEEN`) | ✗ (external) | ✗ | ✗ | ✗ |
| Git-style branches | ✓ O(1), 10k+ concurrent | ✗ | ✗ | ✗ | ✗ |
| Incremental views / subscriptions | ✓ one Z-set engine | Partial (CDC) | ✗ | Triggers/streams | Partial |
| Transactional vector search | ✓ (snapshot + `AS OF` + branch) | Plugin (eventual) | ✗ | Plugin | ✗ |
| In-DB analytics catalog | 550+ (fnx, cursor-borrowed) | GDS library | Limited | MAGE | Built-in |
| Deterministic, replayable results | ✓ plan certificates | ✗ | ✗ | ✗ | ✗ |
| Deterministic simulation testing | ✓ lab runtime + DPOR | ✗ | ✗ | ✗ | ✗ |
| Status of the project | Active, self-owned ecosystem asset | Commercial | **Archived 2025 (acquired by Apple)** | Commercial | Commercial |

> **Target state.** The `frankengraphdb` column is the design target. Where today differs (checked 2026-09-24): **branches** exist only as read selectors: `AT BRANCH name` reads a pinned snapshot that the host maps the name to (`Database::query_with_branch_resolver`, `crates/fgdb/src/query_branch.rs`). There is no branch catalog, fork, merge or branch write (`BranchManifest` is deliberately absent from `fgdb-chronicle`). **Vector search** is snapshot-consistent and honours `AS OF`, but a search builds its HNSW index in memory for that call, or reads a resident index whose generations are refreshed explicitly (`crates/fgdb/src/query_beacon.rs`, re-checked 2026-09-28); there is no durable, commit-maintained index, freshness gate or branch scope yet. **Replayable results** cover the native read subset (`Database::replay` over native result certificates), not every statement. **Deterministic simulation** runs the lab runtime and DPOR over bounded, declared scenarios (`crates/fgdb-sim`: generated transaction schedules, crash-point differentials), not yet the whole database including server and replication.

## The `fgdb` CLI

> The CLI mirrors the embedded surface. Robot mode emits line-oriented, versioned NDJSON so an agent can pipe and validate the stream against a frozen contract (`fgdb robot schema`); human output is the default.

**Runs today** — one binary (`crates/fgdb-cli`), invoked below as `fgdb`. Prefix any invocation with `--robot` for the NDJSON contract. Stable exit codes: 0 success, 2 usage/schema, 3 query refusal, 4 open/key, 5 I/O/corruption. A key file is three nonempty lines of 64 hex characters (object-id key, security namespace, encryption key), owner-only (`chmod 600`) on Unix:

```bash
# Create a database directory under an explicit key file
fgdb create --db mydb.fgdbdir --key-file fgdb.keys

# Run a GQL query (human output; --robot for versioned NDJSON)
fgdb query --db mydb.fgdbdir --key-file fgdb.keys "<gql>"

# Stream native rows one at a time, or save a portable result certificate
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --stream "<gql>"
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --certify-to result.cert "<gql>"

# Read checkpoint objects through a bounded graph cache and flush each result row
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --property score=1 \
  --buffered --buffer-memory-bytes 67108864 \
  "MATCH (n) RETURN n AS node, n.score AS score LIMIT 100"

# External ORDER BY/DISTINCT with private query scratch in an existing directory
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --property score=1 \
  --spill-dir /tmp --spill-memory-bytes 67108864 --spill-disk-bytes 1073741824 \
  "MATCH (n) RETURN n AS node ORDER BY n.score DESC LIMIT 100"

# Numeric grouping with bounded resident groups and exact aggregate values
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --property score=1 \
  --spill-dir /tmp \
  "MATCH (n) RETURN n.score AS score, count(*) AS count, avg(n.score) AS mean"

# Combine the bounded graph cache with external ordering or grouping
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --property score=1 \
  --buffered --buffer-memory-bytes 67108864 \
  --spill-dir /tmp --spill-memory-bytes 67108864 \
  "MATCH (n) RETURN n.score AS score ORDER BY score DESC LIMIT 100"
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --property score=1 \
  --buffered --spill-dir /tmp \
  "MATCH (n) RETURN n.score AS score, count(*) AS count, sum(n.score) AS total
   GROUP BY n.score ORDER BY total DESC"

# Commit a write, then compare two committed revisions of one query
fgdb write --db mydb.fgdbdir --key-file fgdb.keys "<gql>"
fgdb diff  --db mydb.fgdbdir --key-file fgdb.keys --before <seq> --after <seq> "<gql>"

# Ordered multi-statement transaction (one commit; --rollback discards everything)
fgdb transaction --db mydb.fgdbdir --key-file fgdb.keys --write "<gql>" --query "<gql>"

# Bulk CSV import, NDJSON delta load with checkpoints, compaction
fgdb import-csv --db mydb.fgdbdir --key-file fgdb.keys --input edges.csv "<gql>"
fgdb load       --db mydb.fgdbdir --key-file fgdb.keys --input batch.ndjson
fgdb compact    --db mydb.fgdbdir --key-file fgdb.keys

# Verify every capsule and block; repair damaged redundancy in place
fgdb scrub --db mydb.fgdbdir --key-file fgdb.keys

# In-database analytics over an explicit projection (registered Prism procedures)
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --relation KNOWS=1 \
  --graph-relation KNOWS --direction undirected "CALL fnx.connected_components() YIELD vertex, component"

# Beacon retrieval: BM25 text, exact/ANN vector, or exact-RRF hybrid over one sequence
fgdb search --db mydb.fgdbdir --key-file fgdb.keys --property title=1 --property x=2 \
  --text "graph memory" --text-property title --vector 0.5 --vector-property x --k 5

# ...or the same retrieval inside GQL, composed with the rest of the statement
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --property title=1 --property x=2 \
  "CALL hybrid.search(text => 'graph memory', text_property => 'title', vector => [0.5],
     vector_properties => ['x'], k => 5) YIELD node, score
   RETURN node.title, score ORDER BY score DESC"

# Real embeddings live in ONE property as packed little-endian f32 bytes
# (`--param e=vector:0.1,0.2,...`, or {"$vector": [...]} over HTTP)
fgdb write --db mydb.fgdbdir --key-file fgdb.keys --label Doc=1 --property title=1 --property emb=3 \
  --param 'e=vector:0.12,0.80,0.05' "CREATE (:Doc {title: 'Ada', emb: \$e})"
fgdb query --db mydb.fgdbdir --key-file fgdb.keys --property title=1 --property emb=3 \
  --param 'q=json:[0.1,0.8,0.1]' \
  "CALL hybrid.search(vector => \$q, vector_property => 'emb', metric => 'cosine', k => 5)
     YIELD node, score RETURN node.title, score ORDER BY score DESC"

# Replay a saved certificate against the current database state
fgdb replay --db mydb.fgdbdir --key-file fgdb.keys --certificate result.cert

# Agent-first surface: the self-describing, frozen event contract
fgdb robot schema

# Serve over the network (FGP over TCP) and query remotely with a capability token
fgdbd keygen --issuer issuer.key
fgdbd serve --listen 127.0.0.1:7687 --database social=mydb.fgdbdir --key-file fgdb.keys \
  --issuer-key-file issuer.key --label Person=1 --relation KNOWS=1 --property name=1
fgdbd token --key-file fgdb.keys --issuer-key-file issuer.key --rights read-write > token && chmod 600 token
fgdb remote --addr 127.0.0.1:7687 --token-file token --database social query "MATCH (p:Person) RETURN p.name"

# Live changefeed: a baseline, then one exact delta per commit
fgdb remote --addr 127.0.0.1:7687 --token-file token --database social \
  subscribe "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name"

# ...or over HTTP/JSON (add --http-listen 127.0.0.1:7474 to serve)
curl -H "Authorization: Bearer $(cat token)" -d '{"statement":"MATCH (p:Person) RETURN p.name"}' \
  http://127.0.0.1:7474/v1/databases/social/query

# ...or from any official Neo4j driver, read-only (add --bolt-listen 127.0.0.1:7688)
python3 -c 'from neo4j import GraphDatabase, bearer_auth
d = GraphDatabase.driver("bolt://127.0.0.1:7688", auth=bearer_auth(open("token").read().strip()))
print(d.execute_query("MATCH (p:Person) RETURN p LIMIT 3").records)'
```

`query --buffered` opens the authenticated checkpoint through the extent cache and feeds the native asynchronous query cursor. On its own, it supports single-vertex and single-edge scans with leading identities, local filters, canonical ordering, `SKIP`/`LIMIT`, parameters, and historical cuts. Preparation and physical-plan admission finish before storage opens. Unsupported joins, probes, aggregation and alternate ordering refuse at that point. Each row keeps its graph-memory reservation until its output is flushed; an error or missing terminal result means incomplete delivery. This flag cannot be combined with `--stream`, `--certify-to`, or a standalone `CALL fnx`.

**`query --buffered --spill-dir <existing-directory>` connects the bounded graph reader to external query execution.** It accepts direct local field, identity and path projections without a leading identity, property and hidden-key ordering, output `DISTINCT`, and pagination. Native computed `RETURN` and `WITH` projections also support arithmetic, scalar functions, lists, maps, comprehensions and reductions, with computed filters between stages. Each stage preserves its own canonical ordering, `DISTINCT`, `ORDER BY`, and `SKIP`/`LIMIT`: selecting an inner page precedes the next projection, and a later `LIMIT 0` cannot hide an earlier expression failure. Numeric grouping uses the same external partitioner and native reducer described below, including computed inputs, `COUNT`/`SUM`/`AVG(DISTINCT ...)`, scalar/list/map outputs, `HAVING`, exact aggregate values, and post-aggregate `RETURN DISTINCT`. These profiles accept one vertex or single-edge source at the current or a historical cut. The bound physical definition is compiled once before storage opens; even `LIMIT 0` cannot hide an unsupported probe, join, `UNWIND`, set operation, relational aggregate input or `COLLECT`. There is no resident source fallback. Source rows remain charged through encoding and asynchronous scratch writes, and complete evaluation of every stage, grouping and sorting precedes columns or rows. The graph pool and scratch pool retain separate limits: their combined allowance is `--buffer-memory-bytes + --spill-memory-bytes`, in addition to the documented preparation, recovery and transport memory outside those pools. Source, intermediate-row, final-row and external-work limits retain their distinct scopes.

`--buffer-memory-bytes` defaults to 64 MiB and covers graph admission, cache contents, native source evaluation and retained source rows. The root byte ceiling is `min(1 MiB, memory/64)`; decoded root metadata reserves sixteen times that ceiling, with cache bookkeeping and compact history metadata charged separately. `--buffer-source-bytes` defaults to 1 GiB and bounds initial admission reads, including exact birth-patch refaults. `--max-work-units` independently bounds initial admission and the cumulative source and native expression evaluation; relational stages continue that allowance. In combined mode, buffered input encoding checks the frame size and reserves its overlapping row, cell and scalar encoding workspaces from the scratch pool before allocation. Relational stage decoding, expression workspaces and output encoding also reserve scratch memory before allocation. Every stage's decoding, encoding, sorting, copying and pagination shares one cumulative `--max-sort-work` allowance, and every intermediate append spends disk quota. Only the final selected rows spend `--max-result-rows`. Chronicle recovery, query preparation and parameters, and the CLI row transport workspace remain outside the graph-memory cap; final sorted-result decoding remains outside both pools. A missing or lagging checkpoint requires ordinary recovery; this mode does not fall back to a resident graph. Admission can still refuse when the metadata for distinct historical versions exceeds the pool.

`query --spill-dir <existing-directory>` uses the native external sorter for vertex and fixed-edge scans, including property-only projections, local predicates, `DISTINCT`, terminal `SKIP`/`LIMIT`, and historical cuts. `ORDER BY` may use unreturned vertex or edge properties and supported path functions: these private cells determine ordering and pagination, then are removed before result rows and column names are published. Multiple sort keys, direction and null placement use the ordinary query semantics. Under `DISTINCT`, sort keys must be projected. Optional or variable-length joins and other unsupported native instructions refuse without an eager retry. It cannot be combined with `--stream`, `--certify-to`, or a standalone `CALL fnx`. Adding `--buffered` selects the bounded vertex/single-edge source profile above.

The same flag supports numeric grouping over vertex and fixed-edge inputs: `count`, `sum`, `avg`, `min`, and `max`, including global summaries and historical cuts. Computed grouping keys and aggregate arguments, such as `sum(n.quantity * n.price)` or `avg(n.p + $weights.delta)`, run through the native expression evaluator before partitioning. `COUNT`, `SUM`, and `AVG` also accept `DISTINCT` arguments, including computed values and hidden `HAVING`/ordering arguments. Membership uses exact native value types: integer `1` and floating `1.0` are distinct members; nulls are ignored. Each distinct argument column is externally sorted within a completed group partition, so membership needs no resident seen-set. Ordinary aggregates in the same query still count all occurrences. Every declared input expression runs, including hidden columns and when `LIMIT 0` requests no output. Input occurrences are written into deterministic partitions, and a partition that exceeds the resident group allowance is split again before reduction. Every group finishes before `HAVING`, computed output expressions, output `DISTINCT`, exact typed `ORDER BY`, and `SKIP`/`LIMIT`; even `LIMIT 0` cannot hide a later invalid qualified group. Computed outputs such as `sum(n.p) * 2`, conditional scalar expressions, lists and maps use the native evaluator once per qualified group and remain in scratch beside the complete hidden ranking cells. `RETURN DISTINCT` externally sorts the final visible tuples by native equality, retains the first complete group under the original ordering for each tuple, and then ranks those representatives before pagination. Equal numeric summaries use their exact native comparison domain; the selected representative keeps its original count, wide integer, scalar or rational value type. Ordering can use hidden aggregate columns, and visible grouping keys may be omitted or repeated. Full canonical grouping keys determine expression evaluation order, break ordering ties, and select representatives when no explicit order is supplied; hidden cells are removed before delivery. Counts remain unsigned counts, integer sums retain their full wide range, and exact averages retain their numerator and denominator. `COLLECT` and relational aggregate inputs still refuse before source execution. Hash collisions or skew that cannot fit within the bounded partition contract return a resource refusal.

The scratch files share one memory allowance (`--spill-memory-bytes`, default 64 MiB), and split one append-only disk allowance (`--spill-disk-bytes`, default 1 GiB) equally across two files for ordering or three for grouping. Every intermediate pass and metadata write spends disk quota. `--max-spill-rows` (default 1,000,000) bounds input occurrences independently of final `--max-result-rows`; `--max-sort-work` (default 1,000,000,000) bounds additional partition, reduction, and sort work independently of the query's source budget. Encoded evaluation rows, including hidden sort keys, and private aggregate frames are capped at 1 MiB; a computed-output or output-`DISTINCT` frame includes both its complete ranking row and its projected row. External grouping charges its resident groups, decoded aggregate rows, computed-output workspaces, projection copies, and distinct-comparison frames to the shared pool before allocation. Deduplication retains one charged previous frame between pulls and preserves the original result cells without reevaluating expressions. The decoded source generation, one native source/computed input row and final transport encoding remain outside that pool; input and output expressions share the source cursor's work and scratch-entry budgets.

Grouping and sorting finish before delivery begins. Rows are decoded and flushed individually with the ordinary native cell types and streaming completion contract. Each invocation creates a private scratch directory and exclusive files, then retires its own files before the success result; failures and cancellation also clean them up. Process death can leave that invocation's private directory, which later invocations never reuse or sweep. An error, unsuccessful exit, or missing terminal result means incomplete delivery.

`fgdbd` (`crates/fgdb-server`) serves the FGP handshake, one autocommit GQL read or write per `EXECUTE`, ephemeral flow-controlled result streams, and live `SUBSCRIBE TO` changefeeds (a baseline, then one exact delta per commit), plus the same statements over an HTTP/1.1 JSON adapter. `CREATE`/`INSERT`, matched `SET`/`REMOVE`/deletion, and vertex `MERGE` can return rows with their native transaction outcome. Their `RETURN` expressions run against the authorized statement result before commit, so expression or result-quota failures discard the writes; `LIMIT 0` still applies effects. The read-only Bolt-compat subset (`BoltCompatProfileV1`, `--bolt-listen`) accepts official Neo4j drivers: autocommit and explicit read transactions each use one pinned generation, nodes include their labels and properties, and writes refuse with `Neo.ClientError.Statement.AccessMode`. Every statement runs through a capability-authorized session built from the connection's Warden token, so a token's label/relation/property scope applies before expansion. It does not yet serve durable result retention (ACK/release/resume), explicit multi-statement write transactions, Bolt writes or relationship/path values, durable or capability-masked subscriptions, or the HTTP/2, gRPC and WebSocket adapters; [docs/FABRIC_PROTOCOL.md](docs/FABRIC_PROTOCOL.md) lists exactly what is served.

To encrypt all configured listeners, add `--tls-cert-file server-chain.pem --tls-key-file server.key` to `fgdbd serve`. The PEM private key must be owner-only. FGP, HTTPS and Bolt then require TLS 1.3 through the pinned foundation; a refused handshake never falls back to plaintext. FGP requires ALPN `fgp/1`, HTTPS requires `http/1.1`, and Bolt retains its encrypted version negotiation without requiring ALPN. Capability authentication and fresh checks before physical output still apply. Clients connect with an explicit trusted CA and the name on the server certificate:

```bash
fgdb remote --addr 127.0.0.1:7687 --token-file token --database social \
  --tls-server-name db.example.test --tls-ca-file ca.pem \
  query "MATCH (p:Person) RETURN p.name"
```

The same TLS options apply to remote writes and subscriptions. HTTP clients use `https://` with their CA verification enabled; Neo4j drivers use their encrypted Bolt configuration. Server identity is validated at startup, early data is disabled, and drain cancels unfinished handshakes.

> **Target state.** The interactive shell, `branch`, `backup`/`restore` archives, `doctor`/`analyze` operations, `robot health`, a `--json` output flag, and the remaining `fgdbd` surfaces above remain W10 composition work (`registries/workspace_topology.toml`). The commands above are exactly the ones that run today.

## Installation

**1. Install script — not yet available.** There are no git tags, no GitHub Releases, no signed binaries, and no fgdb installer script in this repository today; `fgdb-epic-w10-mhq.1` owns shipping them. The curl one-liner returns here when the artifact it fetches exists (a claims-lint tripwire fails the gate if that instruction reappears before the script is a tracked file).

**2. From source** (requires the pinned nightly toolchain, which `rust-toolchain.toml` auto-selects). This is the path that runs today:

```bash
git clone https://github.com/Dicklesworthstone/frankengraphdb
cd frankengraphdb
cargo build --release                        # builds the library workspace
cargo run -p fgdb --example open_a_database  # a real main(): create, commit, reopen, verify
```

A `fgdb` CLI binary is produced today (`cargo build -p fgdb-cli`; [The `fgdb` CLI](#the-fgdb-cli) lists the commands it runs), and so is the `fgdbd` server binary (`cargo build -p fgdb-server`), which serves the FGP subset described above.

**3. Embedded, as a Rust library:**

```toml
# Cargo.toml
[dependencies]
fgdb = { git = "https://github.com/Dicklesworthstone/frankengraphdb" }
```

> **Target state.** The snippet below is the 1.0 embedded surface. Today's crate root differs in shape, not substance (checked 2026-09-24): `Database::create`/`open`/`open_memory`/`write`/`compact` are `async fn`s taking an asupersync capability context (`&CommitCx`), so the caller owns the runtime; GQL runs through `db.query(&cx, text, &params, symbols, policy)`, pinned read views (`db.read_session()`), reusable prepared statements (`PreparedNativeRead::prepare`, `prepare_gql_query`), ordered write transactions (`db.begin(&txn_cx)` → `WriteTxn`), and capability-scoped `AuthorizedReadSession`s. What does not exist yet is this synchronous `db.session().prepare()` shape. The `open_a_database` example and `crates/fgdb/examples/` show today's calls.

```rust
use fgdb::Database;

fn main() -> fgdb::Result<()> {
    // Synchronous, blocking API; the async runtime is an owned internal detail.
    let db = Database::open("mydb.fgdbdir")?;      // or Database::open(":memory:")?
    let mut session = db.session()?;

    let stmt = session.prepare("MATCH (p:Person)-[:KNOWS]->(f) RETURN p.name, count(f) AS deg")?;
    for row in stmt.query(&[])? {
        let name: &str = row.get("p.name")?;
        let deg:  i64  = row.get("deg")?;
        println!("{name}: {deg}");
    }
    Ok(())
}
```

**Buffered native reads available today.** `Database::open_buffered_read_view` opens an owned, fixed checkpoint without constructing the resident graph snapshot or a block writer. Its async `vertex`, `edge`, and bounded incoming/outgoing `adjacency_at` methods fault authenticated immutable objects through the Strata extent cache. The corresponding `*_at` methods read historical statements with the same winning-version and retirement metadata as the ordinary resident view. `open_buffered_read_view_with_vfs` provides the same path through an injected filesystem.

The caller supplies a shared `MemoryPool` and `BufferedReadLimits`: root bytes, source bytes inspected during admission, block/vertex-patch counts, per-operation work, and cache frame/extent limits. Metadata, decoding workspace, admission history, cache frames and returned `BufferedValue` rows retain reservations for their lifetimes. A held result remains charged after its view is dropped. Point reads currently scan the admitted descriptors; bounded adjacency returns the complete winning incidence or a resource error, and parallel edges remain distinct.

Within the caller's async runtime, with `commit`, `query`, and `keys` supplied as the ordinary capability contexts and database keys:

```rust
use fgdb::{BufferLimits, BufferedReadLimits, Database, MemoryPool};
use fgdb_types::VId;

let memory = MemoryPool::new(16 * 1024 * 1024, 0)?;
let limits = BufferedReadLimits {
    max_root_bytes: 64 * 1024,
    max_source_bytes: 64 * 1024 * 1024,
    max_blocks: 512,
    max_vertex_patches: 512,
    max_work: 1_000_000,
    buffer: BufferLimits {
        max_frames: 8,
        max_ghost_entries: 16,
        max_extent_bytes: 16 * 1024,
    },
};
let mut view = Database::open_buffered_read_view(
    &commit, "mydb.fgdbdir", keys, memory.clone(), limits,
).await?;
let vertex = view.vertex(&query, VId(1)).await?;
```

**Buffered GQL execution.** The same view now exposes `stream_graph_values_governed` and its historical `_at` counterpart. Prepare and bind with the existing native GQL compiler and your symbol resolver; execution then pulls canonical vertex histories through the extent cache. The supported projection begins with the scanned vertex identity and can include its canonical properties. Labels, local `WHERE` expressions, `DISTINCT`/`ALL`, and `SKIP`/`LIMIT` use the existing evaluator and row semantics. `stream_graph_vertices_governed` provides the identity-only counterpart for native prepared patterns.

For example, with property name `score` bound to the application's catalog ID:

```rust
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};

let prepared = PreparedGraphText::prepare(
    "MATCH (n) WHERE n.score >= 10 RETURN n, n.score LIMIT 100",
    |kind, name| match (kind, name) {
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    },
)?.bind_parameters(&GqlParameters::new())?;
let policy = GqlQueryPolicy::new(100_000, 100, 10_000_000, 1_000_000);
let mut cursor = view.stream_graph_values_governed(&query, &prepared, policy)?;
while let Some(row) = cursor.next().await {
    println!("{:?}", row?.values());
}
```

The scan merges sorted patches with one small head per patch and one decoded patch, while scan misses bypass point-cache admission. It admits each candidate before reading that identity's history and shares one cumulative query budget across storage and evaluation. Evaluator temporaries and returned `BufferedQueryRow` values reserve bytes before allocation; a returned row stays charged after dropping the cursor or view. A failed or dropped in-flight pull permanently stops its cursor. Earlier successful pulls remain delivered after a later error, so complete success requires exhausting the cursor. Joins, edge expansion, probes, aggregation, property-only output, and alternate ordering still refuse in these direct streaming methods. These owner-level methods do not apply Warden session masking.

For buffered external queries, call `PreparedNativeRead::prepare_buffered_order` or `prepare_buffered_aggregate` with the parameters before opening the view. Each returned bound definition has `spool_in_view`, which pulls the same buffered vertex or single-edge source into the ordinary external sorter or partitioned aggregate executor. The ordering definition also admits native unary projection/filter/scope relations, preserving every intermediate ordering and pagination boundary through separate completed scratch stages; `stage_count()` reports their number. Its `spool_in_view_with_resolver` counterpart accepts the pinned scalar artifact resolver for decoding intermediate values such as zoned timestamps; the CLI threads its existing `--tzdb-file` resolver through these stages. The source uses the view's memory pool for decoding and evaluation, while intermediate stage decoding, evaluation and encoding reserve from the destination scratch pool. It preserves the original visible schema, hidden ranking cells and temporal selector. The source and stage rows retain their reservations through each asynchronous scratch append; the completed spool is accepted only after all source, expression, ordering and result limits succeed. Use two private spill files for ordering and relational stages or three for grouping with their existing shared scratch pool and cleanup contract. The CLI combination above exercises these same embedded APIs.

Opening checks the existing slot, manifest coordinates, Chronicle binding and complete object/history admission. A missing or lagging checkpoint returns `BufferedOpenError::RecoveryRequired`; recovery is an explicit ordinary writable open. The buffered view holds no writer lease once returned, so later writes and compaction leave its selected root unchanged. Vertex-history admission retains compact lifecycle metadata and birth-patch locators; a restatement refaults its original authenticated patch for exact label and property comparison. It keeps at most two decoded object workspaces instead of retaining the complete property history. Metadata still grows with distinct version count and can exhaust the pool. Both original reads and birth-patch refaults consume the source-byte and work limits; source-byte refusal can inspect one format-bounded object beyond the requested limit. Chronicle recovery and caller-owned query preparation/catalog metadata remain outside the pool. The view does not supply a cross-process object-retention/GC lease; the database's immutable object directory must remain available.

**4. Python bindings** (ABI3 wheels, with a zero-friction `to_fnx()` / `from_fnx()` bridge and NumPy views over `Embedding` columns). **Target state:** no wheels are published — `pip install frankengraphdb` installs nothing of ours today. When releases exist:

```bash
pip install frankengraphdb
```

```python
import frankengraphdb as fgdb
db = fgdb.open("mydb.fgdbdir")
for row in db.query("MATCH (p:Person) RETURN p.name LIMIT 5"):
    print(row["p.name"])
```

## Quick start

> **Target state.** The workflow below shows the 1.0 shape. The `fgdb` binary is real today for `create`/`query`/`write`/`diff`/`transaction`/`import-csv`/`load`/`compact`/`scrub`/`search`/`replay` and `fgdb robot schema` (see [The `fgdb` CLI](#the-fgdb-cli)); the `--branch` step, and `fgdbd`'s TOML config and Bolt protocol, await W10 composition (`fgdbd serve` itself runs today with flags, serving FGP and the HTTP/JSON adapter; `fgdb remote subscribe` streams a changefeed from it). The minimal runnable witness is `cargo run -p fgdb --example open_a_database` (see [Installation](#installation)).

```bash
# 1. Create a database directory and bulk-load a graph
fgdb load city.fgdbdir --edges roads.csv --vertices intersections.csv --format csv

# 2. Ask a question
fgdb query city.fgdbdir \
  "MATCH p = SHORTEST (a:Intersection {id: 42})-[:ROAD]->{1,20}(b:Intersection {id: 9001})
   RETURN path_length(p) AS hops"

# 3. Run an in-database analytic over the live snapshot (zero-copy fnx)
fgdb query city.fgdbdir \
  "CALL fnx.betweenness_centrality(GRAPH city) YIELD node, score
   RETURN node.id, score ORDER BY score DESC LIMIT 20"

# 4. Fork a branch, mutate it, compare, and throw it away; O(1), zero-copy
fgdb branch city.fgdbdir create roadworks --from trunk
fgdb query city.fgdbdir --branch roadworks \
  "MATCH (:Intersection {id: 42})-[r:ROAD]->(:Intersection {id: 77}) DELETE r"

# 5. Serve it and connect a Neo4j driver over the Bolt-compat subset
fgdbd --data ./city.fgdbdir --listen 127.0.0.1:7687 --protocols fgp,bolt
```

## Configuration

`fgdbd` reads a TOML config; every value has a safe default and can be overridden per environment. The commit stream and retention tiers make most "tuning" a matter of *policy*, not knobs.

```toml
# fgdb.toml
[storage]
data_dir          = "./mydb.fgdbdir"
space_amp_trigger = 2.0          # compaction fires above this space amplification (§5.5)
repair_overhead   = 0.20         # RaptorQ repair symbols (heals torn/corrupt data)

[retention]
policy   = "KEEP ALL"            # "KEEP ALL" | "KEEP 90d" | "KEEP 100000 seqs" | "KEEP NONE"
anchor_every = { seqs = 100_000, bytes = "8GiB", time = "24h" }   # AnchorSnapshot cadence

[transactions]
default_isolation = "SERIALIZABLE"   # SNAPSHOT | READ_ONLY_HISTORICAL | SNAPSHOT_FOLLOWER | BRANCH_CAUSAL
result_determinism = "STRICT"        # STRICT (byte-identical, incl. order) | RELAXED (declared in certificate)
tie_break_policy   = "InsertionOrder"

[query]
semantic_profile   = "gql-2024-strict"   # pins null/duplicate/ordering/coercion semantics
certificate_mode   = "Compact"           # Compact | Budgeted | Full | Forensic
per_query_memory   = "4GiB"              # spills to temp ECS objects past budget, never OOM

[server]
listen     = "0.0.0.0:7687"
protocols  = ["fgp", "http2", "grpc", "ws", "bolt"]
tls_cert   = "/etc/fgdb/cert.pem"
tls_key    = "/etc/fgdb/key.pem"

[security]
at_rest_encryption = true         # Argon2id → KEK → per-DB DEK; XChaCha20-Poly1305, encrypt-then-code
require_capability = true          # macaroon-gated; caveats compile to planner predicates

[replication]
role   = "leader"                  # leader | follower
peers  = ["10.0.0.2:7688", "10.0.0.3:7688"]
```

## Performance

Numbers below are the provisional CI **gates** (§17 `EmpiricalGate`s, activated only under pinned benchmark manifests) on the reference machine (32-core/64-thread, 256 GB RAM, PCIe-4 NVMe at 7 GB/s, single node), chosen from measured SOTA anchors with leapfrog margins. Six standing laws bind every published figure: **no benchmark-only semantics** (durability/isolation/result-consumption match production), **distributions not averages** (p50/p95/p99/p99.9 and worst hot-key), **never hide compaction** (foreground latency during compaction/checkpoint/GC/index-build is part of the result), **memory is a first-class metric** (bytes/live-edge include versions, indexes, witnesses, and allocator slack), **adaptive numbers disclose their policy epoch**, and **no unpriced protocol weight** (every gate names its operation class in the plan's operation-cost registry and is derivable from it).

| Domain | Gate |
|---|---|
| Cold bulk load (CSV/Parquet-lite → sealed runs) | ≥ 40M edges/s sustained (≥ 60% of NVMe seq-write ceiling) |
| Transactional ingest (small txns, honest group-commit fsync) | ≥ 2M edge-inserts/s for the pinned batch/txn mix; latency includes final rebase/constraint work, payload D1, root D2, and configured encryption/FEC |
| Point reads (vertex by key, 1-hop existence; `SnapshotQuery` class) | ≥ 8M lookups/s across cores; p99 < 15 µs warm |
| Neighbor scans, sealed runs (decoded-cache path) | ≥ 500M edges/s per core; ≥ 10B edges/s node aggregate |
| 2-hop factorized count (10⁸-flat-row equivalent) | < 50 ms (must **not** materialize) |
| Triangle count (WCOJ over compressed runs) | within 2× of best static-CSR WCOJ systems; ≥ 20× any pointer-chasing GDBMS |
| LDBC SNB Interactive SF-100 | throughput ≥ 3× Neo4j, ≥ 1.5× best published embedded engine |
| LDBC Graphalytics (BFS/PR/WCC/CDLP/LCC/SSSP) | within 1.5× of dedicated static analytics engines, *on transactional storage* |
| Time-travel overhead (KEEP ALL, recent `AS OF`) | current-time OLTP degradation < 8%; `AS OF` in anchor window < 2.5× current-time cost |
| Vector: 10M × 768-d f32 HNSW | ≥ 20k QPS @ recall ≥ 0.95 (k=10); insert-to-searchable < 100 ms (the freshness gate) |
| Ripple view maintenance | ≥ 1M input-changes/s per circuit worker; subscription end-to-end p99 < 10 ms |
| Branch create / snapshot open | O(1), < 100 µs |
| Recovery (crash @ 1 TB) | < 30 s to first query (anchor-mapped, capsule-tail replay) |

Every gate has a bench binary, a committed baseline, a variance budget, and a flamegraph artifact on regression. **Complexity-witness regression locks** fail CI when an operator's observed op-count exceeds its declared bound; a regression is a build break, not a dashboard blip.

> **Target state.** None of the gates above is measured yet (checked 2026-09-24). One bench binary (`crates/fgdb-bench`) runs its shapes and labels every event `empirical_gate_activated=false`; no baseline, variance budget or flamegraph is tracked. The numbers in the table are commitments, not results.

## Determinism, verification & governance

- **Simulation-first.** The entire database (storage, transactions, compaction, Ripple, replication, server) runs under asupersync's lab runtime with virtual time, a fault-injecting virtual disk (torn writes, bit flips, ENOSPC, fsync lies), and DPOR schedule exploration. Every concurrency bug is a seed; failing runs auto-attach crashpacks with replay commands.
- **A reference oracle.** `fgdb-reference` is a deliberately simple, single-threaded, obviously-correct implementation of the full logical semantics, compiled for tests only. "What should this return" is a *program*, not a debate, and it exists before the first optimized line.
- **Continuous consistency oracles.** SI and SSI oracles reconstruct the dependency graph from traces and assert no committed dangerous structure (the database's own cycle detection verifies its own serialization graphs), alongside Elle-class history checking and obligation-leak detection.
- **Formal anchors (scoped, honest).** Lean proves MVCC visibility, block-level SSI safety, merge-ladder soundness, and the Z-set operator subset; TLA+/TLC models the two-fsync commit + recovery, compaction publish/retire, Raft-marker interaction, and branch fork/merge. Every load-bearing invariant carries a stable ID (`FG-INV-01 … FG-INV-20`) in a machine-readable registry that CI cross-checks for a live checker.
- **Plan certificates.** Every query result is an auditable artifact: plan hash, tie-break policy, snapshot seq, per-operator observed-vs-bound counts, re-plan events, and a BLAKE3 decision-path hash. `replay(certificate, seq, seed)` reproduces a result byte-for-byte. For agents and regulated pipelines, this is a feature no competitor ships.
- **Governance applied before expansion.** Macaroon caveats compile to mandatory planner predicates: a capability that can't see an edge type can't observe its existence via degree either. Absence-of-results witnesses are scoped to the authorized subgraph, so the serializability machinery can never become an oracle about data you can't see.

## Limitations

A few honest boundaries:

- **Horizontal sharding is designed-in, not yet activated.** Single-node excellence plus replication (durability, availability, read scale, multi-writer) is the shipping product. Strata's partition grid, per-partition dense ordinals, capsule-based movement, and `topology_epoch` fields are all present *so that sharding is an activation, not a rewrite*, but distributed FreeJoin and per-shard Raft groups are the final workstream. If you need a graph sharded across dozens of machines *today*, that milestone is still landing.
- **Multi-writer replication follows single-leader consensus.** Writer-anywhere with merge-ladder rebase gives skew-commutative workloads (agent swarms appending facts) near-linear write scaling; true conflicts behave exactly like local first-committer-wins. It is sequenced after the single-leader path.
- **The native language is GQL, not Gremlin.** Gremlin is imperative and optimizer-hostile; it is provided only as a possible later compatibility shim, if ever. openCypher is the pragmatic on-ramp; the Bolt-compat subset is an adoption wedge for read/query workloads, not the native path.
- **Property graph first; RDF is import/view, not the core model.** GQL, LDBC, and fnx are binary-graph worlds. N-ary facts are modeled by reification (an event vertex plus typed edges), which the planner already optimizes.
- **It targets documented conformance, not universal Cypher.** Where standards diverge, behavior is pinned by a versioned SemanticProfile and every divergence is a published matrix entry: folklore-free, but not "runs every Cypher snippet unchanged."

## FAQ

**Is this production-ready today?** The README describes the 1.0 target state (see the note at the top). Track the convergence gates in [§19 of the plan](./COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md) for exactly what is enforced at each milestone: G1 "The Engine Lives", G2 "One Version Universe", G3 "Verified & Networked", G4 "Leapfrog, Published".

**Why build every codec, index, and parser in-house instead of using great existing crates?** The closed dependency universe is the moat, not an albatross. The entire dependency surface, down to the RaptorQ decoder and the HNSW graph, is auditable, deterministic under the lab runtime, and owned. That is what makes FoundationDB-class deterministic testing *of the whole system* possible; you cannot seed-replay a bug that lives inside an opaque third-party thread pool.

**How can time travel be nearly free?** Because it isn't a separate feature. Retired MVCC versions don't get deleted; they cool into retention tiers, and those tiers *are* the durability layer. `AS OF s` resolves to the nearest anchor ≤ s plus a forward-apply of commit capsules the database already stored for durability. AeonG bolts temporal onto Memgraph; here it's a corollary of how MVCC works (current-time OLTP degradation gate: < 8%).

**Are the branches real, or copy-on-write snapshots with a fancy name?** Real. Because state is `{ anchor set + capsule chain }` and everything is content-addressed, a branch is a `BranchManifest` that structurally shares all sealed objects: O(1) creation, O(live-delta) memory, 10k+ concurrent. Merge replays the branch's intent log through the same semantic ladder that resolves write conflicts and replication rebases: one mechanism, three features. *Today* that is not built: `BranchManifest` is deliberately absent from `fgdb-chronicle`, so there is no fork, merge or branch write. What exists is the read half: `AT BRANCH name` on a read, against a pinned snapshot the host resolves (`Database::query_with_branch_resolver`).

**Is vector search actually transactional, or eventually-consistent like the plugins?** Transactional. The HNSW index uses the same delta→sealed→compaction lifecycle and MVCC visibility filtering as adjacency, so your ANN results respect your snapshot, `AS OF` applies, vectors are branch-scoped, and freshness is measured in commit-latency, not reindex-hours. *Today* results are snapshot-consistent and honour `AS OF`, but a search builds its HNSW index for that call, or reads a resident index whose generations are refreshed explicitly (`crates/fgdb/src/query_beacon.rs`). There is no durable, commit-maintained index and no branch scope yet.

**Can I embed it in my Rust or Python program?** Yes, that's a primary goal. The 1.0 library API is synchronous and blocking; the engine owns its runtime internally, so there's no async plumbing to thread through your code. Python gets ABI3 wheels and a zero-copy fnx / NumPy bridge. *Today* the Rust API is `async` and takes an asupersync capability context, so the caller drives the runtime, and there are no Python wheels yet (see [Installation](#installation)).

**Will it connect to my existing Neo4j tooling?** For read/query workloads, yes, via the Bolt-compat subset (enough of Bolt v5 + Neo4j type mapping for standard drivers and visualization tools). Divergences are documented; it is an adoption wedge, not the native surface.

**Why does every result come with a "certificate"?** So results are reproducible and auditable. For agent memory and regulated pipelines, "why did this query return this, and can I prove it again bit-for-bit" is the actual requirement, and certificates make a query result a replayable artifact instead of a transient event.

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

## License

The `frankengraphdb` source code is licensed under the **MIT License with an OpenAI/Anthropic Rider**, Copyright (c) 2026 Jeffrey Emanuel (see [`LICENSE`](./LICENSE)). The rider withholds all rights from OpenAI, Anthropic, their affiliates, and anyone acting on their behalf, including any use of the software or derivative works in a machine-learning dataset, training corpus, evaluation harness, or pipeline. In any conflict between the rider and the rest of the license, the rider controls.

## See also

- [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md`](./COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md), the master plan: the six bets, the foundation audit, the SOTA distillation (adopt/adapt/reject), every subsystem (Chronicle, Strata, Loom, Ripple, Beacon, Prism, Warden, Fabric, Aegis), the verification doctrine, the workstreams and convergence gates, the on-disk formats, the graph intent-log vocabulary, the GLA operator inventory, the invariant registry, and the operation-cost registry.
- [`AGENTS.md`](./AGENTS.md), conventions for human and AI agents working in this codebase, including the engineering doctrine and the verification ladder.
