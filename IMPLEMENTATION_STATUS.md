# FrankenGraphDB Implementation Status

Ground-truth snapshot: `main` at `6c5103782bfad39c8da8a02a07bcf0e68cb0a72e` on 2026-09-01 UTC.

This document is the concise, agent-facing map of **what the repository can execute now**, **what each evidence object actually proves**, and **what remains architectural target state**. The comprehensive plan remains normative for the finished system; this file records the inhabitable subset at the snapshot above.

## Product reality

FrankenGraphDB is not yet a released graph-database product. There are no tagged releases, installer, CLI binary, server binary, Python package, or compatibility promise. The working product surface is an embedded Rust composition crate over real Chronicle durability and real Strata tier-D storage, with a deliberately bounded GQL read slice and a one-batch write-transaction subset.

The most important distinction is:

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

| Surface | Live/pinned | Exact historical sequence | Plan certificate |
|---|---:|---:|---:|
| `Database` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_certified[_at]` |
| `EmbeddedReadView` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_certified[_at]` |

The exact-sequence methods execute first and mint evidence only after a successful read. A typed refusal therefore returns no certificate.

`BoundPlan` is the honest prepared form of the current bounded grammar. It is not yet a parameterized `PreparedStatement`: statement parameters, typed parameter schemas, catalog epochs, authorization context, physical-plan selection, result cursors, and invalidation policy remain open.

### Bounded write transaction

`WriteTxn` pins one basis sequence, overlays staged vertex and edge mutations for read-your-own-writes behavior, records read dependencies, and commits one prepared same-relation batch through the production FCW seam. It is deliberately not full SSI and must not be described as the finished transaction model.

The source owner is `crates/fgdb/src/write_txn.rs`.

## Evidence and certificate truth table

The current certificate types are useful only when their claim boundaries remain explicit.

| Evidence | Binds | Does not bind |
|---|---|---|
| `GqlCertificate` | exact statement bytes, canonical `RelationBind`, snapshot sequence | parsed/bound plan, returned rows, staged transaction overlay, physical plan, runtime cost |
| `GqlPlanCertificate` | every current `BoundPlan` field and snapshot sequence under transcript v2 | statement spelling, returned rows, staged transaction overlay, physical plan, runtime cost |
| `execute_gql_with_certificates[_at]` | returns both certificate layers from one bind and one successful execution at one sequence | result-row attestation |

The v2 plan transcript includes `BoundPlan::neq`, which the historical v1 transcript omitted. V1 verification is exposed only through an explicitly named legacy method; new certificates use v2.

Certificate digest comparisons use constant-work byte accumulation rather than direct early-exit equality.

There is **no result certificate yet**. Returned rows sit beside the existing certificates but are not cryptographically committed by either one. There is also no portable canonical certificate serialization or durable replay bundle in the live source at this snapshot.

## Agent source-ownership map

Use this map before editing the embedded/query vertical:

| Concern | Canonical owner |
|---|---|
| durable database composition, open/reopen, writes, temporal graph reads, `EmbeddedReadView`, text GQL entrypoints, plan-only certificate entrypoints | `crates/fgdb/src/lib.rs` |
| one shared exact-sequence GQL execution kernel; prepared-plan and read-session execution methods | `crates/fgdb/src/gql_exec.rs` |
| input/plan certificate transcripts, verification, and aligned dual-certificate issuance | `crates/fgdb/src/gql_cert.rs` |
| staged write overlay, read-set tracking, one-batch transaction commit | `crates/fgdb/src/write_txn.rs` |
| parser, binder, `RelationBind`, `BoundPlan` | `crates/fgdb-gql/src/lib.rs` |
| independent semantic oracle | `crates/fgdb-reference` |
| crash/differential harness | `crates/fgdb-sim` |

Do not add a second execution kernel, a second session type, or a certificate transcript in another crate merely to make a narrow test pass. Extend these owners or deliberately revise the ownership map in the same change.

## Focused executable witnesses

Useful starting points include:

- `cargo run -p fgdb --example open_a_database`
- `cargo run -p fgdb --example gql_time_travel`
- `cargo run -p fgdb --example gql_certified_query`
- `cargo run -p fgdb --example gql_prepared_read_session`
- `cargo run -p fgdb --example gql_aligned_certificates`

Focused integration suites for the newest embedded surface include:

- `crates/fgdb/tests/embedded_read_session.rs`
- `crates/fgdb/tests/gql_prepared_at.rs`
- `crates/fgdb/tests/gql_plan_certificate_at.rs`
- `crates/fgdb/tests/gql_plan_certificate_refusals.rs`
- `crates/fgdb/tests/gql_aligned_certificates.rs`

## Validation status at this snapshot

The repository-wide authority remains:

```bash
bash scripts/check.sh
```

This 2026-09-01 delta has been checked by full-file inspection, delimiter-aware Rust lexical validation, duplicate inherent-method ownership scans, call-site-to-method inventory checks, exact blob-identity checks against the committed GitHub files, and `git diff --check` in the recovered source capsule. A current Rust toolchain was not available in the connector execution environment, so this document does **not** claim a fresh compiled or whole-chain green verdict for the snapshot SHA above.

That distinction is intentional: checked-in tests describe the intended laws, but only an executed toolchain verdict proves they compile and pass.

## Major remaining systems

The following remain incomplete or absent and must not be inferred from the bounded verticals above:

- full session ownership, authorization, reattach/renew/expiry, and synchronous embedded facade;
- parameterized prepared statements, typed rows/columns, cursor and result-stream lifecycle;
- full transaction ownership and SSI, predicate/range conflict tracking, and merge ladder integration;
- full ISO GQL parsing, GLA lowering, Loom operators, optimizer, morsel-parallel execution, spill, and larger-than-memory query execution;
- Strata tiers I/R/A and their production compaction/migration policy;
- Ripple incremental views/subscriptions and recursion;
- Beacon indexing and hybrid retrieval;
- Prism/`fnx-*` integration;
- Warden capabilities and secure views;
- Fabric server protocols and Aegis operational control plane;
- CLI, robot-mode NDJSON, Python bindings, packaging, signed releases, installer, and upgrade tooling;
- result-row certificates, physical-plan/runtime-cost evidence, and portable durable replay artifacts.

## Dependency-ordered next work

The most accretive near-term sequence is:

1. make the current embedded prepared/session/evidence slice compile-clean under the pinned nightly and close its focused Beads only with executed evidence;
2. introduce an owned prepared-query definition that keeps statement bytes, canonical bind, and `BoundPlan` inseparable, without pretending it is the final parameterized statement protocol;
3. define and version a canonical result-evidence transcript before claiming row attestation or portable replay;
4. extend the one-kernel discipline into the transaction overlay so text and prepared transaction reads bind once and share one body;
5. then advance the larger W10 session/transaction contract rather than widening the bounded grammar opportunistically.

## Recent evolution

The current embedded/evidence wave is anchored by:

- `92e39e59` — one snapshot execution kernel for database and immutable read views;
- `617bd687`, `2944eba8`, `07352733`, `b91031e8` — initial pinned sessions and reusable plans;
- `74dc4fe4`, `b276e7ec`, `f4f40d61` — exact historical prepared execution and certification across sessions;
- `01f28d39`, `4e05d862`, `219d0819`, `3862f9bf`, `f4a617c4` — externally verifiable v2 plan/input certificates and corrected no-claim language;
- `051b90ba`, `950d7ebe`, `6c510378` — aligned input-plus-plan evidence from one successful execution.

Keep this file synchronized when a capability crosses from design or red-bar acceptance test into an inhabitable public path.
