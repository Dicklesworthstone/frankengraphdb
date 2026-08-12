//! Campaign claim typing (plan §15.1 lines 1128/1140, bead fgdb-verif-sim-q97e).
//!
//! Line 1140 requires reports "structurally incapable of asserting 'verified
//! fault-free'". A test that merely checks today's three variants do not say
//! it would pass forever while someone adds a fourth that does — so the guard
//! here runs over *every* outcome and its rendering, and the interesting case
//! is `bounded_exhaustion_still_does_not_claim_absence`: the one outcome
//! strong enough to be mistaken for a proof.

use fgdb_sim::campaign::{
    CampaignOutcome, ClaimClass, EXPECTED_LIFECYCLE_CONSUMERS, EXPECTED_LIFECYCLE_COVERAGE_IDS,
    EXPECTED_LIFECYCLE_OWNER_BEADS, FORBIDDEN_CLAIMS, LIFECYCLE_COVERAGE_ROWS,
    LIFECYCLE_FIRST_REQUIRED_GATE, LifecycleCampaignEntrypoint, LifecycleConsumerCompletion,
    LifecycleCoverageState, LifecycleOwnerCompletion, LifecycleRegistryError,
    lifecycle_campaign_entrypoint, lifecycle_coverage_jsonl,
    validate_lifecycle_consumer_completion, validate_lifecycle_coverage_rows,
    validate_lifecycle_owner_completion,
};
use std::path::PathBuf;

/// Every outcome the type can express. Extended deliberately when a variant is
/// added — the guards below are only as total as this list.
fn every_outcome() -> Vec<CampaignOutcome> {
    vec![
        CampaignOutcome::Falsified {
            replay: "durable-append:0x1:512:always:never:never:none".to_string(),
            failure_kind: "AcknowledgedBytesLost".to_string(),
        },
        CampaignOutcome::NotFalsified {
            sampling_model: "uniform-over-declared-faults".to_string(),
            explored: 10_000,
        },
        CampaignOutcome::BoundedExhausted {
            model: "two-writer-one-crash".to_string(),
            states: 4_096,
        },
    ]
}

#[test]
fn no_outcome_renders_a_forbidden_claim() {
    for outcome in every_outcome() {
        let rendered = outcome.to_string().to_ascii_lowercase();
        for forbidden in FORBIDDEN_CLAIMS {
            assert!(
                !rendered.contains(forbidden),
                "{outcome:?} rendered a forbidden claim {forbidden:?}: {rendered}"
            );
        }
        assert!(
            !rendered.is_empty(),
            "an outcome that renders nothing cannot be audited for what it claims"
        );
    }
}

#[test]
fn no_claim_class_licence_promises_absence() {
    // The licence strings are what a report header quotes, so they are as
    // capable of overclaiming as the outcomes themselves.
    for class in [
        ClaimClass::Falsification,
        ClaimClass::Statistical,
        ClaimClass::BoundedFormal,
    ] {
        let licence = class.licence().to_ascii_lowercase();
        for forbidden in FORBIDDEN_CLAIMS {
            assert!(
                !licence.contains(forbidden),
                "{class:?} licence claims {forbidden:?}: {licence}"
            );
        }
    }
}

/// THE CASE THAT MATTERS. Bounded exhaustion is the outcome most easily read
/// as "we proved it clean" — it really did exhaust its model. Line 1128 says
/// that is still not absence of bugs, because the bound and the independence
/// relation are assumptions the campaign cannot discharge about itself.
#[test]
fn bounded_exhaustion_still_does_not_claim_absence() {
    let outcome = CampaignOutcome::BoundedExhausted {
        model: "two-writer-one-crash".to_string(),
        states: 4_096,
    };
    assert_eq!(outcome.claim_class(), ClaimClass::BoundedFormal);
    assert!(
        !outcome.found_counterexample(),
        "premise: this outcome found nothing, which is exactly why it is temptingly readable as a proof"
    );

    // It must name its bound in the rendering, or a reader sees "exhausted"
    // with nothing qualifying it.
    let rendered = outcome.to_string();
    assert!(
        rendered.contains("two-writer-one-crash"),
        "bounded exhaustion must name the model it exhausted: {rendered}"
    );
    assert!(
        rendered.contains("nothing is claimed"),
        "bounded exhaustion must state the limit of its claim: {rendered}"
    );
}

#[test]
fn finding_nothing_is_not_reported_as_the_same_claim_as_exhausting_a_model() {
    // §15.1: "Deterministic bounded-state completion is reported separately
    // from statistical/heuristic stopping." Same observation — no
    // counterexample — must not collapse into one claim class.
    let sampled = CampaignOutcome::NotFalsified {
        sampling_model: "uniform".to_string(),
        explored: 1,
    };
    let exhausted = CampaignOutcome::BoundedExhausted {
        model: "m".to_string(),
        states: 1,
    };
    assert_eq!(
        sampled.found_counterexample(),
        exhausted.found_counterexample()
    );
    assert_ne!(
        sampled.claim_class(),
        exhausted.claim_class(),
        "two different claims collapsed into one class"
    );
}

#[test]
fn only_falsification_asserts_anything_unconditionally() {
    let counts = every_outcome()
        .iter()
        .filter(|outcome| outcome.claim_class() == ClaimClass::Falsification)
        .count();
    assert_eq!(
        counts, 1,
        "exactly one outcome carries an unconditional claim"
    );

    // And it is the one that found a bug — the asymmetry the module is built
    // around. This is the control: without it, a `found_counterexample` that
    // always returned false would satisfy every other test here.
    for outcome in every_outcome() {
        assert_eq!(
            outcome.found_counterexample(),
            outcome.claim_class() == ClaimClass::Falsification,
            "{outcome:?}: counterexample and claim class disagree"
        );
    }
    assert!(
        every_outcome()
            .iter()
            .any(CampaignOutcome::found_counterexample),
        "no outcome reports a counterexample; every assertion above would then be vacuous"
    );
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

fn tracked_owner_completion() -> Vec<LifecycleOwnerCompletion> {
    let jsonl = std::fs::read_to_string(repository_root().join(".beads/issues.jsonl"))
        .expect("the tracked Beads export is mandatory input to this local CI check");
    EXPECTED_LIFECYCLE_OWNER_BEADS
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
                .expect("an owner Bead must carry a status");
            LifecycleOwnerCompletion {
                owner_bead: owner,
                complete: status == "closed",
            }
        })
        .collect()
}

fn tracked_consumer_completion() -> Vec<LifecycleConsumerCompletion> {
    let jsonl = std::fs::read_to_string(repository_root().join(".beads/issues.jsonl"))
        .expect("the tracked Beads export is mandatory input to this local CI check");
    EXPECTED_LIFECYCLE_CONSUMERS
        .iter()
        .map(|consumer| {
            let matching: Vec<&str> = jsonl
                .lines()
                .filter(|line| json_string_field(line, "id") == Some(*consumer))
                .collect();
            assert_eq!(
                matching.len(),
                1,
                "consumer {consumer:?} must occur exactly once in the tracked Beads export"
            );
            let status = json_string_field(matching[0], "status")
                .expect("a lifecycle consumer must carry a status");
            LifecycleConsumerCompletion {
                consumer_id: consumer,
                complete: status == "closed",
            }
        })
        .collect()
}

#[test]
fn lifecycle_matrix_is_the_exact_plan_inventory_with_closed_ownership() {
    validate_lifecycle_coverage_rows(LIFECYCLE_COVERAGE_ROWS)
        .expect("the static lifecycle matrix validates");
    assert_eq!(
        LIFECYCLE_COVERAGE_ROWS
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        EXPECTED_LIFECYCLE_COVERAGE_IDS
    );
    assert!(
        LIFECYCLE_COVERAGE_ROWS
            .iter()
            .all(
                |row| EXPECTED_LIFECYCLE_OWNER_BEADS.contains(&row.owner_bead)
                    && row.first_required_gate == LIFECYCLE_FIRST_REQUIRED_GATE
            ),
        "a lifecycle row escaped the closed owner/gate universe"
    );

    let plan = std::fs::read_to_string(
        repository_root().join("COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"),
    )
    .expect("the normative plan is present");
    for row in LIFECYCLE_COVERAGE_ROWS {
        assert!(
            plan.contains(row.source_phrase),
            "row {:?} cites a phrase absent from the normative plan: {:?}",
            row.id,
            row.source_phrase
        );
    }
}

#[test]
fn lifecycle_matrix_omission_and_cross_owner_mutations_fail_closed() {
    let without_workspace_zero: Vec<_> = LIFECYCLE_COVERAGE_ROWS
        .iter()
        .copied()
        .filter(|row| row.id != "workspace-zero-recovery")
        .collect();
    assert_eq!(
        validate_lifecycle_coverage_rows(&without_workspace_zero),
        Err(LifecycleRegistryError::InventoryLength {
            expected: EXPECTED_LIFECYCLE_COVERAGE_IDS.len(),
            actual: EXPECTED_LIFECYCLE_COVERAGE_IDS.len() - 1,
        })
    );

    let mut wrong_owner = LIFECYCLE_COVERAGE_ROWS.to_vec();
    wrong_owner[0].owner_bead = "fgdb-w2-compaction-zmkv";
    assert_eq!(
        validate_lifecycle_coverage_rows(&wrong_owner),
        Err(LifecycleRegistryError::WrongOwner {
            id: "lost-begin-accepted"
        })
    );

    let mut wrong_gate = LIFECYCLE_COVERAGE_ROWS.to_vec();
    wrong_gate[1].first_required_gate = "fgdb-gate-g3-30m";
    assert_eq!(
        validate_lifecycle_coverage_rows(&wrong_gate),
        Err(LifecycleRegistryError::WrongGate {
            id: "duplicate-begin-key"
        })
    );

    let mut missing_joint_owner = LIFECYCLE_COVERAGE_ROWS.to_vec();
    missing_joint_owner[10].required_owner_beads = &["fgdb-w2-outcome-tokens-v1w1"];
    assert_eq!(
        validate_lifecycle_coverage_rows(&missing_joint_owner),
        Err(LifecycleRegistryError::WrongRequiredOwners {
            id: "terminal-ack-release-race"
        }),
        "a cross-owner race must not be attributed to only one side of the seam"
    );
}

#[test]
fn lifecycle_activation_cannot_be_faked_with_state_or_evidence_alone() {
    let mut enabled_without_evidence = LIFECYCLE_COVERAGE_ROWS.to_vec();
    enabled_without_evidence[0].implementation_enabled = true;
    enabled_without_evidence[0].row_state = LifecycleCoverageState::Live;
    assert_eq!(
        validate_lifecycle_coverage_rows(&enabled_without_evidence),
        Err(LifecycleRegistryError::LiveMissingEvidence {
            id: "lost-begin-accepted"
        })
    );

    let mut fake_live_evidence = LIFECYCLE_COVERAGE_ROWS.to_vec();
    fake_live_evidence[0].implementation_enabled = true;
    fake_live_evidence[0].row_state = LifecycleCoverageState::Live;
    fake_live_evidence[0].coverage_evidence_ref = Some("plausible-but-unregistered-test");
    assert_eq!(
        validate_lifecycle_coverage_rows(&fake_live_evidence),
        Err(LifecycleRegistryError::LiveEvidenceUnregistered {
            id: "lost-begin-accepted"
        }),
        "a nonempty string is not executable lifecycle evidence"
    );

    let mut pending_with_evidence = LIFECYCLE_COVERAGE_ROWS.to_vec();
    pending_with_evidence[0].coverage_evidence_ref = Some("plausible-but-unregistered-test");
    assert_eq!(
        validate_lifecycle_coverage_rows(&pending_with_evidence),
        Err(LifecycleRegistryError::PendingCarriesEvidence {
            id: "lost-begin-accepted"
        })
    );
}

#[test]
fn every_live_lifecycle_evidence_reference_resolves_to_one_exact_test() {
    let root = repository_root();
    for row in LIFECYCLE_COVERAGE_ROWS
        .iter()
        .filter(|row| row.row_state == LifecycleCoverageState::Live)
    {
        let reference = row
            .coverage_evidence_ref
            .expect("metadata validation requires live evidence");
        let (path, selector) = reference.rsplit_once("::").unwrap_or(("", ""));
        assert!(
            !path.is_empty() && !selector.is_empty(),
            "lifecycle row {:?} has a non-resolvable evidence reference {reference:?}",
            row.id
        );
        let source = std::fs::read_to_string(root.join(path)).unwrap_or_default();
        let function = format!("fn {selector}(");
        assert_eq!(
            source.matches(&function).count(),
            1,
            "lifecycle row {:?} evidence {reference:?} must resolve to one exact test function",
            row.id
        );
        let function_offset = source.find(&function).unwrap_or(source.len());
        let prefix = source.get(..function_offset).unwrap_or_default();
        assert_eq!(
            prefix.lines().rev().find(|line| !line.trim().is_empty()),
            Some("#[test]"),
            "lifecycle row {:?} evidence {reference:?} is not a #[test] function",
            row.id
        );
    }
}

#[test]
fn completed_owner_without_passing_campaign_is_a_hard_failure() {
    let mut owners: Vec<_> = EXPECTED_LIFECYCLE_OWNER_BEADS
        .iter()
        .map(|owner| LifecycleOwnerCompletion {
            owner_bead: owner,
            complete: false,
        })
        .collect();
    validate_lifecycle_owner_completion(LIFECYCLE_COVERAGE_ROWS, &owners)
        .expect("pending rows are legal while their exact owners remain incomplete");

    owners[0].complete = true;
    assert_eq!(
        validate_lifecycle_owner_completion(LIFECYCLE_COVERAGE_ROWS, &owners),
        Err(LifecycleRegistryError::CompletedOwnerMissingCampaign {
            owner_bead: "fgdb-w2-txn-lifecycle-mhae",
            row_id: "lost-begin-accepted",
        })
    );
}

#[test]
fn tracked_owner_completion_cannot_outrun_lifecycle_campaign_evidence() {
    let owners = tracked_owner_completion();
    validate_lifecycle_owner_completion(LIFECYCLE_COVERAGE_ROWS, &owners).expect(
        "a tracked complete lifecycle owner has pending or unevidenced campaign rows; land its campaigns in the same change",
    );
}

#[test]
fn genesis_and_fault_torture_cannot_complete_over_a_partial_lifecycle_matrix() {
    let mut consumers: Vec<_> = EXPECTED_LIFECYCLE_CONSUMERS
        .iter()
        .map(|consumer| LifecycleConsumerCompletion {
            consumer_id: consumer,
            complete: false,
        })
        .collect();
    validate_lifecycle_consumer_completion(LIFECYCLE_COVERAGE_ROWS, &consumers)
        .expect("pending rows remain legal before their full-list consumers complete");
    consumers[0].complete = true;
    assert_eq!(
        validate_lifecycle_consumer_completion(LIFECYCLE_COVERAGE_ROWS, &consumers),
        Err(LifecycleRegistryError::CompletedConsumerMissingCampaign {
            consumer_id: "fgdb-gate-genesis-lce",
            row_id: "lost-begin-accepted",
        })
    );

    validate_lifecycle_consumer_completion(LIFECYCLE_COVERAGE_ROWS, &tracked_consumer_completion())
        .expect(
            "a tracked full-list consumer is complete while lifecycle campaigns remain pending",
        );
}

#[test]
fn lifecycle_entrypoints_delegate_every_current_row_to_its_product_owner() {
    let mut exercised = 0usize;
    for row in LIFECYCLE_COVERAGE_ROWS {
        exercised += 1;
        assert_eq!(
            lifecycle_campaign_entrypoint(row.id),
            Ok(LifecycleCampaignEntrypoint::Delegated {
                owner_bead: row.owner_bead,
                required_owner_beads: row.required_owner_beads,
                first_required_gate: row.first_required_gate,
                row_state: LifecycleCoverageState::Pending,
            }),
            "the base harness must not count pending product coverage"
        );
    }
    assert_eq!(exercised, EXPECTED_LIFECYCLE_COVERAGE_IDS.len());
    assert_eq!(
        lifecycle_campaign_entrypoint("invented-lifecycle-row"),
        Err(LifecycleRegistryError::UnknownRequestedId),
        "an invented campaign id must fail rather than borrow a real owner's delegation"
    );
}

#[test]
fn lifecycle_jsonl_emits_every_required_field_for_every_row() {
    let jsonl = lifecycle_coverage_jsonl().expect("the complete matrix serializes");
    let lines: Vec<&str> = jsonl.lines().collect();
    assert_eq!(lines.len(), EXPECTED_LIFECYCLE_COVERAGE_IDS.len());
    for (line, row) in lines.into_iter().zip(LIFECYCLE_COVERAGE_ROWS) {
        for field in [
            "id",
            "source_phrase",
            "owner_bead",
            "required_owner_beads",
            "first_required_gate",
            "implementation_enabled",
            "row_state",
            "coverage_evidence_ref",
        ] {
            assert!(
                line.contains(&format!("\"{field}\":")),
                "row {:?} omitted JSON field {field:?}: {line}",
                row.id
            );
        }
        assert!(line.contains(&format!("\"id\":\"{}\"", row.id)));
        assert!(line.contains("\"row_state\":\"pending\""));
        assert!(line.contains("\"coverage_evidence_ref\":null"));
    }
}
