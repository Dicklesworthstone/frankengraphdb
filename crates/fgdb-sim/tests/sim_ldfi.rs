//! The LDFI target registry's honesty (plan §15.1 line 1132, bead fgdb-verif-sim-q97e).
//!
//! The registry's whole purpose is to keep the coverage denominator equal to
//! the plan's rather than to what we happen to have built, so the tests are
//! aimed at that and not at the table's shape:
//!
//! * `every_row_quotes_a_phrase_that_appears_in_the_plan_line` — a row nobody
//!   can find in line 1132 is invented, and inventing rows inflates the
//!   denominator just as omitting them deflates it;
//! * `the_coverage_gap_is_reported_not_hidden` — the gap must be non-zero and
//!   stated, because at this HEAD it emphatically is;
//! * `unreachable_targets_name_an_owning_bead` — an unreachable row without an
//!   owner is a permanent silent zero.

use fgdb_sim::ldfi::{
    Reachability, TARGETS, coverage_statement, reachable_count, unreachable_count,
};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Plan line 1132 — the LDFI target sentence. 1-based, as cited.
const TARGET_LINE: usize = 1132;

fn plan_line() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/<crate> has a repo root")
        .join("COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md");
    let plan = std::fs::read_to_string(path).expect("plan is readable");
    plan.lines()
        .nth(TARGET_LINE - 1)
        .expect("plan has line 1132")
        .to_ascii_lowercase()
}

#[test]
fn every_row_quotes_a_phrase_that_appears_in_the_plan_line() {
    let line = plan_line();

    // The anchor first: if line 1132 stops being the LDFI sentence, every
    // assertion below is meaningless and this is what says so.
    for marker in ["lineage-driven fault injection", "d1/d2", "raft"] {
        assert!(
            line.contains(marker),
            "plan line {TARGET_LINE} is not the LDFI target sentence (missing {marker:?})"
        );
    }

    for target in TARGETS {
        // A source phrase may be an ellipsis-joined excerpt ("key ... zero"),
        // so each non-elided fragment must appear rather than the whole string.
        for fragment in target.source_phrase.to_ascii_lowercase().split(" ... ") {
            assert!(
                line.contains(fragment.trim()),
                "target {:?} quotes {fragment:?}, which is not in plan line {TARGET_LINE}",
                target.id
            );
        }
    }
}

#[test]
fn ids_are_unique() {
    let ids: BTreeSet<&str> = TARGETS.iter().map(|target| target.id).collect();
    assert_eq!(
        ids.len(),
        TARGETS.len(),
        "duplicate LDFI target id inflates the denominator"
    );
}

#[test]
fn unreachable_targets_name_an_owning_bead() {
    for target in TARGETS {
        if let Reachability::NotYetBuilt { bead } = target.reachability {
            assert!(
                bead.starts_with("fgdb-"),
                "target {:?} is unreachable with no owning bead: {bead:?}",
                target.id
            );
        }
    }
}

/// THE TEST THE REGISTRY EXISTS FOR. Coverage is reported against the plan's
/// denominator, and the gap is a number rather than an omission.
#[test]
fn the_coverage_gap_is_reported_not_hidden() {
    assert_eq!(
        reachable_count() + unreachable_count(),
        TARGETS.len(),
        "the counts do not partition the table; coverage arithmetic would be wrong"
    );

    // Both sides non-zero, which is the honest state at this HEAD and also the
    // non-vacuity control: with no reachable targets the registry would be
    // aspirational, and with none unreachable it would be lying.
    assert!(
        reachable_count() > 0,
        "no target is reachable; the lab VFS faults D1/D2 writes and syncs today"
    );
    assert!(
        unreachable_count() > 0,
        "every declared target is reachable, which at this HEAD would mean the \
         denominator was quietly redefined to what we built"
    );

    let statement = coverage_statement();
    assert!(
        statement.contains(&TARGETS.len().to_string()),
        "the coverage statement must name the plan's denominator: {statement}"
    );
    assert!(
        statement.contains(&unreachable_count().to_string()),
        "the coverage statement must name the gap: {statement}"
    );
}

#[test]
fn reachable_targets_are_exactly_the_witnessed_ones() {
    // Reachability is a CLAIM that the lab VFS can fault the target, and a
    // claim needs a witness — a test in this repository that actually injects
    // there. The allowlist is exact on purpose: flipping a row to reachable
    // must arrive together with its witness, or this test names the overclaim.
    //
    //   d1/d2 file writes and syncs — witnessed by the FaultVfs section of
    //     `durability_semantics_e2e.rs` (fsync lies, interior tear, ENOSPC,
    //     bit flip, all through the real commit path);
    //   dual-root ordered + physical boundaries — witnessed below in this
    //     file (`a_lying_publish_sync_*`, `enospc_refuses_the_publish_*`);
    //   directory-sync — witnessed by the dirent-durability section of
    //     `lab_vfs.rs` (fgdb-3a3u: a lying directory sync settles nothing
    //     and the armed loss takes the name at crash; the honest control
    //     keeps it);
    //   dual-root certificate + external-CAS boundaries — witnessed below in
    //     this file (fgdb-1dgm: `damaged_publish_bytes_mint_no_certificate_*`,
    //     `a_stale_forked_or_absent_continuity_head_*`).
    let witnessed: BTreeSet<&str> = [
        "d1-file-write",
        "d1-file-sync",
        "d2-file-write",
        "d2-file-sync",
        "directory-sync",
        "dual-root-ordered-boundary",
        "dual-root-certificate-boundary",
        "dual-root-external-cas-boundary",
        "dual-root-physical-side-effect-boundary",
    ]
    .into_iter()
    .collect();
    let reachable: BTreeSet<&str> = TARGETS
        .iter()
        .filter(|target| target.reachability.is_reachable())
        .map(|target| target.id)
        .collect();
    assert_eq!(
        reachable, witnessed,
        "reachable rows and witnessed rows must be the same set — an entry \
         only on the left is an overclaim, one only on the right is a stale \
         inventory"
    );
}

// ---------------------------------------------------------------------------
// The witnesses for the dual-root rows (bead fgdb-s41i). `RootStore` went
// Vfs-generic at 9b80da3 and the FaultVfs composes with real durable paths
// since 8876ea4, so the ordered (write-inactive-slot-then-sync) and physical
// (the slot bytes reaching the platter) boundaries of dual-root publication
// are faultable — and a row may only say so because these tests inject there.
// ---------------------------------------------------------------------------

use asupersync::fs::{OpenOptions, Vfs, VfsFile};
use asupersync::io::AsyncWrite;
use asupersync::lab::ldfi::{
    HittingSetBudget, LdfiExperimentBudget, LdfiExperimentObservation, LdfiExperimentStatus,
};
use asupersync::lab::{AutoAdvanceTermination, LabConfig, LabRuntime, run_async_under_lab};
use asupersync::trace::{TraceData, TraceEvent};
use asupersync::types::Budget;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_chronicle::root::{NONCE_CAPACITY, OPENER_PAYLOAD_LEN, RootBootstrap, RootSlot};
use fgdb_chronicle::store::{ContinuityAuthority, ContinuityHead, RootStore, StoreError};
use fgdb_delta_types::RelationId;
use fgdb_sim::artifact::{FailureKind, Replay, Scenario};
use fgdb_sim::ldfi::{InjectableFaultClass, TraceLdfiError, derive_fault_hypotheses};
use fgdb_sim::shrink::shrink;
use fgdb_sim::vfs::{FAULT_POINT_TRACE_PREFIX, FaultKind, FaultPlan, FaultVfs, Trigger};
use fgdb_types::VId;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use std::future::poll_fn;
use std::pin::Pin;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-sim-ldfi-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
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

/// The timer-bearing counterpart to [`under_lab`]. `run_async_under_lab`
/// deliberately does not auto-advance virtual time, so an injected latency
/// would otherwise spin to its step budget and escape the LDFI report as a
/// harness panic instead of completing the exact generated experiment.
fn under_lab_auto_advance<T, Fut>(
    seed: u64,
    test: impl FnOnce(asupersync::Cx<asupersync::cx::cap::All>) -> Fut + Send + 'static,
) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let mut runtime = LabRuntime::new(LabConfig::new(seed).with_auto_advance());
    let root = runtime.state.create_root_region(Budget::INFINITE);
    let (task_id, mut handle) = runtime
        .state
        .create_task(root, Budget::INFINITE, async move {
            let cx = asupersync::Cx::current().expect("lab task runs with an ambient Cx");
            test(cx).await
        })
        .expect("lab task spawns");
    runtime
        .scheduler
        .lock()
        .schedule(task_id, Budget::INFINITE.priority);
    let auto_report = runtime.run_with_auto_advance();
    assert!(
        matches!(auto_report.termination, AutoAdvanceTermination::Quiescent),
        "lab run did not quiesce: {auto_report:?}"
    );
    let report = runtime.report();
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    handle
        .try_join()
        .expect("lab task joined")
        .expect("lab task finished")
}

fn bootstrap(seed: u8) -> RootBootstrap {
    RootBootstrap {
        root_encoding_id: [seed; 32],
        root_placement_id: [seed.wrapping_add(1); 32],
        root_placement_epoch: 1,
        failure_domain_policy_id: 2,
        root_failure_domain_id: 7,
        segment_id: 11,
        offset: 0,
        encoded_len: 4096,
        root_symbol_inventory_digest: [seed.wrapping_add(2); 32],
        object_kind: 0x0001,
        canonical_plaintext_len: 1024,
        codec_profile: 1,
        compressed_len: 1024,
        data_crypto_profile: 1,
        dek_id: [seed.wrapping_add(3); 16],
        nonce_len: NONCE_CAPACITY as u16,
        nonce_or_siv: [seed.wrapping_add(4); NONCE_CAPACITY],
        object_tag_len: 16,
        fec_profile: 1,
        transfer_length: 4096,
        oti_common: 0x0001_0002_0003_0004,
        oti_scheme: 0x0005_0006,
        symbol_size: 256,
        source_block_count: 1,
        symbol_auth_profile: 1,
        ciphertext_id: [seed.wrapping_add(5); 32],
        ciphertext_digest: [seed.wrapping_add(6); 32],
        opener_kind: 1,
        oid_key_id: [seed.wrapping_add(7); 16],
        opener_payload_len: 32,
        opener_payload: [seed.wrapping_add(8); OPENER_PAYLOAD_LEN],
        opener_digest: [seed.wrapping_add(9); 32],
    }
}

fn root_slot(generation: u64) -> RootSlot {
    RootSlot {
        format_major: 1,
        format_minor: 0,
        slot_generation: generation,
        local_writer_fence_epoch: 3,
        database_id: [0xaa; 16],
        database_security_namespace_id: [0x5a; 32],
        cluster_incarnation: 4,
        incarnation_continuity_profile_id: 1,
        cluster_incarnation_continuity_digest: [0xc3; 32],
        continuity_cas_version: 12,
        service_visibility_epoch: 5,
        root_manifest_oid: [0x77; 32],
        bootstrap: bootstrap(generation as u8),
    }
}

/// THE ORDERED-BOUNDARY WITNESS: the sync that makes a publish durable LIES.
/// Since the evidence reread landed (45ea028, fgdb-1dgm), the store itself
/// catches the lie — the post-barrier reread goes through the same Vfs, sees
/// the durable bytes, and refuses with `PublicationNotObservable` instead of
/// letting the caller believe an unobservable publication. Before AND after a
/// crash, recovery selects the PRIOR generation — cleanly, because the
/// ordering wrote the slot nobody was depending on. A faultless twin proves
/// the workload publishes generation 2 when the barrier is honest, so the
/// refusal is attributable to the lie. (The pre-45ea028 contract this witness
/// originally pinned — lie acknowledged, generation lost silently at crash —
/// is exactly what the reread exists to remove: fgdb-bh1n.)
#[test]
fn a_lying_publish_sync_is_caught_by_the_reread_and_loses_cleanly() {
    let faulted_dir = scratch_dir("lying-publish");
    let control_dir = scratch_dir("lying-publish-control");
    under_lab(90, move |cx| async move {
        let cx = &cx;
        // Eligible syncs: create's file sync is #1, publish's is #2.
        let vfs = FaultVfs::unix(FaultPlan {
            fsync_lie: Trigger::Nth(2),
            ..FaultPlan::faultless()
        });
        let store = RootStore::with_vfs(vfs.clone(), &faulted_dir);
        store.create(cx, &root_slot(1)).await.expect("genesis");
        let refused = store.publish(cx, &root_slot(2)).await;
        assert!(
            matches!(
                &refused,
                Err(StoreError::PublicationNotObservable {
                    expected_generation: 2
                })
            ),
            "the reread must catch the lying barrier and refuse typed; got {refused:?}"
        );

        let events = vfs.events();
        assert_eq!(events.len(), 1, "exactly the planned lie fired: {events:?}");
        assert!(matches!(events[0].kind, FaultKind::FsyncLie { .. }));
        assert_eq!(
            events[0].path,
            store.path(),
            "the 2nd eligible sync must be the publish barrier on manifest.root"
        );

        let pre_crash = store.current(cx).await.expect("store still opens");
        assert_eq!(
            pre_crash.slot_generation, 1,
            "a refused publication must leave the prior generation selected"
        );

        vfs.crash().await.expect("crash rollback");
        let reopened = RootStore::with_vfs(vfs.clone(), &faulted_dir);
        let recovered = reopened.current(cx).await.expect("recovery still opens");
        assert_eq!(
            recovered.slot_generation, 1,
            "the lied-about generation 2 never reached the platter, and its \
             loss is CLEAN because the ordering damaged only the inactive slot"
        );

        let control = RootStore::with_vfs(FaultVfs::unix(FaultPlan::faultless()), &control_dir);
        control.create(cx, &root_slot(1)).await.expect("genesis");
        control.publish(cx, &root_slot(2)).await.expect("publish");
        assert_eq!(
            control.current(cx).await.expect("control").slot_generation,
            2,
            "with an honest barrier the same workload publishes generation 2 — \
             without this the lie test could pass against a broken workload"
        );
    });
}

/// THE PHYSICAL-BOUNDARY WITNESS: the platter refuses the publish's bytes
/// (ENOSPC), the error is the kernel's own, and the published root is intact.
/// Space returning makes the same publish succeed.
#[test]
fn enospc_refuses_the_publish_and_the_prior_root_survives() {
    let dir = scratch_dir("enospc-publish");
    under_lab(91, move |cx| async move {
        let cx = &cx;
        // Genesis lands honestly; a fresh fault model with a budget smaller
        // than one slot then owns the publish.
        RootStore::new(&dir)
            .create(cx, &root_slot(1))
            .await
            .expect("genesis");

        let vfs = FaultVfs::unix(FaultPlan {
            space_budget: Some(8),
            ..FaultPlan::faultless()
        });
        let store = RootStore::with_vfs(vfs.clone(), &dir);
        let refused = store.publish(cx, &root_slot(2)).await;
        // assert!(matches!) rather than a match-with-panic arm: NobleMoose's
        // fgdb-j8lt ratchet re-pin is certifying as this lands, and this test
        // needs no new panic-class site to say what it means.
        assert!(
            matches!(
                &refused,
                Err(StoreError::Io(error)) if error.raw_os_error() == Some(28)
            ),
            "a full disk must surface as the kernel's own ENOSPC; got {refused:?}"
        );
        let events = vfs.events();
        assert_eq!(events.len(), 1, "exactly the planned refusal: {events:?}");
        assert!(matches!(events[0].kind, FaultKind::OutOfSpace { .. }));

        let intact = RootStore::new(&dir);
        assert_eq!(
            intact
                .current(cx)
                .await
                .expect("prior root")
                .slot_generation,
            1,
            "a refused publish must leave the published root untouched"
        );
        intact
            .publish(cx, &root_slot(2))
            .await
            .expect("the same publish succeeds once space returns");
        assert_eq!(
            intact.current(cx).await.expect("new root").slot_generation,
            2
        );
    });
}

// ---------------------------------------------------------------------------
// The witnesses for the remaining dual-root rows (bead fgdb-1dgm). The
// certificate and external-CAS machinery landed in 45ea028
// (`publish_evidenced` / `publish_with_continuity`); a row may say Reachable
// only because these tests inject at its boundary.
// ---------------------------------------------------------------------------

/// THE CERTIFICATE-BOUNDARY WITNESS: one bit of the publish flush is flipped,
/// the barrier reports success, and the post-barrier evidence reread refuses —
/// no `RootPublicationEvidence` exists, and recovery still selects the prior
/// generation. A faultless twin proves the same workload mints evidence for
/// generation 2 when the flush is honest, so the refusal is attributable to
/// the damage and not to the workload.
#[test]
fn damaged_publish_bytes_mint_no_certificate_and_the_prior_root_survives() {
    let faulted_dir = scratch_dir("bitflip-evidence");
    let control_dir = scratch_dir("bitflip-evidence-control");
    under_lab(92, move |cx| async move {
        let cx = &cx;
        // Eligible flushes: create's is #1, publish's is #2 (directory syncs
        // consume nothing — same arithmetic the lying-sync witness pins).
        let vfs = FaultVfs::unix(FaultPlan {
            bit_flip: Trigger::Nth(2),
            ..FaultPlan::faultless()
        });
        let store = RootStore::with_vfs(vfs.clone(), &faulted_dir);
        store.create(cx, &root_slot(1)).await.expect("genesis");

        let refused = store.publish_evidenced(cx, &root_slot(2)).await;
        assert!(
            matches!(
                &refused,
                Err(StoreError::PublicationNotObservable {
                    expected_generation: 2
                })
            ),
            "damaged published bytes must mint no evidence; got {refused:?}"
        );
        let events = vfs.events();
        assert_eq!(
            events.len(),
            1,
            "exactly the planned flip fired: {events:?}"
        );
        assert!(matches!(events[0].kind, FaultKind::BitFlip { .. }));
        assert_eq!(
            events[0].path,
            store.path(),
            "the 2nd eligible flush must be the publish barrier on manifest.root"
        );

        let recovered = store.current(cx).await.expect("prior root recovers");
        assert_eq!(
            recovered.slot_generation, 1,
            "the ordering wrote the slot nobody depended on, so the damage \
             cost only the new generation"
        );

        let control = RootStore::with_vfs(FaultVfs::unix(FaultPlan::faultless()), &control_dir);
        control.create(cx, &root_slot(1)).await.expect("genesis");
        let evidence = control
            .publish_evidenced(cx, &root_slot(2))
            .await
            .expect("with an honest flush the same workload mints evidence");
        assert_eq!(evidence.slot_generation, 2);
    });
}

/// A lab continuity authority: the registered external-authority model in its
/// smallest useful form — a CAS register whose head the test sets exactly.
/// `None` models an outage; the store must treat every non-`Ok` as
/// fail-closed, so this is the complete behaviour space the boundary has.
struct LabContinuityRegister(Option<ContinuityHead>);

impl ContinuityAuthority for LabContinuityRegister {
    async fn current_head(&self, _cx: &CommitCx) -> std::io::Result<ContinuityHead> {
        self.0
            .ok_or_else(|| std::io::Error::other("continuity authority unreachable"))
    }
}

/// THE EXTERNAL-CAS-BOUNDARY WITNESS: a stale head, a forked head, and an
/// absent authority each refuse the publication BEFORE the irreversible slot
/// write — the root file is byte-identical after every refusal — and the
/// matching head then admits the same slot, so the refusals were the
/// authority's doing and not a poisoned store.
#[test]
fn a_stale_forked_or_absent_continuity_head_refuses_before_the_slot_write() {
    let dir = scratch_dir("continuity-refusals");
    under_lab(93, move |cx| async move {
        let cx = &cx;
        let store = RootStore::with_vfs(FaultVfs::unix(FaultPlan::faultless()), &dir);
        store.create(cx, &root_slot(1)).await.expect("genesis");
        let pristine = std::fs::read(store.path()).expect("baseline bytes");

        // root_slot carries continuity_cas_version 12 and digest [0xc3; 32].
        let matching = ContinuityHead {
            cas_version: 12,
            cluster_incarnation_continuity_digest: [0xc3; 32],
        };
        let stale = LabContinuityRegister(Some(ContinuityHead {
            cas_version: 11,
            ..matching
        }));
        let advanced = LabContinuityRegister(Some(ContinuityHead {
            cas_version: 13,
            ..matching
        }));
        let forked = LabContinuityRegister(Some(ContinuityHead {
            cluster_incarnation_continuity_digest: [0xee; 32],
            ..matching
        }));
        let absent = LabContinuityRegister(None);

        for (name, authority) in [
            ("stale", &stale),
            ("advanced", &advanced),
            ("forked", &forked),
            ("absent", &absent),
        ] {
            let refused = store
                .publish_with_continuity(cx, &root_slot(2), authority)
                .await;
            assert!(
                matches!(
                    &refused,
                    Err(StoreError::ContinuityVersionSkew { .. }
                        | StoreError::ContinuityForked { .. }
                        | StoreError::ContinuityUnavailable(_))
                ),
                "{name}: expected a continuity refusal, got {refused:?}"
            );
            assert_eq!(
                std::fs::read(store.path()).expect("bytes after refusal"),
                pristine,
                "{name}: the refusal must precede the irreversible write"
            );
        }

        let evidence = store
            .publish_with_continuity(cx, &root_slot(2), &LabContinuityRegister(Some(matching)))
            .await
            .expect("the matching head admits the same slot");
        assert_eq!(evidence.slot_generation, 2);
    });
}

// ---------------------------------------------------------------------------
// The executable lineage adapter (fgdb-verif-sim-q97e).
// ---------------------------------------------------------------------------

const DURABLE_APPEND_OUTCOME: &str = "fgdb.sim.outcome/v1:durable-append-survived";
const SPINE_OUTCOME: &str = "fgdb.sim.outcome/v1:embedded-spine-acknowledged-write-survives-crash";

fn ldfi_keys() -> DatabaseKeys {
    DatabaseKeys {
        k_oid: [0x31; 32],
        namespace: DatabaseSecurityNamespaceId([0x32; 32]),
        dek: [0x33; 32],
    }
}

fn successful_durable_append_trace(dir: PathBuf) -> Vec<TraceEvent> {
    let (events, report) = run_async_under_lab(1_401, move |root| async move {
        let trace = root.trace_buffer().expect("lab root has a trace buffer");
        let vfs = FaultVfs::unix(FaultPlan::faultless());
        let path = dir.join("append.log");
        let mut expected = Vec::new();
        for sector in 0u8..4 {
            expected.extend(std::iter::repeat_n(sector + 1, 512));
        }
        let mut file = vfs
            .open(
                &path,
                &OpenOptions::new().write(true).create(true).truncate(true),
            )
            .await
            .expect("baseline opens");
        let mut written = 0usize;
        while written < expected.len() {
            let count =
                poll_fn(|task_cx| Pin::new(&mut file).poll_write(task_cx, &expected[written..]))
                    .await
                    .expect("baseline writes");
            assert!(count > 0, "baseline write must make progress");
            written += count;
        }
        file.sync_all().await.expect("baseline syncs honestly");
        vfs.crash().await.expect("baseline crashes cleanly");
        assert_eq!(
            vfs.read(&path)
                .await
                .expect("baseline reopens durable bytes"),
            expected,
            "the successful trace must actually establish its outcome"
        );
        root.trace(DURABLE_APPEND_OUTCOME);
        trace.snapshot()
    });
    assert!(report.lab_test_passed(), "baseline lab report: {report:?}");
    events
}

fn successful_embedded_spine_trace(dir: PathBuf) -> Vec<TraceEvent> {
    let (events, report) = run_async_under_lab(1_400, move |root| async move {
        let trace = root.trace_buffer().expect("lab root has a trace buffer");
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        drop(
            Database::create(&cx, &dir, ldfi_keys())
                .await
                .expect("spine genesis"),
        );

        let vfs = FaultVfs::unix(FaultPlan::faultless());
        let mut database = Database::open_with_vfs(&cx, vfs.clone(), &dir, ldfi_keys())
            .await
            .expect("spine opens through the traced VFS");
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(1), vec![], vec![]);
        database.write(&cx, batch).await.expect("spine commits");
        assert!(
            database
                .vertex(VId(1))
                .expect("spine remains readable")
                .is_some(),
            "the outcome marker must follow a real graph read"
        );
        vfs.crash()
            .await
            .expect("the successful run crashes cleanly");
        drop(database);

        let reopened = Database::open(&cx, &dir, ldfi_keys())
            .await
            .expect("the successful run reopens after process loss");
        assert_eq!(
            reopened
                .frontier()
                .expect("the reopened frontier is readable"),
            fgdb_types::CommitSeq(1),
            "the outcome marker must follow recovery of the acknowledged frontier"
        );
        assert!(
            reopened
                .vertex(VId(1))
                .expect("the reopened graph is readable")
                .is_some(),
            "the outcome marker must follow recovery of the acknowledged vertex"
        );
        root.trace(SPINE_OUTCOME);
        trace.snapshot()
    });
    assert!(report.lab_test_passed(), "spine lab report: {report:?}");
    events
}

#[test]
fn successful_embedded_spine_trace_reaches_the_upstream_ldfi_search() {
    let events = successful_embedded_spine_trace(scratch_dir("lineage-spine"));

    let derived = derive_fault_hypotheses(
        &events,
        SPINE_OUTCOME,
        HittingSetBudget {
            max_depth: 2,
            max_hypotheses: 256,
        },
    )
    .expect("the real spine trace is consumable by asupersync LDFI");
    assert_eq!(derived.source_event_count, events.len());
    assert!(
        derived.fault_point_count > 0,
        "no VFS seam reached the trace"
    );
    assert_eq!(derived.outcome_count, 1);
    assert!(
        !derived.hypotheses.is_empty(),
        "successful graph state had no derived fault hypothesis: {derived:?}; markers={:?}",
        events
            .iter()
            .filter(|event| matches!(&event.data, TraceData::Message(message) if message.starts_with(FAULT_POINT_TRACE_PREFIX) || message == SPINE_OUTCOME))
            .collect::<Vec<_>>()
    );
    assert!(
        derived
            .hypotheses
            .iter()
            .all(|hypothesis| hypothesis.to_plan(7, 100).is_ok()),
        "the representative spine's minimal hypotheses must map exactly"
    );

    // Negative mutation: a versioned marker with ordinal zero is not ignored.
    // Ignoring it would silently remove one fault from the successful lineage.
    let mut malformed = events;
    let marker = malformed
        .iter_mut()
        .find(|event| {
            matches!(
                &event.data,
                TraceData::Message(message) if message.starts_with(FAULT_POINT_TRACE_PREFIX)
            )
        })
        .expect("the positive control found a marker");
    marker.data = TraceData::Message(format!("{FAULT_POINT_TRACE_PREFIX}fsync-lie:0"));
    assert!(matches!(
        derive_fault_hypotheses(&malformed, SPINE_OUTCOME, HittingSetBudget::default()),
        Err(TraceLdfiError::MalformedFaultPoint { .. })
    ));
}

#[derive(Debug)]
struct SpineExperimentResult {
    observation: LdfiExperimentObservation,
    acknowledged: Option<u64>,
    recovered_frontier: Option<u64>,
    recovered_vertex: bool,
    events: Vec<fgdb_sim::vfs::FaultEvent>,
    detail: String,
}

/// Execute one generated plan against the same integrated create/open/write/
/// crash/reopen workload that produced [`SPINE_OUTCOME`].
///
/// A clean refusal is not a safety failure. The only falsifying result is an
/// acknowledged write whose commit frontier or vertex is absent after process
/// loss. This keeps LDFI from winning by turning an ordinary I/O error into an
/// alleged durability counterexample.
fn execute_spine_hypothesis(plan: FaultPlan, dir: PathBuf, lab_seed: u64) -> SpineExperimentResult {
    under_lab_auto_advance(lab_seed, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        drop(
            Database::create(&cx, &dir, ldfi_keys())
                .await
                .expect("experiment genesis"),
        );

        let vfs = FaultVfs::unix_with_clock(plan, root);
        let opened = Database::open_with_vfs(&cx, vfs.clone(), &dir, ldfi_keys()).await;
        let mut database = match opened {
            Ok(database) => database,
            Err(error) => {
                return SpineExperimentResult {
                    observation: LdfiExperimentObservation::InvariantHeld,
                    acknowledged: None,
                    recovered_frontier: None,
                    recovered_vertex: false,
                    events: vfs.events(),
                    detail: format!("open refused before an acknowledgement: {error}"),
                };
            }
        };

        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(1), vec![], vec![]);
        let write = database.write(&cx, batch).await;
        let acknowledged = write.as_ref().ok().map(|seq| seq.0);
        let write_detail = write.as_ref().err().map_or_else(
            || "write acknowledged".to_string(),
            |error| error.to_string(),
        );

        let crash = vfs.crash().await;
        drop(database);
        let reopened = Database::open(&cx, &dir, ldfi_keys()).await;
        let recovered_frontier = reopened
            .as_ref()
            .ok()
            .and_then(|database| database.frontier().ok())
            .map(|seq| seq.0);
        let recovered_vertex = reopened
            .as_ref()
            .ok()
            .and_then(|database| database.vertex(VId(1)).ok())
            .flatten()
            .is_some();
        let reopen_detail = reopened
            .as_ref()
            .map_or_else(|error| error.to_string(), |_| "ok".to_string());

        let acknowledgement_survived = acknowledged.is_none()
            || (crash.is_ok() && recovered_frontier == acknowledged && recovered_vertex);
        let observation = if acknowledgement_survived {
            LdfiExperimentObservation::InvariantHeld
        } else {
            LdfiExperimentObservation::InvariantViolated
        };
        let detail = format!(
            "{write_detail}; crash={crash:?}; reopen={reopen_detail}; \
             acknowledged={acknowledged:?}; recovered_frontier={recovered_frontier:?}; \
             recovered_vertex={recovered_vertex}"
        );
        SpineExperimentResult {
            observation,
            acknowledged,
            recovered_frontier,
            recovered_vertex,
            events: vfs.events(),
            detail,
        }
    })
}

#[test]
fn marker_barrier_lie_is_not_acknowledged_by_the_embedded_spine() {
    let result = execute_spine_hypothesis(
        FaultPlan {
            fsync_lie: Trigger::Nth(2),
            ..FaultPlan::faultless()
        },
        scratch_dir("lineage-spine-marker-readback"),
        1_409,
    );
    assert!(
        matches!(result.observation, LdfiExperimentObservation::InvariantHeld),
        "an unobservable marker must not become an acknowledged loss: {result:?}"
    );
    assert_eq!(
        result.acknowledged, None,
        "the writer acknowledged a marker its fresh readback could not observe: {result:?}"
    );
    assert_eq!(result.events.len(), 1, "the exact planned event fired");
    assert!(matches!(result.events[0].kind, FaultKind::FsyncLie { .. }));
    assert!(
        result.detail.contains("post-barrier readback"),
        "the refusal must name the failed evidence boundary: {}",
        result.detail
    );
}

#[test]
fn trace_derived_faults_execute_against_the_same_embedded_spine_workload() {
    let events = successful_embedded_spine_trace(scratch_dir("lineage-spine-exec-baseline"));
    let derived = derive_fault_hypotheses(
        &events,
        SPINE_OUTCOME,
        HittingSetBudget {
            max_depth: 2,
            max_hypotheses: 512,
        },
    )
    .expect("the successful spine trace derives hypotheses");
    assert!(
        derived.search.exhausted,
        "the bounded spine trace must be exhausted before making a campaign claim"
    );

    // Planted negative: the same full database workload under a faultless plan
    // acknowledges and recovers. If the experiment merely reports failure (or
    // the reopen check is disconnected), this control catches it.
    let control = execute_spine_hypothesis(
        FaultPlan::faultless(),
        scratch_dir("lineage-spine-exec-control"),
        1_410,
    );
    assert!(
        matches!(
            control.observation,
            LdfiExperimentObservation::InvariantHeld
        ),
        "faultless control failed: {control:?}"
    );
    assert_eq!(control.acknowledged, Some(1));
    assert_eq!(control.recovered_frontier, Some(1));
    assert!(control.recovered_vertex);
    assert!(control.events.is_empty());

    let mut experiment_ordinal = 0usize;
    let mut results = Vec::new();
    let report = derived.run_experiments(
        LdfiExperimentBudget {
            max_experiments: 512,
        },
        |hypothesis| {
            experiment_ordinal += 1;
            let plan = hypothesis
                .to_plan(0x1df2_0000 + experiment_ordinal as u64, 100)
                .expect("the bounded spine hypotheses map exactly");
            eprintln!(
                "spine LDFI experiment={experiment_ordinal} lab_seed={} hypothesis={hypothesis:?} plan={plan:?}",
                1_410 + experiment_ordinal as u64
            );
            let result = execute_spine_hypothesis(
                plan,
                scratch_dir(&format!("lineage-spine-exec-{experiment_ordinal}")),
                1_410 + experiment_ordinal as u64,
            );
            let observation = result.observation;
            results.push(result);
            observation
        },
    );
    match &report.status {
        LdfiExperimentStatus::RefutedUpToDepth { max_depth } => {
            assert_eq!(
                *max_depth, 2,
                "the clean verdict must cover the requested depth"
            );
        }
        LdfiExperimentStatus::FoundViolation { hypothesis: found } => {
            let fault = derived
                .hypotheses
                .iter()
                .find(|hypothesis| &hypothesis.events == found)
                .expect("the reported event set came from the derived spine hypotheses");
            let reproduced = execute_spine_hypothesis(
                fault
                    .to_plan(0x1df2_ffff, 100)
                    .expect("the found spine hypothesis maps exactly"),
                scratch_dir("lineage-spine-exec-replay"),
                1_999,
            );
            assert!(
                matches!(
                    reproduced.observation,
                    LdfiExperimentObservation::InvariantViolated
                ),
                "the reported full-spine violation did not reproduce: {reproduced:?}"
            );
            assert_eq!(
                reproduced.acknowledged,
                Some(1),
                "only an acknowledged write may count as this safety violation: {}",
                reproduced.detail
            );
            assert_eq!(
                reproduced.events.len(),
                fault.points.len(),
                "the replay broadened the exact generated event set: {reproduced:?}"
            );
            panic!(
                "full-spine campaign found a reproducible acknowledged-loss case: {reproduced:?}"
            );
        }
        status => panic!(
            "full-spine campaign did not complete its bounded search: \
             status={status:?}; results={results:#?}"
        ),
    }
}

#[test]
fn trace_derived_fault_is_executed_and_shrunk_without_blind_enumeration() {
    let events = successful_durable_append_trace(scratch_dir("lineage-append-baseline"));
    let derived = derive_fault_hypotheses(
        &events,
        DURABLE_APPEND_OUTCOME,
        HittingSetBudget {
            max_depth: 2,
            max_hypotheses: 64,
        },
    )
    .expect("successful append trace derives hypotheses");
    assert!(
        derived.search.exhausted,
        "the tiny trace search must exhaust"
    );

    let mut experiment_ordinal = 0usize;
    let report = derived.run_experiments(
        LdfiExperimentBudget {
            max_experiments: 64,
        },
        |hypothesis| {
            experiment_ordinal += 1;
            let replay = Replay {
                scenario: Scenario::DurableAppend,
                plan: hypothesis
                    .to_plan(0x1df1_0000 + experiment_ordinal as u64, 100)
                    .expect("this one-sync corpus maps exactly"),
            };
            let outcome = replay.run(&scratch_dir(&format!(
                "lineage-append-experiment-{experiment_ordinal}"
            )));
            if outcome.failure.is_some() {
                LdfiExperimentObservation::InvariantViolated
            } else {
                LdfiExperimentObservation::InvariantHeld
            }
        },
    );
    assert!(
        matches!(report.status, LdfiExperimentStatus::FoundViolation { .. }),
        "the trace-derived fsync lie must be found: {:?}",
        report.status
    );
    let LdfiExperimentStatus::FoundViolation { hypothesis: found } = &report.status else {
        return;
    };
    assert!(report.experiments_run < derived.fault_point_count);

    let fault = derived
        .hypotheses
        .iter()
        .find(|hypothesis| &hypothesis.events == found)
        .expect("reported event set came from the derived hypotheses");
    assert_eq!(fault.points.len(), 1, "the found hypothesis is minimal");
    assert_eq!(fault.points[0].class, InjectableFaultClass::FsyncLie);
    let replay = Replay {
        scenario: Scenario::DurableAppend,
        plan: fault
            .to_plan(0x1df1_ffff, 100)
            .expect("found hypothesis maps exactly"),
    };
    let shrunk = shrink(replay, &scratch_dir("lineage-append-shrink"))
        .expect("the trace-derived failure reproduces for the shrinker");
    assert_eq!(shrunk.failure.kind, FailureKind::AcknowledgedBytesLost);
    assert_eq!(
        shrunk.replay.plan.fsync_lie,
        Trigger::At(1),
        "shrinking must retain the one event that caused the failure"
    );
}

#[test]
fn an_exact_ldfi_ordinal_fires_once_not_periodically() {
    let dir = scratch_dir("lineage-exact-ordinal");
    under_lab(1_402, move |_cx| async move {
        let path = dir.join("exact.log");
        let expected = vec![0xa5; 2_048];
        let vfs = FaultVfs::unix(FaultPlan {
            fsync_lie: Trigger::At(1),
            ..FaultPlan::faultless()
        });
        let mut file = vfs
            .open(
                &path,
                &OpenOptions::new().write(true).create(true).truncate(true),
            )
            .await
            .expect("exact-ordinal fixture opens");
        let count = poll_fn(|task_cx| Pin::new(&mut file).poll_write(task_cx, &expected))
            .await
            .expect("fixture writes");
        assert_eq!(count, expected.len());

        file.sync_all().await.expect("first sync lies");
        assert_eq!(vfs.events().len(), 1, "the exact first boundary fired");
        file.sync_all()
            .await
            .expect("second eligible sync is honest");
        assert_eq!(
            vfs.events().len(),
            1,
            "At(1) must not repeat at a later eligible boundary"
        );
        vfs.crash().await.expect("fixture crashes");
        assert_eq!(
            vfs.read(&path).await.expect("durable bytes reopen"),
            expected,
            "the honest second sync must persist what the one-shot lie left dirty"
        );
    });
}
