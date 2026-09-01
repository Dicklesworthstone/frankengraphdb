# Bounded GQL Result Evidence

Status: live embedded subset on unreleased `main` as of the 2026-09-01 result-evidence continuation.

This document names the exact evidence stack implemented by the bounded GQL surface. It is deliberately narrower than the finished certificate and external-verifier architecture in the comprehensive plan.

## The three evidence layers

A successful bounded GQL execution can now produce three aligned layers:

1. **`GqlCertificate`** binds the exact statement bytes, canonical `RelationBind`, and MVCC snapshot sequence.
2. **`GqlPlanCertificate`** binds every current field of the executor-ready `BoundPlan` and the same snapshot sequence under the v2 plan transcript.
3. **Ordered-result digest** binds the plan-certificate digest, its snapshot sequence, the exact result-row count, and every returned `VId` in order under `fgdb:gql-ordered-result-digest:v1`.

The result digest is intentionally chained to the plan certificate rather than only to raw rows. Equal rows produced by different plans or snapshots therefore do not share result identity.

## Live APIs

Text execution:

```rust
let (rows, input, plan, result) =
    db.execute_gql_with_result_digest(statement, &bind)?;
assert!(input.verifies_at(statement, &bind, plan.snapshot_seq));
assert!(plan.verifies_result_digest(&rows, result));
```

Historical text execution uses `execute_gql_with_result_digest_at`. The same live and historical pair exists on `EmbeddedReadView`.

Already-bound execution:

```rust
let plan = db.prepare_gql_plan(statement, &bind)?;
let (rows, plan_certificate, result) =
    db.execute_prepared_gql_with_result_digest(&plan)?;
assert!(plan_certificate.verifies_result_digest(&rows, result));
```

Historical prepared execution uses `execute_prepared_gql_with_result_digest_at`. The same pair exists on `EmbeddedReadView`.

Every API executes through the existing exact-sequence GQL kernel. Evidence is computed only after the read succeeds. A parse, bind, fenced-handle, or beyond-frontier refusal returns no evidence tuple.

## Transcript definition

The v1 ordered-result transcript is, in order:

```text
"fgdb:gql-ordered-result-digest:v1"
plan_certificate.digest[32]
plan_certificate.snapshot_seq as u64 big-endian
row_count as u64 big-endian
for each row in result order:
    VId as u64 big-endian
```

Consequences:

- changing one row changes the digest;
- reordering rows changes the digest;
- truncating or extending the result changes the digest;
- changing the bound plan changes the digest through the plan-certificate link;
- changing the snapshot changes both the plan certificate and the explicit result transcript field;
- an empty result has a stable identity distinct from every nonempty result.

Final digest comparison uses the same constant-work byte-accumulation helper as the input and plan verifiers.

## Evidence boundary

The ordered-result digest **does attest**:

- one complete `BoundPlan` certificate;
- one exact MVCC snapshot;
- exact row count;
- exact row order;
- every returned vertex identifier.

It **does not attest**:

- a physical operator tree or optimizer decision;
- execution cost, elapsed time, allocation, spill, or resource consumption;
- transaction-overlay state;
- authorization or secure-view predicates;
- catalog epoch or prepared-statement invalidation;
- a portable, self-describing, durable replay artifact;
- server cursor, lease, streaming, or backpressure semantics.

The result is presently a domain-separated `Digest` returned beside its plan certificate. A portable artifact must be introduced through the repository’s registered format constitution, with versioning, canonical encoding, strict decoding, mutation tests, and an independently usable verifier boundary. An ad hoc `to_bytes()` helper would create an unregistered durable format and is therefore not the next step.

## Tests and executable witness

Focused laws live in:

- `crates/fgdb/src/gql_cert.rs` unit tests;
- `crates/fgdb/tests/gql_result_digest.rs` integration tests;
- `crates/fgdb/examples/gql_result_digest.rs` runnable witness.

The tests cover row reorder, replacement, truncation, plan mutation, snapshot mutation, empty results, text/prepared parity, database/read-view parity, historical replay after live advancement, and future-frontier refusal.

## Next dependency-ordered work

1. Complete the transaction-overlay prepared-plan refactor so text and prepared transaction reads share one bound-plan body.
2. Introduce an owned prepared-query definition that keeps source text, canonical bind, and bound plan inseparable.
3. Add typed parameters at parser/binder level and include canonical parameter values in input, plan, and result evidence.
4. Register a portable execution-evidence format before adding persistence or external-verifier APIs.
5. Extend evidence to physical-plan and deterministic resource transcripts only after the corresponding Loom/GLA execution surfaces exist.
