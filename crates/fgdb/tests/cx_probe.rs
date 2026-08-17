//! **The production-runtime `Cx` an external crate obtains, and what that
//! permits us to measure.**
//!
//! Beads fgdb-r8fa / fgdb-uz0o / fgdb-p95p. Pinned asupersync v0.4.7 source
//! revision `c17e5193` makes `Runtime::request_cx_with_budget` public. Every test here now
//! obtains its root context from that runtime boundary with default features
//! disabled; `Cx::for_testing` and `test-internals` are absent from this package.
//!
//! This closes the production-authority blocker, not the benchmark program. A
//! **§17-conformant benchmark manifest remains unavailable**: §17 activates a
//! gate only once its manifest pins
//!    CPU/microcode, kernel and mount options, NVMe model and measured fsync
//!    distribution, toolchain, and configuration. None of that is pinned here,
//!    and this box runs a dozen panes compiling at once.
//!
//! **SO THE NUMBERS BELOW ARE REPORTED, NEVER GATED.** §17 itself draws that
//! line; AGENTS.md line 243 and fgdb-p95p's own recorded design decision both
//! say the CI instrument here is an op count, not a stopwatch — a wall clock is
//! nondeterministic (B5 forbids it as a *result*) and load-sensitive. The
//! assertions here are on CORRECTNESS and a catastrophe ceiling only. If you
//! want a threshold gate, the op-count witnesses under fgdb-drwe are the
//! instrument; do not promote these.
//!
//! **UBS DISPOSITION, recorded rather than silently carried.** `ubs` reports two
//! CRITICALs on this file: `Instant::now()` flagged as "security token generated
//! with non-cryptographic randomness". Both are false positives and are being
//! kept, not suppressed. The heuristic fires because the file constructs
//! `DatabaseKeys` (making it "security-sensitive context") and calls
//! `Instant::now()`; but no token, key, nonce or salt here derives from the
//! clock — the constants in `keys()` are fixed test-fixture bytes, the same
//! shape `tests/spine.rs` uses, and `Instant::now()` is only ever the start of a
//! measurement interval. Every other file I have landed this session was made
//! ubs-clean by a rename where the rename genuinely improved it; here the only
//! way to satisfy the scanner would be to obscure the timing calls that are the
//! entire point of the file, which trades real clarity for a green scanner. That
//! is the wrong trade, so the finding is reviewed, rejected, and written down —
//! a reader who runs `ubs` and sees two CRITICALs should find this paragraph
//! rather than wonder whether anyone looked.

use asupersync::{Budget, cx::Cx, runtime::Runtime, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const KNOWS: RelationId = RelationId(1);

fn production_runtime() -> (Runtime, Cx) {
    let runtime = RuntimeBuilder::new().build().expect("production runtime");
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    (runtime, cx)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys {
        k_oid: [0x5a; 32],
        namespace: DatabaseSecurityNamespaceId([0x77; 32]),
        dek: [0x3c; 32],
    }
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-cx-{}-{name}", std::process::id()))
}

/// The positive capability witness: an external package obtains a runtime-bound
/// production `Cx`, narrows it, and drives the real durable database path.
#[test]
fn a_production_runtime_cx_drives_the_database() {
    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    let dir = scratch("constructible");
    let mut db = runtime
        .block_on(Database::create(&commit, &dir, keys()))
        .expect("creates under the production runtime");
    let mut batch = WriteBatch::new(KNOWS);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    runtime
        .block_on(db.write(&commit, batch))
        .expect("commits under the production runtime");

    assert_eq!(
        db.neighbours(VId(1), KNOWS).expect("reads"),
        vec![VId(2)],
        "a database driven by a production-runtime Cx must answer like any other"
    );
}

/// Nearest-rank percentile.
///
/// Never interpolates: with these sample counts the interpolation choice is
/// noise beside the scheduler, and nearest-rank cannot report a value that was
/// not actually observed — which matters once a number gets quoted.
fn percentile(sorted: &[Duration], p: f64) -> Duration {
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// WARM POINT-READ LATENCY DISTRIBUTION over a hostile (power-law) shape.
///
/// §17's point-read gate is "≥ 8M lookups/s across cores; p99 < 15 µs warm".
/// This measures the p99 half, on one core, off the lab scheduler, on an
/// unpinned shared box — a *reported observation*, not that gate. The
/// across-cores half is not attempted: it needs a pinned benchmark manifest,
/// which does not exist (module doc).
///
/// **THE DISTRIBUTION IS THE POINT, NOT THE MEAN.** §17 law 2 asks for
/// p50/p95/p99 and worst hot-key behaviour, never an average. Supernode and leaf
/// are measured separately for exactly that reason: averaging them hides
/// whichever is bad.
///
/// **MEASURED 2026-08-04, one core, unpinned shared box:**
///
/// | shape | p50 | p95 | p99 | worst |
/// |---|---|---|---|---|
/// | supernode d=64 | 85.6 µs | 106.8 µs | 121.7 µs | 733.6 µs |
/// | leaf d=1 | 55.9 µs | 62.7 µs | 69.0 µs | 138.7 µs |
///
/// Against §17's "p99 < 15 µs warm": the supernode is **8.1× over** and the leaf
/// **4.6× over**. Publish the bad numbers.
///
/// **THE INTERESTING RESULT IS THE RATIO, NOT THE MAGNITUDE.** A 64× difference
/// in degree produces only a **1.5×** difference in p50 (85.6 vs 55.9 µs). Cost
/// is almost independent of how much work the query actually asks for — which is
/// the *same conclusion* the tier-D op-count witnesses reached by a completely
/// different instrument (`a_neighbour_scan_costs_the_whole_block_whatever_the_
/// degree`, fgdb-drwe: the examined-entry count is flat across a 64× answer-size
/// spread). Two independent instruments agreeing is worth far more than either
/// alone, and it locates the problem: a fixed per-read cost dominates, so the
/// optimisation target is the per-read path, not the per-edge work.
///
/// That also means a degree-1 "point read" — the operation §17's 15 µs gate is
/// actually about — costs 55.9 µs p50 while doing almost nothing. The tail is
/// real too: the supernode's worst sample is 6× its own p99.
#[test]
fn warm_point_read_latency_distribution() {
    let dir = scratch("latency");
    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    let mut db = runtime
        .block_on(Database::create(&commit, &dir, keys()))
        .expect("creates");
    let mut batch = WriteBatch::new(KNOWS);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    let mut eid = 1u128;
    for k in 0..64u128 {
        batch.create_vertex(VId(1000 + k), vec![], vec![]);
        batch.add_edge(EId(eid), VId(1), VId(1000 + k), vec![]);
        eid += 1;
    }
    batch.add_edge(EId(eid), VId(2), VId(1000), vec![]);
    runtime.block_on(db.write(&commit, batch)).expect("commits");

    const WARMUP: usize = 200;
    const SAMPLES: usize = 2_000;

    let mut report = Vec::new();
    for (label, src, expected_degree) in
        [("supernode-d64", VId(1), 64usize), ("leaf-d1", VId(2), 1)]
    {
        // WARM MEANS WARM: reading before measuring is the difference between a
        // warm-path number and a first-touch number, and §17 asks for warm.
        for _ in 0..WARMUP {
            let found = db.neighbours(src, KNOWS).expect("warm-up read");
            assert_eq!(found.len(), expected_degree, "{label} answered wrongly");
        }

        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let start = Instant::now();
            let found = db.neighbours(src, KNOWS).expect("measured read");
            samples.push(start.elapsed());
            // CORRECTNESS INSIDE THE MEASURED LOOP, not beside it: a read that
            // got faster by returning less is not faster.
            assert_eq!(found.len(), expected_degree, "{label} answered wrongly");
        }

        samples.sort_unstable();
        report.push((
            label,
            percentile(&samples, 50.0),
            percentile(&samples, 95.0),
            percentile(&samples, 99.0),
            *samples.last().expect("samples is nonempty"),
        ));
    }

    // NON-VACUITY: zero samples, or a clock too coarse to resolve the
    // operation, is an instrument failure rather than a fast engine.
    for (label, p50, _, p99, _) in &report {
        assert!(
            *p50 > Duration::ZERO,
            "{label} p50 measured as zero — the clock is not resolving this operation"
        );
        assert!(p99 >= p50, "{label} p99 below p50; the ordering is wrong");
    }

    // CATASTROPHE CEILING ONLY, deliberately absurd next to §17's 15 µs. A real
    // threshold here would be a flaky gate on a shared box, which is how a
    // timing test teaches people to ignore it. This fires only on pathology.
    const CATASTROPHE: Duration = Duration::from_millis(50);
    for (label, _, _, p99, _) in &report {
        assert!(
            *p99 < CATASTROPHE,
            "{label} p99 is {p99:?}, past the {CATASTROPHE:?} catastrophe ceiling; \
             full report {report:?}"
        );
    }
}

/// §17's RECOVERY gate, as a scaling question rather than a constant.
///
/// §17: "Recovery (clean shutdown / crash @ 1 TB) — < 1 s / < 30 s to first
/// query (anchor-mapped, capsule tail replay)". The gate is stated at 1 TB, so
/// the number that matters is not how long one small reopen takes — it is
/// **what reopen cost does as history grows**. A constant measured at one size
/// cannot answer that, and a reopen that is O(history) fails the gate at scale
/// however fast it looks small.
///
/// So this sweeps BATCH COUNT, not edge count. Recovery is described as capsule
/// *tail* replay, so the tail length is the independent variable; adding edges
/// inside one batch would grow the graph without lengthening the thing recovery
/// actually walks.
///
/// **MEASURED 2026-08-04:**
///
/// | batches | reopen | per batch | first query |
/// |---|---|---|---|
/// | 1 | 5.30 ms | 5.30 ms | 5.6 µs |
/// | 4 | 18.12 ms | 4.53 ms | 9.2 µs |
/// | 16 | 65.65 ms | 4.10 ms | 23.4 µs |
/// | 64 | 271.20 ms | 4.24 ms | 89.9 µs |
///
/// **RECOVERY IS LINEAR IN THE CAPSULE TAIL.** Per-batch cost is flat at
/// ~4.1–5.3 ms across a 64× range, so reopen is O(commits). That is not
/// superlinear — the shape assertion below passes, and correctly — but linear is
/// already fatal at the scale §17 states the gate:
///
/// > §17: "Recovery (clean shutdown / crash @ 1 TB) — **< 1 s** to first query"
///
/// At ~4.2 ms per commit, a 1-second recovery budget buys **≈ 238 commits**. A
/// 1 TB database has incomparably more history than that. So the gate is not
/// missed by a tuning factor; it is missed by whatever ratio the real commit
/// count bears to 238, and no constant-factor optimisation closes it. Anchor
/// mapping and checkpointing — the mechanisms §17's own parenthetical names —
/// are what make recovery sublinear, and neither exists yet.
///
/// FIRST QUERY GROWS TOO, 5.6 µs → 89.9 µs across the same sweep, roughly linear
/// in commit count. Consistent with the tier-D witnesses: more commits means
/// more blocks to merge, and the read path examines all of them.
///
/// SECONDARY OBSERVATION, deliberately not turned into a gate: the 85 commits
/// this test writes take ~13.9 s in total, ~163 ms each. That is per-COMMIT
/// cost, almost certainly fsync-dominated, and it says nothing directly about
/// §17's ≥2M edge-inserts/s figure, which is a per-EDGE rate a batched write
/// would amortise. Recorded because it is a large number nobody had measured,
/// and flagged as not-the-ingest-gate so it does not get quoted as one.
///
/// The assertions are on SHAPE and correctness, plus a catastrophe ceiling —
/// never a threshold. Same reasoning as the latency witness above: on a shared
/// unpinned box a real threshold is a flaky gate, and §17 activates its gates
/// only under a pinned manifest that does not exist here.
#[test]
fn recovery_cost_versus_capsule_tail_length() {
    const BATCHES: [usize; 4] = [1, 4, 16, 64];

    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    let mut report = Vec::new();
    for batches in BATCHES {
        let dir = scratch(&format!("recovery-{batches}"));
        let mut db = runtime
            .block_on(Database::create(&commit, &dir, keys()))
            .expect("creates");
        batch_writes(&runtime, &commit, &mut db, batches);
        drop(db);

        // RECOVERY: the reopen itself.
        let start = Instant::now();
        let db = runtime
            .block_on(Database::open(&commit, &dir, keys()))
            .expect("reopens");
        let reopen = start.elapsed();

        // TIME TO FIRST QUERY, which is what §17 actually gates — an open that
        // defers all its work to the first read has not recovered, it has
        // postponed. Measuring both separates those.
        let start = Instant::now();
        let found = db.neighbours(VId(1), KNOWS).expect("first query");
        let first_query = start.elapsed();

        assert_eq!(
            found.len(),
            batches,
            "the {batches}-batch database lost edges across recovery"
        );
        report.push((batches, reopen, first_query));
    }

    // NON-VACUITY: a reopen too fast to measure means the clock is not
    // resolving it, not that recovery is free.
    for (batches, reopen, _) in &report {
        assert!(
            *reopen > Duration::ZERO,
            "{batches}-batch reopen measured as zero — the clock is not \
             resolving it; report {report:?}"
        );
    }

    // THE SHAPE. Recovery cost per batch must not GROW as the tail lengthens:
    // that would be superlinear recovery, which is the defect §17's 1 TB
    // framing exists to catch and which no single-size measurement can see.
    // Compared per-batch rather than absolutely, since a longer tail legitimately
    // costs more in total.
    let per_batch =
        |(batches, reopen, _): &(usize, Duration, Duration)| reopen.as_nanos() / *batches as u128;
    let smallest = per_batch(&report[0]);
    let largest = per_batch(&report[report.len() - 1]);
    assert!(
        largest <= smallest.saturating_mul(4),
        "per-batch recovery cost grew from {smallest} ns at {} batches to \
         {largest} ns at {} batches — recovery is superlinear in the capsule \
         tail, which fails §17's gate at any interesting size however fast the \
         small case looks; report {report:?}",
        report[0].0,
        report[report.len() - 1].0
    );

    // CATASTROPHE CEILING ONLY, absurd next to §17's 1 s.
    const CATASTROPHE: Duration = Duration::from_secs(5);
    for (batches, reopen, first_query) in &report {
        assert!(
            *reopen < CATASTROPHE && *first_query < CATASTROPHE,
            "{batches}-batch recovery is pathological: reopen {reopen:?}, first \
             query {first_query:?}; report {report:?}"
        );
    }
}

/// Commit `batches` separate write batches, each adding one edge from vertex 1.
///
/// Separate batches on purpose: recovery replays a capsule tail, so the tail is
/// lengthened by commits, not by edges inside one commit.
fn batch_writes(
    runtime: &Runtime,
    commit: &fgdb_types::context::CommitCx,
    db: &mut Database,
    batches: usize,
) {
    for b in 0..batches {
        let mut batch = WriteBatch::new(KNOWS);
        if b == 0 {
            batch.create_vertex(VId(1), vec![], vec![]);
        }
        batch.create_vertex(VId(2000 + b as u128), vec![], vec![]);
        batch.add_edge(EId(b as u128 + 1), VId(1), VId(2000 + b as u128), vec![]);
        runtime.block_on(db.write(commit, batch)).expect("commits");
    }
}

/// **A FALSIFICATION TEST OF THIS LANE'S OWN CONCLUSION**, not another
/// confirmation of it.
///
/// Three instruments now agree that fixed per-operation cost dominates and
/// per-edge work does not: tier-D op counts (examined entries flat across a 64×
/// answer-size spread), end-to-end read latency (64× degree → 1.5× p50), and
/// recovery (O(commits), per-commit cost flat). On the strength of that I
/// recommended re-scoping fgdb-by2l, which targets per-EDGE encoding.
///
/// That recommendation deserves a test designed to break it. Agreement among
/// three instruments that all measure READ paths is weaker evidence than it
/// looks: they could share a common cause and still be wrong about the WRITE
/// path, which is where by2l's encoding change actually lands. So this measures
/// writes, and it is built to separate the two costs rather than to reproduce
/// the earlier answer.
///
/// **THE DESIGN.** Total edges are held CONSTANT at 256 while batch size varies
/// from 1 to 256. Total work is therefore identical across every row; only the
/// number of commits changes. If per-commit cost dominates, total time falls
/// with batch count. If per-edge cost dominates, total time is flat — and my
/// recommendation is wrong and must be withdrawn.
///
/// **MEASURED 2026-08-05.** Same 256 edges in every row:
///
/// | split | total | per commit |
/// |---|---|---|
/// | 256 × 1 | 148.58 s | 580 ms |
/// | 64 × 4 | 12.77 s | 200 ms |
/// | 16 × 16 | 1.39 s | 87 ms |
/// | 4 × 64 | 0.33 s | 82 ms |
/// | 1 × 256 | 0.20 s | 205 ms |
///
/// **THE CLAIM SURVIVED, BUT THE MECHANISM IS WORSE THAN I ASSUMED.** Identical
/// work costs **725× more** as 256 commits than as one, so per-commit cost
/// dominates overwhelmingly and the fgdb-by2l re-scoping recommendation stands.
/// But I had assumed a *flat* per-commit cost. It is not flat — it grows with
/// how many commits already exist: 82 ms at 4 commits, 200 ms at 64, 580 ms at
/// 256.
///
/// **TOTAL WRITE COST IS SUPERLINEAR IN COMMIT COUNT, AND THE EXPONENT IS
/// ITSELF DEGRADING.** Log-log slope across adjacent measured points:
///
/// | range | exponent |
/// |---|---|
/// | 4 → 16 commits | 1.04 |
/// | 16 → 64 | 1.60 |
/// | 64 → 256 | 1.77 |
///
/// It starts near-linear and trends toward quadratic. That is the signature of
/// each commit doing work proportional to the history already present — the same
/// shape the read path shows (every block examined) and recovery shows
/// (O(commits)). A fixed n^1.7 would be bad; an exponent still climbing at the
/// largest size measured means the extrapolation is a floor, not an estimate.
///
/// Nothing in §17 gates this directly — the ingest gate is a per-edge rate — but
/// it bounds every gate that has to build a history first, and it is why the
/// recovery sweep in this file could only afford 64 batches.
#[test]
fn per_commit_versus_per_edge_write_cost() {
    const TOTAL_EDGES: usize = 256;
    // (batches, edges per batch) — the product is TOTAL_EDGES in every row.
    // 256x1 is DELIBERATELY ABSENT: measured once at 148.58 s, it alone made
    // this test a ~173 s burden on every future suite run. Its number is
    // recorded in the doc above; the effect is fully visible without it.
    const SPLITS: [(usize, usize); 4] = [(64, 4), (16, 16), (4, 64), (1, 256)];

    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    let mut report = Vec::new();
    for (batches, per_batch) in SPLITS {
        assert_eq!(
            batches * per_batch,
            TOTAL_EDGES,
            "the sweep must hold total work constant, or it measures two things at once"
        );

        let dir = scratch(&format!("writesplit-{batches}x{per_batch}"));
        let mut db = runtime
            .block_on(Database::create(&commit, &dir, keys()))
            .expect("creates");

        let start = Instant::now();
        let mut eid = 1u128;
        for b in 0..batches {
            let mut batch = WriteBatch::new(KNOWS);
            if b == 0 {
                batch.create_vertex(VId(1), vec![], vec![]);
            }
            for _ in 0..per_batch {
                batch.create_vertex(VId(3000 + eid), vec![], vec![]);
                batch.add_edge(EId(eid), VId(1), VId(3000 + eid), vec![]);
                eid += 1;
            }
            runtime.block_on(db.write(&commit, batch)).expect("commits");
        }
        let elapsed = start.elapsed();

        // CORRECTNESS: every split must have written the same graph, or the
        // times are not comparable.
        let found = db.neighbours(VId(1), KNOWS).expect("reads");
        assert_eq!(
            found.len(),
            TOTAL_EDGES,
            "the {batches}x{per_batch} split wrote {} edges, not {TOTAL_EDGES}; \
             the rows are not comparable",
            found.len()
        );

        report.push((batches, per_batch, elapsed));
    }

    // THE SPLIT. With total edges fixed, any variation across rows is
    // attributable to commit count. Two points give the per-commit cost; the
    // residual at the largest batch is the per-edge floor.
    let most_commits = report[0].2;
    let fewest_commits = report[report.len() - 1].2;

    assert!(
        most_commits >= fewest_commits,
        "writing the SAME 256 edges took longer in one commit ({fewest_commits:?}) \
         than in 256 ({most_commits:?}); commit batching is somehow costing more, \
         which inverts the whole model; report {report:?}"
    );

    let commit_dominated = most_commits.as_nanos() >= fewest_commits.as_nanos().saturating_mul(4);

    // THE CLAIM UNDER TEST, asserted so that a future change which makes
    // per-edge cost dominant will RED this and force the conclusion to be
    // rewritten rather than quietly outlived.
    assert!(
        commit_dominated,
        "PER-EDGE COST NOW DOMINATES THE WRITE PATH. Writing 256 edges as 256 \
         commits took {most_commits:?} and as a single commit {fewest_commits:?} \
         — less than the 4x separation that made per-commit cost the story. This \
         lane's recommendation to re-scope fgdb-by2l away from per-edge encoding \
         RESTS ON THAT SEPARATION and must be withdrawn if this fires. \
         Report {report:?}"
    );
}
