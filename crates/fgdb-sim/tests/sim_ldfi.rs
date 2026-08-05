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

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::root::{NONCE_CAPACITY, OPENER_PAYLOAD_LEN, RootBootstrap, RootSlot};
use fgdb_chronicle::store::{ContinuityAuthority, ContinuityHead, RootStore, StoreError};
use fgdb_sim::vfs::{FaultKind, FaultPlan, FaultVfs, Trigger};
use fgdb_types::context::{CommitCx, PurposeContexts};

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
