# Changelog

This file records landed, executable, or mechanically enforced capability on unreleased `main`. Reserved registry rows, architectural plans, and unchecked acceptance tests are not treated as shipped behavior. FrankenGraphDB has not reached the planned 1.0 product surface.

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
