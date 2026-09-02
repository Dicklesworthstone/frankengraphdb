# Bounded GQL Result Evidence

Status: **live bounded embedded evidence with an unreleased application envelope**, not the final physical-plan, wire, durable-object, or external-verifier protocol.

This document defines what the current input, plan, durable-result, staged-effect, staged-result, deterministic-budget, and evidence-envelope values prove. It also states what they deliberately do not prove.

## Durable-read evidence layers

### Input certificate

`GqlCertificate` binds:

- exact statement bytes;
- `RelationBind::canonical_bytes()`;
- one exact `CommitSeq`.

It does not bind the parsed plan or returned rows. Verification uses constant work over final digest comparisons.

### Plan certificate

`GqlPlanCertificate` binds:

- every current `BoundPlan` field;
- one exact `CommitSeq`;
- the v2 plan-transcript domain.

V2 includes `BoundPlan::neq`, which the historical v1 transcript omitted. Legacy verification is explicitly named; new evidence always uses v2.

The plan certificate does not bind statement spelling. Two statements that bind to an identical plan at an identical snapshot have the same plan identity.

### Exact ordered-result digest

The durable-read result transcript uses:

```text
fgdb:gql-ordered-result-digest:v1
```

It binds:

- complete plan-certificate digest;
- snapshot sequence;
- exact row count;
- row order;
- every returned `VId`.

Equal row sets in a different order do not verify. Equal rows under a different plan or snapshot have a different identity.

## Coherent owned preparation

`PreparedGqlQuery` owns exact statement bytes, the canonical name-binding map, and the derived plan as one immutable definition. Its fields are private. A caller cannot change the original statement or bind map after preparation and thereby alter the retained query.

`verifies_definition()` reparses and rebinds the retained inputs as an explicit audit. Normal execution uses the retained plan directly and does not reparse or rebind.

For `Database` and `EmbeddedReadView`, `execute_prepared_query_with_result_digest[_at]` returns:

1. rows;
2. input certificate from the retained statement and bind;
3. plan certificate from the retained plan;
4. exact ordered-result digest from those rows and that plan certificate.

All layers name the same successful exact-sequence read. A read refusal returns no evidence tuple.

The current owned input certificate has no parameter digest because typed parameters are not yet implemented. Canonical parameter values must be bound explicitly when parameter support lands.

## Staged transaction evidence

### Canonical staged-effect digest

`WriteTxn::staged_effect_digest` uses:

```text
fgdb:write-txn-staged-effect:v1
```

It binds the transaction basis and either an explicit empty-overlay tag or the complete canonical `LogicalDeltaTemplate` retained by the prepared write.

The identity is semantic: API-call histories that normalize to the same canonical effect have the same digest. The digest does not carry the staged bytes and cannot replay them by itself.

### Exact staged-overlay result certificate

`GqlOverlayResultCertificate` uses:

```text
fgdb:gql-staged-overlay-result:v1
```

It binds:

- transaction basis;
- plan-certificate digest;
- canonical staged-effect digest;
- exact row count;
- row order;
- every returned `VId`.

`WriteTxn::execute_prepared_query_with_overlay_result_certificate` executes the ordinary overlay path first and mints evidence only after success. `verifies_prepared_query_overlay_result` recomputes the plan and staged-effect identities from the current live transaction.

A later staged mutation invalidates earlier evidence. An equivalent transaction at the same basis with the same canonical net effect can verify it. Row reorder, replacement, insertion, or deletion fails verification.

This is exact in-process evidence, not standalone replay. The certificate does not contain the durable snapshot, staged template bytes, graph rows, read set, or conflict state.

## Strict application evidence envelopes

The current tree has two self-contained framing types:

- `GqlPreparedResultArtifact` for a durable prepared-query result;
- `GqlOverlayResultArtifact` for a staged transaction-overlay result.

They share the v1 envelope prefix:

```text
magic[8] = "FGQEVID1"
version_major: u16be = 1
version_minor: u16be = 0
kind: u8
reserved[3] = 0
```

The kind-specific body then carries:

- exact snapshot sequence or transaction basis;
- statement digest;
- canonical bind digest;
- plan-certificate digest;
- staged-effect digest for overlay artifacts;
- exact row count;
- ordered big-endian `VId` values;
- the applicable result digest.

Artifact fields are private. Construction derives input identities from one `PreparedGqlQuery`; overlay construction also derives its result digest through `GqlOverlayResultCertificate`. Ordinary `Debug` output redacts rows.

### Strict decoder behavior

`from_bytes` refuses:

- truncated headers or bodies;
- invalid magic;
- unsupported major or minor versions;
- the wrong closed artifact kind;
- nonzero reserved bytes;
- row counts that do not fit the platform;
- arithmetic length overflow;
- a result transcript inconsistent with the included rows and context;
- any trailing bytes.

The tests walk every truncated prefix, not only selected cut points.

The durable artifact decoder independently recomputes the frozen v1 ordered-result transcript. Product audit then cross-checks that result digest against the canonical `GqlPlanCertificate` verifier. This deliberately gives the portable decoder an independent implementation while preventing the two implementations from drifting silently.

### Durable issuance and audit

`Database` and `EmbeddedReadView` expose:

- `execute_prepared_query_artifact`;
- `execute_prepared_query_artifact_at`;
- `audit_prepared_query_artifact`.

Audit proceeds in this order:

1. strict decode;
2. exact retained statement and bind verification;
3. canonical plan-certificate recomputation at the artifact sequence;
4. canonical product result-digest verification;
5. exact-sequence query re-execution;
6. ordered-row equality.

The ordering keeps refusal classes legible: malformed bytes, wrong preparation, wrong plan, execution failure, and replay mismatch remain distinct.

An artifact issued at an older sequence remains auditable after later writes advance the live database because replay uses the sequence in the artifact. An immutable read view can audit only sequences retained by its own pinned generation.

### Staged-overlay issuance and audit

`WriteTxn` exposes:

- `execute_prepared_query_overlay_artifact`;
- `audit_prepared_query_overlay_artifact`.

Transaction audit verifies, in order:

1. strict decode;
2. exact transaction basis;
3. retained statement and bind;
4. plan certificate at the basis;
5. current canonical staged-effect digest;
6. exact overlay re-execution and ordered rows.

Staging another mutation after issuance causes `StagedEffectMismatch` before the old rows can be accepted against the new overlay.

### Envelope status and compatibility boundary

The v1 framing is deterministic and endian-stable, but it is an **unreleased application artifact**. It is not currently registered as:

- an Appendix-A logical object;
- a bootstrap or pre-bootstrap frame;
- an FGP wire type;
- a signed evidence object;
- a long-term compatibility contract.

This distinction is load-bearing. The repository may revise or replace the framing before release without carrying a compatibility shim. Promotion requires an explicit registry/constitution decision, format size ceilings, frozen golden vectors, decoder compatibility rules, and a separate publisher-authenticity mechanism where provenance matters.

The staged artifact remains an identity-and-row envelope. It does not carry the staged template or durable snapshot material needed for standalone transaction replay.

Runnable witness:

```bash
cargo run -p fgdb --example gql_evidence_artifact
```

## Deterministic execution budgets

`GqlExecutionBudget` is adjacent execution metadata, not cryptographic evidence.

Current dimensions:

- `SnapshotRecords`: complete immutable vertex table admitted by a node scan or edge table admitted by an edge pattern;
- `ResultRows`: final deterministic rows after filtering, projection, sorting, deduplication, `SKIP`, and `LIMIT`.

Exact limits succeed. Exceeded limits return a typed `GqlBudgetExceeded` naming the dimension, configured limit, and observed count. No partial rows escape. Successful calls return `GqlExecutionStats`.

Budget configuration and observed stats are not included in the input, plan, durable-result, staged-effect, staged-result, or artifact transcripts. The evidence stack therefore does not attest that a particular budget was requested or consumed.

The executor currently materializes and counts the relevant table before ordinary execution reads it. These bounds do not prove wall-clock time, allocations, bytes read, operator work, spill behavior, cancellation latency, or physical cost.

## Mutation-sensitive laws

The focused tests require that:

- statement-byte drift invalidates input evidence;
- bind drift invalidates input evidence;
- any `BoundPlan` field mutation changes the v2 plan certificate;
- snapshot drift invalidates input and plan evidence;
- row replacement, deletion, insertion, or reorder invalidates result evidence;
- caller mutation after preparation does not change the owned definition;
- database and immutable-view execution agree at one exact sequence;
- exact budget boundaries succeed and one-below boundaries refuse;
- node scans count admitted vertices while edge patterns count admitted edges;
- identical canonical staged effects verify across transactions;
- any later staged effect invalidates prior staged-result evidence;
- every truncated artifact prefix refuses;
- magic, version, kind, reserved-byte, row-byte, and trailing-byte mutations refuse;
- a durable artifact audits historically after the live frontier advances;
- product audit cross-checks the independent artifact transcript against the canonical plan certificate.

## No-claim boundary

The current evidence does not attest:

- physical operator tree or optimizer decision path;
- wall-clock or deterministic operator cost;
- I/O, allocation, memory, spill, or network behavior;
- authorization or capability context;
- complete ISO GQL semantics;
- publisher authenticity;
- released format compatibility;
- standalone staged-overlay replay;
- external-verifier conformance.

Those require separate registered transcripts, payload formats, signatures, and verification surfaces.
