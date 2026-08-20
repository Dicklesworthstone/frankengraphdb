# Audit verdict

I read every line of AGENTS.md:1, README.md:1, and the 874-line COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:1.

The vision is unusually strong, coherent at the conceptual level, and genuinely original. But the plan is not yet safe to treat as a normative implementation specification. I found several cross-subsystem contradictions where the advertised invariants cannot all hold simultaneously, followed by substantial missing contracts that could lead independent
crate implementations to be incompatible.

The most important problems are at the seams between Chronicle, transactions, branches, Ripple, replication, security, and historical indexes.

## P0 — Blocking architectural contradictions

1. The coordinator can commit a capsule different from the transaction it approved.

    The writer finalizes, encodes, and persists an immutable capsule before the coordinator validates it (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:235). The coordinator can then rebase the transaction and regenerate constraints, indexes, statistics, and Ripple deltas (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:392). But the marker
    still references the old capsule format defined in COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:794.

    A successful rebase can change resolved IDs, before/after values, physical deltas, witnesses, and logical changefeed effects. Recovery or replication could therefore apply something other than what was validated.

    Capsule finalization must occur after the last successful rebase. The commit state machine needs a canonical CommittedEffectSet, a validated apply-basis digest, regenerated witnesses, and an invariant that the marker’s effect digest equals the coordinator-approved digest.

2. The marker format cannot represent both the global commit stream and branch ancestry.

    The plan requires one total, gap-free CommitSeq for transactions, Ripple, and replication (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:208), while each branch has its own marker chain (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:259). Yet CommitMarker contains only one prev_marker_oid and one branch
    (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:796).

    One predecessor cannot encode both global order and branch-local ancestry. The singular branch field also cannot represent the claimed atomic transaction spanning multiple graph/branch pairs.

    The design needs:
    - one global marker chain;
    - per-touched-branch predecessor/head updates;
    - touched_graph_branches[];
    - merge commits with multiple parent heads and a merge base.

3. manifest.root, the sole mutable root of trust, has no crash-atomic format.

    The plan says every object is immutable except manifest.root (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:229), but Appendix A does not define the root at all. Missing items include generation numbering, checksums, dual slots or superblocks, torn-write selection, directory durability, rollback behavior, and the durable ObjectId-to-location
    index required for bounded recovery.

    There is also a contradiction between markers being ECS/RaptorQ objects and markers being raw approximately 100-byte appends.

    Before storage implementation, specify:
    - immutable root objects plus a checksummed dual-slot pointer;
    - exact file and directory durability ordering;
    - marker framing and partial-tail recovery;
    - segment indexes and trailers;
    - durable-closure and root-survival invariants.

4. The single-writer guarantee is process-local.

    The embedded library and server can independently open the same database path, but nothing fences two WriteCoordinator instances. The plan even names multi-process manifest-race testing (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:620) without defining the protocol being tested.

    Add an exclusive writer lease/file lock, a persisted fencing epoch in every root and marker, takeover recovery, stale-writer rejection, and an explicit supported-filesystem contract.

5. Mutable labels conflict with VId identity and label-pair physical placement.

    Vertices have mutable multi-label sets, while VId embeds a 12-bit label_class; directories are per label class and runs are keyed by (src_label, edge_type, dst_label). AddLabel and RemoveLabel are ordinary intents (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:807).

    If label_class reflects logical labels, changing a label changes identity and potentially requires rewriting every incident edge. If it is merely a stable physical class, the plan does not define the separate logical-label membership index or which label-pair run owns an edge whose endpoints have multiple labels.

    Identity and physical partitioning should be independent of mutable labels. Store versioned label membership separately and treat label-pair projections as derived access paths. The 8-bit generation field also wraps after 256 slot reuses; either widen it substantially or prohibit reuse while any historical/external reference can survive.

6. The Graph-SSI contract does not yet prove serializability.

    Several distinct issues combine here:
    - Marker order is asserted to be the serialization order, but SSI does not generally serialize in commit order.
    - The epoch comparison described for readers cannot detect every writer that commits later.
    - SSI read metadata must survive reader commit until all overlapping transactions finish.
    - Refining a predicate to elements actually observed loses gaps and absent elements needed for phantom protection.
    - Negative patterns, ORDER BY/LIMIT, index gaps, and path shortcuts need witness domains beyond returned elements.
    - The path witness (NFA state, partition, settled bound) has no formal may_affect(write, witness) relation.

    Cahill–Röhm–Fekete’s SSI design explicitly retains read-conflict information beyond transaction commit while overlapping transactions remain active; dangerous structures are defined through rw-dependencies, not by assuming commit order. Serializable Isolation for Snapshot Databases (https://www.cs.cornell.edu/~sowell/dbpapers/serializable_isolation.pdf)

    Define an abstract predicate-access semantics and prove every concrete witness is conservative for it. Include range gaps, negative domains, early-stop boundaries, and path-frontier cuts. Either ordinary readers may receive serialization failures, or every reader must obtain a safe snapshot and the plan must admit that this can wait.

7. Raft can commit a marker whose capsule no surviving replica can recover.

    Aegis sequences tiny markers through Raft while capsule bytes travel out of band (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:571). No durable payload-availability quorum is required before marker commitment, and “decoded” is weaker than “fsynced and independently recoverable.”

    Raft requires committed state-machine commands to remain available to future leaders. Raft extended paper (https://raft.github.io/raft.pdf)

    The state machine should be:
    1. Finalize the canonical capsule.
    2. Persist sufficient recoverable symbols on the required failure-domain quorum.
    3. Collect authenticated availability receipts.
    4. Propose the marker plus receipts through Raft.
    5. Derive CommitSeq from committed consensus order.
    6. Publish heads and acknowledge the client only after the declared durability condition.

    Leadership-change recovery, orphan cleanup, COMMIT_UNKNOWN, and idempotent status lookup also need contracts.

8. Per-shard Raft has no distributed transaction or time model.

    The design depends on one total CommitSeq, cross-graph atomicity, global branch heads, SSI, uniqueness, and linearly ordered Ripple input. Per-shard Raft groups (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:578) provide none of those automatically.

    topology_epoch fences ownership but does not supply atomic commitment.

    Choose one of:
    - a global metadata/sequencing group plus recoverable prepare/commit across shard groups and distributed SSI; or
    - a partially ordered time model, requiring Chronicle, branches, Ripple, and certificates to be redesigned around vector/lattice time.

    “Sharding is activation, not rewrite” is not supported by the current design.

9. Ripple conflates state-dependent commands, integer deltas, and provenance semirings.

    The plan says the intent stream is “precisely” a Z-set stream and calls Z-sets a counting semiring (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:474). But EnsureEdge, compare-and-set, generated keys, cascade deletes, schema operations, LWW, and merge policies are commands whose effects depend on state and may be no-ops.

    DBSP’s Z-sets use integer weights with additive inverses; Boolean and tropical semirings cannot simply replace that group while preserving deletion and differentiation. DBSP paper, §4.1 (https://www.vldb.org/pvldb/vol16/p1601-budiu.pdf)

    Persist a post-validation LogicalDeltaBatch of canonical insertions and retractions derived from the final before/after state. Make it the only Ripple/CDC input. Keep provenance or cost annotations as a separate product/semimodule algebra with defined deletion semantics. Boolean is existence provenance, not standard why-provenance.

10. MVCC-filtered HNSW does not provide stable historical ANN semantics.

    Filtering invisible nodes from output is insufficient. Future, deleted, or unauthorized nodes can influence traversal as routing bridges; deleted nodes can disconnect old search topology; rebuilds can change candidates for the same snapshot; and asynchronous index feeding leaves an unindexed delta tail.

    Exact reranking only reranks candidates already found—it does not bound missed recall.

    Every query must pin an index-generation root and watermark. Search topology must be snapshot- and capability-legal, with delta-tail compensation and an exact fallback when no compatible historical generation exists. The certificate must include the index generation.

11. Authorization is an optimizer rewrite rather than an end-to-end security boundary.

    Warden caveats are compiled into mandatory planner predicates (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:542), but many paths bypass ordinary scans and expands:
    - Prism procedures;
    - materialized views and indexes;
    - subscriptions and CDC;
    - mutation constraints and cascade effects;
    - statistics and cardinality errors;
    - branch merge and backup/export;
    - UDFs, system graph, certificates, and conflict reports.

    Hidden data could leak through degree, ANN routing, IDF statistics, uniqueness failures, timing, or error detail.

    Introduce an unforgeable AuthorizedSnapshot/AuthorizedGraphView required by every cursor and derived-state consumer. Add property and system-field masks, invoker/definer semantics for views, policy-epoch reauthorization for streams, capability-safe errors, and a complete API/operator noninterference matrix.

12. The replay certificate does not identify the execution being replayed.

    certificate + seq + seed omits at least:
    - database/root identity;
    - graph and branch head;
    - normalized query and parameter digest;
    - semantic profile;
    - schema, topology, policy, and capability epochs;
    - UDF/procedure code;
    - binary/toolchain/CPU numeric profile;
    - index-generation roots;
    - external inputs and redaction profile.

    KEEP NONE can also reclaim the very state needed to replay.

    Define replay over a canonical logical result encoding and bind the certificate to the full evidence closure. Either archive every referenced object and executable artifact or scope FG-INV-19 to the period for which that closure remains retained.

13. Per-branch encryption contradicts structural sharing and branch-key handoff.

    A child branch structurally shares encrypted ancestor objects, so handing a partner only the child branch DEK cannot decrypt those objects. Re-encrypting them would destroy O(1) branching and deduplication.

    The plan must separate logical object identity from physical encoding identity and define a key-envelope DAG. A robust shape is one random object DEK per stored encoding, wrapped under versioned database/branch/recipient KEKs. Handoff should wrap keys to a recipient, not expose a raw database-derived key. Rotation, revocation limits, backup escrow,
    signing-key lifecycle, and deterministic-replay entropy boundaries are currently absent.

14. Transparency proves less than the plan claims.

    A cached MMR root can detect rollback against one client, but not split-view equivocation between isolated clients. That requires mandatory gossip, independent witnesses, or cross-logging. Certificate Transparency uses exchanged/signed tree heads to address this class of attack. RFC 6962 (https://www.rfc-editor.org/rfc/rfc6962)

    Likewise, an MMR inclusion proof establishes that an emitted commit belongs to history; it does not prove that a filtered subscription omitted no matching events. COALESCE also deliberately changes observable event granularity.

    Narrow the claim to membership and per-client consistency unless witness/gossip requirements become mandatory. Gapless feeds need contiguous sequence proofs or a committed capability-specific delta journal, including empty commitments.

15. Retention contradicts the “commit stream is always the sole truth” doctrine.

    If old capsules are physically reclaimed, the latest retained anchor/checkpoint becomes authoritative for recovery. A mixed-graph capsule also cannot be partially reclaimed for different per-graph policies while preserving its object identity. Branches, replicas, backups, leases, and certificates can independently extend required retention.

    Define an authoritative checkpoint and truncation horizon. Reclaim a capsule only after the checkpoint is durably published and every branch/lease/backup/replica/certificate root permits collection. Mixed-graph effects need separately addressed subobjects or union retention.

## Other high-priority specification gaps

### Storage and formats

- Object identity: FG-INV-09 says ObjectId ≡ content, but ObjectId is a truncated 128-bit hash. Equal locators do not prove equal contents. Store the full digest and treat 128 bits as an index key with collision resolution.
- Logical versus physical identity: Encryption, compression parameters, RaptorQ parameters, key epoch, nonce, and ciphertext digest need a separate EncodingId. Donors must never combine symbols from different encodings of the same logical object.
- SymbolRecord is incomplete: COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:775 lacks an explicit source-block number, full framing, endianness, canonical bounds, transfer length, encoding identity, and partial-tail behavior. RFC 6330 identifies encoding symbols within source blocks; OTI plus ESI alone is not a complete record contract. RFC 6330
(https://www.rfc-editor.org/rfc/rfc6330)

- Compression contradiction: offsets are Elias–Fano while neighbors are delta-varint, but the executor later claims Elias–Fano rank/select directly over compressed neighbor values. Either encode neighbor sequences with Elias–Fano or add skip/fence indexes and say “without full-list decompression.”
- Historical holes: a current hole bitmap cannot hide deleted positions from old snapshots. Holes must be versioned, or a run can only be rewritten after the relevant historical horizon is gone.
- Snapshot pinning: pinning an object generation against reclamation is different from pinning every touched buffer frame in memory. The latter makes a long analytical snapshot capable of rendering a larger-than-memory database unevictable.
- Constraint indexes: unique indexes enforce correctness, but FG-INV-18 says every derived index can be discarded and rebuilt. Constraint state must either be synchronous canonical commit state, block writes during rebuild, or fall back to authoritative base validation.
- Branch merges: BranchManifest has only one parent and fork sequence. Auditable and idempotent merging needs source head, target basis, canonical merge base, imported range/token, all parent heads, policy digest, and final resolved-effect digest.
- Time selectors: GQL examples use timestamps, but the durable model only clearly defines CommitSeq; marker wall clocks can regress. Define a monotonic commit-time/HLC mapping and its branch semantics.
- Inline adjacency descriptors: a descriptor per (direction, edge type) cannot stay packed inline when the schema has unbounded edge types. A sparse descriptor table or indirection is required.

### Query language and execution

- Path memoization: (NFA state, vertex) is sound only for history-insensitive WALK reachability. TRAIL needs used edges; SIMPLE/ACYCLIC need visited vertices; temporal paths need accumulated interval; quantified captures and costs may distinguish prefixes.
- Path termination: unbounded ALL WALK over a cycle has infinitely many results. SIMPLE and TRAIL enumeration can be exponential, and cheapest paths require explicit negative-edge/negative-cycle rules. The binder needs a legality matrix for every {mode, selector, bound, cost domain}.
- Incomplete logical algebra: COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:825 lacks null-extending optional matching, semi/anti joins, correlated Apply, NOT EXISTS, and EXCEPT/INTERSECT. FreeJoin cannot supply those semantics by itself. A binder/type-checker/normalizer crate is also absent from the crate layout.
- Appendix C is not yet normative: it says each operator declares determinism, spill, cancellation, incrementalization, witness, and path contracts, but provides only an inventory. The promised matrix is essential.
- Factorization is underspecified: Factorized(parent_col, run_slice) cannot represent arbitrary union/product DAGs, multiple parents, fractured slices, multiplicity, order, path DAGs, overlays, or capability masking. Define a recursive factorization algebra and which operations preserve it.
- Larger-than-memory execution: spill algorithms are missing for full sort, distinct, WCOJ/COLT, factorized intermediates, recursive arrangements, Ripple state, Prism materialization, and vector/hybrid search. “Never OOM” also needs a deterministic resource-exhaustion result when the minimum working set cannot fit.
- Adaptive replanning: a morsel boundary is not automatically a safe cutover for global joins, aggregates, distinct, order, fixpoints, or path enumeration. Each adaptive operator needs a proved continuation/state-transfer contract, or replanning must occur only before output or at a materialization barrier.
- Prism is not presently zero-copy: the actual /dp/franken_networkx/crates/fnx-algorithms/src/lib.rs:152 returns &[usize], while the proposed cache contains u32/u64. Safe Rust cannot expose those as &[usize] zero-copy. The trait also does not natively encode FrankenGraph’s typed directed multigraph projection. Add a cursor/GAT interface upstream or
acknowledge materialization and specify a FnxProjectionSpec.

- Insertion-order conflict: the default InsertionOrder tie policy conflicts with destination-sorted CSR runs and compaction. Preserve an explicit logical incidence-order token or use a storage-independent canonical default.
- Floating-point determinism: pairwise/Kahan reduction alone does not ensure heterogeneous-machine byte identity. SIMD width, FMA, denormals, NaN bits, and math functions can differ. Rust explicitly documents platform-dependent floating behavior. Rust f64 documentation (https://doc.rust-lang.org/stable/core/primitive.f64.html) Define a strict portable
numeric profile or scope replay to identical targets/builds.

- Materialized-view shape: arbitrary GQL views may return tables, aggregates, paths, or constructed graphs. They cannot all be “virtual labels/edge types.” Separate table views from explicitly constructed graph views.
- View watermark semantics: silently joining both the base and view at w downgrades a current query. Define explicit WAIT, ALLOW STALE, AS OF, and delta-tail-compensation modes.
- Trigger exactly-once claim: a durable obligation provides durable retry or at-least-once invocation, not exactly-once effects in an external system. Require a transactional sink or stable idempotency key/outbox.
- PQ guarantee: product quantization does not generally provide a Johnson–Lindenstrauss-style universal multiplicative distance guarantee. Seeded construction makes it reproducible, not multiplicatively exact. Record additive/residual or empirical reconstruction bounds instead.
- Hybrid retrieval: specify candidate depths, normalization, tie order, duplicate aggregation, expansion behavior, snapshot-specific BM25 statistics, index watermarks, capability-safe candidate generation, and exact certificate fields.

### Security, protocols, and operations

- Resource isolation: budgets omit durable disk, temporary spill, output bytes, network egress, retained snapshots, branch/view/index counts, subscription backlog, backup bandwidth, and control-plane work. A slow client can otherwise retain snapshots and resources indefinitely.
- Cancellation and ambiguous commits: cancellation at arbitrary publication stages needs a transaction state machine, idempotency token, COMMIT_UNKNOWN, and status lookup. Cancellation checkpoints inside large decodes, kernels, and UDF operations must be bounded.
- FGP is only a sketch: COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:829 lacks transaction begin/rollback, query/request/stream IDs, acknowledgements, resumable cursors, flow control, auth refresh, fatal-versus-retryable errors, and atomic snapshot-plus-subscription bootstrap.
- Backup and restore: define a signed backup manifest containing the exact root/head, object inventory, base dependencies, key envelopes, format/profile versions, and transparency checkpoint. Creation must acquire a retention lease; restore must stage, validate, replay/check semantic digests, and only then atomically promote.
- Audit retention: if commit history can be reclaimed, it cannot simultaneously serve as an indefinite compliance audit log. Administrative reads, denied authorization, key/token changes, backups, exports, and policy changes also need an event taxonomy and separate retention policy.
- Native Rust UDF determinism: a test suite cannot prove arbitrary native code deterministic. Such UDFs should be excluded from replicated/logical-state-changing paths or isolated behind a deterministic VM with explicitly mediated effects.
- Differential privacy: define the stable privacy principal and neighboring-data relation. Per-token accounting is bypassable via multiple tokens or aliases. Deterministic replay also requires carefully scoped sticky/PRF-derived noise without exposing a seed that allows noise removal.
- Crypto lifecycle: the plan proposes implementing Argon2id, XChaCha20-Poly1305/SIV, signatures, and other primitives in-house. Test vectors do not establish constant-time behavior or side-channel safety. Prefer audited foundation implementations; otherwise add a dedicated cryptographic review, constant-time verification program, key-management
specification, and external audit gate.

### Verification and statistical claims

- DPOR scope: “exhaustive over inequivalent interleavings” is true only for a bounded scenario and a sound independence relation, not for the whole database.
- Independent oracle required: using FrankenGraphDB’s own cycle detector to validate its serialization graphs creates a common-mode failure. Implement a deliberately separate tiny SCC/Tarjan checker in the reference/oracle layer.
- Conformal coverage is overstated: continuously refreshed split conformal assumes exchangeability; workload adaptation and policy selection violate the simple guarantee. Nonexchangeable conformal methods require explicit penalties or modified frameworks. JMLR: Conformal Prediction Beyond Exchangeability (https://www.jmlr.org/papers/v25/23-1553.html)
- Off-policy evaluation needs support: logging propensities does not make “any candidate policy” evaluable. Whenever the candidate chooses an action, the logging policy must give that action positive probability. Wang–Agarwal–Dudík (https://proceedings.mlr.press/v70/wang17a/wang17a.pdf)
- Statistical monitors are not safety enforcement: e-processes, fitted MTTDL, controller stability regions, and leakage estimates are model-relative evidence or SLOs, not invariants equivalent to crash atomicity or access control.
- TLA+ trace inclusion is limited: showing captured traces are permitted by a model is useful, but it is not an implementation refinement proof and does not show unobserved executions conform.
- Scrub statistics: predictable deterministic-order sampling does not support a corpus-wide corruption-rate claim under correlated or adversarial faults. Use keyed randomized, failure-domain-stratified sampling plus guaranteed full sweeps.
- Claim taxonomy needed: distinguish theorem, bounded model check, runtime assertion, statistical confidence statement, and empirical benchmark gate in invariants.toml.

## Workstream and dependency problems

The sequencing in COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:725 conflicts with the architecture:

- W4 depends only on the Strata read path, but full Loom mutation semantics require Chronicle/Txn, snapshots, schema, and SSI.
- Warden arrives in W8 after storage cursors, indexes, views, Prism, caches, and certificates are fixed, even though authorization changes all of them.
- Aegis arrives after the local commit protocol is fixed, although Raft changes sequence allocation, marker publication, and client acknowledgement.
- Formal anchors land at G3, after G1 and G2 already claim serializability, recovery, branches, Ripple, and historical indexes.
- W6’s unique indexes are needed for correctness before “Beacon complete.”
- W8 combines Fabric, Warden, encryption, Raft, replication, and multi-writer—a scope too broad for one independently verifiable workstream.
- Sharding is described as the final workstream, but W1–W8 contain no sharding workstream; G4 freezes only a design document.

Security types, root/commit semantics, and proof obligations need to move into W1/W2. Fabric, Warden, and Aegis should be independently gated.

## Concrete document and repository errors

1. The plan has seven bets, including Sextant/B7 (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:18); AGENTS.md:43 and README.md:93 still say six.
2. The plan defines invariants through FG-INV-28 (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:863); AGENTS and README say FG-INV-01 through FG-INV-20.
3. The plan says the full invariants.toml exists in-repo (COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md:839). It does not currently exist. Neither do the promised Cargo.toml, rust-toolchain.toml, or scripts/check.sh.
4. AGENTS.md:136 requires #![forbid(unsafe_code)] at every crate root while allowing nested #[allow(unsafe_code)] islands. Rust’s forbid level cannot be lowered by an inner allow. rustc lint-level documentation (https://doc.rust-lang.org/rustc/lints/levels.html) Ledgered unsafe islands must be separate crates whose roots do not inherit forbid; their safe
    consumers can retain it.

5. The README’s shortest-path examples call path_length(p) without binding p: README.md:60, README.md:297.
6. The hybrid example passes a pattern literal as a value—seeds => [(:Person ...)]—but the plan defines no pattern-literal expression (README.md:79). Bind the node in a preceding match/subquery or pass a documented ID/node value.
7. The plan calls the FQL additions “namespaced,” but most listed productions are unnamespaced keywords. Use an explicit dialect/feature declaration or FQL-prefixed/versioned productions.
8. fgdb-python/PyO3 needs an explicit dependency exception. The closed universe forbids external crates, while the Python plan requires PyO3.

## Recommended rewrite order

1. Freeze the logical state model: identity, labels, graphs, branches, scalar time, merge parents, and canonical logical effects.
2. Rewrite Chronicle’s root, marker, capsule, checkpoint, retention, and multi-process fencing protocols.
3. Rewrite the transaction state machine so rebase precedes final capsule construction and specify full SSI witness/lifecycle semantics.
4. Define authorization and resource capabilities as mandatory cursor/storage types.
5. Complete the typed GQL semantic layer and the real Appendix C operator-contract matrix.
6. Specify historical/index-generation semantics, Ripple’s canonical delta boundary, and replay evidence closure.
7. Make the local commit protocol Raft-ready, then define payload availability and only afterward decide whether sharding retains global scalar time.
8. Replace Appendix F’s mixed claims with a machine-readable registry containing claim class, assumptions, checker path, owner, dependencies, and gate.
9. Reorder workstreams around those contracts and synchronize README/AGENTS from the authoritative registry.

## What is already excellent

The foundational direction is worth preserving:

- the commit-stream/branch/time-travel unification is powerful;
- source-of-truth versus derived-state separation is exactly the right instinct;
- the no-prototype doctrine prevents architectural shortcuts from becoming permanent;
- simulation-first development, the reference engine, crashpacks, and scoped formal anchors are unusually disciplined;
- decision cards and deterministic fallbacks are a serious answer to adaptive-system auditability;
- the plan is unusually candid about several risks.

The weaknesses are mostly not a lack of ideas. They are places where individually compelling subsystems have not yet been given one mutually consistent state, durability, security, and replay contract.