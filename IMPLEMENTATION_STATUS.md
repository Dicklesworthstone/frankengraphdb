# FrankenGraphDB Implementation Status

Capability baseline: unreleased `main` through the ordered-result-evidence code commit `65d3d8323432ced15e9255388f8b74e00fae1b5f`, with documentation synchronized on 2026-09-01 UTC.

This is the concise, agent-facing map of **what the repository can execute now**, **what each evidence object actually proves**, and **what remains architectural target state**. The comprehensive plan remains normative for the finished system; this file records the inhabitable subset.

## Product reality

FrankenGraphDB is not yet a released graph-database product. There are no tagged releases, installer, CLI binary, server binary, Python package, or compatibility promise. The working product surface is an embedded Rust composition crate over real Chronicle durability and real Strata tier-D storage, with a deliberately bounded GQL read slice and a one-batch write-transaction subset.

The important distinction is:

- the repository already contains real durable mechanisms and executable cross-layer paths;
- it does **not** yet contain the full W10 product surface, the full GQL algebra/planner/executor, or the complete distributed and incremental system described by the plan.

## Live verticals

### Durable embedded database

`fgdb::Database` can:

- create and reopen a database through the real Chronicle two-fsync commit path;
- recover the authoritative marker chain and authenticate checkpoint-selected Strata roots against it;
- commit typed `WriteBatch` mutations under the production first-committer-wins validator;
- read vertices, edges, adjacency, and complete vertex/edge collections at the live frontier or an exact retained `CommitSeq`;
- run over the ordinary filesystem VFS or the in-memory VFS without substituting an in-memory graph model for the durable composition path.

The source owner is `crates/fgdb/src/lib.rs`. The simplest executable witness is `crates/fgdb/examples/open_a_database.rs`.

### Bounded GQL execution

The live grammar is intentionally smaller than ISO GQL. It includes a deterministic subset of:

- labeled node scans;
- directed, incoming, and undirected one-hop patterns;
- bounded two-hop patterns;
- selected equality, inequality, and integer-property predicates;
- deterministic projection, `SKIP`, and `LIMIT`.

Parsing and binding live in `crates/fgdb-gql/src/lib.rs`. Snapshot execution lives in `crates/fgdb/src/gql_exec.rs`. Live database reads, historical reads, and immutable read-session reads share one exact-sequence execution kernel; they do not maintain independent semantic implementations.

### Pinned embedded read sessions

`Database::read_session()` returns an immutable `EmbeddedReadView` owning one decoded generation. A session:

- keeps its frontier, manifest, partition root, and decoded graph state stable after later database writes;
- can be cloned while sharing the same immutable decoded generation;
- can execute text or prepared GQL at its pinned frontier;
- can execute older retained sequences at or below its own frontier;
- refuses a later sequence through the existing typed `ReadError::BeyondFrontier` path.

This is a real read-session subset, not the final W10 session protocol. It has no authorization negotiation, lease/reattach protocol, parameter schema, cursor lifecycle, server transport, or synchronous facade.

### Reusable prepared plans

`Database::prepare_gql_plan` and `EmbeddedReadView::prepare_gql_plan` expose the immutable executor-ready `BoundPlan`. The same plan can be reused without reparsing or rebinding through:

| Surface | Live/pinned | Exact historical sequence | Plan + result evidence |
|---|---:|---:|---:|
| `Database` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_with_result_digest[_at]` |
| `EmbeddedReadView` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_with_result_digest[_at]` |

The exact-sequence methods execute first and mint evidence only after a successful read. A typed refusal therefore returns no evidence.

`BoundPlan` is the honest prepared form of the current bounded grammar. It is not yet a parameterized `PreparedStatement`: statement parameters, typed parameter schemas, catalog epochs, authorization context, physical-plan selection, result cursors, and invalidation policy remain open.

### Bounded write transaction

`WriteTxn` pins one basis sequence, overlays staged vertex and edge mutations for read-your-own-writes behavior, records read dependencies, and commits one prepared same-relation batch through the production FCW seam. It is deliberately not full SSI and must not be described as the finished transaction model.

The source owner is `crates/fgdb/src/write_txn.rs`. Its text and certified GQL paths still need to be refactored so an already-bound plan can execute directly over the overlay without rebinding.

## Evidence truth table

| Evidence | Binds | Does not bind |
|---|---|---|
| `GqlCertificate` | exact statement bytes, canonical `RelationBind`, snapshot sequence | parsed/bound plan, returned rows, transaction overlay, physical plan, runtime cost |
| `GqlPlanCertificate` | every current `BoundPlan` field and snapshot sequence under transcript v2 | statement spelling, returned rows, transaction overlay, physical plan, runtime cost |
| ordered-result digest | plan-certificate digest, snapshot, exact row count, row order, every returned `VId` | physical plan, cost, authorization, transaction overlay, portable artifact framing |
| `execute_gql_with_result_digest[_at]` | returns all three layers from one bind and one successful execution | portable replay bundle or external-verifier protocol |

The v2 plan transcript includes `BoundPlan::neq`, which the historical v1 transcript omitted. V1 verification is exposed only through an explicitly named legacy method; new certificates use v2.

The ordered-result transcript uses the domain `fgdb:gql-ordered-result-digest:v1` and chains through the plan-certificate digest. Equal rows under different plans or snapshots therefore have different identities. Result evidence is available for text and already-bound execution on both `Database` and `EmbeddedReadView`, live or historical.

Certificate digest comparisons use constant-work byte accumulation rather than direct early-exit equality.

There is still **no portable, self-describing execution artifact**. The result digest is returned beside its plan certificate. Persistence and external verification require a registered, versioned canonical format under the repository’s format constitution; an ad hoc serialization helper would be an unregistered durable format.

The detailed contract is in `docs/GQL_RESULT_EVIDENCE.md`.

## Agent source-ownership map

| Concern | Canonical owner |
|---|---|
| durable database composition, open/reopen, writes, temporal graph reads, `EmbeddedReadView`, text GQL entrypoints, plan-only certificate entrypoints | `crates/fgdb/src/lib.rs` |
| one shared exact-sequence GQL execution kernel; prepared-plan and read-session execution methods | `crates/fgdb/src/gql_exec.rs` |
| input/plan transcripts, ordered-result digest, verification, and aligned execution evidence | `crates/fgdb/src/gql_cert.rs` |
| staged write overlay, read-set tracking, one-batch transaction commit | `crates/fgdb/src/write_txn.rs` |
| parser, binder, `RelationBind`, `BoundPlan` | `crates/fgdb-gql/src/lib.rs` |
| independent semantic oracle | `crates/fgdb-reference` |
| crash/differential harness | `crates/fgdb-sim` |

Do not add a second execution kernel, session type, or evidence transcript in another crate merely to make a narrow test pass. Extend these owners or deliberately revise the ownership map in the same change.

## Focused executable witnesses

- `cargo run -p fgdb --example open_a_database`
- `cargo run -p fgdb --example gql_time_travel`
- `cargo run -p fgdb --example gql_certified_query`
- `cargo run -p fgdb --example gql_prepared_read_session`
- `cargo run -p fgdb --example gql_aligned_certificates`
- `cargo run -p fgdb --example gql_result_digest`

Focused integration suites for the newest embedded surface include:

- `crates/fgdb/tests/embedded_read_session.rs`
- `crates/fgdb/tests/gql_prepared_at.rs`
- `crates/fgdb/tests/gql_plan_certificate_at.rs`
- `crates/fgdb/tests/gql_plan_certificate_refusals.rs`
- `crates/fgdb/tests/gql_aligned_certificates.rs`
- `crates/fgdb/tests/gql_result_digest.rs`

## Validation status

The repository-wide authority remains:

```bash
bash scripts/check.sh
```

For the ordered-result-evidence continuation, the committed blobs were checked for exact source identity, balanced Rust delimiters, method-name uniqueness, transcript-field order, and whitespace/diff integrity. The connector execution environment does not contain the repository-pinned Rust toolchain, so this status document does **not** claim a fresh compiled, Clippy, test, or whole-chain green verdict for the new commits.

Checked-in tests describe the intended laws; only an executed pinned-toolchain verdict proves they compile and pass.

## Major remaining systems

The following remain incomplete or absent and must not be inferred from the bounded verticals above:

- full session ownership, authorization, reattach/renew/expiry, and synchronous embedded facade;
- parameterized prepared statements, typed rows/columns, cursor and result-stream lifecycle;
- transaction prepared-plan execution, full transaction ownership and SSI, predicate/range conflict tracking, and merge-ladder integration;
- full ISO GQL parsing, GLA lowering, Loom operators, optimizer, morsel-parallel execution, spill, and larger-than-memory query execution;
- Strata tiers I/R/A and their production compaction/migration policy;
- Ripple incremental views/subscriptions and recursion;
- Beacon indexing and hybrid retrieval;
- Prism/`fnx-*` integration;
- Warden capabilities and secure views;
- Fabric server protocols and Aegis operational control plane;
- CLI, robot-mode NDJSON, Python bindings, packaging, signed releases, installer, and upgrade tooling;
- registered portable execution artifacts, physical-plan evidence, and deterministic runtime-cost evidence.

## Dependency-ordered next work

1. Execute the pinned toolchain locally and fix any formatting, compiler, Clippy, or focused-test failures in the current prepared/session/evidence slice.
2. Refactor `WriteTxn` so text, already-bound, and certified overlay reads bind once and share one body.
3. Introduce an owned prepared-query definition that keeps statement bytes, canonical bind, and `BoundPlan` inseparable.
4. Add typed statement parameters in the parser/binder and bind canonical parameter values into every evidence layer.
5. Register a portable execution-evidence format before adding persistence or external-verifier APIs.
6. Advance the larger W10 session/transaction contract rather than widening the bounded grammar opportunistically.

## Recent evolution

- `92e39e59` — one snapshot execution kernel for database and immutable read views;
- `617bd687`, `2944eba8`, `07352733`, `b91031e8` — initial pinned sessions and reusable plans;
- `74dc4fe4`, `b276e7ec`, `f4f40d61` — exact historical prepared execution and certification across sessions;
- `01f28d39`, `4e05d862`, `219d0819`, `3862f9bf`, `f4a617c4` — externally verifiable v2 plan/input certificates and corrected no-claim language;
- `051b90ba`, `950d7ebe`, `6c510378` — aligned input-plus-plan evidence from one successful execution;
- `1175b620`, `a3714204`, `11b84204`, `2619a17f`, `65d3d832` — exact ordered-row digest, historical replay laws, executable witness, and prepared-plan parity.

Keep this file synchronized when a capability crosses from design or red-bar acceptance test into an inhabitable public path.
