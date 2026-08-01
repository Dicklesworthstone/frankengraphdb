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

*(Written after the ambition rounds; see revision history at the bottom.)*

---

## Revision history

- **2026-07-31, pass 1** — initial measurement and bridge plan (JadeSnow).
