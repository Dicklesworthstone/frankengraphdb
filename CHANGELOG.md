# Changelog

This file records landed, executable, or mechanically enforced capability on unreleased `main`. Reserved registry rows, plans, and unchecked acceptance tests are not treated as shipped behavior. FrankenGraphDB has not reached the planned 1.0 product surface.

## Unreleased — 2026-09-02 strict query-evidence envelopes and replay audit

Representative commits: `97d09787`, `57b13803`, `46175c92`, `68111462`, `62a546c7` under `fgdb-gate-genesis-lce.2`.

### Canonical application envelopes

- Added `GqlPreparedResultArtifact` for exact durable prepared-query results.
- Added `GqlOverlayResultArtifact` for exact staged-overlay results.
- Added one explicit v1 framing with eight-byte `FGQEVID1` magic, major/minor version, a closed kind tag, zero-required reserved bytes, exact sequence or basis, input/plan identities, staged-effect identity where applicable, exact row count, ordered `VId` bytes, and the applicable result digest.
- Artifact fields are private; ordinary `Debug` output exposes public metadata and digests while redacting rows.
- The strict decoder rejects invalid magic, unsupported versions, wrong artifact kind, nonzero reserved bytes, row-count or length overflow, every truncated prefix, trailing bytes, and a result transcript inconsistent with the included plan/snapshot or staged-overlay identity.
- Digest comparisons use constant work over all digest bytes.

The framing is endian-stable and deterministic but remains an **unreleased application artifact**. It is not an Appendix-A Chronicle object, FGP frame, publisher signature, external-verifier SDK, or compatibility promise.

### Product issuance and audit

- Added `Database::execute_prepared_query_artifact[_at]` and `audit_prepared_query_artifact`.
- Added the same issuance and audit surface on immutable `EmbeddedReadView`.
- Durable audit verifies strict framing, exact retained statement/bind identity, the canonical plan certificate at the artifact sequence, and the product certificate's ordered-result digest before re-executing the query at that exact historical sequence and requiring ordered-row equality.
- The artifact decoder independently recomputes the frozen result transcript; product audit cross-checks it against `GqlPlanCertificate`, preventing silent drift between the application decoder and canonical issuing authority.
- An artifact remains auditable after later writes advance the live frontier because replay uses the sequence named by the artifact rather than current state.
- Added `WriteTxn::execute_prepared_query_overlay_artifact` and `audit_prepared_query_overlay_artifact`.
- Transaction audit additionally verifies the current basis and canonical staged-effect digest. A later staged mutation returns typed `StagedEffectMismatch` for the old artifact before overlay replay.

### Mutation-sensitive laws and witness

The new integration law covers:

- database and immutable-view audit of the same artifact;
- historical replay after the live frontier advances;
- rejection of a different prepared input;
- row-byte corruption and trailing-data refusal;
- staged-overlay round trip and exact replay;
- staged row corruption refusal;
- invalidation after a later canonical staged effect.

A runnable witness was added:

```bash
cargo run -p fgdb --example gql_evidence_artifact
```

It issues durable and staged envelopes, audits both, advances durable and staged state, and demonstrates exact historical replay plus staged-effect invalidation.

### Exact boundary

The durable envelope carries enough information to bind and replay one result against a database that retains the named sequence. The staged envelope remains an identity-and-row package, not standalone transaction replay: it omits the durable snapshot, staged template bytes, read-set state, and conflict state.

Promoting this internal v1 envelope into a released format requires a deliberate registry/constitution decision, stable size ceilings, frozen golden vectors, compatibility rules, and a separate authenticity layer where publisher provenance matters.

### Validation boundary

Hosted GitHub Actions were not used as evidence. The connector environment did not provide the repository-pinned Rust toolchain, `shellcheck`, or a runnable UBS installation. The changed surface received focused mechanical checks for exact blob identity, source/module ownership, include closure, delimiter balance, whitespace and line width, private-field construction, strict length and reserved-byte checks, every-prefix truncation coverage, diagnostic redaction, constant-work digest comparison, canonical issuer/independent-decoder cross-checking, and mutation-sensitive integration coverage.

Checked-in tests state intended laws. This entry does not claim a fresh rustfmt, compile, Clippy, Rust-test, shellcheck, UBS, or complete `scripts/check.sh` verdict for the current tree. The next proof-bearing step is an exact-tree run captured by `scripts/local_proof.sh` on a machine with the pinned toolchain.

## Unreleased — 2026-09-01 owned preparation, deterministic bounds, and staged results

### Coherent owned prepared queries

Representative commits: `c7a0558e`, `61970273`, `3f19227b`, `3f85fa61` under `fgdb-w10-embedded-54r.1`.

- Added `PreparedGqlQuery`, owning exact statement bytes, a cloned canonical `RelationBind`, and the derived `BoundPlan` behind private fields.
- Moved the existing parser/binder byte-for-byte behind `parser.rs`; no second parser or binder was introduced.
- Added redacted diagnostics and an explicit preparation-coherence audit.
- Added owned preparation and execution on `Database`, `EmbeddedReadView`, and `WriteTxn`.
- Added live and exact historical execution without reparsing or rebinding.
- Added aligned input, plan, and exact ordered-result evidence for durable database and immutable-view reads.
- Added a cross-surface law proving caller mutation after preparation cannot alter the retained query, durable/view evidence agrees at one sequence, and transactions see staged read-your-own-writes state.

This is not yet the final parameterized prepared-statement protocol. Typed parameters, catalog epochs, authorization, cursor lifecycle, physical planning, invalidation, and released persistence remain open.

### Deterministic prepared-query budgets

Representative commits: `6c40d30c`, `4619dd65`, `0b06da07`, `00523439`, `78137cfb`.

- Added `GqlExecutionBudget`, typed dimensions and refusals, exact execution stats, and a generic execution-versus-budget error split.
- `SnapshotRecords` counts the complete immutable vertex table admitted by a node scan or edge table admitted by an edge pattern.
- `ResultRows` counts final rows after predicates, projection, sorting, deduplication, `SKIP`, and `LIMIT`.
- Exact boundaries succeed; observations above a limit return the dimension, configured limit, and observed count.
- Added budgeted execution for live/historical database reads, immutable views, and staged transactions.
- No partial rows escape a budget refusal.
- A transaction admission refusal conservatively records read dependencies because counting the overlay is itself a transaction read.
- Added edge-pattern, node-scan, durable/view parity, and staged-transaction exact/one-below laws.

The current implementation counts a materialized table before ordinary execution reads it. These are deterministic downstream-work/result guards, not wall-clock cancellation, allocation or I/O preemption, memory/spill governance, backpressure, or physical runtime-cost evidence.

### Exact staged-overlay result evidence

Owning workstreams: `fgdb-w4-g1-txn-core-qpmg.4` and `fgdb-gate-genesis-lce.2`.

- Added a canonical staged-effect digest under `fgdb:write-txn-staged-effect:v1`.
- The staged-effect transcript binds the durable transaction basis and either an explicit empty-overlay tag or the complete canonical `LogicalDeltaTemplate` retained by the prepared write.
- Added `GqlOverlayResultCertificate` under `fgdb:gql-staged-overlay-result:v1`.
- The result certificate binds basis, plan digest, staged-effect digest, exact row count, row order, and every returned `VId`.
- Added `WriteTxn::execute_prepared_query_with_overlay_result_certificate` and `verifies_prepared_query_overlay_result`.
- Evidence is minted only after successful overlay execution.
- Equivalent transactions at the same basis with the same canonical semantic effect verify the same evidence.
- Row reorder, replacement, truncation, a later staged mutation, or a different overlay invalidates the certificate.
- Digest comparisons use constant work over all digest bytes.

This closes the exact in-process staged-result evidence gap. It does not create standalone replay: the certificate does not carry the durable snapshot, staged template bytes, graph rows, or transaction conflict state.

### Runnable witnesses

```bash
cargo run -p fgdb --example gql_owned_prepared
cargo run -p fgdb --example gql_txn_overlay_result_evidence
```

The first demonstrates retained preparation, deterministic counters, typed refusal, and aligned durable-read evidence. The second demonstrates exact staged-result certification and invalidation after a later staged effect.

### Immediate crate-root repair

Commit `0c1be1b3` restored the exact known-good `crates/fgdb/src/lib.rs` blob after a contents-API write replaced that file instead of patching a re-export list. The final tree retains the prior crate root. Prepared-query types remain owned by `fgdb-gql`; embedded methods use those types directly.

### Documentation synchronized

- Rebuilt `IMPLEMENTATION_STATUS.md` around the actual owned-preparation, budget, and staged-result subset.
- Updated `docs/GQL_RESULT_EVIDENCE.md` to separate durable-read evidence, staged-result evidence, and adjacent deterministic counters.
- Updated `docs/TRANSACTION_GQL.md` with canonical staged-effect identity, exact in-process result evidence, and the replay boundary.

## Earlier 2026-09-01 continuation

### Prepared transaction-overlay GQL

Commit `005a8397` made `WriteTxn::execute_prepared_gql` the plan-only overlay body, changed text/certified execution to bind once and delegate, added plan-only certification, preserved deterministic read-your-own-writes and FCW dependency tracking, and decomposed the large transaction module into responsibility-focused include units.

### Exact-SHA local context and proof packages

- Context format v2 binds an exact commit/tree, recomputes source/history from an imported bundle, rejects source substitution, and can materialize a verified detached checkout.
- Proof format v2 binds the exact commit, tree, and tracked `scripts/check.sh` blob while preserving stable pass, stable red, and moving-tree void as distinct verdicts.

### Query evidence and historical prepared execution

Representative commits: `01f28d39` through `65d3d832`.

- Unified live, historical, and immutable-view execution on one exact-sequence kernel.
- Added pinned embedded read sessions and reusable `BoundPlan` execution.
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
- typed rows/columns, genuine streaming cursors, and backpressure;
- full session ownership, authorization, renewal/expiry, and synchronous facade;
- full SSI and predicate/range conflict tracking;
- standalone staged-overlay replay payloads and verifier;
- a registered query-evidence compatibility contract and external verifier SDK;
- full ISO GQL, GLA/Loom execution, optimizer, spill, and larger-than-memory queries;
- Strata tiers I/R/A, Ripple, Beacon, Prism, Warden, Fabric, and Aegis;
- CLI/robot mode, server, Python bindings, installer, signed releases, and upgrades.

## Guidance for future entries

- Record only executable or mechanically enforced behavior.
- Name the exact subset and refusal/no-claim boundary.
- Separate checked-in tests from executed proof.
- Preserve red and void evidence; never summarize it as green.
- Keep Git, live Beads state, derived indexes, context capsules, and proof bundles as separate authorities.
