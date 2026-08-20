# COMPREHENSIVE PLAN REVIEW BY KIMI K3

**Subject:** `COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md` (1.52 MB, 3,065 lines, read in full including Appendices A–F)
**Nature of this review:** adversarial engineering audit. Every finding below was checked twice against the plan text — once on first read, once on a fresh-eyes re-read with deliberate attempts to falsify it. Scope and ambition are treated as **fixed and correct**: nothing here argues for doing less. Several findings argue for doing *more*.
**Verdict in one paragraph:** the plan's conceptual core (B1–B6) is sound and, in places, genuinely ahead of anything shipping. Its failure modes are not conceptual. They are: (i) two places where the plan's own protocol weight contradicts its own published performance gates — fixable, and fixing them makes the system *stronger*, not smaller; (ii) an external-authority lattice whose availability arithmetic is never written down; (iii) a set of omissions that are all *additive* — missing lifecycle protocols, missing surfaces, missing gates; and (iv) a market paragraph that needs a 2026 factual refresh. Each finding states what the plan says, why it is wrong or incomplete, and the revision I propose.

---

## PART 1 — FINDINGS THAT CHANGE THE PLAN

---

### F-1. The lightweight read path does not exist, and §17's point-read gates are arithmetically inconsistent with the plan's own lifecycle

**Severity: highest. This is an internal contradiction inside the plan — the gates and the protocol cannot both be true.**

**What the plan gates (§17):** "Point reads (vertex by key, 1-hop existence) — ≥ 8M lookups/s across cores; p99 < 15 µs warm."

**What the plan requires for one autocommit point read (§5.2, §13, Appendix A):**

1. `LocalAttemptRegistrationSpec::Autocommit` — a semantic, Raft-logged `ControlCommand` ("No statement/storage observation may precede visible registration"; §5.2: "Autocommit uses `LocalAttemptRegistrationSpec::Autocommit` and the same lifecycle without emitting BEGIN frames").
2. `LocalReadCloseSpec` — a second semantic command that "advances LogicalCommandSeq/HLC but not CommitSeq."
3. Every semantic command is ordered through a fresh `LocalOrderAttemptInput` plus a `LocalOrderAttemptAvailabilityCertificate` built under the universal certificate protocol (plan → signer locks → shares), D1-certified as a repeatable supplement (§5.2 step 6).
4. The full result-delivery machine for *every* published result: `AtomicProtocolDetach` → `ResultIndependentRetentionRecord` (PendingVisibility / ReleasedAwaitingActivation / ActiveDeliverable), `ResultDeliveryPolicy`, `ResultResourceEscrow`, a delivery lease with a fresh `ResultDeliveryLeaseTimeBasis` requiring `TimeValidationEvidence::Usable`, a resume-capability lineage, and ACK/release transitions (Appendix A result-owner contracts; §13: "Execution returns only an audit-visible `PublishedResultStream`"; "Published results use an explicit capability-owned transfer").

So one point read costs **two sequenced semantic commands** (each with its own order-attempt supplement and availability certificate) **plus a durable result-delivery lifecycle**. Apply is sequential by construction — every entry is canonical encoding + BLAKE3 hashing + persistent-map update + digest verification on the single state-machine path.

**The arithmetic.** 8M reads/s × 2 commands = 16M sequential applies/s ≈ 62 ns per apply. Nothing in this design — canonical encoding, content-addressed object construction, certificate plan/lock/share bookkeeping, persistent-map installation — runs at 62 ns. At a more realistic 0.5–1 µs per applied entry, the sequencer alone saturates 8–16 cores on the 32-core reference machine before a single lookup executes; and the delivery machine adds per-result durable objects, capability records, and time-evidence validation on top. The p99 < 15 µs latency gate additionally collides with at least one RootSlot publication (an fsync) on the result-release path for every read.

Group commit amortizes *fsyncs*, not *apply*, and does not merge per-command order-attempt supplements — each command names exactly one (`§5.2`, Appendix A `LocalOrderAttemptInput`/`CommitCommand` contract).

**I tried to falsify this finding and could not:** I checked whether point reads could ride explicit multi-statement transactions (amortize one registration across N statements — no: statements are "serialized in v1" and each statement still registers + publishes), whether autocommit skips statement records (it does — classification rides the attempt registration — which is why I count 2 commands, not 3–4), and whether any surface bypasses the lifecycle (none — §13 embedded returns only `PublishedResultStream`).

**Revision F-1-a — add a `SnapshotQuery` lightweight read class (additive; does not weaken any existing machine).** A single-envelope, permit-validated, snapshot-pinned, ephemeral, session-owned result stream: no begin reservation, no attempt registration, no statement records, no durable result retention, no delivery lease; dropped on disconnect — the industry norm for small reads. It reads through the same `AuthorizedSnapshot`/`fgdb-secure-view` boundary, it is audit-`NotRequired`, it is explicitly non-resumable, and §17's point-read and 1-hop gates are re-scoped to measure it. The heavyweight machine remains — untouched and mandatory — for explicit transactions, subscriptions, exports, replay, DP results, and large/long-lived results. This is a *second honest result class* with its own registry rows and `AnswerContract` behavior, not a shortcut: FG-INV-20 (authorization precedes observation) and FG-INV-04 (visible-endpoint reads) bind it exactly as hard as the durable path.

**Revision F-1-b — fuse the autocommit write lifecycle.** Register one composite sequenced command for autocommit single-statement writes (reservation + registration + publication + completion semantics in one arm, identical canonical effects). The command-contract registry bijection mechanism supports adding the arm; the current 4–6 separate coordinator round-trips per micro-write is orchestration tax, not semantics.

**Revision F-1-c — re-derive §17 from a latency anatomy (see F-2-c) and state which operation class each gate measures.**

---

### F-2. The durability-barrier anatomy is unwritten; the no-mutable-file doctrine adds a barrier most systems don't pay

**What the plan says:** two durability barriers D1/D2 "not literally two syscalls… Group commit amortizes both" (§5.2); "Raft persistence obeys the no-extra-mutable-file doctrine… A node syncs the new closure before vote/append/receipt/durability replies" (§14.1).

**Counted sequentially on the smallest audit-`Required` write path:**

1. D1 prepared-root RootSlot publication (§5.2 step 5: publish `RootManifest` + `RootSlot` *before* D1 completes or any receipt);
2. Raft append/hard-state closure sync (§14.1 — hard state lives in immutable content-addressed `RaftStateRoot` objects, so every append/vote creates and syncs a new closure);
3. D2 apply publication;
4. Audit-visibility advance publication (§12.8: "RootManifest publishes the new protocol root and visible endpoint in one D2 slot update");
5. Result activation publication (independent Protocol maintenance).

Group commit amortizes *throughput*; it does not amortize the **sequential barrier chain within one transaction's latency**. At healthy-NVMe fsync latencies that is a ~100 µs floor for a small audited commit; at cloud-block latencies, ~0.5 ms — before the external audit-resolution RTT inside barrier 4. The plan never writes this anatomy down, and its commit-latency gate (§17 transactional ingest) covers "final rebase/constraint work, payload D1, root D2, configured encryption/FEC" but **not** visibility/activation latency.

Barrier 2 deserves specific attention: every production Raft keeps `HardState` in one small mutable structure because term/vote/commit-index change per append; republishing it as immutable objects is the one place the no-mutable-file doctrine costs a full extra durability barrier per log entry.

**Revisions:**

- **F-2-a — write the latency anatomy as a normative appendix:** for each of ~8 operation classes (point read, autocommit write, explicit write txn, audited write, subscription delta, branch create, merge, restore open), every protocol step with its µs budget, barrier count, and external-authority interactions. §17's numbers must be *derivable* from this table. (This is not a scope question; it is making the plan's own performance laws enforceable against itself.)
- **F-2-b — evaluate a second registered mutable bootstrap frame class for Raft hard state.** `bootstrap_frames.toml` already exists for exactly "fixed-location mutable frames"; RootSlot is the only inhabitant. A `RaftHardFrame` inhabitant removes barrier 2. If the doctrine keeps object-recreation hard state, gate it instead: "Raft append-path sync count per entry" becomes a §17 line.
- **F-2-c — first-class audit-`Required` gates:** commit-to-`TerminalReady` and commit-to-streamable p50/p99, with and without the external audit RTT, reported beside the no-audit baseline. The governance profile is the flagship demo path; it deserves flagship numbers, and the current gates quietly exclude the visibility stage.

---

### F-3. The control/consensus stream is unsized — subscription fan-out, result delivery, and time evidence all pay consensus + time-authority costs per event

**What the plan says:** subscription transitions are Protocol-plane maintenance transitions, and every maintenance transition "consumes one same-group Raft maintenance-log position" (§9.5, Appendix A `MetaMaintenanceCommand`); per cursor-delta there are `PublishDeltaPending`, audit release, activation (fresh lease + fresh `SubscriptionDeliveryLeaseTimeBasis` + `TimeValidationEvidence`), and ACK-complete. The 10k-branch objective is stated; no subscription/cursor scale target is stated anywhere, yet "subscription end-to-end p99 < 10 ms" is gated.

**The problem.** Per cursor *delta*: ~3–4 maintenance-logged transitions plus a time-evidence requirement per activation. At a plausible agent-memory scale (10k live cursors × 1 delta/s): **30–40k consensus-logged maintenance entries/s and ~10k time-evidence validations/s**, all sharing the Raft plane with every commit, checkpoint, GC epoch, audit advance, format transition, and result activation. Nothing in the plan sizes this stream, and the maintenance plane is the same plane that carries the audit-visibility prefix whose stall blocks all visibility (see F-5-b). The same applies to result ACK/release/resume transitions for ordinary queries.

**Revisions:**

- **F-3-a — a normative capacity model for the command/maintenance stream:** entries/s by class (semantic, maintenance, audit), the share consumed by control-plane traffic at stated scales (branches, cursors, subscriptions, result deliveries, certificate attempts), and the resulting head-of-line latency for user commits. The plan has gates for edges and none for the sequencer it all flows through.
- **F-3-b — amortize time evidence across deliveries.** One authority observation should cover a cursor's delivery *class* within its signed interval (the guard-budget machinery already exists), or delivery windows should derive arithmetically from cursor-creation epoch + profile duration with one observation per window. A fresh challenged observation per cursor-delta is a network RTT on the fan-out hot path.
- **F-3-c — coalesce per-cursor transitions.** Baseline/reset events genuinely need consensus; per-delta lease/ACK bookkeeping can batch N cursors per maintenance entry with per-cursor sub-records (the Protocol-plane indexes already centralize this state; the coalescing law is mechanical). State a cursor-scale gate ("X live cursors at Y deltas/s within Z ms p99") and derive it from F-3-a.

---

### F-4. W12 certification is serialized where it could safely pipeline

**What the plan says (§14.6):** "V1 forbids speculative semantic multi-command certification: every commit or read-close certificate is built against the fully applied immediately prior global prefix… Group commit may batch fsync/network transport of already sequentially certified entries, but **two candidates cannot share a basis** or omit cross-candidate edges."

**Why this leaves throughput on the floor.** The certifier replays `may_affect` in both directions against every overlapping prepared/committed/closed transaction and re-runs SCC. Serial certification against the fully applied prior prefix means certification of transaction *k+1* cannot begin until *k* is applied — certify and apply never overlap. Global write throughput is capped at 1/(certify+apply) regardless of shard count, on a post-1.0 workstream whose own risk table promises to "publish the ceiling honestly."

**The safe relaxation (standard, Calvin-style):** certify a *batch* of candidates against the same applied prefix, provided (a) every intra-batch pair's edges are included in the acyclicity check in both directions, and (b) the batch applies in a canonical deterministic order. The checked dependency graph and the execution order coincide, so serializability is preserved exactly; nothing else in the protocol changes. Certification of batch *n+1* then overlaps application of batch *n* — a pipelining gain the current text forbids outright rather than gates by measurement.

**Revision F-4 — replace the blanket prohibition with a batch-certification law:** same-basis certification is legal iff the complete intra-batch edge set is checked and apply order is the canonical batch order; *speculative* certification (basis newer than applied, incomplete edges) remains forbidden. Publish ceilings both ways, as the plan's own risk row requires.

---

### F-5. The external-authority lattice needs *engineering*, not just specification

The plan's full-strength posture can require, on or near the write path: **cluster-incarnation continuity, time authority, audit continuity, privacy continuity, archive/grant authority, reservation authority, catalog authority, restore journal authority, transparency witnesses, KMS/HSM** — ten distinct threshold-signed/CAS-linear services, each with its own availability SLO. Three concrete gaps:

**F-5-a. The DirectoryBound allocation-epoch fence is a genuine hole in the document.** §4.5 states, unconditionally: "Before an epoch may issue an ID, a threshold-signed/witnessed `IdentityContinuityRecord`… is durably CAS-published **outside the rollbackable database closure** (operator HSM/KMS registry or configured witnesses)." Appendix A repeats it: "its external registry/witness copy is the anti-rollback authority." Meanwhile §5.1's `DirectoryBound` profile forbids same-identity restore to another directory entirely (clone-only) and the writer-takeover protocol validates original directory/inode continuity — which means a DirectoryBound database is *already* rollback-fenced at the inode layer: a restored copy has a different inode identity and fails takeover. The plan never says whether (i) embedded DirectoryBound deployments still need an external witness service to mint vertex IDs — a deployment surprise that contradicts the "usable like DuckDB" promise — or (ii) the DirectoryBound inode/manifest chain doubles as the allocation anti-rollback fence, in which case the carve-out is missing from §4.5/Appendix A. **Revision:** state it explicitly. Recommended: register a `DirectoryBound` allocation-fence mode deriving epoch anti-rollback from the same inode continuity (same-identity restore is already impossible there), with `ExternalCas` mandatory only for W12, and document the minimal external-authority footprint per posture in one table.

**F-5-b. The audit pipeline's head-of-line property needs a domain answer, not just backpressure.** One stalled `Required` resolution blocks *all later visibility* ("NotRequired/AuditSystem-ready entries cannot leapfrog an earlier Required entry"), and an applied-hidden security candidate stops all new user work behind it (§12.8 security barrier). The plan calls backpressure "the honest overload behavior" — true, but the consequence is that a single global pipeline makes one tenant's audit stall every tenant's visibility, and makes write availability the product of every critical-path authority's availability. **Revisions:** (i) state the availability theorem plainly — *database write availability = product of critical-path authorities' availabilities* — with per-posture numbers; (ii) per-tenant/per-domain audit pipelines (the pipeline is keyed by `role_and_group`; adding a domain axis is an additive registry change); (iii) HA/durability requirements for the authorities themselves, since they are now inside the TCB and the availability set.

**F-5-c. Time evidence granularity on the delivery path** — covered in F-3-b; flagged here because it is an authority-interaction problem as much as a throughput one.

---

### F-6. EId width: a durable-format decision made without a cost model

**What the plan says (§4.5):** every edge receives a 128-bit `{ allocation_epoch: u64, partition: 20 bits, slot: 44 bits }` identity, never recycled — "the unconditional parallel-edge discriminator."

**The cost.** EId is stored *per adjacency entry*: forward family, the default-on reverse materialization, and Tier D delta blocks store stable `(dst_VId, EId)` pairs (§6.2). On a 1B-edge graph with both directions materialized, 16B vs 8B of EId is ~16 GB before properties — paid exactly in the hot transactional tier where the plan invites Sortledton comparisons, and invisible to the dense `OrdinalMapId` projections that rescue only sealed runs. Most workloads never address an edge by identity; `(src, type, dst, user_key?)` is already the indexed logical tuple. A 64-bit epoch+partition-slot EId preserves every stated law (never recycled, epoch-fenced, stable across merge/compaction/time travel) at half the bytes, under the plan's own law 4 ("memory is a first-class metric").

**Revision F-6 — re-open the decision with a costed comparison:** 64-bit epoch+slot vs 128-bit, priced against the §17 bytes-per-live-edge metric and the Tier D write-amplification model. If 128 survives, it survives with numbers attached and a named workload that needs it (e.g., edge-centric time-travel at extreme scale) — not by symmetry with VId.

---

### F-7. Byte-economy, round two: per-object identity overhead, object churn, group packing, tiered FEC

Four identity layers (`ObjectId`, `CiphertextId`, `EncodingId`, `PlacementId`) plus descriptors and per-symbol headers/MACs ≈ 200+ bytes of metadata per durable object before payload; the FEC repair budget adds ~20% and per-symbol MACs ~5% on 4 KiB objects. For multi-MB runs this is noise; but the control plane mints small objects continuously (reservations, bindings, specs, records, capabilities, pipeline entries) and every commit produces roughly a dozen objects (capsule, effect set, template, batch, marker, outcome records, pipeline entry, …). At high commit rates the Chronicle's object count and metadata ratio inflate faster than the graph data — a cost invisible to every current §17 gate.

**Revisions:**

- **F-7-a — two new §17 metrics:** "metadata bytes per small durable object" and "Chronicle stream bytes per user commit, by class (semantic/maintenance/audit)." Law 4 already demands them.
- **F-7-b — commit-group object packing:** batch per-group capsules/effects/templates into one packed object (the `MarkerBatch` precedent generalized), cutting object count and per-object identity overhead by an order of magnitude on the group-commit path.
- **F-7-c — make the FEC *policy* tiered (the `fec_profile` mechanism already exists):** full RaptorQ + repair budget on sealed/immutable/archive tiers (runs, checkpoints, capsules, backups); CRC + replication on hot short-lived Tier D blocks, which are replicated and sealed within seconds anyway. Uniform "RaptorQ everywhere" buys erasure tolerance where the object lifetime doesn't justify the CPU and space; tiering keeps the self-healing story where it pays.

---

## PART 2 — OMISSIONS (all additive; each one *increases* capability)

1. **Branch retirement.** Branch creation, fork, merge, grants, and GC floors are specified at full strength; there is **no branch-deletion protocol anywhere in the document**. With branch-per-agent as the flagship B6 pattern and a 10k-branch objective, agent sessions end, and deletion interacts with retention cuts, key-envelope two-stage destruction, merge ledgers, constraint roots, delta batches, and subscriptions. Needed: a `BranchRetire` control protocol — head tombstone, envelope/key destruction path, derived-generation reset, cursor `GAP`/reset semantics, merge-ledger disposition, and the corresponding registry rows, floors, and crash matrices. This is a lifecycle hole in a flagship feature, not a detail.
2. **Targeted erasure (right-to-erasure) vs. append-only history.** The governance stack (audit, DP, Warden, redaction) has no answer to "erase this person from *history*": retention cuts are coarse (`KEEP <duration|seqs>`), crypto-erasure is per-key. Add: per-subject/per-tenant DEK-wrap domains so erasure = key destruction at subject granularity; erasure tombstones honored by time-travel visibility within retention (a fourth visibility predicate, with the same witness/registry treatment as retirement); and a stated audit-chain policy — which the design already makes easy, since audit events bind commitments rather than plaintext. For a system pitching regulated industries and agent memory (people *will* ask to be forgotten), this belongs in the flagship story.
3. **Element-level TTL / decay.** Agent memory needs forgetting as a schema contract, not as client chores: `EXPIRES_AT` per element/type, driven through the ordinary effect/visibility machinery, with salience-weighted retention priority feeding GC policy. Cheap to specify now, genuinely differentiated, and directly on the B6 wedge.
4. **Arrow IPC / ADBC-shaped export + single-file archive open.** Constraint #1 bans arrow-rs, but nothing prevents *writing Arrow IPC bytes in-house* — and the GraphRAG/data-science audience expects Arrow-shaped results and ADBC drivers. Separately: the backup-archive machinery is one small step from "open a dataset read-only directly from a single `.fgdb` file" — dataset distribution is a real workflow (Kùzu's final release moved to single-file for exactly this reason), and the plan already owns every ingredient.
5. **The Python API contract.** `fgdb-python` is one W10 row. The ownership model (explicit `finish()`/`release()`, ownership leases, resume capabilities, `Drop` *cannot* authorize release) collides with Python's GC idiom (`with` blocks, refcounting, Jupyter). It needs its own contract section: context-manager ↔ ownership mapping, explicit close disciplines, streaming iterators ↔ the result machine, and the stated consequence of letting a result garbage-collect (it lives until profiled expiry — currently surprising and undocumented).
6. **macOS/Windows filesystem profiles.** The only writable profile is `FGFS-LINUX-LOCAL-V1`, and the snapshot/retirement machinery uses **OFD locks, which are Linux-specific**. The profile abstraction anticipates this (`required_lock_primitive`), but the embedded wedge's developers are disproportionately on Macs. An APFS profile (`F_FULLFSYNC`, flock-based lease, directory fsync) and a Windows profile should be scheduled as first-class work, not waved at as "a future platform profile."
7. **A spatial/geo index** — `Point(2D/3D)` is a stored property type; Beacon has no spatial index. At minimum a named plan or an explicit registered non-goal.
8. **Per-branch statistics strategy.** `StatsSegment`s are branch/schema/policy-watermarked; at the 10k-branch objective, per-branch statistics collection/storage is its own scaling problem (and interacts with the capability-partitioning rule of §8.5). The natural answer — shared base statistics + branch-delta statistics — is unmentioned.
9. **Sustained-load gates.** Every §17 gate is a point measurement. Add: 24-hour sustained-ingest-with-compaction, compaction-debt ceiling, long-run memory stability. Law 3 ("never hide compaction") deserves a gate that can actually catch thrash — this is where adaptive tiering proves itself.
10. **The `fgdb-reference` engine needs independence guarantees, not just existence.** It implements "the full logical semantics" as the differential oracle for everything; under the stated development methodology there is correlated-bug risk between it and the optimized engine. Require enforced stylistic independence (naive persistent maps, no indexes, no shared code) plus external cross-oracles (Neo4j/Memgraph differentials, openCypher TCK) so its bugs can't hide behind the main engine's. Also: apply Elle-class checking to the *external authorities' CAS chains*, not just the database.

---

## PART 3 — MARKET-REALITY CORRECTIONS (2026) AND WEDGE SHARPENING

The plan's §0 framing — "the company folded in 2025, leaving a scattering of community forks" and "a Kùzu-shaped hole in the market" — needs a factual update; the corrected picture makes the wedge *sharper*, not weaker:

- **Kùzu Inc. was acquired by Apple** (disclosed via Apple's EU DMA filing; surfaced publicly in February 2026), and the upstream repository was archived/read-only in October 2025.[^1^][^2^] The team and IP are inside Apple — a wildcard the plan should name rather than omit.
- **The embedded-graph scramble is already underway:** LadybugDB markets itself explicitly as the Kùzu successor for agentic AI;[^3^] Kineviz forked the codebase as "bighorn";[^4^] GitLab's preview Knowledge Graph is built on Kùzu;[^5^] FalkorDB is running Kùzu-migration content;[^6^] ArcadeDB publishes Kùzu comparisons (claiming ~9× on LDBC Graphalytics and 97.8% openCypher TCK).[^7^]
- **The GQL mindshare has cloud gravity:** Microsoft Fabric's Graph ships native ISO GQL with NL2GQL over OneLake, and Spanner Graph is GQL-compatible.[^8^] "We implement the standard" is not a wedge against that; the wedge is everything they *structurally cannot do*.

**Revision:** rewrite the §0 market paragraph to end on the defensible sentence: *"the only embedded graph engine with git-grade branches, byte-level determinism and replay, capability governance, and outsider-verifiable proofs."* And upgrade the Bolt-compat subset from read-only to **read-write on the 1.x roadmap explicitly** — read-only Bolt cannot carry a Neo4j migration test, and Neo4j is the largest installed base.

---

## PART 4 — SMALLER TECHNICAL NOTES (verified on re-read; each stands)

1. **Strict serializability is under-claimed.** §7.1 defaults to plain SERIALIZABLE and treats strict as an additional contract. But in single-node posture — single sequencer, effects computed against the running basis, results acknowledged only after visible publication — a transaction that begins after another's ack completed sees it by construction. The Local default is strict-serializable already; claim it (it's a marketing-grade property) and reserve named non-strict contracts for follower/bounded-staleness reads.
2. **Make rebase incremental by design, not by implication.** The merge ladder "evaluates the original intents against the current branch heads" (§7.4); `CapturedIntentObservation` already carries deterministic recomputation rules. "Re-evaluate only intents whose captured observations changed" should be the *center* of the rebase design — coordinator-side full re-execution of a 10k-intent transaction is the conflict-path CPU bomb.
3. **One precision note on escrow (§7.4):** the text sells escrow as "the missing middle" for coordination-freedom; inside one total order with final certification, its actual mechanism is abort/rebase-rate reduction for counter-shaped invariants — which is still excellent and worth stating accurately, since the Lean/conservation proof obligations price it as coordination-freedom.
4. **System-graph degraded mode:** observability dogfoods Prism (cycle detection over the wait-for graph). If Prism is off in a capability manifest, declare the observability plane's degraded mode explicitly — the control plane shouldn't silently depend on the analytics plane.
5. **fsync scoping nit:** state that parent-directory syncs on publication are confined to create/rename operations, not paid per slot pwrite.

---

## PART 5 — BOLDER DIRECTIONS (where the plan can go *further*)

The plan's own machinery supports product surfaces nobody else can build. These are additions, not changes:

1. **Deterministic agent replay as a first-class product.** The lab runtime + `Cx` threading + `ReplayManifest` = the only database that can record an agent's *entire* interaction and replay it byte-identically. Package it: "time-travel debugging for agent memory," including a mode where *the user's own agent code* runs under the deterministic lab (`fgdb-sim` as a customer-facing feature). No competitor has the substrate to copy this.
2. **A tiny, dependency-free verifier SDK.** The transparency layer (MMR, `TransparencyCheckpoint`, witnessed policies) is server-side today; ship a 2–3 KLOC embeddable verifier library so *third parties* can check checkpoint chains, inclusion proofs, and proof-carrying changefeeds without database access. "The database you can cross-examine" becomes a marketable product surface, and it forces the proof formats to stay honest.
3. **`DIFF` as a first-class GLA operator.** Branch merge exists; expose `DIFF graph@a...graph@b` (and `...AS OF`) as an operator producing canonical delta tables, composable with views and subscriptions. Git-style diff/merge UX is the most legible killer feature of B1/B6 for the agent-collaboration story, and it currently exists only as plumbing.
4. **Freshness as a headline covenant.** The market's flagged complaint is stale derived state; the plan already has commit→delta-tail-searchable and commit→merged-generation latency hooks. Elevate them to a top-line §17 gate and a `FRESHNESS` contract on hybrid retrieval (a `SUBSCRIBE … WITH FRESHNESS <bound>` surface), backed by Ripple watermarks.
5. **Memory economics:** TTL/decay (O-3) + salience-weighted GC priority + provenance-confidence propagation as a queryable lattice (extend Ripple's provenance-polynomial annotations to the query surface). This is the agent-memory feature set no one else is positioned to build.
6. **Chaos-mode-as-a-feature:** ship the LDFI/chaos harness to users ("prove your agent's memory layer survives faults") — B5 as a product, not just a dev tool.

---

## PART 6 — WHAT NOT TO TOUCH

Recorded so the revisions above don't accidentally erode them:

1. **The claim-type constitution** (`invariants.toml` vs `evidence.toml` vs `slo.toml`; invariant/proof/bounded_model/statistical/slo/benchmark). Prevents marketing claims from metastasizing into safety claims — best in class.
2. **"No correctness is inherited merely by analogy"** — the frankensqlite dissection discipline.
3. **The applied-vs-visible two-plane audit architecture** (pipeline + `AtomicProtocolDetach` + independent activation): resolves audit-before-output × output-liveness × time-authority decoupling. A real contribution.
4. **Escaping-allocation semantics** — identity stable at *first escape*, permanent spent tombstones. Subtle and correct.
5. **`AnswerRequirement`/`AnswerContract` as a semantic type** with total per-operator transfer rules — the first principled treatment of approximation-as-type; exactly what hybrid retrieval needs.
6. **Conditional references + checkpoint authority transfer** (`InstallProvisionalCut` dominating inherited absence but authorizing no new reclamation) — kills the floor→checkpoint→floor retention cycle correctly.
7. **Squash-merge single-parent visibility** with `OriginBirthOrder` vs `BranchAdmissionOrder`, plus idempotent `ImportedRangeKey` coverage.
8. **The graph-SSI witness algebra** (`NegativePattern`, `PathCut` mode-aware state, gap registration from `ORDER BY/LIMIT`/early stops) with the "reject in SERIALIZABLE mode until proved" gate — attacks the actual hard problem at the right abstraction level.
9. **The rejection list** (hyperedges, representation zoo, default plan racing, TrueTime, learned indexes as authoritative, WASM UDFs, external JIT).
10. **The five performance laws** (no benchmark-only semantics; distributions not averages; never hide compaction; memory first-class; adaptive numbers disclose policy epoch).

---

## PART 7 — CONSOLIDATED REVISION LIST

**P0 — changes formats/protocols; decide at G0:**

| # | Revision | Finding |
|---|----------|---------|
| 1 | Add `SnapshotQuery` lightweight read class; fuse autocommit write lifecycle; re-scope §17 point-read gates | F-1 |
| 2 | Write the normative latency anatomy; audit-`Required` first-class gates | F-2 |
| 3 | Resolve DirectoryBound allocation-epoch fence (register the local mode or document the external dependency) | F-5-a |
| 4 | `BranchRetire` protocol | O-1 |
| 5 | Targeted-erasure domains + erasure visibility predicate | O-2 |
| 6 | EId width: costed 64-vs-128 decision | F-6 |
| 7 | Capacity model for the command/maintenance stream; cursor-scale gate | F-3 |
| 8 | Claim strict serializability for Local | N-1 |

**P1 — engineering decisions with format consequences:**

| # | Revision | Finding |
|---|----------|---------|
| 9 | Raft hard-state frame evaluation or append-sync gate | F-2-b |
| 10 | Time-evidence amortization across deliveries | F-3-b |
| 11 | Per-cursor transition coalescing | F-3-c |
| 12 | Batch certification law for W12 (complete intra-batch edges) | F-4 |
| 13 | Per-tenant audit domains; authority-surface/availability section | F-5-b |
| 14 | Commit-group object packing; tiered FEC policy; two byte-economy metrics | F-7 |
| 15 | Incremental (observation-driven) rebase as the merge-ladder center | N-2 |

**P2 — additive surfaces and gates:**

| # | Revision | Finding |
|---|----------|---------|
| 16 | TTL/decay + salience-weighted retention | O-3, Bolder-5 |
| 17 | Arrow IPC export; single-file archive read-only open | O-4 |
| 18 | Python API contract | O-5 |
| 19 | APFS + Windows filesystem profiles | O-6 |
| 20 | Spatial index plan or registered non-goal | O-7 |
| 21 | Per-branch/base+delta statistics | O-8 |
| 22 | Sustained-load gates (24h ingest, compaction debt, memory stability) | O-9 |
| 23 | Reference-engine independence rules; Elle on external authorities | O-10 |

**P3 — positioning:**

| # | Revision | Finding |
|---|----------|---------|
| 24 | Rewrite §0 market paragraph for 2026 (Apple/Kùzu, forks, Fabric/Spanner); sharpen the wedge sentence | Part 3 |
| 25 | Read-write Bolt subset on the explicit 1.x roadmap | Part 3 |
| 26 | Bolder-direction workstreams: verifier SDK, `DIFF` operator, agent replay product, freshness covenant, chaos-as-a-feature | Part 5 |

---

### Closing judgment

The plan's architecture is right and, in several places (two-plane audit visibility, escaping allocation, approximation-as-type, the witness algebra), ahead of the field. Two of its own performance gates contradict its own lifecycle protocol — fix the protocol (F-1) or the gates, and the fix is strictly additive. Its sequencer stream, durability-barrier chain, and external-authority availability are unpriced — price them (F-2, F-3, F-5). It is missing lifecycle protocols (branch retirement, erasure, TTL) that its flagship use cases will demand — add them. And its market paragraph should be updated for the world as it actually is in 2026, where the conclusion is not that the hole is empty but that **this is the only design in the hole with branches, determinism, governance, and proofs**. None of this trims the ambition. All of it is in service of the ambition actually landing.

---

[^1^]: Kùzu repository, archived read-only October 10, 2025: https://github.com/kuzudb/kuzu
[^2^]: "Kùzu was acquired by Apple" — Hacker News discussion of Apple's EU DMA filing (February 2026): https://news.ycombinator.com/item?id=47071607 ; Apple DMA disclosure listing Kùzu Inc.: https://www.euromarketfilings.com/
[^3^]: LadybugDB — "the KùzuDB successor" positioning for agentic AI: https://ladb.dev
[^4^]: Kineviz fork of Kùzu ("bighorn"): https://github.com/kineviz/bighorn
[^5^]: GitLab Knowledge Graph (preview) built on Kùzu: https://kernelgems.substack.com/p/gitlab-knowledge-graph
[^6^]: FalkorDB — "Graph Databases for RAG: Moving from Kùzu to FalkorDB" (2026): https://www.falkordb.com/blog/graph-databases-rag-kuzu-falkordb/
[^7^]: ArcadeDB vs Kùzu comparison (LDBC Graphalytics and openCypher TCK claims): https://docs.arcadedb.com/arcadedb-vs-kuzu
[^8^]: Graph in Microsoft Fabric — native ISO GQL support and NL2GQL: https://learn.microsoft.com/en-us/fabric/graph/overview
