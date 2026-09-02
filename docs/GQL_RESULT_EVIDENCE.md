# Bounded GQL Result Evidence

Status: **live bounded embedded evidence**, not the final physical-plan or external-verifier protocol.

This document defines what the current input, plan, durable-result, staged-effect, staged-result, and deterministic-budget values prove. It also states what they deliberately do not prove.

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

The digest is a transcript layer, not a self-describing portable artifact. Persistence or external verification needs a registered versioned framing under the format constitution.

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

This is exact in-process evidence, not standalone replay. The certificate does not contain the durable snapshot, staged template bytes, graph rows, or conflict state.

## Deterministic execution budgets

`GqlExecutionBudget` is adjacent execution metadata, not cryptographic evidence.

Current dimensions:

- `SnapshotRecords`: complete immutable vertex table admitted by a node scan or edge table admitted by an edge pattern;
- `ResultRows`: final deterministic rows after filtering, projection, sorting, deduplication, `SKIP`, and `LIMIT`.

Exact limits succeed. Exceeded limits return a typed `GqlBudgetExceeded` naming the dimension, configured limit, and observed count. No partial rows escape. Successful calls return `GqlExecutionStats`.

Budget configuration and observed stats are not included in the input, plan, durable-result, staged-effect, or staged-result transcripts. The evidence stack therefore does not attest that a particular budget was requested or consumed.

The executor currently materializes and counts the relevant table before ordinary execution reads it. These bounds do not prove wall-clock time, allocations, bytes read, operator work, spill behavior, cancellation latency, or physical cost.

## Historical and immutable-view equivalence

A historical `Database` call and an `EmbeddedReadView` call over the same retained sequence, prepared definition, and graph state produce equivalent rows and durable-read evidence.

An immutable view never observes later database writes. A future sequence is refused through the existing typed read error and produces no certificate.

## Mutation-sensitive laws

The focused tests require that:

- statement-byte drift invalidates input evidence;
- bind drift invalidates input evidence;
- any `BoundPlan` field mutation changes the v2 plan certificate;
- snapshot drift invalidates input and plan evidence;
- row replacement, deletion, insertion, or reorder invalidates durable-result evidence;
- caller mutation after preparation does not change the owned definition;
- database and immutable-view execution agree at one exact sequence;
- exact budget boundaries succeed and one-below boundaries refuse;
- node scans count admitted vertices while edge patterns count admitted edges;
- identical canonical staged effects verify across transactions;
- any later staged effect invalidates prior staged-result evidence.

## No-claim boundary

The current evidence does not attest:

- physical operator tree or optimizer decision path;
- wall-clock or deterministic operator cost;
- I/O, allocation, memory, spill, or network behavior;
- authorization or capability context;
- complete ISO GQL semantics;
- standalone staged-overlay replay;
- portable artifact framing or publisher authenticity.

Those require separate registered transcripts, payload formats, and verification surfaces.
