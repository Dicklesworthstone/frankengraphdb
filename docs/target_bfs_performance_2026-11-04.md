# Target BFS performance work — 2026-11-04

## Integrated qualification follow-up — 2026-11-04

The integrated source cut `c65ed09a5e955a34a5f85fd99099273b12b96393`
(tree `010aa47e398bd3df11dae4b63abb201bad6743d6`) now passes all 50 mandatory
repository gates with native producer and repository verifier exits of zero.
The all-feature workspace and both full default-feature runs each report
13,976 passed, zero failed, 38 ignored and zero filtered across 1,095 summaries.
All 155 selected acceptance functions, including the eight incoming target BFS
functions, execute successfully. The default runs have byte-identical normalized
results, test inventories and binary inventories; no prior test occurrence was
removed. The single actual Miri invocation passes nine tests. The scanner ratchet
conserves 277 existing critical identities; it is not a clean security scan.
Two new manual target-CALL benchmarks remain among the 38 ignored tests.

This executes the final selector-floor, full native CALL/cursor/integration,
compressed source-error and cancellation tests that were unrun in the original
report below. Integration repairs preserve their expected results and refusal
boundaries. Recovery build composition adds one cold-open ownership allocation
and one pointer word per bounded ordered-read slot; no extra per-read allocation
or recursion-limit relaxation was introduced. These costs are not measured RSS.

Fresh ordinary CLI/server artifacts on that cut also pass the unchanged
58-command public campaign, preserving 72 database files and empty spill storage;
all three servers drain with native exit zero. This is functional surface evidence.
The separate ordinary release/live-GAPBS BFS smoke was executed on source
`7a02946ad0eb5dce80bceacae2f3103160f02d6e`: two trials on eight vertices,
12 complete checked vectors, 51 checked target answers and one rejecting corrupt
upstream-result control. Every empirical gate remains false. All timings and
losses remain retained with their preparation, reopen and result-encoding scopes.
Neither this smoke nor the native tests establish representative performance,
p99, larger-than-memory behavior or a competitive win. `fgdb-xohc8` and
`fgdb-62mzm` remain in progress.

Retained local evidence: `/data/ssd/tmp/fgdb-local-proof-integrated-peer-source-20261104-v32/manifest.txt`
(SHA-256 `ca72afb76e3824dfcaad66db3f88867a54bd901b3db7be8260efbf1fae34cfc5`),
`/data/ssd/build/fgdb-integrated-acceptance-20261104-v32/qualification-c32-readback-ack.json`,
and `/data/ssd/build/fgdb-integrated-acceptance-20261104-v30/gapbs-smoke-readback.json`.
The original report, source hashes and pre-floor measurements below are retained
as historical evidence. This follow-up does not retroactively qualify those timings.

## Qualification status

**Native Cargo, complete public-CALL execution, database/GQL composition, live GAPBS comparison, and exact-tree qualification remain UNRUN. This record is not a passing gate or a competitive benchmark claim.** Existing work under `fgdb-xohc8` and `fgdb-62mzm` remains open.

The pinned `asupersync` dependency compiler was repeatedly killed by SIGKILL under the shared 8 GiB memory limit, including one-job/one-frontend-thread attempts, debug-information-only reductions, and a metadata-only check. The attempts exited 101 before Prism tests executed. No dependency, assertion, overflow check, feature, verifier, or acceptance threshold was changed to make those attempts pass. The shared overlay then filled, followed by temporary storage; the execution environment disconnected. The final verification attempt did not execute.

The last successfully compiled bidirectional candidate had SHA-256 `007cf1119d7f1f228adebc759e1d93c27d553158c3f76c6aaa0470594a131f60`. The final production source has SHA-256 `c5613958c2a94a3d665f2182353e490edc1b9239325defa984f09cfd13b4d649`: the subsequent change adds a 1,024-vertex floor for nonempty asymmetric seeds, based on the negative small-graph measurements below. That final restriction and its boundary tests were reviewed but **not compiled or executed** after the environment failed. Do not attribute the earlier compiled verdict to the final integrated tree.

## Changes

- Decoded FIFO and one-hop target BFS batch infallible bitmap/multi-edge work into runs of at most 64 logical units. Singleton rows keep the scalar path; this corrected a measured narrow-row regression. Logical charge counts, refusal ordinals, complete results, witnesses and workspace match the scalar predecessor in the bounded diagnostic.
- A decoded-only bidirectional kernel reuses both admitted CSR faces, visits complete distance layers and drains the discovering row. Two bitmaps and live queues replace the predecessor's population-sized append-only queue on selected requests. Its certificate reports a conservative backing-allocation peak, including temporary old/new capacity during growth; this is not allocator RSS.
- Automatic selection retains trivial/one-hop handling, typed endpoint errors and the old work allowance. It selects the candidate only when its work bound fits and either a seed face proves unreachability, or the graph has at least 1,024 vertices and `(1 + outdegree(source)) > 2 * (1 + indegree(target))`. The arithmetic avoids overflow. This is a conservative heuristic, not a latency guarantee for every target or reachable subgraph.
- Direction switches, observed frontier sizes and arc counts, policy version, dwell and benefit bounds enter a bounded decision card. The full Prism certificate and root-independent native procedure evidence bind every field. Calls without the new card retain the prior envelope layout; source identity naturally changes with source changes.
- Compressed providers retain their existing FIFO and source-error observation order. The decoded/sealed integration test still compares exact FIFO counters by also exercising the decoded budget fallback, while checking the routed answer and result digest.
- The GAPBS example now supports `--algorithm bfs|wcc|tc|sssp`, retains the default suite and selected verifier negative controls, and applies DeltaStep-specific input restrictions only to active SSSP runs. It adds fresh immediate, cooperative, and composed sealed target requests, with preparation, execution, complete result encoding and release phases. First-trial target coverage is complete; subsequent trials use deterministic rotating pairs. Target timings never enter all-destination speedup ratios.

## Executed bounded checks

| Check | Result | Exact scope |
| --- | --- | --- |
| Scalar predecessor versus batched decoded kernel | 15,196 refusal cuts passed | Exact result, witness, workspace, logical units and refusal position |
| Target kernel against independent BFS | 5,298,764 endpoint/cutoff cases passed | All directed four-vertex topologies, plus all three projection directions through three vertices; pre-floor candidate and routed selector |
| Asymmetric 17-vertex graphs | 41,616 cases passed | Exact distances, geometric switch bound; up to three direction changes |
| Bidirectional cancellation/work bounds | 482 refusal cuts passed | Pre-floor primitive kernel, not public CALL or runtime cancellation |
| Native procedure evidence transcript | 1 exact-source test passed | Real crypto/foundation artifacts and identity module; every new field, ordering/presence and excluded generation identities |
| Benchmark CLI/checker/weighted parsing | 9 exact-source tests passed | Selection, unchanged defaults, sampling, typed input domains, verifier tokens and complete row comparison |
| Individually edited source formatting and whitespace | Completed | Pinned formatter invocations and whitespace checks; final combined formatter invocation was blocked by the disconnected environment |
| Final floor and new complete-CALL/cursor/integration tests | UNRUN | Source reviewed; requires normal native compilation |
| Full workspace, Clippy, exact-tree proof and live DB/GAPBS run | UNRUN | No passing verdict claimed |

The kernel diagnostic compiled the actual production projection, target FIFO, bidirectional source and identity module, plus unchanged excerpts of the execution primitives. It linked actual pinned `fnx_algorithms` (`9d710b1c33e99412c94de7fa4de2f7ce4954110f`) and `fgdb_crypto` artifacts. It did not replace database/runtime types with simulated ones; those layers were outside the diagnostic. Compiler: rustc 1.100.0-nightly, revision `90850177249efe0321573c569aec5d12b257f8d6`; optimization level 2, debug assertions and overflow checks enabled, one frontend thread.

## Measurements that informed selection

These are same-invocation **maintenance diagnostics** on prebuilt projections: primitive execution, typed value conversion/hash, decision hash and cleanup, with an opaque observable meter. They exclude projection construction, public CALL admission/certification, signed host checks, durable I/O, sealed sources and database composition. There was no live competitor in these timings. They were collected on a shared, contended machine and are not confidence intervals or end-to-end speedups.

The large-graph run used 21 alternating sample pairs with three repeats per sample. Times below are p50 microseconds, from the pre-floor candidate hash above. The final floor leaves these populations eligible under the same degree policy, but the final source was not remeasured.

| 65,536-vertex fixture | Batched FIFO µs | Unconditional candidate µs | Degree-routed µs | FIFO arcs | Routed arcs | FIFO workspace bytes | Routed workspace bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Chain | 778.261 | 1,879.478 | 786.263 | 65,535 | 65,535 | 532,480 | 532,480 |
| Directed grid, far corner | 917.175 | 1,705.952 | 886.082 | 130,560 | 130,560 | 532,480 | 532,480 |
| Branching factor four | 846.042 | 10.599 | 9.174 | 65,535 | 8 | 532,480 | 16,736 |
| Disconnected, target has incoming edges | 459.722 | 1,179.440 | 465.204 | 32,767 | 32,767 | 532,480 | 532,480 |
| Isolated target | 949.717 | 3.956 | 3.331 | 65,534 | 0 | 532,480 | 0 |
| Hub, last target | 759.753 | 10.312 | 8.980 | 65,535 | 1 | 532,480 | 16,736 |

The unconditional candidate loses on the chain, grid and nonempty disconnected controls. The degree selector retains identical FIFO work, workspace and metering on these cases. The 16,384-vertex controls showed the same algorithmic pattern, with timing noise: routed/FIFO medians were 257.841/253.143 µs for chain, 356.361/341.836 µs for grid and 127.951/136.887 µs for nonempty disconnected.

A separate small-graph probe used 21 alternating sample pairs with 64 repeats. Its pre-floor results were:

| Vertices | Branching FIFO µs | Branching routed µs | Hub FIFO µs | Hub routed µs |
| ---: | ---: | ---: | ---: | ---: |
| 16 | 4.839 | 9.775 | 4.817 | 10.111 |
| 64 | 5.576 | 10.151 | 5.173 | 9.821 |
| 256 | 7.983 | 9.449 | 8.529 | 10.144 |
| 1,024 | 19.530 | 10.075 | 17.241 | 9.466 |

Those negative controls motivated the final size floor. Proved-empty seed faces bypass frontier allocation and remain eligible independently of size. The floor is not sufficient evidence for all real-world workloads: near targets, small reachable components within large graphs, cache behavior, and full host costs still need native end-to-end calibration.

## Native qualification to complete

Run the new Prism target tests, the retained compressed source-error/cancellation suite, the native procedure evidence transcript test and `prism_cooperative` target integration test using the pinned toolchain. Then run the real example in a fresh output directory, for example:

```sh
cargo run --locked --release -p fgdb-bench --example gapbs_compare -- \
  --gapbs ../gapbs --fixture smoke --algorithm bfs --trials 2 --sealed \
  --output /path/to/new-gapbs-bfs-results
```

Two trials cover complete initial target coverage and the deterministic follow-up sampler. Extend to representative sparse/hub and supplied real-world graph inputs, preserving full result checks and phase accounting. Qualify the exact integrated main tree with the repository's local proof workflow; do not use GitHub Actions. Keep the qualification work open until the required native verdict exists.

## Final staged source identities

These hashes identify the reviewed content preserved before the environment disconnected, not a successful native build:

| Repository path | SHA-256 |
| --- | --- |
| `crates/fgdb-prism/src/lib.rs` | `c0915e4a31cf5a47f52e951d622ba86e5a974c3373ed0d4f95d6a2b339ad42b9` |
| `crates/fgdb-prism/src/call.rs` | `0e4f2f89eb07fa4b504a201fabd090c1d1195499c11844585a82aae9362cdc5a` |
| `crates/fgdb-prism/src/execute.rs` | `ec5b5626c886af73c5ff716ea060ea69e4e23591bb28da8e2513610a3b2a62c7` |
| `crates/fgdb-prism/src/sealed_execute.rs` | `20b2730526c829f4400d2d1fe7f628ae6fb4ddce7cf34047cdcefd211b2086c1` |
| `crates/fgdb-prism/src/target_bfs_tests.rs` | `98093a8bc4f4d92c6918a22a9589a2aed40ad453cf3def135a71c7e7f0938600` |
| `crates/fgdb-prism/src/target_bidirectional.rs` | `c5613958c2a94a3d665f2182353e490edc1b9239325defa984f09cfd13b4d649` |
| `crates/fgdb-prism/src/target_bidirectional_tests.rs` | `2c55bdd9bd240fd0290f6be6a2d1b146a45e8067551109421011cd3a33c37da4` |
| `crates/fgdb/src/gql_cert.rs` | `160a95e0260321f7e1c282a328b924b895236789a08ade9ec5044a2f34a93428` |
| `crates/fgdb/tests/prism_cooperative/target_bfs.rs` | `1adfb908dda2336770ccc0e2dbc24068bc8e0572a0624b470b5fa35becd37fb4` |

Benchmark harness SHA-256: `7dded3da0f9aae789c89994d65aff0b58d1ccb5521d1317c7189b2f2858d6f32`.
Decoded target FIFO SHA-256: `f7846beb65321e43369bdef2df46b382c10ccf30766ca2ceed24fd17abde05bd`.
