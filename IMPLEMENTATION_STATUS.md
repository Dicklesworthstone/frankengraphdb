# FrankenGraphDB Implementation Status

Capability baseline: unreleased `main`. Each section carries the date it was last re-derived from the code; the latest source review is 2026-11-22 (qualification is stated separately).

This document is the compact, agent-facing map of **what executes now**, **where each live behavior is owned**, **what its evidence proves**, and **what remains target architecture**. The comprehensive plan remains normative for the finished system; this file records the present inhabitable subset.

## Product reality

FrankenGraphDB is not yet a released graph-database product. There are no tagged releases, supported installer, CLI/server distribution, Python package, or compatibility promise. The live product surface is an embedded Rust composition crate over real Chronicle durability and real Strata tier-D storage, with a deliberately bounded GQL read slice and a bounded write-transaction overlay.

The repository contains real durable mechanisms and executable cross-layer paths. It does **not** yet contain the complete W10 surface, full ISO GQL, GLA/Loom physical execution, full SSI, or the distributed and incremental system described by the plan.

## Live product verticals

### Authorized mixed demand retrieval — qualification pending

Authenticated native CALLs can now join demand-driven graph traversal to
compatible maintained text/vector lanes. The existing complete-incidence and
complete-field visibility checks run before any registry lookup. Reuse requires
the selected snapshot's exact frontier and an exact matching index definition;
missing, historical, mismatched, approximate or failed-maintenance generations
refuse without rebuilding a corpus or switching the requested source profile.

The complete live vertex population spends the original signed node and native
snapshot-record allowances once. Demand membership and incidence probes retain
their work/scratch admission without charging the same population twice.
Eligibility, empty output, k=0, cancellation and final authority checks remain
active. Masked graph-only queries and staged transaction behavior are unchanged.
This avoids query-time corpus construction; it does not establish a process-RSS
bound, change the default database-open path or prove a measured speedup.

Four embedded regression sources cover pinned/prepared/current reads, index
removal, edge/property changes, complete-population quotas, cumulative UNION
budgets, incompatible/missing generations and each authority mask independently.
A served-database regression compares literal text/vector/graph ranks with the
existing indexed profile, saves a checkpoint at sequence 9, replaces an edge at
sequence 10, then reopens with zero source-staging allowance and checks the new
neighbor. Source review caught and corrected a test grant which otherwise would
have inherited the production constructor's deny-all default scopes.

Pinned formatting and whitespace checks passed on the final corrected sources;
two independent reviews found no further source-level issue. Compilation,
runtime assertions, strict Clippy, UBS and complete exact-tree proof remain
UNRUN after dependency-fetch failures, disk exhaustion and executor loss.
Qualification is recorded locally as `fgdb-authorized-mixed-demand-nql3y`,
related to `fgdb-w7-hybrid-99if` and `fgdb-rc1111-durable-watermarks-c1g98`;
the integrated gate remains `fgdb-qualify-integrated-nov22-maz0p`.

The exact-ID Beads export first ran out of space. After relocating the same
checkout, the available exporter rewrote unrelated existing records and the
attribution guard rejected the export. Both new records remain in progress
locally. The rejected JSONL was not published, and no gate bypass or closure ran.

### Capability-scoped Bolt default database — qualification pending

Implicit Bolt database selection now counts only selectors admitted by the
connection's live capability. Adding an inaccessible database therefore no
longer makes an otherwise valid default RUN, BEGIN or ROUTE ambiguous. Explicit
names keep the same authorization check; zero or multiple admitted selectors
keep the uniform missing-or-unauthorized refusal.

Two regression sources compare complete query, transaction and routing
transcripts before and after adding hidden selectors on both sides of the
visible name. They cover omitted, null and empty defaults, explicit hidden and
missing names, multiple admitted selectors, an empty server, malformed values
and retired authority. Two independent source reviews, the pinned 2026-08-31
formatter and `git diff --check` passed. Rust compilation, runtime tests,
Clippy, UBS and exact-tree proof remain UNRUN. The pinned toolchain was found
outside PATH. Cargo exited 101 before compiling this project: the required
asupersync revision was absent offline and its online Git fetch repeatedly
failed with invalid index-pack output while the workspace was out of space.
The first full-gate attempt detected concurrent source movement and returned
nonzero; the subsequent stable-tree attempt lost its executor before a
terminal verdict.

Qualification remains open in `fgdb-bolt-visible-default-q97kg`, related to
`fgdb-w9-enforcement-j0fg`; the integrated gate stays tracked by
`fgdb-qualify-integrated-nov22-maz0p`. This is a database-selection repair,
not a claim of complete server noninterference.

### Bounded post-seal overlays and borrowed property admission — qualification pending

The authenticated Tier-R/Tier-D overlay now bounds distinct suffix statements
before inserting winners, and bounds each retained or visible descriptor map
before allocation. Restatements reuse the same winner slot. Property rows stay
borrowed during winner selection. Each retained winner passes cumulative
canonical-byte admission before one owned payload is shared by outgoing,
incoming and retained views.

Materialization feeds complete source descriptors into the existing incremental
image builder in canonical order. It checks combined incidence, descriptor and
property-byte allowances before each decoded row copy, releases completed
descriptor buffers, and observes cancellation inside delta-only merge loops.
The final image and anchor retain the ordinary sealer's encoding and scope.
Shared property-row admission also uses the existing scalar and collection size
contracts instead of allocating a serialized patch merely to measure it.

Seven added regression tests cover literal and encoded byte counts across property
formats, exact/insufficient limits, restatement coalescing, both traversal faces,
future retained versions, shared payload ownership, independent retained-history
and image parity, and every observed build/materialization interruption point.
Two independent source reviews found no blocking production issue. Compiler,
runtime, Rustfmt, Clippy, UBS and exact-tree qualification are UNRUN: the execution
environment became unavailable before toolchain installation and a later retry
reported no usable execution environment.

These are explicit population and canonical-payload bounds. The complete
operation still has no MemoryPool/RSS guarantee or spillable descriptor builder;
existing encoded-image and incoming-index workspace semantics are unchanged.
Work remains open under `fgdb-rc1111-persistent-tier-r-7hsan`,
`fgdb-w3-buffer-g2v`, `fgdb-w3-properties-gou`, and
`fgdb-qualify-integrated-nov22-maz0p`.

### Correlated filters and indexed authorized writes — qualification pending

Source commits `004a14b`, `1ee4a2b` and `2d264e0` repair three live
preparation and write-selection seams.

Native write-script preparation now preserves the first work, expiry or
cancellation refusal from representative binding. Ordinary argument errors
still pass live controls before disclosure, and successful preparation keeps
its complete original input and binding bill. The prepared same-value SET
regression now follows the plan's write terminal contract: each success advances
once with empty graph effects; refusals leave the frontier unchanged. Exact
single/batch UTF-8 budget boundaries remain asserted as history grows.

The existing refused-WHERE retry now follows inline CALL scopes, including
explicit, legacy, empty and nested imports. Imported scalar comparisons can
therefore reach the native row-filter and aggregate machinery. Working keyed
graph predicates keep their existing lowering, and OPTIONAL MATCH filtering
remains inside Apply before null extension.

Unstaged authorized write overlays now reuse the existing visible, total
equality-selection proof and the native indexed candidate owner. The overlay
continues retaining positive, absent-key and incomplete-attempt observations.
Candidates still pass normal masking and shared allowance admission. Staged,
hidden-predicate and unsupported plans retain their prior source path.

Regression sources cover literal bags, nullable and duplicate imports,
parallel edges, optional COUNT/NULL behavior, scope errors before catalog
access, independent full-scan results, overlapping/absent roots, hidden
postings, exact quotas and conflict witnesses after refused reads. The original
`authorized_unwind_write` frozen-update assertions are unchanged.

Pinned formatting of all nine changed Rust files, whitespace checks, static
UBS scans (zero critical findings) and independent source review completed.
Locked dependency fetch and offline metadata resolution completed. The initial
serialized server build was interrupted during dependency compilation; the
later GQL test build lost its executor before a first-party or runtime verdict.
Runtime tests, strict Clippy and complete exact-tree proof remain **UNRUN**.
The initial workspace-format check's reported changed-file diff was corrected,
but the whole-workspace check was not rerun. The local Beads status edits could
not be exported through `scripts/br_sync.sh` after executor loss.

Qualification remains open under `fgdb-qualify-integrated-nov22-maz0p`,
`fgdb-prepared-write-budget-kg0hx`,
`fgdb-filtered-count-subscription-7r618` and `fgdb-irqu2`.
No measured speedup, general staged-index capability or workstream completion
is claimed.

### Maintained native directed reachability — qualification pending

Native registration recognizes one uncaptured, unbounded, directed TRAIL with
exactly one-or-more hops and DISTINCT endpoint pairs. It composes the existing
duplicate-preserving one-hop row source, deletion-safe Closure, and endpoint
alias/permutation projection. Forward/reverse traversal, relation unions and
wildcard types retain their ordinary source semantics. Capability masking
precedes recursion; the compiler does not run an eager path enumeration after
each commit or replace the existing reachability kernel.

Preflight accounts for all three owners. Private parallel-edge occurrences
retain source/work/scratch limits without consuming the final reachable-pair
row allowance. Construction and rebuild preserve the existing atomic staging
guard. Undirected trails, bags/captures, zero-hop/minimum-above-one/bounded
intervals, predicates and pattern-local order/pages retain their refusal.
Typed `register_standing_rows` is unchanged. This is session-local maintained
state, not a durable subscription or complete recursion workstream.

Public regression sources compare independent BFS, eager TRAIL and exact
delivered bags/deltas across twelve snapshots, parallel-edge removals, cycles,
self-loops, full-width identities, relation changes, vertex deletion and
rebuild. An authenticated hidden-bridge test compares a scoped subscription
with a separate visible-only graph and a live unscoped control. A private
regression covers every staged construction/rebuild interruption, final-row
quotas, commit-time fencing and recovery with a sufficient rebuild budget.

All seven changed Rust files pass the pinned formatter and whitespace checks.
Compiler/runtime and full integrated-tree qualification remain UNRUN after an
executor outage during dependency compilation. Public laws live in the
`trail_text` and `native_standing_sets` integration targets; the injected
atomicity law remains a library test. `fgdb-w6-recursion-x78` and
`fgdb-w6-zset-circuits-kjv` retain their broader scope and open dependencies.

### Boolean clauses in composable hybrid retrieval — qualification pending

The canonical exact-RRF request now accepts the existing bounded Boolean BM25
query as its text lane. Native `CALL hybrid.search` binds required, optional and
excluded clauses plus an optional-term minimum, borrowing admitted argument
storage. It composes these candidates with vector and graph lanes through the
same ranking owner, source admission, endpoint eligibility, capability masks,
maintained-index selection and staged transaction overlays as ordinary text.
Exclusion removes text support; it is not an authorization or all-lane filter.
Ordinary text/fuzzy modes and Boolean clauses are mutually exclusive on an
active text lane. Disabled lanes retain their prior no-evaluation contract.

The standalone CLI uses the same request for Boolean/vector/graph combinations.
Boolean-only CLI output keeps ordinary BM25 scores; native calls and fused CLI
output retain exact-RRF Decimal scores and separate ranks/scores for each lane.
Purely negative, empty, contradictory and impossible-minimum Boolean queries
do not become corpus scans. Active clauses share query byte/term limits even
when zero output or empty eligibility suppresses all result rows.

Regression sources cover independently specified membership and lane support,
exact score/rank parity, eligibility before candidate limits, pinned updates,
every kernel cancellation checkpoint, native parameters/property composition,
transaction savepoint rollback and commit, and hidden-corpus noninterference
with a maintained privileged index. Compiler/runtime and integrated-tree
qualification are being tracked under `fgdb-w7-hybrid-99if`; the broader W7
workstream and compositional answer-contract obligations remain open.

### Optional correlated aggregate calls — qualification pending

The maintained correlated-summary circuit now admits `OPTIONAL CALL` for its
existing reader-free global aggregate subset. It completes each distinct
invocation's empty global group before applying ordered row projections,
filters and DISTINCT, then restores the original outer occurrences. An empty
COUNT yields zero; a summary removed by a later filter instead yields optional
NULL exports. A surviving all-NULL summary retains its private invocation key.
Expression failures remain errors, and a tail removed by an earlier filter is
not evaluated again on the fabricated optional row.

Post-group work uses the distinct invocation domain. Private keys survive
projections and DISTINCT; a key-only completion input preserves the previous
width boundary for repeated imports. Native registration and its resource
preflight account for all additional owners. Existing admission of the original
outer source is unchanged. Aggregate-internal exact-domain HAVING/output
transforms, per-invocation pages, grouped summaries and reader/ordinal effects
retain their prior refusal.

Regression sources add 1,024 independently calculated optional weighted
transitions, no-import activation, duplicate and nullable invocations, first/last
support retractions, exact delivered deltas, rebuild, cancellation checkpoints,
width limits and atomic quota refusals. Ordered LIMIT 0/1 error cases have
matching successful controls and require the exact arithmetic failure. The
concurrent inner-call continuation tests remain present, with their inconsistent
WriteBatch match-arm types repaired.

Pinned changed-source formatting and whitespace checks pass, and independent
source review is complete. Compilation, runtime assertions and full exact-tree
qualification remain pending under `fgdb-w6-zset-circuits-kjv` and
`fgdb-qualify-integrated-nov22-maz0p`; no W6 completion or measured performance
claim is made.

### Maintained aggregates, streamed subscriptions, and recovery integrity — qualification pending

The correlated incremental compiler now admits ordered, reader-free projections
and filters after global aggregate tuples. Empty COUNT/NULL restoration,
complete-tuple overflow checks, outer invocation identity and bag multiplicity
precede those stages. Subquery-local DISTINCT remains per invocation. Explicit
aggregate-owned wide-domain HAVING, grouped correlated summaries and effectful
stages remain outside this slice; the implementation uses the existing
incremental operators rather than whole-query replay.

The FGP SDK exposes `ChangeChunk` and `subscribe_streaming`; the CLI consumes
and flushes each validated frame before requesting the next. A callback refusal
cancels once, suppresses later callbacks/grants and drains with protocol checks
intact. Application checkpoints advance only on accepted complete batches.
The collected API deliberately retains a complete batch. These are session-local
checkpoints, not durable reconnect/resume tokens, and the robot schema is unchanged.

Beacon checkpoint admission now reconciles current text postings and vector lanes
with live documents, enforces segment generation order, rejects count-derived
allocations unsupported by remaining bytes, and preserves legitimate Unicode
lowercase expansion and stale segments. Strata overlays now require the sealed
base and suffix to share the admitted root, publication and verified horizon.
Only the authenticated store walk can mint a suffix receipt; caller-owned
reopened tuples cannot manufacture one. A carried run may retain its older
original source root. This closes an authority gap, not the remaining overlay
memory-admission work.

Regression sources cover corrupted postings, scores and phrases after restore,
Unicode expansion, cancellation/truncation, foreign-root overlays, subscription
credit windows and sink failures, 1,024 weighted aggregate transitions, native
commit deltas, rebuilds and quota rollback. Independent source reviews found no
blocking issue. Pinned formatting and whitespace checks completed for Beacon,
Strata and subscriptions; the aggregate whitespace check completed, but its
formatter terminal status was lost during executor failure. The focused Beacon
Cargo test exited 101 while retrieving pinned asupersync (HTTP/2 truncation, then
proxy CONNECT 403), before compilation or assertions. Compiler/runtime and
exact-tree qualification remain UNRUN.

The ordinary-build source errors identified in the November 22 audit are also
repaired: CALL UNION scanner errors use the declared pattern-error type, and the
Prism test checksum handles the Boolean result variant. This has not yet been
confirmed by a completed compiler run. Follow-up remains open under
`fgdb-gql-prism-compile-gd65n`, `fgdb-qualify-integrated-nov22-maz0p`,
`fgdb-rc1111-persistent-tier-r-7hsan`, `fgdb-rc1111-durable-watermarks-c1g98`,
`fgdb-w6-sub-delivery-qol`, and `fgdb-w6-zset-circuits-kjv`.

### Intermediate DISTINCT in resident aggregate streams — qualification pending

Vertex and edge aggregate cursors now admit scan-local WITH DISTINCT stages
before reduction, including multiple stages separated by UNWIND. Each stage
retains its own set of complete native tuples across source roots and cooperative
suspensions. NULL cells remain part of tuple equality; lists, maps and columns
discarded by a later projection remain part of the DISTINCT key. The complete
projection evaluates before membership, so duplicate elimination cannot hide an
expression error. Subsequent UNWIND stages retain their own bag multiplicity.

Support comparisons and retained recursive copies share the cursor's cumulative
work/scratch meter. Exhaustion, close and failure release support. This is a
resident, budget-governed implementation; the stateless spill/async adapter still
refuses these stages. Intermediate ordering/pages, graph-reading or ordinal-
dependent expressions and order-sensitive COLLECT keep their prior admission.

Regression sources cover both cursor families, batch parity and literal
answers, repeated stages, complete native tuples, grouped output, pinned native
views, cooperative resume, close/cancel cleanup, late errors under LIMIT 0/1,
and exact/insufficient quotas. Pinned changed-source formatting and whitespace
checks pass. Static-only UBS exited 1 on the existing test-only source-bypass
panic; no new production critical finding was identified. Runtime and exact-tree
qualification remain open under `fgdb-w5-executor-olp`.
The pinned single-job compiler attempt exited 101
because rustc was killed by SIGKILL while compiling the unchanged asupersync
dependency, before first-party compilation or runtime assertions.

### Streamed conflict validation and schema allocation — qualification pending

Prepared writes and strict transaction completion now authenticate omitted
Chronicle history one batch at a time and borrow the already-loaded suffix.
They preserve the database's retained delta window and pinned generations.
Prepared FCW retains last-writer and adjacency summaries only for the exact
preparation's dependencies, including normalized guards, ensure aliases,
created-edge endpoints and cascade/deletion dependencies. Unrelated intervening
writers no longer populate that validation map. The history is still traversed.

Strict completion reuses the existing point, negative, count and scan witness
logic. Its earliest conflict remains private until the complete source interval
authenticates; prepared validation retains latest-writer diagnostics and its
canonical conflict precedence. A source failure, cancellation or dropped future
cannot publish a partial candidate. Semantic rebase still uses explicit history
materialization. Strict writes retain the sole ordinary prepared-commit path,
which may authenticate an omitted prefix a second time; no cold-write latency
improvement is claimed.

Automatic schema-name allocation uses the same streaming traversal to reserve
numeric IDs from removed fields and deleted elements. Origin replay also
authenticates historical schema objects and transitions, keeping only its
current private catalog and checking the final reference against the selected
snapshot. It does not install historical schema generations or lower the delta
floor. Traversal reports an encountered source failure before semantic conflicts;
the order between multiple corrupt source objects follows the streamed walk.

Regression sources cover bounded FCW summaries against the full validator,
lazy reopen, mixed omitted/resident history, exact conflict witnesses, late
corruption, cancellation, dropped futures, successful write/read completion,
missing/corrupt prior schema objects and clean allocation retry. Pinned
changed-source formatting, whitespace checks and static-only UBS pass. UBS ran
without Cargo/Clippy and supplies no compiler or runtime verdict. Compiler, runtime and
exact-tree qualification remain open under the broader `fgdb-agp1o`,
`fgdb-e8gr4`, `fgdb-d5vo4` and `fgdb-7r8sy` work. One replay batch bounds payload
retention, not total handle memory: current catalogs, snapshots, dependency sets
and the existing verification diagnostics remain owned.

### Maintained correlated aggregate tuples — qualification pending

Reader-free correlated global summaries now admit a complete tuple of COUNT,
integer SUM and AVG, including argument DISTINCT, plus typed MIN and MAX.
The compiler retains the exact ordered body filters and projections, including
multiple relational DISTINCT stages. Each DISTINCT key includes the private
invocation identity even when the public projection drops imported columns.
The circuit groups the actual complete shared source and restores every original
outer occurrence. Duplicate outer arguments cannot multiply an aggregate.
The existing native grouping owner maintains weighted support and retractions;
updates do not re-execute each correlated query.

Only COUNT receives integer zero-fill. Empty and all-NULL non-COUNT summaries
retain NULL, AVG retains its native float result, and extrema retain their
original value domains. The optimized COUNT and collected-count recipes remain
COUNT-only. Every aggregate output is converted before publication, including
values hidden by a later public projection or LIMIT 0. Unsupported reader
effects, per-invocation pages, grouped summaries and transformed aggregate
outputs retain their conservative refusal.

Regression sources include 1,536 independently calculated weighted transitions
across six aggregate variants, nullable and duplicate invocations, simultaneous
argument/witness changes, last-support retractions, and vertex/text extrema.
Native integration cases compare literal expected bags with batch and maintained
results and check exact delivered deltas, redelivery and rebuild. They also
exercise private-witness versus public-row quotas, every registration/rebuild
checkpoint, dormant errors, invalid numeric inputs and checked SUM output
overflow without partial group publication.

The tuple extension adds 768 independently calculated weighted transitions
over nine mixed outputs, complete aliases, two ordered DISTINCT barriers,
nullable/repeated imports and cancellation at each construction checkpoint.
Native regression sources cover simultaneous argument/witness/property changes,
first/last duplicate retraction, full delivered deltas, rebuild, private-witness
versus final-row quotas and atomic registration refusal. A valid COUNT preceding
an overflowing SUM must not publish a prefix even when SUM is hidden or LIMIT 0
suppresses the final rows. Relational DISTINCT inside COUNT bodies now shares
the maintained path.

All three Rust files in the tuple extension pass individual pinned Rustfmt and
whitespace checks. Independent source review found no remaining blocker.
Static-only UBS exited 0 with no critical findings; its AST rule pack was
unavailable and Cargo phases were excluded. These are limited static checks.
Compilation, runtime assertions, Clippy and full exact-tree qualification remain
**UNRUN**: pinned tools and locked dependencies are available, but shared memory
headroom fell below the bounded compiler-start threshold. No full
incremental-query coverage or W6 completion is claimed;
`fgdb-w6-zset-circuits-kjv` and `fgdb-epic-w6-65w` remain open.

### Bounded multi-property buffered queries — qualification pending

Vertex-local and single-edge scan plans now distinguish topology, a bounded
set of exact property keys, and complete-record demand. The shared inline
PropertySelection contains up to 16 sorted, deduplicated full-width keys;
empty selects no properties. Every local predicate, eager Boolean operand,
projection cell and hidden sort/aggregate input contributes to that proof.
Repeated uses share a selected field. A seventeenth distinct key, graph probe,
endpoint-property demand or unproved operation retains complete-record admission
instead of dropping a requested field.

The buffered facade applies the proof to ordinary and identity-seeded vertex
inputs, plus external sorting and aggregation. The ordered FGSV merge chooses
the complete same-cut history winner before decoding all requested fields in
one pass. Positional materialization reuses the active patch workspace; seeded
pulls resolve history once for the complete selection. Both return independently
charged opaque partial owners. Selection changes cannot turn a partial owner
into a complete property map or revive a missing field from an older version.

Edge sidecars decode the selected columns across admitted block histories in
one pass. Native suffix reads select one winning row before decoding its fields.
Native base reads meter each key search and use bounded inline positions to
measure and reserve selected payloads before cloning them. The Prism weight
reader uses the same selected-field owner. Existing scalar point APIs remain
single-key operations. Complete extent authentication and structural validation
remain required even when values are omitted.

Regression sources cover full-width keys, absent versus actual NULL, removed
properties, retired identities, historical labels, mixed complete/projected
pulls, pinned cuts through compaction, resolver-bound values, corruption,
every refusal/cancellation boundary and reservation ownership. Public tests
exercise a 96-cell two-property vertex projection with only 4 MiB remaining,
two-property external sort, duplicate identity seeds and COUNT/SUM/AVG inputs.
The edge public regression requests two small fields through native base and
suffix history with only 3 MiB remaining, despite an unrelated 12 KiB property.
Explicitly demanding that payload still charges it. Decoder and compiler tests
cover empty, duplicate, full-width and 16-key selections, exact 17-key fallback,
missing versus stored NULL, malformed skipped fields and cancellation prefixes.

All 25 changed Rust files pass individual pinned Rustfmt checks;
whitespace checks and independent production/test source reviews are complete.
Static-only UBS exited 0 with no critical findings; Cargo phases were excluded
and the AST rule pack was unavailable, so this is limited static evidence.
Compilation, runtime assertions, Clippy and full exact-tree qualification remain
**UNRUN** under insufficient shared memory headroom. No performance benchmark is
claimed. Whole native base residency and complete root admission remain, and broad
`fgdb-w3-properties-gou` and `fgdb-w3-buffer-g2v` remain open.

### Streaming aggregates over collected neighborhoods — qualification pending

The vertex aggregate cursor now carries each root's collected neighborhood
through the ordinary complete projection before grouping and reduction. Native
pull queries can sum neighborhood sizes, count nonnull lists (including empty
lists), count distinct lists and group by neighborhood-derived values. The
existing indexed walker owns correlation, parallel-edge multiplicity, property
visibility and per-root list construction; collected members are not additional
outer rows. Order-sensitive outer COLLECT retains its identity-order admission.

Regression sources compare the streamed path with the batch evaluator and
hand-derived answers, including direct list-valued inputs, NULL list members,
empty groups, hidden projection operands, pages, pinned snapshots, late errors,
source failure, cancellation and cumulative work/scratch limits. Pinned
changed-source formatting and whitespace checks pass. Static UBS exited 1 on an
existing panic in the test-only source that detects bypassed admitted records;
it found no introduced production-code critical issue. Cargo/Clippy were not
part of that scan. Compiler, runtime and exact-tree qualification remain open;
the broader pattern-comprehension work in `fgdb-1f8f7` is not closed.

### Authenticated durable-run admission from actual contents — qualification pending

Buffered native reads now retain the actual encoded object capacity while
authenticating a durable relation run, then admit a conservative decoder bound
from its authenticated byte length and incidence population. Configured run
ceilings still reject oversized input, but their full capacity is no longer
charged up front. Property decoding retains a conservative embedded-image byte
bound. This permits small runs under ordinary query allowances even when the
configured format ceilings admit much larger runs.

The same ownership extends through raw loose files, encrypted extents and
uncached physical pack ranges. Corrupt copies may use the existing authenticated
copy retry; cancellation, query-budget and memory refusals remain terminal.
Complete and carried runs retain their root/snapshot binding and suffix proof.
The complete base image is still decoded and retained; descriptor-only cold
opening and paged base scans remain unfinished larger-than-memory work.

Regression sources cover authenticated malformed counts, incoming/forward row
asymmetry, original refusal errors, cached-pack bypass and dropped-proof refunds.
A public raw/encrypted query matrix compares narrow and generous ceilings across
history cuts, carried suffixes and all directions under the same 16 MiB owner.
Pinned changed-source formatting and whitespace checks pass. Runtime and
exact-tree qualification remain pending; the broader
`fgdb-rc1111-persistent-tier-r-7hsan` and `fgdb-teypc` remain open.

Static-only UBS covered all fifteen changed Rust files in these storage and
OPTIONAL MATCH increments. It exited 1 on an existing authorization-test
comparison of a public budget dimension, falsely classified as a secret
comparison. Review found no introduced production-code finding; added-line
findings concern test assertions and fixture operations. Cargo and Clippy
were excluded from that scan, so it supplies no compiler or runtime verdict.

### Clause-local OPTIONAL MATCH expression predicates — qualification pending

The native read compiler now routes a top-level OPTIONAL MATCH's expression
predicate through its existing correlated Apply without requiring an explicit
preceding WITH. The predicate evaluates inside the optional body before null
extension. Incoming identities and values, parallel-edge multiplicities and
unmatched rows therefore keep their existing owners. Leading OPTIONAL MATCH
also retains its unit input. The bounded PreparedGraphText facade keeps its
existing syntax boundary; PreparedNativeRead and the native public dispatchers
own the composition.

Regression sources cover Boolean label tests, imported values, unknown labels,
three-valued logic, comma patterns, existence predicates, cancellation and late
errors including LIMIT 0. Public cases compare a physically masked graph with
capability-authorized execution and reuse a prepared query across pinned and
updated label snapshots. Pinned changed-source formatting and whitespace checks
pass. Runtime and exact-tree qualification remain pending while the offline
pinned build runs under intermittent executor I/O stalls.
The broader `fgdb-qo3jv` remains open.

### Streaming restoration of derived checkpoints — qualification pending

Beacon and Ripple checkpoint restoration now consume authenticated Chronicle
history one complete logical batch at a time. They preserve the database's lazy
delta window and its materialized-history floor instead of loading the entire
omitted suffix through `ensure_delta_window`.

Beacon collects a bounded union of affected vertex IDs across the suffix and
applies their final state once, preserving resident-index update ordering and
using one cumulative catch-up work allowance. Ripple retains cumulative work
and scratch accounting across checkpoint decode, boundary validation and every
replayed tick. Its embedded marker and template digest are checked at the
checkpoint and final boundaries, including a zero-tail restoration.

Both consumers build private candidates and publish a handle only after the
whole restoration succeeds. Cancellation, dropping a suspended future, quota
refusal and later-capsule corruption leave existing handles and the pinned read
basis intact. Authentication or I/O failures in authoritative replay are typed
terminal errors; optional checkpoint corruption and explicit unavailable-history
cases retain their permitted rebuild paths.

Regression sources cover repeated vertex edits, unrelated and empty ticks,
selected-edge additions and retractions, exact cumulative budget boundaries,
Send futures, embedded-anchor mismatch, cancellation during a later capsule
read, dropped futures, later-record corruption and clean retry. Pinned formatting
and source review are complete; compilation, runtime assertions and full
exact-tree qualification remain pending. The runtime evidence must not be
inferred from these source assertions.

This bounds the consumer's retained replay batches to one; the affected-ID set,
Ripple arrangement, admitted snapshot and existing replay verification
diagnostics still consume memory. It does not establish a whole-handle memory
bound or an end-to-end O(delta) time bound. The broader
`fgdb-rc1111-durable-watermarks-c1g98` and `fgdb-w6-delta-consumption-iyc`
qualification remains open.

### Factorized join multiplicities through ordered projection — qualification pending

The calibrated factorized FreeJoin strategy now visits complete bindings with
their original multiplicity factors. DISTINCT and bounded ordered pages apply
the same terminal occurrence cap as the generic FreeJoin strategy; they no
longer flatten the entire parallel-edge product first. Bags whose total
cardinality overflows `u128` remain traversable without reopening the join.
Unbounded bag projection still emits every occurrence. LIMIT 0 still evaluates
each binding's projection, including hidden sort cells, so source errors and
cancellation remain observable.

Regression sources compare the factorized strategy with the generic strategy
and the existing GLA evaluator, and cover weighted products/unions, nullary
relations, overflow, offsets, hidden order keys, resource limits and refused
visits. This change avoids duplicate expansion; it still enumerates distinct
complete bindings and retains the existing in-memory factor tree. Spill,
RunSlice integration and the broader factorization work remain open under
`fgdb-w5-factorization-awm`. Runtime and exact-tree qualification are recorded
with the landing evidence rather than inferred from source review.

### Selected edge-property buffered queries — qualification pending

The single-edge compiler exposes one complete field-demand proof:
topology only, up to 16 exact edge-property keys, or complete records. Repeated uses
of the same keys across filters, projections, hidden sort cells and aggregate
inputs share the selected source. Exceeding the key bound, any endpoint property,
joined/probed plan or unrecognized program retains complete-record admission.
Boolean inputs include eager operands in unselected scalar branches.

The buffered facade connects this proof to Strata's authenticated selected-key
reader for ordinary, seeded, sorted and aggregate single-edge inputs. A distinct
owned record carries only those edge fields and both endpoints' same-cut labels
and identities. It cannot escape as a complete property map. Self-loops share
one endpoint owner; missing keys, true NULL, property removal, source failures,
candidate/work limits, cancellation and independent output guards preserve their
existing semantics.

This reduces selected row and output reservation demand from unrelated payloads.
It does not make cold opening or native base-image admission descriptor-only:
selected sidecars still authenticate their containing bytes, and the existing
whole-base validation/residency bounds remain. No benchmark speedup or complete
larger-than-memory qualification is claimed.

Compiler and storage regression sources cover exact full-width key identity,
hidden demands, selected corruption, absent/NULL/removal semantics, historical
endpoint labels, ownership, cancellation and work limits. Public regressions
exercise pinned and compacted history in every direction, aggregate results,
a 3 MiB remaining read allowance with large unrelated payloads, multi-key native
base/suffix histories and charged admission of additional required payloads.
The original single-key implementation is recorded in `b9e5d3b`; the bounded
multi-property extension and its qualification are described above.
The broader `fgdb-w3-properties-gou` and `fgdb-w3-tier-r-0tj` remain open.

### Boolean filters over topology-only buffered records — qualification pending

The compiled vertex and single-edge scan plans now derive Boolean property
demand from the existing complete eager-operand metadata. Identity comparisons,
constant predicates and identity arithmetic can use the authenticated topology
sources, retaining same-cut endpoint labels without loading unrelated property
records. The same proof reaches external-sort and aggregate input plans, including
their hidden cells. It neither evaluates nor removes a predicate.

Every referenced vertex/edge property still requires its source fields, including
operands behind OR, AND, CASE and COALESCE. Joined/probed and unrecognized programs
remain conservative. Source visibility, candidate/work accounting, cancellation,
arithmetic errors and independent result reservations keep their existing owners.

The increment adds compiler demand cases and public buffered-query regression
sources. The implementation is recorded in `55955b3`; its runtime qualification
and the broader property-storage and Tier-R contracts remain open.

### Buffered field-demand qualification receipt — 2026-11-22

Independent source reviews of both field-demand increments found no remaining
source blocker. All twelve changed Rust files pass the pinned nightly's
individual Rustfmt checks, and whitespace checks pass. The bounded static UBS
scan of those files exited 0 with no critical findings. Its ast-grep rule pack
was unavailable, and its explicit `--no-cargo` mode did not run compiler,
Clippy or dependency checks; this is limited static evidence, not a full gate.

The real RCH force-local command
`cargo check --locked --offline -p fgdb-gql --all-targets` exited 137 during
dependency compilation. Its last diagnostic was the bare word `Killed`; it
did not identify the killed process or establish a product failure. The attempt
used the exact pinned foundations, nightly 2026-08-31, one Cargo job,
`-Z threads=1`, no incremental compilation and zero debug info. Its source was
`b9e5d3b` plus only the seven formatter changes (Rust/manifest diff SHA-256
`36bc33c24236e75bb7887d63a411f816b10d40bc777b93229a4f07e9f1cba9c2`).
No changed-crate compiler verdict or new runtime assertion was reached.
The executor subsequently disconnected again before exact-tree proof could
start. Clippy, runtime qualification and the complete proof remain open.

Progress comments were added through `br` for open `fgdb-w3-properties-gou`
and `fgdb-w3-tier-r-0tj`. The required exact-ID export was rejected with exit 65:
`br 0.2.16` reordered all 1,421 records and also changed 13 unrelated records
semantically. The rejected export, database and exact comments were preserved;
only the exporter-authored tracked changes were restored to the byte-exact
HEAD stream. The canonical Beads export remains pending. Neither broad issue
is closed, and no unrelated record normalization is included in these changes.

### Managed read-only transaction batches — qualification pending

The Rust SDK now exposes collected and streaming read-only transaction batches;
the remote CLI selects them with `transaction --read-only`. Complete script
admission precedes BEGIN, and every envelope must be Read. The CLI also refuses
savepoint controls before opening credentials. Existing mixed batches retain
their write-owner behavior. Streamed rows remain provisional until the native
ReadClosed receipt confirms the original basis and statement count; cancellation
cleanup and refusal of malformed completion receipts retain the existing client
connection fences.

HTTP exposes `POST /v1/databases/<name>/read-transaction` with the existing
transaction JSON envelope. Read authority precedes body diagnostics; a later
Write envelope refuses the complete script before BEGIN. All statements use
one native read owner, snapshot and cumulative signed allowance. Successful
close returns the original basis with `committed:false`; no graph publication
occurs. Complete response admission withholds successful-prefix results on
late failure, and the original recovery generation fences physical output.
The existing snapshot-admission fallback remains; this is not a universal
nonblocking-read claim.

Regression sources cover scoped read credentials, preflight refusals, read
completion validation, cleanup, shared execution/output limits, a retained
published snapshot while a writer is held, and success/error recovery fences.
Changed-source formatting and independent source reviews are complete. Full
exact-tree qualification remains open, as do the broader `fgdb-huu9`,
`fgdb-w10-embedded-54r`, `fgdb-w10-server-rte` and `fgdb-w10-adapters-b1f` beads.
See [the HTTP contract and examples](docs/FABRIC_PROTOCOL.md#read-only-http-transaction-scripts).

### Conditional percentile and deviation summaries — qualification pending

`percentileDisc`, `percentileCont`, `stDev` and `stDevP` now compose with lazy
CASE and COALESCE through the existing aggregate value-expression owner in
RETURN, WITH, HAVING and ORDER BY. Unselected summary applications are skipped;
selected failures remain observable even under LIMIT 0. Grouping still admits
and evaluates aggregate input expressions and builds their collections before
branch selection. Invalid nested aggregates and local-input scopes still refuse.

Selected COUNT, wide integer SUM and exact AVG values preserve their native
domains. Simple CASE compares these values through the existing exact numeric
comparison helper, without i64 narrowing or binary64 rounding. Ordinary graph
value equality and NULL/cross-domain behavior retain their existing semantics.

Regression sources cover selected/unselected failures, empty and all-NULL
groups, parameter rebinding, HAVING and ranking, native/pipeline routes, and
exact comparisons around 2^53, above i64, and at u64::MAX. Full exact-tree
qualification and the broader `fgdb-fkl6u` bead remain open. See
[the query semantics and example](docs/graph_patterns.md#conditional-percentile-and-deviation-summaries).

### Authorized transaction savepoints — qualification pending

The existing explicit authorized write owner now accepts typed SAVEPOINT,
ROLLBACK TO [SAVEPOINT], and RELEASE [SAVEPOINT] controls. FGP Write envelopes,
HTTP transaction batches, and Bolt explicit write transactions use the same
bounded parser and native workspace. Rust SDK helpers and remote CLI statement
files expose those controls without new protocol tags or a separate commit
path; CLI preflight validates modes, parameters and the case-sensitive stack.

Rollback restores the prepared overlay and the chain-retirement journal,
including revived vertex/edge metadata. Graph effects and the live savepoint
stack rewind; identity reservations, consumed allowances, successful statement
ordinals and read/conflict witnesses remain cumulative. Controls alone retain
ReadClosed completion. A successful graph write retains WriteCommitted
classification even if all its effects are later rolled back. Any failed
authorized control aborts the complete unpublished owner.

Regression sources cover nested/shadowed names, release, restored deletions
and metadata, conflict witnesses, zero-effect completion, non-refunded limits,
expiry/retirement/cancellation and rejected output, plus parser, SDK wire,
FGP/HTTP/Bolt and fragmented CLI transport behavior. Provisional rows delivered
before rollback remain observations; this does not add durable transaction
leases, reattachment, retries or recovery of an already failed owner.

Independent source review found no remaining blocker. All sixteen changed Rust
files, including the merged COUNT/savepoint exports, were parsed and formatted
with pinned Rustfmt; whitespace checks passed. Compilation, runtime assertions,
Clippy/UBS and full exact-tree proof remain **UNRUN** for this increment under
the interrupted executor/foundation-build environment. Progress is recorded
against open `fgdb-w10-embedded-54r`, `fgdb-w10-server-rte` and `fgdb-huu9`;
none is closed by this slice.

### Lazy post-seal native property reads — qualification pending

Native durable-run readers now retain admitted suffix topology, original root
ordinals and compact property locators. After resolving exact-cut visibility
and last-publication precedence, root scans and both incident/probe paths fault
only the winning row's requested property data through the existing
authenticated buffered readers. Topology-only and propertyless winners do not
demand property sidecars. Selected rows retain their own measured memory charge.

The suffix receipt no longer retains every historical property body, and
selected rows do not re-decode block topology. The regression source includes
512 distinct 14 KiB property values with a 6 MiB read pool, historical edits,
nested values, removals, retirements, selected-key reads, post-admission
corruption, terminal errors, late cancellation and reservation refunds.

This is a suffix-property residency improvement, not descriptor-only opening
or a claim that the whole query is larger-than-memory. Complete initial source
admission, the whole sealed base image, suffix topology and the EId directory
remain required. Durable formats, dependency pins and cold-open validation
are unchanged.

Independent production and regression-source reviews found no remaining
source blocker. All eleven changed Rust files were parsed and formatted with
the pinned Rustfmt; whitespace checks passed. Compilation, runtime assertions
and full exact-tree proof remain **UNRUN** for this increment. The broader
`fgdb-rc1111-persistent-tier-r-7hsan` and `fgdb-w3-tier-r-0tj` remain open.

### Continuation qualification receipt — source `4bfa197`

The authorized external aggregate core and server integration are on `main`
in `117db0d` and `b0ab2da`; batch-at-a-time authoritative delta replay is in
`4bfa197`. The integrated implementation was clean locally and both `main`
and its required `master` mirror were verified at
`4bfa19747f3ec1405b4257392a5c569a6e0e2ac6` before the executor outage.

Twenty changed Rust files passed individual pinned Rustfmt checks.
The remaining changed Rust file, `fgdb/src/lib.rs`, retains the exact same
formatter-diff text as continuation baseline `10d2f21`; its new exports/module
declaration are formatted. Whitespace checks and independent implementation
and regression-source reviews passed. Sixteen new Rust regression tests were
authored: eight native aggregate, three server/FGP, and five replay tests.
Their assertions remain **UNRUN**.

The required `scripts/check.sh` attempt ran against integrated source
`4bfa197` with the pinned nightly, one Cargo job, `-Z threads=1`, no
incremental compilation, zero dev/test debug info and a temporary build target.
Observed core results:

- Shell lint, all 49 Studio Node tests, verdict-contract closure and gate-domain
  closure passed.
- `cargo check --all-targets --locked` exited 137 during dependency checking.
- `cargo clippy --all-targets --all-features --locked -- -D warnings` and
  `cargo test --workspace --all-features --locked --no-fail-fast` each exited
  101 after rustc compiling pinned asupersync received `SIGKILL`. Neither
  reached new first-party test assertions or a successful compiler verdict.
- Repository-wide formatting failed on 43 paths. Forty-two are byte-identical
  to continuation baseline `10d2f21`; the remaining path is the preserved
  `lib.rs` formatting described above.
- File-coverage closure still rejects the pre-existing
  `.beads/fgdb-nbrog-import-source.jsonl`. UBS is absent. Registered checks
  also exposed existing assignment-pin/provenance failures and unavailable
  cargo-audit/Lean prerequisites. These were not weakened or relabeled as
  passes.

The registered commit-provenance and threat checks passed. The executor then
became unavailable while the remaining registered checks were in progress;
the process service reported an exec-server transport disconnect and timed-out
recovery. No final wrapper exit or complete exact-tree proof bundle was
acknowledged, so the overall gate remains **interrupted and unqualified**.

Progress comments for `fgdb-w10-snapshotquery-rzh` and
`fgdb-w6-delta-consumption-iyc` were written to the local Beads database without
automatic export during the gate. The executor outage prevented the required
exact-ID tracked export; that export remains pending. Their implementation
progress and qualification limits are preserved in this document and the
source commits. Both broader contracts remain open, and no bead was closed.

### Filtered correlated COUNT maintenance — qualification pending

Native standing queries now admit a single correlated COUNT with row-local
WHERE or join predicates, including imported scalar thresholds and complete
static property-selected graph sources. COUNT(*), COUNT(value), and
COUNT(DISTINCT value) use the existing exact group owner.

The fallback compiles one exact nullable invocation domain, deduplicates only
the imported arguments, retains every body filter/projection in order, groups
before restoring the original outer occurrences, and zero-fills empty bodies.
Duplicate outer rows therefore retain their multiplicity without multiplying
the count. Existing optimized equality-count recipes and ordinary one-shot or
pull query lowering are unchanged. Native subscription admission counts all
private owners before source access, and witness rows use input budgets rather
than the final result-row allowance.

Regression sources cover weighted/null invocations, simultaneous source
changes, static RHS property updates, imported-property changes, parallel-edge
support/retractions, empty groups, count-distinct, cancellation, whole-circuit
rebuild, private witness budgets, and failure before a zero-length ordered
page. An uninvoked fallible body remains unexecuted; an invoked discarded
expression is still evaluated in its original position. Deferred RHS property
readers, unproved argument domains, relational DISTINCT inside the count body,
nested invocation and unsupported ordering/page profiles still refuse.

The subscription regression also covers scoped baseline and delta delivery,
exact pending-batch redelivery, acknowledgment frontiers, and hidden commits
that must not advance a subscriber's public frontier.

Manual source review is complete. After the execution workspace recovered,
the pinned Rustfmt successfully parsed and formatted all twelve changed Rust
files. First-party compilation, runtime assertions and full exact-tree
qualification remain **UNRUN** for this increment. Earlier pinned-foundation
compiler terminations are not qualification of this changed tree.
The broader open `fgdb-w6-zset-circuits-kjv` and `fgdb-epic-w6-65w` remain open.

### Batch-at-a-time authoritative delta replay — qualification pending

Source review 2026-11-20: the privileged embedded `delta_replay(after, through)`
and `next_delta_replay(&CommitCx, &mut cursor)` APIs replay one complete Local
committed batch per pull through the existing `read_delta_batch` verifier.
Checkpoint-omitted history does not require installing a full missing suffix
in the resident `LocalDeltaBatchIndex`. Later commits cannot extend the admitted
end, and empty logical ticks retain their canonical batch identity.

Each pull rechecks the current root identity tuple, position/end marker and
chain commitments, exact next predecessor, health, range and retention. Future
cuts, substituted history, missing retained markers/capsules, corruption and
I/O failures are typed refusals. Existing explicit retirement remains distinct
from an unloaded checkpoint prefix. All awaits and fallible checks precede the
sole cursor advance; failures, cancellation and dropped pending pulls preserve
the cursor, graph snapshot, loaded history and materialization floor.

The reader holds no snapshot lease, retention obligation, durable subscription
record or ACK authority. It is not a masked feed or public CDC/resume protocol.
Identity inherits the existing namespace and committed-history authority, not
physical-directory uniqueness. One batch is indivisible; existing verification
diagnostic evidence remains retained and can grow across pulls. No whole-handle
memory ceiling is claimed.

Five regression sources exercise checkpoint reopen, canonical bytes and empty
ticks, one capsule read per pull, fixed ends across later commits, clone repeat
delivery, foreign/divergent sources, both boundary commitments, explicit cuts,
I/O/missing/substituted capsule failures, pending drop, cancellation, health
fencing and u64 endpoint arithmetic. Pinned formatting and independent source
review pass. Compiler/runtime and full exact-tree qualification remain open
under `fgdb-w6-delta-consumption-iyc`; durable consumer/retention and subscription
delivery machinery remain unfinished.

### Authorized external terminal aggregates — qualification pending

Source review 2026-11-20: `AuthorizedOrderedReadSession::execute_ordered_aggregate`
connects terminal COUNT/SUM/AVG/MIN/MAX/COLLECT to the existing complete scoped
relation owner and external group reducer. FGP `ReadOrdered` and prepared
`OrderedStream` select this typed cursor for aggregate facades, including exact
historical cuts and admitted WITH/UNWIND/set inputs. Unsupported definitions
refuse before query input or scratch writes; no eager fallback occurs.

Complete source occurrences, grouping, HAVING, computed output, DISTINCT,
ordering and paging retain native semantics. Count cells remain u64, integer
sums i128 and exact averages reduced rationals through scratch and wire
conversion. COLLECT and COLLECT DISTINCT retain native occurrence order across
partition passes; each collection must fit its per-value memory and row limits.
Final group slots preserve hidden/repeated keys without narrowing values through
ordinary rows. LIMIT zero still validates all reached input and group expressions.

The source and reducer share the session's historical pin, scope and signed
permit. Physical sort work is prepaid; only final delivery spends signed rows.
Live authority and served-generation checks surround asynchronous work and
delivery. Failure or a dropped pending pull cannot later report successful
exhaustion. Existing anonymous scratch, memory reservations and concurrency
leases retain their normal cleanup. Terminal output expressions that require
additional graph-element reads remain unsupported; carried input-stage property
reads retain their scoped reader. The source graph remains resident, and this
adds neither durable result retention nor a whole-query memory bound.

New native/server/FGP regression sources cover exact values, forced partitioning,
collection order, masked inputs, prepared old/new/old snapshots, flow credit,
private input versus delivered-row limits, cancellation, late failures beneath
LIMIT zero, recovery and issuer retirement. Independent source review caught
and corrected nested input error classification and the small-memory fixture's
catalog admission. Runtime and full exact-tree qualification remain open under
`fgdb-w10-snapshotquery-rzh`; no broader SnapshotQuery contract is closed.

### 2026-11-20 session qualification receipt

The typed MERGE-map correction, DAG procedures, ECS native-run admission and
frozen CLI catalog are preserved on `main` in `a84d0d7`, `c4247a0`, `0940911`
and `3ef7aa1`. The following receipt applies to those source changes; it is
not an exact-tree green-gate claim.

Passed: pinned Rustfmt checks on all 38 changed Rust files individually;
whitespace checks; independent DAG, query-host and storage-admission review;
binary/frozen CLI schema equality and exact 32-procedure inventory checks;
and the exact-ID Beads export for the three implementation records. All 1,418
unrelated Beads records and existing comment identities were preserved.

`cargo test --locked --offline -p fgdb-prism --test dag` exited 101 because
rustc compiling the unchanged pinned asupersync library received `SIGKILL`.
It did not reach the changed crates' compiler diagnostics or test assertions.
The build used the pinned nightly, one Cargo job, `-Z threads=1`, disabled
incremental compilation and zero dev/test debug info.

The required `bash scripts/check.sh` attempt ran on source head `3ef7aa1`
with the same build settings. Shell lint, Studio Node tests, verdict-contract
closure and gate-domain closure passed. Repository-wide formatting reported
44 files, and file-coverage closure reported
`.beads/fgdb-nbrog-import-source.jsonl`; every one of those paths is unchanged
from initial main `c4093f9`, and none is among the 38 Rust files changed here.
The metadata-only `cargo check --all-targets --locked` also exited 101 after
rustc received `SIGKILL` compiling pinned asupersync. Clippy was still checking
that dependency when the 600-second session bound interrupted the wrapper
(exit 124). Workspace Rust tests, UBS and the remaining registered gates are
**UNRUN**. No new feature's runtime assertions are claimed to have passed.

Compiler/runtime and complete exact-tree qualification therefore remain open.
The MERGE bug stays in progress; the catalog and persistent Tier-R beads also
retain their broader unfinished scope. The implementation descriptions below
state source-reviewed behavior and authored regression coverage.

### Directed dependency ordering and cycle detection — qualification pending

Source review 2026-11-20: registry version 36 exposes 32 procedures, including
`fnx.topological_sort` and `fnx.is_directed_acyclic_graph`. The sort emits
dependency-ordered vertices and exact zero-based ranks using the pinned FNX
deterministic DFS reverse-postorder convention. The predicate emits one typed
Boolean, including `true` for an empty graph. Directed and reversed projections
are supported; undirected input refuses. A retained self-loop is a cycle.

Decoded, compressed and cooperative execution use one iterative state machine
with admitted vertex arrays and bounded cursor stacks. Compressed traversal
does not copy adjacency. Full traversal precedes publication, so discovering a
cycle cannot hide a later source error or cancellation. Sorts refuse a cycle
without partial results. Query hosts preserve dependency order through ordinary
composition, pagination and collection, and avoid the canonical-vertex cursor
shortcut. Explicit query ordering retains its normal presentation semantics.

Native composition includes capability masking, transaction overlays, retained
history and certified replay. Boolean cells have their own result-digest tag
and native/CLI conversions. Standing-query maintenance explicitly refuses these
procedures. New regression sources compare all three-node directed graphs with
pinned FNX, exercise a 32,768-vertex chain, full-width identities, projection
laws, exact budgets, cancellation, late source errors, empty predicates, staged
cycles, hidden cycles and real compressed storage. Independent source review
passes; compiler/runtime and complete exact-tree qualification remain pending.
This is a bounded catalog increment under open `fgdb-nu3mt`.

### ECS-coded durable runs in bounded native reads — qualification pending

Source review 2026-11-20: opt-in native typed-edge scans, join children and
correlated probes may now select eligible durable relation runs in ECS-coded
storage. They retain the existing run-plus-authenticated-suffix history and
selected-source failure semantics. There is no new durable format or dependency.

The reader's private store clone charges frame scratch and repair workspace to
the same source pool as the admitted run image and suffix. Loose extents stream
one frame at a time and verify the complete Merkle commitment, including empty
objects; packed members read their required authenticated frame ranges. Repair
capacity is reserved before allocation using authenticated coding geometry and
RFC 6330 padding. Healthy reads do not reserve the full decoder allowance.
Insufficient capacity returns the typed memory refusal, terminal across copy
retries. Temporary reservations follow the read future's lifetime.

Bounded readers try known indexed packs and loose copies, but do not refresh
the pack directory through uncharged metadata decoding. Newly published repair
packs require reopening the bounded reader; ordinary readers retain discovery.
Initial source admission remains separate. Synchronous FEC decoding retains its
existing CPU cancellation granularity, and multiple damaged frames may repeat
repair of the same stripe. No overall-RSS or bounded CPU-latency claim is made.

Regression sources cover authenticated geometry differing from writer settings,
healthy selective reads, repair admission/refunds, wrong keys, byte ceilings,
Merkle failures, pending-I/O future-drop refunds in query and ancestor pools,
pack discovery, historical and suffix parity, and damaged
selected-run refusal without falling back to Tier-D. Independent review caught
and closed an uncharged pack-refresh path and redundant future allocation.
Compiler/runtime and complete exact-tree qualification remain pending under
open `fgdb-rc1111-persistent-tier-r-7hsan`.

### Typed complete-map reads after relationship MERGE — qualification pending

Source review 2026-11-20: complete-map boundary reads now remain structurally
distinct from named-property reads through relationship `MERGE`, hidden tail
projections, `WITH`, grouping and computed ordering. A property literally named
`*` cannot alias the complete-map request. Complete maps use final transaction
state and capability masking; missing named fields remain NULL.

Native commit `a84d0d7d9e0956781e2f5d05e8097def214ce14e` contains the correction
and regressions for plain, unique and plural relationship merges, both read
orders, repeated occurrences, hidden fields and late-error rollback beneath an
empty page. Scoped pinned Rustfmt and whitespace checks pass. Compiler/runtime
and complete exact-tree qualification remain pending under
`fgdb-merge-tail-maps-q2fhs`; this is not a closure claim.

### Lazy conditional COLLECT subqueries — qualification pending

Searched and simple `CASE`, and `coalesce`, containing `COLLECT { ... }` now
lower through native argument-scoped Apply and Collect operators. Only selected
conditions, result arms and fallbacks execute their graph reads and scalar
expressions. A simple CASE retains its selector once per input occurrence;
selected NULL values, correlation, duplicate occurrences and collection order
are preserved. Unknown names and writes still refuse during preparation, even
inside an unselected arm. No interpreter or execution opcode is added.

This also guards EXISTS and COUNT sources within those conditional bodies.
Expressions containing only EXISTS/COUNT retain their existing eager contract.
Postfix access, arithmetic/comparison suffixes, nested conditionals, ordinary
list/map wrappers and keyword variable/property names keep the complete CASE
boundary. Literal wrappers remain transparent while list comprehensions retain
their local scope. Lookahead stacks use the existing 64-level nesting bound
before allocation.

New `multipart_reads::collect_subqueries` regression sources assert skipped
source callbacks and division errors, one-time selector reads, parameter
rebinding, correlation and NULL results, exact cumulative budget boundaries,
cancellation, LIMIT-zero error visibility and typed nesting refusals before
catalog access. Changed-file pinned Rustfmt and whitespace checks pass. Targeted
runtime builds and a metadata-only compiler check stopped when rustc compiling
pinned asupersync received SIGKILL; fgdb-gql compiler diagnostics and runtime
assertions were not reached.
Compiler/runtime and full exact-tree qualification remain **UNRUN**. This is an
increment under open `fgdb-w5-parsers-nje`, not a language-conformance or bead
closure claim.

### Bounded Beacon checkpoint encoding — qualification pending

Native maintained-index saves now apply the existing 256 MiB plaintext ceiling
during encoding, before growing the output buffer. The Beacon codec exposes a
bounded encoder and a transactional append operation: failed byte admission or
cancellation restores the caller's original prefix. Capacity grows geometrically
within the admitted ceiling, and allocation failure is a typed error.

The native definition and nested index share one buffer, removing the previous
full-index intermediate copy. The prefix and index-length field count against
the same ceiling. The version-one format and decoder remain unchanged; an
encoding refusal occurs before staging-file publication and leaves a previously
saved checkpoint intact. This bounds requested output capacity, not total RSS
or the separately encoded ECS container.

Regression sources cover frozen empty-generation bytes, populated hybrid
roundtrips, every byte cutoff, exact-bound success, prefix rollback, capacity
growth and cancellation. Scoped pinned Rustfmt and whitespace checks pass.
First-party compilation, runtime assertions and complete exact-tree
qualification remain **UNRUN**. The locked metadata check downloaded its
missing registry dependencies, then exited 101 when the pinned asupersync
compiler received SIGKILL before reaching Beacon or Ripple. The targeted
runtime test had previously stopped in dependency resolution; neither attempt
is passing evidence. The broader `fgdb-rc1111-durable-watermarks-c1g98` remains
open.

### Bounded reachability checkpoint encoding — qualification pending

Reachability checkpoints now count their fixed-width version-one payload with
checked arithmetic before reserving output. Schema epochs, edge identities and
every reachable pair contribute to the same 256 MiB native plaintext ceiling.
The sizing pass borrows the arrangement, stops as soon as oversize is known,
and remains cancellable; a successful encoding makes one fallible reservation.
Typed errors distinguish control failure, size overflow, byte refusal and
allocation failure. An early byte refusal reports a lower bound on required
space rather than promising the complete size.

Native saves pass this limit through the standing-query owner. Encoding and
container construction finish before directory creation or staging publication,
so a refused save preserves both the published cache and the live frontier.
The public `CommittedReachability::encode_checkpoint` error type is now
`ReachabilityCheckpointEncodeError<E>`; repository callers are adapted.

Four codec regressions cover frozen origin bytes, exact lengths and every
smaller limit on a parallel-edge cyclic graph, cancellation throughout sizing
and serialization, and arithmetic overflow. A native regression exercises
exact-bound saves and zero/one-byte-short refusal for both Beacon and Ripple,
checks unchanged saved bytes and source frontiers, and restores both caches.
Scoped pinned Rustfmt, whitespace checks and independent source review pass.
The locked metadata command failed with exit 101 when rustc compiling pinned
asupersync received SIGKILL; it reached no first-party diagnostics or assertions.
First-party compilation, runtime assertions and complete exact-tree
qualification remain **UNRUN**. The full mandatory gate has no passing verdict.
The broader `fgdb-rc1111-durable-watermarks-c1g98` remains open.

### Exclusive derived-checkpoint publication and oversized recovery — qualification pending

Both native checkpoint-save APIs now require exclusive database access. This
serializes their staging-file publication with other saves and graph writes,
preventing two same-name saves from truncating a shared staging file. This
changes no durable format or graph-commit protocol.

The bounded loader and cancellation regressions from native commit
`74db47bc47fa1f4740233b3966e7212af8eb17a3` remain intact. An added integration
regression presents real sparse 1 TiB file metadata for both Beacon and Ripple
checkpoints and asserts zero payload reads, authoritative rebuild, and
continued search/reachability updates after another graph commit. Independent
source review and changed-file pinned Rustfmt/whitespace checks pass.
Compiler/runtime and complete exact-tree qualification remain pending under
`fgdb-rc1111-durable-watermarks-c1g98`.

### CLI search reuses named durable indexes — qualification pending

`fgdb search --index-checkpoint <name>` opts into the native writer-owned
checkpoint lifecycle for current text, vector and hybrid retrieval. It restores
a matching saved index, catches up committed deltas, executes the search and
persists refreshed state before emitting rows. The reported graph sequence does
not advance. Current healthy checkpoints skip encoding and fsync; missing,
corrupt or incompatible checkpoints rebuild from authoritative data. Normal
search retains its read-only one-call index. Historical cuts and graph expansion
refuse the checkpoint option before opening storage. Names are bounded to
1..=128 ASCII letters, digits, underscores, dots and hyphens.

Both coordinate and packed vector projections use their existing admission
rules. Eligibility filters run before candidate limits, and explicit ANN uses
the restored handle's maintained HNSW topology. Exact-search parity is covered
against a fresh index; ANN remains approximate and its history-dependent
topology need not match a newly rebuilt index. Save failures emit no successful
rows or result, and preserve the distinction between storage I/O and native
execution refusal. Help, robot schema and README describe the opt-in writer
lease and persistence behavior.

Process regression sources cover cold/warm reuse, committed text/vector changes,
eligibility, changed projection definitions, corruption, unchanged-cache reuse,
save failure before output and invalid mode/name refusal before storage access.
Compiler, runtime and exact-tree qualification are pending; the broader durable
index lifecycle remains open under `fgdb-rc1111-durable-watermarks-c1g98` and
`fgdb-t38fv`.

### Served Beacon indexes restore durable checkpoints — qualification pending

Operator-configured `--beacon-index` definitions now restore their authenticated,
Chronicle-watermarked checkpoints at server startup and authoritative recovery.
The native owner verifies the saved definition and marker-chain commitment,
replays the committed delta suffix, and installs the resulting exact-cut index
before the served generation becomes readable. Missing, corrupt, incompatible
or unavailable checkpoints use the existing authoritative rebuild path. This
connects the existing durable Beacon engine to the server; ordinary database
opening and the rest of its graph admission retain their current costs.

Rebuilt, advanced or repaired checkpoints are saved at that admission boundary.
An intact checkpoint already at the current head is reused without another
encoding or fsync. Graph commits retain the existing in-memory maintenance path,
with no new per-write checkpoint I/O. Optional save failures keep a healthy
database available and are reported only through trusted-host index status,
alongside the restore/catch-up outcome and acknowledged checkpoint sequence.
Typed cancellation remains terminal, including during save and final admission.
Query selection, full-visibility checks, signed budgets and exact-only automatic
reuse remain in their existing owners; the name is not a durable schema catalog
or a new client capability.

Trusted hosts can also force a fresh save with
`Server::checkpoint_beacon_indexes`. An opt-in
`--beacon-checkpoint-interval-seconds` schedule has its own completion-based
interval; periodic saving is disabled by default. The existing owned recovery
worker checkpoints healthy configured indexes after graceful drain. Scheduled
and shutdown saves reuse a healthy current checkpoint when its acknowledged
sequence already matches and no previous save error needs retrying. Explicit
saves still perform the I/O, including when repairing a removed cache at an
unchanged graph cut. Saves hold the existing database writer lease; individual
failures do not fence the graph or prevent healthy siblings from saving.
Trusted-host diagnostics remain available after healthy shutdown, including
the last checkpoint sequence and any final save error.

Regression sources exercise real server restart with a two-vertex, one-commit
suffix while a full corpus build is forbidden; parity with an independently
rebuilt query and a one-record warm-query allowance; authoritative recovery;
corruption and cache-save I/O failures; unchanged-checkpoint reuse without a save;
every one-shot query interruption during checkpoint saving and final admission;
periodic saves without a graph commit; graceful restart; idle-save avoidance;
explicit refresh and per-index save/retry isolation. Scoped Rustfmt and
whitespace checks pass. Compiler, runtime and
exact-tree qualification are pending. The broader durable-watermark and index
lifecycle work remains open under `fgdb-rc1111-durable-watermarks-c1g98` and
`fgdb-t38fv`.

### Automatic symbol allocation and local schema administration — qualification pending

Native commit `057bb8b3682391301ab49849dcd63cb8e91a06e9` adds
`Database::register_symbol_names` to the existing durable catalog owner. Explicit
bindings and floors are merged privately, automatic requests are sorted and
deduplicated by namespace/name, and the entire extension publishes through one
ordinary schema transaction. Returned bindings are available only after success.

Before allocating a new name, the owner authenticates complete retained Chronicle
history and reserves every numeric label, relationship type and property key it
has used, including removals, deleted elements, counters, escrow properties and
numeric writes since an earlier registration. It does not infer unused IDs from
the current graph. Unavailable or explicitly retired origin history refuses new
allocation. An already-bound request remains a no-op without that census.
The cost is proportional to retained history for new automatic names; ordinary
graph writes gain no additional history scan.

The CLI exposes `schema` for current/historical metadata inspection and atomic
registration with mixed `NAME` and `NAME=ID` declarations. `--if-current` checks
the real writer's frontier before effects. `--as-of` is read-only and incompatible
with registration flags. `create --persist-bindings` also accepts name-only
declarations. Robot output uses existing row framing with kind/name/id columns,
an exact selected/committed sequence, and decimal-string wide integers for IDs
above the signed 64-bit range. Inspection reads authenticated catalog metadata,
without requiring resident graph payloads.

Five native regression sources cover canonical/native-width allocation, real
query use, deleted numeric history, compaction/reopen, intervening numeric writes,
atomic refusal, idempotence, retired history and cancellation. Four process-level
CLI sources cover name-only creation, later writes/reads without flags, guarded
registration, historical catalogs, compaction/reopen, deleted IDs, conflicts,
exhaustion, input-mode refusal, Unicode/escaped names and full-width output.

Independent source review completed. Local changed-source Rustfmt and whitespace
checks passed before the execution environment disconnected. The final GitHub
files reconstruct the reviewed logical patches against their immutable base,
preserving base formatting rather than guessing the unavailable formatted bytes.
Exact-anchor reversal and GitHub blob readback verify that reconstruction; the
robot schema and expected JSON rows parse. These are source checks, not a Rust
compiler or runtime verdict. Compiler checks, runtime regressions, final-tree
Rustfmt, Clippy, UBS, the full mandatory gate and exact-tree proof remain
**UNRUN**. Earlier pinned asupersync compiler processes received SIGKILL; this session's
smaller attempt was interrupted before first-party
diagnostics, and the executor subsequently became unavailable.

`fgdb-7r8sy` remains in progress. General DDL, constraints, durable index
lifecycle, multi-coordinate catalogs, and automatic interning within graph writes
remain outside this increment. The unavailable executor also prevents the
normal Beads export helper; this entry records qualification without claiming a
closed bead or a passing gate.

### Server sessions resolve durable names at their selected snapshots — qualification pending

Startup discovery landed in `0e8ecd25d0b8373c394d37064ffaf672ae78a0ec`.
The server merges persisted label, relationship-type and property-key names with
explicit operator declarations; exact redeclarations are harmless and conflicts
refuse. Current and historical sessions bind and hydrate graph results with the
catalog at their selected cut. Requests spanning incompatible name maps refuse.

Prepared reads and writes retain their effective name map. Data commits and
allocation-floor changes remain compatible; a changed mapping returns `Conflict`
and requires preparation again. FGP and Bolt explicit transactions pin names at
BEGIN. Deferred Bolt RETURN also retains its catalog through output admission
and checks compatibility under the writer before publication.

HTTP schema discovery reads the current authorized view and masks names by token
scope. Subscription registration uses the live catalog; a later incompatible
mapping explicitly invalidates the retained definition. Configured Beacon indexes
resolve durable names before binding, and a startup frontier check rejects a
publication change between metadata discovery and full opening.

Regression sources cover startup conflicts, historical and prepared reads,
ordinary/batch/conditional/explicit/prepared writes, Bolt graph metadata and
deferred-write refusal, HTTP masking, and subscription invalidation. Edited
server sources pass Rustfmt and diff checks. The workspace formatting check
reports existing drift in unchanged native/schema sources; those sources are
not reformatted here. Compiler/runtime results, Clippy, UBS and exact-tree
qualification remain pending. General schema DDL, constraints, index lifecycle
and multi-coordinate catalogs remain open under `fgdb-7r8sy`.

### CLI resolves durable names at the selected snapshot — qualification pending

The CLI now discovers persisted label, relationship-type and property-key names
before preparing local queries, writes, transactions, CSV imports, loads and
searches. `create --persist-bindings` explicitly registers supplied flags and
bindings-file declarations through the native schema transaction. Existing
flag-only creation keeps sequence zero; a nonempty registration consumes its own
commit sequence. Exact redeclarations are harmless, conflicting names or IDs
refuse, and extra declarations supplied to a read stay local to that invocation.

A native `SymbolCatalogReadView` retains only the authenticated immutable catalog
and selected frontier. Its current-publication path does not materialize graph
payloads. Ordinary opening retains the existing interrupted-publication recovery
and live-writer arbitration behavior. Buffered and direct sealed CLI profiles
select a current-only metadata opener that refuses lagging or unavailable
publication without a resident graph fallback.

Historical binding uses the existing bounded, comment-aware temporal selector
parser and native UInt64 parameter rules. Certificate replay selects names at the
certificate's retained snapshot. Diff and ordered transaction requests that
would share one resolver across different visible catalogs explicitly refuse;
floor-only changes do not cause that refusal. Ordinary reader and writer opens
check the selected frontier before executing or emitting output. Direct sealed
readers pin their existing as-of argument to that frontier. A concurrent
publication therefore cannot silently pair a prepared resolver with another
current generation.

Existing pure input and plan validation still completes before a deferred
metadata-open failure takes precedence. This preserves missing-database refusal
ordering for malformed CSV, invalid batch rows, unsupported buffered plans and
bad analytics arguments. Catalog conflicts and incompatible selected cuts remain
immediate errors. Ordinary metadata discovery may finish recovery of an already
committed publication before later input validation refuses a command; the
refused command contributes no new graph effects.

Regression sources cover cross-process discovery for named graph reads and
writes, streamed and buffered reads, compaction/reopen, historical names and
certificate replay, conflict refusal, ephemeral declarations, old creation
behavior, and a deterministic concurrent-publication import race. Native tests
exercise current, pinned and historical metadata, damaged graph payloads,
interrupted publication and strict bounded-host admission; GQL tests cover
selector syntax, comments, types and limits. Compiler checks, runtime tests,
Rustfmt, Clippy, UBS and exact-tree qualification remain **UNRUN** because the
execution environment is unavailable. Independent source review and exact-base
blob reconstruction are recorded separately from executable qualification.

General schema DDL and constraints, index lifecycle and multi-coordinate catalog
behavior remain open under `fgdb-7r8sy`. This increment makes the durable name
catalog directly usable from the local CLI; it does not close that broader owner.

### Durable symbol catalogs share Chronicle and Strata publication — qualification pending

The native embedded owner now registers immutable label, relationship-type and
property-key names with `Database::register_symbols`. A registration merges
explicit native-width bindings and caller-supplied allocation floors; identical
declarations are a no-op and conflicting names or IDs refuse before publication.
This does not infer allocation floors from unnamed numeric IDs in existing graph
data and does not allocate new symbol IDs on a caller's behalf.

Each accepted extension is one existing Schema delta in the ordinary
capsule/marker transaction stream. Its immutable registered catalog and transition
objects become durable before D1. Admission checks the exact existing predecessor
marker, before root/epoch, coordinate and immutable successor; neither object
names a future assigned commit sequence. FGSM V4 pins the selected catalog beside
the partition root. Catalog-free manifests keep their V3 bytes, and V2/V3 decode
with no catalog.

Normal writes, full replay, checkpoint suffix recovery, read-only and sealed
reads, compaction and adjacency-run publication carry the same catalog generation.
Pinned views keep their names. Historical name selection hides later bindings and
retains conservative allocation floors. Synchronous historical preparation uses
the exact admitted schema generation, including floor-only epochs; explicit
origin history materialization rebuilds that cache before replacing the retained
window. Ordinary prepared writes remain subject to FCW; read-dependent
transactions and refresh/rebase paths may conservatively refuse after a schema
change. No broader schema-compatibility claim is made.

Scrub re-verifies the selected catalog and all retained capsule-referenced
transitions and before/after catalog roots through the ordinary authenticated
storage path. Missing or damaged schema dependencies fence the handle, including
historical objects needed by full recovery. Catalog objects are immutable
transaction dependencies, not graph blocks that derived-data repair can invent.

Eight native regression functions cover named GQL reads without external bindings,
pins/history, idempotence/conflict, normal and full reopen, floor-only historical
preparation, nine post-D2 publication stages, three pre-marker crash boundaries,
forged apply-basis refusal, missing/corrupt schema objects, nonzero-epoch creation
and incremental composition, and standing-view initialization. New standing views
start at the selected catalog epoch; a later schema transition still requires an
explicit rebuild. Eight codec, manifest and storage tests cover canonical bounds,
Unicode/u64 IDs, conservative
floors, epoch/coordinate refusal, legacy bytes and authenticated kind separation.
All new test execution, compiler checks, final Rustfmt, Clippy, registry-check,
UBS and exact-tree qualification remain **UNRUN** because the execution
environment is unavailable. Independent source review and exact-base patch/blob
reconstruction are complete; JavaScript registry transcript checks do not replace
the Rust registry gate.

This is the name-catalog subset of `fgdb-7r8sy`; schema constraints, general DDL,
index lifecycle and multi-coordinate catalogs remain open.

### Write CALL scope imports survive every mutation phase — qualification pending

Commit `07dfae3ba3e8711aff2612ab860f1d32633ef546` extends explicit and wildcard
scope imports through write-bearing CALL bodies: WITH, aggregation and DISTINCT,
CREATE and mutation selection, node/edge MERGE, FOREACH, and nested unit,
returning, optional and UNION calls. Each native invocation retains its metered
immutable argument frame. FOREACH inherits it; nested CALL scopes shadow it and
restore the enclosing frame on completion or error. Typed Arguments and
RestoreArguments stay in the existing algebra and executor. Legacy importing
WITH retains ordinary projection lifetime.

Three GQL and five native regression functions cover the mutation phases,
duplicate/NULL inputs, empty aggregates, nested scopes, late refusal and atomic
rollback. The change is committed directly on main and mirrored to master.
Independent source review and exact-base reconstruction are complete. Compiler,
runtime tests, final formatting, Clippy, UBS and exact-tree qualification remain
**UNRUN** after executor disconnection. Broader parser and write-CALL owners remain
open; this is an unqualified source increment.

### Read CALL scope imports survive WITH — qualification pending

Explicit `CALL (x, ...)` and wildcard `CALL (*)` read subqueries retain
their imported variables across inner WITH projections, DISTINCT, grouping and
ordering. The compiler restores missing names from that invocation's immutable
Apply argument frame through the existing relational operators. Imports are
reattached after aggregation or DISTINCT rather than becoming implicit grouping
keys, preserving the single aggregate row over empty input and each duplicate
outer occurrence.

Nested scopes use their own argument frames, including empty scopes, and restore
the enclosing scope on return. Legacy importing WITH keeps its existing ordinary
projection lifetime. A same-name pass-through of an imported column is legal;
rebinding it to a computed value or aggregate is rejected before catalog access.
Imported names remain available to ORDER BY after DISTINCT, while a truly dropped
local ordering key retains the existing refusal. Hidden property and expression
columns stay behind the visible schema and have their offsets shifted together.
The new restore stage contributes to template identity and definition depth and
uses existing execution work, scratch and cancellation controls.

Five focused GQL regression functions and one native integration regression cover
scope boundaries, NULLs and duplicate rows, empty aggregates, ordered collections,
graph identities, redeclaration preflight on empty inputs, wildcard/explicit
template identity, exact budget refusal, pinned snapshots, staged writes and
capability masking. Independent source review corrected an imported-variable
DISTINCT ordering edge and a test value's boxed-list type. Compiler/runtime tests,
final formatting, Clippy, UBS and exact-tree qualification remain UNRUN after the
execution environment disconnected during dependency compilation.

Read CALL uses its actual Apply frame. The newer write-scope increment above
adds native invocation-frame integration across mutation phases. The broader
`fgdb-w5-parsers-nje` and `fgdb-unit-write-call-z7amx` work remains open.

### Bolt explicit write output admission — qualification pending

The explicit write-transaction adapter now keeps graph serialization, retained
cursor capacity accounting and complete RUN SUCCESS admission inside the native
checked-output callback. It uses the shared graph serializer's measured owned
capacities instead of scanning the result again after native authority checks.
Column copies, summary construction, metadata traversal and encoding spend the
same cumulative signed work allowance as the statement.

The callback reserves the statement completion summary, including maximum-width
integer counter slots, before exposing the private cursor. After native success,
actual counters replace those scalar slots and zero counters are removed without
growing retained allocations. This conservative reservation can refuse a very
small frame limit even when the eventual short counters would fit. Failure in
serialization, frame admission, retained capacity, signed work or live authority
drops the taken transaction and its complete unpublished prefix. No result cursor
is returned before the native final checks succeed.

Regression sources cover exact RUN metadata bounds, one-byte-over refusal with
whole-prefix rollback, retained capacity and counter preservation, cumulative
signed work, and authority retirement during metadata admission. The tight RUN
frame test drives the production connection directly; it does not claim a
demonstrated socket-level exploit. Scoped formatter execution, compiler/runtime
tests, Clippy, UBS and exact-tree proof have no completed passing verdict: the
execution environment disconnected during dependency compilation. Source review
and exact-base reconstruction are complete; this is an unqualified source
increment. The broader Bolt profile beads remain open
(`fgdb-roadmap-bolt-v2-vb6e`, `fgdb-w10-bolt-a0s`).

### Autocommit read graph output retains its selected snapshot — qualification pending

Native commit `5c249eb4` adds `AuthorizedReadSession::query_with_graph_output`.
The query, trusted host conversion and graph metadata reads share one signed
execution permit and the exact bound snapshot, including historical selectors.
The shared output boundary also serves explicit read transactions. User rows
are reserved once before conversion; metadata spends cumulative node/work limits
and an additional native work/scratch ceiling without becoming another user
row. Field, label and both-endpoint masks apply before catalog access or copying.
Late authority, cancellation and swallowed metadata failures prevent delivery
and pending cache publication.

The Bolt autocommit adapter uses this boundary and the existing checked graph
serializer for nodes, relationships, paths and nested values. Complete RECORD
and RUN SUCCESS sizes are admitted before RUN succeeds. The older per-field
metadata queries are removed. Historical output and completion bookmarks use
the selected sequence, while each independent autocommit statement still
receives its own allowance. Unsatisfied-bookmark errors retain the selected
source generation before frontier validation, including recovery during a
partial send or pending flush.

Four native regression sources cover historical/pinned metadata, cumulative
node/work/row limits, opaque host error mapping and late expiry. Three TCP
sources cover signed hydration limits, masked historical graph values across
later writes, exact bookmarks and oversized records before RUN SUCCESS;
existing recovery-output tests also cover autocommit bookmark failures.
Scoped pinned Rustfmt and whitespace checks pass. Compiler/runtime, Clippy,
UBS and exact-tree qualification are still pending.
This increment does not complete durable output retention or the full Bolt
compatibility profile.

### Wildcard correlated read imports — qualification pending

The read compiler now accepts `CALL (*)` and `OPTIONAL CALL (*)`, expanding
the caller's visible variables through the existing explicit-import Apply and
UNION operators. Hidden boundary columns remain private. Write-bearing CALL
already accepted wildcard imports; nested read bodies now use the same spelling
inside atomic write chains. Focused regression sources cover duplicate and NULL
occurrences, empty scopes and inputs, nested/optional/UNION bodies, malformed
imports, canonical-plan and budget parity, pinned and staged graph reads,
capability masking, reopen and complete rollback on late result-limit failure.
Scoped pinned Rustfmt and whitespace checks passed for that wildcard increment;
its runtime qualification remains pending. Read CALL import lifetime and
redeclaration enforcement are added by the newer scope increment above.
Write-bearing CALL lifetime parity is added by the newer write-scope increment
above; broader conformance remains open.

### Bolt read transactions retain native authority — qualification pending

The 2026-11-14 Bolt read adapter retains `AuthorizedReadTransaction` across
BEGIN, RUN and COMMIT. Read-mode BEGIN and Read-only grants select this owner;
write-mode BEGIN with Write or ReadWrite authority retains the existing native
writer. Each read RUN uses the original immutable snapshot and cumulative signed
input, work, node and delivered-row allowances. The host's statement ceiling
also applies. A structured temporal selector must resolve to the BEGIN basis.
There is no database lock between messages.

Graph values, including nested lists, maps, relationships and paths, are hydrated
through `query_with_graph_output` under the original permit and masks. Metadata
does not count as another user row or statement. The shared Bolt encoder admits
actual owned capacities, complete RECORD sizes, transaction cursor retention and
the RUN SUCCESS frame before native final delivery. Failures discard the owner
and all provisional cursors. COMMIT consumes the native owner and rechecks live
authority/cancellation, returning the original snapshot bookmark without a new
commit sequence.

Data-dependent failures now retain the source recovery-generation fence through
physical writes and flushes. Read and write BEGIN retain their exact selected
generation before basis-dependent bookmark refusals. Only typed recovery and
unavailable controls and unknown write completion are generation independent; successful publication cannot be
turned into a retryable precommit failure. The read owner and its graph output
stay pinned while independent FGP writes advance the head. Durable transaction
leases, reattachment, full Bolt V2 and full SSI remain outside this increment.
Autocommit reads now use the same native output boundary through their
per-execution ReadSession, as described in the newer autocommit increment above.

Six added regression functions cover signed metadata-node and user-row budgets,
concurrent FGP writes with frozen/masked graph cursors, original bookmarks,
oversized graph output before RUN success, statement and temporal limits, live
authority at an unused owner's close, and recovery fencing for source-derived
query and read/write BEGIN failures at unwritten, partially written and pending
flush boundaries. Independent native/API and adapter source reviews completed.
The executor disconnected before final Bolt formatting, static scanning, compilation
or runtime checks, so those gates remain **UNRUN**. The prior 58-test protocol
result excludes the server adapter and is not Bolt qualification. The recovered
sources were reviewed through GitHub; no passing integrated-tree claim is made.

### Optional write subqueries and graph-output admission — qualification pending

The 2026-11-14 write-bearing `OPTIONAL CALL` increment uses the existing native
write-chain owner. A returning body that produces no rows preserves each outer
occurrence with NULL exports, after all UNION arms complete. Body effects remain
staged even if its later filters remove every result. Duplicate and NULL outer
values retain their occurrence semantics; typed graph export domains are
preserved. Unit bodies accept OPTIONAL as redundant. Nested calls and preceding
or following reads and writes retain the existing transaction owner. Errors,
cancellation, authority and budget refusals propagate without null extension.

The ordinary `Database::query_write`, native transaction and server prepared
write paths all reach the same compiler. Parser regressions and six native
integration scenarios cover public dispatch, UNION, unit/nested calls, empty
input, aggregates, durable effects and reopen, rollback preserving a staged
prefix, preflight refusals, cumulative creation/result limits and late expiry.
[The README](README.md#writing-optional-subqueries--qualification-pending)
shows a writing optional subquery.

The original eight-file working patch passed pinned scoped Rustfmt, whitespace
checks and independent source review. Static UBS exited 1: its sole critical
report is the integration-test helper's intentional panic on an unexpected query
failure (632 warnings and 211 informational findings were also reported). No
suppression or gate change was made. After the executor disconnected, the reviewed
patches were recovered through GitHub; reconstructed test formatting has not been
rechecked, so the earlier checks are not a final-tree verdict. Compilation,
runtime and Clippy remain UNRUN under the foundation build failure and outage.

Commit `cdc19771` separately adds
`AuthorizedReadTransaction::query_with_graph_output` and the bounded
`AuthorizedReadOutput` metadata interface. Trusted host conversion can hydrate
graph values from the original immutable snapshot under the original owning
Read permit. Existing field/label and both-endpoint masks apply before catalog
access and copying. Metadata calls spend cumulative signed work and node limits;
a shared output-phase evaluator bound limits their work and scratch independently
of the statement evaluator. A swallowed metadata refusal still closes the owner.
Only successful final delivery publishes pending exact-count cache entries.

Native metadata regression sources cover concurrent graph changes, masked labels
and properties, graph identity hydration, hidden endpoints, cumulative
limits, missing catalog entries, swallowed errors and pin release. The committed
API passed scoped Rustfmt, whitespace checks and independent source review.
Its static UBS scan exited 1 on the already-reviewed diagnostic-enum equality
false positive. Native runtime and full integrated qualification remain UNRUN.

### Marker clocks and supplied predecessor identity — qualification pending

Commit `c0671b7` validates committed time and supplied global predecessors in
Chronicle admission and both retained and streaming recovery. The first marker
must have positive commit HLC, each successor must strictly advance it, and a
history ending at `u64::MAX` cannot accept a successor. A supplied `prev_global`
must match both the immediately preceding marker OID and commit sequence; no
predecessor is valid at genesis. Existing writers and histories that use the
optional `None` representation remain supported. No durable encoding changed.

Invalid proposals leave commit sequence, heads and allocation floors untouched
and refuse before capsule D1 publication. Regression sources include exact-link
and clock failures, exhaustion across reopen, and fully rehashed on-disk
corruptions rejected by complete and checkpoint-prefix recovery. Complete
corruption is not truncated as an incomplete tail. Scoped pinned Rustfmt,
whitespace checks and static UBS passed (0 critical findings). Runtime tests and
Clippy remain UNRUN because the pinned foundation build was SIGKILLed before
first-party compilation. This increment does not supply the normative root,
predecessor-population or HLC evidence owners.

### Cumulative read-only transactions and allocation floors — qualification pending

Source added 2026-11-14 implements an explicit native read owner in `eb3c3e7`
and a connection-scoped FGP adapter. `Database::begin_authorized_read` and the
matching pinned-view method admit Read credentials, then retain one snapshot
and one owning Warden `ReadAccess` permit. Sequential requests use the existing
selector, compiler and masked native executor. Original input, execution, nodes,
rows and checked output spend cumulative signed limits; per-statement native
policy remains separate. A statement never renews the permit or borrows a past
request's context. Different bound temporal cuts refuse before source access.

FGP `BEGIN_READ_TRANSACTION` (`0x0037`) and Rust
`Client::begin_read_transaction` use the existing transaction handles and
provisional result/completion machines. Client-side wrong-mode admission sends
nothing; a raw Write request aborts the server owner. Read-only completion must
be `ReadClosed` at the original basis with the exact statement count. Closing
allocates no commit sequence. Other writers can advance independently, and
recovery fences both physical rows and source-dependent errors from the old
generation. The client transaction regression helpers were also moved out of
the accidentally unclosed production `server_error` function and back into
their test module. This repairs compilation structure and test registration;
transport-feature runtime execution is still unqualified.

Native regression sources cover scoped Read credentials, stable snapshots,
cumulative node/work/row limits, pre-conversion row admission, swallowed meter
failures, temporal selection, cancellation, expiry, retirement, clock rollback,
unwind and pin release. Adapter sources add four TCP scenarios, client mode and
completion-forgery cases, and generation-fenced row/error delivery. Row admission
does not publish pending exact-count caches; only final successful output does.
The [Fabric protocol](docs/FABRIC_PROTOCOL.md#explicit-read-only-transactions)
documents the API and bounds. Durable leases, reattachment, outcome lookup and
full SSI remain outside this increment; the broader transaction and FGP beads
remain open.

Chronicle commit `c6eacb2` independently protects committed allocation floors.
Both retained and streaming marker validators reject a decrease in either vertex
or edge counters, preserve floors across legacy counter-less markers, and update
their constant-size state only after all admission checks succeed. Engine reopen
uses that validated floor. Regression sources include pre-D1 refusal, later CAS
failure, exhausted counters, fully rehashed corrupt logs, complete recovery and
checkpoint-prefix recovery. No durable encoding changed. This does **not** solve
the separate CREATE RETURN / rollback / reopen first-escape reservation gap;
`fgdb-w2-id-allocator-3kd` and `fgdb-w2-escape-bindings-qcp` remain open.

Validation on the pinned toolchain: `cargo test --locked -p fgdb-protocol --lib
--test protocol` passed **41 unit and 17 integration tests**. That default-feature
run excludes transport and therefore proves neither client nor server runtime
behavior. Scoped Rustfmt and whitespace checks passed. Static UBS passed for the
Chronicle repair. The native-owner scan exited 1 on a reviewed false positive
comparing diagnostic error enums. The adapter scan also exited 1: its eight
critical reports concern existing public binding equality, binary frame-header
test assertions and an unreachable constant nonzero-construction panic. Those
were reviewed as false positives; neither scan is reported as a green gate.
Static scanning is not runtime qualification.

The restored combined production check was SIGKILLed in pinned `asupersync`
before first-party compilation, both with the repository frontend scheduling
and with one compiler frontend thread. A distinct normal-profile Chronicle test
attempt without server TLS features was also killed there. No dependency pin,
source, test profile or assertion setting was changed to obtain a passing gate.
Native/Chronicle/GQL/server runtime tests, Clippy and complete integrated-tree
qualification remain **UNRUN**. These terminal build observations supersede the
earlier interrupted-build report below. The final HTTP regression file was also
rechecked with pinned Rustfmt after the executor returned.

### Atomic HTTP transactions and OPTIONAL CALL — qualification pending

Source added 2026-11-14 in `ad50728` and `84d5aac` implements two bounded surface
features through the existing native transaction and relational owners.

`POST /v1/databases/<name>/transaction` accepts 1–32 independently parameterized
Read/Write statements. Later statements see earlier staged effects, while the
original capability, basis and cumulative allowances remain in force. All input
adaptation precedes BEGIN; all result conversion and admission of the complete
typed JSON response precede the sole COMMIT. Retention includes JSON escaping
and a reservation for maximum completion counters. A definite earlier failure
discards the unpublished workspace and exposes no successful prefix. Native
unknown completion remains unknown, and physical response bytes retain the
original recovery-generation fence. Read-only scripts still require write
authority because this endpoint owns a write-capable transaction; read-only
credentials use `/query-batch`. The full request, response, limits and curl
example are in [the Fabric protocol](docs/FABRIC_PROTOCOL.md#atomic-http-transaction-scripts).

Inline returning read-only `OPTIONAL CALL` and named optional procedures lower
through native Optional Apply. Empty invocations preserve each caller occurrence
and null-extend only exported columns. A procedure's attached `YIELD ... WHERE`
belongs inside optional evaluation; an explicit following `WITH ... WHERE`
filters the completed row. Existing typed identities, imports, aggregation,
UNION, host errors, capability masking and cumulative cancellation/resource
controls remain owned by the native operators. Optional write bodies are added
by the later write-chain increment above. Wildcard scope imports for read-only
CALL and full language conformance remain outside this read increment.
[The README](README.md#optional-subqueries-and-procedures--qualification-pending)
shows the syntax.

Committed regression sources cover HTTP staged visibility, one completion,
rollback, empty/net-zero writes, cumulative and escaped output limits,
authorization, input cancellation and recovery fences; query sources cover
empty results, duplicates, parallel edges, NULL correlation, aggregate/UNION
semantics, procedure filtering and errors, cumulative limits, cancellation,
private transaction topology, pinned snapshots, certificate replay and masked
authorized results. The late arithmetic HTTP regression preserves the existing
native `execution` class (HTTP 500), including a data-dependent mutation failure.

Qualification is incomplete. Pinned Rustfmt and whitespace checks passed for
the two feature checkpoints. Scoped static UBS over the four changed HTTP Rust
files exited 0 with 0 critical findings, 285 warnings and 55 informational
findings; it ran with `--no-cargo` and is not runtime evidence. Whole-workspace
`cargo fmt --check` exited 1 on pre-existing formatting differences.

The pinned `nightly-2026-08-31` toolchain and `cargo fetch --locked` completed.
`cargo check --locked -p fgdb-gql -p fgdb -p fgdb-server -p fgdb-cli --lib --bins`
was still compiling dependencies (last observed: `nix` and `num-bigint`) when
the execution environment became unavailable. No terminal compiler verdict was
received. Changed-source compilation is interrupted/unqualified; targeted
runtime tests, Clippy, full workspace tests and exact-tree proof remain
**UNRUN**, and the final documentation/test-expectation update could not be
rechecked locally. No passing integrated-tree verdict is claimed. The existing
`fgdb-w10-adapters-b1f` and `fgdb-w5-parsers-nje` beads remain open with their
original broader scope and blockers.

### Explicit no-effect writes retain write completion — qualification pending

Source updated 2026-11-14 preserves the compiler's write classification when
an authorized explicit statement selects no rows or emits no intents. After
successful statement and output admission, the native owner retains an empty
canonical prepared template if no write is already prepared. Subsequent reads
cannot downgrade it to `ReadClosed`; completion validates the original read
and absence observations, binds the authorization decision, and publishes once
through the existing Chronicle path. Public empty `WriteBatch` admission and
ordinary read-only completion retain their existing contracts.

Regression sources cover empty MATCH mutations with and without RETURN,
empty UNWIND creation, later reads and real writes, exactly one committed
sequence with unchanged graph data, signed decision persistence on reopen,
rollback/drop/output refusal, and a concurrent insertion into the previously
empty selection. Independent source review, pinned Rustfmt for the changed
files and whitespace checks passed. Compilation, runtime tests and complete
exact-tree qualification remain pending under shared-host resource pressure.
`fgdb-w2-txn-lifecycle-mhae` remains open.

The same review identified a separate unresolved first-escape gap under
`fgdb-w2-escape-bindings-qcp`: provisional engine identities are allocated in
memory, and their counter floor is persisted only by a later engine commit.
CREATE RETURN followed by rollback and immediate close/reopen can therefore
reissue the provisional identity. This is source-traced, not yet a runtime
verdict. It requires an ordered, recoverable allocation reservation before
escape; the existing transaction snapshot and graph atomicity must survive
that reservation. No durable first-escape guarantee is claimed by this slice.

### Capability-scoped point mutations — qualification pending

Source updated 2026-11-14 routes both returning and nonreturning mutation
continuations through the canonical seeded overlay selector under narrowed
capabilities. The selector applies visibility masks before expansion, declines
hidden seed properties and retains its existing fallback and transaction
observations. Point-target SET/REMOVE continuations no longer require full
incidence and property authority to avoid admitting the unrelated graph.

The focused regression covers property roots, earlier staged creation and
rekeying, returning and nonreturning mutations, equal-key hidden vertices,
hidden properties, and identical accounting across 8- and 128-vertex visible
populations. It also checks exact allowances and atomic rollback one work unit
below the measured requirement. Independent source review, pinned Rustfmt for
the changed files, and whitespace checks passed. Runtime and exact-tree
qualification remain pending; no performance or full-SSI claim is made.
The existing `fgdb-2acff` and `fgdb-cpny9` work remains open.

### Managed FGP transaction batches — qualification pending

The Rust client's `execute_transaction_batch` and callback-based counterpart
compose the existing explicit transaction owner for 1–32 independent Read/Write
statements. All modes, values and complete transaction request envelopes are
admitted against the negotiated frame limit before BEGIN, including selector
overhead. The server retains the original signed authority, cumulative native
limits and staged own-write visibility. No new wire operation or commit lane
is introduced.

Collected results become available only after a validated native completion.
Streaming callbacks carry statement indexes and the original basis; all remain
provisional until the final completion. A rejected statement-end callback rolls
back the drained owner before another statement or commit. Existing row/schema
rejection uses cancellation and terminal draining. A guard fences active
ownership on future drop or unwind; unconfirmed rollback cannot leave the
connection available for another operation. Once COMMIT is attempted, the
driver sends no rollback or retry and preserves unknown completion.

Regression sources cover mixed statements, independent typed arguments, exact
negotiated-envelope admission before BEGIN, late local and server refusals,
callback rollback, cleanup fencing and missing or invalid commit receipts.
Source review is separate from execution evidence. Compilation, Rustfmt,
Clippy, UBS, runtime tests and exact-tree qualification remain **UNRUN**: the
workspace disk was full, then the executor disconnected during dependency
setup before a compiler process was acknowledged. The existing
`fgdb-w2-txn-lifecycle-mhae`, `fgdb-w10-fgp-core-5b1` and
`fgdb-w10-result-machines-eh7` work remains open. This is the connection-scoped
local transaction subset, without durable leases, reattachment, outcome lookup
or full SSI.


### Transaction dependencies and subscription wire bounds — qualification pending

The first ordinary graph read in an authorized native transaction now retains
the canonical transaction source even before any mutation has been staged.
Both ordinary pattern selection and factorized endpoint selection therefore
keep the existing positive and absence observations for final validation.
A concurrent change after the last statement can no longer evade that native
validation through the former immutable-source fallback. This preserves the
existing conservative conflict profile; it does not establish full SSI or
new commit-refusal noninterference.

Subscription delivery now caps each fragment by encoded bytes, row count and
cumulative value nodes. The subscription codec exposes entry costs through the
same result-row cost owner. A schema and first row that fit in separate frames
are sent separately; a row that cannot fit by itself remains a typed refusal.
The original flow-credit, cancellation, signed-weight and frontier owners
continue to govern delivery.

FGP explicit transactions retain the execution generation while sending an
ordinary statement or definite commit refusal. Recovery before the first byte
replaces the stale diagnostic with a recovery terminal; recovery after partial
output closes the transport. OutcomeUnknown and recovery/unavailability
terminals remain independent of the invalidated generation, so a completed
write cannot be presented as a retryable statement failure.

Regression sources cover changed and absent read dependencies before the first
write, read-close and staged-write completion, MERGE endpoint phantoms and a
label-domain-disjoint control; subscription frame/node/entry boundaries and
schema separation; and blocked or partial transaction refusal delivery.
Independent source/API reviews completed. The local executor disconnected
during validation. Final-tree compilation, runtime tests, Rustfmt, Clippy, UBS
and full exact-tree qualification remain **UNRUN**. These repairs do not close
`fgdb-w2-txn-lifecycle-mhae`, `fgdb-w4-ssi-witnesses-jds`,
`fgdb-w10-fgp-core-5b1` or `fgdb-w10-result-machines-eh7`.

The CLI now exposes this driver as `remote transaction --statements-file`.
Its complete regular UTF-8 JSON array contains 1–32 explicitly classified
read/write statements with independent typed arguments. Controlled parsing,
input/value bounds and client frame admission precede credentials. The command
rejects unrelated input, read-mode, conditional-write and prepared-mode flags.

Columns and rows flush without collecting the full answer and carry an
explicit provisional marker plus a one-based statement index; columns retain
the original basis sequence. Only the final `transaction` result asserts
write-commit or read-close completion. Original output failures are retained.
Unknown completion produces no success or rollback claim, while output failure
after a validated receipt retains the known sequence and forbids automatic
replay. Established-connection transport failures keep the I/O exit class.

The CLI regression sources exercise literal robot/human output, empty results,
full-width and fresh parameters, late refusals, callback cancellation with END
in flight, exact negotiated frame boundaries, lost completion and failed final
flush. Parser controls cover malformed late input, finite count and byte/value
bounds, and cancellation. The independent robot schema freeze is extended for
the transaction fields; its previously missing prepared-batch description is
also reconciled with the already implemented contract. Source/API review and
JSON parsing of the embedded schema completed. All Rust compilation, runtime
tests and repository gates remain **UNRUN** under the executor failure above.

### Consistent prepared read batches across FGP and the CLI — qualification pending

Source added 2026-11-14 exposes `EXECUTE_PREPARED_READ_BATCH` (0x002d)
and the Rust client's collected and callback APIs. One connection-owned
prepared read accepts 1–32 fresh argument sets. The server attaches the frozen
template once to one authorized native read session and executes all sets
through its existing prepared-batch owner. Ordinary reads share the session's
pin; explicit temporal arguments retain their selected cuts. Every set shares
the same cumulative signed allowance.

Handle ownership, read-rights precedence, release/refresh invalidation and
database-generation fences apply. Complete execution, conversion and wire
admission finish before the first schema or row. Later binding, quota or
representability failures expose no successful prefix. Existing batch result
framing, flow credit, cancellation and terminal draining remain the delivery
owner. Unsupported peers or refused requests never trigger independent reads
or an automatic retry.

The CLI selects this operation explicitly with `remote --prepared-batch
--rows-file <path> query <statement>`. Complete JSON/CSV and typed-value
admission, the 32-set limit and both request envelopes are checked before
credentials or connection creation. The first set supplies representative
structural types for one PREPARE; every execution supplies its own complete
values. The flag requires buffered reads, is mutually exclusive with
`--consistent-batch`, and uses the same indexed output and one final
`read_batch` terminal. Raw consistent batches retain their existing semantics.
Parameterized rows-file JSON now uses the controlled parser so cancellation
also reaches input parsing.

Regression sources cover exact wire and aggregate value bounds, fresh binding,
current/historical cuts, scoped visibility, cumulative budgets, late refusal
without a successful prefix, foreign/released/refreshed handles, callback
cancellation, recovery before send and while credit-blocked, JSON/CSV CLI
requests, failed output, and local rejection before credential access.
Independent protocol, server and CLI source/API reviews completed; the embedded
robot schema parses as JSON. Before these changes, the pinned compiler ran
**37 core unit tests and 17 protocol integration tests** on an immutable
baseline. Those results do not validate this implementation. The executor
disconnected before changed-source compilation; compilation, Rustfmt, Clippy,
UBS, new runtime tests and full exact-tree qualification remain **UNRUN**.
The broader `fgdb-w10-fgp-core-5b1`, `fgdb-w10-result-machines-eh7` and
`fgdb-5jyok` work remains open.

### Capability-scoped seeded lookups over staged writes — qualification pending

Source added 2026-11-14 removes the all-fields/all-incidence restriction from
eligible identity- and property-seeded write continuations. Narrowed write
chains and mutation sources can follow carried vertex identities or visible
property roots without admitting the unrelated visible graph. This includes
relational continuations after UNWIND or staged hybrid retrieval. The existing
immutable-read closure now shares one borrowed-source interface with the
canonical transaction overlay; masking, connected closure discovery and GLA
evaluation have one implementation.

The overlay resolves staged winners and tombstones before admission, so
earlier CREATE, SET, rekey and DELETE effects are visible without reviving
basis rows. Label, relation and endpoint masks apply before expansion;
property masks apply before values reach the evaluator. Reverse, undirected
and bounded quantified traversals use the same visible closure. A lazy
incidence index over staged edge coordinates avoids rescanning every staged
edge at every reached vertex. Hidden seed properties and unsupported
definitions retain ordinary masked admission.

Point absence, property membership and relation-gap observations are retained
before source probes. A property observation completes only after successful
evaluation; refusal, cancellation and savepoint rollback retain the existing
conservative conflict dependencies. Native budgets and the original signed
permit continue through every continuation. Physical hidden-source probes
only poll, while visible admission and execution consume their existing
allowances. This remains the native bounded conflict-checking profile, not a
claim of complete SSI or a whole-process memory bound.

Three regression sources compare seeded answers against complete masked
admission, clean versus hidden graphs, staged rekeys/deletions and forward/
reverse/undirected/quantified traversal; check exact and one-less native
thresholds and every signed interruption boundary; exercise post-read
concurrent point, membership, relation and absence conflicts plus savepoint
rollback; and run a public narrowed write chain with an initial CREATE and
successive SETs across 8- and 128-vertex populations, including full rollback
on late refusal. Source/API review completed independently. Changed-source
compilation, formatting, Clippy, UBS, runtime tests and exact-tree
qualification remain **UNRUN** after the executor disconnect described above.
The broader `fgdb-2acff`, `fgdb-cpny9` and
`fgdb-authorized-staged-hybrid-w7g9p` work remains open.

### Explicit FGP native transactions — qualification pending

Source added 2026-11-13 under `fgdb-w2-txn-lifecycle-mhae` and
`fgdb-w10-fgp-core-5b1`. FGP now connects BEGIN, transactional execution,
COMMIT and ROLLBACK to the existing `AuthorizedWriteTransaction`. One Ready
connection owns one private workspace, original signed permit and recovery
generation. It holds no database lock between requests. Graph/set reads and
exact terminal aggregates observe earlier staged writes; other connections
see only committed state. Native scope, source/work/scratch, result-row,
creation, mutation and statement allowances remain cumulative.

The wire uses additive tags 0x002e–0x0036, following the independently added
prepared-read-batch tag 0x002d. Provisional rows have distinct transaction
chunk/end frames with a connection-owned handle, one-based statement ordinal
and exact delivered row count. Existing autocommit encodings are unchanged.
The Rust client exposes collected and callback-based execution, validates
every result and terminal binding, and reports durable completion only after
the correct handle, original basis and completed statement count agree.
An empty owner may read-close; it cannot claim a committed write.

The dispatcher takes ownership before fallible operations and restores it only
after the complete provisional result was delivered. Statement refusal,
cancellation, frame/value admission failure, disconnect, drain and recovery
discard the unpublished prefix. Cancellation of the latest completed stream
also aborts its retained owner, covering callback rejection after END was
already sent. Idle recovery wakes the connection and releases the owner.
Unrelated business requests and credential refresh are Busy while a
transaction is active, preserving the same authority and allowance.

COMMIT consumes the owner before awaiting native publication. Subscription
notification follows completion and writer-lock release. No late recovery
fence turns a committed write into a retryable refusal: it produces
OutcomeUnknown or closes the transport, and the client never replays it.
The native scoped binder is shared with Bolt, preserving its classification
and error mapping. Negotiated schema/row admission covers ordinary and exact
aggregate output before native statement success.

Regression sources add three body laws, six client laws, four native owner
laws and four real TCP laws, plus the existing phase/direction test extension.
They cover exact bytes, malformed/truncated bodies, shared node bounds,
provisional delivery, own-writes, one publication, rollback/drop, cumulative
limits, mode/handle isolation, callback cancellation including a deterministic
post-END cancel, stale basis and ambiguous completion. Independent source/API
reviews and whitespace inspection completed. The pinned compiler was located,
but the executor disconnected during dependency setup before compilation.
Compilation, Rustfmt, Clippy, UBS, runtime tests and the full exact-tree proof
remain **UNRUN**. Broader lifecycle and FGP beads remain open.

This is the connection-scoped local transaction subset. Durable ownership
leases, restart/reattachment, durable outcome lookup, full SSI, general
historical writer reads and non-catalog transaction procedure hosts remain
separate. Concurrent basis advancement retains native conservative refusal
and native final conflict validation.

### FGP value and write-result admission — qualification pending

The protocol codec now shares one value-node allowance across each complete
request or result body. Encoder admission matches the decoder for nested
lists/maps, packed identity vectors and paths, including parameters spread
across batch records. Collection decoding refuses a declared count that cannot
fit the remaining node allowance before reserving member storage. Row-cost
measurement exposes both encoded bytes and value nodes to chunk assembly;
zero-weight subscription entries refuse symmetrically.

Raw, prepared and exact-frontier conditional FGP writes admit their complete
RETURN schema and every delivered row against the negotiated frame limit
inside the existing checked native write owner, while rollback is still
available. The same signed permit pays the existing conversion/codec work.
Schemas and rows are admitted separately, allowing a schema-only first frame.
Ordinary result chunks respect both the row-count and cumulative value-node
ceilings. A defensive framing refusal reached after a committed write reports
OutcomeUnknown, with no automatic retry. Cancellation and transport failure
continue to govern delivery and do not establish rollback.

Five protocol regression sources cover exact and one-over node boundaries
across nine request bodies and four output bodies, literal row costs, early
collection refusal and nonzero subscription weights. A collector regression
covers independent schema/row thresholds and unchanged work. Three TCP
regression sources cover raw/prepared/conditional rollback, freshly rebound
parameters, stale-frontier precedence, a late oversized row with zero result
prefix, exact-size schema-separated delivery, LIMIT 0 and durable reopen.

Source review and independent boundary arithmetic are complete. Compilation,
Rustfmt, Clippy, UBS, runtime tests and full exact-tree qualification remain
UNRUN because the execution environment disconnected after its workspace disk
filled. This bounded work belongs to `fgdb-w10-fgp-core-5b1` and
`fgdb-w10-result-machines-eh7`; neither broader bead is closed.

### Heterogeneous consistent read batches from the CLI — qualification pending

Source added 2026-11-12 for `fgdb-5jyok.1` exposes the existing
`EXECUTE_READ_STATEMENTS` operation as `fgdb remote query-batch
--statements-file reads.json`. A finite JSON array supplies 1–32 independent
read texts and typed parameter maps. Ordinary reads share one pinned native
session and one cumulative signed allowance; historical selectors keep their
own cuts. Each result retains its own schema, one-based statement index and
exact sequence. The existing `read_batch` robot terminal confirms the entire
delivery, with no new result framing.

Complete regular-file, controlled JSON, typed-value and encoded-frame admission
precedes credential access and connection. Output failure uses the existing
cancel-and-drain owner. Refused or unsupported operations never fall back to
independent reads. Existing parameterized consistent batches share the same
delivery implementation and retain their human and robot output.

Eight regression sources cover flags, independent parameter domains, malformed
late input, raw and encoded size bounds, cancellation during JSON parsing,
different schemas and historical cuts, refusals, and failed output delivery.
Changed-file pinned formatting and whitespace checks pass; independent source
review found and corrected a JSON-parser cancellation gap. Production-only
static UBS checking reports zero critical findings (65 warnings
and 111 informational findings in the selected files). Compilation and
runtime qualification remain pending after the pinned asupersync compiler
was SIGKILLed before first-party compilation in ordinary and serial/no-debug
checks. This source change does not claim a passing workspace gate.

### Heterogeneous consistent read batches over FGP — transport qualification pending

Source added 2026-11-13 exposes the existing native mixed-statement read batch
through `EXECUTE_READ_STATEMENTS` (0x002c). The Rust client provides
`execute_read_statements` and `execute_read_statements_streaming` for 1–32
independent read texts, parameter maps and result schemas. Ordinary current
reads share one pinned session; explicit historical selectors retain their
individual cuts. All reads share the same cumulative signed execution allowance.

The request uses the existing authorized batch executor and result delivery
owner. Native execution and whole-answer wire preflight finish before the
first schema or row; a later execution/preflight refusal exposes no successful
prefix. Streaming refers to delivery of that buffered result. Existing flow
credit, cancellation/draining, authority and database-generation fences apply.
Structural non-read modes refuse before dispatch; write-spelled read text
cannot mutate. The client and decoder both enforce one body-wide value-node
allowance across every statement. Existing frame encodings are unchanged;
older servers reject the new request without a client fallback.

Three codec regressions cover fixed bytes, truncated/trailing payloads,
count/mode/parameter rules and exact cumulative node admission. Five TCP
regression sources cover mixed schemas and historical cuts, masking, shared
budgets, late errors, connection reuse, flow windows and callback cancellation.
The pinned core protocol suite passes **32 unit tests and 17 protocol tests**
with one test thread; core protocol Clippy with `-D warnings`, changed-file
formatting and whitespace checks pass. The optional transport/client and
server compilation and TCP execution remain **UNRUN** after the pinned
asupersync compiler was SIGKILLed in both ordinary and reduced-memory builds.
Full workspace and exact-tree qualification remain open. This closes a native
SDK entry-point gap, without claiming explicit FGP transactions or full FGP
conformance.

### Capability-scoped maintained analytics — qualification pending

Source added 2026-11-12 for `fgdb-scoped-maintained-analytics-s047k` connects
the existing component, core and triangle maintainers to capability-projected
subscription inputs. Connected, weakly connected and strongly connected
components, core numbers, triangles and clustering coefficients can now
register through native and server subscription owners under narrowed label,
relation and property scopes, within their existing maintained profiles.

Scoped registrations always compile the selected row topology, including
isolated visible vertices and both endpoint masks. They never use privileged
direct graph sources. Resource preflight accounts for the complete projected
circuit. Selected maintainers validate and bootstrap at the capability-visible
frontier, so hidden history does not change their public sequence or make the
parent frontier inconsistent. Unsupported procedures and profiles still refuse
before source access. Existing polling authority, replay, ACK and cleanup owners
remain in force.

The six-algorithm server regression compares literal expected answers, deltas,
frontiers and registration reservations across clean and hidden-data databases.
It covers hidden prehistory and later hidden-only commits, reciprocal/parallel
edges, isolates, deletion, insertion, and physical endpoint deletion versus
loss of its permitted label. It also checks budgets and changed/expired tokens.
Pinned formatting of changed Rust files and independent source/API review pass. The pinned compiler
was SIGKILLed inside unchanged `asupersync`, before first-party compilation;
runtime tests, Clippy and exact-tree qualification remain UNRUN.

### Authorized hybrid retrieval over staged writes — qualification pending

Source added 2026-11-12 for `fgdb-authorized-staged-hybrid-w7g9p` connects
`CALL hybrid.search` to the canonical authorized transaction overlay. Text,
vector and materialized graph retrieval observe earlier CREATE, SET and DELETE
effects, and returned vertex identities can feed later writes in the same
statement. All authorized write-source families share the adapter.

Vertex, property, relation and endpoint masks apply before corpus statistics,
vector validation and routing, or graph expansion. Native work, scratch and
source limits share the original signed permit and retain the first typed
refusal. Private search rows do not consume the final delivery allowance.
The surrounding statement owns rollback and its single final publication.
Explicit indexed and graph-only demand retrieval are supported by the
extensions below. A demand request combining text or vectors still refuses
instead of changing profiles.

Source extended 2026-11-13 requires exact vector retrieval: explicit ANN
requests refuse before source acquisition because these private rows can feed
later mutation. The retained permit is revalidated after result conversion.

Source extended 2026-11-23 adds `graph_access => 'demand'` for graph-only
retrieval in authorized staged write chains. Membership probes borrow the
pinned history through the canonical sparse overlay; adjacency follows only
reached incoming, outgoing or undirected incidences. Earlier staged CREATE,
label changes and DELETE effects are visible, including newly created edges
and deleted seeds. This path does not build a complete vertex corpus or
materialize the graph's unrelated edges. It retains full-width identities,
parallel edges and self-loop semantics from the canonical adjacency source.

Authority is resolved before visible node, edge, work and scratch admission.
Authorized vertices rejected by a user label selector still spend their
ordinary probe allowance; forbidden labels and hidden transit do not affect
observable receipts. Membership, incidence, BFS, eligibility, fusion and result
conversion share the original statement allowances. Empty and refused probes
retain a conservative all-change conflict witness; this is not a claim of
precise predicate SSI or a process-memory bound. The surrounding transaction
still publishes once, after the entire continuation succeeds.

The same extension adds `graph_access => 'indexed'` for staged graph-only and
mixed text/vector/graph retrieval. It builds the existing masked vertex corpus
and exact search index, then visits only reached adjacency through the shared
canonical source. The full visible vertex corpus still spends its ordinary
allowance; disconnected edge populations are not materialized or admitted.
Text statistics, vector distances, per-lane ranks, endpoint eligibility and
fusion retain their existing semantics. The selected directory resolves
endpoint visibility before incidence admission without charging nodes twice.
Ordinary corpus observations and the global edge insertion-gap witness remain
conservative transaction dependencies. The default materialized profile and
the exact-vector requirement are unchanged.

Regression sources cover staged multimodal retrieval, hidden malformed data,
exact native and signed resource boundaries, private rows with zero public
allowance, rollback under LIMIT 0, and live capability expiry. The FGP
conditional-write regression covers create/search/update, stale-frontier
rejection and late result errors without publication. Further FGP regressions
cover raw and freshly rebound prepared write chains; Bolt explicit transactions
cover staged text, vectors, topology, property hydration, commit, rollback and
failed-transaction reset after a refused search profile.

Demand regression sources additionally compare all three graph directions
with literal expected hops and materialized results, exercise sparse staged
topology and label changes, and compare complete receipts with hidden rows
present or physically absent. Exact allowance tests cover a small reached
component amid unrelated visible data. FGP loopback coverage exercises a
conditional create/search/update, prepared values rebound on each execution,
late-result rollback and stale-frontier rejection.

Indexed regression sources assert literal lane ranks and exact parity with
materialized outputs in all three directions. A fixed visible corpus with 256
additional disconnected edges checks unchanged indexed receipts and separate
native record/work refusals for materialized access. Additional cases cover
one-less source allowances and successful write publication under LIMIT 0
with no public row allowance. FGP covers staged mixed-lane search/update and
late-result rollback; Bolt explicit transactions exercise demand, indexed and
mixed indexed reads before commit, plus rollback after an unsupported mixed
demand request.

Earlier qualification attempts were SIGKILLed in the pinned asupersync
dependency before first-party compilation. During the 2026-11-23 extension,
the locked dependency fetch completed and the pinned compiler progressed
through dependencies, but repeated executor losses prevented a first-party
verdict. Independent source/API reviews, pinned formatting of all changed Rust
files and whitespace checks passed for both increments. At landing, integrated
compilation, runtime tests, Clippy, UBS and exact-tree qualification remain
**UNRUN**; the bead stays in progress.

### UNION inside write-bearing CALL — qualification pending

Source added 2026-11-12 extends the native scoped write-chain owner for
`fgdb-unit-write-call-z7amx`. A CALL body can contain independently scoped
read and write arms, with a common RETURN schema or unit bodies throughout.
Explicit imports and legacy importing WITH are resolved independently for each
arm. Returned column names are aligned before the existing typed set kernel
applies UNION ALL or DISTINCT. Deduplication belongs to one outer occurrence;
duplicate outer rows still invoke every arm and retain their effects.

Arms execute in textual order against the same transaction overlay. A later
arm sees earlier staged writes; imported identities observe current properties
while separately captured scalar values keep their original values. Nested
CALL and FOREACH retain the bounded native scheduler. Prepared CALL metadata
exposes typed arms through `arms()` and `PreparedGraphCallArm` instead of one
write-only `body()`. Read-only arms keep the existing relational executor, and
entirely read-only CALL retains its original owner.

Authorized execution admits every arm before evaluating its outer source,
including empty sources and LIMIT 0. Static creation label sets, property keys
and relationship types receive the same scope checks as staging; creation
read demand also includes carried-element expressions and procedures. Visible
reads stay masked and intermediate UNION rows remain private. All arms share
work, scratch and effect allowances, one statement rollback boundary and one
final publication. Only delivered statement rows spend the public row allowance.

Regression sources in `fgdb-gql/tests/unit_call_text.rs`,
`fgdb/tests/unit_call_write_chain.rs` and
`fgdb/tests/authorized_write_chains.rs` cover arm framing and schemas, fresh
imports, staged reads, typed DISTINCT equality, duplicate outer occurrences,
private row quotas, exact resource refusals and complete rollback after late
failures. Pinned formatting and independent source/API review are source-level
checks only. The full workspace formatting gate reports existing differences in
14 files unchanged by this session. First-party compilation, runtime tests, Clippy, UBS and exact-tree
qualification remain **UNRUN**: both the pinned test build and a lighter
metadata-only check were killed in the unchanged `asupersync` dependency before
first-party compilation. No broader language or transaction bead is closed.

### Procedures over staged write graphs — qualification pending

Source added 2026-11-12 connects standalone CREATE, SET and MERGE and scoped write-chain
procedure phases to the existing transaction procedure executor. Registered
analytics, catalog and hybrid reads on these native paths observe earlier
staged writes, and their result rows can drive later creations. The original
factor choices, shared work allowance and statement rollback owner remain.

Authorized write chains, standalone creation, plain/returning SET and scoped MERGE inputs share a
Prism procedure host over the canonical transaction overlay. It masks vertices,
relations, endpoints and weight properties before selection and graph
construction, retains the native table witnesses, and charges the same live
write permit throughout source admission, projection, kernel work and private
row conversion. The complete-source mutation collector preserves optional seeded
reads and feeds the existing simultaneous proposal reducer; computed-map fields
retain their individual authorization checks. Catalog calls retain their scoped
owner. Authorized staged hybrid procedures now use the masked Beacon adapter
described above; a selected procedure error never retries through a privileged
host. Only final statement rows spend delivery allowance.

Six native and eight authorized regression sources cover literal staged BFS and
weighted shortest-path results, earlier body effects, creation and MERGE from
procedure rows, duplicate occurrences, hidden malformed weights, exact signed
work boundaries, required read rights, and rollback after source or kernel
refusal even under LIMIT 0. Pinned formatting and independent source review
pass. Compilation and runtime qualification remain pending; no broad language,
Prism or transaction bead is closed by this bounded adapter work.

### CLI selection of native durable-run sources — qualification pending

Source added 2026-11-12 exposes the native run-plus-suffix profile through local
`query --buffered`, including its supported external-query sources with
`--spill-dir`. Callers supply all four row, incidence, image-byte and property-byte
limits explicitly. Parsing rejects partial, duplicate, nonrepresentable and
inapplicable profiles before storage opens; the engine's allocation-free
validator owns capacity arithmetic for both CLI preflight and the view setter.
The existing source pool and cumulative work allowance are unchanged.

This configures the ordinary buffered view before its existing executor runs.
Source selection stays lazy: direct streaming `LIMIT 0` does not load an attached
run; external spilling retains its complete-input semantics before a final page.
Selected run/suffix failures keep the native typed refusal and terminal behavior.
The CLI preserves buffered error sources so explicit decoder, suffix and resident
ceilings report the robot `budget` code. Corruption remains an execution failure;
storage I/O retains its separate exit and code.
Analytics and compressed CALL profiles retain their separate admission choices;
the flags do not add an authorized buffered source or a server execution path.
CLI regression sources exercise explicit-profile validation and real
seal/write/query histories, corruption, limits and unchanged read-only storage.
Pinned formatting and independent source review provide source-level evidence
only; compilation, runtime, Clippy, UBS and exact-tree qualification remain UNRUN
under the dependency build limitation described below.

### Native MATCH over durable runs and post-seal histories — qualification pending

Source added 2026-11-12 for `fgdb-teypc`, `fgdb-w3-tier-r-0tj` and
`fgdb-w3-buffer-g2v`. `BufferedReadView::set_native_run_limits` enables a real
native source arm for typed edge MATCH, aggregate input, typed join children
and vertex- or edge-rooted correlated probes. The existing GQL operators still own
predicates, direction, captures, pagination and output. Exact-cut endpoints
come from the same admitted buffered generation.

The source uses a compact runtime EId/version locator over the authenticated
compressed image, with separate outgoing and incoming permutations. It keeps
original CSR property locators, full-width IDs, parallel edges and every
retained historical candidate. It never scans the whole EId directory for
each incident successor. The first identity is admitted from root descriptors
before FGRN loading; the completed locator must agree with it. Independently
demanded typed incidents select their own source without admitting that global
minimum: only an actual endpoint successor emits an incident identity. One
incident relation image and locator may remain charged beside the root source,
reused across outer vertices and edges. Switching relations releases the old
incident source before admitting its replacement. Direct streaming LIMIT 0 and
sources closed before first demand do not load the run.

After a relation changes, the same source combines the unchanged run with only
its authenticated post-horizon Tier-D blocks. The locator indexes original
base and suffix coordinates, collapses exact EId/birth restatements in root
publication order and retains every distinct historical birth. It borrows the
winning source's properties, including empty replacements, without resealing
the base or constructing a visible-only graph. The store-minted suffix receipt
binds root, relation, horizon and publication; it authorizes the composed read
without changing the immutable base's verified horizon.

This explicit profile currently requires plain, non-ECS storage and a floor-zero
run. The run and complete suffix cover the entire pinned frontier even for a
query at an older cut, preserving future and retired candidate identities.
Pruned runs and untyped/alternate root relations retain ordinary sources.
Vertex roots and explicitly selected root positions
keep their ordinary readers; typed incidents can independently select an
eligible run. The standalone incidence-stream API retains its ordinary merge.
No selected-run or suffix error retries another source. The decoder's declared capacity
and byte/population work are admitted before payload I/O, and the source pool
owns the complete decode/image allowance, suffix blocks and locator. The base
population plus raw suffix statements must fit the incidence ceiling before
the first suffix payload is read; owner vectors and individual block decoders
are admitted before allocation. Packed reads
bypass whole-pack payload caches. Returned properties are measured and reserved
before cloning; output records retain their independent charges.

Locator unit regressions, native MATCH integration regressions and the
packed-read regression cover historical candidate counts, literal order
and values, V7/V8 carry, compaction/reopen, directed joins and probes, actual
run reuse after redundant Tier-D damage, vertex-rooted EXISTS/NOT EXISTS,
mixed-relation joins, post-seal property removal and retirement, new IDs before
the base's first ID, raw-suffix admission before damaged-payload reads,
selected-source corruption refusal, quota precedence, cancellation and memory
lifetimes. Failed incident loads never establish
absence or become cached ineligibility. Pinned targeted formatting,
diff hygiene and independent source/API reviews pass. First-party compilation,
runtime tests, Clippy, UBS and exact-tree qualification remain UNRUN: the prior
pinned test and metadata builds were killed in `asupersync` before any fgdb
crate, and the unchanged constrained host supplies no new execution proof.
Whole-run decoding, complete suffix retention, conservative capacity reservation,
opt-in selection and
initial full Tier-D admission remain; no larger-than-memory native Tier-R,
measured speedup, or broader storage-bead closure is claimed.

### Server write input and binding continuity — qualification pending

Source added 2026-11-12 extends original canonical argument admission through
raw, conditional, prepared and Bolt write ingress. Original payload bytes,
including unused map fields, are admitted before lowering, statement-time
capture or binding. Conditional writes retain the exact-frontier comparison
before argument work. Prepared script and chain batches admit every original
record before the first record binds, so an oversized later record cannot
follow an already-observed calendar value or a first-record binding error.

Prepared scripts bind under the server's retained allowance, then hand the
complete program or batch to the existing native bound owner with a token
attenuated by the consumed input and binding work. RETURN, chains and Bolt
use the same remaining-budget handoff. Derived fields are not billed as
another caller input. Raw stats writes pass unchanged originals to the native
session, which retains their actual input debit through publication. Recovery
generation, cancellation, authority, statement limits and atomic completion
remain in their existing owners.

Three added regression sources cover exact UTF-8 input deltas for single
script, chain and RETURN execution and script/chain batches; pre-clock
refusals across raw, conditional and prepared ingress; late-record admission
before first binding; and Bolt pending-owner refusal. Existing prepared
RETURN and Bolt exact-budget laws now include the canonical empty transcript.
Independent source/API review found no remaining publication blocker. Pinned
formatting and final formatting checks pass for all seven changed Rust files.
Compilation, runtime tests, Clippy, UBS and exact-tree proof remain **UNRUN**.

Definition-only script PREPARE now passes original text and arguments to the
namespace compiler. One native permit admits those bytes, positive source
expansion, controlled catalog work and representative binding. Its private
`PreparedNativeWriteScript` metadata owner retains the checked script and a
value-free map recipe together; neither representative operands, a graph pin,
an execution permit nor a statement-time observation survives preparation.
The server cache uses this owner for fresh execution binding and original-source
diagnostics. Two added native laws and one server law check exact UTF-8 work
deltas, whole-map expansion, fresh execution values and time, and source offsets.
Pinned formatting and independent source review pass; compilation and runtime
qualification remain pending. The broader authorization and surface beads
remain open.

### Independent relationship-upsert batches — qualification pending

Source added 2026-11-12 alongside the bulk-write work in `fgdb-d04vu` and
`fgdb-2l3pt`. The existing frozen relationship profile now includes native
edge upserts with ON CREATE, ON MATCH and trailing SET actions. Each row keeps
its ordinary native workspace and ordered action semantics. The program
evaluates independent rows against the entry overlay, then composes their
original intents once in input order instead of preparing each growing
inter-row prefix. Embedded owner and capability-authorized execution share
the same profile and final publication boundary.

Admission still proves a common directed relationship shape, equality-keyed
endpoint scans and pairwise distinct endpoint keys. Duplicate or
numeric-equivalent keys, undirected patterns, unkeyed scans and selections
that read relationships retain the ordinary sequential path. Upsert actions
are limited by the existing native type to the chosen relationship; the
profile does not admit endpoint mutations or arbitrary graph-dependent rows.

Regression sources compare final images, ordered receipts and allocation
requests with ordinary execution, including parallel matches, duplicate
endpoint vertices, both ON branches followed by SET, empty selections and
mixed plain-MERGE/upsert programs. Late arithmetic, field-scope and cumulative
effect-budget refusals restore the prior workspace and publish no prefix.
Invalid repeated allocator identities also reject atomically. Independent
source/API review found no blocker. Pinned formatting and the final formatting
check pass for all six changed Rust files after execution-service recovery.
Compilation, runtime tests, Clippy, UBS and exact-tree proof remain **UNRUN**;
no measured speedup or broader bulk-workstream closure is claimed.

### Original write-parameter admission — qualification pending

Source added 2026-11-12 for the remaining input-admission gap under
`fgdb-4iiho`. Native authorized text, prepared-session, script and explicit
transaction ingress reserve the original text and complete canonical parameter
transcripts before map lowering, statement-time capture or binding. Unused map
members and UTF-8 payload bytes remain part of that price. Batches reserve every
original record before capture; derived fields and previously bound programs
are not treated as another caller input.

The debit stays on the permit that stages and publishes the write. Immutable
ReadWrite and relationship-scope checks still precede direct-script argument
inspection. Explicit transaction exhaustion discards its unpublished prefix.
No parameter-count proxy or fresh per-statement allowance replaces the byte
reservation.

Six added regression sources cover pre-catalog and pre-clock refusal, exact
original-byte deltas, cumulative batch input, UNWIND admission, direct-script
binding order and transaction-prefix rollback. Two existing exact-budget laws
now include the original canonical input term. Independent source/API review
found no blocker. After the execution service recovered, pinned formatting
and the final formatting check passed for all nine changed Rust files.
Compilation, runtime tests, Clippy, UBS and exact-tree proof are **UNRUN**:
the shared host lost memory headroom during dependency acquisition and its
execution transport disconnected before any compiler started. The broader
authorization bead remains open.

### Buffered durable runs and generation-bound analytics — qualification pending

Source added 2026-11-12 for `fgdb-teypc`, `fgdb-w3-tier-r-0tj` and
`fgdb-w3-buffer-g2v`. Buffered compressed analytics now reuse authenticated
relation runs selected by their pinned root. Native V7 and carried V8 images
retain their original provenance, floor and coverage; stale images combine
with only selected Tier-D blocks whose sequence spans extend past the verified
horizon. Missing membership and below-floor cuts retain ordinary sealing.
Damaged attachments and source, memory or cancellation refusals do not reseal.

The complete selected raw-history population is still admitted before run
reads, including the GQL source callback and cache receipt. Root parsing and
decoded suffix inputs retain source-pool reservations. The stale path currently
holds the complete selected suffix during overlay construction, so it can
refuse a pool where family-at-a-time sealing would fit. Initial buffered open
still authenticates Tier-D history, and run decoding and final images retain
their separate sealing bounds; descriptor-only open and streamed run frames
remain unfinished.

Compressed projection digests and analytics certificates now identify the
authenticated selected generation, including a native V7 wrapper, while the
immutable run header continues to identify its original adjacency source.
Regression sources cover live/cold certificate bindings, direct and stale
buffered runs, history, compaction/reopen, empty relations, corruption refusal,
raw-history admission, cancellation and reservation refunds. Pinned targeted
formatting, diff hygiene and independent source/API reviews pass. The locked
targeted test build and a separate metadata-only check both stopped when the
pinned `asupersync` compiler was killed by signal 9, before first-party
compilation or any test assertion. Compilation and runtime qualification are
UNRUN; full workspace checks, Clippy and UBS remain unrun. No complete
exact-tree proof or closure of the broader storage beads is claimed.

### Sealed-run continuity during checkpoint recovery — qualification pending

Source added 2026-11-12 for `fgdb-rc1111-persistent-tier-r-7hsan` and
`fgdb-w2-retention-checkpoints-cn1l`. When Chronicle contains committed work
beyond the selected root slot, suffix recovery now carries that checkpoint's
authenticated relation-run inventory into the healed root. Native V7
references are resolved against the original checkpoint's source and publication
before being carried as V8; existing V8 sources, floors, publications and
verification horizons remain exact. Replay does not certify new mutations as
already included in an old run. Subsequent reads continue through the bounded
post-seal delta overlay, and later writes cannot advance coverage across that
unmerged interval. Explicit authoritative rebuild still reconstructs from
Chronicle without inheriting the selected root's derived run membership.

Regression sources cover native V7, covered V8 and stale V8 checkpoints at two
post-commit publication fault stages, original run bytes and provenance,
literal recovered mutations, bidirectional overlays, historical and pinned
views, a second reopen, later writes and authoritative rebuild. Independent
source/API review found no blocker. Rust compilation, runtime tests, formatting,
Clippy, UBS and exact-tree proof remain **UNRUN** because the execution workspace
is unavailable. The broader recovery and persistent Tier-R beads remain open.

### Persistent sealed runs through compaction — qualification pending

Source added 2026-11-12 for `fgdb-w3-seal-compact-h3p` and
`fgdb-rc1111-persistent-tier-r-7hsan`. Consolidation now carries the selected
root's authenticated relation-run inventory into the replacement root before
the existing manifest and root-slot publication. Each run keeps its original
source root, retained floor, publication and verified-through frontier.
Compaction does not reseal an image or promote an outdated run: readers still
combine its unchanged compressed base with the required post-seal delta suffix.
Pinned generations remain independent, and later writes retain the same run
membership.

The change uses the existing V7/V8 root envelope and publication owner; the
consolidation floor remains zero. Regression sources cover persisted membership,
historical and current results, writable reopen, subsequent writes and damaged
run refusal. Independent source review is recorded with the landing. Rust
compilation, runtime tests, formatting, Clippy, UBS and exact-tree proof are
**UNRUN** because the execution workspace is unavailable. The broader storage
beads remain open.

### Bolt explicit native write transactions — qualification pending

Source added 2026-11-11 under `fgdb-roadmap-bolt-v2-vb6e` and
`fgdb-w2-txn-lifecycle-mhae`. The Bolt adapter now composes the native
`AuthorizedWriteTransaction` owner across BEGIN, RUN and COMMIT.
Write/default BEGIN with signed Write or ReadWrite rights opens a writer;
read-only grants and read mode retain the native read owner described above
(added 2026-11-14).

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
Explicit writer reads use native graph/set, exact aggregates, catalog calls and
supported staged fnx/hybrid hosts. Historical and branch forms outside the
native writer's basis contract refuse and discard the staged prefix. Explicit
read mode uses the retained committed-snapshot owner described above. Concurrent
write-basis advancement retains the native conservative refusal, and full SSI
remains separate.
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

### Implicit-return COUNT and EXISTS unions — qualification pending

Source added 2026-11-12 for `fgdb-og3db` and `fgdb-2ht8c`. COUNT bodies may
omit RETURN in every UNION ALL branch; EXISTS bodies may omit it in every
UNION, UNION DISTINCT or UNION ALL branch. Branches keep independent copies
of the original imports, their local names, filters and pages. A private
literal projection after each completed branch gives different local schemas
one native union shape without dropping duplicate occurrences. The union
remains inside the expression's existing Apply owner.

For example, `COUNT { MATCH (n)-[:R]->(a) UNION ALL MATCH (n)-[:S]->(b) }`
counts both relation bags for the same outer occurrence. COUNT with DISTINCT
UNION still requires explicit RETURN in every branch. Mixing returning and
implicit branches refuses before catalog access; CALL and COLLECT continue
to require RETURN. Return classification scans only the span already consumed
by the ordinary branch parser, so nested RETURN clauses and variables or
properties named `union` cannot change the outer contract.

Four GQL regression functions cover bags, NULL and optional rows, differing
branch schemas, independent correlation, local pages, nested/list-local
bodies, return-contract preflight, rebinding, exact resource ceilings,
interruptions and late failures under LIMIT 0. A native integration regression
covers capability filtering in every branch, pinned snapshots and staged
transaction overlays. These are added regression sources, not passing runtime
evidence. Targeted pinned formatting and `git diff --check` passed, and
independent source/API review found no remaining blocker. Scoped UBS completed
with reviewed diagnostics and no actionable changed-production finding.
Compilation, runtime tests, Clippy and exact-tree qualification remain
**UNRUN**: the earlier execution host disconnected before product compilation,
and the restored host could not admit a compiler under memory pressure.
The broader language beads remain open.

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

This is a local native API. The Bolt adapter and the explicit FGP adapter above
use this owner. Durable ownership leases, reattachment and full SSI remain
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

### Collection parameter types through WITH and aggregate clauses

Source added 2026-11-12 for `fgdb-l5k31`. Prepared set queries and both
aggregate preparation facades infer List and Map contracts from structural
uses of bare parameters and identity aliases. Renames, `WITH *`, later graph
clauses, terminal aggregate arguments, grouping, computed RETURN, HAVING and
ORDER BY participate in the same complete unresolved inference phase. For
example, `WITH $items AS xs RETURN sum(head(xs)) AS total` and
`MATCH (n) RETURN $items AS xs, count(*) AS c ORDER BY size(xs)` derive the
same canonical templates as their explicit List declarations.

The ordinary parser records binding origins and rebuilds the complete
unresolved template when a new collection constraint appears. Only after
constraints stabilize may aggregate finishers resolve graph symbols or
materialize their private generated aliases. Late direct uses such as
`head($items)` participate in the same retry mechanism. Each restart adds a
distinct parameter constraint, bounded by existing parameter and source
limits; compilation work is not cumulatively metered across those restarts.

Explicit declarations, including Text for `size` and canonical Null, retain
precedence. Result aliases shadow input names, and only valid grouping inputs
remain visible after grouping. Computed values and aggregate results do not
transfer parameter identity. List/Map requirements must agree at every alias
or bare use; incompatible contracts and invalid terminal clauses refuse
before catalog access. Multi-origin aliases exported by inner UNION retain
their existing explicit-type requirements.

Five set-query regressions and nine added aggregate regressions cover canonical
equivalence, aliases and graph boundaries, direct and terminal consumers,
rebinding bags/empty lists/NULL, explicit domains, shadowing, empty grouping
and zero catalog callbacks on conflicts. They are in
`crates/fgdb-gql/tests/list_values.rs`,
`crates/fgdb-gql/tests/pipeline_aggregate_text.rs` and
`crates/fgdb-gql/tests/computed_aggregate_text.rs`.

Pinned formatting and whitespace checks passed for the final feature sources.
Independent production and regression reviews found no remaining concrete
API, lifetime, scope or oracle blocker. Scoped UBS completed; its sole critical
diagnostic compared public authorization-error variants in a test and was
reviewed as a false positive, and changed production findings had no actionable
defect. Cargo compilation, runtime tests, Clippy and exact-tree qualification
remain **UNRUN**. The controlled build launcher refused before spawning Cargo
when available host memory fell to approximately 1.70 GiB. The broader bead
remains open.

### Pinned embedded read views

`Database::read_session()` returns an immutable `EmbeddedReadView` owning one decoded generation. A view:

- keeps its frontier and decoded graph stable after later database writes;
- can be cloned while sharing the same immutable generation;
- executes text, plan-only, or owned prepared GQL at its pinned frontier;
- executes older retained sequences at or below that frontier;
- refuses a later sequence through `ReadError::BeyondFrontier`.

This is a real embedded read-view subset. It is not authorization negotiation, lease/reattach, cursor ownership, server transport, or the synchronous facade.

### Signed admission before native read compilation and prepared binding

Source added 2026-11-12 for `fgdb-w10-embedded-54r` and `fgdb-w10-qos-cxp`.
Textual authorized reads reserve the original UTF-8 statement length plus
the original canonical parameter-byte length before field lowering, selector
parsing, binding or catalog callbacks. Prepared invocations now reserve their
original canonical argument bytes on the live native execution permit before
selector binding, map-field derivation or native binding. Unused map payloads
and branch-only arguments remain part of that input. Derived fields and trusted
statement-time values are not charged as a second input.

The retained native owner applies this admission to buffered execution, source
and aggregate cursors, ordered spill execution, Beacon reads and standalone
authorized prepared reads. Prepared batches preflight each invocation's input
cumulatively; differences admit one binding for both endpoints. FGP prepared
delivery forwards original arguments and retains its immutable lowering recipe
in the authorized handle, removing the separate temporary argument permit.
Trusted attachment checks exact recipe/selector source coherence and preserves
the original public map parameter schema. Definition preparation and later
execution retain their existing separate operation permits.

Server PREPARE also admits original source and argument bytes before lowering.
Fresh authority, cancellation and generation checks precede the argument
reservation; a failed reservation retains its typed Budget refusal. Handle
ownership, expiry and retired-authority precedence remain enforced. Regression
sources cover unused UTF-8 payloads, malformed-binding precedence, exact byte
boundaries, batch/difference accounting, recipe coherence, original arguments
across native/server delivery modes, retained ordering allowance and EOF work.

Targeted pinned formatting, whole-tree whitespace checks and independent
source/API and call-site reviews passed. Compilation, runtime tests, Clippy
and exact-tree qualification remain **UNRUN**. Earlier dependency builds were
killed for memory pressure; in this session the restored host's available
memory fell below the controlled launch threshold before Cargo was started.
The broader embedded and QoS beads remain open.

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
