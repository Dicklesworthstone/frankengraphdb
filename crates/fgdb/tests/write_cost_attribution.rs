//! **Differential attribution of the O(history) per-commit write cost — now
//! the regression lock that keeps it fixed**
//! (bead `fgdb-fujt`; evidence chain 2543473 → b0ffc45 → ffe05f6).
//!
//! HISTORY OF THIS FILE'S VERDICTS. As landed (ffe05f6) the three verdicts
//! CONVICTED `fgdb::rebuild()`'s capsule re-read loop: marginal write
//! 81→183→445 ms over 8→32→96 commits of history, 95% of it re-reading and
//! re-decoding every historical capsule, with the raw commit protocol flat at
//! ~17 ms. The fujt fix (incremental snapshot: `Database` retains the fold and
//! publishes from a clone; `rebuild()` unchanged as the open/recovery path)
//! made the marginal write flat — 47.7→46.9→49.4 ms measured on the same
//! instruments — so verdicts 1 and 2 are now INVERTED: they assert the
//! marginal write stays bounded and stays well under the full-rebuild
//! replica, and they RED if per-commit history-proportional work returns.
//! The instruments and the report format are unchanged from the attribution
//! era, so the numbers stay comparable across the flip.
//!
//! The sweep in `cx_probe.rs` proved per-commit cost GROWS with the number of
//! commits already present (82 ms at 4 → 580 ms at 256, log-log exponent still
//! climbing at the largest size). It did not say WHERE. This file splits the
//! marginal commit into its components and measures each at several history
//! sizes, so the O(history) term is attributed to a specific code path by
//! measurement rather than by reading.
//!
//! **THE CANDIDATE, from reading `fgdb::Database::write_with_crash`:** after
//! the two-fsync durable commit, `rebuild()` reconstructs the ENTIRE derived
//! partition from scratch — for every historical entry it re-reads the capsule
//! (RaptorQ decode + AEAD open + decompress), re-verifies the template digest
//! (FG-INV-09), re-decodes the template, re-folds every row, then republishes
//! every block and reopens the root. Reading names the suspect; the numbers
//! below convict or acquit it.
//!
//! **THE DESIGN.** Three instruments per history size N ∈ {8, 32, 96}:
//!
//! 1. **Marginal write** — one `Database::write` of one edge on top of N
//!    commits. This is the quantity the sweep showed growing.
//! 2. **Rebuild replica** — the same loop `rebuild()` runs, reimplemented here
//!    against the same directory through public APIs, with each stage timed
//!    separately: capsule read+recover, digest verify, template decode, row
//!    fold, block publish+root reopen.
//! 3. **Chronicle-only control** — a raw `CommitCoordinator::commit` of a
//!    fixed capsule at history N, with no rebuild. If the durable protocol
//!    itself were the O(history) term, THIS would grow; if it is flat, the
//!    attribution excludes the commit protocol.
//!
//! **VERDICT SHAPE.** Attribution holds when: the marginal write grows in both
//! raw time and sentinel-normalized time (mechanism present at this scale);
//! the rebuild replica accounts for the majority of the marginal write at
//! large N (the suspect is the cost); and the chronicle-only control stays
//! flat (the alternative is excluded). Requiring both growth witnesses is
//! deliberate: neighboring load can inflate raw time while a transiently slow
//! early sentinel can inflate only the normalized ratio. Neither alone proves
//! history-proportional work. Per `cx_probe.rs`'s own doctrine these numbers
//! are REPORTED, never promoted to a §17 speed gate, and the assertions are on
//! shape, not speed.
//!
//! **UBS DISPOSITION:** as in `cx_probe.rs`, `Instant::now()` here is a
//! measurement interval start, never key material; `keys()` is the same fixed
//! test fixture every spine test uses.

use asupersync::{Budget, cx::Cx, runtime::Runtime, runtime::RuntimeBuilder};
use fgdb::marker_for_capsule;
use fgdb::{Database, DatabaseKeys, WriteBatch, prepare_capsule, template_digest};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{CoordinateEntry, DeltaRow, LogicalDeltaTemplate, RelationId, SchemaEpoch};
use fgdb_strata::store::BlockStore;
use fgdb_strata::writer::BlockWriter;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::{BranchId, DatabaseSecurityNamespaceId, GraphId, ObjectId};
use fgdb_types::{EId, VId};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn production_runtime() -> (Runtime, Cx) {
    let runtime = RuntimeBuilder::new().build().expect("production runtime");
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    (runtime, cx)
}

const KNOWS: RelationId = RelationId(1);
/// The spine's partition coordinates (`fgdb::lib` GRAPH/BRANCH/PARTITION are
/// private consts; these must match them or the replica folds nothing).
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const PARTITION: u64 = 0;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-fujt-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// One constant-work write-through in the subject directory. The floor keeps
/// clock quantization from manufacturing an enormous ratio on a suspiciously
/// fast filesystem response.
fn fsync_sentinel(path: &Path) -> Duration {
    use std::io::Write as _;

    let payload = [0u8; 4096];
    let start = Instant::now();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .expect("sentinel opens");
    file.write_all(&payload).expect("sentinel writes");
    file.sync_all().expect("sentinel syncs");
    start.elapsed().max(Duration::from_micros(50))
}

fn median_duration<const N: usize>(mut samples: [Duration; N]) -> Duration {
    assert!(N > 0, "a sentinel median needs at least one sample");
    samples.sort_unstable();
    samples[N / 2]
}

/// Compare work after dividing out adjacent constant-work latency, without a
/// floating-point verdict. This is the exact predicate licensed by
/// `sentinel_normalization_rejects_history_growth_without_false_redding_load`.
fn normalized_growth_within(
    small_work: Duration,
    small_sentinel: Duration,
    large_work: Duration,
    large_sentinel: Duration,
    max_factor: u128,
) -> bool {
    let observed = large_work
        .as_nanos()
        .checked_mul(small_sentinel.as_nanos())
        .expect("measured duration cross-product fits u128");
    let allowed = small_work
        .as_nanos()
        .checked_mul(large_sentinel.as_nanos())
        .and_then(|value| value.checked_mul(max_factor))
        .expect("measured duration budget fits u128");
    observed <= allowed
}

/// A history-growth conviction needs both witnesses: raw work exceeds the
/// bound, and dividing by the adjacent load control still exceeds it. If
/// either witness stays bounded, the run did not attribute growth to history.
fn history_growth_within(
    small_work: Duration,
    small_sentinel: Duration,
    large_work: Duration,
    large_sentinel: Duration,
    max_factor: u128,
) -> bool {
    let raw_allowed = small_work
        .as_nanos()
        .checked_mul(max_factor)
        .expect("measured raw duration budget fits u128");
    large_work.as_nanos() <= raw_allowed
        || normalized_growth_within(
            small_work,
            small_sentinel,
            large_work,
            large_sentinel,
            max_factor,
        )
}

/// Require the normalized reference cost to be at least `factor` times the
/// normalized subject cost. This is Verdict 2's load-independent form.
fn normalized_reference_dominates(
    subject: Duration,
    subject_sentinel: Duration,
    reference: Duration,
    reference_sentinel: Duration,
    factor: u128,
) -> bool {
    let required = subject
        .as_nanos()
        .checked_mul(reference_sentinel.as_nanos())
        .and_then(|value| value.checked_mul(factor))
        .expect("measured duration requirement fits u128");
    let observed = reference
        .as_nanos()
        .checked_mul(subject_sentinel.as_nanos())
        .expect("measured duration cross-product fits u128");
    required <= observed
}

#[test]
fn sentinel_normalization_rejects_history_growth_without_false_redding_load() {
    // Fivefold raw inflation caused entirely by fivefold neighboring I/O load
    // is the same amount of attributed work and must remain green.
    assert!(history_growth_within(
        Duration::from_millis(80),
        Duration::from_millis(10),
        Duration::from_millis(400),
        Duration::from_millis(50),
        2,
    ));

    // The same raw inflation with an unchanged sentinel is real subject growth
    // and must still red. This prevents normalization from becoming a waiver.
    assert!(!history_growth_within(
        Duration::from_millis(80),
        Duration::from_millis(10),
        Duration::from_millis(400),
        Duration::from_millis(10),
        2,
    ));

    // A transiently slow early sentinel can make the normalized ratio alone
    // exceed 2x even while the subject remains flat. Raw boundedness prevents
    // that control noise from being mislabeled as an O(history) regression.
    assert!(history_growth_within(
        Duration::from_millis(80),
        Duration::from_millis(10),
        Duration::from_millis(100),
        Duration::from_millis(5),
        2,
    ));

    // Both witnesses exceeding the bound remains red; the raw companion is
    // an attribution requirement, not a waiver for real growth.
    assert!(!history_growth_within(
        Duration::from_millis(80),
        Duration::from_millis(10),
        Duration::from_millis(200),
        Duration::from_millis(8),
        2,
    ));

    // Equal load inflation on the subject and reference cannot change whether
    // the rebuild is at least twice as expensive as the live write.
    assert!(normalized_reference_dominates(
        Duration::from_millis(100),
        Duration::from_millis(10),
        Duration::from_millis(200),
        Duration::from_millis(10),
        2,
    ));
    assert!(normalized_reference_dominates(
        Duration::from_millis(500),
        Duration::from_millis(50),
        Duration::from_millis(1000),
        Duration::from_millis(50),
        2,
    ));

    // A genuinely under-dominant rebuild remains red after normalization.
    assert!(!normalized_reference_dominates(
        Duration::from_millis(100),
        Duration::from_millis(10),
        Duration::from_millis(150),
        Duration::from_millis(10),
        2,
    ));
}

/// One edge per commit, the sweep's history-building shape.
fn build_history(
    runtime: &Runtime,
    commit: &fgdb_types::context::CommitCx,
    db: &mut Database,
    commits: usize,
) {
    for b in 0..commits {
        let mut batch = WriteBatch::new(KNOWS);
        if b == 0 {
            batch.create_vertex(VId(1), vec![], vec![]);
        }
        batch.create_vertex(VId(2000 + b as u128), vec![], vec![]);
        batch.add_edge(EId(b as u128 + 1), VId(1), VId(2000 + b as u128), vec![]);
        runtime
            .block_on(db.write(commit, batch))
            .expect("history commit");
    }
}

/// Timed stages of one full derived-partition rebuild, replicating
/// `fgdb::rebuild` through public APIs against an already-built directory.
#[derive(Debug, Default, Clone, Copy)]
struct ReplicaStages {
    open_recover_chain: Duration,
    capsule_read_recover: Duration,
    digest_verify: Duration,
    template_decode: Duration,
    row_fold: Duration,
    publish_and_reopen: Duration,
}

impl ReplicaStages {
    fn total(&self) -> Duration {
        self.open_recover_chain
            + self.capsule_read_recover
            + self.digest_verify
            + self.template_decode
            + self.row_fold
            + self.publish_and_reopen
    }
}

/// Runs the rebuild replica against `dir`, which must contain a database with
/// no live writer (drop the `Database` first — the coordinator lease is the
/// sole-writer authority and `open` would otherwise refuse).
fn rebuild_replica(dir: &PathBuf) -> ReplicaStages {
    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();
    let keys = keys();
    let mut stages = ReplicaStages::default();

    let start = Instant::now();
    let coordinator = runtime
        .block_on(CommitCoordinator::open(&commit, dir, capsule_keys(&keys)))
        .expect("replica coordinator opens");
    stages.open_recover_chain = start.elapsed();

    let store = BlockStore::open(&commit, dir, keys.shared_k_oid(), keys.namespace)
        .expect("replica store opens");

    let entries: Vec<_> = coordinator.chain().entries().to_vec();

    let start = Instant::now();
    let mut plaintexts = Vec::with_capacity(entries.len());
    for entry in &entries {
        let fgdb_chronicle::marker::EffectSource::Local { capsule_ref, .. } =
            &entry.marker.effect_source;
        plaintexts.push((
            entry.marker.commit_seq,
            runtime
                .block_on(coordinator.read_capsule(&commit, *capsule_ref, &mut Vec::new()))
                .expect("replica capsule read"),
        ));
    }
    stages.capsule_read_recover = start.elapsed();

    let start = Instant::now();
    for (_, bytes) in &plaintexts {
        // The digest is recomputed exactly as rebuild does for FG-INV-09; the
        // replica discards the comparison because the subject directory was
        // written moments ago by the same process.
        std::hint::black_box(template_digest(bytes));
    }
    stages.digest_verify = start.elapsed();

    let start = Instant::now();
    let mut templates = Vec::with_capacity(plaintexts.len());
    for (seq, bytes) in &plaintexts {
        templates.push((
            *seq,
            LogicalDeltaTemplate::decode_canonical(bytes).expect("replica template decode"),
        ));
    }
    stages.template_decode = start.elapsed();

    let start = Instant::now();
    let mut writer = BlockWriter::new(GRAPH, BRANCH, PARTITION);
    let mut frontier = fgdb_types::CommitSeq(0);
    for (seq, template) in &templates {
        frontier = fgdb_types::CommitSeq(*seq);
        for coordinate in template.coordinate_entries() {
            if (coordinate.graph, coordinate.branch) != (GRAPH, BRANCH) {
                continue;
            }
            for row in &coordinate.rows {
                writer
                    .apply(
                        (keys.k_oid(), keys.namespace),
                        fgdb_types::CommitSeq(*seq),
                        row,
                    )
                    .expect("replica fold");
            }
        }
    }
    stages.row_fold = start.elapsed();

    let start = Instant::now();
    let (root, blocks, patches) = writer
        .publish((keys.k_oid(), keys.namespace), frontier)
        .expect("replica publish");
    for block in &blocks {
        store.put(&commit, &block.bytes).expect("replica block put");
    }
    for patch in &patches {
        store
            .put_patch(&commit, &patch.bytes)
            .expect("replica patch put");
    }
    let root_id = store.put_root(&commit, &root).expect("replica root put");
    store.reopen(&commit, root_id).expect("replica reopen");
    stages.publish_and_reopen = start.elapsed();

    stages
}

/// A minimal but well-formed template whose capsule the chronicle-only
/// control commits repeatedly. The semantics oid is arbitrary — the control
/// never rebuilds, so nothing downstream interprets it.
fn control_capsule_bytes(round: u64) -> (Vec<u8>, fgdb::PreparedCapsule) {
    let template = LogicalDeltaTemplate::build(
        ObjectId([0xAB; 32]),
        [0u8; 32],
        vec![CoordinateEntry {
            graph: GRAPH,
            branch: BRANCH,
            relation: KNOWS,
            schema_epoch: SchemaEpoch(0),
            schema_transition: None,
            rows: vec![DeltaRow::CreateVertex {
                vid: VId(u128::from(round) + 10_000),
                birth_ordinal: round,
                labels: vec![],
                props: vec![],
                valid_time: None,
            }],
        }],
    )
    .expect("control template");
    let keys = keys();
    let capsule =
        prepare_capsule(keys.k_oid(), keys.namespace, &template).expect("control capsule");
    (capsule.bytes.clone(), capsule)
}

#[test]
fn the_o_history_term_lives_in_rebuild_not_in_the_commit_protocol() {
    const HISTORY_POINTS: [usize; 3] = [8, 32, 96];

    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    // ---- Instrument 1 + 2: marginal write and rebuild replica per history size.
    let mut marginal = Vec::new();
    let mut replicas: Vec<(usize, ReplicaStages, Duration)> = Vec::new();
    for &n in &HISTORY_POINTS {
        let dir = scratch(&format!("hist-{n}"));
        let mut db = runtime
            .block_on(Database::create(&commit, &dir, keys()))
            .expect("creates");
        build_history(&runtime, &commit, &mut db, n);

        let sentinel_path = dir.join("marginal-write-sentinel");
        let sentinel_before = fsync_sentinel(&sentinel_path);
        let start = Instant::now();
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(900_000), vec![], vec![]);
        batch.add_edge(EId(900_000), VId(1), VId(900_000), vec![]);
        runtime
            .block_on(db.write(&commit, batch))
            .expect("marginal commit");
        let t_marginal = start.elapsed();
        let sentinel_after_1 = fsync_sentinel(&sentinel_path);
        let sentinel_after_2 = fsync_sentinel(&sentinel_path);
        let sentinel = median_duration([sentinel_before, sentinel_after_1, sentinel_after_2]);
        marginal.push((n, t_marginal, sentinel));

        drop(db);
        let replica_sentinel_before = fsync_sentinel(&sentinel_path);
        let replica = rebuild_replica(&dir);
        let replica_sentinel_after_1 = fsync_sentinel(&sentinel_path);
        let replica_sentinel_after_2 = fsync_sentinel(&sentinel_path);
        let replica_sentinel = median_duration([
            replica_sentinel_before,
            replica_sentinel_after_1,
            replica_sentinel_after_2,
        ]);
        replicas.push((n, replica, replica_sentinel));
    }

    // ---- Instrument 3: chronicle-only control — raw commits, no rebuild.
    let control_dir = scratch("chronicle-only");
    let control_keys = keys();
    let mut coordinator = runtime
        .block_on(CommitCoordinator::open(
            &commit,
            &control_dir,
            capsule_keys(&control_keys),
        ))
        .expect("control coordinator");
    // Each commit is timed against an ADJACENT constant-work sentinel — a
    // 4 KiB write + fsync in the same directory — and the verdict is over
    // the RATIO. Machine load (this box runs many panes, fgdb-j57r measured
    // three false reds in one session) inflates numerator and denominator
    // together, so a sustained neighbour build no longer reads as an
    // O(history) term; only work that grows with THIS stream's history can
    // move the ratio.
    let sentinel_path = control_dir.join("sentinel");
    let mut control: Vec<(Duration, f64)> = Vec::new();
    for round in 0..HISTORY_POINTS[HISTORY_POINTS.len() - 1] as u64 + 1 {
        let (bytes, capsule) = control_capsule_bytes(round);
        let sentinel = fsync_sentinel(&sentinel_path);
        let start = Instant::now();
        runtime
            .block_on(coordinator.commit(&commit, &bytes, |seq, oid| {
                marker_for_capsule(seq, oid, &capsule, Vec::new())
            }))
            .expect("control commit");
        let elapsed = start.elapsed();
        control.push((elapsed, elapsed.as_secs_f64() / sentinel.as_secs_f64()));
    }

    // ---- REPORT, in full, before any verdict: every number the assertions
    // use must be reconstructable from the test log alone.
    eprintln!("fujt attribution report");
    for ((n, t, sentinel), (_, stages, replica_sentinel)) in marginal.iter().zip(&replicas) {
        eprintln!(
            "  history={n:>3} marginal_write={t:?} sentinel={sentinel:?} \
             marginal/sentinel={:.2} replica_total={:?} \
             replica_sentinel={replica_sentinel:?} replica/sentinel={:.2} \
             [chain_recover={:?} capsule_read={:?} digest={:?} decode={:?} \
             fold={:?} publish_reopen={:?}]",
            t.as_secs_f64() / sentinel.as_secs_f64(),
            stages.total(),
            stages.total().as_secs_f64() / replica_sentinel.as_secs_f64(),
            stages.open_recover_chain,
            stages.capsule_read_recover,
            stages.digest_verify,
            stages.template_decode,
            stages.row_fold,
            stages.publish_and_reopen,
        );
    }
    let median = |window: &[(Duration, f64)]| -> f64 {
        let mut ratios: Vec<f64> = window.iter().map(|(_, ratio)| *ratio).collect();
        ratios.sort_by(|left, right| left.total_cmp(right));
        ratios[ratios.len() / 2]
    };
    let control_early = median(&control[1..9]);
    let control_late = median(&control[control.len() - 8..]);
    eprintln!(
        "  chronicle-only control: early(median commit/sentinel of 8)={control_early:.2} \
         late={control_late:.2} raw_first={:?} raw_last={:?}",
        control[1].0,
        control[control.len() - 1].0,
    );

    // ---- VERDICT 1 (the regression lock this file became once the fujt fix
    // landed): the marginal write is BOUNDED — it must not grow with history.
    // Attribution-era numbers, kept for the record: 81→183→445 ms over
    // 8→32→96 (a 5.5x climb, 95% of it rebuild's capsule re-read loop).
    // Post-fix: 47.7→46.9→49.4 ms, flat. The adjacent same-directory sentinel
    // removes machine-wide I/O inflation, while the raw companion refuses to
    // convict a flat subject merely because the early sentinel was slower.
    // The 2x headroom is unchanged; a real per-commit rebuild exceeds both
    // witnesses by >5x on this sweep.
    let (small_n, small_t, small_sentinel) = marginal[0];
    let (large_n, large_t, large_sentinel) = marginal[marginal.len() - 1];
    assert!(
        history_growth_within(small_t, small_sentinel, large_t, large_sentinel, 2,),
        "THE O(HISTORY) WRITE COST IS BACK: marginal write at {large_n} \
         commits ({large_t:?} / {large_sentinel:?} sentinel) is more than 2x \
         the raw AND sentinel-normalized marginal write at {small_n} commits \
         ({small_t:?} / {small_sentinel:?} sentinel). The incremental snapshot path \
         (fgdb-fujt) bounded this; something reintroduced per-commit work \
         proportional to history. Report above."
    );

    // ---- VERDICT 2: the live write path no longer pays the full rebuild.
    // The replica STILL measures the rebuild (it is the open/recovery path,
    // deliberately untouched), so at the largest history the marginal write
    // must be well under it — inverted from the attribution era, when the
    // replica accounted for ≥ half the marginal write.
    let (_, large_stages, large_replica_sentinel) = replicas[replicas.len() - 1];
    assert!(
        normalized_reference_dominates(
            large_t,
            large_sentinel,
            large_stages.total(),
            large_replica_sentinel,
            2,
        ),
        "the sentinel-normalized marginal write at {large_n} commits \
         ({large_t:?} / {large_sentinel:?}) is not well under the \
         sentinel-normalized full-rebuild replica ({:?} / {:?}) — the write path is paying \
         history-proportional rebuild work again; report above.",
        large_stages.total(),
        large_replica_sentinel,
    );

    // ---- VERDICT 3: the durable commit protocol is NOT the term — raw
    // chronicle commits stay flat as history grows. 4x is the same coarse
    // separation the cx_probe sweep uses, applied to the sentinel-normalized
    // ratio so neighbour load cancels instead of deciding the verdict
    // (fgdb-j57r).
    assert!(
        control_late <= control_early * 4.0,
        "the chronicle-only control GREW with history (early ratio \
         {control_early:.2} → late ratio {control_late:.2}, sentinel-normalized): \
         the durable commit protocol itself carries an O(history) term and \
         the rebuild attribution is incomplete."
    );
}

/// `DatabaseKeys::capsule_keys` is private to the spine; the replica rebuilds
/// the same value from the same public fields and the public capsule vocabulary.
fn capsule_keys(keys: &DatabaseKeys) -> fgdb_chronicle::capsule::CapsuleKeys {
    keys.capsule_keys()
}
