# Reality Check and Bridge Plan

**Measured 2026-07-31.** Revised in place on each pass — do not fork this document.

---

## Phase 1 — Where we REALLY are

### The one-sentence answer

**FrankenGraphDB has an excellent, deeply-verified storage and semantics foundation and
is not yet a database.** Nothing a user could install, open, write to, or query
exists — not the library, not the server, not the CLI, not the query language, not
the transaction manager. The 46% bead-completion figure is real and it is measuring
something other than product progress.

### The numbers, measured not estimated

| Measure | Value | Source |
|---|---|---|
| Beads closed | 274 of 600 (46%) | `br list --status=closed` |
| — of those, registry/catalog/gate/ceremony | **129 of 274 (47%)** | title match on `appendix\|registry\|catalog\|g0-\|census\|pin\|gate\|doc\|adr\|audit\|provenance\|classif` |
| Beads open | 295 (67 of them still ceremony) | `br list --status=open` |
| Crates activated | **19 of 71 (27%)** | `registries/workspace_topology.toml` |
| Workspace tests | 2150 passing, 0 failing, 119 suites | `cargo test --workspace` |
| Benchmark harness | **none exists** | no `benches/` directory anywhere |
| Product binaries | **none** | only `tools/registry-check` has `[[bin]]` |

### What IS working — and it is genuinely good

These are not stubs. Each is law-bound, mutation-tested, and differentially verified.

1. **Chronicle — the commit stream (B1).** Two-fsync protocol with the marker as the
   commit; torn-tail discrimination (missing bytes = crash, wrong bytes = damage);
   §5.1 identity pipeline; RaptorQ erasure coding with per-symbol MACs; a crash-point
   matrix; and a bit-rot campaign proving healing to the repair budget and fail-closed
   past it, on real capsule files.
2. **`fgdb-reference` — the §15 semantics oracle.** Complete against §15's list:
   values, visibility, path modes (all four with discriminating graphs), intents with
   the mismatch trichotomy, temporal selectors, branches with historical forking,
   snapshot isolation with an anomaly oracle, an SSI dangerous-structure checker,
   workspace generations, and terminal attempt semantics.
3. **`fgdb-strata` tier D (B2, first tier only).** Canonical delta-block format,
   content identity, partition roots, cross-block merge under tombstone supersede,
   the tier-D writer, a durable content-addressed store, reopen-from-identity, and
   compaction — with three differentials against the oracle.
4. **`fgdb-sim` — the differential harness.** Durability-vs-semantics, the whole write
   path, concurrency-vs-durability, and Strata-vs-oracle.
5. **The registry/gate apparatus.** Genuinely rigorous, and see the gap below.

### What is NOT working — by vision goal

| # | Goal (source) | Status | Evidence |
|---|---|---|---|
| 1 | B1 One Version Universe — commit stream, MVCC, time-travel, branches | **PARTIAL** | Stream + branches + time-travel exist in the ORACLE. Replication and change subscriptions: no code. |
| 2 | B2 Strata — three temperature tiers | **PARTIAL** | Tier D done. Tier R (sealed CSR runs) and archived anchors: no code. `fgdb-props`, `fgdb-buffer`, `fgdb-scratch` planned. |
| 3 | B3 Loom — Free-Join/WCO execution | **NOT_STARTED** | All 8 `loom` crates planned. No algebra, no planner, no executor. |
| 4 | B4 Ripple — DBSP Z-set incremental engine | **NOT_STARTED** | All 3 `ripple` crates planned. |
| 5 | B5 Determinism as a product feature | **PARTIAL** | Doctrine-4 canonicality is enforced everywhere and lab-runtime tests exist. Plan certificates, decision cards, `replay(certificate, seq, seed)`: no code. |
| 6 | B6 Agent-native — branch-per-agent, macaroons, GraphRAG | **PARTIAL** | Branch isolation exists in the oracle. Macaroon authz, provenance edges, hybrid retrieval: no code (`warden`, `beacon` layers empty). |
| 7 | **Embedded library `fgdb::Database::open`** | **NOT_STARTED** | `fgdb` crate planned. **No user entry point exists.** |
| 8 | **Server `fgdbd` (FGP/HTTP2/gRPC/WS/Bolt)** | **NOT_STARTED** | `fgdb-server`, `fgdbd`, `fgdb-protocol`, `fgdb-bolt` all planned. |
| 9 | **CLI `fgdb` with robot mode** | **NOT_STARTED** | `fgdb-cli` planned. No binary. |
| 10 | **GQL (ISO 39075) + openCypher surface** | **NOT_STARTED** | `fgdb-gql`, `fgdb-cypher` planned. No parser, no grammar. |
| 11 | §17 performance gates (≥8M point-reads/s, p99 <15µs) | **UNPROVEN** | **No benchmark harness exists at all.** Not one number has been measured. |
| 12 | Larger-than-memory as a property of every operator | **NOT_STARTED** | No operators exist. |
| 13 | Lab VFS before the first fsync (§15, W1) | **VIOLATED** | `fgdb-1xtp`: the first fsync shipped long ago; chronicle uses blocking `std::fs`. One of four fault classes (bit rot) closed; fsync lies, interior tears, ENOSPC still uninjectable. |
| 14 | Verification ladder | **PARTIAL, STRONGEST AREA** | Oracle, differentials, crash matrix, erasure campaigns all real. TCK, Neo4j/Memgraph differential, DPOR exploration, formal lanes: no code. |

### Would completing all open+in-progress beads close the gap?

**Yes on paper, no in practice, and the distinction is the finding.** Bead coverage
is not the problem: every headline goal has beads (GQL 2, embedded 2, fgdbd 2, CLI 1,
Loom 2, Ripple 4, Prism 4, vector 5, benchmark 1). There is no significant `NO_BEAD`
gap. The problem is **throughput allocation**:

- 47% of everything closed so far is registry/catalog/gate work.
- 67 of 295 open beads are more of the same.
- The G0 "ready" queue is almost entirely catalog beads, so an agent that picks up
  `br ready` work is overwhelmingly steered into ceremony.

The swarm is not drifting because it lacks direction. It is drifting because **the
work that is easiest to pick up is the work that does not build the product.**

### What is actually blocking us

1. **No spine.** There is no `fgdb` crate, so there is nowhere for a user-facing API
   to live and nothing to integrate the pieces into. Chronicle, Strata and the oracle
   are three islands that only meet inside test files in `fgdb-sim`.
2. **No query path.** Between "Strata answers `neighbours(v, rel, as_of)`" and "a user
   runs a GQL query" there is: a parser, a binder, an algebra, a planner, an executor.
   All eight `loom` crates are planned; none started.
3. **No transaction manager.** `fgdb-txn` is planned. Today's transaction semantics
   live entirely in `fgdb-reference`, which is explicitly *never shipped*.
4. **Performance is entirely unmeasured.** §17 sets hard numeric gates and there is no
   harness. Every performance claim in the README is currently unfalsifiable.
5. **The lab VFS ordering violation (`fgdb-1xtp`)** blocks honest crash coverage for
   three of four fault classes and gets more expensive with every fsync added.
6. **Ceremony gravity**, as measured above.

---

## Phase 2 — The bridge plan

The ordering principle: **build the thinnest possible vertical slice that a human can
run, then thicken it.** Every horizontal layer completed before there is a vertical
path is a layer whose integration risk is unmeasured.

### Track A — The spine (unblocks everything, nothing else unblocks it)

- **A1. Activate `fgdb`** — the embedded library crate. `Database::open(path)`,
  `Database::open_in_memory()`, a session handle, and a `close`. Internally: open a
  Chronicle coordinator, recover, and expose a read handle over Strata. This is the
  first place the three islands meet in *production* code rather than in a test.
- **A2. Root manifest** — currently blocked and blocking. `RootSlot.root_manifest_oid`
  points at an object nobody has defined, so a database cannot find its partitions on
  open. Needs an Appendix A catalog row; the Strata side already guarantees a
  partition reopens from a 32-byte identity.
- **A3. Write path in the library** — `Database::write(|txn| ...)` producing a real
  commit, using the effect vocabulary that already exists.
- **A4. Read path in the library** — adjacency and vertex reads at a snapshot, served
  from Strata, falling back to stream replay only on a cold partition.

### Track B — The narrowest real query surface

- **B1. `fgdb-gql` lexer + parser for a deliberately tiny subset**: `MATCH (a)-[:R]->(b) RETURN b`.
  Nothing else. Grammar fuzzed from day one per §15.
- **B2. `fgdb-algebra`** — the operator vocabulary for that subset only: scan, expand,
  project.
- **B3. `fgdb-exec`** — a single-threaded interpreter over Strata. Explicitly a subset
  of Loom, never a substitute: no factorization, no WCO, no vectorization, and the
  module doc must say so.
- **B4. Differential**: every query result must equal `fgdb-reference`'s answer for the
  same graph. This is the instrument that makes the whole surface honest.

### Track C — Measurement (currently zero)

- **C1. A benchmark harness** — `fgdb-bench`, activated, with the §17 metrics named as
  the things it measures even when the numbers are bad.
- **C2. Publish the first honest numbers**, however unflattering. A measured 400k
  point-reads/s against a target of 8M is *progress*; an unmeasured claim of 8M is a
  liability, and doctrine 7 forbids reporting a non-durable benchmark mode as a result.

### Track D — Close the fault-injection violation

- **D1.** `fgdb-1xtp` step 1: make Chronicle's durable path async against asupersync's
  `Vfs`/`VfsFile`, behaviour unchanged, `UnixVfs` underneath.
- **D2.** The faulting VFS: fsync lies, interior torn writes, ENOSPC, latency.
- **D3.** Re-express the crash matrix against it, removing nothing until the new tests
  are mutation-proven at least as strong.

### Track E — Steering the swarm away from ceremony gravity

- **E1.** Stop treating G0 catalog completeness as a prerequisite for engine work. It
  is a gate on *shipping*, not on *building*.
- **E2.** Every new engine bead must name the differential that proves it.

---

## Phase 4 — Ambition pass

Three escalation rounds against Phase 2. What survived is below; Phase 2 stands as
the *ordering*, and this is what makes it worth doing.

### Round 1 — the oracle is a GENERATOR, not just a checker

Phase 2 treats `fgdb-reference` as something to compare against. That is thinking far
too small. **We have a complete, executable specification of the logical semantics
before the engine exists.** Almost no database has ever had that, and every one of
them pays for it forever in "which of these two is right" arguments. The leverage:

- **Model-based history generation.** Generate random *valid* graph histories
  (intents, not effects — so the generator cannot produce a state the system could not
  reach) plus random queries. Run both engines. Compare. This scales verification
  without a human writing each case, and it is the only way the combinatorics of
  MVCC × branches × path modes × temporal selectors ever get covered.
- **Shrinking.** A failing generated history must shrink to a minimal one. Without
  shrinking a 400-step counterexample is a curiosity; with it, it is a bug report.
- **Under DPOR.** asupersync ships deterministic partial-order reduction. Generated
  histories run under explored schedules make concurrency bugs *seeds*, not folklore.
- **The oracle also generates EXPECTED PLANS.** A query the oracle answers by brute
  force gives the executor a result to match AND a cardinality to compare its estimate
  against — a free planner-accuracy signal from day one.

This turns verification from a cost centre into the thing that lets the engine be
written fast and aggressively, because a wrong optimization is caught in seconds by a
generator rather than in months by a user.

### Round 2 — the vertical slice is the wrong shape without these three

**(a) Branch creation must be O(1) from the first line of engine code.** plan:451
requires branch creation to add "only metadata and key wraps", with reads following
branch-parent links "atop structurally shared objects". The oracle copies, is O(n),
and says so at the definition. If the engine's first storage structures are not
*persistent* in the Driscoll–Sarnak–Sleator–Tarjan sense — path-copying or fat-node,
with confluent persistence for merge — then B1's git-style branching and B6's
branch-per-agent isolation are both economically dead, and retrofitting persistence
into a mutable structure is a rewrite, not an optimization. **This is the single
highest-cost mistake available to us right now**, because tier D is young enough to
absorb the decision and tier R is not yet written.

**(b) Tier migration is a competitive-analysis problem, and the plan already says so
without naming it.** "Expected benefit must exceed conversion cost plus uncertainty",
plus minimum dwell time, plus a pinned deterministic fallback — that is *ski-rental*
with hysteresis. Naming it buys the actual result: a 2-competitive deterministic
policy (convert once accumulated read penalty equals conversion cost) and a
randomized e/(e−1)≈1.58-competitive one. Since B5 forbids nondeterminism without a
declared certificate, the deterministic 2-competitive rule is the pinned fallback and
the learned estimator is the advisory feature on the decision card. **The decision card
schema should carry the competitive ratio it is claiming**, so a policy change is a
measurable regression rather than a vibe.

**(c) Measurement must be adversarial, not confirmatory.** A benchmark that measures
what we built measures nothing. §17's gates (≥8M point-reads/s, p99 <15µs warm) need a
harness that also generates *hostile* shapes: degree skew (power-law, so the
supernode path is exercised), adversarial branch depth, worst-case version-chain
length, and cold-partition reopen. Publish the bad numbers.

### Round 3 — the mathematics that actually buys us something

Not decoration; each of these maps to a named plan requirement that is currently
hand-waved.

| Technique | Where it lands | What it buys |
|---|---|---|
| **Generic Join / AGM bound** (Ngo–Porat–Ré–Rudra 2012) | B3 Loom's WCO operator | Worst-case optimality is a *theorem* about the join, not a hope. Cyclic queries (triangles) go from O(n²) to O(n^1.5). This is the difference between "we have a join" and "we beat Neo4j on the queries that matter". |
| **Factorised representations** (Olteanu–Závodný) | Loom's intermediates | Results represented as a factorised d-tree are exponentially smaller than flat tuples; the plan's "factorized intermediates" is exactly this and it needs the representation, not just the word. |
| **DBSP** (Budiu et al. 2022) | B4 Ripple | Z-sets with a differentiation/integration calculus make recursive queries, views, subscriptions and analytics **one** engine. The commit stream is already a Z-set stream — this is the least-cost bet on the board and it is entirely unstarted. |
| **Elias–Fano / quasi-succinct** (Vigna 2013) | Tier R sealed runs (`fgdb-w3-tier-r-0tj` literally says "EF offsets") | Monotone offset sequences at ~2 bits/element above the information-theoretic bound with O(1) random access. Makes larger-than-memory adjacency scans real rather than aspirational. |
| **Ribbon filters** (Dillinger–Walzer 2021) | Block skipping in the partition root | Strictly dominates Bloom at the same false-positive rate with ~30% less space and better locality. The root already carries ranges; a ribbon filter per block turns "skip by sequence" into "skip by key". |
| **PGM-index / piecewise-linear** (Ferragina–Vinciguerra 2020) | Ordinal maps (`fgdb-w3-vertex-directory-nde`) | A learned index with *worst-case guarantees* — unlike naive learned indexes — so dense-ordinal lookup keeps Kùzu-class performance without ordinals becoming identity. |
| **HyperLogLog / Theta sketches** | Planner cardinality estimation — `fgdb-sketch` is already an ACTIVE crate | Mergeable cardinality estimates across partitions with bounded error, which is exactly what a cost model needs and what the sketch crate exists for. It is active and unused by any planner because no planner exists. |
| **Count–Min / CountSketch** | Degree-skew detection for the supernode path | Tells the executor when it is about to expand a hub, which is when the WCO path and the factorised representation earn their keep. |
| **Persistent / confluently-persistent structures** (DSST 1989) | Tier D and tier R, urgently | See Round 2(a). O(1) branch fork with structural sharing, and confluence for merge. |
| **Ski-rental / competitive analysis** | Decision cards for tier migration | See Round 2(b). |
| **Fekete et al. 2005 dangerous structures** | Already implemented in `fgdb-reference::ssi` | Cited because it is the proof that this approach works: the SSI oracle is a theorem-backed checker, not a heuristic, and it is already load-bearing. |

**The synthesis.** Generic Join + factorised intermediates + Elias–Fano runs is not
three optimizations; it is one coherent claim — *the storage layout is already the
trie the WCO join wants to walk*, which is precisely what §2 means by "running over
Strata runs that are already tries". Building tier R without Elias–Fano and then
adding Generic Join later means the join walks a structure that was not designed for
it, and the headline B3 bet quietly becomes a normal hash join with a fancy name.

---

## Phase 3a — What this pass actually filed

Every finding above is now a bead with self-contained notes, so this document is a
record rather than a dependency. Nobody should have to read it to do the work.

| Bead | P | What it is |
|---|---|---|
| `fgdb-j0vu` | **P1** | **THE SPINE** — a minimal end-to-end slice a human can run, long before W10. |
| `fgdb-ge6a` | **P1** (bug) | `RootSlot.root_manifest_oid` dangles — no object kind defines what it resolves to, so **no database can currently be reopened**. |
| `fgdb-lc1t` | **P0** | Persistent structures decision, recorded *before* tier R is written. Added as a **blocker on `fgdb-w3-tier-r-0tj`**. |
| `fgdb-p95p` | P1 | Adversarial benchmark harness. Activates the planned `fgdb-bench` crate. |
| `fgdb-z5y0` | P1 | Model-based history generator with shrinking, driving both engines. |
| `fgdb-yago` | P2 | Ski-rental decision-card policy with a declared competitive ratio. Blocked on `fgdb-w3-write-amp-bnn` for its cost constants. |

Two further defects were found *by* the refinement rather than planned into it, both
by re-deriving a claim instead of accepting it:

| Bead | P | What it is |
|---|---|---|
| `fgdb-s50d` | **P1** (bug) | The **oracle admits identity recycling** — create-after-delete of the same `VId`/`EId` is accepted, though plan:221 says spent slots "remain spent forever". Found by testing `fgdb-0trr`'s *fix sketch*, whose premise it falsifies; now a blocker on that bead and on `fgdb-z5y0`. |
| `fgdb-teqw` | **P1** (bug) | `scripts/check.sh` is **red for every pane**: `9e11f4a` removed 5 panic-class findings without updating `UBS_CRITICAL_BASELINE` in the same commit. Filed, not fixed — it is another pane's lane and the pin lives in a file this pass was already editing. |

These form one critical path, and it starts at a bead that is **ready right now**:

```
fgdb-lc1t (P0, ready) ──▶ fgdb-ge6a ──▶ fgdb-j0vu ──┬─▶ fgdb-p95p
  persistence decision    root manifest    the spine └─▶ fgdb-w10-embedded-54r
```

Annotations were also added to `fgdb-w3-tier-r-0tj`, `fgdb-rz12` and
`fgdb-w5-planner-tvi` so the mathematics lands where the work is picked up, not only
here.

## Phase 5 — Refinement: the finding that changes the ordering

The refinement rounds turned up one thing that outranks everything in Phase 2, and it
was invisible until the beads were read as a *schedule* rather than a list.

**The spine exists as exactly one bead — `fgdb-w10-embedded-54r`, the full
`fgdb::Database` surface with the explicit ownership contract — at P2, in workstream
10.** That bead is correctly scoped and correctly late. The problem is that it is the
*only* one, so the vertical slice is scheduled after every horizontal layer.

That is the shape of a project that reaches "46% complete" while remaining 0% usable,
and it defers all integration risk to the most expensive possible moment. Chronicle,
Strata and the oracle are three islands today; the only place they meet is inside test
files in `fgdb-sim`.

`fgdb-j0vu` is the correction: `open → write → neighbours → drop → reopen`, and
nothing else. Thin in **surface**, not in mechanism — it uses the real two-fsync
commit path and real Strata blocks, because doctrine 7 permits a subset of a final
abstraction and prohibits a substitute for it. When `fgdb-w10-embedded-54r` lands, the
slice is absorbed into it rather than left beside it.

### The blocker underneath it: nothing can be reopened

Scoping the spine surfaced a defect rather than a task. `RootSlot` is an **active**
row and carries `root_manifest_oid` — and no object kind defines what that OID
resolves to. Recovery can select a credible root slot, authenticate it, read the OID,
and then has nothing to resolve it against. **No database can be reopened today.**
Everything that currently looks like a reopen is a test holding a `PartitionRoot`
identity in memory across the close. Filed as `fgdb-ge6a`.

It is small: Strata already guarantees a partition reopens from a 32-byte identity, so
the manifest needs only the `(graph, partition)` coordinate and the root's object id.
It is a durable format, so it owes an Appendix A catalog row — and that is worth
naming, because it puts G0 catalog work on the critical path of the first runnable
engine. **That is the one place where "catalog blocks engine" is true.** It should not
become a precedent for the general case.

A second refinement caught an edge I had just drawn too coarsely: the spine was
initially blocked on all of `fgdb-w3-tier-d-ctj`, a large bead with several in-flight
children. The spine needs *one object* out of it, not the rest. Splitting `fgdb-ge6a`
out and re-pointing the edge is the same throughput-allocation correction this
document is about — applied to my own dependency graph.

### The steering item that is the owner's call, not mine

**47% of all closed work (129 of 274) is registry, catalog, gate or ceremony.** The
queue is worse than the history. Measured at `9e11f4a`, *before* this pass's
dependencies were wired: of the **23 beads `br ready` surfaced, 16 were catalog, gate,
fixture or owner-ruling work and 7 were engine work** — and three of those seven had
just been filed by this reality-check pass. (The count moves as edges are added; the
ratio is the point, not the integer.)

This is not waste. The catalog is a real gate on shipping a durable format, and the
G0 work is high quality. But `br ready` surfaces those beads preferentially *because
they are unblocked by construction* — catalog work depends on nothing, and engine work
depends on other engine work. So **the work that is easiest to pick up is the work
that does not build the product**, and an agent doing exactly what the queue tells it
is doing the wrong thing through no fault of its own. The swarm is not drifting for
lack of direction; it is drifting because the queue points there.

Re-prioritising 67 beads is an ownership decision. The recommendation is Track E:
**stop treating G0 catalog completeness as a prerequisite for engine work.** It is a
gate on shipping, not a gate on building.

---

## Revision history

- **2026-07-31, pass 1** — initial measurement and bridge plan (JadeSnow).
- **2026-07-31, pass 2** — ambition rounds 1–3 revised in place; Phase 3a and Phase 5
  added. Phase 5 found the spine-scheduling gap (`fgdb-j0vu`), the dangling root
  manifest (`fgdb-ge6a`), the oracle's identity-recycling hole (`fgdb-s50d`) and a
  repo-wide red ratchet (`fgdb-teqw`). Landing this document also required registering
  it in `claims_lint.toml` and `check.sh` — pass 1 had left `registry-check all` red
  repo-wide with `unclaimed_prose`, which is a small lesson in its own right about
  what "the document is written" does and does not mean.
