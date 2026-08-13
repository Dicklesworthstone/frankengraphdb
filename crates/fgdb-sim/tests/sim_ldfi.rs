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
    ActivationRejection, BASE_HARNESS_OWNER, CampaignEntrypoint, EXPECTED_LDFI_OWNER_BEADS,
    EXPECTED_TARGET_IDS, G1_GATE, G3_GATE, G3_PHASE_OWNER, GENESIS_GATE, LOCAL_TORTURE_OWNER,
    LdfiOwnerCompletion, LdfiOwnerCompletionError, LdfiTarget, Reachability,
    RegistryValidationError, TARGETS, TargetMetadataError, TargetRowState, W12_GATE,
    W12_PHASE_OWNER, campaign_entrypoint, coverage_statement, expected_phase_boundary,
    g3_campaign_entrypoint, reachable_count, registry_jsonl, unreachable_count,
    validate_activation, validate_ldfi_owner_completion, validate_registry_rows,
    validate_target_metadata, w12_campaign_entrypoint,
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

fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/<crate> has a repository root")
        .to_path_buf()
}

fn json_string_field<'a>(line: &'a str, field: &str) -> Option<&'a str> {
    let needle = format!("\"{field}\":\"");
    let value = line.split_once(&needle)?.1;
    value.split_once('"').map(|(value, _)| value)
}

fn tracked_owner_completion() -> Vec<LdfiOwnerCompletion> {
    let jsonl = std::fs::read_to_string(repository_root().join(".beads/issues.jsonl"))
        .expect("the tracked Beads export is mandatory input to this local CI check");
    EXPECTED_LDFI_OWNER_BEADS
        .iter()
        .map(|owner| {
            let matching: Vec<&str> = jsonl
                .lines()
                .filter(|line| json_string_field(line, "id") == Some(*owner))
                .collect();
            assert_eq!(
                matching.len(),
                1,
                "owner {owner:?} must occur exactly once in the tracked Beads export"
            );
            let status = json_string_field(matching[0], "status")
                .expect("an LDFI owner Bead must carry a status");
            LdfiOwnerCompletion {
                owner_bead: owner,
                complete: status == "closed",
            }
        })
        .collect()
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
    let ordered_ids: Vec<&str> = TARGETS.iter().map(|target| target.id).collect();
    assert_eq!(
        ordered_ids, EXPECTED_TARGET_IDS,
        "the live table must preserve the complete normative target inventory in plan order"
    );
    let ids: BTreeSet<&str> = TARGETS.iter().map(|target| target.id).collect();
    assert_eq!(
        ids.len(),
        TARGETS.len(),
        "duplicate LDFI target id inflates the denominator"
    );
    for target_id in EXPECTED_TARGET_IDS {
        assert!(
            expected_phase_boundary(target_id).is_some(),
            "normative target {target_id:?} has no production phase mapping"
        );
    }
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

#[test]
fn every_row_has_closed_phase_owner_gate_state_and_evidence_metadata() {
    for target in TARGETS {
        assert_eq!(
            validate_target_metadata(target),
            Ok(()),
            "target {:?} has invalid metadata",
            target.id
        );

        let expected = expected_phase_boundary(target.id)
            .expect("every normative target id has one production phase mapping");
        assert_eq!(
            (target.phase_owner_bead, target.first_required_gate),
            expected,
            "target {:?} is attributed to the wrong phase",
            target.id
        );
        if target.row_state == TargetRowState::Live {
            assert_eq!(
                expected,
                (BASE_HARNESS_OWNER, GENESIS_GATE),
                "only q97e's current fixture witnesses are live in the base harness"
            );
        }
    }
}

#[test]
fn phase_authority_ids_are_pinned_to_the_tracker_contract() {
    assert_eq!(BASE_HARNESS_OWNER, "fgdb-verif-sim-q97e");
    assert_eq!(GENESIS_GATE, "fgdb-gate-genesis-lce");
    assert_eq!(LOCAL_TORTURE_OWNER, "fgdb-verif-torture-ddcl");
    assert_eq!(G1_GATE, "fgdb-gate-g1-6vc");
    assert_eq!(G3_PHASE_OWNER, "fgdb-g3-protocol-ha-torture-jni4");
    assert_eq!(G3_GATE, "fgdb-gate-g3-30m");
    assert_eq!(W12_PHASE_OWNER, "fgdb-w12-formal-torture-ejx0");
    assert_eq!(W12_GATE, "fgdb-gate-w12-w2y");
}

#[test]
fn tracked_owner_completion_cannot_hide_pending_ldfi_rows() {
    let tracked = tracked_owner_completion();
    validate_ldfi_owner_completion(TARGETS, &tracked)
        .expect("open future owners may carry explicit pending rows");

    let mut closed_base = tracked.clone();
    closed_base[0].complete = true;
    validate_ldfi_owner_completion(TARGETS, &closed_base)
        .expect("the q97e base owner has live evidence for every row it owns");

    let mut prematurely_closed_local = tracked.clone();
    prematurely_closed_local[1].complete = true;
    assert_eq!(
        validate_ldfi_owner_completion(TARGETS, &prematurely_closed_local),
        Err(LdfiOwnerCompletionError::CompletedOwnerMissingCampaign {
            owner_bead: LOCAL_TORTURE_OWNER,
            target_id: "checkpoint-install",
        })
    );

    assert_eq!(
        validate_ldfi_owner_completion(TARGETS, &tracked[..tracked.len() - 1]),
        Err(LdfiOwnerCompletionError::OwnerInventoryLength {
            expected: EXPECTED_LDFI_OWNER_BEADS.len(),
            actual: EXPECTED_LDFI_OWNER_BEADS.len() - 1,
        })
    );

    let mut reordered = tracked;
    reordered.swap(1, 2);
    assert_eq!(
        validate_ldfi_owner_completion(TARGETS, &reordered),
        Err(LdfiOwnerCompletionError::OwnerInventoryId { index: 1 })
    );
}

#[test]
fn fake_cross_phase_and_duplicate_targets_fail_the_registry_closed() {
    let template = *TARGETS
        .iter()
        .find(|target| target.id == "d1-file-sync")
        .expect("the fixed target list contains d1-file-sync");
    let fake_owner = LdfiTarget {
        phase_owner_bead: "fgdb-made-up-owner",
        ..template
    };
    assert_eq!(
        validate_target_metadata(&fake_owner),
        Err(TargetMetadataError::PhaseBoundaryMismatch)
    );
    assert_eq!(
        validate_registry_rows(&[fake_owner]),
        Err(RegistryValidationError::InvalidRow {
            target_id: "d1-file-sync",
            error: TargetMetadataError::PhaseBoundaryMismatch,
        })
    );

    let fake_gate = LdfiTarget {
        first_required_gate: "fgdb-gate-invented",
        ..template
    };
    assert_eq!(
        validate_target_metadata(&fake_gate),
        Err(TargetMetadataError::PhaseBoundaryMismatch)
    );

    let invented_target = LdfiTarget {
        id: "invented-target",
        ..template
    };
    assert_eq!(
        validate_target_metadata(&invented_target),
        Err(TargetMetadataError::UnknownTargetId)
    );

    for (target_id, wrong_owner, wrong_gate) in [
        ("checkpoint-install", G3_PHASE_OWNER, G3_GATE),
        ("attempt-generation", G3_PHASE_OWNER, G3_GATE),
        ("raft", W12_PHASE_OWNER, W12_GATE),
        ("local-to-w12-seal", BASE_HARNESS_OWNER, GENESIS_GATE),
    ] {
        let mut reattributed = *TARGETS
            .iter()
            .find(|target| target.id == target_id)
            .expect("the cross-phase negative names a normative target");
        reattributed.phase_owner_bead = wrong_owner;
        reattributed.first_required_gate = wrong_gate;
        if let Reachability::NotYetBuilt { bead } = &mut reattributed.reachability {
            *bead = wrong_owner;
        }
        assert_eq!(
            validate_target_metadata(&reattributed),
            Err(TargetMetadataError::PhaseBoundaryMismatch),
            "known target {target_id:?} accepted a different registered phase"
        );
    }

    assert_eq!(
        validate_registry_rows(&[template, template]),
        Err(RegistryValidationError::DuplicateTargetId {
            target_id: "d1-file-sync",
        })
    );

    let without_ticket_claim: Vec<LdfiTarget> = TARGETS
        .iter()
        .copied()
        .filter(|target| target.id != "ticket-claim")
        .collect();
    assert_eq!(
        validate_registry_rows(&without_ticket_claim),
        Err(RegistryValidationError::TargetInventoryLength {
            expected: EXPECTED_TARGET_IDS.len(),
            actual: EXPECTED_TARGET_IDS.len() - 1,
        }),
        "removing a pending row must not shrink the normative denominator"
    );
}

#[test]
fn activation_before_the_owning_implementation_lands_is_rejected() {
    let mut exercised = 0;
    for target in TARGETS
        .iter()
        .filter(|target| target.row_state == TargetRowState::Pending)
    {
        exercised += 1;
        assert_eq!(
            validate_activation(
                target.id,
                target.phase_owner_bead,
                target.first_required_gate,
                Some("plausible-but-unregistered-evidence"),
            ),
            Err(ActivationRejection::ImplementationDisabled),
            "pending target {:?} activated before its implementation",
            target.id
        );
    }
    assert!(exercised > 0, "the premature-activation law was vacuous");
}

#[test]
fn every_live_row_activates_only_with_its_exact_owner_gate_and_evidence() {
    let mut exercised = 0;
    for target in TARGETS
        .iter()
        .filter(|target| target.row_state == TargetRowState::Live)
    {
        exercised += 1;
        let evidence = target
            .coverage_evidence_ref
            .expect("metadata validation requires live evidence");
        assert_eq!(
            validate_activation(
                target.id,
                target.phase_owner_bead,
                target.first_required_gate,
                Some(evidence),
            ),
            Ok(target)
        );
        assert_eq!(
            validate_activation(
                target.id,
                G3_PHASE_OWNER,
                target.first_required_gate,
                Some(evidence),
            ),
            Err(ActivationRejection::WrongPhaseOwner)
        );
        assert_eq!(
            validate_activation(
                target.id,
                target.phase_owner_bead,
                target.first_required_gate,
                Some("different-evidence"),
            ),
            Err(ActivationRejection::EvidenceMismatch)
        );
    }
    assert!(exercised > 0, "the live-activation law was vacuous");
}

#[test]
fn every_live_evidence_reference_resolves_to_one_exact_test() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/<crate> has a repo root");
    for target in TARGETS
        .iter()
        .filter(|target| target.row_state == TargetRowState::Live)
    {
        let reference = target
            .coverage_evidence_ref
            .expect("metadata validation requires live evidence");
        let (path, selector) = reference.rsplit_once("::").unwrap_or(("", ""));
        assert!(
            !path.is_empty() && !selector.is_empty(),
            "target {:?} has a non-resolvable evidence reference {reference:?}",
            target.id
        );
        let source = std::fs::read_to_string(root.join(path)).unwrap_or_default();
        let function = format!("fn {selector}(");
        assert_eq!(
            source.matches(&function).count(),
            1,
            "target {:?} evidence {reference:?} must resolve to one exact test function",
            target.id
        );
        let function_offset = source.find(&function).unwrap_or(source.len());
        let prefix = source.get(..function_offset).unwrap_or_default();
        assert_eq!(
            prefix.lines().rev().find(|line| !line.trim().is_empty()),
            Some("#[test]"),
            "target {:?} evidence {reference:?} is not a #[test] function",
            target.id
        );
    }
}

#[test]
fn local_g3_and_w12_entrypoints_are_delegated_not_covered_by_the_base_harness() {
    assert_eq!(
        campaign_entrypoint("checkpoint-install"),
        Ok(CampaignEntrypoint::Delegated {
            phase_owner_bead: LOCAL_TORTURE_OWNER,
            first_required_gate: G1_GATE,
            row_state: TargetRowState::Pending,
        })
    );
    assert_eq!(
        campaign_entrypoint("raft"),
        Ok(CampaignEntrypoint::Delegated {
            phase_owner_bead: G3_PHASE_OWNER,
            first_required_gate: G3_GATE,
            row_state: TargetRowState::Pending,
        })
    );
    assert_eq!(
        campaign_entrypoint("local-to-w12-seal"),
        Ok(CampaignEntrypoint::Delegated {
            phase_owner_bead: W12_PHASE_OWNER,
            first_required_gate: W12_GATE,
            row_state: TargetRowState::Pending,
        })
    );
    assert_eq!(
        g3_campaign_entrypoint("raft"),
        Err(ActivationRejection::ImplementationDisabled),
        "the dedicated G3 entrypoint must refuse until its product owner activates the row"
    );
    assert_eq!(
        w12_campaign_entrypoint("local-to-w12-seal"),
        Err(ActivationRejection::ImplementationDisabled),
        "the dedicated W12 entrypoint must refuse until its product owner activates the row"
    );
    assert_eq!(
        g3_campaign_entrypoint("local-to-w12-seal"),
        Err(ActivationRejection::WrongPhaseOwner),
        "a W12 row must not be executable through the G3 adapter"
    );
    assert_eq!(
        w12_campaign_entrypoint("raft"),
        Err(ActivationRejection::WrongPhaseOwner),
        "a G3 row must not be executable through the W12 adapter"
    );

    let covered =
        campaign_entrypoint("d1-file-sync").expect("the current D1 witness has a base entrypoint");
    assert!(
        matches!(covered, CampaignEntrypoint::Covered { .. }),
        "the positive control must prove the router can report real coverage"
    );
}

#[test]
fn jsonl_emits_every_phase_boundary_field_for_every_declared_target() {
    let jsonl = registry_jsonl().expect("the complete registry validates before export");
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), TARGETS.len());

    let keys = [
        "event_version",
        "target_id",
        "phase_owner_bead",
        "first_required_gate",
        "implementation_enabled",
        "row_state",
        "coverage_evidence_ref",
    ];
    for (line, target) in lines.into_iter().zip(TARGETS) {
        assert!(line.starts_with('{') && line.ends_with('}'));
        for key in keys {
            assert_eq!(
                line.matches(&format!("\"{key}\":")).count(),
                1,
                "target {:?} does not emit exactly one {key:?}: {line}",
                target.id
            );
        }
        assert!(
            line.contains(&format!("\"target_id\":\"{}\"", target.id)),
            "JSONL target order drifted from the registry: {line}"
        );
        match target.coverage_evidence_ref {
            Some(evidence) => assert!(line.contains(evidence)),
            None => assert!(line.contains("\"coverage_evidence_ref\":null")),
        }
    }
}

/// THE TEST THE REGISTRY EXISTS FOR. Coverage is reported against the plan's
/// denominator, and the gap is a number rather than an omission.
#[test]
fn the_coverage_gap_is_reported_not_hidden() {
    let reachable = reachable_count().expect("the complete registry validates before counting");
    let unreachable = unreachable_count().expect("the complete registry validates before counting");
    assert_eq!(
        reachable + unreachable,
        TARGETS.len(),
        "the counts do not partition the table; coverage arithmetic would be wrong"
    );

    // Both sides non-zero, which is the honest state at this HEAD and also the
    // non-vacuity control: with no reachable targets the registry would be
    // aspirational, and with none unreachable it would be lying.
    assert!(
        reachable > 0,
        "no target is reachable; the lab VFS faults D1/D2 writes and syncs today"
    );
    assert!(
        unreachable > 0,
        "every declared target is reachable, which at this HEAD would mean the \
         denominator was quietly redefined to what we built"
    );

    let statement = coverage_statement().expect("the complete registry validates before reporting");
    assert!(
        statement.contains(&TARGETS.len().to_string()),
        "the coverage statement must name the plan's denominator: {statement}"
    );
    assert!(
        statement.contains(&unreachable.to_string()),
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
    //     `durability_semantics_e2e.rs`; write ENOSPC is injected before the
    //     volatile image accepts any byte, separately from sync faults;
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
        .filter(|target| target.row_state == TargetRowState::Live)
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
    FaultEventId, HittingSetBudget, LdfiExperimentBudget, LdfiExperimentObservation,
    LdfiExperimentStatus,
};
use asupersync::lab::{AutoAdvanceTermination, LabConfig, LabRuntime, run_async_under_lab};
use asupersync::trace::{TraceData, TraceEvent};
use asupersync::types::Budget;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_chronicle::root::{NONCE_CAPACITY, OPENER_PAYLOAD_LEN, RootBootstrap, RootSlot};
use fgdb_chronicle::store::{ContinuityAuthority, ContinuityHead, RootStore, StoreError};
use fgdb_delta_types::RelationId;
use fgdb_sim::artifact::{FailureKind, Replay, Scenario};
use fgdb_sim::campaign::{CampaignOutcome, ClaimClass};
use fgdb_sim::ldfi::{
    InjectableFaultClass, TraceLdfiCampaignError, TraceLdfiError, TraceLdfiFailureDisposition,
    TraceLdfiReplayCampaignConfig, TraceLdfiReplayContract, TracedFaultPoint,
    derive_fault_hypotheses,
};
use fgdb_sim::redaction::{MediatedRecord, RecordClass, RedactionPolicy};
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

        let observation =
            observe_spine_durability(acknowledged, &crash, recovered_frontier, recovered_vertex);
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

fn observe_spine_durability(
    acknowledged: Option<u64>,
    crash: &std::io::Result<()>,
    recovered_frontier: Option<u64>,
    recovered_vertex: bool,
) -> LdfiExperimentObservation {
    if acknowledged.is_none()
        || (crash.is_ok() && recovered_frontier == acknowledged && recovered_vertex)
    {
        LdfiExperimentObservation::InvariantHeld
    } else {
        LdfiExperimentObservation::InvariantViolated
    }
}

#[test]
fn acknowledged_loss_observer_detects_a_planted_missing_commit() {
    let control = observe_spine_durability(Some(1), &Ok(()), Some(1), true);
    assert_eq!(
        control,
        LdfiExperimentObservation::InvariantHeld,
        "the near-identical durable control must remain clean"
    );
    let planted_loss = observe_spine_durability(Some(1), &Ok(()), None, false);
    assert_eq!(
        planted_loss,
        LdfiExperimentObservation::InvariantViolated,
        "the detector must reject the same acknowledgement with its commit missing"
    );
}

#[test]
fn one_shot_marker_sync_lie_is_reinforced_before_the_spine_acknowledges() {
    let result = execute_spine_hypothesis(
        FaultPlan {
            // Eligible file syncs are D1 then D2. The D2 primary call lies;
            // its same-handle reinforcement is the next honest eligible call.
            fsync_lie: Trigger::At(2),
            ..FaultPlan::faultless()
        },
        scratch_dir("lineage-spine-marker-reinforcement"),
        1_409,
    );
    assert!(
        matches!(result.observation, LdfiExperimentObservation::InvariantHeld),
        "one transient marker-sync lie became an acknowledged loss: {result:?}"
    );
    assert_eq!(result.acknowledged, Some(1));
    assert_eq!(result.recovered_frontier, Some(1));
    assert!(result.recovered_vertex);
    assert_eq!(result.events.len(), 1, "the exact planned lie fired");
    assert!(matches!(result.events[0].kind, FaultKind::FsyncLie { .. }));
    assert_eq!(
        result.events[0]
            .path
            .file_name()
            .and_then(|name| name.to_str()),
        Some(fgdb_chronicle::commit::COMMIT_LOG_NAME),
        "the injected boundary must be Chronicle D2"
    );
}

#[test]
fn trace_derived_faults_execute_against_the_same_embedded_spine_workload() -> Result<(), String> {
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
            let plan = fault
                .to_plan(0x1df2_ffff, 100)
                .expect("the found spine hypothesis maps exactly");
            let reproduced =
                execute_spine_hypothesis(plan, scratch_dir("lineage-spine-exec-replay"), 1_999);
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

            // Cross the existing artifact + shrink boundary with the exact
            // generated plan. The campaign remains repair-neutral: a clean
            // exhausted campaign is green, while a violation is useful only
            // if it leaves a standalone reproducer rather than requiring the
            // product to stay buggy.
            let replay = Replay {
                scenario: Scenario::SpineDurability,
                plan,
            };
            let artifact_run = replay.run(&scratch_dir("lineage-spine-artifact"));
            assert_eq!(
                artifact_run.failure.as_ref().map(|failure| failure.kind),
                Some(FailureKind::AcknowledgedCommitLost),
                "the generated violation did not cross the structured replay boundary: \
                 {artifact_run:?}"
            );
            assert!(
                artifact_run.artifact.is_some(),
                "the generated violation reproduced without emitting its artifact"
            );
            let shrunk = shrink(replay, &scratch_dir("lineage-spine-shrink"))
                .map_err(|error| format!("could not create isolated shrink attempt: {error}"))?
                .expect("the generated violation reproduced for the shrinker");
            let minimal = shrunk
                .replay
                .run(&scratch_dir("lineage-spine-shrunk-replay"));
            assert_eq!(
                minimal.failure.as_ref().map(|failure| failure.kind),
                Some(FailureKind::AcknowledgedCommitLost),
                "the shrunk full-spine artifact did not reproduce: {minimal:?}"
            );
            assert!(minimal.artifact.is_some());
        }
        status => {
            return Err(format!(
                "full-spine campaign did not complete its bounded search: \
                 status={status:?}; results={results:#?}"
            ));
        }
    }
    Ok(())
}

#[test]
fn trace_derived_forensics_emits_artifact_and_shrinks_a_planted_fault() {
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

    let policy = RedactionPolicy::fail_closed()
        .retain(RecordClass::FaultInjection)
        .expect("fault injection is retainable")
        .retain(RecordClass::UserPayload)
        .expect("user payload is retainable");
    let mediated_records = [
        MediatedRecord {
            class: RecordClass::UserPayload,
            payload: b"retained-workload-record".to_vec(),
        },
        MediatedRecord {
            class: RecordClass::CryptoEntropy,
            payload: b"crypto-entropy-secret-material".to_vec(),
        },
    ];
    let campaign_parent = scratch_dir("lineage-append-production-campaign");
    let experiment_root = campaign_parent.join("experiments");
    let shrink_root = campaign_parent.join("shrink");
    let output_root = campaign_parent.join("filed");
    let run = derived
        .run_and_file_replay_experiments(&TraceLdfiReplayCampaignConfig {
            contract: TraceLdfiReplayContract::DurableAppendDurability,
            budget: LdfiExperimentBudget {
                max_experiments: 64,
            },
            experiment_seed_base: 0x1df1_0000,
            latency_micros: 100,
            experiment_root: &experiment_root,
            shrink_root: &shrink_root,
            output_root: &output_root,
            redaction_policy: &policy,
            mediated_records: &mediated_records,
        })
        .expect("the production LDFI adapter must file its exact observed violation");
    let report = &run.experiment_report;
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
    let record = run
        .filed_falsification
        .as_ref()
        .expect("FoundViolation must carry its immutable filed record");
    let source_replay = record.source_replay();
    assert_eq!(source_replay.scenario, Scenario::DurableAppend);
    assert_eq!(
        source_replay.plan,
        fault
            .to_plan(source_replay.plan.seed, 100)
            .expect("found hypothesis maps exactly at its executed seed"),
        "the filed source must be the exact generated hypothesis execution"
    );
    assert_eq!(
        record.source_failure_kind(),
        FailureKind::AcknowledgedBytesLost
    );
    assert!(!record.source_execution_digest().is_empty());
    let mut shrink_attempts: Vec<String> = std::fs::read_dir(&shrink_root)
        .expect("inspect exact observed-run shrink lineage")
        .map(|entry| {
            entry
                .expect("read shrink lineage entry")
                .file_name()
                .into_string()
                .expect("shrink attempt names are ASCII")
        })
        .collect();
    shrink_attempts.sort();
    assert_eq!(
        shrink_attempts,
        ["shrink-attempt-0000"],
        "the exact observed execution is the lineage root; a hidden rerun would shift the sole candidate attempt"
    );
    assert!(matches!(
        record.outcome(),
        CampaignOutcome::Falsified { .. }
    ));
    let CampaignOutcome::Falsified {
        replay: minimized,
        failure_kind: minimized_failure,
    } = record.outcome()
    else {
        return;
    };
    assert_eq!(*minimized_failure, FailureKind::AcknowledgedBytesLost);
    assert_eq!(
        *minimized, source_replay,
        "the one-point causal hypothesis is already the exact minimal replay"
    );
    assert_eq!(
        minimized.plan.fsync_lie,
        Trigger::At(1),
        "shrinking must retain the one event that caused the failure"
    );
    assert_eq!(record.scenario_id(), "durable-append");
    assert_eq!(record.seed(), source_replay.plan.seed);
    assert!(!record.injected_faults().is_empty());
    assert_eq!(
        record.artifact_fields_asserted(),
        fgdb_sim::artifact::CONTRACT_FIELDS
    );
    assert!(record.shrink_iterations() > 0);
    assert!(record.final_reproducer_path().is_dir());
    assert!(record.bundle_path().is_file());
    assert_eq!(record.outcome().claim_class(), ClaimClass::Falsification);
    let log = record.log_lines().join("\n");
    for required in [
        "scenario_id=durable-append",
        "virtual_clock_epoch_nanos=",
        "injected_fault",
        "artifact_fields_asserted=",
        "shrink_iterations=",
        "final_reproducer_path=",
        "withheld_record_classes=",
        "verdict_class=Falsification",
    ] {
        assert!(
            log.contains(required),
            "campaign record omitted {required:?}:\n{log}"
        );
    }
    let bundle = record
        .bundle_bytes()
        .expect("the immutable record remains admissible at serialization");
    assert_eq!(
        std::fs::read(record.bundle_path()).expect("emitted campaign receipt is readable"),
        bundle,
        "the returned record and emitted receipt diverged"
    );
    assert!(bundle.starts_with(b"fgdb-sim-campaign/v1\n"));
    let hex = |bytes: &[u8]| -> Vec<u8> {
        bytes
            .iter()
            .flat_map(|byte| format!("{byte:02x}").into_bytes())
            .collect()
    };
    let retained = hex(b"retained-workload-record");
    assert!(
        bundle
            .windows(retained.len())
            .any(|window| window == retained.as_slice()),
        "the scan control did not find an explicitly retained record"
    );
    let forbidden = hex(b"crypto-entropy-secret-material");
    assert!(
        !bundle
            .windows(forbidden.len())
            .any(|window| window == forbidden.as_slice()), // ubs:ignore -- public fixture-byte absence scan, not authentication.
        "never-recordable crypto entropy escaped into the bundle"
    );
    // A latency-only hypothesis is genuinely nonviolating under the *same*
    // durability contract. This is the bounded-refutation control: it cannot
    // relabel an acknowledged loss or select an impossible target.
    let latency_prefix = format!("{FAULT_POINT_TRACE_PREFIX}latency:");
    let latency_events: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                &event.data,
                TraceData::Message(message)
                    if message.starts_with(&latency_prefix) || message == DURABLE_APPEND_OUTCOME
            )
        })
        .cloned()
        .collect();
    let latency_only = derive_fault_hypotheses(
        &latency_events,
        DURABLE_APPEND_OUTCOME,
        HittingSetBudget {
            max_depth: 1,
            max_hypotheses: 8,
        },
    )
    .expect("the latency-only trace derives a compatible hypothesis");
    assert_eq!(latency_only.hypotheses.len(), 1);
    assert_eq!(latency_only.hypotheses[0].points.len(), 1);
    assert_eq!(
        latency_only.hypotheses[0].points[0].class,
        InjectableFaultClass::Latency
    );
    let clean_parent = scratch_dir("lineage-append-clean-campaign");
    let clean_shrink = clean_parent.join("shrink");
    let clean_output = clean_parent.join("filed");
    let clean = latency_only
        .run_and_file_replay_experiments(&TraceLdfiReplayCampaignConfig {
            contract: TraceLdfiReplayContract::DurableAppendDurability,
            budget: LdfiExperimentBudget {
                max_experiments: 64,
            },
            experiment_seed_base: 0x1df1_1000,
            latency_micros: 100,
            experiment_root: &clean_parent.join("experiments"),
            shrink_root: &clean_shrink,
            output_root: &clean_output,
            redaction_policy: &policy,
            mediated_records: &mediated_records,
        })
        .expect("the clean campaign completes without filing");
    assert!(matches!(
        clean.experiment_report.status,
        LdfiExperimentStatus::RefutedUpToDepth { max_depth: 1 }
    ));
    assert!(clean.filed_falsification.is_none());
    assert!(
        !clean_shrink
            .try_exists()
            .expect("inspect clean shrink root")
    );
    assert!(
        !clean_output
            .try_exists()
            .expect("inspect clean output root")
    );

    // Failure meaning is contract-owned and exhaustive. A durability loss can
    // never be caller-laundered into a held observation, while generic I/O can
    // never mint either a refutation or a falsification.
    let contract = TraceLdfiReplayContract::DurableAppendDurability;
    assert_eq!(contract.scenario(), Scenario::DurableAppend);
    assert_eq!(
        contract.violation_kind(),
        FailureKind::AcknowledgedBytesLost
    );
    for (failure, expected) in [
        (
            FailureKind::AcknowledgedBytesLost,
            TraceLdfiFailureDisposition::Violation,
        ),
        (
            FailureKind::SyncRefused,
            TraceLdfiFailureDisposition::AdmissibleNonViolation,
        ),
        (
            FailureKind::WriteRefused,
            TraceLdfiFailureDisposition::AdmissibleNonViolation,
        ),
        (
            FailureKind::UnexpectedSurvival,
            TraceLdfiFailureDisposition::Inconclusive,
        ),
        (
            FailureKind::IoFailed,
            TraceLdfiFailureDisposition::Inconclusive,
        ),
        (
            FailureKind::CommittedNeedsRecovery,
            TraceLdfiFailureDisposition::Inconclusive,
        ),
        (
            FailureKind::RecoveryProtocolDrift,
            TraceLdfiFailureDisposition::Inconclusive,
        ),
        (
            FailureKind::AcknowledgedCommitLost,
            TraceLdfiFailureDisposition::Inconclusive,
        ),
    ] {
        assert_eq!(contract.disposition(failure), expected, "{failure:?}");
    }

    // Public-field tampering cannot manufacture an unrepresentable repeated
    // class: the sealed derivation rejects it before plan mapping or I/O.
    let mut unrepresentable = derived.clone();
    let first_point = unrepresentable.hypotheses[0].points[0];
    let extra_event = FaultEventId::new(u64::MAX);
    unrepresentable.hypotheses[0].points.push(TracedFaultPoint {
        event: extra_event,
        ordinal: first_point.ordinal + 1,
        ..first_point
    });
    unrepresentable.hypotheses[0].events.insert(extra_event);
    unrepresentable.search.hypotheses[0] = unrepresentable.hypotheses[0].events.clone();
    let invalid_parent = scratch_dir("lineage-append-invalid-mapping");
    let invalid_experiments = invalid_parent.join("experiments");
    let invalid = unrepresentable.run_and_file_replay_experiments(&TraceLdfiReplayCampaignConfig {
        contract: TraceLdfiReplayContract::DurableAppendDurability,
        budget: LdfiExperimentBudget {
            max_experiments: 64,
        },
        experiment_seed_base: 0x1df1_3000,
        latency_micros: 100,
        experiment_root: &invalid_experiments,
        shrink_root: &invalid_parent.join("shrink"),
        output_root: &invalid_parent.join("filed"),
        redaction_policy: &policy,
        mediated_records: &mediated_records,
    });
    assert!(matches!(
        invalid,
        Err(TraceLdfiCampaignError::HypothesisRegistryMismatch)
    ));
    assert!(
        !invalid_experiments
            .try_exists()
            .expect("inspect invalid experiment root")
    );

    // The public report/enrichment structure cannot be desynchronised into a
    // panic or an experiment over a hypothesis the upstream search never made.
    let mut disconnected = derived.clone();
    disconnected.hypotheses[0].events.clear();
    let disconnected_parent = scratch_dir("lineage-append-disconnected-registry");
    let disconnected_experiments = disconnected_parent.join("experiments");
    let disconnected_result =
        disconnected.run_and_file_replay_experiments(&TraceLdfiReplayCampaignConfig {
            contract: TraceLdfiReplayContract::DurableAppendDurability,
            budget: LdfiExperimentBudget { max_experiments: 1 },
            experiment_seed_base: 0x1df1_4000,
            latency_micros: 100,
            experiment_root: &disconnected_experiments,
            shrink_root: &disconnected_parent.join("shrink"),
            output_root: &disconnected_parent.join("filed"),
            redaction_policy: &policy,
            mediated_records: &mediated_records,
        });
    assert!(matches!(
        disconnected_result,
        Err(TraceLdfiCampaignError::HypothesisRegistryMismatch)
    ));
    assert!(
        !disconnected_experiments
            .try_exists()
            .expect("inspect disconnected experiment root")
    );

    // The derivation authority seals more than event-set membership. Rebinding
    // the same event to a different injector coordinate, strengthening the
    // upstream coverage status, or changing any census must all fail before a
    // replay can execute or a bounded claim can be returned.
    let mut changed_class = derived.clone();
    changed_class.hypotheses[0].points[0].class = InjectableFaultClass::BitFlip;
    let mut changed_ordinal = derived.clone();
    changed_ordinal.hypotheses[0].points[0].ordinal += 1;
    let mut changed_exhaustion = derived.clone();
    changed_exhaustion.search.exhausted = !changed_exhaustion.search.exhausted;
    let mut changed_depth = derived.clone();
    changed_depth.search.max_depth += 1;
    let mut changed_counts = derived.clone();
    changed_counts.source_event_count += 1;
    changed_counts.fault_point_count += 1;
    changed_counts.outcome_count += 1;
    for (name, mutated) in [
        ("class", changed_class),
        ("ordinal", changed_ordinal),
        ("exhaustion", changed_exhaustion),
        ("depth", changed_depth),
        ("counts", changed_counts),
    ] {
        let parent = scratch_dir(&format!("lineage-append-mutated-authority-{name}"));
        let experiments = parent.join("experiments");
        let result = mutated.run_and_file_replay_experiments(&TraceLdfiReplayCampaignConfig {
            contract: TraceLdfiReplayContract::DurableAppendDurability,
            budget: LdfiExperimentBudget { max_experiments: 1 },
            experiment_seed_base: 0x1df1_5000,
            latency_micros: 100,
            experiment_root: &experiments,
            shrink_root: &parent.join("shrink"),
            output_root: &parent.join("filed"),
            redaction_policy: &policy,
            mediated_records: &mediated_records,
        });
        assert!(matches!(
            result,
            Err(TraceLdfiCampaignError::HypothesisRegistryMismatch)
        ));
        assert!(
            !experiments
                .try_exists()
                .expect("inspect mutated-authority experiment root"),
            "{name}: mutated derivation reached filesystem execution"
        );
    }
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
