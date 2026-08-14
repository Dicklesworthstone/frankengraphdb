//! **End-to-end §17 numbers on the real durable path, under hostile shapes.**
//!
//! Bead fgdb-uz0o, the half of fgdb-p95p that needed the spine. Its sibling
//! fgdb-drwe covers the tier-D read path in op counts; this covers what only
//! becomes measurable once `Database::create/write/open` exist: bytes actually
//! written to a real directory, by a real commit, read back by a real reopen.
//!
//! **LAW 1 IS THE REASON THIS FILE EXISTS AT ALL.** §17's first standing law is
//! *no benchmark-only semantics* — durability, isolation and result consumption
//! must match the declared production mode. Every number below is taken from a
//! database created by the ordinary `Database::create` path, written by the
//! ordinary `write`, and read back after `drop` + `Database::open`. Nothing is
//! measured in memory and labelled as durable. Doctrine 7 forbids reporting a
//! non-durable benchmark mode as a result, and this file is the first place in
//! the project where that distinction could actually be got wrong.
//!
//! **BYTES, NOT SECONDS, FOR THE SAME REASON AS THE OP COUNTS.** A wall clock is
//! nondeterministic (B5 forbids it as a *result*) and load-sensitive on a box
//! where a dozen panes compile at once. Bytes on disk after a fixed history are
//! exact, reproducible, and diffable — and §17's law 4 asks for bytes per live
//! edge *including* everything the payload does not cover, which is precisely
//! what a directory measurement gives and an in-memory estimate does not.
//!
//! **WHICH HOSTILE SHAPES ARE COVERED, AND WHICH ARE NOT.** fgdb-p95p names
//! five. Stating the unreachable ones explicitly, because a harness that
//! silently covers three of five reads exactly like one that covers all five:
//!
//! | shape | status |
//! |---|---|
//! | power-law degree skew | COVERED — `bytes_per_live_edge_under_power_law_skew` |
//! | cold partition reopen | COVERED — every witness reads after drop + reopen |
//! | long version chains | COVERED — `a_long_edge_version_chain_survives_cold_reopen_and_compaction` drives 65 exact property versions through the public durable write path, reads every historical value after a cold reopen, and proves compaction preserves them. |
//! | deep branch chains | NOT REACHABLE: the spine has no branch API. Chronicle's branch-parent walk is the cost this shape exists to expose, and it does not exist to call. |
//! | compaction under load | PARTIAL: `Database::compact` is now a public, durable trigger and the long-chain witness proves it never moves historical answers or increases the referenced-block count. Concurrent foreground load remains unreachable: `compact` requires exclusive `&mut Database`, the spine owns an exclusive writer lease, and it exposes no read-only snapshot handle. The latency-under-load half remains owed by fgdb-p95p. |
//!
//! Three covered, one partial, and one absent. The remaining portions are
//! absent because the engine cannot yet express the thing, not because the
//! harness declined to look.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::{Path, PathBuf};

const KNOWS: RelationId = RelationId(1);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys {
        k_oid: K_OID,
        namespace: NAMESPACE,
        dek: [0x3c; 32],
    }
}

/// A scratch directory that does not yet exist, so `create` owns making it.
///
/// Pid-scoped: concurrent panes run this suite against one `/tmp`, and a shared
/// fixture path makes one pane's run fail on another's leftovers. Nothing is
/// ever removed — cleanup would mean a test deleting directories, and rule 1
/// carves out no exception for test code.
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-hostile-{}-{name}", std::process::id()))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

/// Total bytes of every regular file under `dir`, recursively.
///
/// This is the honest denominator for law 4: it counts the manifest, the root,
/// every block, and any index or padding the format writes — not just the
/// payload someone remembered to add up. A number that flatters is worse than
/// no number.
fn bytes_on_disk(dir: &Path) -> u64 {
    fn walk(dir: &Path, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => walk(&path, total),
                Ok(kind) if kind.is_file() => {
                    if let Ok(meta) = entry.metadata() {
                        *total += meta.len();
                    }
                }
                _ => {}
            }
        }
    }
    let mut total = 0;
    walk(dir, &mut total);
    total
}

/// A power-law-ish degree distribution: one supernode, then a halving tail.
///
/// The whole point of the shape. A uniform-degree fixture makes every vertex
/// cost the same for the honest reason — every vertex IS the same — so it
/// cannot distinguish a cost that tracks degree from one that does not.
const SKEW: [(u128, u128); 7] = [(1, 64), (2, 32), (3, 16), (4, 8), (5, 4), (6, 2), (7, 1)];

fn live_edges() -> u128 {
    SKEW.iter().map(|(_, degree)| degree).sum()
}

/// WITNESS: bytes per LIVE edge on disk, after a real commit and a real reopen,
/// over a power-law-skewed graph.
///
/// **THIS PUBLISHES A BAD NUMBER AND THAT IS THE POINT.** §17 targets an
/// effective ≥4 B/edge for sealed runs. Nothing is sealed yet, there is no
/// compression, and the durable directory carries a manifest and a root beside
/// the blocks — so the measured figure is orders of magnitude above target. It
/// is what the engine does today, and writing it down is what makes the eventual
/// codec work measurable rather than anecdotal (fgdb-by2l is the slice that
/// should move it).
///
/// The assertion is a CEILING, not an equality: an equality here would red on
/// every unrelated format change and teach people to loosen it. A ceiling reds
/// only when the number gets worse, which is the regression worth catching.
#[test]
fn bytes_per_live_edge_under_power_law_skew() {
    let dir = scratch("skew-bytes");
    let measured = under_lab(41, {
        let dir = dir.clone();
        move |cx| async move {
            let cx = &cx;
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");

            let mut batch = WriteBatch::new(KNOWS);
            let mut eid = 1u128;
            // Destination vids are SHARED across hubs, and vertex identities
            // are create-once (the oracle refuses a duplicate CreateVertex, and
            // since fgdb-3xoi the tier-D fold refuses it identically) — so the
            // fixture creates each destination exactly once.
            let mut created = std::collections::BTreeSet::new();
            for (src, degree) in SKEW {
                batch.create_vertex(VId(src), vec![], vec![]);
                for k in 0..degree {
                    if created.insert(1000 + k) {
                        batch.create_vertex(VId(1000 + k), vec![], vec![]);
                    }
                    batch.add_edge(EId(eid), VId(src), VId(1000 + k), vec![]);
                    eid += 1;
                }
            }
            db.write(cx, batch).await.expect("the skewed batch commits");
            drop(db);

            // COLD REOPEN, so the number describes a durable database rather
            // than a process that still has everything in memory.
            let db = Database::open(cx, &dir, keys()).await.expect("reopens");

            // CORRECTNESS FIRST: a fast path that returns the wrong graph is
            // not a result. The bead requires every benchmark to assert what it
            // computed, and the skew is what makes this assertion sharp.
            let supernode = db.neighbours(VId(1), KNOWS).expect("reads the supernode");
            assert_eq!(
                supernode.len(),
                64,
                "the supernode must answer with all 64 neighbours after a reopen"
            );
            let leaf = db.neighbours(VId(7), KNOWS).expect("reads the leaf");
            assert_eq!(leaf.len(), 1, "the degree-one vertex must answer with one");

            bytes_on_disk(&dir)
        }
    });

    let per_live_edge = measured / live_edges() as u64;

    // MEASURED 2026-08-04: 298 B per live edge (37,920 bytes for 127 live
    // edges). §17's sealed-run target is an effective 4 B/edge, so this shape
    // sits about 75x above it — nothing is sealed, nothing is compressed, and
    // the directory carries a manifest and a root beside the blocks.
    //
    // The ceiling is set just under 2x the measured value. That is deliberate:
    // the first draft used 4096, which is 13x what the engine actually does and
    // would have sat green through a tenfold regression. A ceiling loose enough
    // never to fire is a comment with a test's salary.
    const CEILING_BYTES_PER_LIVE_EDGE: u64 = 512;
    assert!(
        per_live_edge <= CEILING_BYTES_PER_LIVE_EDGE,
        "durable cost is {per_live_edge} B per live edge ({measured} bytes for \
         {} live edges), above the {CEILING_BYTES_PER_LIVE_EDGE} B ceiling — \
         §17's sealed-run target is an effective 4 B/edge, so a regression here \
         moves away from a number already far off",
        live_edges()
    );

    // And the measurement is not vacuous: an empty directory would divide to 0
    // and pass the ceiling trivially.
    assert!(
        per_live_edge > 0,
        "measured zero bytes on disk for a committed graph — the durable path \
         wrote nothing, or the directory walk is looking in the wrong place"
    );
}

/// WITNESS (§17 law 2, distributions not averages): bytes per live edge swept
/// across scale, not sampled at one convenient size.
///
/// **ONE NUMBER IS AN AVERAGE WEARING A BOUND.** The headline 298 B/edge above
/// is a single point on a curve, and which point matters enormously here: the
/// durable directory carries fixed overhead — a manifest, a root — that one edge
/// pays in full and a thousand edges share. Reporting only the small-graph
/// figure overstates the steady-state cost; reporting only the large-graph
/// figure hides what a small database actually costs. §17 asks for the
/// distribution, so this sweeps it and asserts both ends.
///
/// The shape of the curve is itself the finding: if bytes/edge does NOT fall as
/// the graph grows, fixed overhead is not the story and the per-edge encoding
/// is, which points the optimisation somewhere completely different.
#[test]
fn bytes_per_live_edge_swept_across_scale() {
    const SIZES: [u128; 5] = [1, 4, 16, 64, 256];

    let measured: Vec<(u128, u64)> = under_lab(44, move |cx| async move {
        let cx = &cx;
        let mut measured = Vec::new();
        for edges in SIZES {
            let dir = scratch(&format!("sweep-{edges}"));
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(1), vec![], vec![]);
            for k in 0..edges {
                batch.create_vertex(VId(1000 + k), vec![], vec![]);
                batch.add_edge(EId(k + 1), VId(1), VId(1000 + k), vec![]);
            }
            db.write(cx, batch).await.expect("commits");
            drop(db);

            // Cold reopen, and assert the answer, so a cheap directory is
            // never mistaken for an efficient one.
            let db = Database::open(cx, &dir, keys()).await.expect("reopens");
            let found = db.neighbours(VId(1), KNOWS).expect("reads");
            assert_eq!(
                found.len() as u128,
                edges,
                "the {edges}-edge database lost neighbours across the reopen"
            );

            measured.push((edges, bytes_on_disk(&dir) / edges as u64));
        }
        measured
    });

    let worst = measured
        .iter()
        .map(|(_, per_edge)| *per_edge)
        .max()
        .expect("the sweep is nonempty");
    let best = measured
        .iter()
        .map(|(_, per_edge)| *per_edge)
        .min()
        .expect("the sweep is nonempty");

    // MEASURED 2026-08-04, (edges, bytes/edge):
    //   1 -> 4992,  4 -> 1408,  16 -> 539,  64 -> 328,  256 -> 277
    // RE-MEASURED 2026-08-09 after fgdb-ge6a's slot binding, (edges, bytes/edge):
    //   1 -> 13620, 4 -> 3557,  16 -> 1067, 64 -> 451,  256 -> 299
    // The jump is the dual-slot root file (doctrine 5's ONE mutable object,
    // fixed size) plus one retained manifest object per publish joining the
    // durable set — per-DATABASE and per-COMMIT costs, not per-edge ones,
    // exactly the fixed overhead the doctrine below says not to optimise.
    // The steady-state end moved 277 -> 299 and stays under its ceiling,
    // which is the evidence the added cost amortises as claimed.
    //
    // BOTH ENDS ARE PUBLISHED because they say different things. 4,992 B for a
    // one-edge database is the honest cost of a manifest plus a root plus a
    // block — what a small database actually costs, which a steady-state figure
    // would hide. 277 B/edge at 256 edges is the closest thing to a steady state
    // this engine has, and it is still ~69x §17's effective 4 B/edge target for
    // sealed runs.
    //
    // THE CURVE IS FLATTENING, and that is the load-bearing observation. The
    // fall is 18x across the sweep, but only 1.18x from 64 to 256 — fixed
    // overhead is nearly amortised by 256 edges, so ~277 B/edge is close to the
    // real per-edge encoding cost rather than a small-graph artefact. Anyone
    // optimising this should target the per-edge encoding (fgdb-by2l), NOT the
    // per-database overhead: the latter is already amortised away at any
    // interesting size, and a fix there would move the headline number while
    // changing nothing that matters.
    //
    // Ceilings sit ~1.4-1.6x above measured: tight enough to catch a real
    // regression, loose enough not to red on an unrelated format change.
    const WORST_CASE_CEILING: u64 = 20480;
    const STEADY_STATE_CEILING: u64 = 384;

    assert!(
        worst <= WORST_CASE_CEILING,
        "worst-case durable cost is {worst} B/edge, above the \
         {WORST_CASE_CEILING} B ceiling; full distribution {measured:?}"
    );
    assert!(
        best <= STEADY_STATE_CEILING,
        "best-case (largest graph) durable cost is {best} B/edge, above the \
         {STEADY_STATE_CEILING} B steady-state ceiling; full distribution \
         {measured:?}"
    );

    // THE SHAPE OF THE CURVE, pinned as a property rather than left implied:
    // cost per edge must fall monotonically as the graph grows. If it ever
    // rises, per-edge cost is growing with graph size — superlinear durable
    // growth, which is a far more serious defect than a bad constant and would
    // otherwise hide inside a ceiling that only checks the endpoints.
    for pair in measured.windows(2) {
        let (small, small_cost) = pair[0];
        let (large, large_cost) = pair[1];
        assert!(
            large_cost <= small_cost,
            "cost per edge ROSE from {small_cost} B at {small} edges to \
             {large_cost} B at {large} edges — durable size is growing \
             superlinearly in the graph; distribution {measured:?}"
        );
    }
}

/// CONTROL: the byte measurement actually tracks the graph.
///
/// Without this, `bytes_per_live_edge_under_power_law_skew` could be reading a
/// fixed-size manifest and reporting a stable, meaningless number. A bigger
/// history must cost more bytes; if it does not, the instrument is broken
/// rather than the engine being efficient.
#[test]
fn the_byte_measurement_responds_to_history_size() {
    let small_dir = scratch("bytes-small");
    let large_dir = scratch("bytes-large");

    async fn write_n(cx: &CommitCx, dir: &Path, edges: u128) -> u64 {
        let mut db = Database::create(cx, dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        for k in 0..edges {
            batch.create_vertex(VId(1000 + k), vec![], vec![]);
            batch.add_edge(EId(k + 1), VId(1), VId(1000 + k), vec![]);
        }
        db.write(cx, batch).await.expect("commits");
        drop(db);
        bytes_on_disk(dir)
    }

    let (small, large) = under_lab(42, {
        let (small_dir, large_dir) = (small_dir.clone(), large_dir.clone());
        move |cx| async move {
            let cx = &cx;
            let small = write_n(cx, &small_dir, 4).await;
            let large = write_n(cx, &large_dir, 64).await;
            (small, large)
        }
    });

    assert!(
        large > small,
        "a 64-edge history ({large} bytes) did not cost more than a 4-edge one \
         ({small} bytes); the byte measurement is not tracking the graph"
    );
}

/// WITNESS: a cold reopen answers identically to the process that wrote it.
///
/// The reopen path is the one that reads from the store rather than from
/// memory, and it is the shape the bead names as "the path that reads from the
/// store rather than from memory". Correctness across it is the precondition
/// for every number in this file: bytes measured on a directory that cannot be
/// read back are bytes describing nothing.
#[test]
fn a_cold_reopen_answers_identically_across_the_whole_skew() {
    let dir = scratch("cold-reopen");
    under_lab(43, {
        let dir = dir.clone();
        move |cx| async move {
            let cx = &cx;
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            let mut eid = 1u128;
            // Same create-once discipline as the byte-measurement fixture
            // above: shared destinations are created exactly once.
            let mut created = std::collections::BTreeSet::new();
            for (src, degree) in SKEW {
                batch.create_vertex(VId(src), vec![], vec![]);
                for k in 0..degree {
                    if created.insert(1000 + k) {
                        batch.create_vertex(VId(1000 + k), vec![], vec![]);
                    }
                    batch.add_edge(EId(eid), VId(src), VId(1000 + k), vec![]);
                    eid += 1;
                }
            }
            db.write(cx, batch).await.expect("commits");

            // Answers taken from the WARM database, before the drop.
            let warm: Vec<Vec<VId>> = SKEW
                .iter()
                .map(|(src, _)| db.neighbours(VId(*src), KNOWS).expect("warm read"))
                .collect();
            drop(db);

            let db = Database::open(cx, &dir, keys()).await.expect("reopens");
            for ((src, degree), warm_answer) in SKEW.iter().zip(warm) {
                let cold = db.neighbours(VId(*src), KNOWS).expect("cold read");
                assert_eq!(
                    cold, warm_answer,
                    "vertex {src} answered differently after a cold reopen"
                );
                assert_eq!(
                    cold.len() as u128,
                    *degree,
                    "vertex {src} lost neighbours across the reopen"
                );
            }
        }
    });
}

/// WITNESS: one live edge can accumulate a hostile content-version chain
/// through the public durable surface, and neither a cold reopen nor
/// compaction may move any historical answer.
///
/// This deliberately does **not** assert that the uncompacted root must carry
/// one block per update. That is today's expensive layout, not a law: an
/// incremental writer that packs history earlier is an improvement and must
/// stay green. The bound has teeth in the other direction — referenced blocks
/// may grow at most linearly with the number of committed versions, and a
/// compaction may never increase that read-amplification denominator.
#[test]
fn a_long_edge_version_chain_survives_cold_reopen_and_compaction() {
    const UPDATE_COMMITS: i64 = 64;

    let dir = scratch("long-edge-version-chain");
    under_lab(45, {
        let dir = dir.clone();
        move |cx| async move {
            let cx = &cx;
            let property = PropertyKeyId(7);
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");

            let mut first = WriteBatch::new(KNOWS);
            first.create_vertex(VId(1), vec![], vec![]);
            first.create_vertex(VId(2), vec![], vec![]);
            first.add_edge(
                EId(10),
                VId(1),
                VId(2),
                vec![(property, CanonicalScalar::Int(0))],
            );
            let mut expected = vec![(db.write(cx, first).await.expect("commits"), 0i64)];

            for value in 1..=UPDATE_COMMITS {
                let mut update = WriteBatch::new(KNOWS);
                update.set_edge_property(EId(10), property, Some(CanonicalScalar::Int(value)));
                expected.push((db.write(cx, update).await.expect("commits update"), value));
            }
            assert_eq!(
                expected.len(),
                65,
                "the hostile corpus must contain the creation plus 64 content updates"
            );

            let assert_history = |db: &Database| {
                for (sequence, value) in &expected {
                    let edge = db
                        .edge_at(EId(10), *sequence)
                        .expect("reads historical edge")
                        .expect("the edge remains live throughout the chain");
                    assert_eq!(
                        edge.props,
                        vec![(property, CanonicalScalar::Int(*value))],
                        "sequence {sequence:?} answered with the wrong content version"
                    );
                }
            };

            assert_history(&db);
            let root_before_reopen = db.partition_root().expect("healthy root");
            drop(db);

            // The cold path must reconstruct the entire historical chain, not
            // merely the frontier value cached by the writer.
            let mut db = Database::open(cx, &dir, keys()).await.expect("reopens");
            assert_eq!(
                db.partition_root().expect("healthy reopened root"),
                root_before_reopen,
                "cold open selected a different pre-compaction generation"
            );
            assert_history(&db);

            let store =
                fgdb_strata::store::BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
            let root_before_id = db.partition_root().expect("healthy root");
            let root_before = store
                .get_root(cx, root_before_id)
                .expect("resolves pre-compaction root");
            let blocks_before = root_before.blocks.len();
            assert!(
                blocks_before > 0,
                "a 65-version live edge references no blocks"
            );
            assert!(
                blocks_before <= 65,
                "{} committed versions reference {blocks_before} blocks — more than one new \
                 edge-block reference per version for this fixed hostile corpus",
                expected.len()
            );

            db.compact(cx).await.expect("compacts the hostile chain");
            let compacted_root = db.partition_root().expect("healthy compacted root");
            assert_ne!(
                compacted_root, root_before_id,
                "the hostile chain carried compaction debt, but compact published no replacement"
            );
            let root_after = store
                .get_root(cx, compacted_root)
                .expect("resolves compacted root");
            assert!(
                root_after.blocks.len() <= blocks_before,
                "compaction increased referenced blocks from {blocks_before} to {}",
                root_after.blocks.len()
            );
            assert_history(&db);
            drop(db);

            // The compacted generation is durable, and all 65 old answers are
            // still reconstructable after a second cold open.
            let db = Database::open(cx, &dir, keys())
                .await
                .expect("reopens compacted generation");
            assert_eq!(db.partition_root().expect("healthy root"), compacted_root);
            assert_history(&db);
        }
    });
}
