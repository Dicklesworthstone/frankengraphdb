# FrankenGraphDB Implementation Status

Capability baseline: unreleased `main` through the 2026-09-01 owned prepared-query and deterministic-budget continuation.

This document is the compact, agent-facing map of **what executes now**, **where each live behavior is owned**, **what its evidence proves**, and **what remains target architecture**. The comprehensive plan remains normative for the finished system; this file is the present-tense reality map.

## Product reality

FrankenGraphDB is not yet a released graph-database product. There are no tagged releases, installer, supported CLI/server distribution, Python package, or compatibility promise. The live product surface is an embedded Rust composition crate over real Chronicle durability and real Strata tier-D storage, with a deliberately bounded GQL read slice and a bounded write-transaction overlay.

The repository contains real durable mechanisms and executable cross-layer paths. It does **not** yet contain the complete W10 product surface, full ISO GQL, GLA/Loom physical execution, full SSI, or the distributed and incremental system described by the plan.

## Live product verticals

### Durable embedded database

`fgdb::Database` can:

- create and reopen through the real Chronicle capsule-first, marker-last two-fsync path;
- recover the authoritative marker chain and authenticate checkpoint-selected Strata roots against it;
- commit typed `WriteBatch` mutations under the production first-committer-wins validator;
- read vertices, edges, adjacency, and complete collections at the live frontier or an exact retained `CommitSeq`;
- run over the ordinary filesystem VFS or the in-memory VFS without substituting an in-memory graph model for the durable composition path.

Canonical owner: `crates/fgdb/src/lib.rs`.

Runnable witness:

```bash
cargo run -p fgdb --example open_a_database
```

### Bounded GQL execution

The live grammar is intentionally smaller than ISO GQL. It includes a deterministic subset of:

- labeled node scans;
- directed, incoming, and undirected one-hop patterns;
- bounded two-hop patterns;
- selected equality, inequality, and integer-property predicates;
- deterministic projection, `SKIP`, and `LIMIT`.

Parsing and binding live in `crates/fgdb-gql/src/parser.rs`. Live database reads, historical reads, and immutable read-session reads share one exact-sequence execution kernel in `crates/fgdb/src/gql_exec.rs`.

### Pinned embedded read sessions

`Database::read_session()` returns an immutable `EmbeddedReadView` owning one decoded generation. A view:

- keeps its frontier and decoded graph stable after later database writes;
- can be cloned while sharing the same immutable generation;
- executes text, plan-only, or owned prepared GQL at its pinned frontier;
- executes older retained sequences at or below that frontier;
- refuses a later sequence through `ReadError::BeyondFrontier`.

This is a real embedded read-session subset. It is not authorization negotiation, lease/reattach, cursor ownership, server transport, or the synchronous facade.

### Reusable plan-only execution

`BoundPlan` remains the executor-ready plan-only form. It is appropriate when statement spelling and the caller's name-to-ID map are intentionally outside the value's identity.

| Surface | Live/pinned | Exact retained sequence | Plan evidence |
|---|---:|---:|---:|
| `Database` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_certified[_at]` |
| `EmbeddedReadView` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_certified[_at]` |
| `WriteTxn` | `execute_prepared_gql` | transaction basis only | `execute_prepared_gql_certified` |

### Coherent owned preparation

`PreparedGqlQuery` is the coherent reusable definition. It owns, behind private fields:

- the exact statement bytes;
- a cloned canonical `RelationBind`;
- the `BoundPlan` derived from those inputs exactly once.

The original statement and bind map may be changed by the caller after preparation without changing the prepared definition. `Debug` redacts statement, bind, and plan contents. `verifies_definition()` reparses and rebinds the retained inputs as an explicit audit check; normal execution does not need to do so.

Current entrypoints include:

| Surface | Preparation | Execution | Historical | Full durable-read evidence |
|---|---|---|---|---|
| `Database` | `prepare_gql_query` | `execute_prepared_query` | `execute_prepared_query_at` | `execute_prepared_query_with_result_digest[_at]` |
| `EmbeddedReadView` | `prepare_gql_query` | `execute_prepared_query` | `execute_prepared_query_at` | `execute_prepared_query_with_result_digest[_at]` |
| `WriteTxn` | `prepare_gql_query` | `execute_prepared_query` | pinned basis only | plan evidence only |

The high-level methods live in `crates/fgdb/src/write_txn_parts/owned_prepared.rs`; the owned definition and its construction invariant live in `crates/fgdb-gql/src/prepared.rs`. They reuse the existing binder and execution bodies rather than creating a second query path.

Runnable witness:

```bash
cargo run -p fgdb --example gql_owned_prepared
```

This is not yet the final parameterized prepared-statement protocol. Typed parameters, catalog epochs, authorization context, physical-plan selection, cursor lifecycle, invalidation, and portable persistence remain open.

### Deterministic prepared-query budgets

Owned prepared execution accepts `GqlExecutionBudget` on `Database`, `EmbeddedReadView`, and `WriteTxn`.

The two current dimensions are exact and deliberately narrow:

1. `SnapshotRecords`: the complete immutable vertex table admitted by a node scan, or edge table admitted by an edge pattern.
2. `ResultRows`: final rows after predicates, projection, sorting, deduplication, `SKIP`, and `LIMIT`.

`observed == limit` succeeds. `observed > limit` returns `BudgetedGqlError::Budget(GqlBudgetExceeded { dimension, limit, observed })`. No partial rows escape. Successful execution returns `GqlExecutionStats` beside the value.

The current implementation reads/counts the relevant table before the ordinary executor reads it. The budget is therefore a deterministic downstream-work and result guard, **not**:

- wall-clock cancellation;
- allocation or I/O preemption;
- memory or spill governance;
- a streaming/backpressure protocol;
- physical runtime-cost evidence.

A transaction snapshot-count refusal still records the conservative overlay read set because counting the staged view is itself a transaction read.

### Bounded write transaction

`WriteTxn` pins one basis sequence, overlays staged vertex and edge mutations for read-your-own-writes behavior, records read dependencies and MATCH expansions, and commits one prepared same-relation batch through the production FCW seam.

Text execution binds once and delegates to the plan-only overlay executor. Owned preparation also delegates to that body. The implementation is decomposed under `crates/fgdb/src/write_txn_parts/` while retaining one private module and one `WriteTxn` state authority.

This is not full SSI. Predicate/range conflict tracking, merge-ladder integration, transaction ownership/session policy, and multi-relation writes remain incomplete.

## Query evidence truth table

| Evidence | Binds | Does not bind |
|---|---|---|
| `GqlCertificate` | exact statement bytes, canonical `RelationBind`, snapshot sequence | parsed plan, returned rows, transaction overlay, physical plan, runtime cost |
| `GqlPlanCertificate` | every current `BoundPlan` field and snapshot sequence under transcript v2 | statement spelling, returned rows, transaction overlay, physical plan, runtime cost |
| ordered-result digest | plan-certificate digest, snapshot, exact row count, row order, every returned `VId` | physical plan, cost, authorization, transaction overlay, portable framing |
| owned prepared durable-read evidence | input certificate + plan certificate + ordered-result digest from one retained definition and successful exact-sequence read | typed parameters, physical plan, runtime cost, portable replay bundle |
| deterministic budget stats | admitted table count and final row count for one successful budgeted call | cryptographic attestation, I/O/allocation cost, operator work, wall time |

The v2 plan transcript includes `BoundPlan::neq`, omitted by historical v1. New certificates use v2; legacy-v1 verification is explicit.

Transaction-overlay execution deliberately stops at plan evidence. The plan certificate names the durable basis and bound plan, not the staged mutation sequence. Issuing an ordered-result digest for staged rows before defining canonical overlay identity would overclaim replayability.

Detailed contracts:

- `docs/GQL_RESULT_EVIDENCE.md`
- `docs/TRANSACTION_GQL.md`

## Live agent-operability verticals

### Exact-SHA local context capsules

`scripts/agent_context.sh` emits a credential-free advisory package containing an exact commit/tree, one-head Git bundle, deterministic source archive, history and tracked-file inventories, stable clean/dirty worktree evidence, and truthful Beads observation modes.

The v2 verifier imports the bundle into an isolated retained repository and recomputes the tree, archive, paths, and recent history. `agent_context_checkout.sh` materializes a verified detached checkout with no remote.

Contract: `docs/AGENT_CONTEXT_CAPSULE.md`.

### Exact-tree local proof bundles

`scripts/local_proof.sh` captures `bash scripts/check.sh` on one clean exact tree and preserves the raw exit, stdout/stderr, tool versions, committed tree, tracked gate-driver blob, and before/after worktree state.

Verdicts remain three-valued:

- `pass`: stable tree and check exit zero;
- `red`: stable tree and nonzero check exit;
- `void`: commit or worktree state moved.

Contract: `docs/LOCAL_PROOF_BUNDLE.md`.

## Agent source-ownership map

| Concern | Canonical owner |
|---|---|
| durable database composition and temporal graph reads | `crates/fgdb/src/lib.rs` |
| one exact-sequence GQL read kernel | `crates/fgdb/src/gql_exec.rs` |
| parser, binder, `RelationBind`, `BoundPlan` | `crates/fgdb-gql/src/parser.rs` |
| `PreparedGqlQuery` and deterministic budget vocabulary | `crates/fgdb-gql/src/prepared.rs` |
| owned prepared execution adapters | `crates/fgdb/src/write_txn_parts/owned_prepared.rs` |
| input/plan/result evidence transcripts | `crates/fgdb/src/gql_cert.rs` |
| staged transaction overlay and conflict tracking | `crates/fgdb/src/write_txn_parts/` |
| advisory local context package | `scripts/agent_context*.sh` |
| exact-tree local proof package | `scripts/local_proof*.sh` |
| independent semantic oracle | `crates/fgdb-reference` |
| crash/differential harness | `crates/fgdb-sim` |

Do not add a second parser, binder, execution kernel, session type, evidence transcript, or authority merely to make a narrow test pass.

## Focused witnesses and tests

```bash
cargo run -p fgdb --example open_a_database
cargo run -p fgdb --example gql_time_travel
cargo run -p fgdb --example gql_result_digest
cargo run -p fgdb --example gql_owned_prepared

bash scripts/agent_context_selftest.sh
bash scripts/local_proof_selftest.sh
```

Focused integration suites include:

- `embedded_read_session.rs`
- `gql_prepared_at.rs`
- `gql_plan_certificate_refusals.rs`
- `gql_aligned_certificates.rs`
- `gql_result_digest.rs`
- `gql_undirected_certified.rs`
- `gql_owned_prepared.rs`

## Validation status

The repository-wide authority remains:

```bash
bash scripts/check.sh
```

For the owned-preparation and deterministic-budget continuation, the connector environment did not contain the repository-pinned Rust toolchain, `shellcheck`, or a runnable UBS installation. The changes were checked mechanically for source-owner closure, module/include closure, exact blob identity, delimiter balance, trailing whitespace, line width, duplicate inherent methods, redacted diagnostics, evidence/no-claim consistency, and gate-sensitive scanner hazards.

Checked-in Rust tests state intended laws. This document does **not** claim a fresh rustfmt, compile, Clippy, Rust-test, shellcheck, UBS, or complete `scripts/check.sh` verdict for the current head. The next authoritative step is an exact-tree run captured with `scripts/local_proof.sh` on a machine with the pinned toolchain.

## Major remaining systems

The following remain incomplete or absent:

- typed prepared-statement parameters and canonical parameter evidence;
- typed rows/columns, genuine streaming cursors, and backpressure;
- full session ownership, authorization, renew/expiry/reattach, and synchronous facade;
- full SSI, predicate/range conflicts, merge ladder, and general multi-relation transactions;
- registered portable query-evidence framing and external verifier SDK;
- staged-overlay identity and exact transaction-result evidence;
- full ISO GQL, GLA lowering, Loom operators, optimizer, spill, and larger-than-memory query execution;
- Strata tiers I/R/A and production compaction/migration policy;
- Ripple, Beacon, Prism, Warden, Fabric, and Aegis;
- CLI/robot mode, server, Python bindings, packaging, signed releases, installer, and upgrade tooling.

## Dependency-ordered next work

1. Run the pinned toolchain and preserve the exact-tree verdict with `scripts/local_proof.sh`.
2. Add typed parameters as structural parser operands; never use string substitution.
3. Bind canonical parameter values into preparation identity and every applicable evidence layer.
4. Register a portable query-evidence format before persistence or external-verifier APIs.
5. Define canonical staged-overlay identity before issuing exact transaction-result evidence.
6. Add genuine cursor/backpressure ownership and earlier resource enforcement in the storage/operator path.
7. Continue into full session ownership, SSI, and GLA/Loom lowering.

## Recent evolution

- `92e39e59` — one exact-sequence execution kernel for database and immutable read views.
- `617bd687` through `f4f40d61` — pinned sessions and historical reusable plans.
- `01f28d39` through `f4a617c4` — verifiable v2 input/plan certificates.
- `051b90ba` through `65d3d832` — aligned input/plan evidence and exact ordered-row digest.
- `005a8397` — prepared transaction-overlay execution without rebinding.
- `c7a0558e` — coherent owned `PreparedGqlQuery` definition.
- `61970273` through `3f85fa61` — owned execution across durable/view/transaction surfaces and gate-clean stability law.
- `6c40d30c` through `78137cfb` — deterministic budget vocabulary, execution paths, and cross-surface boundary tests.
- `d9909a49` — runnable owned-preparation/budget witness.

Keep this file synchronized whenever a capability crosses from plan or red-bar acceptance test into an inhabitable public path.
