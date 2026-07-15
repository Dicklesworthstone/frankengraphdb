# COMPREHENSIVE PLAN FOR THE DESIGN OF FRANKENGRAPHDB

*A blank-slate, memory-safe, ultra-high-performance graph database in Rust, built entirely on the Franken/asupersync ecosystem, designed to leapfrog every commercial and open-source graph system in existence.*

---

## 0. The Thesis: What "Leapfrog" Actually Means Here

Every graph database on the market today is a compromise fossilized around a decision made years ago:

- **Neo4j** fossilized around pointer-chasing "index-free adjacency" on the JVM: superb ergonomics, mediocre analytics, painful memory behavior, and a query runtime that took 15 years to get vectorized.
- **TigerGraph** fossilized around MPP analytics with a proprietary language and an operational footprint that makes it a platform decision, not a library.
- **Kùzu** got the query processing story *right* (columnar CSR + vectorization + factorization + worst-case-optimal joins) and then the company folded in 2025, leaving a scattering of community forks and proof that the ideas work but the execution surface was incomplete: single-writer, no replication story, no temporal story, no deterministic testing story.
- **Memgraph / FalkorDB** are fast in-memory engines with thin durability and no principled answer to larger-than-memory, history, or verification.
- **JanusGraph / NebulaGraph** fossilized around generic KV underlays (Cassandra/RocksDB) and pay a permanent impedance-mismatch tax on every traversal.
- The **academic frontier** (Sortledton, Teseo, LiveGraph, GTX, Spruce, RadixGraph; factorized WCOJ processing; DBSP; AeonG) has solved, *in isolation*, essentially every hard subproblem — transactional adjacency at near-CSR speed, unified join processing, automatic incrementalization, cheap temporal storage — and **no shipping system has ever composed them.**

FrankenGraphDB's leapfrog is not one trick. It is the *composition* of six bets, each individually at or beyond the current frontier, made feasible only because the foundation libraries already exist:

| # | Bet | One-line statement | Why nobody else can do it |
|---|-----|--------------------|---------------------------|
| **B1** | **One Version Universe** | MVCC versions, time-travel history, replication stream, change subscriptions, and git-style database branches are *the same mechanism* — an append-only, content-addressed, RaptorQ-coded commit stream. | Requires ECS-style durability substrate (frankensqlite) + fountain codes (asupersync) designed in from byte zero. |
| **B2** | **Graph-Structured LSM ("Strata")** | Adjacency lives in three tiers — versioned delta blocks → sealed compressed CSR runs → archived anchors — giving transactional writes at millions of ops/sec *and* analytics scans at static-CSR speed on the same store. | Every existing system picks one point on the write/scan/space Pareto surface; Strata moves along it per-vertex, per-temperature. |
| **B3** | **Unified Factorized/WCO Execution ("Loom")** | One join operator family (Free-Join-style) subsumes binary hash joins, worst-case-optimal multiway joins, and factorized intermediates, running vectorized and morsel-parallel over Strata runs that *are already tries*. | Kùzu proved the pieces; Free Join proved the unification; nobody has built the unified engine over an adjacency store shaped for it. |
| **B4** | **Incremental Everything ("Ripple")** | A DBSP-style Z-set delta algebra is the single engine for recursive queries, materialized graph views, standing queries/subscriptions, and incremental analytics — fed directly by the commit stream, which is *already* a Z-set stream. | Requires B1 (commit stream = delta stream) and a from-scratch executor willing to make deltas the primitive. |
| **B5** | **Determinism as a Product Feature** | CGSE tie-break policies, complexity witnesses, and plan certificates make every query result *reproducible and auditable*; every adaptive decision anywhere in the system (plan choice, tier migration, victim selection, compaction pacing) emits a replayable **decision card** under a versioned policy epoch; the whole database runs under asupersync's lab runtime for DPOR-explored, seed-replayable, chaos-injected simulation testing — FoundationDB-class verification with formal (Lean/TLA+) anchors. | Requires franken_networkx's CGSE + asupersync's lab runtime + a codebase that threads `Cx` capability contexts through every effect from day one. |
| **B6** | **Agent-Native by Construction** | Branch-per-agent isolation with semantic intent-log merge, capability-scoped (macaroon) subgraph authorization, provenance as first-class edges, hybrid vector+text+graph retrieval in one planner, and deterministic replay of any agent's reads — purpose-built for the 2026 workload that is actually driving graph adoption: GraphRAG and multi-agent memory. | Requires B1 (branches), B4 (fresh incremental views), B5 (replayable reads), and asupersync's macaroon security layer. |

The 2026 market context makes the timing exact: GraphRAG and agent memory have become the first mainstream non-fraud use case pulling enterprises into graph databases; practitioners have converged on hybrid vector+graph architectures; and reviewers explicitly flag governance/access-control and freshness-of-derived-state as the unsolved problems of current systems. Meanwhile the strongest technical design in the space (Kùzu) is orphaned. There is a Kùzu-shaped hole in the market, and the correct move is not to fill it — it is to build the system Kùzu would have become in 2030, plus the durability, temporal, verification, and multi-writer stories it never had.

**Anti-goals of this document:** There is no "MVP", no "v1 simplification", no "phase where we use RocksDB temporarily." Section 19 sequences *integration*, not ambition: every workstream below is specified at full strength. Where something is genuinely staged (e.g., horizontal sharding), it is staged because it *depends on* other full-strength components, and its design constraints are baked into the architecture from the first commit so that no rewrite is ever required.

---

## 1. Constraints and Non-Negotiables

1. **The dependency universe is closed.** Allowed: `core`/`alloc`/`std`, the Rust nightly toolchain (both asupersync and franken_networkx already pin nightly), and the three foundation projects — `asupersync` (with whatever it vendors internally; its choices are its own), the `fnx-*` crates of `franken_networkx`, and *design-level* reuse of `frankensqlite` (we fork/adapt specific modules into `fgdb-*` crates rather than linking `fsqlite-*` wholesale, because graph objects are not SQLite pages — see §2.3). Everything else — compression codecs, sketches, ANN indexes, inverted indexes, radix trees, wire protocols, columnar readers — is built in-house (§18 is the complete inventory). No serde, no tokio, no rocksdb, no arrow, no tantivy, no hnswlib. Ever.
2. **Memory safety is structural.** Workspace-level `unsafe_code = "forbid"` with a frankensqlite-style *unsafe boundary ledger* for the handful of crates that need raw pointers (buffer arenas, SIMD kernels, mmap in the VFS). Every unsafe block gets a ledger row: path, invariant, evidence, no-claim boundary — the asupersync auditing pattern, inherited verbatim.
3. **`Cx` everywhere.** Every function that performs I/O, takes a lock, allocates from a shared arena, or can block accepts `&Cx` (asupersync's capability context). This is what makes B5 possible: swap the `Cx`, and the entire database runs under the lab runtime with virtual time, deterministic scheduling, and fault injection. It is also what makes cancellation-correct query timeouts, deadline propagation, and capability narrowing (read-only connections *cannot* express writes) structural rather than conventional.
4. **Deterministic by default.** Same database state + same query + same policy ⇒ byte-identical results, always, including result order (CGSE tie-break policies, §8.6). Nondeterminism (e.g., parallel aggregation over floats) is opt-in and declared in the plan certificate.
5. **Single artifact, three postures.** One codebase produces: (a) an embedded library (`fgdb`) usable like Kùzu/DuckDB from Rust and Python; (b) a server binary (`fgdbd`) speaking the native protocol, HTTP/2, gRPC, WebSocket, and a Bolt-compat subset; (c) a CLI (`fgdb-cli`). Larger-than-memory operation is first-class in all three.
6. **The commit stream is the source of truth.** There is no mutable primary file. The only mutable object in a database directory is the `RootManifest` (frankensqlite Native-mode discipline). Everything else is immutable, content-addressed, and erasure-coded.
7. **No JVM, no GC, no mmap-as-durability, no generic KV underlay, no eventual-consistency defaults.** These are the fossils we refuse to inherit.
8. **Narrowed capability contexts.** Subsystems receive purpose-typed wrappers over `Cx` (`QueryCx`, `TxnCx`, `CommitCx`, `MaintCx`, `ReplCx`) exposing only their legal effects as obligations (`pin_snapshot`, `reserve_wal`, `publish_segment`, ...). The sharpest instance: the merge ladder's intent-replay evaluator receives a `Cx` with **no clock, entropy, network, or filesystem capability** — deterministic rebase is guaranteed *by construction*, not by review.
9. **Prohibited shortcuts (constitutional).** No global-lock "interim" transaction model; no `HashMap<VId, Vec<EId>>` presented as storage; no snapshot isolation quietly labeled "ACID"; no parser-interprets-AST engine; no non-durable benchmark mode reported as a result; no serde-derived enum as a durable format; no detached background thread. Early code may implement a *subset* of each final abstraction — never a substitute for it.

---

## 2. Foundation Audit: Exactly What We Stand On

This section is the result of a full review of the three repositories. It maps *specific existing assets* to *specific FrankenGraphDB subsystems*, because the leapfrog claim is only credible if the reuse plan is concrete.

### 2.1 asupersync (~580 KLOC): the operating system

| Asset (module) | What it gives us | Used by |
|---|---|---|
| Region tree, obligations, `Outcome` lattice, three-lane scheduler (cancel/EDF/ready), fibers/tasks/actors | Structured concurrency: no orphan tasks; region close ⇒ quiescence; bounded-cleanup cancellation protocol; work-stealing parallelism with priority lanes | Every background service (compactor, checkpointer, GC, replicator) and every parallel query (§8.8): a query *is* a region; cancelling it drains morsels deterministically and frees its region heap |
| `Cx` capability contexts + capability narrowing | Deadline/cancellation/authority threading | Whole codebase (constraint #3) |
| Lab runtime: virtual time, seeded deterministic scheduling, DPOR/Mazurkiewicz schedule exploration, chaos injection, futurelock detection, trace capture/replay, crashpacks | The verification substrate for B5 | `fgdb-sim` (§15) |
| Formal layer: Lean-checked invariants, TLA+ export discipline, proof-lane manifests | The house style for formal claims | Commit protocol + visibility invariants (§15.5) |
| `src/raptorq/`: RFC 6330 systematic RaptorQ, GF(256) SIMD kernels, decode proofs, per-symbol authentication, deterministic decode planner | Fountain coding for WAL/objects/replication | Chronicle (§5), Aegis (§14) — we do **not** reimplement RaptorQ; frankensqlite's "RaptorQ everywhere" doctrine is executed with asupersync's implementation |
| ATP transport (`src/net/atp/`), bonded multi-donor pulls, transport router, multipath aggregator with dedup/reorder | Symbol-oriented bulk transfer over lossy paths, pulling one object from N donors | Replica seeding, backup/restore, anchor shipping (§14.4) |
| Networking: TCP, UDP, QUIC (native), HTTP/1.1, HTTP/2 (HPACK, flow control), HTTP/3, WebSocket, TLS (rustls-backed), DNS, gRPC | The entire server surface | Fabric (§13) — FrankenGraphDB writes *zero* protocol plumbing below the graph wire format |
| Channels (two-phase reserve/commit MPSC, oneshot, broadcast, watch, session), sync primitives, combinators (quorum, hedge, circuit-breaker, bulkhead, rate-limit, map_reduce, pipeline) | Cancel-correct internal plumbing; the write-coordinator mailbox; subscription fan-out; admission control | Txn engine (§7.6), Ripple fan-out (§9.5), server QoS (§13.6) |
| Spork/OTP: supervision topologies, gen_server, monitors, name-lease registry | Service lifecycle for the ~12 long-lived database actors | §16.1 process tree |
| `remote.rs`: leases, idempotency store, session-typed protocols, sagas, logical clocks; distributed consistent hashing, snapshots, vector clocks | Consensus/replication building blocks | Aegis (§14) |
| Security: macaroons, authenticated types; per-symbol auth on RaptorQ planes | Capability tokens with caveats | Warden (§12) |
| fs + VFS + io_uring reactor; region heaps with generational handles; epoch reclamation; bytes/codec (length-delimited framing); observability (metrics, OTel export, spectral wait-graph health) | Storage I/O, query-scoped memory, EBR for lock-free readers, wire framing, ops | Buffer manager (§6.7), executor memory (§8.8), everything |

**The deep synergy:** asupersync is not "an async runtime we happen to use." Its obligations model maps exactly onto database invariants (a `CommitCapsule` publication is a two-phase obligation; a checkpoint lease is a lease obligation; an un-acked subscription delivery is a send permit). Its lab runtime turns MVCC interleaving bugs into seed-replayable test failures. Its region heaps make query memory accounting and cancellation-time reclamation structural. FrankenGraphDB is, in a precise sense, *a database written in the asupersync programming model*, the way FoundationDB is a database written in Flow.

### 2.2 franken_networkx (~247 KLOC, 12 crates): the algorithm brain and the semantics doctrine

| Asset | What it gives us | Used by |
|---|---|---|
| `fnx-algorithms`: 550+ functions across 25+ families (shortest paths, centrality, communities, flow, matching, isomorphism/VF2++, planarity, spectral, k-core/k-truss, link prediction, DAG ops, …) | An in-database analytics catalog larger than Neo4j GDS, day one | Prism (§11): `CALL fnx.pagerank(...)`, `CALL fnx.louvain(...)` |
| `GraphView` trait (in `fnx-algorithms`): string access + **integer index rows** (`neighbors_indices(&self, idx) -> Option<&[usize]>`, `in_neighbors_indices`) | The zero-copy bridge: Strata snapshot CSR runs implement `GraphView` directly; algorithms traverse database memory with no materialization | §11.2 — plus upstream workstream W7 to widen the `GraphView`-generic surface across fnx families |
| CGSE: 13 `TieBreakPolicy` variants, `ComplexityWitness { n, m, dominant_term, observed_count, policy, seed, decision_path_blake3 }`, `WitnessLedger`, Strict/Hardened modes, per-algorithm policy registry | The determinism doctrine (B5) generalized from algorithms to *queries*: FrankenGraphDB adopts tie-break policies as part of the query contract and emits plan certificates containing witnesses | §8.6, §15.2 |
| `fnx-readwrite`: fuzz-hardened native parsers/writers for edgelist, adjlist, GraphML, GML, JSON node-link, Pajek, GEXF (+ graph6/sparse6 composition) | Import/export for the whole legacy graph ecosystem | Formats layer (§13.7); bulk loader feeds Strata's sealed-run builder directly |
| `fnx-generators` (SBM, Barabási–Albert, Watts–Strogatz, LFR, lattices, social datasets) | Deterministic synthetic workloads for benchmarks and sim tests | §15, §17 |
| `fnx-durability` (RaptorQ sidecars, integrity scrub, decode proofs) | Precedent + shared artifact conventions for scrubbing | Chronicle scrubber (§5.7) |
| `fnx-classes` (IndexMap adjacency, insertion-order semantics), `fnx-views` (borrowed/cached snapshot views) | The *compatibility semantics reference* (NetworkX-parity ordering) and small-graph materialization path | §11.4 (when an algorithm needs a mutable working copy) |

### 2.3 frankensqlite (~1.28 MLOC): the architectural donor

frankensqlite is inspiration-by-dissection: we adopt its *designs*, re-instantiate them for graph objects, and where a module is genuinely object-agnostic we fork it into an `fgdb-*` crate (attribution preserved) rather than depending on `fsqlite-*` and dragging SQLite's page/B-tree world along.

| frankensqlite design | Verbatim lesson | FrankenGraphDB instantiation |
|---|---|---|
| **Page-granularity MVCC**: version chains in a bump arena, `Snapshot { high: CommitSeq }`, O(1) visibility via single integer compare, EBR reclamation, INV-1/2/3 invariants | Choose the versioning granule to match the storage granule; make visibility a compare, not a bitmap | **Block-granularity MVCC** on Strata adjacency blocks and property column chunks (§7.1) — the graph analogue of pages, with the same three invariants restated |
| **SSI (page-Cahill/Fekete) + first-committer-wins + eager locking ⇒ deadlock freedom by construction** | Conservative dangerous-structure detection at storage granularity is cheap and sound | Graph-SSI at block granularity + *label-epoch predicate locks* for phantom protection on pattern scans (§7.3) |
| **Intent logs + deterministic rebase merge ladder** (semantic replay → structured patch → abort) | Same-granule conflicts are often semantically commutative; replaying intents beats aborting | The ladder gets *stronger* in graph land: `AddEdge`/`RemoveEdge`/`SetProp` on the same block commute far more often than B-tree cell edits (§7.4). This is also the branch-merge engine (§5.6) |
| **Native mode / ECS**: content-addressed `ObjectId = Trunc128(BLAKE3(domain ‖ header ‖ payload_hash))`, `SymbolRecord` envelope (Magic/ObjectId/OTI/ESI/data/XXH3/auth-tag), `RootManifest` as the *only* mutable file, `CommitCapsule` + `CommitMarker` chain, two-fsync commit protocol, mark-and-compact with segment leases | The durability substrate | Chronicle (§5) adopts this wholesale, graph-flavored object kinds (§Appendix A) |
| **WriteCoordinator**: bulk durability off the critical path; a single sequencer serializes only validation + seq allocation + ~96-byte marker append; group commit amortizes two fsyncs | The write path shape | §7.6, with asupersync two-phase channel obligations making the coordinator cancel-correct |
| **ARC buffer pool, MVCC-aware; cache-line-padded sharded lock/siread tables; aligned direct-I/O buffers; prefetch on descent** | Mechanical sympathy checklist | Buffer manager (§6.7) |
| **Time travel** (`FOR SYSTEM_TIME AS OF`, snapshot ring today, VersionStore design) + **anchor+delta** thinking | Temporal support should ride the MVCC machinery, not duplicate it | Chronicle retention tiers (§5.4) — and we go past frankensqlite by making history *unbounded and queryable* (AeonG-class), not a 256-entry ring |
| **Encryption**: Argon2id KEK → per-DB DEK, XChaCha20-Poly1305 per-object, encrypt-then-code | Correct layering with FEC | Warden at-rest encryption (§12.4), implemented in `fgdb-crypto` following this exact design |
| **Verification posture**: conformance corpora, differential harnesses vs. reference engines, feature-universe ledgers, exit-criteria contracts, metamorphic tests | The QA house style | §15 throughout |

### 2.4 What the foundations do *not* provide (and we therefore build)

Columnar graph storage, adjacency compression codecs, a query language front end, a cost-based optimizer, a vectorized/factorized executor, WCOJ machinery, a Z-set incremental engine, secondary/FTS/vector indexes, graph statistics sketches, the wire protocol's graph payloads, and the replication state machine. That is the actual FrankenGraphDB codebase, estimated 250–400 KLOC of new Rust — large, but *smaller than frankensqlite*, on a foundation that already absorbed the hardest systems problems (scheduling, cancellation, networking, fountain coding, deterministic testing). The complete build-it-ourselves inventory is §18.

---

## 3. SOTA Distillation: The Field, and Where It Falls Short

A condensed map of the review (systems + literature), organized as *adopt / adapt / reject* decisions. Full bibliography in Appendix E.

### 3.1 Dynamic graph storage (the write/scan/space trilemma)

| Source | Core idea | Verdict for FrankenGraphDB |
|---|---|---|
| **CSR** (static baseline) | Contiguous offset+neighbor arrays; unbeatable scans; zero update capability | **Adopt** as the *sealed run* format, per-label-pair segmented, Elias-Fano/delta-compressed (§6.3) |
| **Sortledton** (VLDB '22) | Sorted neighborhoods in cache-friendly blocks; unrolled skip lists for hub vertices; transactional; ~1.22× CSR analytics at ~2.1× CSR memory | **Adapt** as the *delta tier* block layout (§6.2): sorted 256-edge blocks, per-block version pointers; hub vertices escalate to skip-list-of-blocks |
| **Teseo** (VLDB '21) | PMA + ART fat tree; great point latency, rebalance latency spikes, high memory on skewed graphs | **Reject** PMA as primary (rebalance storms conflict with p99 goals); **adopt** its lesson that the vertex table must not be a straitjacket — our vertex directory is an index, not the store |
| **LiveGraph** (VLDB '20) | Purely sequential per-vertex logs; fast ingest; scans degrade, deletes awkward | **Adapt**: the *hot-vertex overflow log* (§6.2.4) borrows the append-only trick for write bursts, but logs are bounded and compacted into sorted blocks |
| **Aspen** (PLDI '19) | Purely functional compressed trees; O(1) snapshots; single writer; 3–10× CSR memory | **Adopt the spirit** (immutable structural sharing ⇒ free snapshots) at *run granularity* instead of tree-node granularity — same benefit, a fraction of the pointer overhead |
| **GTX / Spruce / RapidStore / RadixGraph** ('24–'26) | Per-vertex delta chains for concurrency; ART-indexed buffers; decoupled read/write paths; latch-free log append; space discipline | **Adapt**: delta-chain-per-block concurrency (GTX lesson) + latch-free MPSC block append (RadixGraph lesson); the 2025 "Revisiting DGS" study's headline — 4–9× CSR space overhead across the field — is precisely the disease Strata's tiering cures (hot minority pays delta overhead; cold majority sits *below* CSR via compression) |
| **AeonG** (VLDB '24) | Current/historical hybrid stores; anchor+delta history; anchor-based version retrieval; ≈5.7× storage reduction, ≈2.6× temporal latency reduction, <10% OLTP overhead | **Adopt and unify**: our MVCC retirement path *is* the historical store; anchors = periodic sealed snapshots in ECS; deltas = the commit stream we already durably have (§5.4). AeonG bolts temporal onto Memgraph; we make it a corollary of durability |

### 3.2 Query processing

| Source | Core idea | Verdict |
|---|---|---|
| **Kùzu** (CIDR '23) + Graphflow lineage ("Columnar storage and list-based processing", A+ indexes) | Vectorized batches; **factorized intermediates** (compressed Cartesian-product form defeats many-to-many blowup); ASP-Join; WCOJ integrated into cost-based plans; dense per-label IDs into CSR join indexes | **Adopt** wholesale as the executor baseline (§8.7–8.8); dense per-label vertex IDs (§4.5); A+-style configurable secondary adjacency views become Beacon adjacency indexes (§10.2) |
| **WCOJ theory** (AGM bound; NPRR/Generic Join; Leapfrog Triejoin) + **EmptyHeaded**, **ADOPT** (adaptive attribute orders), **HoneyComb** (parallel WCOJ '25) | Cyclic patterns need multiway intersection to hit the AGM bound; attribute order matters; parallelization is subtle | **Adopt** via Free Join (below); attribute-order selection is cost-based with runtime adaptivity (§8.5); intersections run on Elias-Fano runs with SIMD galloping (§8.7) |
| **Free Join** (SIGMOD '23) | One algorithm continuum unifying binary hash joins and Generic Join; lazy trie (COLT) construction; up to ~15× over binary plans on cyclic queries | **Adopt** as *the* join operator: `FreeJoin` over "vertical runs" — Strata's sealed runs are natively the (src → type → dst) tries COLT would build lazily, so the trie tier is often free (§8.7). This is the single highest-leverage architectural fit in the whole design |
| **Factorized databases** (Olteanu et al.), FAQ | Aggregations/projections push through factorized representations at optimal complexity | **Adopt**: factorization is a *type* in the algebra, not an executor trick (§8.4); results can stay factorized over the wire (§13.3) |
| **Morsel-driven parallelism** (HyPer), push-based vectorized engines, **Umbra** (variable-size pages, pointer swizzling), **LeanStore** (optimistic lock coupling) | The modern engine checklist | **Adopt**: morsels = asupersync tasks in a query region; buffer manager per §6.7 |
| **GQL ISO/IEC 39075:2024** (610 pp., published 2024-04-12, revision underway), **SQL/PGQ 9075-16:2023**, openCypher | The standard exists now; quantified path patterns are the hard part; Spanner Graph / Fabric already conform | **Adopt** GQL as the native language with an openCypher compatibility surface (§8.1); we target *documented conformance tables* like Spanner's from the start |
| **Converged relational-graph optimization** (SPJM work) | Pattern matching and relational ops must share one optimizer | **Adopt**: one algebra, one cost model, no "graph engine bolted to SQL engine" seam (§8.3) |

### 3.3 Incremental computation, temporal, vector

| Source | Core idea | Verdict |
|---|---|---|
| **DBSP** (VLDB '23, Lean-formalized; Feldera) / **Differential Dataflow** / streaming-graph query work | Z-sets + linear stream time + the chain rule ⇒ *mechanically* incrementalize rich languages including stratified recursion; deletions cost the same as insertions | **Adopt DBSP's model** (linear time suffices — our commit stream is totally ordered by `CommitSeq`); Ripple (§9) is a from-scratch Z-set circuit engine specialized for graph operators; DD's lattice generality is rejected as unneeded complexity single-stream |
| **Incremental analytics** (delta-PageRank, dynamic connectivity, Graphsurge-style multi-view sharing) | Many analytics maintain under deltas far cheaper than recompute | **Adapt**: Ripple hosts per-algorithm incremental maintainers with recompute fallback + staleness contracts (§9.6) |
| **HNSW** (+ the '25–'26 disaggregated/hybrid literature), IVF/PQ | Graph-based ANN is the default; freshness under churn is the open sore | **Adopt** HNSW built in-house *with the Strata pattern applied to the index itself* (delta layer + sealed layers + MVCC visibility filtering), so vector search is transactional and time-travelable — which no mainstream system offers (§10.4) |
| **BM25 / FTS** (fsqlite-ext-fts5 precedent in-family) | Standard inverted index + BM25 | **Adopt**, segment-based with the same seal/compact lifecycle (§10.3) |
| **2026 market synthesis** (GraphRAG/agent-memory adoption; hybrid convergence; flagged gaps: governance/access control, derived-state freshness, incremental reindexing) | What buyers actually can't get today | These gaps are B4 (freshness via Ripple), B6 + Warden (capability governance), §10.4 (fresh transactional ANN) — the leapfrog aims where the market says it hurts |

### 3.4 What we deliberately reject

Generic KV underlays (JanusGraph/Nebula tax); Gremlin as the primary language (imperative, optimizer-hostile — provided only as a later compat shim if ever); RDF-first modeling (property graph first; RDF import/view later); JVM anything; mmap-as-the-durability-story; PMA global rebalancing; eventual consistency as a default; external crates (constraint #1).

Four seductive alternatives were evaluated in depth and rejected *with reasons recorded*, because each fails a composition test rather than a taste test: (a) **native hyperedges / n-ary incidence storage** — GQL, LDBC, and fnx are binary-graph worlds; n-ary facts are modeled by reification (an event vertex plus typed edges), which the planner already optimizes, at zero cost to the binary hot path; (b) **a per-fragment adjacency-representation zoo** (inline/vector/tree/log/bitmap/matrix/... with an online conversion governor) — the 2025 dynamic-graph-storage study shows representation proliferation is exactly where the field's 4–9× space overhead and test-matrix explosions come from; Strata deliberately fixes a *bounded* representation set (inline → blocks → runs → anchors) and gets equivalent coverage from tiering plus derived Beacon projections (§10.2); (c) **a full GraphBLAS engine as a separate executor** — sealed runs already *are* CSR, so Loom/Prism get masked semiring SpMV/SpMSpV kernels (§8.7, §11.4) at a fraction of the surface area; (d) **plan racing as the default execution strategy** — morsel-boundary re-optimization is cheaper than running losers; racing survives only as uncertainty-gated hedging (§8.5). Likewise, Weaver-style refinable timestamps for distributed ordering were studied and set aside: they trade away the bit-identical deterministic-replica property that Aegis's marker-ordered design buys (§14.1), and that property is load-bearing for verification.

---

## 4. Data Model

### 4.1 The FrankenGraph model

A **bitemporal, schema-gradient, multi-label property multigraph**:

- **Vertices** carry a *label set* (not a single label), properties, and identity.
- **Edges** are typed (exactly one edge type), directed (undirected exposed as a view/semantics flag per type), may be parallel (distinguished by an edge key), and carry properties.
- **Properties** are typed values: `Null, Bool, Int64, Float64, Decimal128, String, Bytes, Date, Time, Timestamp(tz), Duration, Point(2D/3D geo), Uuid, List<T>, Map<String,T>, Struct, Embedding{dim, dtype: f32|f16|i8}`. The scalar universe is a strict superset of fnx's `CgseValue` (lossless bridge to Prism) and of GQL's type system.
- **System time (bitemporal axis 1)** is universal and automatic: every vertex/edge/property version carries `[created_seq, retired_seq)` in `CommitSeq` space. This is not a feature flag; it is how MVCC works (§5.4).
- **Valid time (bitemporal axis 2)** is opt-in per element type: schema can declare `VALID_TIME` columns and the planner gets temporal-join operators, declared interval non-overlap constraints, and temporal path semantics (`DURING`-constrained patterns; a time-monotone path mode where each hop's validity must overlap-and-advance) over them.
- **Provenance** is first-class: any element can carry a `provenance` system property (source id, extraction confidence, agent/session id), and the `DERIVED_FROM` system edge type is reserved — the GraphRAG lineage story is modeled, not improvised.

### 4.2 Schema as a gradient, not a gate

Three enforcement postures, per label/edge type, changeable online:

1. **Open** (NetworkX/Neo4j-style): any property, any value; types inferred and recorded as statistics.
2. **Shaped**: declared properties are typed and validated; extra properties allowed into an overflow map column.
3. **Strict**: declared properties only; constraints active (existence, uniqueness/key, range/regex/check, edge endpoint label constraints, cardinality constraints like `AT MOST 1 :MOTHER edge out`).

Schema objects (labels, edge types, property defs, constraints, indexes) are versioned in the commit stream like data (schema epoch in every snapshot, as frankensqlite does), so **schema changes are transactional, time-travelable, and branchable**. Migration between postures — and any typed-column promotion of a hot overflow property — is an online shadow-build job reusing the seal/publish protocol (§6.3): build against a pinned snapshot, catch up deltas, validate, atomically publish the new schema epoch; old snapshots keep their epoch's interpretation. The **schema epoch is part of snapshot identity** (§5.3) and of every plan-cache key, so prepared plans bind to an epoch-compatibility range and invalidate precisely.

### 4.3 Graphs, branches, and the catalog

A **database** contains named **graphs** (GQL requires multi-graph catalogs). Each graph has a **trunk** branch plus arbitrary user branches (§5.6). Queries address `graph@branch` or `graph@branch AS OF <time|seq>`; the default is `trunk@now`. Cross-graph queries are permitted within a transaction (single commit stream per database ⇒ cross-graph atomicity is free).

### 4.4 Reserved system surface

`_id`, `_labels`, `_type`, `_key`, `_created_seq`, `_retired_seq`, `_valid_from`, `_valid_to`, `_provenance`, `_embedding` (conventional default vector slot), `_score` (query-time). System edge types: `DERIVED_FROM`, `SAME_AS` (entity-resolution assertions with confidence — the agent-memory dedup primitive that engines like current merge/upsert APIs only fake).

### 4.5 Identity

- **VId (vertex identity)**: `u64` = `{ label_class: 12 bits | dense_ordinal: 44 bits | generation: 8 bits }`. Dense per-label-class ordinals give Kùzu-style O(1) CSR addressing and compact frontier bitmaps; the generation byte makes dangling references detectable after vertex recycling (asupersync region-heap trick, applied to graph identity).
- **EId (edge identity)**: `(src VId, edge_type: u16, dst VId, key: u32)` logically; physically edges are *positions in runs*, and a stable `u64` edge surrogate exists only when the user or an index demands one (edge property columns are addressed by run-position, not surrogate — this is where Kùzu-style columnar edge storage gets its speed).
- **External keys**: any `Shaped/Strict` label may declare a key; a per-label dictionary (ART-based, §18) maps key → VId. Bulk import does dictionary-building in the sort pipeline, not row-at-a-time.
- **Distribution forward-compatibility**: VId ordinal spaces are allocated per partition range, and every snapshot coordinate, capsule header, and manifest carries a reserved `topology_epoch` field (single-node: constant 0). When sharding activates (§14.5), ownership maps version through topology epochs with zero format migration — these ID and format decisions are made now precisely so the distributed system is an activation, not a retrofit.

---

## 5. Chronicle: The Commit Stream, Durability, Time, and Branches (Bet B1)

Chronicle is the ECS-native durability substrate — frankensqlite's Native mode, born as the *only* mode, for graph objects.

### 5.1 Objects

Every durable thing is an immutable, content-addressed **ECS object**: `ObjectId = Trunc128(BLAKE3("fgdb:ecs:v1" ‖ canonical_header ‖ payload_hash))`, stored as RaptorQ `SymbolRecord`s (asupersync encoder; envelope per Appendix A) with configurable repair overhead (default 20%), XXH3 integrity words, and optional per-symbol auth tags. Object kinds: `CommitCapsule`, `CommitMarker`, `AdjRunSegment`, `PropChunkSegment`, `VertexDirectorySegment`, `DeltaCheckpoint`, `IndexSegment` (btree/fts/vector/path), `StatsSegment`, `SchemaSnapshot`, `AnchorSnapshot`, `BranchManifest`, `RootManifest-delta`. The single mutable file per database is `manifest.root`.

### 5.2 Commit protocol

The frankensqlite Native two-fsync protocol, verbatim in shape (§2.3), with graph payloads:

1. Writers (concurrent): finalize write set → run Graph-SSI validation (§7.3) → build `CommitCapsule` deterministically = { `snapshot_basis`, graph **intent log** (Appendix B), block deltas, read/write-set digests, SSI witnesses } → RaptorQ-encode → persist capsule symbols via bulk I/O (io_uring path) **off the critical section** → submit a tiny publish request (capsule ObjectId + write summary + witnesses) to the WriteCoordinator over a two-phase MPSC (send permit = obligation: a cancelled writer can *never* half-publish).
2. WriteCoordinator (single actor, group-committing): FCW check on write summaries → SSI re-validation for dangerous structures formed since local validation → merge ladder on conflicts (§7.4) → allocate gap-free `CommitSeq` → **FSYNC₁** (capsule + proof durable) → append ~100-byte `CommitMarker` → **FSYNC₂** → publish `CommitSeq` with Release ordering → respond.

Group commit batches both fsyncs across the queue; target marker-append critical section < 5 µs (§17). Recovery = read marker chain from last checkpointed position, decode capsules (RaptorQ heals torn/corrupt symbols — **no double-write journaling anywhere**), replay intent logs into delta tier. Markers without decodable capsules are, by construction (FSYNC₁), impossible short of media loss exceeding repair overhead; the scrubber (§5.7) bounds that exposure.

### 5.3 Snapshots

A read snapshot is `{ commit_seq_high, schema_epoch, topology_epoch, branch, manifest_pin, retention_lease }` — visibility is one integer compare (frankensqlite invariant), plus a pinned set of sealed-run generations (epoch-protected, so compaction never yanks memory from under a reader). Snapshot acquisition is wait-free. Long-lived snapshots (analytics, backups) hold an explicit **retention lease** rather than an invisible pin: version-GC pressure is attributable per lease, the pressure governor may *request* cancellation of a lease with evidence, and nothing may ever silently slide a snapshot forward.

### 5.4 One Version Universe: retention tiers = temporal database

Retired versions don't die; they *cool*:

- **Tier 0 (live delta)**: current MVCC version chains in the delta tier, reclaimed past the GC horizon exactly as in frankensqlite — *except* reclamation means **demotion**, not deletion.
- **Tier 1 (warm history)**: demoted deltas are batched into `DeltaCheckpoint` objects (columnar, sorted by `(element, seq)`), directly queryable for recent `AS OF`/`BETWEEN` predicates.
- **Tier 2 (anchored history)**: periodically (policy: every N seqs / M bytes / T time), the compactor seals an `AnchorSnapshot` (full compressed state of a partition) — AeonG's anchor+delta, where the deltas are commit capsules we *already stored for durability*. Anchor-based retrieval: resolve `AS OF s` = nearest anchor ≤ s + forward-apply capsules (RaptorQ-delta-encoded against the anchor, the frankensqlite version-chain compression trick).
- **Retention policy** per graph: `KEEP ALL` (bitemporal ledger), `KEEP <duration|seqs>`, or `KEEP NONE` (pure OLTP; tiers 1–2 disabled, versions truly reclaimed). Time-travel queries under `KEEP ALL` have the AeonG cost profile (<10% overhead on current-time OLTP; bounded anchor-scan for historical), and unlike AeonG they need no separate storage engine — the tiers *are* the durability layer.

Consequences that fall out for free: point-in-time restore (pin a manifest at seq), audit ("who changed this edge and when" = scan its version chain), `MATCH ... FOR SYSTEM_TIME BETWEEN a AND b` temporal patterns (§8.2), and the replication/subscription feeds (§9, §14) — all one mechanism. That is Bet B1.

### 5.5 Compaction

Mark-and-compact per frankensqlite's four-phase design (mark from RootManifest + marker stream; copy live symbols to `.compacting` segments; two-phase publish; lease-guarded retire), run as a supervised Spork actor with pacing bound to a p99-latency budget (compaction I/O flows through a rate-limit combinator; the spectral-health monitor watching the scheduler wait graph is wired to shed compaction first). Space-amp trigger default 2.0×.

### 5.6 Branches: git for graphs

Because state = { anchor set + capsule chain } and everything is content-addressed, a **branch** is just a `BranchManifest`: `{ parent_branch, fork_seq, own marker chain }`. Creation is O(1) and zero-copy (structural sharing of all sealed objects). Reads on a branch overlay its chain on the parent's state at `fork_seq`. **Merge** replays the branch's accumulated intent log onto the target via the semantic merge ladder (§7.4): commuting intents merge cleanly; true conflicts surface as a *conflict report* (a graph of conflicting intents — inspectable, queryable, resolvable programmatically). Use cases this unlocks, which no shipping graph database has as a native primitive: what-if analysis (fork, mutate, run analytics, discard), agent-swarm isolation (branch per agent, merge on approval — Bet B6), blue/green schema migrations, and reproducible ML feature snapshots. Branch count target: 10k+ concurrent branches with O(live-delta) memory each.

### 5.7 Scrubbing and self-healing

A background scrubber (fnx-durability's pattern, generalized): continuously samples symbol records, verifies XXH3 + decode proofs, re-encodes lost repair symbols, and escalates unrecoverable objects (below K symbols) to the replication layer for donor repair (§14.4). Bit-rot is a *maintenance event*, not an outage. Every scrub emits evidence records into the witness ledger.


---

## 6. Strata: The Storage Engine (Bet B2)

Strata resolves the write/scan/space trilemma by refusing to pick one point on it. Adjacency for each `(vertex, direction, edge_type)` triple exists across three tiers, migrating by temperature.

### 6.1 Topology of the store

```
                         VERTEX DIRECTORY (per label class)
        VId ordinal → { label set, prop row locator, adjacency descriptor per (dir, type) }
        packed array + generation bytes; O(1) by ordinal; ART dictionary for external keys
                                        │
        ┌───────────────────────────────┼─────────────────────────────────────┐
        ▼                               ▼                                     ▼
  TIER D: DELTA (mutable, versioned)  TIER R: SEALED RUNS (immutable)   TIER A: ANCHORS (ECS, cold)
  sorted 256-edge blocks              per-(label,type,label) CSR runs   full compressed partition
  per-block MVCC version chains       Elias–Fano offsets +              snapshots + capsule deltas
  hub vertices: skip-list of blocks   delta-varint neighbor gaps        (time travel / restore /
  hot-burst overflow log (bounded)    run-position-addressed edge       replica seeding)
  latch-free append, GTX-style        property columns; zone maps;
  delta chains under contention       optional per-run bitmap ("holes")
        │            ▲                          ▲
        └─ seal ─────┘── compact (k-way merge, generational) ──┘
```

- **Tier I (inline micro-adjacency)**: below the diagram's three tiers sits a zeroth one — the vertex directory row itself (§6.5) inlines up to 8 `(dst_ordinal, key)` incidences per (dir, type) descriptor before any block exists. In power-law graphs the *majority of vertices* live their whole lives here: zero pointer chase, zero block overhead, copy-on-write versioned with the directory segment. Overflow promotes to a Tier-D block; demotion back is a compaction decision. This is where Strata undercuts even raw CSR on memory for the long tail.
- **A logical adjacency list = merge(inline slot, delta blocks, [newest…oldest] sealed runs)**, exactly an LSM, except runs are *graph-shaped* (CSR segments sorted by `(src_ordinal, dst_ordinal, key)`) rather than KV SSTables. Deletions are tombstones in Tier D and hole-bitmaps on runs until compaction. Most vertices, most of the time, have **zero** Tier-D presence and read as pure CSR — this is how Strata escapes the field-wide 4–9× space overhead of dynamic graph structures: only the write-hot working set pays delta costs; sealed data sits *below* raw CSR size via Elias-Fano + gap compression (typical 2.5–5 bits/edge on real graphs vs 32–64 bits raw).
- **Direction**: forward runs always; reverse runs maintained by default per edge type (droppable per type for append-only fact streams). Reverse is just another run family — no special casing.
- **Partitioning**: runs are segmented by src-ordinal ranges (~256K vertices per segment) — the unit of compaction, buffer management, statistics, checksums, parallel scans, and (later) sharding. Partition boundaries respect label classes so analytics can address contiguous ordinal spaces.

### 6.2 Tier D internals

1. **Sorted edge blocks** (Sortledton lesson): 4 KB blocks holding ≤256 `(dst_ordinal, key, prop_row_ref, flags)` entries, sorted; binary search inside, SIMD lower-bound. A vertex's Tier-D presence is 0 blocks (common), 1 block, or a two-level skip list of blocks (hubs).
2. **Per-block MVCC**: each block heads a version chain in a bump arena (frankensqlite `VersionArena` design, `prev_idx` links, INV-3 ordering). Readers walk to the first version with `commit_seq ≤ snapshot.high` — one compare per hop, usually zero hops.
3. **Concurrency**: writers take the block's eager exclusive lock via the sharded, cache-line-padded `BlockLockTable` (64-way, frankensqlite layout); lock-unavailable ⇒ immediate `Busy` ⇒ deadlock-free by construction. Under measured contention a block escalates to GTX-style delta-chain appends (writers append intents latch-free; readers merge on the fly; compactor collapses) — the escalation is per-block and reversible. Giant hubs additionally *stripe*: the skip-list of blocks partitions the neighbor-ID space into independently locked, independently versioned, independently sealable ranges, so a 10⁸-degree vertex is a set of parallel publication domains rather than one convoy — and witness granularity (§7.3) follows the stripe, not the hub.
4. **Hot-burst overflow**: a bounded per-partition append log (LiveGraph lesson) absorbs ingest spikes ≥ the block path's throughput; a drain actor sorts it into blocks. The log is bounded by an admission semaphore — backpressure, never unbounded memory.
5. **Property deltas** live beside adjacency deltas as row-patches; sealed property data is columnar (§6.4).

### 6.3 Tier R internals (sealed runs)

- **Layout per run segment**: header (fenceposts, stats, checksum, seq-range) · offsets as an Elias-Fano monotone sequence (O(1) select ⇒ O(1) neighbor-list location, near-entropy size) · neighbor gaps as SIMD-decodable delta-varint (StreamVByte-style, in-house) with a per-list "dense range" fast path (contiguous ordinal runs encode as intervals — huge on generated/imported graphs) · optional hole bitmap (roaring-style, in-house) · edge-prop row locators implicit by position.
- **Intersections without decompression**: Elias-Fano supports galloping `select/rank` directly on compressed form — WCOJ intersections (§8.7) run on sealed runs natively; this is the Strata↔Loom co-design payoff.
- **Runs are generational**: seal (Tier D → R₀ run), then size-tiered k-way merge R₀…R_k per partition. A snapshot pins run generations via epochs; compaction publishes a new segment set atomically in the manifest and retires the old under lease — readers never block, never see mixed generations.

### 6.4 Property storage

Columnar chunks per `(label, property)` for vertices and per `(edge_type, property)` for edges, aligned to the partition grid: dictionary encoding (ART-backed build), FoR/bit-packing for ints, per-chunk zone maps (min/max/null-count/ndv-sketch), validity bitmaps, overflow heap for large strings/bytes with content-addressed dedup. Edge property chunks are ordered by *run position* so pattern-matching pipelines read them as sequential vectors (the Kùzu columnar-edge advantage). Tier-D property patches overlay chunks through the same MVCC merge as adjacency. `Embedding` columns store contiguous f32/f16/i8 matrices — the vector index (§10.4) and any future GraphBLAS-style kernels address them zero-copy.

### 6.5 Vertex directory

Per label class: a packed array indexed by dense ordinal → `{ generation, label-set ref, prop row locator, per-(dir,type) adjacency descriptor (inline degree + Tier-I micro-adjacency slot + tier pointers) }`. Degree is thus O(1) — the optimizer's most-used statistic is a load, not a scan. Ordinal recycling uses a free-list + generation bump; directory segments are themselves versioned objects so vertex creation/deletion is MVCC like everything else.

### 6.6 Write amplification & space discipline

Targets: ingest write-amp ≤ 3× at steady state (capsule + seal + one merge tier, amortized); space overhead vs. entropy-optimal ≤ 1.35× including 20% repair symbols; hole-bitmap density triggers per-segment rewrite at 15%. The compactor's pacing controller is a small control loop (asupersync EXP3-style adaptive machinery is available if a static PID proves insufficient — the house has form here). Every tier migration (inline↔block, block↔run, escalate↔de-escalate, compaction scheduling) is an adaptive decision and therefore emits a **decision card** — policy epoch, observed features, candidate actions, expected-benefit interval, hysteresis state — under two anti-thrash rules: minimum dwell time per descriptor, and expected benefit must exceed conversion cost *plus* uncertainty. A pinned deterministic fallback policy exists so the lab runtime (§15.1) replays every storage decision bit-for-bit.

### 6.7 Buffer & memory management

- **Extent-based buffer manager** over variable-size segments (Umbra lesson): segment handles are swizzlable — hot segments pin to direct pointers; cold segments fault through the manager. Reads use optimistic epoch-protected access (asupersync EBR): version-stamped, retry-on-conflict, no reader latches (LeanStore OLC discipline).
- **Admission/eviction**: ARC-inspired with MVCC awareness (frankensqlite §2.3): a segment needed by any pinned snapshot is unevictable; ghost lists cleaned on GC-horizon advance. Scan-resistant admission for analytics sweeps.
- **I/O**: io_uring via asupersync fs; O_DIRECT with page-aligned buffers; readahead planned by the executor (runs declare their scan intent, the buffer manager prefetches segment chains — B-tree-descent prefetch generalized).
- **Query memory**: every query's scratch (hash tables, factorized vectors, frontiers) allocates from its region heap; cancellation or completion frees it deterministically and instantly. Per-query and global budgets are asupersync `Budget`s; exceeding spills (hash-join partitions, frontier bitmaps) to temp ECS objects.
- **NUMA**: partition→socket affinity for pinned segments; morsel scheduler prefers local partitions.

---

## 7. Transactions: Block-MVCC + Graph-SSI + Semantic Merge

### 7.1 Isolation model

- Default: **SERIALIZABLE** via SSI (Cahill/Fekete dangerous-structure rule) at block granularity — frankensqlite's Page-SSI transposed to graph blocks, same "no txn with both incoming and outgoing rw-antidependency edges commits" rule, same `SireadTable` sharding, same O(1) snapshot-visibility compares. On a single node the commit protocol makes this **strict serializability** — the serialization order *is* the real-time marker order. Weaker modes exist only as *named, typed, unmistakable* contracts: `SNAPSHOT` (explicit opt-out), `READ_ONLY_HISTORICAL` (pinned `AS OF` snapshot), `SNAPSHOT_FOLLOWER(max_lag)` (replica reads, §14.2), and `BRANCH_CAUSAL` (agent branches with merge-on-integrate semantics, §5.6). A session may declare a *minimum* isolation contract, which the server must reject rather than silently degrade.
- Readers never block, never abort (pure SI reads); writers conflict only on (a) same-block FCW after the merge ladder fails, or (b) SSI dangerous structures.
- **Invariants (restated for Strata)** — INV-1 monotonic TxnIds (AtomicU64/SeqCst); INV-2 at most one holder of any block lock; INV-3 version chains strictly newer-first by creator. These three lines get the Lean treatment (§15.5).

### 7.2 Granularity choices (and why)

Blocks (~256 edges) are the graph analogue of frankensqlite's pages: fine enough that writers to different neighborhoods never touch (fan-out of real workloads means most concurrent writes hit distinct vertices, let alone blocks), coarse enough that lock/siread tables stay small and cache-resident. Vertex-level would false-conflict on hubs; edge-level would drown in metadata. Hub blocks that *do* contend escalate to delta-chains (§6.2.3), which converts conflict into commutative appends — contention is handled by *changing the data structure*, not by tuning locks.

### 7.3 Phantom protection for pattern queries

SSI needs predicate reads, and graph patterns read predicates constantly (`MATCH (a:Person)-[:KNOWS]->(b)` reads "all KNOWS-edges of a"). Four-level read tracking, arranged as a *refinable lattice*:

1. **Block reads** → siread entries (exact).
2. **Adjacency-descriptor reads** ("the whole (v, out, KNOWS) list") → a versioned per-descriptor epoch; any structural change bumps it; validation compares epochs — cheap coverage of the dominant pattern-read shape without per-block entries for long lists.
3. **Label/type scans** ("all :Person vertices", "all KNOWS edges") → per-(label|type, partition) scan epochs, bumped by creation/deletion in that partition.
4. **Path-frontier witnesses**: reachability/shortest/cheapest reads record the explored automaton-product frontier — `(NFA state, partition, settled cost bound)` triples — instead of every traversed edge. A later write conflicts only if it could *improve or extend* that frontier (e.g., a new edge whose head lies in a frontier partition below the settled bound); writes provably outside it never abort the path reader.

**Refinement before abort.** Levels 2–4 are conservative summaries, so an apparent overlap is not yet a conflict — it is a *refinement obligation*. Executors already emit a compact `WitnessTrace` side-channel as a byproduct of execution (blocks actually scanned, neighbor ranges actually touched, index key ranges probed, frontier states expanded); validation, on a budget, refines the coarse witness against the writer's footprint down the lattice (population → range → block → element) and aborts only when overlap survives exact comparison — or when the refinement budget expires, in which case the conservative abort is taken and the abort report records that refinement was truncated. False negatives are impossible by construction (refinement only ever *narrows* a superset — an invariant with its own Lean lemma, FG-INV-06); false positives become a tunable engineering knob instead of a correctness tax. The same `WitnessTrace` feeds optimizer cardinality feedback and plan certificates — one encoding, three consumers.

This four-level **refinable witness lattice** gives sound serializability for pattern workloads with a false-conflict rate that engineering can drive toward zero — a design point no published transactional graph store states precisely; we will publish it (§15.7).

### 7.4 The semantic merge ladder (and why graphs love it)

On base-drift at commit (block changed since snapshot), before aborting:

1. **Intent rebase**: replay the txn's graph intent log (Appendix B) against current state. `AddEdge`/`RemoveEdge`/`SetProp` on *different* elements in the same block always commute; even same-element ops often resolve (`SetProp` last-writer-wins is *not* silently applied — only commutative merges pass rung 1; LWW is an opt-in rung-1.5 per property).
2. **Structured patch**: disjoint byte-range block patches (entry-disjoint inserts into one sorted block).
3. **Constraint & derived-state regeneration**: any rung-1/2 success re-validates affected constraints (uniqueness, endpoint, cardinality) and regenerates affected index entries, statistics deltas, and Ripple input deltas *from the merged intents* — never by patching stale physical deltas — before publication. This is the frankensqlite rule that secondary state is regenerated from semantic intent, kept verbatim.
4. **Abort/retry** with `BusySnapshot` + machine-readable conflict report.

frankensqlite built this ladder to rescue B-tree page conflicts; graph intents are *far more* commutative than B-tree cell edits, so the ladder's hit rate — and therefore effective write concurrency under skew — should be dramatically higher. The same ladder is the branch-merge engine (§5.6) and the multi-writer replication rebase (§14.3): one mechanism, three features (the B1 pattern again). Rung-1 replay determinism is enforced by construction: the rebase evaluator runs under a capability-stripped `Cx` — no clock, no entropy, no I/O (constraint #8) — so an intent that smuggled nondeterminism in cannot even *express* it at replay time.

### 7.5 Bulk & DDL paths

`COPY`/bulk-import bypasses Tier D: sort pipeline → direct sealed-run construction → single manifest publish (one "mega-commit" capsule referencing the runs). Target: saturate NVMe sequential write (§17). Online index build = snapshot scan + delta catch-up under a build obligation; DDL rides schema-epoch bumps.

### 7.6 The write path as asupersync choreography

WriteCoordinator is a supervised actor; commit requests arrive on a two-phase MPSC (reserve/commit — a cancelled client can never leave a half-submitted commit); its response is a oneshot obligation (it *cannot* forget to answer — region close would flag the leaked obligation); group-commit batching uses the pipeline combinator with an EDF deadline lane so latency-sensitive commits aren't starved by bulk. Every one of these guarantees is structural, inherited, and lab-testable. This paragraph is the whole thesis of building on asupersync in miniature.

---

## 8. Loom: Languages, Algebra, Optimizer, Execution (Bet B3)

### 8.1 Language surface

- **Native: GQL (ISO/IEC 39075:2024)** — full graph pattern matching (GPM) including quantified path patterns, path modes (WALK/TRAIL/ACYCLIC/SIMPLE), SHORTEST/CHEAPEST path selectors, label expressions, `LET`/`FILTER`/`FOR`, composite queries, catalog/DDL, transactions/sessions. We maintain a public conformance matrix against the standard's feature IDs (Spanner-style) and track the in-flight ISO revision (path-pattern extensions) as it lands.
- **openCypher compatibility layer**: the pragmatic on-ramp (drivers, tooling, LLM familiarity). One parser front produces one algebra; Cypher-vs-GQL divergences are handled at the front (documented dialect table + openCypher TCK in CI).
- **FQL extensions** (namespaced, never squatting on standard syntax): `FOR SYSTEM_TIME AS OF/BETWEEN` temporal clauses on `MATCH`; `AT BRANCH`; `CALL fnx.*` (Prism); `CALL vector.search / text.search / hybrid.search`; `CREATE MATERIALIZED VIEW … REFRESH INCREMENTAL` and `SUBSCRIBE TO` (Ripple); `EXPLAIN (CERTIFICATE)`; graph-project/branch/merge admin verbs.
- **Datalog core**: recursive views and Ripple circuits are expressible in a stratified Datalog-with-aggregates surface (`fgdb-datalog`) that compiles to the same algebra — the power tool under the standard-language hood, and the natural target for machine-generated queries.

### 8.2 Semantics discipline

GQL's formal-semantics literature is tracked; where the standard leaves latitude (result ordering, path enumeration order, duplicate semantics under morphism modes), FrankenGraphDB pins behavior via CGSE tie-break policies (§8.6) and documents each pin. Temporal clauses get algebraic semantics (snapshot-relativized pattern matching; `BETWEEN` yields per-binding validity intervals with interval-algebra predicates).

Each pin set is packaged as a versioned **SemanticProfile** — `gql-2024-strict`, `cypher-compat-<n>`, `fnx-parity` — fixing null/UNKNOWN handling, duplicate semantics per morphism mode, ordering defaults, coercion rules, and tie-break-policy bindings. The profile ID rides in every prepared plan, plan-cache key, and certificate: two results are comparable only under the same profile, and dialect drift becomes a versioned artifact instead of folklore.

Path expressions carry an explicit, rewrite-protected semantics record — `{ mode: WALK|TRAIL|ACYCLIC|SIMPLE, selector: ANY|SHORTEST|ALL_SHORTEST|K|CHEAPEST, multiplicity, hop/cost bounds, tie_break_policy, temporal_rule, truncation_policy }`. Optimizer rewrites must preserve it or insert an explicit restoring operator; in particular, anti-joins/`NOT EXISTS` carry their phantom-witness obligations (§7.3) through every rewrite — no transformation may narrow the domain a negative pattern must witness. Enumeration under explosive semantics (unbounded SIMPLE-path counting) requires an explicit budget or truncation policy in Hardened posture; the engine never silently substitutes WALK semantics for SIMPLE.

### 8.3 One algebra ("GLA" — Graph-Logical Algebra)

Typed logical operators over *binding tables whose column types include factorized structure*: `ScanVertices(label, pred, asof)`, `Expand(dir, types, pred, quantifier)`, `PathFind(mode, selector, cost)`, `FreeJoin([subpatterns])`, `Select/Project/Aggregate/OrderBy/Limit/Distinct`, `Fixpoint(delta_plan)` (recursion), `TemporalJoin`, `IndexProbe(beacon)`, `SemiringMxV(⊕,⊗,mask)`, `Mutate(intents)`, `ViewDelta` (Ripple tap). Relational and pattern operators live in one algebra with one cost model — the SPJM lesson — so `MATCH` + aggregation + subqueries optimize jointly, no seam.

### 8.4 Factorization as a type

A binding table column is `Flat(vec)` or `Factorized(parent_col, unnest_of run-slice)`. `Expand` produces factorized columns natively (a run slice *is* the compressed representation — zero-copy factorization); aggregates, `DISTINCT`, projections, and count-of-paths push through factorized columns at FDB-optimal cost; flattening is an explicit operator the optimizer inserts only when an operator demands flat input. Wire encoding can ship factorized frames (§13.3): a 3-hop friends-of-friends result that would be 10⁸ flat rows ships as ~10⁴ run slices.

### 8.5 Optimizer

- **Statistics** (StatsSegments, incrementally maintained by Ripple): per-(src_label, type, dst_label) edge counts and degree histograms (both directions); NDV sketches (HLL) per property; zone maps; a *path synopsis* — sampled 2-path correlation table capturing the joint distributions binary independence assumptions butcher (the classic graph-cardinality failure mode); KLL quantile sketches for cost-relevant value distributions.
- **Enumeration**: DP over connected subpatterns (DPccp-style) producing hybrid plans where each join node is a `FreeJoin` configuration — the plan space *contains* pure binary plans, pure Generic-Join plans, and everything between (the Free Join insight); cyclic cores get WCOJ configurations by construction when the AGM bound beats the best binary estimate. Variable/attribute orderings chosen by cost with an ADOPT-style adaptive override hook.
- **Adaptivity**: operators carry cardinality guards; a >Kx misestimate triggers morsel-boundary re-optimization of the remaining plan (safe under morsel-driven execution because state is partitioned). Every re-plan event is recorded in the plan certificate.
- **Cost as an interval, risk in the objective**: estimates propagate as `(low, est, high)` intervals with correlation-aware widening from the path synopsis; plan choice minimizes expected cost *plus* explicit penalties for tail blowup, spill probability, and SSI-abort exposure (write plans price their witness footprint). When the top plans' intervals overlap heavily *and* the query is cheap enough to duplicate, the executor may **hedge**: launch the top two via the asupersync `hedge` combinator in sibling regions, keep the first useful morsel stream, cancel-and-drain the loser (obligations make abandonment impossible), and record the hedge in the certificate. Racing is the exception, gated by uncertainty; morsel-boundary re-planning stays the default adaptivity mechanism. Learned cardinality estimators are admissible only as *advisory* features on a decision card, with the analytic model as the deterministic fallback.
- **Views**: Ripple-maintained materialized views register as rewrite targets (view matching over the algebra).

### 8.6 Determinism contracts (CGSE at query level)

Every query executes under a declared **tie-break policy** (default `InsertionOrder`-compatible run order; alternatives per CGSE's 13-policy vocabulary, e.g. `LexMin` for order-independent reproducibility). `ORDER BY` always wins; the policy governs everything unordered. Parallel plans that would break the contract either merge deterministically (ordered morsel reassembly — default) or require `SET result_determinism = RELAXED` (recorded in the certificate). Aggregation over floats defaults to deterministic pairwise/Kahan reduction trees. **Plan certificates** (§15.2) carry: plan hash, policy, snapshot seq, per-operator observed counts vs. complexity witness bounds, re-plan events, and a BLAKE3 decision-path hash — a query result becomes an auditable, replayable artifact. For agents and regulated pipelines, this is a feature no competitor ships. Certificate detail follows the frankensqlite budgeted-evidence pattern via four modes — `Compact` (hashes, counts, policy IDs; the default), `Budgeted` (full detail up to a byte budget, then deterministic truncation), `Full`, and `Forensic` (RaptorQ-protected trace journal) — with the asymmetry preserved: abort, conflict, and repair evidence is always richer than happy-path evidence.

### 8.7 Execution engine

- **Push-based, vectorized** (1024-row vectors of flat or factorized columns), **morsel-driven parallel**: morsels = partition-aligned work units executed as tasks in the query's region; work-stealing via the three-lane scheduler; cancellation drains at morsel boundaries (`cx.checkpoint()` per morsel — query timeout is the cancellation protocol, not a bolt-on).
- **FreeJoin operator**: unified binary/multiway join with COLT-style lazy trie building for non-graph inputs — and the punchline: for graph atoms, **sealed runs already are the (src → dst) tries**, Elias-Fano-searchable in compressed form, so the "build" phase for adjacency is free and intersections run as SIMD galloping over compressed neighbor lists, merged with Tier-D deltas on the fly. Hash tables (in-house raw SwissTable-style, vectorized probe) serve the binary end of the continuum and property-side joins.
- **Path & recursion operators**: BFS/shortest/cheapest with frontier bitmaps over dense ordinals (GraphBLAS-mask spirit), bidirectional search, K-shortest (Yen/Eppstein) — planner-integrated, not library calls; general recursion lowers to `Fixpoint` over Ripple's delta evaluation (semi-naïve for free, §9.3); regular path queries / quantified patterns compile to NFA-product expansion with on-the-fly state minimization and memoized (state, vertex) frontiers. Bulk traversals additionally lower to **masked semiring kernels** — SpMV/SpMSpV with pluggable `(⊕, ⊗)` over sealed runs, which *are* CSR, with push/pull direction-optimization chosen by frontier density and deterministic reduction modes honoring §8.6 — one kernel substrate shared with Prism's native algorithms (§11.4). `RETURN PATH` results stream in **succinct factorized form** (path-multiset representation: shared-prefix DAGs over run slices) rather than exploded per-path rows, flattening only on demand.
- **SIMD**: nightly `core::arch` + portable-SIMD kernels for varint decode, EF select, bitmap ops, hash probing, predicate eval — the asupersync GF(256) SIMD kernel discipline (deterministic per-process kernel selection, policy snapshots) reused.
- **Spilling**: grace-hash partitioning and frontier spill to temp ECS objects under Budget pressure; larger-than-memory is a property of every operator, not a mode.

### 8.8 Result delivery

Streaming from first morsel; bounded by two-phase channel obligations end-to-end (a slow client backpressures the executor; a cancelled query can never half-deliver a frame). Factorized frames on the wire when the client opts in (§13.3).


---

## 9. Ripple: Incremental Everything (Bet B4)

### 9.1 The unification claim

Four features that every vendor builds separately — recursive query evaluation, materialized views, standing queries/subscriptions, and incremental analytics — are one mathematical object: computation over **Z-sets** (multisets with integer weights; deletions are weight −1) advanced by a totally ordered stream of deltas. DBSP proved the theory (with Lean-verified theorems — pleasingly, the same proof assistant anchoring asupersync's invariants) and showed the chain rule lets you incrementalize operator-by-operator. FrankenGraphDB's structural advantage: **the commit stream already is the delta stream** — every `CommitCapsule`'s intent log is precisely a Z-set delta over vertices/edges/properties, totally ordered by `CommitSeq` (DBSP's "linear synchronous time" simplification is not a compromise for us; it is our native geometry).

### 9.2 The circuit engine

`fgdb-ripple` implements a from-scratch DBSP-style circuit runtime specialized for graph operators: Z-set arithmetic on the same vectorized binding-table representation as Loom (indexed Z-sets = our hash/ART structures; factorized Z-sets supported for `Expand` deltas), lift/differentiate/integrate/delay operators, the chain-rule incrementalizer applied to GLA plans, and nested circuits for `Fixpoint`. Circuits are compiled *from the same optimizer output* as batch plans — one algebra, two execution modes (batch, incremental).

### 9.3 Recursion = incremental fixpoint

`Fixpoint(plan)` in batch mode executes as a Ripple nested circuit fed its own output delta: this *is* semi-naïve evaluation, derived rather than hand-implemented, and it automatically extends to recursion with aggregates under stratification (DBSP's covered class). Transitive closure, reachability, same-generation, shortest-path-with-preferences — one mechanism, incremental under updates for free when materialized.

### 9.4 Materialized graph views

`CREATE MATERIALIZED VIEW … REFRESH INCREMENTAL` installs a circuit whose integrated output persists as first-class Strata data (a view is a virtual label/edge-type — pattern-matchable, indexable, even vector-indexable). Consistency contract: views are **snapshot-consistent at a published `CommitSeq` watermark** (readers see view@w joined with base@w — no read anomaly between base and view; the watermark advances as the circuit drains). Maintenance runs as a supervised actor pool consuming the marker stream; lag is observable; recompute-from-anchor is the repair path. Each view declares its deletion semantics up front — Z-set multiplicity counting (the default: deletions are weight −1 and cost the same as inserts) or support-set provenance for the small non-monotone class that needs it — so "append-only view that lies under deletes" is unrepresentable.

### 9.5 Standing queries & changefeeds

`SUBSCRIBE TO MATCH …` = the same circuit, with output deltas fanned out over asupersync broadcast channels to WebSocket/gRPC-stream sessions (two-phase delivery; drop policies explicit per subscriber: `BUFFER n | COALESCE | DISCONNECT`). Plain changefeeds (`SUBSCRIBE TO CHANGES ON label/type [WHERE …]`) are the degenerate circuit. Triggers (`ON MATCH DO CALL proc`) execute in the *post-commit* phase with exactly-once obligations recorded in the stream — no in-commit trigger foot-guns.

### 9.6 Incremental analytics maintainers

For algorithm families with known delta algorithms, Ripple hosts maintainers that keep results warm under updates: delta-PageRank (residual propagation), incremental connected components (union-find + bounded rebuild on deletes), incremental triangle/k-core counts, degree/label statistics (this is how §8.5's StatsSegments stay fresh). Each maintainer declares a **staleness contract** (exact | bounded-error(ε) | eventually-refreshed) surfaced in query certificates; recompute via Prism is the universal fallback. This directly answers the market's "derived state goes stale" complaint — including for the vector/FTS indexes, whose ingestion pipelines (§10) are Ripple consumers too.

---

## 10. Beacon: The Index Fabric

All secondary structures share one lifecycle (delta tier → sealed segments → compaction; MVCC visibility via seq-stamps; snapshot pinning via epochs) and one registration surface (the optimizer sees every Beacon as an access path with cost & determinism metadata). Building N index types is therefore one framework plus N payload formats.

1. **Property B-trees** (in-house, cache-optimized, prefix-truncated) and **hash indexes** per (label, property); unique key indexes back `Strict` key constraints; composite keys supported. Zone maps (§6.4) make many point/range predicates index-free.
2. **Adjacency views (A+-style)**: user-declared alternative sort orders / partitionings of an edge type's runs — e.g. `INDEX ON :RATED ORDER BY (dst, rating DESC)` — giving Graphflow-style tunable secondary adjacency for query shapes the primary run order can't serve. These are just extra run families; the whole Strata machinery is reused. The same mechanism hosts **dense projections**: bitmap or dense-tile adjacency views over selected (label, type, label) subgraphs — derived, Ripple-maintained, snapshot-cursored — feeding the semiring kernels (§8.7) and set-intersection fast paths without ever becoming a storage-tier citizen.
3. **Full-text**: segment-based inverted index, in-house tokenization (Unicode word-boundary + language-pluggable stemmers), BM25 with positions/phrase/fuzzy(edit≤2 via Levenshtein automata over the term ART); frankensqlite's FTS5 module is the in-family behavioral reference.
4. **Vector (native ANN)**: HNSW built in-house with the Strata pattern applied to the index itself — a mutable delta graph absorbing inserts + sealed HNSW segments merged/rebuilt by the compactor; deletes are visibility-filtered at search (seq-stamped nodes) and purged on merge; search fans across segments with a k-way result merge. Consequences nobody else offers together: **transactional** vector search (your ANN results respect your snapshot), **time-travel** vector search (`AS OF` applies), **branch-scoped** vectors, and freshness measured in commit-latency not reindex-hours. Quantization (SQ8, PQ) as segment codecs; `Embedding` columns are the ground truth for exact re-rank. Metrics: cosine/dot/L2, SIMD kernels.
5. **Path/reachability indexes (adaptive)**: 2-hop/pruned-landmark labeling built *on demand* per (edge-type-set) subgraph when the optimizer observes repeated reachability/shortest-path load, maintained incrementally by Ripple, dropped under decay — indexes as a cached, self-managing resource rather than a DBA chore.
6. **Hybrid retrieval operator**: `CALL hybrid.search(text?, vector?, seeds?, expand_pattern?, k, fusion=RRF|weighted)` — ANN + BM25 + graph expansion fused *inside the planner* (score-aware pruning, factorized expansion), i.e., GraphRAG's retrieval step as one optimized operator instead of three round-trips. This plus §12's capability scoping is the B6 product core.

---

## 11. Prism: In-Database Analytics via franken_networkx (zero copy)

### 11.1 Surface

`CALL fnx.<algorithm>(graph_view, params) [YIELD …]` exposes the entire applicable fnx catalog (500+ functions: centralities, communities, flows, matchings, isomorphism, spectral, k-core, link prediction, …) inside queries, composable with `MATCH` (e.g., compute Louvain on a temporal-filtered projected subgraph, then pattern-match against community ids — one transaction, one snapshot).

### 11.2 The zero-copy bridge

A Strata snapshot exposes `SnapshotGraphView` implementing fnx's `GraphView` trait: `neighbors_indices(idx) -> Option<&[usize]>` is answered from **decoded run caches** — per-segment, lazily materialized, epoch-shared flat `u32/u64` neighbor arrays that the executor also uses for repeated-scan plans. For pure-Tier-R vertices (the overwhelming majority under analytics), this is a pointer into cache; Tier-D residue merges once into the per-query overlay. Ordinal↔name mapping satisfies the string side of the trait. Algorithms thus traverse database memory with **no graph materialization**, under full snapshot isolation.

### 11.3 Determinism & witnesses flow through

CGSE policies and `ComplexityWitness` emission work unchanged — an in-database PageRank run yields the same witness ledger entries as standalone fnx, folded into the query's plan certificate. Analytics results become auditable artifacts (B5).

### 11.4 Coverage strategy

fnx algorithms not yet `GraphView`-generic take the materialization path (snapshot → `fnx-classes` graph build, parallel, ~50M edges/s target) with an explicit `MATERIALIZED` badge in the certificate. **Workstream W7 upstreams genericity**: extend `GraphView`-generic coverage across fnx families (priority: traversal, components, centrality, community, shortest-path — the in-DB heavy hitters), benefiting both projects. Long-degree kernels (PageRank/label-prop/BFS-class) additionally get native vectorized implementations over run caches (the §8.7 masked-semiring kernels, frontier-masked) when profiling shows the trait-call overhead matters; fnx remains the semantics oracle for their differential tests.

### 11.5 Scheduling

Analytics run in dedicated regions on a bulkhead-isolated lane (combinator: `bulkhead`) so a Louvain over 10⁹ edges cannot starve p99 OLTP; budgets + cancellation apply; long runs checkpoint progress as obligations (a cancelled community detection cleans up its scratch deterministically).

---

## 12. Warden: Security & Governance

1. **Capability tokens (macaroons)** — asupersync's implementation with graph caveats: `graph=g`, `branch=b`, `labels⊆{…}`, `edge_types⊆{…}`, `subgraph=MATCH-predicate`, `asof≤seq`, `ops⊆{read,write,ddl,subscribe,analytics}`, `expiry`, third-party discharge for SSO. Enforcement is *planner-integrated*: caveats compile to mandatory predicates/label masks on every scan/expand (row/subgraph-level security with index-aware pushdown, not post-filtering). A capability that can't see an edge type can't observe its existence via degree, either (descriptor masking). This is the direct answer to the market's governance gap — and capability *narrowing* means an agent handed a caveated token physically cannot exceed it (B6).
2. **AuthN**: mTLS (asupersync TLS), token exchange endpoint, per-session `Cx` carries the capability — authority flows through the same channel as cancellation. Audit log = the commit stream (writes) + a query-certificate ledger (reads): tamper-evident by content addressing.
3. **At rest**: Argon2id → KEK → per-database DEK; XChaCha20-Poly1305 per ECS object, encrypt-then-code (corrupted ciphertext heals via RaptorQ *then* decrypts — the frankensqlite layering); per-branch DEK derivation enables cryptographic branch hand-off (give a partner the key to a branch, not the database). `fgdb-crypto` implements the primitives in-house against published test vectors (small, well-specified ciphers only — we do **not** hand-roll TLS; that's asupersync's vendored problem).
4. **In flight**: TLS 1.3 everywhere; RaptorQ symbol planes use per-symbol auth (fail-closed posture inherited from asupersync's ATP hardening).
5. **Security-context identity everywhere derived state lives**: plan caches, prepared statements, materialized-view eligibility, statistics visibility, and result caches key on a digest of the effective capability — a plan compiled under one caveat set is *unreachable* from another, and absence-of-results witnesses (§7.3) are scoped to the authorized subgraph, so the serializability machinery can never become an oracle about data the caller cannot see.
6. **Leakage posture & evidence redaction**: timing/cardinality side channels on caveated subgraphs are acknowledged, not hand-waved — Hardened deployments get padded error taxonomies, bounded-cardinality error responses, and per-tenant plan/memory pools; certificates and the system graph (§16.5) apply **redaction profiles** (fields inline, hashed, or privileged-only), so evidence remains auditable without becoming an exfiltration channel.

---

## 13. Fabric: Server, Protocols, and the Embedded API

1. **Embedded**: `fgdb::Database::open(path | :memory:)` → sessions → prepared statements → streaming results; Rust-native typed row/column accessors; register Rust procedures/UDFs (capability-gated). Python bindings via the fnx PyO3 playbook (`fgdb-python`, ABI3), including a `to_fnx()`/`from_fnx()` zero-friction bridge and NumPy views over `Embedding`/result columns.
2. **`fgdbd` server**: multi-database, multi-tenant by capability; connection admission via semaphore + rate-limit combinators; graceful drain on shutdown (region close = no dropped in-flight commits, structurally).
3. **FGP (native wire protocol)**: length-delimited frames (asupersync codec) over TCP/TLS or QUIC; handshake with feature negotiation; typed columnar result frames with optional **factorized frames** (§8.4) and optional streaming compression; server-push frames for subscriptions; cancellation as a first-class frame mapped to `Cx` cancellation; optional RaptorQ FEC framing for lossy/WAN links (the ATP machinery — a bulk export over a flaky link just works). Columnar result frames define a documented, Arrow-compatible logical layout (an owned client-edge adapter maps frames ↔ Arrow IPC) — dataframe interop without admitting the dependency. Appendix D sketches frames.
4. **HTTP/2 + gRPC + WebSocket** (asupersync http): query endpoint, subscription streams, health/metrics, bulk import/export endpoints (multipart or ATP bonded pull).
5. **Bolt-compat subset** (`fgdb-bolt`): enough of Bolt v5 + Neo4j type mapping to let standard Neo4j drivers and visualization tools connect for read/query workloads — the adoption wedge. Documented divergence table; not the native path.
6. **QoS**: per-capability token buckets; EDF deadline lane for interactive vs. bulk; hedged reads on replicas (combinator: `hedge`); circuit breakers on downstream fan-outs.
7. **Formats & migration** (`fgdb-formats`): everything fnx-readwrite parses (GraphML/GEXF/GML/Pajek/edgelist/adjlist/JSON node-link, graph6/sparse6) imports natively; CSV/JSONL bulk loaders (in-house, SIMD-accelerated delimiting); **Parquet-lite** reader/writer (in-house: plain/dictionary/RLE-bitpacked encodings, snappy + uncompressed pages — deliberately a subset, covering the overwhelming majority of real files; DEFLATE available via asupersync's http compress module); Neo4j dump/CSV-export importer; `.fgdb` archive = self-contained ECS bundle with decode proofs (a *verifiable* backup format).

---

## 14. Aegis: Replication, HA, and the Distribution Doctrine

**Doctrine**: single-node excellence is the product; replication is for durability, availability, and read scale; *sharding is designed-in but sequenced last* — and Strata's partition grid, dense ordinal spaces, and capsule-based movement are the sharding substrate, decided now so nothing needs a rewrite.

1. **Replicated state machine**: the marker stream is the log; Raft-class consensus (`fgdb-raft`, in-house on asupersync leases/quorum/session-typed remote protocol) sequences `CommitMarker`s; capsule payloads flow out-of-band as RaptorQ symbols (leader streams; followers ack on decode-proof) — consensus carries ~100-byte markers, bulk bytes ride the fountain-coded plane. Deterministic apply (intent logs are deterministic by construction) means replicas are bit-identical — divergence is *detectable by ObjectId comparison*, continuously.
2. **Reads**: followers serve snapshot reads at their applied watermark; `read_your_writes` via seq tokens; leader leases for linearizable reads; hedged cross-replica reads for tail latency.
3. **Multi-writer posture** (post-consensus workstream): optimistic writer-anywhere — followers build capsules against local snapshots and submit through the leader's merge ladder (§7.4 as a *replication rebase*). Skew-commutative workloads (agent swarms appending facts) get near-linear write scaling without giving up serializability; true conflicts behave exactly like local FCW.
4. **Seeding/repair/backup**: new replica = anchor pull via **bonded multi-donor ATP** (fetch one anchor from N peers simultaneously, any K-of-N symbols suffice) + capsule catch-up; scrubber-escalated object loss heals from any replica holding symbols; `BACKUP TO` emits `.fgdb` archives with decode proofs; PITR = restore anchor + replay to seq (all mechanisms already required by B1 — replication is Chronicle over the network, not a second system).
5. **Sharding (final workstream, design constraints active from day one)**: partition-grid-aligned vertex-cut; distributed FreeJoin with factorized shuffle frames; distributed Ripple watermarks; per-shard Raft groups; the asupersync `distributed` module's consistent hashing + vector clocks + sagas are the toolkit. Everything above (dense ordinals per partition, capsule movement, deterministic apply) was chosen to make this a *composition*, not a rewrite. Topology epochs (§4.5) version every ownership map; a transaction binds to an epoch, and repartitioning is a shadow-copy / delta-catch-up / atomic-cutover job — the §6.3 publish discipline over the network — never in-place ownership mutation.


---

## 15. Determinism, Verification & Testing Doctrine (Bet B5)

This is not a QA appendix; it is a design pillar with its own budget. The stack, from cheapest to strongest:

### 15.1 Simulation-first development (`fgdb-sim`)

The entire database — storage, txns, compaction, Ripple, replication, server — runs under asupersync's **lab runtime**: virtual time, seeded deterministic scheduling, virtual TCP, virtual disk (a lab VFS with injectable latency, torn writes, bit flips, ENOSPC, fsync lies), chaos injection points at every obligation boundary. Consequences: every concurrency bug is a seed; CI explores schedules with DPOR (Mazurkiewicz-trace pruning — exhaustive over *inequivalent* interleavings, not a random walk); failing runs auto-attach crashpacks with replay commands. This is the FoundationDB discipline, obtained largely by inheritance rather than construction — the single biggest schedule-risk reducer in the whole plan.

### 15.2 Consistency oracles

In-sim checkers run continuously, not post-hoc: snapshot-isolation oracle (no read sees seq > snapshot.high), SSI oracle (reconstruct the rw-dependency graph from traces; assert no committed dangerous structure — note the pleasing recursion: **FrankenGraphDB's own cycle detection verifies its own serialization graphs**), obligation-leak oracle (inherited), quiescence oracle, Elle-style history checking (in-house implementation of cycle-based isolation anomaly detection over lab histories), and e-process anytime-valid monitors on invariant streams (asupersync machinery) for statistical anomalies in long soak sims. Plan certificates (§8.6) make *production* results replayable: certificate + seq + seed ⇒ re-execution must match byte-for-byte.

### 15.3 Fault & recovery torture

Crash-point matrix over the two-fsync protocol (kill at every labeled point; assert marker/capsule invariant); torn-write + bit-rot campaigns asserting RaptorQ recovery up to the configured overhead and *fail-closed* beyond it; compaction crash/lease-expiry storms; multi-process manifest races; replication partition/donor-loss during bonded pulls. Every campaign is a deterministic lab scenario with a scenario-runner profile (asupersync's `--bundle` pattern).

### 15.4 Differential & conformance testing

- **Semantics oracles**: an executable **reference engine** (`fgdb-reference`) — a deliberately simple, single-threaded, obviously-correct implementation of the full logical semantics (values, visibility, path modes, intents, temporal selectors, branches) over canonical maps, compiled for tests/fuzzing/model-checking only, never shipped, never optimized, so "what should this return" is a program rather than a debate; openCypher TCK; a GQL feature-conformance corpus keyed to ISO feature IDs (published as a matrix); differential vs. Neo4j & Memgraph on a curated corpus (behavior parity where standards align, documented divergence elsewhere); Prism results differential vs. standalone fnx (which is itself NetworkX-parity-locked — a two-hop oracle chain to ground truth). Model-generated histories run against both engines and compare snapshots, results, certificates, and permitted abort outcomes.
- **Storage oracles**: model-based testing of Strata against an in-memory reference graph (arbitrary op interleavings, equality after every op under every open snapshot); metamorphic suites in the house style (e.g., pattern-match results invariant under compaction, seal, branch-fork, encode/decode round-trips).
- **Fuzzing**: parser (GQL/Cypher grammars), FGP frames, every fgdb-formats reader (extending fnx's 8-fuzzer precedent), SymbolRecord decoder.

### 15.5 Formal anchors (scoped, honest — the asupersync posture)

Lean: MVCC visibility (INV-1/2/3 + snapshot rule ⇒ SI reads), SSI safety at block granularity (dangerous-structure rule ⇒ serializability, following Cahill/Fekete), merge-ladder rung-1 soundness (commutativity conditions), Z-set incrementalization correctness for our operator subset (leaning on DBSP's published Lean development). TLA+/TLC: two-fsync commit + recovery, compaction publish/retire, Raft-marker interaction, branch fork/merge. Each claim gets a proof-lane manifest row stating exactly what is and is not proven — no blanket claims, per the house contract style. Every named invariant in this document carries a stable ID in the **invariant registry** (Appendix F): statement, enforcement mechanism (Lean lane, TLA+ model, runtime oracle, or CI gate), and owning crate — the registry is machine-readable (`invariants.toml`) and CI cross-checks that every ID has a live checker, so an invariant cannot silently lose its enforcement.

### 15.6 Performance regression apparatus

Criterion-style in-house microbench harness + macro suites (§17) with baselines committed, variance-aware gating (the asupersync/fnx baseline artifact discipline), and complexity-witness regression locks: an algorithm or operator whose observed-op count exceeds its declared bound *fails CI*, not just a dashboard. Beyond performance, every verification domain in this section is a *permanent* CI gate (semantics, transaction anomalies, crash matrix, formats, representation-equivalence, incremental correctness); a release may bypass a gate only with a public, expiring waiver recorded in the ledger.

### 15.7 Publish the design

Sortledton/Teseo/Kùzu earned trust by publishing. Target four papers/tech-reports from this work: (1) Strata: temperature-tiered transactional adjacency; (2) One Version Universe: unifying MVCC, temporal, branching, and replication over a fountain-coded commit stream; (3) Refinable predicate witnesses: graph serializability with near-zero false conflicts and no per-edge read tracking; (4) Plan certificates & decision cards: deterministic, auditable query execution. Ambition includes the receipts.

---

## 16. Observability & Operations

1. **Process tree** (Spork supervision, restart topologies declared):
```
FgdbRootRegion
 ├── ChronicleRegion      (WriteCoordinator · scrubber · checkpointer)
 ├── StrataRegion         (sealers · compactors per partition-group · GC/demotion)
 ├── RippleRegion         (view circuits · subscription fan-out · stats maintainers · index feeders)
 ├── BeaconRegion         (index builders · HNSW mergers · path-index manager)
 ├── PrismRegion          (analytics bulkhead)
 ├── AegisRegion          (raft · replication streams · donor service)
 ├── FabricRegion         (listeners · sessions · admission)
 └── ObservatoryRegion    (metrics export · task inspector · spectral health · deadline monitor)
```
2. **Telemetry**: asupersync metrics (zero-alloc hot path) + OTel export; per-query certificates double as trace spans; `EXPLAIN (ANALYZE, CERTIFICATE)`; `SHOW` surfaces for txn lifecycle introspection (frankensqlite's bd-t6sv2.5 pattern), compaction debt, Ripple lag, replication watermarks, buffer residency, obligation counts.
3. **Early warning**: the spectral wait-graph monitor watches the *database's own* task graph — Fiedler-trend degradation pages before a stall does. Futurelock detection catches "held a commit permit, stopped polling" classes structurally.
4. **Ops verbs**: online `COMPACT/SCRUB/REBUILD INDEX/ANALYZE`; `fgdb doctor` (manifest/chain/proof verification); crashpack bundles attach to every panic-contained incident with replay instructions.
5. **The system graph**: the database's own runtime — sessions, transactions, queries, plans, operators, snapshots + leases, obligations, segments, compaction jobs, subscriptions, replication streams, repairs, decision cards — is exposed as a read-only, access-controlled *temporal property graph*, queryable in GQL like any other graph (`MATCH (q:Query)-[:WAITS_FOR]->(x) WHERE q.elapsed_ms > 250 RETURN q, x`). The wait-for graph is literally a graph: deadlock and contention analysis run Prism's own cycle detection and centrality over it, and incidents become subgraphs you can diff across time. The database dogfoods its data model as its control plane — bounded cardinality, sampled under load, redacted per capability (§12.6).
6. **Admission, backup, upgrade**: admission control is hierarchical asupersync `Budget`s (tenant → session → query) reserved *before* launch, with structured refusals (a rejected query gets a machine-readable reason and retry class, not an OOM); `BACKUP` = the content-addressed manifest closure (§13.7's `.fgdb`), incremental by ObjectId, restore-verified by decode proofs and canonical digests before the restored database opens; scheduled **decode drills** and crash-recovery rehearsals run as lab scenarios on a calendar, not after incidents; rolling upgrades follow a format-compatibility matrix (formats versioned additive-minor/breaking-major; mixed-version replica sets validated in sim before any release tags).

---

## 17. Performance Doctrine & Targets

Reference machine: 32-core/64-thread, 256 GB RAM, PCIe-4 NVMe (7 GB/s), single node. Numbers are *gates* (CI-enforced on the reference class), chosen from measured SOTA anchors (CSR scan bandwidth, Sortledton ingest, Kùzu/LDBC results, published fsync floors) with leapfrog margins:

| Domain | Gate |
|---|---|
| Cold bulk load (CSV/Parquet-lite → sealed runs) | ≥ 40M edges/s sustained (I/O-bound: ≥ 60% of NVMe seq-write ceiling) |
| Transactional ingest (small txns, group commit, fsync honest) | ≥ 2M edge-inserts/s sustained; single-txn commit p50 < 250 µs, p99 < 1.5 ms |
| Point reads (vertex by key, 1-hop existence) | ≥ 8M lookups/s across cores; p99 < 15 µs warm |
| Neighbor scans, sealed runs (decoded-cache path) | ≥ 500M edges/s per core; ≥ 10B edges/s node aggregate (memory-bandwidth-bound: EF+varint decode ≥ 4 B/edge effective) |
| 2-hop factorized count (10⁸-flat-row equivalent) | < 50 ms (factorization gate: must not materialize) |
| Triangle count (WCOJ over compressed runs), e.g. com-Orkut-class | within 2× of best published static-CSR WCOJ systems; ≥ 20× any pointer-chasing GDBMS |
| LDBC SNB Interactive SF-100 | full compliance run; throughput ≥ 3× Neo4j, ≥ 1.5× best published embedded engine on reference class |
| LDBC Graphalytics (BFS/PR/WCC/CDLP/LCC/SSSP) | within 1.5× of dedicated static analytics engines *on transactional storage* (the Sortledton bar, beaten) |
| LDBC FinBench (write-heavy, temporal-flavored) | publishable leadership run |
| Time-travel query overhead (KEEP ALL, AS OF recent) | current-time OLTP degradation < 8%; AS OF within anchor window < 2.5× current-time cost (beat AeonG's 2.57× *ratio to specialized systems* framing outright) |
| Vector: 10M × 768-d f32, HNSW | ≥ 20k QPS @ recall ≥ 0.95 (k=10) across cores; insert-to-searchable < 100 ms (the freshness gate) |
| Ripple view maintenance | delta application ≥ 1M input-changes/s per circuit worker; subscription end-to-end p99 < 10 ms |
| Branch create / snapshot open | O(1), < 100 µs |
| Recovery (clean shutdown / crash @ 1 TB) | < 1 s / < 30 s to first query (anchor-mapped, capsule tail replay) |

Method: every gate has a bench binary, a committed baseline, a variance budget, and a flamegraph artifact on regression. Performance work follows the asupersync "How We Made It Fast" discipline: profile → remove one contention/allocation → re-verify determinism and cancel-correctness → commit with evidence. Four standing laws bind every published number: (1) *no benchmark-only semantics* — durability, isolation, and result consumption match the declared production mode; (2) *distributions, not averages* — p50/p95/p99/p99.9 and worst hot-key behavior are always reported; (3) *never hide compaction* — foreground latency during compaction, checkpoint, GC, and index build is part of the result; (4) *memory is a first-class metric* — bytes per live edge include versions, indexes, witnesses, and allocator slack, not just payload.

---

## 18. Workspace Layout & the Build-It-Ourselves Inventory

### 18.1 Crates (`fgdb-*` workspace, layered; Cargo-enforced boundaries per frankensqlite)

| Layer | Crates |
|---|---|
| Foundation | `fgdb-types` (VId/EId/CommitSeq/values/errors — newtype discipline), `fgdb-codec` (varint, EF, bitpack, StreamVByte, roaring-like bitmaps, snappy), `fgdb-sketch` (HLL, KLL, CountMin, Theta), `fgdb-collections` (raw vectorized hash tables, ART, succinct rank/select), `fgdb-crypto` (BLAKE3, XXH3, XChaCha20-Poly1305, Argon2id — vectors-tested), `fgdb-evidence` (certificates, witnesses, decision cards, redaction profiles — one envelope for every proof-bearing artifact) |
| Chronicle | `fgdb-ecs` (objects, SymbolRecords, manifest), `fgdb-chronicle` (capsules, markers, WriteCoordinator, recovery, scrubber, compaction), `fgdb-branch` |
| Strata | `fgdb-strata` (tiers, seal/compact, vertex directory), `fgdb-props` (columnar chunks), `fgdb-buffer` (extents, swizzling, ARC, io) |
| Txn | `fgdb-txn` (MVCC, Graph-SSI, merge ladder, intent logs) |
| Loom | `fgdb-gql` + `fgdb-cypher` (lexers/parsers/ASTs), `fgdb-algebra`, `fgdb-planner` (stats, DP, adaptivity), `fgdb-exec` (vectors, factorization, FreeJoin, path ops, SIMD kernels), `fgdb-linalg` (masked-semiring SpMV/SpMSpV kernels over sealed runs), `fgdb-datalog` |
| Ripple | `fgdb-ripple` (Z-sets, circuits, incrementalizer), `fgdb-views`, `fgdb-subs` |
| Beacon | `fgdb-index-core`, `fgdb-btree`, `fgdb-fts`, `fgdb-vector`, `fgdb-pathidx` |
| Prism | `fgdb-prism` (SnapshotGraphView, fnx bridge, native kernels) |
| Surface | `fgdb` (embedded API), `fgdb-server`, `fgdb-protocol` (FGP), `fgdb-bolt`, `fgdb-formats`, `fgdb-cli`, `fgdb-python` |
| Aegis | `fgdb-raft`, `fgdb-repl` |
| Verification | `fgdb-sim`, `fgdb-reference` (executable semantics oracle), `fgdb-oracles`, `fgdb-bench`, `fgdb-conformance`, `fgdb-fuzz` |

Plus the governance artifacts the house style demands from day one: `AGENTS.md`, unsafe-boundary ledger, feature-universe ledger, exit-criteria contracts, proof-lane manifests, witness-ledger conventions.

### 18.2 Built in-house (because the universe is closed)

Compression codecs (EF, delta-varint, bitpacking, snappy, roaring-like), sketches, ART/radix structures, succinct rank/select, vectorized hash tables, B-tree, HNSW + quantizers, masked-semiring SpMV/SpMSpV kernels, inverted index + BM25 + Levenshtein automata, tokenizers, CSV/JSONL/Parquet-lite readers, GQL/Cypher parsers (hand-written recursive descent + Pratt — the frankensqlite parser school), the DBSP-style circuit runtime, Raft, FGP, Bolt subset, crypto primitives, bench harness. **Explicitly not built**: async runtime, scheduler, channels, TLS/QUIC/HTTP/gRPC stacks, RaptorQ, macaroons, metrics/OTel, deterministic lab, supervision — all asupersync; graph algorithms & legacy formats — all fnx. The closed-universe constraint, which sounds like an albatross, is the moat: the entire dependency surface is auditable, deterministic under lab, and owned.


---

## 19. Workstreams & Convergence Gates

No MVP. No "simple v1." Eight parallel workstreams, each specified at full strength above, sequenced only by *dependency*, converging at four gates that are integration checkpoints — not scope reductions. This is agent-swarm-shaped work: crisp crate boundaries, ledger-driven exit criteria, deterministic tests as the coordination substrate (the methodology the foundation repos were themselves built with).

| WS | Name | Contents | Depends on |
|---|---|---|---|
| W1 | Bedrock | fgdb-types/codec/sketch/collections/crypto; fgdb-ecs; fgdb-sim harness bootstrapped **first** (sim-first means the lab VFS exists before the first fsync); `fgdb-reference` executable-semantics oracle alongside it (the oracle exists before the first optimized line) | — |
| W2 | Chronicle+Txn | capsules/markers/WriteCoordinator/recovery/scrubber/compaction; MVCC/SSI/merge ladder; retention tiers; branches | W1 |
| W3 | Strata | delta tier, sealed runs, vertex directory, property chunks, buffer manager, bulk loader | W1 (interfaces co-designed with W2) |
| W4 | Loom | parsers → algebra → planner/stats → executor (FreeJoin, factorization, path ops) | W3 read path |
| W5 | Ripple | Z-set runtime, incrementalizer, views, subscriptions, stats/index feeders | W2 stream, W4 algebra |
| W6 | Beacon | index-core lifecycle, btree/hash, adjacency views, FTS, vector, path indexes | W3, W5 feeds |
| W7 | Prism (+fnx upstream) | SnapshotGraphView, procedure surface, native kernels; upstream GraphView-genericity PRs to fnx | W3 |
| W8 | Fabric+Warden+Aegis | embedded API, FGP/HTTP/gRPC/WS, Bolt subset, macaroon enforcement, encryption; raft, replication, multi-writer, seeding | W2–W6 |

**The Genesis slice.** The first cross-workstream integration target (inside W1–W4, well before G1) is a single vertical slice exercising *final abstractions end to end*: GQL `MATCH`/`CREATE` → parser/binder under a SemanticProfile → GLA → planner → one traversal plan and one FreeJoin plan → Strata cursors over Tiers I/D/R → intent log + refinable witnesses → Graph-SSI validation → merge ladder → two-fsync capsule/marker commit → crash-replay recovery → plan certificate — all running under `fgdb-sim` with DPOR on the interleavings, differential-checked against `fgdb-reference`. Narrow in workload, complete in architecture: a slice that stubs the transaction model or bypasses the algebra is not a slice, it is a prototype, and prototypes are prohibited (constraint #9).

**Gate G1 — "The Engine Lives":** embedded fgdb; full GQL/Cypher pattern surface on Strata; serializable txns with merge ladder; recovery torture green; openCypher TCK green (documented exceptions); LDBC SNB-I SF-10 clean; determinism contracts enforced; entire suite runs under sim with DPOR on core interleavings.

**Gate G2 — "One Version Universe":** time-travel + retention tiers + branches + branch-merge; Ripple views/subscriptions with watermark consistency; Beacon complete including transactional HNSW; Prism catalog live; hybrid.search operator; SNB SF-100 + Graphalytics gates from §17 hit.

**Gate G3 — "Verified & Networked":** fgdbd with FGP/HTTP/gRPC/Bolt; Warden capabilities + at-rest encryption; Raft replication + bonded seeding + PITR; Elle-class oracle campaigns and formal anchors (§15.5) landed; FinBench run.

**Gate G4 — "Leapfrog, Published":** every §17 gate green and public; multi-writer replication; conformance matrices (GQL feature IDs, Cypher dialect) published; the three papers (§15.7) drafted; sharding design doc frozen against the implemented partition substrate; 1.0.

---

## 20. Risks & Mitigations

| Risk | Reality check | Mitigation |
|---|---|---|
| Scope: this is a multi-hundred-KLOC system | True — and smaller than frankensqlite, on stronger foundations | Agent-swarm methodology with ledger-driven exit criteria (proven twice in-family at this scale); sim-first testing collapses the debugging tail that normally dominates DB schedules; crate boundaries sized for parallel agents |
| Free-Join + factorization + adaptivity interplay is the hardest novel engineering | Correct — this is the crown-jewel risk | Build order inside W4: binary FreeJoin → run-trie WCOJ → factorized columns → adaptivity; each stage independently benchmarked & certified; Kùzu's and Free Join's published artifacts are behavioral references |
| Graph-SSI phantom design is publish-grade novelty (i.e., it could be wrong) | Yes | Three-level epochs are *conservatively sound* (false positives only) by construction; Elle-class oracles + DPOR sims hunt the soundness bugs; Lean anchor for the block-level rule; SNAPSHOT fallback contains blast radius |
| HNSW under MVCC/compaction (delete churn, recall drift) | Known-hard industry-wide | Segment lifecycle + visibility filtering is the same pattern as Tier R (one mechanism to trust); recall regression gates in CI; exact re-rank from Embedding columns bounds worst case |
| Adaptive tiering thrashes (representation churn eats its own benefit) | The classic failure mode of every adaptive store | Decision cards with hysteresis: minimum dwell time, benefit must exceed conversion cost + uncertainty, per-descriptor cooldown; pinned deterministic fallback policy under lab; thrash rate is a gated §17 metric |
| Witness refinement becomes a second query (cost eats the abort savings) | Possible on pathological predicates | Refinement is budgeted with conservative-abort fallback; `WitnessTrace` is a byproduct of normal execution (no re-execution); refined-vs-saved accounting on every decision card; the knob defaults conservative |
| Nightly Rust | Both foundations already pin nightly; MSRV churn is real | Pinned toolchain per repo (house pattern), portable-SIMD isolated behind `fgdb-codec`/kernel traits with scalar fallbacks |
| fnx algorithms not yet trait-generic where we need them | Verified: `GraphView` exists with integer rows, coverage is partial | W7 upstream workstream; materialization path with certificate badge as the always-correct fallback; native kernels for the hot five families regardless |
| RaptorQ CPU cost on the write path | Encode is cheap relative to fsync at our capsule sizes; still, budgets | Systematic symbols mean the no-loss read path is a straight copy; encode off the critical section (protocol step 5–6); overhead knob per object class; measured in every bench run |
| Kùzu's fate as a cautionary tale (great engine, no oxygen) | The sharpest non-technical risk | FrankenGraphDB is an open-source ecosystem asset, not a venture bet: it monetizes indirectly (consulting, hedge-fund tooling, the skills business), its dependency universe is self-owned, and its differentiators (B1/B4/B5/B6) target the 2026 demand that actually pays — agent memory, auditability, governance |

---

## Appendix A — On-Disk Object Formats (normative sketches)

**SymbolRecord** (physical atom, inherited shape):
```
┌────────┬─────┬───────────┬─────┬─────┬─────────────┬───────┬───────────┐
│ "FGEC" │ ver │ ObjectId  │ OTI │ ESI │ symbol data │ XXH3  │ auth tag? │
│ 4B     │ u8  │ [u8;16]   │ var │ u32 │ [u8;T]      │ u64   │ [u8;16]   │
└────────┴─────┴───────────┴─────┴─────┴─────────────┴───────┴───────────┘
```

**AdjRunSegment** payload:
```
header { magic, version, graph, branch_epoch, (src_label, etype, dst_label),
         src_ordinal_range, edge_count, seq_range, flags(dense-ranges|holes),
         stats digest, checksum }
offsets   : Elias–Fano monotone sequence over prefix-degree sums
neighbors : per-list [ dense-interval runs ]* + [ SIMD delta-varint gaps ]*
holes     : optional roaring-like bitmap over run positions
propmap   : run-position → property-chunk row locators (implicit stride + exceptions)
```

**CommitCapsule** payload: `{ snapshot_basis, branch, schema_epoch, topology_epoch, intent_log (App. B), block_deltas[], read_set_digest, write_set_summary, ssi_witnesses, witness_ledger_refs }` — deterministic encoding (canonical field order, length-prefixed) so capsule ⇒ ObjectId is reproducible across replicas.

**CommitMarker** (~100 B): `{ commit_seq, capsule_oid, prev_marker_oid, branch, schema_epoch, topology_epoch, txn_token, wallclock, flags, chain_hash }` — the hash chain gives tamper-evidence independent of storage.

**BranchManifest**: `{ branch_id, parent, fork_seq, head_marker_oid, dek_wrap?, policy }`.

## Appendix B — Graph Intent Log (the semantic vocabulary)

```
CreateVertex   { tmp_ref, label_set, props }
DeleteVertex   { vid, cascade_policy }
AddLabel/RemoveLabel { vid, label }
AddEdge        { src, etype, dst, key?, props }        // key allocated if absent
RemoveEdge     { src, etype, dst, key }
SetProp        { elem, name, value, merge: Set|LWW|CRDTCounter|Max|Min }
RemoveProp     { elem, name }
EnsureEdge     { src, etype, dst, unique_by, props }    // idempotent upsert on a uniqueness key
CompareAndSet  { elem, name, expected, value }          // mismatch fails the intent, not the txn
SetInsert      { elem, name, element }                  // set-semantics collection ops
SetRemove      { elem, name, element }
SetValidTime   { elem, from?, to? }
AssertSameAs   { a, b, confidence, source }            // entity-resolution intent
BulkRunRef     { sealed_run_oid, count }               // bulk-load mega-commit
SchemaOp       { ... }                                  // DDL as intents
```
Commutativity table (per op-pair, per rung-1 rules) is a versioned artifact with its own property tests — the merge ladder's soundness is *data*, reviewable and lockable, not folklore. The CRDT-flavored `merge:` modes are what make agent-swarm branches merge at high rates without semantic surprises. Every intent additionally carries its captured read footprint (exact where small; conservative-with-refinement-handle otherwise, §7.3) and a structural-effects bitmask — the exact inputs the merge ladder's eligibility check consumes.

## Appendix C — GLA Operator Inventory (executor contracts)

Each operator declares: input/output factorization shape, determinism class, memory budget behavior (bounded | spillable), cancellation drain point, incremental derivative (for Ripple), `WitnessTrace` emission contract, and path-semantics preservation obligations. Inventory: `ScanVertices, ScanEdges, IndexProbe{btree,hash,fts,vector,path}, Expand{quantified}, VarLengthExpand, PathFind{shortest,cheapest,k}, SemiringMxV{push,pull}, FreeJoin, HashJoin(degenerate FreeJoin), Select, Project, Unnest/Flatten, Aggregate{hash,ordered,factorized}, Distinct, OrderBy{topk}, Limit, Union, Fixpoint, TemporalSlice, TemporalJoin, Mutate, ViewDelta, CertEmit`.

## Appendix D — FGP Frame Sketch

`HELLO{versions,features,auth} · AUTH{macaroon} · PREPARE{text,dialect} · EXECUTE{stmt,params,consistency{seq?,branch,asof?},determinism,fetch} · RESULT_SCHEMA · RESULT_FRAME{flat|factorized, columnar} · CERTIFICATE · SUBSCRIBE / DELTA_FRAME{zset} · CANCEL{query} · COMMIT_TOKEN{seq} · FEC_BEGIN{oti}/FEC_SYMBOL · ERROR{code,conflict_report?} · GOODBYE`. All frames length-delimited; big transfers may switch to ATP bonded mode by negotiation.

## Appendix E — Working Bibliography (anchors reviewed for this plan)

Storage: Sortledton (PVLDB 15(6), 2022) · Teseo (PVLDB 14(6), 2021) · LiveGraph (PVLDB 13(7), 2020) · Aspen (PLDI 2019) · GraphOne (FAST 2019) · GTX / Spruce / RapidStore (2024–25) · RadixGraph (2026) · "Revisiting In-Memory Dynamic Graph Storage" (2025). Processing: Kùzu (CIDR 2023) + "Columnar Storage and List-based Processing" (PVLDB 14(11)) + A+ Indexes · NPRR/AGM & Leapfrog Triejoin · EmptyHeaded (SIGMOD 2016) · Free Join (SIGMOD 2023) · ADOPT (2023) · HoneyComb (2025) · Umbra (CIDR 2020) · LeanStore · morsel-driven parallelism (SIGMOD 2014) · SPJM converged optimization (2024). Incremental: DBSP (PVLDB 16 / VLDBJ 2025, Lean-formalized; Feldera) · Differential/Timely Dataflow · streaming-graph SGQ work. Temporal: AeonG (PVLDB 17(6), 2024; VLDBJ 2025) · SQL:2011 system time · T-GQL, Clock-G. Languages: ISO/IEC 39075:2024 (GQL) · ISO/IEC 9075-16:2023 (SQL/PGQ) · GQL/PGQ pattern-matching & expressive-power papers · path-multiset representations (Martens et al.) · openCypher TCK. Vector: HNSW (Malkov & Yashunin) · DiskANN/Vamana · 2025–26 fresh-ANN and disaggregated-HNSW literature. Verification: FoundationDB simulation testing · Elle (Kingsbury & Alvaro) · Cahill/Fekete SSI · Serial Safety Net (Wang et al.) · asupersync formal-semantics & proof-lane artifacts. Distributed alternatives (evaluated, not adopted — §3.4, §14): Weaver refinable timestamps · G-Tran RDMA MV-OCC. Benchmarks: LDBC SNB / Graphalytics / FinBench. Market context (2026): GQL adoption in Spanner Graph & Fabric; Kùzu wind-down and forks; GraphRAG/agent-memory adoption analyses and their governance/freshness gap findings.

## Appendix F — Invariant Registry (excerpt)

Every load-bearing invariant carries a stable ID; the full registry lives as `invariants.toml` in-repo, and CI cross-checks that each ID has a live checker (Lean lane, TLA+ model, runtime oracle, property test, or CI gate). An excerpt of the spine:

| ID | Invariant | Enforcement |
|---|---|---|
| FG-INV-01 | TxnIds strictly monotonic (INV-1) | Lean + runtime assert |
| FG-INV-02 | At most one holder of any block lock (INV-2) | Lean + lock-table oracle |
| FG-INV-03 | Version chains strictly newer-first (INV-3) | Lean + arena debug oracle |
| FG-INV-04 | No read observes `seq > snapshot.high` | continuous in-sim SI oracle |
| FG-INV-05 | No committed SSI dangerous structure | trace-reconstructed rw-graph oracle + Lean (block rule) |
| FG-INV-06 | Witness refinement is monotone: refined ⊆ coarse, never drops a real access | property tests + Lean lemma |
| FG-INV-07 | A durable marker implies a decodable capsule (FSYNC₁ ordering) | crash-point matrix + TLA+ commit model |
| FG-INV-08 | Marker chain hash continuity | recovery check + `fgdb doctor` |
| FG-INV-09 | ObjectId ≡ content (recompute-verify) | scrubber + decode proofs |
| FG-INV-10 | Manifest closure: every referenced object present or recoverable | doctor + scrub campaigns |
| FG-INV-11 | Adjacency incidence ⇔ edge-record coherence (fwd/rev/holes) | model-based storage tests + compaction metamorphic suite |
| FG-INV-12 | Every representation transition (seal/compact/promote/demote) preserves the canonical logical digest | digest validator at every publish |
| FG-INV-13 | Commits immutable; branch heads move only by atomic publish | TLA+ branch model + runtime assert |
| FG-INV-14 | No object reachable from a lease/branch/backup root is reclaimed | GC root-accounting oracle |
| FG-INV-15 | View watermark monotone; view@w ≡ base-query@w | Ripple differential harness |
| FG-INV-16 | All obligations resolve before region close | asupersync leak oracle (inherited) |
| FG-INV-17 | Rebase replay is deterministic (capability-stripped `Cx`; same inputs ⇒ same digest) | type-level + replay tests |
| FG-INV-18 | Derived structures are never more authoritative than the commit stream | recovery discard-and-rebuild drills |
| FG-INV-19 | `replay(certificate, seq, seed)` ⇒ byte-identical result | certificate replay CI gate |
| FG-INV-20 | Capability caveats apply before expansion — no post-filter security | planner conformance suite + Warden tests |

---

*Fin. The graph database the 2030s deserve, built from parts the 2020s already proved — ours.*
