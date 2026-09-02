# Bounded GQL Result Evidence

Status: **live bounded embedded evidence with unreleased application envelopes and stateless materialized-result paging**, not the final physical-plan, wire, durable-object, cursor, or external-verifier protocol.

This document defines what each current evidence layer proves, the order in which untrusted bytes are admitted and audited, how result-bound continuation tokens work, and what remains deliberately outside the claim.

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

V2 includes `BoundPlan::neq`, which historical v1 omitted. Legacy verification is explicitly named; new evidence uses v2.

The plan certificate does not bind statement spelling. Two statements that bind to an identical plan at an identical snapshot have the same plan identity.

### Exact ordered-result digest

The durable-read result transcript uses:

```text
fgdb:gql-ordered-result-digest:v1
```

It binds:

- the complete plan-certificate digest;
- snapshot sequence;
- exact row count;
- row order;
- every returned `VId`.

Equal row sets in a different order do not verify. Equal rows under a different plan or snapshot have a different identity.

## Coherent owned preparation

`PreparedGqlQuery` owns exact statement bytes, the canonical name-binding map, and the derived plan as one immutable definition. Its fields are private. Changing the caller's original statement or bind map after preparation cannot alter the retained query.

`verifies_definition()` reparses and rebinds the retained inputs as an explicit audit. Normal execution uses the retained plan directly and does not reparse or rebind.

For `Database` and `EmbeddedReadView`, `execute_prepared_query_with_result_digest[_at]` returns:

1. exact rows;
2. input certificate from the retained statement and bind;
3. plan certificate from the retained plan;
4. ordered-result digest from those rows and that plan certificate.

All layers name the same successful exact-sequence read. A read refusal returns no evidence tuple.

Typed parameters are not yet part of the live preparation or input certificate. When parameter support lands, canonical values must be bound explicitly rather than inferred from the concrete plan alone.

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

The tree has two self-contained framing types:

- `GqlPreparedResultArtifact` for a durable prepared-query result;
- `GqlOverlayResultArtifact` for a staged transaction-overlay result.

They share the v1 prefix:

```text
magic[8] = "FGQEVID1"
version_major: u16be = 1
version_minor: u16be = 0
kind: u8
reserved[3] = 0
```

The kind-specific body carries:

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
- trailing bytes.

The tests walk every truncated prefix, not only selected cut points.

The durable artifact decoder independently recomputes the frozen ordered-result transcript. Product audit then cross-checks that result digest against the canonical `GqlPlanCertificate` verifier. This preserves an independent decoder while preventing silent drift from the issuing authority.

## Resource-safe artifact admission

`GqlEvidenceLimits` applies caller policy before untrusted bytes allocate row storage. It has two dimensions:

- `EncodedBytes`;
- `Rows`.

`GqlEvidenceLimits::DEFAULT_UNTRUSTED` is a conservative application default, not a format maximum or performance promise. Callers may provide an explicit larger or smaller policy.

For a valid header, the declared row count is read and checked before the strict decoder allocates the row vector. Malformed headers are not reclassified as resource errors; they continue to the strict decoder and retain its existing syntax taxonomy.

Exact byte and row ceilings succeed. One-below ceilings return `GqlEvidenceLimitExceeded { dimension, limit, observed }`.

Product APIs expose both default-untrusted and caller-limited audit paths across `Database`, `EmbeddedReadView`, and `WriteTxn`.

## Product-level replay audit

### Durable artifacts

A resource-safe durable audit proceeds in this order:

1. enforce encoded-byte and declared-row policy;
2. strictly decode the envelope;
3. verify exact retained statement and canonical bind;
4. recompute the canonical plan certificate at the artifact sequence;
5. cross-check the result digest against the canonical product certificate;
6. re-execute the query at the exact historical sequence;
7. require exact ordered-row equality.

An artifact issued at an older sequence remains auditable after later writes advance the live database. An immutable read view can audit only sequences retained by its pinned generation.

### Staged-overlay artifacts

A transaction audit additionally verifies:

- exact current transaction basis;
- exact current canonical staged-effect digest.

Staging another mutation after issuance causes `StagedEffectMismatch` before old rows can be accepted against the changed overlay.

## Result-bound stateless paging

### Token format

`GqlEvidencePageToken` uses the fixed-width v1 encoding:

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

The total encoded width is fixed. The token binds:

- artifact kind;
- exact snapshot sequence or transaction basis;
- complete ordered-result digest;
- next row offset.

The checksum uses the domain:

```text
fgdb:gql-evidence-page-token:v1
```

It is unkeyed. `verifies_checksum()` means only that the token bytes are internally consistent with that public checksum transcript. It does **not** establish authenticity, authorization, integrity against a malicious writer, capability possession, or publisher provenance.

Strict decoding rejects:

- every truncated prefix;
- any trailing byte;
- invalid magic;
- unsupported version;
- unknown kind;
- nonzero reserved bytes;
- checksum mismatch.

### Page semantics

`GqlEvidencePage` is one contiguous clone from an already materialized exact result. It exposes:

- artifact kind;
- snapshot sequence or transaction basis;
- complete result digest;
- start and end offsets;
- total and remaining row counts;
- page rows;
- optional next token;
- terminal status.

Rows are redacted from ordinary `Debug` output. Page size is caller policy and is intentionally not part of token identity, so a continuation may request a different positive page size. An exact end offset returns an empty terminal page; an offset past the result refuses.

### Audit-and-page order

The product adapters on `Database`, `EmbeddedReadView`, and `WriteTxn` use this order:

1. reject zero page size;
2. strictly decode and checksum-check optional token bytes;
3. enforce artifact byte and declared-row limits before row allocation;
4. strictly decode and internally verify the artifact;
5. verify prepared input, plan, snapshot/basis, and staged effect where applicable;
6. re-execute the exact historical or staged query and compare ordered rows;
7. bind the token to artifact kind, sequence/basis, result digest, and offset;
8. return the contiguous page.

Request-local syntax is intentionally preflighted before expensive replay. Token-to-result binding remains after artifact audit, so a valid token cannot bypass evidence admission or re-execution.

### Paging no-claim boundary

Evidence paging is not:

- a database cursor;
- operator streaming;
- bounded-buffer flow control;
- backpressure;
- a session or lease;
- cancellation-aware incremental execution;
- authentication or authorization;
- a reduction in full artifact decode or replay cost.

Every product-level page call re-admits, decodes, verifies, and replays the complete artifact before returning its slice. A genuine cursor requires explicit owner/session identity, renewal/expiry/cancellation semantics, bounded buffering, backpressure, and an execution/storage path that can stop before materializing the full result.

Runnable witness:

```bash
cargo run -p fgdb --example gql_evidence_pages
```

## Deterministic query-execution budgets

`GqlExecutionBudget` is adjacent execution metadata, not cryptographic evidence.

Current dimensions:

- `SnapshotRecords`: complete immutable vertex table admitted by a node scan or edge table admitted by an edge pattern;
- `ResultRows`: final rows after filtering, projection, sorting, deduplication, `SKIP`, and `LIMIT`.

Budget configuration and observed stats are not included in the input, plan, result, staged-effect, artifact, or page-token transcripts. The evidence stack therefore does not attest that a particular execution budget was requested or consumed.

The executor currently materializes and counts the relevant table before ordinary execution reads it. These bounds do not prove wall-clock time, allocations, bytes read, operator work, spill behavior, cancellation latency, or physical cost.

## Mutation-sensitive laws

The focused tests require that:

- statement or bind drift invalidates input evidence;
- any `BoundPlan` field mutation changes the current plan certificate;
- snapshot drift invalidates input and plan evidence;
- row replacement, deletion, insertion, or reorder invalidates result evidence;
- caller mutation after preparation does not change the owned definition;
- database and immutable-view execution agree at one exact sequence;
- exact execution and artifact-admission boundaries succeed and one-below boundaries refuse;
- identical canonical staged effects verify across transactions;
- a later staged effect invalidates prior staged evidence and artifacts;
- every truncated artifact and page-token prefix refuses;
- magic, version, kind, reserved-byte, row-byte, checksum, and trailing-byte mutations refuse;
- a durable artifact and token resume historically after the live frontier advances;
- tokens refuse cross-kind, cross-sequence, cross-result, and past-end use;
- invalid page size and token syntax refuse before artifact replay;
- valid tokens never bypass artifact resource admission or exact replay.

## Envelope and token compatibility boundary

The artifact and page-token framings are deterministic and endian-stable, but they are **unreleased application formats**. They are not currently registered as:

- Appendix-A logical objects;
- bootstrap or pre-bootstrap frames;
- FGP wire types;
- signed evidence objects;
- long-term compatibility contracts.

The repository may revise or replace them before release without carrying a compatibility shim. Promotion requires an explicit registry/constitution decision, stable size ceilings, frozen golden vectors, decoder compatibility rules, and a separate publisher-authenticity mechanism where provenance matters.

## General no-claim boundary

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
