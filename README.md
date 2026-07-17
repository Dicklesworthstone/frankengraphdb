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
curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/frankengraphdb/main/scripts/install.sh | bash
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
| Dependencies | **Closed universe.** `std` + the pinned nightly + three owned foundations. No serde, no tokio, no rocksdb, no arrow, no tantivy, no hnswlib. Ever. |

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

-- GraphRAG retrieval in one planner-fused operator (ANN + BM25 + graph expansion)
CALL hybrid.search(
  text  => 'computing pioneers',
  vector => $query_embedding,
  seeds  => [ (:Person {name: 'Ada'}) ],
  expand_pattern => '-[:KNOWS]->{1,2}',
  k => 20, fusion => RRF
) YIELD node, score RETURN node.name, score;

-- Every result is auditable: get its plan certificate
EXPLAIN (CERTIFICATE) MATCH (a:Person)-[:KNOWS]->(b) RETURN count(*);
```

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

## The `fgdb` CLI

> The CLI mirrors the server and embedded surfaces. Robot mode emits line-oriented, versioned NDJSON so an agent can pipe and validate the stream against a frozen contract (`fgdb robot schema`).

```bash
# Open a database and run a query (human output, or --json / --robot)
fgdb query mydb.fgdbdir "MATCH (p:Person) RETURN p.name LIMIT 10"
fgdb query mydb.fgdbdir --file traversal.gql --json

# Interactive shell (GQL, with EXPLAIN / EXPLAIN (ANALYZE, CERTIFICATE))
fgdb shell mydb.fgdbdir

# Bulk-load a graph straight into sealed runs (bypasses the delta tier)
fgdb load mydb.fgdbdir --edges edges.csv --vertices nodes.csv --format csv
fgdb load mydb.fgdbdir --input graph.graphml          # GraphML/GEXF/GML/Pajek/edgelist/JSON node-link

# Time travel, branches, and subscriptions from the shell or CLI
fgdb query mydb.fgdbdir "MATCH (n) FOR SYSTEM_TIME AS OF '2026-01-01T00:00Z' RETURN count(n)"
fgdb branch mydb.fgdbdir create experiment --from trunk
fgdb subscribe mydb.fgdbdir "SUBSCRIBE TO CHANGES ON :Person"      # streams NDJSON deltas

# Backup to a verifiable, self-contained ECS archive (with decode proofs)
fgdb backup mydb.fgdbdir -o snapshot.fgdb
fgdb restore snapshot.fgdb -o restored.fgdbdir           # decode-proof-verified before it opens

# Serve it
fgdbd --data ./mydb.fgdbdir --listen 0.0.0.0:7687 --protocols fgp,http2,grpc,ws,bolt

# Agent-first surfaces (versioned NDJSON contract, stable exit codes)
fgdb robot schema        # self-describing event/contract schema
fgdb robot health        # data present? arch features? thread/memory budget?

# Operations
fgdb doctor mydb.fgdbdir            # manifest/chain/decode-proof verification (FG-INV-08/09/10)
fgdb compact  mydb.fgdbdir
fgdb scrub    mydb.fgdbdir          # sample symbols, verify XXH3 + decode proofs, re-encode losses
fgdb analyze  mydb.fgdbdir          # refresh statistics segments
```

## Installation

**1. Install script (recommended).** Detects your platform, fetches the signed release binary, and installs `fgdb` + `fgdbd`:

```bash
curl -fsSL https://raw.githubusercontent.com/Dicklesworthstone/frankengraphdb/main/scripts/install.sh | bash
```

**2. From source** (requires the pinned nightly toolchain, which `rust-toolchain.toml` auto-selects):

```bash
git clone https://github.com/Dicklesworthstone/frankengraphdb
cd frankengraphdb
cargo build --release          # produces target/release/fgdb and target/release/fgdbd
```

**3. Embedded, as a Rust library:**

```toml
# Cargo.toml
[dependencies]
fgdb = { git = "https://github.com/Dicklesworthstone/frankengraphdb" }
```

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

**4. Python bindings** (ABI3 wheels, with a zero-friction `to_fnx()` / `from_fnx()` bridge and NumPy views over `Embedding` columns):

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

**Are the branches real, or copy-on-write snapshots with a fancy name?** Real. Because state is `{ anchor set + capsule chain }` and everything is content-addressed, a branch is a `BranchManifest` that structurally shares all sealed objects: O(1) creation, O(live-delta) memory, 10k+ concurrent. Merge replays the branch's intent log through the same semantic ladder that resolves write conflicts and replication rebases: one mechanism, three features.

**Is vector search actually transactional, or eventually-consistent like the plugins?** Transactional. The HNSW index uses the same delta→sealed→compaction lifecycle and MVCC visibility filtering as adjacency, so your ANN results respect your snapshot, `AS OF` applies, vectors are branch-scoped, and freshness is measured in commit-latency, not reindex-hours.

**Can I embed it in my Rust or Python program?** Yes, that's a primary goal. The library API is synchronous and blocking; the engine owns its runtime internally, so there's no async plumbing to thread through your code. Python gets ABI3 wheels and a zero-copy fnx / NumPy bridge.

**Will it connect to my existing Neo4j tooling?** For read/query workloads, yes, via the Bolt-compat subset (enough of Bolt v5 + Neo4j type mapping for standard drivers and visualization tools). Divergences are documented; it is an adoption wedge, not the native surface.

**Why does every result come with a "certificate"?** So results are reproducible and auditable. For agent memory and regulated pipelines, "why did this query return this, and can I prove it again bit-for-bit" is the actual requirement, and certificates make a query result a replayable artifact instead of a transient event.

## About Contributions

Please don't take this the wrong way, but I do not accept outside contributions for any of my projects. I simply don't have the mental bandwidth to review anything, and it's my name on the thing, so I'm responsible for any problems it causes; thus, the risk-reward is highly asymmetric from my perspective. I'd also have to worry about other "stakeholders," which seems unwise for tools I mostly make for myself for free. Feel free to submit issues, and even PRs if you want to illustrate a proposed fix, but know I won't merge them directly. Instead, I'll have Claude or Codex review submissions via `gh` and independently decide whether and how to address them. Bug reports in particular are welcome. Sorry if this offends, but I want to avoid wasted time and hurt feelings. I understand this isn't in sync with the prevailing open-source ethos that seeks community contributions, but it's the only way I can move at this velocity and keep my sanity.

## License

The `frankengraphdb` source code is licensed under the **MIT License with an OpenAI/Anthropic Rider**, Copyright (c) 2026 Jeffrey Emanuel (see [`LICENSE`](./LICENSE)). The rider withholds all rights from OpenAI, Anthropic, their affiliates, and anyone acting on their behalf, including any use of the software or derivative works in a machine-learning dataset, training corpus, evaluation harness, or pipeline. In any conflict between the rider and the rest of the license, the rider controls.

## See also

- [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md`](./COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md), the master plan: the six bets, the foundation audit, the SOTA distillation (adopt/adapt/reject), every subsystem (Chronicle, Strata, Loom, Ripple, Beacon, Prism, Warden, Fabric, Aegis), the verification doctrine, the workstreams and convergence gates, the on-disk formats, the graph intent-log vocabulary, the GLA operator inventory, the invariant registry, and the operation-cost registry.
- [`AGENTS.md`](./AGENTS.md), conventions for human and AI agents working in this codebase, including the engineering doctrine and the verification ladder.
