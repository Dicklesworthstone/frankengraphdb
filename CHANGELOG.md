# Changelog

This file records landed, executable, or mechanically enforced capability on unreleased `main`. Reserved registry rows, plans, and unchecked acceptance tests are not treated as shipped behavior. FrankenGraphDB has not reached the planned 1.0 product surface.

## Unreleased — 2026-09-01 owned preparation and deterministic bounds

### Coherent owned prepared queries

Representative commits: `c7a0558e`, `61970273`, `3f19227b`, `3f85fa61` under `fgdb-w10-embedded-54r.1`.

- Added `PreparedGqlQuery`, owning exact statement bytes, a cloned canonical `RelationBind`, and the derived `BoundPlan` behind private fields.
- Moved the existing parser/binder byte-for-byte behind `parser.rs`; no second parser or binder was introduced.
- Added redacted diagnostics and an explicit preparation-coherence audit.
- Added owned preparation and execution on `Database`, `EmbeddedReadView`, and `WriteTxn`.
- Added live and exact historical execution without reparsing or rebinding.
- Added aligned input, plan, and exact ordered-result evidence for durable database and immutable-view reads.
- Preserved the transaction boundary: staged-overlay execution returns plan evidence only because staged mutation identity is not yet part of the certificate.
- Added a cross-surface law proving caller mutation after preparation cannot alter the retained query, durable/view evidence agrees at one sequence, and transactions see staged read-your-own-writes state.

This is not yet the final parameterized prepared-statement protocol. Typed parameters, catalog epochs, authorization, cursor lifecycle, physical planning, invalidation, and portable persistence remain open.

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

### Runnable witness

Commit `d9909a49` added:

```bash
cargo run -p fgdb --example gql_owned_prepared
```

The example demonstrates retained preparation after caller bind mutation, exact counters, a typed zero-record refusal, and aligned input/plan/ordered-result evidence.

### Immediate crate-root repair

Commit `0c1be1b3` restored the exact known-good `crates/fgdb/src/lib.rs` blob after a contents-API write replaced that file instead of patching a re-export list. The final tree retains the prior crate root. Prepared-query types remain owned by `fgdb-gql`; embedded methods use those types directly.

### Documentation synchronized

- Rebuilt `IMPLEMENTATION_STATUS.md` around the actual owned-preparation and budget subset.
- Updated `docs/GQL_RESULT_EVIDENCE.md` to separate cryptographic evidence from adjacent deterministic counters.
- Updated `docs/TRANSACTION_GQL.md` with owned preparation, conservative budget reads, and the staged-overlay evidence boundary.

### Validation boundary

The connector execution environment did not contain the repository-pinned Rust toolchain, `shellcheck`, or a runnable UBS installation. This wave received mechanical checks for source/module ownership, exact blob identity, delimiter balance, trailing whitespace, line width, duplicate methods, redacted diagnostics, exact-boundary logic, and evidence/no-claim consistency.

Checked-in tests state intended laws. This changelog does not claim a fresh rustfmt, compile, Clippy, Rust-test, shellcheck, UBS, or complete `scripts/check.sh` verdict for the current tree. The next proof-bearing step is an exact-tree run captured by `scripts/local_proof.sh` on a machine with the pinned toolchain.

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
- staged-overlay identity and exact transaction-result evidence;
- portable query evidence and an external verifier SDK;
- full ISO GQL, GLA/Loom execution, optimizer, spill, and larger-than-memory queries;
- Strata tiers I/R/A, Ripple, Beacon, Prism, Warden, Fabric, and Aegis;
- CLI/robot mode, server, Python bindings, installer, signed releases, and upgrades.

## Guidance for future entries

- Record only executable or mechanically enforced behavior.
- Name the exact subset and refusal/no-claim boundary.
- Separate checked-in tests from executed proof.
- Preserve red and void evidence; never summarize it as green.
- Keep Git, live Beads state, derived indexes, context capsules, and proof bundles as separate authorities.
