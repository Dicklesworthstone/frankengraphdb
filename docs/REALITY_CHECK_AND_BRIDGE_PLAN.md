# Reality Check and Bridge Plan

**Current measurement: 2026-08-20.** This document is revised in place. Older
commit-bound assessments are retained below as superseded historical snapshots because
they explain several decisions; their counts and statements about missing seams are not
current unless the 2026-08-20 delta repeats them.

---

## Current delta — 2026-08-20

### Product verdict

**FrankenGraphDB still is not the database product the README describes in the
present tense.** HEAD `df733010a46ac2a725df141204d7940b4e37d4b7` has a real
embedded durability *spine*: a caller with a production asupersync `CommitCx`
and raw keys can create a directory, commit a `WriteBatch` through Chronicle's
authenticated-and-RaptorQ capsule plus two-fsync marker protocol, fold it into
durable Strata Tier-D blocks, read neighbours/vertices/edges (including `*_at`
historical sequences), compact, crash, drop the handle, and reopen the same
content-addressed partition. Crate docs and `crates/fgdb/tests/spine.rs` earn
that claim. The README's GQL shell, sessions, prepared statements, SSI
transactions, git-style branches, Loom FreeJoin, Ripple views, Beacon HNSW,
Prism `CALL fnx.*`, Warden macaroons, Fabric (`fgdbd` / FGP / Bolt), Aegis
Raft, Python wheels, `scripts/install.sh`, and §17 performance gates **do not
exist as runnable product**. Fifty of seventy topology slots remain `planned`
directories that must not exist; the nineteen live crates are foundation,
unsafe islands, Chronicle, Strata Tier D, the spine library, sim, and
reference.

The two days after the 2026-08-18 measurement did not change that
classification. They fenced cancelled Chronicle commits (`fgdb-8x5e`), made
mixed `DeleteVertex` cascades fail closed without partial retire, added a
changelog, and moved historical plan-review markdown into `docs/planning/`.
That is spine correctness work. It is not a product increment.

### The five questions this skill asks

1. **What is working right now.** Durable open/write/read/drop/reopen on one
   hard-coded `(GraphId(1), BranchId(1), partition 0)`. Chronicle identity →
   AEAD → RaptorQ capsules → D1/D2 markers → dual-slot `manifest.root`. Strata
   Tier-D blocks, vertex/edge patches, partition roots, manifests, MVCC
   `[created_at, retired_at)` visibility, incremental publish, compaction,
   checkpoint-selected reopen bound to the marker-chain commitment. Production
   `Runtime::request_cx_with_budget` → `CommitCx` (no lab scheduler).
   `fgdb-reference` as a real semantics oracle. `fgdb-sim` as a real lab
   harness. `fgdb-calibrate` as a real Sextant library with no product
   controller consuming it. G0 claim machinery (registries, `registry-check`,
   `scripts/check.sh` verdict contract) is real and self-aware that invariant
   enforcement is zero. One Lean lane (`formal/lean/VersionChain.lean`) is
   checked.

2. **What is not working or not yet implemented.** Sessions, prepared
   statements, any query language, the production transaction manager, secure
   views, streaming typed results, multi-graph/branch/partition product APIs,
   Tier I / Tier R / Tier A, retention-cooling as a temporal database, Strata
   objects sealed as Chronicle capsules, CLI `fgdb`, server `fgdbd`, Python
   package, `scripts/install.sh`, plan certificates, hybrid search, Prism,
   Warden, Fabric, Aegis, §17 benches, 19 of 20 FG-INV checkers, 9 of 10
   formal lanes, TLA+ tree (directory absent). `WriteBatch` is explicitly not
   a transaction. `Database::open` is async, requires `&CommitCx` and
   `DatabaseKeys`, and has no `:memory:` path.

3. **What is blocking us.** Not a missing idea. Sequencing and an empty ready
   queue. Before this audit's three new beads, `br stats` reported **763
   records, 274 open, 15 in progress, 466 closed, 284 dependency-blocked,
   0 ready**. After filing and rewiring them, the tracker is **766 / 277
   open / 3 ready** (`fgdb-g0-doc-sync-usq.1`, `fgdb-tvg8.1`,
   `fgdb-epic-w2-6hc.1`). G0 is not frozen: 176
   `[[contract]]` rows in `command_contracts.toml` are `status = "reserved"`
   and **zero are `live`**, so the plan §5.1 two-way bijection over inhabitable
   command-union arms and exhaustive apply handlers still quantifies over an
   empty live set. `fgdb-5uw2` owns that closure and is in progress under
   another owner. Invariant clauses remain 20/20 stub,
   `expected_enforced_clauses = 0`. Later workstreams (W4 txn, W5 Loom, W10
   surfaces) cannot honestly start while those contracts and the Genesis slice
   (`fgdb-gate-genesis-lce`) are unclosed. Swarm energy since 2026-08-18 went
   into spine fences, which is correct locally and does not unblock the
   product critical path.

4. **If we implemented all open and in-progress beads, would we close the
   gap?** **Yes for tracking, no as a one-line guarantee of 1.0.** All 22
   epics remain open and they already name G0, W1–W12, Loom, Ripple, Beacon,
   Prism, Warden, Fabric, Aegis, verification, performance, and observability.
   Completing every open bead *to its written acceptance criteria* would
   cover the README's 1.0 target. Completing them *as currently written* would
   not automatically produce G4: many W5–W11 beads are still whole-subsystem
   slices, G0 command rows are reserved rather than live, and zero FG-INV
   clauses are enforced. The gap is execution and gate honesty, not an
   untracked vision.

5. **Vision goals with no bead.** Essentially none of the README's product
   claims lack an owner epic. Thin spots, not `NO_BEAD` holes: (a) README
   still documents `curl …/scripts/install.sh` and that file does not exist —
   covered only generically by `fgdb-epic-w10-mhq.1` and `fgdb-g0-doc-sync-usq`;
   (b) `fgdb-chronicle/src/lib.rs` crate header still says RaptorQ / capsules /
   `WriteCoordinator` are later increments after those modules shipped; (c)
   implemented P0/P1 spine bugs (`fgdb-8x5e`, `fgdb-w3-tier-d-ctj.4`) remain
   `in_progress` after landing commits, so the tracker lags the tree.

### Vision checklist (code = ground truth)

| # | Goal | Source | Status | Bead coverage | Evidence |
|---|---|---|---|---|---|
| 1 | Embedded `Database::open(path\|:memory:)` sync library | README L257-273 | **PARTIAL** | `fgdb-j0vu`, `fgdb-w10-embedded-54r` | Async `create`/`open(cx, path, keys)` only; no `:memory:`; `crates/fgdb/src/lib.rs:50-71,1632+` |
| 2 | Durable commit stream, no double-write journal | README L34, B1 | **PARTIAL** | `fgdb-epic-w2-6hc`, `fgdb-w2-g1-engine-core-yosi` | Capsules + D1/D2 + `manifest.root` real; retention tiers / product branches / replication not |
| 3 | Temperature-tiered Strata (I/D/R/A) | README L37, B2 | **PARTIAL** | `fgdb-epic-w3-umx`, `fgdb-w3-tier-d-ctj` | Tier D only; I/R/A named absences in `fgdb-strata/src/lib.rs:25-28` |
| 4 | GQL + openCypher + FQL | README L59-88, B3 | **NOT_STARTED** | `fgdb-epic-w5-ba9`, `fgdb-w5-parsers-nje`, `fgdb-g0-language-contracts-54g` | No parser crate; reference path-mode tests only |
| 5 | FreeJoin / WCO / factorized execution | README L38 | **NOT_STARTED** | `fgdb-5vp9`, `fgdb-rz12`, `fgdb-w5-executor-olp` | Planned crates; no operator code |
| 6 | Ripple incremental views / `SUBSCRIBE` | README L75, B4 | **NOT_STARTED** | `fgdb-epic-w6-65w` | `ZWeight` ring only |
| 7 | Deterministic STRICT results + certificates | README L40, B5 | **PARTIAL** | `fgdb-epic-verif-phi` | Canonical encodings + reference + sim; no plan certificates |
| 8 | Agent-native branches, macaroons, hybrid.search | README L41, B6 | **NOT_STARTED** | `fgdb-epic-w9-lcy`, `fgdb-w7-vector-79hu`, `fgdb-epic-w2-6hc` | Oracle branch tests; no product API |
| 9 | Server `fgdbd` (FGP/HTTP2/gRPC/WS/Bolt) | README L221, L2 | **NOT_STARTED** | `fgdb-epic-w10-mhq`, `fgdb-w10-server-rte` | No crate, no binary |
| 10 | CLI `fgdb` robot mode | README L196-232 | **NOT_STARTED** | `fgdb-huu9` | No crate, no binary |
| 11 | Python ABI3 wheels | README L276-287 | **NOT_STARTED** | `fgdb-w10-python-kkb` | No package tree |
| 12 | Install script / signed releases | README L17, L234-238 | **NOT_STARTED** | `fgdb-epic-w10-mhq.1` | `scripts/install.sh` **does not exist** |
| 13 | SSI / Graph-SSI transactions | README L52, plan §7 | **STUB in production** | `fgdb-epic-w4-7en`, `fgdb-w4-g1-txn-core-qpmg` | Real in `fgdb-reference`; `WriteBatch` disclaims txn |
| 14 | Larger-than-memory operators | README L12, AGENTS product shape | **NOT DEMONSTRATED** | `fgdb-tvg8`, W3/W5 spill beads | Durable objects exist; live `Snapshot` is decoded in RAM |
| 15 | §17 empirical gates | README L354-374 | **UNPROVEN** | `fgdb-epic-perf-4xe` | Zero Cargo bench targets |
| 16 | FG-INV-01..20 live checkers | AGENTS spec-first, plan §19 G1 | **STUB** | `fgdb-g0-invariant-spine-tmm` | `expected_enforced_invariants = 0` |
| 17 | Lab VFS before first fsync | AGENTS W1 | **PARTIAL** | `fgdb-verif-sim-q97e` | Chronicle/root under VFS; Strata `BlockStore` still path-backed |
| 18 | Closed dependency universe | README L45, doctrine 1 | **WORKING** | topology + deny policy | Enforced; no serde/tokio/rocksdb |
| 19 | `unsafe_code = "forbid"` + ledger | README L44 | **WORKING** | `fgdb-w1-unsafe-islands-eqrq` | Three named islands, CI ledger |
| 20 | G0 constitutional freeze | plan §19 | **PARTIAL** | `fgdb-epic-g0-597`, `fgdb-5uw2` | Registries exist; 176 command contracts reserved, 0 live |

**Vision delivery: 2 of 20 fully working (closed-universe + unsafe ledger). The
spine is a serious PARTIAL on goals 1–3, 7, 17, 20. Everything a user of the
README would type is NOT_STARTED.**

### Current evidence boundary

Pinned to tracked commit `df733010a46ac2a725df141204d7940b4e37d4b7`
(2026-08-20 17:24:21 -0400, `test(spine): recover D2 cancel after a durable
commit-log marker`). The shared checkout again contained the same three
untracked foreign artifacts (`.beads/beads.db-wal-cert`,
`.beads/beads.db-wal-cert-head`, `tools/registry-check/src/claims.rs`); they
were neither edited nor removed.

README.md still carries the explicit target-state tense note at L20. The
crate-level honesty in `crates/fgdb/src/lib.rs:50-62` is the more accurate
product description.

A fresh `rch exec -- cargo run -p fgdb --example open_a_database` on this
HEAD compiled and printed:

```
OK: opened, wrote, dropped, reopened, agreed.
```

Remote command exit was 0 (`committed at seq CommitSeq(1)`, neighbours
`[VId(2), VId(3)]` before drop and after reopen). RCH then failed artifact
retrieval (`RCH-E309`) and returned 102, the same pattern as 2026-08-18.
That is behavioral evidence that the spine example still runs, not a green
local command and not a substitute for `scripts/check.sh`.

### Current measured inventory

| Measure | 2026-08-18 | 2026-08-20 | Interpretation |
|---|---:|---:|---|
| HEAD | `8d295653` | `df733010` | ~8 spine/docs commits, no new posture |
| Topology | 70 slots: 19 active, 50 planned, 1 reserved | unchanged | server/CLI still deferred |
| Cargo packages | 20 | 20 | 19 engine + `registry-check` |
| Product binaries `fgdb`/`fgdbd` | 0 | 0 | five binaries are checker tools |
| Cargo bench targets | 0 | 0 | no §17 harness |
| Invariant enforcement | 0 / 0 | 0 / 0 | still deliberately zero |
| Checker rows | 57 live / 42 stub | 58 live / 43 stub (102 tables) | G0 surface grew; not invariant promotion |
| Formal lanes | 1 checked / 9 declared | unchanged | `formal/tla/` still absent |
| Command contracts | metadata in progress | **176 reserved, 0 live** | bijection still empty on the live side |
| Tracker | 759 / 462 closed / 0 ready | **766 / 466 closed / 3 ready** | three new ready seams from this audit; 15 in progress unchanged |
| Epics | 22 open | 22 open | none of G0–W12 closed |
| Engine `todo!()` | — | **0** in `crates/**` | gaps are named absences, not keyword stubs |

`br list --status=blocked` reports 8 explicitly blocked records; `br stats`
reports 284 dependency-blocked. `bv --robot-triage` uses a broader actionable
predicate than `br ready`. Compare them only with that caveat. Dependency
cycles were last reported empty; this audit did not wait out a hung `bv`
insights job to re-hash.

### What the code actually does (unchanged architecture)

```text
WriteBatch (one RelationId; not a transaction)
  -> canonical LogicalDeltaTemplate
  -> Chronicle capsule (AEAD + RaptorQ symbols)
  -> fsync D1
  -> chained commit marker
  -> fsync D2                         commit authority
  -> Tier-D Strata block/patch/root   derived publication (plain bytes)
  -> decoded in-memory Snapshot
  -> low-level vertex/edge/neighbour reads
```

Authority direction is still correct: Chronicle is source of truth; Strata is
rebuildable derived state. Normal commit validation is still
`PassThroughValidator`. Strata `BlockStore` still persists canonical plain
bytes; capsule composition of derived objects is a sim proof, not the
production path.

### Bridge plan — close every remaining product gap

Order is **vision impact**, not ease. Storage hardening after 2026-08-18 is
necessary and must stop being treated as a substitute for the product seams.

#### Gap 1 — G0 live command universe (`fgdb-5uw2`) — PARTIAL → WORKING

**Current:** 176 reserved contract rows, 0 live; no generated inhabitable
union/body/result/handler bijection.
**Target:** every v1 Local (and the G1-required Meta subset) arm is `live`,
has one handler, and the checker fails if a live row lacks an arm or an arm
lacks a row.
**Success:** registry-check bijection tests red on a deleted handler; a
single typed apply path exists for the spine's current write commands.
**Would existing beads close it?** Yes — do not fork `fgdb-5uw2`.
**Complexity:** XL. **Blocks:** Genesis, txn, query.

#### Gap 2 — Real transactions over the spine (`fgdb-w4-g1-txn-core-qpmg`) — NOT_STARTED in production → WORKING

**Current:** `WriteBatch` atomic durability; SSI lives in `fgdb-reference`.
**Target:** session/txn ownership, first-committer-wins, SSI validation on
the commit validator seam, typed abort, purpose-narrowed `TxnCx`.
**Success:** two concurrent writers, one aborts, reference oracle agrees;
`PassThroughValidator` is gone from the product open path.
**Would existing beads close it?** Yes, after Gap 1.
**Complexity:** XL.

#### Gap 3 — Minimum GQL → GLA → Loom → streaming results — NOT_STARTED → WORKING

**Current:** no parser crate.
**Target:** one generated GQL subset (node/edge match + return) lowered to a
registered GLA operator set, executed over Strata cursors, differential
against `fgdb-reference`. Parser breadth without algebra is not an increment.
**Success:** `MATCH (a)-[:R]->(b) RETURN b` over the spine example graph
returns the same rows as the oracle; a plan certificate stub hashes the
bound statement + snapshot seq.
**Would existing beads close it?** Partially — W5 beads exist but are staged
as whole-engine work; the Genesis slice `fgdb-gate-genesis-lce` is the
integration bead. Keep it as the vertical slice, not a fourth planner.
**Complexity:** XL.

#### Gap 4 — Bounded recovery and larger-than-memory — PARTIAL → WORKING

**Current:** checkpoint-selected open exists; live snapshot is still a
decoded graph; Tier R/A absent; Strata not fully VFS-injected.
**Target:** open bounded by checkpoint size + suffix; selective block reads;
Tier R seal path; lab VFS on `BlockStore`.
**Would existing beads close it?** Yes (`fgdb-tvg8` and W3 children).
**Complexity:** L.

#### Gap 5 — Promote invariants only with live checkers — STUB → WORKING

**Current:** 20 stub clauses, ledger pinned at zero.
**Target:** promote FG-INV-03/08/09/18 only when their checker and a
distinct negative test are live in the liveness.rs sense; bump
`expected_enforced_*` in the same change.
**Would existing beads close it?** Yes. Do not raise the ledger early.
**Complexity:** M per clause.

#### Gap 6 — One vertical product surface — NOT_STARTED → WORKING

**Current:** example binary is not the CLI; install URL 404s.
**Target:** after Gaps 2–3, absorb the spine into `fgdb-cli` robot mode and
the documented sync `Database` API; stop advertising `install.sh` until it
exists (`fgdb-g0-doc-sync-usq`).
**Would existing beads close it?** Yes (`fgdb-huu9`, `fgdb-w10-embedded-54r`,
`fgdb-epic-w10-mhq.1`).
**Complexity:** L after Gaps 2–3; currently would wrap an API the README
does not match.

#### Gap 7 — Remaining 1.0 layers (Ripple, Beacon, Prism, Warden, Fabric, Aegis)

Covered by open P2 epics. They are **not** the next capability step. Doing
them before Gaps 1–3 produces more islands.

### Ambition constraint (why more spine work is the wrong local optimum)

The swarm's revealed preference is to deepen Chronicle/Strata evidence
because those crates compile, have crash matrices, and close P0 bugs. That
is locally rational and globally insufficient. The README's leapfrog is the
*composition* of B1–B6. B1/B2 at Tier D without B3 language, B4 incremental,
or a transaction model cannot pass G1's Genesis slice. The conservative
deterministic fallback for this project is therefore: **freeze G0 live
commands, wire the validator, land one GQL match, then harden**. Further
compaction/cancel/fence work is welcome only when it unblocks those seams or
repairs a red gate.

### Tracker hygiene found by this measurement

- `fgdb-8x5e` remains `in_progress` after `92ef0ed` / `df73301`. Close only
  after independent review of the D2-cancel witness, not from the commit
  message.
- `fgdb-w3-tier-d-ctj.4` remains `in_progress` after `c73af64`. Same rule.
- `fgdb-9p13` (CHANGELOG coverage) is in progress; `7957016` folded CHANGELOG
  into claims-lint — verify before close.
- Do not create parallel Loom/Ripple/CLI epics. The 22 existing epics already
  own those goals.

### Beads filed from this measurement (only uncovered seams)

The existing 22 epics already cover the README's 1.0 surface. This audit
refused to clone them. Three seams had no owner row:

| ID | Gap |
|---|---|
| `fgdb-epic-w2-6hc.1` | Chronicle crate-root docs still describe landed capsules/RaptorQ/`CommitCoordinator` as future work |
| `fgdb-g0-doc-sync-usq.1` | README advertises `scripts/install.sh` / `fgdb` / `fgdbd` / `pip install` commands that cannot run |
| `fgdb-tvg8.1` | Strata `BlockStore` is not on the lab VFS, so post-D2 derived publication is not faultable |

`fgdb-5uw2` already owns the 176-reserved/0-live command-contract closure;
this audit left a measurement comment there instead of a fork.

### Genesis slice (the only valid "are we a database yet?" test)

G1's Genesis slice (`fgdb-gate-genesis-lce`) is the honest product
integration target. Treat it as passed only when all of the following are
true in one lab campaign, not as separate crate greens:

1. Fresh directory → three-phase bootstrap → `Operational`.
2. A pinned GQL subset statement binds, plans, and executes through an
   authorized Strata cursor (not `WriteBatch` neighbour scans).
3. Commit validation is not `PassThroughValidator`.
4. Crash before/after D1, D2, and Strata publication; reopen matches
   `fgdb-reference`.
5. Result includes a replayable certificate over snapshot seq + bound
   statement.
6. Every FG-INV clause reachable from that capability manifest is `live`
   with a distinct negative test.

Anything short of that — including a richer crash matrix on neighbour
scan — is still the spine, not G1.

---

## Current delta — 2026-08-18

### Product verdict

**The product classification has not changed: FrankenGraphDB has a real embedded
durability spine, but it is still not the graph-database product described by the
README.** The 23 commits after the prior measurement materially harden that spine and
its evidence rather than adding a new product posture:

1. graph publication and its delta are now installed as one authoritative cut, while
   compaction-safe read views pin the exact immutable objects they observe;
2. creation durably synchronizes the database parent directory, VFS-backed open is
   namespace-confined, root-generation exhaustion fails closed, and root/capsule reads
   are bounded before allocation or decoding;
3. secret-bearing failures use redacted error surfaces and shared scrubbed key
   ownership, including drop-path coverage; and
4. the LAB dual-run harness can force schedules, persists replay artifacts, and shrinks
   both event and scheduler-decision axes.

Those are worthwhile B1/B2/B5 advances. They do not make `WriteBatch` a transaction,
remove the fixed graph/branch/partition coordinates, or add sessions, prepared
statements, GQL/openCypher execution, Loom, Ripple, secure query views, streaming typed
results, a product CLI, `fgdbd`, a Python package, or an installable release. Tier R,
archived anchors, product-scale admission/spill, and §17 benchmark proof are also still
absent. No G1-G4 product gate follows from this hardening work.

### Current evidence boundary

This delta is pinned to tracked commit
`8d295653354b05ee448f1d5164bcdf12c9cdf448`. `README.md`, the comprehensive plan,
and the threat model are byte-unchanged from the 2026-08-16 assessment baseline. The
shared checkout again contained the same three untracked foreign artifacts
(`.beads/beads.db-wal-cert`, `.beads/beads.db-wal-cert-head`, and
`tools/registry-check/src/claims.rs`); they were neither edited nor removed.

Current static measurements remain 70 topology slots (19 active, 50 planned, one
reserved), 20 Cargo packages, 20 library targets, 97 integration-test targets, five
examples, five checker-tool binaries, and zero benchmark targets. There is still no
product `fgdb` or `fgdbd` binary. The invariant ledger still has 20 IDs with zero
enforced clauses and zero enforced invariants; the checker registry has 57 live and 42
stub rows; and one of ten formal lanes is checked.

The tracker now contains 759 records: 462 closed, 275 open, 14 in progress, and eight
explicitly blocked. `br stats` classifies 284 records as dependency-blocked and reports
zero ready records, while `bv --robot-triage` reports 19 actionable records under its
broader predicate. `bv --robot-insights` still reports zero dependency cycles; this
snapshot's data hash is `732fd3b8336b8a63`.

Focused remote execution at this exact source identity proved the integrated
open/write/read/drop/reopen test (`1 passed`, exit 0). The runnable example also reached
`OK: opened, wrote, dropped, reopened, agreed.` on its remote worker, but RCH then failed
artifact retrieval with `RCH-E309` and returned 102; it is therefore useful behavioral
output, not a green command. The authoritative full-repository gate is intentionally
reported with the landing rather than asserted inside bytes that it has not yet checked.
A subsequent local rerun of the example completed with the same agreement and exit 0;
it is local runnable-behavior evidence, not a substitute for that repository gate.

### Bridge consequence

The six current priorities from the 2026-08-16 delta remain correctly ordered. In
particular, `fgdb-5uw2` already owns the command-union/body/result/handler closure and is
in progress under another owner; this audit does not fork that lane. The next capability
step is still to turn the registered command universe into executable contracts, then
build real transaction/session ownership and the minimum GQL-to-GLA-to-Loom streaming
slice over the proven storage spine. Storage hardening is necessary groundwork, not a
substitute for those missing product layers.

---

## Current delta — 2026-08-16

### Product verdict

**FrankenGraphDB still is not the database product described by the README, but three
important parts of the embedded-spine verdict have materially improved since the
2026-08-09 snapshot.**

1. The pinned asupersync v0.4.7 source revision exposes
   `Runtime::request_cx_with_budget`, and the runnable example plus external-package
   probes obtain a production runtime context through that API with default features
   disabled. They narrow it to `CommitCx`; neither `Cx::for_testing` nor the LAB
   scheduler participates. Production context acquisition is no longer a blocker.
2. Normal open may select an authenticated durable Strata checkpoint from the root
   slot, verify its Chronicle chain commitment, reopen those immutable objects, and
   fold only the later suffix. A forced full rebuild remains the equivalence oracle.
   Open still recovers and verifies the Chronicle marker chain, so this is not a claim
   that total recovery cost is independent of retained history or checkpoint size.
3. Every post-D2 failure in the integrated publication path now fences the live handle
   in `NeedsAuthoritativeRecovery`. Reads, writes, and maintenance refuse stale state;
   deterministic failure/cancellation/ENOSPC/fsync-lie and Strata publication-crash
   tests prove ordinary reopen or same-handle recovery agrees with authoritative
   Chronicle replay exactly once. P0 `fgdb-l96k` is closed.

The product no-claim boundary is otherwise unchanged. `WriteBatch` is explicitly not a
transaction; the integrated database still hard-codes one graph, branch, and partition;
and there are no sessions, prepared statements, GQL/openCypher execution, production
transaction manager, secure-view query path, streaming typed results, product CLI,
server, Python package, or installable release. Tier R, archived anchors, product-scale
admission/spill, and the complete server-side larger-than-memory path do not exist. The
checkpoint path removes a full-fold bottleneck; it does not demonstrate the finished
larger-than-memory promise or any G1-G4 product gate.

### Current evidence boundary

This delta was measured at tracked commit
`4c0ee2a64da33d7e19eb68269f16af6c16a711b4`. The shared checkout contained three
untracked foreign artifacts (`.beads/beads.db-wal-cert`,
`.beads/beads.db-wal-cert-head`, and `tools/registry-check/src/claims.rs`); they were
neither edited nor removed. The older line-number index below remains bound to its
stated 2026-08-09 commits and must not be used as a current source-location map.

Current executable anchors are:

- `crates/fgdb/examples/open_a_database.rs`: a production-runtime create/write/read/
  drop/reopen program using `Runtime::request_cx_with_budget`;
- `crates/fgdb/src/lib.rs`: checkpoint-selected open, forced-rebuild equivalence,
  `NeedsAuthoritativeRecovery`, and purpose-narrowed `CommitCx` entrypoints;
- `crates/fgdb/tests/cx_probe.rs`: external-package production-context and recovery
  probes;
- `crates/fgdb-sim`: registered LAB-runtime, replay-completeness, forensics, and
  submodular-premise selectors, without a whole-product simulation claim;
- `registries/invariants.toml`: the still-deliberate zero-enforcement invariant ledger.

The final exit code for a documentation landing belongs in that landing's evidence
record; this document does not convert a focused test or an earlier full gate into proof
for later bytes.

### Current measured inventory

| Measure | 2026-08-16 value | Interpretation |
|---|---:|---|
| Workspace topology | 70 crate slots: 19 active, 50 planned, 1 reserved | embedded posture is live; server and CLI remain deferred |
| Cargo metadata | 20 packages, 20 library targets, 97 integration-test targets, 5 examples, 5 binaries | all five binaries are checker tools, not `fgdb` or `fgdbd` |
| Implemented Cargo benchmark targets | 0 | no §17 benchmark target or product baseline exists |
| Invariant registry | 20 IDs; 0 enforced clauses; 0 enforced invariants | no G1 invariant-coverage claim is active |
| Checker registry | 57 live rows, 42 stub rows | the gate surface grew substantially; a live row is not an invariant promotion |
| Formal lanes | 1 checked of 10; 9 declared | one Lean lane is checked; the other formal lanes remain future work |
| Tracker | 745 records: 275 open, 14 in progress, 8 explicitly blocked, 448 closed | `br stats` reports 284 dependency-blocked records and zero `br ready` records |
| Dependency graph | 1,568 edges; zero cycles | `bv --robot-insights` data hash `8ab0055c698b392d` |

`bv --robot-triage` reports 19 actionable records under its broader scoring predicate,
while `br ready --json` returns none. The highest-impact executable work remains the
G0 command-contract closure (`fgdb-5uw2`): the registries and extensive reserved Local
and Meta rows now exist, but the generated runtime union/body/result/handler bijection
required by plan section 5.1 does not. The tempting `RemoteGrantTargetRef` quick win is
correctly blocked: its plan-derived closed universe has 74 certified-remote target
kinds, but only one currently resolves while satisfying the construction DAG. Landing
a partial union would freeze a false contract.

The current bridge priorities are therefore:

1. finish source-grounded command families and then generate the executable closed
   command unions and exhaustive handlers rather than accumulating metadata forever;
2. land real transaction/session ownership and validation over the durable spine;
3. connect the minimum GQL-to-GLA-to-Loom slice to streaming embedded results;
4. add Tier R, bounded admission/spill, and product-scale recovery evidence;
5. promote invariant clauses only alongside their exact live checkers and negative
   controls;
6. expose one correct vertical slice through CLI/server/package surfaces instead of
   forking separate engines.

---

## Superseded reality snapshot — 2026-08-09

### The answer in one paragraph

**FrankenGraphDB now has a real, durable, low-level embedded storage spine; it is not
yet the database product described by the README.** The integrated path creates a
database, commits graph mutations through Chronicle's implemented D1/D2 capsule-and-
marker path, folds them into real Tier-D Strata objects, reads them, drops the handle,
and reopens the same data. That is a material advance over the older snapshot below.
It is still a
narrow fixed-coordinate API with no sanctioned production `Cx` acquisition path; the
example runs under lab, while a non-lab all-capability context is available only through
development/test internals. There are no sessions, prepared statements, GQL/openCypher
execution, production transaction manager, capability-filtered views, streaming result
API, server, product CLI, Python package, or installable release. Open still
reconstructs the partition by replaying history, and the in-memory `Snapshot` retains
every decoded block and patch. Thus neither the larger-than-memory promise nor any
G1–G4 product gate has been demonstrated.

The most important governance result is equally stark: all registered clauses under the
20 FG-INV IDs remain stubs, with **zero enforced clauses and zero enforced invariants**.
The gate and registry machinery is substantial, but machinery for checking claims is
not the same thing as a promoted claim.

### Evidence boundary

| Item | Measured state |
|---|---|
| Architecture source of truth | `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md`; README is explicitly a target-state 1.0 document |
| Tracked implementation read | architecture audit pinned to `62cb97970c4666a7228f73c38cb1625d150c775f`; focused landing probes pinned to later spine commit `d87ae4eb4104eae86a353bafd64dc6c2c68d6d74` |
| Shared-tree condition | concurrent uncommitted work existed in `fgdb`, `fgdb-chronicle`, registry tooling, tests, `.beads/`, and `rc/`; it was neither edited nor reverted by this audit |
| Runtime evidence | `62cb9797` did not compile the example because `RebuildError::Slot` lacked a `Display` arm; that fix landed in `d87ae4e`, where the focused reopen test and runnable example both exited 0 on an unchanged HEAD |
| Full repository gate | **UNRUN at this point in the audit**; it is run only after the exact bead export and final document content, with the final handoff as the result record |
| Tracker snapshot | 688 records after this audit's delivery task and P0 bug: 384 closed, 278 open, 19 in progress, 7 blocked |

The passing `d87ae4e` probes are focused behavior evidence, not a full repository-gate
result and not retroactive evidence for `62cb9797`. A line number, benchmark, or test
result without a content identity and tree condition is not durable evidence.

#### Pinned evidence index

The source line references in this table are bound to tracked commit `62cb9797`; the
registry/doc references were re-read in the same audit before this document was edited.

| Claim | Primary evidence |
|---|---|
| README is target state, not a readiness claim | `README.md:20`, `README.md:397` |
| advertised install/API/Python surfaces | `README.md:234-280` |
| advertised performance and verification state | `README.md:354-382` |
| spine is real and its omitted surfaces are explicit | `crates/fgdb/src/lib.rs:1-16`, `:50-71` |
| production `Cx` is unavailable | `crates/fgdb/src/lib.rs:113-145`; `crates/fgdb/tests/cx_probe.rs:20-46` |
| fixed coordinate and caller-supplied raw keys | `crates/fgdb/src/lib.rs:197-217` |
| `WriteBatch` is not a transaction | `crates/fgdb/src/lib.rs:543-552` |
| snapshot retains decoded structures and live version heads | `crates/fgdb/src/lib.rs:709-744` |
| actual async create/open surface | `crates/fgdb/src/lib.rs:905-958` |
| incremental write path | `crates/fgdb/src/lib.rs:1425-1556` |
| post-D2 errors can precede the live-state swap | `crates/fgdb/src/lib.rs:371-373`, `:1438-1665` |
| normal Chronicle open uses pass-through validation | `crates/fgdb-chronicle/src/commit.rs:254-258`, `:329-337`; `crates/fgdb-chronicle/src/validate.rs:96-110` |
| Strata store persists plain canonical bytes | `crates/fgdb-strata/src/store.rs:44-50` |
| low-level current/historical reads | `crates/fgdb/src/lib.rs:1680-1805` |
| open/recovery walks the marker chain | `crates/fgdb/src/lib.rs:2038-2105` |
| Tier-D scope and missing Tier R/anchors/selective reads | `crates/fgdb-strata/src/lib.rs:1-28` |
| measured point-read and recovery observations | `crates/fgdb/tests/cx_probe.rs:157-184`, `:285-304` |
| measured write-cost repair | `crates/fgdb/tests/write_cost_attribution.rs:1-16` |
| 20 invariants and deliberate zero-live ledger | `registries/invariants.toml:13-17`, `:44-50`, `:97-120` |
| one checked formal lane and stale header | `registries/proof_lanes.toml:74-80`, `:127-133` |
| topology and posture state | `docs/WORKSPACE_TOPOLOGY.md:10-18`, `:185-193` |
| normative gates G0-G4/W12 | `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:1311-1358` |
| normative invariant statements | `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:3200-3225` |

### What the code actually does

```text
WriteBatch
  -> canonical LogicalDeltaTemplate
  -> Chronicle capsule (authenticated encryption + RaptorQ symbols)
  -> fsync D1
  -> chained commit marker
  -> fsync D2                         commit authority
  -> Tier-D Strata block/patch/root   derived publication
  -> decoded in-memory Snapshot
  -> low-level vertex/edge/neighbour reads
```

The implemented slice follows the intended authority direction: the chained Chronicle
marker is the commit point, while Strata is rebuildable derived state. Capsule
identities, marker linkage, torn-tail handling, authenticated symbols, real durable
object writes, and reopen-from-identity are implemented rather than mocked. This is not
yet a proved sound authority boundary. Normal Chronicle open installs a
`PassThroughValidator`, so no production first-committer-wins, SSI, merge-ladder, or
constraint validator examines the draft before commit. The reference crate and
simulation harness provide focused differential and crash/recovery coverage for the
semantics they currently contain; no full-gate or invariant-promotion result upgrades
that to product correctness.

There is also an inferred post-D2 correctness hazard in tracked code. After the marker
is durable, the write path performs fallible fold, Strata, manifest, root-slot, decode,
and snapshot-refresh work. `self.writer` and `self.snapshot` are replaced only at the
end. A failure in between returns a rebuild error while leaving the handle callable
with pre-commit derived state; a later write can therefore derive from stale state and
publish a root omitting the prior durable commit until reopen. This audit filed P0 bug
`fgdb-l96k` to require an explicit poisoned/recovery-required state and deterministic
fault proof.

The durability envelope is narrower than the README also implies: Chronicle capsules
use authenticated encryption and RaptorQ, but the current `BlockStore` explicitly
persists ordinary Strata blocks, patches, roots, and manifests as canonical plain bytes.
Capsule composition for those derived objects is not yet wired.

The public integrated API is nevertheless lower level than the README contract:
`Database::create` and `Database::open` are asynchronous and require a caller-supplied
`CommitCx`, path, and raw keys. `WriteBatch` explicitly disclaims transaction semantics.
The database currently hard-codes one graph, one branch, and one partition. Production
context acquisition is unavailable at the pinned asupersync revision, so the runnable
example constructs a lab runtime and states that it is not a product binary.

On write, the persistent writer is now incremental rather than rebuilding all history.
On open, however, recovery still reads and decodes the full marker chain, folds all
history, and retains all decoded blocks and patches plus live version-chain heads in
memory. Tier R, archived anchors/checkpoints, stable-ID lookup, adaptive tier migration,
and selective
unopened-block reads do not exist. This is an honest early subset, not a disguised
`HashMap<VId, Vec<EId>>`, but it cannot yet support the finished larger-than-memory
claim.

### Current measured inventory

| Measure | Current value | Interpretation |
|---|---:|---|
| Workspace topology | 70 crate slots: 19 active, 50 planned, 1 reserved | embedded posture is live; server and CLI are deferred |
| Cargo metadata | 20 packages, 20 library targets, 96 integration-test targets, 5 examples, 5 binaries | all five binaries are checker tools, not `fgdb` or `fgdbd` |
| Implemented Cargo benchmark targets | 0 | the plan declares product targets, but no `benches/`, benchmark executable, activated manifest, committed baseline, or gate artifact exists |
| Invariant registry | 20 IDs; 0 enforced clauses; 0 enforced invariants | G1 invariant coverage is not active |
| Checker registry | 41 live rows, 42 stub rows | strong scaffolding with more than half the declared rows still stubbed |
| Formal lanes | 1 checked of 10; 9 declared | one Lean version-chain lane; five other Lean and four TLA+ lanes are not checked |
| Tracker | 688 records, 1,946 dependency edges, no cycles | 304 not closed; 279 dependency-blocked (91.8%) |
| Tracker types | 232 bugs, 22 epics, 5 features, 429 tasks | all 22 epics remain open |
| Recent tracker velocity | 77 closed in 7 days; 384 in 30 days | activity is high, but completion is concentrated before product gates |
| Source census | approximately 270,000 tracked Rust lines and 2,400 `#[test]` declarations | size/declared tests are not executed-test or quality claims |

`bv --robot-triage` reported `phase2_ready=true` and no graph cycles. Its 25
“actionable” records include states that `br ready --json` does not return, so the two
commands must not be compared as if they implement the same readiness predicate. The
highest-centrality blockers remain G0 identity registries, the W2 commit protocol,
command contracts, durable-format arms, the Appendix-A catalog, generated parsers,
object identity, the cost registry, and A01 reference roots.

### Vision-to-code matrix

| Vision goal | Verdict | Present reality | Missing exit condition |
|---|---|---|---|
| B1 — One Version Universe | **PARTIAL** | real Chronicle capsules, two-fsync markers, low-level historical reconstruction | production MVCC ownership, branches, bounded history/checkpoints, replication, subscriptions |
| B2 — Strata | **PARTIAL** | durable Tier-D blocks, patches, roots, compaction, reopen | Tier R, anchors, stable IDs, migration decision cards, selective larger-than-memory access |
| B3 — Loom | **NOT_STARTED** | operator types/catalog groundwork only | parser-to-algebra lowering, planner, vectorized/morsel execution, Free Join/WCO/factorization |
| B4 — Ripple | **NOT_STARTED** | delta and weight foundations only | DBSP engine, recursive fixpoint, incremental views/subscriptions/analytics |
| B5 — deterministic product | **PARTIAL** | canonical encodings, deterministic reference semantics, lab/simulation infrastructure | production plan certificates, decision cards, STRICT result replay, bounded registered DPOR/chaos campaigns |
| B6 — agent-native product | **NOT_STARTED** | branch semantics exist in the test oracle | production branch isolation, macaroons, pre-expansion masking, provenance graph, hybrid retrieval |
| Embedded Rust library | **PARTIAL** | durable low-level `Database` spine and runnable lab example | production `Cx`, sessions, prepared statements, transactions, streaming typed rows, multiple coordinates |
| Server `fgdbd` | **NOT_STARTED** | protocol/architecture contracts only | multi-database service and FGP/HTTP2/gRPC/WebSocket/Bolt surfaces |
| CLI `fgdb` | **NOT_STARTED** | robot-mode contract beads only | human CLI, versioned NDJSON robot mode, schema self-description, contract tests |
| GQL/openCypher/FQL | **NOT_STARTED** | language contracts and planned crates | generated parser, semantic analysis, algebra, execution, conformance corpus |
| Transactions | **NOT_STARTED in production** | SI/SSI semantics and anomaly logic in reference/testing crates | connection/session ownership, FCW/SSI, constraints, merge ladder, typed aborts |
| Warden security | **NOT_STARTED in production** | capability/context type groundwork | macaroon caveats, mandatory planner predicates, descriptor masking, audit proofs |
| Prism/fnx | **NOT_STARTED in production** | development-only generator use | snapshot bridge, cache/materialize/spill semantics, differential results |
| Beacon search/GraphRAG | **NOT_STARTED** | registries/design only | native vector/text/graph indexes and one hybrid retrieval operator |
| Fabric/admin/config | **NOT_STARTED** | contract and tracker work | catalog, migrations, backup/restore, validated configuration, multi-tenancy |
| Python ABI3 | **NOT_STARTED** | binding-semantics bead | ABI boundary, wheels, import/install tests |
| Larger-than-memory | **NOT DEMONSTRATED** | durable objects exist; integrated snapshot is materialized | bounded open/recovery, spillable operators, admission control, 1 TB scale proof |
| §17 performance | **UNPROVEN** | diagnostic probes and one write-cost improvement exist | gated harness, baselines, variance policy, scale data, complexity witnesses |
| Verification ladder | **PARTIAL** | reference oracle, focused simulations and crash/bit-rot tests, one Lean lane | 20 enforced invariants, remaining formal lanes, TCK/differentials, bounded DPOR, fuzz and scale closure |

No row above meets the plan's G4 definition. G0 has substantial apparatus but its epic
remains open. G1 cannot pass while the invariant registry intentionally expects zero
enforced clauses; G2 and G3 depend on missing transaction, query, incremental,
security, and product layers; G4 additionally requires conformance and benchmark
evidence that does not yet exist.

### Performance reality

There are now measurements, which corrects the historical claim that no number had
ever been taken, but they are diagnostics rather than product gates. A shared-box
supernode probe recorded p99 around 121.7 us against a 15 us target, and a leaf probe
around 69 us. The recovery probe documents full replay as linear and therefore
incompatible with the 1 TB recovery objective without checkpoints/anchors. Conversely,
the persistent-writer work removed a history-proportional write regression: its
attribution test records roughly flat later writes where the old path grew sharply.

These numbers are useful direction. They are not §17 passes: they are unpinned,
non-isolated observations without the required benchmark binary, hardware manifest,
sample distribution, committed baseline, regression policy, or flamegraph evidence.

### Documentation conflicts found

The master plan is the architectural source of truth. Machine registries are the exact
authorities for the rendered topology, threat model, durable catalog, invariants, and
cost contracts they own. Several README statements are useful aspirations but conflict
with those sources and must be repaired under the existing documentation-sync work:

1. “Unbounded history” conflicts with the plan's explicit configurable retention and
   compaction policy.
2. A universally zero-copy Prism bridge conflicts with the current fnx slice contract,
   which permits cache, materialize, and spill behavior.
3. “Every result” replay overstates the plan's eligible STRICT-result contract and its
   evidence-closure preconditions.
4. “Sharding is activation, not rewrite” conflicts directly with the plan, which calls
   sharding non-configuration-only activation requiring new protocols, its own
   workstream, and a separate proof gate after G4.
5. A single sub-100-us figure for snapshot and branch creation hides the plan's
   distinction between an in-memory handle and a durably forked branch requiring fsync.
6. README install commands, signed binaries, Rust installation, and Python wheels have
   no implementation or release pipeline yet.
7. README reduces mutable state to one `manifest.root` and truth to an unqualified
   commit stream, while the master defines two fixed root-pointer slots and, after
   retention cuts, recovery from an installed checkpoint plus the complete retained
   logical-command suffix, including nontransaction control commands.
8. README states every performance gate already has a bench binary/baseline/variance
   budget/flamegraph and presents the complete lab/formal/invariant apparatus as live;
   the repository has zero benchmark targets, one checked formal lane, and zero
   enforced invariants.
9. README promises a synchronous embedded API whose runtime is internally owned; the
   current slice is asynchronous and requires a caller-supplied `CommitCx`.

Two implementation-adjacent descriptions are also stale: `fgdb`'s crate-level prose
still says the root pointer is undefined and every write rebuilds history, while the
current code contains a dual-slot root and incremental writer; `proof_lanes.toml`
claims zero artifacts even though one Lean lane is checked. These are documentation
bugs, not evidence that the missing product layers exist.

### Gap analysis: the seams that matter next

The largest gaps are not isolated missing functions. They are executable seams between
already-serious foundations:

1. **Authority to bounded recovery.** Preserve Chronicle as the source of truth while
   making root/checkpoint open bounded, validated, and fail-closed. Add load admission
   and use the authenticated installed checkpoint plus complete retained logical-command
   suffix as the post-retention authority. Full genesis replay remains only an
   oracle/fallback where that complete history is actually retained.
2. **Durability to real transactions.** Replace raw `WriteBatch` ownership with
   session/transaction lifecycle, first-committer-wins and SSI validation, constraints,
   graph intents, cancellation, and deterministic merge-ladder evaluation.
3. **Stored graph to executable language.** Land the minimum GQL slice end to end:
   generated grammar -> typed AST -> GLA -> Loom -> streaming results, always
   differential against the reference oracle. Parser breadth without algebra is not a
   product increment.
4. **Context types to production authority.** Obtain the upstream asupersync production
   `Cx` acquisition contract, thread purpose-narrowed contexts through storage and
   transactions, and make lab VFS injection cover Strata as well as Chronicle.
5. **Single-machine spine to larger-than-memory behavior.** Tier R, anchors, stable-ID
   lookup, buffer/scratch admission, spill, and adaptive migrations must compose before
   any scale or throughput claim.
6. **Semantics to product surfaces.** Only after one correct query/transaction seam,
   expose the same contract through the embedded library, CLI robot mode, server
   transports, and Python ABI rather than creating four divergent engines.
7. **Claims to enforced invariants.** Promote invariant clauses only with their live
   checker and negative evidence; keep the current zero count honest until then.
8. **Source tree to distributable product.** Treat installers, signed artifacts,
   crates/Python packaging, and README command smoke tests as conformance surfaces.

Before those seams can close, the normative appendices need executable coverage rather
than implied completeness:

| Normative contract | Current state |
|---|---|
| Appendix A — formats/identity/references | substantial catalog scaffold and live source/projection/closure checker, but catalog/field/union/identity blockers remain open; G0 is not frozen |
| Appendix B — graph intent vocabulary | partial intent/template and reference semantics exist; the complete registered intent-to-canonical-final-effect vocabulary is not implemented end to end |
| Appendix C — GLA inventory | **NOT_STARTED** as a complete registered operator/algebra/executor surface |
| Appendix D — FGP protocol | **NOT_STARTED** as a product protocol/state-machine implementation; only contracts and beads exist |
| Appendix F — invariants | all 20 IDs materialized, every clause still stub, zero enforced |
| Appendix G — operation costs | plan table and ownership bead exist; no complete live operation-cost registry or derivability gate |

### Threat and trust boundary

“Memory safe” and “capability-scoped” must not be read as a stronger adversary model
than the project claims. The current threat registry assumes a trusted host/process and
administrator boundary, no TEE, crash-fault rather than Byzantine Raft, and explicit
external authority/time/continuity assumptions. `DirectoryBound` continuity does not
protect against every volume rollback. Side-channel/leakage properties are measured
and bounded, not globally proved; BFT, FHE, ORAM, malicious-host confidentiality, and
universal noninterference are outside the declared 1.0 boundary. Warden is not yet a
production enforcement layer, so these are future-contract qualifiers, not current
security guarantees (`docs/THREAT_AND_TRUST_MODEL.md:56-89`, `:197-265`).

### Bead coverage and the one uncovered delivery surface

The live graph already has broad ownership for every engine and product subsystem:
Chronicle (`fgdb-epic-w2-6hc`), Strata (`fgdb-epic-w3-umx`), Loom
(`fgdb-w5-executor-olp` and related epics), Ripple (`fgdb-epic-w6-65w`), Beacon
(`fgdb-epic-w7-xmk0`), Prism (`fgdb-epic-w8-syz`), Fabric/product
(`fgdb-epic-w10-mhq`), transactions (`fgdb-epic-w4-7en`), Warden/security
(`fgdb-w1-authz-policy-10y`, `fgdb-w9-enforcement-j0fg`), Aegis
(`fgdb-epic-w11-5r8`), server and protocols
(`fgdb-w10-server-rte`, `fgdb-w10-fgp-core-5b1`, `fgdb-w10-adapters-b1f`,
`fgdb-w10-bolt-a0s`), CLI robot mode (`fgdb-huu9`), language
(`fgdb-g0-language-contracts-54g`, `fgdb-w5-parsers-nje`), embedded API
(`fgdb-w10-embedded-54r`), Python (`fgdb-w10-python-kkb`), and performance/verification
(`fgdb-epic-perf-4xe` and `fgdb-verif-*`). Duplicating these would make the graph worse.

One genuine `NO_BEAD` surface survived a search of open and closed records: delivery
of the README's installable artifacts. This audit created
`fgdb-epic-w10-mhq.1`, **“Ship and verify installable release artifacts for CLI,
server, Rust library, and Python wheels.”** It owns signed target binaries, manifest
verification and rollback-safe `scripts/install.sh`, the Rust publication/install
story, the ABI3 wheel matrix, and smoke tests for every advertised install command. It
depends on the existing CLI, server, embedded, Python, and production-context work and
does not duplicate their implementations. `fgdb-gate-g4-3uc` now depends on this
delivery task, so the graph cannot report G4 complete while those artifacts are absent.

The architecture re-review found one additional unowned P0 correctness seam and filed
`fgdb-l96k`, **“Poison or recover the Database handle after any post-D2
derived-publication failure.”** `fgdb-j0vu` now depends on it. The task requires fault
injection across every D2-to-snapshot boundary, typed committed-needs-recovery state,
refusal of stale reads/writes, and a two-write proof that no derived root can omit an
already-durable commit.

Tracker reconciliation was recorded without premature closure:

- `fgdb-ge6a` has landed root-slot code but still owns fast-open/catalog residue.
- `fgdb-j0vu` no longer accurately says there is no runnable human path; it still owns
  the missing supported product posture and attributable full-gate evidence.
- `fgdb-g0-doc-sync-usq` now carries the README/source/proof-lane drift above.
- the stale close reason on `fgdb-0b8r` was annotated; production context acquisition
  remains live in `fgdb-r8fa`.

### Dependency-ordered bridge

This is a bridge through existing beads, not a second planning system.

**Track 0 — land and attest the current spine.** Fix P0 `fgdb-l96k`, reconcile
`fgdb-ge6a` and `fgdb-j0vu`, finish `fgdb-r8fa`, correct the stale code/docs, and obtain a clean
current-HEAD `scripts/check.sh` result. Exit: the durable example is supported,
production-context construction is real, open is bounded by an explicit policy, and
every claim is bound to the landed commit.

**Track 1 — close G0's actual critical frontier.** Prioritize the central blockers
identified by `bv`: identity registries, commit protocol, command/language contracts,
format arms, Appendix-A rows, generated parsers, object identity, cost registry, and
A01 reference roots. Exit: G0's own acceptance criteria pass; not merely a large count
of registry files.

**Track 2 — prove one secure transactional query slice (G1).** Complete the W2/W3
authority and storage seam, W4 transaction ownership/SSI/constraints, the minimum W5
language-to-Loom path, reference differential, and simulation/fault oracles. Promote
only the invariants exercised by live checkers. Exit: one supported transaction can
prepare and stream a deterministic query result, crash/reopen, and reproduce it under
lab with no unauthorized observation.

**Track 3 — compose B1–B4, Beacon, and Prism (G2).** Add Tier R/anchors/bounded open,
full Loom families and spill, Ripple incremental maintenance/recursion, branches and
subscriptions, then W7 Beacon and W8 Prism in their declared order. Exit: Local
one-version-universe workloads pass under bounded memory, including tail-correct
indexes, safe ANN roots, authorized Prism projections, recovery, conformance, and
complexity witnesses.

**Track 4 — secure, expose, and replicate the product (G3).** Continue in declared
order through W9 Warden, W10 Fabric, and W11 Aegis. Expose the same engine through
embedded sessions, server transports, CLI robot mode, and Python; add multi-member
replication here rather than in G2. Exit: the networked/secured/replicated product gate
passes, including pre-expansion authorization and negative-observation tests.

**Track 5 — earn G4 and ship.** Build the §17 benchmark/conformance apparatus, run the
1 TB and crash/recovery campaigns, finish installer/signing/package automation under
`fgdb-epic-w10-mhq.1`, and smoke-test every README command. Only after G4 should W12
sharding begin, because the master plan defines it as non-configuration-only activation
with new protocols, a separate workstream, and a separate gate.

### Ambition rounds

#### Round 1 — make recovery an indexed authority proof

The dual-slot root should evolve into a bounded recovery certificate: authenticate the
root, validate its Chronicle high-water mark and Strata closure, admit the referenced
working set under a memory budget, and fall back to the authenticated checkpoint plus
complete retained logical-command suffix on inconsistency. Full replay is admissible
only in a posture that retains the complete history; otherwise recovery fails closed.
This is more ambitious than “open faster” because it keeps the commit stream
authoritative while turning recovery cost into an explicit, testable contract. Crash
and corruption campaigns should mutate every root/checkpoint boundary and prove both
bounded success and fail-closed fallback.

#### Round 2 — use the first transaction/query slice as an invariant crucible

Do not grow a broad parser ahead of execution. Choose one standards-keyed GQL slice
that exercises visibility, adjacency expansion, projection, deterministic ordering,
and a write conflict. Carry it through generated parsing, typed GLA, Loom, capability
predicates, Chronicle/Strata, reference differential, SSI oracle, cancellation, and
replay evidence. This single vertical slice forces the architectural seams to agree
and provides a reusable conformance harness for every later language feature.

#### Round 3 — make distribution part of semantic conformance

An installer or wheel is not a marketing afterthought. Each published artifact should
embed its format/protocol/policy epochs, invariant/checker manifest, source revision,
and signed evidence digest. CI should install into clean target environments, execute
the README's embedded/CLI/server/Python examples, crash/reopen the same portable
fixture, and reject incompatible or unsigned upgrades. This turns packaging into the
last link of the database's determinism and provenance story.

### Risks and mitigations

| Risk | Current signal | Mitigation / owning frontier |
|---|---|---|
| Full replay and retained decoded history defeat scale | integrated open is O(history) and snapshot holds all blocks | bounded root/checkpoint recovery, anchors, admission, spill; `fgdb-ge6a` plus W3 |
| Post-D2 failure leaves a stale live handle | durable marker precedes fallible derived publication and final state swap | poison or authoritatively recover before any later read/write; P0 `fgdb-l96k` |
| Transaction claims get inferred from durability | `WriteBatch` explicitly is not a transaction | W4 lifecycle/FCW/SSI/constraints before ACID language |
| Security becomes a post-filter | no production planner or Warden path | capability predicates and descriptor masking in the first vertical slice |
| Registry volume is mistaken for invariant enforcement | 41 live checker rows but 0 enforced FG-INV clauses | report both counts; promote clause and checker atomically |
| Lab-only success is called a product API | production `Cx` remains upstream-blocked | finish `fgdb-r8fa`; retain compile-time tripwire |
| Direct Strata filesystem access weakens fault injection | Chronicle has VFS seam; integrated Strata uses `std::fs` | thread purpose-scoped VFS through Strata before recovery claims |
| Strata durability is mistaken for encrypt-and-code coverage | current BlockStore writes canonical plain derived bytes | wire derived objects through the registered Chronicle/FEC/encryption contract before making the README claim |
| Performance anecdotes become benchmark claims | diagnostic probes, no benchmark target | §17 harness, manifests, distributions, baselines, witnesses |
| README becomes a competing specification | several direct conflicts with master plan | `fgdb-g0-doc-sync-usq`; label target state and link precise gates |
| Tracker progress hides critical-path blockage | 91.8% of not-closed work dependency-blocked | use `bv` centrality/impact, finish deepest blockers, avoid duplicate beads |
| Product exists in source but cannot be obtained safely | no installer, signing, Rust/Python publication | `fgdb-epic-w10-mhq.1` after surface implementations |

### Refinement record

The skill's bridge and beads were refined to stability rather than accepted after the
first inventory:

1. **Completeness pass:** compared every README, master-plan, appendix, and threat-model
   promise against source and all tracker states. Found the uncovered delivery surface
   and created `fgdb-epic-w10-mhq.1`.
2. **Overlap pass:** searched headline subsystem, protocol, language, Python, CLI,
   embedded, security, and performance terms across open and closed records. Reused the
   existing beads and removed no work; the new task owns only artifact delivery.
3. **Dependency pass:** attached the new task under W10 and made CLI, server, embedded,
   Python, and production-context completion explicit prerequisites; then made the G4
   gate depend on the delivery task rather than leaving release closure as prose.
4. **Executability/evidence pass:** strengthened its acceptance criteria to require
   signed manifests, failure modes, clean-environment smoke tests, and durable logs;
   annotated stale tracker claims instead of closing them from code existence alone.
5. **Conflict/graph pass:** peer re-derivation found that delivery did not actually
   block G4 and that post-D2 failure left an unowned stale-handle risk. The G4 edge was
   repaired, P0 `fgdb-l96k` was created and made a spine prerequisite, and graph triage
   was rerun. The graph remained cycle-free; no further uncovered or overlapping bead
   survived, so refinement stopped.

### Validation and provenance

- Tracked architecture read: `62cb97970c4666a7228f73c38cb1625d150c775f`;
  focused landing probes: unchanged `d87ae4eb4104eae86a353bafd64dc6c2c68d6d74`.
- `rch exec -- cargo test ...` did not run because remote build admission was paused
  during daemon-restart remediation; this is an infrastructure refusal, not a test
  result.
- Tracked-HEAD focused test at `d87ae4e`:
  `cargo test -p fgdb --test spine open_write_read_drop_reopen_returns_the_same_graph -- --exact`
  exited 0 with 1 passed and 23 filtered.
- Tracked-HEAD example at `d87ae4e`: `cargo run -p fgdb --example open_a_database`
  exited 0 and demonstrated create/write/read/drop/reopen. Cargo also printed the
  expected parse diagnostic from asupersync's intentionally malformed migration fixture;
  both commands nevertheless exited 0.
- Exact export through `scripts/br_sync.sh` was attempted for the eight audit-owned
  record IDs and refused before writing: the shared database had 50 dirty records, which
  cannot fit an eight-ID intent. `.beads/issues.jsonl` remained byte-unchanged; foreign
  records were not widened into this landing.
- The first full `bash scripts/check.sh` attempt started and ended at `d87ae4e` but
  exited 1 after concurrent edits moved `crates/fgdb-reference/src/lib.rs` and
  `crates/fgdb/src/lib.rs`. Shell lint passed; file coverage was voided and 8 of 9 core
  plus all 24 registered live gates were explicitly `UNRUN`. This is neither a test
  failure nor a pass.
- The exact full-gate command, start/end identities, and exit status are intentionally
  recorded in the final handoff after the final document and exact bead export exist;
  they are never inferred from these partial cargo results.

### Corrections to the historical snapshot

The older analysis below remains valuable as a dated account, but these statements are
now superseded:

- “no library” and “nothing a user could open/write/read” are false after the durable
  `fgdb` spine; the supported session/query product remains absent.
- topology is now 70 slots/19 active, not 71/19.
- the tracker is now 688 records, not 600 or 633.
- performance observations now exist, although no §17 benchmark gate exists.
- Chronicle now has an asupersync VFS seam; the fault-injection closure is still
  incomplete because integrated Strata uses direct filesystem calls.
- the root pointer and incremental writer now exist; bounded fast open remains open.
- “there is no significant `NO_BEAD` gap” was almost true for subsystems but missed
  release/install/package delivery; that gap now has `fgdb-epic-w10-mhq.1`.

---

## Historical snapshot — 2026-07-31 through 2026-08-01 (superseded)

## Phase 1 — Where we REALLY are

### The one-sentence answer

**FrankenGraphDB has an excellent, deeply-verified storage and semantics foundation and
is not yet a database.** Nothing a user could install, open, write to, or query
exists — not the library, not the server, not the CLI, not the query language, not
the transaction manager. The 46% bead-completion figure is real and it is measuring
something other than product progress.

### The numbers, measured not estimated

| Measure | Value | Source |
|---|---|---|
| Beads closed | 274 of 600 (46%) | `br list --status=closed` |
| — of those, registry/catalog/gate/ceremony | **129 of 274 (47%)** | title match on `appendix\|registry\|catalog\|g0-\|census\|pin\|gate\|doc\|adr\|audit\|provenance\|classif` |
| Beads open | 295 (67 of them still ceremony) | `br list --status=open` |
| Crates activated | **19 of 71 (27%)** | `registries/workspace_topology.toml` |
| Workspace tests | 2150 passing, 0 failing, 119 suites | `cargo test --workspace` |
| Benchmark harness | **none exists** | no `benches/` directory anywhere |
| Product binaries | **none** | only `tools/registry-check` has `[[bin]]` |

### What IS working — and it is genuinely good

These are not stubs. Each is law-bound, mutation-tested, and differentially verified.

1. **Chronicle — the commit stream (B1).** Two-fsync protocol with the marker as the
   commit; torn-tail discrimination (missing bytes = crash, wrong bytes = damage);
   §5.1 identity pipeline; RaptorQ erasure coding with per-symbol MACs; a crash-point
   matrix; and a bit-rot campaign proving healing to the repair budget and fail-closed
   past it, on real capsule files.
2. **`fgdb-reference` — the §15 semantics oracle.** Complete against §15's list:
   values, visibility, path modes (all four with discriminating graphs), intents with
   the mismatch trichotomy, temporal selectors, branches with historical forking,
   snapshot isolation with an anomaly oracle, an SSI dangerous-structure checker,
   workspace generations, and terminal attempt semantics.
3. **`fgdb-strata` tier D (B2, first tier only).** Canonical delta-block format,
   content identity, partition roots, cross-block merge under tombstone supersede,
   the tier-D writer, a durable content-addressed store, reopen-from-identity, and
   compaction — with three differentials against the oracle.
4. **`fgdb-sim` — the differential harness.** Durability-vs-semantics, the whole write
   path, concurrency-vs-durability, and Strata-vs-oracle.
5. **The registry/gate apparatus.** Genuinely rigorous, and see the gap below.

### What is NOT working — by vision goal

| # | Goal (source) | Status | Evidence |
|---|---|---|---|
| 1 | B1 One Version Universe — commit stream, MVCC, time-travel, branches | **PARTIAL** | Stream + branches + time-travel exist in the ORACLE. Replication and change subscriptions: no code. |
| 2 | B2 Strata — three temperature tiers | **PARTIAL** | Tier D done. Tier R (sealed CSR runs) and archived anchors: no code. `fgdb-props`, `fgdb-buffer`, `fgdb-scratch` planned. |
| 3 | B3 Loom — Free-Join/WCO execution | **NOT_STARTED** | All 8 `loom` crates planned. No algebra, no planner, no executor. |
| 4 | B4 Ripple — DBSP Z-set incremental engine | **NOT_STARTED** | All 3 `ripple` crates planned. |
| 5 | B5 Determinism as a product feature | **PARTIAL** | Doctrine-4 canonicality is enforced everywhere and lab-runtime tests exist. Plan certificates, decision cards, `replay(certificate, seq, seed)`: no code. |
| 6 | B6 Agent-native — branch-per-agent, macaroons, GraphRAG | **PARTIAL** | Branch isolation exists in the oracle. Macaroon authz, provenance edges, hybrid retrieval: no code (`warden`, `beacon` layers empty). |
| 7 | **Embedded library `fgdb::Database::open`** | **NOT_STARTED** | `fgdb` crate planned. **No user entry point exists.** |
| 8 | **Server `fgdbd` (FGP/HTTP2/gRPC/WS/Bolt)** | **NOT_STARTED** | `fgdb-server`, `fgdbd`, `fgdb-protocol`, `fgdb-bolt` all planned. |
| 9 | **CLI `fgdb` with robot mode** | **NOT_STARTED** | `fgdb-cli` planned. No binary. |
| 10 | **GQL (ISO 39075) + openCypher surface** | **NOT_STARTED** | `fgdb-gql`, `fgdb-cypher` planned. No parser, no grammar. |
| 11 | §17 performance gates (≥8M point-reads/s, p99 <15µs) | **UNPROVEN** | **No benchmark harness exists at all.** Not one number has been measured. |
| 12 | Larger-than-memory as a property of every operator | **NOT_STARTED** | No operators exist. |
| 13 | Lab VFS before the first fsync (§15, W1) | **VIOLATED** | `fgdb-1xtp`: the first fsync shipped long ago; chronicle uses blocking `std::fs`. One of four fault classes (bit rot) closed; fsync lies, interior tears, ENOSPC still uninjectable. |
| 14 | Verification ladder | **PARTIAL, STRONGEST AREA** | Oracle, differentials, crash matrix, erasure campaigns all real. TCK, Neo4j/Memgraph differential, DPOR exploration, formal lanes: no code. |

### Would completing all open+in-progress beads close the gap?

**Yes on paper, no in practice, and the distinction is the finding.** Bead coverage
is not the problem: every headline goal has beads (GQL 2, embedded 2, fgdbd 2, CLI 1,
Loom 2, Ripple 4, Prism 4, vector 5, benchmark 1). There is no significant `NO_BEAD`
gap. The problem is **throughput allocation**:

- 47% of everything closed so far is registry/catalog/gate work.
- 67 of 295 open beads are more of the same.
- The G0 "ready" queue is almost entirely catalog beads, so an agent that picks up
  `br ready` work is overwhelmingly steered into ceremony.

The swarm is not drifting because it lacks direction. It is drifting because **the
work that is easiest to pick up is the work that does not build the product.**

### What is actually blocking us

1. **No spine.** There is no `fgdb` crate, so there is nowhere for a user-facing API
   to live and nothing to integrate the pieces into. Chronicle, Strata and the oracle
   are three islands that only meet inside test files in `fgdb-sim`.
2. **No query path.** Between "Strata answers `neighbours(v, rel, as_of)`" and "a user
   runs a GQL query" there is: a parser, a binder, an algebra, a planner, an executor.
   All eight `loom` crates are planned; none started.
3. **No transaction manager.** `fgdb-txn` is planned. Today's transaction semantics
   live entirely in `fgdb-reference`, which is explicitly *never shipped*.
4. **Performance is entirely unmeasured.** §17 sets hard numeric gates and there is no
   harness. Every performance claim in the README is currently unfalsifiable.
5. **The lab VFS ordering violation (`fgdb-1xtp`)** blocks honest crash coverage for
   three of four fault classes and gets more expensive with every fsync added.
6. **Ceremony gravity**, as measured above.

---

## Phase 2 — The bridge plan

The ordering principle: **build the thinnest possible vertical slice that a human can
run, then thicken it.** Every horizontal layer completed before there is a vertical
path is a layer whose integration risk is unmeasured.

### Track A — The spine (unblocks everything, nothing else unblocks it)

- **A1. Activate `fgdb`** — the embedded library crate. `Database::open(path)`,
  `Database::open_in_memory()`, a session handle, and a `close`. Internally: open a
  Chronicle coordinator, recover, and expose a read handle over Strata. This is the
  first place the three islands meet in *production* code rather than in a test.
- **A2. Bind Strata into the durable object graph** — currently blocked and blocking.
  Neither `DeltaBlockVersion` nor `PartitionRoot` is a landed Appendix A kind, and no
  field anywhere references a partition root, so a database cannot find its partitions
  on open. Register both formats first, *then* add the binding; the Strata side already
  guarantees a partition reopens from a 32-byte identity. See `fgdb-ge6a`.
- **A3. Write path in the library** — `Database::write(|txn| ...)` producing a real
  commit, using the effect vocabulary that already exists.
- **A4. Read path in the library** — adjacency and vertex reads at a snapshot, served
  from Strata, falling back to stream replay only on a cold partition.

### Track B — The narrowest real query surface

- **B1. `fgdb-gql` lexer + parser for a deliberately tiny subset**: `MATCH (a)-[:R]->(b) RETURN b`.
  Nothing else. Grammar fuzzed from day one per §15.
- **B2. `fgdb-algebra`** — the operator vocabulary for that subset only: scan, expand,
  project.
- **B3. `fgdb-exec`** — a single-threaded interpreter over Strata. Explicitly a subset
  of Loom, never a substitute: no factorization, no WCO, no vectorization, and the
  module doc must say so.
- **B4. Differential**: every query result must equal `fgdb-reference`'s answer for the
  same graph. This is the instrument that makes the whole surface honest.

### Track C — Measurement (currently zero)

- **C1. A benchmark harness** — `fgdb-bench`, activated, with the §17 metrics named as
  the things it measures even when the numbers are bad.
- **C2. Publish the first honest numbers**, however unflattering. A measured 400k
  point-reads/s against a target of 8M is *progress*; an unmeasured claim of 8M is a
  liability, and doctrine 7 forbids reporting a non-durable benchmark mode as a result.

### Track D — Close the fault-injection violation

- **D1.** `fgdb-1xtp` step 1: make Chronicle's durable path async against asupersync's
  `Vfs`/`VfsFile`, behaviour unchanged, `UnixVfs` underneath.
- **D2.** The faulting VFS: fsync lies, interior torn writes, ENOSPC, latency.
- **D3.** Re-express the crash matrix against it, removing nothing until the new tests
  are mutation-proven at least as strong.

### Track E — Steering the swarm away from ceremony gravity

- **E1.** Stop treating G0 catalog completeness as a prerequisite for engine work. It
  is a gate on *shipping*, not on *building*. This got **stronger** on 2026-08-01: the
  single exception this document claimed — the root-manifest/Strata registration in
  `fgdb-ge6a` — turned out on measurement not to be one. Registration there is waiting
  on the engine, not the other way round. **No catalog bead currently blocks any
  engine bead.**
- **E2.** Every new engine bead must name the differential that proves it.

---

## Phase 4 — Ambition pass

Three escalation rounds against Phase 2. What survived is below; Phase 2 stands as
the *ordering*, and this is what makes it worth doing.

### Round 1 — the oracle is a GENERATOR, not just a checker

Phase 2 treats `fgdb-reference` as something to compare against. That is thinking far
too small. **We have a complete, executable specification of the logical semantics
before the engine exists.** Almost no database has ever had that, and every one of
them pays for it forever in "which of these two is right" arguments. The leverage:

- **Model-based history generation.** Generate random *valid* graph histories
  (intents, not effects — so the generator cannot produce a state the system could not
  reach) plus random queries. Run both engines. Compare. This scales verification
  without a human writing each case, and it is the only way the combinatorics of
  MVCC × branches × path modes × temporal selectors ever get covered.
- **Shrinking.** A failing generated history must shrink to a minimal one. Without
  shrinking a 400-step counterexample is a curiosity; with it, it is a bug report.
- **Under DPOR.** asupersync ships deterministic partial-order reduction. Generated
  histories run under explored schedules make concurrency bugs *seeds*, not folklore.
- **The oracle also generates EXPECTED PLANS.** A query the oracle answers by brute
  force gives the executor a result to match AND a cardinality to compare its estimate
  against — a free planner-accuracy signal from day one.

This turns verification from a cost centre into the thing that lets the engine be
written fast and aggressively, because a wrong optimization is caught in seconds by a
generator rather than in months by a user.

### Round 2 — the vertical slice is the wrong shape without these three

**(a) Branch creation must be O(1) from the first line of engine code.** plan:451
requires branch creation to add "only metadata and key wraps", with reads following
branch-parent links "atop structurally shared objects". The oracle copies, is O(n),
and says so at the definition. If the engine's first storage structures are not
*persistent* in the Driscoll–Sarnak–Sleator–Tarjan sense — path-copying or fat-node,
with confluent persistence for merge — then B1's git-style branching and B6's
branch-per-agent isolation are both economically dead, and retrofitting persistence
into a mutable structure is a rewrite, not an optimization. **This is the single
highest-cost mistake available to us right now**, because tier D is young enough to
absorb the decision and tier R is not yet written.

**(b) Tier migration is a competitive-analysis problem, and the plan already says so
without naming it.** "Expected benefit must exceed conversion cost plus uncertainty",
plus minimum dwell time, plus a pinned deterministic fallback — that is *ski-rental*
with hysteresis. Naming it buys the actual result: a 2-competitive deterministic
policy (convert once accumulated read penalty equals conversion cost) and a
randomized e/(e−1)≈1.58-competitive one. Since B5 forbids nondeterminism without a
declared certificate, the deterministic 2-competitive rule is the pinned fallback and
the learned estimator is the advisory feature on the decision card. **The decision card
schema should carry the competitive ratio it is claiming**, so a policy change is a
measurable regression rather than a vibe.

**(c) Measurement must be adversarial, not confirmatory.** A benchmark that measures
what we built measures nothing. §17's gates (≥8M point-reads/s, p99 <15µs warm) need a
harness that also generates *hostile* shapes: degree skew (power-law, so the
supernode path is exercised), adversarial branch depth, worst-case version-chain
length, and cold-partition reopen. Publish the bad numbers.

### Round 3 — the mathematics that actually buys us something

Not decoration; each of these maps to a named plan requirement that is currently
hand-waved.

| Technique | Where it lands | What it buys |
|---|---|---|
| **Generic Join / AGM bound** (Ngo–Porat–Ré–Rudra 2012) | B3 Loom's WCO operator | Worst-case optimality is a *theorem* about the join, not a hope. Cyclic queries (triangles) go from O(n²) to O(n^1.5). This is the difference between "we have a join" and "we beat Neo4j on the queries that matter". |
| **Factorised representations** (Olteanu–Závodný) | Loom's intermediates | Results represented as a factorised d-tree are exponentially smaller than flat tuples; the plan's "factorized intermediates" is exactly this and it needs the representation, not just the word. |
| **DBSP** (Budiu et al. 2022) | B4 Ripple | Z-sets with a differentiation/integration calculus make recursive queries, views, subscriptions and analytics **one** engine. The commit stream is already a Z-set stream — this is the least-cost bet on the board and it is entirely unstarted. |
| **Elias–Fano / quasi-succinct** (Vigna 2013) | Tier R sealed runs (`fgdb-w3-tier-r-0tj` literally says "EF offsets") | Monotone offset sequences at ~2 bits/element above the information-theoretic bound with O(1) random access. Makes larger-than-memory adjacency scans real rather than aspirational. |
| **Ribbon filters** (Dillinger–Walzer 2021) | Block skipping in the partition root | Strictly dominates Bloom at the same false-positive rate with ~30% less space and better locality. The root already carries ranges; a ribbon filter per block turns "skip by sequence" into "skip by key". |
| **PGM-index / piecewise-linear** (Ferragina–Vinciguerra 2020) | Ordinal maps (`fgdb-w3-vertex-directory-nde`) | A learned index with *worst-case guarantees* — unlike naive learned indexes — so dense-ordinal lookup keeps Kùzu-class performance without ordinals becoming identity. |
| **HyperLogLog / Theta sketches** | Planner cardinality estimation — `fgdb-sketch` is already an ACTIVE crate | Mergeable cardinality estimates across partitions with bounded error, which is exactly what a cost model needs and what the sketch crate exists for. It is active and unused by any planner because no planner exists. |
| **Count–Min / CountSketch** | Degree-skew detection for the supernode path | Tells the executor when it is about to expand a hub, which is when the WCO path and the factorised representation earn their keep. |
| **Persistent / confluently-persistent structures** (DSST 1989) | Tier D and tier R, urgently | See Round 2(a). O(1) branch fork with structural sharing, and confluence for merge. |
| **Ski-rental / competitive analysis** | Decision cards for tier migration | See Round 2(b). |
| **Fekete et al. 2005 dangerous structures** | Already implemented in `fgdb-reference::ssi` | Cited because it is the proof that this approach works: the SSI oracle is a theorem-backed checker, not a heuristic, and it is already load-bearing. |

**The synthesis.** Generic Join + factorised intermediates + Elias–Fano runs is not
three optimizations; it is one coherent claim — *the storage layout is already the
trie the WCO join wants to walk*, which is precisely what §2 means by "running over
Strata runs that are already tries". Building tier R without Elias–Fano and then
adding Generic Join later means the join walks a structure that was not designed for
it, and the headline B3 bet quietly becomes a normal hash join with a fancy name.

---

## Phase 3a — What this pass actually filed

Every finding above is now a bead with self-contained notes, so this document is a
record rather than a dependency. Nobody should have to read it to do the work.

| Bead | P | What it is |
|---|---|---|
| `fgdb-j0vu` | **P1** | **THE SPINE** — a minimal end-to-end slice a human can run, long before W10. |
| `fgdb-ge6a` | **P1** (bug) | Strata's two durable formats (`DeltaBlockVersion`, `PartitionRoot`) are **outside the Appendix A catalog**, and no field anywhere references a partition root — so **no database can currently be reopened**. |
| `fgdb-lc1t` | **P0** | Persistent structures decision, recorded *before* tier R is written. Added as a **blocker on `fgdb-w3-tier-r-0tj`**. |
| `fgdb-p95p` | P1 | Adversarial benchmark harness. Activates the planned `fgdb-bench` crate. |
| `fgdb-z5y0` | P1 | Model-based history generator with shrinking, driving both engines. |
| `fgdb-yago` | P2 | Ski-rental decision-card policy with a declared competitive ratio. Blocked on `fgdb-w3-write-amp-bnn` for its cost constants. |

Two further defects were found *by* the refinement rather than planned into it, both
by re-deriving a claim instead of accepting it:

| Bead | P | What it is |
|---|---|---|
| `fgdb-s50d` | **P1** (bug) | The **oracle admits identity recycling** — create-after-delete of the same `VId`/`EId` is accepted, though plan:221 says spent slots "remain spent forever". Found by testing `fgdb-0trr`'s *fix sketch*, whose premise it falsifies; now a blocker on that bead and on `fgdb-z5y0`. |
| `fgdb-teqw` | **P1** (bug) | `scripts/check.sh` is **red for every pane**: `9e11f4a` removed 5 panic-class findings without updating `UBS_CRITICAL_BASELINE` in the same commit. Filed, not fixed — it is another pane's lane and the pin lives in a file this pass was already editing. |

These form one critical path, and it starts at a bead that is **ready right now**:

```
fgdb-lc1t (P0, ready) ──▶ fgdb-ge6a ──▶ fgdb-j0vu ──┬─▶ fgdb-p95p
  persistence decision    root manifest    the spine └─▶ fgdb-w10-embedded-54r
```

Annotations were also added to `fgdb-w3-tier-r-0tj`, `fgdb-rz12` and
`fgdb-w5-planner-tvi` so the mathematics lands where the work is picked up, not only
here.

## Phase 5 — Refinement: the finding that changes the ordering

The refinement rounds turned up one thing that outranks everything in Phase 2, and it
was invisible until the beads were read as a *schedule* rather than a list.

**The spine exists as exactly one bead — `fgdb-w10-embedded-54r`, the full
`fgdb::Database` surface with the explicit ownership contract — at P2, in workstream
10.** That bead is correctly scoped and correctly late. The problem is that it is the
*only* one, so the vertical slice is scheduled after every horizontal layer.

That is the shape of a project that reaches "46% complete" while remaining 0% usable,
and it defers all integration risk to the most expensive possible moment. Chronicle,
Strata and the oracle are three islands today; the only place they meet is inside test
files in `fgdb-sim`.

`fgdb-j0vu` is the correction: `open → write → neighbours → drop → reopen`, and
nothing else. Thin in **surface**, not in mechanism — it uses the real two-fsync
commit path and real Strata blocks, because doctrine 7 permits a subset of a final
abstraction and prohibits a substitute for it. When `fgdb-w10-embedded-54r` lands, the
slice is absorbed into it rather than left beside it.

### The blocker underneath it: nothing can be reopened

Scoping the spine surfaced a defect rather than a task. **No database can be reopened
today.** Everything that currently looks like a reopen is a test holding a
`PartitionRoot` identity in memory across the close. Filed as `fgdb-ge6a`.

> **Corrected 2026-08-01.** The first version of this section — and of the bead —
> said `RootSlot.root_manifest_oid` "points at an object nobody has defined". **That
> is false**, and it was written from a code grep when the definition lives in the
> registries. `RootManifest` is a landed, active kind (`object_kind = 0x0007`) and
> the chain resolves the whole way: `RootSlot.root_manifest_oid → RootManifest →
> LogicalStateRoot → LogicalStatePayload`. Nothing dangles. Leaving the wrong version
> standing would send whoever picks the bead up at the wrong target entirely.

The real defect is larger. Follow that chain to its end and it terminates in
`LogicalStatePayload`, whose **entire** field set is two scalars —
`applied_logical_command_seq` and `applied_commit_seq`. A recovered database learns
which sequence it had applied and nothing at all about where its graph data lives.
Measured against the registries:

- `target_schema_id = "PartitionRoot"` over `durable_fields.toml`: **zero hits**. No
  field of any schema references a Strata partition root.
- `PartitionRoot` appears in **no registry at all** — not a kind, not a wire type, not
  even a reservation.
- `DeltaBlockVersion` has a **reservation only** (`0x04d4`, `disposition =
  "reserved"`); it is not among the 555 landed `[[kind]]` rows.
- Neither `FGSB` nor `FGSR` — the magic numbers Strata actually writes — appears
  anywhere under `registries/`.

So Strata writes two durable on-disk formats with versioned headers and canonical
encodings, and **neither is in the Appendix A catalog**. The tier sits outside the
normative format contract, which means none of the catalog's cross-cutting machinery
— identity class, construction order, retention and cut rules, golden corpora, GC
reachability under FG-INV-14 — currently applies to the only place graph data lives.

> **Corrected again, 2026-08-01, and this one reverses the conclusion.** I wrote
> that this put catalog work on the critical path — "the one place where *catalog
> blocks engine* is true". **It is the reverse.** I took the catalog token to start
> registering `DeltaBlockVersion` at its reserved `0x04d4`, measured the format
> first, and released the token without editing.
>
> Registering a format freezes it as the normative contract, and what Strata writes
> today is not the format the plan specifies. Its header is `4 + 2 + 4` bytes — magic,
> format, entry count — so six of the nine normative fields (`partition_id`,
> `descriptor_key`, `stripe_range`, `property_patch_refs`, `predecessor`,
> `canonical_logical_digest`) are simply absent. Worse, the plan requires entries
> "encoded under the registered identity-column codec (≤16 B/entry — **raw 128-bit
> identities would cap near 95 entries**)"; today's entry is **72 bytes of raw
> 128-bit identities**, exactly what that sentence exists to forbid. At 72 B/entry a
> 4 KiB block holds 56 entries against a target of 256. Cataloguing that would
> enshrine a >4× density regression as the on-disk contract.
>
> So `fgdb-w3-tier-d-ctj` gating `fgdb-ge6a` is **correct**, and my earlier removal
> of that edge was wrong. Its rollup reads "closed (3 closed)" — that counts *child
> beads*, not scope; skip-list nodes, predecessor chains, EBR, the overflow log and
> hub striping are all still unbuilt. **A closed rollup is not a finished bead.**

What survives is the exposure itself: a durable format is in production use with no
catalog row, so none of the catalog's cross-cutting machinery reaches the only place
graph data lives. The remedy is to finish the format and register it once — not to
register the interim shape and churn it. **There is no catalog work on this critical
path that could start today; the path runs through tier D itself.**

A second refinement caught an edge I had just drawn too coarsely: the spine was
initially blocked on all of `fgdb-w3-tier-d-ctj`, a large bead with several in-flight
children. The spine needs *one object* out of it, not the rest. Splitting `fgdb-ge6a`
out and re-pointing the edge is the same throughput-allocation correction this
document is about — applied to my own dependency graph.

### The steering item that is the owner's call, not mine

**47% of all closed work (129 of 274) is registry, catalog, gate or ceremony.** The
queue is worse than the history. Measured at `9e11f4a`, *before* this pass's
dependencies were wired: of the **23 beads `br ready` surfaced, 16 were catalog, gate,
fixture or owner-ruling work and 7 were engine work** — and three of those seven had
just been filed by this reality-check pass. (The count moves as edges are added; the
ratio is the point, not the integer.)

This is not waste. The catalog is a real gate on shipping a durable format, and the
G0 work is high quality. But `br ready` surfaces those beads preferentially *because
they are unblocked by construction* — catalog work depends on nothing, and engine work
depends on other engine work. So **the work that is easiest to pick up is the work
that does not build the product**, and an agent doing exactly what the queue tells it
is doing the wrong thing through no fault of its own. The swarm is not drifting for
lack of direction; it is drifting because the queue points there.

### The quantified version, measured 2026-08-01 with `bv --robot-triage`

The ratio above understates it. The graph is now **633 beads, 330 closed (52%)**, and
of the 303 not closed:

| | |
|---|---|
| dependency-blocked | **279** |
| actionable | **24 (7.9%)** |
| in progress | 13 |
| cycles | none |

Velocity is not the problem — 330 closed in 30 days, 139 in the last 7, mean 1.35 days
to close. **The problem is that 92% of open work cannot be started.** And `bv`'s own
top picks come back scoring ~0.10 with `unblocks: 0` — it is not recommending
low-value work out of bad judgement; it is reporting that nothing better is reachable.

That is the mechanism behind "ceremony gravity" stated exactly: catalog and procedure
beads are unblocked *because they depend on nothing*, so as the engine graph deepens
they become an ever-larger share of what any agent can legally pick up. The queue is
not mis-sorted. It is nearly empty, and ceremony is what is left in it.

The lever this points at is not re-prioritisation but **depth**: the highest-value
action is finishing whichever bead unblocks the most others. `bv` ranks that as
`fgdb-w1-foundation-types-tjk` (the `fgdb-types`/`bigint`/`delta-types`/`claim`/
`evidence`/`resource` foundations), which another pane is actively landing — so the
swarm is, on this measure, already pointed correctly.

Re-prioritising 67 beads is an ownership decision. The recommendation is Track E:
**stop treating G0 catalog completeness as a prerequisite for engine work.** It is a
gate on shipping, not a gate on building.

---

## Revision history

- **2026-08-09, current pass** — re-read the complete governing corpus and current
  implementation; replaced the obsolete “no library/spine” verdict with an
  evidence-bound assessment of the durable embedded slice; measured the live tracker,
  invariant/checker/formal state, topology, target surface, and runtime probes; added
  the dependency-ordered bridge, three ambition rounds, risk register, and five-pass
  refinement record; created the previously missing release/install/package delivery
  task `fgdb-epic-w10-mhq.1`; and preserved the earlier report as an explicitly
  superseded snapshot.
- **2026-07-31, pass 1** — initial measurement and bridge plan (JadeSnow).
- **2026-08-01, pass 3** — `fgdb-z5y0` landed (`8c53adb`), `fgdb-1xqd` fixed
  (`89c969e`), and `fgdb-ge6a` was re-derived by its own author **twice**, wrong both
  times: the root-manifest chain does not dangle, and the catalog work is not on the
  critical path — the engine format is unfinished and registration correctly waits on
  it. Both corrections are in place above. A reality check whose own findings are
  exempt from re-derivation is just a longer opinion; two reversals in one pass is
  what that principle costs when it is actually applied.
- **2026-07-31, pass 2** — ambition rounds 1–3 revised in place; Phase 3a and Phase 5
  added. Phase 5 found the spine-scheduling gap (`fgdb-j0vu`), the uncatalogued Strata
  formats (`fgdb-ge6a`), the oracle's identity-recycling hole (`fgdb-s50d`) and a
  repo-wide red ratchet (`fgdb-teqw`). Landing this document also required registering
  it in `claims_lint.toml` and `check.sh` — pass 1 had left `registry-check all` red
  repo-wide with `unclaimed_prose`, which is a small lesson in its own right about
  what "the document is written" does and does not mean.
