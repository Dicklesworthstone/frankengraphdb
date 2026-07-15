# AGENTS.md — frankengraphdb

> Guidelines for AI coding agents working in this Rust codebase.

---

## RULE 0 — THE FUNDAMENTAL OVERRIDE PREROGATIVE

If I tell you to do something, even if it goes against what follows below, YOU MUST LISTEN TO ME. I AM IN CHARGE, NOT YOU.

---

## RULE NUMBER 1: NO FILE DELETION

**YOU ARE NEVER ALLOWED TO DELETE A FILE WITHOUT EXPRESS PERMISSION.** Even a new file that you yourself created, such as a test code file. You have a horrible track record of deleting critically important files or otherwise throwing away tons of expensive work. As a result, you have permanently lost any and all rights to determine that a file or folder should be deleted.

**YOU MUST ALWAYS ASK AND RECEIVE CLEAR, WRITTEN PERMISSION BEFORE EVER DELETING A FILE OR FOLDER OF ANY KIND.**

---

## Irreversible Git & Filesystem Actions — DO NOT EVER BREAK GLASS

1. **Absolutely forbidden commands:** `git reset --hard`, `git clean -fd`, `rm -rf`, or any command that can delete or overwrite code/data must never be run unless the user explicitly provides the exact command and states, in the same message, that they understand and want the irreversible consequences.
2. **No guessing:** If there is any uncertainty about what a command might delete or overwrite, stop immediately and ask the user for specific approval. "I think it's safe" is never acceptable.
3. **Safer alternatives first:** When cleanup or rollbacks are needed, request permission to use non-destructive options (`git status`, `git diff`, `git stash`, copying to backups) before ever considering a destructive command.
4. **Mandatory explicit plan:** Even after explicit user authorization, restate the command verbatim, list exactly what will be affected, and wait for a confirmation that your understanding is correct. Only then may you execute it.
5. **Document the confirmation:** When running any approved destructive command, record (in the session notes / final response) the exact user text that authorized it, the command actually run, and the execution time.

---

## Branch Policy

- Primary branch is `main`.
- Do not reference `master` in docs/scripts.
- If release instructions require sync, push `main:master` after `main`.

---

## Project Mission

`frankengraphdb` is a **blank-slate, memory-safe, ultra-high-performance property-graph database in Rust**, built entirely on the Franken/asupersync ecosystem. It ships as one codebase in three postures: an **embedded library** (`fgdb`), a **server** (`fgdbd`) speaking the native FGP wire protocol plus HTTP/2, gRPC, WebSocket, and a Bolt-compat subset, and a **CLI** (`fgdb`). Larger-than-memory operation is first-class in all three.

The leapfrog is not one trick; it is the *composition* of six bets, each at or beyond the current frontier, made feasible only because the foundation libraries already exist:

- **B1 — One Version Universe.** MVCC versions, time-travel history, replication, change subscriptions, and git-style database branches are *the same mechanism*: an append-only, content-addressed, RaptorQ-coded commit stream (**Chronicle**).
- **B2 — Graph-Structured LSM ("Strata").** Adjacency lives in three temperature tiers (versioned delta blocks → sealed compressed CSR runs → archived anchors): transactional writes *and* static-CSR scan speed on one store.
- **B3 — Unified Factorized/WCO Execution ("Loom").** One Free-Join-style operator family subsumes binary hash joins, worst-case-optimal multiway joins, and factorized intermediates, running vectorized and morsel-parallel over Strata runs that *are already tries*.
- **B4 — Incremental Everything ("Ripple").** A DBSP-style Z-set delta algebra is the single engine for recursive queries, materialized views, standing queries/subscriptions, and incremental analytics — fed by the commit stream, which is *already* a Z-set stream.
- **B5 — Determinism as a Product Feature.** CGSE tie-break policies, complexity witnesses, and plan certificates make every result reproducible and auditable; every adaptive decision emits a replayable **decision card**; the whole database runs under asupersync's lab runtime for DPOR-explored, seed-replayable, chaos-injected testing.
- **B6 — Agent-Native by Construction.** Branch-per-agent isolation, capability-scoped (macaroon) subgraph authorization, provenance as first-class edges, and one hybrid vector+text+graph retrieval operator — purpose-built for GraphRAG and multi-agent memory.

**The single source of truth for what we are building and why is [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md`](COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md).** Read it before writing any subsystem.

### What we stand on (the closed dependency universe)

- `/dp/asupersync` — the operating system: structured-concurrency runtime (regions, obligations, `Cx` capability contexts, three-lane scheduler), the **lab runtime** (virtual time, DPOR, chaos, crashpacks), RaptorQ (RFC 6330), the full networking stack (TCP/UDP/QUIC/HTTP/1.1/2/3/WebSocket/TLS/gRPC), channels/combinators, Spork/OTP supervision, macaroons, metrics/OTel. Consumed as-is; we reimplement **none** of it.
- `/dp/franken_networkx` (`fnx-*`) — the algorithm brain: 550+ graph algorithms behind the `GraphView` trait, the **CGSE** determinism doctrine (tie-break policies, complexity witnesses, witness ledgers), fuzz-hardened legacy-format parsers/writers, generators. Consumed via **Prism** (zero-copy `SnapshotGraphView` bridge).
- `/dp/frankensqlite` — the architectural donor: we adopt its *designs* (page→block MVCC, SSI + merge ladder, Native/ECS content-addressing, WriteCoordinator, ARC buffer pool, time-travel, encrypt-then-code) and **re-instantiate them for graph objects** as `fgdb-*` crates. We do **not** link `fsqlite-*` — graph objects are not SQLite pages.

---

## Product Shape

The project must be all three at once:
1. A reusable Rust library — `fgdb::Database::open(path | :memory:)` → sessions → prepared statements → streaming results; typed row/column accessors; capability-gated Rust UDFs/procedures. Larger-than-memory is a property of every operator, not a mode.
2. A server binary `fgdbd` — multi-database, multi-tenant by capability, speaking **FGP** (native), HTTP/2, gRPC, WebSocket, and a Bolt-compat subset.
3. A CLI `fgdb` with **robot mode** (agent-first, versioned NDJSON, self-describing `robot schema`) and a human mode.

Native query language is **GQL (ISO/IEC 39075:2024)** with an openCypher compatibility surface and namespaced FQL extensions (`FOR SYSTEM_TIME`, `AT BRANCH`, `CALL fnx.*`, `CREATE MATERIALIZED VIEW … REFRESH INCREMENTAL`, `SUBSCRIBE TO`, `EXPLAIN (CERTIFICATE)`). No general KV underlay, no JVM, no GC, no external crates at any layer (see Doctrine #1).

---

## Spec-First Workflow

Implementation follows the plan, not ad-hoc invention. Read in this order:
1. [`COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md`](COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md) — architecture, the six bets, every subsystem (Chronicle, Strata, Loom, Ripple, Beacon, Prism, Warden, Fabric, Aegis), the verification doctrine, and the workstream/gate sequencing (§19).
2. **The Invariant Registry (Appendix F / `invariants.toml`)** — every load-bearing invariant (FG-INV-01 … FG-INV-20) with its stable ID, statement, and enforcement mechanism (Lean lane, TLA+ model, runtime oracle, property test, or CI gate). **CI cross-checks that every ID has a live checker.**
3. **The on-disk formats (Appendix A), the graph intent-log vocabulary (Appendix B), and the GLA operator inventory (Appendix C)** — the normative contracts a new crate must honor.

**Hard rule: no subsystem ships against an unenforced invariant.** A workstream exit gate (G1–G4, §19) cannot pass while any invariant it depends on lacks a live checker in `invariants.toml`. Promote a design assumption to enforced only after the checker exists.

---

## The FrankenGraphDB Engineering Doctrine (READ THIS BEFORE WRITING CODE)

These are the constitutional, non-negotiable rules from §1 of the plan. Violating any of them is a revert, memorialized in `docs/NEGATIVE_EVIDENCE.md`.

1. **The dependency universe is closed.** Allowed: `core`/`alloc`/`std`, the pinned Rust nightly, and the three foundations (`asupersync`, the `fnx-*` crates, design-level reuse of `frankensqlite`). Everything else — compression codecs, sketches, ANN indexes, inverted indexes, radix trees, wire protocols, columnar readers — is built in-house (§18 is the complete inventory). **No serde, no tokio, no rocksdb, no arrow, no tantivy, no hnswlib. Ever.** This constraint is the moat, not an albatross: the entire dependency surface is auditable, deterministic under lab, and owned.

2. **Memory safety is structural.** Workspace-level `unsafe_code = "forbid"`, with a frankensqlite-style **unsafe boundary ledger** for the handful of crates that genuinely need raw pointers (buffer arenas, SIMD kernels, mmap in the VFS). Every `unsafe` block gets a ledger row: path, invariant, evidence, no-claim boundary. Each SIMD kernel carries a **bit-identical scalar fallback** that cross-compiles to every target.

3. **`Cx` everywhere.** Every function that performs I/O, takes a lock, allocates from a shared arena, or can block accepts `&Cx` (asupersync's capability context). This is what makes B5 possible — swap the `Cx` and the whole database runs under the lab runtime. Subsystems receive purpose-typed wrappers (`QueryCx`, `TxnCx`, `CommitCx`, `MaintCx`, `ReplCx`) exposing only their legal effects. **The sharpest instance:** the merge ladder's intent-replay evaluator receives a `Cx` with **no clock, entropy, network, or filesystem capability** — deterministic rebase is guaranteed *by construction* (FG-INV-17), not by review.

4. **Deterministic by default.** Same database state + same query + same policy ⇒ byte-identical results, always, *including result order* (CGSE tie-break policies, §8.6). Nondeterminism (e.g. parallel float aggregation) is opt-in and declared in the plan certificate. `replay(certificate, seq, seed)` must reproduce a result bit-for-bit (FG-INV-19).

5. **The commit stream is the source of truth.** There is no mutable primary file. The only mutable object in a database directory is `manifest.root`. Everything else is immutable, content-addressed (`ObjectId = Trunc128(BLAKE3(...))`), and RaptorQ-erasure-coded. **No double-write journaling anywhere** — RaptorQ heals torn/corrupt symbols. Derived structures (indexes, views, stats) are **never more authoritative than the commit stream** (FG-INV-18); recovery discards and rebuilds them.

6. **Narrowed capability contexts are load-bearing security.** Read-only connections *cannot express* writes. A capability that can't see an edge type can't observe its existence via degree either (descriptor masking). Caveats compile to mandatory planner predicates — **security applies before expansion, never as a post-filter** (FG-INV-20).

7. **Prohibited shortcuts (constitutional).** No global-lock "interim" transaction model; no `HashMap<VId, Vec<EId>>` presented as storage; no snapshot isolation quietly labeled "ACID"; no parser-interprets-AST engine; no non-durable benchmark mode reported as a result; no serde-derived enum as a durable format; no detached background thread. Early code may implement a *subset* of a final abstraction — **never a substitute for it.** A slice that stubs the transaction model or bypasses the algebra is a prototype, and prototypes are prohibited.

8. **Correctness outranks speed, always.** The verification ladder (§15) and the reference oracle (`fgdb-reference`) come first; performance work follows the asupersync "How We Made It Fast" discipline (profile → remove one contention/allocation → re-verify determinism and cancel-correctness → commit with evidence). A faster path that drifts a result is reverted, not landed.

---

## Adaptive-Decision Contract (decision cards)

Every adaptive decision anywhere in the system — plan choice, tier migration (inline↔block, block↔run, escalate↔de-escalate), compaction pacing, victim selection, hedge/race gating, witness refinement — must emit a replayable **decision card** under a versioned **policy epoch**, carrying:
- explicit state space, candidate actions, and observed features
- an expected-benefit interval and the hysteresis/dwell state
- a deterministic **pinned fallback policy** so the lab runtime replays the decision bit-for-bit

Two anti-thrash rules bind every tier migration: minimum dwell time per descriptor, and expected benefit must exceed conversion cost *plus* uncertainty. Learned estimators are admissible **only as advisory features** on a decision card, with the analytic model as the deterministic fallback. No adaptive controller ships without its conservative deterministic fallback.

---

## Code Editing Discipline

### No Script-Based Changes
**NEVER** run a script that mass-edits code files. Brittle regex transforms create more problems than they solve. Make code changes manually (use parallel subagents for many simple changes; do subtle/complex changes methodically yourself).

### No File Proliferation
Revise existing files in place. **NEVER** create `plannerV2.rs` / `strata_improved.rs` / `exec_enhanced.rs`. New files are reserved for genuinely new functionality; the bar is incredibly high.

---

## Backwards Compatibility

We are in early development with **no users**. Do things the **RIGHT** way with **NO TECH DEBT**. Never create compatibility shims or wrappers for deprecated APIs. Just fix the code directly. (Durable on-disk formats are the one exception: they are versioned additive-minor/breaking-major from day one, per §16.6.)

---

## Toolchain

- Rust 2024 edition. Nightly toolchain (`rust-toolchain.toml`) — **required** for `core::arch` / `portable_simd` kernels and the features both foundations pin.
- `#![forbid(unsafe_code)]` at every crate root; `unsafe_code = "forbid"` at the workspace. `unsafe` is permitted **only** inside named, ledgered SIMD/arena/VFS islands behind an `#[allow(unsafe_code)]` boundary, each load carrying a `// SAFETY:` note and a bit-identical scalar fallback. Every such island has a row in the unsafe-boundary ledger.
- Cargo only, with crate-boundary enforcement per §18.1 (foundation → Chronicle → Strata → Txn → Loom → Ripple → Beacon → Prism → Surface → Aegis → Verification). Durable run state and telemetry ride the ECS/Chronicle substrate — never an external database.

---

## Mandatory Checks After Substantive Changes

```bash
cargo fmt --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
ubs $(git diff --name-only)
```

If any check fails, fix root causes before handing off.

### The `cargo test` gate (green-bar requirement)

`cargo test` is a **hard gate**: it MUST exit `0` before any change is handed off or a bead is closed. The convenience wrapper `scripts/check.sh` runs `cargo fmt --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` in order and stops on the first failure. When CI is added, wire `scripts/check.sh` as the CI test step rather than duplicating the commands.

Beyond the bare gate, **every verification domain in §15 is a permanent CI gate** — semantics conformance, transaction-anomaly oracles, the crash-point matrix, format fuzzers, representation-equivalence, incremental correctness, and complexity-witness regression locks (an operator whose observed op-count exceeds its declared bound *fails CI*). A release may bypass a gate only with a public, expiring waiver recorded in the ledger.

---

## Testing Policy — the Verification Ladder (plan §15)

This is a design pillar with its own budget, not a QA appendix. From cheapest to strongest:

- **Simulation-first (`fgdb-sim`).** The entire database runs under asupersync's lab runtime: virtual time, seeded deterministic scheduling, virtual TCP, a lab VFS (injectable latency, torn writes, bit flips, ENOSPC, fsync lies), chaos at every obligation boundary. Every concurrency bug is a seed; CI explores schedules with **DPOR**; failing runs auto-attach crashpacks with replay commands. **The lab VFS exists before the first fsync** (W1).
- **The reference oracle (`fgdb-reference`).** A deliberately simple, single-threaded, obviously-correct implementation of the full logical semantics (values, visibility, path modes, intents, temporal selectors, branches) over canonical maps. Compiled for tests/fuzzing/model-checking only, never shipped, never optimized. "What should this return" is a *program*, not a debate — and it exists before the first optimized line.
- **Consistency oracles (in-sim, continuous):** SI oracle (no read sees `seq > snapshot.high`), SSI oracle (reconstruct the rw-graph from traces; assert no committed dangerous structure — *our own cycle detection verifies our own serialization graphs*), obligation-leak oracle, quiescence oracle, Elle-class history checking.
- **Differential & conformance:** openCypher TCK; a GQL feature-conformance corpus keyed to ISO feature IDs (published as a matrix); differential vs. Neo4j & Memgraph on a curated corpus (parity where standards align, *documented divergence* elsewhere); Prism results differential vs. standalone fnx (itself NetworkX-parity-locked — a two-hop oracle chain to ground truth).
- **Storage oracles:** model-based testing of Strata against an in-memory reference graph (equality after every op under every open snapshot); metamorphic suites (pattern-match results invariant under compaction, seal, branch-fork, encode/decode round-trips).
- **Fault & recovery torture:** crash-point matrix over the two-fsync protocol; torn-write + bit-rot campaigns asserting RaptorQ recovery up to overhead and fail-closed beyond it; compaction crash/lease storms; replication partition/donor-loss during bonded pulls.
- **Formal anchors (scoped, honest):** Lean for MVCC visibility, block-level SSI safety, merge-ladder rung-1 soundness, and the Z-set operator subset; TLA+/TLC for the two-fsync commit + recovery, compaction publish/retire, Raft-marker interaction, and branch fork/merge. Each claim gets a proof-lane manifest row stating exactly what is and is not proven.
- **Fuzzing:** GQL/Cypher grammars, FGP frames, every `fgdb-formats` reader, the SymbolRecord decoder.

Plan certificates make **production** results replayable: certificate + seq + seed ⇒ byte-identical re-execution (FG-INV-19).

---

## Agent Ergonomics Requirements

CLI robot mode must be: stable versioned schema, deterministic where possible, explicit exit codes, line-oriented NDJSON, easy to pipe. Do not mix human decoration with machine output in robot mode. `fgdb robot schema` self-describes the contract; a contract test validates emitted events against a frozen JSON schema fixture. Beyond the CLI, the database's *own* runtime (sessions, transactions, queries, plans, operators, snapshots, obligations, segments, compaction jobs, subscriptions, replication streams, decision cards) is exposed as a read-only, access-controlled temporal property graph — queryable in GQL like any other graph. Dogfood it.

---

## Session Completion ("Landing the Plane")

Before finishing a work session you MUST:
1. File beads issues for remaining work (anything needing follow-up).
2. Run quality gates (if code changed) — tests, clippy, fmt, `ubs`.
3. Update issue status — close finished work, update in-progress.
4. `br sync --flush-only` to export beads to JSONL, then `git add .beads/`.
5. Hand off — summarize what changed, gates run + results, remaining risks/gaps, concrete next steps.

---

## MCP Agent Mail — Multi-Agent Coordination

A mail-like layer for agents to coordinate via MCP tools/resources: identities, inbox/outbox, searchable threads, advisory file reservations with human-auditable Git artifacts.

- **Register identity:** `ensure_project(project_key=<abs-path>)` → `register_agent(project_key, program, model)`.
- **Reserve files before editing:** `file_reservation_paths(project_key, agent_name, ["crates/fgdb-strata/**"], ttl_seconds=3600, exclusive=true, reason="br-###")`.
- **Communicate with threads:** `send_message(..., thread_id="br-###")`, `fetch_inbox`, `acknowledge_message`.
- **Prefer macros:** `macro_start_session`, `macro_prepare_thread`, `macro_file_reservation_cycle`, `macro_contact_handshake`.
- Common pitfalls: `"from_agent not registered"` → `register_agent` in the right `project_key` first; `"FILE_RESERVATION_CONFLICT"` → adjust patterns / wait / use non-exclusive.

---

## Beads (br) — Dependency-Aware Issue Tracking

This project uses [beads_rust](https://github.com/Dicklesworthstone/beads_rust) (`br`). Issues live in `.beads/` and are tracked in git. **`br` is non-invasive — it NEVER runs git.** After `br sync --flush-only`, manually `git add .beads/ && git commit`.

```bash
br ready                 # issues ready to work (no blockers)
br list --status=open
br show <id>             # full detail with dependencies
br create --title="..." --type=task|bug|feature|epic --priority=2   # 0=critical..4=backlog (NUMBERS)
br update <id> --status=in_progress
br close <id> [<id2> ...] [--reason "..."]
br dep add <issue> <depends-on>
br sync --flush-only     # export to JSONL (NO git ops)
```

Conventions: use the bead ID (e.g. `br-123`) as the Agent-Mail `thread_id` and prefix subjects with `[br-123]`; put the issue ID in the file-reservation `reason`; include `br-###` in commit messages. Map beads to workstreams (W1 Bedrock … W8 Fabric+Warden+Aegis) and gates (G1–G4) from §19.

---

## bv — Graph-Aware Triage

`bv` computes PageRank/betweenness/critical-path/cycles over `.beads/beads.jsonl`. **Use ONLY `--robot-*` flags — bare `bv` launches a blocking TUI.** Start with `bv --robot-triage` (counts + top picks + quick wins + blockers). `bv --robot-plan` for parallel tracks; `bv --robot-insights` for full metrics (check `.Cycles` — must be empty).

---

## UBS — Ultimate Bug Scanner

`ubs <changed-files>` before every commit. Exit 0 = safe; exit >0 = fix & re-run.

```bash
ubs file.rs file2.rs                    # specific files (< 1s)
ubs $(git diff --name-only --cached)    # staged files — before commit
ubs --only=rust,toml crates/            # language filter
```
Parse `file:line:col` → location, 💡 → suggested fix. Fix root cause, not symptom. Critical (always fix): memory safety, UB, data races. Important: unwrap panics, resource leaks, overflow.

---

## RCH — Remote Compilation Helper

RCH offloads `cargo build/test/clippy` to remote workers to avoid local compilation storms. Installed at `~/.local/bin/rch`, hooked into Claude Code's PreToolUse — usually transparent. Manual: `rch exec -- cargo build --release`. Health: `rch doctor`, `rch status`. Fails open (builds run locally if workers unavailable). **Codex/GPT users:** no auto-hook — manually `rch exec -- <cmd>` for heavy builds.

---

## ast-grep vs ripgrep vs warp_grep

- **`ast-grep`** when structure matters (refactors/codemods, policy checks, safe rewrites): `ast-grep run -l Rust -p '$X.unwrap()'`.
- **`ripgrep`** for raw text/literal hunts and pre-filtering.
- **`mcp__morph-mcp__warp_grep`** for exploratory "how does X work?" — an AI agent expands the query, reads files, returns line ranges with context. Don't use it to find a known symbol (use `rg`); don't use `rg` to understand architecture (use `warp_grep`).

---

## cass — Cross-Agent Session Search

`cass` indexes prior agent conversations so we can reuse solved problems. **Never run bare `cass` (TUI)** — always `--robot` or `--json`.

```bash
cass search "graph-ssi phantom witness" --robot --limit 5
cass view /path/to/session.jsonl -n 42 --json
```
stdout is data-only, stderr diagnostics, exit 0 = success. Treat it as a way to avoid re-solving problems other agents already handled (this project's own kickoff research lives there).

---

## Note for Codex/GPT agents — unexpected working-tree changes

If `git status` shows edits you did not make (in `Cargo.toml`, `crates/**/*.rs`, etc.), those are from the **other agents working on this project concurrently** — a normal, frequent occurrence. **NEVER** stash, revert, or overwrite another agent's work. Treat those changes exactly as if you made them yourself. Do not stop to ask about them.

---

## Note on Built-in TODO Functionality

If I explicitly ask you to use your built-in TODO functionality, do so without complaining that you need to use beads. Always comply with such orders.
