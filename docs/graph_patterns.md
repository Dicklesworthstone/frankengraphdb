# Typed connected graph patterns and correlated binding rows

Source implemented on unreleased `main`, September 9, 2026. Native Rust build,
tests and repository gates for these connector-authored changes are
**unverified here**. Owners `fgdb-boundplan-gla-lowering-seam-r2kd` and
`fgdb-w10-embedded-54r` remain open pending complete acceptance evidence.

## Capability

`fgdb_gql::algebra::GraphPatternBuilder` prepares connected positive patterns
beyond the legacy text parser's two edge positions. It supports longer fixed
paths, branching motifs, closed walks, chords, self-loops, mixed directions,
predicates on any declared vertex, and explicit equality/inequality between
any pair of declared vertices.

A pattern can return either a sorted unique vertex-ID vector or **correlated
multi-column binding rows**. The two outputs share one connected-pattern
compiler and one GLA evaluator. They do not execute one query per column, zip
independent column sets, or reconstruct a Cartesian product after traversal.

**This is a typed Rust preparation API, not an extension of GQL text syntax.**
It consumes caller-resolved label/property/relation IDs, like the existing
bound-plan API. It neither resolves catalog names nor grants graph authority.
Text parsing, numeric templates and their existing statement/evidence formats
remain unchanged. No external dependency or storage backend was added.

## Pattern preparation

Declare each variable once, then reuse its name in edge atoms and predicates.
An edge's direction is relative to its named endpoints: Forward means source
to destination, Reverse means destination to source, and Undirected allows
either orientation.

```rust
use fgdb_gql::algebra::{GlaDirection, GraphPatternBuilder};

let mut builder = GraphPatternBuilder::new();
for name in ["person", "friend", "company"] {
    builder.vertex(name)?;
}
builder.edge("person", knows, GlaDirection::Forward, "friend")?;
builder.edge("friend", works_at, GlaDirection::Forward, "company")?;
let pattern = builder.prepare("company", 0, Some(100))?;
let result = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
```

The first edge seeds a binding row. The compiler repeatedly selects the first
remaining edge connected to an already bound variable. When only its
right-hand variable is bound it reverses traversal, not the atom's meaning.
An initially disconnected edge can therefore be deferred until a connector
makes it available. Every declared variable must eventually belong to the
same edge-connected component; a missing connector produces Disconnected,
not a dropped component or implicit Cartesian product.

A previously bound endpoint becomes an explicit identity check after expansion.
A newly bound endpoint receives its predicates once. Explicit identities are
emitted when both variables are available. One edgeless vertex is a deliberate
vertex scan and may be unlabeled; multiple edgeless variables refuse.

Definition ceilings are structural admission bounds, not performance SLOs:
64 edge atoms, 65 declared vertices, 256 predicates, 64 identity constraints,
and 128 ASCII bytes per name. Names follow `[A-Za-z_][A-Za-z0-9_]*`. Mutators
validate before changing the builder. Invalid/duplicate/missing names,
disconnected definitions and excess definitions produce typed errors without
echoing protected names or values.

## Correlated multi-column projection

Use `prepare_bindings` to retain several variables from each complete match:

```rust
let pattern = builder.prepare_bindings(
    &["person", "friend", "company"],
    0,
    Some(100),
)?;
let result = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
for row in &result.value {
    for (name, value) in pattern.columns().iter().zip(row.values()) {
        println!("{name}: {value:?}");
    }
}
```

The projection requires 1..=65 distinct declared variable names, in the desired
column order. EmptyProjection, DuplicateProjection, UnknownVariable and a
Columns limit error reject invalid definitions without changing the builder.
The same variable cannot be repeated as two output columns in this bounded
surface; aliases and arbitrary expressions are not implemented here.

`PreparedGraphPattern<GraphBindingRow>` owns an immutable ordered column schema.
`columns()[i]` names `row.get(i)`. `GraphBindingRow` owns only that row's vertex
IDs; `values()` borrows the full slice, `get(i)` returns None out of range, and
`len()`/`is_empty()` describe the shape. Produced rows are nonempty and have
exactly the prepared schema's width. Rows and result Debug output redact IDs;
`columns()`, `values()`, `get()`, `plan()` and canonical byte getters are
explicit data exports, not redacted log interfaces.

Consider a graph whose matching pairs are `(person1, company20)` and
`(person2, company21)`. The tuple result retains those two pairs. Independently
projecting person and company would lose which person belonged to which
company; neither invented pair is introduced by this implementation.

The shared compiler emits ProjectBindings with its ordered slot list,
Distinct, OrderByBindings and Limit. Distinctness is over the **complete
projected row**, not each column or the unprojected path. Rows are ordered
lexicographically by their declared column order and numeric 128-bit VIds.
Offset/count pagination applies after that ordering and duplicate elimination.
Parallel edges and multiple witnesses of the same tuple collapse at this
explicit final distinct operation. Different tuples sharing one column remain
different. An empty result still has its schema on the prepared pattern.

### Statically determined output shape

`GlaPlan<Row = VId>`, `PreparedGraphPattern<Row = VId>`,
`GlaExecution<Row = VId>` and `GqlQueryExecution<Row = VId>` retain the original
scalar type as their default. Existing `prepare` calls still return VIds.
`prepare_bindings` chooses GraphBindingRow. The output domain is sealed to
these two shapes; applications cannot install another collector or turn an
immutable tuple plan into a scalar plan. Only the private compiler constructs
plans and their corresponding terminal operators.

The traversal, source, predicate cache, work meter and result-limit checks are
shared. The only output-specific step collects the terminal projection. A
single-column tuple query agrees with the scalar query's IDs but remains a
different typed result, not a silently flattened vector.

Existing scalar logical transcript tags and bytes are unchanged. Tuple
projection and tuple ordering have distinct tags; the slot sequence is encoded,
so changing output column order changes logical identity. Renaming variables
without changing their relationships preserves the logical transcript. Column
names remain separately owned schema metadata, not part of the name-erased
logical transcript. Neither that transcript nor the schema is an authorization
token or a complete result/replay certificate.

## Semantics and physical boundaries

The contract is a connected positive pattern with vertex identity constraints.
The same variable always denotes the same vertex. Different names may denote
the same vertex unless an inequality forbids it. Edge atoms may reuse the same
edge occurrence; self-loop atoms require equal endpoints.

Both outputs have **set projection semantics**, not general GQL bags. Tuple
rows contain vertex IDs only, not property values, edge IDs, nullable optional
bindings, complete paths, path counts or factorized batches. No TRAIL, ACYCLIC,
SIMPLE, edge-variable identity or general GQL morphism contract is implied.
Fixed-length WALK-like reuse is explicit in this typed surface.

Labels and integer predicates use the existing VertexPredicate implementation.
Missing/noninteger properties fail integer comparisons, including NotEqual;
there is no new coercion policy. Count zero is legal and yields no final rows
after normal source admission and evaluation checks. LIMIT is not permission
to perform unlimited work finding the answer.

The deterministic connected-edge order is not a cost-based optimizer or the
registered FreeJoin physical family. A finite 64-edge pattern can still have
an enormous number of witnesses on a dense graph. The ceiling is not a cheap
execution guarantee. Product execution requires an explicit policy.

## Product reads and resource controls

Database and EmbeddedReadView expose `execute_graph_pattern_governed` and its
exact-sequence `_at` variant. WriteTxn exposes
`execute_graph_pattern_governed(database, query_cx, pattern, policy)`. All five
accept either prepared output shape, inferred from the pattern, and return
`GqlQueryExecution<Row>` with rows and exact source/result/work/scratch counts.
There is no separate tuple-only database API or second query-source engine.

Source, ownership, health and frontier errors retain their precedence over
cancellation/resource refusals. Historical reads use their exact sequence;
pinned views cannot observe a later generation. The transaction source folds
canonical prepared effects over its original basis and borrows property values,
never interpreting raw intentions to manufacture rows.

One GqlQueryPolicy allowance covers source construction and evaluation.
SnapshotRecords counts the admitted base table or final transaction overlay,
not projected cells or just the relations surviving filters. ResultRows counts
complete final rows after distinct/order/pagination. No partial vector or
incomplete tuple is returned on failure.

Tuple collection uses a fixed, definition-bounded stack key to check whether a
whole row already exists. Every projected cell consumes a Work checkpoint.
A new tuple reserves one scratch entry for the set and one per owned cell,
checking before growth. Duplicate path occurrences allocate no new tuple.
Completed tuples move into final output after a ResultRow guard; they are not
cloned again one cell at a time at release. Scalar collection retains its
previous event sequence. Source work is not repeated per output column.

These are logical work/scratch-entry budgets, **not exact allocator-byte or
peak-memory limits**. Fixed-width lookup keys, allocator capacities, individual
B-tree operations and cleanup are not per-machine-instruction accounting.
Ordering comparisons are bounded by the 65-column definition ceiling. No spill,
hard wall-clock cancellation deadline, transaction-lifetime budget or
larger-than-memory storage claim is made.

Transaction dependencies are not projected away with columns. Unprojected
predicate vertices, observed edge IDs, endpoints and insertion witnesses
remain relevant after a result-budget or evaluator refusal. Positive-label
node patterns retain label-scoped insertion witnesses; unlabeled scans retain
the table witness. Unrelated vertex-only insertions need not conflict with an
edge scan. An unrelated unlabeled insertion need not conflict with a
label-scoped node scan. Existing ownership and history-conflict checks apply.

Typed patterns are not squeezed into the two-hop BoundPlan certificate format.
There is still no public pattern artifact/audit API. Logical bytes are
application identity, not snapshot evidence or a registered replay manifest.

## Complete example

`crates/fgdb/examples/graph_patterns.rs` initializes a multi-relation graph in
one atomic write, prepares a five-edge branching cycle with a chord and a risk
predicate, then projects both a carrier-ID set and correlated
`(company, supplier, carrier)` rows. It reads the column schema and row
accessors, checks exact limits, applies a staged update, commits, checks pinned
and historical results, and exercises real QueryCx cancellation for both shapes.

```text
cargo run -p fgdb --example graph_patterns
```

The example is committed but **unrun in this environment**.

## Verification and remaining work

This continuation adds **ten Rust tests**: two sealed collector laws, two
projection-schema/maximum-width laws and six integration tests in
`crates/fgdb/tests/graph_bindings.rs`. The existing assignment oracle is extended
with 6,912 tuple projections across graph/direction/shape/column combinations.
It enumerates full assignments independently of GLA slots and edge scheduling;
it does not zip the previous scalar oracle's output columns.

Product tests use an independent nested-loop join over ordinary owned storage
rows. They cover tuple correlation, column order, full-row distinctness,
pagination, singleton/scalar equivalence, all five read surfaces, 128-bit IDs,
exact and one-below limits, retained unprojected dependencies, ensures, staged
updates/deletions, compaction, reopening, pinned/history isolation, source-error
precedence and interruption at every low-level evaluator checkpoint. The
collector test interrupts every row-construction checkpoint before insertion.
Existing scalar, source, policy and transaction regression cases are retained.

These tests and the updated example are **ADDED BUT UNRUN here**. Cargo, rustc,
rustfmt and rch are unavailable. There is no passing native build, Clippy,
formatting, exact-tree proof or repository-gate verdict. No hosted workflow was
dispatched and no bead was closed on source inspection.

Actually executed separately: a finite Python specification compared complete
assignment enumeration against connected streaming projection for **27,648
exhaustive tuple cases and 1,000 randomized connected patterns**, including
parallel edges, mixed directions, cycles, inequalities, column permutations,
pagination and IDs above 64 bits. It also checked **4,355 interrupted collector
prefixes across widths 1..65**. These passed as algorithm models. They do not
compile or execute Rust, validate source admission, prove transaction behavior,
or establish durability or repository acceptance.

The previous pattern continuation's twelve Rust tests remain, along with its
separate 15,147-projection Python model and explicitly reported large
mixed-direction fixture timeout. Those earlier results are not reclassified
as validation of the new Rust tuple implementation.

Remaining: general text grammar/binding, scalar/property/edge/path columns,
bag and optional-match output semantics, pattern evidence/catalog/session
contracts, registered FreeJoin/authorized-Strata access, variable-length and
path-mode semantics, spill and byte-accurate whole-operation resource governance.
These vertex binding rows do not complete the general GQL result contract.
