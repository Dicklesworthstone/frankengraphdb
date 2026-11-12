# FrankenGraphDB Implementation Status

Capability baseline: unreleased `main`. Each section carries the date it was last re-derived from the code; the latest source review is 2026-11-11 (qualification is stated separately).

This document is the compact, agent-facing map of **what executes now**, **where each live behavior is owned**, **what its evidence proves**, and **what remains target architecture**. The comprehensive plan remains normative for the finished system; this file records the present inhabitable subset.

## Product reality

FrankenGraphDB is not yet a released graph-database product. There are no tagged releases, supported installer, CLI/server distribution, Python package, or compatibility promise. The live product surface is an embedded Rust composition crate over real Chronicle durability and real Strata tier-D storage, with a deliberately bounded GQL read slice and a bounded write-transaction overlay.

The repository contains real durable mechanisms and executable cross-layer paths. It does **not** yet contain the complete W10 surface, full ISO GQL, GLA/Loom physical execution, full SSI, or the distributed and incremental system described by the plan.

## Live product verticals

### Bolt explicit native write transactions — qualification pending

Source added 2026-11-11 under `fgdb-roadmap-bolt-v2-vb6e` and
`fgdb-w2-txn-lifecycle-mhae`. The Bolt adapter now composes the native
`AuthorizedWriteTransaction` owner across BEGIN, RUN and COMMIT.
Write/default BEGIN with signed Write or ReadWrite rights opens a writer;
read-only grants and read mode retain the existing pinned read session.

Each accepted statement stages into the same private workspace. Later graph/set
and exact terminal aggregate reads see earlier staged effects, while other
connections see committed state. Aggregate inputs retain their complete
patterns, row pipelines, grouping and output clauses; counts, wide sums and
rational means preserve their native exact domains. Collected graph values
are hydrated from the staged workspace. Catalog calls use the retained scope.
The checked output collector hydrates graph values before publishing a cursor.
PULL/DISCARD consume provisional results marked `fgdb_transaction_local: true`;
only COMMIT publishes and returns a commit bookmark. ROLLBACK, RESET,
GOODBYE, disconnect and any failed statement discard the unpublished prefix.
Outstanding result cursors must be consumed before COMMIT.

Native authority, source/work/result/creation/mutation/statement allowances
remain cumulative. Generation admission covers each operation and lock wait;
no database lock or active-operation permit is held between messages.
Statement preparation also uses the retained permit, including original text,
parameter admission, trusted statement time, scoped symbol lookup and binding.
An explicit read mode on RUN cannot stage a write. Subscription notification
follows native completion and writer-guard release. A fence after successful
commit cannot be translated into a retryable precommit refusal.

Regression sources cover the production state machine and real Bolt TCP with
an independent FGP observer, including own-writes, isolation before commit,
one publication, rollback/disconnect, restricted rights and output quotas.
Source/API reviews completed. The aggregate and preparation extension adds
state-machine and TCP regressions for empty/grouped summaries, exact native
values, staged collection hydration, original-text budgets and failure rollback.
Targeted pinned formatting and `git diff --check` passed. Server and smaller
query-library compile attempts were killed by SIGKILL while checking the pinned
`asupersync` dependency, before project diagnostics. Runtime tests, Clippy and
the exact-tree proof remain **UNRUN**; whole-workspace formatting also reports
pre-existing differences. The broader roadmap beads remain open.

This is a bounded local owner, not durable reattachment or full Bolt V2.
Explicit writer reads use the native graph/set and exact aggregate subset;
non-catalog procedure hosts and historical/branch forms refuse and discard
the staged prefix. Explicit read mode retains the
broader committed-snapshot reader. Concurrent basis advancement retains the
native conservative refusal, and full SSI remains separate.
See `docs/bolt-autocommit-writes.md` for the protocol boundary.

### Correlated COLLECT subqueries — qualification pending

Source added 2026-11-11, related to `fgdb-1f8f7`. A scalar
`COLLECT { ... RETURN value }` expression now lowers through the existing
governed Collect Apply operator and relational List continuation. It builds
nested graph results without another executor, dependency or transcript tag.

```cypher
MATCH (n:Person)
RETURN n.name AS name,
       COLLECT {
           MATCH (n)-[:KNOWS]->(friend)
           RETURN friend.name AS name ORDER BY name
       } AS friends
ORDER BY name
```

Bodies may be source-free or correlated, use WITH/filter/grouping/DISTINCT
and ordered pages, nest further collection subqueries, and capture list-local
bindings. An explicit one-column RETURN is required. Body variables remain
private; parameters, catalog admission, depth and execution budgets retain
their existing owners. Body order, outer occurrence order, duplicate rows and
NULL members are preserved. Empty bodies yield `[]`; a NULL imported binding
still permits `COLLECT { RETURN binding }` to yield `[NULL]`. This differs from
the aggregate `collect(binding)`, which drops NULL inputs.

The current expression owner gathers graph bodies before scalar evaluation.
Direct COLLECT subqueries inside CASE/COALESCE therefore refuse during
preparation. A collection first bound in WITH is an ordinary list and can be
used in either lazy expression.

The 2026-11-12 extension admits UNION, UNION DISTINCT and UNION ALL inside
collection bodies through the existing native set operator. Each branch sees
the original correlated row and returns the same single-column schema;
deduplication stays local to each outer occurrence. Returning COUNT and EXISTS
subquery branches use the same composition. Native integration regression
sources cover capability masking in every branch, pinned snapshots and staged
transaction reads. The new regressions remain UNRUN: dependency compilation
was killed by SIGKILL, and the serial retry lost its execution environment
before project compilation or a terminal verdict. Exact-tree qualification
remains open.

Six added regression functions cover those semantics, ordered outer and body
rows, nested/list-local capture, schema and scope refusals before catalog
callbacks, rebinding, resource ceilings, cancellation and late errors under
LIMIT 0. Independent source review caught and corrected private-projection
ordering and lazy-arm lexical edge cases. Targeted pinned formatting and
`git diff --check` passed. Compiler attempts were killed by SIGKILL in the
pinned `asupersync` dependency before project diagnostics; compilation,
runtime tests, Clippy and exact-tree qualification remain **UNRUN**. The
broader graph-list language bead remains open.

### Parameterized regular expressions — qualification pending

Source added 2026-11-11 under `fgdb-20foe`. Literal and parameter patterns
share the existing bounded regex compiler. Expressions such as
`MATCH (n) WHERE n.name =~ $pattern RETURN n.name` now accept a typed text
parameter; graph and row filters, RETURN/CASE, aggregate keys and mutation
expressions use the existing scalar VM and execution metering.

Binding compiles patterns before graph access, preserving template/argument
identity. NULL yields UNKNOWN while retaining operand validation and lazy CASE
evaluation. Malformed, oversized, unsupported-profile and wrongly typed
patterns refuse at preparation/binding, including under LIMIT 0. Row-dependent
pattern expressions remain outside this surface.

Three new regression functions cover rebinding, literal-equivalent results
and work, template/bound identities, row WHERE routing, grouping, NULL,
invalid-pattern preflight and mutation selection/assignment. Source review
completed; formatting, compilation, tests, Clippy and full proof are **UNRUN**
because the execution environment became unavailable. The broader language
bead remains open.

### Bolt write RETURN graph metadata — qualification pending

Source added 2026-11-11 under `fgdb-roadmap-bolt-v2-vb6e`. Autocommit
`CREATE`/`INSERT`, mutations, vertex and relationship MERGE, and native write
chains can return nodes, relationships, paths and nested collections over
Bolt. The narrow `AuthorizedWriteOutput` reader uses the final unpublished
workspace and the existing masked metadata owner, including endpoint visibility,
deletion witnesses, live authority and cumulative native work/scratch limits.
The output collector owns and admits every Bolt record before the sole native
commit. A later metadata, depth, frame, signed budget or cancellation failure
therefore cannot publish an earlier write or row. RUN remains deferred;
completed results retain their exact metadata without a postcommit query.

Regression sources cover final-state metadata, nested values, masked properties,
full-width identities, reverse path orientation, cumulative metadata quotas,
swallowed admission errors and late oversized output under PULL 1 and DISCARD.
Source and caller reviews completed. A recovered local candidate passed the
pinned formatter, but resource exhaustion killed dependency compilation (an
initial exit 137 and a later `syn` SIGKILL); the executor then became unavailable
again before its formatted source could be retrieved. This commit preserves
the complete reviewed source. The exact committed tree's formatting, product
compilation, Clippy, tests, UBS and full proof remain **UNRUN**, not passing.
The original roadmap bead remains open; this does not qualify complete Bolt V2.
The explicit-transaction extension above has its own pending qualification.

### Prism label and relationship unions — qualification pending

Source added 2026-11-11 for `fgdb-luq0b` and `fgdb-w8-projection-sd7`.
A literal procedure graph map now selects up to 16 distinct labels and 16
distinct relationship types. A vertex qualifies when it carries ANY selected
label; an edge qualifies when its type is selected and both endpoints qualify.
Selected isolated vertices remain. Empty lists retain all already admitted
identities, and the graph map never grants additional capability scope.

The same canonical selection is used by native and pinned reads, authorized
reads, transaction overlays and decoded buffered reads. Input order and repeated
identities do not change the selection, its cache key or its projection
transcript. Existing all/singleton transcript and digest bytes are preserved.
Transaction selection sees staged labels and relationship changes and retains
the underlying read/conflict witnesses.

```cypher
CALL fnx.pagerank(
  {labels:['Person','Company'], relations:['KNOWS','WORKS_WITH'],
   weight:'weight', parallel:'sum', self_loops:'drop'},
  0.85, 100, 1e-9, true)
YIELD node, score
RETURN node, score ORDER BY score DESC
```

The existing sealed source owns one physical relationship partition: it accepts
label unions but requires exactly that one relationship type. It explicitly
refuses relationship unions. The maintained extension below carries complete
union selectors through six analytic owners. Dynamic projection maps and
named graph catalogs remain outside this surface.

The standalone canonical selector tests passed (2/2) under the pinned compiler
before the environment failure. Added regression sources cover binding and
canonical transcripts, historical reads, staged overlays and relation phantoms,
hidden labels, buffered cache identity, sealed source boundaries and maintained
query refusal. Source/caller reviews and a recovered local formatting pass
completed. The exact committed tree's formatting, product compilation,
integration tests, Clippy, UBS and full proof remain **UNRUN** after resource
exhaustion and the subsequent executor loss. The broader beads remain open.

### Maintained analytics over schema unions — qualification pending

Source added 2026-11-11 under `fgdb-w8-projection-sd7`. Native standing
queries and server subscriptions now accept the same bounded label/type unions
for connected components, weakly connected components, strongly connected
components, core numbers, triangle counts and clustering coefficients.

The compiler builds a shared DISTINCT selected-vertex relation, preserves
counted edge support across disjoint relationship-selector chunks, filters both
edge endpoints with the existing Semi joins, and retains isolated vertices.
These ordinary maintained row operators feed the existing selected analytic
owners. Removing one overlapping label or one parallel edge does not remove
support that remains through another selected label or relationship. Later
label/type changes, deletions and rebuilds use the same dependencies.

```cypher
CALL fnx.core_number(
  {labels:['Person','Company'], relations:['KNOWS','WORKS_WITH'],
   direction:'undirected', parallel:'collapse', self_loops:'drop'})
YIELD vertex, core
RETURN vertex, core
```

Both dimensions retain the full 16-distinct-ID bound. The complete private
circuit participates in subscription admission, including shared children,
endpoint joins, isolate projection and analytic output. Failed registration or
rebuild discards the entire private suffix. Existing algorithm, direction,
weight, loop and per-owner limits remain in force. Server subscriptions still
require unrestricted label, relationship and property scope; this extension
does not introduce capability-masked maintained subscriptions.

Regression sources include independent connectivity/motif expectations,
overlapping labels, cross-type parallel support, isolates, the ninth and
sixteenth relationship types, full-bound registration/rebuild rollback,
subscription replay and signed admission. Production and test source review
completed. Product compilation, integration tests and exact-tree formatting/
Clippy/UBS/full proof remain **UNRUN**: resource-constrained builds were killed
inside dependencies, and the executor became unavailable. The standalone
canonical selector's two tests passed under the pinned compiler. This does not
qualify durable maintained views or multi-relation sealed sources.

### Explicit authorized native transactions — qualification pending

Source added 2026-11-11 under `fgdb-w2-txn-lifecycle-mhae`.
`Database::begin_authorized_write` returns an
`AuthorizedWriteTransaction<'issuer, Clock>` that stages multiple statements
and commits once. Its `prepare_statement`, `stage_chain`, `read`,
`read_aggregate`, `statistics`, `commit` and `rollback` methods expose local
explicit ownership of the existing native workspace. Dropping the owner
discards the unpublished prefix.

The owner retains one live Warden permit, a cloned commit context and the
canonical transaction workspace. It holds no database borrow or lock while
idle. Statements reuse the existing authorized chain collectors, original
before/after image checks, masked overlay reads, conflict witnesses and checked
graph-output admission. No statement autocommits. Later statements read their
own earlier effects, while ordinary database reads still see committed state.

Preparation retains the live permit through text admission, parameter capture,
trusted statement-clock observation and the compiler callback. Meter refusals
are latched even when the compiler swallows them. Exact terminal aggregates
reuse the complete authorized staged source and preserve wide integer sums,
rational averages, grouping and collected graph values. Scoped catalog calls
are available in both row and aggregate inputs.

`PreparedGraphWriteChainText::prepare_statement` and its
`prepare_statement_with_parameter_types` counterpart admit ordinary single
CREATE, SET/REMOVE/DELETE and MERGE statements through the existing chain
compiler. The original `prepare` and `is_chain` routing contracts are retained.
`VerifiedCapability::into_write_at` moves verified predicates into a
non-clonable permit while preserving the live issuer and cumulative usage.

Native source/work/scratch, result-row, creation, mutation and statement
allowances accumulate across calls. Standalone reads and RETURN require read
authority. A failed statement, output callback, swallowed metadata/work error,
cancellation or unwind terminally aborts the entire prefix. Capability expiry,
issuer retirement and monotonic-clock checks remain live through COMMIT.
A foreign database handle refuses before its frontier is observed. New
statements conservatively refuse if the original database basis has advanced;
the final commit still uses the existing native conflict/completion owner.

Native regression sources cover own-writes, one final commit,
rollback/drop/deletion, masking and rights, cumulative quotas, output failures,
unwind/cancellation, foreign handles, stale basis, retirement/expiry, metered
preparation and exact staged aggregate reads. The
output-boundary additions recheck cancellation and live authority after a
successful admission callback, before returning provisional rows or restoring
active ownership. Tests inject expiry, retirement and cancellation immediately
after hydration and require the whole earlier prefix to be discarded. Two
Warden tests cover movable permits and retained terminal behavior. Independent
source review verified the shared staging/output body against the existing
autocommit implementation and reviewed the public API and regressions.
Compilation, runtime tests and exact-tree formatting/Clippy/UBS/full proof are
**UNRUN** because dependency compilation was killed by resource exhaustion and
the executor became unavailable before product validation.

This is a local native API. The Bolt adapter above uses this owner. FGP explicit
write adapters, durable ownership leases, reattachment and full SSI remain
separate work; the lifecycle roadmap bead remains open. Ambiguous commit outcomes retain their native meaning and
are never automatically replayed.

### Durable embedded database

`fgdb::Database` can:

- create and reopen through the real Chronicle capsule-first, marker-last two-fsync path;
- recover the authoritative marker chain and authenticate checkpoint-selected Strata roots against it;
- commit typed `WriteBatch` mutations under the production first-committer-wins validator;
- read vertices, edges, adjacency, and complete collections at the live frontier or an exact retained `CommitSeq`;
- run over the ordinary filesystem VFS or the in-memory VFS without substituting an in-memory graph model for the durable composition path.

Canonical owner: `crates/fgdb/src/lib.rs`.

### Bounded deterministic GQL

Re-derived from the code on 2026-09-28 (fgdb-truthup-0927-6vsim). The live text surface is a bounded, deterministic subset of ISO GQL with an openCypher compatibility surface. Each item below names a test that executes it.

Reads, through `Database::query` → `PreparedNativeRead::prepare` (`crates/fgdb/src/query_explain.rs`):

- The shared positive-pattern `MATCH` compiler defaults to `DIFFERENT EDGES` (`fgdb-8gbaz`): an actual relationship ID cannot occur twice across fixed atoms, quantified segments or comma-separated parts of one clause, including traversals in opposite directions. Parallel relationship IDs and repeated vertices remain legal. `MATCH REPEATABLE ELEMENTS` explicitly permits reuse; each later `MATCH`, `OPTIONAL MATCH` and existential pattern has an independent domain. The same selection compiler feeds query-selected writes. Native GLA constraints and shortest-path history checks enforce the rule before accepting a witness or selecting a shortest answer; fixed stream joins and maintained edge joins retain it. Singleton variable probes can use the existing Trail cursor; stream probe combinations that cannot retain cross-atom path identities refuse before source access. Typed `GraphPatternBuilder` retains its generic repeatable default and exposes `match_mode`. Relation-disjoint fixed atoms need no identity constraint. Regression sources: `crates/fgdb-gql/tests/match_modes.rs`, the graph-text assignment oracle, and `crates/fgdb/src/standing_query/edge/multi_hop/match_mode_tests.rs`. This feature does not lift the existing finite-bound, scoped path-capture or full-language conformance limits.
- Explicit sorting and `min`/`max` share language orderability (`fgdb-3wlho`): mixed integers, decimals and floats compare by exact numeric value; lists compare recursively with nested NULL last; maps compare entry count, sorted keys and then values. For example, `max` of `1,2.0,5,3.2` is `5`. Batch, streaming and incremental extrema retain typed support across deletions, with a canonical representation tie-break for equal numeric values. Explicit sort keys compare all semantic keys before the canonical row tie-break; `NULLS FIRST/LAST` keeps its existing independent placement. Storage/grouping/DISTINCT identity and implicit canonical row ordering are unchanged. Affected bound plan transcripts distinguish these semantics from the former storage-order behavior. Regression sources: `crates/fgdb-gql/src/algebra/orderability/tests.rs`, `crates/fgdb-gql/tests/ordered_values.rs`, `crates/fgdb-gql/tests/source_free_aggregates.rs`, and the row/standing aggregate support tests. This is an orderability repair, not a complete GQL/openCypher conformance claim.
- Computed `ORDER BY` expressions on read-pipeline `RETURN` and `WITH` (`fgdb-mf7og`), for example `MATCH (n) RETURN n.p AS p ORDER BY n.p + n.q * $weight DESC, p ASC LIMIT $count`, use the existing scalar compiler and native projection/sort operators. Returned aliases take precedence; private input/sort columns are removed after ranking and pagination. Expression errors, cancellation and work limits remain effective even with `LIMIT 0`. `DISTINCT` accepts returned aliases and property/function reads determined by a returned graph element, including a vertex carried through a multipart identity join; other private inputs are refused. Terminal aggregate `RETURN` and compound set-expression tails also accept computed ordering over their result scope. Terminal write `RETURN` support is described below; write `WITH` heads retain their existing column-based ordering. Carried property/function reads across `OPTIONAL MATCH` still need projection before that boundary. Standing subscriptions refuse a dynamic final order whose hidden sort columns are no longer available; source-free constant sequences retain their existing exact-sequence owner. Regression source: `crates/fgdb-gql/tests/computed_return_text.rs`.
- `MATCH` over labels, property maps, directed/incoming/undirected edges and multi-pattern joins (`crates/fgdb-gql/tests/disconnected_patterns.rs`); `WHERE` with `AND`/`OR`/`NOT`, comparisons, `IS [NOT] NULL`, `IN [...]`, `STARTS WITH`/`ENDS WITH`/`CONTAINS` and `=~` over an in-house bounded regex engine (`crates/fgdb-gql/tests/text_predicates_and_functions.rs`); `EXISTS { ... }` pattern predicates, also spelled as a bare pattern or `exists(<pattern>)`; those two openCypher spellings may only name bound variables, as openCypher requires, while `EXISTS { MATCH ... }` may declare new ones (`crates/fgdb-gql/tests/scoped_text.rs`).
- `OPTIONAL MATCH` (`crates/fgdb-gql/tests/multipart_optional_reads.rs`), including directly after `UNWIND` (one NULL-extended row per unmatched value; an aggregate over it needs a `WITH` boundary), and including a named relationship in an `OPTIONAL` or later required `MATCH`: `r`, `type(r)` and `r.p` are NULL where the clause has no witness, and the relationship aggregates, crosses `WITH`, feeds `SET`/`DELETE`, and is masked under a capability token (`crates/fgdb/tests/scoped_edge_variables.rs`, `crates/fgdb/tests/authorized_patterns.rs`).
- Quantified paths with a mandatory upper bound (`-[:R]->{1,4}`, `-[:R*1..3]->`), `ANY SHORTEST`/`ALL SHORTEST` (also spelled `shortestPath(...)`/`allShortestPaths(...)`, the same compiled plan: `crates/fgdb-gql/tests/any_shortest_walk_text.rs`) and `CHEAPEST` walks, and the path functions `nodes`/`relationships`/`length`/`path_length` (`crates/fgdb-gql/tests/any_shortest_walk_text.rs`, `crates/fgdb-gql/tests/shortest_walk_text.rs`, `crates/fgdb-gql/tests/restricted_path_text.rs`, `crates/fgdb/tests/weighted_path_text.rs`, `crates/fgdb-gql/tests/path_values.rs`, `crates/fgdb-gql/tests/graph_insert_return_endpoints.rs`).
- Projections with `DISTINCT`, `ORDER BY` (`ASC`/`ASCENDING`, `DESC`/`DESCENDING`: `crates/fgdb/tests/embedded_query_entrypoint.rs`), `SKIP` and `LIMIT`; computed integer and text expressions, `CASE`, `COALESCE`, `NULLIF`, `toUpper`/`toLower`/`trim`/`left`/`right`/`replace`/`size`/`length`/`toString`/`toInteger`, `||` and text `+`; `substring(s, start [, length])` is openCypher's 0-based form (a negative start is refused, as in Neo4j) and `SUBSTRING(s FROM start [FOR length])` the 1-based GQL/SQL form (`crates/fgdb-gql/tests/text_predicates_and_functions.rs`); `XOR`, three-valued, binding between `AND` and `OR`; hexadecimal, octal and binary integer literals (`0x1F`, `0o17`, `0b101`: `crates/fgdb-gql/tests/scalar_text.rs`); float arithmetic with negative and exponent float literals (`-0.5`, `1e5`, `1.5e-3`, `.5`; an out-of-range exponent is refused, never infinity), Int-to-Float comparison by numeric value, and `toFloat`/`floor`/`ceil`/`round`/`sqrt` rounded once by the integer-only binary64 kernel (`round` ties toward positive infinity; `toInteger` truncates a float and reads numeric text as a number, so `toInteger('2.9')` is 2; `toString` prints its shortest round-trip text) (`crates/fgdb-gql/tests/case_expressions.rs`, `crates/fgdb-gql/tests/computed_return_text.rs`, `crates/fgdb-gql/tests/text_predicates_and_functions.rs`); list literals, indexing and `size` (`crates/fgdb-gql/tests/list_values.rs`); list comprehensions `[x IN list WHERE p | e]`, `any`/`all`/`none`/`single`, `head`/`last`/`tail`, slices `l[a..b]`, `range` and `reduce` (`crates/fgdb/tests/list_comprehensions.rs`), each also an operand of the scalar operators around it, as are `size()` and a map's `m.key` (`none(x IN l WHERE p) = (NOT any(x IN l WHERE p))`, `size([x IN l WHERE p]) > 0`, `m.a + 1`: `crates/fgdb-gql/tests/text_predicates_and_functions.rs`, `crates/fgdb/tests/map_values.rs`); a list or map literal compared with `=`, `<>` or `IN`, three-valued element by element (`[1, 2] = [1]`, `{k: null} = {}`, `[1] IN [1, [1]]`; mixed domains such as `['a'] = [1]` are UNKNOWN under the row-predicate law, where openCypher says FALSE: `crates/fgdb-gql/tests/text_predicates_and_functions.rs`); `date(...)` from one calendar's fields (`{year, month, day}`, the ISO week `{year, week, dayOfWeek}`, `{year, ordinalDay}`, `{year, quarter, dayOfQuarter}`, optionally over a `date: d` base), from ISO 8601 text or a date, and its fields `d.year`, `d.month`, `d.day`, `d.week`, `d.weekYear`, `d.weekDay`, `d.ordinalDay`, `d.quarter`, `d.dayOfQuarter`, rendered as a `date` cell; `localtime(...)`, `time(...)` and `localdatetime(...)` from clock fields (`hour` … `nanosecond`; `time` also takes a `timezone` offset such as `'+01:00'`), the calendar fields for a local date-time, ISO 8601 text, another temporal value, or `time:`/`date:`/`datetime:` bases with overrides (a time base that has an offset moves to the same instant at a new `timezone`), with their fields (`t.hour` … `t.nanosecond`, and `t.timezone`, `t.offset`, `t.offsetMinutes`, `t.offsetSeconds` on a time), rendered as `localtime`, `time` and `localdatetime` cells; `datetime(...)` is a timestamp at a UTC offset from the same fields and a `timezone` offset (UTC without one), from ISO 8601 text, from `epochSeconds` or `epochMillis`, or over `date:`/`time:`/`datetime:` bases (a `datetime` base moves to the same instant at a new `timezone`), with the calendar and clock fields at its offset and `d.timezone`, `d.offset`, `d.offsetMinutes`, `d.offsetSeconds`, `d.epochSeconds`, `d.epochMillis`, and `toString` its text; `date.truncate`, `localtime.truncate`, `time.truncate`, `localdatetime.truncate` and `datetime.truncate(unit, value[, map])` cut a value to the start of `millennium` … `microsecond` and set the map's fields over it (a map `timezone` sets the offset without moving the clock), and `datetime.fromepoch(s, ns)`/`datetime.fromepochmillis(ms)` give a UTC datetime; `duration(...)` keeps months, days and a time part apart, built from a map of counts (fractions cascade down, a month as 2,629,746 seconds) or ISO 8601 text, with its fields (`d.years` … `d.nanoseconds`, `d.monthsOfYear`, `d.secondsOfMinute`, …), rendered as a `duration` cell and compared only for equality (`<` between durations is NULL), and `duration.between(a, b)` with `duration.inMonths`, `duration.inDays` and `duration.inSeconds` measure from one temporal value to another (whole months, then days, then time, one sign; the right side read at the left's offset when both have one; times of day only when a side lacks a date); a temporal value plus or minus a duration moves by calendar months (the day clamped to the month), then days, then the time part (a date keeps only whole days of the time part, a time of day only the time part, and an offset stays), durations add and subtract component by component, and a duration times or divided by an Int or Float scales exactly with each leftover fraction cascading down and truncated (TCK Temporal8: `temporal_arithmetic_follows_the_tck`); these values compare chronologically (a time at an offset by its UTC time of day first), `toString` gives the java.time text and `ORDER BY` places them by openCypher orderability; each is also a storable property, kept through open, rebuild, compaction and read views, because the delta template and the vertex and edge property patches each declare the format version their values need, so a build that predates them refuses the bytes by name (`crates/fgdb/tests/zoned_timestamp.rs`, `crates/fgdb-delta-types/tests/canonical_encoding.rs`, `crates/fgdb-strata/tests/vertex_patch_format.rs`, `crates/fgdb-strata/tests/edge_property_patch_format.rs`: fgdb-hbi8s) (a named `timezone` such as `'Europe/Stockholm'` needs a zone database the value language does not have, and a constructor with no argument needs a statement clock: fgdb-txbg9; `crates/fgdb-gql/tests/text_predicates_and_functions.rs`); map literals `{k: e, ...}`, `m.key` and `keys(m)` over values, map projection `n{.a, k: e, v}` over graph rows (NULL for a NULL `n` from `OPTIONAL MATCH`), including maps built from aggregate outputs and `UNWIND` of a list of maps, rendered as a `map` cell by the CLI (`crates/fgdb/tests/map_values.rs`, `crates/fgdb-cli/tests/cli_robot.rs`).
- `count`/`sum`/`avg`/`min`/`max`/`collect` with `DISTINCT`, grouping and `HAVING` (`crates/fgdb-gql/tests/aggregate_text.rs`, `crates/fgdb-gql/tests/having_text.rs`); `sum`/`avg` over float (or mixed integer and float) values return the correctly rounded exact sum or mean, one rounding whatever the row order, with no cancellation loss or intermediate overflow (`crates/fgdb-gql/tests/exact_averages.rs`, `fgdb_types::ExactBinary64Sum`); unaliased RETURN items are named as openCypher names them (`a.title`, `count(b)`, `size(a.title)`) when the GQL-derived name would repeat (`crates/fgdb-gql/src/graph_text.rs` tests); `WITH` pipelines, `UNWIND` and a `MATCH` after `WITH`, including grouped `WITH` (`crates/fgdb-gql/tests/multipart_reads.rs`, `crates/fgdb/tests/with_grouping.rs`).
- `UNION [ALL]`, `INTERSECT`, `EXCEPT` (`crates/fgdb-gql/tests/set_text.rs`, `crates/fgdb/tests/set_text_queries.rs`).
- `FOR SYSTEM_TIME AS OF SEQ` (`crates/fgdb/tests/temporal_graph_text.rs`); `AT BRANCH` through a host resolver (`crates/fgdb/tests/query_branches.rs`); `CALL fnx.* ... YIELD` composed into reads, including capability-authorized reads over the masked projection (`crates/fgdb/tests/gql_call_fnx.rs`, `crates/fgdb/tests/authorized_prism.rs`); `EXPLAIN (CERTIFICATE)` (`crates/fgdb/tests/embedded_query_explain.rs`).
- Query-local Prism graph maps (`fgdb-luq0b`, source added 2026-10-11; Rust compilation, regression execution and final formatting **unverified here** after ENOSPC and executor shutdown): `CALL fnx.pagerank({labels:['Person'], relations:['KNOWS'], weight:'weight', parallel:'sum', self_loops:'drop'}, 0.85, 100, 1e-9, true) YIELD node, score RETURN node, score` binds literal schema names into the prepared plan, then applies the same projection in ordinary, pinned, capability-authorized and transaction read hosts. Template and bound plan identity retain every projection option and full-width schema identity; unprojected call transcripts retain their existing encoding. Algorithm parameters remain independently rebindable. The 2026-11-11 union extension admits up to 16 distinct identities in each literal `labels` and `relations` list, with ANY-label selection and both selected endpoints required; empty lists retain all admitted identities. Dynamic maps and named graph catalogs remain unimplemented. See the union vertical above for validation and sealed/maintained-source boundaries. Regressions cover weighted/reversed paths, all parallel reductions, missing-weight policies, excluded malformed weights, isolated vertices, staged changes/conflicts, capability masking and certified replay in `procedure_calls.rs`, `gql_call_fnx.rs`, `authorized_prism.rs` and `cli_robot.rs`.
- Single- and double-quoted text, `//` and `/* */` comments, and backtick-delimited identifiers, under one lexical rule (`crates/fgdb-gql/tests/scalar_text.rs`). Three scanners sit outside it (re-checked 2026-10-05): the `SUBSCRIBE TO` header takes `--` and nested `/* */` comments but not `//` (`crates/fgdb/src/standing_query/native/changes/statement.rs`); the standalone `CALL fnx.*` parser takes no comments and reads numbers such as `1.` and `.5`, which native text refuses (native text reads exponent literals `1e5`, `1.5e-3`, `2E+10` as floats) (`crates/fgdb-prism/src/call.rs`); and the legacy `BoundPlan` parser has its own (`crates/fgdb-gql/src/parser.rs`).

Source additions on 2026-10-11, with Rust compilation and regression execution **unverified here**:

- Complete graph-element maps (`fgdb-2jw3z`): `properties(n/r)`, `keys(n/r)`, wildcard projections `n/r{.*}` and explicit overrides now lower through native map projections and the shared row/aggregate owners. The source enumerates actual properties from snapshots, retained views or effective transaction overlays. Every visible property needs a host-declared reverse name; missing names refuse even with `LIMIT 0`. Capability sources mask fields before name lookup, payload inspection and per-field charges. The CLI/server supply their declared property bindings. `MATCH ... SET/REMOVE ... RETURN` combines frozen complete maps with the final conflict-checked assignments before atomic staging; removed fields disappear and aliases share the same final map. `CREATE`/`INSERT ... RETURN` also projects maps and keys of created vertices/edges and matched source elements, including imported `UNWIND` inputs, grouping and aggregates. Created maps use the checked insertion fields before identity allocation; projection overrides do not alter storage. Known missing names or duplicate reverse names refuse during preparation. Late expression, budget or cancellation failures preserve earlier staged effects and publish nothing. Single-vertex `MERGE ... RETURN` projects the complete selected vertex after the chosen ON clause and trailing SET through a borrowed point source over the pinned basis and staged effects. It masks before naming/payload charges, records a conservative full-element witness for absent/new property keys, and shares the original rollback workspace, final aggregate owner and signed ReadWrite permit. The library's `query_write`/`query_write_engine` retain catalog-bearing resolvers. Regression sources cover optional NULLs, empty maps, `WITH`, grouping/collect, overlays, compaction/reopen, exact history, field masking, post-write results and FGP authority refresh: `crates/fgdb-gql/tests/graph_property_maps.rs`, `crates/fgdb-gql/tests/insertion_property_maps.rs`, `crates/fgdb/tests/graph_property_maps.rs`, and the CLI/server suites. Scalar-only streaming and standing subsets still refuse unsupported map columns; relationship-pattern or multi-row `MERGE RETURN` and insertion map-projection syntax on a generic imported MAP row remain outside these slices. Ordinary `properties(row)`/`keys(row)` remain row-value expressions. The new write-map regressions have pinned formatter/source checks but remain unexecuted here.
- UNION semantics (`fgdb-d9q6v`): text operands must return the same names, and reordered names are aligned through the native projection before set arithmetic. Incompatible names and mixed ALL/DISTINCT chains refuse before catalog access, including ordinary and temporal native preparation. Parenthesized expressions keep independent quantifier scopes. Generic typed positional set operations are unchanged. Regression sources: `crates/fgdb-gql/tests/set_text.rs` and `crates/fgdb/tests/set_text_queries.rs`.

Writes, through `PreparedGraphWriteScript` (`crates/fgdb-gql/src/graph_text/scoped/mutation/script.rs`) as one atomic program per script:

- `INSERT`/`CREATE`, including `MATCH`- and `UNWIND`-driven creation, consecutive clauses over shared bindings (`CREATE (a), (b) CREATE (a)-[:T]->(b)`), named edges with or without `RETURN`, and `RETURN` of what was created (`crates/fgdb-gql/tests/graph_insert_text.rs`, `crates/fgdb-gql/tests/graph_insert_query_text.rs`), and property values read from map rows (`UNWIND $rows AS row CREATE (:Person {name: row.name})`, with `$rows` a list of maps, from the CLI as `--param 'rows=json:[...]'`: `crates/fgdb-cli/tests/cli_robot.rs`), and the CLI's `write --rows json:[...]` runs any write statement (a deduplicating `MERGE` upsert included) once per object in one atomic program (`crates/fgdb-cli/tests/cli_robot.rs`) or computed from them (`row.tags[0]`, `size(row.tags)`, `IN`); a row whose value is a list or map is a typed refusal that commits nothing (`crates/fgdb/tests/map_values.rs`); `MERGE` with `ON CREATE SET`/`ON MATCH SET` and a plain trailing `SET` (both branches; it wins over an ON clause's assignment) (`crates/fgdb-gql/tests/vertex_upsert_text.rs`, `crates/fgdb-gql/tests/edge_upsert_text.rs`, `crates/fgdb-gql/tests/edge_merge_text.rs`); `SET`/`REMOVE` (`crates/fgdb-gql/tests/mutation_queries.rs`), and `MATCH ... SET/REMOVE/DETACH DELETE ... RETURN` with one row per matched occurrence whose property reads are the element's post-statement value (its own simultaneous assignment, else the old value; a deleted vertex's property is a typed error), through `PreparedGraphMutationQueryText` and `WriteTxn::execute_graph_mutation_query_governed`, and the CLI's `write` (`crates/fgdb/tests/mutation_returning.rs`); `MERGE (n ...) [ON MATCH SET ...] [ON CREATE SET ...] [SET ...] RETURN ...` returning the one chosen vertex after every clause, read from the staged transaction state, through `PreparedGraphVertexUpsertQueryText` and `WriteTxn::execute_graph_vertex_upsert_query_engine_governed`, and the CLI's `write` (a failing RETURN rolls the upsert back; `crates/fgdb/tests/mutation_returning.rs`); a write `RETURN` (CREATE, SET or MERGE) may order by an item's source spelling (`ORDER BY n.p`); write `RETURN` also accepts top-level aggregates and grouping over the statement's frozen occurrence rows (see the bounded-write section below); `DELETE`/`DETACH DELETE` (`crates/fgdb-gql/tests/delete_write_scripts.rs`); multi-statement scripts end to end (`crates/fgdb/tests/write_script_execution.rs`); CSV-bound scripts (`crates/fgdb-gql/tests/csv_write_scripts.rs`).

Typed refusals today (CLI probe and a 35-query agent-style battery, 2026-09-30): untyped relationship patterns `(a)-->(b)` and `(a)-[r]-(b)` (fgdb-du0xg); a path binding in a later `MATCH`, e.g. `MATCH (a), (b) MATCH p = shortestPath((a)-[*..5]->(b))` (put the endpoint predicates inline instead) (fgdb-20foe); a list comprehension or `reduce` over an aggregate output (fgdb-20foe); reading a vertex element's property, a typed non-map failure when evaluated (a map element's entries read: fgdb-o5o9t); complete graph-element maps in relationship-pattern or multi-row `MERGE RETURN` clauses, a map-valued parameter (a list of maps binds), a map as a stored property value, and `UNWIND` rows driving `MERGE` or `SET` in the text (re-checked 2026-10-05: refused with "CREATE or INSERT after UNWIND/MATCH"; the CLI's `write --rows` runs them once per row, and library text support is fgdb-bapr6) (fgdb-2jw3z); `startNode`/`endNode` of an `OPTIONAL` relationship; `CALL { }` and `COUNT { }` subqueries, `size(<pattern>)` and pattern comprehensions; `MERGE` of a pattern with an unbound endpoint (fgdb-2277w); `id()`/`elementId()` (fgdb-j687q); `CREATE GRAPH` and branch DDL (`CREATE BRANCH`, `MERGE BRANCH`).

Seven statement parsers still probe the input; UNION schema and quantifier errors are now authoritative, while other refusals still use the furthest-reaching parser and can report a position or wording from an unintended grammar (fgdb-one-lexer-one-dispatch-285i2). The legacy `crates/fgdb-gql/src/parser.rs` (`BoundPlan`) is not among them: it is the separate parser behind `Database::execute_gql`, through `RelationBind::bind` (re-checked 2026-10-05 against `crates/fgdb/src/query_explain.rs`). Live database reads, historical reads, and immutable read-view reads share one exact-sequence execution kernel in `crates/fgdb/src/gql_exec.rs`.

Typed statement parameters have landed in `crates/fgdb-gql/src/parameters.rs` as structural parser operands, and the engine execution spine consumes them (see the 2026-09-08 → 2026-09-22 reality check below).

### Native catalog procedures

Source added 2026-11-07 for `fgdb-hne`: `CALL db.labels()`,
`CALL db.relationshipTypes()` and `CALL db.propertyKeys()` use the shared native
read pipeline in embedded databases, pinned read views, transactions, the CLI
and authorized server sessions. Their default output names are `label`,
`relationshipType` and `propertyKey`; explicit `YIELD`, `YIELD *`, aliases,
filters, joins, projection and aggregation compose normally. For example:

```cypher
CALL db.labels() YIELD label
WITH label WHERE label STARTS WITH 'P'
RETURN collect(label) AS labels
```

These procedures enumerate the complete reverse catalog supplied by the trusted
host, including declared names not yet used by any element. They do not infer a
durable catalog or scan graph records. A resolver without a complete reverse
catalog refuses preparation. Each prepared call freezes its selected names and
full-width identities into its template and plan transcript. Authorized hosts
apply the live label, relationship and property scope before charging, copying
or returning a name. Visible work and payload scratch share the enclosing query
allowance; hidden entries receive cancellation polls without visible charges.
The former Bolt execution shortcut is removed while its standalone no-bookmark
response metadata is preserved. Specialized streaming, standing-query and
prepared-index profiles retain their existing explicit procedure restrictions.

Regression sources: `fgdb-gql/tests/procedure_calls.rs`,
`fgdb/tests/catalog_procedures.rs`, `fgdb-cli/tests/cli.rs`,
`fgdb-cli/tests/cli_robot.rs` and FGP, HTTP and Bolt loopback tests.
Compilation and runtime qualification are pending for this source slice;
the bead remains open until its required verification completes.

### Relationship endpoint values

Source added 2026-11-07 for `fgdb-udb`: `startNode(relationship)` and
`endNode(relationship)` participate in native value expressions, including
relationships carried through `WITH`, `UNWIND`, list subscripts and
comprehensions. Endpoint values support identity comparisons, `IS NULL`,
property access and direct `MATCH ... WHERE` predicates. For example:

```cypher
MATCH (a)-[r:KNOWS]-(b)
WHERE a = startNode(r)
RETURN startNode(r).name AS source, endNode(r).name AS target
```

The functions use the stored relationship direction, including for incoming
and undirected pattern matches. A NULL relationship produces NULL endpoints;
an unrelated value domain produces a typed refusal. Authorized readers admit
the relationship and both endpoints before returning either identity or
following its properties. Snapshot, transaction-overlay, aggregate and spill
readers use the same endpoint metadata contract. Endpoint property projections
on newly created relationships use their frozen write-row fields.

Regression sources cover the GQL value, insertion and spill owners, native
database/read-view/transaction execution, optional relationships, signed
authorization and hidden endpoints. Runtime qualification is pending for this
source slice; `fgdb-udb` remains open until its required verification completes.

### Verification of the 2026-11-07 query additions

`cargo fmt --all -- --check` and `git diff --check` passed. The pinned
`nightly-2026-08-31` build, with one Cargo job, one compiler frontend thread
and debug symbols disabled, reached the unchanged `asupersync` dependency at
`3d0151aecae2443a735388e6ae4fbd387e78db2c`. Its compiler was killed by signal 9
under sustained pressure at the shared 8 GiB memory limit. The check exited
101 before compiling the changed GraphDB crates. Compilation of these source
changes, runtime tests, Clippy and exact-tree proof therefore remain UNRUN.
UBS was also unavailable. Neither bead is closed or claimed fully qualified.
The dependency pins and required gates are unchanged.

### Relationship MERGE property maps

Added 2026-11-09 for `fgdb-2jw3z`. The current relationship write owners already
execute `MERGE RETURN`, including plural input and bare `properties(r)`/`keys(r)`.
The earlier broad refusal summaries above describe older cuts. This addition
closes the remaining map-projection syntax gap: wildcard and selective maps,
explicit overrides and shorthand fields in plain relationship `MERGE RETURN`,
including endpoint maps and identities carried through `WITH`, plus wildcard
maps in the dedicated `ON CREATE`/`ON MATCH`/`SET` owner.

The compiler uses the existing final-overlay metadata expression and point
property bindings. It adds no second graph reader, interpreter or transaction
owner. Every complete map retains current authority masking before reverse
naming/payload admission, complete-field conflict observations, cumulative
budgets and the statement rollback workspace. Grouping and tails retain frozen
final values for every selected occurrence. A NULL optional identity produces a
NULL map; a map override changes the result, not storage. Invalid source scopes,
path-as-map sources and duplicate field names refuse before catalog resolution.

Eight regression functions in `fgdb-gql/tests/edge_merge_text.rs`,
`fgdb-gql/tests/edge_upsert_return_text.rs`, `fgdb/tests/edge_upsert_returning.rs`
and authorized `edge_upsert_query_tests.rs` cover both branches, prior staging,
plural multiplicity, grouping/tails, NULL guards, missing reverse names and late
errors beneath `LIMIT 0`, and equal visible work for absent versus hidden large
properties. Pinned rustfmt and whitespace checks passed. Product typechecking,
runtime tests, Clippy and exact-tree proof remain **UNRUN**: the pinned foundation
previously exhausted the shared 8 GiB compiler limit, and no approved remote
compiler is available. The broader map bead remains open.

### List predicates inside aggregates

`COUNT(ALL(x IN values WHERE predicate))` and `COLLECT(ALL(...))` retain the list predicate through native read pipelines, aggregating `WITH`, and simple or computed write `RETURN` expressions (`fgdb-xv0sy`). All aggregate argument owners use the shared lexical lookahead that distinguishes the list predicate from an explicit set quantifier such as `COUNT(ALL n.p)`. Both forms compose, including `COUNT(ALL ALL(...))`; comments and keyword case follow the ordinary lexer. NULL predicates remain absent from COUNT/COLLECT, false remains a value, and an empty list retains ALL's vacuous truth. Regression sources are `fgdb-gql/tests/multipart_aggregates.rs` and `fgdb-gql/tests/write_return_aggregates.rs`.

### Computed ordering of write results

Added 2026-11-02 for `fgdb-mf7og`. Terminal `RETURN` after `CREATE`/`INSERT`, matched mutations, vertex `MERGE` and relationship `MERGE` compiles computed `ORDER BY` expressions through the existing native scalar compiler. Returned aliases, repeated source spellings and returned element identities are available; unreturned bindings refuse. Plain column ordering keeps its original fast path. Computed ordering uses the write owner's existing governed relational tail after final-state projection and `DISTINCT`, then removes its private keys before delivery. Grouped outputs follow the same path. Final paging governs result rows without limiting effects; expression, budget and cancellation failures restore the unpublished statement, including under `LIMIT 0`. Write `WITH` heads retain column-based ordering because their later hidden-property columns have a separate scope.

Regression sources are `fgdb-gql/tests/write_return_aggregates.rs`, `fgdb/tests/mutation_returning.rs`, `fgdb/tests/edge_upsert_returning.rs`, authorized `mutation_query_tests.rs` and CLI `transaction/returning_tests.rs`. They cover final-state values, duplicate occurrences, grouped output, private schema, identity-bound property masking, exact resource boundaries, retained earlier writes and atomic CLI output. This extends the bounded resident write-result path; it does not introduce external write-result sorting.

### Deterministic label-propagation communities

Added 2026-11-02 for `fgdb-nu3mt`. `CALL fnx.label_propagation_communities() YIELD node, component` is appended at procedure ordinal 26 in registry epoch 28. It implements the pinned foundation's semi-synchronous algorithm: canonical greedy coloring, color-ordered majority updates, current-label retention on maximal ties, otherwise the smallest maximal label, and a 100-round cap returning the final partition. Each community is represented by its minimum full-width vertex identity. The signature admits an undirected, simple, loop-free projection and takes no parameters or randomness.

One bounded integer state machine drives decoded, compressed and cooperative execution. Five vertex-sized integer vectors retain colors, labels, vote stamps, counts and traversal order; graph rows remain borrowed cursors. Existing admissions cover iteration, work, workspace and output limits, and scalar/coloring/vote phases remain cancellable. Engine hosts retain masked topology, transaction overlays, historical cuts and certificate replay. The CLI robot catalog lists the new procedure. The broader `fgdb-nu3mt` catalog remains incomplete; other unregistered algorithms remain outside this slice.

### Deterministic multilevel Louvain communities

Source added 2026-11-09 for `fgdb-nu3mt`. `CALL fnx.louvain_communities() YIELD node, component` is procedure ordinal 29 in registry epoch 32. The typed `LouvainOptions::new(resolution, threshold, max_levels, weighted)` and four positional/named query arguments share validation and call identity. Defaults are `(1.0, 1e-7, NULL, true)`; resolution must be positive, threshold nonnegative, both finite, and a level ceiling positive. The named-only `max_iter` defaults to 100 positive total sweeps; `LouvainOptions::with_max_iter` sets the same control without changing the constructor or positional signature, and every value participates in call identity. Native projection fields select the graph and optional weight property, for example `CALL fnx.louvain_communities(weight => 'weight', resolution => 1.0, threshold => 1e-7, max_levels => NULL, weighted => true, max_iter => 100) YIELD node, component RETURN node, component ORDER BY node`.

The kernel performs positive-gain local optimization and weighted graph coarsening across completed levels. It reads an undirected simple projection with finite nonnegative weights, counts a retained self-loop twice in degree, and retains isolated vertices. First-level canonical vertex order and stable edge encounter order replace RNG; each returned community is named by its minimum full-width `VId`. The threshold limits interlevel modularity improvement, while `max_levels` deliberately returns that completed level. Exhausting `max_iter` refuses unfinished optimization. Host iteration and work ceilings independently admit that requested allowance before execution, so the estimate uses the algorithm's bounded request rather than the host's potentially much larger maximum.

`crates/fgdb-prism/src/louvain.rs` owns the shared state machine; `sealed_louvain.rs` supplies authenticated compressed rows and cooperative control. Compressed input is read once into an admitted `O(V+E)` working graph, with bounded reusable edge buffers for coarsening. Work, kernel workspace and result admissions remain separate, and all phases are cancellable. This is not an external-memory algorithm. Generic native, capability-authorized, transaction, historical and certified-replay hosts use the same registered call; standing-query maintenance is not included.

Regression sources compare small weighted graphs and a coarsening fixture with the pinned foundation, and cover options, named sweep controls and digest identity, numeric refusals, sweep exhaustion, host admission and every cancellation boundary. `fgdb/tests/gql_call_fnx.rs`, `authorized_prism.rs`, `prism_cooperative.rs` and `buffered_sealed_prism.rs` cover composition, weighted overlays, historical replay, scope masking, signed limits and cooperative cursor release. Compilation and runtime qualification are blocked by SIGKILL while compiling the pinned `asupersync` dependency, including a metadata-only attempt, before product typechecking. Full integration tests and validation gates remain **UNRUN**; this source addition is not runtime-qualified.

### Correlated graph expressions in OPTIONAL MATCH filters

Source added 2026-11-09, extending the correlated-query work in `fgdb-og3db` and `fgdb-2ht8c`. An OPTIONAL continuation after `WITH`, `UNWIND` or returning `CALL` now admits `EXISTS`/`NOT EXISTS`, `COUNT { ... }`, pattern comprehensions and list predicates in its WHERE, including supported multi-clause subquery bodies. The existing parser identifies graph-expression sources before parsing the optional clause, retains its complete typed candidate row, and lowers those sources plus the Boolean predicate inside the existing optional `Apply`. Only the optional clause's local bindings are exported. Each input occurrence therefore keeps all qualifying matches or exactly one NULL extension; filtering never runs after the left join and erases an unmatched outer row.

The shared native operators retain imported-value typing, subquery scope privacy, eager governed graph-source evaluation, scalar CASE laziness, parameter binding and definition limits. No new runtime interpreter or host-specific query path is added. Tests in `fgdb-gql/tests/multipart_optional_reads.rs` assert literal bags, parallel edges, duplicate and NULL imports, following-clause boundaries, private names, parameters, exact resource limits and every cancellation checkpoint. `fgdb/tests/native_optional_subqueries.rs` adds staged-overlay, pinned-view, exact-history, reopen, capability-pruned-oracle and conflict-witness cases.

Pinned formatting and patch applicability checks are available for this source addition. Rust compilation and these regression executions remain **UNRUN**: the pinned foundation compilation previously exhausted the environment's memory allowance, and the canonical filesystem is full. These sources do not close either broader bead or claim a completed exact-tree proof.

### Pinned embedded read views

`Database::read_session()` returns an immutable `EmbeddedReadView` owning one decoded generation. A view:

- keeps its frontier and decoded graph stable after later database writes;
- can be cloned while sharing the same immutable generation;
- executes text, plan-only, or owned prepared GQL at its pinned frontier;
- executes older retained sequences at or below that frontier;
- refuses a later sequence through `ReadError::BeyondFrontier`.

This is a real embedded read-view subset. It is not authorization negotiation, lease/reattach, cursor ownership, server transport, or the synchronous facade.

### Recovery through intact physical copies

Packed-object readers now recover from a missing, truncated or identity-invalid cached copy through intact older packs, loose objects, or packs published after the reader opened (`fgdb-0qjmf`). The caller's exact object-family identity or decoder proves each candidate; a valid transcript from another family cannot satisfy the read. Discovery runs at most once per failed read and can retain a repair discovered before an unrelated non-interruption error. If every candidate fails, the original refusal remains authoritative. `Interrupted` stops copy recovery immediately.

The same recovery applies to bounded roots and segments, batched block/hosted-property proofs, and buffered payload extents. Every candidate retains the original byte ceiling; admitted buffered extents also retain their length and checksum. Healthy reads keep their existing proof path, including batched hashing. Regressions in `fgdb-strata/src/store/pack_tests.rs` cover live-handle repair discovery, damaged/missing/truncated packs, wrong-family candidates, limits, interruptions, partial discovery, and block-window/hosted-patch fallback. Runtime qualification remains open until the pinned Cargo and exact-tree gates complete.

### Selective cold point and adjacency reads

Source added on 2026-10-12 (`fgdb-w3-buffer-g2v`): `BufferedPartition::vertex_at`, `edge_at` and `adjacency_at` now select possible payload extents from bounded routing summaries retained during the mandatory authenticated root open. Vertex reads use patch identity ranges; edge reads use EId minima/maxima computed independently of adjacency order; adjacency reads use the admitted source, relation and destination bounds. All potentially matching histories remain in publication order, including later retirement restatements. Descriptor visits and the complete bounded decode are admitted before payload I/O. Point-edge reads resolve the winning topology first and fetch only its property's containing patch; adjacency, missing edges and propertyless winners do not read property sidecars.

This removes unrelated payload faults and decodes from small cold reads without an eager adjacency cache, a per-edge identity index, a durable-format change or a weaker opening validator. Descriptor selection is still linear in the bounded root metadata; broad ranges can still require multiple extents. Visited extents retain their immutable checksum/decoder checks, while reopening still authenticates the whole declared source. Five new regression sources in `crates/fgdb-strata/tests/buffered_partition.rs` cover exact extent visits, nonmonotonic edge identities, historical restatements, parallel edges, both adjacency directions, relation/range gaps, selected property reads, post-open corruption, candidate caps, memory/work admission and owner refunds. The existing private work-admission test now checks refusal before the first topology fault. Pinned formatting and whitespace checks pass; source review is complete. These Rust regressions remain uncompiled and unexecuted because the pinned asupersync dependency exhausts the available compiler memory before first-party diagnostics.

### Capability-scoped external ordering

Source added on 2026-10-11 (`fgdb-w10-snapshotquery-rzh`): `Database::authorized_ordered_read_session` creates an `AuthorizedOrderedReadSession` with a pinned generation, verified capability, catalog, clock, native policy, two host-owned `SpillFile` values and fixed `AuthorizedSortLimits`. Its `prepare` retains an exact-session template; `execute_ordered` rebinds parameters and applies the native query's own ordering, DISTINCT and final SKIP/LIMIT through the existing authenticated external merge pipeline. Property-only projections and compiler-admitted hidden sort keys no longer require a public identity column. Vertex scans, fixed-edge joins and admitted probes use the same masked native sources as ordinary authorized cursors, including exact retained temporal cuts. Unsupported physical profiles refuse even beneath LIMIT 0.

The complete private input and final window finish before an `AuthorizedSpoolCursor` becomes available. Only this single live-checked cursor can read the canonical final frames: request code receives neither scratch files, hidden intermediate rows, raw spool handles nor an independent reader. The original signed permit pays a nonrefundable reservation for the host's complete sort-work ceiling, native source charges, and final encoded delivery bytes. Signed rows are charged only as final frames are delivered. Governed input encoding uses the existing byte-admitted encoder and memory pool. Live authority checks surround every asynchronous poll and final delivery; expiry, retirement, backwards time and callback unwind fence the pinned session. A dropped in-flight read fuses its cursor.

Host-prepared external ordering added on 2026-10-12: `Database::authorized_prepared_ordered_read_session` imports shared immutable selector/native bindings into a fresh pinned ordered session, with the same trusted catalog contract and exact-session handle ownership as ordinary prepared reads. It rebinds temporal, branch and value arguments without reparsing or catalog callbacks. Authority and template coherence checks precede physical-limit admission. `execute_ordered` returns an `AuthorizedOrderExecution` future with its admitted column schema and effective snapshot available before the first poll, allowing a transport to announce a cancellable stream before sorting. Metadata contains no rows, counts or scratch progress; the complete sort still precedes its first result row. Regression sources exercise fresh versus historical pins, masked properties, parameter rebinding, wrong-owner and branch refusal, authentication/limit precedence, Send ownership, and dropping an unpolled execution without scratch writes. These new engine regressions remain uncompiled and unexecuted under the same dependency compiler-memory blocker described below.

The same authorized ordered-result path is now available through explicit FGP `ReadOrdered` and prepared `OrderedStream` modes, described under server posture below. The decoded source graph remains resident; FGP SourceStream retains its source-driven cursor profiles. Host scratch backends must be trusted, and synchronous bounded sort sections observe expiry at their next poll boundary. Logical work reservations are not exact physical-I/O accounting. Regression sources are `crates/fgdb/src/query_authorized/relational/native/session/stream/ordered/tests.rs` and `crates/fgdb/src/query_spool/governed_tests.rs`; compilation and execution remain unverified because the pinned asupersync dependency exhausts the available compiler memory before first-party diagnostics.

### Maintained native collection aggregates

Native standing-query registration now maintains `COLLECT` and `COLLECT DISTINCT` over a vertex or one uncaptured relationship through the existing relational projection/group circuit (`fgdb-w6-views-2wz`). For example, `MATCH (a)-[:R]->(b) RETURN a AS source, collect(b.p) AS values` retains endpoint predicates, parallel multiplicity and the batch evaluator's source/destination occurrence order. Reverse and undirected orientations, self-loops and any-relation scans use the same proof. Computed aggregate inputs retain the complete original source-row order before evaluating expressions. Registration and subscription footprint admit the same definition.

Endpoint property changes, edge additions/retractions and cascade deletions flow through the existing incident-aware delta source; compaction and rebuild preserve the same result. Captured edges, extra hops and scopes remain outside the plain-input occurrence adapter. Computed inputs can additionally retain supported joins and scoped sources with complete canonical rows, subject to independent admission by the maintained circuit. Hidden source columns and excessive schemas still refuse. Regression sources cover explicit collection order, eight mutation/delta stages, frozen parameters, delayed replay, reopen, quotas, interruption and zero-page failures in `crates/fgdb/tests/standing_collection_groups.rs`, `crates/fgdb-gql/tests/computed_aggregate_text.rs` and `crates/fgdb/src/standing_query/native/set/group_tests.rs`; the broader computed-source tests are in `crates/fgdb/tests/standing_computed_collection_sources.rs`. The vertex/one-edge additions have pinned formatter checks and independent source review; runtime execution remains blocked by the foundation compiler memory limit.

### Maintained analytics over label-selected graphs

Source added on 2026-11-05: `Database::register_standing_native` accepts native `fnx.core_number`, `fnx.triangles` and `fnx.clustering_coefficient` calls over a single-label induced graph, optionally restricted to one relation. For example, register this query to keep the selected core numbers current after ordinary committed writes:

```cypher
CALL fnx.core_number({labels:['Person'], relations:['KNOWS'],
                      direction:'undirected', parallel:'collapse', self_loops:'drop'})
YIELD vertex AS person, core
RETURN person, core
```

Use `fnx.triangles` with `YIELD vertex, triangles`, or `fnx.clustering_coefficient` with `YIELD vertex, score`, for the same selection. Both endpoints must carry the selected label; selected isolates retain zero-valued rows. Omitting `relations` includes all relation types, including types first added by later commits. Label additions/removals, edge changes and vertex cascades update the maintained topology before the existing exact analytics kernel consumes its derivative. Parallel and opposite edges contribute one neighbor, and vertex identities retain all 128 bits.

`standing_native_query` reads the current accepted generation; `rebuild_standing_query` preserves the selection and output schema while rebuilding at the current frontier. A maintenance refusal fences the view without undoing a durable commit or blocking healthy siblings. The private topology is not charged as public result rows; snapshot, work and scratch bounds still apply, with work/scratch ceilings per registry node. This slice uses unit weights and simple undirected support; triangle and core calls require dropped self-loops. Registrations remain session-local and memory-resident, without a durable view catalog or spill.

The same maintainers also compose with an existing maintained row relation through `register_standing_triangle_counts_from_rows`, `register_standing_clustering_coefficients_from_rows` and `register_standing_core_numbers_from_rows`. Each accepts the source handle and two zero-based Vertex column indexes. For example, a maintained source can select enabled relationships with a property predicate:

```cypher
MATCH (a)-[e:KNOWS]->(b) WHERE e.enabled = 1
RETURN 9 AS marker, a AS source, 'ignored' AS note, b AS target
```

Attach each analytic with endpoint indexes `[1, 3]`. Changing only `e.enabled` changes the selected topology and its analytic output, while the physical edge remains in the database. The parent can use any filters, joins, set operations or pages that its own maintained circuit admits; each parent and child retains its own policy. The selected vertex population consists exactly of supported non-NULL endpoints. One non-NULL endpoint supplies an isolate, two NULLs supply nothing, and duplicate or opposite edge witnesses retain support until the last occurrence retracts. Endpoint types and handle ownership are checked even for empty sources. All three handles expose the existing native query, cursor, delta and replay surfaces; core handles also expose typed core-number reads. Repair a separately registered failed parent before rebuilding its dependent analytics. These APIs are trusted embedded composition, not a new authorization boundary.

Regression sources in `crates/fgdb/tests/native_standing_sets.rs`, `crates/fgdb/src/standing_query/kcore/tests.rs` and `crates/fgdb/src/standing_query/triangle_counts/tests.rs` cover independent graph oracles, label/property changes, counted support, signed-delta integration, source admission, cancellation, quota refusals and parent-first rebuilds. `crates/fgdb-server/src/execute/subscription/tests.rs` covers selected analytics through wire delivery, acknowledgement/replay and capability refusal. These tests are authored but uncompiled and unrun: both focused Cargo attempts stopped when the unchanged pinned asupersync dependency compiler was killed for exceeding available memory, before first-party compilation. Formatting and whitespace checks pass; typechecking, runtime tests and Clippy remain unverified.

### Maintained correlated Apply

Source added 2026-11-09 extends the maintained native query circuit (`fgdb-w6-views-2wz`, `fgdb-og3db`) with a bounded inner/optional Apply profile. The GQL definition owner proves the entire correlated body and returns a recipe over its original complete children, the existing row join and ordinary row projections. The database compiler and subscription-footprint calculation consume the same recipe. Committed changes flow through the existing dependency-ordered signed-bag derivatives; this adds no per-commit query replay or separate subquery interpreter.

Imported positions may be reordered or repeated, and the original outer-row multiplicity remains intact. Optional bodies keep their predicates inside the join. A private TRUE witness distinguishes a real match whose values are NULL from an absent match, and lazy output guards produce NULL for every unmatched export, including constants and expressions over imported values. The original definition and native transcript remain unchanged. Existing private registration guards, maintenance failure fencing and whole-circuit rebuild cover the added nodes.

The initial profile admits a single independently safe graph source with identity/path outputs, total column/literal wrappers, imported arguments, supported joins, filters and ALL projections. Independent property and catalog outputs are refused because valid stored values or missing names can otherwise raise errors when an empty outer input would never execute the original body. General per-invocation aggregation, pages, nested Apply, graph-reading exports and positional random values remain unsupported. Relational DISTINCT has the separate profile below. Each child must also pass its ordinary maintained-source admission; no first-leaf substitute or source omission is used. Width limits and existing per-node maintenance allowances remain in force. Registrations are resident and session-local, without durable catalog ownership or spill.

The native `EXISTS`/`NOT EXISTS` and existence-valued RETURN lowering now has its own maintained presence recipe. Only an optional Apply ending in one literal TRUE column with ALL, LIMIT 1, zero offset and no order qualifies. The proof retains the entire input below that compiler-generated wrapper, including all refusal and effect boundaries. Its complete left/right children compile once; existing Semi and Anti joins partition the original outer bag, project TRUE or typed NULL, and combine through UNION ALL before restoring exact public names. Six derived stages preserve outer multiplicity even with parallel witnesses, repeated/nonprefix imports and actual NULL-valued matches. The shared sequence gate ignores only that proven presence page; child/outer pages and ordering remain subject to their existing admission. Predicates may cross only column-copy projections; later total column/literal exports preserve nonemptiness, and discarded computed expressions are never optimized away. This is a presence rewrite, with no generic pagination admission.

A separate bounded count recipe admits correlated `COUNT { ... }` and returning `CALL ... count(*)`, `count(value)` or `count(DISTINCT value)` bodies. It requires one untransformed, keyless count over a safe imported-argument/graph join, with total ALL projections and no extra ON or postfilter conditions. Value counts trace their argument backwards through every copy/reorder projection to an original RHS column; arbitrary outer-only values cannot substitute for a per-key RHS value. Every discarded projection cell still has to be total. Existing RHS-Semi-outer matching restricts the counted relation to currently requested keys before native grouping and checked Int64 conversion. The native aggregate owner retains nonnull/distinct support and its last-occurrence deletion law. Grouping deduplicates RHS key positions while retaining every original typed equality constraint in both joins; duplicate outer rows never scale the count. The original outer bag then Left-joins the groups, and the existing integer COALESCE VM fills missing counts with scalar zero. Keyless and NULL-import cases follow the same recipe. The grouping owner retains the actual complete active-input definition. Both original children compile once, with five derived nodes; subscription admission counts the same nodes. RHS/restriction/group stages use the established private aggregate-input policy, so a large witness bag or the internal keyless zero group does not spend the final-result-row allowance. Additional correlated predicates, computed/property count arguments, HAVING, relational DISTINCT, local pages and computed/discarded effects remain refused.

Regression sources cover literal bags, parallel witnesses, duplicate and NULL imports, native CALL/OPTIONAL/EXISTS/COUNT continuations, first/last-witness transitions, actual NULL-valued matches, guarded output expressions and domains, signed-delta integration, subscription footprint and atomic registration/rebuild refusal. The existence suite includes an independent 256-transition presence oracle, numeric-versus-typed equality checks and exact literal-page/effect refusal boundaries. Count regressions add an independent 256-transition weighted-count oracle, inactive counts above Int64, simultaneous last-outer removal, repeated key constraints, zero defaults and private-witness/public-row quota checks. The value-count extension adds 2,048 weighted transitions through reordered aliases, NULL exclusion, distinct support above Int64 occurrence counts, and twelve native topology transitions distinguishing parallel edges from unique neighbors. Focused pinned formatting and whitespace checks pass for these source additions. A single-job, single-thread pinned `cargo check -p fgdb --lib` stopped at the foundation `asupersync` compile with SIGKILL under the shared 8 GiB memory limit, before product typechecking. Compilation, Clippy and regression execution remain **UNRUN**; no broader bead or exact-tree qualification is claimed.

### Maintained per-invocation DISTINCT

Source added 2026-11-09 extends `fgdb-w6-views-2wz` with native correlated
Inner/Optional Apply bodies containing DISTINCT projections. Both complete
sources compile once. A private projection retains distinct imported argument
tuples, and existing joins/projections execute the body once per tuple. Every
DISTINCT barrier carries an exact private invocation key; a final join restores
the ORIGINAL outer bag and its multiplicities. Later ALL projections preserve
their resulting counts. Optional null extension occurs only at this final join,
after all real body filters and projections. A true all-NULL export remains a
matching witness, while absence never evaluates a fabricated expression row.

Invocation keys encode each scalar/stored/vertex/edge import as its NULL flag
and a nonnull value in the same domain. A specifically checked column-default
constructor proves the output type without a general type assertion. Ordinary
typed join equality remains unchanged; false, numeric zero, vertex/edge zero
and NULL cannot collide. Stored bounded collections retain canonical identity.
Reordered/repeated imports and no-import bodies use the same recipe. Local
pages/order, grouping, nested invocation, unsupported import domains and
reader/ordinal effects refuse. The independent RHS effect proof is unchanged.

Circuit layouts now retain explicit private/public roles for every compiled
node. Rebuild consumes those immutable roles, preserving private witness
allowances even after an initially unbounded policy changes to a small public
row limit. Shared public outer sources remain subject to their quotas. Count's
private restriction/group states retain the same explicit distinction. This
replaces inference from transitive group dependencies; failed rebuilds still
publish no replacement state. All four public Apply recipe types have public
crate-root names, as required by the workspace's `unnameable_types` lint.

Nine GQL regression functions include an independent 2,048-transition oracle,
NULL/filler collisions, stored collections, multiple DISTINCT barriers,
filter/error ordering, residual ON remapping and every preparation checkpoint.
Three database regressions cover native legacy/scoped CALL text, committed
parallel-witness/label/cascade changes, literal and eager expected bags, repeated
deltas, Optional NULL behavior and atomic registration/rebuild failures. The
quota regression keeps over 64 private witnesses under two public output rows,
including a rebuild from an unbounded allowance. Existing rebuild tests now
pass the captured role map. Pinned formatting, whitespace and independent
source reviews passed; product typechecking, Clippy, runtime assertions and
exact-tree proof remain **UNRUN** under the foundation compiler memory blocker.
The broader durable-view bead remains open.

### Buffered terminal OPTIONAL execution

Source added 2026-11-09 extends the larger-than-memory row path from plan §8.7.
`AsyncEdgeJoinPlan` admits an
edge-rooted fixed-hop prefix followed by exactly one terminal correlated GLA
OPTIONAL scope. The scope supports copied outer bindings, fixed expansion,
native vertex/property/Boolean predicates, clause-local different-edge rules
and fixed path/edge captures. It exports original and nullable local identities,
properties and captures through the original final projection. The existing
external owner applies its complete sort, DISTINCT, hidden-key and page tail.

The async probe traversal is now a resumable witness state shared with existing
EXISTS/collection execution. Each optional witness escapes once; only successful
exhaustion can emit the one NULL extension. Source refusal, malformed admission,
cancellation and resource errors cannot become absence. The cursor retains a
definition-bounded prefix/local traversal, with source-owned record guards and
an independent output reservation grown before payload/capture copies. Dropping
a pending pull drops the entire private traversal and permanently fails that
cursor. Historical buffered reads use one selected cut throughout.

Admission proves the complete prefix, optional body and exact final tail;
extra instructions or duplicate projections cannot be silently discarded.
Independent optional scans, variable-length paths inside the scope, nested or
multiple OPTIONAL, later filters/clauses, prefix probes/collections, foreign
capture operands in optional predicates and catalog/map projections still
refuse. This extension selects the async external-order source; ordinary
synchronous streams and aggregate source profiles retain their prior contracts.

Regression sources in `edge_stream/join/asynchronous/tests/optional.rs` and
`query_spool/sort/buffered_optional_tests.rs` cover 135 directional/filter
differentials, literal parallel/no-match outputs, captures and repetition,
invisible candidates, every cancellation checkpoint, pending-pull drop, all
four query limits, malformed programs and historical cold-spool results with
DISTINCT/pages. Late body errors refuse publication even under LIMIT 0. Pinned
rustfmt and whitespace checks pass. Product compilation, runtime tests, Clippy
and exact-tree proof remain **UNRUN** under the shared foundation compiler
memory blocker; no performance or full external-memory GQL claim is made.

### Reusable plan-only execution

`BoundPlan` is the executor-ready plan-only form. It is appropriate when statement spelling and the caller's name-to-ID map are intentionally outside the value's identity.

| Surface | Live or pinned | Exact retained sequence | Plan evidence |
|---|---:|---:|---:|
| `Database` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_certified[_at]` |
| `EmbeddedReadView` | `execute_prepared_gql` | `execute_prepared_gql_at` | `execute_prepared_gql_certified[_at]` |
| `WriteTxn` | `execute_prepared_gql` | transaction basis only | `execute_prepared_gql_certified` |

### Coherent owned preparation

`PreparedGqlQuery` owns, behind private fields:

- exact statement bytes;
- a cloned canonical `RelationBind`;
- the `BoundPlan` derived from those inputs exactly once.

Changing the original statement or bind map cannot change the prepared definition. `Debug` redacts statement, bind, and plan contents. `verifies_definition()` reparses and rebinds the retained inputs as an explicit audit; normal execution does not need to do so.

| Surface | Preparation | Execution | Historical | Exact result evidence |
|---|---|---|---|---|
| `Database` | `prepare_gql_query` | `execute_prepared_query` | `execute_prepared_query_at` | digest and artifact paths |
| `EmbeddedReadView` | `prepare_gql_query` | `execute_prepared_query` | `execute_prepared_query_at` | digest and artifact paths |
| `WriteTxn` | `prepare_gql_query` | `execute_prepared_query` | pinned basis only | staged certificate and artifact |

The owned definition and query-budget vocabulary live in `crates/fgdb-gql/src/prepared.rs`. High-level adapters live in `crates/fgdb/src/write_txn_parts/owned_prepared.rs`. They reuse the existing binder and execution bodies rather than creating another query path.

This is not yet the final parameterized prepared-statement protocol. Typed parameters have landed (`crates/fgdb-gql/src/parameters.rs`); canonical parameter evidence, catalog epochs, authorization context, physical-plan selection, cursor lifecycle, invalidation, and a released persistence contract remain open.

### Deterministic query-execution budgets

Owned prepared execution accepts `GqlExecutionBudget` on `Database`, `EmbeddedReadView`, and `WriteTxn`.

Current dimensions:

1. `SnapshotRecords`: complete immutable vertex table admitted by a node scan, or edge table admitted by an edge pattern.
2. `ResultRows`: final rows after predicates, projection, sorting, deduplication, `SKIP`, and `LIMIT`.

`observed == limit` succeeds. `observed > limit` returns `BudgetedGqlError::Budget(GqlBudgetExceeded { dimension, limit, observed })`. No partial rows escape. Successful execution returns exact `GqlExecutionStats`.

The current implementation materializes and counts the relevant table before ordinary execution reads it. The budget is a deterministic downstream-work and result guard, not wall-clock cancellation, allocation or I/O preemption, memory/spill governance, streaming backpressure, or physical runtime-cost evidence.

### Bounded write transaction

`WriteTxn` pins one basis sequence, overlays staged vertex and edge mutations for read-your-own-writes behavior, records read dependencies and MATCH expansions, and commits normalized staged effects through the production FCW seam.

Text execution binds once and delegates to the plan-only overlay executor. Owned preparation delegates to the same body. The implementation is decomposed under `crates/fgdb/src/write_txn_parts/` while retaining one private module and one `WriteTxn` state authority.

Source added 2026-11-09 extends exact count metadata to this transaction source (`fgdb-61p1n`, `fgdb-rc1017-bridge-wvze5.18`). `WriteTxn` captures the immutable generation's discardable `ExactCounts` at begin. Eligible all-vertex, single-label, all-directed-edge and single-relation counts use that exact basis, with the existing recognizer also admitting proven nonnull/distinct identity counts and universally true endpoint-label filters. A sparse per-identity membership fold adjusts the answer from canonical staged effects, borrowing only touched historical label/incidence before-images and deduplicating cascade retirements. It never copies the population or its property payloads. Raw staged proposals are used only as conflict observations, including conditional ENSURE no-ops; they never supply numeric effects.

The new count-domain witnesses retain both positive and negative populations through failed finalization, zero output pages and savepoint rollback. The shared completion/refresh/rebase validator checks every intervening committed effect, including label exits and actual historical edge relations for deletions and cross-relation cascades. Conditional staged identities remain observed so idempotent replay cannot activate or remove a counted local birth when the concurrent winner lies outside the counted label domain. Accepted refresh and all four native rebase preparation routes advance the pinned metadata together with the basis; refusal retains the prior generation. The existing native aggregate finalizer still owns HAVING, output expressions, paging, checked conversion, cancellation and all four query allowances. Unknown or ineligible metadata falls back to the ordinary overlay source, without changing capability-scoped owners or claiming full SSI, degree histograms, persisted statistics or a competitive performance result.

`fgdb/tests/write_txn_count_metadata.rs` adds twelve public regressions covering 1/32/512-element populations with one metadata record, pinned concurrent changes, staged effects/savepoints, 25 count-domain conflict cases, first-member arrivals, universal-label proof invalidation, exact budgets, late errors, every stable query checkpoint, safe fallback, all four public rebase policies and both conditional ENSURE counterexamples. A private four-policy regression verifies accepted rebase preparations carry the new count generation before completion. Pinned formatting, whitespace and independent source/API review pass. Product typechecking, Clippy and all added runtime regressions remain **UNRUN**: the single-job, single-thread pinned build was killed while compiling `asupersync` under the shared 8 GiB memory limit. The broader metadata-count beads and exact-tree qualification remain open.

Source added 2026-11-09 extends MAP field arguments (`fgdb-8srlm`) to `WriteTxn::query` and CLI transaction read steps. Both owners reuse `GqlMapArgumentText::lower_fields` for nested node and relationship property operands, preserving source byte positions and caller parameters still used by other expressions. The CLI stores the derived arguments with each prepared read; the embedded owner validates transaction ownership and lifecycle before lowering. Statement time, overlay observations, historical basis bounds and exact-cut metadata retain their existing owners. Primitive `PreparedNativeRead` and certificate schemas are unchanged.

`fgdb/tests/native_transaction_map_fields.rs` adds literal overlay/history laws, fresh arguments across calls, aggregate/set families, missing and NULL fields, generated-name collisions, owner/lifecycle precedence, resource refusals, retained staged effects and statement-clock cases. `fgdb-cli/src/transaction/tests.rs` adds per-step parameter and view assertions plus buffered rollback on binding or late execution errors. Pinned formatting, patch applicability and source review passed; compilation and regression execution remain **UNRUN** under the environment's existing memory and disk constraints. This source increment does not close the broader MAP-argument bead or establish exact-tree qualification.

Native `CALL fnx.* ... YIELD` now composes into transaction set queries and aggregates over that same canonical overlay. Staged edges across relations, cascading deletions, changed properties and isolated vertices remain visible to analytics and subsequent `MATCH` stages. Source admission, projection, kernel execution and result conversion share the query's work and scratch allowances; `UNION` also shares its snapshot-record allowance. Full vertex/edge-table witnesses and observed element dependencies survive empty results, failed calls and savepoint rollback. `crates/fgdb/tests/gql_call_fnx.rs` includes all 14 registered procedures plus staged-topology, conflict and budget boundary regressions. Query-local projection maps now select a label/relation-induced graph, effective edge weights, direction, parallel-edge reduction and self-loop policies in this same overlay. Excluded endpoints and dropped loops are filtered before weight inspection, selected isolated vertices remain, and staged label/property changes participate in selection. Conservative source witnesses still detect changes to observed rows and table phantoms. This remains a bounded resident projection; it exposes no durable snapshot certificate for uncommitted graph data.

Native `CALL hybrid.search(...) YIELD` also composes into transaction reads and aggregates, including the CLI's ordered `transaction --write ... --query ...` path. It reuses the transaction Beacon engine over the pinned basis and canonical staged effects: text, coordinate or packed vectors, labels and graph topology participate in the same retrieval, and later projections read staged properties. Canonical source records, search work and source/conversion scratch continue the enclosing query's allowance; private search hits do not consume the final aggregate-row page. Beacon's index and expansion retain their own bounded engine ceilings. Search observations survive failed calls and savepoint rollback, while ordinary transaction completion still validates conflicts. Regression sources are `crates/fgdb/tests/gql_call_hybrid.rs` and `crates/fgdb-cli/src/transaction/tests.rs`. This is per-execution resident search with the existing exact-RRF candidate fusion and ANN semantics, not a durable index or an exhaustive hybrid top-k certificate.

The CLI's ordered `transaction` driver accepts one native `CREATE`/`INSERT`, matched `SET`/`REMOVE`/deletion, or vertex `MERGE` statement with `RETURN` in each `--write` step. It shares preparation and execution with ordinary `fgdb write`, binds each step's own parameters before beginning the transaction, and freezes that statement's post-effect values. `ON CREATE`/`ON MATCH` and trailing `SET` retain native clause order. All read and write results share one transaction-wide row and encoded-output allowance and remain buffered until the sole completion boundary. Rolling back to a savepoint discards the later effects and buffered results and restores their output allowance; full rollback or any precommit expression/output refusal publishes neither effects nor rows. `LIMIT 0` still executes effects and validates return expressions. Regression sources are `crates/fgdb-cli/src/transaction/returning_tests.rs`. Relationship `MERGE RETURN` remains outside the supported native shapes.

Write `RETURN` now shares the native aggregate kernel for top-level `COUNT`, `SUM`, `MIN`, `MAX`, `COLLECT` and their supported `DISTINCT` argument forms, with implicit grouping by the other output expressions. `CREATE`/`INSERT`, matched mutations and vertex `MERGE` aggregate the exact post-statement occurrence rows without rescanning the graph; duplicate matches retain bag multiplicity. Global empty input produces the native empty aggregate row, while grouped empty input produces no groups. Grouping, checked result conversion, `DISTINCT`, ordering and paging share the original work/scratch allowance; only final selected rows spend the result-row allowance. `LIMIT 0` still evaluates every aggregate input, and any late type, arithmetic or resource refusal discards the statement's effects. These are ordinary row-domain write results: counts and integer sums must fit Int64, floating aggregates retain native float semantics, and a nonnull exact integer-domain `AVG` remains a typed refusal because the row value domain has no exact rational variant. Regression sources are `crates/fgdb-gql/tests/write_return_aggregates.rs`, `crates/fgdb/tests/mutation_returning.rs`, `crates/fgdb/src/write_txn_parts/authorized/upsert_query_tests.rs` and `crates/fgdb-cli/src/transaction/returning_tests.rs`, covering native aggregation, frozen values, duplicate occurrences, parameter binding, cancellation, shared budgets, authorization masking and statement/transaction rollback. The added integration tests remain unexecuted in this session while the local compiler/process service is unavailable.

Endpoint eligibility now crosses the native transaction and standalone CLI surfaces (`fgdb-w7-hybrid-99if`, source added 2026-10-11). `CALL hybrid.search(..., eligible => [vertex, ...])` uses the same bounded preparation, live work meter and first-error latch in immutable and staged hosts. It counts input occurrences before allocating the private set, charges unique retained IDs to source and enclosing-query scratch, and applies membership before each candidate limit. Omission/NULL is unrestricted; an empty list selects no endpoints. The complete selected corpus and graph transit remain visible, including excluded documents' BM25 contributions and all transaction conflict witnesses. `WriteTxn::beacon_search_filtered` and `beacon_search_graph_filtered` expose the same result restriction to embedded callers. New native and CLI transaction regressions cover staged mutations, packed vectors, full-width IDs, malformed lists, exact budget boundaries, cancellation, savepoints and buffered output.

Standalone `search --eligible <vid,...>` accepts at most 100,000 unsigned-u128 input occurrences before sorting and deduplicating; an empty value selects no results. The new `Database`/`EmbeddedReadView::beacon_search_graph_indexed_filtered` APIs retain seed-directed incidence expansion and filter graph/text/vector candidate populations before truncation and fusion. Regression sources in `query_beacon/indexed.rs`, `fgdb-cli/src/search.rs` and `fgdb-cli/tests/cli_robot.rs` cover excluded transit, disconnected edge limits, historical pinning, parsing and each retrieval lane. This remains per-call resident retrieval with the existing candidate-limited exact-RRF and ANN contracts. Validation in this session: pinned Rust formatting and whitespace checks passed for the implementation; Rust compilation, all new regression execution, Clippy and the full gate remain **UNRUN** after two guarded Cargo attempts stopped at the environment's memory ceiling before first-party diagnostics. Static-only UBS returned nonzero on test panic/assertion and other heuristic findings; it is not a passing gate.

Full SSI, merge-ladder integration and durable transaction ownership/session policy remain incomplete. Local capability-scoped explicit ownership is described in the native transaction vertical above; wire adapters and reattachment remain open.

### Supervised served-database recovery

Added 2026-10-10 for `fgdb-t79am`. `fgdbd` owns one host-region recovery child per opened database, shared by FGP, HTTP and Bolt. A write guard observes the native handle's state on drop, including future cancellation, and fences the current generation before releasing the database lock. New requests receive `database_recovering`; retained sessions, pending results and subscriptions keep their original generation and cannot resume against a reopened handle. Each protected physical write/flush poll checks that generation as well as the existing Warden authority. Partially written frames close the connection on invalidation.

The child waits for admitted source operations, consumes the old `Database`, and calls the production `recover_authoritatively` path. A successful reopen restores admission with a new generation and an empty subscription registry. A failed or interrupted reopen remains fenced with `database_unavailable`; it does not retry automatically or serve the old handle. Trusted embedding hosts can inspect `Server::database_status`, including the retained failure reason. Recovery does not replay client statements. Post-marker uncertainty, post-D2 publication failure, and a completed write whose queued output is invalidated retain an `outcome_unknown` classification and no-replay diagnostic.

FGP subscriptions terminate with `SUBSCRIPTION_RESET` at a complete frame boundary, independently of flow credit. Its optional sequence names the last complete batch delivered, including an empty baseline. The native client validates that checkpoint, discards any incomplete batch, and returns a typed reset; the CLI emits `subscription_reset` with `resubscribe_required` and `last_delivered_seq`. A new subscription replaces the old bag with a fresh baseline. This is recovery for ephemeral served sessions, not durable subscription resume, retained result recovery, or automatic write retry.

Canonical owners are `crates/fgdb-server/src/recovery.rs`, the server execution/transport adapters, and `crates/fgdb-protocol/src/{body,client,frame}.rs`. Regression sources in `crates/fgdb-server/src/recovery/tests.rs` drive real Chronicle marker/publication failure boundaries, cancellation, generation invalidation, failed reopen and worker ownership. Standalone protocol library/mechanism tests pass on the pinned toolchain. The new server and client-transport regressions have not run in this session: compilation of the unchanged pinned asupersync dependency was killed by the environment's memory limit before reaching them.

## Reality check 2026-09-08 → 2026-09-22

### 2026-09-08 — full verification green; repairs landed

Full verification at commit `9adf484d`: **9/9 core gates and 40/40 registered gates green**. *Correction 2026-09-24: `9adf484d` is not an ancestor of `main`, so this run proves no commit `main` contains; NE-0046 rejects it as proof.* Repairs landed in the same window:

- identity-E2E pin fix;
- cursor-policy export (`fgdb-w10-embedded-54r.1.1`);
- txn bulk scans (`fgdb-w10-embedded-54r.1.2`);
- handle ownership (`fgdb-w10-embedded-54r.1.3`);
- vertex patch packing (`fgdb-w3-properties-gou.1`/`.2`/`.3` series; `.2` closed 2026-09-22).

### 2026-09-17 — robot CLI binary

`crates/fgdb/src/bin/fgdb.rs` landed (commit `79548aca`; *since moved into the `crates/fgdb-cli` crate by `4dc0b7fc`*): `create`/`write`/`query --stream`/`--certify-to`/`diff`/`transaction`/`replay`/`load`/`robot schema`, emitting the NDJSON `{"v":1,...}` envelope, with a frozen contract test (`tests/cli_robot.rs`) plus 33 process-level `cli_*` tests.

### 2026-09-2x — execution, algorithm, and kernel landings

- FreeJoin joins compiled into maintained relational circuits (`c0f0740f`); six-kind checked joins over complete relational queries (`f8c37559`).
- GQL relational WHERE fused with ordered equality probes (`ee4d964d`).
- CSV incremental decode (`6ae9747f`).
- Prism weighted shortest paths directly over compressed Strata rows (`b2d7761c`).
- Aegis payload-survival math across failure domains (`2f6abad0`, `d390c6bd`).

### 2026-09-22 — milestone closures and seven new workspace crates

Milestone beads closed: `fgdb-w4-g1-txn-core-qpmg` (W4/G1 transaction core; *reopened 2026-09-22 and still open 2026-09-24: its SSI, secure-view and merge-ladder blockers are open, and the tracker is authoritative*) and `fgdb-boundplan-gla-lowering-seam-r2kd` (BoundPlan GLA lowering seam); `fgdb-w3-properties-gou.2` closed. Seven new workspace crates materialized 2026-09-21/22:

- `fgdb-cli`;
- `fgdb-order` — Aegis deterministic Raft transition kernel;
- `fgdb-policy` — capability policy verifier IR;
- `fgdb-prism` — snapshot projections + sealed-view FNX kernels including Dijkstra;
- `fgdb-repl` — bonded replica catch-up;
- `fgdb-warden` — macaroon issuance/attenuation;
- `fgdb-beacon` — memory-resident HNSW/BM25/hybrid index. Reachable from GQL as `CALL hybrid.search(name => value, ...) YIELD node, score, ...` (text, vector and graph lanes fused by exact RRF; a vector is one property per coordinate or one packed little-endian f32 byte property, the embedding form; schema-name arguments resolve at prepare time; privileged and capability-authorized hosts, so `fgdbd` serves it). Built per call at the read's snapshot: no durable index definition yet.

Topology: `g0_topology_e2e` is green and `topology_registry_validates_clean` passes as of 2026-10-05, with all seven once-contested crates registered; bead `fgdb-topology-seven-crates-9n8ao` is still open and should be reconciled against that. The checker index grew from 99 rows (57 live) at the 2026-09-07 census to 120 rows (74 live: 16 script, 11 binary, 47 cargo-test) by 2026-09-22.

### Posture claim, limited on purpose

The embedded library posture is live. The CLI binary is real, but the robot contract is mid-consolidation. The server posture is live as a subset since 2026-10-06 (see below). No claim beyond that is made here.

## Server posture (2026-10-06)

`fgdbd` (`crates/fgdb-server`) composes the embedded engine behind `fgdb-protocol`'s FGP connection machine over asupersync TCP. Witness: `crates/fgdb-server/tests/loopback.rs` over real loopback TCP and a durable on-disk database.

- Handshake HELLO/HELLO_ACK/AUTH/AUTH_OK/SELECT_DATABASE/READY with typed canonical bodies (`crates/fgdb-protocol/src/body.rs`), a transcript-derived session binding, and one uniform refusal for a nonexistent or unauthorized database.
- Authentication by Warden capability token (`fgdbd token`); each EXECUTE builds a fresh `authorized_read_session` or `authorized_write_session`, so scope, expiry and signed budgets are rechecked per statement and masking applies before expansion.
- Results as ephemeral SNAPSHOT_RESULT chunks on a server-minted stream under exact byte-and-row flow credit, with QUERY_CANCEL, PING and DRAIN served while a stream waits; SIGINT/SIGTERM drain with GOODBYE.
- Native source-driven reads through explicit `ExecuteMode::ReadStream` (wire tag 3): identity-leading vertex and fixed-edge/join row plans, plus native vertex/fixed-edge aggregates, retain one pinned source and one signed permit. Client credit drives actual native cursor pulls. Aggregates consume their complete masked input on first demand, then deliver completed groups with exact count, wide sum and rational average values; only delivered groups spend signed rows. Input failures precede aggregate output, including with LIMIT 0. The server holds no database read lock while waiting, buffers at most one frame plus one pending row, and rechecks recovery and capability at protected writes. Aggregate groups, DISTINCT and COLLECT retain separately governed in-memory state. Unsupported cursor profiles refuse; ordinary `Read` and prepared `Buffered` reads retain their existing eager execution. Prepared `SourceStream` reads use this same physical source and flow-credit path. `AuthorizedReadSession::stream_send` and `stream_aggregate_send` expose the same native drivers to async hosts while the local APIs still support non-Send clocks. Protocol/native/loopback regression sources cover row windows, exact values, historical reads, masking, work/scratch/row limits, cancellation, authority retirement and recovery. Compilation and runtime validation remain blocked here by memory exhaustion in the pinned asupersync dependency before first-party diagnostics. See `docs/FABRIC_PROTOCOL.md`.
- External ordered reads (2026-10-12, `fgdb-w10-snapshotquery-rzh`): explicit `ExecuteMode::ReadOrdered` (wire 4) and `PreparedReadMode::OrderedStream` (wire 2) use one authorized external-order session for masked vertex/fixed-edge joins and admitted probes, including property-only projections, hidden sort keys, DISTINCT and final windows at exact historical cuts. `DatabaseConfig.ordered_reads` or per-database `fgdbd --query-scratch-dir` supplies trusted scratch; omission returns a statement refusal. Defaults provide a shared 64 MiB tracked spill pool, four concurrent sessions, two 512 MiB append-limited files per session, 100,000 input rows and a 10,000,000-unit sort-work ceiling prepaid from the signed permit. Native source and final delivery spend that same permit. Files are exclusively created and unlinked before query bytes are written on Unix. Admission, sorting and final row reads retain their single future across controls; pending setup observes disconnect, drain, shutdown and recovery even before schema announcement. A credited columns-only chunk exposes the child stream before the first sort poll, allowing cancellation. Complete sorting precedes all result rows, and final spool reads and output consume byte/row credit. Live capability and generation guards protect each poll and physical send; recovery holds no operation lease across pending filesystem I/O or credit waits. The adapter retains one frame and one measured pending row. Aggregates and other unsupported physical profiles refuse without eager fallback; the source graph remains resident. New scratch, control-loop, authorized-execution and TCP loopback regressions received independent source review, scoped Rustfmt and whitespace checks; compilation/runtime validation of these sources remains blocked by pinned dependency compiler memory. See `docs/FABRIC_PROTOCOL.md` for limits and exact accounting boundaries.
- Clients: `fgdb_protocol::client::Client` and the CLI's `fgdb remote query|write`. Remote `query --read-mode buffered|source|ordered` selects an explicit execution mode, defaulting to buffered. Source/ordered modes flush columns and rows incrementally while retaining only schema width and a fully flushed row count between callbacks. Their schema omits the unknown effective snapshot sequence; only a successful END supplies the final rows result's exact `seq`. A schema or row callback failure sends one cancellation, suppresses subsequent callbacks and grants, and drains the original terminal before connection reuse. Both SDK streaming callbacks are fallible. Late errors, EOF and sink failures emit no successful CLI terminal; no query is retried. `uint:` arguments bind the full top-level `u64` range. Protocol/client regressions and five scripted CLI tests cover canonical mode tags, exact cells, flush-before-next-frame consumption, schema/row cancellation, reuse, late failures and selected-cut metadata. On 2026-10-12 the dependency-free protocol library command `cargo test -p fgdb-protocol --lib --no-default-features --locked --offline -j1` passed all 10 tests; transport and CLI tests remain unexecuted. The binary and fixture robot schemas match exactly.
- Connection-owned prepared native reads: `Client::prepare_read`, `execute_prepared`, `execute_prepared_streaming` and `release_prepared`. Preparation admits syntax/structural operand types under live read authority without executing or pinning a snapshot; execution rebinds all parameters and uses the existing authorized native executor. `execute_prepared` selects `Buffered`; `execute_prepared_streaming` takes an explicit `PreparedReadMode` after `cx`. `SourceStream` and `OrderedStream` import shared immutable templates into a fresh authorized session without reparsing, retaining their selected snapshot and cumulative permit across credit pauses. Ordered execution also receives fresh private scratch. Unsupported profiles refuse without eager fallback. The wire execution body requires mode 0/1/2 after its handle. Top-level `WireValue::Count` binds exact unsigned temporal/window operands. Opaque handles are scoped to one connection, discarded on AUTH_REFRESH, and fenced permanently by database recovery. The cache is limited to 64 templates and 4 MiB of original source text per connection, with idempotent release. Streaming callback errors cancel and drain the original terminal before connection reuse. Protocol, native session/ownership, zero-credit, cache/recovery and TCP lifecycle regressions are included. Protocol-core tests (27) and core Clippy passed in the earlier workspace; the current dependency-free library check above covers 10 tests. Transport/native/server tests and final integrated-tree gates remain unverified.
- An HTTP/1.1 JSON adapter (`fgdbd serve --http-listen`) over the same execution path, with a Host allow-list.
- Live subscriptions: `SUBSCRIBE TO <read>` over FGP pushes the engine's maintained-query baseline and exact per-commit deltas as SUBSCRIPTION_BATCH frames under flow credit (`fgdb remote subscribe`). They require an unrestricted-scope capability (maintained queries are not capability-masked yet) and are in-process only: no durable resume across reconnects.
- Signed subscription budgets (2026-10-10): preparation reserves one work grant and divides its remainder across the native circuit, replay sink, initial compressed baseline and checked wire conversion. Native work and scratch control events share each reservation. A compiler-shaped prepass counts circuit nodes and obtains graph-source widths from the admitted maintainer shape; conservative work ceilings bound graph-node admissions without claiming exact Warden hooks in native maintenance. Private intermediate rows retain server ceilings, while signed final-row limits govern the entire compressed delivered bag, including cached and unacknowledged batches. A later poll shares one reservation across the normal pull, a possible `ReplayGap`/`DeltaGap`/`DeltaUnavailable` replacement baseline and conversion. Registered maintenance keeps its original bounded policy. Setup, replay, baseline and final host admission form one rollback boundary; refusal removes only the private registry suffix and restores the registration count, and no handle escapes before acceptance. Source-free grouping works with a zero signed graph-node grant. New native and server regressions cover footprint bounds, rollback, zero grants, pending redelivery, retained maintenance limits and replay eviction; they remain unexecuted because the local process service is unavailable. These are per-registration/tick/poll ceilings, not lifetime accounting, capability masking or durable resume.

- The Bolt-compat surface (`fgdbd serve --bolt-listen`, crate `fgdb-bolt`): official Neo4j drivers run autocommit reads, bounded autocommit writes and explicit read transactions over Bolt 5.0 under a capability token. Nodes, relationships and paths include capability-visible metadata from the same session and selected historical sequence; paths preserve direction and repeated identities, also inside collections. Writes inside explicit transactions refuse with `Neo.ClientError.Statement.AccessMode` before graph access. Autocommit writes execute once on PULL/DISCARD; graph-valued write RETURN and its current qualification limits are described above. The base driver surface was verified with the official Python driver (`execute_query`, `execute_read`, sessions, `neo4j://` routing); graph-value coverage lives in `tests/loopback.rs` and the adapter's unit tests.

Not yet: durable result retention with ACK/release/resume, durable transaction preparation, explicit multi-statement write transactions and reattachment, capability-masked and durable subscriptions, multi-tenant admission/QoS, the complete Bolt V2 contract, and the HTTP/2, gRPC and WebSocket adapters. Names resolve through operator-declared bindings because there is no durable catalog.

## Query evidence tower

| Evidence | Binds | Does not bind |
|---|---|---|
| `GqlCertificate` | exact statement bytes, canonical `RelationBind`, snapshot sequence | parsed plan, rows, transaction overlay, physical plan, runtime cost |
| `GqlPlanCertificate` | every current `BoundPlan` field and snapshot sequence under transcript v2 | statement spelling, rows, transaction overlay, physical plan, runtime cost |
| durable ordered-result digest | plan digest, snapshot, exact row count, order, every returned `VId` | physical plan, cost, authorization, envelope framing |
| staged-effect digest | transaction basis and canonical staged `LogicalDeltaTemplate`, or explicit empty overlay | durable snapshot, staged bytes as payload, API-call history |
| `GqlOverlayResultCertificate` | basis, plan digest, staged-effect digest, exact row count, order, every returned `VId` | durable/staged payloads, conflict state, standalone replay |
| query-execution stats | admitted table count and final row count | cryptographic attestation, I/O/allocation cost, operator work, wall time |

The v2 plan transcript includes `BoundPlan::neq`, omitted by historical v1. New certificates use v2; legacy-v1 verification is explicit.

### Exact staged-overlay result evidence

`WriteTxn::staged_effect_digest` binds the basis and canonical semantic net effect under `fgdb:write-txn-staged-effect:v1`.

`execute_prepared_query_with_overlay_result_certificate` executes first and then issues exact rows, a plan certificate, and `GqlOverlayResultCertificate`. The result certificate binds basis, plan digest, staged-effect digest, row count, order, and every returned `VId`.

`verifies_prepared_query_overlay_result` derives the current plan and staged-effect identities from the live transaction. A later staged mutation invalidates old evidence. Equivalent transactions at the same basis with the same canonical net effect can verify the same certificate.

This is exact in-process evidence. It is not standalone replay because the certificate does not carry the durable snapshot, staged template bytes, graph rows, read-set state, or conflict state.

## Strict evidence envelopes

The live tree has two canonical **unreleased application envelopes**:

- `GqlPreparedResultArtifact` for one durable prepared-query result;
- `GqlOverlayResultArtifact` for one staged-overlay result.

The common v1 framing is owned by `crates/fgdb-gql/src/evidence_artifact.rs` and contains:

- eight-byte magic `FGQEVID1`;
- explicit major/minor version;
- a closed artifact-kind tag;
- zero-required reserved bytes;
- exact snapshot sequence or transaction basis;
- statement, canonical bind, and plan digests;
- staged-effect digest for overlay artifacts;
- exact row count and ordered `VId` bytes;
- the applicable exact result digest.

Decoding fails closed on invalid magic, unsupported versions, wrong kind, nonzero reserved bytes, row-count or length overflow, every truncated prefix, trailing bytes, and an inconsistent result transcript. Artifact fields are private and rows are redacted from `Debug` output.

### Resource-safe evidence admission

`GqlEvidenceLimits` is policy applied before untrusted artifacts allocate their row vectors. It bounds:

- total encoded bytes;
- declared row count.

`GqlEvidenceLimits::DEFAULT_UNTRUSTED` is an application default, not a format maximum or product SLO. Callers may supply a different policy. Exact limits succeed; one-below limits return `GqlEvidenceLimitExceeded` with the dimension, configured limit, and observed value.

Malformed headers retain the strict decoder's existing syntax errors. A valid header with a hostile declared row count is refused before row allocation.

Canonical owner: `crates/fgdb-gql/src/evidence_limits.rs`. Product adapters live in `crates/fgdb/src/write_txn_parts/evidence_limits.rs`.

### Durable issuance and audit

`Database` and `EmbeddedReadView` expose:

- `execute_prepared_query_artifact[_at]`;
- `audit_prepared_query_artifact`;
- `audit_untrusted_prepared_query_artifact`;
- `audit_prepared_query_artifact_with_limits`.

A resource-safe durable audit:

1. checks encoded-byte and declared-row policy;
2. strictly decodes the envelope;
3. verifies the retained statement and canonical bind;
4. recomputes the canonical plan certificate at the artifact sequence;
5. cross-checks the independently decoded result transcript against that certificate;
6. re-executes the query at the exact historical sequence;
7. requires exact ordered-row equality.

A later write may advance the live frontier without invalidating an older artifact: audit reopens the sequence named by the artifact rather than sampling current state.

### Staged-overlay issuance and audit

`WriteTxn` exposes equivalent issuance and resource-safe audit for `GqlOverlayResultArtifact`. A staged audit additionally verifies the current transaction basis and canonical staged-effect digest before re-executing the overlay. A later staged mutation returns `StagedEffectMismatch`.

## Result-bound evidence paging

The current tree supports deterministic paging over an **already materialized and audited** evidence artifact.

### Token identity and framing

`GqlEvidencePageToken` has a fixed-width v1 encoding owned by `crates/fgdb-gql/src/evidence_page.rs`:

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

The token binds:

- artifact kind;
- exact snapshot sequence or transaction basis;
- complete ordered-result digest;
- next row offset.

The checksum is unkeyed. It detects accidental or unsophisticated mutation but is **not** a MAC, signature, authorization decision, capability, or publisher-authenticity proof.

Strict token decoding rejects wrong length, trailing bytes, invalid magic, unsupported version, unknown kind, nonzero reserved bytes, and checksum mismatch. Unit tests walk every truncated prefix.

### Page object

`GqlEvidencePage` contains one contiguous row slice and explicit progress metadata:

- `start_offset`;
- `end_offset`;
- `total_rows`;
- `remaining_rows`;
- optional next token;
- terminal status.

Rows are redacted from ordinary `Debug` output.

### Product-level audit-and-page order

`Database`, `EmbeddedReadView`, and `WriteTxn` expose default-untrusted and caller-limited audit-and-page methods. Their order is deliberate:

1. reject zero page size;
2. strictly decode and checksum-check optional fixed-width token bytes;
3. enforce artifact byte and declared-row policy before row allocation;
4. strictly decode and verify the artifact transcript;
5. verify prepared input, plan, snapshot/basis, and staged effect where applicable;
6. re-execute the exact historical or staged query and compare ordered rows;
7. bind the token to kind, sequence/basis, result digest, and offset;
8. return the contiguous slice.

This avoids expensive replay for an intrinsically invalid page request while preventing a valid token from bypassing artifact admission or replay.

### Exact paging boundary

This is stateless resumability over materialized evidence. It is **not**:

- a database cursor;
- operator streaming;
- bounded-buffer flow control;
- backpressure;
- a session or lease;
- cancellation-aware incremental execution;
- authentication or authorization;
- a reduction in full artifact decode/replay cost.

Every product-level page call re-audits and replays the complete artifact before returning its slice. A genuine cursor requires explicit owner/session identity, lease and cancellation semantics, bounded buffers, backpressure, and an execution/storage path that can stop before materializing the full result.

## Agent-operability verticals

### Exact-SHA local context capsules

`scripts/agent_context.sh` emits a credential-free advisory package containing an exact commit/tree, one-head Git bundle, deterministic source archive, history and tracked-file inventories, stable clean/dirty worktree evidence, and truthful Beads observation modes.

The v2 verifier imports the bundle into an isolated repository and recomputes the tree, archive, paths, and recent history. `agent_context_checkout.sh` materializes a verified detached checkout with no remote.

### Exact-tree local proof bundles

`scripts/local_proof.sh` captures `bash scripts/check.sh` on one clean exact tree and preserves the raw exit, stdout/stderr, tool versions, committed tree, tracked gate-driver blob, and before/after worktree state.

Verdicts remain three-valued: stable `pass`, stable `red`, and moving-tree `void`.

The producer follows one evidence lifecycle: **observe → exclusively claim output → execute → observe → checksum → publish**. Failed state capture, a failed directory claim, incomplete inventory enumeration, or failed checksumming aborts collection with nonzero exit and retained diagnostics. Collection failure is not a fourth completed-proof verdict and cannot be relabeled as `void`: `void` records observed state movement, not a missing observation. No completed `LOCAL_PROOF_*` marker is emitted after a collection failure, and a failed directory claim must not write into another producer's evidence.

`scripts/local_proof_verify.sh` independently validates the complete inventory, checksums, manifest, and reporting contract. Shell statuses must be canonical decimal integers in `0..255`; captured after-state object IDs must be complete single-line Git IDs. Existing format-v1 reading and format-v2 production remain separate from these semantic checks.

**Verified is not green.** A consistent `red` or `void` bundle also passes independent verification. A landing needs a verified `pass` bundle bound to its intended source tree, `check_exit=0`, and `tree_stable=true`. A stable check exiting `125` remains `red`; observed tree movement produces `void` with wrapper exit `125`. Neither wrapper exit `125` nor verifier exit `0`, alone, identifies a green run.

The failure-path witness is `bash scripts/local_proof_selftest.sh`: it uses retained temporary Git repositories and fake check commands to exercise capture and verification without compiling Rust. It is not the repository-wide quality gate. Checksums establish internal consistency, not producer identity; before/after snapshots alone do not prove the absence of every transient edit between observations.

## Agent source-ownership map

| Concern | Canonical owner |
|---|---|
| durable database composition and temporal graph reads | `crates/fgdb/src/lib.rs` |
| one exact-sequence GQL read kernel | `crates/fgdb/src/gql_exec.rs` |
| parser, binder, `RelationBind`, `BoundPlan` | `crates/fgdb-gql/src/parser.rs` |
| typed statement parameters | `crates/fgdb-gql/src/parameters.rs` |
| `PreparedGqlQuery` and query-execution budgets | `crates/fgdb-gql/src/prepared.rs` |
| staged-result transcript | `crates/fgdb-gql/src/overlay_evidence.rs` |
| strict evidence framing and independent decoder | `crates/fgdb-gql/src/evidence_artifact.rs` |
| evidence byte/row admission | `crates/fgdb-gql/src/evidence_limits.rs` |
| result-bound page tokens and slices | `crates/fgdb-gql/src/evidence_page.rs` |
| owned prepared execution adapters | `crates/fgdb/src/write_txn_parts/owned_prepared.rs` |
| canonical staged-effect authority | `crates/fgdb/src/write_txn_parts/overlay_evidence.rs` |
| artifact issuance and replay audit | `crates/fgdb/src/write_txn_parts/portable_evidence.rs` |
| artifact resource admission adapters | `crates/fgdb/src/write_txn_parts/evidence_limits.rs` |
| audit-and-page adapters | `crates/fgdb/src/write_txn_parts/evidence_page.rs` |
| durable certificate authority | `crates/fgdb/src/gql_cert.rs` |
| staged transaction overlay and conflict tracking | `crates/fgdb/src/write_txn_parts/` |
| independent semantic oracle | `crates/fgdb-reference` |
| crash/differential harness | `crates/fgdb-sim` |
| exact-tree proof capture and publication | `scripts/local_proof.sh` |
| independent proof-bundle validation | `scripts/local_proof_verify.sh` |
| proof capture/validation fault witnesses | `scripts/local_proof_selftest.sh` |

Do not add a second parser, binder, execution kernel, session type, or issuing certificate authority merely to make a narrow test pass. Independent artifact decoding is allowed only while product audit cross-checks the canonical certificate transcript, as the current path does.

## Focused witnesses and tests

```bash
cargo run -p fgdb --example open_a_database
cargo run -p fgdb --example gql_owned_prepared
cargo run -p fgdb --example gql_evidence_artifact
cargo run -p fgdb --example gql_evidence_limits
cargo run -p fgdb --example gql_evidence_pages
cargo run -p fgdb --example gql_txn_overlay_result_evidence
```

Focused integration suites include:

- `embedded_read_session.rs`;
- `gql_prepared_at.rs`;
- `gql_plan_certificate_refusals.rs`;
- `gql_aligned_certificates.rs`;
- `gql_result_digest.rs`;
- `gql_owned_prepared.rs`;
- `gql_txn_overlay_result_evidence.rs`;
- `gql_evidence_artifact.rs`;
- `gql_evidence_limits.rs`;
- `gql_evidence_pages.rs`.

## Validation status

The repository-wide authority remains:

```bash
bash scripts/check.sh
```

Every cargo gate in that chain runs `--locked`, so a manifest edit landed without regenerating `Cargo.lock` reds the gate with cargo's own message instead of being rewritten silently. The verdict for a tree is the exit code of that chain executed on that exact tree, captured by `scripts/local_proof.sh` and independently checked with `scripts/local_proof_verify.sh --repository <checkout> <proof-directory>`. Per `AGENTS.md`, automatic hosted CI is not a substitute for this local evidence; do not assume a scheduled or push-triggered run supplies a missing verdict.

A paragraph in this file or in `CHANGELOG.md` describing checks that were not run is not a verdict and must not be read as one. Between 2026-08-31 and 2026-09-02, 141 commits were landed from an environment without the pinned toolchain; the tree did not compile for 33 of them, and the accompanying prose said so instead of refusing. That is recorded as NE-0045 in `docs/NEGATIVE_EVIDENCE.md`, and the repair is `b51e3232` plus the commit that lands this paragraph.

## Major remaining systems

The following remain incomplete or absent:

- canonical parameter evidence for the typed parameters that have landed;
- typed rows/columns and genuine streaming cursors with backpressure;
- full session ownership, authorization, renew/expiry/reattach, and synchronous facade;
- full SSI, predicate/range conflicts, merge ladder, and general multi-relation transactions;
- a registered, compatibility-governed evidence format and external verifier SDK;
- standalone staged-overlay replay carrying the staged template and snapshot authority;
- storage/operator-level early resource enforcement;
- full ISO GQL, GLA lowering, Loom operators, optimizer, spill, and larger-than-memory execution;
- Strata tiers I/R/A and production compaction/migration policy;
- the Ripple crate, Fabric, and maturation of the 2026-09-21/22 crates (`fgdb-order`, `fgdb-policy`, `fgdb-prism`, `fgdb-repl`, `fgdb-warden`, `fgdb-beacon`) into their full planned subsystems;
- the server's remaining surfaces (TLS, durable result machines, explicit transactions, subscriptions, adapters), Python bindings, packaging, signed releases, installer, and upgrade tooling (the robot CLI binary and the `fgdbd` FGP subset have landed — see above).

## Dependency-ordered next work

1. Establish a trustworthy evidence baseline before further feature landings: run the pinned toolchain through `scripts/local_proof.sh`, independently verify the complete bundle against the intended checkout, and distinguish a verified green result from a verified red/void result or aborted collection. Focused proof-wrapper tests do not discharge the whole-tree gate.
2. Typed parameters landed as structural parser operands (`crates/fgdb-gql/src/parameters.rs`); bind canonical parameter values into preparation identity, certificates, and evidence envelopes.
3. Decide whether the application envelope and page token graduate into registered compatibility contracts; freeze ceilings and golden vectors first.
4. Build a real cursor/session owner with cancellation, lease, bounded buffering, and backpressure.
5. Move resource enforcement into storage/operator admission before full table/result materialization.
6. Package staged template bytes and exact snapshot authority before claiming standalone transaction replay.
7. Continue into full SSI and GLA/Loom lowering.

## Recent evolution

- `92e39e59` — one exact-sequence execution kernel for database and immutable read views.
- `01f28d39` through `65d3d832` — verifiable input/plan/result evidence.
- `005a8397` — prepared transaction-overlay execution without rebinding.
- `c7a0558e` through `78137cfb` — owned preparation and deterministic query budgets.
- `80de85a6` through `3e8ff789` — exact staged-effect and staged-result evidence.
- `97d09787` through `10871100` — strict application envelopes and replay audit.
- `a3441930` through `f004bd1c` — pre-allocation evidence byte/row admission.
- `901e4ef5` through `49adac5f` — result-bound stateless paging, audit adapters, progress metadata, and request-preflight laws.
- `9adf484d` — 2026-09-08 full verification (9/9 core, 40/40 registered) with the same-window repair series. Not an ancestor of `main` (NE-0046); not proof of any `main` commit.
- `79548aca` — 2026-09-17 robot CLI binary with frozen NDJSON robot contract.
- `c0f0740f`, `f8c37559`, `ee4d964d`, `6ae9747f`, `b2d7761c`, `2f6abad0`/`d390c6bd` — FreeJoin relational circuits, six-kind checked joins, WHERE/equality-probe fusion, CSV incremental decode, Prism sealed-kernel shortest paths, Aegis payload-survival math.
- 2026-09-21/22 — seven new workspace crates (`fgdb-cli`, `fgdb-order`, `fgdb-policy`, `fgdb-prism`, `fgdb-repl`, `fgdb-warden`, `fgdb-beacon`); milestone closures `fgdb-w4-g1-txn-core-qpmg` (reopened the same day; open) and `fgdb-boundplan-gla-lowering-seam-r2kd`.

Keep this file synchronized whenever a capability crosses from plan or red-bar acceptance test into an inhabitable public path.
