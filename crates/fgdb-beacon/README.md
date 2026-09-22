# fgdb-beacon

Memory-resident, derived HNSW and BM25 search kernels and immutable index generations. This is an implemented subset of Beacon, not completion of its durable, secure-view, or graph-transaction integration contract. Chronicle remains the durable authority.

## Search and incremental publication

`Hnsw::build` constructs deterministic multilevel graphs from full-width `VId` identities. `Hnsw::search` supports squared Euclidean, cosine, and negative-dot-product distance, with explicit approximate or exact search. An eligible-result predicate does not prevent traversal through excluded routing nodes. It is not an authorization boundary: narrow the source domain before building the graph.

`Bm25::build` analyzes Unicode alphanumeric runs and lowercases terms, without stemming or normalization. Search supports ANY/ALL matching and stable ID tie-breaking. The analyzer's Unicode tables follow the pinned Rust compiler.

`BeaconIndex::apply_batch` publishes vector and text updates, live visibility, and live-corpus BM25 statistics in one immutable generation. Every mutation is validated, including operations later overwritten in the same batch. Updates and deletions suppress superseded results while retained `IndexSnapshot`s preserve their previous view. Fixed-policy compaction rebuilds a base segment without mutating retained snapshots.

`IndexSnapshot::knn`, `text_search`, and `hybrid_search` share the same derived generation. Hybrid search uses weighted reciprocal-rank fusion over explicitly bounded candidate sets; it does not claim exhaustive hybrid top-k recall. Approximate HNSW is never silently replaced by an exact scan.

## One-pass bootstrap and replacement

`BeaconIndex::build` builds a complete unique-ID corpus into one base segment. `try_build` accepts a fallible document source. This avoids repeatedly copying a growing live map and vocabulary or constructing intermediate HNSW segments during bootstrap.

`replace_all` and `try_replace_all` prepare a complete replacement off-side and publish only after source exhaustion, both index builds, and a final work/cancellation checkpoint succeed. Source errors remain source errors, not end-of-stream. Duplicate IDs, invalid vectors, resource refusals, and cancellation leave the preceding writer generation unchanged. Retained snapshots remain usable after replacement, including replacement with an empty corpus.

Bulk construction is governed by live-corpus and work limits rather than the incremental `max_batch_operations` limit; it does not modify that incremental policy. Limits are checked for every accepted input prefix. Source polling is preceded by a checkpoint, but the iterator must enforce its own I/O and memory contracts.

All potentially long operations take caller-owned `WorkControl`. Accounting covers logical work and configured corpus/staging limits; it is not a byte-exact allocator quota. Incremental successor metadata still costs O(live documents + vocabulary). Retaining many snapshots can retain multiple corpora; there is no disk spill or global retained-snapshot memory governor in this crate.

## Integration boundary

These APIs consume caller-supplied derived documents. They do not establish that a graph transaction committed, authenticate a Chronicle marker, or automatically bind an index generation to an admitted graph read view. The saved embedded `Database` adapter is not wired into this published crate. Durable index descriptors and catalog DDL, typed embedding-matrix storage, GQL/GLA query integration, secure-view authority bindings, and larger-than-memory execution remain outstanding.

## Validation status

The crate includes 47 authored Rust tests: HNSW topology and vector regressions, BM25 scoring oracles, mutation/rebuild differential checks, retained-snapshot and compaction checks, failure-budget sweeps, and eight bulk-build/replacement regressions.

The publishing environment had no Cargo, rustc, rustfmt, or rch. These tests have NOT been executed here; compilation, formatting, Clippy, workspace lockfile reconciliation, and repository acceptance gates are unverified. Uploaded source/test Git blob hashes were checked against local files. TOML parsing and whitespace-diagnostic checks are not substitutes for Rust execution.
