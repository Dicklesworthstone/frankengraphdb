//! Campaign claim typing (plan §15.1 lines 1128/1140, bead fgdb-verif-sim-q97e).
//!
//! Line 1140 requires reports "structurally incapable of asserting 'verified
//! fault-free'". A test that merely checks today's three variants do not say
//! it would pass forever while someone adds a fourth that does — so the guard
//! here runs over *every* outcome and its rendering, and the interesting case
//! is `bounded_exhaustion_still_does_not_claim_absence`: the one outcome
//! strong enough to be mistaken for a proof.

use asupersync::lab::ExplorationBudgetConfig;
use fgdb_calibrate::exploration::{
    ExplorationAssumptionAttestation, ExplorationBudgetIdentity, ExplorationBudgetMonitor,
    ExplorationBudgetProfile, ExplorationDisposition, ExplorationSelection,
    MAX_EXPLORATION_ESTIMATION_WORK,
};
use fgdb_sim::artifact::{FailureKind, Replay, Scenario};
use fgdb_sim::campaign::{
    CampaignModelId, CampaignModelIdError, CampaignNoveltyTracker, CampaignOutcome,
    CampaignRecordError, CampaignSampleError, ClaimClass, CoverageCandidate, CoveragePolicyError,
    EXPECTED_LIFECYCLE_CONSUMERS, EXPECTED_LIFECYCLE_COVERAGE_IDS, EXPECTED_LIFECYCLE_OWNER_BEADS,
    FORBIDDEN_CLAIMS, FalsificationPipelineError, LIFECYCLE_COVERAGE_ROWS,
    LIFECYCLE_FIRST_REQUIRED_GATE, LifecycleCampaignEntrypoint, LifecycleConsumerCompletion,
    LifecycleCoverageState, LifecycleOwnerCompletion, LifecycleRegistryError,
    PrioritizedCampaignConfig, PrioritizedCampaignError, StoppingPolicyError, file_falsification,
    lifecycle_campaign_entrypoint, lifecycle_coverage_jsonl, prioritize_coverage_candidates,
    run_model_qualified_campaign, run_prioritized_model_qualified_campaign,
    validate_lifecycle_consumer_completion, validate_lifecycle_coverage_rows,
    validate_lifecycle_owner_completion,
};
use fgdb_sim::redaction::{RecordClass, RedactionPolicy};
use fgdb_sim::vfs::{FaultPlan, Trigger};
use fgdb_types::ObjectId;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Every outcome the type can express. Extended deliberately when a variant is
/// added — the guards below are only as total as this list.
fn every_outcome() -> Vec<CampaignOutcome> {
    vec![
        CampaignOutcome::Falsified {
            replay: failing_replay(),
            failure_kind: FailureKind::AcknowledgedBytesLost,
        },
        CampaignOutcome::NotFalsified {
            sampling_model: model("uniform-over-declared-faults"),
            explored: 10_000,
        },
        CampaignOutcome::BoundedExhausted {
            model: model("two-writer-one-crash"),
            states: 4_096,
        },
    ]
}

fn model(value: &str) -> CampaignModelId {
    CampaignModelId::parse(value).expect("test model id is valid")
}

fn failing_replay() -> Replay {
    failing_replay_with_seed(0xCA11)
}

fn failing_replay_with_seed(seed: u64) -> Replay {
    Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed,
            fsync_lie: Trigger::Always,
            ..FaultPlan::faultless()
        },
    }
}

fn clean_replay(seed: u64) -> Replay {
    Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            seed,
            ..FaultPlan::faultless()
        },
    }
}

fn observed_samples(
    replay: Replay,
    count: usize,
    name: &str,
) -> Vec<Result<fgdb_sim::campaign::CampaignSample, CampaignSampleError>> {
    let root = std::env::temp_dir().join(format!(
        "fgdb-sim-observed-campaign-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("observed campaign scratch");
    let mut novelty = CampaignNoveltyTracker::new();
    (0..count)
        .map(|ordinal| {
            let dir = root.join(format!("run-{ordinal:04}"));
            std::fs::create_dir_all(&dir).expect("observed run scratch");
            novelty.observe(replay.run(&dir))
        })
        .collect()
}

fn observed_existing_class_samples(
    replay: Replay,
    count: usize,
    name: &str,
) -> Vec<Result<fgdb_sim::campaign::CampaignSample, CampaignSampleError>> {
    let root = std::env::temp_dir().join(format!(
        "fgdb-sim-existing-class-campaign-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("existing-class campaign scratch");
    let mut novelty = CampaignNoveltyTracker::new();
    let priming_dir = root.join("priming-run");
    std::fs::create_dir_all(&priming_dir).expect("priming run scratch");
    novelty
        .observe({
            let run = replay.run(&priming_dir);
            assert!(
                run.failure.is_none(),
                "clean priming run failed: {:?}",
                run.failure
            );
            run
        })
        .expect("priming run is sealed");
    (0..count)
        .map(|ordinal| {
            let dir = root.join(format!("run-{ordinal:04}"));
            std::fs::create_dir_all(&dir).expect("existing-class run scratch");
            let run = replay.run(&dir);
            assert!(
                run.failure.is_none(),
                "clean observed run failed: {:?}",
                run.failure
            );
            novelty.observe(run)
        })
        .collect()
}

fn coverage_candidate(
    id: &'static str,
    covers: &'static [&'static str],
    cost: u64,
    seed: u64,
) -> CoverageCandidate {
    CoverageCandidate {
        id,
        covers,
        cost,
        replay: clean_replay(seed),
    }
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
        model: model("two-writer-one-crash"),
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
        sampling_model: model("uniform"),
        explored: 1,
    };
    let exhausted = CampaignOutcome::BoundedExhausted {
        model: model("m"),
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

#[test]
fn model_qualified_stopping_uses_the_foundation_bound_and_names_its_assumptions() {
    let config = ExplorationBudgetConfig::new(0.5, 0.9)
        .min_samples(20)
        .max_additional_runs(500);
    let identity = ExplorationBudgetIdentity::try_new(
        ObjectId([1; 32]),
        ObjectId([2; 32]),
        ObjectId([3; 32]),
        7,
        100,
        199,
        ObjectId([4; 32]),
        ObjectId([5; 32]),
    )
    .expect("identity is valid");
    let profile = ExplorationBudgetProfile::try_new(config, 100, MAX_EXPLORATION_ESTIMATION_WORK)
        .expect("profile is bounded");
    let mut monitor = ExplorationBudgetMonitor::try_new(
        identity,
        profile,
        ExplorationAssumptionAttestation::fully_supported(),
    )
    .expect("monitor is valid");
    let mut samples = observed_existing_class_samples(clean_replay(0x5100), 100, "qualified-stop");
    let stop_samples = samples.split_off(10);
    let continue_decision =
        run_model_qualified_campaign("uniform-seed-sweep-v1", &mut monitor, samples)
            .expect("the named model is admissible");
    assert!(
        continue_decision.outcome().is_none(),
        "a campaign below the minimum sample count recommended a stop: {:?}",
        continue_decision.evidence()
    );
    assert!(!continue_decision.evidence().target_met());
    assert!(continue_decision.evidence().recommended_additional_runs() > 0);

    let stop_decision =
        run_model_qualified_campaign("uniform-seed-sweep-v1", &mut monitor, stop_samples)
            .expect("the named model is admissible");
    assert!(stop_decision.evidence().target_met());
    let explored = stop_decision.evidence().total_runs();
    assert_eq!(
        stop_decision.outcome(),
        Some(&CampaignOutcome::NotFalsified {
            sampling_model: model("uniform-seed-sweep-v1"),
            explored,
        })
    );
    assert!(
        explored < 100,
        "the lazy campaign runner ignored the model-qualified stop"
    );
    let through = stop_decision
        .evidence()
        .through_sequence()
        .expect("the stop consumed at least one sample");
    let through_field = format!("through_sequence=Some({through})");
    for required in [
        "budget_oid=",
        "window_oid=",
        "regime_epoch=7",
        "first_sequence=100",
        "last_sequence=199",
        through_field.as_str(),
        "alpha_bits=",
        "min_samples=20",
        "max_additional_runs=500",
        "attest_exchangeable=true",
        "selection=CandidateDecision",
    ] {
        assert!(
            stop_decision.log_line().contains(required),
            "stopping record omitted {required:?}: {}",
            stop_decision.log_line()
        );
    }
    assert!(
        stop_decision
            .log_line()
            .contains("unexplored-space=uncharacterised")
    );
    assert_eq!(
        stop_decision
            .outcome()
            .expect("target was met")
            .claim_class(),
        ClaimClass::Statistical,
        "model-qualified stopping must never masquerade as bounded exhaustion"
    );
}

#[test]
fn a_statistical_stop_refuses_claim_prose_in_the_sampling_model() {
    let identity = ExplorationBudgetIdentity::try_new(
        ObjectId([11; 32]),
        ObjectId([12; 32]),
        ObjectId([13; 32]),
        0,
        0,
        0,
        ObjectId([14; 32]),
        ObjectId([15; 32]),
    )
    .expect("identity");
    let profile = ExplorationBudgetProfile::try_new(
        ExplorationBudgetConfig::default().min_samples(1),
        1,
        MAX_EXPLORATION_ESTIMATION_WORK,
    )
    .expect("profile");
    let mut monitor = ExplorationBudgetMonitor::try_new(
        identity,
        profile,
        ExplorationAssumptionAttestation::fully_supported(),
    )
    .expect("monitor");
    assert!(matches!(
        run_model_qualified_campaign(
            "verified fault-free",
            &mut monitor,
            observed_samples(clean_replay(0x5101), 1, "invalid-model"),
        ),
        Err(StoppingPolicyError::InvalidSamplingModel(
            CampaignModelIdError::InvalidCharacter
        ))
    ));
}

#[test]
fn a_known_counterexample_can_never_be_reported_not_falsified() {
    let identity = ExplorationBudgetIdentity::try_new(
        ObjectId([21; 32]),
        ObjectId([22; 32]),
        ObjectId([23; 32]),
        0,
        0,
        99,
        ObjectId([24; 32]),
        ObjectId([25; 32]),
    )
    .expect("identity");
    let profile = ExplorationBudgetProfile::try_new(
        ExplorationBudgetConfig::new(0.5, 0.9).min_samples(20),
        100,
        MAX_EXPLORATION_ESTIMATION_WORK,
    )
    .expect("profile");
    let mut monitor = ExplorationBudgetMonitor::try_new(
        identity,
        profile,
        ExplorationAssumptionAttestation::fully_supported(),
    )
    .expect("monitor");
    let replay = failing_replay();
    let dir = std::env::temp_dir().join(format!(
        "fgdb-sim-model-stop-counterexample-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("counterexample scratch");
    let run = replay.run(&dir);
    assert!(run.failure.is_some());
    let mut tampered = replay.run(&dir);
    tampered.virtual_clock_epoch_nanos ^= 1;
    assert!(matches!(
        CampaignNoveltyTracker::new().observe(tampered),
        Err(CampaignSampleError::ExecutionEvidenceMutated)
    ));
    let violated = CampaignNoveltyTracker::new()
        .observe(run)
        .expect("the real fixture replay falsifies its durability expectation");
    let decision = run_model_qualified_campaign(
        "uniform-seed-sweep-v1",
        &mut monitor,
        std::iter::once(Ok(violated)).chain(observed_samples(
            clean_replay(0x5102),
            100,
            "post-counterexample",
        )),
    )
    .expect("typed counterexample is an admissible campaign result");
    assert!(matches!(
        decision.outcome(),
        Some(CampaignOutcome::Falsified { .. })
    ));
    assert_eq!(monitor.total_runs(), 0, "iteration must halt at the bug");
}

#[test]
fn unsupported_stopping_assumptions_select_the_pinned_fallback() {
    let identity = ExplorationBudgetIdentity::try_new(
        ObjectId([31; 32]),
        ObjectId([32; 32]),
        ObjectId([33; 32]),
        0,
        0,
        99,
        ObjectId([34; 32]),
        ObjectId([35; 32]),
    )
    .expect("identity");
    let profile = ExplorationBudgetProfile::try_new(
        ExplorationBudgetConfig::new(0.5, 0.9).min_samples(20),
        100,
        MAX_EXPLORATION_ESTIMATION_WORK,
    )
    .expect("profile");
    let mut monitor = ExplorationBudgetMonitor::try_new(
        identity,
        profile,
        ExplorationAssumptionAttestation::new(false, true, true),
    )
    .expect("monitor");
    let decision = run_model_qualified_campaign(
        "nonexchangeable-seed-order-v1",
        &mut monitor,
        observed_existing_class_samples(clean_replay(0x5103), 100, "unsupported-assumptions"),
    )
    .expect("unsupported assumptions select fallback rather than corrupt input");
    assert!(
        decision.outcome().is_none(),
        "unsupported assumptions produced an outcome: {:?}",
        decision.evidence()
    );
    assert_eq!(
        decision.evidence().selection(),
        ExplorationSelection::PinnedFallback
    );
    assert_eq!(
        decision.evidence().disposition(),
        ExplorationDisposition::AssumptionsUnsupported
    );
}

#[test]
fn finite_ci_time_prioritizes_uncovered_classes_with_a_pinned_fallback() {
    let candidates = [
        coverage_candidate("expensive-two-class", &["d1", "d2"], 10, 0x5200),
        coverage_candidate("cheap-one-class", &["d2"], 2, 0x5201),
        coverage_candidate("already-covered", &["baseline"], 1, 0x5202),
        coverage_candidate("same-score-stable-id", &["d3"], 2, 0x5203),
    ];
    let card = prioritize_coverage_candidates(&candidates, &["baseline"], 4, 14)
        .expect("the coverage objective satisfies its declared premise");
    assert_eq!(
        card.selections()
            .iter()
            .map(|selection| selection.id)
            .collect::<Vec<_>>(),
        vec![
            "cheap-one-class",
            "same-score-stable-id",
            "expensive-two-class",
        ],
        "marginal coverage-per-cost must be recomputed, with stable ID as final tie-break"
    );
    assert_eq!(card.policy_epoch(), 4);
    assert_eq!(card.budget(), 14);
    assert_eq!(
        card.pinned_fallback_selections(),
        vec![
            "cheap-one-class",
            "same-score-stable-id",
            "expensive-two-class"
        ]
    );
    assert_eq!(
        card.selections()[2].newly_covered,
        vec!["d1"],
        "d2 was already covered by the first selection and must not be counted twice"
    );
}

#[test]
fn coverage_prioritization_refuses_invalid_premises_and_is_permutation_stable() {
    let duplicate_class = [coverage_candidate("dup-class", &["d1", "d1"], 1, 0x5210)];
    assert_eq!(
        prioritize_coverage_candidates(&duplicate_class, &[], 1, 1),
        Err(CoveragePolicyError::DuplicateCoverageClass)
    );
    let zero_cost = [coverage_candidate("zero-cost", &["d1"], 0, 0x5211)];
    assert_eq!(
        prioritize_coverage_candidates(&zero_cost, &[], 1, 1),
        Err(CoveragePolicyError::ZeroCost)
    );
    let empty_coverage = [coverage_candidate("empty-coverage", &[], 1, 0x5212)];
    assert_eq!(
        prioritize_coverage_candidates(&empty_coverage, &[], 1, 1),
        Err(CoveragePolicyError::EmptyCoverageSet)
    );
    let first = [
        coverage_candidate("b", &["d2"], 1, 0x5213),
        coverage_candidate("a", &["d1"], 1, 0x5214),
    ];
    let second = [first[1], first[0]];
    assert_eq!(
        prioritize_coverage_candidates(&first, &[], 2, 2),
        prioritize_coverage_candidates(&second, &[], 2, 2),
        "input order cannot change a replayable decision card"
    );
}

#[test]
fn prioritized_campaign_files_minimized_reproducer_or_nothing() {
    static INVOCATION: AtomicU64 = AtomicU64::new(0);
    let candidates = [
        CoverageCandidate {
            id: "bug-replay",
            covers: &["durability"],
            cost: 1,
            replay: failing_replay(),
        },
        coverage_candidate("later-clean", &["other"], 2, 0x5220),
    ];
    let identity = ExplorationBudgetIdentity::try_new(
        ObjectId([41; 32]),
        ObjectId([42; 32]),
        ObjectId([43; 32]),
        3,
        0,
        9,
        ObjectId([44; 32]),
        ObjectId([45; 32]),
    )
    .expect("identity");
    let profile = ExplorationBudgetProfile::try_new(
        ExplorationBudgetConfig::new(0.5, 0.9).min_samples(2),
        10,
        MAX_EXPLORATION_ESTIMATION_WORK,
    )
    .expect("profile");
    let mut monitor = ExplorationBudgetMonitor::try_new(
        identity,
        profile,
        ExplorationAssumptionAttestation::fully_supported(),
    )
    .expect("monitor");
    let dir = std::env::temp_dir().join(format!(
        "fgdb-sim-prioritized-campaign-{}-{}",
        std::process::id(),
        INVOCATION.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::create_dir_all(&dir).expect("campaign scratch");
    let policy = RedactionPolicy::fail_closed()
        .retain(RecordClass::FaultInjection)
        .expect("fault injection records are retainable");
    let shrink_root = dir.join("filed-shrink");
    let output_root = dir.join("filed-output");
    let mut executed = Vec::new();
    let mut observed_source_digest = None;
    let run = run_prioritized_model_qualified_campaign(
        &candidates,
        &[],
        8,
        3,
        &mut monitor,
        PrioritizedCampaignConfig::new(
            "coverage-ranked-fixture-v1",
            &shrink_root,
            &output_root,
            &policy,
            &[],
        ),
        |candidate| {
            executed.push(candidate.id);
            let candidate_dir = dir.join(candidate.id);
            std::fs::create_dir_all(&candidate_dir).expect("candidate scratch");
            let outcome = candidate.replay.run(&candidate_dir);
            observed_source_digest = outcome.replay_completeness_digest();
            outcome
        },
    )
    .expect("the decision card drives an admissible campaign");
    assert_eq!(
        run.decision_card()
            .selections()
            .iter()
            .map(|selection| selection.id)
            .collect::<Vec<_>>(),
        vec!["bug-replay", "later-clean"]
    );
    assert_eq!(executed, vec!["bug-replay"]);
    assert!(matches!(
        run.stopping().outcome(),
        Some(CampaignOutcome::Falsified { .. })
    ));
    assert_eq!(monitor.total_runs(), 0);
    let filed = run
        .filed_falsification()
        .expect("a detected counterexample must traverse the shrink/file pipeline");
    assert_eq!(filed.source_replay(), failing_replay());
    assert_eq!(
        Some(filed.source_execution_digest()),
        observed_source_digest.as_deref(),
        "filing must preserve the exact execution root that triggered the campaign"
    );
    assert_eq!(
        filed.source_failure_kind(),
        FailureKind::AcknowledgedBytesLost
    );
    assert_eq!(filed.scenario_id(), "durable-append");
    assert!(filed.shrink_iterations() > 0);
    assert!(filed.final_reproducer_path().is_dir());
    assert!(filed.bundle_path().is_file());
    assert_eq!(
        filed.bundle_path(),
        output_root.join(format!(
            "{}.campaign-receipt.fgsc",
            filed.source_execution_digest()
        )),
        "the immutable namespace is the exact sealed source execution"
    );
    assert_eq!(
        filed
            .final_reproducer_path()
            .parent()
            .and_then(std::path::Path::parent),
        Some(output_root.as_path()),
        "staging must live on the destination filesystem"
    );
    let minimized_replay = Replay {
        scenario: Scenario::DurableAppend,
        plan: FaultPlan {
            fsync_lie: Trigger::At(1),
            ..failing_replay().plan
        },
    };
    assert_eq!(
        filed.outcome(),
        &CampaignOutcome::Falsified {
            replay: minimized_replay,
            failure_kind: FailureKind::AcknowledgedBytesLost,
        }
    );
    let filed_bytes = std::fs::read(filed.bundle_path()).expect("filed receipt is readable");
    let filed_text = std::str::from_utf8(&filed_bytes).expect("receipt is canonical text");
    assert!(filed_text.contains(&format!(
        "source_replay={} source_execution_digest={} source_failure_kind=AcknowledgedBytesLost",
        failing_replay().encode(),
        observed_source_digest.as_deref().expect("source digest"),
    )));
    assert!(filed_text.contains("verdict=falsified: AcknowledgedBytesLost"));
    assert!(filed_text.contains(&minimized_replay.encode()));

    let first_bundle_path = filed.bundle_path().to_path_buf();
    let first_bundle_bytes = filed_bytes.clone();
    let second_replay = failing_replay_with_seed(0xCA12);
    let second_candidates = [CoverageCandidate {
        id: "second-bug-replay",
        covers: &["durability-second-seed"],
        cost: 1,
        replay: second_replay,
    }];
    let second = run_prioritized_model_qualified_campaign(
        &second_candidates,
        &[],
        8,
        1,
        &mut monitor,
        PrioritizedCampaignConfig::new(
            "coverage-ranked-fixture-v1",
            &dir.join("second-filed-shrink"),
            &output_root,
            &policy,
            &[],
        ),
        |candidate| {
            let candidate_dir = dir.join(candidate.id);
            std::fs::create_dir_all(&candidate_dir).expect("second candidate scratch");
            candidate.replay.run(&candidate_dir)
        },
    )
    .expect("a distinct failure shares the campaign collection without overwriting it");
    let second_filed = second
        .filed_falsification()
        .expect("the second failure files its own record");
    assert_ne!(first_bundle_path, second_filed.bundle_path());
    assert_eq!(
        second_filed.bundle_path(),
        output_root.join(format!(
            "{}.campaign-receipt.fgsc",
            second_filed.source_execution_digest()
        ))
    );
    assert!(second_filed.bundle_path().is_file());
    assert_eq!(
        std::fs::read(&first_bundle_path).expect("first receipt remains readable"),
        first_bundle_bytes,
        "a later campaign cannot overwrite earlier evidence"
    );
    let exact_retry = run_prioritized_model_qualified_campaign(
        &candidates,
        &[],
        8,
        1,
        &mut monitor,
        PrioritizedCampaignConfig::new(
            "coverage-ranked-fixture-v1",
            &dir.join("exact-retry-shrink"),
            &output_root,
            &policy,
            &[],
        ),
        |candidate| {
            let candidate_dir = dir.join("exact-retry-execution");
            std::fs::create_dir_all(&candidate_dir).expect("exact retry scratch");
            candidate.replay.run(&candidate_dir)
        },
    );
    assert!(matches!(
        exact_retry,
        Err(PrioritizedCampaignError::Filing(
            FalsificationPipelineError::Record(CampaignRecordError::FalsificationAlreadyFiled)
        ))
    ));
    assert_eq!(
        std::fs::read(&first_bundle_path).expect("first receipt survives exact retry"),
        first_bundle_bytes,
        "an exact retry cannot replace its previously filed evidence"
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&first_bundle_path)
        .and_then(|mut file| {
            use std::io::Write as _;
            file.write_all(b"campaign tampered_but_source_line_preserved=true\n")
        })
        .expect("plant a post-publication mutation");
    let tampered_retry = run_prioritized_model_qualified_campaign(
        &candidates,
        &[],
        8,
        1,
        &mut monitor,
        PrioritizedCampaignConfig::new(
            "coverage-ranked-fixture-v1",
            &dir.join("tampered-retry-shrink"),
            &output_root,
            &policy,
            &[],
        ),
        |candidate| {
            let candidate_dir = dir.join("tampered-retry-execution");
            std::fs::create_dir_all(&candidate_dir).expect("tampered retry scratch");
            candidate.replay.run(&candidate_dir)
        },
    );
    assert!(matches!(
        tampered_retry,
        Err(PrioritizedCampaignError::Filing(
            FalsificationPipelineError::Record(CampaignRecordError::FalsificationPathConflict)
        ))
    ));

    let blocked_output = dir.join("occupied-output");
    std::fs::create_dir_all(&blocked_output).expect("occupied output collection");
    let publish_failure_replay = failing_replay_with_seed(0xCA13);
    let publish_probe_dir = dir.join("publish-failure-probe");
    std::fs::create_dir_all(&publish_probe_dir).expect("publish probe scratch");
    let publish_digest = publish_failure_replay
        .run(&publish_probe_dir)
        .replay_completeness_digest()
        .expect("publish probe is sealed");
    let occupied_receipt = blocked_output.join(format!("{publish_digest}.campaign-receipt.fgsc"));
    std::fs::write(&occupied_receipt, b"foreign-incomplete-receipt")
        .expect("occupy the canonical receipt path");
    let publish_failure = file_falsification(
        publish_failure_replay,
        &dir.join("publish-failure-staging"),
        &blocked_output,
        &policy,
        &[],
    );
    assert!(matches!(
        publish_failure,
        Err(FalsificationPipelineError::Record(
            CampaignRecordError::FalsificationPathConflict
        ))
    ));
    assert_eq!(
        std::fs::read(&occupied_receipt).expect("failed publication preserves its target"),
        b"foreign-incomplete-receipt"
    );
    let recovered = file_falsification(
        publish_failure_replay,
        &dir.join("publish-retry-staging"),
        &dir.join("publish-retry-output"),
        &policy,
        &[],
    )
    .expect("a failed atomic publication does not poison a fresh retry")
    .expect("the retry files the original counterexample");
    assert!(recovered.bundle_path().is_file());

    let clean_candidates = [coverage_candidate("clean", &["clean"], 1, 0x5221)];
    let clean_identity = ExplorationBudgetIdentity::try_new(
        ObjectId([51; 32]),
        ObjectId([52; 32]),
        ObjectId([53; 32]),
        4,
        0,
        9,
        ObjectId([54; 32]),
        ObjectId([55; 32]),
    )
    .expect("clean identity");
    let clean_profile = ExplorationBudgetProfile::try_new(
        ExplorationBudgetConfig::new(0.5, 0.9).min_samples(2),
        10,
        MAX_EXPLORATION_ESTIMATION_WORK,
    )
    .expect("clean profile");
    let mut clean_monitor = ExplorationBudgetMonitor::try_new(
        clean_identity,
        clean_profile,
        ExplorationAssumptionAttestation::fully_supported(),
    )
    .expect("clean monitor");
    let clean_shrink = dir.join("clean-shrink-must-not-exist");
    let clean_output = dir.join("clean-output-must-not-exist");
    let clean = run_prioritized_model_qualified_campaign(
        &clean_candidates,
        &[],
        9,
        1,
        &mut clean_monitor,
        PrioritizedCampaignConfig::new(
            "clean-fixture-v1",
            &clean_shrink,
            &clean_output,
            &policy,
            &[],
        ),
        |candidate| {
            let candidate_dir = dir.join("clean-execution");
            std::fs::create_dir_all(&candidate_dir).expect("clean execution scratch");
            candidate.replay.run(&candidate_dir)
        },
    )
    .expect("a passing campaign remains admissible");
    assert!(clean.filed_falsification().is_none());
    assert!(
        !clean_output.exists(),
        "a passing campaign manufactured a falsification bundle"
    );
    assert!(
        !clean_shrink.exists(),
        "a passing campaign entered the shrink pipeline"
    );

    let mismatched_output = dir.join("mismatched-output-must-not-exist");
    let mismatched_shrink = dir.join("mismatched-shrink-must-not-exist");
    let mismatched = run_prioritized_model_qualified_campaign(
        &candidates,
        &[],
        8,
        3,
        &mut monitor,
        PrioritizedCampaignConfig::new(
            "coverage-ranked-fixture-v1",
            &mismatched_shrink,
            &mismatched_output,
            &policy,
            &[],
        ),
        |_candidate| {
            let mismatched_dir = dir.join("mismatched-action");
            std::fs::create_dir_all(&mismatched_dir).expect("mismatched action scratch");
            clean_replay(0xDEAD).run(&mismatched_dir)
        },
    );
    assert!(matches!(
        mismatched,
        Err(PrioritizedCampaignError::Stopping(
            StoppingPolicyError::Sample(CampaignSampleError::ActionReplayMismatch)
        ))
    ));
    assert!(!mismatched_output.exists());
    assert!(!mismatched_shrink.exists());

    let mutated_output = dir.join("mutated-output-must-not-exist");
    let mutated_shrink = dir.join("mutated-shrink-must-not-exist");
    let mutated = run_prioritized_model_qualified_campaign(
        &candidates,
        &[],
        8,
        3,
        &mut monitor,
        PrioritizedCampaignConfig::new(
            "coverage-ranked-fixture-v1",
            &mutated_shrink,
            &mutated_output,
            &policy,
            &[],
        ),
        |candidate| {
            let mutated_dir = dir.join("mutated-execution");
            std::fs::create_dir_all(&mutated_dir).expect("mutated execution scratch");
            let mut outcome = candidate.replay.run(&mutated_dir);
            outcome.virtual_clock_epoch_nanos ^= 1;
            outcome
        },
    );
    assert!(matches!(
        mutated,
        Err(PrioritizedCampaignError::Stopping(
            StoppingPolicyError::Sample(CampaignSampleError::ExecutionEvidenceMutated)
        ))
    ));
    assert!(!mutated_output.exists());
    assert!(!mutated_shrink.exists());
}

#[test]
fn submodular_premises_card_fallback_and_campaign_execution_are_governed() {
    coverage_prioritization_refuses_invalid_premises_and_is_permutation_stable();
    finite_ci_time_prioritizes_uncovered_classes_with_a_pinned_fallback();
    prioritized_campaign_files_minimized_reproducer_or_nothing();
}

#[test]
fn the_reusable_filing_path_shrinks_failures_and_files_nothing_for_a_repair() {
    let root =
        std::env::temp_dir().join(format!("fgdb-sim-automatic-filing-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("automatic filing scratch");
    let policy = RedactionPolicy::fail_closed()
        .retain(RecordClass::FaultInjection)
        .expect("fault injection records are retainable");

    let passing = file_falsification(
        Replay {
            scenario: Scenario::DurableAppend,
            plan: FaultPlan::faultless(),
        },
        &root.join("passing-shrink"),
        &root.join("passing-output"),
        &policy,
        &[],
    )
    .expect("a repaired/passing replay is an admissible pipeline result");
    assert!(
        passing.is_none(),
        "the filing gate requires the product to remain buggy"
    );

    let filed = file_falsification(
        failing_replay(),
        &root.join("failing-shrink"),
        &root.join("failing-output"),
        &policy,
        &[],
    )
    .expect("the failing replay can be shrunk and materialized")
    .expect("a counterexample files one record");
    assert_eq!(filed.scenario_id(), "durable-append");
    assert!(filed.shrink_iterations() > 0);
    assert!(filed.final_reproducer_path().is_dir());
    assert!(filed.bundle_path().is_file());
    assert!(matches!(
        filed.outcome(),
        CampaignOutcome::Falsified {
            failure_kind: FailureKind::AcknowledgedBytesLost,
            ..
        }
    ));
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
