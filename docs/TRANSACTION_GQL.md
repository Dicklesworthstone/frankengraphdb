# Transaction-Overlay GQL

Status: **live bounded subset** on unreleased `main`, introduced by `005a839701d3a9d270c17ab68d5641dc8fa74a48` under `fgdb-w4-g1-txn-core-qpmg.4`.

This document defines the GQL read surface of `WriteTxn`. It is a staged read-your-own-writes overlay over one pinned durable basis. It is not the final SSI transaction engine, session protocol, or portable replay format.

## One executor, three entry shapes

The overlay has one executor whose input is `&BoundPlan`:

```rust
WriteTxn::execute_prepared_gql(&self, &Database<V>, &BoundPlan)
```

The text and certified APIs are adapters:

```rust
WriteTxn::execute_gql(&self, &Database<V>, statement, &RelationBind)
WriteTxn::execute_gql_certified(&self, &Database<V>, statement, &RelationBind)
WriteTxn::execute_prepared_gql_certified(&self, &Database<V>, &BoundPlan)
WriteTxn::prepared_gql_plan_certificate(&self, &BoundPlan)
```

`execute_gql` parses and binds exactly once, then delegates. `execute_gql_certified` also binds exactly once; it does not call the text executor and bind a second time. Evidence is minted only after successful overlay execution.

The expanded `gql_undirected_certified.rs` integration test proves parity among text, prepared, certified-prepared, and plan-only certification. It also proves that a directed plan produces different rows and a different plan digest from the undirected plan.

## Overlay semantics

The executor reads:

1. the transaction's pinned durable `CommitSeq`;
2. every staged vertex/edge mutation in call order;
3. staged label and integer-property changes through `WriteTxn::vertex`;
4. staged edge creates/deletes and vertex-delete cascades;
5. the same deterministic projection, sort, deduplication, `SKIP`, and `LIMIT` discipline as the bounded database/read-view surface.

It records:

- concrete vertex and edge observations in the transaction read set;
- MATCH expansion coordinates used by first-committer-wins read-conflict detection;
- projected vertices as observations even when the expansion already encountered them.

A subsequent commit still goes through the existing prepared-write and FCW validation seam. Query preparation does not publish anything.

## Source ownership

`crates/fgdb/src/write_txn.rs` is the private module map. Its included files divide responsibility without dividing state authority:

| File | Responsibility |
|---|---|
| `preamble.rs` | Imports, typed errors, `WriteTxn` state. |
| `lifecycle.rs` | Begin, basis, staged write preparation. |
| `vertex_reads.rs` | Vertex and all-vertices staged overlay. |
| `edge_reads.rs` | Edge, adjacency, and incident staged overlay. |
| `gql_types.rs` | Internal overlay graph and predicate-set vocabulary. |
| `gql_entry.rs` | Text binding and the single plan-only dispatch. |
| `gql_node.rs` | Node-only labeled scan over staged rows. |
| `gql_overlay_graph.rs` | Durable-plus-staged graph materialization. |
| `gql_edge_match.rs` | Directional/two-hop/predicate MATCH execution. |
| `gql_api.rs` | Certified and plan-only public methods. |
| `finish.rs` | Commit, abort, conflict detection, pin release. |
| `traits_and_tests.rs` | Diagnostics, drop behavior, focused unit law. |

Every file is `include!`d into one private module. The fields of `WriteTxn` remain private, and there is only one transaction state machine.

## Evidence boundary

A transaction plan certificate binds:

- the complete bound-plan transcript;
- the transaction's durable basis sequence.

It does **not** bind:

- the staged mutation sequence or combined write template;
- the transaction read set or expansion set;
- exact returned overlay rows;
- commit outcome;
- a portable or cross-process transaction identity.

Consequently, database/read-view ordered-result digests must not be reused as transaction replay claims. A staged-overlay identity must be defined first, with an explicit transcript and mutation-sensitive tests.

## Refusals

All prepared faces retain the existing typed refusals:

- `Finished` when the transaction pin has been released;
- `Gql(Parse)` or `Gql(Bind)` for text preparation failures;
- `Read` for a durable-basis read refusal;
- `Write` for commit/preparation failures outside query execution.

No certificate is returned when execution refuses.

## Remaining work

The next transaction-query increments are dependency-ordered:

1. run the pinned Rust toolchain and preserve the exact-tree verdict;
2. introduce an owned prepared definition containing statement bytes, canonical bind, and plan;
3. define staged-overlay identity before exact transaction-result evidence;
4. bind typed parameter values into preparation and evidence;
5. integrate full SSI/predicate-range conflict ownership rather than extending the one-batch subset indefinitely.
