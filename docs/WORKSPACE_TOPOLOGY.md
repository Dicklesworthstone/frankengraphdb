<!-- GENERATED FILE — DO NOT EDIT.
     Source: registries/workspace_topology.toml
     Regenerate: cargo run -p registry-check --bin topology-check -- --root . --write
     Owner bead: fgdb-g0-workspace-topology-1q9m -->

# Workspace Topology — Crates, Layers, and the Build-Versus-Consume Inventory

This document is generated from `registries/workspace_topology.toml` and checked byte-exact in CI. The registry is the master; this file is its rendering. Every plan excerpt below is embedded verbatim under an `fnv1a64` pin, so plan drift turns the gate red rather than silently invalidating the map.

* **Layers:** 14
* **Crates:** 70 (20 active, 49 planned, 1 reserved)
* **Inventory rows:** 51 (23 build-here, 19 consume-from, 9 design-only)
* **Replay:** `cargo run -p registry-check --bin topology-check -- --root .`
* **Constraints bound:** FG-CON-01, FG-CON-02

## What `activation_status` means

`active` — the crate exists in the workspace with its first real final-abstraction slice. `planned` — the row is frozen and the directory **must not exist**; the checker fails on a planned crate with a directory just as it fails on a directory with no row. `reserved` — named by the plan as belonging to a later workstream only (`fgdb-shard`, W12).

## Layers and legal dependency direction

| # | Layer | Id | May depend on | Charter |
|---|---|---|---|---|
| 1 | Foundation | `foundation` | `foundation`, `unsafe_islands` | Canonical value/type/claim/policy/resource vocabulary, in-house codecs, sketches, collections, crypto, calibration, and evidence envelopes. No I/O, no durable state machines. |
| 2 | Unsafe islands | `unsafe_islands` | `foundation`, `unsafe_islands` | The only crate roots not using forbid(unsafe_code); every site is ledgered and exposed only through safe APIs. |
| 3 | Chronicle | `chronicle` | `chronicle`, `foundation`, `unsafe_islands` | The append-only content-addressed commit substrate: identity/encoding/bootstrap, durable order, roots and capsules, branches, key management, audit, backup. |
| 4 | Strata | `strata` | `chronicle`, `foundation`, `strata`, `unsafe_islands` | Graph-structured LSM storage: label-independent tiers, property storage, buffer pool, query scratch. |
| 5 | Txn + secure access | `txn_secure_access` | `chronicle`, `foundation`, `strata`, `txn_secure_access`, `unsafe_islands` | MVCC/Graph-SSI, constraint enforcement, and the sole authorized storage/permit facade. Depends on Chronicle + Strata + the foundation policy verifier. |
| 6 | Loom | `loom` | `chronicle`, `foundation`, `loom`, `strata`, `txn_secure_access`, `unsafe_islands` | Syntax, binder, algebra, planner, executor, linear algebra, and Datalog. All reads flow through fgdb-secure-view. |
| 7 | Ripple | `ripple` | `chronicle`, `foundation`, `loom`, `ripple`, `strata`, `txn_secure_access`, `unsafe_islands` | Z-set delta algebra, circuits, incrementalizer, materialized views, subscriptions. |
| 8 | Beacon | `beacon` | `beacon`, `chronicle`, `foundation`, `loom`, `ripple`, `strata`, `txn_secure_access`, `unsafe_islands` | Rebuildable secondary indexes: B-tree, full-text, vector ANN, path/connectivity. |
| 9 | Prism | `prism` | `beacon`, `chronicle`, `foundation`, `loom`, `prism`, `ripple`, `strata`, `txn_secure_access`, `unsafe_islands` | The authorized projection bridge to franken_networkx: cursor/cache/materialization paths and native kernels. |
| 10 | Warden | `warden` | `beacon`, `chronicle`, `foundation`, `loom`, `prism`, `ripple`, `strata`, `txn_secure_access`, `unsafe_islands`, `warden` | Capability issuance/revocation/discharges, policy administration, privacy and redaction. The verifier IR it administers lives in foundation, which is why no lower layer needs to depend on Warden. |
| 11 | Surface/operations | `surface_operations` | `beacon`, `chronicle`, `foundation`, `loom`, `prism`, `ripple`, `strata`, `surface_operations`, `txn_secure_access`, `unsafe_islands`, `warden` | FGP state machine, Bolt subset, formats, the deterministic UDF VM, observatory, and the self-describing system graph. |
| 12 | Aegis | `aegis` | `aegis`, `beacon`, `chronicle`, `foundation`, `loom`, `prism`, `ripple`, `strata`, `surface_operations`, `txn_secure_access`, `unsafe_islands`, `warden` | Multi-member consensus over the Chronicle order core, payload availability, anti-entropy, reconfiguration, fenced GC. |
| 13 | Composition | `composition` | `aegis`, `beacon`, `chronicle`, `composition`, `foundation`, `loom`, `prism`, `ripple`, `strata`, `surface_operations`, `txn_secure_access`, `unsafe_islands`, `warden` | The three postures plus the two packaging boundaries. Composition crates are entry points: nothing below them may depend on them. |
| 14 | Verification | `verification` | `aegis`, `beacon`, `chronicle`, `composition`, `foundation`, `loom`, `prism`, `ripple`, `strata`, `surface_operations`, `txn_secure_access`, `unsafe_islands`, `verification`, `warden` | Simulation, the executable reference oracle, consistency oracles, benches, conformance corpora, fuzzers. Never shipped in any posture closure. |

The allowed set is **derived**, not declared: `allowed(L) = { M : M.source_order <= L.source_order } ∪ { unsafe_islands }`, and the checker recomputes every row. The union term is the single registered exception to table order — The unsafe islands are leaf infrastructure: foundation codec/crypto/collections kernels consume them (per fgdb-w1-unsafe-ledger-icp's own consumer list), and they consume foundation types. Crate-level acyclicity is enforced separately and unconditionally.

## The crate map

Exactly three crates may carry `deny_ledgered`; every other row carries `forbid`, and every ACTIVE forbid-crate root is scanned for the attribute and for any attempt to lower it. The same three islands are rostered in [`registries/unsafe_boundary_ledger.toml`](registries/unsafe_boundary_ledger.toml), which owns the site-level ledger and the site↔allow bijection; this map owns the crate-level policy column. The two rosters are checked as a bijection with agreeing statuses, so neither registry can drift from the other.

### 1. Foundation

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-types` | active | `forbid` | all | W1 | fgdb-w1-foundation-types-tjk | Core identity newtypes, bounded bytes, the typed reference family, and the canonical scalar value union. |
| 2 | `fgdb-bigint` | active | `forbid` | all | W1 | fgdb-w1-foundation-types-tjk | canonical exact integers |
| 3 | `fgdb-delta-types` | active | `forbid` | all | W1 | fgdb-w1-foundation-types-tjk | G0/W2 delta schema only |
| 4 | `fgdb-claim` | active | `forbid` | all | W1 | fgdb-w1-foundation-types-tjk | The claim-type constitution as a type system: the six registry claim classes and the lattice law as a compile error. |
| 5 | `fgdb-authz-types` | planned | `forbid` | all | W1 | — | Authority, permit, and security-binding value types shared by the secure view and Warden. |
| 6 | `fgdb-policy` | planned | `forbid` | all | W1 | — | restricted verifier IR |
| 7 | `fgdb-resource` | active | `forbid` | all | W1 | fgdb-w1-resource-ledger-contract-ym2g | Typed resource accounting and escrow vocabulary. |
| 8 | `fgdb-codec` | active | `forbid` | all | W1 | fgdb-w1-codecs-3x8 | In-house compression kernels for the registered durable codec layer. |
| 9 | `fgdb-sketch` | active | `forbid` | all | W1 | fgdb-w1-sketch-calibrate-tpj | Deterministic frequency, degree, and cardinality summaries. |
| 10 | `fgdb-collections` | active | `forbid` | all | W1 | fgdb-w1-collections-lcg | ART/radix structures, succinct rank/select, and vectorized hash tables as safe scalar kernels. |
| 11 | `fgdb-crypto` | active | `forbid` | all | W1 | fgdb-w1-crypto-y5o | Keyed identity, AEAD/KDF profiles, and the primitives no audited foundation supplies. |
| 12 | `fgdb-calibrate` | active | `forbid` | all | W1 | fgdb-w1-sketch-calibrate-tpj | Identity-bound calibration wrappers over the runtime's e-process/conformal machinery. |
| 13 | `fgdb-evidence` | active | `forbid` | all | W1 | fgdb-w1-foundation-types-tjk | Evidence envelopes and the disclosure fields every statistical claim must carry. |

### 2. Unsafe islands

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-unsafe-simd` | active | `deny_ledgered` | all | W1 | fgdb-w1-unsafe-islands-eqrq | SIMD/vector kernels with bit-identical scalar fallbacks. |
| 2 | `fgdb-unsafe-arena` | active | `deny_ledgered` | all | W1 | fgdb-w1-unsafe-islands-eqrq | Bump/region arena internals and generational-handle plumbing behind safe APIs. |
| 3 | `fgdb-unsafe-vfs` | active | `deny_ledgered` | all | W1 | fgdb-w1-unsafe-islands-eqrq | Raw file/mapping syscall surfaces beneath the filesystem-profile layer. |

### 3. Chronicle

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-ecs` | planned | `forbid` | all | W2 | — | identity/encoding/bootstrap/object locator |
| 2 | `fgdb-order` | planned | `forbid` | all | W2 | fgdb-w2-order-raft-0a90 | durable Raft log/state core and quorum-one driver |
| 3 | `fgdb-chronicle` | active | `forbid` | all | W2 | fgdb-w2-object-identity-t0f | capsule/marker/logical-local roots, allocator, outcomes, checkpoints/retention, recovery/GC/scrub |
| 4 | `fgdb-branch` | planned | `forbid` | all | W2 | — | Branch fork, merge, grants, and retirement over the shared commit stream. |
| 5 | `fgdb-keymgr` | planned | `forbid` | all | W2 | — | Key-envelope DAG, rotation, and the two-stage key lifecycle. |
| 6 | `fgdb-audit` | planned | `forbid` | all | W2 | — | The durable audit stream, its admission gates, and visibility pipeline. |
| 7 | `fgdb-backup` | planned | `forbid` | all | W2 | — | Local backup, receipt-first restore, and lease-fenced anchor shipping. |

### 4. Strata

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-strata` | active | `forbid` | all | W3 | fgdb-w3-tier-d-ctj | label-independent tiers, seal/compact, stable-ID directory |
| 2 | `fgdb-props` | planned | `forbid` | all | W3 | — | Property chunk storage, typed columns, and overflow promotion. |
| 3 | `fgdb-buffer` | planned | `forbid` | all | W3 | — | The MVCC-aware ARC buffer pool and aligned direct-I/O buffers. |
| 4 | `fgdb-scratch` | planned | `forbid` | all | W3 | — | Query-scoped spill scratch under bounded resources. |

### 5. Txn + secure access

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-txn` | planned | `forbid` | all | W4 | fgdb-w4-g1-txn-core-qpmg | MVCC, Graph-SSI/witness lifecycle, coordinator, final-effect merge ladder |
| 2 | `fgdb-constraints` | planned | `forbid` | all | W4 | — | branch-scoped canonical enforcement |
| 3 | `fgdb-secure-view` | planned | `forbid` | all | W4 | — | sole authorized storage/permit facade |

### 6. Loom

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-gql` | active | `forbid` | all | W5 | fgdb-w5-parsers-nje | syntax only |
| 2 | `fgdb-cypher` | planned | `forbid` | all | W5 | fgdb-w5-parsers-nje | syntax only |
| 3 | `fgdb-bind` | planned | `forbid` | all | W5 | fgdb-w5-binder-bt5 | Name resolution, typing, and parameter binding for both surfaces. |
| 4 | `fgdb-algebra` | planned | `forbid` | all | W5 | fgdb-w5-gla-algebra-lj2 | The complete graph logical algebra and its answer contracts. |
| 5 | `fgdb-planner` | planned | `forbid` | all | W5 | fgdb-w5-planner-tvi | Cost-based planning, statistics, and plan certificates. |
| 6 | `fgdb-exec` | planned | `forbid` | all | W5 | fgdb-w5-executor-olp | The vectorized, morsel-parallel, spillable factorized executor. |
| 7 | `fgdb-linalg` | planned | `forbid` | all | W5 | — | Masked-semiring SpMV/SpMSpV kernels over Strata runs. |
| 8 | `fgdb-datalog` | planned | `forbid` | all | W5 | — | The validated recursive/Datalog profile over the same algebra. |

### 7. Ripple

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-ripple` | planned | `forbid` | all | W6 | — | Z-sets, circuits, incrementalizer |
| 2 | `fgdb-views` | planned | `forbid` | all | W6 | — | Policy-labeled materialized table and graph views. |
| 3 | `fgdb-subs` | planned | `forbid` | all | W6 | — | Standing queries, subscriptions, resets, and resume cursors. |

### 8. Beacon

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-index-core` | planned | `forbid` | all | W7 | — | Shadow build, validate, activate, and watermark/tail-correctness shared by every index. |
| 2 | `fgdb-btree` | planned | `forbid` | all | W7 | — | The in-house B-tree property index. |
| 3 | `fgdb-fts` | planned | `forbid` | all | W7 | — | Segment-based inverted index, BM25, tokenizers, and Levenshtein automata. |
| 4 | `fgdb-vector` | planned | `forbid` | all | W7 | fgdb-w7-vector-79hu | HNSW generations, quantizers, and the IVF-PQ cold tier. |
| 5 | `fgdb-pathidx` | planned | `forbid` | all | W7 | — | 2-hop/landmark + the persistent-union-find temporal-connectivity index, §10.7 |

### 9. Prism

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-prism` | planned | `forbid` | all | W8 | — | authorized projection bridge, fnx cursor/cache/materialization paths, native kernels |

### 10. Warden

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-warden` | planned | `forbid` | all | W9 | — | issuance/revocation/discharges/policy admin |
| 2 | `fgdb-privacy` | planned | `forbid` | all | W9 | — | Differential-privacy state, budgets, and the irreversible-before-access rule. |
| 3 | `fgdb-redaction` | planned | `forbid` | all | W9 | — | Ticket-gated redaction, erasure cuts, and rollback reconciliation. |

### 11. Surface/operations

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-protocol` | planned | `forbid` | all | W10 | — | FGP state machine |
| 2 | `fgdb-bolt` | planned | `forbid` | all | W10 | — | The Bolt-compat subset with exact epoch/status parity. |
| 3 | `fgdb-formats` | planned | `forbid` | all | W10 | — | Import/export profiles under explicit size, recursion, schema, and decompression limits. |
| 4 | `fgdb-udf-vm` | planned | `forbid` | all | W10 | — | The deterministic UDF bytecode VM. |
| 5 | `fgdb-observatory` | planned | `forbid` | all | W10 | — | Metrics, decision cards, replay grades, and operator-facing diagnostics. |
| 6 | `fgdb-system-graph` | planned | `forbid` | all | W10 | — | The database's own runtime exposed as a read-only temporal property graph. |

### 12. Aegis

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-raft` | planned | `forbid` | all | W11 | — | multi-member protocol over `fgdb-order` |
| 2 | `fgdb-repl` | planned | `forbid` | all | W11 | — | payload availability, anti-entropy, reconfiguration, fenced GC |
| 3 | `fgdb-shard` | reserved | `forbid` | all | W12 | — | future `fgdb-shard` belongs only to W12 |

### 13. Composition

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb` | active | `forbid` | entry_embedded | W10 | fgdb-j0vu | embedded API |
| 2 | `fgdb-server` | planned | `forbid` | entry_server | W10 | — | top-level Fabric+Warden+Aegis composition |
| 3 | `fgdb-cli` | planned | `forbid` | entry_cli | W10 | — | The CLI binary with robot mode and a human mode. |
| 4 | `fgdb-python` | planned | `forbid` | packaging_boundary | W10 | — | allowed fnx-python packaging boundary only |
| 5 | `fgdb-adbc` | planned | `forbid` | packaging_boundary | W10 | — | C-ABI ADBC packaging at the same boundary; §13.7 |

### 14. Verification

| # | Crate | Status | Unsafe | Posture | Owner | Bead | Role |
|---|---|---|---|---|---|---|---|
| 1 | `fgdb-sim` | active | `forbid` | test_only | W1 | fgdb-verif-sim-q97e | The whole database under the lab runtime: virtual time, DPOR, chaos, crashpacks. |
| 2 | `fgdb-reference` | active | `forbid` | test_only | W1 | fgdb-verif-reference-3kkp | executable semantics oracle |
| 3 | `fgdb-oracles` | planned | `forbid` | test_only | verification_ladder | fgdb-verif-oracles-fqie | SI/SSI, obligation-leak, quiescence, and Elle-class history oracles. |
| 4 | `fgdb-bench` | planned | `forbid` | test_only | verification_ladder | — | The §17 bench harness: one binary per gate with committed baselines. |
| 5 | `fgdb-conformance` | planned | `forbid` | test_only | verification_ladder | — | openCypher TCK and the GQL feature-conformance corpus keyed to ISO feature IDs. |
| 6 | `fgdb-fuzz` | planned | `forbid` | test_only | verification_ladder | — | Grammar, frame, format-reader, and SymbolRecord fuzz targets. |

## The three postures

| Posture | Entry crate | Binary | Status | Deferred to | Anchor |
|---|---|---|---|---|---|
| Embedded library | `fgdb` | — | live | — | §1 constraint 5(a), §13.1 |
| Server binary | `fgdb-server` | fgdbd | deferred | the owner bead of fgdb-server (W10 composition) | §1 constraint 5(b), §13 |
| CLI binary | `fgdb-cli` | fgdb | deferred | the owner bead of fgdb-cli (W10 composition) | §1 constraint 5(c) |

A posture closure is the transitive dependency set of its entry crate over the LIVE graph. A `test_only`, `packaging_boundary`, or foreign-entry crate inside a shipped closure is a violation. While every entry crate is `planned` the law reports `deferred` — never `pass` — and the closure evaluator is proved against synthetic graphs in the suite instead.

## The legal external universe

| Project | Linkage | Pinned revision | Package prefixes | Default features | Anchor |
|---|---|---|---|---|---|
| asupersync | linked | `c17e51931f3223d55bd4961ff13eb3c5c4022fdf` | `asupersync` | must be disabled (workspace_convention_unanimous) | §2.1 |
| franken_networkx (fnx-*) | linked | `9d710b1c33e99412c94de7fa4de2f7ce4954110f` | `fnx-` | permitted (plan_permits_defaults: FG-CON-01 admits the fnx-* crates and, for the Python package, their already-pinned transitive binding runtime — the exact thing a blanket default-features=false rule would break) | §2.2 |
| frankensqlite (design-level only) | design_only | — | `fsqlite-` | permitted (not_applicable: nothing links it) | §2.3 |

| Forbidden dependency | Selector | Prefix | Why |
|---|---|---|---|
| `no-linked-frankensqlite` | all_crates | `fsqlite-` | frankensqlite is the architectural donor, not a dependency: graph objects are not SQLite pages. Designs are adopted; packages are never linked. |
| `no-direct-pyo3` | all_crates | `pyo3` | The engine, server, CLI, and durable formats stay free of the Python packaging boundary. fgdb-python reaches the binding runtime transitively through fnx-python only. |
| `no-tooling-in-engine` | all_crates | `registry-check` | G0 constitutional tooling validates the registries; an engine crate depending on the validator would invert the gate. |

## Named crate-level edges

| Edge | From | To | Source phrase | Why |
|---|---|---|---|---|
| `txn-over-chronicle` | layer `txn_secure_access` | layer `chronicle` | depends on Chronicle + Strata + foundation policy verifier | The secure view cannot be a facade over storage it does not reach. |
| `txn-over-strata` | layer `txn_secure_access` | layer `strata` | depends on Chronicle + Strata + foundation policy verifier | Block-granularity MVCC needs the block store in the same closure. |
| `txn-over-policy-verifier` | layer `txn_secure_access` | crate `fgdb-policy` | foundation policy verifier | Caveats compile to mandatory planner predicates, so the verifier IR must sit BELOW the facade that enforces it — this is why fgdb-policy is a foundation crate and not a Warden crate. |
| `loom-reads-through-secure-view` | layer `loom` | crate `fgdb-secure-view` | all reads flow through `fgdb-secure-view` | FG-INV-20: authorization precedes observation. A Loom crate reaching Strata directly would be a post-filter. |
| `raft-over-order` | crate `fgdb-raft` | crate `fgdb-order` | multi-member protocol over `fgdb-order` | Multi-member consensus extends the durable order core; it does not restate it. |
| `prism-over-fnx` | crate `fgdb-prism` | foundation `franken_networkx` | fnx cursor/cache/materialization paths | Prism is the bridge; a Prism that reimplemented an algorithm would duplicate a foundation capability. |
| `python-over-fnx-python` | crate `fgdb-python` | foundation `franken_networkx` | allowed fnx-python packaging boundary only | The binding runtime is reached transitively through fnx-python, never by a direct PyO3 dependency. |
| `calibrate-over-asupersync` | crate `fgdb-calibrate` | foundation `asupersync` | wraps the runtime's e-process/conformal machinery rather than reimplementing it | Live today: the one required edge both of whose endpoints are already active, which is what keeps this law from being vacuous at G0. |

The evaluated-edge ratchet is the monotone set-floor ["calibrate-over-asupersync"]. New live edges may be observed without changing this floor; raising it is a deliberate append-only ratchet and is never required merely to keep another pane green. Once an edge is in the floor, moving it back to `deferred` fails even if another edge becomes live in the same change.

**Narrowing — `fgdb-reference`.** Layers ["foundation"], plus crates ["fgdb-gql", "fgdb-cypher"], plus foundation projects [] (§15.2). Importing any engine crate is a CI-rejected boundary violation in the unsafe-boundary-ledger style, so the differential cannot be quietly gutted by code sharing. The parser is the one recorded sharing exception.

## Build here, or consume from a foundation

Coverage of §18.2 is **proved by residue**: every phrase below is deleted from the frozen source line, and what remains must be punctuation plus the registered rationale allowances. A capability §18.2 names and this registry drops fails as leftover residue.

### Built here

| Capability | Owning crate | Note |
|---|---|---|
| Compression codecs (EF, delta-varint, bitpacking, snappy, roaring-like) | `fgdb-codec` | Adjacency and identity-column codecs are graph-shaped; no foundation ships them. |
| canonical signed-limb exact integers | `fgdb-bigint` | Canonical encoding is part of FG-INV-12 coherence, so the representation is ours. |
| sketches | `fgdb-sketch` | Graph statistics sketches (§2.4). |
| ART/radix structures | `fgdb-collections` | — |
| succinct rank/select | `fgdb-collections` | — |
| vectorized hash tables | `fgdb-collections` | — |
| B-tree | `fgdb-btree` | frankensqlite's B-tree is a design reference, not a dependency. |
| HNSW + quantizers + IVF-PQ cold tier | `fgdb-vector` | No hnswlib, ever (§1 constraint 1). |
| persistent union-find | `fgdb-pathidx` | The temporal-connectivity index of §10.7. |
| masked-semiring SpMV/SpMSpV kernels | `fgdb-linalg` | — |
| inverted index + BM25 + Levenshtein automata | `fgdb-fts` | No tantivy (§1 constraint 1); frankensqlite's FTS5 is the in-family behavioral reference. |
| tokenizers | `fgdb-fts` | Unicode word-boundary plus language-pluggable stemmers. |
| CSV/JSONL/Parquet-lite readers | `fgdb-formats` | No arrow (§1 constraint 1). Legacy GRAPH formats are a separate, consumed capability. |
| GQL/Cypher parsers (hand-written recursive descent + Pratt — the frankensqlite parser school) | `fgdb-gql` | fgdb-cypher is the second surface of the same capability; both are syntax-only. |
| the DBSP-style circuit runtime | `fgdb-ripple` | — |
| Raft | `fgdb-order` | Raft SEQUENCING is ours (§2.4); membership, anti-entropy, coded fan-out, and convergent metadata are consumed compositions. fgdb-raft extends this core to multi-member. |
| FGP | `fgdb-protocol` | The graph wire format only: every byte below it is asupersync networking. |
| Bolt subset | `fgdb-bolt` | — |
| crypto profiles/primitives not already supplied by an audited foundation | `fgdb-crypto` | The phrase is scoped by construction: what the foundation audits, we consume. |
| the MMR accumulator + transparency checkpoints | `fgdb-audit` | The verifiability accumulator of §2.4. |
| the Sextant calibration scores/ledgers (over asupersync's e-process core) | `fgdb-calibrate` | The graph-specific scores and ledgers are ours; the e-process core underneath them is consumed, which is exactly what `calibrate-over-asupersync` requires. |
| the deterministic UDF bytecode VM (§13.8) | `fgdb-udf-vm` | — |
| bench harness | `fgdb-bench` | One bench binary per §17 gate, with a committed baseline and a variance budget. |

### Consumed from a foundation

| Capability | Project | Asset (exact evidence) | Note |
|---|---|---|---|
| async runtime | asupersync | Region tree, obligations, `Outcome` lattice, three-lane scheduler | No tokio, ever. Doctrine #3 (`Cx` everywhere) is only possible because the runtime is consumed whole. |
| scheduler | asupersync | three-lane scheduler (cancel/EDF/ready) | — |
| channels | asupersync | Channels (two-phase reserve/commit MPSC, oneshot, broadcast, watch, session) | The write-coordinator mailbox is a consumed channel, not a new one. |
| TLS/QUIC/HTTP/gRPC stacks | asupersync | Networking: TCP, UDP, QUIC (native), HTTP/1.1, HTTP/2 (HPACK, flow control), HTTP/3, WebSocket, TLS (rustls-backed), DNS, gRPC | FrankenGraphDB writes zero protocol plumbing below the graph wire format. |
| RaptorQ | asupersync | `src/raptorq/`: RFC 6330 systematic RaptorQ, GF(256) SIMD kernels, decode proofs, per-symbol authentication, deterministic decode planner | frankensqlite's "RaptorQ everywhere" doctrine executed with asupersync's implementation. |
| macaroons | asupersync | Security: macaroons, authenticated types; per-symbol auth on RaptorQ planes | Warden administers consumed capability tokens; it does not implement the token format. |
| metrics/OTel | asupersync | observability (metrics, OTel export, spectral wait-graph health) | — |
| deterministic lab | asupersync | Lab runtime: virtual time, seeded deterministic scheduling, DPOR/Mazurkiewicz schedule exploration, chaos injection, futurelock detection, trace capture/replay, crashpacks | fgdb-sim composes the lab; it is not a second lab. |
| supervision | asupersync | Spork/OTP: supervision topologies, gen_server, monitors, name-lease registry | No detached background thread anywhere (§1 constraint 7). |
| cluster membership (SWIM+Lifeguard) | asupersync | Distributed protocol machines: SWIM+Lifeguard membership with gossip + lease reactor | — |
| Merkle-range anti-entropy | asupersync | Merkle-range anti-entropy | fgdb-repl composes it over graph payloads. |
| coded symbol distribution | asupersync | fountain-coded symbol distribution | — |
| CRDT obligation ledgers | asupersync | CRDT obligation ledgers | — |
| HLC | asupersync | hybrid-logical-clock mode | The logical-command sequence/HLC contract is ours; the clock is consumed. |
| choreographic projection | asupersync | choreographic/session-typed | — |
| LDFI/delta-debug/dual-run/trace-export tooling | asupersync | Verification & forensics tooling: lineage-driven fault injection (LDFI), hierarchical delta-debugging + replay minimization + divergence diagnostics, dual-run lab-vs-live differential harness, TLA+ trace export | — |
| Kafka/JetStream clients | asupersync |  | CDC egress composes consumed clients. This is the one consume_from row with no §2.1 asset row — see the asset_evidence_gap below. |
| graph algorithms | franken_networkx | `fnx-algorithms`: 550+ functions across 25+ families | Reached through Prism's authorized projection, never by copying an algorithm into an fgdb crate. |
| legacy formats | franken_networkx | `fnx-readwrite`: fuzz-hardened native parsers/writers for edgelist, adjlist, GraphML, GML, JSON node-link, Pajek, GEXF | Distinct from the CSV/JSONL/Parquet-lite readers we build: those are tabular bulk-load formats, these are graph interchange formats. |

**Registered asset-evidence gaps.** A consumed capability normally names exactly one asset row of §2.1/§2.2. Where §18.2 attributes a capability the asset tables never enumerate, the absence is registered here and the checker verifies it: a gap whose asset row exists is itself a violation.

| Capability | Absent from | Finding |
|---|---|---|
| `queue-clients` | `plan-asupersync-assets-v1` | §18.2 names the capability; the §2.1 asset table has no messaging row. Verified present in the pinned foundation at src/messaging/ behind the default-off `kafka` feature — which is exactly what asupersync's mandatory default-features = false keeps out of our closure. |

### Adopted as design only (never linked)

| frankensqlite design | Re-instantiated in | Note |
|---|---|---|
| **Page-granularity MVCC** | `fgdb-txn` | Re-instantiated as block-granularity MVCC on Strata adjacency blocks; the donor's integer-only visibility rule does not transfer. |
| **SSI (page-Cahill/Fekete) + first-committer-wins + eager locking** | `fgdb-txn` | Graph-SSI plus logical predicate witnesses; deadlock freedom and SSI soundness do not transfer by analogy. |
| **Intent logs + deterministic rebase merge ladder** | `fgdb-txn` | Context-derived graph rebase and explicit-base branch merge, with hit rate measured rather than assumed. |
| **Native mode / ECS** | `fgdb-ecs` | Chronicle re-specifies the root protocol, full-digest identity, encoding variants, and final-capsule ordering. |
| **WriteCoordinator** | `fgdb-txn` | An ordering shape, not a promise that authorization or consensus is off the critical path. |
| **ARC buffer pool, MVCC-aware; cache-line-padded sharded lock/siread tables; aligned direct-I/O buffers; prefetch on descent** | `fgdb-buffer` | The mechanical-sympathy checklist for the buffer manager. |
| **Time travel** | `fgdb-chronicle` | Temporal support rides the MVCC machinery; history is queryable for the configured retention contract, never described as unbounded. |
| **Encryption** | `fgdb-keymgr` | Nonce, AAD, per-encoding DEK, symbol-MAC, rewrap, and KMS/HSM details are respecified; no correctness is inherited. |
| **Verification posture** | `fgdb-conformance` | The QA house style: conformance corpora, differential harnesses, feature-universe ledgers, exit-criteria contracts. |

| Residue allowance | Text | Why it is not a capability |
|---|---|---|
| `not-built-marker` | **Explicitly not built**: | The section marker separating the two lists; not a capability. |
| `asupersync-attribution` |  — all asupersync | The attribution tail for the preceding consume_from run; each row carries the same attribution in its `foundation_project`. |
| `fnx-attribution` |  — all fnx | The attribution tail for the two fnx rows. |
| `moat-rationale` | The closed-universe constraint, which sounds like an albatross, is the moat: the entire dependency surface is auditable, deterministic under lab, and owned. | Rationale sentence, not an inventory item. Pinned verbatim so this allowance cannot be widened to hide a capability. |

## Ownership vocabulary

| Id | Kind | Title | Crates | Anchor |
|---|---|---|---|---|
| `G0` | gate | Constitutional Contracts | 0 | §19 Gate G0 |
| `W1` | workstream | Bedrock + security foundations | 18 | §19 W1 |
| `W2` | workstream | Chronicle contracts + durability core | 7 | §19 W2 |
| `W3` | workstream | Strata | 4 | §19 W3 |
| `W4` | workstream | Transactions + Secure View | 3 | §19 W4 |
| `W5` | workstream | Loom | 8 | §19 W5 |
| `W6` | workstream | Ripple | 3 | §19 W6 |
| `W7` | workstream | Beacon | 5 | §19 W7 |
| `W8` | workstream | Prism + fnx upstream | 1 | §19 W8 |
| `W9` | workstream | Warden | 3 | §19 W9 |
| `W10` | workstream | Fabric + operations | 11 | §19 W10 |
| `W11` | workstream | Aegis replication + HA | 2 | §19 W11 |
| `W12` | workstream | Post-1.0 sharding | 1 | §19 W12 |
| `verification_ladder` | bead_family | Verification ladder | 4 | §15 (fgdb-verif-* beads; the §19 table names no verification workstream) |

## Checked plan sources

| Block | Plan lines | Lines | Bytes | fnv1a64 | Embedded | Covers |
|---|---|---|---|---|---|---|
| `plan-closed-universe-v1` | 37–37 | 1 | 1061 | `0x4145afb24082e377` | yes | §1 constraint 1 — the dependency universe is closed (FG-CON-01) |
| `plan-memory-safety-v1` | 38–38 | 1 | 570 | `0xf6598dc89fedc674` | yes | §1 constraint 2 — memory safety is structural (FG-CON-02) |
| `plan-three-postures-v1` | 41–41 | 1 | 376 | `0xb7ce112fce602ad2` | pin only | §1 constraint 5 — single artifact, three postures |
| `plan-asupersync-assets-v1` | 58–75 | 18 | 5887 | `0xf4c674b2960c6e0c` | pin only | §2.1 — the asupersync asset table (consume_from evidence) |
| `plan-fnx-assets-v1` | 81–89 | 9 | 2459 | `0x3768f87d4ec27de2` | pin only | §2.2 — the franken_networkx asset table (consume_from evidence) |
| `plan-frankensqlite-donor-v1` | 95–105 | 11 | 3218 | `0x87134ba230bd874a` | pin only | §2.3 — the frankensqlite donor table (design_only evidence) |
| `plan-reference-allowlist-v1` | 1156–1156 | 1 | 1924 | `0x4869259b5194c04d` | pin only | §15.2 — the fgdb-reference dependency allowlist and its one sharing exception |
| `plan-crate-layer-table-v1` | 1285–1300 | 16 | 2642 | `0xcc94f5b08108d544` | yes | §18.1 — the crate/layer table (PARSED: the crate universe is derived from it) |
| `plan-g0-materialization-v1` | 1302–1302 | 1 | 236 | `0x539eaa5c90799968` | pin only | §18 — what G0 materializes before Genesis implementation |
| `plan-build-inventory-v1` | 1306–1306 | 1 | 1392 | `0xb3b636c76dfb854b` | yes | §18.2 — build-it-ourselves inventory (DECOMPOSED: residue coverage) |
| `plan-workstream-table-v1` | 1319–1332 | 14 | 5939 | `0xfecc8909f6b9d2cf` | pin only | §19 — the workstream table (ownership vocabulary) |

### plan-closed-universe-v1 — §1 constraint 1 — the dependency universe is closed (FG-CON-01)

<!-- BEGIN plan-closed-universe-v1 -->
1. **The dependency universe is closed.** Allowed: `core`/`alloc`/`std`, the Rust nightly toolchain (both asupersync and franken_networkx already pin nightly), and the three foundation projects — `asupersync` (with whatever it vendors internally; its choices are its own), the `fnx-*` crates of `franken_networkx`, and *design-level* reuse of `frankensqlite` (we fork/adapt specific modules into `fgdb-*` crates rather than linking `fsqlite-*` wholesale, because graph objects are not SQLite pages — see §2.3). Everything else — compression codecs, sketches, ANN indexes, inverted indexes, radix trees, wire protocols, columnar readers — is built in-house (§18 is the complete inventory). No serde, no tokio, no rocksdb, no arrow, no tantivy, no hnswlib. The Python package may consume `fnx-python` as an allowed `fnx-*` foundation crate and therefore its already-pinned transitive binding runtime, but `fgdb-*` crates may not add a direct PyO3 dependency; the database engine, server, CLI, and durable formats remain free of that packaging boundary.
<!-- END plan-closed-universe-v1 -->

### plan-memory-safety-v1 — §1 constraint 2 — memory safety is structural (FG-CON-02)

<!-- BEGIN plan-memory-safety-v1 -->
2. **Memory safety is structural.** Every ordinary crate root and the workspace default use `unsafe_code = "forbid"`. Rust's `forbid` level cannot be lowered, so the few raw-pointer implementations (buffer arenas, SIMD kernels, VFS mappings) live in separately named `fgdb-unsafe-*` boundary crates whose roots use `deny(unsafe_code)` plus narrowly scoped, ledgered `allow` sites. Safe-facing crates never relax `forbid`. Every unsafe operation gets a ledger row: path, invariant, evidence, scalar or safe fallback, and no-claim boundary; CI rejects an unledgered site.
<!-- END plan-memory-safety-v1 -->

### plan-crate-layer-table-v1 — §18.1 — the crate/layer table (PARSED: the crate universe is derived from it)

<!-- BEGIN plan-crate-layer-table-v1 -->
| Layer | Crates |
|---|---|
| Foundation | `fgdb-types`, `fgdb-bigint` (canonical exact integers), `fgdb-delta-types` (G0/W2 delta schema only), `fgdb-claim`, `fgdb-authz-types`, `fgdb-policy` (restricted verifier IR), `fgdb-resource`, `fgdb-codec`, `fgdb-sketch`, `fgdb-collections`, `fgdb-crypto`, `fgdb-calibrate`, `fgdb-evidence` |
| Unsafe islands | `fgdb-unsafe-simd`, `fgdb-unsafe-arena`, `fgdb-unsafe-vfs` — the only crate roots not using `forbid(unsafe_code)`; every site is ledgered and exposed only through safe APIs |
| Chronicle | `fgdb-ecs` (identity/encoding/bootstrap/object locator), `fgdb-order` (durable Raft log/state core and quorum-one driver), `fgdb-chronicle` (capsule/marker/logical-local roots, allocator, outcomes, checkpoints/retention, recovery/GC/scrub), `fgdb-branch`, `fgdb-keymgr`, `fgdb-audit`, `fgdb-backup` |
| Strata | `fgdb-strata` (label-independent tiers, seal/compact, stable-ID directory), `fgdb-props`, `fgdb-buffer`, `fgdb-scratch` |
| Txn + secure access | `fgdb-txn` (MVCC, Graph-SSI/witness lifecycle, coordinator, final-effect merge ladder), `fgdb-constraints` (branch-scoped canonical enforcement), `fgdb-secure-view` (sole authorized storage/permit facade); depends on Chronicle + Strata + foundation policy verifier |
| Loom | `fgdb-gql` + `fgdb-cypher` (syntax only), `fgdb-bind`, `fgdb-algebra`, `fgdb-planner`, `fgdb-exec`, `fgdb-linalg`, `fgdb-datalog`; all reads flow through `fgdb-secure-view` |
| Ripple | `fgdb-ripple` (Z-sets, circuits, incrementalizer), `fgdb-views`, `fgdb-subs` |
| Beacon | `fgdb-index-core`, `fgdb-btree`, `fgdb-fts`, `fgdb-vector`, `fgdb-pathidx` (2-hop/landmark + the persistent-union-find temporal-connectivity index, §10.7) |
| Prism | `fgdb-prism` (authorized projection bridge, fnx cursor/cache/materialization paths, native kernels) |
| Warden | `fgdb-warden` (issuance/revocation/discharges/policy admin), `fgdb-privacy`, `fgdb-redaction` |
| Surface/operations | `fgdb-protocol` (FGP state machine), `fgdb-bolt`, `fgdb-formats`, `fgdb-udf-vm`, `fgdb-observatory`, `fgdb-system-graph` |
| Aegis | `fgdb-raft` (multi-member protocol over `fgdb-order`), `fgdb-repl` (payload availability, anti-entropy, reconfiguration, fenced GC); future `fgdb-shard` belongs only to W12 |
| Composition | `fgdb` (embedded API), `fgdb-server` (top-level Fabric+Warden+Aegis composition), `fgdb-cli`, `fgdb-python` (allowed fnx-python packaging boundary only), `fgdb-adbc` (C-ABI ADBC packaging at the same boundary; §13.7) |
| Verification | `fgdb-sim`, `fgdb-reference` (executable semantics oracle), `fgdb-oracles`, `fgdb-bench`, `fgdb-conformance`, `fgdb-fuzz` |
<!-- END plan-crate-layer-table-v1 -->

### plan-build-inventory-v1 — §18.2 — build-it-ourselves inventory (DECOMPOSED: residue coverage)

<!-- BEGIN plan-build-inventory-v1 -->
Compression codecs (EF, delta-varint, bitpacking, snappy, roaring-like), canonical signed-limb exact integers, sketches, ART/radix structures, succinct rank/select, vectorized hash tables, B-tree, HNSW + quantizers + IVF-PQ cold tier, persistent union-find, masked-semiring SpMV/SpMSpV kernels, inverted index + BM25 + Levenshtein automata, tokenizers, CSV/JSONL/Parquet-lite readers, GQL/Cypher parsers (hand-written recursive descent + Pratt — the frankensqlite parser school), the DBSP-style circuit runtime, Raft, FGP, Bolt subset, crypto profiles/primitives not already supplied by an audited foundation, the MMR accumulator + transparency checkpoints, the Sextant calibration scores/ledgers (over asupersync's e-process core), the deterministic UDF bytecode VM (§13.8), bench harness. **Explicitly not built**: async runtime, scheduler, channels, TLS/QUIC/HTTP/gRPC stacks, RaptorQ, macaroons, metrics/OTel, deterministic lab, supervision, cluster membership (SWIM+Lifeguard), Merkle-range anti-entropy, coded symbol distribution, CRDT obligation ledgers, HLC, choreographic projection, LDFI/delta-debug/dual-run/trace-export tooling, Kafka/JetStream clients — all asupersync; graph algorithms & legacy formats — all fnx. The closed-universe constraint, which sounds like an albatross, is the moat: the entire dependency surface is auditable, deterministic under lab, and owned.
<!-- END plan-build-inventory-v1 -->

## Pins

* `id_table_hash` = `fnv1a64:b422bc59c3da23ca` — every stable id, sorted.
* `semantic_contract_hash` = `fnv1a64:e365cf08c82c2750` — every normative decision, prose excluded.
