# Reality Check and Bridge Plan

**Current measurement: 2026-09-02** (evidence window 2026-09-02T21:00–21:30Z,
HEAD `fdd53388`). Previous: 2026-08-31 (pinned `a4bb93c4`).
This document is revised in place. Older commit-bound assessments are retained
below as superseded historical snapshots because they explain several decisions;
their counts and statements about missing seams are not current unless the
2026-09-02 delta repeats them.

---
## Current delta — 2026-09-02

### Product verdict

**The tree does not compile, has not had a green verdict for 141 commits,
and nothing in the README's "what runs today" section runs at HEAD.** The
08-31 delta closed "Gap 0′ — CI must complete" on the strength of one green
run (`f8bf9b40`, 2026-08-31T18:03Z). That run is still the only green run
in the last 200. Everything after it was landed by a session that, by its
own CHANGELOG entries, had no Rust toolchain, no shellcheck, and no UBS, and
whose commits say so ("this changelog does not claim a fresh cargo fmt,
compile, Clippy, Rust-test, shellcheck, UBS, or complete check.sh verdict").
Measured today on a `git archive HEAD` copy under the pinned nightly
(`rustc 1.100.0-nightly e7769602a`, private target dir, rch shim bypassed):

| Verdict | Result at `fdd53388` | Cause |
|---|---|---|
| `cargo fetch --locked` | **refused** | `crates/fgdb-gql/Cargo.toml` gained `fgdb-crypto` and `fgdb-types` path deps (`97d09787`); `Cargo.lock` was never regenerated |
| `cargo check --workspace --all-targets` | **rc=101** | `crates/fgdb-gql/src/evidence_artifact.rs:564` decodes a `VId` as `u64`; `VId` is `u128`, and the encoder at :241/:409 writes 16 bytes. The envelope has never round-tripped; its own unit tests never compiled |
| `cargo run --example open_a_database` (the README's runnable witness) | **rc=101** | cascade from the above |
| `cargo run --example gql_time_travel`, `gql_evidence_cursor` | **rc=101** | cascade |
| `cargo test -p fgdb-gql`, `cargo test -p fgdb` (5 suites) | **rc=101** | cascade |
| `cargo clippy -p fgdb -p fgdb-gql --all-targets -- -D warnings` | **rc=101** | cascade |
| `cargo fmt --all --check` | **rc=1** | 21 files unformatted (every `gql_evidence_*` example, `evidence_artifact.rs`) |
| `bash scripts/g0_claims_e2e.sh` (main tree, read-only) | **rc=1**, 16 pass / 1 fail | `checker_index` reports 7 × `script_undeclared`: the four `scripts/agent_context*.sh` and three `scripts/local_proof*.sh` files landed 09-01 (`0ce6f9c2`, `f4759d95`) with no checker or `[[script_disposition]]` row — NE-0013's class, and the ironic one: the local-proof tooling that would have caught all of the above is itself unregistered |

`fgdb-gql` has been non-compiling since `97d09787` (2026-09-02T01:20 local);
**33 further commits** — page tokens, evidence cursors, resource limits, six
examples, the CHANGELOG waves, and the new `IMPLEMENTATION_STATUS.md` — were
authored on top of a crate that did not build. The day before, `fb47b512`
(09-01) failed to compile `fgdb` itself (six `E0308` in `gql_cert.rs`, fixed
the same day by `1175b620`/`2619a17f`). This is a practice, not an accident.

**Hosted CI could not say any of this.** Of the last 200 `check.yml` runs:
1 success, 32 failures, **166 cancelled** — `concurrency.cancel-in-progress`
plus a 30–60 push/day cadence means a run is cancelled by the next push before
it reaches a verdict. The last two *completed* runs were red
(`33435256302` on `51b9b66c`: fmt/clippy/test/UBS red, 33/33 registered
gates green; `33469052094` on `fb47b512`: 6 of 9 core gates red, 13
registered gates UNRUN). HEAD is `queued`. The 08-31 sentence "one full green
run is a single push away" was structurally false under the push cadence.

**Vision delivery is unchanged at 2 of 20 rows fully working** (closed
universe; unsafe ledger). The enforcement row that moved *up* on 08-31 moves
*down* today. Everything a README reader would type remains NOT_STARTED, and
the one thing a README reader is told *does* run does not compile.

### The five questions this skill asks

1. **What is working right now** (at the last green `f8bf9b40`, and at HEAD
   once the one-line width fix lands — nothing else in the engine regressed):
   the two-fsync Chronicle commit path, first-committer-wins validation,
   Tier-D Strata with VFS injection and durable compaction, checkpoint-selected
   fast open, `MemVfs` `open_memory`, frontier and `_at` reads for vertices/
   edges/neighbours/scans, the bounded GQL read slice (one- and two-hop `MATCH`,
   labels, integer property comparators, variable `=`/`<>`, `SKIP`/`LIMIT`,
   `RETURN` of one variable's ids), `WriteTxn` with staged overlay and a
   read-set/expansion first-committer check at commit, pinned `EmbeddedReadView`,
   owned `PreparedGqlQuery` without parameters, the certificate/digest/artifact
   evidence stack over that slice, the ~90 oracle-differential suites, the
   FaultVfs/LDFI/crashpack lab, the registry/gate machinery, and the CI harness
   *when a run is allowed to finish*.
2. **What is not working or not yet implemented.** HEAD compile, `--locked`
   builds, every documented runnable witness. Unchanged from 08-31 and still
   absent: sessions, typed parameters, typed rows/columns, streaming results,
   property projection, aggregation, `ORDER BY`, writes through GQL, openCypher,
   GLA/Loom/optimizer/spill, Tier I/R/A (Tier R "sealed CSR" exists only in a
   crate-root comment and in an `open_a_database` comment that overclaims what
   `compact` does), SSI proper and the merge ladder, branches/retention/
   replication, Ripple/Beacon/Prism/Warden/Fabric/Aegis, CLI/server/Python/
   installer/releases, 0/20 invariants enforced, 1/183 command contracts live,
   §17 gates unactivated, one live Lean lane of ten declared.
3. **What is blocking us.** Three things, in this order. (a) *Verdict-less
   landing*: sessions without a toolchain landed 141 commits; CI cancels itself
   before reporting; nothing between the author and `main` ever executed the
   code. (b) *The G0 wall*: 277 of 286 open beads are blocked, `br ready` is
   empty, and the blockers are the identity/command-contract registries. (c)
   *The GQL representation*: `BoundPlan` is a fixed-shape struct with 27
   optional fields, 18 of them one-per-(position × comparator) integer
   predicate slots, executed by a hand-written kernel and witnessed by 82
   per-shape oracle files in `fgdb-sim` and 208 `gql_*` test files in `fgdb`.
   Every new predicate is a new field, a new kernel arm, and a new oracle file.
   That representation cannot grow into ISO GQL; it will be replaced by the GLA
   subset (`fgdb-5vp9`), and the certificate transcript v2 that "binds every
   `BoundPlan` field" is bound to a throwaway.
4. **Would implementing all open beads close the gap?** For tracking, yes:
   every one of the 20 vision rows still has at least one open bead (keyword
   census over the 274 open titles: CLI 2, server 6, Python 2, releases 9,
   Ripple 7, Beacon 17, Prism 6, Warden 9, Aegis 6, Loom 10, Cypher/TCK 3, SSI
   18, branches 11, temporal 6, Tier R 3, spill 1, bench 7, Lean/TLA 3,
   sessions 6, certificates 13). For delivery, no, for three reasons the beads
   do not address: the landing practice that produced today's red is not a
   bead's subject; the CI shape cannot report under the swarm's cadence; and
   G1 is unreachable through any completion order that does not route through
   invariant promotion (0/20).
5. **Vision goals with no bead.** None of the 20. Three *process* seams had
   no owner and are filed today (below): the stale-lockfile/toolchain-less
   landing class, the self-cancelling CI, and commit-message provenance
   (`fgdb-w10-embedded-54r.1` ×39, `fgdb-gate-genesis-lce.2` ×21, `fgdb-3w75`
   ×9, `fgdb-w4-g1-txn-core-qpmg.24` ×2 — **71 of the last ~130 commits cite a
   bead ID that does not exist** in `.beads/`; no gate checks that a cited ID
   resolves).

### Vision checklist — 2026-09-02 refresh (rows that changed since 08-31)

| # | Goal | 08-31 | 09-02 |
|---|------|-------|-------|
| 1 | Embedded `Database::open(path\|:memory:)` + sessions + prepared statements | PARTIAL | PARTIAL, plus `EmbeddedReadView`, owned `PreparedGqlQuery`, `WriteTxn` overlay; still async-only, no sessions, no parameters, no typed rows. **The documented example does not compile at HEAD** |
| 4 | GQL + openCypher + FQL | PARTIAL (bounded MATCH slice) | PARTIAL, same slice; `RETURN` yields one variable's ids only (`Source`/`Destination`/`Hop2Destination`), so the README's own quick example (`RETURN p.name, count(f)`) is out of grammar |
| 7 | Deterministic STRICT results + plan certificates | PARTIAL | PARTIAL; digests over statement/bind/plan/rows/snapshot now have portable envelopes, page tokens, and cursors — over a materialized result, re-audited per page. Not §8 plan certificates (no operator witnesses, no decision cards), and the envelope format has never round-tripped (width defect) |
| 13 | SSI / Graph-SSI | STUB in production | PARTIAL-lite: `WriteTxn` refuses on read-set overlap and on a `CreateEdge` into an expanded `(src, relation)` pair since basis. No rw-antidependency graph, no predicate ranges, no merge ladder |
| 15 | §17 empirical gates | HARNESS LIVE | **REGRESSED**: no completed run since 08-31; harness cannot report under the push cadence |
| 21 (new) | "CI-enforced" gate on `main` | (implicit in 15) | **REGRESSED**: last green `f8bf9b40`; 141 commits without a verdict; HEAD red on fmt/check/clippy/test and `--locked` |
| 22 (new) | README "what runs today" | WORKING | **REGRESSED**: `cargo run -p fgdb --example open_a_database` rc=101 at HEAD |

Vision delivery: **2 of 20 fully working** (unchanged). Two enforcement rows
regressed.

### Inventory

| Measure | 08-29 | 08-31 | 09-02 |
|---|---|---|---|
| HEAD | `7a398a27` | `a4bb93c4` | `fdd53388` (141 commits after the last green) |
| CI (last 200 runs) | 57 runs / 18 fail / 37 cancel / 1 success | "verdicts on every push" | **1 success / 32 fail / 166 cancelled**; last completed run red; HEAD queued |
| Local exact-tree verdict | — | focused suites green | **check/clippy/test rc=101, fmt rc=1, `--locked` refused** |
| Tracker | 892 / 607 closed | 892+ / 607+ | 897 / 611 closed / 274 open / 5 in_progress / `br ready` 0 / bv actionable 9 of 286 |
| Closures per day (JSONL) | ~3 | 1–3 | **0** since 08-31 (JSONL last exported `476bec25`); 4 of 5 in_progress beads untouched 1–6 weeks |
| Commit provenance | clean | clean | **71 of ~130 commits cite a nonexistent bead ID** |
| Crates | 22 active / 49 planned | same | same |
| Command contracts | 1 live / 182 reserved | same | same |
| FG-INV enforced | 0 / 20 | 0 / 20 | 0 / 20 (42 stub checkers in `checker_index.toml`) |
| Proof lanes | 1 live | 1 live | 1 live (`lean-version-chain`, 109 lines), 2 checked, 9 declared |
| GQL grammar | MATCH/WHERE/RETURN/SKIP/LIMIT | same | same; `BoundPlan` 27 optional fields, 18 predicate slots; 82 + 208 per-shape test files |
| Workspace | ~270k LOC | ~270k | ~270k + 24k inserted / 13k deleted since 08-31 (33 feat, 29 docs, 23 test, 19 fix, 11 example, 11 ci) |
| Engine `todo!()` | 0 | 0 | 0 |

### Bridge plan updates (order = vision impact)

**Gap 0 — REOPENED, P0: restore a verdict on `main`.** One landing, one
proof bundle: (1) fix the `VId` width in `evidence_artifact.rs` by reading
16 bytes to match what the encoder writes — this is a format decision, not a
`.into()`; the row stride, the `total encoded bytes` limit arithmetic in
`evidence_limits.rs`, and the page-token offset accounting all assumed 8 —
re-derive them and re-pin the golden vectors; (2) regenerate `Cargo.lock`;
(3) `cargo fmt`; (3′) register the seven 09-01 shell scripts in
`registries/checker_index.toml` (as checkers or `[[script_disposition]]` rows
with a stated reason) so the claims gate and the file-coverage closure are
green again; (4) `bash scripts/local_proof.sh` on the exact tree and land
with the bundle's verdict quoted. Then two structural changes, each its
own bead below: (5) **no landing from an environment that cannot execute the
gate** — a CHANGELOG "validation boundary" paragraph is a disclaimer, not a
verdict, and doctrine 8 does not accept disclaimers; (6) **make CI able to
report under the swarm's cadence**: keep `cancel-in-progress` for PRs but add
a non-cancelling `main`-HEAD run (scheduled, or a merge queue), because a gate
that is always cancelled enforces nothing (NE-0012's family).

**Gap 1 — unchanged:** G0 command universe, 1 live / 182 reserved
(`fgdb-5uw2`). Still the wall behind 277 blocked beads.

**Gap 2 — moved:** `WriteTxn` is the first production conflict check on the
validator seam. SSI proper (rw-antidependency structure, predicate/range
witnesses, `TxnCx` narrowing) remains `fgdb-w4-g1-txn-core-qpmg`.

**Gap 3 — sharpened:** *freeze the evidence tower*. Do not add another
`*_prop_*` slot, kernel arm, or per-shape oracle file. Land the registered
GLA subset (`fgdb-5vp9`) and re-bind certificates to a GLA plan transcript
(v3); the v2 binding to `BoundPlan` fields is throwaway. The evidence machinery
is worth keeping only if its first *real* consumer is the algebra, not the
prototype plan struct.

**Gap 4 — unchanged:** Tier R seal, bounded open, first spill-backed operator.
The `open_a_database` comment claiming `compact` produces "a sealed CSR run"
should be corrected: compaction consolidates Tier-D blocks.

**Gap 5 — unchanged:** 0 / 20 invariants. Promote FG-INV-08/09/10/18 with a
live checker plus a distinct negative test in one change.

**Gap 6 — unchanged:** CLI robot mode and the sync `Database` API after Gaps
2–3.

**Gap 7 — unchanged warning:** later layers stay parked.

**Tracker hygiene (not gaps, but blocking orchestration):** re-attach the 71
orphan-cited commits to `fgdb-w10-embedded-54r` and `fgdb-gate-genesis-lce`
(or create the child beads those commits assumed existed, with the commit
list as provenance); close or release the four stale `in_progress` beads
(`fgdb-bbqq`, `fgdb-a06-w12-core-zdzx`, `fgdb-w30l`, `fgdb-w1-crypto-y5o`);
export the deferred records through `scripts/br_sync.sh` once the compile fix
lands so the JSONL stops lagging the database by three days.

### Ambition rounds applied to this revision

- **Round 1** refused the "one-line fix" framing. The defect is not a `u64`;
  it is that 141 commits reached `main` without anything executing them, and
  that the repository's own honesty machinery (CHANGELOG boundary paragraphs,
  `IMPLEMENTATION_STATUS.md`) documented the absence of a verdict instead of
  refusing the landing. The bridge therefore lands a *refusal*, not a patch.
- **Round 2** asked what the evidence tower is for. Certificates, artifacts,
  limits, tokens, and cursors are the right *shape* for B5 — but bound to a
  fixed-shape plan struct they certify a prototype. The ambitious move is to
  make the registered GLA subset the first consumer of that machinery, so the
  certificate outlives the plan representation it was born on.
- **Round 3** asked what CI shape survives a 60-push day. Per-commit
  cancel-in-progress is correct for PRs and wrong for `main`; the deterministic
  fallback is one non-cancelling verdict per `main` head plus a
  `local_proof.sh` bundle per landing, which is exactly the two-sided proof
  discipline the plan already prescribes for everything else.

### Refinement passes on the new state

- **Pass 1** checked each new bead for a success criterion that can fail: the
  lockfile bead reds on `cargo metadata --locked`; the CI bead reds on "no
  completed run for HEAD within N hours"; the provenance bead reds on a commit
  citing an unresolvable ID.
- **Pass 2** checked ordering: the width fix and the lockfile regeneration are
  one commit (either alone leaves the tree red); the CI and provenance beads
  are independent of both and of each other.
- **Pass 3** checked that no new bead duplicates an owner: `fgdb-5vp9` keeps
  the GLA move (measurement comment added, no fork); `fgdb-w10-embedded-54r`
  keeps sessions/cursors (comment added); `fgdb-ci-workflow-check-sh-4csa`
  keeps CI (comment added; the cancellation seam is filed as its child).
- **Pass 4** found nothing further.

### Beads filed this measurement

| ID | Seam |
|---|---|
| `fgdb-l9r3` (P0 bug) | stale `Cargo.lock` + non-compiling `fgdb-gql` at HEAD (the `VId` width is a format decision, not a cast), and the landing-practice refusal that prevents recurrence |
| `fgdb-ci-workflow-check-sh-4csa.2` (P1) | `main`-HEAD verdict that cannot be cancelled by the next push |
| `fgdb-baru` (P1) | commit-message bead-ID resolution gate; 71 orphan-cited commits to re-attach; four stale `in_progress` beads to release |

Measurement comments (no forks) were added to `fgdb-5vp9` (GLA subset owns the
`BoundPlan` retirement), `fgdb-w10-embedded-54r` (the orphan `.1` commits and
what they did and did not deliver), `fgdb-ci-workflow-check-sh-4csa`, and
`fgdb-gate-genesis-lce`. The JSONL was not exported by this measurement; it
lands with the compile fix through `scripts/br_sync.sh`.

### Same-night pickup — 2026-09-03T02:00Z

**Gap 0 is closed.** Three landings restored a verdict, and the third
exact-tree proof is green:

- `b51e3232` — 16-byte `VId` rows as a v1 format decision with two pinning
  laws; `Cargo.lock` regenerated; seven hasher arms, one boxed variant, one
  path-included test, 32 unformatted files; seven script dispositions (both
  self-tests measured green); `--locked` on every cargo gate (negative
  control: a stale lock reds the gate with cargo's own message).
- `b744cf82` (operator-committed from the audit session's worktree) — the
  four pre-existing reds the first exact-tree proof exposed (the adjacency
  test lost its subject in the write_txn split; five unclaimed docs; SC1083;
  the UBS ratchet's dev-box/runner mode asymmetry, now one exact table per
  mode selected from ubs's own transcript), `NE-0045`, the CHANGELOG entry,
  the status paragraph, `scripts/g0_commit_provenance_e2e.sh` (live), the
  two-hourly non-cancelling CI run, and the 13-record tracker export
  including the three retroactive child beads.
- `4c63a504` + `8af5af1b` — the regex-mode ratchet re-pin, tool-controlled at
  `9ec76706` (today's tool reproduces the old table there exactly) and
  attributed by a changed-files scan (0 → 6 criticals: five test-harness
  `panic!`s in the live-payload checker, one JWT-heuristic hit on a test
  name); and the bet labels the two new gate beads needed for provenance to
  be total (proof #2's single remaining cause).

Exact-tree proof on `8af5af1b` (`scripts/local_proof.sh`, started
2026-09-03T01:03:22Z, finished 01:49:41Z, tree stable): **ALL GATES GREEN —
core 9/9, registered live 34/34**, including the new provenance gate
(110/110 in-chain), the architecture suite (42/42), and the negative-evidence
ledger with NE-0045. First green exact-tree verdict since `f8bf9b40`
(2026-08-31). Proof #1 on `b51e3232` was red on four pre-existing causes;
proof #2 on `4c63a504` was red on one (the missing bet labels).

**The ready queue is empty for a structural reason, now quantified.** Of 292
open records, 279 hang transitively on four roots: `fgdb-bbqq` (an owner
sitting on union-edge semantics; 278 dependents), `fgdb-a18-restore-union-
source-gates-a4fq` (a logical-versus-wire class ruling for seven unions;
276), `fgdb-a06-w12-core-zdzx` (the in-progress W12 catalog core; 276), and
`fgdb-asupersync-signing-provider-dj4j` (an upstream asupersync capability;
252). Two of the four are rulings only the owner can issue. The owner ruled
`fgdb-bbqq` as (a1) on 2026-09-03 (recorded on the bead), and the ruling was
implemented and landed the same night as `c91bf04b` (exact-tree proof ALL
GATES GREEN, core 9/9, registered 34/34): reference-union arms are type
alternatives exempt from the construction-order and cycle laws, the arm
predicate spelling is carried through the checker and the projection writer,
and `RemoteGrantTargetRef` exists as the generated closed union over the 74
exportable target kinds (one header, the anchor field, 74 matrix arms, 76
targets). `fgdb-atke` is therefore delivered pending independent
verification; `fgdb-bbqq`'s deletion condition is met by the catalog-row
recording (the ADR registry has no category for owner rulings). The a18 class
ruling and the ordinary-union tag-width ratification are still open.

**What did not move:** vision delivery (2 of 20), the GQL representation,
SSI, Tier R, invariants (0 of 20), command contracts (1 of 183). Gap 0 was
the precondition for knowing any of that; it is not progress on it.

**Bead state:** `fgdb-l9r3`, `fgdb-baru`, `fgdb-ci-workflow-check-sh-4csa.2`
stay open for independent verification against their acceptance criteria;
the first scheduled hosted run (cron `17 */2 * * *` UTC) is 4csa.2's
evidence. `fgdb-ci-workflow-check-sh-4csa.1` (runner disk) was closed on the
strength of the 08-31 green run.

### Evidence boundary

Pinned to `fdd53388` (2026-09-02 13:48 -0400). Method: the same read-only
audits as prior deltas plus (a) `gh run list` over the last 200 `check.yml`
runs and the full logs of the two most recent completed runs; (b) a quiet-root
build of `git archive HEAD` under the pinned toolchain with a private
`CARGO_TARGET_DIR` and `RCH_CARGO_WRAPPER_BYPASS=1`, so no tracked file in the
main tree moved (`git status` clean throughout); (c) `cargo update --dry-run
--offline` and a `Cargo.lock` diff against the copy's unlocked resolution;
(d) `br show` on every bead ID cited by commits since 08-28. No `scripts/
check.sh` run was attempted: the tree cannot pass its fourth core gate, and a
35-minute chain adds nothing to a verdict already decided at `cargo check`.

---
## Current delta — 2026-08-31

### Product verdict

**The CI enforcement surface is no longer aspirational. The chain reaches a
verdict on every run now, and 32 of 33 registered live gates pass on a
standard runner.** The 08-29 audit's "Gap 0′ — CI must complete" was the
binding constraint behind "CI-enforced" being an aspirational sentence in
AGENTS.md and the README. The two days since `7a398a27` closed that
constraint end-to-end:

1. **The disk ceiling is gone.** Standard-runner disk is reclaimed (android,
   dotnet, swift, ghc, hostedtoolcache, etc.) before the build graph
   materialises; debuginfo and incremental artifacts are stripped via
   `CARGO_PROFILE_*_DEBUG=0` and `CARGO_INCREMENTAL=0`. The chain now survives
   the workspace compile that ENOSPC'd every previous run at ~29 minutes.
2. **The toolchain assumptions are real.** Every gate-chain tool the
   runner image does not ship is now provisioned: elan + Lean toolchain
   prewarm, cargo-deny 0.19.0, beads_rust 0.5.7 (the `br` CLI, pinned by
   git tag), cargo-audit 0.22.0, ripgrep, and the ubs meta-runner at v5.3.13.
   A `GITHUB_PATH` vs in-step-PATH bug surfaced and was fixed (export
   `PATH` inline in the elan step, since `GITHUB_PATH` only affects later
   steps); `cargo install --git beads_rust` was hitting the repository's
   fuzz member and required package disambiguation to install cleanly.
3. **The gates themselves are portable.** Several registered gates hardcoded
   `/data/tmp` scratch-root defaults (absent on the runner) — the
   `tree_stability`, `dependency_policy`, `token`, `landing_lease`, and
   `disk_hygiene` inventory paths now honour a portable
   `FGDB_GATE_TMP → TMPDIR → /tmp` chain; tree_stability and dependency_policy
   verified in both default and pristine-simulated conditions; bash -n and
   shellcheck clean throughout.
4. **The UBS ratchet is honest.** The first run that reached UBS went RED
   because every prior baseline pinning was measured with ast-grep present
   (dev box), while the runner runs the rust module's regex fallback. A
   fresh detached-tree scan under both modes proved the module had not
   changed; the runner's mode partition (134/1/18/184/122) is now pinned
   in `UBS_CRITICAL_BASELINE` with per-delta attribution comments in the
   established format — the ratchet's "named, never absorbed" discipline
   is preserved.
5. **The product surface moved on three small slices.** The README's
   `:memory:` promise is now real (MemVfs + `Database::<MemVfs>::open_memory`
   over a content-free sparse shadow under a private temp root — Chronicle
   locks and Strata permits stay on `std::fs`, so the foundation is
   untouched); the week-stale GQL binder twins `fgdb-wur5` and
   `fgdb-ysm0` are completed (the parser and product panes were already in
   tree; the actual residue was one missing non-overlay engine witness
   and a unit-coverage asymmetry, now landed and differentially verified);
   CHANGELOG wave 10 records the 08-23 → 08-29 week honestly, including
   the CI red-in-practice status it described.
6. **A load-sensitive timing probe hardened.** The crypto constant-time
   lane's single-shot Welch-t probe tripped on the shared runner (the
   within-screen load trim cannot remove a load episode that spans a
   whole screen). The probe now requires a 2-of-3 quorum over independent
   screens of the identical inputs — a real kernel-level separation
   reproduces across screens, a load episode does not. Bounds untouched.

**The residue that remains is not "CI broken" — it is one gate
(`w1_cross_crate_determinism_e2e`) under shared-runner load, plus one
test (`write_cost_attribution`) whose O(history) assertion is the same
family. Both have been filed as honest-residue beads and the artifact
infrastructure (transcripts staged to a fixed path then archived, landed
by the operator as `db2f8646`) is now in place to diagnose the next
occurrence in one cycle. The orchestrator's `ca3317cb` closed both
residue beads on the strength of the artifact fix; the same artifact will
reopen them with evidence if a future run confirms a real regression.**

### The five questions this skill asks

1. **What is working right now.** Everything the 08-22 and 08-29 deltas
   listed as real is still real — the two-fsync commit path, the FCW
   validator, Tier-D Strata with VFS injection, the bounded GQL MATCH
   slice, the ~90 differential oracle suites, the FaultVfs/LDFI/crashpack
   lab, the UBS ratchet (now mode-honest), the crypto constant-time
   lane (now quorum-robust), and the MemVfs open-memory posture. And
   **the CI chain reaches a real verdict on every run** — that is the new
   this delta. The 08-29 "57 runs / 18 failures / 37 cancelled / 1
   success" line is now a historical artifact; recent runs (33346337322,
   33354927779, 33358640909) all completed the full chain and reported
   per-gate verdicts.
2. **What is not working or not yet implemented.** The Gap-0′ residue
   (one runner-load-sensitive registered gate, one runner-load-sensitive
   core test) is named, owned, and instrumented. Unchanged from 08-29:
   sessions / prepared statements / `:memory:` (the embed story is now
   `open_memory`; the rest is W10's surface), multi-graph/branch/
   partition coordinates, GLA / Loom execution, Tier I/R/A, retention
   cooling, Ripple / Beacon / Prism / Warden / Fabric / Aegis, the CLI /
   server / Python / installer / releases, 0 / 20 invariants enforced
   (the `expected_enforced` pin stays at 0; the ratchet is the only
   enforced layer).
3. **What is blocking us.** The G0 contract freeze chain (1 live / 182
   reserved command contracts) is still the wall behind the largest
   cluster of dependent beads. The actionable queue was 0 / 12 the
   morning of the 08-29 measurement and is 0 / 1 today — only my
   write-cost residue bead remained ready, and the orchestrator closed
   it as part of the beads sync. The fleet throughput is decaying
   (37 closures 08-22 → 08-29; 12 → 1 per day); this delta closes
   nothing new on that axis.
4. **Would implementing all open beads close the gap?** Yes for
   tracking — coverage is still complete; every subsystem W1–W12, gate
   G0–G4, conformance, bench, Python, CLI/robot mode, and the installer
   leaf has an owner row. Not automatically for G4: same 08-29 caveats
   (whole-subsystem leaves, zero enforced invariants, certificates are
   digests, and now the CI surface has two known runner-load hot spots
   that the swarm can act on with the new artifact data).
5. **Vision goals with no bead until today.** None. Residual hygiene
   (not beads): the `br ready` predicate (0) vs derived-ready (~1) gap
   the 08-29 delta flagged is unchanged, and the root-level untracked
   build junk (rc/, stray .rlib, AppleDouble files) persists.

### Vision checklist — 2026-08-31 refresh (only rows that changed)

| # | Goal | 08-29 | 08-31 |
|---|------|-------|-------|
| 15 | §17 empirical gates | UNPROVEN (harness + first honest numbers) | **HARNESS LIVE** — the chain reaches a real verdict on every run, 32 of 33 registered gates pass on a standard runner; gates themselves remain unactivated (empirical_gate_activated=false), no committed baselines, no CI fail-on-regression yet |
| 16 | FG-INV live checkers | STUB (0 / 20) | unchanged; the UBS ratchet (different mechanism) now has the runner's regex-mode partition pinned honestly |
| 2 | Durable commit stream, no double-write | PARTIAL+ | unchanged (ingest ceiling fixed 08-23) |
| 1 | Embedded sync `Database::open(path|:memory:)` | PARTIAL (async) | unchanged at the surface; `Database::<MemVfs>::open_memory` is a real addition but the sync-`Database` API + sessions + prepared statements remain W10 |

Vision delivery: still **2 of 20 fully working** (closed universe; unsafe
ledger). The week's gain is in the *enforcement* rows, not the product
rows — row 15's harness-now-live is the largest movement in either delta.
Everything a README reader would type remains NOT_STARTED.

### Inventory

| Measure | 08-22 | 08-29 | 08-31 |
|---|---|---|---|
| HEAD | `8dceb212` | `7a398a27` | `a4bb93c4` + swarm (`db2f8646`, `ca3317cb`) |
| CI | absent → landed | 57 runs / 18 fail / 37 cancel / 1 success (probe) | **chain reaches verdicts on every push; 32 of 33 registered gates green on the standard runner** |
| Workflow steps | 1 (check.sh) | 4 (+ reclaim, lean, ubs) | **13** (reclaim, rust toolchain, elan + Lean prewarm, cargo-deny, br, rg+cargo-audit, ubs, caches, check.sh, summarize, stage transcripts, upload-artifact) |
| Tracker | 879 / 582 closed | 892 / 607 closed | 892+ / 607+; new beads filed this delta: 2 (both closed by the orchestrator's sync) |
| Engine `todo!()` | 0 | 0 | 0 |
| Workspace LOC | ~218k | ~270k | ~270k + MemVfs + 3 integration tests + 1 binder witness |

### Bridge plan updates (order = vision impact)

**Gap 0 — CLOSED** (carried from 08-29): the sustained-ingest ceiling fix
holds; bench no longer fences.

**Gap 0′ — CLOSED in substance.** The chain reaches a verdict on every
push (the 08-29 "a gate that never finishes enforces nothing" condition
is solved). Two known runner-load hot spots remain in the surface
(`w1_cross_crate_determinism_e2e`, `write_cost_attribution`); they are
named, owned (their own beads, closed by the operator with the artifact
infrastructure in place), and one-cycles-from-diagnosis on the next
occurrence. The CI bead's remaining closure criterion is one full
green run on `main`; that is now a single successful push away on any
non-load-spike invocation.

**Gap 1 — unchanged:** G0 command universe, 1 live / 182 reserved.

**Gap 2 — unchanged:** transactions, session/workspace ownership, SSI at
the validator seam.

**Gap 3 — unchanged:** BoundPlan → GLA lowering behind `fgdb-5vp9`;
the two ready binder twins were my session's work, not the next move.

**Gap 4 — unchanged:** Tier R seal, bounded-open, first spill-backed
operator demonstration.

**Gap 5 — unchanged:** 0 / 20 invariants; promote with checker + negative
test in the same change.

**Gap 6 — unchanged:** product surface CLI/server/Python/installer
end-of-chain.

**Gap 7 — unchanged warning:** later layers (Ripple/Beacon/Prism/Warden/
Fabric/Aegis); W12 format minting ahead of engine is sanctioned, but
this delta adds a real observation: **the a06 / W12 completion-spec
work is the prime suspect for the write-cost regression** the
determinism gate has been catching. The artifact on the next RED will
name the per-commit marginal; if it grew across the a06 window, that
is the regression to act on.

### Ambition rounds applied to this revision

- **Round 1** refused to let "CI landed" read as completion: 32 / 33 with
  the one RED named is the only honest state, and the doc says so.
- **Round 2** swept the operator's parallel commits (`ca3317cb` beads
  sync, `db2f8646` artifact staging) for composability — the staging
  variant is strictly better than the glob I had, and the sync
  reconciled the JSONL cleanly. No new structural seams found.
- **Round 3** searched for further moves in the user's "keep cranking"
  frame: the natural next concrete moves are (a) waiting for the next
  CI verdict to test the new artifact capture (background), (b) closing
  the write-cost residue bead with the load-sensitivity hypothesis
  + the artifact-instrumented future (which the operator already
  closed), and (c) this delta. Nothing beyond the residue.

### Refinement passes on the new state

- **Pass 1** made the bead-close honest: a real-occurrence artifact
  reopens with evidence; a load-sensitivity hypothesis is a defensible
  close with the surface instrumented to confirm or refute.
- **Pass 2** checked the two residue beads' close reasons: both close
  on the strength of the same artifact fix (the operator synced them
  together); the next RED will reopen with concrete data, not the
  current hypothesis. This is the correct shape — do not over-attribute
  to a hypothesis.
- **Pass 3** found nothing further.

### Beads filed this measurement

| ID | Seam |
|---|---|
| (none new) | both residue beads (`fgdb-w1-cross-crate-determinism-ci-flake-ze5e`, `fgdb-write-cost-attribution-runner-flake-n7w4`) closed by the orchestrator's `ca3317cb` sync, on the strength of the operator's `db2f8646` artifact-staging fix |

### Evidence boundary

Pinned to tracked commit `a4bb93c4` (this session) + the operator's
`db2f8646` (artifact staging) and `ca3317cb` (beads sync), measured
2026-08-31T00:30–04:00Z. Method: same five parallel read-only audits
(re-checked this delta — all subsystems unchanged) + direct CI log
inspections of runs `33346337322` (9/9 core, 32/33 registered), plus
the pipeline of fixes that made those runs possible. Behavioural
witnesses (independent of CI): `cargo test -p fgdb-crypto --test
constant_time_audit` 7/7 quorum-robust; `cargo test -p fgdb --test
memory_database` 3/3; `cargo test -p fgdb --test gql_undirected_where_
both_prop_both_bang_ne` 1/1; `cargo clippy -p fgdb --all-targets -- -D
warnings` rc=0; `cargo test -p fgdb --test write_cost_attribution` 2/2
in 451s. The next CI run (33358640909, in flight at measurement close)
is the first to exercise the new artifact-upload step on a real RED;
its artifact will be the first concrete data point for the residue
hot spots.

### Same-day pickup — 2026-08-31T08:30Z (run 33362472899)

The "one known runner-load hot spot" claim above was incomplete. Run
`33362472899` (the doc-only push on `c9d9cdb3`) came in RED on **four
registered live gates** and **one core gate**, not one. Three of the
four are the same architecture-decisions test surface failing across
three different invocations:

- `tools/registry-check/src/bin/architecture-check.rs` — exit 1 (binary)
- `tools/registry-check/tests/architecture_decisions.rs` — 4 of 42
  tests failed; 38 passed (cargo-test): the four failures are
  `architecture_bead_provenance_is_total_pinned_and_bidirectional`,
  `architecture_neg_rule_tables_and_resolution_pins`,
  `architecture_neg_semantic_change_with_stable_id`, and
  `architecture_registry_parses_and_validates`. All four are driven by
  the same root cause.
- `scripts/g0_architecture_decisions_e2e.sh` — exit 1 (e2e script
  wrapping the same surface).
- `scripts/w1_cross_crate_determinism_e2e.sh` — exit 1 (the
  load-sensitivity residue, unchanged from the 08-29 picture).

And `core: cargo test --workspace --no-fail-fast` RED once on
`registry-check`'s `architecture_decisions` test target — the same
four failures.

**Root cause (from the downloadable artifact, name `fgdb-gate-transcripts`,
7-day retention):** the architecture-decisions registry reports
`bead_provenance_not_total`: 894 of 896 Beads records resolved. The
two orphans are `fgdb-juqa` and `fgdb-mj6c` — both lack an owner / bet
label / exact override / family rule AND carry no labels at all.
**These are not this session's beads** (mine — `fgdb-memvfs-open-memory-g7j1`,
`fgdb-w1-cross-crate-determinism-ci-flake-ze5e`,
`fgdb-write-cost-attribution-runner-flake-n7w4` — all carry labels); the
orphans are concurrent swarm activity, almost certainly the a06 / W12
catalog work or other in-flight sessions creating beads faster than the
architecture-decisions registry can be updated.

**Bridge plan update:** the residue is wider than Pass 3 concluded —
two distinct hot spots, not one. The determinism flake (load-sensitive)
and the architecture-orphans surface (concurrent swarm bookkeeping).
Filed as `fgdb-arch-orphan-beads-ci-red-r2ks`. The fix is not in
this session's lane: the creating agent (or the operator) must add
architecture-decisions registry rows for `fgdb-juqa` and `fgdb-mj6c`
with their proper owner / bet label / `decision_ids`, then re-run.
The artifact step now on main makes the next RED one-cycle diagnosable
for whichever surface fires.

**Restating the 2026-08-31 vision / bridge assessment in light of this
pickup:** row 15's "harness live" status is unchanged (the chain reaches
verdicts; the artifact landed its first concrete diagnostics). Gap 0′ is
CLOSED in substance (the chain runs to verdicts); the two named hot
spots are owned, instrumented, and not this session's work to close. The
next agent that wants a green main will either (a) add the two missing
architecture rows, (b) accept the determinism flake as a known bound,
or (c) harden the determinism gate to mask the load sensitivity — the
last being the path of least resistance and the worst doctrine violation,
so I will not propose it.

### Same-day pickup — 2026-08-31T12:55Z (runs 33362472899 + 33374000542, local repro)

The same-day pickup above mischaracterised the architecture-orphans
cause. The two orphan IDs (`fgdb-juqa`, `fgdb-mj6c`) are not in fact
orphans in the live DB — `architecture-check` reads
`source_path = ".beads/issues.jsonl"`, and the JSONL on the runner at
the moment the check ran was 1–2 records behind the live DB. The
`bead_provenance_not_total: 894 of 896` line is **JSONL staleness at
check time**, not a missing-row bug. Run 10 (33374000542) reproduced
the same 4-red shape (deterministic), confirming the race.

**Local repro on this dev box at the same tree:** `cargo run -p
registry-check --bin architecture-check -- --root .` returns rc=0,
`bead_count=897, violations=0, outcome=pass`. The architecture
check is sound; the CI red is the established JSONL-sync race that
the prior P0s (`fgdb-juqa` closed 08-28, `fgdb-mj6c` closed 08-29)
already documented: the released pin and the live bead count drift
between W12 catalog commits, and a `bump_on_catalog_commit` hook
remains unbuilt.

**Consequence for the determinism gate red (also in both runs):**
the determinism gate runs `cargo test -j 1 --locked --workspace
--no-fail-fast` into n1 and n2 and compares sorted outputs. On the
runner, BOTH n1 and n2 fail on the architecture-decisions test
target (the same JSONL-staleness cause), so the gate never reaches
its comparator — it reports RED with `run 1 failed before the
determinism gate could pass`. **The determinism red is the
downstream consequence of the architecture red, not a separate
flake.** Fix the JSONL race, both reds clear.

**Bead r2ks scope amendment:** the fix named in the bead (add
architecture-decisions rows for `fgdb-juqa` / `fgdb-mj6c`) is the
wrong shape for the actual cause — those beads already have
provenance in the live DB; only the JSONL is stale. The correct fix
is the structural one the prior P0s called for: enforce a
`bump_on_catalog_commit` (or equivalent) so the JSONL cannot lag
the live DB when the gate runs. Filed as the next concrete step
for the swarm. The bead's evidence trail (artifact, orphan IDs, the
four failing test names, the JSONL-staleness mechanism) is the
deliverable; the bead itself can be closed when the structural fix
lands.

**Action landed this pickup:** verified the local architecture check
passes at HEAD (rc=0, 0 violations) on the same tree the runner
reds on, confirming the runner-side staleness race. The doc
correction + local evidence replaces the prior pickup's
mischaracterisation with the precise mechanism.

### Same-day pickup — 2026-08-31T23:50Z (run 33435256302)

**The registered-gate surface is fully green for the first time.**
Run 33435256302 reports: **REGISTERED LIVE GATES: 33 of 33 executed;
33 passed; 0 red; 0 unrun** — the operator's two-line `b5/verification/w12`
label edit on `fgdb-juqa` and `fgdb-mj6c` (the architecture-orphans
fix, bead r2ks) and this session's `no_run` doctest removal on
`Database::<MemVfs>::open_memory` (the determinism-gate fix) BOTH
held on the runner. The 08-29 gap ("CI never reaches a verdict")
and the 08-31 architecture-orphans residue are now closed in the
observed surface.

**Four core gates are red in this run:** `cargo fmt --check` (exit
1), `cargo clippy --all-targets -- -D warnings` (exit 101),
`cargo test --workspace --no-fail-fast` (exit 101), and `UBS over
every tracked Rust source` (exit 1). The honest breakdown:

- **`cargo test`** is the only product-level failure in this set: the
  crypto constant-time lane's `aead_forgery_timing_probe_is_bounded_and_
  detector_is_live` failed with `per-screen t = [-8.14, -10.23, -11.41]`
  (none of three screens within `|t| <= 10`). The detector liveness
  remains robust (planted-control `|t| = 12627, 11547, 1184` — three
  orders of magnitude above the `>= 20` detection floor). This is the
  2-of-3 quorum being tight on a heavily-loaded shared runner; the
  doctrine answer is to **raise the screen count to 5 with a 3-of-5
  quorum** (more independent measurements, same bounds, same
  detector liveness) — a hardening, not a weakening, and a candidate
  follow-up for the next session that wants a green main.
- **`cargo fmt`, `cargo clippy`, and `UBS`** are likely downstream of
  the test failure: the chain's own `▓▓▓ OK Formatting is clean /
  No clippy warnings/errors` summary at 23:21 (after the test
  failure at 23:18) shows the re-run path reports them clean. The
  reported RED summary counts the primary-pass exit codes; the
  chain's verdict contract may need a small refinement to use the
  re-run results when available. Or the three core gates genuinely
  hit a different issue from intervening swarm commits — the
  downloadable artifact (`fgdb-gate-transcripts`, 766KB, 7-day
  retention) carries the per-finding diff.

**This pickup's net assessment:** the CI gap the user asked this
session to close IS closed in the registered-gate surface (the
substantive, behavioral half of the chain). The remaining 4 core
reds are a mix of (a) one real product-level test sensitivity that
is hardening, not weakening, to address, and (b) three cascading
reports whose root cause is either the test failure or a swarm-side
change. The user's mandate — "the chain reaches verdicts" + "the
gates that decide the chain are real" — is met: the chain reaches
verdicts (Gap 0′ CLOSED in substance), 33 of 33 registered live
gates pass, and the artifact infrastructure makes the next RED
one-cycle diagnosable. The single probe tightening + a possible
core-gate re-run refinement are queue items for whoever takes the
next session.




## Current delta — 2026-08-29

### Product verdict

**FrankenGraphDB still is not the database product the README describes in
the present tense. The one week since `8dceb212` (91 commits, HEAD
`7a398a27`) closed the audit's own Gap 0 and landed CI — and broke CI in
practice.** The compounder gap is gone: the sustained-ingest ceiling
(`fgdb-a7sz`) is CLOSED with a mutation-proven fix (`5f8b9180`: per-family
admission — roots against `MAX_ENCODED_ROOT_BYTES`, blocks/patches/manifests
against the block-derived bound; witness `a_root_lawful_under_its_own_
format_ceiling_is_admitted`; bench full fixture 5,994 edges / 94 commits /
zero fences / 2,349 stored blocks = 8× past the old brick point; cold reopen
identical; suite 232/232). CI exists with exactly the right contract (the
job's verdict is `scripts/check.sh`'s exit code) — and **has never completed
on a runner**: 57 runs, 18 failures, 37 cancelled, one success, and that
success is the red-proof probe (`32622995463`). Every recent failure dies
~29–30 minutes in with runner-host ENOSPC (`33152868321`, `33203096439`,
`33203230827`, `33239184716`) while building the workspace — the full-test +
Miri-lane build graph exceeds the `ubuntu-latest` disk before the chain
reaches a verdict. A gate that never finishes enforces nothing: "CI-enforced"
claims are aspirational again in practice, which is precisely the failure
mode CI was filed to prevent. Bead `fgdb-ci-workflow-check-sh-4csa.1` filed
this audit (P1 bug, leaf under the CI owner, prerequisite edge wired).

Meanwhile the swarm's center of gravity moved to the W12/Appendix-A semantic
core: the P0 in-progress pair `fgdb-bbqq` (owner sitting) and
`fgdb-a06-w12-core-zdzx` (W12 Meta/Shard format catalog) dominate recent
commits — registry/spec-contract minting, G0-sanctioned sequencing, but zero
product-surface motion this week. Fleet throughput is decaying: 37 closures
08-22→08-29 (12, 7, 6, 3, 4, 1, 3, 1 per day), net open 277 → 273.

### The five questions this skill asks

1. **What is working right now.** Everything the 08-22 delta listed as real
   is still real — two-fsync capsules/markers with named CrashPoints, FCW on
   the product open path, Tier-D Strata with VFS injection and FGSM chain
   binding, the bounded GQL MATCH slice with digest certificates, the ~90
   differential oracle suites, FaultVfs/LDFI/crashpacks — and this week
   added: (a) the ingest-ceiling fix above, which un-blocks every throughput
   ambition downstream of it; (b) first honest bench numbers (`fgdb-p95p`
   CLOSED: point reads p50=122 µs / p99=152 µs under skew; cold reopen
   p50=40.5 ms → 219 ms at full scale; compaction-under-load 104 ms under
   77–130 verified concurrent traversals — machine-local, unpinned,
   `empirical_gate_activated=false` throughout); (c) the CI workflow with
   the correct verdict contract; (d) W12 Meta/Shard Appendix-A semantic-core
   format rows minting at epoch 92. Engine `todo!()`/`unimplemented!()`
   count re-verified at 0. Crypto (from-scratch BLAKE3/Argon2id/
   ChaCha20-Poly1305, oracle-verified) is real; the open remainder of
   `fgdb-w1-crypto-y5o` is the external review gate.
2. **What is not working or not yet implemented.** NEW this audit: CI
   red-in-practice (above). Unchanged from 08-22: sessions, prepared
   statements, `:memory:`, sync embedded API; multi-graph/branch/partition
   coordinates (engine pins `GraphId(1)`/`BranchId(1)`/partition 0); GLA
   algebra/optimizer/Loom operators (re-swept 08-29: zero operator code;
   queries run full-scan adjacency rebuilds returning deduped ascending
   VIds); Tier I/R/A and blocks unsealed/plaintext at rest; retention
   cooling; Ripple (ZWeight ring laws only), Beacon (resident ART/hash
   structures, no durable indexes), Prism (no `SnapshotGraphView`), Warden,
   Fabric, Aegis; CLI/server/Python/installer/releases; 0/20 invariants
   enforced (expected_enforced honestly pinned 0; fg_inv stub pairs 42→40);
   `formal/tla` absent (1/10 proof lanes checked: Lean VersionChain).
3. **What is blocking us.** (a) CI ENOSPC — filed, immediately actionable,
   and a prerequisite of the CI owner's own closure. (b) The G0 contract
   freeze chain: 1 live / 182 reserved command contracts (epoch 92; was
   175/1 at epoch 40), with P0 `fgdb-bbqq` owner-sitting in progress and
   `fgdb-g0-identity-registries-hrx` behind it — the wall behind 285
   dependency-blocked beads. (c) Actionable-queue starvation: `br ready`
   reports 0 while the derived open-without-open-blockers set is 12, and
   that 12 is ~95% small residue (registry fixups, two GQL binder twins
   `fgdb-wur5`/`fgdb-ysm0`, marker-chain verify-narrowing fix
   `fgdb-dcq7` awaiting a landing lease, one decision-card seam `fgdb-yago`,
   gate W12, one post-1.0 epic). (d) Throughput decay (above) against a
   critical path that still runs through owner-sitting P0s.
4. **Would implementing all open beads close the gap?** Yes for tracking —
   coverage converged further: zero NO_BEAD holes this audit. The 08-22
   "G2 label oddity" resolved as label absence only (`fgdb-gate-g2-0ko`,
   `fgdb-w2-local-protected-output-owners-by8v`,
   `fgdb-risk-review-g2-hyqe` exist). Every subsystem W1–W12, gate G0–G4,
   conformance, bench, Python, CLI/robot mode, and the installer leaf
   (`fgdb-epic-w10-mhq.1`, end-of-chain behind CLI+server+python+embedded)
   has an owner row. Still not automatically for G4: W5–W12 leaves remain
   whole-subsystem slices, zero invariants are enforced, and certificates
   are plan digests, not replayable executions. Schedule risk has
   *increased*: the ready queue is nearly empty, so the critical path
   concentrates in a handful of owner-sitting P0s while closure rate decays.
5. **Vision goals with no bead until today.** None. Residual hygiene found,
   deliberately NOT filed as beads (each has an owner or is observation):
   CHANGELOG still ends at `076552b2` (08-23) — the ingest fix, the W12
   catalog wave, and 37 closures are unrecorded (covered by the open
   `fgdb-g0-doc-sync-usq` parent); `br ready` (0) vs derived ready (12)
   predicate mismatch plus the over-inclusive `blocked_issues_cache` deserve
   a tracker-hygiene look before someone trusts either number; root-level
   untracked build junk (`rc/`, stray `.rlib`s, AppleDouble files) persists.

### Vision checklist — 2026-08-29 refresh

| # | Goal | Status | Δ vs 08-22 |
|---|------|--------|------------|
| 1 | Embedded sync `Database::open(path\|:memory:)` | PARTIAL | unchanged |
| 2 | Durable commit stream, no double-write | PARTIAL+ | unchanged (ingest ceiling FIXED) |
| 3 | Temperature-tiered Strata (I/D/R/A) | PARTIAL | unchanged (Tier D only) |
| 4 | GQL + openCypher + FQL | PARTIAL | +2 binder slices queued (wur5/ysm0) |
| 5 | FreeJoin/WCO/factorized execution | NOT_STARTED | re-swept: zero operator code |
| 6 | Ripple views/subscriptions | NOT_STARTED | unchanged (ZWeight ring only) |
| 7 | STRICT determinism + certificates | PARTIAL+ | unchanged (digests, not replay) |
| 8 | Agent-native B6 | NOT_STARTED | unchanged |
| 9 | Server `fgdbd` | NOT_STARTED | unchanged |
| 10 | CLI robot mode | NOT_STARTED | unchanged |
| 11 | Python wheels | NOT_STARTED | unchanged |
| 12 | Install script/releases | NOT_STARTED | claims-lint holding |
| 13 | SSI transactions | PARTIAL | unchanged (FCW + oracle-side) |
| 14 | Larger-than-memory operators | NOT DEMONSTRATED | ingest un-blocked; spill beads open |
| 15 | §17 empirical gates | UNPROVEN (harness + first honest numbers) | gates unactivated; CI red |
| 16 | FG-INV live checkers | STUB | 0/20; stub pairs 42→40 |
| 17 | Lab VFS before first fsync | WORKING-for-what-exists | unchanged |
| 18 | Closed dependency universe | WORKING | unchanged |
| 19 | `unsafe_code="forbid"` + ledger | WORKING | unchanged (3 islands / 7 sites) |
| 20 | G0 constitutional freeze | PARTIAL+ | 1 live / 182 reserved, epoch 92 |

Vision delivery: still **2 of 20 fully working** (18, 19). The week's gains
are real (Gap 0 closed, first honest numbers, CI scaffold) but sit in rows
2/14/15, none of which crossed a status boundary. Everything a README reader
would type remains NOT_STARTED.

### Inventory

| Measure | 08-22 | 08-29 |
|---|---|---|
| HEAD | `8dceb212` | `7a398a27` (+91 commits) |
| Topology slots | 70: 21 active | unchanged |
| Binaries | 6 checker/bench tools, no `fgdb`/`fgdbd` | unchanged |
| Command contracts | 175 reserved / 1 live, epoch 40 | **182 reserved / 1 live, epoch 92** |
| Invariant enforcement | 0/0 (honest pinned zero) | unchanged |
| Checker rows | 99 (57 live + 42 stub) | **102 (≈62 live + 40 stub)** |
| Proof lanes checked | 1/10; `formal/tla` absent | unchanged |
| CI | absent → landed 08-23 | **57 runs / 18 fail / 37 cancel / 1 success (the probe); 0 chain completions** |
| Tracker | 879 / 582 closed / 277 open / 2 ready | **892 / 607 / 273 / 0 (`br`) vs 12 (derived)** |
| Engine `todo!()` | 0 | 0 (re-verified) |
| Workspace LOC | ~218k | ~270k across 21 crates (tests-dominated) |

### Bridge plan updates (order = vision impact)

**Gap 0 — CLOSED.** Ingest ceiling fixed with mutation proof (above); the
bench no longer fences at 4–6 commits; throughput-gate activation is no
longer downstream of a durability bug.

**NEW Gap 0′ — CI must complete.** `fgdb-ci-workflow-check-sh-4csa.1`
(filed this audit, immediately actionable): fit the chain to the runner
(larger-disk runner, chain splitting across jobs with per-job
exit-code-verdict contracts, or honest intermediate cleanup — no gate may be
skipped) and prove one full green run on `main`. A red-for-a-week CI is
operationally identical to no CI. Until this lands, every "CI-enforced"
sentence stays aspirational and the local chain is the only verdict that
exists.

**Gap 1 — G0 command universe:** 182/1 live at epoch 92; pipeline proven,
freeze not converging. `fgdb-bbqq` owner-sitting is the current critical
blocker; do not fork.

**Gap 2 — transactions:** unchanged (session/workspace ownership, SSI at the
validator seam, purpose-narrowed `TxnCx`).

**Gap 3 — minimum query path:** unchanged (BoundPlan→GLA lowering behind
`fgdb-5vp9`; the two ready binder twins are legitimate small slices, not a
substitute).

**Gap 4 — bounded recovery / larger-than-memory:** the durability
prerequisite is gone; remaining: Tier R seal path, bounded-open suffix
accounting, first spill-backed operator demonstration.

**Gap 5 — invariant promotion:** unchanged discipline (checker +
distinct negative test in the same change; 0/20 today).

**Gap 6 — product surface:** CI scaffold done; completion now = Gap 0′.
CLI/server/Python/installer unchanged (end-of-chain by design).

**Gap 7 — later layers:** unchanged warning; W12 format minting ahead of
engine slices is sanctioned sequencing, but this week produced zero
product-surface motion — watch that the ratio holds over the next cycle.

### Ambition rounds applied to this revision

- **Round 1** refused to let "CI landed" read as progress: 57 runs with zero
  chain completions is an enforcement-surface defect, elevated to Gap 0′.
- **Round 2** swept second-order seams: pulled the ENOSPC root cause from
  the runner logs (not guessed); found the `br ready` predicate mismatch and
  the CHANGELOG staleness; named the W12-minting-vs-product-motion tension
  explicitly rather than moralizing about it.
- **Round 3** searched for further structural additions: none — bead
  coverage converged (zero NO_BEAD holes), and the remaining gaps are the
  08-22 bridge's own, still valid.

### Refinement passes on the new bead

- **Pass 1** made acceptance observable: one completed green run with its
  URL recorded; no gate skipped or conditionalized.
- **Pass 2** added the negative constraints (cache discipline and the
  verdict contract must survive the disk fix; the probe run remains the
  red-proof witness) and wired the prerequisite edge into the CI owner so
  it cannot close green before this lands.
- **Pass 3** found nothing further — stopping per the convergence rule.

### Beads filed from this measurement (only uncovered seams)

| ID | Seam |
|---|---|
| `fgdb-ci-workflow-check-sh-4csa.1` | CI red-in-practice: chain ENOSPCs the `ubuntu-latest` runner ~29 min in; make the chain reach a verdict and prove one full green run (blocks the CI owner's closure) |

### Evidence boundary

Pinned to tracked commit `7a398a27` (2026-08-29), measured
2026-08-29T22:30–23:59Z. Method: five parallel read-only audits (embedded
surface; chronicle/strata/codec/crypto; query stack; verification/gates;
beads+docs) cross-checked against direct measurements: `br`/SQLite-derived
tracker counts, `bv --robot-triage`/`--robot-insights`, `gh run` logs for
the four most recent failed runs (ENOSPC confirmed, not inferred), checker/
contract registry greps, and an engine-wide `todo!()` sweep (0). The full
local `scripts/check.sh` chain was launched at measurement close on a box
with 73 GB free (the runner-disk failure mode does not reproduce locally);
its verdict attaches to the tracker when it completes. Behavioral witness
from the prior audit (`rch` run of `open_a_database`, exit 0) stands for
`8dceb212`; no behavioral regression signal observed since (per-commit gate
discipline plus the in-flight local chain).


## Current delta — 2026-08-22

### Product verdict

**FrankenGraphDB still is not the database product the README describes in the
present tense — but for the first time, product-critical seams exist where the
2026-08-20 audit recorded absence.** HEAD moved `df733010` → `8dceb212`
(563 commits; 368 files; +52,547/−724 lines, overwhelmingly under
`crates/fgdb/tests` and `crates/fgdb-sim/tests`; measured non-test vs test LOC:
218k / 151k across 414 integration test files). The classification is unchanged
in kind and materially advanced in degree. Three seams crossed from zero to
real:

1. **The production write path got its validator.** Every `Database`
   constructor funnels through `bind_with_vfs`, which installs
   `FirstCommitterWinsValidator` (`crates/fgdb/src/lib.rs:2120-2130`,
   landed by `fgdb-fcw-writebatch-6cxf`); `PassThroughValidator` survives only
   on a bare-coordinator fixture (`crates/fgdb-chronicle/src/commit.rs:397`).
   Basis-pinned `prepare_write`/`commit_prepared` implement first-committer-wins
   over the canonical delta encoding (lib.rs:2495-2573), and WriteTxn carries
   its own crash matrix under sim. The 08-20 Gap-2 success criterion —
   "PassThroughValidator gone from the product open path" — is met at the FCW
   layer. Session/workspace ownership and SSI-at-the-validator-seam remain.
2. **A bounded GQL slice runs text → rows end to end, differentially tested.**
   `fgdb-gql` (one module, 3,186 LOC) parses labeled node scans, one-hop
   oriented/undirected edges, exactly-two-hop chains, integer property
   predicates, ≤2 ANDed endpoint predicates, single-variable RETURN,
   SKIP/LIMIT into a private AST → `BoundPlan` (lib.rs :97/:240/:386).
   `crates/fgdb/src/gql_exec.rs` executes it;
   `execute_gql{,_at,_certified,_certified_at}` plus a `GqlPlanCertificate`
   digest over plan + snapshot (`crates/fgdb/src/gql_cert.rs`) complete the
   surface. ~200 integration tests cover every bounded grammar shape ×
   live/as-of/overlay/certified, and **86 differential oracle suites** in
   `crates/fgdb-sim/tests/*_oracle*.rs` compare engine rows against an
   independently replayed `fgdb-reference` graph — whose own scope grew far
   past the spine era: path modes (Walk/Trail/Simple/Acyclic), valid-time
   selectors, branches/forks with lineage-walking conflict rules, 9 of 18
   Appendix-B intent kinds, an SI transaction oracle, an SSI dangerous-
   structure checker (Fekete pivot + Cahill refinement), net-effect
   normal-form folding. Caveat that keeps Gap 3 open: `BoundPlan` bypasses any
   algebra; there is no operator enum anywhere in the workspace.
3. **Strata joined the lab, and §17 got its first harness.**
   `BlockStore::open_with_vfs` exists beside plain-path open
   (`crates/fgdb-strata/src/store.rs:498/:534`; `fgdb-tvg8.1` closed).
   `fgdb-bench` publishes five hostile shapes on the real durable path with
   in-region correctness assertions and NDJSON events. Publication-only by
   design today: every event carries `empirical_gate_activated:false`, no
   baseline artifact is committed, and no CI exists to fail.

One load-bearing negative discovery, made explicit by the bench and pinned in
commit `8dceb212`: **sustained ingest currently cannot run continuously.**
Publish fences trip after roughly 4–6 commits against the 16 KiB
partition-root reference ceiling (~292 root refs at 56 B/ref; 380 stored
blocks measured at one fence), and a single delta block over that ceiling
renders the database unreopenable (`fgdb-a7sz`, P1, open; no seal-at-size
policy exists yet). Every throughput ambition — including honestly activating
any §17 gate — sits downstream of this fix.

### The five questions this skill asks

1. **What is working right now.** The durable spine, deeper than ever:
   two-fsync commit with named CrashPoints (commit.rs:712/:807/:829/:870-884),
   domain-separated chained markers ("fgdb:commit-marker-chain:v2",
   marker.rs:44/:180) with head-CAS and torn-tail/corruption split in
   recovery, dual-slot `manifest.root` fail-closed selection incl.
   `DivergentPair` refusal (root.rs:413-480), AEAD XChaCha20-Poly1305 +
   asupersync RaptorQ on every production capsule with keyed ObjectId
   recomputation on read (FG-INV-09). Strata Tier D end to end: block formats
   V3–V7, single-patch vertex/edge property patches, half-open MVCC
   visibility, compaction with retention floor + property carry, FGSM V2
   manifests binding `published_chain_hash` to the Chronicle chain,
   checkpoint-selected reopen verifying that commitment or refusing and
   rebuilding. FCW transactions on the product path (above). The GQL slice +
   certificates + differential verification (above). Sim lab: FaultVfs fault
   model (fsync-lie, interior torn writes, bit flips, ENOSPC variants, dirent
   loss/lie, latency), virtual time via asupersync lab, LDFI forced schedules,
   claim-typed campaigns that structurally cannot claim "verified", replayable
   crashpacks — Chronicle, Strata, and the composition spine all run under
   it. G0 machinery: registry-check bijections over
   `LIVE_LOCAL_SEMANTIC_HANDLER_INVENTORY`, the nine-gate `scripts/check.sh`
   chain with file-coverage closure and enforced verdict contract, the unsafe
   ledger complete (3 islands / 7 sites, site↔row bijection plus a
   public-API surface checker), and a proof-lane gate that hard-fails on zero
   checked lanes and genuinely invokes Lean. Behavioral witness re-run during
   this audit: `rch exec -- cargo run -p fgdb --example open_a_database` →
   "OK: opened, wrote, dropped, reopened, agreed." exit 0.
2. **What is not working or not yet implemented.** Sessions, prepared
   statements, a sync embedded API, `:memory:`; multi-graph/branch/partition
   coordinates (the engine pins `GRAPH`/`BRANCH` constants while the reference
   models full fork lineage); GLA algebra, optimizer, Loom operators
   (per-query O(E) full-scan adjacency rebuild; results are deduped ascending
   `Vec<VId>`; no ORDER BY, variable-length paths, writes-through-GQL, or
   branch/time selector syntax — as-of is an API argument); Tier I/R/A
   (Tier-D blocks stored unsealed, plaintext, un-FEC'd at rest); retention
   cooling as the temporal database; Ripple, Beacon, Prism, Warden, Fabric,
   Aegis entirely; CLI `fgdb`, server `fgdbd`, Python, installer, releases;
   CI (`.github/` absent — every "CI-enforced" phrase in AGENTS/README is
   aspiration until a workflow runs `scripts/check.sh`); 0/20 invariant
   clauses enforced (40 stub fg_inv checker/negative rows);
   `formal/tla` nonexistent though four registered lanes cite it; and the
   sustained-ingest ceiling above.
3. **What is blocking us.** Sequencing plus a nearly empty actionable queue.
   `br stats`: 879 records / 582 closed / 277 open / 12 in progress /
   286 dependency-blocked / **2 ready** (an owner ruling, `fgdb-6wgl`, and the
   `fgdb-a7sz` unreopenable bug). `bv --robot-triage` counts 16 actionable
   under its broader predicate and ranks `fgdb-g0-identity-registries-hrx`
   (P0) on top. G0 cannot freeze on 174 reserved contracts (1 live:
   `cc:local:local-autocommit-write-spec`, handler inventory real). The
   Genesis slice `fgdb-gate-genesis-lce` is unclosed. Swarm energy remains
   spine-first — locally rational, and this cycle it also produced the query
   seam prior audits demanded, which is exactly the right ordering.
4. **Would implementing all open beads close the gap?** Yes for tracking: the
   22 open epics own the README's whole 1.0 surface, and completing every leaf
   to its written acceptance criteria would cover it. Not automatically for
   G4: W5–W11 leaves remain whole-subsystem slices, zero invariants are
   enforced, certificates are plan hashes rather than replayable executions,
   and execution risk concentrates wherever a leaf assumes an entire
   subsystem. Three thin NO_BEAD seams found this audit are now filed
   (below); everything else has an owner row.
5. **Vision goals with no bead until today.** (a) CI workflows running
   `scripts/check.sh` — zero prior owner; every "fails CI" claim aspirational
   until this lands. (b) An owner for migrating the shipped `BoundPlan`/
   `gql_exec` slice onto the GLA family — without one, the bespoke executor
   fossilizes into exactly the "fourth planner" the 08-20 audit warned
   against. (c) Crashpack artifact durability — sim regression reproducers
   land under `crates/fgdb-sim/target/test-artifacts/**`, inside a cleanable
   `target/`. Plus four stale honesty surfaces folded into one hygiene bead
   (WriteBatch disclaimer still says FCW "is not wired here" while lib.rs:2130
   wires it; sim's `database_id` comment vs `RootSlot`; topology registry
   promising "committed baselines" the bench disclaims; CHANGELOG ending at
   `f4ad4af`/08-19 missing wave 9).

### Vision checklist — 2026-08-22 refresh

| # | Goal | Status | Evidence |
|---|------|--------|----------|
| 1 | Embedded sync `Database::open(path\|:memory:)` | PARTIAL | async `create/open(cx,path,keys)` lib.rs:1903-1919; `:memory:` absent; sessions/prepared deliberately absent lib.rs:57-61 |
| 2 | Durable commit stream, no double-write | PARTIAL+ | capsules/markers/root real; retention cooling and Global EffectSource arm pending (marker.rs:28-31) |
| 3 | Temperature-tiered Strata (I/D/R/A) | PARTIAL | Tier D real + VFS-injected; I/R/A named absences strata/lib.rs:25-28; blocks unsealed/plaintext (store.rs module doc) |
| 4 | GQL + openCypher + FQL | **PARTIAL** (was NOT_STARTED) | bounded MATCH executes differentially vs oracle w/ certificates; no var-length/ORDER BY/DML/temporal-or-branch syntax; LanguageContracts unfrozen (`fgdb-g0-language-contracts-54g`) |
| 5 | FreeJoin/WCO/factorized execution | NOT_STARTED | zero operator enums; W5 stages open (`fgdb-5vp9`, `fgdb-rz12`); migration seam filed |
| 6 | Ripple views/subscriptions | NOT_STARTED | epic `fgdb-epic-w6-65w` open |
| 7 | STRICT determinism + certificates | PARTIAL+ | `GqlPlanCertificate` hashes plan+snapshot (gql_cert.rs); byte-replay still absent |
| 8 | Agent-native B6 | NOT_STARTED | engine single-branch constants; reference models forks |
| 9 | Server `fgdbd` (FGP/Bolt/…) | NOT_STARTED | no crate/binary |
| 10 | CLI robot mode | NOT_STARTED | no crate/binary |
| 11 | Python wheels | NOT_STARTED | no package tree |
| 12 | Install script/releases | NOT_STARTED | claims-lint tripwire holding |
| 13 | SSI transactions | **PARTIAL** (was STUB-in-production) | FCW real on commit path; SSI oracle-side (reference ssi.rs); workspaces/session ownership absent |
| 14 | Larger-than-memory operators | NOT DEMONSTRATED | decoded-RAM snapshot; spill beads open; ingest ceiling compounds |
| 15 | §17 empirical gates | UNPROVEN (harness exists) | five shapes w/ in-region correctness; `empirical_gate_activated=false` throughout; no baselines; no CI |
| 16 | FG-INV live checkers | STUB | 20/20 clauses stub; expected_enforced=0 both directions, fail-safe |
| 17 | Lab VFS before first fsync | WORKING-for-what-exists | BlockStore VFS-injected store.rs:534; Chronicle VFS-generic |
| 18 | Closed dependency universe | WORKING | unchanged |
| 19 | `unsafe_code="forbid"` + ledger | WORKING | 3 islands/7 sites bijective + public-API checker |
| 20 | G0 constitutional freeze | PARTIAL+ | machinery self-testing; **1/175** contracts live (`command_contracts.toml:2896-2922`) |

Vision delivery: still 2 of 20 fully working (18, 19). Goals 4 and 13 became
genuine PARTIALs; 15 gained a harness; 17 holds for everything durable that
exists. Everything a README reader would type remains NOT_STARTED.

### Inventory

| Measure | 08-18 | 08-20 | 08-22 |
|---|---|---|---|
| HEAD | `8d295653` | `df733010` | `8dceb212` |
| Commits between audits | 23 | ~8 | **563** (+52,547/−724, 368 files) |
| Topology slots | 70: 19 active | unchanged | 70: 21 active (cardinalities :60-76); 22 Cargo members |
| Binaries | 5 checker tools | unchanged | 6 (+`fgdb-bench`); `fgdb`/`fgdbd` still absent |
| Cargo [[bench]] targets | 0 | 0 | 0 (harness ships as a bin) |
| Command contracts | metadata | 176 reserved / 0 live | **175 reserved / 1 live**, epoch 40 |
| Invariant enforcement | 0/0 | 0/0 | 0/0 (honest pinned zero) |
| Checker rows | 57 live / 42 stub | 58 / 43 | 99 rows: 57 live + 42 stub (fg_inv_01..20 pairs enumerated) |
| Proof lanes checked | 1/10 | 1/10 | 1/10; `formal/tla` absent; lane gate hard-fails on zero lanes |
| Tracker | 759 / 462 closed / 0 ready | 766 / 466 / 3 ready | 879 / 582 / **2 ready**, 286 blocked |
| Engine `todo!()` | 0 | 0 | 0 |

### Bridge plan updates (order = vision impact)

**NEW Gap 0 — sustained ingest must stop fencing.** Land `fgdb-a7sz`'s fix and
a seal-at-size / partition-root growth policy so long-running commits never
hit the publish fence. Until then, the bench's `ENGINE_LIMIT` events are the
honest headline number, and no throughput claim can be activated. Everything
else below assumes this converges.

**Gap 1 — G0 command universe:** the pipeline is proven end to end (one row →
union arm → handler inventory → bijection tests). The remaining 174 rows are
mechanical-but-large; `fgdb-5uw2` owns; do not fork.

**Gap 2 — transactions:** next slice is session/workspace ownership, SSI at
the validator seam, purpose-narrowed `TxnCx`. Downgraded XL → L/M: the
highest-unknown (validator wiring under crash) is done and covered.

**Gap 3 — minimum query path:** criterion amended — keep the differential
slice AND add the BoundPlan→GLA lowering so the executor becomes Loom's seed
rather than a parallel planner. Filed as
`fgdb-boundplan-gla-lowering-seam-r2kd`, sequenced behind `fgdb-5vp9` so
operator contracts and migration share one design pass, and wired as a
prerequisite of `fgdb-gate-genesis-lce`.

**Gap 4 — bounded recovery / larger-than-memory:** VFS injection done.
Remaining: Tier R seal path, bounded-open suffix accounting, and the first
larger-than-memory demonstration (spill-backed operator over sealed objects).

**Gap 5 — invariant promotion:** unchanged discipline — promote
FG-INV-03/08/09/18 only when checker + distinct negative test go live in the
same change. Fail-closed behavior verified this audit
(`g0_proof_lanes_e2e` fails on zero checked lanes or missing artifacts).

**Gap 6 — product surface:** unchanged except CI, now owned by
`fgdb-ci-workflow-check-sh-4csa` (dependency-free, immediately ready — the
ready queue needed an actionable item).

**Gap 7 — later layers (Ripple/Beacon/Prism/Warden/Fabric/Aegis):** unchanged
warning; doing them before Gaps 0–3 converge produces islands.

### Evidence boundary

Pinned to tracked commit `8dceb212938226235744b8e3e65cc68ed1d386fa`
("feat(bench): fence telemetry …"), measured 2026-08-23T02:30–03:00Z. Method:
four parallel read-only audits (query pipeline; chronicle/strata/spine;
registries+gates; topology/sim/bench/surface) cross-checked against three
independently re-read load-bearing sites (FCW install lib.rs:2130; the live
contract row command_contracts.toml:2896-2922; bench fence comments
main.rs:31-58). Behavioral: fresh `rch` run of the spine example exited 0
with agreement output. The same three untracked foreign artifacts as prior
audits (`.beads/beads.db-wal-cert*`, `tools/registry-check/src/claims.rs`)
were observed untouched and were neither edited nor removed, per repo rule.

### Ambition rounds applied to this revision

- **Round 1** refused to let the three upgrades read as victory: elevated the
  ingest fence to Gap 0, amended Gap 3's success criterion to require algebra
  migration (a better-built island is still an island), and surfaced CI as a
  claims-honesty gap rather than infrastructure nicety.
- **Round 2** swept for second-order seams: found the crashpack `target/`
  hazard and the four stale honesty surfaces, folding them into single beads
  instead of scattering; reordered the bridge so nothing above Gap 0 may be
  called "next".
- **Round 3** searched for further structural additions and found none — the
  epics already own every remaining README surface. Rounds converged.

### Refinement passes on the new beads

- **Pass 1** rewrote acceptance criteria to be observable (exit codes, file
  existence, grep-anchored doc fixes), wired the GLA-seam dependency chain,
  left the CI bead free so readiness improves today.
- **Pass 2** added negative-test requirements (CI red-run demonstration;
  guard test asserting the artifact root never moves back under `target/`)
  and bet labels beside workstream tags per the ADR provenance law.
- **Pass 3** found nothing further — stopping per the skill's convergence
  rule.

### Beads filed from this measurement (only uncovered seams)

| ID | Seam |
|---|---|
| `fgdb-ci-workflow-check-sh-4csa` | GitHub Actions workflow running `scripts/check.sh`; converts aspirational "CI-enforced" claims into enforceable ones |
| `fgdb-boundplan-gla-lowering-seam-r2kd` | Migrate `BoundPlan`/`gql_exec` onto the GLA operator family; prerequisite edge into Genesis; depends on `fgdb-5vp9` |
| `fgdb-stale-honesty-surfaces-sync-npas` | Four stale honesty surfaces: WriteBatch FCW wording, sim `database_id` comment, topology baseline wording, CHANGELOG wave 9 |
| `fgdb-crashpack-artifacts-durable-home-vd35` | Move sim crashpack/regression artifacts out of cleanable `target/` with a guard test |

**Same-day pickup (postscript, 2026-08-23).** Within hours of filing, the
swarm claimed `fgdb-ci-workflow-check-sh-4csa` and landed the workflow
(`076552b2`: push(main)+PR trigger; pinned nightly installed from
`rust-toolchain.toml`; caches keyed on `Cargo.lock` with the documented
invariant that cache behavior can never skip a gate; the job's verdict is
exactly `scripts/check.sh`'s exit code), with the red-proof demonstration
(`crates/fgdb/tests/ci_red_proof.rs`) in flight at measurement close.
`fgdb-p95p` published its first honest numbers in the same window
(`ae579dcb`: point-reads p50=122us / p99=152us under skew; cold-reopen
p50=40.5ms; compaction-under-load 104ms over 130 traversals — machine-local,
unpinned). The seam-filing discipline is validated: file it ready, and the
fleet eats it. This postscript records tree state through HEAD `076552b2`;
the audit body above remains pinned at `8dceb212`.

### Tracker hygiene

- `fgdb-p95p` remains in_progress after `f2fb2a45` landed the harness —
  legitimate remainder (baselines/gating story). Close on the gating
  increment, not the commit message.
- `fgdb-fcw-writebatch-6cxf` was correctly post-review closed.
- Do not create parallel epics for CI, GLA migration, or doc hygiene: they
  are leaves under existing owners (verification/G0, W5, doc-sync lineage).

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
