# Changelog

This file records landed, executable, or mechanically enforced capability on unreleased `main`. Reserved registry rows, architectural plans, and unchecked acceptance tests are not treated as shipped behavior. FrankenGraphDB has not reached the planned 1.0 product surface.

## Unreleased — 2026-09-04 five live invariant clauses

Owning bead: `fgdb-owrc` (P1), after `fgdb-j6aq` and `fgdb-1sto`.

- **`FG-INV-04.pinned-snapshot-visibility`** (owner `fgdb`, G1) binds the system-time half of MVCC visibility in both directions: `the_pinned_seq_answer_is_unmoved_by_later_commits` (a commit after the captured frontier widens the live answer to `[2, 5]` and must leave the pinned answer at `[2]`, with the pinned call made from the same handle before and after the live scan so a cached answer cannot fake it) and `execute_gql_at_live_frontier_equals_execute_gql` (the paired control against a pinning path that passes by hiding legitimate effects). The two symbols live in different artifacts, which the clause law allows — distinct *symbols* is the rule.
- **`FG-INV-19.replay-grade-monotonicity`** (owner `fgdb-sim`, G3) binds `a_diverging_replay_is_downgraded_to_structural` with `a_faithful_replay_can_reach_the_top_grade` as its negative — the test the suite itself labels "THE CONTROL … this one proves the grader can return it, so those assertions mean something", which is exactly what a `negative_test_entrypoint` is for.
- `expected_enforced_clauses` 3 → 5, across five different invariant IDs. `expected_enforced_invariants` stays **0**: every `.core` is still stub.

Both are narrowings and the registry says so above each row. FG-INV-04's statement also covers branch ancestry, valid-time slice sets, create/retire and schema-coordinate rules, workspace overlays, erasure tombstones and the expiry predicate; FG-INV-19's covers the evidence-slot matrix and the lease/trust/authority weakening rules. None of that is bound.

### Verification

`registry-check all` 0 violations; `--test claims` 38/38 with the control re-derived to the exact five-key set; `fgdb --test gql_exec_at` 2/2, `--test gql_exec_at_equals_live` 1/1; `fgdb-sim --test sim_completeness` 9/9; `g0_claims_e2e` and `g0_spine_e2e` ALL GREEN. **Six mutation controls**: renaming any of the four bound symbols fires `checker_symbol_unresolved` + `clause_promoted_without_live_checker` + `enforcement_coverage_drift`; under-claiming the ledger (3 against 5) and over-claiming enforced IDs (2 against 0) each fire drift; the final control returns to the noise floor.

## Unreleased — 2026-09-04 two more live invariant clauses

Owning bead: `fgdb-j6aq` (P1), following `fgdb-1sto`.

- **`FG-INV-09.four-layer-identity-recomputation`** (owner `fgdb-chronicle`, G1) binds `crates/fgdb-chronicle/tests/identity_pipeline.rs`: `every_identity_recomputes_from_its_inputs` runs `IdentifiedObject → protect → encode → place` twice and requires `ObjectId`, `CiphertextId`, `EncodingId` and `PlacementId` to be byte-identical; `dedup_does_not_cross_namespaces_or_keys` changes the security namespace and then `K_oid` and requires `ObjectId` to move and deduplication to be refused.
- **`FG-INV-05.first-committer-wins`** (owner `fgdb`, G1) binds `crates/fgdb/tests/first_committer_wins.rs`: `overlapping_prepared_batches_abort_the_second_committer` is the product write-path witness that commit validation is not a pass-through, and `overlapping_abort_is_attributable_to_fcw_not_the_fold` requires the loser's abort to name `FG-LAW-FCW-01` rather than a fold arm or the generic commit wrap, so the checker cannot pass for the wrong reason.
- `expected_enforced_clauses` 1 → 3. `expected_enforced_invariants` stays **0**: every `.core` clause is still stub, and an ID counts only when all of its clauses are enforced.

**Both are narrowings, and the registry says so in place.** FG-INV-09's Appendix F sentence says identities recompute from "the exact keyed, namespaced canonical logical transcript"; nothing here pins that transcript against a golden vector, and `idr_golden_corpus_replay` and `idr_blake3_identity_recompute` are still stub rows. FG-INV-05's statement is full serializability — an acyclic dependency graph with real-time precedence edges; first-committer-wins is a conservative write-write mechanism that builds no such graph, says nothing about read-write anomalies, and can reject histories serializability would admit.

### Verification

`registry-check all` 0 violations; `-p registry-check --test claims` 38/38 with the control re-derived to the exact three-key set; `fgdb-chronicle --test identity_pipeline` 14/14; `fgdb --test first_committer_wins` 5/5; `g0_claims_e2e` and `g0_spine_e2e` ALL GREEN. **Six mutation controls** (scratch copy, constant 2-violation noise floor, clean final control): renaming either bound test of either clause fires `checker_symbol_unresolved` + `clause_promoted_without_live_checker` + `enforcement_coverage_drift` (four arms, all fired); declaring one enforced clause while three are fires drift; claiming one fully enforced invariant ID while none is fires drift.

## Unreleased — 2026-09-03 the invariant spine enforces something; GitHub Actions retired

Owning beads: `fgdb-1sto` (P1), `fgdb-ci-workflow-check-sh-4csa.2` (closed premise-void).

### What was wrong

- `registries/invariants.toml` held twenty invariant IDs and twenty clauses, **every one `stub`**, and every `checker_entrypoint` resolved to a `checker_index.toml` row that was itself `stub` and named `crates/fgdb-oracles/`, a crate that does not exist in the workspace. AGENTS.md's *Spec-First* item 2 ("CI cross-checks that every ID has a live checker") and the hard rule under it ("no subsystem ships against an unenforced invariant; a workstream exit gate G1–G4 cannot pass while any invariant it depends on lacks a live checker") therefore quantified over an **empty set** and passed. The declared ledger said so honestly — `expected_enforced_clauses = 0` — but nothing in the tree had ever been bound.
- `AGENTS.md` claimed "CI exists: `.github/workflows/check.yml` runs `scripts/check.sh` verbatim on every push to `main` and every pull request, so 'CI-enforced' means enforced". No hosted job for this repository had started since 2026-09-01T04:13Z.

### What changed

- **The first live clause.** `FG-INV-12.canonical-scalar-coherence` binds the one sentence of FG-INV-12 whose apparatus already runs on every `cargo test` — "Canonical scalar equality, hashing, ordering, and encoding are coherent" — to two named integration tests in `crates/fgdb-types/tests/canonical_value_laws.rs`: `canonical_scalar_byte_order_equals_value_order` as the checker and `non_canonical_float_bits_are_rejected_rather_than_repaired` as the negative. Both are new `kind = "cargo-test"`, `status = "live"`, `unit = "symbol"` rows in `registries/checker_index.toml`, so renaming or deleting either test turns the registry red. `expected_enforced_clauses` rose 0 → 1 in the same change, which the registry header defines as a G-gate event.
- **`FG-INV-12.core` stays `stub`** and keeps the whole Appendix F statement; the rest of it (answer-equivalent rewrites, workspace materialization, safe replan, `AnswerContract`) has no apparatus. An ID counts as enforced only when every clause under it does, so `expected_enforced_invariants` stays 0: **one enforced clause, zero enforced IDs.**
- `tools/registry-check/tests/claims.rs`: `claims_enforcement_ledger_control` was **re-derived, not re-pinned**, as its own message demanded. It now asserts the measured enforced set is exactly `["FG-INV-12.canonical-scalar-coherence"]` through the same readers the ledger uses, that the declaration equals its length, and that no ID is enforced. The drift mutant moved from a hard-coded `1` to `measured + 1` and gained its opposite direction (declare 0 against a measured 1), a mutant that could not be written while the base was empty.
- **GitHub Actions is retired here** (owner ruling: this project does not use it for any reason; releases go through the `dsr` self-releaser). Both workflows are `workflow_dispatch` only — the recipe is kept and runnable by hand, the automatic `push`/`pull_request`/`schedule`/`workflow_run` triggers are gone. `AGENTS.md` now names `scripts/local_proof.sh` on the exact committed tree as the verdict of record, and `scripts/check.sh`'s file-coverage exemption for workflow files no longer rests on "a red workflow surfaces as a failed run on GitHub".

### Verification

`registry-check all --root .` 0 violations (`spine_clauses` 21); `cargo test -p registry-check` all suites green, `--test claims` 38/38; `fgdb-types --test canonical_value_laws` 28/28; `scripts/g0_claims_e2e.sh` 17/17, `scripts/g0_spine_e2e.sh` 15/15, `scripts/g0_negative_evidence_e2e.sh` 10/10, all ALL GREEN. **Mutation controls** (scratch copy; the copy's own 2-violation noise floor is constant across every arm): renaming the bound test → `checker_symbol_unresolved` + `clause_promoted_without_live_checker` + `enforcement_coverage_drift`; flipping the checker row to `stub` → promotion + drift; pointing the artifact at a file `cargo test` never compiles → `checker_not_invocable` ×2 + promotion ×2 + drift; restating the declaration as 0 → drift. Reverting every mutation returns to the noise floor.

### Not claimed

Nineteen invariants remain wholly `stub`, and fourteen of them name owning subsystems (`fgdb-txn`, `fgdb-ecs`, `fgdb-branch`, `fgdb-ripple`, `fgdb-observatory`, `fgdb-secure-view`) that do not exist. No workstream exit gate passes because of this change. The clause claims coherence over the shipped canonical-scalar corpus and its boundary values, and nothing outside it.

## Unreleased — 2026-09-02 verdict restored

Owning beads: `fgdb-l9r3` (P0), `fgdb-ci-workflow-check-sh-4csa.2`, `fgdb-baru`. Representative commits: `b51e3232` (the repair) and the commit that lands this entry.

### What was wrong

- `main` had no green hosted verdict for 141 commits after `f8bf9b40`: of the last 200 `check.yml` runs, 166 were cancelled by the next push, 32 failed, 1 succeeded.
- `crates/fgdb-gql` did not compile from `97d09787`: the evidence envelope decoded a `VId` as 8 bytes while `VId` is a `u128` and the encoder writes 16. The envelope had never round-tripped.
- `Cargo.lock` was stale (two `fgdb-gql` path dependencies added without regeneration), so every `--locked` build refused.
- `crates/fgdb` carried seven `E0308` errors (`None => hasher.update(&[0])` arms), one clippy `large_enum_variant`, 32 unformatted files, and a test that included a source module by path to reach a private function.
- Seven shell scripts and five documents landed on 09-01/02 with no checker, disposition, or coverage inspector; the claims gate, the file-coverage closure, and shell lint were red.
- 71 commits cited bead ids that did not exist, and nothing in the chain read commit messages.

### What changed

- `VId` rows are an explicit 16-byte v1 format decision (`ROW_LEN`), pinned by a full-width round-trip law and a canonical-length law in `fgdb-gql`.
- `Cargo.lock` regenerated. `scripts/check.sh` passes `--locked` to cargo check, clippy, and every cargo test, so a stale lock reds the gate with cargo's own message instead of being rewritten silently (negative control measured on a scratch copy).
- The seven scripts have `[[script_disposition]]` rows (both self-tests executed here, exit 0, recorded as measured candidates) and the five documents have coverage inspectors.
- The fabricated ids `fgdb-w10-embedded-54r.1`, `fgdb-gate-genesis-lce.2`, and `fgdb-w4-g1-txn-core-qpmg.24` now exist as retroactive records stating exactly what those commits delivered and where they stop; `fgdb-3w75` (unreproducible) is recorded inside `fgdb-gate-genesis-lce.2` and adjudicated in the gate below.
- New registered gate `scripts/g0_commit_provenance_e2e.sh`: every bracketed bead id in the last seven days of commits must resolve (tracked export, then local database, then the adjudication table), with a 24-hour export grace on hosts without a database and a negative control inside the gate.
- `.github/workflows/check.yml`: scheduled (every two hours) and manual runs get a concurrency group of their own, so the push cadence cannot cancel every verdict.
- `IMPLEMENTATION_STATUS.md` no longer carries a "validation boundary" paragraph; NE-0045 records that class.

### Verification

Piecemeal on `b51e3232`: `cargo check`/`clippy --locked -D warnings` rc 0, `fgdb-gql` 72/72, eight `fgdb` evidence suites green, six examples rc 0, `cargo fmt --all --check` rc 0, `g0_claims_e2e.sh` 17/17, the provenance gate 107/107 with 9 adjudicated. The exact-tree `scripts/check.sh` verdict for any tree is established by `scripts/local_proof.sh` on that tree and by the scheduled hosted run; it is deliberately not quoted here, because this file is part of the tree it would describe.

### Exact boundary

This restores the gate. It does not widen the GQL grammar, change the engine, or promote any invariant. The evidence envelope remains an unreleased application artifact.

## Unreleased — 2026-09-02 resource-safe evidence paging

Owning workstream: `fgdb-w10-embedded-54r.1`.

Representative commits: `901e4ef5`, `bdff21d9`, `43e1b155`, `61adb1fd`, `bede52c5`, `a277f0ae`, `3ed7ec32`, `49adac5f`.

### Result-bound continuation tokens

- Added `GqlEvidencePageToken`, a fixed-width v1 token using magic `FGQPAGE1`.
- The token binds artifact kind, exact snapshot sequence or transaction basis, the complete ordered-result digest, and the next row offset.
- Added strict decoding for exact length, magic, major/minor version, closed kind, zero-required reserved bytes, and checksum.
- Added every-prefix truncation tests, trailing-byte refusal, checksum mutation controls, and cross-kind/snapshot/result rejection.
- The checksum comparison uses constant work over all digest bytes.
- The checksum is unkeyed and is named accordingly: it is not a MAC, signature, authorization decision, capability, or publisher-authenticity proof.

### Deterministic materialized pages

- Added `GqlEvidencePage` over both durable and staged evidence artifacts.
- Pages expose exact `start_offset`, `end_offset`, `total_rows`, `remaining_rows`, terminal status, and an optional next token.
- Rows remain redacted from ordinary `Debug` output.
- Page size may change between calls; it is caller policy rather than token identity.
- Exact end offsets produce an empty terminal page; offsets beyond the result refuse with a typed error.
- Page size zero refuses explicitly.

### Resource-safe product audit-and-page adapters

Added default-untrusted and caller-limited page APIs on:

- `Database`;
- `EmbeddedReadView`;
- `WriteTxn`.

The product order is deliberate:

1. reject zero page size;
2. strictly decode and checksum-check optional token bytes;
3. enforce artifact byte and declared-row limits before row allocation;
4. strictly decode and verify the artifact;
5. verify input, plan, snapshot/basis, and staged effect where applicable;
6. re-execute the exact historical or staged query and compare ordered rows;
7. bind the token to the reproduced result;
8. return one contiguous slice.

This avoids expensive replay for an intrinsically invalid page request without allowing a valid token to bypass artifact admission or exact replay.

### Cross-surface laws and witness

- Added a durable integration law spanning `Database` and immutable `EmbeddedReadView`.
- Proved that a token resumes the same historical result after the live frontier advances.
- Proved that a token cannot resume a newer snapshot, another artifact kind, or a different ordered result.
- Proved nested resource-limit refusal and request-preflight precedence.
- Added staged-overlay paging and proved that a later staged effect refuses during audit before a page is returned.

Runnable witness:

```bash
cargo run -p fgdb --example gql_evidence_pages
```

### Exact boundary

This is stateless pagination over an already materialized exact evidence artifact. Every product-level page call re-admits, decodes, verifies, and replays the complete artifact. It is not a database cursor, streaming operator, bounded-buffer flow-control mechanism, lease, backpressure protocol, authentication token, or lower-cost replay path.

A genuine cursor still requires explicit session/owner identity, cancellation and lease semantics, bounded buffering, backpressure, and an operator/storage path that can stop before materializing the full result.

### Validation boundary

The connector environment used for this continuation did not contain the pinned Rust toolchain, `shellcheck`, or a runnable UBS installation. Hosted GitHub Actions were excluded from evidence.

The changed surface received focused checks for exact Git blob identity, fast-forward history, module/include closure, delimiter balance, whitespace and line width, fixed-width token accounting, checked offset arithmetic, reserved/trailing-byte handling, every-prefix truncation laws, diagnostic redaction, checksum mutation, cross-kind/snapshot/result binding, resource-limit nesting, and request-preflight order.

Checked-in tests state intended laws. This changelog does not claim a fresh `cargo fmt`, compile, Clippy, Rust-test, shellcheck, UBS, or complete `scripts/check.sh` verdict for the final tree.

## Unreleased — 2026-09-02 resource-bounded evidence ingestion

Representative commits: `a3441930`, `caf11153`, `ce6a38e9`, `f004bd1c` under `fgdb-gate-genesis-lce.2`.

- Added configurable total-byte and declared-row ceilings for untrusted prepared-result and staged-overlay envelopes.
- Added preflight screening before the decoder allocates row vectors.
- Preserved strict decoder error classes for malformed headers.
- Added exact and one-below encoding and decoding laws.
- Added resource-aware product audit adapters for database, immutable-view, and staged transaction surfaces.
- Added a runnable `gql_evidence_limits` witness.

The default limits are application policy, not a format maximum or product SLO.

## Unreleased — 2026-09-02 strict evidence envelopes and replay audit

Representative commits: `97d09787`, `57b13803`, `46175c92`, `68111462`, `62a546c7`, `9cbd554d`, `e54fc251`, `00a52aa7`, `10871100` under `fgdb-gate-genesis-lce.2`.

- Added canonical v1 `GqlPreparedResultArtifact` and `GqlOverlayResultArtifact` envelopes.
- Added versioned, kind-tagged, endian-stable, self-delimiting framing with zero-required reserved bytes.
- Added strict refusal for invalid magic, unsupported version, wrong kind, overflow, every truncated prefix, trailing data, and result-transcript mismatch.
- Added durable issuance and exact historical replay audit on `Database` and `EmbeddedReadView`.
- Added staged-overlay issuance and audit on `WriteTxn`.
- Cross-checked the independent artifact decoder against the canonical product certificate before replay acceptance.
- Added mutation-sensitive integration tests and the `gql_evidence_artifact` witness.

The bytes are an unreleased application envelope, not an Appendix-A Chronicle object, FGP frame, signed attestation, or compatibility promise.

## Unreleased — 2026-09-02 exact staged-overlay result evidence

Representative commits: `b33e1d86`, `80de85a6`, `16309187`, `40b9c09e`, `107bb154`, `cfa6ea43`, `f377562a`, `f499858e`, `d5c1c36f`, `9d8a81c5`, `3e8ff789`.

- Added a canonical staged-effect digest binding transaction basis and canonical staged `LogicalDeltaTemplate`, or an explicit empty overlay.
- Added `GqlOverlayResultCertificate` binding basis, plan digest, staged-effect digest, exact row count, row order, and every returned `VId`.
- Added transaction issuance and current-overlay verification APIs.
- Proved equivalent canonical effects verify across transactions while row or staged-effect mutation refuses.
- Added the `gql_txn_overlay_result_evidence` witness.

This closes exact in-process staged-result evidence, not standalone staged replay.

## Unreleased — 2026-09-01 owned preparation and deterministic query bounds

Representative commits: `c7a0558e` through `78137cfb` under `fgdb-w10-embedded-54r.1`.

### Coherent owned preparation

- Added `PreparedGqlQuery`, owning exact statement bytes, a cloned canonical `RelationBind`, and the derived `BoundPlan` behind private fields.
- Kept one parser, binder, and execution kernel.
- Added owned preparation and execution across `Database`, historical reads, immutable views, and staged transactions.
- Added redacted diagnostics and an explicit preparation-coherence audit.
- Added aligned input, plan, and exact ordered-result evidence for durable reads.

### Deterministic query budgets

- Added `GqlExecutionBudget`, typed dimensions and refusals, and exact execution statistics.
- `SnapshotRecords` counts the admitted immutable vertex or edge table.
- `ResultRows` counts final deterministic rows after predicates, projection, sort/deduplication, `SKIP`, and `LIMIT`.
- Exact boundaries succeed; one-below limits refuse without partial rows.
- Added live, historical, immutable-view, and staged-transaction paths.

The current budget counts a materialized table before the normal executor reads it. It is not wall-clock cancellation, memory/spill governance, backpressure, or physical-cost evidence.

Runnable witness:

```bash
cargo run -p fgdb --example gql_owned_prepared
```

## Earlier 2026-09-01 continuation

### Prepared transaction-overlay GQL

Commit `005a8397` made `WriteTxn::execute_prepared_gql` the plan-only overlay body, changed text and certified execution to bind once and delegate, added plan-only certification, preserved deterministic read-your-own-writes and FCW dependency tracking, and decomposed the large transaction module into responsibility-focused include units.

### Exact-SHA local context and proof packages

- Context format v2 binds an exact commit/tree, recomputes source/history from an imported bundle, rejects source substitution, and can materialize a verified detached checkout.
- Proof format v2 binds the exact commit, tree, and tracked `scripts/check.sh` blob while preserving stable pass, stable red, and moving-tree void as distinct verdicts.

### Query evidence and historical prepared execution

Representative commits: `01f28d39` through `65d3d832`.

- Unified live, historical, and immutable-view execution on one exact-sequence kernel.
- Added pinned embedded read views and reusable `BoundPlan` execution.
- Added historical prepared execution and plan certification.
- Versioned the plan transcript to v2 to bind the historical `neq` omission.
- Added aligned input-plus-plan evidence and a domain-separated ordered-result digest.

## Prior implementation waves

### Constitution and foundations — 2026-07-15 to 2026-07-28

- Master plan, claim lattice, invariant spine, identity/evidence registries, proof lanes, workspace topology, dependency universe, and unsafe policy.
- Exact identifiers, bounded codecs, collections, calibration, crypto, Appendix-A catalog, and mutation-sensitive registry checks.

### Chronicle and Strata — 2026-07-28 to 2026-08-09

- Content-addressed identity, RaptorQ symbols, authenticated capsules, dual-slot root recovery, chained markers, and the capsule-first/marker-last two-fsync protocol.
- Canonical tier-D blocks, strict decoder refusal, content-addressed block store, cascade validation, compaction, snapshots, and a real create/open/write/read/reopen database path.

### Reference semantics and deterministic lab — 2026-07-29 to 2026-08-17

- Independent semantic oracle and end-to-end durability differential.
- FaultVfs crash campaigns, fsync lies, tears, ENOSPC, latency, deterministic replay, retained artifacts, and shrinking controls.

### Product FCW and bounded GQL — 2026-08-13 to 2026-08-31

- Production first-committer-wins validation, basis-pinned writes, and bounded transaction overlay.
- Bounded GQL with labels, directions, two hops, integer predicates, deterministic projection, `SKIP`, and `LIMIT`.
- VFS-backed Strata, sustained-ingest ceiling repair, adversarial harness, and repository-authoritative gate infrastructure.

## Current no-claim boundary

Still incomplete or absent:

- typed parameters and canonical parameter evidence;
- typed rows/columns and genuine streaming cursors with backpressure;
- full session ownership, authorization, renewal/expiry, and synchronous facade;
- full SSI and predicate/range conflict tracking;
- standalone staged-overlay replay payloads;
- a registered compatibility-governed evidence format and external verifier SDK;
- storage/operator-level early resource enforcement;
- full ISO GQL, GLA/Loom execution, optimizer, spill, and larger-than-memory queries;
- Strata tiers I/R/A, Ripple, Beacon, Prism, Warden, Fabric, and Aegis;
- CLI/robot mode, server, Python bindings, installer, signed releases, and upgrades.

## Guidance for future entries

- Record only executable or mechanically enforced behavior.
- Name the exact subset and refusal/no-claim boundary.
- Separate checked-in tests from executed proof.
- Preserve red and void evidence; never summarize it as green.
- Keep Git, live Beads state, derived indexes, context capsules, proof bundles, and application artifacts as separate authorities.
