# Changelog

This file records **landed, executable, or mechanically enforced capability** on unreleased `main`. It does not treat reserved registry rows, plans, or checked-in acceptance tests as shipped product behavior. Beads IDs are tracker records in `.beads/issues.jsonl`, not GitHub Issues.

FrankenGraphDB has not reached the planned 1.0 surface. The workspace remains an embedded Rust database substrate with a real Chronicle/Strata durability path, a bounded GQL slice, a bounded write transaction, extensive constitutional gates, and deterministic verification infrastructure.

## Unreleased — 2026-09-01 continuation

### Prepared transaction-overlay GQL

Commit: `005a839701d3a9d270c17ab68d5641dc8fa74a48` (`fgdb-w4-g1-txn-core-qpmg.4`).

- Added `WriteTxn::execute_prepared_gql` as the single transaction-overlay MATCH executor over `&BoundPlan`.
- Changed `WriteTxn::execute_gql` to parse/bind exactly once and delegate to the prepared executor.
- Changed `WriteTxn::execute_gql_certified` to bind exactly once instead of rebinding through the text path.
- Added `execute_prepared_gql_certified` and `prepared_gql_plan_certificate`.
- Preserved staged read-your-own-writes behavior, deterministic ordering, `SKIP`, `LIMIT`, result read dependencies, expansion tracking, and FCW conflict detection.
- Extended the undirected certified integration test to compare text, prepared, certified-prepared, repeat execution, and plan-only evidence, including a directed-plan mutation that changes both rows and plan digest.
- Decomposed the former 1,600-line `write_txn.rs` into responsibility-focused files under `write_txn_parts/` while retaining one private module and one `WriteTxn` state authority.

No exact transaction-result replay claim was added. The current plan certificate binds the durable basis and plan, not the staged overlay.

### Exact-SHA local agent context, format v2

Commits: `306e33e2`, `0985ceea`, `e7dd89c5`, `06945a44`.

- `agent_context.sh` now records the exact committed tree and exports one bundle head named `HEAD`.
- The producer captures stable clean/dirty state, deterministic `git archive`, tracked-file inventory, recent history, and truthful Beads evidence modes.
- `agent_context_verify.sh` imports the bundle into an isolated retained repository and recomputes the source archive, tree, tracked paths, and history instead of trusting adjacent checksums.
- Added strict manifest and regular-file inventories, v1 migration verification, dirty-patch applicability checks, and safe untracked-path checks.
- Added `agent_context_checkout.sh`, producing a verified detached checkout with no remote and optional application of the tracked dirty patch.
- Mutation controls reject checksum-invalid capsules, checksum-consistent source substitution, fabricated history, duplicate manifest keys, and undeclared files.

The package remains advisory. It does not run or inherit the product gate and does not authenticate the distributor.

### Exact-tree local proof bundles, format v2

Commits: `2f08dc72`, `4c035342`, `a047c93b`.

- `local_proof.sh` now binds a proof to the exact commit, committed tree, and tracked `scripts/check.sh` blob.
- Stable green, stable red, and moving-tree void remain separate verdicts; red preserves the raw gate exit and void returns wrapper exit `125`.
- `local_proof_verify.sh` enforces exact v1/v2 manifest and file inventory, checksums, source stability, and the full anchored `check.sh` reporting contract.
- Optional `--repository DIR` verification binds the proof commit, tree, and gate-driver blob to an independently supplied Git object database.
- Mutation controls cover v1 compatibility, exit-7 red, moving-tree void, checksum corruption, undeclared files, duplicate keys, false trees, and malformed red reporting.

Successful proof-bundle verification confirms attribution and internal consistency. The recorded `verdict` remains authoritative for what the captured gate established.

### Query evidence and prepared-plan evolution

Representative commits: `01f28d39`, `f4a617c4`, `92e39e59`, `617bd687`, `74dc4fe4`, `051b90ba`, `1175b620`, `65d3d832`.

- Unified live, historical, and immutable-read-view GQL execution on one exact-sequence kernel.
- Added pinned embedded read sessions and reusable `BoundPlan` execution.
- Added historical prepared execution and plan certification.
- Made input and plan certificates externally verifiable with constant-work digest comparison.
- Versioned the plan transcript to v2 to bind `BoundPlan::neq`, explicitly preserving legacy-v1 verification only for migration.
- Added aligned input-plus-plan evidence from one successful execution.
- Added a domain-separated ordered-result digest binding plan certificate, snapshot, exact row count, row order, and every returned `VId`.
- Added text and prepared result-evidence paths across `Database` and `EmbeddedReadView`.

There is not yet an owned prepared definition keeping statement bytes, canonical bind, and plan inseparable. There is also no registered portable query-evidence artifact.

### Documentation synchronized

Representative commits: `da57fe88`, `7b9bf136`, `2d7559e3`, `67b6bf3e`.

- Rewrote `IMPLEMENTATION_STATUS.md` around the actual current subset and explicit no-claim boundaries.
- Updated the agent-context and local-proof contracts for format v2.
- Added `docs/TRANSACTION_GQL.md` as the source-ownership and evidence contract for staged overlay reads.
- Corrected stale documentation that still described transaction prepared execution as absent or the local packages as checksum-only.

### Validation status for this continuation

The context/proof scripts passed local `bash -n` and their retained semantic self-tests before landing; the committed blobs match the tested files.

The transaction refactor passed:

- `git diff --check`;
- exact Git blob-identity checks;
- include-target closure;
- delimiter-aware lexical scanning;
- unique inherent-method ownership checks.

The execution environment used for this continuation did **not** contain the repository-pinned Rust toolchain or `shellcheck`. Therefore this changelog does not claim a fresh rustfmt, compile, Clippy, Rust-test, shellcheck, or full `scripts/check.sh` verdict for the transaction commit. The next proof-bearing step is to run the pinned toolchain locally and preserve the exact-tree result with `scripts/local_proof.sh`.

---

## Prior implementation waves

### 2026-07-15 to 2026-07-21 — Constitution before implementation

Representative anchors: `1cf64cce`, `ee8aa1a5`.

- Master plan, README, AGENTS, and adversarial plan reviews.
- Claim-class lattice, twenty-invariant spine, identity constitution, evidence/SLO registries, checker liveness, negative-evidence identities, proof lanes, and architecture-decision provenance.
- Frozen workspace topology, dependency universe, unsafe policy, and whole-tree gate closure.

### 2026-07-21 to 2026-07-28 — Foundation crates and exact catalogs

Representative anchors: `bfdc44d0`, `364ac8c0`, `6d3a9c99`, `a172dd3b`.

- Exact identifiers and durable vocabulary in `fgdb-types`.
- Canonical bounded codecs, exact integers, sketches, collections, calibration, crypto, and safe wrappers around registered unsafe islands.
- Appendix-A exact catalog, generated projections, source census, command/delta vocabulary, and mutation-sensitive registry checks.

### 2026-07-28 to 2026-08-05 — Chronicle durability

Representative anchors: `f630102`, `4d5eaa19`, `5f63fee`, `9b80da3`.

- Content-addressed object identity and RaptorQ symbolization.
- Authenticated symbol/capsule formats and erasure recovery.
- Dual-slot `manifest.root` recovery.
- Chained commit markers and the two-fsync protocol: capsule durable first, marker durable last.
- VFS-generic commit coordinator, crash instants, uncertain-D2 fencing, and pre-durability commit validation.

### 2026-07-31 to 2026-08-09 — Strata tier D and a runnable database

Representative anchors: `def35d6f`, `41eb61c`, `42b4b0d3`, `897eb029`, `fd5df4f`.

- Canonical tier-D blocks, strict decoder refusal, content-addressed block store, exact cascade validation, compaction, and snapshot semantics.
- Real `fgdb::Database` create/open/write/read/drop/reopen path over Chronicle and Strata.
- Incremental publication replacing O(history) and O(blocks) marginal-write behavior.
- Durable partition manifests and statement-chain version derivation.

### 2026-07-29 to 2026-08-17 — Reference semantics and deterministic lab

Representative anchors: `08bfadf0`, `0949f0ba`, `bac511b`, `9987473`, `8876ea4`.

- Independent executable semantic oracle.
- Durability-versus-semantics differential through the full write path.
- FaultVfs crash matrices, fsync lies, tears, ENOSPC, latency, deterministic replay, dual-run checking, retained failure artifacts, and shrinking controls.

### 2026-08-13 to 2026-08-20 — Recovery and semantic hardening

Representative anchors: `0e8d77a`, `bb2c0c8`, `ee2f8d1`, `64f9e813`, `411078dc`.

- Delete-cascade and pre-seal hardening.
- Secret-free crypto verification events and fail-closed profile handling.
- Cancelled-publication fencing and authoritative reopen/rebuild rules.
- First inhabitable Local semantic-command arm with bidirectional registry/source handler inventory.

### 2026-08-19 to 2026-08-31 — Product FCW, bounded GQL, VFS unification, and operational evidence

Representative anchors: `ea40cd90`, `c23faeda`, `37de0fdd`, `5f8b9180`, `076552b2`, `51b9b66c`.

- Product `FirstCommitterWinsValidator` installed on embedded database constructors.
- Basis-pinned write preparation/commit and bounded `WriteTxn` read-your-own-writes subset.
- Bounded GQL parse→bind→execute path with labels, directions, two hops, integer predicates, deterministic projection, `SKIP`, and `LIMIT`.
- VFS-backed Strata and one-plane fault injection.
- Adversarial §17 harness and sustained-ingest root-ceiling repair.
- Repository-authoritative `scripts/check.sh` wired as the quality contract; later local proof tooling makes hosted CI optional rather than authoritative.

---

## Current no-claim boundary

The following remain incomplete or absent and must not be inferred from the landed subset:

- owned and parameterized prepared statements;
- typed parameters, typed row/column metadata, genuine streaming cursors, and backpressure;
- full session authorization/ownership/renewal/expiry and synchronous facade;
- full SSI and predicate/range conflict tracking;
- staged-overlay identity and exact transaction-result evidence;
- registered portable evidence, signatures, or an external verifier SDK;
- full ISO GQL, GLA lowering, Loom operators, optimizer, spill, and larger-than-memory execution;
- Strata tiers I/R/A, Ripple, Beacon, Prism, Warden, Fabric, and Aegis;
- CLI, robot-mode NDJSON, Python bindings, installer, signed releases, and upgrade tooling.

## Guidance for future entries

- Record only behavior that is executable or mechanically enforced.
- Name the exact subset and its refusal/no-claim boundary.
- Separate checked-in tests from executed proof.
- Preserve raw red and void evidence; never summarize it as green.
- Keep Git, live Beads state, derived indexes, context capsules, and proof bundles as distinct authorities.
