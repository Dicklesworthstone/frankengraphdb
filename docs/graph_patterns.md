# Typed connected patterns, canonical values and duplicate-preserving rows

Source implemented on unreleased `main`, September 9, 2026. Native Rust build,
tests and repository gates for these connector-authored changes are
**unverified here**. Owners `fgdb-boundplan-gla-lowering-seam-r2kd`,
`fgdb-w5-parsers-nje` and `fgdb-w10-embedded-54r` remain open pending complete
acceptance evidence.

## Capability

`fgdb_gql::algebra::GraphPatternBuilder` prepares connected positive patterns
beyond the legacy text parser's two edge positions. It supports longer fixed
paths, branching motifs, closed walks, chords, self-loops, mixed directions,
predicates on any declared vertex, and explicit equality/inequality between
any pair of declared vertices.

A pattern can return vertex IDs, correlated multi-column vertex bindings, or
canonical vertex properties mixed with identities. Each builder preparation
defaults to whole-row DISTINCT. `with_duplicates()` selects ALL matching
occurrences. The outputs and multiplicities share one compiler, source path
and evaluator; they do not run separate column queries, zip independent sets,
or reconstruct a Cartesian product after traversal.

GraphPatternBuilder remains a typed Rust API consuming caller-resolved IDs.
The new `PreparedGraphText` is an explicit bounded text front end for that same
compiler, described below. Neither grants graph authority. The legacy
`RelationBind::bind`, `PreparedGqlQuery`, numeric templates and their existing
statement/evidence formats remain unchanged. No external dependency or storage
backend was added, and no refusal falls back to a different parser.

## Connected-pattern text and reusable numeric templates

`fgdb_gql::PreparedGraphText::prepare(statement, resolve)` parses a bounded
connected-pattern profile and resolves its graph names. Binding returns the
existing `PreparedGraphPattern<GraphValueRow>`; execute that value through
`execute_graph_pattern_governed` on Database, EmbeddedReadView or WriteTxn.
There is no text-interpreting executor and no independent query per column.

```text
MATCH (person:Person)-[:KNOWS]->(friend)-[:WORKS_AT]->(company)-[:SHIPS]->(carrier),
      (carrier)-[:BACKS]->(person)
WHERE company.score >= $minimum AND person <> carrier
RETURN ALL person AS owner, company.name AS company, carrier
LIMIT $take
```

```rust
let template = PreparedGraphText::prepare(statement, symbols)?;
let arguments = GqlParameters::new()
    .with_int64("minimum", 80)?
    .with_uint64("take", 10)?;
let pattern = template.bind_parameters(&arguments)?;
let result = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
```

`symbols` is the host's typed schema callback:
`FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>`. A relation must resolve
to GraphSymbol::Relation, a label to Label, and a property to Property. An
unknown name or wrong-domain result refuses; numeric identities are never
cast between domains. Syntax is completely parsed before the callback runs.
Each distinct (kind,name) is resolved once per preparation. This callback is
an integration seam for an existing host catalog, not a second catalog,
automatic schema discovery, authorization token, or session-generation lease.
The legacy RelationBind maps and their artifact transcript are not decoded or
reinterpreted to simulate name resolution.

The accepted profile includes named node patterns, multiple positive labels
with colon notation, fixed-length paths, comma-connected path components,
per-edge outgoing/incoming/undirected direction, and repeated node variables.
WHERE accepts AND-conjoined integer-property comparisons (`=`, `<>`, `!=`,
`<`, `<=`, `>`, `>=`) on any bound vertex, plus arbitrary vertex equality or
inequality. Edges can be supplied before the atom that connects them; the
shared compiler schedules connected atoms. Truly disconnected definitions
refuse instead of dropping a component or inserting a Cartesian product.

RETURN selects named vertices and canonical vertex properties, with optional
AS aliases. An unaliased vertex uses its variable name; an unaliased property
uses the property name. Colliding output names require explicit distinct
aliases and otherwise refuse. Repeated expressions under different aliases
are legal. RETURN * selects each declared variable once in first-occurrence
order and cannot be mixed with additional columns in this profile.

**Text RETURN defaults to ALL; DISTINCT is explicit.** This is different from
the typed builder's DISTINCT default and the old single-ID text API. The
profile choice is the explicit PreparedGraphText API, never an implicit retry
or widening of an existing prepared query. SKIP and LIMIT count canonically
ordered whole rows/occurrences; both accept zero, unlike the legacy parser's
positive-LIMIT restriction. Missing properties project as canonical nulls.
No arbitrary ORDER BY or expression-ordering semantics are implied.

Keywords are ASCII case-insensitive, while names are case-sensitive ASCII
identifiers. Unicode whitespace is accepted and errors carry byte offsets.
Quoted identifiers, comments, semicolon terminators, multiple statements,
edge variables, property maps, OR/NOT, optional matches, variable-length paths,
aggregates, arbitrary projection expressions and general ORDER BY are outside
this profile and refuse rather than being silently ignored. This is not full
GQL/openCypher conformance or the complete registered LanguageContract.

Numeric `$name` operands are structural syntax. `parameter_schema()` exposes
the existing GqlParameterSpec objects, including type and occurrence count.
Property predicates require Int64; SKIP/LIMIT require UInt64. One name cannot
have conflicting roles. `bind_parameters` accepts exactly the required
GqlParameters names/types, refusing missing, extra and mistyped arguments.
It never performs string substitution, reparses source, resolves graph names,
or reads storage. It does clone the bounded resolved builder and run its
ordinary lowering with the concrete values; there is no claim that binding
is allocation-free or skips compilation. Previously bound plans stay immutable.

Definition admission limits are 65,536 UTF-8 bytes, 8,192 lexical tokens and
the builder's existing edge/vertex/predicate/identity/column/name limits. The
lexer keeps one lookahead token and borrows token text. These are bounded
preparation resources, not the runtime QueryCx work allowance or allocator-byte
accounting. Structural/error Debug output redacts names, query text, catalog
IDs and values. `statement()` and `parameter_schema()` are explicit exports.

The resulting plans use the same borrowed source and governed execution as
Rust-built patterns. Ownership, retained snapshots, canonical staged effects,
interruption, resource refusals and transaction dependency tracking remain
owned by those existing paths. This change does not alter the CLI, server
wire parser or legacy prepared-result artifact formats, nor add typed-pattern
evidence. There is no new statement certificate claiming these plans are old
BoundPlan queries.

## Pattern preparation

Declare each variable once, then reuse it in edge atoms and predicates.
An edge's direction is relative to its named endpoints: Forward means source
to destination, Reverse means destination to source, and Undirected permits
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
An initially disconnected edge can be deferred until a connector makes it
available. Every declared variable must belong to the same edge-connected
component; a missing connector produces Disconnected rather than a dropped
component or implicit Cartesian product.

A previously bound endpoint becomes an identity check after expansion. A new
endpoint receives its predicates once. Explicit identities are emitted when
both variables are available. One edgeless vertex is a deliberate vertex scan
and may be unlabeled; multiple edgeless variables refuse.

Definition ceilings are structural bounds, not performance SLOs: 64 edge
atoms, 65 vertices, 256 predicates, 64 identity constraints and 128 ASCII bytes
per name. Names follow `[A-Za-z_][A-Za-z0-9_]*`. Mutators validate before changing
the builder. Invalid/duplicate/missing names, disconnected definitions and
excess definitions produce typed errors without echoing names or values.

## Correlated vertex bindings

Use `prepare_bindings` to retain several variables from each complete match:

```rust
let pattern = builder.prepare_bindings(
    &["person", "friend", "company"], 0, Some(100),
)?;
let result = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
for row in &result.value {
    for (name, value) in pattern.columns().iter().zip(row.values()) {
        println!("{name}: {value:?}");
    }
}
```

This projection requires 1..=65 distinct declared variable names in the desired
column order. EmptyProjection, DuplicateProjection, UnknownVariable and a
Columns-limit error reject invalid definitions without changing the builder.
A repeated variable is not accepted as two binding columns; value projections
below provide distinct aliases for repeated expressions.

`PreparedGraphPattern<GraphBindingRow>` owns its ordered column schema.
`columns()[i]` names `row.get(i)`. `values()` borrows the row's VId slice;
`get(i)` returns None out of range. Every produced row is nonempty and has the
prepared schema's width. An empty result still has its schema on the pattern.
Rows and result Debug output redact IDs; explicit accessors are data exports.

Pairs `(person1, company20)` and `(person2, company21)` remain those two pairs.
Independent person and company projections cannot reconstruct that correlation.
The compiler emits ProjectBindings, terminal Distinct, OrderByBindings and
Limit. Default distinctness is over the complete projected row, not columns
individually or the unprojected path. Rows order lexicographically by column
order and numeric 128-bit VIds; pagination follows whole-row ordering.

## Canonical vertex-property columns

The concurrently landed property projection composes with the same collector:

```rust
use fgdb_gql::algebra::GraphColumn;

let pattern = builder.prepare_values(&[
    GraphColumn::vertex("person", "person"),
    GraphColumn::property("company_name", "company", name_key),
    GraphColumn::property("company_score", "company", score_key),
], 0, Some(100))?;
let result = db.execute_graph_pattern_governed(&query_cx, &pattern, policy)?;
for row in &result.value {
    let person = row.get(0).and_then(|value| value.as_vertex());
    let score = row.get(2).and_then(|value| value.as_scalar());
    println!("{person:?}: {score:?}");
}
```

`GraphValueRow` owns correlated `GraphValue::Vertex(VId)` and
`GraphValue::Scalar(CanonicalScalar)` cells. Values preserve their canonical
type, collation and timestamp binding. Scalar order is the canonical scalar
order; vertex identities occupy a separate domain after scalars. No numeric
coercion turns an integer and a floating value into the same projected cell.

A missing property projects as Scalar(Null), so it compares equal to a stored
canonical Null under DISTINCT. A property-source error remains a source error,
not a null or missing row. This does not add optional graph bindings. All
matched vertices still satisfy the positive connected pattern.

Aliases must be distinct valid names; the same expression may appear under
several aliases. `columns()` exposes aliases; `value_columns()` exposes bound
expressions. Property key/slot order is in logical bytes, whereas aliases are
separate schema metadata. Value/row/result Debug output redacts payloads.
Explicit scalar accessors intentionally expose values to the caller.

Value plans require an explicit property source. The sealed GlaIdentityOutput
bound restricts predicate-only low-level execution to identity outputs; no
fallback silently substitutes null when a value source was omitted. Product
entrypoints use their existing borrowed immutable snapshot or canonical
transaction source. Returned rows own values and do not pin that source after
the execution finishes.

This is projection of the existing atomic CanonicalScalar property domain,
not arbitrary expressions, recursive collection-valued properties, edge/path
values, a catalog binding service or an authorization grant.

## DISTINCT and ALL matching occurrences

All three builder-prepared output shapes default to DISTINCT. Explicitly retain
each complete match with the immutable modifier:

```rust
let all = builder
    .prepare_bindings(&["person", "company"], 0, Some(100))?
    .with_duplicates();
assert!(all.preserves_duplicates());
let result = db.execute_graph_pattern_governed(&query_cx, &all, policy)?;
```

`prepare_values(...).with_duplicates()` and `prepare(...).with_duplicates()`
select the same mode. The modifier consumes one definition without changing
its clones. Repeated calls are idempotent. It removes only terminal DISTINCT,
retaining source/traversal, schema, ordering and offset/count. Logical bytes
therefore distinguish the modes; default logical bytes remain unchanged.

Two parallel edges satisfying the first atom and three satisfying the second
produce six ALL occurrences even when their projected tuples are identical.
Assignments to unprojected variables can also produce repeated projected rows.
One undirected self-loop contributes one occurrence, not two orientations.

Whole rows remain canonically ordered. Pagination counts occurrences and may
split equal-row runs; a result-row allowance of two refuses at the third output
occurrence, not the third distinct value. Count zero returns no final rows but
does not bypass source admission, traversal checks or acquired dependencies.
No partial vector is released on a later source, limit or interruption error.

ALL stores each actual occurrence under a private tie ordinal. No product of
estimated cardinalities is materialized as a multiplicity counter. The private
ordinal is absent from public columns and result values. Equal rows remain
equal after it is removed. This implementation is not factorized, compressed
multiplicity storage or a spillable bag: dense matches can still retain many
rows even when the final LIMIT is small.

## Shared execution and resource boundaries

`GlaPlan<Row = VId>`, `PreparedGraphPattern<Row = VId>`,
`GlaExecution<Row = VId>` and `GqlQueryExecution<Row = VId>` keep the original
scalar type as default. The sealed output domain contains VId, GraphBindingRow
and GraphValueRow. Only private compilers pair plans and terminal collectors;
applications cannot install a collector or turn a value plan into a scalar one.

Database and EmbeddedReadView expose `execute_graph_pattern_governed` and its
exact-sequence `_at` variant. WriteTxn exposes
`execute_graph_pattern_governed(database, query_cx, pattern, policy)`. All five
accept every prepared output shape/multiplicity and return rows plus exact
source/result/work/scratch counters. There is no separate bag-only database
API or query source.

Source, ownership, health and frontier failures retain precedence over
cancellation/resource refusals. Pinned views cannot observe later generations.
The transaction source folds canonical prepared effects over its original
basis rather than interpreting raw intentions to manufacture query rows.

One GqlQueryPolicy allowance covers source construction and evaluation.
SnapshotRecords counts the admitted base table or final transaction overlay,
not projected cells or only filtered relations. ResultRows counts complete
final occurrences after requested distinctness, ordering and pagination.

Identity tuple collection builds a bounded stack key. Every selected cell is
a work checkpoint. DISTINCT allocates no new tuple for a repeated key; ALL
reserves every retained occurrence. New tuples charge an entry and each owned
cell before growth. Value projection borrows candidate scalars for lookup and
reserves variable payload units before cloning: each additional 64 bytes of
text/sort-key/byte/zone payload reserves a logical scratch entry. ALL pays those
reservations for every copy. Completed rows move to final output after the
existing ResultRow guard rather than being cloned again at release.

These are logical work/scratch-entry budgets, **not exact allocator-byte or
peak-memory limits**. Allocator capacities, individual B-tree operations,
canonical scalar comparisons/copies and cleanup are not checkpointed at every
machine instruction. The output-width ceiling bounds columns, not total
payload bytes. No spill, hard wall-clock deadline, transaction-lifetime budget
or larger-than-memory storage claim is made.

Transaction dependencies are not projected or deduplicated away. Unprojected
predicate vertices, observed edge IDs, endpoints and insertion witnesses remain
relevant after output or evaluator refusal. Positive-label node patterns retain
label-scoped insertion witnesses; unlabeled scans retain the table witness.
An unrelated vertex-only insertion need not conflict with an edge scan.

Fixed-length edge occurrences may be reused by different atoms. Distinct
variable names may denote the same vertex unless an inequality forbids it.
No TRAIL, ACYCLIC, SIMPLE, edge-variable identity or complete GQL morphism/bag
contract is implied. The deterministic connected-edge schedule is not a
cost-based optimizer or registered FreeJoin. A finite 64-edge definition can
still have an enormous number of witnesses.

Typed patterns are not squeezed into the legacy BoundPlan certificate format.
There is still no public pattern artifact/audit API. Logical bytes and schema
are application identity, not snapshot evidence or a registered replay manifest.

## Complete examples

`crates/fgdb/examples/graph_patterns.rs` atomically initializes a five-edge
branching cycle with a chord, a risk predicate and a real parallel OWNS edge.
It projects a distinct carrier set, correlated company/supplier/carrier tuples
and duplicate-preserving company/risk values. It checks exact policy limits,
staged updates, live publication, historical and pinned reads, and actual
QueryCx cancellation for all three shapes.

```text
cargo run -p fgdb --example graph_patterns
```

`crates/fgdb/examples/graph_text.rs` uses the production runtime to initialize
a multi-relation graph, prepare the named four-edge text example above, bind
and rebind numeric arguments, preserve duplicate property rows, validate exact
policy boundaries, stage an update, publish once, read pinned/history results,
and observe real runtime cancellation.

```text
cargo run -p fgdb --example graph_text
```

Both examples are committed but **unrun in this environment**.

## Verification and remaining work

The text-preparation continuation adds **13 Rust tests**: nine lexical,
structural, schema, parameter, independent-assignment and policy tests in
fgdb-gql, plus four public integration tests in `fgdb/tests/graph_text.rs`.
The core assignment oracle covers 768 graph/profile cases without sharing
text parsing, slot allocation or traversal. Product tests independently join
four concrete owned edge records and read ordinary vertex properties.
Coverage includes long paths, closing motifs, all five read surfaces, aliases,
nulls, multiplicity, 128-bit IDs, strict name domains, numeric boundaries,
UTF-8 prefixes, input/token/shape caps, exact limits, every low-level
interruption checkpoint, staged ensures/deletes, conflict retention, disjoint
commits, compaction/reopen and historical isolation.

These tests and the new example are **ADDED BUT UNRUN**. An attempted
`cargo test -p fgdb-gql` returned exit 127 because Cargo is missing. rustc,
rustfmt, rch and native repository gates are unavailable here. Lexical delimiter
and Git blob-identity checks were performed; neither is compilation, runtime
validation, formatting, Clippy or exact-tree proof. No new Python algorithm
model is represented as validation of the text front end. No hosted workflow
was dispatched and no bead was closed.

The earlier bag/value-composition continuation added **15 Rust tests**: two
multiplicity collector laws, two property-payload/null collector laws, one
property-bag preparation/policy law, four independent pattern-bag tests, and
six product integration tests in `crates/fgdb/tests/graph_bags.rs`. These
supplement rather than replace the concurrent property projection's tests and
earlier scalar, tuple, source, transaction and policy tests.

The earlier pattern oracle enumerates complete variable assignments and
multiplies concrete edge-occurrence counts only inside the bounded test model.
Its 26,244 bag cases also check unchanged DISTINCT answers. Product oracles
enumerate pairs of ordinary owned edge records and independently look up
ordinary stored vertex properties; they do not execute scalar queries and zip
the answers. Coverage includes nulls, type-preserving values, payload
reservations, parallel edges, cycles, mixed directions, every interruption
checkpoint, exact limits, occurrence pagination, ensure aliases, retained
unprojected dependencies, canonical staging, compaction/reopen, owner checks
and pinned/history isolation. Those tests were also added but unrun here.

Previously executed separately: the finite Python bag specification passed
26,244 bag comparisons, the same number of DISTINCT controls, 131,220
pagination comparisons and 13,065 interrupted collector prefixes across widths
1..65. This is an abstract identity/bag model, not Rust execution or validation
of canonical scalar behavior, property-source admission, transactions or
durability. It does not validate this text parser.

Earlier finite model results remain limited to their original domains: the
binding-row model had 27,648 exhaustive tuple cases, 1,000 randomized patterns
and 4,355 interrupted prefixes; the earlier single-column pattern model had
15,147 projection comparisons and an explicitly reported large mixed-direction
fixture timeout. None becomes acceptance evidence for this Rust integration.

Remaining: full text language/profile and CLI/wire integration, quoted names,
comments, optional/edge/path columns, arbitrary projection expressions,
complete GQL bag/path semantics, pattern evidence and catalog/session contracts,
registered FreeJoin/authorized-Strata access, variable-length paths, spill and
byte-accurate whole-operation governance.
