//! What a campaign is allowed to conclude (plan §15.1, lines 1128 and 1140).
//!
//! > "DPOR is exhaustive only within the declared bounded scenario/state model
//! > and the soundness of its independence relation; broader campaigns remain
//! > falsification, not proof of bug absence." (line 1128)
//!
//! > "its reports are claim-typed falsification-only — **structurally
//! > incapable of asserting 'verified fault-free'**" (line 1140)
//!
//! MEASURED before writing this: `CampaignSummary` and `falsification` had
//! zero occurrences across `crates/`.
//!
//! # "Structurally incapable" is the whole specification
//!
//! A doc comment saying "do not claim fault-free" is not what line 1140 asks
//! for — it asks that the claim be *unrepresentable*. So [`CampaignOutcome`]
//! has no variant meaning "clean", and there is deliberately no
//! `is_bug_free()` or `passed()` for a caller to reach for. The closest a
//! campaign can come is [`CampaignOutcome::NotFalsified`], whose name is the
//! claim: nothing was found, under a named model, within a stated budget.
//!
//! The three outcomes are not three flavours of the same thing — they carry
//! **different claim classes**, and the plan requires them reported
//! separately:
//!
//! * [`ClaimClass::Falsification`] — a counterexample exists. The only
//!   outcome that proves anything unconditionally, and it proves a bug, never
//!   its absence.
//! * [`ClaimClass::Statistical`] — exploration stopped under a sampling
//!   policy. Says nothing about what was not explored.
//! * [`ClaimClass::BoundedFormal`] — the declared bounded state model was
//!   exhausted. **Still not "fault-free"**: it is exhaustive within the model
//!   and the soundness of its independence relation, and both are assumptions
//!   the campaign cannot discharge about itself.
//!
//! Reporting the third as if it were the absence of bugs is the specific
//! error line 1128 exists to forbid, which is why `BoundedExhausted` carries
//! the model it exhausted and renders it in every message.

use std::path::{Path, PathBuf};

use fgdb_calibrate::exploration::{
    ExplorationBudgetEvidence, ExplorationBudgetMonitor, ExplorationDisposition,
    ExplorationObserveError, ExplorationSelection, SequencedNovelty,
};

use crate::artifact::{CONTRACT_FIELDS, FailureKind, Replay, RunOutcome};
use crate::redaction::{Disposition, MediatedRecord, RecordClass, RedactionPolicy};
use crate::shrink::{Shrunk, shrink};
use crate::vfs::FaultEvent;

/// What kind of claim an outcome supports. Never "verified".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimClass {
    /// A counterexample was found. Unconditional, and about a bug.
    Falsification,
    /// Stopped under a named sampling policy. Silent about the unexplored.
    Statistical,
    /// A declared bounded model was exhausted. Bounded, and conditional on
    /// the model and its independence relation.
    BoundedFormal,
}

/// Opaque, report-safe identifier for a sampling model or bounded state model.
///
/// Free prose is deliberately not accepted: otherwise a caller can put a
/// forbidden conclusion such as `verified fault-free` into the model name and
/// make a claim-safe outcome render the forbidden claim verbatim.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CampaignModelId(String);

/// Why a campaign model identifier was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CampaignModelIdError {
    Empty,
    TooLong,
    InvalidCharacter,
}

impl CampaignModelId {
    /// Parse a stable machine identifier (`[A-Za-z0-9][A-Za-z0-9._:/-]*`).
    pub fn parse(value: &str) -> Result<Self, CampaignModelIdError> {
        let value = value.trim();
        if value.is_empty() {
            return Err(CampaignModelIdError::Empty);
        }
        if value.len() > 128 {
            return Err(CampaignModelIdError::TooLong);
        }
        if !value.bytes().enumerate().all(|(index, byte)| {
            byte.is_ascii_alphanumeric()
                || (index > 0 && matches!(byte, b'.' | b'_' | b':' | b'/' | b'-'))
        }) {
            return Err(CampaignModelIdError::InvalidCharacter);
        }
        Ok(Self(value.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl ClaimClass {
    /// How strong a claim this class licenses, in one line, for a report
    /// header. Kept beside the variants so a summary cannot be rendered with
    /// a stronger gloss than its class allows.
    #[must_use]
    pub const fn licence(self) -> &'static str {
        match self {
            Self::Falsification => "a counterexample was found",
            Self::Statistical => {
                "nothing found under a sampling policy; the unexplored space is not characterised"
            }
            Self::BoundedFormal => {
                "the declared bounded model was exhausted; outside that model nothing is claimed"
            }
        }
    }
}

/// The complete set of things a campaign may conclude.
///
/// There is no "clean" or "passed" variant, and adding one would be the
/// defect: line 1140 requires that the assertion be unrepresentable rather
/// than merely discouraged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CampaignOutcome {
    /// A counterexample was found, with the replay that reproduces it.
    Falsified {
        /// The typed failing replay — enough to re-run it without reparsing
        /// caller-controlled prose.
        replay: Replay,
        /// Typed failure class reproduced by that replay.
        failure_kind: FailureKind,
    },
    /// Exploration stopped without finding anything, under a named policy.
    NotFalsified {
        /// The sampling model the stop was taken under. Mandatory: a stop
        /// without a named model is an opinion.
        sampling_model: CampaignModelId,
        /// How many cases were explored.
        explored: u64,
    },
    /// The declared bounded state model was exhausted without a
    /// counterexample. Reported separately from `NotFalsified` because it is
    /// a different claim class, not a stronger version of the same one.
    BoundedExhausted {
        /// The model that was exhausted, named so the bound is legible.
        model: CampaignModelId,
        /// States covered within it.
        states: u64,
    },
}

impl CampaignOutcome {
    /// The claim class this outcome carries.
    #[must_use]
    pub const fn claim_class(&self) -> ClaimClass {
        match self {
            Self::Falsified { .. } => ClaimClass::Falsification,
            Self::NotFalsified { .. } => ClaimClass::Statistical,
            Self::BoundedExhausted { .. } => ClaimClass::BoundedFormal,
        }
    }

    /// Whether a counterexample was found.
    ///
    /// Note the asymmetry, which is deliberate: `true` is a fact about a bug.
    /// `false` is **not** a claim that none exists, and there is no method
    /// here that turns it into one.
    #[must_use]
    pub const fn found_counterexample(&self) -> bool {
        matches!(self, Self::Falsified { .. })
    }
}

impl std::fmt::Display for CampaignOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Falsified {
                replay,
                failure_kind,
            } => write!(
                f,
                "falsified: {failure_kind:?} — reproduce with {}",
                replay.encode()
            ),
            Self::NotFalsified {
                sampling_model,
                explored,
            } => write!(
                f,
                "not falsified in {explored} cases under sampling model {:?}; \
                 the unexplored space is not characterised",
                sampling_model.as_str()
            ),
            Self::BoundedExhausted { model, states } => write!(
                f,
                "bounded model {:?} exhausted over {states} states; \
                 outside that model nothing is claimed",
                model.as_str()
            ),
        }
    }
}

/// Phrases a campaign report may never contain.
///
/// Exported so the guard is a shared artifact rather than a private habit of
/// one test: any future report surface can assert against the same list
/// instead of inventing its own and missing a phrase.
pub const FORBIDDEN_CLAIMS: &[&str] = &[
    "verified fault-free",
    "fault-free",
    "no bugs",
    "bug-free",
    "proven correct",
    "proves correctness",
    "guaranteed correct",
];

// ---------------------------------------------------------------------------
// Model-qualified stopping (plan §15.1 force multiplier 2)
// ---------------------------------------------------------------------------

/// Why a statistical exploration policy could not be evaluated honestly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoppingPolicyError {
    InvalidSamplingModel(CampaignModelIdError),
    /// Infrastructure/inconclusive samples cannot be converted into evidence
    /// that an invariant was not falsified.
    InconclusiveSample {
        sequence: u64,
    },
    /// The governed monitor rejected identity, profile, sequence, or window
    /// provenance without mutating its accepted observation stream.
    ObservationRejected(ExplorationObserveError),
    /// A run's execution-root seal did not validate.
    Sample(CampaignSampleError),
}

/// Typed result of one actual campaign experiment. Its representation is
/// private so a falsification sample can only be constructed from a sealed
/// execution outcome, not caller assertions about a replay and failure kind.
#[derive(Debug, PartialEq, Eq)]
pub struct CampaignSample {
    observation: CampaignObservation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum CampaignObservation {
    InvariantHeld {
        discovered_new_class: bool,
    },
    InvariantViolated {
        replay: Replay,
        failure_kind: FailureKind,
    },
    Inconclusive,
}

/// Why an execution outcome cannot become campaign falsification evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CampaignSampleError {
    ExecutionEvidenceMutated,
    ActionReplayMismatch,
}

/// Stateful deterministic novelty oracle for replay executions.
///
/// It consumes each non-cloneable [`RunOutcome`] exactly once and derives
/// novelty from the execution's scenario, injected fault classes, and typed
/// failure class. Callers cannot manufacture a run-held observation or reuse
/// one execution as an arbitrary number of samples.
#[derive(Default)]
pub struct CampaignNoveltyTracker {
    known_classes: std::collections::BTreeSet<String>,
}

impl CampaignNoveltyTracker {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            known_classes: std::collections::BTreeSet::new(),
        }
    }

    /// Consume one sealed run and derive its safety and novelty observation.
    pub fn observe(&mut self, run: RunOutcome) -> Result<CampaignSample, CampaignSampleError> {
        if !run.execution_root_is_valid() {
            return Err(CampaignSampleError::ExecutionEvidenceMutated);
        }
        let replay = run.replay();
        if let Some(failure_kind) = run.failure.as_ref().map(|failure| failure.kind) {
            return Ok(CampaignSample {
                observation: CampaignObservation::InvariantViolated {
                    replay,
                    failure_kind,
                },
            });
        }
        let mut observed = vec![format!("scenario:{}", replay.scenario.id())];
        observed.extend(
            run.events
                .iter()
                .map(|event| format!("fault:{}", event.kind.class())),
        );
        observed.sort();
        observed.dedup();
        let mut discovered_new_class = false;
        for class in observed {
            discovered_new_class |= self.known_classes.insert(class);
        }
        Ok(CampaignSample {
            observation: CampaignObservation::InvariantHeld {
                discovered_new_class,
            },
        })
    }
}

impl CampaignSample {
    /// The run did not produce admissible safety evidence. This constructor is
    /// safe to expose because an inconclusive sample can only make the
    /// campaign refuse; it can never contribute to a no-counterexample claim.
    #[must_use]
    pub const fn inconclusive() -> Self {
        Self {
            observation: CampaignObservation::Inconclusive,
        }
    }
}

/// One deterministic, model-qualified exploration-budget decision.
///
/// `outcome` exists only when the upstream conformal novelty bound says the
/// selected target was met. Even then it is [`CampaignOutcome::NotFalsified`],
/// never bounded completion and never a claim about the unexplored space.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelQualifiedStopping {
    /// Validated model identifier; forbidden claim prose is unrepresentable.
    sampling_model: CampaignModelId,
    /// Identity-, sequence-, assumption-, profile-, and work-bound evidence
    /// supplied by the existing calibration wrapper.
    evidence: ExplorationBudgetEvidence,
    /// Statistical stop result, or `None` when exploration must continue.
    outcome: Option<CampaignOutcome>,
    /// One reconstructable report line with the assumptions and decision.
    log_line: String,
}

impl ModelQualifiedStopping {
    #[must_use]
    pub const fn sampling_model(&self) -> &CampaignModelId {
        &self.sampling_model
    }

    #[must_use]
    pub const fn evidence(&self) -> &ExplorationBudgetEvidence {
        &self.evidence
    }

    #[must_use]
    pub const fn outcome(&self) -> Option<&CampaignOutcome> {
        self.outcome.as_ref()
    }

    #[must_use]
    pub fn log_line(&self) -> &str {
        &self.log_line
    }
}

/// One runnable campaign candidate for deterministic coverage prioritization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverageCandidate {
    /// Stable tie-break identity.
    pub id: &'static str,
    /// Declared coverage classes this candidate could newly exercise.
    pub covers: &'static [&'static str],
    /// Deterministic execution cost estimate in abstract units.
    pub cost: u64,
    /// Exact replay this action executes. The prioritized runner rejects an
    /// executor that returns evidence from any other replay.
    pub replay: Replay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoveragePolicyError {
    InvalidCandidateId,
    DuplicateCandidateId,
    InvalidCoverageClass,
    DuplicateCoverageClass,
    EmptyCoverageSet,
    ZeroCost,
    ZeroBudget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageSelection {
    pub id: &'static str,
    pub cost: u64,
    pub newly_covered: Vec<&'static str>,
    /// Exact analytic interval. Learned estimates are not used here.
    pub expected_benefit_interval: (u64, u64),
}

/// Replayable decision card for one finite-CI coverage allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoverageDecisionCard {
    policy_epoch: u64,
    budget: u64,
    state_space: Vec<String>,
    candidate_actions: Vec<&'static str>,
    observed_covered: Vec<String>,
    selections: Vec<CoverageSelection>,
    /// Conservative analytic action order. Because this controller consumes
    /// no learned estimator, the executed selections are themselves the
    /// pinned fallback; the duplicate ID list makes that equality auditable.
    pinned_fallback_selections: Vec<&'static str>,
    hysteresis_and_dwell_state: &'static str,
}

impl CoverageDecisionCard {
    #[must_use]
    pub const fn policy_epoch(&self) -> u64 {
        self.policy_epoch
    }

    #[must_use]
    pub const fn budget(&self) -> u64 {
        self.budget
    }

    #[must_use]
    pub fn state_space(&self) -> &[String] {
        &self.state_space
    }

    #[must_use]
    pub fn candidate_actions(&self) -> &[&'static str] {
        &self.candidate_actions
    }

    #[must_use]
    pub fn observed_covered(&self) -> &[String] {
        &self.observed_covered
    }

    #[must_use]
    pub fn selections(&self) -> &[CoverageSelection] {
        &self.selections
    }

    #[must_use]
    pub fn pinned_fallback_selections(&self) -> &[&'static str] {
        &self.pinned_fallback_selections
    }

    #[must_use]
    pub const fn hysteresis_and_dwell_state(&self) -> &'static str {
        self.hysteresis_and_dwell_state
    }
}

/// Allocate a finite CI budget by deterministic marginal coverage per cost.
///
/// Cardinality of a set union is monotone submodular, so the premise is
/// mechanically validated by requiring unique action IDs, unique class IDs per
/// action, and strictly positive costs. Marginal benefit is recomputed after
/// each selection. The returned card records the complete state/action space,
/// exact benefit intervals, policy epoch, budget, and stable fallback.
pub fn prioritize_coverage_candidates(
    candidates: &[CoverageCandidate],
    covered: &[&str],
    policy_epoch: u64,
    budget: u64,
) -> Result<CoverageDecisionCard, CoveragePolicyError> {
    if budget == 0 {
        return Err(CoveragePolicyError::ZeroBudget);
    }
    let mut ids = std::collections::BTreeSet::new();
    let mut state_space = std::collections::BTreeSet::new();
    for candidate in candidates {
        if CampaignModelId::parse(candidate.id).is_err() {
            return Err(CoveragePolicyError::InvalidCandidateId);
        }
        if !ids.insert(candidate.id) {
            return Err(CoveragePolicyError::DuplicateCandidateId);
        }
        if candidate.cost == 0 {
            return Err(CoveragePolicyError::ZeroCost);
        }
        if candidate.covers.is_empty() {
            return Err(CoveragePolicyError::EmptyCoverageSet);
        }
        let mut local = std::collections::BTreeSet::new();
        for class in candidate.covers {
            if CampaignModelId::parse(class).is_err() {
                return Err(CoveragePolicyError::InvalidCoverageClass);
            }
            if !local.insert(*class) {
                return Err(CoveragePolicyError::DuplicateCoverageClass);
            }
            state_space.insert((*class).to_string());
        }
    }
    let mut observed_covered: Vec<String> = covered
        .iter()
        .map(|class| CampaignModelId::parse(class))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| CoveragePolicyError::InvalidCoverageClass)?
        .into_iter()
        .map(|class| class.0)
        .collect();
    observed_covered.sort();
    observed_covered.dedup();
    state_space.extend(observed_covered.iter().cloned());
    let mut covered_set: std::collections::BTreeSet<&str> = covered.iter().copied().collect();
    let mut remaining_budget = budget;
    let mut selected = std::collections::BTreeSet::new();
    let mut selections = Vec::new();
    loop {
        let best = candidates
            .iter()
            .filter(|candidate| {
                !selected.contains(candidate.id) && candidate.cost <= remaining_budget
            })
            .filter_map(|candidate| {
                let newly_covered: Vec<&'static str> = candidate
                    .covers
                    .iter()
                    .filter(|class| !covered_set.contains(**class))
                    .copied()
                    .collect();
                let benefit = newly_covered.len() as u64;
                (benefit > 0).then_some((candidate, newly_covered, benefit))
            })
            .max_by(|left, right| {
                (u128::from(left.2) * u128::from(right.0.cost))
                    .cmp(&(u128::from(right.2) * u128::from(left.0.cost)))
                    .then_with(|| right.0.cost.cmp(&left.0.cost))
                    .then_with(|| right.0.id.cmp(left.0.id))
            });
        let Some((candidate, mut newly_covered, benefit)) = best else {
            break;
        };
        newly_covered.sort_unstable();
        remaining_budget -= candidate.cost;
        selected.insert(candidate.id);
        covered_set.extend(newly_covered.iter().copied());
        selections.push(CoverageSelection {
            id: candidate.id,
            cost: candidate.cost,
            newly_covered,
            expected_benefit_interval: (benefit, benefit),
        });
    }
    let mut candidate_actions: Vec<&'static str> = ids.iter().copied().collect();
    candidate_actions.sort_unstable();
    let pinned_fallback_selections = selections.iter().map(|selection| selection.id).collect();
    Ok(CoverageDecisionCard {
        policy_epoch,
        budget,
        state_space: state_space.into_iter().collect(),
        candidate_actions,
        observed_covered,
        selections,
        pinned_fallback_selections,
        hysteresis_and_dwell_state: "one-shot-ci-allocation:not-applicable",
    })
}

/// Coverage allocation plus the actual model-qualified campaign it drove.
#[derive(Clone, Debug, PartialEq)]
pub struct PrioritizedCampaignRun {
    decision_card: CoverageDecisionCard,
    stopping: ModelQualifiedStopping,
}

impl PrioritizedCampaignRun {
    #[must_use]
    pub const fn decision_card(&self) -> &CoverageDecisionCard {
        &self.decision_card
    }

    #[must_use]
    pub const fn stopping(&self) -> &ModelQualifiedStopping {
        &self.stopping
    }
}

/// Why coverage allocation could not drive a campaign honestly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrioritizedCampaignError {
    Coverage(CoveragePolicyError),
    Stopping(StoppingPolicyError),
}

/// Allocate finite CI time, execute the selected campaigns in card order, and
/// feed their typed observations into the governed stopping monitor.
///
/// The iterator is lazy: a counterexample or admissible model-qualified stop
/// prevents later selections from running. This is the integration boundary
/// that keeps the decision card from becoming unused planning output.
pub fn run_prioritized_model_qualified_campaign(
    candidates: &[CoverageCandidate],
    covered: &[&str],
    policy_epoch: u64,
    budget: u64,
    sampling_model: &str,
    monitor: &mut ExplorationBudgetMonitor,
    mut execute: impl FnMut(&CoverageCandidate) -> RunOutcome,
) -> Result<PrioritizedCampaignRun, PrioritizedCampaignError> {
    let decision_card = prioritize_coverage_candidates(candidates, covered, policy_epoch, budget)
        .map_err(PrioritizedCampaignError::Coverage)?;
    let mut novelty = CampaignNoveltyTracker::new();
    let samples = decision_card.selections.iter().map(|selection| {
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.id == selection.id)
            .expect("a selection came from the validated candidate set");
        let run = execute(candidate);
        if run.replay() != candidate.replay {
            return Err(CampaignSampleError::ActionReplayMismatch);
        }
        novelty.observe(run)
    });
    let stopping = run_model_qualified_campaign(sampling_model, monitor, samples)
        .map_err(PrioritizedCampaignError::Stopping)?;
    Ok(PrioritizedCampaignRun {
        decision_card,
        stopping,
    })
}

/// Run typed campaign samples through the identity-bound exploration monitor.
///
/// Iteration is lazy and stops at the first admissible statistical stop or
/// counterexample. Novelty never doubles as pass/fail: only
/// a sealed, non-failing replay reaches the estimator. A violation is
/// returned as falsification, while an inconclusive run is a hard refusal.
///
/// # Errors
///
/// Returns an error rather than turning malformed provenance or an
/// inconclusive execution into a statistical no-counterexample claim.
pub fn run_model_qualified_campaign(
    sampling_model: &str,
    monitor: &mut ExplorationBudgetMonitor,
    samples: impl IntoIterator<Item = Result<CampaignSample, CampaignSampleError>>,
) -> Result<ModelQualifiedStopping, StoppingPolicyError> {
    let sampling_model = CampaignModelId::parse(sampling_model)
        .map_err(StoppingPolicyError::InvalidSamplingModel)?;
    let mut evidence = monitor.evidence();
    let mut outcome = None;
    for sample in samples {
        let sample = sample.map_err(StoppingPolicyError::Sample)?;
        let sequence = monitor.next_sequence().unwrap_or_else(|| {
            evidence
                .through_sequence()
                .unwrap_or(monitor.identity().last_sequence())
        });
        match sample.observation {
            CampaignObservation::InvariantViolated {
                replay,
                failure_kind,
            } => {
                outcome = Some(CampaignOutcome::Falsified {
                    replay,
                    failure_kind,
                });
                break;
            }
            CampaignObservation::Inconclusive => {
                return Err(StoppingPolicyError::InconclusiveSample { sequence });
            }
            CampaignObservation::InvariantHeld {
                discovered_new_class,
            } => {
                evidence = monitor
                    .observe(SequencedNovelty::new(
                        monitor.identity(),
                        monitor.profile().clone(),
                        sequence,
                        discovered_new_class,
                    ))
                    .map_err(StoppingPolicyError::ObservationRejected)?;
                if evidence.selection() == ExplorationSelection::CandidateDecision
                    && evidence.disposition() == ExplorationDisposition::CandidateSupported
                {
                    outcome = Some(CampaignOutcome::NotFalsified {
                        sampling_model: sampling_model.clone(),
                        explored: evidence.total_runs(),
                    });
                    break;
                }
            }
        }
    }
    let profile = evidence.profile();
    let attestation = evidence.assumption_attestation();
    let identity = evidence.identity();
    let log_line = format!(
        "model-qualified-stopping sampling_model={} budget_oid={:?} window_oid={:?} regime_oid={:?} regime_epoch={} first_sequence={} last_sequence={} through_sequence={:?} alpha_bits=0x{:016x} target_coverage_bits=0x{:016x} min_samples={} max_additional_runs={} max_observations={} max_estimation_work={} total_runs={} discoveries={} residual_rate_bits=0x{:016x} conformal_upper_bound_bits=0x{:016x} target_residual_rate_bits=0x{:016x} target_met={} recommended_additional_runs={} exhausted_recommendation={} attest_exchangeable={} attest_binary_novelty={} attest_existing_classes={} selection={:?} disposition={:?} unexplored-space=uncharacterised",
        sampling_model.as_str(),
        identity.budget_oid(),
        identity.window_oid(),
        identity.regime_oid(),
        identity.regime_epoch(),
        identity.first_sequence(),
        identity.last_sequence(),
        evidence.through_sequence(),
        profile.alpha_bits(),
        profile.target_coverage_bits(),
        profile.min_samples(),
        profile.max_additional_runs(),
        profile.max_observations(),
        profile.max_estimation_work(),
        evidence.total_runs(),
        evidence.discoveries(),
        evidence.residual_discovery_rate_bits(),
        evidence.conformal_upper_bound_bits(),
        evidence.target_residual_rate_bits(),
        evidence.target_met(),
        evidence.recommended_additional_runs(),
        evidence.exhausted_recommendation(),
        attestation.exchangeable_runs(),
        attestation.binary_novelty_score(),
        attestation.additional_runs_hit_existing_classes(),
        evidence.selection(),
        evidence.disposition(),
    );
    Ok(ModelQualifiedStopping {
        sampling_model,
        evidence,
        outcome,
        log_line,
    })
}

// ---------------------------------------------------------------------------
// One reconstructable record for a filed falsification
// ---------------------------------------------------------------------------

/// Why a failure artifact and its minimized reproducer cannot be filed as one
/// campaign record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CampaignRecordError {
    /// The artifact omitted a normative contract field.
    ArtifactIncomplete,
    /// The artifact invented a field outside the normative contract.
    ArtifactHasUnknownFields,
    /// Shrinking changed which typed failure the artifact records.
    ReproducerChangedFailureKind,
    /// Shrink provenance did not replay exactly as recorded.
    OriginalReplayProvenanceMismatch,
    /// The caller's output path cannot be represented without lossy display.
    NonUtf8ReproducerPath,
    /// Scratch/reproducer/receipt materialization failed.
    MaterializationFailed(std::io::ErrorKind),
    /// Never-recordable bytes reached the final serialization boundary.
    ForbiddenRecordClass,
    /// Inspectable shrink fields were changed after the shrinker sealed them.
    ShrinkProvenanceMismatch,
    /// The reported accepted plus rejected shrink attempts overflowed.
    ShrinkIterationOverflow,
}

/// Why the automatic replay-to-filed-falsification pipeline stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FalsificationPipelineError {
    /// The shrinker could not create an isolated attempt directory.
    ShrinkIo(std::io::ErrorKind),
    /// The minimized run could not be provenance-checked or materialized.
    Record(CampaignRecordError),
}

/// Complete evidence needed to reconstruct one filed falsification verdict.
///
/// This joins facts that previously lived in unrelated test output: the
/// scenario and seed, lab epoch, exact injected faults, artifact-field closure,
/// shrink work, final reproducer, and claim-typed outcome.
#[derive(Clone, Debug)]
pub struct FalsificationCampaignRecord {
    scenario_id: &'static str,
    seed: u64,
    virtual_clock_epoch_nanos: u64,
    injected_faults: Vec<FaultEvent>,
    artifact_fields_asserted: Vec<&'static str>,
    shrink_iterations: usize,
    final_reproducer_path: PathBuf,
    bundle_path: PathBuf,
    withheld_record_classes: Vec<String>,
    retained_records: Vec<MediatedRecord>,
    redaction_policy: RedactionPolicy,
    outcome: CampaignOutcome,
}

impl FalsificationCampaignRecord {
    /// Re-execute one shrink lineage and materialize its final reproducer plus
    /// structured campaign receipt.
    ///
    /// # Errors
    ///
    /// The constructor owns both replay executions. Epoch, faults, artifact,
    /// and final path therefore come from one chain instead of arbitrary
    /// caller arguments that merely happen to share a coarse failure kind.
    pub fn materialize(
        shrunk: &Shrunk,
        output_root: &Path,
        redaction_policy: &RedactionPolicy,
        mediated_records: &[MediatedRecord],
    ) -> Result<Self, CampaignRecordError> {
        if !shrunk.provenance_is_valid() {
            return Err(CampaignRecordError::ShrinkProvenanceMismatch);
        }
        let shrink_iterations = shrunk
            .steps
            .len()
            .checked_add(shrunk.rejected)
            .ok_or(CampaignRecordError::ShrinkIterationOverflow)?;
        if output_root.to_str().is_none() {
            return Err(CampaignRecordError::NonUtf8ReproducerPath);
        }
        std::fs::create_dir_all(output_root)
            .map_err(|error| CampaignRecordError::MaterializationFailed(error.kind()))?;
        let original_path = output_root.join("source-replay-validation");
        std::fs::create_dir_all(&original_path)
            .map_err(|error| CampaignRecordError::MaterializationFailed(error.kind()))?;
        let original = shrunk.original_replay.run(&original_path);
        if original.failure.as_ref() != Some(&shrunk.original_failure)
            || original.events != shrunk.original_events
        {
            return Err(CampaignRecordError::OriginalReplayProvenanceMismatch);
        }
        let final_reproducer_path = output_root.join("minimal-reproducer");
        std::fs::create_dir_all(&final_reproducer_path)
            .map_err(|error| CampaignRecordError::MaterializationFailed(error.kind()))?;
        let final_run = shrunk.replay.run(&final_reproducer_path);
        if final_run
            .events
            .iter()
            .any(|event| event.path.to_str().is_none())
        {
            return Err(CampaignRecordError::NonUtf8ReproducerPath);
        }
        if final_run.failure.as_ref() != Some(&shrunk.failure) {
            return Err(CampaignRecordError::ReproducerChangedFailureKind);
        }
        let artifact = final_run
            .artifact
            .as_ref()
            .ok_or(CampaignRecordError::ReproducerChangedFailureKind)?;
        if !artifact.unaccounted_fields().is_empty() {
            return Err(CampaignRecordError::ArtifactIncomplete);
        }
        if !artifact.unregistered_fields().is_empty() {
            return Err(CampaignRecordError::ArtifactHasUnknownFields);
        }
        if artifact.replay() != shrunk.replay || artifact.failure_kind() != shrunk.failure.kind {
            return Err(CampaignRecordError::ReproducerChangedFailureKind);
        }
        let replay = shrunk.replay;
        let retained_records = redaction_policy.filter_records(mediated_records);
        let bundle_path = output_root.join("campaign-receipt.fgsc");
        let record = Self {
            scenario_id: replay.scenario.id(),
            seed: replay.plan.seed,
            virtual_clock_epoch_nanos: final_run.virtual_clock_epoch_nanos,
            injected_faults: if redaction_policy.disposition(RecordClass::FaultInjection)
                == Disposition::Retained
            {
                final_run.events
            } else {
                Vec::new()
            },
            artifact_fields_asserted: CONTRACT_FIELDS.to_vec(),
            shrink_iterations,
            final_reproducer_path,
            bundle_path: bundle_path.clone(),
            withheld_record_classes: redaction_policy.withheld_classes(),
            retained_records,
            redaction_policy: redaction_policy.clone(),
            outcome: CampaignOutcome::Falsified {
                replay: shrunk.replay,
                failure_kind: shrunk.failure.kind,
            },
        };
        let bytes = record.bundle_bytes()?;
        std::fs::write(&bundle_path, &bytes)
            .map_err(|error| CampaignRecordError::MaterializationFailed(error.kind()))?;
        Ok(record)
    }

    #[must_use]
    pub const fn scenario_id(&self) -> &'static str {
        self.scenario_id
    }

    #[must_use]
    pub const fn seed(&self) -> u64 {
        self.seed
    }

    #[must_use]
    pub const fn virtual_clock_epoch_nanos(&self) -> u64 {
        self.virtual_clock_epoch_nanos
    }

    #[must_use]
    pub fn injected_faults(&self) -> &[FaultEvent] {
        &self.injected_faults
    }

    #[must_use]
    pub fn artifact_fields_asserted(&self) -> &[&'static str] {
        &self.artifact_fields_asserted
    }

    #[must_use]
    pub const fn shrink_iterations(&self) -> usize {
        self.shrink_iterations
    }

    #[must_use]
    pub fn final_reproducer_path(&self) -> &Path {
        &self.final_reproducer_path
    }

    #[must_use]
    pub fn bundle_path(&self) -> &Path {
        &self.bundle_path
    }

    #[must_use]
    pub const fn outcome(&self) -> &CampaignOutcome {
        &self.outcome
    }

    /// Canonical line-oriented record. A verdict reader needs no test output
    /// from another subsystem to recover what ran and what should be replayed.
    #[must_use]
    pub fn log_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "campaign scenario_id={} seed={:#x} virtual_clock_epoch_nanos={}",
            self.scenario_id, self.seed, self.virtual_clock_epoch_nanos
        )];
        for event in &self.injected_faults {
            lines.push(format!(
                "campaign injected_fault seq={} class={} path={} detail={:?}",
                event.seq,
                event.kind.class(),
                event
                    .path
                    .to_str()
                    .expect("materialize rejected non-UTF-8 event paths")
                    .escape_default(),
                event.kind,
            ));
        }
        lines.push(format!(
            "campaign artifact_fields_asserted={}",
            self.artifact_fields_asserted.join(",")
        ));
        lines.push(format!(
            "campaign shrink_iterations={} final_reproducer_path={}",
            self.shrink_iterations,
            self.final_reproducer_path
                .to_str()
                .expect("materialize rejected non-UTF-8 output paths")
                .escape_default()
        ));
        lines.push(format!(
            "campaign withheld_record_classes={}",
            self.withheld_record_classes.join(",")
        ));
        lines.push(format!(
            "campaign verdict_class={:?} verdict={} licence={}",
            self.outcome.claim_class(),
            self.outcome,
            self.outcome.claim_class().licence()
        ));
        lines
    }

    /// Versioned bytes suitable for attaching as the one structured campaign
    /// receipt. Only record classes retained by the fail-closed policy reach
    /// this output; the never-recordable crypto-entropy class can appear only
    /// by name in the withheld inventory, never as captured entropy bytes.
    pub fn bundle_bytes(&self) -> Result<Vec<u8>, CampaignRecordError> {
        if self.retained_records.iter().any(|record| {
            record.class.is_never_recordable()
                || self.redaction_policy.disposition(record.class) != Disposition::Retained
        }) {
            return Err(CampaignRecordError::ForbiddenRecordClass);
        }
        let mut output = String::from("fgdb-sim-campaign/v1\n");
        for line in self.log_lines() {
            output.push_str(&line);
            output.push('\n');
        }
        for record in &self.retained_records {
            use std::fmt::Write as _;
            let _ = writeln!(
                output,
                "campaign retained_record class={} len={} encoding=hex",
                record.class.name(),
                record.payload.len()
            );
            for byte in &record.payload {
                let _ = write!(output, "{byte:02x}");
            }
            output.push('\n');
        }
        Ok(output.into_bytes())
    }
}

/// Execute, same-kind shrink, provenance-check, redact, and file one replay.
///
/// `Ok(None)` is the passing-run outcome: no falsification artifact is filed.
/// `Ok(Some(_))` is the only filing path and necessarily contains a minimized
/// replay re-executed by [`FalsificationCampaignRecord::materialize`]. This
/// makes the reusable entrypoint repair-neutral: fixing a formerly failing
/// replay turns the next run into `None` rather than breaking the gate.
pub fn file_falsification(
    replay: Replay,
    shrink_root: &Path,
    output_root: &Path,
    redaction_policy: &RedactionPolicy,
    mediated_records: &[MediatedRecord],
) -> Result<Option<FalsificationCampaignRecord>, FalsificationPipelineError> {
    let Some(shrunk) = shrink(replay, shrink_root)
        .map_err(|error| FalsificationPipelineError::ShrinkIo(error.kind()))?
    else {
        return Ok(None);
    };
    FalsificationCampaignRecord::materialize(
        &shrunk,
        output_root,
        redaction_policy,
        mediated_records,
    )
    .map(Some)
    .map_err(FalsificationPipelineError::Record)
}

#[cfg(test)]
mod campaign_record_invariant_tests {
    use super::*;
    use crate::artifact::Scenario;
    use crate::vfs::FaultPlan;

    #[test]
    fn serialization_rechecks_the_never_recordable_class() {
        let policy = RedactionPolicy::fail_closed();
        let record = FalsificationCampaignRecord {
            scenario_id: "durable-append",
            seed: 1,
            virtual_clock_epoch_nanos: 0,
            injected_faults: Vec::new(),
            artifact_fields_asserted: CONTRACT_FIELDS.to_vec(),
            shrink_iterations: 0,
            final_reproducer_path: PathBuf::from("fixture"),
            bundle_path: PathBuf::from("fixture/receipt"),
            withheld_record_classes: policy.withheld_classes(),
            retained_records: vec![MediatedRecord {
                class: RecordClass::CryptoEntropy,
                payload: b"must-never-serialize".to_vec(),
            }],
            redaction_policy: policy,
            outcome: CampaignOutcome::Falsified {
                replay: Replay {
                    scenario: Scenario::DurableAppend,
                    plan: FaultPlan::faultless(),
                },
                failure_kind: FailureKind::AcknowledgedBytesLost,
            },
        };
        assert_eq!(
            record.bundle_bytes(),
            Err(CampaignRecordError::ForbiddenRecordClass),
            "the final serialization boundary trusted mutated retained records"
        );
    }
}

// ---------------------------------------------------------------------------
// Transaction-lifecycle campaign coverage (plan §15.1)
// ---------------------------------------------------------------------------

/// First gate that requires the complete Local lifecycle campaign matrix.
pub const LIFECYCLE_FIRST_REQUIRED_GATE: &str = "fgdb-gate-genesis-lce";

/// Consumers that may not complete while any lifecycle row remains pending.
pub const EXPECTED_LIFECYCLE_CONSUMERS: &[&str] =
    &["fgdb-gate-genesis-lce", "fgdb-verif-torture-ddcl"];

/// The only Beads allowed to activate lifecycle rows in this registry.
pub const EXPECTED_LIFECYCLE_OWNER_BEADS: &[&str] = &[
    "fgdb-w2-txn-lifecycle-mhae",
    "fgdb-w2-prepare-terminal-uhkw",
    "fgdb-w2-outcome-tokens-v1w1",
    "fgdb-w2-compaction-zmkv",
];

/// The fixed §15.1 lifecycle campaign inventory in plan order.
///
/// This list is independent of [`LIFECYCLE_COVERAGE_ROWS`]. Whole-registry
/// validation compares the two, so removing a pending row cannot silently
/// shrink the denominator.
pub const EXPECTED_LIFECYCLE_COVERAGE_IDS: &[&str] = &[
    "lost-begin-accepted",
    "duplicate-begin-key",
    "conflicting-begin-key",
    "denial-before-registration",
    "abandonment-before-registration",
    "workspace-zero-recovery",
    "successor-registered-outcome-rooting",
    "cancel-with-prior-results",
    "cancel-with-prior-workspace",
    "cancel-with-prior-grants",
    "terminal-ack-release-race",
    "autocommit-ack-release-race",
    "terminal-pending-missing-postcondition-combinations",
    "status-before-compaction",
    "status-during-compaction",
    "status-after-compaction",
    "status-after-detail-reclamation",
];

/// Whether a lifecycle campaign row is future work, executable evidence, or
/// intentionally unavailable under a selected product posture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleCoverageState {
    Pending,
    Live,
    Disabled,
}

impl LifecycleCoverageState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Live => "live",
            Self::Disabled => "disabled",
        }
    }
}

/// One machine-readable lifecycle campaign obligation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleCoverageRow {
    pub id: &'static str,
    pub source_phrase: &'static str,
    pub owner_bead: &'static str,
    pub required_owner_beads: &'static [&'static str],
    pub first_required_gate: &'static str,
    pub implementation_enabled: bool,
    pub row_state: LifecycleCoverageState,
    pub coverage_evidence_ref: Option<&'static str>,
}

const fn pending_lifecycle(
    id: &'static str,
    source_phrase: &'static str,
    owner_bead: &'static str,
    required_owner_beads: &'static [&'static str],
) -> LifecycleCoverageRow {
    LifecycleCoverageRow {
        id,
        source_phrase,
        owner_bead,
        required_owner_beads,
        first_required_gate: LIFECYCLE_FIRST_REQUIRED_GATE,
        implementation_enabled: false,
        row_state: LifecycleCoverageState::Pending,
        coverage_evidence_ref: None,
    }
}

const TXN_LIFECYCLE_OWNER: &str = "fgdb-w2-txn-lifecycle-mhae";
const PREPARE_TERMINAL_OWNER: &str = "fgdb-w2-prepare-terminal-uhkw";
const OUTCOME_TOKENS_OWNER: &str = "fgdb-w2-outcome-tokens-v1w1";
const COMPACTION_OWNER: &str = "fgdb-w2-compaction-zmkv";
const TXN_ONLY: &[&str] = &[TXN_LIFECYCLE_OWNER];
const PREPARE_ONLY: &[&str] = &[PREPARE_TERMINAL_OWNER];
const TERMINAL_ACK_SEAM: &[&str] = &[PREPARE_TERMINAL_OWNER, OUTCOME_TOKENS_OWNER];
const AUTOCOMMIT_ACK_SEAM: &[&str] = &[
    TXN_LIFECYCLE_OWNER,
    PREPARE_TERMINAL_OWNER,
    OUTCOME_TOKENS_OWNER,
];
const STATUS_COMPACTION_SEAM: &[&str] = &[OUTCOME_TOKENS_OWNER, COMPACTION_OWNER];

/// The complete lifecycle campaign matrix required by plan §15.1.
///
/// Every row is pending because none of the four product owners is complete at
/// this HEAD. Pending is data, not a skip: [`validate_lifecycle_owner_completion`]
/// makes owner completion illegal until each owned row is live and evidenced.
pub const LIFECYCLE_COVERAGE_ROWS: &[LifecycleCoverageRow] = &[
    pending_lifecycle(
        "lost-begin-accepted",
        "lost `BEGIN_ACCEPTED`",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "duplicate-begin-key",
        "duplicate/conflicting begin keys",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "conflicting-begin-key",
        "duplicate/conflicting begin keys",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "denial-before-registration",
        "denial/abandonment before registration",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "abandonment-before-registration",
        "denial/abandonment before registration",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "workspace-zero-recovery",
        "workspace-zero recovery",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "successor-registered-outcome-rooting",
        "successor Registered-outcome rooting",
        TXN_LIFECYCLE_OWNER,
        TXN_ONLY,
    ),
    pending_lifecycle(
        "cancel-with-prior-results",
        "cancel with prior results/workspace/grants",
        PREPARE_TERMINAL_OWNER,
        PREPARE_ONLY,
    ),
    pending_lifecycle(
        "cancel-with-prior-workspace",
        "cancel with prior results/workspace/grants",
        PREPARE_TERMINAL_OWNER,
        PREPARE_ONLY,
    ),
    pending_lifecycle(
        "cancel-with-prior-grants",
        "cancel with prior results/workspace/grants",
        PREPARE_TERMINAL_OWNER,
        PREPARE_ONLY,
    ),
    pending_lifecycle(
        "terminal-ack-release-race",
        "terminal/autocommit ACK/release races",
        OUTCOME_TOKENS_OWNER,
        TERMINAL_ACK_SEAM,
    ),
    pending_lifecycle(
        "autocommit-ack-release-race",
        "terminal/autocommit ACK/release races",
        OUTCOME_TOKENS_OWNER,
        AUTOCOMMIT_ACK_SEAM,
    ),
    pending_lifecycle(
        "terminal-pending-missing-postcondition-combinations",
        "every TerminalPending missing-postcondition combination",
        PREPARE_TERMINAL_OWNER,
        PREPARE_ONLY,
    ),
    pending_lifecycle(
        "status-before-compaction",
        "status before/during/after compaction and detail reclamation",
        OUTCOME_TOKENS_OWNER,
        STATUS_COMPACTION_SEAM,
    ),
    pending_lifecycle(
        "status-during-compaction",
        "status before/during/after compaction and detail reclamation",
        COMPACTION_OWNER,
        STATUS_COMPACTION_SEAM,
    ),
    pending_lifecycle(
        "status-after-compaction",
        "status before/during/after compaction and detail reclamation",
        COMPACTION_OWNER,
        STATUS_COMPACTION_SEAM,
    ),
    pending_lifecycle(
        "status-after-detail-reclamation",
        "status before/during/after compaction and detail reclamation",
        COMPACTION_OWNER,
        STATUS_COMPACTION_SEAM,
    ),
];

fn expected_lifecycle_owner(id: &str) -> Option<&'static str> {
    match id {
        "lost-begin-accepted"
        | "duplicate-begin-key"
        | "conflicting-begin-key"
        | "denial-before-registration"
        | "abandonment-before-registration"
        | "workspace-zero-recovery"
        | "successor-registered-outcome-rooting" => Some(TXN_LIFECYCLE_OWNER),
        "cancel-with-prior-results"
        | "cancel-with-prior-workspace"
        | "cancel-with-prior-grants"
        | "terminal-pending-missing-postcondition-combinations" => Some(PREPARE_TERMINAL_OWNER),
        "terminal-ack-release-race"
        | "autocommit-ack-release-race"
        | "status-before-compaction" => Some(OUTCOME_TOKENS_OWNER),
        "status-during-compaction"
        | "status-after-compaction"
        | "status-after-detail-reclamation" => Some(COMPACTION_OWNER),
        _ => None,
    }
}

fn expected_lifecycle_required_owners(id: &str) -> Option<&'static [&'static str]> {
    match id {
        "lost-begin-accepted"
        | "duplicate-begin-key"
        | "conflicting-begin-key"
        | "denial-before-registration"
        | "abandonment-before-registration"
        | "workspace-zero-recovery"
        | "successor-registered-outcome-rooting" => Some(TXN_ONLY),
        "cancel-with-prior-results"
        | "cancel-with-prior-workspace"
        | "cancel-with-prior-grants"
        | "terminal-pending-missing-postcondition-combinations" => Some(PREPARE_ONLY),
        "terminal-ack-release-race" => Some(TERMINAL_ACK_SEAM),
        "autocommit-ack-release-race" => Some(AUTOCOMMIT_ACK_SEAM),
        "status-before-compaction"
        | "status-during-compaction"
        | "status-after-compaction"
        | "status-after-detail-reclamation" => Some(STATUS_COMPACTION_SEAM),
        _ => None,
    }
}

/// Exact evidence identity registered for a live lifecycle row.
///
/// No row is live yet. Adding a live row requires adding its exact
/// `path::test_selector` here in the same change; arbitrary non-empty strings
/// cannot activate coverage or satisfy an owner/consumer completion tripwire.
fn expected_lifecycle_evidence_ref(_id: &str) -> Option<&'static str> {
    None
}

/// Why lifecycle coverage metadata is not authoritative enough to consume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleRegistryError {
    InventoryLength {
        expected: usize,
        actual: usize,
    },
    InventoryId {
        index: usize,
    },
    DuplicateId {
        id: &'static str,
    },
    UnknownBoundary {
        id: &'static str,
    },
    UnknownRequestedId,
    WrongOwner {
        id: &'static str,
    },
    WrongRequiredOwners {
        id: &'static str,
    },
    WrongGate {
        id: &'static str,
    },
    PendingImplementationEnabled {
        id: &'static str,
    },
    PendingCarriesEvidence {
        id: &'static str,
    },
    LiveImplementationDisabled {
        id: &'static str,
    },
    LiveMissingEvidence {
        id: &'static str,
    },
    LiveEvidenceUnregistered {
        id: &'static str,
    },
    LiveEvidenceMismatch {
        id: &'static str,
    },
    DisabledImplementationEnabled {
        id: &'static str,
    },
    DisabledCarriesEvidence {
        id: &'static str,
    },
    OwnerInventoryLength {
        expected: usize,
        actual: usize,
    },
    OwnerInventoryId {
        index: usize,
    },
    CompletedOwnerMissingCampaign {
        owner_bead: &'static str,
        row_id: &'static str,
    },
    ConsumerInventoryLength {
        expected: usize,
        actual: usize,
    },
    ConsumerInventoryId {
        index: usize,
    },
    CompletedConsumerMissingCampaign {
        consumer_id: &'static str,
        row_id: &'static str,
    },
}

impl std::fmt::Display for LifecycleRegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid lifecycle campaign registry: {self:?}")
    }
}

impl std::error::Error for LifecycleRegistryError {}

/// Validate one complete lifecycle matrix without consulting tracker state.
pub fn validate_lifecycle_coverage_rows(
    rows: &[LifecycleCoverageRow],
) -> Result<(), LifecycleRegistryError> {
    if rows.len() != EXPECTED_LIFECYCLE_COVERAGE_IDS.len() {
        return Err(LifecycleRegistryError::InventoryLength {
            expected: EXPECTED_LIFECYCLE_COVERAGE_IDS.len(),
            actual: rows.len(),
        });
    }
    let mut seen = std::collections::BTreeSet::new();
    for (index, (row, expected_id)) in rows.iter().zip(EXPECTED_LIFECYCLE_COVERAGE_IDS).enumerate()
    {
        if !seen.insert(row.id) {
            return Err(LifecycleRegistryError::DuplicateId { id: row.id });
        }
        if row.id != *expected_id {
            return Err(LifecycleRegistryError::InventoryId { index });
        }
        let Some(expected_owner) = expected_lifecycle_owner(row.id) else {
            return Err(LifecycleRegistryError::UnknownBoundary { id: row.id });
        };
        if row.owner_bead != expected_owner {
            return Err(LifecycleRegistryError::WrongOwner { id: row.id });
        }
        let Some(expected_required_owners) = expected_lifecycle_required_owners(row.id) else {
            return Err(LifecycleRegistryError::UnknownBoundary { id: row.id });
        };
        if row.required_owner_beads != expected_required_owners {
            return Err(LifecycleRegistryError::WrongRequiredOwners { id: row.id });
        }
        if row.first_required_gate != LIFECYCLE_FIRST_REQUIRED_GATE {
            return Err(LifecycleRegistryError::WrongGate { id: row.id });
        }
        match row.row_state {
            LifecycleCoverageState::Pending => {
                if row.implementation_enabled {
                    return Err(LifecycleRegistryError::PendingImplementationEnabled {
                        id: row.id,
                    });
                }
                if row.coverage_evidence_ref.is_some() {
                    return Err(LifecycleRegistryError::PendingCarriesEvidence { id: row.id });
                }
            }
            LifecycleCoverageState::Live => {
                if !row.implementation_enabled {
                    return Err(LifecycleRegistryError::LiveImplementationDisabled { id: row.id });
                }
                let Some(actual_evidence) =
                    row.coverage_evidence_ref.filter(|value| !value.is_empty())
                else {
                    return Err(LifecycleRegistryError::LiveMissingEvidence { id: row.id });
                };
                let Some(expected_evidence) = expected_lifecycle_evidence_ref(row.id) else {
                    return Err(LifecycleRegistryError::LiveEvidenceUnregistered { id: row.id });
                };
                if actual_evidence != expected_evidence {
                    return Err(LifecycleRegistryError::LiveEvidenceMismatch { id: row.id });
                }
            }
            LifecycleCoverageState::Disabled => {
                if row.implementation_enabled {
                    return Err(LifecycleRegistryError::DisabledImplementationEnabled {
                        id: row.id,
                    });
                }
                if row.coverage_evidence_ref.is_some() {
                    return Err(LifecycleRegistryError::DisabledCarriesEvidence { id: row.id });
                }
            }
        }
    }
    Ok(())
}

/// Tracker completion state supplied by the CI adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleOwnerCompletion {
    pub owner_bead: &'static str,
    pub complete: bool,
}

/// Completion state for a gate or verification consumer of the whole matrix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleConsumerCompletion {
    pub consumer_id: &'static str,
    pub complete: bool,
}

/// Enforce the owner-completion tripwire required by q97e.
///
/// The owner list is an exact ordered inventory, not a caller-selected subset.
/// Once an owner is complete, every row it owns must be live and carry an
/// evidence reference. Before completion, pending rows remain visible and
/// legal but never count as coverage.
pub fn validate_lifecycle_owner_completion(
    rows: &[LifecycleCoverageRow],
    owners: &[LifecycleOwnerCompletion],
) -> Result<(), LifecycleRegistryError> {
    validate_lifecycle_coverage_rows(rows)?;
    if owners.len() != EXPECTED_LIFECYCLE_OWNER_BEADS.len() {
        return Err(LifecycleRegistryError::OwnerInventoryLength {
            expected: EXPECTED_LIFECYCLE_OWNER_BEADS.len(),
            actual: owners.len(),
        });
    }
    for (index, (owner, expected)) in owners
        .iter()
        .zip(EXPECTED_LIFECYCLE_OWNER_BEADS)
        .enumerate()
    {
        if owner.owner_bead != *expected {
            return Err(LifecycleRegistryError::OwnerInventoryId { index });
        }
        if !owner.complete {
            continue;
        }
        if let Some(row) = rows.iter().find(|row| {
            row.required_owner_beads.contains(&owner.owner_bead)
                && (row.row_state != LifecycleCoverageState::Live
                    || row.coverage_evidence_ref.is_none_or(str::is_empty))
        }) {
            return Err(LifecycleRegistryError::CompletedOwnerMissingCampaign {
                owner_bead: owner.owner_bead,
                row_id: row.id,
            });
        }
    }
    Ok(())
}

/// Prevent Genesis or the fault-torture owner from completing over a partial
/// lifecycle matrix.
pub fn validate_lifecycle_consumer_completion(
    rows: &[LifecycleCoverageRow],
    consumers: &[LifecycleConsumerCompletion],
) -> Result<(), LifecycleRegistryError> {
    validate_lifecycle_coverage_rows(rows)?;
    if consumers.len() != EXPECTED_LIFECYCLE_CONSUMERS.len() {
        return Err(LifecycleRegistryError::ConsumerInventoryLength {
            expected: EXPECTED_LIFECYCLE_CONSUMERS.len(),
            actual: consumers.len(),
        });
    }
    for (index, (consumer, expected)) in consumers
        .iter()
        .zip(EXPECTED_LIFECYCLE_CONSUMERS)
        .enumerate()
    {
        if consumer.consumer_id != *expected {
            return Err(LifecycleRegistryError::ConsumerInventoryId { index });
        }
        if !consumer.complete {
            continue;
        }
        if let Some(row) = rows
            .iter()
            .find(|row| row.row_state != LifecycleCoverageState::Live)
        {
            return Err(LifecycleRegistryError::CompletedConsumerMissingCampaign {
                consumer_id: consumer.consumer_id,
                row_id: row.id,
            });
        }
    }
    Ok(())
}

/// Base-harness routing result for a lifecycle campaign row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleCampaignEntrypoint {
    Covered {
        coverage_evidence_ref: &'static str,
    },
    Delegated {
        owner_bead: &'static str,
        required_owner_beads: &'static [&'static str],
        first_required_gate: &'static str,
        row_state: LifecycleCoverageState,
    },
}

/// Resolve a lifecycle row without turning delegation into base-harness proof.
pub fn lifecycle_campaign_entrypoint(
    id: &str,
) -> Result<LifecycleCampaignEntrypoint, LifecycleRegistryError> {
    validate_lifecycle_coverage_rows(LIFECYCLE_COVERAGE_ROWS)?;
    let Some(row) = LIFECYCLE_COVERAGE_ROWS.iter().find(|row| row.id == id) else {
        return Err(LifecycleRegistryError::UnknownRequestedId);
    };
    if row.row_state == LifecycleCoverageState::Live {
        let evidence = row
            .coverage_evidence_ref
            .ok_or(LifecycleRegistryError::LiveMissingEvidence { id: row.id })?;
        Ok(LifecycleCampaignEntrypoint::Covered {
            coverage_evidence_ref: evidence,
        })
    } else {
        Ok(LifecycleCampaignEntrypoint::Delegated {
            owner_bead: row.owner_bead,
            required_owner_beads: row.required_owner_beads,
            first_required_gate: row.first_required_gate,
            row_state: row.row_state,
        })
    }
}

fn push_json_string(output: &mut String, value: &str) {
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(output, "\\u{:04x}", ch as u32);
            }
            ch => output.push(ch),
        }
    }
    output.push('"');
}

/// Serialize the complete validated matrix as one JSON object per line.
pub fn lifecycle_coverage_jsonl() -> Result<String, LifecycleRegistryError> {
    validate_lifecycle_coverage_rows(LIFECYCLE_COVERAGE_ROWS)?;
    let mut output = String::new();
    for row in LIFECYCLE_COVERAGE_ROWS {
        output.push_str("{\"id\":");
        push_json_string(&mut output, row.id);
        output.push_str(",\"source_phrase\":");
        push_json_string(&mut output, row.source_phrase);
        output.push_str(",\"owner_bead\":");
        push_json_string(&mut output, row.owner_bead);
        output.push_str(",\"required_owner_beads\":[");
        for (index, owner) in row.required_owner_beads.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            push_json_string(&mut output, owner);
        }
        output.push(']');
        output.push_str(",\"first_required_gate\":");
        push_json_string(&mut output, row.first_required_gate);
        output.push_str(",\"implementation_enabled\":");
        output.push_str(if row.implementation_enabled {
            "true"
        } else {
            "false"
        });
        output.push_str(",\"row_state\":");
        push_json_string(&mut output, row.row_state.as_str());
        output.push_str(",\"coverage_evidence_ref\":");
        match row.coverage_evidence_ref {
            Some(reference) => push_json_string(&mut output, reference),
            None => output.push_str("null"),
        }
        output.push_str("}\n");
    }
    Ok(output)
}
