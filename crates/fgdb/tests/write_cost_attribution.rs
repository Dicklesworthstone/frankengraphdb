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
//! **VERDICT SHAPE.** Attribution holds when: the marginal write reproduces
//! the growth (mechanism present at this scale), the rebuild replica accounts
//! for the majority of the marginal write at large N (the suspect is the
//! cost), and the chronicle-only control stays flat (the alternative is
//! excluded). Each is asserted with deliberately coarse ratios — this box runs
//! many panes; per `cx_probe.rs`'s own doctrine these numbers are REPORTED,
//! never promoted to a §17 gate, and the assertions are on shape, not speed.
//!
//! **UBS DISPOSITION:** as in `cx_probe.rs`, `Instant::now()` here is a
//! measurement interval start, never key material; `keys()` is the same fixed
//! test fixture every spine test uses.

use asupersync::cx::Cx;
use fgdb::marker_for_capsule;
use fgdb::{Database, DatabaseKeys, WriteBatch, prepare_capsule, template_digest};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{CoordinateEntry, DeltaRow, LogicalDeltaTemplate, RelationId, SchemaEpoch};
use fgdb_strata::store::BlockStore;
use fgdb_strata::writer::BlockWriter;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::{BranchId, DatabaseSecurityNamespaceId, GraphId, ObjectId};
use fgdb_types::{EId, VId};
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

/// Minimal single-future executor for the non-lab lane — same shape and
/// rationale as `cx_probe.rs`: this file measures OFF the lab runtime, and
/// asupersync's `spawn_blocking` falls back to dedicated threads without a
/// runtime, so the durable-path futures complete and wake this parked thread.
/// The parked-thread round trip is microseconds against the millisecond-scale
/// operations measured here.
fn drive<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut task_cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut task_cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

const KNOWS: RelationId = RelationId(1);
/// The spine's partition coordinates (`fgdb::lib` GRAPH/BRANCH/PARTITION are
/// private consts; these must match them or the replica folds nothing).
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const PARTITION: u64 = 0;

fn keys() -> DatabaseKeys {
    DatabaseKeys {
        k_oid: [0x5a; 32],
        namespace: DatabaseSecurityNamespaceId([0x77; 32]),
        dek: [0x3c; 32],
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-fujt-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// One edge per commit, the sweep's history-building shape.
fn build_history(commit: &fgdb_types::context::CommitCx, db: &mut Database, commits: usize) {
    for b in 0..commits {
        let mut batch = WriteBatch::new(KNOWS);
        if b == 0 {
            batch.create_vertex(VId(1), vec![], vec![]);
        }
        batch.create_vertex(VId(2000 + b as u128), vec![], vec![]);
        batch.add_edge(EId(b as u128 + 1), VId(1), VId(2000 + b as u128), vec![]);
        drive(db.write(commit, batch)).expect("history commit");
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
    let cx = Cx::for_testing();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();
    let keys = keys();
    let mut stages = ReplicaStages::default();

    let start = Instant::now();
    let coordinator = drive(CommitCoordinator::open(&commit, dir, capsule_keys(&keys)))
        .expect("replica coordinator opens");
    stages.open_recover_chain = start.elapsed();

    let store =
        BlockStore::open(&commit, dir, keys.k_oid, keys.namespace).expect("replica store opens");

    let entries: Vec<_> = coordinator.chain().entries().to_vec();

    let start = Instant::now();
    let mut plaintexts = Vec::with_capacity(entries.len());
    for entry in &entries {
        let fgdb_chronicle::marker::EffectSource::Local { capsule_ref, .. } =
            &entry.marker.effect_source;
        plaintexts.push((
            entry.marker.commit_seq,
            drive(coordinator.read_capsule(&commit, *capsule_ref)).expect("replica capsule read"),
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
                        (&keys.k_oid, keys.namespace),
                        fgdb_types::CommitSeq(*seq),
                        row,
                    )
                    .expect("replica fold");
            }
        }
    }
    stages.row_fold = start.elapsed();

    let start = Instant::now();
    let (root, blocks) = writer
        .publish((&keys.k_oid, keys.namespace), frontier)
        .expect("replica publish");
    for block in &blocks {
        store.put(&commit, &block.bytes).expect("replica block put");
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
    let capsule = prepare_capsule(&keys.k_oid, keys.namespace, &template).expect("control capsule");
    (capsule.bytes.clone(), capsule)
}

#[test]
fn the_o_history_term_lives_in_rebuild_not_in_the_commit_protocol() {
    const HISTORY_POINTS: [usize; 3] = [8, 32, 96];

    let cx = Cx::for_testing();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    // ---- Instrument 1 + 2: marginal write and rebuild replica per history size.
    let mut marginal = Vec::new();
    let mut replicas: Vec<(usize, ReplicaStages)> = Vec::new();
    for &n in &HISTORY_POINTS {
        let dir = scratch(&format!("hist-{n}"));
        let mut db = drive(Database::create(&commit, &dir, keys())).expect("creates");
        build_history(&commit, &mut db, n);

        let start = Instant::now();
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(900_000), vec![], vec![]);
        batch.add_edge(EId(900_000), VId(1), VId(900_000), vec![]);
        drive(db.write(&commit, batch)).expect("marginal commit");
        let t_marginal = start.elapsed();
        marginal.push((n, t_marginal));

        drop(db);
        replicas.push((n, rebuild_replica(&dir)));
    }

    // ---- Instrument 3: chronicle-only control — raw commits, no rebuild.
    let control_dir = scratch("chronicle-only");
    let control_keys = keys();
    let mut coordinator = drive(CommitCoordinator::open(
        &commit,
        &control_dir,
        capsule_keys(&control_keys),
    ))
    .expect("control coordinator");
    let mut control = Vec::new();
    for round in 0..HISTORY_POINTS[HISTORY_POINTS.len() - 1] as u64 + 1 {
        let (bytes, capsule) = control_capsule_bytes(round);
        let start = Instant::now();
        drive(coordinator.commit(&commit, &bytes, |seq, oid| {
            marker_for_capsule(seq, oid, &capsule, Vec::new())
        }))
        .expect("control commit");
        control.push(start.elapsed());
    }

    // ---- REPORT, in full, before any verdict: every number the assertions
    // use must be reconstructable from the test log alone.
    eprintln!("fujt attribution report");
    for ((n, t), (_, stages)) in marginal.iter().zip(&replicas) {
        eprintln!(
            "  history={n:>3} marginal_write={t:?} replica_total={:?} \
             [chain_recover={:?} capsule_read={:?} digest={:?} decode={:?} \
             fold={:?} publish_reopen={:?}]",
            stages.total(),
            stages.open_recover_chain,
            stages.capsule_read_recover,
            stages.digest_verify,
            stages.template_decode,
            stages.row_fold,
            stages.publish_and_reopen,
        );
    }
    let control_early: Duration = control[1..9].iter().sum::<Duration>() / 8;
    let control_late: Duration = control[control.len() - 8..].iter().sum::<Duration>() / 8;
    eprintln!(
        "  chronicle-only control: early(mean of 8)={control_early:?} \
         late(mean of 8)={control_late:?}"
    );

    // ---- VERDICT 1 (the regression lock this file became once the fujt fix
    // landed): the marginal write is BOUNDED — it must not grow with history.
    // Attribution-era numbers, kept for the record: 81→183→445 ms over
    // 8→32→96 (a 5.5x climb, 95% of it rebuild's capsule re-read loop).
    // Post-fix: 47.7→46.9→49.4 ms, flat. 2x headroom keeps machine noise from
    // deciding the verdict; a real regression to per-commit rebuild is >5x.
    let (small_n, small_t) = marginal[0];
    let (large_n, large_t) = marginal[marginal.len() - 1];
    assert!(
        large_t.as_nanos() <= small_t.as_nanos().saturating_mul(2),
        "THE O(HISTORY) WRITE COST IS BACK: marginal write at {large_n} \
         commits ({large_t:?}) is more than 2x the marginal write at \
         {small_n} commits ({small_t:?}). The incremental snapshot path \
         (fgdb-fujt) bounded this; something reintroduced per-commit work \
         proportional to history. Report above."
    );

    // ---- VERDICT 2: the live write path no longer pays the full rebuild.
    // The replica STILL measures the rebuild (it is the open/recovery path,
    // deliberately untouched), so at the largest history the marginal write
    // must be well under it — inverted from the attribution era, when the
    // replica accounted for ≥ half the marginal write.
    let (_, large_stages) = replicas[replicas.len() - 1];
    assert!(
        large_t.as_nanos() * 2 <= large_stages.total().as_nanos(),
        "the marginal write at {large_n} commits ({large_t:?}) is not well \
         under the full-rebuild replica ({:?}) — the write path is paying \
         history-proportional rebuild work again; report above.",
        large_stages.total()
    );

    // ---- VERDICT 3: the durable commit protocol is NOT the term — raw
    // chronicle commits stay flat as history grows. 4x is the same coarse
    // separation the cx_probe sweep uses.
    assert!(
        control_late.as_nanos() <= control_early.as_nanos().saturating_mul(4),
        "the chronicle-only control GREW with history (early {control_early:?} \
         → late {control_late:?}): the durable commit protocol itself carries \
         an O(history) term and the rebuild attribution is incomplete."
    );
}

/// `DatabaseKeys::capsule_keys` is private to the spine; the replica rebuilds
/// the same value from the same public fields and the public capsule vocabulary.
fn capsule_keys(keys: &DatabaseKeys) -> fgdb_chronicle::capsule::CapsuleKeys {
    fgdb_chronicle::capsule::CapsuleKeys {
        k_oid: keys.k_oid,
        namespace: keys.namespace,
        dek: keys.dek,
        object_kind: fgdb::CAPSULE_OBJECT_KIND,
        profile: fgdb_chronicle::capsule::CapsuleProfile::balanced(),
    }
}
