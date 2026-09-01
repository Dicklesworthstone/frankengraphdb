# FrankenGraphDB Implementation Status

Capability baseline: unreleased `main` through the local agent-context and exact-tree proof tooling landed on 2026-09-01. Product-query baseline remains the ordered-result-evidence slice through `65d3d8323432ced15e9255388f8b74e00fae1b5f`.

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

### Reusable prepared plans

`Database::prepare_gql_plan` and `EmbeddedReadView::prepare_gql_plan` expose the immutable executor-ready `BoundPlan`.

| Surface | Live/pinned | Exact historical sequence | Plan + result evidence |
|---|---:|---:|---:|
| `Database` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_with_result_digest[_at]` |
| `EmbeddedReadView` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_with_result_digest[_at]` |

Exact-sequence methods execute first and mint evidence only after a successful read. A typed refusal therefore returns no evidence.

`BoundPlan` is the honest prepared form of the current bounded grammar. It is not yet a parameterized `PreparedStatement`: parameters, typed parameter schemas, catalog epochs, authorization context, physical-plan selection, result cursors, and invalidation policy remain open.

### Bounded write transaction

`WriteTxn` pins one basis sequence, overlays staged vertex and edge mutations for read-your-own-writes behavior, records read dependencies, and commits one prepared same-relation batch through the production FCW seam. It is deliberately not full SSI.

The source owner is `crates/fgdb/src/write_txn.rs`. Its text and certified GQL paths still need to be refactored so an already-bound plan executes directly over the overlay without rebinding.

## Query evidence truth table

| Evidence | Binds | Does not bind |
|---|---|---|
| `GqlCertificate` | exact statement bytes, canonical `RelationBind`, snapshot sequence | parsed plan, returned rows, transaction overlay, physical plan, runtime cost |
| `GqlPlanCertificate` | every current `BoundPlan` field and snapshot sequence under transcript v2 | statement spelling, returned rows, transaction overlay, physical plan, runtime cost |
| ordered-result digest | plan-certificate digest, snapshot, exact row count, row order, every returned `VId` | physical plan, cost, authorization, transaction overlay, portable framing |
| `execute_gql_with_result_digest[_at]` | all three layers from one bind and one successful execution | portable replay bundle or external-verifier protocol |

The v2 plan transcript includes `BoundPlan::neq`, omitted by historical v1. New certificates use v2; legacy verification is explicitly named.

The ordered-result transcript uses `fgdb:gql-ordered-result-digest:v1` and chains through the plan-certificate digest. Equal rows under different plans or snapshots therefore have different identities. Result evidence is available for text and already-bound execution on both `Database` and `EmbeddedReadView`, live or historical.

There is still **no portable, self-describing query-execution artifact**. Persistence and external verification require a registered canonical format rather than an ad hoc serialization helper. The detailed boundary is in `docs/GQL_RESULT_EVIDENCE.md`.

## Live agent-operability verticals

### Exact-SHA local context capsules

`scripts/agent_context.sh` exports a credential-free advisory handoff containing:

- an exact-HEAD Git bundle;
- a deterministic tracked-source archive;
- tracked-file and recent-history inventories;
- the initial worktree state;
- optional dirty tracked patch plus a byte-identical end-of-export stability proof;
- untracked names but never untracked contents;
- tracked Beads JSONL and read-only live `br` views when available;
- a manifest and strict SHA-256 inventory.

The producer refuses dirty trees by default, refuses output inside the repository, never overwrites an existing directory, and voids an export if `HEAD` or worktree state moves.

`scripts/agent_context_verify.sh` independently checks required files, symlink absence, manifest semantics, exact checksum inventory, bundle/commit agreement, archive hygiene, dirty-state evidence, and Beads-mode closure.

`scripts/agent_context_selftest.sh` retains and prints clean, dirty, and deliberately corrupted fixtures. The complete contract is `docs/AGENT_CONTEXT_CAPSULE.md`.

### Exact-tree local proof bundles

`scripts/local_proof.sh` runs the authoritative `bash scripts/check.sh` from one clean exact commit and preserves stdout, stderr, raw exit code, tool versions, before/after commit and worktree state, manifest, and checksums.

Its verdict is deliberately three-valued:

- `pass`: stable tree and check exit `0`;
- `red`: stable tree and nonzero check exit, preserving that exit code;
- `void`: `HEAD` or worktree state moved, returning wrapper exit `125`.

`scripts/local_proof_verify.sh` independently validates bundle integrity and verdict consistency. A verified red or void remains red or void; artifact verification never manufactures a green product verdict.

`scripts/local_proof_selftest.sh` retains and prints pass, exit-7 red, moving-tree void, and checksum-tampered controls. The complete contract is `docs/LOCAL_PROOF_BUNDLE.md`.

The context capsule and proof bundle remain separate so refreshing cheap observability cannot silently inherit or create an expensive product verdict.

## Agent source-ownership map

| Concern | Canonical owner |
|---|---|
| durable database composition, open/reopen, writes, temporal graph reads, `EmbeddedReadView`, text GQL entrypoints | `crates/fgdb/src/lib.rs` |
| one exact-sequence GQL kernel; prepared-plan and read-session execution | `crates/fgdb/src/gql_exec.rs` |
| input/plan transcripts, ordered-result digest, verification, aligned evidence | `crates/fgdb/src/gql_cert.rs` |
| staged write overlay, read-set tracking, one-batch transaction commit | `crates/fgdb/src/write_txn.rs` |
| parser, binder, `RelationBind`, `BoundPlan` | `crates/fgdb-gql/src/lib.rs` |
| advisory local context package | `scripts/agent_context*.sh`, `docs/AGENT_CONTEXT_CAPSULE.md` |
| exact-tree local proof package | `scripts/local_proof*.sh`, `docs/LOCAL_PROOF_BUNDLE.md` |
| independent semantic oracle | `crates/fgdb-reference` |
| crash/differential harness | `crates/fgdb-sim` |

Do not add a second execution kernel, session type, evidence transcript, context authority, or proof authority merely to make a narrow test pass.

## Focused witnesses and controls

```bash
cargo run -p fgdb --example open_a_database
cargo run -p fgdb --example gql_time_travel
cargo run -p fgdb --example gql_prepared_read_session
cargo run -p fgdb --example gql_result_digest

bash scripts/agent_context_selftest.sh
bash scripts/local_proof_selftest.sh
```

Focused query suites include `embedded_read_session.rs`, `gql_prepared_at.rs`, `gql_plan_certificate_refusals.rs`, `gql_aligned_certificates.rs`, and `gql_result_digest.rs` under `crates/fgdb/tests/`.

## Validation status

The repository-wide authority remains:

```bash
bash scripts/check.sh
```

The local context and proof scripts passed `bash -n`, whitespace/diff checks, and retained semantic fixtures covering clean/dirty export, no-overwrite, checksum and inventory verification, corruption rejection, stable pass, stable red with exit preservation, and moving-tree void semantics.

The connector execution environment does not contain the repository-pinned Rust toolchain. This status therefore does **not** claim a fresh compiled, Clippy, Rust-test, or complete `scripts/check.sh` verdict for the current head. Checked-in Rust tests state intended laws; only an executed pinned-toolchain proof establishes them.

## Major remaining systems

The following remain incomplete or absent:

- full session ownership, authorization, reattach/renew/expiry, and synchronous embedded facade;
- parameterized prepared statements, typed rows/columns, cursor and result-stream lifecycle;
- transaction prepared-plan execution, full SSI, predicate/range conflicts, and merge-ladder integration;
- full ISO GQL, GLA lowering, Loom operators, optimizer, morsel-parallel execution, spill, and larger-than-memory queries;
- Strata tiers I/R/A and production compaction/migration policy;
- Ripple incremental views/subscriptions and recursion;
- Beacon indexing and hybrid retrieval;
- Prism/`fnx-*` integration;
- Warden capabilities and secure views;
- Fabric server protocols and Aegis operational control plane;
- CLI, robot-mode NDJSON, Python bindings, packaging, signed releases, installer, and upgrade tooling;
- registered portable query artifacts, signed context/proof distribution, physical-plan evidence, and deterministic runtime-cost evidence.

## Dependency-ordered next work

1. Execute the pinned toolchain locally and preserve the result with `scripts/local_proof.sh`.
2. Refactor `WriteTxn` so text, already-bound, and certified overlay reads bind once and share one body.
3. Introduce an owned prepared-query definition keeping statement bytes, canonical bind, and `BoundPlan` inseparable.
4. Add typed statement parameters and bind canonical parameter values into every evidence layer.
5. Register a portable execution-evidence format before persistence or external-verifier APIs.
6. Add signed publication only through the release/external-verifier constitution, not by silently strengthening local checksum claims.
7. Advance the larger W10 session/transaction contract rather than widening the bounded grammar opportunistically.

## Recent evolution

- `92e39e59` — one snapshot execution kernel for database and immutable read views;
- `617bd687` through `f4f40d61` — pinned sessions and historical reusable plans;
- `01f28d39` through `f4a617c4` — externally verifiable v2 input/plan certificates;
- `051b90ba` through `6c510378` — aligned input-plus-plan evidence;
- `1175b620` through `65d3d832` — exact ordered-row digest and prepared-plan parity;
- `0ce6f9c2`, `ff5631ed`, `a0a2d73d`, `020fd69d` — exact-SHA local context producer, independent verifier, semantic controls, and contract;
- `f4759d95`, `5eefc216` — exact-tree local proof producer/verifier/controls and contract.

Keep this file synchronized when a capability crosses from design or red-bar acceptance test into an inhabitable public path.
