# Bounded GQL Result Evidence

Status: **live bounded embedded evidence**, not the final physical-plan or external-verifier protocol.

This document defines what the current input, plan, ordered-result, owned-preparation, and deterministic-budget values prove. It also states what they deliberately do not prove.

## Evidence layers

### Input certificate

`GqlCertificate` binds:

- exact statement bytes;
- `RelationBind::canonical_bytes()`;
- one exact `CommitSeq`.

It does not bind the parsed plan or returned rows. Verification is constant-work over the final digest comparison.

### Plan certificate

`GqlPlanCertificate` binds:

- every current `BoundPlan` field;
- one exact `CommitSeq`;
- the v2 plan-transcript domain.

V2 includes `BoundPlan::neq`, which the historical v1 transcript omitted. Legacy verification is explicitly named; new evidence always uses v2.

The plan certificate does not bind statement spelling. Two statements that bind to an identical plan at an identical snapshot have the same plan identity.

### Exact ordered-result digest

The result transcript uses the domain:

```text
fgdb:gql-ordered-result-digest:v1
```

It binds:

- the complete plan-certificate digest;
- the plan certificate's snapshot sequence;
- exact row count;
- row order;
- every returned `VId`.

Equal row sets in a different order do not verify. Equal rows under a different plan or snapshot have a different identity.

The digest is a transcript layer, not a self-describing portable artifact. Persistence or external verification needs a registered versioned framing under the format constitution.

## Coherent owned preparation

`PreparedGqlQuery` owns the exact statement, canonical name-binding map, and derived plan as one immutable definition. Its fields are private. A caller cannot change the original statement or bind map after preparation and thereby alter the retained query.

`verifies_definition()` reparses and rebinds the retained inputs as an explicit audit. Normal execution uses the retained plan directly and does not reparse or rebind.

For `Database` and `EmbeddedReadView`, `execute_prepared_query_with_result_digest[_at]` returns:

1. rows;
2. an input certificate derived from the prepared query's retained statement and bind;
3. a plan certificate derived from its retained plan;
4. an exact ordered-result digest derived from those rows and that plan certificate.

All layers name the same successful exact-sequence read. A read refusal returns no evidence tuple.

The current owned input certificate has no parameter digest because typed parameters are not yet implemented. When parameters land, canonical values must be bound explicitly rather than inferred from a concrete plan alone.

## Deterministic execution budgets

`GqlExecutionBudget` is adjacent execution metadata, not cryptographic evidence.

Current dimensions:

- `SnapshotRecords`: complete immutable vertex table admitted by a node scan or edge table admitted by an edge pattern.
- `ResultRows`: final deterministic rows after filtering, projection, sorting, deduplication, `SKIP`, and `LIMIT`.

Exact limits succeed. Exceeded limits return a typed `GqlBudgetExceeded` naming the dimension, configured limit, and observed count. No partial rows escape. Successful calls return `GqlExecutionStats`.

Budget configuration and observed stats are **not** included in `GqlCertificate`, `GqlPlanCertificate`, or the ordered-result digest. Therefore the current evidence stack does not attest that a particular budget was requested or consumed.

The executor currently materializes/counts the relevant immutable table before ordinary execution reads it. These bounds do not prove wall-clock time, allocation count, bytes read, operator work, spill behavior, cancellation latency, or physical cost.

## Historical and immutable-view replay

A `Database` historical call and an `EmbeddedReadView` call over the same retained sequence, prepared definition, and graph state produce equivalent rows and evidence.

An immutable view never observes later database writes. A future sequence is refused through the existing typed read error and produces no certificate.

## Transaction-overlay boundary

`WriteTxn` can execute and plan-certify `BoundPlan` or `PreparedGqlQuery` against its durable basis plus staged read-your-own-writes state.

It does not issue the durable-read ordered-result digest for staged rows. The plan certificate binds the durable basis and plan, not:

- staged mutation order;
- staged vertex or edge contents;
- compare-and-set outcomes in the overlay;
- the exact overlay rows that produced the answer.

Until a canonical staged-overlay identity exists, describing a transaction plan certificate as result replay evidence would be incorrect.

## Mutation-sensitive laws

The focused tests require that:

- statement-byte drift invalidates input evidence;
- bind drift invalidates input evidence;
- any `BoundPlan` field mutation changes the v2 plan certificate;
- snapshot drift invalidates input and plan evidence;
- row replacement, deletion, insertion, or reorder invalidates the result digest;
- caller mutation after preparation does not change the owned definition;
- database and immutable-view execution agree at one exact sequence;
- exact budget boundaries succeed and one-below boundaries refuse;
- node scans count admitted vertices while edge patterns count admitted edges;
- transaction overlays can reuse the owned plan but do not overclaim durable result replay.

## No-claim boundary

The current evidence does not attest:

- physical operator tree or optimizer decision path;
- wall-clock or deterministic operator cost;
- I/O, allocation, memory, spill, or network behavior;
- authorization or capability context;
- complete ISO GQL semantics;
- staged transaction-overlay identity;
- portable artifact framing or publisher authenticity.

Those require separate registered transcripts and verification surfaces.
