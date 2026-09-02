# FrankenGraphDB Implementation Status

Capability baseline: unreleased `main` through the 2026-09-02 strict query-evidence envelope and replay-audit continuation.

This document is the compact, agent-facing map of **what executes now**, **where each live behavior is owned**, **what its evidence proves**, and **what remains target architecture**. The comprehensive plan remains normative for the finished system; this file is the present-tense reality map.

## Product reality

FrankenGraphDB is not yet a released graph-database product. There are no tagged releases, supported installer, CLI/server distribution, Python package, or compatibility promise. The live product surface is an embedded Rust composition crate over real Chronicle durability and real Strata tier-D storage, with a deliberately bounded GQL read slice and a bounded write-transaction overlay.

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

`PreparedGqlQuery` owns, behind private fields:

- exact statement bytes;
- a cloned canonical `RelationBind`;
- the `BoundPlan` derived from those inputs exactly once.

Changing the original statement or bind map cannot change the prepared definition. `Debug` redacts statement, bind, and plan contents. `verifies_definition()` reparses and rebinds the retained inputs as an explicit audit; normal execution does not need to do so.

| Surface | Preparation | Execution | Historical | Exact result evidence |
|---|---|---|---|---|
| `Database` | `prepare_gql_query` | `execute_prepared_query` | `execute_prepared_query_at` | digest and artifact paths |
| `EmbeddedReadView` | `prepare_gql_query` | `execute_prepared_query` | `execute_prepared_query_at` | digest and artifact paths |
| `WriteTxn` | `prepare_gql_query` | `execute_prepared_query` | pinned basis only | staged-overlay certificate and artifact |

The definition and budget vocabulary live in `crates/fgdb-gql/src/prepared.rs`. High-level adapters live in `crates/fgdb/src/write_txn_parts/owned_prepared.rs`. They reuse the existing binder and execution bodies rather than creating another query path.

This is not yet the final parameterized prepared-statement protocol. Typed parameters, catalog epochs, authorization context, physical-plan selection, cursor lifecycle, invalidation, and a released persistence contract remain open.

### Deterministic prepared-query budgets

Owned prepared execution accepts `GqlExecutionBudget` on `Database`, `EmbeddedReadView`, and `WriteTxn`.

Current dimensions:

1. `SnapshotRecords`: complete immutable vertex table admitted by a node scan, or edge table admitted by an edge pattern.
2. `ResultRows`: final rows after predicates, projection, sorting, deduplication, `SKIP`, and `LIMIT`.

`observed == limit` succeeds. `observed > limit` returns `BudgetedGqlError::Budget(GqlBudgetExceeded { dimension, limit, observed })`. No partial rows escape. Successful execution returns exact `GqlExecutionStats`.

The current implementation materializes and counts the relevant table before ordinary execution reads it. The budget is a deterministic downstream-work and result guard, not wall-clock cancellation, allocation or I/O preemption, memory/spill governance, streaming backpressure, or physical runtime-cost evidence.

### Bounded write transaction

`WriteTxn` pins one basis sequence, overlays staged vertex and edge mutations for read-your-own-writes behavior, records read dependencies and MATCH expansions, and commits one prepared same-relation batch through the production FCW seam.

Text execution binds once and delegates to the plan-only overlay executor. Owned preparation delegates to the same body. The implementation is decomposed under `crates/fgdb/src/write_txn_parts/` while retaining one private module and one `WriteTxn` state authority.

This is not full SSI. Predicate/range conflict tracking, merge-ladder integration, transaction ownership/session policy, and multi-relation writes remain incomplete.

## Query evidence tower

| Evidence | Binds | Does not bind |
|---|---|---|
| `GqlCertificate` | exact statement bytes, canonical `RelationBind`, snapshot sequence | parsed plan, rows, transaction overlay, physical plan, runtime cost |
| `GqlPlanCertificate` | every current `BoundPlan` field and snapshot sequence under transcript v2 | statement spelling, rows, transaction overlay, physical plan, runtime cost |
| durable ordered-result digest | plan-certificate digest, snapshot, exact row count, order, every returned `VId` | physical plan, cost, authorization, envelope framing |
| staged-effect digest | transaction basis and canonical staged `LogicalDeltaTemplate`, or explicit empty overlay | durable snapshot, staged bytes as payload, API-call history |
| `GqlOverlayResultCertificate` | basis, plan digest, staged-effect digest, exact row count, order, every returned `VId` | durable/staged payloads, conflict state, standalone replay |
| deterministic budget stats | admitted table count and final row count for one successful call | cryptographic attestation, I/O/allocation cost, operator work, wall time |

The v2 plan transcript includes `BoundPlan::neq`, omitted by historical v1. New certificates use v2; legacy-v1 verification is explicit.

### Exact staged-overlay result evidence

`WriteTxn::staged_effect_digest` binds the basis and canonical semantic net effect under `fgdb:write-txn-staged-effect:v1`.

`execute_prepared_query_with_overlay_result_certificate` executes first and then issues exact rows, a plan certificate, and `GqlOverlayResultCertificate`. The latter binds basis, plan digest, staged-effect digest, row count, order, and every returned `VId` under `fgdb:gql-staged-overlay-result:v1`.

`verifies_prepared_query_overlay_result` derives the current plan and staged-effect identities from the live transaction. A later staged mutation invalidates old evidence. Equivalent transactions at the same basis with the same canonical net effect can verify the same certificate.

This is exact in-process evidence. It is not standalone replay because the certificate does not carry the durable snapshot, staged template bytes, graph rows, read-set state, or conflict state.

## Strict evidence envelopes and replay audit

The current tree now has two canonical **unreleased application envelopes**:

- `GqlPreparedResultArtifact` for one durable prepared-query result;
- `GqlOverlayResultArtifact` for one staged-overlay result.

The common v1 framing is owned by `crates/fgdb-gql/src/evidence_artifact.rs` and contains:

- eight-byte magic `FGQEVID1`;
- explicit major/minor version;
- closed artifact-kind tag;
- zero-required reserved bytes;
- exact snapshot sequence or transaction basis;
- statement, canonical bind, and plan digests;
- staged-effect digest for overlay artifacts;
- exact row count and ordered `VId` bytes;
- the applicable exact result digest.

Decoding fails closed on invalid magic, unsupported versions, wrong kind, nonzero reserved bytes, row-count or length overflow, every truncated prefix, trailing bytes, and a result transcript inconsistent with the included plan/snapshot or overlay identity. Artifact fields are private and rows are redacted from `Debug` output.

### Durable artifact APIs

`Database` and `EmbeddedReadView` expose:

- `execute_prepared_query_artifact[_at]`;
- `audit_prepared_query_artifact`.

A durable audit:

1. decodes the envelope strictly;
2. verifies the exact retained statement and canonical bind;
3. recomputes the canonical `GqlPlanCertificate` at the artifact sequence;
4. cross-checks the envelope's independently recomputed result digest against that product certificate;
5. re-executes the query at the exact historical sequence;
6. requires exact ordered-row equality.

A later write may advance the live frontier without invalidating an older artifact: audit reopens the sequence named by the artifact rather than sampling current state.

### Staged-overlay artifact APIs

`WriteTxn` exposes:

- `execute_prepared_query_overlay_artifact`;
- `audit_prepared_query_overlay_artifact`.

A transaction audit additionally verifies the current transaction basis and canonical staged-effect digest before re-executing the overlay. Staging another mutation after issuance returns a typed `StagedEffectMismatch` for the old artifact.

### Exact boundary of the envelope claim

The framing is versioned and endian-stable, but it is **not yet a released format contract**. It is not:

- an Appendix-A Chronicle object;
- an FGP wire frame;
- a signed publisher attestation;
- a compatibility promise;
- a standalone transaction replay payload;
- an external-verifier SDK.

Promotion beyond the current internal application surface requires a deliberate registry/constitution decision, stable size limits, golden vectors, compatibility policy, and—where provenance matters—a separate signature or transparency layer.

Detailed contracts:

- `docs/GQL_RESULT_EVIDENCE.md`
- `docs/TRANSACTION_GQL.md`

## Agent-operability verticals

### Exact-SHA local context capsules

`scripts/agent_context.sh` emits a credential-free advisory package containing an exact commit/tree, one-head Git bundle, deterministic source archive, history and tracked-file inventories, stable clean/dirty worktree evidence, and truthful Beads observation modes.

The v2 verifier imports the bundle into an isolated retained repository and recomputes the tree, archive, paths, and recent history. `agent_context_checkout.sh` materializes a verified detached checkout with no remote.

### Exact-tree local proof bundles

`scripts/local_proof.sh` captures `bash scripts/check.sh` on one clean exact tree and preserves the raw exit, stdout/stderr, tool versions, committed tree, tracked gate-driver blob, and before/after worktree state.

Verdicts remain three-valued: stable `pass`, stable `red`, and moving-tree `void`.

## Agent source-ownership map

| Concern | Canonical owner |
|---|---|
| durable database composition and temporal graph reads | `crates/fgdb/src/lib.rs` |
| one exact-sequence GQL read kernel | `crates/fgdb/src/gql_exec.rs` |
| parser, binder, `RelationBind`, `BoundPlan` | `crates/fgdb-gql/src/parser.rs` |
| `PreparedGqlQuery` and deterministic budget vocabulary | `crates/fgdb-gql/src/prepared.rs` |
| staged-overlay result transcript | `crates/fgdb-gql/src/overlay_evidence.rs` |
| strict evidence framing and independent transcript decoder | `crates/fgdb-gql/src/evidence_artifact.rs` |
| owned prepared execution adapters | `crates/fgdb/src/write_txn_parts/owned_prepared.rs` |
| canonical staged-effect authority | `crates/fgdb/src/write_txn_parts/overlay_evidence.rs` |
| artifact issuance and product-level replay audit | `crates/fgdb/src/write_txn_parts/portable_evidence.rs` |
| durable input/plan/result certificate authority | `crates/fgdb/src/gql_cert.rs` |
| staged transaction overlay and conflict tracking | `crates/fgdb/src/write_txn_parts/` |
| advisory local context package | `scripts/agent_context*.sh` |
| exact-tree local proof package | `scripts/local_proof*.sh` |
| independent semantic oracle | `crates/fgdb-reference` |
| crash/differential harness | `crates/fgdb-sim` |

Do not add a second parser, binder, execution kernel, session type, or issuing certificate authority merely to make a narrow test pass. Independent artifact decoding is allowed only while product audit cross-checks the canonical certificate transcript, as the current path does.

## Focused witnesses and tests

```bash
cargo run -p fgdb --example open_a_database
cargo run -p fgdb --example gql_time_travel
cargo run -p fgdb --example gql_result_digest
cargo run -p fgdb --example gql_owned_prepared
cargo run -p fgdb --example gql_txn_overlay_result_evidence
cargo run -p fgdb --example gql_evidence_artifact

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
- `gql_txn_overlay_result_evidence.rs`
- `gql_evidence_artifact.rs`

## Validation status

The repository-wide authority remains:

```bash
bash scripts/check.sh
```

The current connector environment does not provide the repository-pinned Rust toolchain, `shellcheck`, or a runnable UBS installation, and hosted GitHub Actions are intentionally excluded from evidence. Checked-in Rust tests state intended laws; only an executed exact-tree proof establishes that the complete tree compiles and passes every registered gate.

The evidence-envelope change received focused mechanical checks for source/module ownership, include closure, exact Git blob identity, delimiter balance, whitespace, line width, private-field construction, reserved-byte and length checks, every-prefix truncation coverage, redacted diagnostics, constant-work digest comparison, transcript-field coverage, canonical issuer/independent-decoder cross-checking, and mutation-sensitive integration coverage. This document does not promote those checks into a whole-tree green verdict.

## Major remaining systems

The following remain incomplete or absent:

- typed prepared-statement parameters and canonical parameter evidence;
- typed rows/columns, genuine streaming cursors, and backpressure;
- full session ownership, authorization, renew/expiry/reattach, and synchronous facade;
- full SSI, predicate/range conflicts, merge ladder, and general multi-relation transactions;
- a registered, compatibility-governed query-evidence format and external verifier SDK;
- standalone staged-overlay replay payloads carrying the staged template and required snapshot authority;
- full ISO GQL, GLA lowering, Loom operators, optimizer, spill, and larger-than-memory execution;
- Strata tiers I/R/A and production compaction/migration policy;
- Ripple, Beacon, Prism, Warden, Fabric, and Aegis;
- CLI/robot mode, server, Python bindings, packaging, signed releases, installer, and upgrade tooling.

## Dependency-ordered next work

1. Run the pinned toolchain and preserve the exact-tree verdict with `scripts/local_proof.sh`.
2. Add typed parameters as structural parser operands; never use string substitution.
3. Bind canonical parameter values into preparation identity, certificates, and both evidence envelopes.
4. Decide whether the v1 application envelope graduates into a registered compatibility contract; if so, freeze size ceilings and golden vectors first.
5. Package staged template bytes and exact snapshot authority before claiming standalone transaction replay.
6. Add genuine cursor/backpressure ownership and earlier resource enforcement in the storage/operator path.
7. Continue into complete session ownership, SSI, and GLA/Loom lowering.

## Recent evolution

- `92e39e59` — one exact-sequence execution kernel for database and immutable read views.
- `617bd687` through `f4f40d61` — pinned sessions and historical reusable plans.
- `01f28d39` through `f4a617c4` — verifiable v2 input/plan certificates.
- `051b90ba` through `65d3d832` — aligned evidence and exact durable ordered-row digest.
- `005a8397` — prepared transaction-overlay execution without rebinding.
- `c7a0558e` through `3f85fa61` — coherent owned preparation across durable, view, and transaction surfaces.
- `6c40d30c` through `78137cfb` — deterministic budget vocabulary and cross-surface laws.
- `80de85a6` through `3e8ff789` — canonical staged-effect identity and exact staged-overlay result evidence.
- `97d09787` through `62a546c7` — strict application envelopes, historical/staged audit, mutation laws, runnable witness, and canonical transcript cross-check.

Keep this file synchronized whenever a capability crosses from plan or red-bar acceptance test into an inhabitable public path.
