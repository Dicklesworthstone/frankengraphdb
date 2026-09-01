# FrankenGraphDB Implementation Status

Capability baseline: unreleased `main` through transaction-overlay prepared GQL commit `005a839701d3a9d270c17ab68d5641dc8fa74a48` and the local context/proof v2 tooling landed earlier on 2026-09-01.

This is the concise, agent-facing map of **what the repository can execute now**, **what each evidence object proves**, and **what remains architectural target state**. The comprehensive plan remains normative for the finished system; this document records the inhabitable subset.

## Product reality

FrankenGraphDB is not yet a released graph-database product. There are no tagged releases, installer, CLI binary, server binary, Python package, or compatibility promise. The working product surface is an embedded Rust composition crate over real Chronicle durability and real Strata tier-D storage, with a deliberately bounded GQL read slice and a one-batch write-transaction subset.

The important distinction is:

- the repository already contains real durable mechanisms and executable cross-layer paths;
- it does **not** yet contain the full W10 product surface, full GQL algebra/planner/executor, or complete distributed and incremental system described by the plan.

## Live product verticals

### Durable embedded database

`fgdb::Database` can:

- create and reopen through the real Chronicle two-fsync commit path;
- recover the authoritative marker chain and authenticate checkpoint-selected Strata roots against it;
- commit typed `WriteBatch` mutations under the production first-committer-wins validator;
- read vertices, edges, adjacency, and complete collections at the live frontier or an exact retained `CommitSeq`;
- run over the ordinary filesystem VFS or the in-memory VFS without substituting an in-memory graph model for the durable path.

The source owner is `crates/fgdb/src/lib.rs`. The simplest executable witness is `crates/fgdb/examples/open_a_database.rs`.

### Bounded GQL execution

The live grammar is intentionally smaller than ISO GQL. It includes a deterministic subset of:

- labeled node scans;
- directed, incoming, and undirected one-hop patterns;
- bounded two-hop patterns;
- selected equality, inequality, and integer-property predicates;
- deterministic projection, `SKIP`, and `LIMIT`.

Parsing and binding live in `crates/fgdb-gql/src/lib.rs`. Snapshot execution lives in `crates/fgdb/src/gql_exec.rs`. Live database reads, historical reads, and immutable read-session reads share one exact-sequence execution kernel.

### Pinned embedded read sessions

`Database::read_session()` returns an immutable `EmbeddedReadView` owning one decoded generation. A session:

- keeps its frontier, manifest, partition root, and decoded graph stable after later writes;
- can be cloned while sharing the same immutable decoded generation;
- can execute text or prepared GQL at its pinned frontier;
- can execute older retained sequences at or below its frontier;
- refuses a later sequence through `ReadError::BeyondFrontier`.

This is a real read-session subset, not the final W10 session protocol. It has no authorization negotiation, lease/reattach protocol, parameter schema, cursor lifecycle, server transport, or synchronous facade.

### Reusable bound plans

`Database::prepare_gql_plan` and `EmbeddedReadView::prepare_gql_plan` expose the immutable executor-ready `BoundPlan`.

| Surface | Live/pinned | Exact historical sequence | Plan + result evidence |
|---|---:|---:|---:|
| `Database` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_with_result_digest[_at]` |
| `EmbeddedReadView` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_with_result_digest[_at]` |

Exact-sequence methods execute first and mint evidence only after a successful read. A typed refusal therefore returns no evidence.

`BoundPlan` is the honest prepared form of the current bounded grammar. It is not yet an owned or parameterized `PreparedStatement`: statement bytes and canonical bindings still travel separately from a raw plan on the database/read-view surface, and typed parameters, catalog epochs, authorization context, physical-plan selection, result cursors, and invalidation policy remain open.

### Bounded write transaction with prepared overlay GQL

`WriteTxn` pins one basis sequence, overlays staged vertex and edge mutations for read-your-own-writes behavior, records read dependencies, and commits one prepared same-relation batch through the production FCW seam. It is deliberately not full SSI.

The transaction GQL path now has one plan-only overlay executor:

- `execute_gql` binds once and delegates to `execute_prepared_gql`;
- `execute_gql_certified` binds once and delegates to `execute_prepared_gql_certified`;
- `prepared_gql_plan_certificate` certifies an already-bound plan at the transaction basis;
- text, prepared, and certified faces share staged-overlay semantics, read-set tracking, ordering, `SKIP`, and `LIMIT`.

The implementation is decomposed under `crates/fgdb/src/write_txn_parts/` by lifecycle, vertex reads, edge reads, predicate evaluation, overlay graph construction, edge matching, public GQL API, finish/conflict handling, and tests. The parent `write_txn.rs` is only the inclusion/ownership map; all parts compile in one private module and retain one `WriteTxn` state authority.

Transaction plan evidence binds the durable basis and bound plan. It does **not** identify the staged overlay and therefore does not attest standalone replay or exact transaction result rows.

## Query evidence truth table

| Evidence | Binds | Does not bind |
|---|---|---|
| `GqlCertificate` | exact statement bytes, canonical `RelationBind`, snapshot sequence | parsed plan, returned rows, transaction overlay, physical plan, runtime cost |
| `GqlPlanCertificate` | every current `BoundPlan` field and snapshot sequence under transcript v2 | statement spelling, returned rows, transaction overlay, physical plan, runtime cost |
| ordered-result digest | plan-certificate digest, snapshot, exact row count, row order, every returned `VId` | physical plan, cost, authorization, transaction overlay, portable framing |
| `execute_gql_with_result_digest[_at]` | all three layers from one bind and one successful database/read-view execution | portable replay bundle or external-verifier protocol |

The v2 plan transcript includes `BoundPlan::neq`, omitted by historical v1. New certificates use v2; legacy verification is explicitly named.

The ordered-result transcript uses `fgdb:gql-ordered-result-digest:v1` and chains through the plan-certificate digest. Equal rows under different plans or snapshots therefore have different identities. Result evidence is available for text and already-bound execution on both `Database` and `EmbeddedReadView`, live or historical.

There is still **no portable, self-describing query-execution artifact**. Persistence and external verification require a registered canonical format rather than an ad hoc serialization helper. The detailed boundary is in `docs/GQL_RESULT_EVIDENCE.md`.

## Live agent-operability verticals

### Exact-SHA local context capsules, format v2

`scripts/agent_context.sh` exports a credential-free advisory handoff containing an exact-HEAD Git bundle, deterministic tracked-source archive, committed tree id, tracked-file and recent-history inventories, initial worktree state, optional dirty patch, and optional Beads views.

Format v2 closes the checksum-only substitution gap:

- the manifest names the exact commit and tree;
- the bundle advertises exactly one `HEAD`;
- `scripts/agent_context_verify.sh` imports that bundle into an isolated retained repository;
- the verifier recomputes the tree, source archive, tracked-file inventory, and recent history from the bundled commit;
- a checksum-consistent replacement archive or fabricated history is rejected;
- strict manifest and file inventories reject duplicate keys and undeclared files;
- format-v1 capsules remain verifiable for migration.

`scripts/agent_context_checkout.sh` turns a verified capsule into a detached checkout with no remote and can optionally apply the verified tracked dirty patch. Untracked contents are never reconstructed because they were never exported.

`scripts/agent_context_selftest.sh` retains clean/dirty v1/v2 fixtures plus checksum-invalid, source-substitution, duplicate-manifest-key, undeclared-file, and fabricated-history controls. The complete contract is `docs/AGENT_CONTEXT_CAPSULE.md`.

### Exact-tree local proof bundles, format v2

`scripts/local_proof.sh` runs the authoritative `bash scripts/check.sh` from one clean exact commit and preserves stdout, stderr, raw exit code, tool versions, before/after commit and worktree state, committed tree, exact tracked `scripts/check.sh` blob, manifest, and checksums.

Its verdict is deliberately three-valued:

- `pass`: stable commit/tree/worktree and check exit `0`;
- `red`: stable commit/tree/worktree and nonzero check exit, preserving that exit code;
- `void`: commit, committed tree, or worktree state moved, returning wrapper exit `125`.

`scripts/local_proof_verify.sh` supports v1/v2, enforces exact manifest/file/checksum inventory and the full `check.sh` reporting contract, and can accept `--repository DIR` to bind the proof commit, tree, and check-script blob to an independently supplied Git object database. Verification never reruns the gate and never turns red or void into green.

`scripts/local_proof_selftest.sh` retains v1/v2 pass, exit-7 red, moving-tree void, checksum-invalid, undeclared-file, duplicate-key, false-tree, and malformed-red-report fixtures. The complete contract is `docs/LOCAL_PROOF_BUNDLE.md`.

The context capsule and proof bundle remain separate so refreshing cheap observability cannot silently inherit or create an expensive product verdict.

## Agent source-ownership map

| Concern | Canonical owner |
|---|---|
| durable database composition, open/reopen, writes, temporal graph reads, `EmbeddedReadView`, text GQL entrypoints | `crates/fgdb/src/lib.rs` |
| one exact-sequence database/read-view GQL kernel; prepared-plan and read-session execution | `crates/fgdb/src/gql_exec.rs` |
| transaction overlay state and execution | `crates/fgdb/src/write_txn.rs` plus `write_txn_parts/` |
| input/plan transcripts, ordered-result digest, verification, aligned evidence | `crates/fgdb/src/gql_cert.rs` |
| parser, binder, `RelationBind`, `BoundPlan` | `crates/fgdb-gql/src/lib.rs` |
| advisory local context package | `scripts/agent_context*.sh`, `docs/AGENT_CONTEXT_CAPSULE.md` |
| exact-tree local proof package | `scripts/local_proof*.sh`, `docs/LOCAL_PROOF_BUNDLE.md` |
| independent semantic oracle | `crates/fgdb-reference` |
| crash/differential harness | `crates/fgdb-sim` |

Do not add a second execution kernel, session type, overlay authority, evidence transcript, context authority, or proof authority merely to make a narrow test pass.

## Focused witnesses and controls

```bash
cargo run -p fgdb --example open_a_database
cargo run -p fgdb --example gql_time_travel
cargo run -p fgdb --example gql_prepared_read_session
cargo run -p fgdb --example gql_result_digest

bash scripts/agent_context_selftest.sh
bash scripts/local_proof_selftest.sh
```

Focused query suites include `embedded_read_session.rs`, `gql_prepared_at.rs`, `gql_plan_certificate_refusals.rs`, `gql_aligned_certificates.rs`, `gql_result_digest.rs`, and the expanded `gql_undirected_certified.rs` under `crates/fgdb/tests/`.

## Validation status

The repository-wide authority remains:

```bash
bash scripts/check.sh
```

The context/proof v2 scripts passed `bash -n` and their retained semantic control suites locally before landing; the committed blobs match the tested local files. The transaction refactor passed whitespace checks, exact blob-identity checks, include-target closure, delimiter-aware lexical scanning, and unique method-ownership checks.

The connector execution environment does not contain the repository-pinned Rust toolchain or `shellcheck`. This status therefore does **not** claim a fresh Rust compile, rustfmt, Clippy, Rust-test, shellcheck, or complete `scripts/check.sh` verdict for commit `005a8397…`. Checked-in Rust tests state intended laws; only an executed pinned-toolchain proof establishes them.

## Major remaining systems

The following remain incomplete or absent:

- an owned prepared-query definition keeping statement bytes, canonical bind, and plan inseparable;
- typed statement parameters, typed rows/columns, cursor and result-stream lifecycle;
- full session ownership, authorization, reattach/renew/expiry, and synchronous embedded facade;
- full SSI, predicate/range conflicts, and merge-ladder integration;
- exact staged-overlay result evidence and portable execution artifacts;
- full ISO GQL, GLA lowering, Loom operators, optimizer, morsel-parallel execution, spill, and larger-than-memory queries;
- Strata tiers I/R/A and production compaction/migration policy;
- Ripple incremental views/subscriptions and recursion;
- Beacon indexing and hybrid retrieval;
- Prism/`fnx-*` integration;
- Warden capabilities and secure views;
- Fabric server protocols and Aegis operational control plane;
- CLI, robot-mode NDJSON, Python bindings, packaging, signed releases, installer, and upgrade tooling;
- signed context/proof distribution, physical-plan evidence, and deterministic runtime-cost evidence.

## Dependency-ordered next work

1. Execute the pinned toolchain locally and preserve the exact-tree result with `scripts/local_proof.sh`.
2. Introduce an owned prepared-query definition keeping statement bytes, canonical bind, and `BoundPlan` inseparable across database, read-view, and transaction surfaces.
3. Add typed statement parameters and bind canonical parameter values into every evidence layer.
4. Define a staged-overlay identity before issuing exact transaction-result evidence.
5. Register a portable execution-evidence format before persistence or external-verifier APIs.
6. Advance the larger W10 session/transaction contract, SSI, and GLA/Loom lowering rather than widening the bounded grammar opportunistically.

## Recent evolution

- `92e39e59` — one snapshot execution kernel for database and immutable read views;
- `617bd687` through `f4f40d61` — pinned sessions and historical reusable plans;
- `01f28d39` through `f4a617c4` — externally verifiable v2 input/plan certificates;
- `051b90ba` through `6c510378` — aligned input-plus-plan evidence;
- `1175b620` through `65d3d832` — exact ordered-row digest and prepared-plan parity;
- `306e33e2`, `0985ceea`, `e7dd89c5`, `06945a44` — exact-tree context v2 producer, deep verifier, checkout consumer, and mutation controls;
- `2f08dc72`, `4c035342`, `a047c93b` — exact-tree proof v2 verifier, producer, and mutation controls;
- `005a8397` — one prepared transaction-overlay execution body and responsibility-focused source decomposition.

Keep this file synchronized when a capability crosses from design or red-bar acceptance test into an inhabitable public path.
