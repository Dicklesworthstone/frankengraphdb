# Changelog

This is a synthesized, agent-facing changelog for the full history of **frankengraphdb**.

Scope window: project inception on **2026-07-15** through unreleased HEAD **[`076552b2`](https://github.com/Dicklesworthstone/frankengraphdb/commit/076552b2)** on **2026-08-23** (wave 9 below).

**frankengraphdb** is a memory-safe property-graph database in Rust: fountain-coded commit stream, temperature-tiered CSR storage, GQL (ISO/IEC 39075:2024) with an openCypher on-ramp, git-style branches, and a deterministic lab runtime. Workspace version is **`0.0.1`**. There are **no git tags** and **no GitHub Releases** as of this writing (`gh release list -R Dicklesworthstone/frankengraphdb` is empty). Do not invent a `v0.x` release page.

This document was rebuilt from:

- git history on `main` (1,675 commits / 1,674 non-merge; 1,236 in 2026-07, 438 in 2026-08)
- tag and GitHub Release metadata (none)
- Beads tracker in [`.beads/issues.jsonl`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) (~662 issues; 357 closed, 22 epics)
- README / plan / `docs/ARCHITECTURE_DECISION_RECORD.md` / `docs/WORKSPACE_TOPOLOGY.md`

It is organized by landed capabilities, not raw diff order. Representative commits use live GitHub URLs. Beads IDs (`fgdb-…`) are records in `.beads/issues.jsonl`, not GitHub Issues.

---

## Version Timeline

`Kind` distinguishes a published GitHub Release from a plain git tag. This repository has neither.

| Version | Kind | Date | Summary |
|---------|------|------|---------|
| inception [`1cf64cce`](https://github.com/Dicklesworthstone/frankengraphdb/commit/1cf64ccee08260ba49662ad866c4e14f23333a6a) | unreleased HEAD | 2026-07-15 | Master plan, README, AGENTS, license. Two days of adversarial plan review (Sol Ultra, Kimi K3, Fable). |
| G0 constitution [`ee8aa1a5`](https://github.com/Dicklesworthstone/frankengraphdb/commit/ee8aa1a58dd82220704e9b0676c7918b44546d08) | unreleased HEAD | 2026-07-21 | Claim registries, twenty-invariant spine, identity constitution. |
| runnable spine [`42b4b0d3`](https://github.com/Dicklesworthstone/frankengraphdb/commit/42b4b0d34b919de36b8cf6faeda21770543de2e6) | unreleased HEAD | 2026-08-04 | `fgdb::Database` a person can actually run; `main()` opens a database. |
| current HEAD [`076552b2`](https://github.com/Dicklesworthstone/frankengraphdb/commit/076552b2) | unreleased HEAD | 2026-08-23 | Product FCW validator, GQL parse→plan→execute slice with certificates, Vfs-backed Strata durability, §17 adversarial bench, GitHub Actions gate. Workspace still `0.0.1`. |

---

## 1) Master plan and adversarial review (2026-07-15 → 2026-07-16)

The first 24 commits are constitution, not code: the plan absorbs Sol Ultra, Kimi K3, and Fable reviews (valid-time, escrow, FGP protocol, operation-cost registry) before any crate exists.

### Delivered capability

- Master plan with Bets B1–B7 (including Calibrated Adaptivity / Sextant).
- Normative on-disk formats, invariant registry, FGP wire protocol, intent-log net-effect normal form.
- Valid-time (second temporal axis) and escrow/conserved-rights subsystems threaded through §5–§20.
- AGENTS.md / README synced to the revised plan.

### Closed workstreams

- Plan-review artifacts later relocated (see wave 8). The plan itself remains the G0 source of truth.

### Representative commits

- [`1cf64cce`](https://github.com/Dicklesworthstone/frankengraphdb/commit/1cf64ccee08260ba49662ad866c4e14f23333a6a) Initial commit: master plan, README, AGENTS.md, license, gitignore.
- [`86f2ca86`](https://github.com/Dicklesworthstone/frankengraphdb/commit/86f2ca86) Add the Sol Ultra design audit of the master plan.
- [`026f34ec`](https://github.com/Dicklesworthstone/frankengraphdb/commit/026f34ec) Add the Kimi K3 adversarial plan review.
- [`2eed48d4`](https://github.com/Dicklesworthstone/frankengraphdb/commit/2eed48d4) Add Claude Fable 5's meta-analysis adjudicating the Kimi K3 review.
- [`75a07629`](https://github.com/Dicklesworthstone/frankengraphdb/commit/75a07629) Data model: add the valid-time (second temporal axis) and escrow/conserved-rights subsystems.
- [`f534ff21`](https://github.com/Dicklesworthstone/frankengraphdb/commit/f534ff21) Appendix G: the Operation Cost Registry — a constitutional cost instrument.

---

## 2) G0 constitution and W1 bedrock crates (2026-07-21 → 2026-07-26)

Gate G0 materializes as code: claim/invariant/identity registries, Appendix A as a byte-exact catalog, and the closed-universe foundation crates.

### Delivered capability

- Claim registries + CLI/e2e/CI wrapper; twenty-invariant spine FG-INV-01..20; identity constitution.
- Appendix A catalog as the single authoring surface with a byte-exact source verifier; architecture-decision registry.
- First engine crates: `fgdb-bigint`, `fgdb-types`, `fgdb-claim`, `fgdb-delta-types`, `fgdb-evidence`, `fgdb-resource`.
- `fgdb-codec`: LEB128, bitpacking, Elias-Fano, identity-column codecs; bounded neighbor intersection.
- `fgdb-collections`: scalar ART, rank-select, vectorized hash kernels.
- Frozen workspace crate/layer topology (`docs/WORKSPACE_TOPOLOGY.md`); `fgdb-unsafe-simd/arena/vfs` islands with a CI-enforced unsafe ledger.

### Closed workstreams

- [`fgdb-g0-claim-registries-myx`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) claim constitution.
- [`fgdb-g0-invariant-spine-tmm`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) FG-INV-01..20.
- [`fgdb-g0-identity-registries-hrx`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) identity constitution.
- [`fgdb-w1-foundation-types-tjk`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) foundation crates.
- [`fgdb-w1-codecs-3x8`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) codecs.
- [`fgdb-w1-collections-lcg`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) collections.
- [`fgdb-g0-workspace-topology-1q9m`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) crate/layer topology.
- [`fgdb-w1-unsafe-islands-eqrq`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) unsafe islands.

### Representative commits

- [`ee8aa1a5`](https://github.com/Dicklesworthstone/frankengraphdb/commit/ee8aa1a58dd82220704e9b0676c7918b44546d08) Land the G0 constitutional tooling, claim registries, and workspace root.
- [`ae970706`](https://github.com/Dicklesworthstone/frankengraphdb/commit/ae970706) Materialize the twenty-invariant spine.
- [`bfdc44d0`](https://github.com/Dicklesworthstone/frankengraphdb/commit/bfdc44d0a390217e975c9392260dcfcebf8aef2f) First engine crates — `fgdb-bigint` exact integers and `fgdb-types`.
- [`364ac8c0`](https://github.com/Dicklesworthstone/frankengraphdb/commit/364ac8c0) `fgdb-codec` safe scalar codec kernels (LEB128, bitpacking, Elias-Fano).
- [`6d3a9c99`](https://github.com/Dicklesworthstone/frankengraphdb/commit/6d3a9c99) Scalar ART, rank-select, and hash kernels.
- [`a172dd3b`](https://github.com/Dicklesworthstone/frankengraphdb/commit/a172dd3b08c9d1dfb4fc408dc7b6f26087aced81) Freeze and enforce the workspace crate/layer topology.
- [`ef7c058`](https://github.com/Dicklesworthstone/frankengraphdb/commit/ef7c058) Land `fgdb-unsafe-simd`, the first unsafe island.

---

## 3) Appendix A exact catalog (2026-07-22 → 2026-07-27)

A parallel swarm mints the on-disk schema as registry rows rather than prose: reference roots, Raft/manifest, genesis/role-transition, storage identity, delivery markers, branch/merge, restore/replay/security. This is the format freeze G0 promised.

### Delivered capability

- RootSlot/RootBootstrap exact-layout migration; 72 staged ambiguity adjudications.
- Catalog slices A01–A21: storage identity, delivery markers, commands/deltas, branch/merge, checkpoint/resources, restore readiness/prebootstrap, replay/authorization/capability.
- Cross-crate determinism gate, mutation-proven red; one verdict contract for all ten `check.sh` gates.

### Closed workstreams

- [`fgdb-a01-reference-roots-2k0q`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) through [`fgdb-a21-replay-security-ye0o`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) (many A-slices closed; epic [`fgdb-appendix-a-catalog-91ol`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) remains open for residue).
- [`fgdb-udco`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) one verdict contract for all ten gates.

### Representative commits

- [`a3e03386`](https://github.com/Dicklesworthstone/frankengraphdb/commit/a3e03386) RootSlot/RootBootstrap exact-layout migration — checker side.
- [`e33fff57`](https://github.com/Dicklesworthstone/frankengraphdb/commit/e33fff57) Land 72 staged ambiguity adjudications over covered keys.
- [`71fd6035`](https://github.com/Dicklesworthstone/frankengraphdb/commit/71fd6035) Land nineteen unions, fifty-nine arms, thirty-three wire variants (A05).
- [`29e18f92`](https://github.com/Dicklesworthstone/frankengraphdb/commit/29e18f92) Mint eleven reserved branch/merge kinds (A13).
- [`e5ee59fa`](https://github.com/Dicklesworthstone/frankengraphdb/commit/e5ee59fa) One verdict contract for all ten gates, with a guard that fails closed.

---

## 4) Chronicle: fountain-coded commit stream (2026-07-28 → 2026-08-05)

Bet B1 becomes code. Durability is a content-addressed, RaptorQ-erasure-coded commit stream — no double-write journal.

### Delivered capability

- Object-identity pipeline; `SymbolRecord` wire format with total MAC transcript.
- RaptorQ symbolization with erasure recovery and FG-INV-09 identity law; PackedObjectGroup homogeneous-key-domain law; scrub evidence with decode-proof attestation.
- `manifest.root` dual-slot frame and recovery rule.
- Dual-root publication evidence and external-CAS continuity seam.
- Vfs-generic `CommitCoordinator` and async durable Database path.
- CommitValidator seam installed before the first durable byte.

### Closed workstreams

- [`fgdb-w2-object-identity-t0f`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) identity pipeline / RaptorQ.
- [`fgdb-w2-root-bootstrap-hbf`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) dual-slot root.
- [`fgdb-1dgm`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) dual-root certificate / external-CAS.
- Parent epic [`fgdb-epic-w2-6hc`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) Chronicle remains open (engine core through recovery is a milestone, not 1.0).

### Representative commits

- [`f630102`](https://github.com/Dicklesworthstone/frankengraphdb/commit/f630102) Land the §5.1 object-identity pipeline.
- [`4d5eaa19`](https://github.com/Dicklesworthstone/frankengraphdb/commit/4d5eaa194e6df263d561ef936d6d5bdece633fba) RaptorQ symbolization with erasure recovery and the FG-INV-09 identity law.
- [`5f63fee`](https://github.com/Dicklesworthstone/frankengraphdb/commit/5f63fee) `manifest.root` dual-slot frame and the recovery rule.
- [`45ea028`](https://github.com/Dicklesworthstone/frankengraphdb/commit/45ea028) Dual-root publication evidence and the external-CAS continuity seam.
- [`9b80da3`](https://github.com/Dicklesworthstone/frankengraphdb/commit/9b80da3) Vfs-generic `CommitCoordinator` and async durable Database path.
- [`cc6191e`](https://github.com/Dicklesworthstone/frankengraphdb/commit/cc6191e) Install steps 2–3 CommitValidator seam before first durable byte.

---

## 5) Strata tier-D storage and the runnable spine (2026-07-31 → 2026-08-08)

Bet B2: temperature-tiered storage. Combined with `fgdb-0b8r`, the database stops being test-only.

### Delivered capability

- Tier-D durable format: blocks with derived content identity, partition roots, merge-across-blocks vs oracle, writer (delta rows in, sealed blocks out).
- Production `Cx` construction path: `fgdb::Database` openable outside tests; a real `main()` that opens a database.
- Incremental snapshot maintenance — the marginal write no longer pays O(history) (`fgdb-fujt`); receipted incremental publish so the marginal commit stops paying O(blocks) (`fgdb-gieu`).
- FGSV V2 version-chained vertex rows; FGSB V4–V6 (property patches, partition_id, predecessor MVCC chain links); FGSM v1 partition manifest; FGSR V3 root commits to its own logical content.
- DeleteVertex fold into vertex-row retirement; CreateEdge refused to a vertex the fold does not hold.

### Closed workstreams

- [`fgdb-w3-tier-d-ctj`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) tier-D format / writer.
- [`fgdb-j0vu`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) / [`fgdb-0b8r`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) runnable Database.
- [`fgdb-fujt`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) O(history) snapshot write cost.
- [`fgdb-gieu`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) O(blocks) incremental publish.
- Parent epic [`fgdb-epic-w3-umx`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) Strata remains open.

### Representative commits

- [`def35d6f`](https://github.com/Dicklesworthstone/frankengraphdb/commit/def35d6fb06fbd094a52ee7c1cc7b1a420291972) Activate the crate with tier one's durable format.
- [`41eb61c`](https://github.com/Dicklesworthstone/frankengraphdb/commit/41eb61c) The tier-D writer — delta rows in, sealed blocks out.
- [`42b4b0d3`](https://github.com/Dicklesworthstone/frankengraphdb/commit/42b4b0d34b919de36b8cf6faeda21770543de2e6) THE SPINE — a database a person can actually run.
- [`77e512d1`](https://github.com/Dicklesworthstone/frankengraphdb/commit/77e512d1dcc8b0ea2d3ed00edde2316d7f4d769f) A real `main()` that opens a database — the spine's binary witness.
- [`897eb029`](https://github.com/Dicklesworthstone/frankengraphdb/commit/897eb02989a30a5bca6643d0c2aef108c0ece8ec) Incremental snapshot maintenance — the marginal write no longer pays O(history).
- [`e045e12`](https://github.com/Dicklesworthstone/frankengraphdb/commit/e045e12) Receipted incremental publish — the marginal commit stops paying O(blocks) disk work.
- [`6f050f3`](https://github.com/Dicklesworthstone/frankengraphdb/commit/6f050f3) FGSV V2 — version-chained vertex rows with contiguity and birth-immutability laws.
- [`ce62c74`](https://github.com/Dicklesworthstone/frankengraphdb/commit/ce62c74) FGSP edge property patches + FGSB V4 locator hosting.
- [`04fe687`](https://github.com/Dicklesworthstone/frankengraphdb/commit/04fe687) FGSR V3 — the root commits to its own logical content.

---

## 6) Deterministic lab, FaultVfs, LDFI (2026-07-29 → 2026-08-17)

The verification doctrine (Bet B5) ships as a lab you can replay: durability-versus-semantics differential, whole write path, FaultVfs crash matrix, dual-run determinism, shrinking fail artifacts.

### Delivered capability

- Durability-versus-semantics differential, end to end; whole write path end to end; GQL path modes WALK/TRAIL/ACYCLIC/SIMPLE on the reference.
- Lab `FaultVfs` and public `write_with_crash` spine path; crash matrix re-expressed (fsync-lie / interior-tear / ENOSPC); dirent durability so directory-sync faults fire; injectable latency (fifth §15 fault class).
- Lab-vs-live dual-run driver and two-runs-one-seed determinism gate.
- Structured failure artifacts that actually replay; shrinking that does not minimise into a different bug; campaign claim typing so "verified fault-free" is unrepresentable.
- Bounded forced schedule candidates and two-axis fixture schedule/workload artifacts.

### Closed workstreams

- [`fgdb-verif-sim-q97e`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) sim-first differential / write path.
- [`fgdb-1xtp`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) lab VFS (the first fsync already shipped — the ordering violation this bead named).
- [`fgdb-qd2s`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) dual-run driver.
- [`fgdb-w14j`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) crash matrix over FaultVfs.
- [`fgdb-milt`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) injectable latency.
- Parent epic [`fgdb-epic-verif-phi`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) remains open.

### Representative commits

- [`0949f0b`](https://github.com/Dicklesworthstone/frankengraphdb/commit/0949f0b) The durability-versus-semantics differential, end to end.
- [`4065eba`](https://github.com/Dicklesworthstone/frankengraphdb/commit/4065eba) The whole write path, end to end.
- [`89d8f4f`](https://github.com/Dicklesworthstone/frankengraphdb/commit/89d8f4f) GQL path modes — WALK, TRAIL, ACYCLIC, SIMPLE.
- [`bac511b`](https://github.com/Dicklesworthstone/frankengraphdb/commit/bac511b79fced45a38135260e82ae7841a53d443) Land lab FaultVfs and public `write_with_crash` spine path.
- [`9987473`](https://github.com/Dicklesworthstone/frankengraphdb/commit/9987473) Lab-vs-live dual-run driver and the two-runs-one-seed determinism gate.
- [`8876ea4`](https://github.com/Dicklesworthstone/frankengraphdb/commit/8876ea4) Re-express the crash matrix over the FaultVfs and land the fsync-lie/interior-tear/ENOSPC campaigns.
- [`37044da`](https://github.com/Dicklesworthstone/frankengraphdb/commit/37044da) Injectable latency — the fifth §15 fault class, awaited for real.
- [`e929d1d`](https://github.com/Dicklesworthstone/frankengraphdb/commit/e929d1d) Execute bounded forced schedule candidates.

---

## 7) Spine hardening (2026-08-13 → 2026-08-19)

After the spine is runnable, the work is refuse-closed recovery: DeleteVertex cascades must pre-seal, Chronicle verification events are secret-free, cancelled compaction is fenced.

### Delivered capability

- Strata: refuse DeleteVertex unless the cascade is the live incident set; pre-seal before an edge restatement that would trip the ceiling; mixed DeleteVertex cascade pre-seals without same-commit edges.
- Chronicle: verification events on every crypto path; fail closed on unregistered AEAD profiles; bound root-file reads past a stale metadata snapshot; unlock the writer lease on Drop.
- Sim: retain Chronicle verification events on replay; treat unfaulted open/write failure as recovery drift.

### Representative commits

- [`0e8d77a`](https://github.com/Dicklesworthstone/frankengraphdb/commit/0e8d77a) Refuse DeleteVertex unless the cascade is the live incident set.
- [`bb2c0c8`](https://github.com/Dicklesworthstone/frankengraphdb/commit/bb2c0c8) Emit secret-free verification events on every crypto path.
- [`0ef3365`](https://github.com/Dicklesworthstone/frankengraphdb/commit/0ef3365) Fail closed on unregistered AEAD profiles and ciphertext identity.
- [`8145e83`](https://github.com/Dicklesworthstone/frankengraphdb/commit/8145e83) Bound root-file reads past a stale metadata snapshot.
- [`c73af64`](https://github.com/Dicklesworthstone/frankengraphdb/commit/c73af64) Mixed DeleteVertex cascade must pre-seal without same-commit edges.
- [`ee2f8d1`](https://github.com/Dicklesworthstone/frankengraphdb/commit/ee2f8d1) Fence cancelled compaction publication.

---

## 8) Aug 19 2026 repo-janitor docs-reorg

Small hygiene. Historical LLM plan reviews leave repo root; the master plan stays at root (README links remain valid).

### Delivered capability

- `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB__FABLE.md`, `__SOL.md`, `COMPREHENSIVE_PLAN_REVIEW_BY_KIMI_K3.md`, `META_ANALYSIS_OF_KIMI_K3_REVIEW_BY_FABLE.md`, and `PLAN_AUDIT_BY_SOL_ULTRA.md` moved to `docs/planning/`.
- `scripts/check.sh` / `registries/claims_lint.toml` paths updated to match.
- A same-day commit *claims* to move root planning docs; the landed diff is `.gitignore` only. The master plan file is still `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md` at repo root.

### Representative commits

- [`3ee449b`](https://github.com/Dicklesworthstone/frankengraphdb/commit/3ee449b14ddd2a6ada2610f4876d9860dbdb3d2a) Untrack skill-loop scratch; move root planning docs into `docs/planning/` (gitignore only).
- [`f4ad4af`](https://github.com/Dicklesworthstone/frankengraphdb/commit/f4ad4af0b534d3ab4af58f974929bc48f7badca9) Move historical LLM plan reviews into `docs/planning/`.

---

## 9) Product FCW, the GQL execution slice, Vfs-backed Strata, and the §17 bench (2026-08-19 → unreleased HEAD)

The wave that turns spine plumbing into product behavior: transactions get a real validator, GQL gets a parse→plan→execute slice with auditable certificates, Strata durability moves onto the production async Vfs seam, and a §17 adversarial harness starts publishing honest engine limits.

### Delivered capability

- Transactions: every `Database` constructor installs `FirstCommitterWinsValidator` (fgdb-fcw-writebatch-6cxf) — no product handle commits under Chronicle's PassThrough default; basis-pinned `prepare_write`/`commit_prepared` reject stale-basis commits; WriteTxn overlap is FCW, not SnapshotAdvanced (fgdb-w4-g1-txn-core-qpmg.2, fgdb-writetxn-pin-l8wb).
- GQL: the parse→plan→execute slice with `GqlPlanCertificate`; hop-1 AND/`!=`/alias families (`<>`, bang-ne) landed across fgdb-w5-parsers-nje.57–64 with three-way witnesses (fgdb test + sim-vs-reference oracle); destination-label filters fold into certificate digests; ~200 integration test files in `crates/fgdb/tests` and 84 differential-oracle suites in `crates/fgdb-sim/tests` (measured 2026-08-23).
- Strata: BlockStore durability I/O runs through the same async Vfs Chronicle uses (fgdb-tvg8.1) — one-plane rollback under a lying Vfs is witnessed; creation-barrier order logged for lab replay.
- Bench: `fgdb-bench` §17 adversarial harness lands five hostile shapes on the real durable path (fgdb-p95p), honestly publishing machine-local baselines (`empirical_gate_activated=false`) — and immediately finds fgdb-a7sz: sustained ingest fences at the 16 KiB partition-root ceiling (~292 refs × 56 B) and re-fences deterministically on reopen; root-format fix routed to W3+G0.
- Gates: GitHub Actions CI runs `scripts/check.sh` verbatim on push/PR (fgdb-ci-workflow-check-sh-4csa), making "CI-enforced" literal.

### Representative commits

- [`ea40cd90`](https://github.com/Dicklesworthstone/frankengraphdb/commit/ea40cd90) Install FCW validator on product open; prepare/commit_prepared (fgdb-fcw-writebatch-6cxf).
- [`c23faeda`](https://github.com/Dicklesworthstone/frankengraphdb/commit/c23faeda) A stale-snapshot WriteBatch cannot become durable under FCW (fgdb-fcw-writebatch-6cxf).
- [`894d9cba`](https://github.com/Dicklesworthstone/frankengraphdb/commit/894d9cba) Take asupersync as a production Vfs dependency (fgdb-tvg8.1).
- [`c7a4c82d`](https://github.com/Dicklesworthstone/frankengraphdb/commit/c7a4c82d) Witness one-plane Strata rollback under a lying Vfs (fgdb-tvg8.1).
- [`f2fb2a45`](https://github.com/Dicklesworthstone/frankengraphdb/commit/f2fb2a45) Land fgdb-bench §17 adversarial harness — five hostile shapes on the real durable path (fgdb-p95p).
- [`8dceb212`](https://github.com/Dicklesworthstone/frankengraphdb/commit/8dceb212) Fence telemetry + corrected a7sz mechanism note (partition-root ceiling arithmetic).
- [`c3fd2762`](https://github.com/Dicklesworthstone/frankengraphdb/commit/c3fd2762) Fold destination-label filter into certificate digest; bind far-end != in two-hop WHERE.
- [`37de0fdd`](https://github.com/Dicklesworthstone/frankengraphdb/commit/37de0fdd) Activate fgdb-gql; expand durable state-slot and transition-writer registries.
- [`076552b2`](https://github.com/Dicklesworthstone/frankengraphdb/commit/076552b2) CI: wire scripts/check.sh as the GitHub Actions gate (fgdb-ci-workflow-check-sh-4csa).

---

## 10) Sustained-ingest ceiling fix, honest §17 baselines, and CI red-in-practice (2026-08-23 → 2026-08-29)

The week after the product wave: the ingest ceiling the bench found gets fixed, the bench publishes its first honest numbers, CI's verdict contract meets standard-runner reality, and the Appendix-A catalog keeps minting under a pinned-floor ruling.

### Delivered capability

- Ingest: per-family object admission on the write path (fgdb-a7sz, 5f8b9180) — roots admit against `MAX_ENCODED_ROOT_BYTES` (the layout law `get_root` always applied; the frozen root format already declared the larger ceiling, so no format change), while blocks/patches/manifests keep the block-derived bound. Mutation-proven witness `a_root_lawful_under_its_own_format_ceiling_is_admitted` (19,294 B root; the shared-bound mutation fails with `ObjectTooLarge {limit: 16384}`). End to end: `fgdb-bench` full fixture — 5,994 edges / 94 commits / zero fences / 2,349 stored blocks (8× past the old brick point), cold reopen identical, fgdb suite 232/232; previously-bricked directories recover automatically.
- Bench: first honest §17 numbers published (fgdb-p95p) — point reads p50=122 µs / p99=152 µs under power-law skew; cold partition reopen p50=40.5 ms → 219 ms at full scale; compaction-under-load 104 ms publish under 77–130 verified concurrent pinned traversals; deep branch chains unreachable (no branch API on the spine — documented, not hidden). Machine-local, unpinned, `empirical_gate_activated=false` throughout, correctness asserted inside every measured region.
- CI, honestly: the gate's verdict is exactly `scripts/check.sh`'s exit code (076552b2), and the chain has never completed on a standard runner — 57 runs / 18 failures / 37 cancelled / 1 success, and that success is the red-proof probe (run 32622995463); every recent failure dies ~29–30 minutes in with runner-host ENOSPC while building the workspace. Making the chain reach a verdict and proving one full green run is owned by fgdb-ci-workflow-check-sh-4csa.1.
- Appendix A / G0: W12 Meta and Shard semantic-core format rows mint under fgdb-a06-w12-core-zdzx (still open — completion-spec hosts, typed receipt refs); command contracts reach epoch 92 (182 reserved / 1 live, from 175/1 at epoch 40). The remote-retention family re-dates above the global construction-order ceiling (order 80 → one shared order 114: 16 family kinds, 17 transitive referrers, and the co-phased global-delta generation) per the P1 pinned-floor ruling — fgdb-bbqq Decision 2, executed by NobleThrush under recorded operator direction (2026-08-24).
- Tracker (2026-08-29 refresh measurement): 892 records / 607 closed / 273 open; 37 closures 08-22 → 08-29 (12, 7, 6, 3, 4, 1, 3, 1 per day), net open 277 → 273. That same refresh filed this changelog's staleness (then ending at 076552b2) under fgdb-g0-doc-sync-usq; this wave is that record.

### Closed workstreams

- [`fgdb-a7sz`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) sustained-ingest ceiling — fixed at 5f8b9180 without a format change.
- [`fgdb-p95p`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl) adversarial §17 harness — honest numbers published, the filed adversarial shape list dispositioned.

### Representative commits

- [`5f8b9180`](https://github.com/Dicklesworthstone/frankengraphdb/commit/5f8b9180) Admit roots against their own format ceiling — sustained ingest unbricked (fgdb-a7sz).
- [`e668d169`](https://github.com/Dicklesworthstone/frankengraphdb/commit/e668d169) Directed-faces harness fix and pre-a7sz fixture scale — point-reads/cold-reopen publish instead of fencing (fgdb-p95p).
- [`a4897f6a`](https://github.com/Dicklesworthstone/frankengraphdb/commit/a4897f6a) Re-date the remote-retention family above the catalog ceiling under the P1 pinned-floor convention (fgdb-bbqq Decision 2).
- [`c2cfffc0`](https://github.com/Dicklesworthstone/frankengraphdb/commit/c2cfffc0) Mint KeyErasureVerification and StorageMemberPhysicalCompletionQuorum shells (fgdb-a06-w12-core-zdzx).
- [`c0f0b830`](https://github.com/Dicklesworthstone/frankengraphdb/commit/c0f0b830) Mint KeyDestructionTerminalReceiptRef and land typed receipt refs (fgdb-a06-w12-core-zdzx).
- [`7a398a27`](https://github.com/Dicklesworthstone/frankengraphdb/commit/7a398a27) Spend vertex identities only after a durable commit — a publish-fence retry can no longer emit edges against vertices that never landed.

---

## Notes for Agents

- Start with the version timeline if you need chronology. There is no `v0.x` tag and no GitHub Release; HEAD is the only published artifact. Workspace version is `0.0.1`.
- The README is present-tense 1.0 target state (G1→G4). This changelog is what has actually landed. Loom (query algebra), Ripple (incremental views), Beacon, Prism, Warden, Fabric, and Aegis are still open epics. GQL path *modes* exist on the reference; a full GQL planner/executor has not landed as a named crate wave.
- Tracker of record is [`.beads/issues.jsonl`](https://github.com/Dicklesworthstone/frankengraphdb/blob/main/.beads/issues.jsonl). Use `br show <id>`.
- Closed-universe law: `std` + pinned nightly + owned foundations. No serde, tokio, rocksdb, arrow, tantivy, hnswlib.
- Master plan remains at repo root. Historical review files live under [`docs/planning/`](https://github.com/Dicklesworthstone/frankengraphdb/tree/main/docs/planning) after the 2026-08-19 janitor.
- `origin/master` exists only as a legacy-URL mirror of `main`.
