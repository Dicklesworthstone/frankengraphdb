# Transaction-Overlay GQL Contract

Status: **live bounded read-your-own-writes query surface with exact in-process evidence, an unreleased application envelope, resource-safe audit, and stateless materialized-result paging**, not full SSI or the final session/cursor transaction protocol.

## One execution body

`WriteTxn` executes the bounded GQL subset over:

1. the transaction's pinned durable basis;
2. staged vertex and edge mutations in transaction order;
3. the shared deterministic projection, ordering, `SKIP`, and `LIMIT` discipline.

`execute_prepared_gql` is the plan-only overlay body. Text execution binds once and delegates to it. Certified text execution also binds once and delegates; it does not call another text path that binds again.

`PreparedGqlQuery` delegates to the same body through `execute_prepared_query`. There is no second transaction parser, binder, or executor.

## Source ownership

The implementation is decomposed under `crates/fgdb/src/write_txn_parts/` while compiling as one private module with one `WriteTxn` state authority.

Relevant files:

- `preamble.rs`: error and state definitions;
- `lifecycle.rs`: begin and staging;
- `vertex_reads.rs`: overlay vertex reads;
- `edge_reads.rs`: overlay edge and adjacency reads;
- `gql_entry.rs`: text and plan-only dispatch;
- `gql_node.rs`: node-scan overlay path;
- `gql_overlay_graph.rs`: staged graph materialization;
- `gql_edge_match.rs`: edge-pattern evaluation;
- `gql_api.rs`: plan certification;
- `owned_prepared.rs`: coherent preparation and deterministic query budgets;
- `overlay_evidence.rs`: canonical staged-effect identity and exact result evidence;
- `portable_evidence.rs`: application-envelope issuance and exact audit;
- `evidence_limits.rs`: byte/row admission before untrusted artifact allocation;
- `evidence_page.rs`: request preflight and audited stateless paging;
- `finish.rs`: commit, conflict checks, abort, and helpers.

Shared envelope, limit, and page-token vocabulary lives in `crates/fgdb-gql/src/`.

## Read dependencies

Transaction query execution records conservative dependencies used by the FCW read-conflict seam:

- observed vertices;
- observed edges;
- returned vertices;
- source/relation MATCH expansions that could be changed by a later inserted edge.

A later commit touching those dependencies after the pinned basis can refuse transaction commit under `FG-LAW-FCW-READ-01`.

This is a bounded FCW read-set mechanism, not full SSI predicate/range tracking.

## Owned preparation

`WriteTxn::prepare_gql_query` creates the same coherent `PreparedGqlQuery` used by durable read surfaces. It retains exact statement bytes, a cloned `RelationBind`, and the bound plan.

`execute_prepared_query` reuses that plan over staged state. Changing the caller's original statement or bind map cannot change the prepared definition.

Finished transactions refuse preparation, execution, evidence issuance, artifact audit, and page audit through `WriteTxnError::Finished` or the corresponding nested audit wrapper.

Typed statement parameters are not yet part of this surface.

## Deterministic query budgets

`execute_prepared_query_budgeted` checks:

- complete overlay edge count for an edge pattern, or complete overlay vertex count for a node scan;
- final result-row count after deterministic query semantics.

An exact boundary succeeds. An exceeded bound returns `BudgetedGqlError::Budget` with no partial rows.

Counting the overlay is itself a transaction read. A snapshot-record refusal therefore leaves the conservative read set populated, preserving conflict safety rather than pretending that an inspected overlay was never observed.

The current implementation materializes and counts the overlay before executing it again. It does not provide storage-level early cancellation, memory governance, spill, backpressure, or physical runtime-cost evidence.

## Canonical staged-effect identity

`WriteTxn::staged_effect_digest` uses:

```text
fgdb:write-txn-staged-effect:v1
```

The transcript binds:

- transaction basis;
- an explicit empty-overlay tag, or the complete canonical `LogicalDeltaTemplate` retained by the prepared write;
- canonical template byte length and bytes.

It identifies the staged **semantic net effect**. Two API-call sequences that normalize to the same canonical effect have the same identity. Incidental call history is intentionally outside the transcript.

The digest contains no staged bytes. It is an identity, not a replay package.

## Exact staged-result certificate

`execute_prepared_query_with_overlay_result_certificate` executes first and then returns:

1. exact ordered rows;
2. the ordinary `GqlPlanCertificate` at the transaction basis;
3. `GqlOverlayResultCertificate`.

The overlay result certificate uses:

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

`verifies_prepared_query_overlay_result` recomputes the plan and current staged-effect identities from the live transaction authority. Staging another mutation after issuance invalidates the old certificate. An equivalent transaction at the same basis with the same canonical effect can verify it.

Final digest comparisons use constant work over all digest bytes.

## Staged-overlay evidence artifact

`execute_prepared_query_overlay_artifact` packages one successful staged result as `GqlOverlayResultArtifact`. Its v1 body carries:

- transaction basis;
- retained statement digest;
- canonical bind digest;
- plan-certificate digest;
- canonical staged-effect digest;
- exact ordered rows;
- staged-overlay result digest.

The artifact is an unreleased application envelope, not an Appendix-A object or FGP frame.

### Resource-safe audit

`audit_untrusted_prepared_query_overlay_artifact` applies `GqlEvidenceLimits::DEFAULT_UNTRUSTED`. `audit_prepared_query_overlay_artifact_with_limits` accepts caller policy.

A valid header's encoded length and declared row count are checked before row allocation. Malformed headers retain the strict decoder's own typed errors.

The complete audit order is:

1. encoded-byte and declared-row admission;
2. strict framing and result-transcript decode;
3. exact transaction basis;
4. retained statement and canonical bind;
5. plan certificate at the basis;
6. current canonical staged-effect digest;
7. exact overlay re-execution and ordered rows.

Staging another mutation after issuance returns `GqlEvidenceAuditError::StagedEffectMismatch` before old rows can be accepted against the new overlay.

## Stateless staged-result paging

`GqlOverlayResultArtifact::page` and `page_from_token_bytes` slice an already materialized result. Product-level transaction methods additionally perform complete resource-safe artifact audit and current-overlay re-execution before returning a page.

The public adapters are:

- `audit_untrusted_prepared_query_overlay_artifact_page`;
- `audit_prepared_query_overlay_artifact_page_with_limits`.

### Request and audit order

The adapter order is explicit:

1. reject zero page size;
2. strictly decode and checksum-check optional fixed-width token bytes;
3. apply artifact byte and row admission;
4. strictly decode the artifact;
5. verify basis, prepared input, plan, and staged effect;
6. re-execute the current overlay and compare exact rows;
7. bind the token to staged artifact kind, basis, result digest, and offset;
8. return one contiguous page.

A malformed token therefore does not trigger an unnecessary overlay materialization and replay. A syntactically valid token cannot bypass artifact audit.

### Token and page meaning

The page token binds:

- staged-overlay artifact kind;
- transaction basis;
- complete staged-overlay result digest;
- next row offset.

Its checksum is unkeyed. It is not a capability, authentication token, authorization proof, or malicious-tamper guarantee.

A page exposes exact start, end, total, and remaining row counts, plus an optional next token. Page size may change between calls because it is caller policy, not result identity.

### Staged paging boundary

This is not a transaction cursor. The entire staged artifact is decoded and the entire overlay query is re-executed for every product-level page request. It does not hold transaction-owned operator state, apply backpressure, stream rows, renew a lease, or reduce materialization cost.

A later staged mutation invalidates the old artifact during audit before token binding. A durable-result token cannot resume a staged artifact because kind binding refuses it.

Runnable witness for the general paging contract:

```bash
cargo run -p fgdb --example gql_evidence_pages
```

Runnable certificate-only staged witness:

```bash
cargo run -p fgdb --example gql_txn_overlay_result_evidence
```

## Evidence and replay boundary

The certificate, artifact, and page token bind staged rows to the concrete plan, basis, canonical staged effect, and exact offset.

They do **not** create standalone or cross-process transaction replay. A verifier holding only these values cannot reconstruct:

- the durable snapshot;
- staged `LogicalDeltaTemplate` bytes;
- graph rows that were read;
- transaction read-set or MATCH-expansion state;
- first-committer-wins conflict state.

Standalone replay requires a package carrying the exact staged template, required durable snapshot authority or material, and strict framing that establishes those bytes produce the named staged-effect digest before query execution.

Publisher authenticity is a separate signature or transparency layer.

## Mutation-sensitive laws

The focused transaction and page tests prove:

- identical canonical staged effects at the same basis verify across transactions;
- row reorder, replacement, insertion, or truncation fails;
- a later staged mutation invalidates old certificate, artifact, and page evidence;
- the unchanged transaction continues to verify its original evidence;
- artifact row corruption fails strict decoding;
- hostile declared row counts refuse before allocation;
- exact and one-below resource limits retain typed outcomes;
- fixed-width page tokens reject every truncated prefix, trailing data, invalid headers, and checksum mutation;
- durable tokens cannot resume staged artifacts;
- token syntax is preflighted before overlay replay;
- valid tokens cannot bypass staged-effect verification;
- page slices are contiguous, repeatable, and terminal at the exact end.

## Deliberate limitations

The current transaction surface does not provide:

- full SSI or serialization-graph validation;
- general predicate/range conflict tracking;
- multi-relation staged writes;
- savepoints or nested transactions;
- transaction-owned streaming cursors;
- typed statement parameters;
- authorization/session ownership;
- standalone portable transaction replay;
- released artifact or page-token compatibility;
- physical-plan or runtime-cost evidence.

These remain dependency-ordered future work rather than implicit claims of the bounded overlay.
