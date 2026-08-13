//! Deterministic, identity-bound Wald sequential probability-ratio tests.
//!
//! This module supplies model-qualified migration evidence over an already
//! authenticated binary projection.  It deliberately does not define the raw
//! metric projection, does not support composite hypotheses, and never
//! bypasses the deterministic dwell and conversion-economics guards enforced
//! by [`crate::policy_epoch`].

use std::fmt;

use fgdb_types::ObjectId;

/// Fixed-point scale used by probabilities and likelihood ratios.
pub const SPRT_SCALE: u128 = 1_000_000_000;

/// Canonical positive-infinity sentinel for an outward upper endpoint that
/// exceeded the finite profile cap.
pub const SPRT_LIKELIHOOD_INFINITY: u128 = u128::MAX;

const SPRT_PROBABILITY_SCALE: u64 = 1_000_000_000;

/// Stable binary format version for [`SprtEvidence`].
pub const SPRT_EVIDENCE_VERSION: u16 = 1;

/// Absolute implementation ceiling for one declared observation window.
pub const MAX_SPRT_OBSERVATIONS: u64 = 1_048_576;

const MAGIC: [u8; 8] = *b"FGDBSPR1";
const FIXED_BYTES: usize = 8 + 2 + 7 * 32 + 10 * 8 + 5 * 16 + 2;

/// Closed binary input vocabulary.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SprtObservation {
    /// The registered binary projection emitted zero.
    Zero = 0,
    /// The registered binary projection emitted one.
    One = 1,
}

impl SprtObservation {
    fn try_from_tag(tag: u8) -> Result<Self, SprtError> {
        match tag {
            0 => Ok(Self::Zero),
            1 => Ok(Self::One),
            _ => Err(SprtError::UnknownObservationTag { tag }),
        }
    }
}

/// Terminal state of one simple-hypothesis SPRT.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SprtDecision {
    /// Neither likelihood-ratio boundary has been reached.
    Continue = 0,
    /// The lower boundary accepted the registered null hypothesis.
    AcceptNull = 1,
    /// The upper boundary accepted the registered alternative hypothesis.
    AcceptAlternative = 2,
}

impl SprtDecision {
    fn try_from_tag(tag: u8) -> Result<Self, SprtError> {
        match tag {
            0 => Ok(Self::Continue),
            1 => Ok(Self::AcceptNull),
            2 => Ok(Self::AcceptAlternative),
            _ => Err(SprtError::UnknownDecisionTag { tag }),
        }
    }
}

/// Inclusive source-sequence window.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SprtSequenceWindow {
    first: u64,
    last: u64,
    length: u64,
}

impl SprtSequenceWindow {
    /// Constructs a finite, non-empty inclusive window.
    pub fn try_new(first: u64, last: u64) -> Result<Self, SprtError> {
        let length = last
            .checked_sub(first)
            .and_then(|distance| distance.checked_add(1))
            .ok_or(SprtError::InvalidSequenceWindow { first, last })?;
        if length > MAX_SPRT_OBSERVATIONS {
            return Err(SprtError::ObservationLimitExceeded {
                actual: length,
                maximum: MAX_SPRT_OBSERVATIONS,
            });
        }
        Ok(Self {
            first,
            last,
            length,
        })
    }

    /// Inclusive first sequence.
    #[must_use]
    pub const fn first(self) -> u64 {
        self.first
    }

    /// Inclusive last sequence.
    #[must_use]
    pub const fn last(self) -> u64 {
        self.last
    }

    /// Number of sequences in the window.
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

/// Immutable identity of one SPRT and the decision it may inform.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SprtIdentity {
    monitor_oid: ObjectId,
    filtration_oid: ObjectId,
    binary_input_contract_oid: ObjectId,
    decision_card_oid: ObjectId,
    window: SprtSequenceWindow,
    regime_epoch: u64,
    candidate_policy_oid: ObjectId,
    pinned_fallback_oid: ObjectId,
}

impl SprtIdentity {
    /// Constructs a complete immutable trial identity.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        monitor_oid: ObjectId,
        filtration_oid: ObjectId,
        binary_input_contract_oid: ObjectId,
        decision_card_oid: ObjectId,
        window: SprtSequenceWindow,
        regime_epoch: u64,
        candidate_policy_oid: ObjectId,
        pinned_fallback_oid: ObjectId,
    ) -> Result<Self, SprtError> {
        if candidate_policy_oid == pinned_fallback_oid {
            return Err(SprtError::CandidateEqualsFallback);
        }
        Ok(Self {
            monitor_oid,
            filtration_oid,
            binary_input_contract_oid,
            decision_card_oid,
            window,
            regime_epoch,
            candidate_policy_oid,
            pinned_fallback_oid,
        })
    }

    /// Registered monitor identity.
    #[must_use]
    pub const fn monitor_oid(self) -> ObjectId {
        self.monitor_oid
    }

    /// Registered filtration identity.
    #[must_use]
    pub const fn filtration_oid(self) -> ObjectId {
        self.filtration_oid
    }

    /// Identity of the authenticated binary projection contract.
    #[must_use]
    pub const fn binary_input_contract_oid(self) -> ObjectId {
        self.binary_input_contract_oid
    }

    /// Identity of the decision card whose hard guards govern this trial.
    #[must_use]
    pub const fn decision_card_oid(self) -> ObjectId {
        self.decision_card_oid
    }

    /// Complete declared source window.
    #[must_use]
    pub const fn window(self) -> SprtSequenceWindow {
        self.window
    }

    /// Regime epoch fixed for the trial.
    #[must_use]
    pub const fn regime_epoch(self) -> u64 {
        self.regime_epoch
    }

    /// Candidate migration-policy identity.
    #[must_use]
    pub const fn candidate_policy_oid(self) -> ObjectId {
        self.candidate_policy_oid
    }

    /// Deterministic fallback-policy identity.
    #[must_use]
    pub const fn pinned_fallback_oid(self) -> ObjectId {
        self.pinned_fallback_oid
    }
}

/// Versioned simple-hypothesis and stopping profile.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SprtProfile {
    profile_oid: ObjectId,
    null_one_probability: u64,
    alternative_one_probability: u64,
    type_i_error_bound_units: u64,
    type_ii_error_bound_units: u64,
    accept_null_ratio: u128,
    accept_alternative_ratio: u128,
    likelihood_ratio_cap: u128,
    maximum_observations: u64,
}

impl SprtProfile {
    /// Constructs a checked fixed-point SPRT profile.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        profile_oid: ObjectId,
        null_one_probability: u64,
        alternative_one_probability: u64,
        type_i_error_bound_units: u64,
        type_ii_error_bound_units: u64,
        accept_null_ratio: u128,
        accept_alternative_ratio: u128,
        likelihood_ratio_cap: u128,
        maximum_observations: u64,
    ) -> Result<Self, SprtError> {
        let scale = SPRT_PROBABILITY_SCALE;
        if null_one_probability == 0
            || null_one_probability >= scale
            || alternative_one_probability == 0
            || alternative_one_probability >= scale
            || null_one_probability == alternative_one_probability
        {
            return Err(SprtError::InvalidHypotheses);
        }
        if type_i_error_bound_units == 0
            || type_i_error_bound_units >= scale
            || type_ii_error_bound_units == 0
            || type_ii_error_bound_units >= scale
        {
            return Err(SprtError::InvalidErrorBounds);
        }
        let minimum_alternative_ratio = div_ceil(
            SPRT_SCALE
                .checked_mul(SPRT_SCALE)
                .ok_or(SprtError::ArithmeticOverflow)?,
            u128::from(type_i_error_bound_units),
        )?;
        if accept_null_ratio == 0
            || accept_null_ratio >= SPRT_SCALE
            || accept_null_ratio > u128::from(type_ii_error_bound_units)
            || accept_alternative_ratio <= SPRT_SCALE
            || accept_alternative_ratio < minimum_alternative_ratio
            || likelihood_ratio_cap < accept_alternative_ratio
            || likelihood_ratio_cap > u128::MAX / SPRT_SCALE
        {
            return Err(SprtError::InvalidLikelihoodBoundaries);
        }
        if maximum_observations == 0 || maximum_observations > MAX_SPRT_OBSERVATIONS {
            return Err(SprtError::ObservationLimitExceeded {
                actual: maximum_observations,
                maximum: MAX_SPRT_OBSERVATIONS,
            });
        }
        Ok(Self {
            profile_oid,
            null_one_probability,
            alternative_one_probability,
            type_i_error_bound_units,
            type_ii_error_bound_units,
            accept_null_ratio,
            accept_alternative_ratio,
            likelihood_ratio_cap,
            maximum_observations,
        })
    }

    /// Stable profile identity.
    #[must_use]
    pub const fn profile_oid(self) -> ObjectId {
        self.profile_oid
    }

    /// Null probability of a one, over [`SPRT_SCALE`].
    #[must_use]
    pub const fn null_one_probability(self) -> u64 {
        self.null_one_probability
    }

    /// Alternative probability of a one, over [`SPRT_SCALE`].
    #[must_use]
    pub const fn alternative_one_probability(self) -> u64 {
        self.alternative_one_probability
    }

    /// Conservative model-conditional type-I bound over [`SPRT_SCALE`].
    #[must_use]
    pub const fn type_i_error_bound_units(self) -> u64 {
        self.type_i_error_bound_units
    }

    /// Conservative model-conditional type-II bound over [`SPRT_SCALE`].
    #[must_use]
    pub const fn type_ii_error_bound_units(self) -> u64 {
        self.type_ii_error_bound_units
    }

    /// Lower likelihood-ratio boundary.
    #[must_use]
    pub const fn accept_null_ratio(self) -> u128 {
        self.accept_null_ratio
    }

    /// Strict upper likelihood-ratio boundary.
    #[must_use]
    pub const fn accept_alternative_ratio(self) -> u128 {
        self.accept_alternative_ratio
    }

    /// Finite lower-endpoint cap; an upper endpoint exceeding it becomes the
    /// explicit [`SPRT_LIKELIHOOD_INFINITY`] sentinel.
    #[must_use]
    pub const fn likelihood_ratio_cap(self) -> u128 {
        self.likelihood_ratio_cap
    }

    /// Maximum observations admitted by the immutable profile.
    #[must_use]
    pub const fn maximum_observations(self) -> u64 {
        self.maximum_observations
    }
}

/// Caller-owned decode and replay admission limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SprtDecodeLimits {
    /// Maximum encoded evidence bytes accepted.
    pub max_encoded_bytes: usize,
    /// Maximum observations accepted before allocation or replay.
    pub max_observations: u64,
}

impl SprtDecodeLimits {
    /// Constructs explicit decode limits.
    #[must_use]
    pub const fn new(max_encoded_bytes: usize, max_observations: u64) -> Self {
        Self {
            max_encoded_bytes,
            max_observations,
        }
    }
}

/// Canonical, replay-validated SPRT evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SprtEvidence {
    identity: SprtIdentity,
    profile: SprtProfile,
    observations: Vec<SprtObservation>,
    through_sequence: Option<u64>,
    likelihood_ratio_lower: u128,
    likelihood_ratio_upper: u128,
    decision: SprtDecision,
}

impl SprtEvidence {
    /// Immutable trial identity.
    #[must_use]
    pub const fn identity(&self) -> SprtIdentity {
        self.identity
    }

    /// Immutable hypothesis and boundary profile.
    #[must_use]
    pub const fn profile(&self) -> SprtProfile {
        self.profile
    }

    /// Complete authenticated observation transcript.
    #[must_use]
    pub fn observations(&self) -> &[SprtObservation] {
        &self.observations
    }

    /// Last accepted source sequence.
    #[must_use]
    pub const fn through_sequence(&self) -> Option<u64> {
        self.through_sequence
    }

    /// Conservative lower endpoint of the exact likelihood ratio.
    #[must_use]
    pub const fn likelihood_ratio(&self) -> u128 {
        self.likelihood_ratio_lower
    }

    /// Conservative upper endpoint of the exact likelihood ratio. The
    /// positive-infinity sentinel is sticky once finite outward arithmetic
    /// exceeds the profile cap.
    #[must_use]
    pub const fn likelihood_ratio_upper(&self) -> u128 {
        self.likelihood_ratio_upper
    }

    /// Current terminal or continuing decision.
    #[must_use]
    pub const fn decision(&self) -> SprtDecision {
        self.decision
    }

    /// Selected policy; only an accepted alternative selects the candidate.
    #[must_use]
    pub const fn selected_policy_oid(&self) -> ObjectId {
        match self.decision {
            SprtDecision::AcceptAlternative => self.identity.candidate_policy_oid,
            SprtDecision::Continue | SprtDecision::AcceptNull => self.identity.pinned_fallback_oid,
        }
    }

    /// Encodes the complete evidence and independently replayable transcript.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, SprtError> {
        let transcript_len = self.observations.len();
        let requested = FIXED_BYTES
            .checked_add(transcript_len)
            .ok_or(SprtError::LengthOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(requested)
            .map_err(|_| SprtError::AllocationFailed { requested })?;
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&SPRT_EVIDENCE_VERSION.to_le_bytes());
        push_oid(&mut bytes, self.identity.monitor_oid);
        push_oid(&mut bytes, self.identity.filtration_oid);
        push_oid(&mut bytes, self.identity.binary_input_contract_oid);
        push_oid(&mut bytes, self.identity.decision_card_oid);
        push_u64(&mut bytes, self.identity.window.first);
        push_u64(&mut bytes, self.identity.window.last);
        push_u64(&mut bytes, self.identity.regime_epoch);
        push_oid(&mut bytes, self.identity.candidate_policy_oid);
        push_oid(&mut bytes, self.identity.pinned_fallback_oid);
        push_oid(&mut bytes, self.profile.profile_oid);
        push_u64(&mut bytes, self.profile.null_one_probability);
        push_u64(&mut bytes, self.profile.alternative_one_probability);
        push_u64(&mut bytes, self.profile.type_i_error_bound_units);
        push_u64(&mut bytes, self.profile.type_ii_error_bound_units);
        push_u128(&mut bytes, self.profile.accept_null_ratio);
        push_u128(&mut bytes, self.profile.accept_alternative_ratio);
        push_u128(&mut bytes, self.profile.likelihood_ratio_cap);
        push_u64(&mut bytes, self.profile.maximum_observations);
        push_u64(
            &mut bytes,
            u64::try_from(transcript_len).map_err(|_| SprtError::LengthOverflow)?,
        );
        bytes.push(u8::from(self.through_sequence.is_some()));
        push_u64(&mut bytes, self.through_sequence.unwrap_or(0));
        push_u128(&mut bytes, self.likelihood_ratio_lower);
        push_u128(&mut bytes, self.likelihood_ratio_upper);
        bytes.push(self.decision as u8);
        bytes.extend(
            self.observations
                .iter()
                .map(|observation| *observation as u8),
        );
        Ok(bytes)
    }

    /// Decodes and replays evidence under caller-owned resource limits.
    pub fn try_from_canonical_bytes(
        encoded: &[u8],
        limits: SprtDecodeLimits,
    ) -> Result<Self, SprtError> {
        if encoded.len() > limits.max_encoded_bytes {
            return Err(SprtError::EncodedBytesLimitExceeded {
                actual: encoded.len(),
                maximum: limits.max_encoded_bytes,
            });
        }
        let mut decoder = Decoder::new(encoded);
        if decoder.read_array::<8>()? != MAGIC {
            return Err(SprtError::CanonicalMagicMismatch);
        }
        let version = decoder.read_u16()?;
        if version != SPRT_EVIDENCE_VERSION {
            return Err(SprtError::UnsupportedCanonicalVersion { actual: version });
        }
        let monitor_oid = decoder.read_oid()?;
        let filtration_oid = decoder.read_oid()?;
        let binary_input_contract_oid = decoder.read_oid()?;
        let decision_card_oid = decoder.read_oid()?;
        let first = decoder.read_u64()?;
        let last = decoder.read_u64()?;
        let regime_epoch = decoder.read_u64()?;
        let candidate_policy_oid = decoder.read_oid()?;
        let pinned_fallback_oid = decoder.read_oid()?;
        let profile_oid = decoder.read_oid()?;
        let null_one_probability = decoder.read_u64()?;
        let alternative_one_probability = decoder.read_u64()?;
        let type_i_error_bound_units = decoder.read_u64()?;
        let type_ii_error_bound_units = decoder.read_u64()?;
        let accept_null_ratio = decoder.read_u128()?;
        let accept_alternative_ratio = decoder.read_u128()?;
        let likelihood_ratio_cap = decoder.read_u128()?;
        let maximum_observations = decoder.read_u64()?;
        let observation_count = decoder.read_u64()?;
        if observation_count > limits.max_observations {
            return Err(SprtError::ObservationLimitExceeded {
                actual: observation_count,
                maximum: limits.max_observations,
            });
        }
        let has_through = decoder.read_bool()?;
        let through_sequence = decoder.read_u64()?;
        if !has_through && through_sequence != 0 {
            return Err(SprtError::NonCanonicalAbsentThroughSequence {
                actual: through_sequence,
            });
        }
        let encoded_ratio_lower = decoder.read_u128()?;
        let encoded_ratio_upper = decoder.read_u128()?;
        let encoded_decision = SprtDecision::try_from_tag(decoder.read_u8()?)?;
        let count = usize::try_from(observation_count).map_err(|_| SprtError::LengthOverflow)?;
        let transcript = decoder.read_exact(count)?;
        decoder.finish()?;

        let window = SprtSequenceWindow::try_new(first, last)?;
        let identity = SprtIdentity::try_new(
            monitor_oid,
            filtration_oid,
            binary_input_contract_oid,
            decision_card_oid,
            window,
            regime_epoch,
            candidate_policy_oid,
            pinned_fallback_oid,
        )?;
        let profile = SprtProfile::try_new(
            profile_oid,
            null_one_probability,
            alternative_one_probability,
            type_i_error_bound_units,
            type_ii_error_bound_units,
            accept_null_ratio,
            accept_alternative_ratio,
            likelihood_ratio_cap,
            maximum_observations,
        )?;
        let mut trial = SprtTrial::try_new(identity, profile)?;
        trial
            .evidence
            .observations
            .try_reserve_exact(count)
            .map_err(|_| SprtError::AllocationFailed { requested: count })?;
        for (offset, tag) in transcript.iter().copied().enumerate() {
            let sequence = first
                .checked_add(u64::try_from(offset).map_err(|_| SprtError::LengthOverflow)?)
                .ok_or(SprtError::LengthOverflow)?;
            trial.observe(sequence, SprtObservation::try_from_tag(tag)?)?;
        }
        let replayed = trial.evidence();
        let encoded_through = has_through.then_some(through_sequence);
        if replayed.through_sequence != encoded_through
            || replayed.likelihood_ratio_lower != encoded_ratio_lower
            || replayed.likelihood_ratio_upper != encoded_ratio_upper
            || replayed.decision != encoded_decision
        {
            return Err(SprtError::DerivedStateMismatch);
        }
        Ok(replayed)
    }
}

/// Stateful, state-atomic SPRT evaluator.
#[derive(Clone, Debug)]
pub struct SprtTrial {
    evidence: SprtEvidence,
}

impl SprtTrial {
    /// Constructs an empty trial after cross-validating its fixed window.
    pub fn try_new(identity: SprtIdentity, profile: SprtProfile) -> Result<Self, SprtError> {
        if identity.window.length > profile.maximum_observations {
            return Err(SprtError::ProfileCannotContainWindow {
                window: identity.window.length,
                maximum: profile.maximum_observations,
            });
        }
        Ok(Self {
            evidence: SprtEvidence {
                identity,
                profile,
                observations: Vec::new(),
                through_sequence: None,
                likelihood_ratio_lower: SPRT_SCALE,
                likelihood_ratio_upper: SPRT_SCALE,
                decision: SprtDecision::Continue,
            },
        })
    }

    /// Current immutable evidence snapshot.
    #[must_use]
    pub fn evidence(&self) -> SprtEvidence {
        self.evidence.clone()
    }

    /// Accepts the next exact sequence or refuses without mutation.
    pub fn observe(
        &mut self,
        sequence: u64,
        observation: SprtObservation,
    ) -> Result<SprtDecision, SprtError> {
        if self.evidence.decision != SprtDecision::Continue {
            return Err(SprtError::TrialAlreadyTerminal {
                decision: self.evidence.decision,
            });
        }
        let observed = u64::try_from(self.evidence.observations.len())
            .map_err(|_| SprtError::LengthOverflow)?;
        let expected = self
            .evidence
            .identity
            .window
            .first
            .checked_add(observed)
            .ok_or(SprtError::LengthOverflow)?;
        if sequence != expected {
            return Err(SprtError::NonContiguousSequence {
                expected,
                actual: sequence,
            });
        }
        if sequence > self.evidence.identity.window.last
            || observed >= self.evidence.profile.maximum_observations
        {
            return Err(SprtError::ObservationLimitExceeded {
                actual: observed.saturating_add(1),
                maximum: self.evidence.profile.maximum_observations,
            });
        }

        let scale = SPRT_PROBABILITY_SCALE;
        let (numerator, denominator) = match observation {
            SprtObservation::One => (
                self.evidence.profile.alternative_one_probability,
                self.evidence.profile.null_one_probability,
            ),
            SprtObservation::Zero => (
                scale - self.evidence.profile.alternative_one_probability,
                scale - self.evidence.profile.null_one_probability,
            ),
        };
        let next_ratio_lower =
            mul_div_floor(self.evidence.likelihood_ratio_lower, numerator, denominator)?
                .min(self.evidence.profile.likelihood_ratio_cap);
        let next_ratio_upper = mul_div_ceil_or_infinity(
            self.evidence.likelihood_ratio_upper,
            numerator,
            denominator,
            self.evidence.profile.likelihood_ratio_cap,
        )?;
        let next_decision = if next_ratio_lower >= self.evidence.profile.accept_alternative_ratio {
            SprtDecision::AcceptAlternative
        } else if next_ratio_upper <= self.evidence.profile.accept_null_ratio {
            SprtDecision::AcceptNull
        } else {
            SprtDecision::Continue
        };
        self.evidence
            .observations
            .try_reserve(1)
            .map_err(|_| SprtError::AllocationFailed {
                requested: self.evidence.observations.len().saturating_add(1),
            })?;
        self.evidence.observations.push(observation);
        self.evidence.through_sequence = Some(sequence);
        self.evidence.likelihood_ratio_lower = next_ratio_lower;
        self.evidence.likelihood_ratio_upper = next_ratio_upper;
        self.evidence.decision = next_decision;
        Ok(next_decision)
    }
}

fn mul_div_floor(value: u128, numerator: u64, denominator: u64) -> Result<u128, SprtError> {
    let product = value
        .checked_mul(u128::from(numerator))
        .ok_or(SprtError::ArithmeticOverflow)?;
    Ok(product / u128::from(denominator))
}

fn mul_div_ceil_or_infinity(
    value: u128,
    numerator: u64,
    denominator: u64,
    finite_cap: u128,
) -> Result<u128, SprtError> {
    if value == SPRT_LIKELIHOOD_INFINITY {
        return Ok(SPRT_LIKELIHOOD_INFINITY);
    }
    let next = match value.checked_mul(u128::from(numerator)) {
        Some(product) => div_ceil(product, u128::from(denominator))?,
        None => return Ok(SPRT_LIKELIHOOD_INFINITY),
    };
    Ok(if next > finite_cap {
        SPRT_LIKELIHOOD_INFINITY
    } else {
        next
    })
}

fn div_ceil(numerator: u128, denominator: u128) -> Result<u128, SprtError> {
    let quotient = numerator / denominator;
    let has_remainder = !numerator.is_multiple_of(denominator);
    quotient
        .checked_add(u128::from(has_remainder))
        .ok_or(SprtError::ArithmeticOverflow)
}

/// Construction, observation, or canonical replay failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SprtError {
    /// Candidate and deterministic fallback identities were equal.
    CandidateEqualsFallback,
    /// The sequence window was reversed or had unrepresentable length.
    InvalidSequenceWindow { first: u64, last: u64 },
    /// Hypothesis probabilities were invalid or equal.
    InvalidHypotheses,
    /// Declared conditional type-I or type-II bounds were invalid.
    InvalidErrorBounds,
    /// Likelihood-ratio boundaries or cap were invalid.
    InvalidLikelihoodBoundaries,
    /// A fixed profile could not contain its declared trial window.
    ProfileCannotContainWindow { window: u64, maximum: u64 },
    /// Observation or caller admission budget was exceeded.
    ObservationLimitExceeded { actual: u64, maximum: u64 },
    /// A source sequence was skipped, duplicated, or reordered.
    NonContiguousSequence { expected: u64, actual: u64 },
    /// No observation is legal after a terminal decision.
    TrialAlreadyTerminal { decision: SprtDecision },
    /// Checked fixed-point arithmetic overflowed.
    ArithmeticOverflow,
    /// Canonical length arithmetic overflowed.
    LengthOverflow,
    /// A bounded allocation failed.
    AllocationFailed { requested: usize },
    /// Encoded evidence exceeded caller-owned admission.
    EncodedBytesLimitExceeded { actual: usize, maximum: usize },
    /// Canonical bytes ended early.
    CanonicalTruncated { needed: usize, remaining: usize },
    /// Canonical magic did not match.
    CanonicalMagicMismatch,
    /// Canonical version was unsupported.
    UnsupportedCanonicalVersion { actual: u16 },
    /// An observation tag was outside the closed vocabulary.
    UnknownObservationTag { tag: u8 },
    /// A decision tag was outside the closed vocabulary.
    UnknownDecisionTag { tag: u8 },
    /// A canonical boolean tag was invalid.
    InvalidBooleanTag { tag: u8 },
    /// An absent optional sequence carried a nonzero hidden value.
    NonCanonicalAbsentThroughSequence { actual: u64 },
    /// Bytes remained after the declared transcript.
    TrailingCanonicalBytes { count: usize },
    /// Encoded derived state disagreed with transcript replay.
    DerivedStateMismatch,
}

impl fmt::Display for SprtError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for SprtError {}

fn push_oid(bytes: &mut Vec<u8>, oid: ObjectId) {
    bytes.extend_from_slice(&oid.0);
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u128(bytes: &mut Vec<u8>, value: u128) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], SprtError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(SprtError::LengthOverflow)?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or(SprtError::CanonicalTruncated {
                needed: len,
                remaining: self.bytes.len().saturating_sub(self.offset),
            })?;
        self.offset = end;
        Ok(slice)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], SprtError> {
        self.read_exact(N)?
            .try_into()
            .map_err(|_| SprtError::CanonicalTruncated {
                needed: N,
                remaining: 0,
            })
    }

    fn read_u8(&mut self) -> Result<u8, SprtError> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_bool(&mut self) -> Result<bool, SprtError> {
        let tag = self.read_u8()?;
        match tag {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(SprtError::InvalidBooleanTag { tag }),
        }
    }

    fn read_u16(&mut self) -> Result<u16, SprtError> {
        Ok(u16::from_le_bytes(self.read_array()?))
    }

    fn read_u64(&mut self) -> Result<u64, SprtError> {
        Ok(u64::from_le_bytes(self.read_array()?))
    }

    fn read_u128(&mut self) -> Result<u128, SprtError> {
        Ok(u128::from_le_bytes(self.read_array()?))
    }

    fn read_oid(&mut self) -> Result<ObjectId, SprtError> {
        Ok(ObjectId(self.read_array()?))
    }

    fn finish(self) -> Result<(), SprtError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(SprtError::TrailingCanonicalBytes {
                count: self.bytes.len() - self.offset,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    fn trial(last: u64) -> Result<SprtTrial, SprtError> {
        let identity = SprtIdentity::try_new(
            oid(1),
            oid(2),
            oid(3),
            oid(7),
            SprtSequenceWindow::try_new(10, last)?,
            7,
            oid(4),
            oid(5),
        )?;
        let profile = SprtProfile::try_new(
            oid(6),
            400_000_000,
            600_000_000,
            500_000_000,
            500_000_000,
            500_000_000,
            2_000_000_000,
            16_000_000_000,
            32,
        )?;
        SprtTrial::try_new(identity, profile)
    }

    #[test]
    fn hand_vector_crosses_only_the_expected_boundary() -> TestResult {
        let mut alternative = trial(20)?;
        assert_eq!(
            alternative.observe(10, SprtObservation::One)?,
            SprtDecision::Continue
        );
        assert_eq!(alternative.evidence.likelihood_ratio_lower, 1_500_000_000);
        assert_eq!(alternative.evidence.likelihood_ratio_upper, 1_500_000_000);
        assert_eq!(
            alternative.observe(11, SprtObservation::One)?,
            SprtDecision::AcceptAlternative
        );
        assert_eq!(alternative.evidence.likelihood_ratio_lower, 2_250_000_000);
        assert_eq!(alternative.evidence.likelihood_ratio_upper, 2_250_000_000);

        let mut null = trial(20)?;
        assert_eq!(
            null.observe(10, SprtObservation::Zero)?,
            SprtDecision::Continue
        );
        assert_eq!(null.evidence.likelihood_ratio_lower, 666_666_666);
        assert_eq!(null.evidence.likelihood_ratio_upper, 666_666_667);
        assert_eq!(
            null.observe(11, SprtObservation::Zero)?,
            SprtDecision::AcceptNull
        );
        assert_eq!(null.evidence.likelihood_ratio_lower, 444_444_444);
        assert_eq!(null.evidence.likelihood_ratio_upper, 444_444_445);
        Ok(())
    }

    #[test]
    fn sequence_profile_and_terminal_failures_are_atomic() -> TestResult {
        assert_eq!(
            SprtProfile::try_new(
                oid(6),
                400_000_000,
                600_000_000,
                250_000_000,
                500_000_000,
                500_000_000,
                2_000_000_000,
                16_000_000_000,
                32,
            ),
            Err(SprtError::InvalidLikelihoodBoundaries)
        );
        assert_eq!(
            SprtProfile::try_new(
                oid(6),
                400_000_000,
                600_000_000,
                500_000_000,
                250_000_000,
                500_000_000,
                2_000_000_000,
                16_000_000_000,
                32,
            ),
            Err(SprtError::InvalidLikelihoodBoundaries)
        );
        let mut trial = trial(20)?;
        let before = trial.evidence();
        assert_eq!(
            trial.observe(11, SprtObservation::One),
            Err(SprtError::NonContiguousSequence {
                expected: 10,
                actual: 11
            })
        );
        assert_eq!(trial.evidence(), before);
        trial.observe(10, SprtObservation::One)?;
        trial.observe(11, SprtObservation::One)?;
        let terminal = trial.evidence();
        assert!(matches!(
            trial.observe(12, SprtObservation::One),
            Err(SprtError::TrialAlreadyTerminal { .. })
        ));
        assert_eq!(trial.evidence(), terminal);
        Ok(())
    }

    #[test]
    fn finite_cap_never_truncates_the_conservative_upper_endpoint() -> TestResult {
        let identity = SprtIdentity::try_new(
            oid(1),
            oid(2),
            oid(3),
            oid(7),
            SprtSequenceWindow::try_new(10, 12)?,
            7,
            oid(4),
            oid(5),
        )?;
        let alternative_boundary = 1_499_999_993;
        let profile = SprtProfile::try_new(
            oid(6),
            400_000_001,
            599_999_999,
            666_666_670,
            500_000_000,
            500_000_000,
            alternative_boundary,
            alternative_boundary,
            3,
        )?;
        let mut trial = SprtTrial::try_new(identity, profile)?;
        assert_eq!(
            trial.observe(10, SprtObservation::Zero)?,
            SprtDecision::Continue
        );
        assert_eq!(
            trial.observe(11, SprtObservation::One)?,
            SprtDecision::Continue
        );
        assert_eq!(
            trial.observe(12, SprtObservation::One)?,
            SprtDecision::Continue
        );
        let evidence = trial.evidence();
        assert!(
            SPRT_SCALE * u128::from(profile.alternative_one_probability)
                > alternative_boundary * u128::from(profile.null_one_probability)
        );
        assert!(evidence.likelihood_ratio() < alternative_boundary);
        assert_eq!(evidence.likelihood_ratio_upper(), SPRT_LIKELIHOOD_INFINITY);
        assert_eq!(
            SprtEvidence::try_from_canonical_bytes(
                &evidence.try_to_canonical_bytes()?,
                SprtDecodeLimits::new(usize::MAX, 3),
            )?,
            evidence
        );
        Ok(())
    }

    #[test]
    fn canonical_roundtrip_replays_and_mutations_fail() -> TestResult {
        let mut trial = trial(20)?;
        trial.observe(10, SprtObservation::One)?;
        let evidence = trial.evidence();
        let bytes = evidence.try_to_canonical_bytes()?;
        let limits = SprtDecodeLimits::new(bytes.len(), 1);
        let decoded = SprtEvidence::try_from_canonical_bytes(&bytes, limits)?;
        assert_eq!(decoded, evidence);
        assert_eq!(decoded.try_to_canonical_bytes()?, bytes);

        let mut wrong_version = bytes.clone();
        wrong_version[8..10].copy_from_slice(&2_u16.to_le_bytes());
        assert!(matches!(
            SprtEvidence::try_from_canonical_bytes(&wrong_version, limits),
            Err(SprtError::UnsupportedCanonicalVersion { actual: 2 })
        ));
        let mut wrong_state = bytes.clone();
        let ratio_offset = wrong_state.len() - 1 - 1 - 16;
        wrong_state[ratio_offset] ^= 1;
        assert_eq!(
            SprtEvidence::try_from_canonical_bytes(&wrong_state, limits),
            Err(SprtError::DerivedStateMismatch)
        );
        assert!(matches!(
            SprtEvidence::try_from_canonical_bytes(&bytes, SprtDecodeLimits::new(bytes.len(), 0)),
            Err(SprtError::ObservationLimitExceeded { .. })
        ));
        Ok(())
    }
}
