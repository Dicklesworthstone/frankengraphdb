# FrankenGraphDB Implementation Status

Capability baseline: unreleased `main` through the 2026-09-02 resource-safe evidence-paging continuation.

This document is the compact, agent-facing map of **what executes now**, **where each live behavior is owned**, **what its evidence proves**, and **what remains target architecture**. The comprehensive plan remains normative for the finished system; this file records the present inhabitable subset.

## Product reality

FrankenGraphDB is not yet a released graph-database product. There are no tagged releases, supported installer, CLI/server distribution, Python package, or compatibility promise. The live product surface is an embedded Rust composition crate over real Chronicle durability and real Strata tier-D storage, with a deliberately bounded GQL read slice and a bounded write-transaction overlay.

The repository contains real durable mechanisms and executable cross-layer paths. It does **not** yet contain the complete W10 surface, full ISO GQL, GLA/Loom physical execution, full SSI, or the distributed and incremental system described by the plan.

## Live product verticals

### Durable embedded database

`fgdb::Database` can:

- create and reopen through the real Chronicle capsule-first, marker-last two-fsync path;
- recover the authoritative marker chain and authenticate checkpoint-selected Strata roots against it;
- commit typed `WriteBatch` mutations under the production first-committer-wins validator;
- read vertices, edges, adjacency, and complete collections at the live frontier or an exact retained `CommitSeq`;
- run over the ordinary filesystem VFS or the in-memory VFS without substituting an in-memory graph model for the durable composition path.

Canonical owner: `crates/fgdb/src/lib.rs`.

### Bounded deterministic GQL

The live grammar is intentionally smaller than ISO GQL. It includes a deterministic subset of:

- labeled node scans;
- directed, incoming, and undirected one-hop patterns;
- bounded two-hop patterns;
- selected equality, inequality, and integer-property predicates;
- deterministic projection, `SKIP`, and `LIMIT`.

Parsing and binding live in `crates/fgdb-gql/src/parser.rs`. Live database reads, historical reads, and immutable read-view reads share one exact-sequence execution kernel in `crates/fgdb/src/gql_exec.rs`.

Typed statement parameters are not yet implemented. Query values are still literals in the live grammar.

### Pinned embedded read views

`Database::read_session()` returns an immutable `EmbeddedReadView` owning one decoded generation. A view:

- keeps its frontier and decoded graph stable after later database writes;
- can be cloned while sharing the same immutable generation;
- executes text, plan-only, or owned prepared GQL at its pinned frontier;
- executes older retained sequences at or below that frontier;
- refuses a later sequence through `ReadError::BeyondFrontier`.

This is a real embedded read-view subset. It is not authorization negotiation, lease/reattach, cursor ownership, server transport, or the synchronous facade.

### Reusable plan-only execution

`BoundPlan` is the executor-ready plan-only form. It is appropriate when statement spelling and the caller's name-to-ID map are intentionally outside the value's identity.

| Surface | Live or pinned | Exact retained sequence | Plan evidence |
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
| `WriteTxn` | `prepare_gql_query` | `execute_prepared_query` | pinned basis only | staged certificate and artifact |

The owned definition and query-budget vocabulary live in `crates/fgdb-gql/src/prepared.rs`. High-level adapters live in `crates/fgdb/src/write_txn_parts/owned_prepared.rs`. They reuse the existing binder and execution bodies rather than creating another query path.

This is not yet the final parameterized prepared-statement protocol. Typed parameters, catalog epochs, authorization context, physical-plan selection, cursor lifecycle, invalidation, and a released persistence contract remain open.

### Deterministic query-execution budgets

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
| durable ordered-result digest | plan digest, snapshot, exact row count, order, every returned `VId` | physical plan, cost, authorization, envelope framing |
| staged-effect digest | transaction basis and canonical staged `LogicalDeltaTemplate`, or explicit empty overlay | durable snapshot, staged bytes as payload, API-call history |
| `GqlOverlayResultCertificate` | basis, plan digest, staged-effect digest, exact row count, order, every returned `VId` | durable/staged payloads, conflict state, standalone replay |
| query-execution stats | admitted table count and final row count | cryptographic attestation, I/O/allocation cost, operator work, wall time |

The v2 plan transcript includes `BoundPlan::neq`, omitted by historical v1. New certificates use v2; legacy-v1 verification is explicit.

### Exact staged-overlay result evidence

`WriteTxn::staged_effect_digest` binds the basis and canonical semantic net effect under `fgdb:write-txn-staged-effect:v1`.

`execute_prepared_query_with_overlay_result_certificate` executes first and then issues exact rows, a plan certificate, and `GqlOverlayResultCertificate`. The result certificate binds basis, plan digest, staged-effect digest, row count, order, and every returned `VId`.

`verifies_prepared_query_overlay_result` derives the current plan and staged-effect identities from the live transaction. A later staged mutation invalidates old evidence. Equivalent transactions at the same basis with the same canonical net effect can verify the same certificate.

This is exact in-process evidence. It is not standalone replay because the certificate does not carry the durable snapshot, staged template bytes, graph rows, read-set state, or conflict state.

## Strict evidence envelopes

The live tree has two canonical **unreleased application envelopes**:

- `GqlPreparedResultArtifact` for one durable prepared-query result;
- `GqlOverlayResultArtifact` for one staged-overlay result.

The common v1 framing is owned by `crates/fgdb-gql/src/evidence_artifact.rs` and contains:

- eight-byte magic `FGQEVID1`;
- explicit major/minor version;
- a closed artifact-kind tag;
- zero-required reserved bytes;
- exact snapshot sequence or transaction basis;
- statement, canonical bind, and plan digests;
- staged-effect digest for overlay artifacts;
- exact row count and ordered `VId` bytes;
- the applicable exact result digest.

Decoding fails closed on invalid magic, unsupported versions, wrong kind, nonzero reserved bytes, row-count or length overflow, every truncated prefix, trailing bytes, and an inconsistent result transcript. Artifact fields are private and rows are redacted from `Debug` output.

### Resource-safe evidence admission

`GqlEvidenceLimits` is policy applied before untrusted artifacts allocate their row vectors. It bounds:

- total encoded bytes;
- declared row count.

`GqlEvidenceLimits::DEFAULT_UNTRUSTED` is an application default, not a format maximum or product SLO. Callers may supply a different policy. Exact limits succeed; one-below limits return `GqlEvidenceLimitExceeded` with the dimension, configured limit, and observed value.

Malformed headers retain the strict decoder's existing syntax errors. A valid header with a hostile declared row count is refused before row allocation.

Canonical owner: `crates/fgdb-gql/src/evidence_limits.rs`. Product adapters live in `crates/fgdb/src/write_txn_parts/evidence_limits.rs`.

### Durable issuance and audit

`Database` and `EmbeddedReadView` expose:

- `execute_prepared_query_artifact[_at]`;
- `audit_prepared_query_artifact`;
- `audit_untrusted_prepared_query_artifact`;
- `audit_prepared_query_artifact_with_limits`.

A resource-safe durable audit:

1. checks encoded-byte and declared-row policy;
2. strictly decodes the envelope;
3. verifies the retained statement and canonical bind;
4. recomputes the canonical plan certificate at the artifact sequence;
5. cross-checks the independently decoded result transcript against that certificate;
6. re-executes the query at the exact historical sequence;
7. requires exact ordered-row equality.

A later write may advance the live frontier without invalidating an older artifact: audit reopens the sequence named by the artifact rather than sampling current state.

### Staged-overlay issuance and audit

`WriteTxn` exposes equivalent issuance and resource-safe audit for `GqlOverlayResultArtifact`. A staged audit additionally verifies the current transaction basis and canonical staged-effect digest before re-executing the overlay. A later staged mutation returns `StagedEffectMismatch`.

## Result-bound evidence paging

The current tree supports deterministic paging over an **already materialized and audited** evidence artifact.

### Token identity and framing

`GqlEvidencePageToken` has a fixed-width v1 encoding owned by `crates/fgdb-gql/src/evidence_page.rs`:

```text
magic[8] = "FGQPAGE1"
version_major: u16be = 1
version_minor: u16be = 0
kind: u8
reserved[3] = 0
sequence_or_basis: u64be
result_digest: [u8; 32]
next_offset: u64be
checksum: [u8; 32]
```

The token binds:

- artifact kind;
- exact snapshot sequence or transaction basis;
- complete ordered-result digest;
- next row offset.

The checksum is unkeyed. It detects accidental or unsophisticated mutation but is **not** a MAC, signature, authorization decision, capability, or publisher-authenticity proof.

Strict token decoding rejects wrong length, trailing bytes, invalid magic, unsupported version, unknown kind, nonzero reserved bytes, and checksum mismatch. Unit tests walk every truncated prefix.

### Page object

`GqlEvidencePage` contains one contiguous row slice and explicit progress metadata:

- `start_offset`;
- `end_offset`;
- `total_rows`;
- `remaining_rows`;
- optional next token;
- terminal status.

Rows are redacted from ordinary `Debug` output.

### Product-level audit-and-page order

`Database`, `EmbeddedReadView`, and `WriteTxn` expose default-untrusted and caller-limited audit-and-page methods. Their order is deliberate:

1. reject zero page size;
2. strictly decode and checksum-check optional fixed-width token bytes;
3. enforce artifact byte and declared-row policy before row allocation;
4. strictly decode and verify the artifact transcript;
5. verify prepared input, plan, snapshot/basis, and staged effect where applicable;
6. re-execute the exact historical or staged query and compare ordered rows;
7. bind the token to kind, sequence/basis, result digest, and offset;
8. return the contiguous slice.

This avoids expensive replay for an intrinsically invalid page request while preventing a valid token from bypassing artifact admission or replay.

### Exact paging boundary

This is stateless resumability over materialized evidence. It is **not**:

- a database cursor;
- operator streaming;
- bounded-buffer flow control;
- backpressure;
- a session or lease;
- cancellation-aware incremental execution;
- authentication or authorization;
- a reduction in full artifact decode/replay cost.

Every product-level page call re-audits and replays the complete artifact before returning its slice. A genuine cursor requires explicit owner/session identity, lease and cancellation semantics, bounded buffers, backpressure, and an execution/storage path that can stop before materializing the full result.

## Agent-operability verticals

### Exact-SHA local context capsules

`scripts/agent_context.sh` emits a credential-free advisory package containing an exact commit/tree, one-head Git bundle, deterministic source archive, history and tracked-file inventories, stable clean/dirty worktree evidence, and truthful Beads observation modes.

The v2 verifier imports the bundle into an isolated repository and recomputes the tree, archive, paths, and recent history. `agent_context_checkout.sh` materializes a verified detached checkout with no remote.

### Exact-tree local proof bundles

`scripts/local_proof.sh` captures `bash scripts/check.sh` on one clean exact tree and preserves the raw exit, stdout/stderr, tool versions, committed tree, tracked gate-driver blob, and before/after worktree state.

Verdicts remain three-valued: stable `pass`, stable `red`, and moving-tree `void`.

## Agent source-ownership map

| Concern | Canonical owner |
|---|---|
| durable database composition and temporal graph reads | `crates/fgdb/src/lib.rs` |
| one exact-sequence GQL read kernel | `crates/fgdb/src/gql_exec.rs` |
| parser, binder, `RelationBind`, `BoundPlan` | `crates/fgdb-gql/src/parser.rs` |
| `PreparedGqlQuery` and query-execution budgets | `crates/fgdb-gql/src/prepared.rs` |
| staged-result transcript | `crates/fgdb-gql/src/overlay_evidence.rs` |
| strict evidence framing and independent decoder | `crates/fgdb-gql/src/evidence_artifact.rs` |
| evidence byte/row admission | `crates/fgdb-gql/src/evidence_limits.rs` |
| result-bound page tokens and slices | `crates/fgdb-gql/src/evidence_page.rs` |
| owned prepared execution adapters | `crates/fgdb/src/write_txn_parts/owned_prepared.rs` |
| canonical staged-effect authority | `crates/fgdb/src/write_txn_parts/overlay_evidence.rs` |
| artifact issuance and replay audit | `crates/fgdb/src/write_txn_parts/portable_evidence.rs` |
| artifact resource admission adapters | `crates/fgdb/src/write_txn_parts/evidence_limits.rs` |
| audit-and-page adapters | `crates/fgdb/src/write_txn_parts/evidence_page.rs` |
| durable certificate authority | `crates/fgdb/src/gql_cert.rs` |
| staged transaction overlay and conflict tracking | `crates/fgdb/src/write_txn_parts/` |
| independent semantic oracle | `crates/fgdb-reference` |
| crash/differential harness | `crates/fgdb-sim` |

Do not add a second parser, binder, execution kernel, session type, or issuing certificate authority merely to make a narrow test pass. Independent artifact decoding is allowed only while product audit cross-checks the canonical certificate transcript, as the current path does.

## Focused witnesses and tests

```bash
cargo run -p fgdb --example open_a_database
cargo run -p fgdb --example gql_owned_prepared
cargo run -p fgdb --example gql_evidence_artifact
cargo run -p fgdb --example gql_evidence_limits
cargo run -p fgdb --example gql_evidence_pages
cargo run -p fgdb --example gql_txn_overlay_result_evidence
```

Focused integration suites include:

- `embedded_read_session.rs`;
- `gql_prepared_at.rs`;
- `gql_plan_certificate_refusals.rs`;
- `gql_aligned_certificates.rs`;
- `gql_result_digest.rs`;
- `gql_owned_prepared.rs`;
- `gql_txn_overlay_result_evidence.rs`;
- `gql_evidence_artifact.rs`;
- `gql_evidence_limits.rs`;
- `gql_evidence_pages.rs`.

## Validation status

The repository-wide authority remains:

```bash
bash scripts/check.sh
```

The connector environment used for the 2026-09-02 paging continuation did not contain the repository-pinned Rust toolchain, `shellcheck`, or a runnable UBS installation. Hosted GitHub Actions were intentionally excluded from evidence. Checked-in tests state intended laws; only an executed exact-tree proof establishes that the complete tree compiles and passes every registered gate.

The paging change received focused mechanical checks for exact blob identity, fast-forward history, module/include closure, delimiter balance, whitespace, line width, fixed-width accounting, checked offset arithmetic, every-prefix truncation coverage, reserved/trailing-byte refusal, row redaction, checksum mutation, cross-kind/snapshot/result rejection, resource-admission nesting, and request-preflight order. This document does not promote those checks into a whole-tree green verdict.

## Major remaining systems

The following remain incomplete or absent:

- typed prepared-statement parameters and canonical parameter evidence;
- typed rows/columns and genuine streaming cursors with backpressure;
- full session ownership, authorization, renew/expiry/reattach, and synchronous facade;
- full SSI, predicate/range conflicts, merge ladder, and general multi-relation transactions;
- a registered, compatibility-governed evidence format and external verifier SDK;
- standalone staged-overlay replay carrying the staged template and snapshot authority;
- storage/operator-level early resource enforcement;
- full ISO GQL, GLA lowering, Loom operators, optimizer, spill, and larger-than-memory execution;
- Strata tiers I/R/A and production compaction/migration policy;
- Ripple, Beacon, Prism, Warden, Fabric, and Aegis;
- CLI/robot mode, server, Python bindings, packaging, signed releases, installer, and upgrade tooling.

## Dependency-ordered next work

1. Run the pinned toolchain and preserve the exact-tree verdict with `scripts/local_proof.sh`.
2. Add typed parameters as structural parser operands; never use string substitution.
3. Bind canonical parameter values into preparation identity, certificates, and evidence envelopes.
4. Decide whether the application envelope and page token graduate into registered compatibility contracts; freeze ceilings and golden vectors first.
5. Build a real cursor/session owner with cancellation, lease, bounded buffering, and backpressure.
6. Move resource enforcement into storage/operator admission before full table/result materialization.
7. Package staged template bytes and exact snapshot authority before claiming standalone transaction replay.
8. Continue into full SSI and GLA/Loom lowering.

## Recent evolution

- `92e39e59` — one exact-sequence execution kernel for database and immutable read views.
- `01f28d39` through `65d3d832` — verifiable input/plan/result evidence.
- `005a8397` — prepared transaction-overlay execution without rebinding.
- `c7a0558e` through `78137cfb` — owned preparation and deterministic query budgets.
- `80de85a6` through `3e8ff789` — exact staged-effect and staged-result evidence.
- `97d09787` through `10871100` — strict application envelopes and replay audit.
- `a3441930` through `f004bd1c` — pre-allocation evidence byte/row admission.
- `901e4ef5` through `49adac5f` — result-bound stateless paging, audit adapters, progress metadata, and request-preflight laws.

Keep this file synchronized whenever a capability crosses from plan or red-bar acceptance test into an inhabitable public path.
