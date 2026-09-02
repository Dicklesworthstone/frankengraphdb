# Transaction-Overlay GQL Contract

Status: **live bounded read-your-own-writes query surface with exact in-process evidence and an unreleased application envelope**, not full SSI or the final session transaction protocol.

## One execution body

`WriteTxn` executes the bounded GQL subset over:

1. the transaction's pinned durable basis;
2. staged vertex and edge mutations in transaction order;
3. the shared deterministic projection, ordering, `SKIP`, and `LIMIT` discipline.

`execute_prepared_gql` is the plan-only overlay body. Text execution binds once and delegates to it. Certified text execution also binds once and delegates; it does not call a text path that binds again.

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
- `owned_prepared.rs`: coherent preparation and deterministic budgets;
- `overlay_evidence.rs`: canonical staged-effect identity and exact result evidence;
- `portable_evidence.rs`: application-envelope issuance and product-level audit;
- `finish.rs`: commit, conflict checks, abort, and helpers.

The common evidence-envelope framing and strict decoder live in `crates/fgdb-gql/src/evidence_artifact.rs`.

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

Finished transactions refuse preparation, execution, evidence issuance, and evidence audit through `WriteTxnError::Finished` or the corresponding `GqlEvidenceAuditError::Execution` wrapper.

## Deterministic budgets

`execute_prepared_query_budgeted` checks:

- complete overlay edge count for an edge pattern, or complete overlay vertex count for a node scan;
- final result-row count after deterministic query semantics.

An exact boundary succeeds. An exceeded bound returns `BudgetedGqlError::Budget` with no partial rows.

Counting the overlay is itself a transaction read. A snapshot-record refusal therefore leaves the conservative read set populated, preserving conflict safety rather than pretending that an inspected overlay was never observed.

The current implementation materializes and counts the overlay before executing it again. It does not provide storage-level early cancellation, memory governance, spill, backpressure, or physical runtime-cost evidence.

## Canonical staged-effect identity

`WriteTxn::staged_effect_digest` uses the domain:

```text
fgdb:write-txn-staged-effect:v1
```

The transcript binds:

- the transaction basis;
- an explicit empty-overlay tag, or the complete canonical `LogicalDeltaTemplate` retained by the prepared write;
- the canonical template byte length and bytes.

It identifies the staged **semantic net effect**. Two API-call sequences that normalize to the same canonical effect have the same identity. Incidental call history is intentionally outside the transcript.

The digest contains no staged bytes. It is therefore an identity, not a replay package.

## Exact staged-result certificate

`execute_prepared_query_with_overlay_result_certificate` executes first and then returns:

1. exact ordered rows;
2. the ordinary `GqlPlanCertificate` at the transaction basis;
3. `GqlOverlayResultCertificate`.

The overlay result certificate uses the domain:

```text
fgdb:gql-staged-overlay-result:v1
```

It binds:

- the transaction basis;
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

`audit_prepared_query_overlay_artifact` refuses in an explicit order:

1. malformed framing or result transcript;
2. wrong transaction basis;
3. wrong retained statement or bind;
4. wrong plan identity;
5. wrong current staged-effect identity;
6. re-executed rows that differ from the artifact.

The audit order makes failures actionable. In particular, staging another mutation after artifact issuance returns `GqlEvidenceAuditError::StagedEffectMismatch` before the old rows can be treated as belonging to the new overlay.

The decoder rejects invalid magic, unsupported version, wrong kind, nonzero reserved bytes, row-count or length overflow, every truncated prefix, trailing bytes, and row/result corruption. Rows are redacted from ordinary artifact diagnostics.

Runnable witness:

```bash
cargo run -p fgdb --example gql_evidence_artifact
```

## Evidence boundary

The certificate and artifact cryptographically bind staged rows to the concrete plan, basis, and canonical staged effect.

They do **not** create standalone or cross-process transaction replay. A verifier holding only the artifact cannot reconstruct:

- the durable snapshot;
- the staged `LogicalDeltaTemplate` bytes;
- the graph rows that were read;
- the transaction's read set or MATCH-expansion set;
- first-committer-wins conflict state.

The artifact is an unreleased application envelope, not an Appendix-A object or FGP frame. Promotion to a compatibility-governed format requires a registry decision, stable size ceilings, golden vectors, and decoder-evolution rules. Publisher authenticity is a separate signature or transparency layer.

Standalone replay requires a package carrying the exact staged template, required durable snapshot authority or material, and all framing needed to establish that those bytes produce the named staged-effect digest before query execution.

## Mutation-sensitive laws

The focused transaction evidence tests prove:

- identical canonical staged effects at the same basis verify across transactions;
- row reorder, replacement, or truncation fails;
- a later staged mutation invalidates old certificate and artifact evidence;
- the unchanged transaction continues to verify its original certificate;
- a certificate for the advanced overlay does not verify against the older overlay;
- artifact row corruption fails strict decoding;
- the plan certificate still verifies independently at the transaction basis;
- current artifact audit re-executes and requires exact rows.

Runnable certificate-only witness:

```bash
cargo run -p fgdb --example gql_txn_overlay_result_evidence
```

## Deliberate limitations

The current transaction surface does not provide:

- full SSI or serialization-graph validation;
- general predicate/range conflict tracking;
- multi-relation staged writes;
- savepoints or nested transactions;
- transaction-owned cursors;
- typed statement parameters;
- authorization/session ownership;
- standalone portable transaction replay;
- released artifact compatibility;
- physical-plan or runtime-cost evidence.

These remain dependency-ordered future work rather than implicit claims of the bounded overlay.
