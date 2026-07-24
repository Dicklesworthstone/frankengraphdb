//! Replayable no-regret scheduler adaptation.
//!
//! This module implements a bounded EXP3-style controller over a canonical
//! policy action space. The controller's regret behavior is advisory
//! statistical evidence, never an invariant claim. Unsupported assumptions,
//! a typed regime transition, or any exhausted bound preserves the explicitly
//! pinned deterministic fallback.
//!
//! Every accepted decision records the complete action distribution and
//! pre-decision weight vector. Every accepted feedback record adds the
//! normalized loss, importance-weighted loss, and post-feedback weight vector.
//! The records use canonical IEEE-754 bits so a replay with the same identity,
//! seed, inputs, compilation target, standard-library floating-point
//! implementation, and foundation version is byte-identical. EXP3 uses
//! `f64::exp`; this module therefore makes no cross-target byte-identity claim.
//! The explicit numeric fingerprint, raw RNG word, full probability bits,
//! exact integer sampling masses, and full weight bits are recorded so a
//! replay outside that environment fails closed instead of silently moving a
//! sampling boundary.

use core::fmt;
use std::collections::VecDeque;

use asupersync::util::DetRng;
use fgdb_types::ObjectId;

/// Absolute action-count ceiling for one controller.
pub const MAX_NO_REGRET_ARMS: usize = 4_096;

/// Absolute decision-epoch ceiling for one controller.
pub const MAX_NO_REGRET_DECISION_EPOCHS: usize = 1_048_576;

/// Absolute regime-epoch ceiling for one controller.
pub const MAX_NO_REGRET_REGIME_EPOCHS: usize = 65_536;

/// Absolute retained replay-event ceiling for one controller.
pub const MAX_NO_REGRET_RETAINED_RECEIPTS: usize = 65_536;

/// Absolute total action positions retained across replay events.
pub const MAX_NO_REGRET_RETAINED_ARM_SLOTS: usize = 1_048_576;

/// Absolute retained vector-payload budget for one controller or replay log.
pub const MAX_NO_REGRET_RETAINED_VECTOR_BYTES: usize = 64 * 1_048_576;

const ACTION_SPACE_ENCODING_MAGIC: [u8; 8] = *b"FGDBNSA1";
const PROFILE_DESCRIPTOR_MAGIC: [u8; 8] = *b"FGDBNRP1";
const DECISION_ENCODING_MAGIC: [u8; 8] = *b"FGDBNRD2";
const FEEDBACK_ENCODING_MAGIC: [u8; 8] = *b"FGDBNRF2";
const REGIME_ENCODING_MAGIC: [u8; 8] = *b"FGDBNRR2";
const REPLAY_LOG_ENCODING_MAGIC: [u8; 8] = *b"FGDBNRL1";
const CANONICAL_ENCODING_VERSION: u16 = 2;
const ACTION_SPACE_ENCODING_VERSION: u16 = 1;
const PROFILE_DESCRIPTOR_VERSION: u16 = 1;
const REPLAY_LOG_ENCODING_VERSION: u16 = 1;
const REPLAY_LOG_RESERVED: u16 = 0;
const REPLAY_LOG_FRAME_RESERVED: [u8; 3] = [0; 3];
const OBJECT_ID_BYTES: usize = 32;
const FLOAT_BYTES: usize = 8;
const MAX_EXPONENT_MAGNITUDE: f64 = 700.0;
const SAMPLE_SCALE: u64 = 1u64 << 53;
const NUMERIC_FINGERPRINT_BYTES: usize = 113;
const IDENTITY_FIXED_BYTES: usize = 288 + NUMERIC_FINGERPRINT_BYTES;
const PROFILE_FIXED_BYTES: usize = 76;
const DECISION_FIXED_BYTES: usize = 10 + IDENTITY_FIXED_BYTES + PROFILE_FIXED_BYTES + 154;
const FEEDBACK_FIXED_BYTES: usize = 34;
const REGIME_RESET_FIXED_BYTES: usize = 10 + IDENTITY_FIXED_BYTES + PROFILE_FIXED_BYTES + 157;
const REPLAY_LOG_FIXED_BYTES: usize =
    8 + 2 + 2 + IDENTITY_FIXED_BYTES + PROFILE_FIXED_BYTES + 1 + 4 + 4 + 8;
const REPLAY_LOG_FRAME_BYTES: usize = 1 + 3 + 4;
const FEEDBACK_RETAINED_BYTES_PER_ARM: usize = OBJECT_ID_BYTES + (FLOAT_BYTES * 4);
const REGIME_RESET_RETAINED_BYTES_PER_ARM: usize = OBJECT_ID_BYTES + (FLOAT_BYTES * 2);

/// Caller-owned admission bounds for one canonical no-regret replay log.
///
/// Encoded profile fields never authorize their own allocation. Decoding
/// validates the complete frame inventory against these limits before
/// materializing an action space or any event receipt.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretReplayLogDecodeLimits {
    /// Largest complete encoded log accepted.
    pub max_encoded_bytes: usize,
    /// Largest canonical action space accepted.
    pub max_arms: usize,
    /// Largest chronological event inventory accepted.
    pub max_events: usize,
    /// Largest individual feedback or regime-reset frame accepted.
    pub max_event_bytes: usize,
    /// Largest cumulative action-position inventory across retained events.
    pub max_retained_arm_slots: usize,
    /// Largest cumulative retained vector payload accepted.
    pub max_retained_vector_bytes: usize,
}

impl NoRegretReplayLogDecodeLimits {
    /// Constructs an explicit replay-log admission policy.
    #[must_use]
    pub const fn new(
        max_encoded_bytes: usize,
        max_arms: usize,
        max_events: usize,
        max_event_bytes: usize,
    ) -> Self {
        Self {
            max_encoded_bytes,
            max_arms,
            max_events,
            max_event_bytes,
            max_retained_arm_slots: MAX_NO_REGRET_RETAINED_ARM_SLOTS,
            max_retained_vector_bytes: MAX_NO_REGRET_RETAINED_VECTOR_BYTES,
        }
    }

    /// Narrows the cumulative retained-vector admission budgets.
    #[must_use]
    pub const fn with_retention_budget(
        mut self,
        max_retained_arm_slots: usize,
        max_retained_vector_bytes: usize,
    ) -> Self {
        self.max_retained_arm_slots = max_retained_arm_slots;
        self.max_retained_vector_bytes = max_retained_vector_bytes;
        self
    }
}

/// Native byte order recorded by the numeric replay fingerprint.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoRegretEndian {
    /// Least-significant byte first.
    Little = 1,
    /// Most-significant byte first.
    Big = 2,
}

impl NoRegretEndian {
    const fn canonical_tag(self) -> u8 {
        self as u8
    }
}

/// Numeric fingerprint component named by a mismatch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoRegretFingerprintField {
    /// Fingerprint schema version.
    SchemaVersion,
    /// IEEE-754 radix, precision, or exponent range.
    FloatAbi,
    /// Native pointer width.
    PointerWidth,
    /// Native byte order.
    Endian,
    /// Rust toolchain identity.
    Toolchain,
    /// asupersync foundation identity.
    Foundation,
    /// Floating-point math implementation identity.
    MathAbi,
}

/// Explicit numeric environment under which EXP3 receipt replay is valid.
///
/// The structural fields are populated from the compiling target. The three
/// object identities bind the precise Rust toolchain, asupersync foundation
/// revision, and `f64::exp` implementation used by the controller. Callers
/// obtain those identities from their authoritative registry; a generic
/// "same target" promise is deliberately insufficient.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretNumericFingerprint {
    schema_version: u16,
    float_radix: u32,
    float_mantissa_digits: u32,
    float_min_exp: i16,
    float_max_exp: i16,
    pointer_width: u16,
    endian: NoRegretEndian,
    toolchain_oid: ObjectId,
    foundation_oid: ObjectId,
    math_abi_oid: ObjectId,
}

impl NoRegretNumericFingerprint {
    /// Binds the current target ABI to authoritative implementation identities.
    #[must_use]
    pub const fn current(
        toolchain_oid: ObjectId,
        foundation_oid: ObjectId,
        math_abi_oid: ObjectId,
    ) -> Self {
        Self {
            schema_version: 1,
            float_radix: f64::RADIX,
            float_mantissa_digits: f64::MANTISSA_DIGITS,
            float_min_exp: f64::MIN_EXP as i16,
            float_max_exp: f64::MAX_EXP as i16,
            pointer_width: usize::BITS as u16,
            endian: if cfg!(target_endian = "little") {
                NoRegretEndian::Little
            } else {
                NoRegretEndian::Big
            },
            toolchain_oid,
            foundation_oid,
            math_abi_oid,
        }
    }

    /// Numeric fingerprint schema version.
    #[must_use]
    pub const fn schema_version(self) -> u16 {
        self.schema_version
    }

    /// Floating-point radix.
    #[must_use]
    pub const fn float_radix(self) -> u32 {
        self.float_radix
    }

    /// Floating-point significand precision.
    #[must_use]
    pub const fn float_mantissa_digits(self) -> u32 {
        self.float_mantissa_digits
    }

    /// Minimum normal floating-point exponent.
    #[must_use]
    pub const fn float_min_exp(self) -> i16 {
        self.float_min_exp
    }

    /// Maximum normal floating-point exponent.
    #[must_use]
    pub const fn float_max_exp(self) -> i16 {
        self.float_max_exp
    }

    /// Native pointer width.
    #[must_use]
    pub const fn pointer_width(self) -> u16 {
        self.pointer_width
    }

    /// Native byte order.
    #[must_use]
    pub const fn endian(self) -> NoRegretEndian {
        self.endian
    }

    /// Authoritative Rust toolchain identity.
    #[must_use]
    pub const fn toolchain_oid(self) -> ObjectId {
        self.toolchain_oid
    }

    /// Authoritative asupersync foundation revision identity.
    #[must_use]
    pub const fn foundation_oid(self) -> ObjectId {
        self.foundation_oid
    }

    /// Authoritative floating-point math implementation identity.
    #[must_use]
    pub const fn math_abi_oid(self) -> ObjectId {
        self.math_abi_oid
    }

    fn validate_current_target_abi(self) -> Result<(), NoRegretError> {
        let current = Self::current(self.toolchain_oid, self.foundation_oid, self.math_abi_oid);
        if let Some(component) = structural_fingerprint_difference(current, self) {
            return Err(NoRegretError::NumericFingerprintMismatch { component });
        }
        Ok(())
    }

    fn validate_against_trusted(
        self,
        trusted: NoRegretNumericFingerprint,
    ) -> Result<(), NoRegretError> {
        trusted.validate_current_target_abi()?;
        self.validate_current_target_abi()?;
        if let Some(component) = fingerprint_difference(trusted, self) {
            return Err(NoRegretError::NumericFingerprintMismatch { component });
        }
        Ok(())
    }
}

/// Exact comparison for public replay metadata.
///
/// Scheduler identities, canonical receipts, sequence counters, and numeric
/// fingerprints are auditable public evidence rather than credentials or key
/// material. They require ordinary semantic equality, not timing-oblivious
/// secret comparison. Keeping that distinction explicit also prevents generic
/// security scanners from prescribing a cryptographic primitive for values
/// whose `PartialEq` implementation is part of the replay contract.
fn public_value_eq<T: PartialEq + ?Sized>(left: &T, right: &T) -> bool {
    left == right
}

fn public_values_differ<T: PartialEq + ?Sized>(left: &T, right: &T) -> bool {
    left != right
}

fn structural_fingerprint_difference(
    expected: NoRegretNumericFingerprint,
    actual: NoRegretNumericFingerprint,
) -> Option<NoRegretFingerprintField> {
    if expected.schema_version != actual.schema_version {
        Some(NoRegretFingerprintField::SchemaVersion)
    } else if expected.float_radix != actual.float_radix
        || expected.float_mantissa_digits != actual.float_mantissa_digits
        || expected.float_min_exp != actual.float_min_exp
        || expected.float_max_exp != actual.float_max_exp
    {
        Some(NoRegretFingerprintField::FloatAbi)
    } else if expected.pointer_width != actual.pointer_width {
        Some(NoRegretFingerprintField::PointerWidth)
    } else if public_values_differ(&expected.endian, &actual.endian) {
        Some(NoRegretFingerprintField::Endian)
    } else {
        None
    }
}

fn fingerprint_difference(
    expected: NoRegretNumericFingerprint,
    actual: NoRegretNumericFingerprint,
) -> Option<NoRegretFingerprintField> {
    if let Some(component) = structural_fingerprint_difference(expected, actual) {
        Some(component)
    } else if expected.toolchain_oid != actual.toolchain_oid {
        Some(NoRegretFingerprintField::Toolchain)
    } else if expected.foundation_oid != actual.foundation_oid {
        Some(NoRegretFingerprintField::Foundation)
    } else if expected.math_abi_oid != actual.math_abi_oid {
        Some(NoRegretFingerprintField::MathAbi)
    } else {
        None
    }
}

/// Explicit assumptions under which EXP3 regret evidence is meaningful.
///
/// These attestations are carried into every receipt. They do not turn a
/// statistical statement into an invariant and cannot remove the fallback.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretAssumptions {
    bounded_normalized_losses: bool,
    selected_action_feedback: bool,
    non_anticipating_losses: bool,
    stable_action_space_within_regime: bool,
}

impl NoRegretAssumptions {
    /// Constructs an explicit assumption attestation.
    #[must_use]
    pub const fn new(
        bounded_normalized_losses: bool,
        selected_action_feedback: bool,
        non_anticipating_losses: bool,
        stable_action_space_within_regime: bool,
    ) -> Self {
        Self {
            bounded_normalized_losses,
            selected_action_feedback,
            non_anticipating_losses,
            stable_action_space_within_regime,
        }
    }

    /// Constructs the complete supported assumption set.
    #[must_use]
    pub const fn fully_supported() -> Self {
        Self::new(true, true, true, true)
    }

    /// Whether every supplied loss is attested to use the normalized interval.
    #[must_use]
    pub const fn bounded_normalized_losses(self) -> bool {
        self.bounded_normalized_losses
    }

    /// Whether feedback observes the selected action without substitution.
    #[must_use]
    pub const fn selected_action_feedback(self) -> bool {
        self.selected_action_feedback
    }

    /// Whether loss assignment is fixed before observing this step's RNG draw.
    #[must_use]
    pub const fn non_anticipating_losses(self) -> bool {
        self.non_anticipating_losses
    }

    /// Whether the action space is stable inside one declared regime.
    #[must_use]
    pub const fn stable_action_space_within_regime(self) -> bool {
        self.stable_action_space_within_regime
    }

    /// Whether all scoped assumptions are explicitly supported.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        self.bounded_normalized_losses
            && self.selected_action_feedback
            && self.non_anticipating_losses
            && self.stable_action_space_within_regime
    }

    fn canonical_flags(self) -> u8 {
        u8::from(self.bounded_normalized_losses)
            | (u8::from(self.selected_action_feedback) << 1)
            | (u8::from(self.non_anticipating_losses) << 2)
            | (u8::from(self.stable_action_space_within_regime) << 3)
    }
}

/// Immutable replay identity for one controller window.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretIdentity {
    monitor_oid: ObjectId,
    profile_oid: ObjectId,
    state_space_oid: ObjectId,
    features_oid: ObjectId,
    policy_epoch_oid: ObjectId,
    regime_oid: ObjectId,
    window_oid: ObjectId,
    regime_epoch: u64,
    first_sequence: u64,
    last_sequence: u64,
    window_capacity: u64,
    pinned_fallback_oid: ObjectId,
    replay_seed: u64,
    numeric_fingerprint: NoRegretNumericFingerprint,
}

impl NoRegretIdentity {
    /// Constructs the complete identity and a finite inclusive decision window.
    #[allow(clippy::too_many_arguments)]
    pub const fn try_new(
        monitor_oid: ObjectId,
        profile_oid: ObjectId,
        state_space_oid: ObjectId,
        features_oid: ObjectId,
        policy_epoch_oid: ObjectId,
        regime_oid: ObjectId,
        window_oid: ObjectId,
        regime_epoch: u64,
        first_sequence: u64,
        last_sequence: u64,
        pinned_fallback_oid: ObjectId,
        replay_seed: u64,
        numeric_fingerprint: NoRegretNumericFingerprint,
    ) -> Result<Self, NoRegretError> {
        let Some(distance) = last_sequence.checked_sub(first_sequence) else {
            return Err(NoRegretError::ReversedWindow {
                first: first_sequence,
                last: last_sequence,
            });
        };
        let Some(window_capacity) = distance.checked_add(1) else {
            return Err(NoRegretError::WindowLengthOverflow {
                first: first_sequence,
                last: last_sequence,
            });
        };
        Ok(Self {
            monitor_oid,
            profile_oid,
            state_space_oid,
            features_oid,
            policy_epoch_oid,
            regime_oid,
            window_oid,
            regime_epoch,
            first_sequence,
            last_sequence,
            window_capacity,
            pinned_fallback_oid,
            replay_seed,
            numeric_fingerprint,
        })
    }

    /// Registered monitor identity.
    #[must_use]
    pub const fn monitor_oid(self) -> ObjectId {
        self.monitor_oid
    }

    /// Registered profile identity.
    #[must_use]
    pub const fn profile_oid(self) -> ObjectId {
        self.profile_oid
    }

    /// Canonical action-state-space identity.
    #[must_use]
    pub const fn state_space_oid(self) -> ObjectId {
        self.state_space_oid
    }

    /// Canonical observed-feature stream used by the scheduler.
    #[must_use]
    pub const fn features_oid(self) -> ObjectId {
        self.features_oid
    }

    /// Stream-sequenced decision-policy epoch identity.
    #[must_use]
    pub const fn policy_epoch_oid(self) -> ObjectId {
        self.policy_epoch_oid
    }

    /// Initial regime identity.
    #[must_use]
    pub const fn regime_oid(self) -> ObjectId {
        self.regime_oid
    }

    /// Finite observation-window identity.
    #[must_use]
    pub const fn window_oid(self) -> ObjectId {
        self.window_oid
    }

    /// Initial regime epoch.
    #[must_use]
    pub const fn regime_epoch(self) -> u64 {
        self.regime_epoch
    }

    /// Inclusive first decision sequence.
    #[must_use]
    pub const fn first_sequence(self) -> u64 {
        self.first_sequence
    }

    /// Inclusive last decision sequence.
    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    /// Number of positions in the finite decision window.
    #[must_use]
    pub const fn window_capacity(self) -> u64 {
        self.window_capacity
    }

    /// Deterministic fallback that every receipt retains.
    #[must_use]
    pub const fn pinned_fallback_oid(self) -> ObjectId {
        self.pinned_fallback_oid
    }

    /// Explicit seed used to replay action sampling.
    #[must_use]
    pub const fn replay_seed(self) -> u64 {
        self.replay_seed
    }

    /// Exact numeric ABI, toolchain, foundation, and math implementation.
    #[must_use]
    pub const fn numeric_fingerprint(self) -> NoRegretNumericFingerprint {
        self.numeric_fingerprint
    }
}

/// Bounded numeric and retention profile for one controller.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretProfile {
    profile_oid: ObjectId,
    learning_rate_bits: u64,
    exploration_rate_bits: u64,
    maximum_arms: usize,
    maximum_decision_epochs: usize,
    maximum_regime_epochs: usize,
    retained_receipts: usize,
}

impl NoRegretProfile {
    /// Validates EXP3 rates and every resource ceiling.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        profile_oid: ObjectId,
        learning_rate: f64,
        exploration_rate: f64,
        maximum_arms: usize,
        maximum_decision_epochs: usize,
        maximum_regime_epochs: usize,
        retained_receipts: usize,
    ) -> Result<Self, NoRegretError> {
        validate_open_closed_unit_rate(NoRegretNumericField::LearningRate, learning_rate)?;
        validate_open_closed_unit_rate(NoRegretNumericField::ExplorationRate, exploration_rate)?;
        validate_nonzero_bound(NoRegretBoundKind::Arms, maximum_arms, MAX_NO_REGRET_ARMS)?;
        validate_nonzero_bound(
            NoRegretBoundKind::DecisionEpochs,
            maximum_decision_epochs,
            MAX_NO_REGRET_DECISION_EPOCHS,
        )?;
        validate_nonzero_bound(
            NoRegretBoundKind::RegimeEpochs,
            maximum_regime_epochs,
            MAX_NO_REGRET_REGIME_EPOCHS,
        )?;
        validate_nonzero_bound(
            NoRegretBoundKind::RetainedReceipts,
            retained_receipts,
            MAX_NO_REGRET_RETAINED_RECEIPTS,
        )?;
        Ok(Self {
            profile_oid,
            learning_rate_bits: canonical_float_bits(learning_rate),
            exploration_rate_bits: canonical_float_bits(exploration_rate),
            maximum_arms,
            maximum_decision_epochs,
            maximum_regime_epochs,
            retained_receipts,
        })
    }

    /// Constructs and authoritatively binds an exact scheduler profile.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_verified<V: NoRegretProfileIdentityVerifier + ?Sized>(
        profile_oid: ObjectId,
        learning_rate: f64,
        exploration_rate: f64,
        maximum_arms: usize,
        maximum_decision_epochs: usize,
        maximum_regime_epochs: usize,
        retained_receipts: usize,
        verifier: &V,
    ) -> Result<Self, NoRegretError> {
        let profile = Self::try_new(
            profile_oid,
            learning_rate,
            exploration_rate,
            maximum_arms,
            maximum_decision_epochs,
            maximum_regime_epochs,
            retained_receipts,
        )?;
        profile.verify_identity(verifier)?;
        Ok(profile)
    }

    /// Emits the complete canonical descriptor an identity authority binds.
    #[allow(clippy::too_many_arguments)]
    pub fn try_canonical_descriptor_bytes(
        learning_rate: f64,
        exploration_rate: f64,
        maximum_arms: usize,
        maximum_decision_epochs: usize,
        maximum_regime_epochs: usize,
        retained_receipts: usize,
    ) -> Result<Vec<u8>, NoRegretError> {
        Self::try_new(
            ObjectId([0; OBJECT_ID_BYTES]),
            learning_rate,
            exploration_rate,
            maximum_arms,
            maximum_decision_epochs,
            maximum_regime_epochs,
            retained_receipts,
        )?
        .try_canonical_bytes()
    }

    /// Returns the canonical descriptor covered by [`Self::profile_oid`].
    pub fn try_canonical_bytes(self) -> Result<Vec<u8>, NoRegretError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(54)
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::CanonicalBytes,
                count: 54,
            })?;
        bytes.extend_from_slice(&PROFILE_DESCRIPTOR_MAGIC);
        push_u16(&mut bytes, PROFILE_DESCRIPTOR_VERSION);
        push_u64(&mut bytes, self.learning_rate_bits);
        push_u64(&mut bytes, self.exploration_rate_bits);
        push_u32(&mut bytes, usize_to_u32(self.maximum_arms)?);
        push_u64(
            &mut bytes,
            u64::try_from(self.maximum_decision_epochs)
                .map_err(|_| NoRegretError::CanonicalLengthOverflow)?,
        );
        push_u64(
            &mut bytes,
            u64::try_from(self.maximum_regime_epochs)
                .map_err(|_| NoRegretError::CanonicalLengthOverflow)?,
        );
        push_u64(
            &mut bytes,
            u64::try_from(self.retained_receipts)
                .map_err(|_| NoRegretError::CanonicalLengthOverflow)?,
        );
        Ok(bytes)
    }

    fn verify_identity<V: NoRegretProfileIdentityVerifier + ?Sized>(
        self,
        verifier: &V,
    ) -> Result<(), NoRegretError> {
        let canonical_profile = self.try_canonical_bytes()?;
        if !verifier.verify_no_regret_profile_oid(self.profile_oid, &canonical_profile) {
            return Err(NoRegretError::ProfileIdentityUnverified {
                claimed: self.profile_oid,
            });
        }
        Ok(())
    }

    /// Registered profile identity.
    #[must_use]
    pub const fn profile_oid(self) -> ObjectId {
        self.profile_oid
    }

    /// Exact canonical learning-rate bits.
    #[must_use]
    pub const fn learning_rate_bits(self) -> u64 {
        self.learning_rate_bits
    }

    /// Learning rate used by the multiplicative update.
    #[must_use]
    pub fn learning_rate(self) -> f64 {
        f64::from_bits(self.learning_rate_bits)
    }

    /// Exact canonical exploration-rate bits.
    #[must_use]
    pub const fn exploration_rate_bits(self) -> u64 {
        self.exploration_rate_bits
    }

    /// Uniform-exploration mixture rate.
    #[must_use]
    pub fn exploration_rate(self) -> f64 {
        f64::from_bits(self.exploration_rate_bits)
    }

    /// Maximum number of actions.
    #[must_use]
    pub const fn maximum_arms(self) -> usize {
        self.maximum_arms
    }

    /// Maximum accepted decision-feedback epochs.
    #[must_use]
    pub const fn maximum_decision_epochs(self) -> usize {
        self.maximum_decision_epochs
    }

    /// Maximum number of distinct regime epochs, including the initial one.
    #[must_use]
    pub const fn maximum_regime_epochs(self) -> usize {
        self.maximum_regime_epochs
    }

    /// Maximum number of feedback/reset events retained in memory.
    #[must_use]
    pub const fn retained_receipts(self) -> usize {
        self.retained_receipts
    }
}

/// Authority that binds a claimed scheduler-profile OID to its complete
/// canonical numeric and resource descriptor.
pub trait NoRegretProfileIdentityVerifier {
    /// Verifies that `claimed_oid` authoritatively identifies
    /// `canonical_profile`.
    fn verify_no_regret_profile_oid(&self, claimed_oid: ObjectId, canonical_profile: &[u8])
    -> bool;
}

/// Authority that verifies the identity of canonical action-space bytes.
///
/// Implementations normally delegate to the database's keyed ObjectId
/// verifier. Returning `false` is fail-closed; this module never substitutes
/// an unkeyed digest for an ObjectId.
pub trait NoRegretStateSpaceIdentityVerifier {
    /// Verifies that `claimed_oid` is the authoritative identity of `bytes`.
    fn verify_state_space_oid(&self, claimed_oid: ObjectId, bytes: &[u8]) -> bool;
}

/// Authority that resolves a regime-change evidence object and authenticates
/// the claimed successor regime and effective sequence.
pub trait NoRegretRegimeTransitionAuthority {
    /// Returns whether the complete transition is authorized by its evidence.
    fn verify_regime_shift(&self, shift: NoRegretRegimeShift) -> bool;
}

/// Complete authority required to decode or replay no-regret receipts.
pub trait NoRegretAuthority:
    NoRegretProfileIdentityVerifier
    + NoRegretStateSpaceIdentityVerifier
    + NoRegretRegimeTransitionAuthority
{
}

impl<T> NoRegretAuthority for T where
    T: NoRegretProfileIdentityVerifier
        + NoRegretStateSpaceIdentityVerifier
        + NoRegretRegimeTransitionAuthority
        + ?Sized
{
}

/// Canonical sorted-unique scheduler action space.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretActionSpace {
    state_space_oid: ObjectId,
    policy_oids: Box<[ObjectId]>,
    fallback_index: usize,
}

impl NoRegretActionSpace {
    /// Validates and authoritatively binds a strictly increasing policy list.
    pub fn try_new<V: NoRegretStateSpaceIdentityVerifier + ?Sized>(
        policy_oids: Vec<ObjectId>,
        pinned_fallback_oid: ObjectId,
        state_space_oid: ObjectId,
        verifier: &V,
    ) -> Result<Self, NoRegretError> {
        let fallback_index = validate_action_space(&policy_oids, pinned_fallback_oid)?;
        let canonical_bytes = canonical_action_space_bytes(&policy_oids, pinned_fallback_oid)?;
        if !verifier.verify_state_space_oid(state_space_oid, &canonical_bytes) {
            return Err(NoRegretError::StateSpaceIdentityUnverified {
                claimed: state_space_oid,
            });
        }
        Ok(Self {
            state_space_oid,
            policy_oids: policy_oids.into_boxed_slice(),
            fallback_index,
        })
    }

    /// Emits the canonical descriptor an identity authority must bind.
    ///
    /// Validation is identical to [`Self::try_new`], but no identity is
    /// claimed yet. This avoids requiring callers to duplicate the durable
    /// action-space framing merely to ask their authority for its ObjectId.
    pub fn try_canonical_descriptor_bytes(
        policy_oids: &[ObjectId],
        pinned_fallback_oid: ObjectId,
    ) -> Result<Vec<u8>, NoRegretError> {
        let _ = validate_action_space(policy_oids, pinned_fallback_oid)?;
        canonical_action_space_bytes(policy_oids, pinned_fallback_oid)
    }

    /// Canonical descriptor bytes covered by the verified state-space OID.
    pub fn try_canonical_bytes(&self) -> Result<Vec<u8>, NoRegretError> {
        canonical_action_space_bytes(
            &self.policy_oids,
            self.policy_oids.get(self.fallback_index).copied().ok_or(
                NoRegretError::InternalIndexOutOfBounds {
                    vector: NoRegretVectorKind::Action,
                    index: self.fallback_index,
                    length: self.policy_oids.len(),
                },
            )?,
        )
    }

    fn verify_identity<V: NoRegretStateSpaceIdentityVerifier + ?Sized>(
        &self,
        verifier: &V,
    ) -> Result<(), NoRegretError> {
        let canonical_bytes = self.try_canonical_bytes()?;
        if !verifier.verify_state_space_oid(self.state_space_oid, &canonical_bytes) {
            return Err(NoRegretError::StateSpaceIdentityUnverified {
                claimed: self.state_space_oid,
            });
        }
        Ok(())
    }

    /// Authoritatively verified canonical state-space identity.
    #[must_use]
    pub const fn state_space_oid(&self) -> ObjectId {
        self.state_space_oid
    }

    /// Canonical action order.
    #[must_use]
    pub fn policy_oids(&self) -> &[ObjectId] {
        &self.policy_oids
    }

    /// Number of actions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.policy_oids.len()
    }

    /// Whether the action space is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.policy_oids.is_empty()
    }

    /// Canonical index of the pinned fallback.
    #[must_use]
    pub const fn fallback_index(&self) -> usize {
        self.fallback_index
    }
}

fn validate_action_space(
    policy_oids: &[ObjectId],
    pinned_fallback_oid: ObjectId,
) -> Result<usize, NoRegretError> {
    if policy_oids.len() < 2 {
        return Err(NoRegretError::TooFewArms {
            actual: policy_oids.len(),
            minimum: 2,
        });
    }
    if policy_oids.len() > MAX_NO_REGRET_ARMS {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::Arms,
            actual: policy_oids.len(),
            maximum: MAX_NO_REGRET_ARMS,
        });
    }
    for (index, pair) in policy_oids.windows(2).enumerate() {
        let [previous, current] = pair else {
            continue;
        };
        if previous == current {
            return Err(NoRegretError::DuplicateArm {
                index: index + 1,
                policy_oid: *current,
            });
        }
        if previous > current {
            return Err(NoRegretError::ArmsOutOfOrder {
                index: index + 1,
                previous: *previous,
                current: *current,
            });
        }
    }
    policy_oids
        .binary_search(&pinned_fallback_oid)
        .map_err(|_| NoRegretError::FallbackNotInActionSpace {
            fallback_oid: pinned_fallback_oid,
        })
}

/// Reason an accepted decision selected its recorded policy.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoRegretSelectionMode {
    /// The supported EXP3 distribution sampled the policy.
    AdaptiveEvidence = 1,
    /// At least one required statistical assumption was unsupported.
    UnsupportedAssumptionsFallback = 2,
    /// A typed regime transition forced the first decision to its fallback.
    RegimeResetFallback = 3,
}

impl NoRegretSelectionMode {
    const fn canonical_tag(self) -> u8 {
        self as u8
    }
}

/// Current regime binding retained in every decision receipt.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretRegime {
    regime_oid: ObjectId,
    regime_epoch: u64,
}

impl NoRegretRegime {
    /// Constructs a typed regime identity.
    #[must_use]
    pub const fn new(regime_oid: ObjectId, regime_epoch: u64) -> Self {
        Self {
            regime_oid,
            regime_epoch,
        }
    }

    /// Regime object identity.
    #[must_use]
    pub const fn regime_oid(self) -> ObjectId {
        self.regime_oid
    }

    /// Monotonic regime epoch.
    #[must_use]
    pub const fn regime_epoch(self) -> u64 {
        self.regime_epoch
    }
}

/// Typed, evidence-bound regime transition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretRegimeShift {
    previous: NoRegretRegime,
    next: NoRegretRegime,
    effective_sequence: u64,
    evidence_oid: ObjectId,
}

impl NoRegretRegimeShift {
    /// Constructs a consecutive regime transition.
    pub fn try_new(
        previous: NoRegretRegime,
        next: NoRegretRegime,
        effective_sequence: u64,
        evidence_oid: ObjectId,
    ) -> Result<Self, NoRegretError> {
        let Some(expected_epoch) = previous.regime_epoch.checked_add(1) else {
            return Err(NoRegretError::RegimeEpochExhausted {
                current: previous.regime_epoch,
            });
        };
        if next.regime_epoch != expected_epoch {
            return Err(NoRegretError::NonConsecutiveRegimeEpoch {
                previous: previous.regime_epoch,
                expected: expected_epoch,
                actual: next.regime_epoch,
            });
        }
        if next.regime_oid == previous.regime_oid {
            return Err(NoRegretError::RegimeIdentityUnchanged {
                regime_oid: next.regime_oid,
            });
        }
        Ok(Self {
            previous,
            next,
            effective_sequence,
            evidence_oid,
        })
    }

    /// Previous regime binding.
    #[must_use]
    pub const fn previous(self) -> NoRegretRegime {
        self.previous
    }

    /// Successor regime binding.
    #[must_use]
    pub const fn next(self) -> NoRegretRegime {
        self.next
    }

    /// First decision sequence governed by the successor regime.
    #[must_use]
    pub const fn effective_sequence(self) -> u64 {
        self.effective_sequence
    }

    /// Statistical change evidence that triggered the transition.
    #[must_use]
    pub const fn evidence_oid(self) -> ObjectId {
        self.evidence_oid
    }
}

/// Copyable result of one accepted choose step.
///
/// The full decision log remains available through
/// [`NoRegretController::pending_decision`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretDecisionSelection {
    sequence: u64,
    ordinal: u64,
    selected_policy_oid: ObjectId,
    selected_probability_bits: u64,
    mode: NoRegretSelectionMode,
}

impl NoRegretDecisionSelection {
    /// Source-stream sequence of this decision.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// Zero-based decision ordinal inside this controller.
    #[must_use]
    pub const fn ordinal(self) -> u64 {
        self.ordinal
    }

    /// Selected policy.
    #[must_use]
    pub const fn selected_policy_oid(self) -> ObjectId {
        self.selected_policy_oid
    }

    /// Exact selected-probability bits.
    #[must_use]
    pub const fn selected_probability_bits(self) -> u64 {
        self.selected_probability_bits
    }

    /// Selected probability.
    #[must_use]
    pub fn selected_probability(self) -> f64 {
        f64::from_bits(self.selected_probability_bits)
    }

    /// Reason the selected policy was eligible.
    #[must_use]
    pub const fn mode(self) -> NoRegretSelectionMode {
        self.mode
    }
}

/// Complete immutable choose-step receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoRegretDecisionReceipt {
    identity: NoRegretIdentity,
    profile: NoRegretProfile,
    regime: NoRegretRegime,
    assumptions: NoRegretAssumptions,
    sequence: u64,
    ordinal: u64,
    mode: NoRegretSelectionMode,
    rng_draw: u64,
    selected_index: usize,
    selected_policy_oid: ObjectId,
    selected_sampling_mass: u64,
    selected_probability_bits: u64,
    pinned_fallback_oid: ObjectId,
    policy_oids: Box<[ObjectId]>,
    probability_bits: Box<[u64]>,
    sampling_masses: Box<[u64]>,
    weight_bits_before: Box<[u64]>,
}

impl NoRegretDecisionReceipt {
    /// Complete immutable trial identity.
    #[must_use]
    pub const fn identity(&self) -> NoRegretIdentity {
        self.identity
    }

    /// Complete bounded numeric profile.
    #[must_use]
    pub const fn profile(&self) -> NoRegretProfile {
        self.profile
    }

    /// Regime active at selection.
    #[must_use]
    pub const fn regime(&self) -> NoRegretRegime {
        self.regime
    }

    /// Explicit statistical assumptions.
    #[must_use]
    pub const fn assumptions(&self) -> NoRegretAssumptions {
        self.assumptions
    }

    /// Source-stream sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Zero-based decision ordinal.
    #[must_use]
    pub const fn ordinal(&self) -> u64 {
        self.ordinal
    }

    /// Selection reason.
    #[must_use]
    pub const fn mode(&self) -> NoRegretSelectionMode {
        self.mode
    }

    /// Exact raw foundation RNG output consumed by selection.
    #[must_use]
    pub const fn rng_draw(&self) -> u64 {
        self.rng_draw
    }

    /// Canonical index of the selected policy.
    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.selected_index
    }

    /// Selected policy identity.
    #[must_use]
    pub const fn selected_policy_oid(&self) -> ObjectId {
        self.selected_policy_oid
    }

    /// Exact selected-action integer mass out of `2^53`.
    #[must_use]
    pub const fn selected_sampling_mass(&self) -> u64 {
        self.selected_sampling_mass
    }

    /// Exact selected-action sampling probability bits.
    #[must_use]
    pub const fn selected_probability_bits(&self) -> u64 {
        self.selected_probability_bits
    }

    /// Pinned fallback retained even for an adaptive selection.
    #[must_use]
    pub const fn pinned_fallback_oid(&self) -> ObjectId {
        self.pinned_fallback_oid
    }

    /// Canonical action ordering for the probability and weight vectors.
    #[must_use]
    pub fn policy_oids(&self) -> &[ObjectId] {
        &self.policy_oids
    }

    /// Exact canonical probability bits in action order.
    #[must_use]
    pub fn probability_bits(&self) -> &[u64] {
        &self.probability_bits
    }

    /// Exact integer sampling masses, summing to `2^53`.
    #[must_use]
    pub fn sampling_masses(&self) -> &[u64] {
        &self.sampling_masses
    }

    /// Materialized action probabilities in canonical order.
    pub fn try_probabilities(&self) -> Result<Vec<f64>, NoRegretError> {
        materialize_floats(
            &self.probability_bits,
            NoRegretAllocationKind::ProbabilityVector,
        )
    }

    /// Exact pre-decision normalized-weight bits in action order.
    #[must_use]
    pub fn weight_bits_before(&self) -> &[u64] {
        &self.weight_bits_before
    }

    /// Constructs the copyable feedback key for this pending decision.
    #[must_use]
    pub fn selection(&self) -> NoRegretDecisionSelection {
        NoRegretDecisionSelection {
            sequence: self.sequence,
            ordinal: self.ordinal,
            selected_policy_oid: self.selected_policy_oid,
            selected_probability_bits: self.selected_probability_bits,
            mode: self.mode,
        }
    }

    /// Emits the unique version-2 canonical decision bytes.
    pub fn try_canonical_bytes(&self) -> Result<Vec<u8>, NoRegretError> {
        let arm_count = self.policy_oids.len();
        let vectors_bytes = arm_count
            .checked_mul(OBJECT_ID_BYTES + FLOAT_BYTES + FLOAT_BYTES + FLOAT_BYTES)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let total = DECISION_FIXED_BYTES
            .checked_add(vectors_bytes)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total)
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::CanonicalBytes,
                count: total,
            })?;
        bytes.extend_from_slice(&DECISION_ENCODING_MAGIC);
        push_u16(&mut bytes, CANONICAL_ENCODING_VERSION);
        push_identity(&mut bytes, self.identity);
        push_profile(&mut bytes, self.profile)?;
        push_object_id(&mut bytes, self.regime.regime_oid);
        push_u64(&mut bytes, self.regime.regime_epoch);
        bytes.push(self.assumptions.canonical_flags());
        push_u64(&mut bytes, self.sequence);
        push_u64(&mut bytes, self.ordinal);
        bytes.push(self.mode.canonical_tag());
        push_u64(&mut bytes, self.rng_draw);
        push_u32(&mut bytes, usize_to_u32(self.selected_index)?);
        push_object_id(&mut bytes, self.selected_policy_oid);
        push_u64(&mut bytes, self.selected_sampling_mass);
        push_u64(&mut bytes, self.selected_probability_bits);
        push_object_id(&mut bytes, self.pinned_fallback_oid);
        push_u32(&mut bytes, usize_to_u32(arm_count)?);
        for policy_oid in &self.policy_oids {
            push_object_id(&mut bytes, *policy_oid);
        }
        for bits in &self.probability_bits {
            push_u64(&mut bytes, *bits);
        }
        for mass in &self.sampling_masses {
            push_u64(&mut bytes, *mass);
        }
        for bits in &self.weight_bits_before {
            push_u64(&mut bytes, *bits);
        }
        Ok(bytes)
    }

    /// Strictly decodes and intrinsically replays version-2 decision bytes.
    pub fn try_from_canonical_bytes<A: NoRegretAuthority + ?Sized>(
        bytes: &[u8],
        authority: &A,
        trusted_fingerprint: NoRegretNumericFingerprint,
    ) -> Result<Self, NoRegretError> {
        Self::try_from_canonical_bytes_with_policy(
            bytes,
            authority,
            trusted_fingerprint,
            MAX_NO_REGRET_ARMS,
            ReceiptRngVerification::FromSeedOrdinal,
        )
    }

    fn try_from_canonical_bytes_with_policy<A: NoRegretAuthority + ?Sized>(
        bytes: &[u8],
        authority: &A,
        trusted_fingerprint: NoRegretNumericFingerprint,
        maximum_arms: usize,
        rng_verification: ReceiptRngVerification,
    ) -> Result<Self, NoRegretError> {
        let maximum = DECISION_FIXED_BYTES
            .checked_add(
                maximum_arms
                    .checked_mul(OBJECT_ID_BYTES + (FLOAT_BYTES * 3))
                    .ok_or(NoRegretError::CanonicalLengthOverflow)?,
            )
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        if bytes.len() > maximum {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::Arms,
                actual: bytes.len(),
                maximum,
            });
        }
        let mut decoder = NoRegretDecoder::new(bytes);
        decoder.expect_magic(DECISION_ENCODING_MAGIC)?;
        decoder.expect_version()?;
        let identity = decoder.read_identity()?;
        identity
            .numeric_fingerprint
            .validate_against_trusted(trusted_fingerprint)?;
        let profile = decoder.read_profile()?;
        profile.verify_identity(authority)?;
        let regime = NoRegretRegime::new(decoder.read_object_id()?, decoder.read_u64()?);
        let assumptions = decode_assumptions(decoder.read_u8()?)?;
        let sequence = decoder.read_u64()?;
        let ordinal = decoder.read_u64()?;
        let mode = decode_selection_mode(decoder.read_u8()?)?;
        let rng_draw = decoder.read_u64()?;
        let selected_index = decoder.read_usize_u32()?;
        let selected_policy_oid = decoder.read_object_id()?;
        let selected_sampling_mass = decoder.read_u64()?;
        let selected_probability_bits = decoder.read_u64()?;
        let pinned_fallback_oid = decoder.read_object_id()?;
        let arm_count = decoder.read_usize_u32()?;
        validate_nonzero_bound(NoRegretBoundKind::Arms, arm_count, maximum_arms)?;
        if arm_count < 2 {
            return Err(NoRegretError::TooFewArms {
                actual: arm_count,
                minimum: 2,
            });
        }
        if arm_count > profile.maximum_arms {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::Arms,
                actual: arm_count,
                maximum: profile.maximum_arms,
            });
        }
        let expected_length = DECISION_FIXED_BYTES
            .checked_add(
                arm_count
                    .checked_mul(OBJECT_ID_BYTES + (FLOAT_BYTES * 3))
                    .ok_or(NoRegretError::CanonicalLengthOverflow)?,
            )
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        if bytes.len() != expected_length {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Length,
            });
        }
        let policy_oids = decoder.read_object_ids(arm_count)?;
        let probability_bits = decoder.read_u64s(arm_count)?;
        let sampling_masses = decoder.read_u64s(arm_count)?;
        let weight_bits_before = decoder.read_u64s(arm_count)?;
        decoder.finish()?;

        let action_space = NoRegretActionSpace::try_new(
            policy_oids.into_vec(),
            pinned_fallback_oid,
            identity.state_space_oid,
            authority,
        )?;
        let receipt = Self {
            identity,
            profile,
            regime,
            assumptions,
            sequence,
            ordinal,
            mode,
            rng_draw,
            selected_index,
            selected_policy_oid,
            selected_sampling_mass,
            selected_probability_bits,
            pinned_fallback_oid,
            policy_oids: action_space.policy_oids,
            probability_bits,
            sampling_masses,
            weight_bits_before,
        };
        verify_decision_intrinsic(&receipt, rng_verification)?;
        if public_values_differ(receipt.try_canonical_bytes()?.as_slice(), bytes) {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Decision,
            });
        }
        Ok(receipt)
    }
}

/// Complete immutable decision-feedback epoch receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoRegretFeedbackReceipt {
    decision: NoRegretDecisionReceipt,
    normalized_loss_bits: u64,
    importance_weighted_loss_bits: u64,
    weight_bits_after: Box<[u64]>,
}

impl NoRegretFeedbackReceipt {
    /// Full choose-step receipt.
    #[must_use]
    pub const fn decision(&self) -> &NoRegretDecisionReceipt {
        &self.decision
    }

    /// Exact normalized-loss bits.
    #[must_use]
    pub const fn normalized_loss_bits(&self) -> u64 {
        self.normalized_loss_bits
    }

    /// Normalized loss in `[0, 1]`.
    #[must_use]
    pub fn normalized_loss(&self) -> f64 {
        f64::from_bits(self.normalized_loss_bits)
    }

    /// Exact importance-weighted-loss bits.
    #[must_use]
    pub const fn importance_weighted_loss_bits(&self) -> u64 {
        self.importance_weighted_loss_bits
    }

    /// Importance-weighted selected-action loss.
    #[must_use]
    pub fn importance_weighted_loss(&self) -> f64 {
        f64::from_bits(self.importance_weighted_loss_bits)
    }

    /// Exact post-feedback normalized-weight bits.
    #[must_use]
    pub fn weight_bits_after(&self) -> &[u64] {
        &self.weight_bits_after
    }

    /// Emits the unique version-2 canonical feedback bytes.
    pub fn try_canonical_bytes(&self) -> Result<Vec<u8>, NoRegretError> {
        let decision_bytes = self.decision.try_canonical_bytes()?;
        let after_bytes = self
            .weight_bits_after
            .len()
            .checked_mul(FLOAT_BYTES)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let total = FEEDBACK_FIXED_BYTES
            .checked_add(decision_bytes.len())
            .and_then(|value| value.checked_add(after_bytes))
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total)
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::CanonicalBytes,
                count: total,
            })?;
        bytes.extend_from_slice(&FEEDBACK_ENCODING_MAGIC);
        push_u16(&mut bytes, CANONICAL_ENCODING_VERSION);
        push_u32(&mut bytes, usize_to_u32(decision_bytes.len())?);
        bytes.extend_from_slice(&decision_bytes);
        push_u64(&mut bytes, self.normalized_loss_bits);
        push_u64(&mut bytes, self.importance_weighted_loss_bits);
        push_u32(&mut bytes, usize_to_u32(self.weight_bits_after.len())?);
        for bits in &self.weight_bits_after {
            push_u64(&mut bytes, *bits);
        }
        Ok(bytes)
    }

    /// Strictly decodes and algorithmically verifies version-2 feedback bytes.
    pub fn try_from_canonical_bytes<A: NoRegretAuthority + ?Sized>(
        bytes: &[u8],
        authority: &A,
        trusted_fingerprint: NoRegretNumericFingerprint,
    ) -> Result<Self, NoRegretError> {
        Self::try_from_canonical_bytes_with_policy(
            bytes,
            authority,
            trusted_fingerprint,
            MAX_NO_REGRET_ARMS,
            ReceiptRngVerification::FromSeedOrdinal,
        )
    }

    fn try_from_canonical_bytes_with_policy<A: NoRegretAuthority + ?Sized>(
        bytes: &[u8],
        authority: &A,
        trusted_fingerprint: NoRegretNumericFingerprint,
        maximum_arms: usize,
        rng_verification: ReceiptRngVerification,
    ) -> Result<Self, NoRegretError> {
        let maximum_decision = DECISION_FIXED_BYTES
            .checked_add(
                maximum_arms
                    .checked_mul(OBJECT_ID_BYTES + (FLOAT_BYTES * 3))
                    .ok_or(NoRegretError::CanonicalLengthOverflow)?,
            )
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let maximum = FEEDBACK_FIXED_BYTES
            .checked_add(maximum_decision)
            .and_then(|value| value.checked_add(maximum_arms.checked_mul(FLOAT_BYTES)?))
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        if bytes.len() > maximum {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Length,
            });
        }
        let mut decoder = NoRegretDecoder::new(bytes);
        decoder.expect_magic(FEEDBACK_ENCODING_MAGIC)?;
        decoder.expect_version()?;
        let decision_length = decoder.read_usize_u32()?;
        if decision_length > maximum_decision {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Decision,
            });
        }
        let decision_bytes = decoder.read_exact(decision_length)?;
        let decision = NoRegretDecisionReceipt::try_from_canonical_bytes_with_policy(
            decision_bytes,
            authority,
            trusted_fingerprint,
            maximum_arms,
            rng_verification,
        )?;
        let normalized_loss_bits = decoder.read_u64()?;
        let importance_weighted_loss_bits = decoder.read_u64()?;
        let after_count = decoder.read_usize_u32()?;
        if after_count != decision.policy_oids.len() {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Weights,
            });
        }
        let weight_bits_after = decoder.read_u64s(after_count)?;
        decoder.finish()?;
        let receipt = Self {
            decision,
            normalized_loss_bits,
            importance_weighted_loss_bits,
            weight_bits_after,
        };
        verify_feedback_intrinsic(&receipt, rng_verification)?;
        if public_values_differ(receipt.try_canonical_bytes()?.as_slice(), bytes) {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Feedback,
            });
        }
        Ok(receipt)
    }
}

/// Copyable result of accepted feedback.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NoRegretFeedbackSummary {
    sequence: u64,
    ordinal: u64,
    selected_policy_oid: ObjectId,
    normalized_loss_bits: u64,
    importance_weighted_loss_bits: u64,
}

impl NoRegretFeedbackSummary {
    /// Completed source-stream sequence.
    #[must_use]
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// Completed zero-based ordinal.
    #[must_use]
    pub const fn ordinal(self) -> u64 {
        self.ordinal
    }

    /// Policy whose selected-action loss was observed.
    #[must_use]
    pub const fn selected_policy_oid(self) -> ObjectId {
        self.selected_policy_oid
    }

    /// Exact normalized-loss bits.
    #[must_use]
    pub const fn normalized_loss_bits(self) -> u64 {
        self.normalized_loss_bits
    }

    /// Exact importance-weighted-loss bits.
    #[must_use]
    pub const fn importance_weighted_loss_bits(self) -> u64 {
        self.importance_weighted_loss_bits
    }
}

/// Immutable proof that a typed regime shift reset the controller to priors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoRegretRegimeResetReceipt {
    identity: NoRegretIdentity,
    profile: NoRegretProfile,
    assumptions: NoRegretAssumptions,
    shift: NoRegretRegimeShift,
    pinned_fallback_oid: ObjectId,
    policy_oids: Box<[ObjectId]>,
    weight_bits_before: Box<[u64]>,
    prior_weight_bits: Box<[u64]>,
}

impl NoRegretRegimeResetReceipt {
    /// Complete immutable controller and trial identity.
    #[must_use]
    pub const fn identity(&self) -> NoRegretIdentity {
        self.identity
    }

    /// Complete numeric and retention profile.
    #[must_use]
    pub const fn profile(&self) -> NoRegretProfile {
        self.profile
    }

    /// Statistical assumptions governing the successor decision.
    #[must_use]
    pub const fn assumptions(&self) -> NoRegretAssumptions {
        self.assumptions
    }

    /// Applied typed transition.
    #[must_use]
    pub const fn shift(&self) -> NoRegretRegimeShift {
        self.shift
    }

    /// Fallback forced for the transition's first decision.
    #[must_use]
    pub const fn pinned_fallback_oid(&self) -> ObjectId {
        self.pinned_fallback_oid
    }

    /// Canonical action identities bound by `identity.state_space_oid`.
    #[must_use]
    pub fn policy_oids(&self) -> &[ObjectId] {
        &self.policy_oids
    }

    /// Exact normalized weights before reset.
    #[must_use]
    pub fn weight_bits_before(&self) -> &[u64] {
        &self.weight_bits_before
    }

    /// Exact uniform prior weights after reset.
    #[must_use]
    pub fn prior_weight_bits(&self) -> &[u64] {
        &self.prior_weight_bits
    }

    /// Emits the unique version-2 canonical reset bytes.
    pub fn try_canonical_bytes(&self) -> Result<Vec<u8>, NoRegretError> {
        let vector_len = self.policy_oids.len();
        let vector_bytes = vector_len
            .checked_mul(OBJECT_ID_BYTES + (FLOAT_BYTES * 2))
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let total = REGIME_RESET_FIXED_BYTES
            .checked_add(vector_bytes)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(total)
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::CanonicalBytes,
                count: total,
            })?;
        bytes.extend_from_slice(&REGIME_ENCODING_MAGIC);
        push_u16(&mut bytes, CANONICAL_ENCODING_VERSION);
        push_identity(&mut bytes, self.identity);
        push_profile(&mut bytes, self.profile)?;
        bytes.push(self.assumptions.canonical_flags());
        push_object_id(&mut bytes, self.shift.previous.regime_oid);
        push_u64(&mut bytes, self.shift.previous.regime_epoch);
        push_object_id(&mut bytes, self.shift.next.regime_oid);
        push_u64(&mut bytes, self.shift.next.regime_epoch);
        push_u64(&mut bytes, self.shift.effective_sequence);
        push_object_id(&mut bytes, self.shift.evidence_oid);
        push_object_id(&mut bytes, self.pinned_fallback_oid);
        push_u32(&mut bytes, usize_to_u32(vector_len)?);
        for policy_oid in &self.policy_oids {
            push_object_id(&mut bytes, *policy_oid);
        }
        for bits in &self.weight_bits_before {
            push_u64(&mut bytes, *bits);
        }
        for bits in &self.prior_weight_bits {
            push_u64(&mut bytes, *bits);
        }
        Ok(bytes)
    }

    /// Strictly decodes and verifies version-2 regime-reset bytes.
    pub fn try_from_canonical_bytes<A: NoRegretAuthority + ?Sized>(
        bytes: &[u8],
        authority: &A,
        trusted_fingerprint: NoRegretNumericFingerprint,
    ) -> Result<Self, NoRegretError> {
        Self::try_from_canonical_bytes_with_limit(
            bytes,
            authority,
            trusted_fingerprint,
            MAX_NO_REGRET_ARMS,
        )
    }

    fn try_from_canonical_bytes_with_limit<A: NoRegretAuthority + ?Sized>(
        bytes: &[u8],
        authority: &A,
        trusted_fingerprint: NoRegretNumericFingerprint,
        maximum_arms: usize,
    ) -> Result<Self, NoRegretError> {
        let maximum = REGIME_RESET_FIXED_BYTES
            .checked_add(
                maximum_arms
                    .checked_mul(OBJECT_ID_BYTES + (FLOAT_BYTES * 2))
                    .ok_or(NoRegretError::CanonicalLengthOverflow)?,
            )
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        if bytes.len() > maximum {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Length,
            });
        }
        let mut decoder = NoRegretDecoder::new(bytes);
        decoder.expect_magic(REGIME_ENCODING_MAGIC)?;
        decoder.expect_version()?;
        let identity = decoder.read_identity()?;
        identity
            .numeric_fingerprint
            .validate_against_trusted(trusted_fingerprint)?;
        let profile = decoder.read_profile()?;
        profile.verify_identity(authority)?;
        let assumptions = decode_assumptions(decoder.read_u8()?)?;
        let previous = NoRegretRegime::new(decoder.read_object_id()?, decoder.read_u64()?);
        let next = NoRegretRegime::new(decoder.read_object_id()?, decoder.read_u64()?);
        let effective_sequence = decoder.read_u64()?;
        let evidence_oid = decoder.read_object_id()?;
        let pinned_fallback_oid = decoder.read_object_id()?;
        let arm_count = decoder.read_usize_u32()?;
        validate_nonzero_bound(NoRegretBoundKind::Arms, arm_count, maximum_arms)?;
        if arm_count < 2 {
            return Err(NoRegretError::TooFewArms {
                actual: arm_count,
                minimum: 2,
            });
        }
        if arm_count > profile.maximum_arms {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::Arms,
                actual: arm_count,
                maximum: profile.maximum_arms,
            });
        }
        let expected_length = REGIME_RESET_FIXED_BYTES
            .checked_add(
                arm_count
                    .checked_mul(OBJECT_ID_BYTES + (FLOAT_BYTES * 2))
                    .ok_or(NoRegretError::CanonicalLengthOverflow)?,
            )
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        if bytes.len() != expected_length {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Length,
            });
        }
        let policy_oids = decoder.read_object_ids(arm_count)?;
        let weight_bits_before = decoder.read_u64s(arm_count)?;
        let prior_weight_bits = decoder.read_u64s(arm_count)?;
        decoder.finish()?;
        let action_space = NoRegretActionSpace::try_new(
            policy_oids.into_vec(),
            pinned_fallback_oid,
            identity.state_space_oid,
            authority,
        )?;
        let shift = NoRegretRegimeShift::try_new(previous, next, effective_sequence, evidence_oid)?;
        if !authority.verify_regime_shift(shift) {
            return Err(NoRegretError::RegimeShiftUnverified {
                next: shift.next,
                evidence_oid: shift.evidence_oid,
            });
        }
        let receipt = Self {
            identity,
            profile,
            assumptions,
            shift,
            pinned_fallback_oid,
            policy_oids: action_space.policy_oids,
            weight_bits_before,
            prior_weight_bits,
        };
        verify_reset_intrinsic(&receipt)?;
        if public_values_differ(receipt.try_canonical_bytes()?.as_slice(), bytes) {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::RegimeReset,
            });
        }
        Ok(receipt)
    }
}

/// Chronological event retained for complete controller replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoRegretReplayEvent {
    /// One complete choose-feedback epoch.
    Feedback(NoRegretFeedbackReceipt),
    /// A typed regime transition and prior reset.
    RegimeReset(NoRegretRegimeResetReceipt),
}

impl NoRegretReplayEvent {
    const fn canonical_tag(&self) -> u8 {
        match self {
            Self::Feedback(_) => 1,
            Self::RegimeReset(_) => 2,
        }
    }

    fn try_canonical_payload(&self) -> Result<Vec<u8>, NoRegretError> {
        match self {
            Self::Feedback(receipt) => receipt.try_canonical_bytes(),
            Self::RegimeReset(receipt) => receipt.try_canonical_bytes(),
        }
    }

    const fn root(&self) -> (NoRegretIdentity, NoRegretProfile, NoRegretAssumptions) {
        match self {
            Self::Feedback(receipt) => (
                receipt.decision.identity,
                receipt.decision.profile,
                receipt.decision.assumptions,
            ),
            Self::RegimeReset(receipt) => (receipt.identity, receipt.profile, receipt.assumptions),
        }
    }
}

/// Self-contained, strictly framed chronological scheduler replay log.
///
/// The log repeats its immutable controller identity, profile, assumptions,
/// and complete canonical action space before the event frames. This makes an
/// empty log unambiguous and lets a decoder compare every root field with
/// caller-owned trusted state before accepting the stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoRegretReplayLog {
    identity: NoRegretIdentity,
    profile: NoRegretProfile,
    action_space: NoRegretActionSpace,
    assumptions: NoRegretAssumptions,
    events: Box<[NoRegretReplayEvent]>,
}

impl NoRegretReplayLog {
    /// Immutable controller identity bound by the log header.
    #[must_use]
    pub const fn identity(&self) -> NoRegretIdentity {
        self.identity
    }

    /// Bounded scheduler profile bound by the log header.
    #[must_use]
    pub const fn profile(&self) -> NoRegretProfile {
        self.profile
    }

    /// Authoritatively verified canonical action space.
    #[must_use]
    pub const fn action_space(&self) -> &NoRegretActionSpace {
        &self.action_space
    }

    /// Explicit assumptions governing every decision receipt.
    #[must_use]
    pub const fn assumptions(&self) -> NoRegretAssumptions {
        self.assumptions
    }

    /// Chronological feedback and regime-reset events.
    #[must_use]
    pub fn events(&self) -> &[NoRegretReplayEvent] {
        &self.events
    }

    /// Number of retained chronological events.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the log has no feedback or reset event.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Emits the unique version-1 framed replay-log bytes.
    pub fn try_canonical_bytes(&self) -> Result<Vec<u8>, NoRegretError> {
        encode_replay_log(
            self.identity,
            self.profile,
            &self.action_space,
            self.assumptions,
            self.events.iter(),
        )
    }

    /// Replays the complete decoded stream against its immutable root.
    pub fn verify<A: NoRegretAuthority + ?Sized>(
        &self,
        authority: &A,
        trusted_fingerprint: NoRegretNumericFingerprint,
    ) -> Result<NoRegretReplaySummary, NoRegretError> {
        NoRegretReplayVerifier::verify_complete(
            self.identity,
            self.profile,
            &self.action_space,
            self.assumptions,
            authority,
            trusted_fingerprint,
            false,
            self.events.iter(),
        )
    }

    /// Strictly decodes, root-binds, and algorithmically replays one log.
    ///
    /// The first pass validates the complete framing and every caller-owned
    /// byte/count bound without allocating. Only then are the action space and
    /// individual event receipts materialized.
    #[allow(clippy::too_many_arguments)]
    pub fn try_from_canonical_bytes<A: NoRegretAuthority + ?Sized>(
        bytes: &[u8],
        limits: NoRegretReplayLogDecodeLimits,
        expected_identity: NoRegretIdentity,
        expected_profile: NoRegretProfile,
        expected_action_space: &NoRegretActionSpace,
        expected_assumptions: NoRegretAssumptions,
        authority: &A,
        trusted_fingerprint: NoRegretNumericFingerprint,
    ) -> Result<Self, NoRegretError> {
        let preflight = preflight_replay_log(bytes, limits)?;
        if preflight.identity != expected_identity
            || preflight.profile != expected_profile
            || preflight.assumptions != expected_assumptions
        {
            return Err(NoRegretError::ReplayMismatch {
                field: NoRegretReplayField::Controller,
            });
        }
        preflight
            .identity
            .numeric_fingerprint
            .validate_against_trusted(trusted_fingerprint)?;
        preflight.profile.verify_identity(authority)?;
        if preflight.event_count > expected_profile.retained_receipts {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::RetainedReceipts,
                actual: preflight.event_count,
                maximum: expected_profile.retained_receipts,
            });
        }

        let mut decoder = NoRegretDecoder::new(bytes);
        decoder.expect_magic(REPLAY_LOG_ENCODING_MAGIC)?;
        if decoder.read_u16()? != REPLAY_LOG_ENCODING_VERSION {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Version,
            });
        }
        if decoder.read_u16()? != REPLAY_LOG_RESERVED {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::ReplayLog,
            });
        }
        let identity = decoder.read_identity()?;
        let profile = decoder.read_profile()?;
        let assumptions = decode_assumptions(decoder.read_u8()?)?;
        let arm_count = decoder.read_usize_u32()?;
        let policy_oids = decoder.read_object_ids(arm_count)?;
        let action_space = NoRegretActionSpace::try_new(
            policy_oids.into_vec(),
            identity.pinned_fallback_oid,
            identity.state_space_oid,
            authority,
        )?;
        if action_space != *expected_action_space {
            return Err(NoRegretError::ReplayMismatch {
                field: NoRegretReplayField::Controller,
            });
        }
        let event_count = decoder.read_usize_u32()?;
        let _framed_bytes = decoder.read_u64()?;

        let mut events = Vec::new();
        events
            .try_reserve_exact(event_count)
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::ReceiptLog,
                count: event_count,
            })?;
        for _ in 0..event_count {
            let tag = decoder.read_u8()?;
            if decoder.read_exact(REPLAY_LOG_FRAME_RESERVED.len())? != REPLAY_LOG_FRAME_RESERVED {
                return Err(NoRegretError::CanonicalMalformed {
                    field: NoRegretCanonicalField::ReplayLog,
                });
            }
            let payload_len = decoder.read_usize_u32()?;
            let payload = decoder.read_exact(payload_len)?;
            let event = match tag {
                1 => NoRegretReplayEvent::Feedback(
                    NoRegretFeedbackReceipt::try_from_canonical_bytes_with_policy(
                        payload,
                        authority,
                        trusted_fingerprint,
                        limits.max_arms,
                        ReceiptRngVerification::DeferredToChronologicalReplay,
                    )?,
                ),
                2 => NoRegretReplayEvent::RegimeReset(
                    NoRegretRegimeResetReceipt::try_from_canonical_bytes_with_limit(
                        payload,
                        authority,
                        trusted_fingerprint,
                        limits.max_arms,
                    )?,
                ),
                _ => {
                    return Err(NoRegretError::CanonicalMalformed {
                        field: NoRegretCanonicalField::ReplayLog,
                    });
                }
            };
            let (event_identity, event_profile, event_assumptions) = event.root();
            if event_identity != identity
                || event_profile != profile
                || event_assumptions != assumptions
            {
                return Err(NoRegretError::ReplayMismatch {
                    field: NoRegretReplayField::Controller,
                });
            }
            events.push(event);
        }
        decoder.finish()?;

        let log = Self {
            identity,
            profile,
            action_space,
            assumptions,
            events: events.into_boxed_slice(),
        };
        let _ = log.verify(authority, trusted_fingerprint)?;
        if log.try_canonical_bytes()?.as_slice() != bytes {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::ReplayLog,
            });
        }
        Ok(log)
    }
}

/// Bounded replayable EXP3 controller.
///
/// The mutable controller deliberately does not implement `Clone`: one live
/// instance owns one RNG stream, one pending-decision slot, and one exact
/// feedback sequence.
#[derive(Debug)]
pub struct NoRegretController {
    identity: NoRegretIdentity,
    profile: NoRegretProfile,
    action_space: NoRegretActionSpace,
    assumptions: NoRegretAssumptions,
    rng: DetRng,
    current_regime: NoRegretRegime,
    weights: Vec<f64>,
    next_sequence: Option<u64>,
    completed_epochs: usize,
    observed_regime_epochs: usize,
    force_fallback_once: bool,
    pending: Option<NoRegretDecisionReceipt>,
    replay_history: VecDeque<NoRegretReplayEvent>,
    replay_history_truncated: bool,
}

impl NoRegretController {
    /// Constructs a controller at uniform priors without consuming RNG state.
    pub fn try_new<
        A: NoRegretProfileIdentityVerifier + NoRegretStateSpaceIdentityVerifier + ?Sized,
    >(
        identity: NoRegretIdentity,
        profile: NoRegretProfile,
        action_space: NoRegretActionSpace,
        assumptions: NoRegretAssumptions,
        trusted_fingerprint: NoRegretNumericFingerprint,
        authority: &A,
    ) -> Result<Self, NoRegretError> {
        if identity.profile_oid != profile.profile_oid {
            return Err(NoRegretError::ProfileIdentityMismatch {
                expected: identity.profile_oid,
                actual: profile.profile_oid,
            });
        }
        identity
            .numeric_fingerprint
            .validate_against_trusted(trusted_fingerprint)?;
        profile.verify_identity(authority)?;
        if identity.state_space_oid != action_space.state_space_oid {
            return Err(NoRegretError::StateSpaceIdentityMismatch {
                expected: identity.state_space_oid,
                actual: action_space.state_space_oid,
            });
        }
        action_space.verify_identity(authority)?;
        if action_space.policy_oids.len() > profile.maximum_arms {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::Arms,
                actual: action_space.policy_oids.len(),
                maximum: profile.maximum_arms,
            });
        }
        validate_exploration_mass_support(profile, action_space.policy_oids.len())?;
        validate_retention_budget(
            action_space.policy_oids.len(),
            profile.retained_receipts,
            MAX_NO_REGRET_RETAINED_ARM_SLOTS,
            MAX_NO_REGRET_RETAINED_VECTOR_BYTES,
        )?;
        let actual_fallback = action_space
            .policy_oids
            .get(action_space.fallback_index)
            .copied()
            .ok_or(NoRegretError::InternalIndexOutOfBounds {
                vector: NoRegretVectorKind::Action,
                index: action_space.fallback_index,
                length: action_space.policy_oids.len(),
            })?;
        if actual_fallback != identity.pinned_fallback_oid {
            return Err(NoRegretError::FallbackIdentityMismatch {
                expected: identity.pinned_fallback_oid,
                actual: actual_fallback,
            });
        }
        let maximum_decision_epochs_u64 =
            u64::try_from(profile.maximum_decision_epochs).map_err(|_| {
                NoRegretError::BoundUnrepresentable {
                    kind: NoRegretBoundKind::DecisionEpochs,
                    actual: profile.maximum_decision_epochs,
                }
            })?;
        if identity.window_capacity > maximum_decision_epochs_u64 {
            return Err(NoRegretError::DecisionWindowExceedsLimit {
                window_capacity: identity.window_capacity,
                limit: profile.maximum_decision_epochs,
            });
        }
        let mut weights = Vec::new();
        weights
            .try_reserve_exact(action_space.policy_oids.len())
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::WeightVector,
                count: action_space.policy_oids.len(),
            })?;
        let prior = uniform_prior(action_space.policy_oids.len())?;
        weights.resize(action_space.policy_oids.len(), prior);

        let mut replay_history = VecDeque::new();
        replay_history
            .try_reserve_exact(profile.retained_receipts)
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::ReceiptLog,
                count: profile.retained_receipts,
            })?;

        Ok(Self {
            identity,
            profile,
            action_space,
            assumptions,
            rng: DetRng::new(identity.replay_seed),
            current_regime: NoRegretRegime::new(identity.regime_oid, identity.regime_epoch),
            weights,
            next_sequence: Some(identity.first_sequence),
            completed_epochs: 0,
            observed_regime_epochs: 1,
            force_fallback_once: false,
            pending: None,
            replay_history,
            replay_history_truncated: false,
        })
    }

    /// Complete immutable controller identity.
    #[must_use]
    pub const fn identity(&self) -> NoRegretIdentity {
        self.identity
    }

    /// Bounded algorithm profile.
    #[must_use]
    pub const fn profile(&self) -> NoRegretProfile {
        self.profile
    }

    /// Canonical action space.
    #[must_use]
    pub const fn action_space(&self) -> &NoRegretActionSpace {
        &self.action_space
    }

    /// Explicit assumption attestation.
    #[must_use]
    pub const fn assumptions(&self) -> NoRegretAssumptions {
        self.assumptions
    }

    /// Current typed regime.
    #[must_use]
    pub const fn current_regime(&self) -> NoRegretRegime {
        self.current_regime
    }

    /// Next sequence that can be selected, or `None` after exhaustion.
    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }

    /// Number of completed choose-feedback epochs.
    #[must_use]
    pub const fn completed_epochs(&self) -> usize {
        self.completed_epochs
    }

    /// Number of observed regime epochs, including the initial epoch.
    #[must_use]
    pub const fn observed_regime_epochs(&self) -> usize {
        self.observed_regime_epochs
    }

    /// Full pending choose receipt, if feedback is outstanding.
    #[must_use]
    pub fn pending_decision(&self) -> Option<&NoRegretDecisionReceipt> {
        self.pending.as_ref()
    }

    /// Completed receipts retained in chronological order.
    #[must_use]
    pub fn retained_receipts(&self) -> impl DoubleEndedIterator<Item = &NoRegretFeedbackReceipt> {
        self.replay_history.iter().filter_map(|event| match event {
            NoRegretReplayEvent::Feedback(receipt) => Some(receipt),
            NoRegretReplayEvent::RegimeReset(_) => None,
        })
    }

    /// Most recently completed retained receipt.
    #[must_use]
    pub fn latest_receipt(&self) -> Option<&NoRegretFeedbackReceipt> {
        self.retained_receipts().next_back()
    }

    /// Number of completed receipts currently retained.
    #[must_use]
    pub fn retained_receipt_count(&self) -> usize {
        self.retained_receipts().count()
    }

    /// Chronological feedback and reset events retained for replay.
    #[must_use]
    pub fn replay_history(&self) -> impl ExactSizeIterator<Item = &NoRegretReplayEvent> {
        self.replay_history.iter()
    }

    /// Whether bounded retention evicted a prefix, making history incomplete.
    #[must_use]
    pub const fn replay_history_truncated(&self) -> bool {
        self.replay_history_truncated
    }

    /// Exact current normalized weight bits.
    pub fn try_weight_bits(&self) -> Result<Vec<u64>, NoRegretError> {
        float_bits_vector(&self.weights, NoRegretAllocationKind::WeightVector)
    }

    /// Current adaptive distribution without consuming the replay RNG.
    ///
    /// Unsupported assumptions and an outstanding regime-reset fallback are
    /// represented as a one-hot fallback distribution.
    pub fn try_current_probability_bits(&self) -> Result<Vec<u64>, NoRegretError> {
        let mode = self.next_selection_mode();
        let probabilities = self.try_probabilities_for_mode(mode)?;
        float_bits_vector(&probabilities, NoRegretAllocationKind::ProbabilityVector)
    }

    /// Accepts the exact next choose step and consumes one foundation RNG word.
    ///
    /// All validation and allocation occurs against a cloned RNG state. An
    /// error leaves the sequence, RNG, weights, pending slot, and receipt log
    /// unchanged.
    pub fn choose(&mut self, sequence: u64) -> Result<NoRegretDecisionSelection, NoRegretError> {
        if let Some(pending) = &self.pending {
            return Err(NoRegretError::FeedbackPending {
                pending_sequence: pending.sequence,
            });
        }
        if self.completed_epochs >= self.profile.maximum_decision_epochs {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::DecisionEpochs,
                actual: self.completed_epochs.saturating_add(1),
                maximum: self.profile.maximum_decision_epochs,
            });
        }
        let expected = self
            .next_sequence
            .ok_or(NoRegretError::DecisionWindowExhausted {
                last_sequence: self.identity.last_sequence,
            })?;
        if sequence != expected {
            return Err(NoRegretError::UnexpectedSequence {
                expected,
                actual: sequence,
            });
        }

        let mode = self.next_selection_mode();
        let probabilities = self.try_probabilities_for_mode(mode)?;
        let probability_bits =
            float_bits_box(&probabilities, NoRegretAllocationKind::ProbabilityVector)?;
        let sampling_masses = exact_sampling_masses(&probability_bits)?.into_boxed_slice();
        let weight_bits_before =
            float_bits_box(&self.weights, NoRegretAllocationKind::WeightVector)?;
        let policy_oids = copy_object_ids(&self.action_space.policy_oids)?;

        let mut trial_rng = self.rng.clone();
        let rng_draw = trial_rng.next_u64();
        let selected_index = match mode {
            NoRegretSelectionMode::AdaptiveEvidence => {
                sample_categorical(&sampling_masses, rng_draw)?
            }
            NoRegretSelectionMode::UnsupportedAssumptionsFallback
            | NoRegretSelectionMode::RegimeResetFallback => self.action_space.fallback_index,
        };
        let selected_policy_oid = self
            .action_space
            .policy_oids
            .get(selected_index)
            .copied()
            .ok_or(NoRegretError::InternalIndexOutOfBounds {
                vector: NoRegretVectorKind::Action,
                index: selected_index,
                length: self.action_space.policy_oids.len(),
            })?;
        let selected_sampling_mass = sampling_masses.get(selected_index).copied().ok_or(
            NoRegretError::InternalIndexOutOfBounds {
                vector: NoRegretVectorKind::Probability,
                index: selected_index,
                length: sampling_masses.len(),
            },
        )?;
        if selected_sampling_mass == 0 {
            return Err(NoRegretError::ZeroMassSelection {
                index: selected_index,
            });
        }
        let selected_probability = sampling_probability(selected_sampling_mass);
        let ordinal = u64::try_from(self.completed_epochs).map_err(|_| {
            NoRegretError::BoundUnrepresentable {
                kind: NoRegretBoundKind::DecisionEpochs,
                actual: self.completed_epochs,
            }
        })?;
        let receipt = NoRegretDecisionReceipt {
            identity: self.identity,
            profile: self.profile,
            regime: self.current_regime,
            assumptions: self.assumptions,
            sequence,
            ordinal,
            mode,
            rng_draw,
            selected_index,
            selected_policy_oid,
            selected_sampling_mass,
            selected_probability_bits: canonical_float_bits(selected_probability),
            pinned_fallback_oid: self.identity.pinned_fallback_oid,
            policy_oids,
            probability_bits,
            sampling_masses,
            weight_bits_before,
        };
        let selection = receipt.selection();

        self.rng = trial_rng;
        self.pending = Some(receipt);
        Ok(selection)
    }

    /// Accepts selected-action feedback for the one pending decision.
    ///
    /// Mismatched sequence, ordinal, policy, or replayed feedback is rejected
    /// without changing any controller state.
    pub fn feedback(
        &mut self,
        selection: NoRegretDecisionSelection,
        normalized_loss: f64,
    ) -> Result<NoRegretFeedbackSummary, NoRegretError> {
        validate_normalized_loss(normalized_loss)?;
        let pending = self
            .pending
            .as_ref()
            .ok_or(NoRegretError::NoPendingDecision)?;
        validate_feedback_key(pending, selection)?;
        let (importance_weighted_loss_bits, next_weights) =
            compute_feedback_update(pending, normalized_loss)?;
        let weight_bits_after =
            float_bits_box(&next_weights, NoRegretAllocationKind::WeightVector)?;

        let normalized_loss_bits = canonical_float_bits(normalized_loss);
        let summary = NoRegretFeedbackSummary {
            sequence: pending.sequence,
            ordinal: pending.ordinal,
            selected_policy_oid: pending.selected_policy_oid,
            normalized_loss_bits,
            importance_weighted_loss_bits,
        };
        let next_sequence = if pending.sequence == self.identity.last_sequence {
            None
        } else {
            Some(pending.sequence + 1)
        };

        let Some(decision) = self.pending.take() else {
            return Err(NoRegretError::NoPendingDecision);
        };
        let receipt = NoRegretFeedbackReceipt {
            decision,
            normalized_loss_bits,
            importance_weighted_loss_bits,
            weight_bits_after,
        };
        self.retain_replay_event(NoRegretReplayEvent::Feedback(receipt));
        self.weights = next_weights;
        self.next_sequence = next_sequence;
        self.completed_epochs += 1;
        if self.force_fallback_once {
            self.force_fallback_once = false;
        }
        Ok(summary)
    }

    /// Applies a typed regime shift, resets weights to uniform priors, and
    /// forces the first decision in the new regime to the pinned fallback.
    ///
    /// A shift is rejected while feedback is pending. All receipt allocations
    /// and transition checks complete before controller state changes.
    pub fn apply_regime_shift<A: NoRegretRegimeTransitionAuthority + ?Sized>(
        &mut self,
        shift: NoRegretRegimeShift,
        authority: &A,
    ) -> Result<NoRegretRegimeResetReceipt, NoRegretError> {
        if let Some(pending) = &self.pending {
            return Err(NoRegretError::RegimeShiftWhileFeedbackPending {
                pending_sequence: pending.sequence,
            });
        }
        if shift.previous != self.current_regime {
            return Err(NoRegretError::PreviousRegimeMismatch {
                expected: self.current_regime,
                actual: shift.previous,
            });
        }
        let expected_sequence =
            self.next_sequence
                .ok_or(NoRegretError::DecisionWindowExhausted {
                    last_sequence: self.identity.last_sequence,
                })?;
        if public_values_differ(&shift.effective_sequence, &expected_sequence) {
            return Err(NoRegretError::RegimeSequenceMismatch {
                expected: expected_sequence,
                actual: shift.effective_sequence,
            });
        }
        if self.observed_regime_epochs >= self.profile.maximum_regime_epochs {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::RegimeEpochs,
                actual: self.observed_regime_epochs.saturating_add(1),
                maximum: self.profile.maximum_regime_epochs,
            });
        }
        if !authority.verify_regime_shift(shift) {
            return Err(NoRegretError::RegimeShiftUnverified {
                next: shift.next,
                evidence_oid: shift.evidence_oid,
            });
        }
        let weight_bits_before =
            float_bits_box(&self.weights, NoRegretAllocationKind::WeightVector)?;
        let prior = uniform_prior(self.action_space.policy_oids.len())?;
        let mut next_weights = Vec::new();
        next_weights
            .try_reserve_exact(self.action_space.policy_oids.len())
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::WeightVector,
                count: self.action_space.policy_oids.len(),
            })?;
        next_weights.resize(self.action_space.policy_oids.len(), prior);
        let prior_weight_bits =
            float_bits_box(&next_weights, NoRegretAllocationKind::WeightVector)?;
        let receipt = NoRegretRegimeResetReceipt {
            identity: self.identity,
            profile: self.profile,
            assumptions: self.assumptions,
            shift,
            pinned_fallback_oid: self.identity.pinned_fallback_oid,
            policy_oids: copy_object_ids(&self.action_space.policy_oids)?,
            weight_bits_before,
            prior_weight_bits,
        };
        let retained_receipt = receipt.clone();

        self.current_regime = shift.next;
        self.observed_regime_epochs += 1;
        self.force_fallback_once = true;
        self.weights = next_weights;
        self.retain_replay_event(NoRegretReplayEvent::RegimeReset(retained_receipt));
        Ok(receipt)
    }

    fn next_selection_mode(&self) -> NoRegretSelectionMode {
        if !self.assumptions.is_supported() {
            NoRegretSelectionMode::UnsupportedAssumptionsFallback
        } else if self.force_fallback_once {
            NoRegretSelectionMode::RegimeResetFallback
        } else {
            NoRegretSelectionMode::AdaptiveEvidence
        }
    }

    fn try_probabilities_for_mode(
        &self,
        mode: NoRegretSelectionMode,
    ) -> Result<Vec<f64>, NoRegretError> {
        probabilities_for(
            self.profile,
            &self.weights,
            self.action_space.fallback_index,
            mode,
        )
    }

    fn retain_replay_event(&mut self, event: NoRegretReplayEvent) {
        if self.replay_history.len() == self.profile.retained_receipts {
            let _ = self.replay_history.pop_front();
            self.replay_history_truncated = true;
        }
        self.replay_history.push_back(event);
    }

    /// Emits a self-contained canonical log for the complete retained history.
    ///
    /// A pending decision has no feedback outcome and is therefore not a
    /// terminal event. A history whose bounded retention evicted a prefix is
    /// likewise refused instead of being mislabeled complete.
    pub fn try_canonical_replay_log_bytes<A: NoRegretAuthority + ?Sized>(
        &self,
        authority: &A,
    ) -> Result<Vec<u8>, NoRegretError> {
        if let Some(pending) = &self.pending {
            return Err(NoRegretError::FeedbackPending {
                pending_sequence: pending.sequence,
            });
        }
        if self.replay_history_truncated {
            return Err(NoRegretError::ReplayHistoryTruncated);
        }
        let _ = self.verify_replay_history(authority, self.identity.numeric_fingerprint)?;
        encode_replay_log(
            self.identity,
            self.profile,
            &self.action_space,
            self.assumptions,
            self.replay_history.iter(),
        )
    }

    /// Replays every retained event from the initial controller state.
    ///
    /// This fails closed when bounded retention has evicted any prefix.
    pub fn verify_replay_history<A: NoRegretAuthority + ?Sized>(
        &self,
        authority: &A,
        expected_fingerprint: NoRegretNumericFingerprint,
    ) -> Result<NoRegretReplaySummary, NoRegretError> {
        NoRegretReplayVerifier::verify_complete(
            self.identity,
            self.profile,
            &self.action_space,
            self.assumptions,
            authority,
            expected_fingerprint,
            self.replay_history_truncated,
            self.replay_history.iter(),
        )
    }
}

/// Final state established by a successful complete-history replay.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NoRegretReplaySummary {
    event_count: usize,
    completed_epochs: usize,
    observed_regime_epochs: usize,
    current_regime: NoRegretRegime,
    next_sequence: Option<u64>,
    final_weight_bits: Box<[u64]>,
}

impl NoRegretReplaySummary {
    /// Total chronological feedback and reset events verified.
    #[must_use]
    pub const fn event_count(&self) -> usize {
        self.event_count
    }

    /// Completed choose-feedback epochs verified.
    #[must_use]
    pub const fn completed_epochs(&self) -> usize {
        self.completed_epochs
    }

    /// Regime epochs observed, including the initial regime.
    #[must_use]
    pub const fn observed_regime_epochs(&self) -> usize {
        self.observed_regime_epochs
    }

    /// Final typed regime.
    #[must_use]
    pub const fn current_regime(&self) -> NoRegretRegime {
        self.current_regime
    }

    /// Next source sequence after replay.
    #[must_use]
    pub const fn next_sequence(&self) -> Option<u64> {
        self.next_sequence
    }

    /// Exact final normalized weights.
    #[must_use]
    pub fn final_weight_bits(&self) -> &[u64] {
        &self.final_weight_bits
    }
}

/// Stateless verifier for a complete chronological no-regret event stream.
pub struct NoRegretReplayVerifier;

impl NoRegretReplayVerifier {
    /// Recomputes every distribution, RNG draw, selection, feedback update,
    /// weight vector, regime reset, and sequence transition.
    #[allow(clippy::too_many_arguments)]
    pub fn verify_complete<'a, I, A>(
        identity: NoRegretIdentity,
        profile: NoRegretProfile,
        action_space: &NoRegretActionSpace,
        assumptions: NoRegretAssumptions,
        authority: &A,
        expected_fingerprint: NoRegretNumericFingerprint,
        history_truncated: bool,
        events: I,
    ) -> Result<NoRegretReplaySummary, NoRegretError>
    where
        I: IntoIterator<Item = &'a NoRegretReplayEvent>,
        A: NoRegretAuthority + ?Sized,
    {
        if history_truncated {
            return Err(NoRegretError::ReplayHistoryTruncated);
        }
        if let Some(component) =
            fingerprint_difference(expected_fingerprint, identity.numeric_fingerprint)
        {
            return Err(NoRegretError::NumericFingerprintMismatch { component });
        }
        expected_fingerprint.validate_current_target_abi()?;
        let mut replay = NoRegretController::try_new(
            identity,
            profile,
            action_space.clone(),
            assumptions,
            expected_fingerprint,
            authority,
        )?;
        let mut event_count = 0usize;
        for event in events {
            match event {
                NoRegretReplayEvent::Feedback(expected) => {
                    let selection = replay.choose(expected.decision.sequence)?;
                    let pending =
                        replay
                            .pending_decision()
                            .ok_or(NoRegretError::ReplayMismatch {
                                field: NoRegretReplayField::EventOrder,
                            })?;
                    if pending != &expected.decision || selection != expected.decision.selection() {
                        return Err(NoRegretError::ReplayMismatch {
                            field: NoRegretReplayField::Selection,
                        });
                    }
                    replay.feedback(selection, expected.normalized_loss())?;
                    match replay.replay_history.back() {
                        Some(NoRegretReplayEvent::Feedback(actual))
                            if public_value_eq(actual, expected) => {}
                        _ => {
                            return Err(NoRegretError::ReplayMismatch {
                                field: NoRegretReplayField::Feedback,
                            });
                        }
                    }
                }
                NoRegretReplayEvent::RegimeReset(expected) => {
                    let actual = replay.apply_regime_shift(expected.shift, authority)?;
                    if &actual != expected {
                        return Err(NoRegretError::ReplayMismatch {
                            field: NoRegretReplayField::RegimeReset,
                        });
                    }
                }
            }
            event_count = event_count
                .checked_add(1)
                .ok_or(NoRegretError::ReplayMismatch {
                    field: NoRegretReplayField::EventOrder,
                })?;
        }
        Ok(NoRegretReplaySummary {
            event_count,
            completed_epochs: replay.completed_epochs,
            observed_regime_epochs: replay.observed_regime_epochs,
            current_regime: replay.current_regime,
            next_sequence: replay.next_sequence,
            final_weight_bits: float_bits_box(
                &replay.weights,
                NoRegretAllocationKind::WeightVector,
            )?,
        })
    }
}

/// Resource bound named by a validation error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoRegretBoundKind {
    /// Canonical actions.
    Arms,
    /// Completed choose-feedback epochs.
    DecisionEpochs,
    /// Distinct typed regime epochs.
    RegimeEpochs,
    /// Completed receipts retained in memory.
    RetainedReceipts,
    /// Cumulative action positions retained across replay events.
    RetainedArmSlots,
    /// Cumulative vector payload retained across replay events.
    RetainedVectorBytes,
}

/// Floating-point field named by a validation error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoRegretNumericField {
    /// Multiplicative update rate.
    LearningRate,
    /// Uniform exploration mixture.
    ExplorationRate,
    /// Caller-supplied selected-action loss.
    NormalizedLoss,
    /// Importance-weighted selected-action loss.
    ImportanceWeightedLoss,
    /// Multiplicative weight-update factor.
    WeightMultiplier,
    /// Internal normalized action weight.
    Weight,
}

/// Fallible allocation named by an error.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoRegretAllocationKind {
    /// Normalized action weights.
    WeightVector,
    /// Complete action distribution.
    ProbabilityVector,
    /// Exact integer sampling masses.
    SamplingMassVector,
    /// Canonical action identity vector.
    ActionVector,
    /// Bounded completed-receipt log.
    ReceiptLog,
    /// Canonical encoded bytes.
    CanonicalBytes,
}

/// Private-vector family named by an impossible-state diagnostic.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoRegretVectorKind {
    /// Canonical policy identities.
    Action,
    /// Action-selection probabilities.
    Probability,
    /// Normalized multiplicative weights.
    Weight,
}

/// Canonical receipt component that failed strict decoding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoRegretCanonicalField {
    /// Eight-byte receipt discriminator.
    Magic,
    /// Canonical format version.
    Version,
    /// Fixed or vector-derived byte length.
    Length,
    /// Numeric fingerprint.
    NumericFingerprint,
    /// Assumption bit flags.
    Assumptions,
    /// Selection-mode tag.
    SelectionMode,
    /// Action count or ordering.
    ActionSpace,
    /// Probability vector.
    Probabilities,
    /// Exact integer sampling masses.
    SamplingMasses,
    /// Normalized weight vector.
    Weights,
    /// Selected action fields.
    Selection,
    /// Embedded decision receipt.
    Decision,
    /// Feedback update.
    Feedback,
    /// Regime reset.
    RegimeReset,
    /// Self-contained replay-log header or frame inventory.
    ReplayLog,
    /// One framed feedback or regime-reset payload.
    ReplayEvent,
}

/// Algorithmic component that diverged during replay.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NoRegretReplayField {
    /// Controller identity or profile.
    Controller,
    /// Numeric ABI/toolchain/foundation fingerprint.
    NumericFingerprint,
    /// RNG output at an ordinal.
    RngDraw,
    /// Pre-decision weights or derived distribution.
    Distribution,
    /// Integer mass apportionment or chosen arm.
    Selection,
    /// Importance-weighted feedback or updated weights.
    Feedback,
    /// Typed regime transition or prior reset.
    RegimeReset,
    /// Complete event ordering.
    EventOrder,
}

/// Construction, sequencing, transition, numeric, or allocation failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NoRegretError {
    /// Inclusive window endpoints were reversed.
    ReversedWindow { first: u64, last: u64 },
    /// Inclusive window length overflowed `u64`.
    WindowLengthOverflow { first: u64, last: u64 },
    /// A floating-point input was not finite.
    NonFiniteNumeric {
        field: NoRegretNumericField,
        bits: u64,
    },
    /// A rate was outside its declared interval.
    RateOutOfRange {
        field: NoRegretNumericField,
        bits: u64,
        zero_allowed: bool,
    },
    /// A configured resource bound was zero.
    ZeroBound { kind: NoRegretBoundKind },
    /// A configured or observed resource count exceeded its ceiling.
    BoundExceeded {
        kind: NoRegretBoundKind,
        actual: usize,
        maximum: usize,
    },
    /// A resource count could not be represented in its canonical scalar.
    BoundUnrepresentable {
        kind: NoRegretBoundKind,
        actual: usize,
    },
    /// The action space had fewer than two policies.
    TooFewArms { actual: usize, minimum: usize },
    /// Canonical action identities were not strictly increasing.
    ArmsOutOfOrder {
        index: usize,
        previous: ObjectId,
        current: ObjectId,
    },
    /// A canonical action identity occurred more than once.
    DuplicateArm { index: usize, policy_oid: ObjectId },
    /// The pinned fallback was absent from the action space.
    FallbackNotInActionSpace { fallback_oid: ObjectId },
    /// The profile supplied to the controller had a different identity.
    ProfileIdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// The profile authority rejected the claimed OID for the complete
    /// canonical descriptor.
    ProfileIdentityUnverified { claimed: ObjectId },
    /// The state-space authority rejected the claimed identity.
    StateSpaceIdentityUnverified { claimed: ObjectId },
    /// A verified action space did not match the controller identity.
    StateSpaceIdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// The receipt numeric environment differs from the active environment.
    NumericFingerprintMismatch { component: NoRegretFingerprintField },
    /// The action-space fallback differed from the trial fallback.
    FallbackIdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// The finite sequence window exceeded the profile's decision limit.
    DecisionWindowExceedsLimit { window_capacity: u64, limit: usize },
    /// A fallible bounded allocation failed.
    AllocationFailed {
        kind: NoRegretAllocationKind,
        count: usize,
    },
    /// A choose call arrived before feedback for the previous choice.
    FeedbackPending { pending_sequence: u64 },
    /// A call did not use the exact next stream sequence.
    UnexpectedSequence { expected: u64, actual: u64 },
    /// The finite decision window has no remaining position.
    DecisionWindowExhausted { last_sequence: u64 },
    /// Feedback arrived without an outstanding decision.
    NoPendingDecision,
    /// Feedback named a different sequence.
    FeedbackSequenceMismatch { expected: u64, actual: u64 },
    /// Feedback named a different decision ordinal.
    FeedbackOrdinalMismatch { expected: u64, actual: u64 },
    /// Feedback named a policy other than the selected policy.
    FeedbackPolicyMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// Feedback changed the selected probability.
    FeedbackProbabilityMismatch {
        expected_bits: u64,
        actual_bits: u64,
    },
    /// Feedback changed the selection mode.
    FeedbackModeMismatch {
        expected: NoRegretSelectionMode,
        actual: NoRegretSelectionMode,
    },
    /// Caller loss was outside the normalized interval.
    LossOutOfRange { bits: u64 },
    /// An internal probability was not strictly positive and finite.
    InvalidInternalProbability { index: usize, bits: u64 },
    /// Sampling selected an action with no exact integer mass.
    ZeroMassSelection { index: usize },
    /// Positive analytic exploration cannot be represented for every arm at
    /// the fixed sampling scale.
    ExplorationMassUnsupported {
        exploration_rate_bits: u64,
        arms: usize,
    },
    /// A private, construction-validated vector index was unexpectedly absent.
    InternalIndexOutOfBounds {
        vector: NoRegretVectorKind,
        index: usize,
        length: usize,
    },
    /// A deterministic numeric computation left its valid domain.
    InvalidInternalComputation {
        field: NoRegretNumericField,
        bits: u64,
    },
    /// A typed transition could not increment its predecessor epoch.
    RegimeEpochExhausted { current: u64 },
    /// A typed transition did not advance by exactly one epoch.
    NonConsecutiveRegimeEpoch {
        previous: u64,
        expected: u64,
        actual: u64,
    },
    /// A typed transition reused the predecessor's regime identity.
    RegimeIdentityUnchanged { regime_oid: ObjectId },
    /// A transition was attempted while selected-action feedback was pending.
    RegimeShiftWhileFeedbackPending { pending_sequence: u64 },
    /// A transition did not name the controller's current regime.
    PreviousRegimeMismatch {
        expected: NoRegretRegime,
        actual: NoRegretRegime,
    },
    /// A transition was not effective at the exact next decision sequence.
    RegimeSequenceMismatch { expected: u64, actual: u64 },
    /// The regime authority rejected the evidence-bound successor.
    RegimeShiftUnverified {
        next: NoRegretRegime,
        evidence_oid: ObjectId,
    },
    /// Canonical byte length arithmetic overflowed.
    CanonicalLengthOverflow,
    /// Canonical bytes exceeded a caller-owned decode budget.
    CanonicalByteLimitExceeded {
        field: NoRegretCanonicalField,
        actual: usize,
        maximum: usize,
    },
    /// Strict canonical decoding rejected one component.
    CanonicalMalformed { field: NoRegretCanonicalField },
    /// Complete algorithmic replay diverged from a receipt.
    ReplayMismatch { field: NoRegretReplayField },
    /// A bounded retained log omitted its chronological prefix.
    ReplayHistoryTruncated,
}

impl fmt::Display for NoRegretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReversedWindow { first, last } => {
                write!(formatter, "decision window {first}..={last} is reversed")
            }
            Self::WindowLengthOverflow { first, last } => write!(
                formatter,
                "decision window {first}..={last} has unrepresentable length"
            ),
            Self::NonFiniteNumeric { field, bits } => {
                write!(formatter, "{field:?} is non-finite ({bits:#018x})")
            }
            Self::RateOutOfRange {
                field,
                bits,
                zero_allowed,
            } => write!(
                formatter,
                "{field:?} bits {bits:#018x} are outside {}",
                if *zero_allowed { "[0, 1]" } else { "(0, 1]" }
            ),
            Self::ZeroBound { kind } => write!(formatter, "{kind:?} bound is zero"),
            Self::BoundExceeded {
                kind,
                actual,
                maximum,
            } => write!(
                formatter,
                "{kind:?} count {actual} exceeds maximum {maximum}"
            ),
            Self::BoundUnrepresentable { kind, actual } => {
                write!(formatter, "{kind:?} count {actual} is unrepresentable")
            }
            Self::TooFewArms { actual, minimum } => {
                write!(
                    formatter,
                    "action count {actual} is below minimum {minimum}"
                )
            }
            Self::ArmsOutOfOrder {
                index,
                previous,
                current,
            } => write!(
                formatter,
                "action {index} ({current:?}) does not follow {previous:?}"
            ),
            Self::DuplicateArm { index, policy_oid } => {
                write!(formatter, "action {index} duplicates {policy_oid:?}")
            }
            Self::FallbackNotInActionSpace { fallback_oid } => {
                write!(formatter, "fallback {fallback_oid:?} is not an action")
            }
            Self::ProfileIdentityMismatch { expected, actual } => write!(
                formatter,
                "profile identity mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::ProfileIdentityUnverified { claimed } => {
                write!(formatter, "profile authority rejected {claimed:?}")
            }
            Self::StateSpaceIdentityUnverified { claimed } => {
                write!(formatter, "state-space authority rejected {claimed:?}")
            }
            Self::StateSpaceIdentityMismatch { expected, actual } => write!(
                formatter,
                "state-space identity mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::NumericFingerprintMismatch { component } => {
                write!(
                    formatter,
                    "numeric replay fingerprint {component:?} mismatch"
                )
            }
            Self::FallbackIdentityMismatch { expected, actual } => write!(
                formatter,
                "fallback identity mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::DecisionWindowExceedsLimit {
                window_capacity,
                limit,
            } => write!(
                formatter,
                "decision window capacity {window_capacity} exceeds profile limit {limit}"
            ),
            Self::AllocationFailed { kind, count } => {
                write!(formatter, "could not allocate {count} {kind:?} entries")
            }
            Self::FeedbackPending { pending_sequence } => write!(
                formatter,
                "feedback remains pending for sequence {pending_sequence}"
            ),
            Self::UnexpectedSequence { expected, actual } => {
                write!(formatter, "expected sequence {expected}, got {actual}")
            }
            Self::DecisionWindowExhausted { last_sequence } => write!(
                formatter,
                "decision window ended at sequence {last_sequence}"
            ),
            Self::NoPendingDecision => formatter.write_str("no decision awaits feedback"),
            Self::FeedbackSequenceMismatch { expected, actual } => write!(
                formatter,
                "feedback sequence mismatch: expected {expected}, got {actual}"
            ),
            Self::FeedbackOrdinalMismatch { expected, actual } => write!(
                formatter,
                "feedback ordinal mismatch: expected {expected}, got {actual}"
            ),
            Self::FeedbackPolicyMismatch { expected, actual } => write!(
                formatter,
                "feedback policy mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::FeedbackProbabilityMismatch {
                expected_bits,
                actual_bits,
            } => write!(
                formatter,
                "feedback probability mismatch: expected {expected_bits:#018x}, got {actual_bits:#018x}"
            ),
            Self::FeedbackModeMismatch { expected, actual } => write!(
                formatter,
                "feedback mode mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::LossOutOfRange { bits } => {
                write!(
                    formatter,
                    "normalized loss bits {bits:#018x} are outside [0, 1]"
                )
            }
            Self::InvalidInternalProbability { index, bits } => write!(
                formatter,
                "internal probability {index} has invalid bits {bits:#018x}"
            ),
            Self::ZeroMassSelection { index } => {
                write!(formatter, "sampling selected zero-mass action {index}")
            }
            Self::ExplorationMassUnsupported {
                exploration_rate_bits,
                arms,
            } => write!(
                formatter,
                "exploration rate {exploration_rate_bits:#018x} cannot give all {arms} arms positive sampling mass"
            ),
            Self::InternalIndexOutOfBounds {
                vector,
                index,
                length,
            } => write!(
                formatter,
                "internal {vector:?} index {index} is outside length {length}"
            ),
            Self::InvalidInternalComputation { field, bits } => write!(
                formatter,
                "internal {field:?} computation has invalid bits {bits:#018x}"
            ),
            Self::RegimeEpochExhausted { current } => {
                write!(formatter, "regime epoch {current} cannot advance")
            }
            Self::NonConsecutiveRegimeEpoch {
                previous,
                expected,
                actual,
            } => write!(
                formatter,
                "regime epoch after {previous} must be {expected}, got {actual}"
            ),
            Self::RegimeIdentityUnchanged { regime_oid } => {
                write!(formatter, "regime identity remained {regime_oid:?}")
            }
            Self::RegimeShiftWhileFeedbackPending { pending_sequence } => write!(
                formatter,
                "cannot shift regimes while sequence {pending_sequence} awaits feedback"
            ),
            Self::PreviousRegimeMismatch { expected, actual } => write!(
                formatter,
                "previous regime mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::RegimeSequenceMismatch { expected, actual } => write!(
                formatter,
                "regime effective sequence mismatch: expected {expected}, got {actual}"
            ),
            Self::RegimeShiftUnverified { next, evidence_oid } => write!(
                formatter,
                "regime authority rejected successor {next:?} with evidence {evidence_oid:?}"
            ),
            Self::CanonicalLengthOverflow => {
                formatter.write_str("canonical encoding length overflow")
            }
            Self::CanonicalByteLimitExceeded {
                field,
                actual,
                maximum,
            } => write!(
                formatter,
                "canonical {field:?} bytes {actual} exceed caller limit {maximum}"
            ),
            Self::CanonicalMalformed { field } => {
                write!(formatter, "canonical {field:?} field is malformed")
            }
            Self::ReplayMismatch { field } => {
                write!(formatter, "algorithmic replay diverged at {field:?}")
            }
            Self::ReplayHistoryTruncated => {
                formatter.write_str("bounded replay history omitted its chronological prefix")
            }
        }
    }
}

impl std::error::Error for NoRegretError {}

fn validate_open_closed_unit_rate(
    field: NoRegretNumericField,
    value: f64,
) -> Result<(), NoRegretError> {
    validate_finite(field, value)?;
    if value <= 0.0 || value > 1.0 {
        return Err(NoRegretError::RateOutOfRange {
            field,
            bits: canonical_float_bits(value),
            zero_allowed: false,
        });
    }
    Ok(())
}

fn validate_finite(field: NoRegretNumericField, value: f64) -> Result<(), NoRegretError> {
    if !value.is_finite() {
        return Err(NoRegretError::NonFiniteNumeric {
            field,
            bits: canonical_float_bits(value),
        });
    }
    Ok(())
}

const fn validate_nonzero_bound(
    kind: NoRegretBoundKind,
    actual: usize,
    maximum: usize,
) -> Result<(), NoRegretError> {
    if actual == 0 {
        return Err(NoRegretError::ZeroBound { kind });
    }
    if actual > maximum {
        return Err(NoRegretError::BoundExceeded {
            kind,
            actual,
            maximum,
        });
    }
    Ok(())
}

fn validate_retention_budget(
    arms: usize,
    retained_events: usize,
    maximum_arm_slots: usize,
    maximum_vector_bytes: usize,
) -> Result<(), NoRegretError> {
    let arm_slots = arms
        .checked_mul(retained_events)
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    if arm_slots > maximum_arm_slots {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::RetainedArmSlots,
            actual: arm_slots,
            maximum: maximum_arm_slots,
        });
    }
    let vector_bytes = arm_slots
        .checked_mul(FEEDBACK_RETAINED_BYTES_PER_ARM)
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    if vector_bytes > maximum_vector_bytes {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::RetainedVectorBytes,
            actual: vector_bytes,
            maximum: maximum_vector_bytes,
        });
    }
    Ok(())
}

fn validate_exploration_mass_support(
    profile: NoRegretProfile,
    arm_count: usize,
) -> Result<(), NoRegretError> {
    let arm_count_u32 =
        u32::try_from(arm_count).map_err(|_| NoRegretError::BoundUnrepresentable {
            kind: NoRegretBoundKind::Arms,
            actual: arm_count,
        })?;
    let minimum_rate = f64::from(arm_count_u32) / (SAMPLE_SCALE as f64);
    if profile.exploration_rate() < minimum_rate {
        return Err(NoRegretError::ExplorationMassUnsupported {
            exploration_rate_bits: profile.exploration_rate_bits,
            arms: arm_count,
        });
    }
    Ok(())
}

fn validate_normalized_loss(loss: f64) -> Result<(), NoRegretError> {
    validate_finite(NoRegretNumericField::NormalizedLoss, loss)?;
    if !(0.0..=1.0).contains(&loss) {
        return Err(NoRegretError::LossOutOfRange {
            bits: canonical_float_bits(loss),
        });
    }
    Ok(())
}

fn validate_feedback_key(
    pending: &NoRegretDecisionReceipt,
    actual: NoRegretDecisionSelection,
) -> Result<(), NoRegretError> {
    if actual.sequence != pending.sequence {
        return Err(NoRegretError::FeedbackSequenceMismatch {
            expected: pending.sequence,
            actual: actual.sequence,
        });
    }
    if actual.ordinal != pending.ordinal {
        return Err(NoRegretError::FeedbackOrdinalMismatch {
            expected: pending.ordinal,
            actual: actual.ordinal,
        });
    }
    if actual.selected_policy_oid != pending.selected_policy_oid {
        return Err(NoRegretError::FeedbackPolicyMismatch {
            expected: pending.selected_policy_oid,
            actual: actual.selected_policy_oid,
        });
    }
    let expected_probability_bits = pending.selected_probability_bits;
    if actual.selected_probability_bits != expected_probability_bits {
        return Err(NoRegretError::FeedbackProbabilityMismatch {
            expected_bits: expected_probability_bits,
            actual_bits: actual.selected_probability_bits,
        });
    }
    if actual.mode != pending.mode {
        return Err(NoRegretError::FeedbackModeMismatch {
            expected: pending.mode,
            actual: actual.mode,
        });
    }
    Ok(())
}

fn probabilities_for(
    profile: NoRegretProfile,
    weights: &[f64],
    fallback_index: usize,
    mode: NoRegretSelectionMode,
) -> Result<Vec<f64>, NoRegretError> {
    let arm_count = weights.len();
    let mut probabilities = Vec::new();
    probabilities
        .try_reserve_exact(arm_count)
        .map_err(|_| NoRegretError::AllocationFailed {
            kind: NoRegretAllocationKind::ProbabilityVector,
            count: arm_count,
        })?;
    match mode {
        NoRegretSelectionMode::AdaptiveEvidence => {
            let exploration = profile.exploration_rate();
            let exploitation = 1.0 - exploration;
            let uniform = uniform_prior(arm_count)?;
            for weight in weights {
                probabilities.push((exploitation * *weight) + (exploration * uniform));
            }
            normalize_nonnegative(&mut probabilities)?;
        }
        NoRegretSelectionMode::UnsupportedAssumptionsFallback
        | NoRegretSelectionMode::RegimeResetFallback => {
            probabilities.resize(arm_count, 0.0);
            let probability = probabilities.get_mut(fallback_index).ok_or(
                NoRegretError::InternalIndexOutOfBounds {
                    vector: NoRegretVectorKind::Probability,
                    index: fallback_index,
                    length: arm_count,
                },
            )?;
            *probability = 1.0;
        }
    }
    Ok(probabilities)
}

fn validate_normalized_weight_bits(bits: &[u64]) -> Result<Vec<f64>, NoRegretError> {
    validate_nonzero_bound(NoRegretBoundKind::Arms, bits.len(), MAX_NO_REGRET_ARMS)?;
    let weights = materialize_floats(bits, NoRegretAllocationKind::WeightVector)?;
    let mut sum = 0.0;
    for value in &weights {
        if canonical_float_bits(*value) != value.to_bits() || !value.is_finite() || *value < 0.0 {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Weights,
            });
        }
        sum += *value;
    }
    let arm_count_u32 =
        u32::try_from(bits.len()).map_err(|_| NoRegretError::CanonicalLengthOverflow)?;
    let tolerance = f64::from(arm_count_u32) * 8.0 * f64::EPSILON;
    if !sum.is_finite() || (sum - 1.0).abs() > tolerance {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Weights,
        });
    }
    Ok(weights)
}

#[cfg(test)]
thread_local! {
    static EXPECTED_RNG_REPLAY_STEPS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

fn expected_rng_draw(seed: u64, ordinal: u64) -> Result<u64, NoRegretError> {
    if ordinal >= MAX_NO_REGRET_DECISION_EPOCHS as u64 {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::DecisionEpochs,
            actual: usize::try_from(ordinal).unwrap_or(usize::MAX),
            maximum: MAX_NO_REGRET_DECISION_EPOCHS,
        });
    }
    let mut rng = DetRng::new(seed);
    let mut draw = 0;
    for _ in 0..=ordinal {
        #[cfg(test)]
        EXPECTED_RNG_REPLAY_STEPS.with(|steps| steps.set(steps.get().saturating_add(1)));
        draw = rng.next_u64();
    }
    Ok(draw)
}

#[derive(Clone, Copy)]
enum ReceiptRngVerification {
    FromSeedOrdinal,
    DeferredToChronologicalReplay,
}

fn verify_decision_intrinsic(
    receipt: &NoRegretDecisionReceipt,
    rng_verification: ReceiptRngVerification,
) -> Result<(), NoRegretError> {
    receipt
        .identity
        .numeric_fingerprint
        .validate_current_target_abi()?;
    if public_values_differ(&receipt.identity.profile_oid, &receipt.profile.profile_oid) {
        return Err(NoRegretError::ProfileIdentityMismatch {
            expected: receipt.identity.profile_oid,
            actual: receipt.profile.profile_oid,
        });
    }
    let arm_count = receipt.policy_oids.len();
    if arm_count < 2
        || arm_count > receipt.profile.maximum_arms
        || receipt.probability_bits.len() != arm_count
        || receipt.sampling_masses.len() != arm_count
        || receipt.weight_bits_before.len() != arm_count
    {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Length,
        });
    }
    validate_exploration_mass_support(receipt.profile, arm_count)?;
    if public_values_differ(
        &receipt.pinned_fallback_oid,
        &receipt.identity.pinned_fallback_oid,
    ) {
        return Err(NoRegretError::FallbackIdentityMismatch {
            expected: receipt.identity.pinned_fallback_oid,
            actual: receipt.pinned_fallback_oid,
        });
    }
    let fallback_index = receipt
        .policy_oids
        .binary_search(&receipt.pinned_fallback_oid)
        .map_err(|_| NoRegretError::FallbackNotInActionSpace {
            fallback_oid: receipt.pinned_fallback_oid,
        })?;
    let expected_sequence = receipt
        .identity
        .first_sequence
        .checked_add(receipt.ordinal)
        .ok_or(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Selection,
        })?;
    if public_values_differ(&receipt.sequence, &expected_sequence)
        || receipt.sequence > receipt.identity.last_sequence
        || receipt.ordinal
            >= u64::try_from(receipt.profile.maximum_decision_epochs)
                .map_err(|_| NoRegretError::CanonicalLengthOverflow)?
    {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Selection,
        });
    }
    if !receipt.assumptions.is_supported()
        && public_values_differ(
            &receipt.mode,
            &NoRegretSelectionMode::UnsupportedAssumptionsFallback,
        )
    {
        return Err(NoRegretError::ReplayMismatch {
            field: NoRegretReplayField::Distribution,
        });
    }
    if receipt.assumptions.is_supported()
        && public_value_eq(
            &receipt.mode,
            &NoRegretSelectionMode::UnsupportedAssumptionsFallback,
        )
    {
        return Err(NoRegretError::ReplayMismatch {
            field: NoRegretReplayField::Distribution,
        });
    }
    let weights = validate_normalized_weight_bits(&receipt.weight_bits_before)?;
    let expected_probabilities =
        probabilities_for(receipt.profile, &weights, fallback_index, receipt.mode)?;
    let expected_probability_bits = float_bits_box(
        &expected_probabilities,
        NoRegretAllocationKind::ProbabilityVector,
    )?;
    if public_values_differ(
        expected_probability_bits.as_ref(),
        receipt.probability_bits.as_ref(),
    ) {
        return Err(NoRegretError::ReplayMismatch {
            field: NoRegretReplayField::Distribution,
        });
    }
    let expected_masses = exact_sampling_masses(&receipt.probability_bits)?;
    if public_values_differ(expected_masses.as_slice(), receipt.sampling_masses.as_ref()) {
        return Err(NoRegretError::ReplayMismatch {
            field: NoRegretReplayField::Selection,
        });
    }
    if matches!(rng_verification, ReceiptRngVerification::FromSeedOrdinal) {
        let replayed_rng_draw = expected_rng_draw(receipt.identity.replay_seed, receipt.ordinal)?;
        if public_values_differ(&replayed_rng_draw, &receipt.rng_draw) {
            return Err(NoRegretError::ReplayMismatch {
                field: NoRegretReplayField::RngDraw,
            });
        }
    }
    let expected_index = match receipt.mode {
        NoRegretSelectionMode::AdaptiveEvidence => {
            sample_categorical(&receipt.sampling_masses, receipt.rng_draw)?
        }
        NoRegretSelectionMode::UnsupportedAssumptionsFallback
        | NoRegretSelectionMode::RegimeResetFallback => fallback_index,
    };
    if public_values_differ(&expected_index, &receipt.selected_index) {
        return Err(NoRegretError::ReplayMismatch {
            field: NoRegretReplayField::Selection,
        });
    }
    let expected_oid = receipt.policy_oids.get(expected_index).copied().ok_or(
        NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Selection,
        },
    )?;
    let expected_mass = receipt.sampling_masses.get(expected_index).copied().ok_or(
        NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Selection,
        },
    )?;
    if expected_mass == 0 {
        return Err(NoRegretError::ZeroMassSelection {
            index: expected_index,
        });
    }
    let expected_selected_probability_bits =
        canonical_float_bits(sampling_probability(expected_mass));
    if public_values_differ(&receipt.selected_policy_oid, &expected_oid)
        || public_values_differ(&receipt.selected_sampling_mass, &expected_mass)
        || public_values_differ(
            &receipt.selected_probability_bits,
            &expected_selected_probability_bits,
        )
    {
        return Err(NoRegretError::ReplayMismatch {
            field: NoRegretReplayField::Selection,
        });
    }
    Ok(())
}

fn compute_feedback_update(
    decision: &NoRegretDecisionReceipt,
    normalized_loss: f64,
) -> Result<(u64, Vec<f64>), NoRegretError> {
    validate_normalized_loss(normalized_loss)?;
    let selected_probability = f64::from_bits(decision.selected_probability_bits);
    if !selected_probability.is_finite() || selected_probability <= 0.0 {
        return Err(NoRegretError::InvalidInternalProbability {
            index: decision.selected_index,
            bits: decision.selected_probability_bits,
        });
    }
    let importance_weighted_loss = normalized_loss / selected_probability;
    if !importance_weighted_loss.is_finite() || importance_weighted_loss < 0.0 {
        return Err(NoRegretError::InvalidInternalComputation {
            field: NoRegretNumericField::ImportanceWeightedLoss,
            bits: canonical_float_bits(importance_weighted_loss),
        });
    }
    let mut next_weights = validate_normalized_weight_bits(&decision.weight_bits_before)?;
    if public_value_eq(&decision.mode, &NoRegretSelectionMode::AdaptiveEvidence) {
        let exponent = (-decision.profile.learning_rate() * importance_weighted_loss)
            .clamp(-MAX_EXPONENT_MAGNITUDE, 0.0);
        let multiplier = exponent.exp();
        if !multiplier.is_finite() || multiplier < 0.0 {
            return Err(NoRegretError::InvalidInternalComputation {
                field: NoRegretNumericField::WeightMultiplier,
                bits: canonical_float_bits(multiplier),
            });
        }
        let length = next_weights.len();
        let weight = next_weights.get_mut(decision.selected_index).ok_or(
            NoRegretError::InternalIndexOutOfBounds {
                vector: NoRegretVectorKind::Weight,
                index: decision.selected_index,
                length,
            },
        )?;
        *weight *= multiplier;
        normalize_nonnegative(&mut next_weights)?;
    }
    Ok((canonical_float_bits(importance_weighted_loss), next_weights))
}

fn verify_feedback_intrinsic(
    receipt: &NoRegretFeedbackReceipt,
    rng_verification: ReceiptRngVerification,
) -> Result<(), NoRegretError> {
    verify_decision_intrinsic(&receipt.decision, rng_verification)?;
    let normalized_loss = f64::from_bits(receipt.normalized_loss_bits);
    if public_values_differ(
        &canonical_float_bits(normalized_loss),
        &receipt.normalized_loss_bits,
    ) {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Feedback,
        });
    }
    let (expected_importance_bits, expected_weights) =
        compute_feedback_update(&receipt.decision, normalized_loss)?;
    let expected_weight_bits =
        float_bits_box(&expected_weights, NoRegretAllocationKind::WeightVector)?;
    if public_values_differ(
        &expected_importance_bits,
        &receipt.importance_weighted_loss_bits,
    ) || public_values_differ(
        expected_weight_bits.as_ref(),
        receipt.weight_bits_after.as_ref(),
    ) {
        return Err(NoRegretError::ReplayMismatch {
            field: NoRegretReplayField::Feedback,
        });
    }
    Ok(())
}

fn verify_reset_intrinsic(receipt: &NoRegretRegimeResetReceipt) -> Result<(), NoRegretError> {
    receipt
        .identity
        .numeric_fingerprint
        .validate_current_target_abi()?;
    if public_values_differ(&receipt.identity.profile_oid, &receipt.profile.profile_oid) {
        return Err(NoRegretError::ProfileIdentityMismatch {
            expected: receipt.identity.profile_oid,
            actual: receipt.profile.profile_oid,
        });
    }
    if public_values_differ(
        &receipt.pinned_fallback_oid,
        &receipt.identity.pinned_fallback_oid,
    ) {
        return Err(NoRegretError::FallbackIdentityMismatch {
            expected: receipt.identity.pinned_fallback_oid,
            actual: receipt.pinned_fallback_oid,
        });
    }
    if receipt.policy_oids.len() != receipt.weight_bits_before.len()
        || receipt.policy_oids.len() != receipt.prior_weight_bits.len()
        || receipt.policy_oids.len() > receipt.profile.maximum_arms
    {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::RegimeReset,
        });
    }
    let _ = validate_normalized_weight_bits(&receipt.weight_bits_before)?;
    let _ = validate_normalized_weight_bits(&receipt.prior_weight_bits)?;
    let prior = uniform_prior(receipt.policy_oids.len())?;
    let expected_prior = vec![canonical_float_bits(prior); receipt.policy_oids.len()];
    if public_values_differ(
        expected_prior.as_slice(),
        receipt.prior_weight_bits.as_ref(),
    ) || receipt.shift.effective_sequence < receipt.identity.first_sequence
        || receipt.shift.effective_sequence > receipt.identity.last_sequence
    {
        return Err(NoRegretError::ReplayMismatch {
            field: NoRegretReplayField::RegimeReset,
        });
    }
    Ok(())
}

fn uniform_prior(arm_count: usize) -> Result<f64, NoRegretError> {
    let arm_count_u32 =
        u32::try_from(arm_count).map_err(|_| NoRegretError::BoundUnrepresentable {
            kind: NoRegretBoundKind::Arms,
            actual: arm_count,
        })?;
    Ok(1.0 / f64::from(arm_count_u32))
}

fn normalize_nonnegative(values: &mut [f64]) -> Result<(), NoRegretError> {
    let mut sum = 0.0;
    for (index, value) in values.iter().copied().enumerate() {
        if !value.is_finite() || value < 0.0 {
            return Err(NoRegretError::InvalidInternalComputation {
                field: NoRegretNumericField::Weight,
                bits: canonical_float_bits(value),
            });
        }
        sum += value;
        if !sum.is_finite() {
            return Err(NoRegretError::InvalidInternalComputation {
                field: NoRegretNumericField::Weight,
                bits: canonical_float_bits(sum),
            });
        }
        if index >= MAX_NO_REGRET_ARMS {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::Arms,
                actual: index.saturating_add(1),
                maximum: MAX_NO_REGRET_ARMS,
            });
        }
    }
    if sum <= 0.0 {
        return Err(NoRegretError::InvalidInternalComputation {
            field: NoRegretNumericField::Weight,
            bits: canonical_float_bits(sum),
        });
    }
    for value in values {
        *value /= sum;
        if *value == 0.0 {
            *value = 0.0;
        }
    }
    Ok(())
}

fn sample_categorical(sampling_masses: &[u64], rng_draw: u64) -> Result<usize, NoRegretError> {
    let target = rng_draw >> 11;
    let mut cumulative = 0u64;
    for (index, mass) in sampling_masses.iter().copied().enumerate() {
        cumulative = cumulative
            .checked_add(mass)
            .ok_or(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::SamplingMasses,
            })?;
        if target < cumulative {
            if mass == 0 {
                return Err(NoRegretError::ZeroMassSelection { index });
            }
            return Ok(index);
        }
    }
    Err(NoRegretError::CanonicalMalformed {
        field: NoRegretCanonicalField::SamplingMasses,
    })
}

/// Deterministically apportions exactly `2^53` integer units.
///
/// The largest-remainder rule is stable by canonical action index. Floating
/// multiplication is exact because the scale is a power of two. A tiny
/// normalization overshoot is handled symmetrically by removing units from
/// the smallest remainders. Arms whose analytic probability is zero always
/// retain zero mass.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn exact_sampling_masses(probability_bits: &[u64]) -> Result<Vec<u64>, NoRegretError> {
    validate_nonzero_bound(
        NoRegretBoundKind::Arms,
        probability_bits.len(),
        MAX_NO_REGRET_ARMS,
    )?;
    let mut masses = Vec::new();
    masses
        .try_reserve_exact(probability_bits.len())
        .map_err(|_| NoRegretError::AllocationFailed {
            kind: NoRegretAllocationKind::SamplingMassVector,
            count: probability_bits.len(),
        })?;
    let mut remainders = Vec::new();
    remainders
        .try_reserve_exact(probability_bits.len())
        .map_err(|_| NoRegretError::AllocationFailed {
            kind: NoRegretAllocationKind::SamplingMassVector,
            count: probability_bits.len(),
        })?;
    let mut total = 0u64;
    for (index, bits) in probability_bits.iter().copied().enumerate() {
        let probability = f64::from_bits(bits);
        if !probability.is_finite() || !(0.0..=1.0).contains(&probability) {
            return Err(NoRegretError::InvalidInternalProbability { index, bits });
        }
        let scaled = probability * (SAMPLE_SCALE as f64);
        let minimum_mass = u64::from(probability > 0.0);
        let base = (scaled.floor() as u64).max(minimum_mass);
        total = total
            .checked_add(base)
            .ok_or(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::SamplingMasses,
            })?;
        masses.push(base);
        remainders.push(scaled - (base as f64));
    }

    if total <= SAMPLE_SCALE {
        let missing = SAMPLE_SCALE - total;
        let mut order: Vec<usize> = (0..probability_bits.len()).collect();
        order.retain(|index| f64::from_bits(probability_bits[*index]) > 0.0);
        order.sort_by(|left, right| {
            remainders[*right]
                .total_cmp(&remainders[*left])
                .then_with(|| left.cmp(right))
        });
        if order.is_empty() {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Probabilities,
            });
        }
        let eligible_count =
            u64::try_from(order.len()).map_err(|_| NoRegretError::CanonicalLengthOverflow)?;
        let complete_passes = missing / eligible_count;
        let partial_pass = usize::try_from(missing % eligible_count)
            .map_err(|_| NoRegretError::CanonicalLengthOverflow)?;
        for (ordinal, index) in order.into_iter().enumerate() {
            let increment = complete_passes + u64::from(ordinal < partial_pass);
            masses[index] =
                masses[index]
                    .checked_add(increment)
                    .ok_or(NoRegretError::CanonicalMalformed {
                        field: NoRegretCanonicalField::SamplingMasses,
                    })?;
        }
    } else {
        let mut excess = total - SAMPLE_SCALE;
        let mut order: Vec<usize> = (0..probability_bits.len()).collect();
        order.retain(|index| {
            let minimum_mass = u64::from(f64::from_bits(probability_bits[*index]) > 0.0);
            masses[*index] > minimum_mass
        });
        order.sort_by(|left, right| {
            remainders[*left]
                .total_cmp(&remainders[*right])
                .then_with(|| right.cmp(left))
        });
        while excess > 0 {
            order.retain(|index| {
                let minimum_mass = u64::from(f64::from_bits(probability_bits[*index]) > 0.0);
                masses[*index] > minimum_mass
            });
            if order.is_empty() {
                return Err(NoRegretError::CanonicalMalformed {
                    field: NoRegretCanonicalField::SamplingMasses,
                });
            }
            let active_count =
                u64::try_from(order.len()).map_err(|_| NoRegretError::CanonicalLengthOverflow)?;
            if excess < active_count {
                let partial =
                    usize::try_from(excess).map_err(|_| NoRegretError::CanonicalLengthOverflow)?;
                for index in order.iter().copied().take(partial) {
                    masses[index] -= 1;
                }
                excess = 0;
                continue;
            }
            let minimum_removable = order
                .iter()
                .map(|index| {
                    let floor = u64::from(f64::from_bits(probability_bits[*index]) > 0.0);
                    masses[*index] - floor
                })
                .min()
                .ok_or(NoRegretError::CanonicalMalformed {
                    field: NoRegretCanonicalField::SamplingMasses,
                })?;
            let complete_passes = (excess / active_count).min(minimum_removable);
            for index in &order {
                masses[*index] -= complete_passes;
            }
            excess -= complete_passes * active_count;
        }
    }
    let final_total = masses.iter().try_fold(0_u64, |total, mass| {
        total
            .checked_add(*mass)
            .ok_or(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::SamplingMasses,
            })
    })?;
    if final_total != SAMPLE_SCALE
        || probability_bits
            .iter()
            .zip(&masses)
            .any(|(bits, mass)| f64::from_bits(*bits) > 0.0 && *mass == 0)
    {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::SamplingMasses,
        });
    }
    Ok(masses)
}

fn sampling_probability(mass: u64) -> f64 {
    (mass as f64) / (SAMPLE_SCALE as f64)
}

fn canonical_float_bits(value: f64) -> u64 {
    if value == 0.0 {
        0.0f64.to_bits()
    } else if value.is_nan() {
        f64::NAN.to_bits()
    } else {
        value.to_bits()
    }
}

fn copy_object_ids(values: &[ObjectId]) -> Result<Box<[ObjectId]>, NoRegretError> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(values.len())
        .map_err(|_| NoRegretError::AllocationFailed {
            kind: NoRegretAllocationKind::ActionVector,
            count: values.len(),
        })?;
    copy.extend_from_slice(values);
    Ok(copy.into_boxed_slice())
}

struct EncodedReplayEvent {
    tag: u8,
    payload: Vec<u8>,
}

fn encode_replay_log<'a, I>(
    identity: NoRegretIdentity,
    profile: NoRegretProfile,
    action_space: &NoRegretActionSpace,
    assumptions: NoRegretAssumptions,
    events: I,
) -> Result<Vec<u8>, NoRegretError>
where
    I: IntoIterator<Item = &'a NoRegretReplayEvent>,
    I::IntoIter: ExactSizeIterator,
{
    let iterator = events.into_iter();
    let event_count = iterator.len();
    if event_count > profile.retained_receipts {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::RetainedReceipts,
            actual: event_count,
            maximum: profile.retained_receipts,
        });
    }
    let action_space_fallback = action_space
        .policy_oids
        .get(action_space.fallback_index)
        .copied()
        .ok_or(NoRegretError::InternalIndexOutOfBounds {
            vector: NoRegretVectorKind::Action,
            index: action_space.fallback_index,
            length: action_space.policy_oids.len(),
        })?;
    if identity.profile_oid != profile.profile_oid
        || identity.state_space_oid != action_space.state_space_oid
        || identity.pinned_fallback_oid != action_space_fallback
    {
        return Err(NoRegretError::ReplayMismatch {
            field: NoRegretReplayField::Controller,
        });
    }

    let mut encoded_events = Vec::new();
    encoded_events
        .try_reserve_exact(event_count)
        .map_err(|_| NoRegretError::AllocationFailed {
            kind: NoRegretAllocationKind::ReceiptLog,
            count: event_count,
        })?;
    let mut framed_bytes = 0usize;
    for event in iterator {
        let (event_identity, event_profile, event_assumptions) = event.root();
        if event_identity != identity
            || event_profile != profile
            || event_assumptions != assumptions
        {
            return Err(NoRegretError::ReplayMismatch {
                field: NoRegretReplayField::Controller,
            });
        }
        let payload = event.try_canonical_payload()?;
        let _ = usize_to_u32(payload.len())?;
        framed_bytes = framed_bytes
            .checked_add(REPLAY_LOG_FRAME_BYTES)
            .and_then(|value| value.checked_add(payload.len()))
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        encoded_events.push(EncodedReplayEvent {
            tag: event.canonical_tag(),
            payload,
        });
    }

    let action_bytes = action_space
        .policy_oids
        .len()
        .checked_mul(OBJECT_ID_BYTES)
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    let total = REPLAY_LOG_FIXED_BYTES
        .checked_add(action_bytes)
        .and_then(|value| value.checked_add(framed_bytes))
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total)
        .map_err(|_| NoRegretError::AllocationFailed {
            kind: NoRegretAllocationKind::CanonicalBytes,
            count: total,
        })?;
    bytes.extend_from_slice(&REPLAY_LOG_ENCODING_MAGIC);
    push_u16(&mut bytes, REPLAY_LOG_ENCODING_VERSION);
    push_u16(&mut bytes, REPLAY_LOG_RESERVED);
    push_identity(&mut bytes, identity);
    push_profile(&mut bytes, profile)?;
    bytes.push(assumptions.canonical_flags());
    push_u32(&mut bytes, usize_to_u32(action_space.policy_oids.len())?);
    for policy_oid in &action_space.policy_oids {
        push_object_id(&mut bytes, *policy_oid);
    }
    push_u32(&mut bytes, usize_to_u32(event_count)?);
    push_u64(
        &mut bytes,
        u64::try_from(framed_bytes).map_err(|_| NoRegretError::CanonicalLengthOverflow)?,
    );
    for event in encoded_events {
        bytes.push(event.tag);
        bytes.extend_from_slice(&REPLAY_LOG_FRAME_RESERVED);
        push_u32(&mut bytes, usize_to_u32(event.payload.len())?);
        bytes.extend_from_slice(&event.payload);
    }
    debug_assert_eq!(bytes.len(), total);
    Ok(bytes)
}

#[derive(Clone, Copy)]
struct ReplayEventPreflight {
    arm_count: usize,
    retained_vector_bytes: usize,
}

fn preflight_decision_receipt(bytes: &[u8], maximum_arms: usize) -> Result<usize, NoRegretError> {
    let mut decoder = NoRegretDecoder::new(bytes);
    decoder.expect_magic(DECISION_ENCODING_MAGIC)?;
    decoder.expect_version()?;
    let _ = decoder.read_identity()?;
    let profile = decoder.read_profile()?;
    let _ = decoder.read_object_id()?;
    let _ = decoder.read_u64()?;
    let _ = decode_assumptions(decoder.read_u8()?)?;
    let _ = decoder.read_u64()?;
    let _ = decoder.read_u64()?;
    let _ = decode_selection_mode(decoder.read_u8()?)?;
    let _ = decoder.read_u64()?;
    let _ = decoder.read_usize_u32()?;
    let _ = decoder.read_object_id()?;
    let _ = decoder.read_u64()?;
    let _ = decoder.read_u64()?;
    let _ = decoder.read_object_id()?;
    let arm_count = decoder.read_usize_u32()?;
    validate_nonzero_bound(NoRegretBoundKind::Arms, arm_count, maximum_arms)?;
    if arm_count < 2 {
        return Err(NoRegretError::TooFewArms {
            actual: arm_count,
            minimum: 2,
        });
    }
    if profile.maximum_arms > maximum_arms || arm_count > profile.maximum_arms {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::Arms,
            actual: profile.maximum_arms.max(arm_count),
            maximum: maximum_arms.min(profile.maximum_arms),
        });
    }
    validate_exploration_mass_support(profile, arm_count)?;
    let vector_bytes = arm_count
        .checked_mul(OBJECT_ID_BYTES + (FLOAT_BYTES * 3))
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    let _ = decoder.read_exact(vector_bytes)?;
    decoder.finish()?;
    Ok(arm_count)
}

fn preflight_feedback_receipt(
    bytes: &[u8],
    maximum_arms: usize,
) -> Result<ReplayEventPreflight, NoRegretError> {
    let mut decoder = NoRegretDecoder::new(bytes);
    decoder.expect_magic(FEEDBACK_ENCODING_MAGIC)?;
    decoder.expect_version()?;
    let decision_length = decoder.read_usize_u32()?;
    let decision_bytes = decoder.read_exact(decision_length)?;
    let arm_count = preflight_decision_receipt(decision_bytes, maximum_arms)?;
    let _ = decoder.read_u64()?;
    let _ = decoder.read_u64()?;
    let after_count = decoder.read_usize_u32()?;
    if after_count != arm_count {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Weights,
        });
    }
    let after_bytes = after_count
        .checked_mul(FLOAT_BYTES)
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    let _ = decoder.read_exact(after_bytes)?;
    decoder.finish()?;
    Ok(ReplayEventPreflight {
        arm_count,
        retained_vector_bytes: arm_count
            .checked_mul(FEEDBACK_RETAINED_BYTES_PER_ARM)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?,
    })
}

fn preflight_regime_reset_receipt(
    bytes: &[u8],
    maximum_arms: usize,
) -> Result<ReplayEventPreflight, NoRegretError> {
    let mut decoder = NoRegretDecoder::new(bytes);
    decoder.expect_magic(REGIME_ENCODING_MAGIC)?;
    decoder.expect_version()?;
    let _ = decoder.read_identity()?;
    let profile = decoder.read_profile()?;
    let _ = decode_assumptions(decoder.read_u8()?)?;
    let previous = NoRegretRegime::new(decoder.read_object_id()?, decoder.read_u64()?);
    let next = NoRegretRegime::new(decoder.read_object_id()?, decoder.read_u64()?);
    let effective_sequence = decoder.read_u64()?;
    let evidence_oid = decoder.read_object_id()?;
    let _ = NoRegretRegimeShift::try_new(previous, next, effective_sequence, evidence_oid)?;
    let _ = decoder.read_object_id()?;
    let arm_count = decoder.read_usize_u32()?;
    validate_nonzero_bound(NoRegretBoundKind::Arms, arm_count, maximum_arms)?;
    if arm_count < 2 {
        return Err(NoRegretError::TooFewArms {
            actual: arm_count,
            minimum: 2,
        });
    }
    if profile.maximum_arms > maximum_arms || arm_count > profile.maximum_arms {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::Arms,
            actual: profile.maximum_arms.max(arm_count),
            maximum: maximum_arms.min(profile.maximum_arms),
        });
    }
    let vector_bytes = arm_count
        .checked_mul(OBJECT_ID_BYTES + (FLOAT_BYTES * 2))
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    let _ = decoder.read_exact(vector_bytes)?;
    decoder.finish()?;
    Ok(ReplayEventPreflight {
        arm_count,
        retained_vector_bytes: arm_count
            .checked_mul(REGIME_RESET_RETAINED_BYTES_PER_ARM)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?,
    })
}

#[derive(Clone, Copy)]
struct ReplayLogPreflight {
    identity: NoRegretIdentity,
    profile: NoRegretProfile,
    assumptions: NoRegretAssumptions,
    event_count: usize,
}

fn preflight_replay_log(
    bytes: &[u8],
    limits: NoRegretReplayLogDecodeLimits,
) -> Result<ReplayLogPreflight, NoRegretError> {
    if bytes.len() > limits.max_encoded_bytes {
        return Err(NoRegretError::CanonicalByteLimitExceeded {
            field: NoRegretCanonicalField::ReplayLog,
            actual: bytes.len(),
            maximum: limits.max_encoded_bytes,
        });
    }
    if bytes.len() < REPLAY_LOG_FIXED_BYTES {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Length,
        });
    }

    let mut decoder = NoRegretDecoder::new(bytes);
    decoder.expect_magic(REPLAY_LOG_ENCODING_MAGIC)?;
    if decoder.read_u16()? != REPLAY_LOG_ENCODING_VERSION {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Version,
        });
    }
    if decoder.read_u16()? != REPLAY_LOG_RESERVED {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::ReplayLog,
        });
    }
    let identity = decoder.read_identity()?;
    let profile = decoder.read_profile()?;
    let assumptions = decode_assumptions(decoder.read_u8()?)?;
    let arm_count = decoder.read_usize_u32()?;
    if arm_count > limits.max_arms || profile.maximum_arms > limits.max_arms {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::Arms,
            actual: arm_count.max(profile.maximum_arms),
            maximum: limits.max_arms,
        });
    }
    if arm_count > profile.maximum_arms {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::Arms,
            actual: arm_count,
            maximum: profile.maximum_arms,
        });
    }
    let action_bytes = arm_count
        .checked_mul(OBJECT_ID_BYTES)
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    let _ = decoder.read_exact(action_bytes)?;
    let event_count = decoder.read_usize_u32()?;
    if event_count > limits.max_events || profile.retained_receipts > limits.max_events {
        return Err(NoRegretError::BoundExceeded {
            kind: NoRegretBoundKind::RetainedReceipts,
            actual: event_count.max(profile.retained_receipts),
            maximum: limits.max_events,
        });
    }
    let framed_bytes =
        usize::try_from(decoder.read_u64()?).map_err(|_| NoRegretError::CanonicalLengthOverflow)?;
    let actual_framed_bytes = bytes
        .len()
        .checked_sub(decoder.cursor)
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    if framed_bytes != actual_framed_bytes {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Length,
        });
    }

    let maximum_arm_slots = limits
        .max_retained_arm_slots
        .min(MAX_NO_REGRET_RETAINED_ARM_SLOTS);
    let maximum_vector_bytes = limits
        .max_retained_vector_bytes
        .min(MAX_NO_REGRET_RETAINED_VECTOR_BYTES);
    let mut retained_arm_slots = 0usize;
    let mut retained_vector_bytes = 0usize;
    for _ in 0..event_count {
        let tag = decoder.read_u8()?;
        if decoder.read_exact(REPLAY_LOG_FRAME_RESERVED.len())? != REPLAY_LOG_FRAME_RESERVED {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::ReplayLog,
            });
        }
        let payload_len = decoder.read_usize_u32()?;
        if payload_len > limits.max_event_bytes {
            return Err(NoRegretError::CanonicalByteLimitExceeded {
                field: NoRegretCanonicalField::ReplayEvent,
                actual: payload_len,
                maximum: limits.max_event_bytes,
            });
        }
        let payload = decoder.read_exact(payload_len)?;
        let event = match tag {
            1 => preflight_feedback_receipt(payload, limits.max_arms)?,
            2 => preflight_regime_reset_receipt(payload, limits.max_arms)?,
            _ => {
                return Err(NoRegretError::CanonicalMalformed {
                    field: NoRegretCanonicalField::ReplayEvent,
                });
            }
        };
        retained_arm_slots = retained_arm_slots
            .checked_add(event.arm_count)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        if retained_arm_slots > maximum_arm_slots {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::RetainedArmSlots,
                actual: retained_arm_slots,
                maximum: maximum_arm_slots,
            });
        }
        retained_vector_bytes = retained_vector_bytes
            .checked_add(event.retained_vector_bytes)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        if retained_vector_bytes > maximum_vector_bytes {
            return Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::RetainedVectorBytes,
                actual: retained_vector_bytes,
                maximum: maximum_vector_bytes,
            });
        }
    }
    decoder.finish()?;
    Ok(ReplayLogPreflight {
        identity,
        profile,
        assumptions,
        event_count,
    })
}

struct NoRegretDecoder<'a> {
    bytes: &'a [u8],
    cursor: usize,
}

impl<'a> NoRegretDecoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, cursor: 0 }
    }

    fn expect_magic(&mut self, expected: [u8; 8]) -> Result<(), NoRegretError> {
        if self.read_array::<8>()? != expected {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Magic,
            });
        }
        Ok(())
    }

    fn expect_version(&mut self) -> Result<(), NoRegretError> {
        if self.read_u16()? != CANONICAL_ENCODING_VERSION {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Version,
            });
        }
        Ok(())
    }

    fn read_identity(&mut self) -> Result<NoRegretIdentity, NoRegretError> {
        let monitor_oid = self.read_object_id()?;
        let profile_oid = self.read_object_id()?;
        let state_space_oid = self.read_object_id()?;
        let features_oid = self.read_object_id()?;
        let policy_epoch_oid = self.read_object_id()?;
        let regime_oid = self.read_object_id()?;
        let window_oid = self.read_object_id()?;
        let regime_epoch = self.read_u64()?;
        let first_sequence = self.read_u64()?;
        let last_sequence = self.read_u64()?;
        let pinned_fallback_oid = self.read_object_id()?;
        let replay_seed = self.read_u64()?;
        let numeric_fingerprint = self.read_numeric_fingerprint()?;
        NoRegretIdentity::try_new(
            monitor_oid,
            profile_oid,
            state_space_oid,
            features_oid,
            policy_epoch_oid,
            regime_oid,
            window_oid,
            regime_epoch,
            first_sequence,
            last_sequence,
            pinned_fallback_oid,
            replay_seed,
            numeric_fingerprint,
        )
    }

    fn read_numeric_fingerprint(&mut self) -> Result<NoRegretNumericFingerprint, NoRegretError> {
        let schema_version = self.read_u16()?;
        let float_radix = self.read_u32()?;
        let float_mantissa_digits = self.read_u32()?;
        let float_min_exp = self.read_i16()?;
        let float_max_exp = self.read_i16()?;
        let pointer_width = self.read_u16()?;
        let endian = match self.read_u8()? {
            1 => NoRegretEndian::Little,
            2 => NoRegretEndian::Big,
            _ => {
                return Err(NoRegretError::CanonicalMalformed {
                    field: NoRegretCanonicalField::NumericFingerprint,
                });
            }
        };
        let fingerprint = NoRegretNumericFingerprint {
            schema_version,
            float_radix,
            float_mantissa_digits,
            float_min_exp,
            float_max_exp,
            pointer_width,
            endian,
            toolchain_oid: self.read_object_id()?,
            foundation_oid: self.read_object_id()?,
            math_abi_oid: self.read_object_id()?,
        };
        fingerprint.validate_current_target_abi()?;
        Ok(fingerprint)
    }

    fn read_profile(&mut self) -> Result<NoRegretProfile, NoRegretError> {
        let profile_oid = self.read_object_id()?;
        let learning_rate_bits = self.read_u64()?;
        let exploration_rate_bits = self.read_u64()?;
        let maximum_arms = self.read_usize_u32()?;
        let maximum_decision_epochs = self.read_usize_u64()?;
        let maximum_regime_epochs = self.read_usize_u64()?;
        let retained_receipts = self.read_usize_u64()?;
        let profile = NoRegretProfile::try_new(
            profile_oid,
            f64::from_bits(learning_rate_bits),
            f64::from_bits(exploration_rate_bits),
            maximum_arms,
            maximum_decision_epochs,
            maximum_regime_epochs,
            retained_receipts,
        )?;
        if profile.learning_rate_bits != learning_rate_bits
            || profile.exploration_rate_bits != exploration_rate_bits
        {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Probabilities,
            });
        }
        Ok(profile)
    }

    fn read_object_ids(&mut self, count: usize) -> Result<Box<[ObjectId]>, NoRegretError> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::ActionVector,
                count,
            })?;
        for _ in 0..count {
            values.push(self.read_object_id()?);
        }
        Ok(values.into_boxed_slice())
    }

    fn read_u64s(&mut self, count: usize) -> Result<Box<[u64]>, NoRegretError> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| NoRegretError::AllocationFailed {
                kind: NoRegretAllocationKind::CanonicalBytes,
                count,
            })?;
        for _ in 0..count {
            values.push(self.read_u64()?);
        }
        Ok(values.into_boxed_slice())
    }

    fn read_object_id(&mut self) -> Result<ObjectId, NoRegretError> {
        Ok(ObjectId(self.read_array::<OBJECT_ID_BYTES>()?))
    }

    fn read_usize_u32(&mut self) -> Result<usize, NoRegretError> {
        usize::try_from(self.read_u32()?).map_err(|_| NoRegretError::CanonicalLengthOverflow)
    }

    fn read_usize_u64(&mut self) -> Result<usize, NoRegretError> {
        usize::try_from(self.read_u64()?).map_err(|_| NoRegretError::CanonicalLengthOverflow)
    }

    fn read_u8(&mut self) -> Result<u8, NoRegretError> {
        Ok(self.read_array::<1>()?[0])
    }

    fn read_u16(&mut self) -> Result<u16, NoRegretError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_i16(&mut self) -> Result<i16, NoRegretError> {
        Ok(i16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32, NoRegretError> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64, NoRegretError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], NoRegretError> {
        let slice = self.read_exact(N)?;
        let mut array = [0u8; N];
        array.copy_from_slice(slice);
        Ok(array)
    }

    fn read_exact(&mut self, length: usize) -> Result<&'a [u8], NoRegretError> {
        let end = self
            .cursor
            .checked_add(length)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let slice = self
            .bytes
            .get(self.cursor..end)
            .ok_or(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Length,
            })?;
        self.cursor = end;
        Ok(slice)
    }

    fn finish(self) -> Result<(), NoRegretError> {
        if self.cursor != self.bytes.len() {
            return Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::Length,
            });
        }
        Ok(())
    }
}

fn decode_assumptions(flags: u8) -> Result<NoRegretAssumptions, NoRegretError> {
    if flags & !0x0f != 0 {
        return Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::Assumptions,
        });
    }
    Ok(NoRegretAssumptions::new(
        flags & 1 != 0,
        flags & 2 != 0,
        flags & 4 != 0,
        flags & 8 != 0,
    ))
}

fn decode_selection_mode(tag: u8) -> Result<NoRegretSelectionMode, NoRegretError> {
    match tag {
        1 => Ok(NoRegretSelectionMode::AdaptiveEvidence),
        2 => Ok(NoRegretSelectionMode::UnsupportedAssumptionsFallback),
        3 => Ok(NoRegretSelectionMode::RegimeResetFallback),
        _ => Err(NoRegretError::CanonicalMalformed {
            field: NoRegretCanonicalField::SelectionMode,
        }),
    }
}

fn canonical_action_space_bytes(
    policy_oids: &[ObjectId],
    pinned_fallback_oid: ObjectId,
) -> Result<Vec<u8>, NoRegretError> {
    let vector_bytes = policy_oids
        .len()
        .checked_mul(OBJECT_ID_BYTES)
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    let total = 8usize
        .checked_add(2)
        .and_then(|value| value.checked_add(OBJECT_ID_BYTES))
        .and_then(|value| value.checked_add(4))
        .and_then(|value| value.checked_add(vector_bytes))
        .ok_or(NoRegretError::CanonicalLengthOverflow)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(total)
        .map_err(|_| NoRegretError::AllocationFailed {
            kind: NoRegretAllocationKind::CanonicalBytes,
            count: total,
        })?;
    bytes.extend_from_slice(&ACTION_SPACE_ENCODING_MAGIC);
    push_u16(&mut bytes, ACTION_SPACE_ENCODING_VERSION);
    push_object_id(&mut bytes, pinned_fallback_oid);
    push_u32(&mut bytes, usize_to_u32(policy_oids.len())?);
    for policy_oid in policy_oids {
        push_object_id(&mut bytes, *policy_oid);
    }
    Ok(bytes)
}

fn float_bits_vector(
    values: &[f64],
    kind: NoRegretAllocationKind,
) -> Result<Vec<u64>, NoRegretError> {
    let mut bits = Vec::new();
    bits.try_reserve_exact(values.len())
        .map_err(|_| NoRegretError::AllocationFailed {
            kind,
            count: values.len(),
        })?;
    for value in values {
        bits.push(canonical_float_bits(*value));
    }
    Ok(bits)
}

fn float_bits_box(
    values: &[f64],
    kind: NoRegretAllocationKind,
) -> Result<Box<[u64]>, NoRegretError> {
    Ok(float_bits_vector(values, kind)?.into_boxed_slice())
}

fn materialize_floats(
    bits: &[u64],
    kind: NoRegretAllocationKind,
) -> Result<Vec<f64>, NoRegretError> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(bits.len())
        .map_err(|_| NoRegretError::AllocationFailed {
            kind,
            count: bits.len(),
        })?;
    for value_bits in bits {
        values.push(f64::from_bits(*value_bits));
    }
    Ok(values)
}

fn usize_to_u32(value: usize) -> Result<u32, NoRegretError> {
    u32::try_from(value).map_err(|_| NoRegretError::CanonicalLengthOverflow)
}

fn push_identity(bytes: &mut Vec<u8>, identity: NoRegretIdentity) {
    push_object_id(bytes, identity.monitor_oid);
    push_object_id(bytes, identity.profile_oid);
    push_object_id(bytes, identity.state_space_oid);
    push_object_id(bytes, identity.features_oid);
    push_object_id(bytes, identity.policy_epoch_oid);
    push_object_id(bytes, identity.regime_oid);
    push_object_id(bytes, identity.window_oid);
    push_u64(bytes, identity.regime_epoch);
    push_u64(bytes, identity.first_sequence);
    push_u64(bytes, identity.last_sequence);
    push_object_id(bytes, identity.pinned_fallback_oid);
    push_u64(bytes, identity.replay_seed);
    push_numeric_fingerprint(bytes, identity.numeric_fingerprint);
}

fn push_numeric_fingerprint(bytes: &mut Vec<u8>, fingerprint: NoRegretNumericFingerprint) {
    push_u16(bytes, fingerprint.schema_version);
    push_u32(bytes, fingerprint.float_radix);
    push_u32(bytes, fingerprint.float_mantissa_digits);
    push_i16(bytes, fingerprint.float_min_exp);
    push_i16(bytes, fingerprint.float_max_exp);
    push_u16(bytes, fingerprint.pointer_width);
    bytes.push(fingerprint.endian.canonical_tag());
    push_object_id(bytes, fingerprint.toolchain_oid);
    push_object_id(bytes, fingerprint.foundation_oid);
    push_object_id(bytes, fingerprint.math_abi_oid);
}

fn push_profile(bytes: &mut Vec<u8>, profile: NoRegretProfile) -> Result<(), NoRegretError> {
    push_object_id(bytes, profile.profile_oid);
    push_u64(bytes, profile.learning_rate_bits);
    push_u64(bytes, profile.exploration_rate_bits);
    push_u32(bytes, usize_to_u32(profile.maximum_arms)?);
    push_u64(
        bytes,
        u64::try_from(profile.maximum_decision_epochs).map_err(|_| {
            NoRegretError::BoundUnrepresentable {
                kind: NoRegretBoundKind::DecisionEpochs,
                actual: profile.maximum_decision_epochs,
            }
        })?,
    );
    push_u64(
        bytes,
        u64::try_from(profile.maximum_regime_epochs).map_err(|_| {
            NoRegretError::BoundUnrepresentable {
                kind: NoRegretBoundKind::RegimeEpochs,
                actual: profile.maximum_regime_epochs,
            }
        })?,
    );
    push_u64(
        bytes,
        u64::try_from(profile.retained_receipts).map_err(|_| {
            NoRegretError::BoundUnrepresentable {
                kind: NoRegretBoundKind::RetainedReceipts,
                actual: profile.retained_receipts,
            }
        })?,
    );
    Ok(())
}

fn push_object_id(bytes: &mut Vec<u8>, oid: ObjectId) {
    bytes.extend_from_slice(&oid.0);
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_i16(bytes: &mut Vec<u8>, value: i16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST_SEQUENCE: u64 = 100;

    fn oid(tag: u8) -> ObjectId {
        ObjectId([tag; 32])
    }

    struct TestStateSpaceVerifier;

    impl NoRegretStateSpaceIdentityVerifier for TestStateSpaceVerifier {
        fn verify_state_space_oid(&self, claimed_oid: ObjectId, bytes: &[u8]) -> bool {
            let Ok(expected) = canonical_action_space_bytes(&[oid(10), oid(20), oid(30)], oid(20))
            else {
                return false;
            };
            claimed_oid == oid(3) && bytes == expected
        }
    }

    impl NoRegretProfileIdentityVerifier for TestStateSpaceVerifier {
        fn verify_no_regret_profile_oid(
            &self,
            claimed_oid: ObjectId,
            _canonical_profile: &[u8],
        ) -> bool {
            claimed_oid == oid(2)
        }
    }

    impl NoRegretRegimeTransitionAuthority for TestStateSpaceVerifier {
        fn verify_regime_shift(&self, shift: NoRegretRegimeShift) -> bool {
            shift
                .next
                .regime_oid
                .0
                .iter()
                .zip(shift.evidence_oid.0)
                .all(|(next, evidence)| next.wrapping_add(1) == evidence)
        }
    }

    struct HashProfileAuthority;

    impl NoRegretProfileIdentityVerifier for HashProfileAuthority {
        fn verify_no_regret_profile_oid(
            &self,
            claimed_oid: ObjectId,
            canonical_profile: &[u8],
        ) -> bool {
            claimed_oid == ObjectId(asupersync::atp::object::compute_hash(canonical_profile))
        }
    }

    struct PermissiveAuthority;

    impl NoRegretProfileIdentityVerifier for PermissiveAuthority {
        fn verify_no_regret_profile_oid(
            &self,
            _claimed_oid: ObjectId,
            _canonical_profile: &[u8],
        ) -> bool {
            true
        }
    }

    impl NoRegretStateSpaceIdentityVerifier for PermissiveAuthority {
        fn verify_state_space_oid(&self, _claimed_oid: ObjectId, _bytes: &[u8]) -> bool {
            true
        }
    }

    impl NoRegretRegimeTransitionAuthority for PermissiveAuthority {
        fn verify_regime_shift(&self, _shift: NoRegretRegimeShift) -> bool {
            true
        }
    }

    fn fingerprint() -> NoRegretNumericFingerprint {
        NoRegretNumericFingerprint::current(oid(80), oid(81), oid(82))
    }

    fn action_space() -> Result<NoRegretActionSpace, NoRegretError> {
        NoRegretActionSpace::try_new(
            vec![oid(10), oid(20), oid(30)],
            oid(20),
            oid(3),
            &TestStateSpaceVerifier,
        )
    }

    fn profile(
        maximum_decision_epochs: usize,
        maximum_regime_epochs: usize,
        retained_receipts: usize,
    ) -> Result<NoRegretProfile, NoRegretError> {
        NoRegretProfile::try_new(
            oid(2),
            0.05,
            0.05,
            3,
            maximum_decision_epochs,
            maximum_regime_epochs,
            retained_receipts,
        )
    }

    fn identity(last_sequence: u64, replay_seed: u64) -> Result<NoRegretIdentity, NoRegretError> {
        NoRegretIdentity::try_new(
            oid(1),
            oid(2),
            oid(3),
            oid(4),
            oid(5),
            oid(6),
            oid(7),
            11,
            FIRST_SEQUENCE,
            last_sequence,
            oid(20),
            replay_seed,
            fingerprint(),
        )
    }

    fn controller(
        maximum_decision_epochs: usize,
        maximum_regime_epochs: usize,
        retained_receipts: usize,
        replay_seed: u64,
        assumptions: NoRegretAssumptions,
    ) -> Result<NoRegretController, NoRegretError> {
        let epoch_span = u64::try_from(maximum_decision_epochs).map_err(|_| {
            NoRegretError::BoundUnrepresentable {
                kind: NoRegretBoundKind::DecisionEpochs,
                actual: maximum_decision_epochs,
            }
        })?;
        let last_sequence = FIRST_SEQUENCE
            .checked_add(epoch_span.saturating_sub(1))
            .ok_or(NoRegretError::WindowLengthOverflow {
                first: FIRST_SEQUENCE,
                last: u64::MAX,
            })?;
        NoRegretController::try_new(
            identity(last_sequence, replay_seed)?,
            profile(
                maximum_decision_epochs,
                maximum_regime_epochs,
                retained_receipts,
            )?,
            action_space()?,
            assumptions,
            fingerprint(),
            &TestStateSpaceVerifier,
        )
    }

    fn selected_loss(selected: ObjectId) -> f64 {
        if selected == oid(10) { 0.0 } else { 1.0 }
    }

    #[test]
    fn replay_is_value_and_byte_identical() -> Result<(), NoRegretError> {
        let mut first = controller(
            512,
            4,
            512,
            0x5eed_1234,
            NoRegretAssumptions::fully_supported(),
        )?;
        let mut second = controller(
            512,
            4,
            512,
            0x5eed_1234,
            NoRegretAssumptions::fully_supported(),
        )?;

        for offset in 0..512u64 {
            let sequence = FIRST_SEQUENCE + offset;
            let first_selection = first.choose(sequence)?;
            let second_selection = second.choose(sequence)?;
            assert_eq!(first_selection, second_selection);

            let loss = selected_loss(first_selection.selected_policy_oid());
            let first_summary = first.feedback(first_selection, loss)?;
            let second_summary = second.feedback(second_selection, loss)?;
            assert_eq!(first_summary, second_summary);
            let first_receipt = first
                .latest_receipt()
                .ok_or(NoRegretError::NoPendingDecision)?;
            let second_receipt = second
                .latest_receipt()
                .ok_or(NoRegretError::NoPendingDecision)?;
            assert_eq!(first_receipt, second_receipt);
            assert_eq!(
                first_receipt.try_canonical_bytes()?,
                second_receipt.try_canonical_bytes()?
            );
        }
        assert_eq!(first.try_weight_bits()?, second.try_weight_bits()?);
        Ok(())
    }

    #[test]
    fn canonical_receipt_lengths_match_reserved_layouts() -> Result<(), NoRegretError> {
        let mut learner = controller(2, 2, 2, 13, NoRegretAssumptions::fully_supported())?;
        let selection = learner.choose(FIRST_SEQUENCE)?;
        let decision_bytes = learner
            .pending_decision()
            .ok_or(NoRegretError::NoPendingDecision)?
            .try_canonical_bytes()?;
        let action_count = learner.action_space().len();
        assert_eq!(
            decision_bytes.len(),
            DECISION_FIXED_BYTES + (action_count * (OBJECT_ID_BYTES + (3 * FLOAT_BYTES)))
        );
        learner.feedback(selection, 0.5)?;
        let feedback_bytes = learner
            .latest_receipt()
            .ok_or(NoRegretError::NoPendingDecision)?
            .try_canonical_bytes()?;
        assert_eq!(
            feedback_bytes.len(),
            FEEDBACK_FIXED_BYTES + decision_bytes.len() + (action_count * FLOAT_BYTES)
        );

        let shift = NoRegretRegimeShift::try_new(
            learner.current_regime(),
            NoRegretRegime::new(oid(70), 12),
            FIRST_SEQUENCE + 1,
            oid(71),
        )?;
        let reset_bytes = learner
            .apply_regime_shift(shift, &TestStateSpaceVerifier)?
            .try_canonical_bytes()?;
        assert_eq!(
            reset_bytes.len(),
            REGIME_RESET_FIXED_BYTES + (action_count * (OBJECT_ID_BYTES + (2 * FLOAT_BYTES)))
        );
        Ok(())
    }

    #[test]
    fn lower_loss_arm_accumulates_most_probability() -> Result<(), NoRegretError> {
        let epochs = 4_000usize;
        let mut learner = controller(
            epochs,
            2,
            8,
            0x7f31_92ab,
            NoRegretAssumptions::fully_supported(),
        )?;
        for offset in 0..u64::try_from(epochs).map_err(|_| NoRegretError::BoundUnrepresentable {
            kind: NoRegretBoundKind::DecisionEpochs,
            actual: epochs,
        })? {
            let selection = learner.choose(FIRST_SEQUENCE + offset)?;
            learner.feedback(selection, selected_loss(selection.selected_policy_oid()))?;
        }
        let probabilities = learner.try_current_probability_bits()?;
        let best = f64::from_bits(*probabilities.first().ok_or(
            NoRegretError::InternalIndexOutOfBounds {
                vector: NoRegretVectorKind::Probability,
                index: 0,
                length: probabilities.len(),
            },
        )?);
        let fallback = f64::from_bits(*probabilities.get(1).ok_or(
            NoRegretError::InternalIndexOutOfBounds {
                vector: NoRegretVectorKind::Probability,
                index: 1,
                length: probabilities.len(),
            },
        )?);
        let third = f64::from_bits(*probabilities.get(2).ok_or(
            NoRegretError::InternalIndexOutOfBounds {
                vector: NoRegretVectorKind::Probability,
                index: 2,
                length: probabilities.len(),
            },
        )?);
        assert!(best > 0.8, "best probability was {best}");
        assert!(best > fallback);
        assert!(best > third);
        Ok(())
    }

    #[test]
    fn every_logged_probability_vector_is_normalized() -> Result<(), NoRegretError> {
        let mut learner = controller(
            256,
            2,
            256,
            0x8844_2211,
            NoRegretAssumptions::fully_supported(),
        )?;
        for offset in 0..256u64 {
            let selection = learner.choose(FIRST_SEQUENCE + offset)?;
            let pending = learner
                .pending_decision()
                .ok_or(NoRegretError::NoPendingDecision)?;
            let probabilities = pending.try_probabilities()?;
            let sum: f64 = probabilities.iter().sum();
            assert!((sum - 1.0).abs() <= 8.0 * f64::EPSILON);
            assert!(
                probabilities
                    .iter()
                    .all(|value| { value.is_finite() && (0.0..=1.0).contains(value) })
            );
            assert_eq!(
                pending.policy_oids().len(),
                pending.probability_bits().len()
            );
            assert_eq!(
                pending.policy_oids().len(),
                pending.weight_bits_before().len()
            );
            learner.feedback(selection, selected_loss(selection.selected_policy_oid()))?;
        }
        Ok(())
    }

    #[test]
    fn unsupported_assumption_selects_and_retains_fallback() -> Result<(), NoRegretError> {
        let assumptions = NoRegretAssumptions::new(true, false, true, true);
        let mut learner = controller(3, 2, 3, 17, assumptions)?;
        let initial_weights = learner.try_weight_bits()?;
        for offset in 0..3u64 {
            let selection = learner.choose(FIRST_SEQUENCE + offset)?;
            assert_eq!(
                selection.mode(),
                NoRegretSelectionMode::UnsupportedAssumptionsFallback
            );
            assert_eq!(selection.selected_policy_oid(), oid(20));
            let pending = learner
                .pending_decision()
                .ok_or(NoRegretError::NoPendingDecision)?;
            assert_eq!(pending.pinned_fallback_oid(), oid(20));
            assert_eq!(pending.assumptions(), assumptions);
            assert_eq!(
                pending.probability_bits(),
                &[0.0f64.to_bits(), 1.0f64.to_bits(), 0.0f64.to_bits()]
            );
            learner.feedback(selection, 0.75)?;
        }
        assert_eq!(learner.try_weight_bits()?, initial_weights);
        Ok(())
    }

    #[test]
    fn sequencing_and_feedback_mismatches_are_atomic() -> Result<(), NoRegretError> {
        let mut learner = controller(4, 2, 4, 19, NoRegretAssumptions::fully_supported())?;
        assert!(matches!(
            learner.choose(FIRST_SEQUENCE + 1),
            Err(NoRegretError::UnexpectedSequence {
                expected: FIRST_SEQUENCE,
                actual
            }) if actual == FIRST_SEQUENCE + 1
        ));
        assert_eq!(learner.next_sequence(), Some(FIRST_SEQUENCE));
        assert_eq!(learner.completed_epochs(), 0);

        let selection = learner.choose(FIRST_SEQUENCE)?;
        let pending_before = learner
            .pending_decision()
            .ok_or(NoRegretError::NoPendingDecision)?
            .try_canonical_bytes()?;
        assert!(matches!(
            learner.choose(FIRST_SEQUENCE),
            Err(NoRegretError::FeedbackPending {
                pending_sequence: FIRST_SEQUENCE
            })
        ));

        let wrong_sequence = NoRegretDecisionSelection {
            sequence: FIRST_SEQUENCE + 1,
            ..selection
        };
        assert!(matches!(
            learner.feedback(wrong_sequence, 0.5),
            Err(NoRegretError::FeedbackSequenceMismatch {
                expected: FIRST_SEQUENCE,
                actual
            }) if actual == FIRST_SEQUENCE + 1
        ));
        let wrong_policy = NoRegretDecisionSelection {
            selected_policy_oid: oid(99),
            ..selection
        };
        assert!(matches!(
            learner.feedback(wrong_policy, 0.5),
            Err(NoRegretError::FeedbackPolicyMismatch { .. })
        ));
        assert!(matches!(
            learner.feedback(selection, f64::NAN),
            Err(NoRegretError::NonFiniteNumeric {
                field: NoRegretNumericField::NormalizedLoss,
                ..
            })
        ));
        assert_eq!(
            learner
                .pending_decision()
                .ok_or(NoRegretError::NoPendingDecision)?
                .try_canonical_bytes()?,
            pending_before
        );
        assert_eq!(learner.completed_epochs(), 0);
        assert_eq!(learner.retained_receipt_count(), 0);

        learner.feedback(selection, -0.0)?;
        let receipt = learner
            .latest_receipt()
            .ok_or(NoRegretError::NoPendingDecision)?;
        assert_eq!(receipt.normalized_loss_bits(), 0.0f64.to_bits());
        assert!(matches!(
            learner.feedback(selection, 0.0),
            Err(NoRegretError::NoPendingDecision)
        ));
        assert_eq!(learner.completed_epochs(), 1);
        Ok(())
    }

    #[test]
    fn construction_and_runtime_bounds_are_enforced() -> Result<(), NoRegretError> {
        let shorter_window = NoRegretController::try_new(
            identity(FIRST_SEQUENCE + 1, 19)?,
            profile(3, 2, 3)?,
            action_space()?,
            NoRegretAssumptions::fully_supported(),
            fingerprint(),
            &TestStateSpaceVerifier,
        )?;
        assert_eq!(shorter_window.identity().window_capacity(), 2);
        assert_eq!(shorter_window.profile().maximum_decision_epochs(), 3);

        assert!(matches!(
            NoRegretController::try_new(
                identity(FIRST_SEQUENCE + 2, 21)?,
                profile(2, 2, 2)?,
                action_space()?,
                NoRegretAssumptions::fully_supported(),
                fingerprint(),
                &TestStateSpaceVerifier,
            ),
            Err(NoRegretError::DecisionWindowExceedsLimit {
                window_capacity: 3,
                limit: 2,
            })
        ));

        assert!(matches!(
            NoRegretActionSpace::try_new(
                vec![oid(20), oid(10)],
                oid(20),
                oid(3),
                &TestStateSpaceVerifier
            ),
            Err(NoRegretError::ArmsOutOfOrder { .. })
        ));
        assert!(matches!(
            NoRegretActionSpace::try_new(
                vec![oid(10), oid(10)],
                oid(10),
                oid(3),
                &TestStateSpaceVerifier
            ),
            Err(NoRegretError::DuplicateArm { .. })
        ));
        assert!(matches!(
            NoRegretActionSpace::try_new(
                vec![oid(10), oid(20)],
                oid(30),
                oid(3),
                &TestStateSpaceVerifier
            ),
            Err(NoRegretError::FallbackNotInActionSpace { .. })
        ));
        assert!(matches!(
            NoRegretProfile::try_new(oid(2), 0.0, 0.1, 2, 2, 2, 2),
            Err(NoRegretError::RateOutOfRange {
                field: NoRegretNumericField::LearningRate,
                ..
            })
        ));
        assert!(matches!(
            NoRegretProfile::try_new(oid(2), 0.1, 0.1, 0, 2, 2, 2),
            Err(NoRegretError::ZeroBound {
                kind: NoRegretBoundKind::Arms
            })
        ));

        let mut learner = controller(3, 2, 2, 23, NoRegretAssumptions::fully_supported())?;
        for offset in 0..3u64 {
            let selection = learner.choose(FIRST_SEQUENCE + offset)?;
            learner.feedback(selection, 0.25)?;
        }
        assert_eq!(learner.retained_receipt_count(), 2);
        let ordinals: Vec<u64> = learner
            .retained_receipts()
            .map(|receipt| receipt.decision().ordinal())
            .collect();
        assert_eq!(ordinals, vec![1, 2]);
        assert!(matches!(
            learner.choose(FIRST_SEQUENCE + 3),
            Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::DecisionEpochs,
                actual: 4,
                maximum: 3
            })
        ));
        Ok(())
    }

    #[test]
    fn profile_identity_covers_retention_and_controller_rejects_joint_memory_budget()
    -> Result<(), NoRegretError> {
        let descriptor = NoRegretProfile::try_canonical_descriptor_bytes(0.05, 0.05, 3, 4, 2, 4)?;
        let profile_oid = ObjectId(asupersync::atp::object::compute_hash(&descriptor));
        let verified = NoRegretProfile::try_new_verified(
            profile_oid,
            0.05,
            0.05,
            3,
            4,
            2,
            4,
            &HashProfileAuthority,
        )?;
        assert_eq!(verified.try_canonical_bytes()?, descriptor);
        assert!(matches!(
            NoRegretProfile::try_new_verified(
                profile_oid,
                0.05,
                0.05,
                3,
                4,
                2,
                5,
                &HashProfileAuthority,
            ),
            Err(NoRegretError::ProfileIdentityUnverified { claimed })
                if claimed == profile_oid
        ));

        let policies = (1_u8..=17).map(oid).collect::<Vec<_>>();
        let fallback = policies[0];
        let large_action_space =
            NoRegretActionSpace::try_new(policies, fallback, oid(90), &PermissiveAuthority)?;
        let oversized_profile = NoRegretProfile::try_new(
            oid(91),
            0.05,
            0.05,
            17,
            1,
            1,
            MAX_NO_REGRET_RETAINED_RECEIPTS,
        )?;
        let oversized_identity = NoRegretIdentity::try_new(
            oid(92),
            oversized_profile.profile_oid(),
            large_action_space.state_space_oid(),
            oid(93),
            oid(94),
            oid(95),
            oid(96),
            1,
            FIRST_SEQUENCE,
            FIRST_SEQUENCE,
            fallback,
            7,
            fingerprint(),
        )?;
        assert!(matches!(
            NoRegretController::try_new(
                oversized_identity,
                oversized_profile,
                large_action_space,
                NoRegretAssumptions::fully_supported(),
                fingerprint(),
                &PermissiveAuthority,
            ),
            Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::RetainedArmSlots,
                actual,
                maximum: MAX_NO_REGRET_RETAINED_ARM_SLOTS,
            }) if actual == 17 * MAX_NO_REGRET_RETAINED_RECEIPTS
        ));
        Ok(())
    }

    #[test]
    fn unsupported_exploration_resolution_is_rejected_before_sampling() -> Result<(), NoRegretError>
    {
        let tiny_profile = NoRegretProfile::try_new(oid(2), 0.05, 1.0e-20, 3, 1, 1, 1)?;
        assert!(matches!(
            NoRegretController::try_new(
                identity(FIRST_SEQUENCE, 27)?,
                tiny_profile,
                action_space()?,
                NoRegretAssumptions::fully_supported(),
                fingerprint(),
                &TestStateSpaceVerifier,
            ),
            Err(NoRegretError::ExplorationMassUnsupported { arms: 3, .. })
        ));
        Ok(())
    }

    #[test]
    fn regime_shift_resets_priors_and_forces_fallback() -> Result<(), NoRegretError> {
        let mut learner = controller(32, 3, 32, 29, NoRegretAssumptions::fully_supported())?;
        for offset in 0..8u64 {
            let selection = learner.choose(FIRST_SEQUENCE + offset)?;
            learner.feedback(selection, selected_loss(selection.selected_policy_oid()))?;
        }
        let trained_weights = learner.try_weight_bits()?;
        let prior_bits = uniform_prior(3)?.to_bits();
        assert_ne!(trained_weights, vec![prior_bits; 3]);

        let wrong_previous = NoRegretRegime::new(oid(90), 11);
        let rejected_shift = NoRegretRegimeShift::try_new(
            wrong_previous,
            NoRegretRegime::new(oid(91), 12),
            FIRST_SEQUENCE + 8,
            oid(92),
        )?;
        assert!(matches!(
            learner.apply_regime_shift(rejected_shift, &TestStateSpaceVerifier),
            Err(NoRegretError::PreviousRegimeMismatch { .. })
        ));
        assert_eq!(learner.try_weight_bits()?, trained_weights);
        assert_eq!(learner.current_regime(), NoRegretRegime::new(oid(6), 11));

        let accepted_shift = NoRegretRegimeShift::try_new(
            learner.current_regime(),
            NoRegretRegime::new(oid(40), 12),
            FIRST_SEQUENCE + 8,
            oid(41),
        )?;
        let reset = learner.apply_regime_shift(accepted_shift, &TestStateSpaceVerifier)?;
        assert_eq!(reset.pinned_fallback_oid(), oid(20));
        assert_eq!(reset.weight_bits_before(), trained_weights);
        assert_eq!(reset.prior_weight_bits(), &[prior_bits; 3]);
        assert_eq!(learner.try_weight_bits()?, vec![prior_bits; 3]);
        assert_eq!(learner.current_regime(), NoRegretRegime::new(oid(40), 12));
        assert_eq!(learner.observed_regime_epochs(), 2);

        let selection = learner.choose(FIRST_SEQUENCE + 8)?;
        assert_eq!(selection.mode(), NoRegretSelectionMode::RegimeResetFallback);
        assert_eq!(selection.selected_policy_oid(), oid(20));
        let pending = learner
            .pending_decision()
            .ok_or(NoRegretError::NoPendingDecision)?;
        assert_eq!(pending.regime(), NoRegretRegime::new(oid(40), 12));
        assert_eq!(
            pending.probability_bits(),
            &[0.0f64.to_bits(), 1.0f64.to_bits(), 0.0f64.to_bits()]
        );
        assert!(matches!(
            learner.apply_regime_shift(
                NoRegretRegimeShift::try_new(
                    NoRegretRegime::new(oid(40), 12),
                    NoRegretRegime::new(oid(42), 13),
                    FIRST_SEQUENCE + 8,
                    oid(43),
                )?,
                &TestStateSpaceVerifier,
            ),
            Err(NoRegretError::RegimeShiftWhileFeedbackPending { .. })
        ));
        learner.feedback(selection, 1.0)?;
        assert_eq!(learner.try_weight_bits()?, vec![prior_bits; 3]);
        Ok(())
    }

    #[test]
    fn regime_successor_and_evidence_require_authority_live_and_on_decode()
    -> Result<(), NoRegretError> {
        let mut learner = controller(2, 2, 2, 30, NoRegretAssumptions::fully_supported())?;
        let before_weights = learner.try_weight_bits()?;
        let fabricated = NoRegretRegimeShift::try_new(
            learner.current_regime(),
            NoRegretRegime::new(oid(40), 12),
            FIRST_SEQUENCE,
            oid(99),
        )?;
        assert!(matches!(
            learner.apply_regime_shift(fabricated, &TestStateSpaceVerifier),
            Err(NoRegretError::RegimeShiftUnverified {
                next,
                evidence_oid,
            }) if next == NoRegretRegime::new(oid(40), 12) && evidence_oid == oid(99)
        ));
        assert_eq!(learner.current_regime(), NoRegretRegime::new(oid(6), 11));
        assert_eq!(learner.try_weight_bits()?, before_weights);
        assert_eq!(learner.replay_history().len(), 0);

        let authorized = NoRegretRegimeShift::try_new(
            learner.current_regime(),
            NoRegretRegime::new(oid(40), 12),
            FIRST_SEQUENCE,
            oid(41),
        )?;
        let mut reset = learner.apply_regime_shift(authorized, &TestStateSpaceVerifier)?;
        reset.shift.evidence_oid = oid(99);
        let fabricated_bytes = reset.try_canonical_bytes()?;
        assert!(matches!(
            NoRegretRegimeResetReceipt::try_from_canonical_bytes(
                &fabricated_bytes,
                &TestStateSpaceVerifier,
                fingerprint(),
            ),
            Err(NoRegretError::RegimeShiftUnverified { evidence_oid, .. })
                if evidence_oid == oid(99)
        ));
        Ok(())
    }

    #[test]
    fn reset_receipts_replay_byte_identically() -> Result<(), NoRegretError> {
        let mut first = controller(4, 2, 4, 31, NoRegretAssumptions::fully_supported())?;
        let mut second = controller(4, 2, 4, 31, NoRegretAssumptions::fully_supported())?;
        let first_selection = first.choose(FIRST_SEQUENCE)?;
        let second_selection = second.choose(FIRST_SEQUENCE)?;
        first.feedback(first_selection, 0.75)?;
        second.feedback(second_selection, 0.75)?;
        let shift = NoRegretRegimeShift::try_new(
            first.current_regime(),
            NoRegretRegime::new(oid(44), 12),
            FIRST_SEQUENCE + 1,
            oid(45),
        )?;
        let first_reset = first.apply_regime_shift(shift, &TestStateSpaceVerifier)?;
        let second_reset = second.apply_regime_shift(shift, &TestStateSpaceVerifier)?;
        assert_eq!(first_reset, second_reset);
        assert_eq!(
            first_reset.try_canonical_bytes()?,
            second_reset.try_canonical_bytes()?
        );
        Ok(())
    }

    #[test]
    fn exact_mass_apportionment_never_selects_zero_mass_tail() -> Result<(), NoRegretError> {
        let probability_bits = [
            (1.0f64 / 3.0).to_bits(),
            (2.0f64 / 3.0).to_bits(),
            0.0f64.to_bits(),
        ];
        let masses = exact_sampling_masses(&probability_bits)?;
        assert_eq!(masses.iter().sum::<u64>(), SAMPLE_SCALE);
        assert!(masses[0] > 0);
        assert!(masses[1] > 0);
        assert_eq!(masses[2], 0);
        assert_eq!(sample_categorical(&masses, u64::MAX)?, 1);

        let undersubscribed = [0.1f64.to_bits(), 0.1f64.to_bits(), 0.0f64.to_bits()];
        let undersubscribed_masses = exact_sampling_masses(&undersubscribed)?;
        assert_eq!(undersubscribed_masses.iter().sum::<u64>(), SAMPLE_SCALE);
        assert_eq!(undersubscribed_masses[2], 0);

        let oversubscribed = [0.9f64.to_bits(), 0.9f64.to_bits(), 0.0f64.to_bits()];
        let oversubscribed_masses = exact_sampling_masses(&oversubscribed)?;
        assert_eq!(oversubscribed_masses.iter().sum::<u64>(), SAMPLE_SCALE);
        assert_eq!(oversubscribed_masses[2], 0);

        let sub_unit_probability = 2.0f64.powi(-54);
        let positive_tail = [
            1.0f64.to_bits(),
            sub_unit_probability.to_bits(),
            0.0f64.to_bits(),
        ];
        let positive_tail_masses = exact_sampling_masses(&positive_tail)?;
        assert_eq!(positive_tail_masses.iter().sum::<u64>(), SAMPLE_SCALE);
        assert_eq!(positive_tail_masses[1], 1);
        assert_eq!(positive_tail_masses[2], 0);
        Ok(())
    }

    #[test]
    fn action_space_identity_is_authoritatively_bound() -> Result<(), NoRegretError> {
        let policies = [oid(10), oid(20), oid(30)];
        let descriptor = NoRegretActionSpace::try_canonical_descriptor_bytes(&policies, oid(20))?;
        let action_space = NoRegretActionSpace::try_new(
            policies.to_vec(),
            oid(20),
            oid(3),
            &TestStateSpaceVerifier,
        )?;
        assert_eq!(action_space.try_canonical_bytes()?, descriptor);
        assert!(matches!(
            NoRegretActionSpace::try_new(
                vec![oid(10), oid(20), oid(30)],
                oid(20),
                oid(99),
                &TestStateSpaceVerifier
            ),
            Err(NoRegretError::StateSpaceIdentityUnverified { claimed }) if claimed == oid(99)
        ));

        let forged_action_space = NoRegretActionSpace::try_new(
            vec![oid(10), oid(20), oid(40)],
            oid(20),
            oid(3),
            &PermissiveAuthority,
        )?;
        assert!(matches!(
            NoRegretController::try_new(
                identity(FIRST_SEQUENCE, 31)?,
                profile(1, 1, 1)?,
                forged_action_space,
                NoRegretAssumptions::fully_supported(),
                fingerprint(),
                &TestStateSpaceVerifier,
            ),
            Err(NoRegretError::StateSpaceIdentityUnverified { claimed }) if claimed == oid(3)
        ));
        Ok(())
    }

    #[test]
    fn controller_rejects_untrusted_numeric_implementation_identity() -> Result<(), NoRegretError> {
        let mut drifted_identity = identity(FIRST_SEQUENCE + 1, 35)?;
        drifted_identity.numeric_fingerprint =
            NoRegretNumericFingerprint::current(oid(99), oid(81), oid(82));
        assert!(matches!(
            NoRegretController::try_new(
                drifted_identity,
                profile(2, 2, 2)?,
                action_space()?,
                NoRegretAssumptions::fully_supported(),
                fingerprint(),
                &TestStateSpaceVerifier,
            ),
            Err(NoRegretError::NumericFingerprintMismatch {
                component: NoRegretFingerprintField::Toolchain
            })
        ));
        Ok(())
    }

    #[test]
    fn canonical_readers_replay_and_reject_tampering() -> Result<(), NoRegretError> {
        let mut learner = controller(4, 2, 8, 37, NoRegretAssumptions::fully_supported())?;
        let selection = learner.choose(FIRST_SEQUENCE)?;
        let decision_bytes = learner
            .pending_decision()
            .ok_or(NoRegretError::NoPendingDecision)?
            .try_canonical_bytes()?;
        let decoded_decision = NoRegretDecisionReceipt::try_from_canonical_bytes(
            &decision_bytes,
            &TestStateSpaceVerifier,
            fingerprint(),
        )?;
        assert_eq!(
            &decoded_decision,
            learner
                .pending_decision()
                .ok_or(NoRegretError::NoPendingDecision)?
        );
        let mut drifted_decision = decoded_decision.clone();
        drifted_decision.identity.numeric_fingerprint =
            NoRegretNumericFingerprint::current(oid(99), oid(81), oid(82));
        let drifted_bytes = drifted_decision.try_canonical_bytes()?;
        assert!(matches!(
            NoRegretDecisionReceipt::try_from_canonical_bytes(
                &drifted_bytes,
                &TestStateSpaceVerifier,
                fingerprint(),
            ),
            Err(NoRegretError::NumericFingerprintMismatch {
                component: NoRegretFingerprintField::Toolchain
            })
        ));

        learner.feedback(selection, 0.75)?;
        let feedback_bytes = learner
            .latest_receipt()
            .ok_or(NoRegretError::NoPendingDecision)?
            .try_canonical_bytes()?;
        let decoded_feedback = NoRegretFeedbackReceipt::try_from_canonical_bytes(
            &feedback_bytes,
            &TestStateSpaceVerifier,
            fingerprint(),
        )?;
        assert_eq!(
            &decoded_feedback,
            learner
                .latest_receipt()
                .ok_or(NoRegretError::NoPendingDecision)?
        );

        let shift = NoRegretRegimeShift::try_new(
            learner.current_regime(),
            NoRegretRegime::new(oid(74), 12),
            FIRST_SEQUENCE + 1,
            oid(75),
        )?;
        let reset = learner.apply_regime_shift(shift, &TestStateSpaceVerifier)?;
        let reset_bytes = reset.try_canonical_bytes()?;
        assert_eq!(
            NoRegretRegimeResetReceipt::try_from_canonical_bytes(
                &reset_bytes,
                &TestStateSpaceVerifier,
                fingerprint(),
            )?,
            reset
        );

        let mut tampered = feedback_bytes;
        let last = tampered
            .last_mut()
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        *last ^= 1;
        assert!(
            NoRegretFeedbackReceipt::try_from_canonical_bytes(
                &tampered,
                &TestStateSpaceVerifier,
                fingerprint(),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn reset_events_are_retained_and_complete_history_replays() -> Result<(), NoRegretError> {
        let mut learner = controller(4, 3, 8, 41, NoRegretAssumptions::fully_supported())?;
        let first = learner.choose(FIRST_SEQUENCE)?;
        learner.feedback(first, 0.25)?;
        let shift = NoRegretRegimeShift::try_new(
            learner.current_regime(),
            NoRegretRegime::new(oid(76), 12),
            FIRST_SEQUENCE + 1,
            oid(77),
        )?;
        let reset = learner.apply_regime_shift(shift, &TestStateSpaceVerifier)?;
        assert_eq!(reset.identity(), learner.identity());
        assert_eq!(reset.profile(), learner.profile());
        assert_eq!(reset.policy_oids(), learner.action_space().policy_oids());
        assert!(matches!(
            learner.replay_history().last(),
            Some(NoRegretReplayEvent::RegimeReset(actual))
                if public_value_eq(actual, &reset)
        ));
        let second = learner.choose(FIRST_SEQUENCE + 1)?;
        learner.feedback(second, 0.5)?;
        let summary = learner.verify_replay_history(&TestStateSpaceVerifier, fingerprint())?;
        assert_eq!(summary.event_count(), 3);
        assert_eq!(summary.completed_epochs(), 2);
        assert_eq!(summary.observed_regime_epochs(), 2);
        assert_eq!(summary.current_regime(), NoRegretRegime::new(oid(76), 12));
        assert_eq!(summary.final_weight_bits(), learner.try_weight_bits()?);

        let wrong_fingerprint = NoRegretNumericFingerprint::current(oid(83), oid(81), oid(82));
        assert!(matches!(
            learner.verify_replay_history(&TestStateSpaceVerifier, wrong_fingerprint),
            Err(NoRegretError::NumericFingerprintMismatch {
                component: NoRegretFingerprintField::Toolchain
            })
        ));
        Ok(())
    }

    #[test]
    fn canonical_replay_log_round_trips_complete_chronology() -> Result<(), NoRegretError> {
        let assumptions = NoRegretAssumptions::fully_supported();
        let mut learner = controller(4, 3, 8, 47, assumptions)?;
        let first = learner.choose(FIRST_SEQUENCE)?;
        learner.feedback(first, 0.25)?;
        learner.apply_regime_shift(
            NoRegretRegimeShift::try_new(
                learner.current_regime(),
                NoRegretRegime::new(oid(84), 12),
                FIRST_SEQUENCE + 1,
                oid(85),
            )?,
            &TestStateSpaceVerifier,
        )?;
        let second = learner.choose(FIRST_SEQUENCE + 1)?;
        learner.feedback(second, 0.5)?;

        let bytes = learner.try_canonical_replay_log_bytes(&TestStateSpaceVerifier)?;
        let limits = NoRegretReplayLogDecodeLimits::new(bytes.len(), 3, 8, bytes.len());
        let decoded = NoRegretReplayLog::try_from_canonical_bytes(
            &bytes,
            limits,
            learner.identity(),
            learner.profile(),
            learner.action_space(),
            assumptions,
            &TestStateSpaceVerifier,
            fingerprint(),
        )?;
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded.identity(), learner.identity());
        assert_eq!(decoded.profile(), learner.profile());
        assert_eq!(decoded.action_space(), learner.action_space());
        assert_eq!(decoded.assumptions(), assumptions);
        assert!(decoded.events().iter().eq(learner.replay_history()));
        assert_eq!(decoded.try_canonical_bytes()?, bytes);
        let summary = decoded.verify(&TestStateSpaceVerifier, fingerprint())?;
        assert_eq!(summary.completed_epochs(), 2);
        assert_eq!(summary.observed_regime_epochs(), 2);
        assert_eq!(summary.final_weight_bits(), learner.try_weight_bits()?);
        Ok(())
    }

    #[test]
    fn replay_log_uses_one_chronological_rng_stream_not_seed_to_ordinal_restarts()
    -> Result<(), NoRegretError> {
        let assumptions = NoRegretAssumptions::fully_supported();
        let mut learner = controller(MAX_NO_REGRET_DECISION_EPOCHS, 2, 1, 48, assumptions)?;
        let selection = learner.choose(FIRST_SEQUENCE)?;
        learner.feedback(selection, 0.25)?;
        let mut hostile_receipt = learner
            .latest_receipt()
            .ok_or(NoRegretError::NoPendingDecision)?
            .clone();
        hostile_receipt.decision.ordinal = u64::try_from(MAX_NO_REGRET_DECISION_EPOCHS - 1)
            .map_err(|_| NoRegretError::CanonicalLengthOverflow)?;
        hostile_receipt.decision.sequence = learner.identity.last_sequence;
        let hostile_event = NoRegretReplayEvent::Feedback(hostile_receipt);
        let bytes = encode_replay_log(
            learner.identity,
            learner.profile,
            &learner.action_space,
            assumptions,
            core::iter::once(&hostile_event),
        )?;
        EXPECTED_RNG_REPLAY_STEPS.with(|steps| steps.set(0));
        let result = NoRegretReplayLog::try_from_canonical_bytes(
            &bytes,
            NoRegretReplayLogDecodeLimits::new(bytes.len(), 3, 1, bytes.len()),
            learner.identity,
            learner.profile,
            &learner.action_space,
            assumptions,
            &TestStateSpaceVerifier,
            fingerprint(),
        );
        assert!(matches!(
            result,
            Err(NoRegretError::UnexpectedSequence {
                expected: FIRST_SEQUENCE,
                actual,
            }) if actual == learner.identity.last_sequence
        ));
        EXPECTED_RNG_REPLAY_STEPS.with(|steps| assert_eq!(steps.get(), 0));
        Ok(())
    }

    #[test]
    fn replay_preflight_bounds_nested_arms_and_cumulative_retention_before_decode()
    -> Result<(), NoRegretError> {
        let assumptions = NoRegretAssumptions::fully_supported();
        let mut learner = controller(1, 1, 1, 50, assumptions)?;
        let selection = learner.choose(FIRST_SEQUENCE)?;
        learner.feedback(selection, 0.25)?;
        let bytes = learner.try_canonical_replay_log_bytes(&TestStateSpaceVerifier)?;
        let admitted = NoRegretReplayLogDecodeLimits::new(bytes.len(), 3, 1, bytes.len());

        let frame_start = REPLAY_LOG_FIXED_BYTES
            .checked_add(
                learner
                    .action_space
                    .len()
                    .checked_mul(OBJECT_ID_BYTES)
                    .ok_or(NoRegretError::CanonicalLengthOverflow)?,
            )
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let payload_start = frame_start
            .checked_add(REPLAY_LOG_FRAME_BYTES)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let decision_start = payload_start
            .checked_add(FEEDBACK_ENCODING_MAGIC.len() + 2 + 4)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let nested_maximum_arms = decision_start
            .checked_add(
                DECISION_ENCODING_MAGIC.len()
                    + 2
                    + IDENTITY_FIXED_BYTES
                    + OBJECT_ID_BYTES
                    + (FLOAT_BYTES * 2),
            )
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let mut oversized_nested_profile = bytes.clone();
        let nested_limit_bytes = oversized_nested_profile
            .get_mut(nested_maximum_arms..nested_maximum_arms + 4)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        nested_limit_bytes.copy_from_slice(&4_u32.to_le_bytes());
        assert!(matches!(
            NoRegretReplayLog::try_from_canonical_bytes(
                &oversized_nested_profile,
                admitted,
                learner.identity,
                learner.profile,
                &learner.action_space,
                assumptions,
                &TestStateSpaceVerifier,
                fingerprint(),
            ),
            Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::Arms,
                actual: 4,
                maximum: 3,
            })
        ));

        assert!(matches!(
            NoRegretReplayLog::try_from_canonical_bytes(
                &bytes,
                admitted.with_retention_budget(2, MAX_NO_REGRET_RETAINED_VECTOR_BYTES),
                learner.identity,
                learner.profile,
                &learner.action_space,
                assumptions,
                &TestStateSpaceVerifier,
                fingerprint(),
            ),
            Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::RetainedArmSlots,
                actual: 3,
                maximum: 2,
            })
        ));
        assert!(matches!(
            NoRegretReplayLog::try_from_canonical_bytes(
                &bytes,
                admitted.with_retention_budget(
                    3,
                    (3 * FEEDBACK_RETAINED_BYTES_PER_ARM) - 1,
                ),
                learner.identity,
                learner.profile,
                &learner.action_space,
                assumptions,
                &TestStateSpaceVerifier,
                fingerprint(),
            ),
            Err(NoRegretError::BoundExceeded {
                kind: NoRegretBoundKind::RetainedVectorBytes,
                actual,
                maximum,
            }) if actual == 3 * FEEDBACK_RETAINED_BYTES_PER_ARM
                && maximum == actual - 1
        ));
        Ok(())
    }

    #[test]
    fn replay_log_preflight_and_completeness_fail_closed() -> Result<(), NoRegretError> {
        let assumptions = NoRegretAssumptions::fully_supported();
        let mut pending = controller(2, 2, 4, 49, assumptions)?;
        let _ = pending.choose(FIRST_SEQUENCE)?;
        assert!(matches!(
            pending.try_canonical_replay_log_bytes(&TestStateSpaceVerifier),
            Err(NoRegretError::FeedbackPending {
                pending_sequence: FIRST_SEQUENCE
            })
        ));

        let mut truncated = controller(3, 2, 1, 51, assumptions)?;
        for offset in 0..2 {
            let selection = truncated.choose(FIRST_SEQUENCE + offset)?;
            truncated.feedback(selection, 0.5)?;
        }
        assert!(matches!(
            truncated.try_canonical_replay_log_bytes(&TestStateSpaceVerifier),
            Err(NoRegretError::ReplayHistoryTruncated)
        ));

        let mut learner = controller(2, 2, 4, 53, assumptions)?;
        let selection = learner.choose(FIRST_SEQUENCE)?;
        learner.feedback(selection, 0.25)?;
        let bytes = learner.try_canonical_replay_log_bytes(&TestStateSpaceVerifier)?;
        let admitted = NoRegretReplayLogDecodeLimits::new(bytes.len(), 3, 4, bytes.len());

        assert!(matches!(
            NoRegretReplayLog::try_from_canonical_bytes(
                &bytes,
                NoRegretReplayLogDecodeLimits::new(bytes.len() - 1, 3, 4, bytes.len()),
                learner.identity(),
                learner.profile(),
                learner.action_space(),
                assumptions,
                &TestStateSpaceVerifier,
                fingerprint(),
            ),
            Err(NoRegretError::CanonicalByteLimitExceeded {
                field: NoRegretCanonicalField::ReplayLog,
                ..
            })
        ));
        assert!(matches!(
            NoRegretReplayLog::try_from_canonical_bytes(
                &bytes,
                NoRegretReplayLogDecodeLimits::new(bytes.len(), 3, 4, 1),
                learner.identity(),
                learner.profile(),
                learner.action_space(),
                assumptions,
                &TestStateSpaceVerifier,
                fingerprint(),
            ),
            Err(NoRegretError::CanonicalByteLimitExceeded {
                field: NoRegretCanonicalField::ReplayEvent,
                ..
            })
        ));

        let mut reserved = bytes.clone();
        let reserved_byte = reserved
            .get_mut(10)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        *reserved_byte = 1;
        assert!(
            NoRegretReplayLog::try_from_canonical_bytes(
                &reserved,
                admitted,
                learner.identity(),
                learner.profile(),
                learner.action_space(),
                assumptions,
                &TestStateSpaceVerifier,
                fingerprint(),
            )
            .is_err()
        );

        let frame_start = REPLAY_LOG_FIXED_BYTES
            .checked_add(
                learner
                    .action_space()
                    .len()
                    .checked_mul(OBJECT_ID_BYTES)
                    .ok_or(NoRegretError::CanonicalLengthOverflow)?,
            )
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        let mut unknown_tag = bytes.clone();
        let tag = unknown_tag
            .get_mut(frame_start)
            .ok_or(NoRegretError::CanonicalLengthOverflow)?;
        *tag = 99;
        assert!(matches!(
            NoRegretReplayLog::try_from_canonical_bytes(
                &unknown_tag,
                admitted,
                learner.identity(),
                learner.profile(),
                learner.action_space(),
                assumptions,
                &TestStateSpaceVerifier,
                fingerprint(),
            ),
            Err(NoRegretError::CanonicalMalformed {
                field: NoRegretCanonicalField::ReplayEvent
            })
        ));
        Ok(())
    }

    #[test]
    fn truncated_history_fails_closed() -> Result<(), NoRegretError> {
        let mut learner = controller(3, 2, 1, 43, NoRegretAssumptions::fully_supported())?;
        for offset in 0..2 {
            let selection = learner.choose(FIRST_SEQUENCE + offset)?;
            learner.feedback(selection, 0.5)?;
        }
        assert!(learner.replay_history_truncated());
        assert!(matches!(
            learner.verify_replay_history(&TestStateSpaceVerifier, fingerprint()),
            Err(NoRegretError::ReplayHistoryTruncated)
        ));
        Ok(())
    }
}
