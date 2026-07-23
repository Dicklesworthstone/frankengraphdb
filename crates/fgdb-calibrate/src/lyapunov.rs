//! Identity-bound wrapper around asupersync's Lyapunov governor.
//!
//! Asupersync owns the potential function and scheduling suggestion. This
//! module binds those computations to immutable FrankenGraphDB identities,
//! an exact source-stream window, explicit applicability attestations, bounded
//! retained evidence, and a deterministic fallback. A stable potential is
//! evidence for using the candidate policy only inside the declared profile;
//! it is not a safety proof.

use core::fmt;

use asupersync::obligation::lyapunov::{
    LyapunovGovernor as FoundationLyapunovGovernor, PotentialRecord, PotentialWeights,
    SchedulingSuggestion, StateSnapshot,
};
use asupersync::types::Time;
use fgdb_types::ObjectId;

/// Absolute ceiling for evidence retained by one governor instance.
pub const MAX_RETAINED_LYAPUNOV_EVIDENCE: usize = 4_096;

/// Complete immutable identity of one governed decision stream.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LyapunovGovernorIdentity {
    governor_oid: ObjectId,
    profile_oid: ObjectId,
    window_oid: ObjectId,
    regime_oid: ObjectId,
    regime_epoch: u64,
    first_sequence: u64,
    last_sequence: u64,
    observation_capacity: u64,
    candidate_oid: ObjectId,
    fallback_oid: ObjectId,
}

impl LyapunovGovernorIdentity {
    /// Constructs an identity with a finite inclusive sequence window.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        governor_oid: ObjectId,
        profile_oid: ObjectId,
        window_oid: ObjectId,
        regime_oid: ObjectId,
        regime_epoch: u64,
        first_sequence: u64,
        last_sequence: u64,
        candidate_oid: ObjectId,
        fallback_oid: ObjectId,
    ) -> Result<Self, LyapunovBuildError> {
        let distance = last_sequence.checked_sub(first_sequence).ok_or(
            LyapunovBuildError::ReversedWindow {
                first: first_sequence,
                last: last_sequence,
            },
        )?;
        let observation_capacity =
            distance
                .checked_add(1)
                .ok_or(LyapunovBuildError::WindowLengthOverflow {
                    first: first_sequence,
                    last: last_sequence,
                })?;
        if candidate_oid == fallback_oid {
            return Err(LyapunovBuildError::CandidateEqualsFallback);
        }
        Ok(Self {
            governor_oid,
            profile_oid,
            window_oid,
            regime_oid,
            regime_epoch,
            first_sequence,
            last_sequence,
            observation_capacity,
            candidate_oid,
            fallback_oid,
        })
    }

    #[must_use]
    pub const fn governor_oid(self) -> ObjectId {
        self.governor_oid
    }

    #[must_use]
    pub const fn profile_oid(self) -> ObjectId {
        self.profile_oid
    }

    #[must_use]
    pub const fn window_oid(self) -> ObjectId {
        self.window_oid
    }

    #[must_use]
    pub const fn regime_oid(self) -> ObjectId {
        self.regime_oid
    }

    #[must_use]
    pub const fn regime_epoch(self) -> u64 {
        self.regime_epoch
    }

    #[must_use]
    pub const fn first_sequence(self) -> u64 {
        self.first_sequence
    }

    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    #[must_use]
    pub const fn observation_capacity(self) -> u64 {
        self.observation_capacity
    }

    #[must_use]
    pub const fn candidate_oid(self) -> ObjectId {
        self.candidate_oid
    }

    #[must_use]
    pub const fn fallback_oid(self) -> ObjectId {
        self.fallback_oid
    }
}

/// Canonical profile for the foundation potential and wrapper guard.
///
/// Floating-point values are retained as canonical IEEE-754 bits so equality
/// and replay never depend on ambient formatting or NaN payloads.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LyapunovGovernorProfile {
    profile_oid: ObjectId,
    task_weight_bits: u64,
    obligation_age_weight_bits: u64,
    draining_region_weight_bits: u64,
    deadline_pressure_weight_bits: u64,
    potential_ceiling_bits: u64,
    maximum_increase_bits: u64,
    minimum_stable_observations: usize,
    max_retained_evidence: usize,
}

impl LyapunovGovernorProfile {
    /// Validates and canonicalizes the foundation weights and wrapper guard.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        profile_oid: ObjectId,
        weights: PotentialWeights,
        potential_ceiling: f64,
        maximum_increase: f64,
        minimum_stable_observations: usize,
        max_retained_evidence: usize,
    ) -> Result<Self, LyapunovBuildError> {
        if !weights.is_valid() {
            return Err(LyapunovBuildError::InvalidFoundationWeights);
        }
        if !potential_ceiling.is_finite() || potential_ceiling < 0.0 {
            return Err(LyapunovBuildError::InvalidPotentialCeiling {
                bits: canonical_float_bits(potential_ceiling),
            });
        }
        if !maximum_increase.is_finite() || maximum_increase < 0.0 {
            return Err(LyapunovBuildError::InvalidMaximumIncrease {
                bits: canonical_float_bits(maximum_increase),
            });
        }
        if minimum_stable_observations == 0 {
            return Err(LyapunovBuildError::ZeroMinimumStableObservations);
        }
        if max_retained_evidence == 0 {
            return Err(LyapunovBuildError::ZeroEvidenceCapacity);
        }
        if max_retained_evidence > MAX_RETAINED_LYAPUNOV_EVIDENCE {
            return Err(LyapunovBuildError::EvidenceCapacityTooLarge {
                actual: max_retained_evidence,
                maximum: MAX_RETAINED_LYAPUNOV_EVIDENCE,
            });
        }
        Ok(Self {
            profile_oid,
            task_weight_bits: canonical_float_bits(weights.w_tasks),
            obligation_age_weight_bits: canonical_float_bits(weights.w_obligation_age),
            draining_region_weight_bits: canonical_float_bits(weights.w_draining_regions),
            deadline_pressure_weight_bits: canonical_float_bits(weights.w_deadline_pressure),
            potential_ceiling_bits: canonical_float_bits(potential_ceiling),
            maximum_increase_bits: canonical_float_bits(maximum_increase),
            minimum_stable_observations,
            max_retained_evidence,
        })
    }

    #[must_use]
    pub const fn profile_oid(self) -> ObjectId {
        self.profile_oid
    }

    #[must_use]
    pub const fn task_weight_bits(self) -> u64 {
        self.task_weight_bits
    }

    #[must_use]
    pub const fn obligation_age_weight_bits(self) -> u64 {
        self.obligation_age_weight_bits
    }

    #[must_use]
    pub const fn draining_region_weight_bits(self) -> u64 {
        self.draining_region_weight_bits
    }

    #[must_use]
    pub const fn deadline_pressure_weight_bits(self) -> u64 {
        self.deadline_pressure_weight_bits
    }

    #[must_use]
    pub const fn potential_ceiling_bits(self) -> u64 {
        self.potential_ceiling_bits
    }

    #[must_use]
    pub const fn maximum_increase_bits(self) -> u64 {
        self.maximum_increase_bits
    }

    #[must_use]
    pub const fn minimum_stable_observations(self) -> usize {
        self.minimum_stable_observations
    }

    #[must_use]
    pub const fn max_retained_evidence(self) -> usize {
        self.max_retained_evidence
    }

    fn foundation_weights(self) -> PotentialWeights {
        PotentialWeights {
            w_tasks: f64::from_bits(self.task_weight_bits),
            w_obligation_age: f64::from_bits(self.obligation_age_weight_bits),
            w_draining_regions: f64::from_bits(self.draining_region_weight_bits),
            w_deadline_pressure: f64::from_bits(self.deadline_pressure_weight_bits),
        }
    }

    fn potential_ceiling(self) -> f64 {
        f64::from_bits(self.potential_ceiling_bits)
    }

    fn maximum_increase(self) -> f64 {
        f64::from_bits(self.maximum_increase_bits)
    }
}

/// Identity, profile, resource, or foundation-construction failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LyapunovBuildError {
    ReversedWindow {
        first: u64,
        last: u64,
    },
    WindowLengthOverflow {
        first: u64,
        last: u64,
    },
    CandidateEqualsFallback,
    InvalidFoundationWeights,
    InvalidPotentialCeiling {
        bits: u64,
    },
    InvalidMaximumIncrease {
        bits: u64,
    },
    ZeroMinimumStableObservations,
    ZeroEvidenceCapacity,
    EvidenceCapacityTooLarge {
        actual: usize,
        maximum: usize,
    },
    ProfileOidMismatch {
        identity: ObjectId,
        profile: ObjectId,
    },
    EvidenceAllocationFailed {
        requested: usize,
    },
}

impl fmt::Display for LyapunovBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::ReversedWindow { first, last } => {
                write!(formatter, "Lyapunov window {first}..={last} is reversed")
            }
            Self::WindowLengthOverflow { first, last } => write!(
                formatter,
                "Lyapunov window {first}..={last} has an unrepresentable length"
            ),
            Self::CandidateEqualsFallback => {
                formatter.write_str("Lyapunov candidate and fallback identities must differ")
            }
            Self::InvalidFoundationWeights => {
                formatter.write_str("asupersync rejected the Lyapunov weights")
            }
            Self::InvalidPotentialCeiling { bits } => write!(
                formatter,
                "Lyapunov potential ceiling 0x{bits:016x} is invalid"
            ),
            Self::InvalidMaximumIncrease { bits } => write!(
                formatter,
                "Lyapunov maximum increase 0x{bits:016x} is invalid"
            ),
            Self::ZeroMinimumStableObservations => {
                formatter.write_str("minimum stable observations must be greater than zero")
            }
            Self::ZeroEvidenceCapacity => {
                formatter.write_str("Lyapunov evidence capacity must be greater than zero")
            }
            Self::EvidenceCapacityTooLarge { actual, maximum } => write!(
                formatter,
                "Lyapunov evidence capacity {actual} exceeds ceiling {maximum}"
            ),
            Self::ProfileOidMismatch { identity, profile } => write!(
                formatter,
                "identity profile OID {identity:?} does not match profile OID {profile:?}"
            ),
            Self::EvidenceAllocationFailed { requested } => write!(
                formatter,
                "could not reserve {requested} Lyapunov evidence records"
            ),
        }
    }
}

impl std::error::Error for LyapunovBuildError {}

/// Explicit applicability conditions for using the candidate policy.
///
/// The wrapper still records a foundation projection when any condition is
/// false, but selects the pinned fallback.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LyapunovAssumptionAttestation {
    snapshot_complete: bool,
    potential_model_applicable: bool,
    candidate_effect_covered: bool,
}

impl LyapunovAssumptionAttestation {
    #[must_use]
    pub const fn new(
        snapshot_complete: bool,
        potential_model_applicable: bool,
        candidate_effect_covered: bool,
    ) -> Self {
        Self {
            snapshot_complete,
            potential_model_applicable,
            candidate_effect_covered,
        }
    }

    #[must_use]
    pub const fn fully_supported() -> Self {
        Self::new(true, true, true)
    }

    #[must_use]
    pub const fn snapshot_complete(self) -> bool {
        self.snapshot_complete
    }

    #[must_use]
    pub const fn potential_model_applicable(self) -> bool {
        self.potential_model_applicable
    }

    #[must_use]
    pub const fn candidate_effect_covered(self) -> bool {
        self.candidate_effect_covered
    }

    #[must_use]
    pub const fn supports_candidate(self) -> bool {
        self.snapshot_complete && self.potential_model_applicable && self.candidate_effect_covered
    }
}

/// Deterministic, replayable projection of a foundation state snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LyapunovStateProjection {
    time_nanos: u64,
    live_tasks: u32,
    pending_obligations: u32,
    obligation_age_sum_ns: u64,
    draining_regions: u32,
    deadline_pressure_bits: u64,
    pending_send_permits: u32,
    pending_acks: u32,
    pending_leases: u32,
    pending_io_ops: u32,
    cancel_requested_tasks: u32,
    cancelling_tasks: u32,
    finalizing_tasks: u32,
    ready_queue_depth: u32,
}

impl LyapunovStateProjection {
    /// Captures every public foundation snapshot field without retaining an
    /// alias to caller-owned state.
    #[must_use]
    pub fn from_foundation(snapshot: &StateSnapshot) -> Self {
        Self {
            time_nanos: snapshot.time.as_nanos(),
            live_tasks: snapshot.live_tasks,
            pending_obligations: snapshot.pending_obligations,
            obligation_age_sum_ns: snapshot.obligation_age_sum_ns,
            draining_regions: snapshot.draining_regions,
            deadline_pressure_bits: canonical_float_bits(snapshot.deadline_pressure),
            pending_send_permits: snapshot.pending_send_permits,
            pending_acks: snapshot.pending_acks,
            pending_leases: snapshot.pending_leases,
            pending_io_ops: snapshot.pending_io_ops,
            cancel_requested_tasks: snapshot.cancel_requested_tasks,
            cancelling_tasks: snapshot.cancelling_tasks,
            finalizing_tasks: snapshot.finalizing_tasks,
            ready_queue_depth: snapshot.ready_queue_depth,
        }
    }

    #[must_use]
    pub const fn time_nanos(self) -> u64 {
        self.time_nanos
    }

    #[must_use]
    pub const fn deadline_pressure_bits(self) -> u64 {
        self.deadline_pressure_bits
    }

    /// Reconstructs the exact normalized foundation input for replay.
    #[must_use]
    pub fn foundation_snapshot(self) -> StateSnapshot {
        StateSnapshot {
            time: Time::from_nanos(self.time_nanos),
            live_tasks: self.live_tasks,
            pending_obligations: self.pending_obligations,
            obligation_age_sum_ns: self.obligation_age_sum_ns,
            draining_regions: self.draining_regions,
            deadline_pressure: f64::from_bits(self.deadline_pressure_bits),
            pending_send_permits: self.pending_send_permits,
            pending_acks: self.pending_acks,
            pending_leases: self.pending_leases,
            pending_io_ops: self.pending_io_ops,
            cancel_requested_tasks: self.cancel_requested_tasks,
            cancelling_tasks: self.cancelling_tasks,
            finalizing_tasks: self.finalizing_tasks,
            ready_queue_depth: self.ready_queue_depth,
        }
    }
}

/// One immutable, exactly sequenced governor input.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SequencedLyapunovSnapshot {
    identity: LyapunovGovernorIdentity,
    profile: LyapunovGovernorProfile,
    stream_sequence: u64,
    snapshot: LyapunovStateProjection,
    assumptions: LyapunovAssumptionAttestation,
}

impl SequencedLyapunovSnapshot {
    #[must_use]
    pub const fn new(
        identity: LyapunovGovernorIdentity,
        profile: LyapunovGovernorProfile,
        stream_sequence: u64,
        snapshot: LyapunovStateProjection,
        assumptions: LyapunovAssumptionAttestation,
    ) -> Self {
        Self {
            identity,
            profile,
            stream_sequence,
            snapshot,
            assumptions,
        }
    }

    #[must_use]
    pub const fn identity(self) -> LyapunovGovernorIdentity {
        self.identity
    }

    #[must_use]
    pub const fn profile(self) -> LyapunovGovernorProfile {
        self.profile
    }

    #[must_use]
    pub const fn stream_sequence(self) -> u64 {
        self.stream_sequence
    }

    #[must_use]
    pub const fn snapshot(self) -> LyapunovStateProjection {
        self.snapshot
    }

    #[must_use]
    pub const fn assumptions(self) -> LyapunovAssumptionAttestation {
        self.assumptions
    }
}

/// Stable projection of asupersync's scheduling suggestion vocabulary.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LyapunovSuggestion {
    DrainObligations,
    DrainRegions,
    MeetDeadlines,
    NoPreference,
}

/// Policy selected after applying the wrapper guard.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LyapunovSelection {
    Candidate,
    PinnedFallback,
}

/// Canonical evidence retained for one accepted snapshot.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct LyapunovEvidence {
    identity: LyapunovGovernorIdentity,
    profile: LyapunovGovernorProfile,
    stream_sequence: u64,
    observation_count: u64,
    snapshot: LyapunovStateProjection,
    assumptions: LyapunovAssumptionAttestation,
    potential_bits: u64,
    task_component_bits: u64,
    obligation_component_bits: u64,
    region_component_bits: u64,
    deadline_component_bits: u64,
    potential_delta_bits: Option<u64>,
    suggestion: LyapunovSuggestion,
    within_potential_ceiling: bool,
    step_guard_satisfied: bool,
    consecutive_stable_observations: u64,
    candidate_authorized: bool,
    selection: LyapunovSelection,
}

impl LyapunovEvidence {
    #[must_use]
    pub const fn identity(&self) -> LyapunovGovernorIdentity {
        self.identity
    }

    #[must_use]
    pub const fn profile(&self) -> LyapunovGovernorProfile {
        self.profile
    }

    #[must_use]
    pub const fn stream_sequence(&self) -> u64 {
        self.stream_sequence
    }

    #[must_use]
    pub const fn observation_count(&self) -> u64 {
        self.observation_count
    }

    #[must_use]
    pub const fn snapshot(&self) -> LyapunovStateProjection {
        self.snapshot
    }

    #[must_use]
    pub const fn assumptions(&self) -> LyapunovAssumptionAttestation {
        self.assumptions
    }

    #[must_use]
    pub const fn potential_bits(&self) -> u64 {
        self.potential_bits
    }

    #[must_use]
    pub const fn task_component_bits(&self) -> u64 {
        self.task_component_bits
    }

    #[must_use]
    pub const fn obligation_component_bits(&self) -> u64 {
        self.obligation_component_bits
    }

    #[must_use]
    pub const fn region_component_bits(&self) -> u64 {
        self.region_component_bits
    }

    #[must_use]
    pub const fn deadline_component_bits(&self) -> u64 {
        self.deadline_component_bits
    }

    #[must_use]
    pub const fn potential_delta_bits(&self) -> Option<u64> {
        self.potential_delta_bits
    }

    #[must_use]
    pub const fn suggestion(&self) -> LyapunovSuggestion {
        self.suggestion
    }

    #[must_use]
    pub const fn within_potential_ceiling(&self) -> bool {
        self.within_potential_ceiling
    }

    #[must_use]
    pub const fn step_guard_satisfied(&self) -> bool {
        self.step_guard_satisfied
    }

    #[must_use]
    pub const fn consecutive_stable_observations(&self) -> u64 {
        self.consecutive_stable_observations
    }

    #[must_use]
    pub const fn candidate_authorized(&self) -> bool {
        self.candidate_authorized
    }

    #[must_use]
    pub const fn selection(&self) -> LyapunovSelection {
        self.selection
    }

    #[must_use]
    pub const fn selected_policy_oid(&self) -> ObjectId {
        match self.selection {
            LyapunovSelection::Candidate => self.identity.candidate_oid,
            LyapunovSelection::PinnedFallback => self.identity.fallback_oid,
        }
    }
}

/// Numeric component whose foundation projection was outside its domain.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LyapunovNumericField {
    TotalPotential,
    TaskComponent,
    ObligationComponent,
    RegionComponent,
    DeadlineComponent,
}

impl fmt::Display for LyapunovNumericField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::TotalPotential => "total potential",
            Self::TaskComponent => "task component",
            Self::ObligationComponent => "obligation component",
            Self::RegionComponent => "region component",
            Self::DeadlineComponent => "deadline component",
        })
    }
}

/// Non-mutating rejection of one proposed observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LyapunovObserveError {
    IdentityMismatch,
    ProfileMismatch,
    UnexpectedSequence {
        expected: u64,
        actual: u64,
    },
    WindowComplete {
        last: u64,
    },
    InvalidDeadlinePressure {
        bits: u64,
    },
    ObligationAgeWithoutPendingObligations {
        age_ns: u64,
    },
    ObligationBreakdownExceedsTotal {
        classified: u64,
        total: u32,
    },
    CancellationBreakdownExceedsLiveTasks {
        cancelling: u64,
        live_tasks: u32,
    },
    InvalidFoundationProjection {
        field: LyapunovNumericField,
        bits: u64,
    },
    CounterExhausted,
}

impl fmt::Display for LyapunovObserveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::IdentityMismatch => formatter.write_str("Lyapunov identity does not match"),
            Self::ProfileMismatch => formatter.write_str("Lyapunov profile does not match"),
            Self::UnexpectedSequence { expected, actual } => write!(
                formatter,
                "expected Lyapunov sequence {expected}, got {actual}"
            ),
            Self::WindowComplete { last } => {
                write!(formatter, "Lyapunov window ended at {last}")
            }
            Self::InvalidDeadlinePressure { bits } => write!(
                formatter,
                "deadline pressure 0x{bits:016x} is not finite and non-negative"
            ),
            Self::ObligationAgeWithoutPendingObligations { age_ns } => write!(
                formatter,
                "obligation age sum {age_ns}ns is nonzero with no pending obligations"
            ),
            Self::ObligationBreakdownExceedsTotal { classified, total } => write!(
                formatter,
                "classified obligations {classified} exceed total pending obligations {total}"
            ),
            Self::CancellationBreakdownExceedsLiveTasks {
                cancelling,
                live_tasks,
            } => write!(
                formatter,
                "cancellation-phase tasks {cancelling} exceed live tasks {live_tasks}"
            ),
            Self::InvalidFoundationProjection { field, bits } => write!(
                formatter,
                "foundation {field} 0x{bits:016x} is not finite and non-negative"
            ),
            Self::CounterExhausted => formatter.write_str("Lyapunov counter is exhausted"),
        }
    }
}

impl std::error::Error for LyapunovObserveError {}

/// Exactly sequenced, bounded wrapper around asupersync's governor.
///
/// This mutable state intentionally does not implement `Clone`.
#[derive(Debug)]
pub struct DecisionLyapunovGovernor {
    identity: LyapunovGovernorIdentity,
    profile: LyapunovGovernorProfile,
    core: FoundationLyapunovGovernor,
    next_sequence: Option<u64>,
    accepted_observation_count: u64,
    previous_potential: Option<f64>,
    consecutive_stable_observations: u64,
    evidence: Vec<LyapunovEvidence>,
}

impl DecisionLyapunovGovernor {
    /// Constructs a governor after validating all foundation preconditions.
    pub fn try_new(
        identity: LyapunovGovernorIdentity,
        profile: LyapunovGovernorProfile,
    ) -> Result<Self, LyapunovBuildError> {
        if identity.profile_oid != profile.profile_oid {
            return Err(LyapunovBuildError::ProfileOidMismatch {
                identity: identity.profile_oid,
                profile: profile.profile_oid,
            });
        }
        let weights = profile.foundation_weights();
        if !weights.is_valid() {
            return Err(LyapunovBuildError::InvalidFoundationWeights);
        }
        let mut evidence = Vec::new();
        evidence
            .try_reserve_exact(profile.max_retained_evidence)
            .map_err(|_| LyapunovBuildError::EvidenceAllocationFailed {
                requested: profile.max_retained_evidence,
            })?;

        Ok(Self {
            identity,
            profile,
            core: FoundationLyapunovGovernor::new(weights),
            next_sequence: Some(identity.first_sequence),
            accepted_observation_count: 0,
            previous_potential: None,
            consecutive_stable_observations: 0,
            evidence,
        })
    }

    #[must_use]
    pub const fn identity(&self) -> LyapunovGovernorIdentity {
        self.identity
    }

    #[must_use]
    pub const fn profile(&self) -> LyapunovGovernorProfile {
        self.profile
    }

    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }

    #[must_use]
    pub const fn accepted_observation_count(&self) -> u64 {
        self.accepted_observation_count
    }

    #[must_use]
    pub fn evidence(&self) -> &[LyapunovEvidence] {
        &self.evidence
    }

    #[must_use]
    pub fn latest_evidence(&self) -> Option<&LyapunovEvidence> {
        self.evidence.last()
    }

    /// Accepts one observation after preflighting every failure path.
    pub fn observe(
        &mut self,
        input: SequencedLyapunovSnapshot,
    ) -> Result<LyapunovEvidence, LyapunovObserveError> {
        if input.identity != self.identity {
            return Err(LyapunovObserveError::IdentityMismatch);
        }
        if input.profile != self.profile {
            return Err(LyapunovObserveError::ProfileMismatch);
        }
        let Some(expected) = self.next_sequence else {
            return Err(LyapunovObserveError::WindowComplete {
                last: self.identity.last_sequence,
            });
        };
        if input.stream_sequence != expected {
            return Err(LyapunovObserveError::UnexpectedSequence {
                expected,
                actual: input.stream_sequence,
            });
        }
        validate_state_projection(input.snapshot)?;

        let observation_count = self
            .accepted_observation_count
            .checked_add(1)
            .ok_or(LyapunovObserveError::CounterExhausted)?;
        let next_sequence = if input.stream_sequence == self.identity.last_sequence {
            None
        } else {
            Some(
                input
                    .stream_sequence
                    .checked_add(1)
                    .ok_or(LyapunovObserveError::CounterExhausted)?,
            )
        };

        let snapshot = input.snapshot.foundation_snapshot();
        let record = self.core.compute_record(&snapshot);
        validate_foundation_record(&record)?;
        let suggestion = project_suggestion(self.core.suggest(&snapshot));
        let potential = record.total;
        let assumptions_supported = input.assumptions.supports_candidate();
        let comparison_baseline = if assumptions_supported {
            self.previous_potential
        } else {
            None
        };
        let potential_delta = comparison_baseline.map(|previous| potential - previous);
        let within_potential_ceiling = potential <= self.profile.potential_ceiling();
        let step_guard_satisfied = assumptions_supported
            && comparison_baseline.is_none_or(|previous| {
                potential <= previous || potential - previous <= self.profile.maximum_increase()
            });
        let consecutive_stable_observations =
            if assumptions_supported && within_potential_ceiling && step_guard_satisfied {
                self.consecutive_stable_observations
                    .checked_add(1)
                    .ok_or(LyapunovObserveError::CounterExhausted)?
            } else {
                0
            };
        let stable_count_sufficient = usize::try_from(consecutive_stable_observations)
            .is_ok_and(|count| count >= self.profile.minimum_stable_observations);
        let candidate_authorized = assumptions_supported
            && suggestion != LyapunovSuggestion::NoPreference
            && within_potential_ceiling
            && step_guard_satisfied
            && stable_count_sufficient;
        let selection = if candidate_authorized {
            LyapunovSelection::Candidate
        } else {
            LyapunovSelection::PinnedFallback
        };
        let evidence = LyapunovEvidence {
            identity: self.identity,
            profile: self.profile,
            stream_sequence: input.stream_sequence,
            observation_count,
            snapshot: input.snapshot,
            assumptions: input.assumptions,
            potential_bits: canonical_float_bits(record.total),
            task_component_bits: canonical_float_bits(record.task_component),
            obligation_component_bits: canonical_float_bits(record.obligation_component),
            region_component_bits: canonical_float_bits(record.region_component),
            deadline_component_bits: canonical_float_bits(record.deadline_component),
            potential_delta_bits: potential_delta.map(canonical_float_bits),
            suggestion,
            within_potential_ceiling,
            step_guard_satisfied,
            consecutive_stable_observations,
            candidate_authorized,
            selection,
        };

        if self.evidence.len() == self.profile.max_retained_evidence {
            self.evidence.remove(0);
        }
        self.evidence.push(evidence.clone());
        self.accepted_observation_count = observation_count;
        self.previous_potential = assumptions_supported.then_some(potential);
        self.consecutive_stable_observations = consecutive_stable_observations;
        self.next_sequence = next_sequence;
        Ok(evidence)
    }
}

fn validate_state_projection(
    projection: LyapunovStateProjection,
) -> Result<(), LyapunovObserveError> {
    let deadline_pressure = f64::from_bits(projection.deadline_pressure_bits);
    if !deadline_pressure.is_finite() || deadline_pressure < 0.0 {
        return Err(LyapunovObserveError::InvalidDeadlinePressure {
            bits: projection.deadline_pressure_bits,
        });
    }
    if projection.pending_obligations == 0 && projection.obligation_age_sum_ns != 0 {
        return Err(
            LyapunovObserveError::ObligationAgeWithoutPendingObligations {
                age_ns: projection.obligation_age_sum_ns,
            },
        );
    }
    let classified_obligations = u64::from(projection.pending_send_permits)
        + u64::from(projection.pending_acks)
        + u64::from(projection.pending_leases)
        + u64::from(projection.pending_io_ops);
    if classified_obligations > u64::from(projection.pending_obligations) {
        return Err(LyapunovObserveError::ObligationBreakdownExceedsTotal {
            classified: classified_obligations,
            total: projection.pending_obligations,
        });
    }
    let cancelling = u64::from(projection.cancel_requested_tasks)
        + u64::from(projection.cancelling_tasks)
        + u64::from(projection.finalizing_tasks);
    if cancelling > u64::from(projection.live_tasks) {
        return Err(
            LyapunovObserveError::CancellationBreakdownExceedsLiveTasks {
                cancelling,
                live_tasks: projection.live_tasks,
            },
        );
    }
    Ok(())
}

fn validate_foundation_record(record: &PotentialRecord) -> Result<(), LyapunovObserveError> {
    validate_foundation_value(LyapunovNumericField::TotalPotential, record.total)?;
    validate_foundation_value(LyapunovNumericField::TaskComponent, record.task_component)?;
    validate_foundation_value(
        LyapunovNumericField::ObligationComponent,
        record.obligation_component,
    )?;
    validate_foundation_value(
        LyapunovNumericField::RegionComponent,
        record.region_component,
    )?;
    validate_foundation_value(
        LyapunovNumericField::DeadlineComponent,
        record.deadline_component,
    )
}

fn validate_foundation_value(
    field: LyapunovNumericField,
    value: f64,
) -> Result<(), LyapunovObserveError> {
    if !value.is_finite() || value < 0.0 {
        return Err(LyapunovObserveError::InvalidFoundationProjection {
            field,
            bits: canonical_float_bits(value),
        });
    }
    Ok(())
}

const fn project_suggestion(suggestion: SchedulingSuggestion) -> LyapunovSuggestion {
    match suggestion {
        SchedulingSuggestion::DrainObligations => LyapunovSuggestion::DrainObligations,
        SchedulingSuggestion::DrainRegions => LyapunovSuggestion::DrainRegions,
        SchedulingSuggestion::MeetDeadlines => LyapunovSuggestion::MeetDeadlines,
        SchedulingSuggestion::NoPreference => LyapunovSuggestion::NoPreference,
    }
}

fn canonical_float_bits(value: f64) -> u64 {
    if value.is_nan() {
        f64::NAN.to_bits()
    } else if value == 0.0 {
        0.0_f64.to_bits()
    } else {
        value.to_bits()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    fn weights() -> PotentialWeights {
        PotentialWeights {
            w_tasks: 1.0,
            w_obligation_age: 10.0,
            w_draining_regions: 5.0,
            w_deadline_pressure: 2.0,
        }
    }

    fn identity(
        first_sequence: u64,
        last_sequence: u64,
    ) -> Result<LyapunovGovernorIdentity, LyapunovBuildError> {
        LyapunovGovernorIdentity::try_new(
            oid(1),
            oid(2),
            oid(3),
            oid(4),
            7,
            first_sequence,
            last_sequence,
            oid(5),
            oid(6),
        )
    }

    fn profile(
        minimum_stable_observations: usize,
        max_retained_evidence: usize,
    ) -> Result<LyapunovGovernorProfile, LyapunovBuildError> {
        LyapunovGovernorProfile::try_new(
            oid(2),
            weights(),
            100.0,
            0.0,
            minimum_stable_observations,
            max_retained_evidence,
        )
    }

    fn snapshot(age_ns: u64) -> StateSnapshot {
        StateSnapshot {
            time: Time::from_nanos(age_ns),
            live_tasks: 1,
            pending_obligations: 2,
            obligation_age_sum_ns: age_ns,
            draining_regions: 0,
            deadline_pressure: 0.0,
            pending_send_permits: 2,
            pending_acks: 0,
            pending_leases: 0,
            pending_io_ops: 0,
            cancel_requested_tasks: 0,
            cancelling_tasks: 0,
            finalizing_tasks: 0,
            ready_queue_depth: 0,
        }
    }

    fn input(
        identity: LyapunovGovernorIdentity,
        profile: LyapunovGovernorProfile,
        sequence: u64,
        snapshot: &StateSnapshot,
        assumptions: LyapunovAssumptionAttestation,
    ) -> SequencedLyapunovSnapshot {
        SequencedLyapunovSnapshot::new(
            identity,
            profile,
            sequence,
            LyapunovStateProjection::from_foundation(snapshot),
            assumptions,
        )
    }

    #[test]
    fn identity_and_profile_reject_invalid_construction() -> TestResult {
        assert_eq!(
            LyapunovGovernorIdentity::try_new(
                oid(1),
                oid(2),
                oid(3),
                oid(4),
                7,
                2,
                1,
                oid(5),
                oid(6),
            ),
            Err(LyapunovBuildError::ReversedWindow { first: 2, last: 1 })
        );
        assert_eq!(
            LyapunovGovernorIdentity::try_new(
                oid(1),
                oid(2),
                oid(3),
                oid(4),
                7,
                0,
                u64::MAX,
                oid(5),
                oid(6),
            ),
            Err(LyapunovBuildError::WindowLengthOverflow {
                first: 0,
                last: u64::MAX,
            })
        );
        assert_eq!(
            LyapunovGovernorIdentity::try_new(
                oid(1),
                oid(2),
                oid(3),
                oid(4),
                7,
                0,
                1,
                oid(5),
                oid(5),
            ),
            Err(LyapunovBuildError::CandidateEqualsFallback)
        );

        let invalid_weights = PotentialWeights {
            w_tasks: -1.0,
            ..weights()
        };
        assert_eq!(
            LyapunovGovernorProfile::try_new(oid(2), invalid_weights, 10.0, 0.0, 1, 1),
            Err(LyapunovBuildError::InvalidFoundationWeights)
        );
        assert_eq!(
            LyapunovGovernorProfile::try_new(oid(2), weights(), f64::NAN, 0.0, 1, 1),
            Err(LyapunovBuildError::InvalidPotentialCeiling {
                bits: f64::NAN.to_bits(),
            })
        );
        assert_eq!(
            LyapunovGovernorProfile::try_new(oid(2), weights(), 10.0, -1.0, 1, 1),
            Err(LyapunovBuildError::InvalidMaximumIncrease {
                bits: (-1.0_f64).to_bits(),
            })
        );
        assert_eq!(
            LyapunovGovernorProfile::try_new(oid(2), weights(), 10.0, 0.0, 0, 1),
            Err(LyapunovBuildError::ZeroMinimumStableObservations)
        );
        assert_eq!(
            LyapunovGovernorProfile::try_new(oid(2), weights(), 10.0, 0.0, 1, 0),
            Err(LyapunovBuildError::ZeroEvidenceCapacity)
        );
        assert_eq!(
            LyapunovGovernorProfile::try_new(
                oid(2),
                weights(),
                10.0,
                0.0,
                1,
                MAX_RETAINED_LYAPUNOV_EVIDENCE + 1,
            ),
            Err(LyapunovBuildError::EvidenceCapacityTooLarge {
                actual: MAX_RETAINED_LYAPUNOV_EVIDENCE + 1,
                maximum: MAX_RETAINED_LYAPUNOV_EVIDENCE,
            })
        );
        assert!(LyapunovGovernorProfile::try_new(oid(2), weights(), 10.0, 0.0, 2, 1).is_ok());
        Ok(())
    }

    #[test]
    fn construction_binds_profile_and_keeps_retention_independent() -> TestResult {
        let id = identity(10, 11)?;
        let wrong_profile = LyapunovGovernorProfile::try_new(oid(9), weights(), 100.0, 0.0, 1, 2)?;
        assert!(matches!(
            DecisionLyapunovGovernor::try_new(id, wrong_profile),
            Err(LyapunovBuildError::ProfileOidMismatch {
                identity,
                profile,
            }) if identity == oid(2) && profile == oid(9)
        ));
        assert!(DecisionLyapunovGovernor::try_new(id, profile(1, 3)?).is_ok());
        assert!(DecisionLyapunovGovernor::try_new(id, profile(3, 1)?).is_ok());
        Ok(())
    }

    #[test]
    fn stable_foundation_guard_authorizes_candidate_after_minimum() -> TestResult {
        let id = identity(100, 102)?;
        let policy = profile(2, 3)?;
        let mut governor = DecisionLyapunovGovernor::try_new(id, policy)?;

        let first = governor.observe(input(
            id,
            policy,
            100,
            &snapshot(1_000_000_000),
            LyapunovAssumptionAttestation::fully_supported(),
        ))?;
        assert_eq!(first.suggestion(), LyapunovSuggestion::DrainObligations);
        assert_eq!(first.consecutive_stable_observations(), 1);
        assert!(!first.candidate_authorized());
        assert_eq!(first.selection(), LyapunovSelection::PinnedFallback);
        assert_eq!(first.selected_policy_oid(), oid(6));

        let second = governor.observe(input(
            id,
            policy,
            101,
            &snapshot(500_000_000),
            LyapunovAssumptionAttestation::fully_supported(),
        ))?;
        assert_eq!(second.consecutive_stable_observations(), 2);
        assert!(second.step_guard_satisfied());
        assert!(second.candidate_authorized());
        assert_eq!(second.selection(), LyapunovSelection::Candidate);
        assert_eq!(second.selected_policy_oid(), oid(5));
        Ok(())
    }

    #[test]
    fn guard_breach_and_missing_assumption_select_pinned_fallback() -> TestResult {
        let id = identity(100, 103)?;
        let policy = profile(2, 4)?;
        let mut governor = DecisionLyapunovGovernor::try_new(id, policy)?;
        let assumptions = LyapunovAssumptionAttestation::fully_supported();
        governor.observe(input(
            id,
            policy,
            100,
            &snapshot(1_000_000_000),
            assumptions,
        ))?;
        governor.observe(input(id, policy, 101, &snapshot(500_000_000), assumptions))?;

        let breached = governor.observe(input(
            id,
            policy,
            102,
            &snapshot(2_000_000_000),
            assumptions,
        ))?;
        assert!(!breached.step_guard_satisfied());
        assert_eq!(breached.consecutive_stable_observations(), 0);
        assert_eq!(breached.selection(), LyapunovSelection::PinnedFallback);

        let unsupported = governor.observe(input(
            id,
            policy,
            103,
            &snapshot(1_000_000_000),
            LyapunovAssumptionAttestation::new(true, true, false),
        ))?;
        assert!(!unsupported.step_guard_satisfied());
        assert_eq!(unsupported.consecutive_stable_observations(), 0);
        assert_eq!(unsupported.potential_delta_bits(), None);
        assert!(!unsupported.assumptions().supports_candidate());
        assert_eq!(unsupported.selection(), LyapunovSelection::PinnedFallback);
        Ok(())
    }

    #[test]
    fn unsupported_observation_breaks_stability_streak_and_baseline() -> TestResult {
        let id = identity(20, 23)?;
        let policy = profile(2, 2)?;
        let mut governor = DecisionLyapunovGovernor::try_new(id, policy)?;
        let supported = LyapunovAssumptionAttestation::fully_supported();

        let first = governor.observe(input(id, policy, 20, &snapshot(4), supported))?;
        assert_eq!(first.consecutive_stable_observations(), 1);

        let unsupported = governor.observe(input(
            id,
            policy,
            21,
            &snapshot(3),
            LyapunovAssumptionAttestation::new(true, false, true),
        ))?;
        assert_eq!(unsupported.consecutive_stable_observations(), 0);
        assert_eq!(unsupported.potential_delta_bits(), None);
        assert!(!unsupported.step_guard_satisfied());

        let restarted = governor.observe(input(id, policy, 22, &snapshot(2), supported))?;
        assert_eq!(restarted.consecutive_stable_observations(), 1);
        assert_eq!(restarted.potential_delta_bits(), None);
        assert!(!restarted.candidate_authorized());

        let authorized = governor.observe(input(id, policy, 23, &snapshot(1), supported))?;
        assert_eq!(authorized.consecutive_stable_observations(), 2);
        assert!(authorized.candidate_authorized());
        Ok(())
    }

    #[test]
    fn potential_ceiling_and_no_preference_keep_fallback() -> TestResult {
        let id = identity(0, 1)?;
        let low_ceiling = LyapunovGovernorProfile::try_new(oid(2), weights(), 1.0, 0.0, 1, 2)?;
        let mut governor = DecisionLyapunovGovernor::try_new(id, low_ceiling)?;
        let over_ceiling = governor.observe(input(
            id,
            low_ceiling,
            0,
            &snapshot(1_000_000_000),
            LyapunovAssumptionAttestation::fully_supported(),
        ))?;
        assert!(!over_ceiling.within_potential_ceiling());
        assert_eq!(over_ceiling.selection(), LyapunovSelection::PinnedFallback);

        let mut quiescent = snapshot(0);
        quiescent.live_tasks = 0;
        quiescent.pending_obligations = 0;
        quiescent.pending_send_permits = 0;
        let no_preference = governor.observe(input(
            id,
            low_ceiling,
            1,
            &quiescent,
            LyapunovAssumptionAttestation::fully_supported(),
        ))?;
        assert_eq!(no_preference.suggestion(), LyapunovSuggestion::NoPreference);
        assert_eq!(no_preference.selection(), LyapunovSelection::PinnedFallback);
        Ok(())
    }

    #[test]
    fn replay_is_bit_identical() -> TestResult {
        let id = identity(50, 52)?;
        let policy = profile(2, 3)?;
        let mut left = DecisionLyapunovGovernor::try_new(id, policy)?;
        let mut right = DecisionLyapunovGovernor::try_new(id, policy)?;
        let inputs = [
            snapshot(1_000_000_000),
            snapshot(500_000_000),
            snapshot(250_000_000),
        ];

        for (offset, snapshot) in inputs.iter().enumerate() {
            let sequence = 50_u64
                .checked_add(u64::try_from(offset)?)
                .ok_or_else(|| std::io::Error::other("test sequence overflow"))?;
            let input = input(
                id,
                policy,
                sequence,
                snapshot,
                LyapunovAssumptionAttestation::fully_supported(),
            );
            assert_eq!(left.observe(input)?, right.observe(input)?);
        }
        assert_eq!(left.evidence(), right.evidence());
        Ok(())
    }

    #[test]
    fn wrapper_projection_matches_actual_foundation() -> TestResult {
        let id = identity(0, 0)?;
        let policy = profile(1, 1)?;
        let source = snapshot(750_000_000);
        let foundation = FoundationLyapunovGovernor::new(weights());
        let foundation_record = foundation.compute_record(&source);
        let foundation_suggestion = foundation.suggest(&source);

        let mut governor = DecisionLyapunovGovernor::try_new(id, policy)?;
        let evidence = governor.observe(input(
            id,
            policy,
            0,
            &source,
            LyapunovAssumptionAttestation::fully_supported(),
        ))?;
        assert_eq!(
            evidence.potential_bits(),
            canonical_float_bits(foundation_record.total)
        );
        assert_eq!(
            evidence.task_component_bits(),
            canonical_float_bits(foundation_record.task_component)
        );
        assert_eq!(
            evidence.obligation_component_bits(),
            canonical_float_bits(foundation_record.obligation_component)
        );
        assert_eq!(
            evidence.region_component_bits(),
            canonical_float_bits(foundation_record.region_component)
        );
        assert_eq!(
            evidence.deadline_component_bits(),
            canonical_float_bits(foundation_record.deadline_component)
        );
        assert_eq!(
            evidence.suggestion(),
            project_suggestion(foundation_suggestion)
        );
        Ok(())
    }

    #[test]
    fn mismatches_sequence_and_invalid_input_reject_atomically() -> TestResult {
        let id = identity(10, 11)?;
        let policy = profile(1, 2)?;
        let mut governor = DecisionLyapunovGovernor::try_new(id, policy)?;

        let wrong_id = LyapunovGovernorIdentity::try_new(
            oid(9),
            oid(2),
            oid(3),
            oid(4),
            7,
            10,
            11,
            oid(5),
            oid(6),
        )?;
        assert_eq!(
            governor.observe(input(
                wrong_id,
                policy,
                10,
                &snapshot(1),
                LyapunovAssumptionAttestation::fully_supported(),
            )),
            Err(LyapunovObserveError::IdentityMismatch)
        );

        let different_profile =
            LyapunovGovernorProfile::try_new(oid(2), weights(), 99.0, 0.0, 1, 2)?;
        assert_eq!(
            governor.observe(input(
                id,
                different_profile,
                10,
                &snapshot(1),
                LyapunovAssumptionAttestation::fully_supported(),
            )),
            Err(LyapunovObserveError::ProfileMismatch)
        );
        assert_eq!(
            governor.observe(input(
                id,
                policy,
                11,
                &snapshot(1),
                LyapunovAssumptionAttestation::fully_supported(),
            )),
            Err(LyapunovObserveError::UnexpectedSequence {
                expected: 10,
                actual: 11,
            })
        );

        let mut nonfinite = snapshot(1);
        nonfinite.deadline_pressure = f64::NAN;
        assert_eq!(
            governor.observe(input(
                id,
                policy,
                10,
                &nonfinite,
                LyapunovAssumptionAttestation::fully_supported(),
            )),
            Err(LyapunovObserveError::InvalidDeadlinePressure {
                bits: f64::NAN.to_bits(),
            })
        );
        assert_eq!(governor.next_sequence(), Some(10));
        assert!(governor.evidence().is_empty());
        Ok(())
    }

    #[test]
    fn inconsistent_snapshot_rejects_without_mutation() -> TestResult {
        let id = identity(0, 0)?;
        let policy = profile(1, 1)?;
        let mut governor = DecisionLyapunovGovernor::try_new(id, policy)?;
        let mut inconsistent = snapshot(1);
        inconsistent.pending_obligations = 1;
        inconsistent.pending_send_permits = 2;
        assert_eq!(
            governor.observe(input(
                id,
                policy,
                0,
                &inconsistent,
                LyapunovAssumptionAttestation::fully_supported(),
            )),
            Err(LyapunovObserveError::ObligationBreakdownExceedsTotal {
                classified: 2,
                total: 1,
            })
        );
        assert_eq!(governor.next_sequence(), Some(0));
        assert!(governor.evidence().is_empty());
        Ok(())
    }

    #[test]
    fn retained_evidence_rotates_while_full_window_is_accepted() -> TestResult {
        let id = identity(10, 12)?;
        let policy = profile(3, 2)?;
        let mut governor = DecisionLyapunovGovernor::try_new(id, policy)?;
        let assumptions = LyapunovAssumptionAttestation::fully_supported();
        governor.observe(input(id, policy, 10, &snapshot(3_000_000_000), assumptions))?;
        governor.observe(input(id, policy, 11, &snapshot(2_000_000_000), assumptions))?;
        let final_evidence =
            governor.observe(input(id, policy, 12, &snapshot(1_000_000_000), assumptions))?;
        assert_eq!(final_evidence.observation_count(), 3);
        assert!(final_evidence.candidate_authorized());
        assert_eq!(governor.accepted_observation_count(), 3);
        assert_eq!(governor.next_sequence(), None);
        assert_eq!(governor.evidence().len(), 2);
        assert_eq!(governor.evidence()[0].stream_sequence(), 11);
        assert_eq!(governor.evidence()[0].observation_count(), 2);
        assert_eq!(governor.evidence()[1].stream_sequence(), 12);
        assert_eq!(governor.evidence()[1].observation_count(), 3);

        let terminal_id = identity(u64::MAX, u64::MAX)?;
        let terminal_profile = profile(1, 1)?;
        let mut terminal = DecisionLyapunovGovernor::try_new(terminal_id, terminal_profile)?;
        terminal.observe(input(
            terminal_id,
            terminal_profile,
            u64::MAX,
            &snapshot(0),
            assumptions,
        ))?;
        assert_eq!(terminal.next_sequence(), None);
        assert_eq!(
            terminal.observe(input(
                terminal_id,
                terminal_profile,
                u64::MAX,
                &snapshot(0),
                assumptions,
            )),
            Err(LyapunovObserveError::WindowComplete { last: u64::MAX })
        );
        Ok(())
    }

    #[test]
    fn nonfinite_foundation_projection_rejects_atomically() -> TestResult {
        let id = identity(0, 0)?;
        let extreme_weights = PotentialWeights {
            w_tasks: f64::MAX,
            w_obligation_age: 0.0,
            w_draining_regions: 0.0,
            w_deadline_pressure: 0.0,
        };
        let policy =
            LyapunovGovernorProfile::try_new(oid(2), extreme_weights, f64::MAX, 0.0, 1, 1)?;
        let mut governor = DecisionLyapunovGovernor::try_new(id, policy)?;
        let mut source = snapshot(0);
        source.live_tasks = 2;
        source.pending_obligations = 0;
        source.pending_send_permits = 0;

        assert_eq!(
            governor.observe(input(
                id,
                policy,
                0,
                &source,
                LyapunovAssumptionAttestation::fully_supported(),
            )),
            Err(LyapunovObserveError::InvalidFoundationProjection {
                field: LyapunovNumericField::TotalPotential,
                bits: f64::INFINITY.to_bits(),
            })
        );
        assert_eq!(governor.next_sequence(), Some(0));
        assert!(governor.evidence().is_empty());
        Ok(())
    }

    #[test]
    fn numeric_projection_normalizes_negative_zero() {
        let mut source = snapshot(0);
        source.deadline_pressure = -0.0;
        let projection = LyapunovStateProjection::from_foundation(&source);
        assert_eq!(projection.deadline_pressure_bits(), 0.0_f64.to_bits());
        assert_eq!(
            projection.foundation_snapshot().deadline_pressure.to_bits(),
            0.0_f64.to_bits()
        );
    }
}
