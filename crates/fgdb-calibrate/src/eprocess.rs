//! Identity-bound wrapper around asupersync's e-process core.
//!
//! The foundation owns the betting martingale. This module owns the database
//! integration contract around it: immutable trial and profile identities,
//! exact stream sequencing, canonical float-bit evidence, and a two-outcome
//! policy gate whose conservative state is always the pinned fallback. The
//! registered monitor and profile define which binary outcome supplies
//! evidence against the no-promotion null; this wrapper does not assume that
//! the `1` outcome is always favorable.

use std::fmt;

use asupersync::lab::oracle::eprocess::EProcess;
pub use asupersync::lab::oracle::eprocess::EProcessConfig;

/// Maximum byte length accepted for one stable identity component.
pub const MAX_ID_BYTES: usize = 256;

/// Names the component of a trial or profile identity that failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityField {
    /// Identity of the registered e-process monitor and its null contract.
    Monitor,
    /// Identity of the registered filtration.
    Filtration,
    /// Identity of the candidate decision policy.
    Decision,
    /// Identity of the deterministic fallback policy.
    Fallback,
    /// Identity of the e-process configuration profile.
    Profile,
}

impl fmt::Display for IdentityField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Monitor => "monitor",
            Self::Filtration => "filtration",
            Self::Decision => "decision",
            Self::Fallback => "fallback",
            Self::Profile => "profile",
        })
    }
}

/// Stable construction failures for trial identities and profiles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// An identity component was empty.
    EmptyIdentity(IdentityField),
    /// An identity component exceeded [`MAX_ID_BYTES`].
    IdentityTooLong {
        /// Component that exceeded the limit.
        field: IdentityField,
        /// Actual byte length supplied by the caller.
        actual: usize,
        /// Maximum accepted byte length.
        maximum: usize,
    },
    /// An identity component was not canonical printable ASCII.
    NonCanonicalIdentity {
        /// Component containing the invalid byte.
        field: IdentityField,
        /// Byte offset of the first invalid byte.
        offset: usize,
    },
    /// The candidate decision and fallback policy had the same identity.
    DecisionEqualsFallback,
    /// A sequence window's inclusive end preceded its start.
    ReversedWindow {
        /// Inclusive first stream sequence.
        first: u64,
        /// Inclusive last stream sequence.
        last: u64,
    },
    /// The inclusive sequence-window length could not be represented.
    WindowLengthOverflow {
        /// Inclusive first stream sequence.
        first: u64,
        /// Inclusive last stream sequence.
        last: u64,
    },
    /// The fixed window cannot fit the foundation's observation counter.
    WindowObservationCountUnrepresentable {
        /// Number of observations in the fixed window.
        length: u64,
    },
    /// The foundation rejected the supplied e-process configuration.
    InvalidEProcessConfig,
    /// Space for an owned identity component could not be reserved.
    IdentityAllocationFailed(IdentityField),
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyIdentity(field) => write!(formatter, "{field} identity must not be empty"),
            Self::IdentityTooLong {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "{field} identity is {actual} bytes; maximum is {maximum}"
            ),
            Self::NonCanonicalIdentity { field, offset } => write!(
                formatter,
                "{field} identity contains a non-canonical byte at offset {offset}"
            ),
            Self::DecisionEqualsFallback => {
                formatter.write_str("decision and fallback identities must differ")
            }
            Self::ReversedWindow { first, last } => {
                write!(formatter, "sequence window {first}..={last} is reversed")
            }
            Self::WindowLengthOverflow { first, last } => write!(
                formatter,
                "sequence window {first}..={last} has an unrepresentable length"
            ),
            Self::WindowObservationCountUnrepresentable { length } => write!(
                formatter,
                "sequence window length {length} cannot fit the foundation observation counter"
            ),
            Self::InvalidEProcessConfig => {
                formatter.write_str("asupersync rejected the e-process configuration")
            }
            Self::IdentityAllocationFailed(field) => {
                write!(formatter, "could not allocate the {field} identity")
            }
        }
    }
}

impl std::error::Error for BuildError {}

/// An inclusive range of source-stream sequence numbers for one trial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceWindow {
    first: u64,
    last: u64,
    length: u64,
}

impl SequenceWindow {
    /// Builds a finite inclusive sequence window.
    pub fn try_new(first: u64, last: u64) -> Result<Self, BuildError> {
        let distance = last
            .checked_sub(first)
            .ok_or(BuildError::ReversedWindow { first, last })?;
        let length = distance
            .checked_add(1)
            .ok_or(BuildError::WindowLengthOverflow { first, last })?;
        Ok(Self {
            first,
            last,
            length,
        })
    }

    /// Returns the inclusive first sequence.
    #[must_use]
    pub const fn first(self) -> u64 {
        self.first
    }

    /// Returns the inclusive last sequence.
    #[must_use]
    pub const fn last(self) -> u64 {
        self.last
    }

    /// Returns the number of observations in the inclusive window.
    #[must_use]
    pub const fn len(self) -> u64 {
        self.length
    }

    /// A validated window is never empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        false
    }
}

/// Complete immutable identity of one calibration trial.
///
/// Identity includes the monitor, filtration, fixed stream window, regime
/// epoch, candidate decision, and deterministic fallback. None of these
/// components can be replaced after construction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TrialIdentity {
    monitor_id: String,
    filtration_id: String,
    window: SequenceWindow,
    regime_epoch: u64,
    decision_id: String,
    fallback_id: String,
}

impl TrialIdentity {
    /// Builds a complete trial identity.
    pub fn try_new(
        monitor_id: &str,
        filtration_id: &str,
        window: SequenceWindow,
        regime_epoch: u64,
        decision_id: &str,
        fallback_id: &str,
    ) -> Result<Self, BuildError> {
        let monitor_id = copy_identity(IdentityField::Monitor, monitor_id)?;
        let filtration_id = copy_identity(IdentityField::Filtration, filtration_id)?;
        let decision_id = copy_identity(IdentityField::Decision, decision_id)?;
        let fallback_id = copy_identity(IdentityField::Fallback, fallback_id)?;
        if decision_id == fallback_id {
            return Err(BuildError::DecisionEqualsFallback);
        }

        Ok(Self {
            monitor_id,
            filtration_id,
            window,
            regime_epoch,
            decision_id,
            fallback_id,
        })
    }

    /// Returns the registered monitor and null-contract identity.
    #[must_use]
    pub fn monitor_id(&self) -> &str {
        &self.monitor_id
    }

    /// Returns the registered filtration identity.
    #[must_use]
    pub fn filtration_id(&self) -> &str {
        &self.filtration_id
    }

    /// Returns the fixed source-stream window.
    #[must_use]
    pub const fn window(&self) -> SequenceWindow {
        self.window
    }

    /// Returns the regime epoch.
    #[must_use]
    pub const fn regime_epoch(&self) -> u64 {
        self.regime_epoch
    }

    /// Returns the candidate decision-policy identity.
    #[must_use]
    pub fn decision_id(&self) -> &str {
        &self.decision_id
    }

    /// Returns the pinned deterministic fallback-policy identity.
    #[must_use]
    pub fn fallback_id(&self) -> &str {
        &self.fallback_id
    }
}

/// Immutable canonical profile for the foundation e-process.
///
/// Floating-point configuration is retained as IEEE-754 bits, making equality
/// and emitted records exact rather than dependent on textual formatting.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EProcessProfile {
    profile_id: String,
    p0_bits: u64,
    lambda_bits: u64,
    alpha_bits: u64,
    max_evalue_bits: u64,
}

impl EProcessProfile {
    /// Validates and canonicalizes a foundation configuration.
    pub fn try_new(profile_id: &str, config: EProcessConfig) -> Result<Self, BuildError> {
        config
            .validate()
            .map_err(|_| BuildError::InvalidEProcessConfig)?;

        Ok(Self {
            profile_id: copy_identity(IdentityField::Profile, profile_id)?,
            p0_bits: canonical_float_bits(config.p0),
            lambda_bits: canonical_float_bits(config.lambda),
            alpha_bits: canonical_float_bits(config.alpha),
            max_evalue_bits: canonical_float_bits(config.max_evalue),
        })
    }

    /// Returns the stable configuration-profile identity.
    #[must_use]
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    /// Returns the exact canonical IEEE-754 bits of `p0`.
    #[must_use]
    pub const fn p0_bits(&self) -> u64 {
        self.p0_bits
    }

    /// Returns the exact canonical IEEE-754 bits of `lambda`.
    #[must_use]
    pub const fn lambda_bits(&self) -> u64 {
        self.lambda_bits
    }

    /// Returns the exact canonical IEEE-754 bits of `alpha`.
    #[must_use]
    pub const fn alpha_bits(&self) -> u64 {
        self.alpha_bits
    }

    /// Returns the exact canonical IEEE-754 bits of `max_evalue`.
    #[must_use]
    pub const fn max_evalue_bits(&self) -> u64 {
        self.max_evalue_bits
    }

    fn foundation_config(&self) -> EProcessConfig {
        EProcessConfig {
            p0: f64::from_bits(self.p0_bits),
            lambda: f64::from_bits(self.lambda_bits),
            alpha: f64::from_bits(self.alpha_bits),
            max_evalue: f64::from_bits(self.max_evalue_bits),
        }
    }
}

/// One canonical binary outcome in the registered monitor's filtration.
///
/// The profile's betting direction determines which outcome grows the
/// e-process. In particular, a valid negative `lambda` can make [`Self::Zero`]
/// increase the e-value, so neither variant embeds a statistical conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BinaryObservation {
    /// Binary outcome zero.
    Zero,
    /// Binary outcome one.
    One,
}

impl BinaryObservation {
    const fn as_foundation_event(self) -> bool {
        matches!(self, Self::One)
    }
}

/// Identity-bound and stream-sequenced input to an e-process trial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencedObservation {
    identity: TrialIdentity,
    profile: EProcessProfile,
    stream_sequence: u64,
    value: BinaryObservation,
}

impl SequencedObservation {
    /// Creates an observation envelope.
    #[must_use]
    pub const fn new(
        identity: TrialIdentity,
        profile: EProcessProfile,
        stream_sequence: u64,
        value: BinaryObservation,
    ) -> Self {
        Self {
            identity,
            profile,
            stream_sequence,
            value,
        }
    }

    /// Returns the bound trial identity.
    #[must_use]
    pub const fn identity(&self) -> &TrialIdentity {
        &self.identity
    }

    /// Returns the bound e-process profile.
    #[must_use]
    pub const fn profile(&self) -> &EProcessProfile {
        &self.profile
    }

    /// Returns the source-stream sequence.
    #[must_use]
    pub const fn stream_sequence(&self) -> u64 {
        self.stream_sequence
    }

    /// Returns the binary observation.
    #[must_use]
    pub const fn value(&self) -> BinaryObservation {
        self.value
    }
}

/// The only policy outcomes an e-process trial may emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PolicyOutcomeKind {
    /// Keep using the pinned deterministic fallback.
    RetainPinnedFallback,
    /// Promote the candidate specifically against the pinned fallback.
    PromoteCandidateAgainstPinnedFallback,
}

/// A policy outcome bound to the trial's candidate and pinned fallback.
///
/// Fields are private so callers cannot attach an outcome to different policy
/// identities after the trial emitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyOutcome {
    kind: PolicyOutcomeKind,
    decision_id: String,
    fallback_id: String,
}

impl PolicyOutcome {
    /// Returns the outcome kind.
    #[must_use]
    pub const fn kind(&self) -> PolicyOutcomeKind {
        self.kind
    }

    /// Returns the candidate decision-policy identity.
    #[must_use]
    pub fn decision_id(&self) -> &str {
        &self.decision_id
    }

    /// Returns the pinned fallback-policy identity.
    #[must_use]
    pub fn fallback_id(&self) -> &str {
        &self.fallback_id
    }

    /// Returns the selected policy identity.
    #[must_use]
    pub fn selected_policy_id(&self) -> &str {
        match self.kind {
            PolicyOutcomeKind::RetainPinnedFallback => &self.fallback_id,
            PolicyOutcomeKind::PromoteCandidateAgainstPinnedFallback => &self.decision_id,
        }
    }
}

/// Canonical record of one accepted binary observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRecord {
    identity: TrialIdentity,
    profile: EProcessProfile,
    stream_sequence: u64,
    value: BinaryObservation,
}

impl ObservationRecord {
    /// Returns the complete trial identity.
    #[must_use]
    pub const fn identity(&self) -> &TrialIdentity {
        &self.identity
    }

    /// Returns the immutable e-process profile.
    #[must_use]
    pub const fn profile(&self) -> &EProcessProfile {
        &self.profile
    }

    /// Returns the source-stream sequence.
    #[must_use]
    pub const fn stream_sequence(&self) -> u64 {
        self.stream_sequence
    }

    /// Returns the binary observation.
    #[must_use]
    pub const fn value(&self) -> BinaryObservation {
        self.value
    }
}

/// Canonical evidence state after zero or more accepted observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRecord {
    identity: TrialIdentity,
    profile: EProcessProfile,
    through_sequence: Option<u64>,
    observations: u64,
    one_observations: u64,
    e_value_bits: u64,
    rejection_threshold_bits: u64,
    first_rejection_sequence: Option<u64>,
    outcome: PolicyOutcome,
}

impl EvidenceRecord {
    /// Returns the complete trial identity.
    #[must_use]
    pub const fn identity(&self) -> &TrialIdentity {
        &self.identity
    }

    /// Returns the immutable e-process profile.
    #[must_use]
    pub const fn profile(&self) -> &EProcessProfile {
        &self.profile
    }

    /// Returns the last accepted source-stream sequence, if any.
    #[must_use]
    pub const fn through_sequence(&self) -> Option<u64> {
        self.through_sequence
    }

    /// Returns the number of accepted observations.
    #[must_use]
    pub const fn observations(&self) -> u64 {
        self.observations
    }

    /// Returns the number of accepted binary-one observations.
    #[must_use]
    pub const fn one_observations(&self) -> u64 {
        self.one_observations
    }

    /// Returns the current e-value's exact IEEE-754 bits.
    #[must_use]
    pub const fn e_value_bits(&self) -> u64 {
        self.e_value_bits
    }

    /// Returns the rejection threshold's exact IEEE-754 bits.
    #[must_use]
    pub const fn rejection_threshold_bits(&self) -> u64 {
        self.rejection_threshold_bits
    }

    /// Returns the first source sequence at which the null was rejected.
    #[must_use]
    pub const fn first_rejection_sequence(&self) -> Option<u64> {
        self.first_rejection_sequence
    }

    /// Returns the identity-bound policy outcome.
    #[must_use]
    pub const fn outcome(&self) -> &PolicyOutcome {
        &self.outcome
    }
}

/// Records produced by one accepted observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationUpdate {
    /// Canonical accepted input record.
    pub observation: ObservationRecord,
    /// Canonical evidence state after applying the input.
    pub evidence: EvidenceRecord,
}

/// Non-mutating failures when applying a sequenced observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserveError {
    /// The observation belongs to a different immutable trial identity.
    TrialIdentityMismatch,
    /// The observation names a different immutable e-process profile.
    ProfileMismatch,
    /// The supplied source sequence was not the exact next sequence.
    UnexpectedSequence {
        /// Sequence the trial required next.
        expected: u64,
        /// Sequence supplied by the observation.
        actual: u64,
    },
    /// The fixed trial window has already consumed its last sequence.
    WindowComplete {
        /// Inclusive final sequence of the fixed window.
        last: u64,
    },
    /// The foundation observation counter cannot represent another event.
    FoundationObservationLimit,
    /// The canonical evidence counter cannot represent another event.
    CanonicalCounterLimit,
}

impl fmt::Display for ObserveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrialIdentityMismatch => {
                formatter.write_str("observation trial identity does not match")
            }
            Self::ProfileMismatch => {
                formatter.write_str("observation e-process profile does not match")
            }
            Self::UnexpectedSequence { expected, actual } => write!(
                formatter,
                "expected stream sequence {expected}, received {actual}"
            ),
            Self::WindowComplete { last } => {
                write!(formatter, "trial sequence window ended at {last}")
            }
            Self::FoundationObservationLimit => {
                formatter.write_str("foundation observation counter is exhausted")
            }
            Self::CanonicalCounterLimit => {
                formatter.write_str("canonical evidence counter is exhausted")
            }
        }
    }
}

impl std::error::Error for ObserveError {}

/// An identity-bound calibration trial backed by asupersync's e-process.
#[derive(Debug)]
pub struct EProcessTrial {
    identity: TrialIdentity,
    profile: EProcessProfile,
    core: EProcess,
    next_sequence: Option<u64>,
    through_sequence: Option<u64>,
    observations: u64,
    one_observations: u64,
    first_rejection_sequence: Option<u64>,
}

impl EProcessTrial {
    /// Constructs a trial after revalidating the exact canonical profile.
    ///
    /// Validation happens before calling the foundation constructor, whose
    /// public API assumes a valid configuration.
    pub fn try_new(identity: TrialIdentity, profile: EProcessProfile) -> Result<Self, BuildError> {
        let config = profile.foundation_config();
        config
            .validate()
            .map_err(|_| BuildError::InvalidEProcessConfig)?;
        let _ = usize::try_from(identity.window.len()).map_err(|_| {
            BuildError::WindowObservationCountUnrepresentable {
                length: identity.window.len(),
            }
        })?;
        let first_sequence = identity.window.first();
        let core = EProcess::new_without_history(identity.monitor_id(), config);

        Ok(Self {
            identity,
            profile,
            core,
            next_sequence: Some(first_sequence),
            through_sequence: None,
            observations: 0,
            one_observations: 0,
            first_rejection_sequence: None,
        })
    }

    /// Returns the immutable trial identity.
    #[must_use]
    pub const fn identity(&self) -> &TrialIdentity {
        &self.identity
    }

    /// Returns the immutable canonical profile.
    #[must_use]
    pub const fn profile(&self) -> &EProcessProfile {
        &self.profile
    }

    /// Returns the exact next sequence, or `None` after the window is complete.
    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }

    /// Returns the current canonical evidence record.
    #[must_use]
    pub fn evidence(&self) -> EvidenceRecord {
        self.make_evidence_record()
    }

    /// Applies one identity-bound, profile-bound, exactly sequenced event.
    ///
    /// All rejection checks and counter bounds run before the foundation core
    /// is mutated.
    pub fn observe(
        &mut self,
        observation: SequencedObservation,
    ) -> Result<ObservationUpdate, ObserveError> {
        if observation.identity != self.identity {
            return Err(ObserveError::TrialIdentityMismatch);
        }
        if observation.profile != self.profile {
            return Err(ObserveError::ProfileMismatch);
        }

        let Some(expected_sequence) = self.next_sequence else {
            return Err(ObserveError::WindowComplete {
                last: self.identity.window.last(),
            });
        };
        if observation.stream_sequence != expected_sequence {
            return Err(ObserveError::UnexpectedSequence {
                expected: expected_sequence,
                actual: observation.stream_sequence,
            });
        }
        if self.core.observations == usize::MAX {
            return Err(ObserveError::FoundationObservationLimit);
        }

        let observations = self
            .observations
            .checked_add(1)
            .ok_or(ObserveError::CanonicalCounterLimit)?;
        let one_observations = self
            .one_observations
            .checked_add(u64::from(observation.value.as_foundation_event()))
            .ok_or(ObserveError::CanonicalCounterLimit)?;

        let stream_sequence = observation.stream_sequence;
        let value = observation.value;
        let next_sequence = if stream_sequence == self.identity.window.last() {
            None
        } else {
            Some(
                stream_sequence
                    .checked_add(1)
                    .ok_or(ObserveError::CanonicalCounterLimit)?,
            )
        };
        self.core.observe(value.as_foundation_event());
        self.through_sequence = Some(stream_sequence);
        self.observations = observations;
        self.one_observations = one_observations;
        if self.first_rejection_sequence.is_none() && self.core.rejected {
            self.first_rejection_sequence = Some(stream_sequence);
        }
        self.next_sequence = next_sequence;

        let observation_record = ObservationRecord {
            identity: observation.identity,
            profile: observation.profile,
            stream_sequence,
            value,
        };
        let evidence = self.make_evidence_record();

        Ok(ObservationUpdate {
            observation: observation_record,
            evidence,
        })
    }

    fn make_evidence_record(&self) -> EvidenceRecord {
        let kind = if self.core.rejected {
            PolicyOutcomeKind::PromoteCandidateAgainstPinnedFallback
        } else {
            PolicyOutcomeKind::RetainPinnedFallback
        };
        EvidenceRecord {
            identity: self.identity.clone(),
            profile: self.profile.clone(),
            through_sequence: self.through_sequence,
            observations: self.observations,
            one_observations: self.one_observations,
            e_value_bits: canonical_float_bits(self.core.e_value()),
            rejection_threshold_bits: canonical_float_bits(self.core.config.threshold()),
            first_rejection_sequence: self.first_rejection_sequence,
            outcome: PolicyOutcome {
                kind,
                decision_id: self.identity.decision_id.clone(),
                fallback_id: self.identity.fallback_id.clone(),
            },
        }
    }
}

fn canonical_float_bits(value: f64) -> u64 {
    let bits = value.to_bits();
    if bits == (-0.0_f64).to_bits() {
        0.0_f64.to_bits()
    } else {
        bits
    }
}

fn copy_identity(field: IdentityField, value: &str) -> Result<String, BuildError> {
    if value.is_empty() {
        return Err(BuildError::EmptyIdentity(field));
    }
    if value.len() > MAX_ID_BYTES {
        return Err(BuildError::IdentityTooLong {
            field,
            actual: value.len(),
            maximum: MAX_ID_BYTES,
        });
    }
    if let Some((offset, _)) = value
        .as_bytes()
        .iter()
        .enumerate()
        .find(|(_, byte)| !(0x21..=0x7e).contains(*byte))
    {
        return Err(BuildError::NonCanonicalIdentity { field, offset });
    }

    let mut owned = String::new();
    owned
        .try_reserve_exact(value.len())
        .map_err(|_| BuildError::IdentityAllocationFailed(field))?;
    owned.push_str(value);
    Ok(owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn config() -> EProcessConfig {
        EProcessConfig {
            p0: 0.2,
            lambda: 1.0,
            alpha: 0.25,
            max_evalue: 1_000.0,
        }
    }

    fn window() -> Result<SequenceWindow, BuildError> {
        SequenceWindow::try_new(40, 49)
    }

    fn identity() -> Result<TrialIdentity, BuildError> {
        TrialIdentity::try_new(
            "monitor:latency",
            "filtration:commit-seq",
            window()?,
            7,
            "decision:fast-path-v3",
            "fallback:analytic-v1",
        )
    }

    fn profile() -> Result<EProcessProfile, BuildError> {
        EProcessProfile::try_new("profile:eprocess-v1", config())
    }

    fn trial() -> Result<EProcessTrial, BuildError> {
        EProcessTrial::try_new(identity()?, profile()?)
    }

    fn accept(
        trial: &mut EProcessTrial,
        sequence: u64,
        value: BinaryObservation,
    ) -> TestResult<ObservationUpdate> {
        let observation = SequencedObservation::new(identity()?, profile()?, sequence, value);
        Ok(trial.observe(observation)?)
    }

    #[test]
    fn construction_validates_identity_window_and_foundation_config() -> TestResult {
        assert_eq!(
            SequenceWindow::try_new(9, 8),
            Err(BuildError::ReversedWindow { first: 9, last: 8 })
        );
        assert_eq!(
            SequenceWindow::try_new(0, u64::MAX),
            Err(BuildError::WindowLengthOverflow {
                first: 0,
                last: u64::MAX
            })
        );
        assert!(matches!(
            TrialIdentity::try_new("", "f", window()?, 0, "d", "b"),
            Err(BuildError::EmptyIdentity(IdentityField::Monitor))
        ));
        assert!(matches!(
            TrialIdentity::try_new("m", "bad id", window()?, 0, "d", "b"),
            Err(BuildError::NonCanonicalIdentity {
                field: IdentityField::Filtration,
                ..
            })
        ));
        assert_eq!(
            TrialIdentity::try_new("m", "f", window()?, 0, "same", "same"),
            Err(BuildError::DecisionEqualsFallback)
        );

        let invalid = EProcessConfig {
            alpha: f64::NAN,
            ..config()
        };
        assert_eq!(
            EProcessProfile::try_new("profile:bad", invalid),
            Err(BuildError::InvalidEProcessConfig)
        );
        let impossible_threshold = EProcessConfig {
            alpha: 0.01,
            max_evalue: 50.0,
            ..config()
        };
        assert_eq!(
            EProcessProfile::try_new("profile:bad-threshold", impossible_threshold),
            Err(BuildError::InvalidEProcessConfig)
        );
        Ok(())
    }

    #[test]
    fn profile_uses_canonical_float_bits() -> TestResult {
        let mut negative_zero = config();
        negative_zero.lambda = -0.0;
        let left = EProcessProfile::try_new("profile:zero", negative_zero)?;
        let mut positive_zero = config();
        positive_zero.lambda = 0.0;
        let right = EProcessProfile::try_new("profile:zero", positive_zero)?;
        assert_eq!(left, right);
        assert_eq!(left.lambda_bits(), 0.0_f64.to_bits());
        Ok(())
    }

    #[test]
    fn profile_preserves_foundation_negative_lambda_bounds() -> TestResult {
        let valid = EProcessConfig {
            p0: 0.8,
            lambda: -4.0,
            alpha: 0.25,
            max_evalue: 1_000.0,
        };
        let profile = EProcessProfile::try_new("profile:negative-valid", valid)?;
        assert_eq!(f64::from_bits(profile.lambda_bits()), -4.0);

        let lower_boundary = EProcessConfig {
            p0: 0.8,
            lambda: -1.0 / (1.0 - 0.8),
            alpha: 0.25,
            max_evalue: 1_000.0,
        };
        assert_eq!(
            EProcessProfile::try_new("profile:negative-boundary", lower_boundary),
            Err(BuildError::InvalidEProcessConfig)
        );
        let upper_boundary = EProcessConfig {
            p0: 0.8,
            lambda: 1.25,
            alpha: 0.25,
            max_evalue: 1_000.0,
        };
        assert_eq!(
            EProcessProfile::try_new("profile:positive-boundary", upper_boundary),
            Err(BuildError::InvalidEProcessConfig)
        );
        Ok(())
    }

    #[test]
    fn negative_bet_can_promote_on_zero_outcomes_without_mislabeling_them() -> TestResult {
        let identity = identity()?;
        let negative_config = EProcessConfig {
            p0: 0.8,
            lambda: -1.0,
            alpha: 0.25,
            max_evalue: 1_000.0,
        };
        let negative_profile =
            EProcessProfile::try_new("profile:negative-bet", negative_config.clone())?;
        let mut trial = EProcessTrial::try_new(identity.clone(), negative_profile.clone())?;
        let mut foundation = EProcess::new_without_history("monitor:latency", negative_config);

        for sequence in 40..=42 {
            let observation = SequencedObservation::new(
                identity.clone(),
                negative_profile.clone(),
                sequence,
                BinaryObservation::Zero,
            );
            trial.observe(observation)?;
            foundation.observe(false);
        }

        let evidence = trial.evidence();
        assert_eq!(
            evidence.e_value_bits(),
            canonical_float_bits(foundation.e_value())
        );
        assert_eq!(
            evidence.observations(),
            u64::try_from(foundation.observations)?
        );
        assert_eq!(foundation.violations_observed, 0);
        assert!(foundation.rejected);
        assert_eq!(evidence.one_observations(), 0);
        assert_eq!(
            evidence.outcome().kind(),
            PolicyOutcomeKind::PromoteCandidateAgainstPinnedFallback
        );
        assert_eq!(evidence.first_rejection_sequence(), Some(42));
        Ok(())
    }

    #[test]
    fn identity_and_profile_records_retain_every_pinned_component() -> TestResult {
        let trial = trial()?;
        let evidence = trial.evidence();
        assert_eq!(evidence.identity().monitor_id(), "monitor:latency");
        assert_eq!(evidence.identity().filtration_id(), "filtration:commit-seq");
        assert_eq!(evidence.identity().window(), window()?);
        assert_eq!(evidence.identity().regime_epoch(), 7);
        assert_eq!(evidence.identity().decision_id(), "decision:fast-path-v3");
        assert_eq!(evidence.identity().fallback_id(), "fallback:analytic-v1");
        assert_eq!(evidence.profile().profile_id(), "profile:eprocess-v1");
        assert_eq!(evidence.profile().p0_bits(), config().p0.to_bits());
        assert_eq!(evidence.profile().lambda_bits(), config().lambda.to_bits());
        assert_eq!(evidence.profile().alpha_bits(), config().alpha.to_bits());
        assert_eq!(
            evidence.profile().max_evalue_bits(),
            config().max_evalue.to_bits()
        );
        assert_eq!(evidence.through_sequence(), None);
        assert_eq!(evidence.observations(), 0);
        assert_eq!(evidence.one_observations(), 0);
        assert_eq!(evidence.e_value_bits(), 1.0_f64.to_bits());
        assert_eq!(
            evidence.rejection_threshold_bits(),
            canonical_float_bits(config().threshold())
        );
        Ok(())
    }

    #[test]
    fn sequencing_failures_do_not_advance_state() -> TestResult {
        let mut trial = trial()?;
        let before = trial.evidence();
        let skipped =
            SequencedObservation::new(identity()?, profile()?, 41, BinaryObservation::One);
        assert_eq!(
            trial.observe(skipped),
            Err(ObserveError::UnexpectedSequence {
                expected: 40,
                actual: 41
            })
        );
        assert_eq!(trial.evidence(), before);
        assert_eq!(trial.next_sequence(), Some(40));

        let accepted = accept(&mut trial, 40, BinaryObservation::Zero)?;
        assert_eq!(accepted.observation.stream_sequence(), 40);
        assert_eq!(trial.next_sequence(), Some(41));

        let duplicate =
            SequencedObservation::new(identity()?, profile()?, 40, BinaryObservation::Zero);
        assert_eq!(
            trial.observe(duplicate),
            Err(ObserveError::UnexpectedSequence {
                expected: 41,
                actual: 40
            })
        );
        assert_eq!(trial.evidence(), accepted.evidence);
        Ok(())
    }

    #[test]
    fn replay_produces_bit_identical_records() -> TestResult {
        let values = [
            BinaryObservation::Zero,
            BinaryObservation::One,
            BinaryObservation::One,
            BinaryObservation::Zero,
            BinaryObservation::One,
        ];
        let mut left = trial()?;
        let mut right = trial()?;
        let mut left_records = Vec::new();
        let mut right_records = Vec::new();

        for (offset, value) in values.into_iter().enumerate() {
            let sequence = 40_u64
                .checked_add(u64::try_from(offset)?)
                .ok_or(ObserveError::CanonicalCounterLimit)?;
            left_records.push(accept(&mut left, sequence, value)?);
            right_records.push(accept(&mut right, sequence, value)?);
        }

        assert_eq!(left_records, right_records);
        assert_eq!(left.evidence(), right.evidence());
        assert_eq!(
            left.evidence().e_value_bits(),
            right.evidence().e_value_bits()
        );
        Ok(())
    }

    #[test]
    fn outcome_retains_fallback_until_foundation_rejects_then_promotes() -> TestResult {
        let mut trial = trial()?;
        let initial = trial.evidence();
        assert_eq!(
            initial.outcome().kind(),
            PolicyOutcomeKind::RetainPinnedFallback
        );
        assert_eq!(
            initial.outcome().selected_policy_id(),
            identity()?.fallback_id()
        );

        let first = accept(&mut trial, 40, BinaryObservation::One)?;
        assert_eq!(
            first.evidence.outcome().kind(),
            PolicyOutcomeKind::RetainPinnedFallback
        );
        let second = accept(&mut trial, 41, BinaryObservation::One)?;
        let third = accept(&mut trial, 42, BinaryObservation::One)?;

        assert_eq!(
            third.evidence.outcome().kind(),
            PolicyOutcomeKind::PromoteCandidateAgainstPinnedFallback
        );
        assert_eq!(
            third.evidence.outcome().selected_policy_id(),
            identity()?.decision_id()
        );
        assert_eq!(
            third.evidence.outcome().fallback_id(),
            identity()?.fallback_id()
        );
        assert_eq!(third.evidence.first_rejection_sequence(), Some(42));
        assert_eq!(third.evidence.observations(), 3);
        assert_eq!(third.evidence.one_observations(), 3);
        assert!(f64::from_bits(third.evidence.e_value_bits()) >= 4.0);
        assert_eq!(
            second.evidence.outcome().kind(),
            PolicyOutcomeKind::RetainPinnedFallback
        );
        let after_rejection = accept(&mut trial, 43, BinaryObservation::Zero)?;
        assert_eq!(
            after_rejection.evidence.outcome().kind(),
            PolicyOutcomeKind::PromoteCandidateAgainstPinnedFallback
        );
        assert_eq!(
            after_rejection.evidence.first_rejection_sequence(),
            Some(42)
        );
        assert_eq!(after_rejection.evidence.observations(), 4);
        assert_eq!(after_rejection.evidence.one_observations(), 3);
        Ok(())
    }

    #[test]
    fn identity_and_profile_mismatches_are_immutable_failures() -> TestResult {
        let mut trial = trial()?;
        let before = trial.evidence();
        let other_identity = TrialIdentity::try_new(
            "monitor:latency",
            "filtration:commit-seq",
            window()?,
            8,
            "decision:fast-path-v3",
            "fallback:analytic-v1",
        )?;
        let wrong_identity =
            SequencedObservation::new(other_identity, profile()?, 40, BinaryObservation::One);
        assert_eq!(
            trial.observe(wrong_identity),
            Err(ObserveError::TrialIdentityMismatch)
        );
        assert_eq!(trial.evidence(), before);

        let other_profile_identity = EProcessProfile::try_new("profile:eprocess-v2", config())?;
        let wrong_profile_identity = SequencedObservation::new(
            identity()?,
            other_profile_identity,
            40,
            BinaryObservation::One,
        );
        assert_eq!(
            trial.observe(wrong_profile_identity),
            Err(ObserveError::ProfileMismatch)
        );
        assert_eq!(trial.evidence(), before);

        let mut alternate_config = config();
        alternate_config.lambda = 1.5;
        let other_profile = EProcessProfile::try_new("profile:eprocess-v1", alternate_config)?;
        let wrong_profile =
            SequencedObservation::new(identity()?, other_profile, 40, BinaryObservation::One);
        assert_eq!(
            trial.observe(wrong_profile),
            Err(ObserveError::ProfileMismatch)
        );
        assert_eq!(trial.evidence(), before);
        assert_eq!(trial.next_sequence(), Some(40));
        Ok(())
    }

    #[test]
    fn fixed_window_rejects_observations_after_its_end() -> TestResult {
        let one = SequenceWindow::try_new(9, 9)?;
        let one_identity = TrialIdentity::try_new("m", "f", one, 1, "candidate", "fallback")?;
        let one_profile = profile()?;
        let mut trial = EProcessTrial::try_new(one_identity.clone(), one_profile.clone())?;
        let first = SequencedObservation::new(
            one_identity.clone(),
            one_profile.clone(),
            9,
            BinaryObservation::Zero,
        );
        assert!(trial.observe(first).is_ok());
        assert_eq!(trial.next_sequence(), None);
        let after =
            SequencedObservation::new(one_identity, one_profile, 10, BinaryObservation::Zero);
        assert_eq!(
            trial.observe(after),
            Err(ObserveError::WindowComplete { last: 9 })
        );
        Ok(())
    }

    #[test]
    fn window_ending_at_maximum_sequence_completes_without_increment_overflow() -> TestResult {
        let edge_window = SequenceWindow::try_new(u64::MAX - 1, u64::MAX)?;
        let edge_identity =
            TrialIdentity::try_new("m", "f", edge_window, 1, "candidate", "fallback")?;
        let edge_profile = profile()?;
        let mut trial = EProcessTrial::try_new(edge_identity.clone(), edge_profile.clone())?;

        for sequence in [u64::MAX - 1, u64::MAX] {
            trial.observe(SequencedObservation::new(
                edge_identity.clone(),
                edge_profile.clone(),
                sequence,
                BinaryObservation::Zero,
            ))?;
        }

        assert_eq!(trial.next_sequence(), None);
        assert_eq!(trial.evidence().through_sequence(), Some(u64::MAX));
        assert_eq!(trial.evidence().observations(), 2);
        Ok(())
    }

    #[test]
    fn exhausted_counters_are_rejected_before_foundation_mutation() -> TestResult {
        let mut foundation_exhausted = trial()?;
        foundation_exhausted.core.observations = usize::MAX;
        let before_foundation = foundation_exhausted.evidence();
        let observation =
            SequencedObservation::new(identity()?, profile()?, 40, BinaryObservation::One);
        assert_eq!(
            foundation_exhausted.observe(observation),
            Err(ObserveError::FoundationObservationLimit)
        );
        assert_eq!(foundation_exhausted.evidence(), before_foundation);

        let mut canonical_exhausted = trial()?;
        canonical_exhausted.observations = u64::MAX;
        let before_canonical = canonical_exhausted.evidence();
        let observation =
            SequencedObservation::new(identity()?, profile()?, 40, BinaryObservation::One);
        assert_eq!(
            canonical_exhausted.observe(observation),
            Err(ObserveError::CanonicalCounterLimit)
        );
        assert_eq!(canonical_exhausted.evidence(), before_canonical);
        assert_eq!(canonical_exhausted.core.observations, 0);
        Ok(())
    }
}
