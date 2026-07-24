//! Deterministic, identity-bound ANN recall and drift evidence.
//!
//! `FG-CAL-03` is statistical evidence, not a per-query correctness claim.
//! This ledger compares complete candidate top-k result sets with complete
//! exact-baseline top-k result sets over a fixed, keyed query sample. It
//! computes every intersection internally, retains a bounded replay history,
//! and, once the fixed window completes, selects exactly one of a candidate
//! policy, a pinned exact fallback, or a rebuild policy.
//!
//! The confidence interval uses a fixed-point Hoeffding bound. For `n`
//! sampled queries and configured failure probability `delta = 2^-q`,
//! Hoeffding gives radius
//! `sqrt(ln(2 / delta) / (2n))`. Because
//! `ln(2 / delta) = (q + 1) ln(2) <= q + 1`, this implementation uses the
//! conservative radius `sqrt((q + 1) / (2n))`. Division and square root round
//! outward in integer arithmetic, so replay is independent of floating-point
//! behavior. The bound treats each per-query recall as one bounded
//! observation; it does not incorrectly treat the `k` neighbors within a
//! query as independent observations.
//!
//! The interval is inferential only when its disclosed sampling and baseline
//! assumptions hold. The ledger verifies identities, sequence, canonical
//! result sets, and resource bounds; it cannot prove that an upstream exact
//! search was implemented correctly or that a declared keyed sample was
//! actually drawn according to its design. Unsupported or unverified
//! assumptions deterministically select the pinned fallback.

use core::fmt;
use std::collections::BTreeSet;

use fgdb_types::ObjectId;

/// Exact denominator used by recalls and confidence bounds.
pub const RECALL_SCALE: u64 = 1_000_000_000;

/// Absolute query-count ceiling for one ledger.
pub const MAX_RECALL_QUERIES: usize = 1_048_576;

/// Absolute top-k ceiling for one query.
pub const MAX_RECALL_TOP_K: usize = 65_536;

/// Absolute number of retained baseline plus candidate result identities.
pub const MAX_RECALL_RESULT_IDS: usize = 4_194_304;

/// Largest supported power-of-two confidence exponent.
pub const MAX_CONFIDENCE_EXPONENT: u8 = 63;

/// Canonical ANN-recall replay-stream version.
pub const ANN_RECALL_REPLAY_VERSION: u16 = 1;

const REPLAY_MAGIC: [u8; 8] = *b"FGDBARR1";
const PROFILE_DESCRIPTOR_MAGIC: [u8; 8] = *b"FGDBARP1";
const PROFILE_DESCRIPTOR_VERSION: u16 = 1;
const REPLAY_RESERVED: u16 = 0;
const REPLAY_PROFILE_RESERVED: [u8; 3] = [0; 3];
const REPLAY_HEADER_BYTES: usize = 460;
const REPLAY_OBSERVATION_FIXED_BYTES: usize = 72;
const OBJECT_ID_BYTES: usize = 32;

/// Caller-owned admission bounds for a canonical ANN-recall replay stream.
///
/// These limits are independent of the encoded profile so input bytes cannot
/// authorize their own allocations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnnRecallReplayDecodeLimits {
    /// Largest complete encoded stream accepted.
    pub max_encoded_bytes: usize,
    /// Largest top-k profile accepted.
    pub max_top_k: usize,
    /// Largest query window or concrete observation count accepted.
    pub max_queries: usize,
    /// Largest total baseline-plus-candidate result inventory accepted.
    pub max_total_result_ids: usize,
}

impl AnnRecallReplayDecodeLimits {
    /// Creates an explicit replay admission policy.
    #[must_use]
    pub const fn new(
        max_encoded_bytes: usize,
        max_top_k: usize,
        max_queries: usize,
        max_total_result_ids: usize,
    ) -> Self {
        Self {
            max_encoded_bytes,
            max_top_k,
            max_queries,
            max_total_result_ids,
        }
    }
}

/// A finite inclusive stream window.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AnnRecallWindow {
    first_sequence: u64,
    last_sequence: u64,
    query_capacity: u64,
}

impl AnnRecallWindow {
    /// Constructs a non-empty inclusive sequence window.
    pub const fn try_new(first_sequence: u64, last_sequence: u64) -> Result<Self, AnnRecallError> {
        let Some(distance) = last_sequence.checked_sub(first_sequence) else {
            return Err(AnnRecallError::ReversedWindow {
                first: first_sequence,
                last: last_sequence,
            });
        };
        let Some(query_capacity) = distance.checked_add(1) else {
            return Err(AnnRecallError::WindowLengthOverflow {
                first: first_sequence,
                last: last_sequence,
            });
        };
        Ok(Self {
            first_sequence,
            last_sequence,
            query_capacity,
        })
    }

    /// Inclusive first source sequence.
    #[must_use]
    pub const fn first_sequence(self) -> u64 {
        self.first_sequence
    }

    /// Inclusive last source sequence.
    #[must_use]
    pub const fn last_sequence(self) -> u64 {
        self.last_sequence
    }

    /// Number of queries in the complete window.
    #[must_use]
    pub const fn query_capacity(self) -> u64 {
        self.query_capacity
    }
}

/// Query-sampling assumption attached to an interval.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum QuerySampleDesign {
    /// A keyed uniform sample without replacement from a fixed finite
    /// authorized query population.
    KeyedUniformWithoutReplacement = 1,
    /// Independent keyed draws with replacement from a fixed authorized query
    /// population.
    KeyedIndependentWithReplacement = 2,
    /// Dependence exists but no supported concentration method is registered.
    UnspecifiedDependence = 3,
}

impl QuerySampleDesign {
    const fn supports_hoeffding(self) -> bool {
        matches!(
            self,
            Self::KeyedUniformWithoutReplacement | Self::KeyedIndependentWithReplacement
        )
    }

    const fn try_from_tag(tag: u8) -> Result<Self, AnnRecallReplayCodecError> {
        match tag {
            1 => Ok(Self::KeyedUniformWithoutReplacement),
            2 => Ok(Self::KeyedIndependentWithReplacement),
            3 => Ok(Self::UnspecifiedDependence),
            _ => Err(AnnRecallReplayCodecError::UnknownSampleDesign { tag }),
        }
    }
}

/// Explicit assumptions under which the interval can be interpreted.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AnnRecallAssumptions {
    sample_design: QuerySampleDesign,
    exact_baseline_complete: bool,
    authorization_domain_fixed: bool,
    candidate_policy_fixed: bool,
}

impl AnnRecallAssumptions {
    /// Constructs the complete assumption disclosure.
    #[must_use]
    pub const fn new(
        sample_design: QuerySampleDesign,
        exact_baseline_complete: bool,
        authorization_domain_fixed: bool,
        candidate_policy_fixed: bool,
    ) -> Self {
        Self {
            sample_design,
            exact_baseline_complete,
            authorization_domain_fixed,
            candidate_policy_fixed,
        }
    }

    /// Sampling design used to choose queries.
    #[must_use]
    pub const fn sample_design(self) -> QuerySampleDesign {
        self.sample_design
    }

    /// Whether each baseline list is declared complete and exact at top-k.
    #[must_use]
    pub const fn exact_baseline_complete(self) -> bool {
        self.exact_baseline_complete
    }

    /// Whether population, snapshot, and authority stay fixed for the window.
    #[must_use]
    pub const fn authorization_domain_fixed(self) -> bool {
        self.authorization_domain_fixed
    }

    /// Whether the candidate search policy stays fixed for the window.
    #[must_use]
    pub const fn candidate_policy_fixed(self) -> bool {
        self.candidate_policy_fixed
    }

    /// Whether the built-in confidence method supports every disclosure.
    #[must_use]
    pub const fn supported(self) -> bool {
        self.sample_design.supports_hoeffding()
            && self.exact_baseline_complete
            && self.authorization_domain_fixed
            && self.candidate_policy_fixed
    }
}

/// Authority that binds a claimed ANN-recall profile OID to its complete
/// canonical resource, threshold, and assumption descriptor.
pub trait AnnRecallProfileIdentityVerifier {
    /// Returns whether `claimed_oid` authoritatively identifies
    /// `canonical_profile`.
    fn verify_ann_recall_profile_oid(
        &self,
        claimed_oid: ObjectId,
        canonical_profile: &[u8],
    ) -> bool;
}

/// Complete immutable identity of one recall-monitoring trial.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AnnRecallIdentity {
    monitor_oid: ObjectId,
    profile_oid: ObjectId,
    authorized_population_oid: ObjectId,
    snapshot_oid: ObjectId,
    authority_domain_oid: ObjectId,
    sample_key_oid: ObjectId,
    sample_design_oid: ObjectId,
    exact_baseline_oid: ObjectId,
    candidate_policy_oid: ObjectId,
    fallback_policy_oid: ObjectId,
    rebuild_policy_oid: ObjectId,
    window: AnnRecallWindow,
    regime_epoch: u64,
}

impl AnnRecallIdentity {
    /// Constructs the immutable monitor, population, sample, baseline, policy,
    /// stream-window, and regime identity.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        monitor_oid: ObjectId,
        profile_oid: ObjectId,
        authorized_population_oid: ObjectId,
        snapshot_oid: ObjectId,
        authority_domain_oid: ObjectId,
        sample_key_oid: ObjectId,
        sample_design_oid: ObjectId,
        exact_baseline_oid: ObjectId,
        candidate_policy_oid: ObjectId,
        fallback_policy_oid: ObjectId,
        rebuild_policy_oid: ObjectId,
        window: AnnRecallWindow,
        regime_epoch: u64,
    ) -> Result<Self, AnnRecallError> {
        if candidate_policy_oid == fallback_policy_oid {
            return Err(AnnRecallError::PolicyIdentityCollision {
                first: RecallPolicyKind::Candidate,
                second: RecallPolicyKind::PinnedFallback,
            });
        }
        if candidate_policy_oid == rebuild_policy_oid {
            return Err(AnnRecallError::PolicyIdentityCollision {
                first: RecallPolicyKind::Candidate,
                second: RecallPolicyKind::Rebuild,
            });
        }
        if fallback_policy_oid == rebuild_policy_oid {
            return Err(AnnRecallError::PolicyIdentityCollision {
                first: RecallPolicyKind::PinnedFallback,
                second: RecallPolicyKind::Rebuild,
            });
        }
        Ok(Self {
            monitor_oid,
            profile_oid,
            authorized_population_oid,
            snapshot_oid,
            authority_domain_oid,
            sample_key_oid,
            sample_design_oid,
            exact_baseline_oid,
            candidate_policy_oid,
            fallback_policy_oid,
            rebuild_policy_oid,
            window,
            regime_epoch,
        })
    }

    /// Registered recall-monitor identity.
    #[must_use]
    pub const fn monitor_oid(self) -> ObjectId {
        self.monitor_oid
    }

    /// Registered evaluation-profile identity.
    #[must_use]
    pub const fn profile_oid(self) -> ObjectId {
        self.profile_oid
    }

    /// Authorized query-population identity.
    #[must_use]
    pub const fn authorized_population_oid(self) -> ObjectId {
        self.authorized_population_oid
    }

    /// Pinned graph/index snapshot identity.
    #[must_use]
    pub const fn snapshot_oid(self) -> ObjectId {
        self.snapshot_oid
    }

    /// Pinned authority-domain identity.
    #[must_use]
    pub const fn authority_domain_oid(self) -> ObjectId {
        self.authority_domain_oid
    }

    /// Secret/keyed sample identity, not raw key material.
    #[must_use]
    pub const fn sample_key_oid(self) -> ObjectId {
        self.sample_key_oid
    }

    /// Registered query-sample design.
    #[must_use]
    pub const fn sample_design_oid(self) -> ObjectId {
        self.sample_design_oid
    }

    /// Registered exact-baseline implementation/profile.
    #[must_use]
    pub const fn exact_baseline_oid(self) -> ObjectId {
        self.exact_baseline_oid
    }

    /// Candidate ANN policy under evaluation.
    #[must_use]
    pub const fn candidate_policy_oid(self) -> ObjectId {
        self.candidate_policy_oid
    }

    /// Pinned deterministic exact fallback.
    #[must_use]
    pub const fn fallback_policy_oid(self) -> ObjectId {
        self.fallback_policy_oid
    }

    /// Pinned rebuild action.
    #[must_use]
    pub const fn rebuild_policy_oid(self) -> ObjectId {
        self.rebuild_policy_oid
    }

    /// Complete fixed stream window.
    #[must_use]
    pub const fn window(self) -> AnnRecallWindow {
        self.window
    }

    /// Regime epoch under which this evidence is valid.
    #[must_use]
    pub const fn regime_epoch(self) -> u64 {
        self.regime_epoch
    }
}

/// Immutable resource and statistical gates for one recall ledger.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AnnRecallProfile {
    profile_oid: ObjectId,
    top_k: usize,
    maximum_queries: usize,
    maximum_total_result_ids: usize,
    confidence_exponent: u8,
    candidate_recall_threshold_units: u64,
    rebuild_recall_threshold_units: u64,
    assumptions: AnnRecallAssumptions,
}

impl AnnRecallProfile {
    /// Constructs bounded resources, fixed-point recall gates, confidence
    /// level, and explicit assumptions.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new(
        profile_oid: ObjectId,
        top_k: usize,
        maximum_queries: usize,
        maximum_total_result_ids: usize,
        confidence_exponent: u8,
        candidate_recall_threshold_units: u64,
        rebuild_recall_threshold_units: u64,
        assumptions: AnnRecallAssumptions,
    ) -> Result<Self, AnnRecallError> {
        if top_k == 0 {
            return Err(AnnRecallError::ZeroTopK);
        }
        if top_k > MAX_RECALL_TOP_K {
            return Err(AnnRecallError::TopKTooLarge {
                actual: top_k,
                maximum: MAX_RECALL_TOP_K,
            });
        }
        if maximum_queries == 0 {
            return Err(AnnRecallError::ZeroQueryLimit);
        }
        if maximum_queries > MAX_RECALL_QUERIES {
            return Err(AnnRecallError::QueryLimitTooLarge {
                actual: maximum_queries,
                maximum: MAX_RECALL_QUERIES,
            });
        }
        if maximum_total_result_ids == 0 {
            return Err(AnnRecallError::ZeroResultIdLimit);
        }
        if maximum_total_result_ids > MAX_RECALL_RESULT_IDS {
            return Err(AnnRecallError::ResultIdLimitTooLarge {
                actual: maximum_total_result_ids,
                maximum: MAX_RECALL_RESULT_IDS,
            });
        }
        let minimum_one_query_ids = top_k
            .checked_mul(2)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        if maximum_total_result_ids < minimum_one_query_ids {
            return Err(AnnRecallError::ResultIdLimitCannotCoverOneQuery {
                required: minimum_one_query_ids,
                maximum: maximum_total_result_ids,
            });
        }
        if confidence_exponent == 0 || confidence_exponent > MAX_CONFIDENCE_EXPONENT {
            return Err(AnnRecallError::InvalidConfidenceExponent {
                actual: confidence_exponent,
                minimum: 1,
                maximum: MAX_CONFIDENCE_EXPONENT,
            });
        }
        if candidate_recall_threshold_units > RECALL_SCALE {
            return Err(AnnRecallError::RecallThresholdAboveOne {
                field: RecallThresholdKind::Candidate,
                actual: candidate_recall_threshold_units,
            });
        }
        if rebuild_recall_threshold_units > RECALL_SCALE {
            return Err(AnnRecallError::RecallThresholdAboveOne {
                field: RecallThresholdKind::Rebuild,
                actual: rebuild_recall_threshold_units,
            });
        }
        if rebuild_recall_threshold_units > candidate_recall_threshold_units {
            return Err(AnnRecallError::RebuildThresholdAboveCandidate {
                rebuild: rebuild_recall_threshold_units,
                candidate: candidate_recall_threshold_units,
            });
        }
        Ok(Self {
            profile_oid,
            top_k,
            maximum_queries,
            maximum_total_result_ids,
            confidence_exponent,
            candidate_recall_threshold_units,
            rebuild_recall_threshold_units,
            assumptions,
        })
    }

    /// Constructs and authoritatively binds an exact profile.
    #[allow(clippy::too_many_arguments)]
    pub fn try_new_verified<V: AnnRecallProfileIdentityVerifier + ?Sized>(
        profile_oid: ObjectId,
        top_k: usize,
        maximum_queries: usize,
        maximum_total_result_ids: usize,
        confidence_exponent: u8,
        candidate_recall_threshold_units: u64,
        rebuild_recall_threshold_units: u64,
        assumptions: AnnRecallAssumptions,
        verifier: &V,
    ) -> Result<Self, AnnRecallError> {
        let profile = Self::try_new(
            profile_oid,
            top_k,
            maximum_queries,
            maximum_total_result_ids,
            confidence_exponent,
            candidate_recall_threshold_units,
            rebuild_recall_threshold_units,
            assumptions,
        )?;
        profile.verify_identity(verifier)?;
        Ok(profile)
    }

    /// Emits the complete canonical descriptor an identity authority binds.
    #[allow(clippy::too_many_arguments)]
    pub fn try_canonical_descriptor_bytes(
        top_k: usize,
        maximum_queries: usize,
        maximum_total_result_ids: usize,
        confidence_exponent: u8,
        candidate_recall_threshold_units: u64,
        rebuild_recall_threshold_units: u64,
        assumptions: AnnRecallAssumptions,
    ) -> Result<Vec<u8>, AnnRecallError> {
        Self::try_new(
            ObjectId([0; OBJECT_ID_BYTES]),
            top_k,
            maximum_queries,
            maximum_total_result_ids,
            confidence_exponent,
            candidate_recall_threshold_units,
            rebuild_recall_threshold_units,
            assumptions,
        )?
        .try_canonical_bytes()
    }

    /// Returns the canonical descriptor covered by [`Self::profile_oid`].
    pub fn try_canonical_bytes(self) -> Result<Vec<u8>, AnnRecallError> {
        let top_k = u32::try_from(self.top_k).map_err(|_| AnnRecallError::ArithmeticOverflow)?;
        let maximum_queries =
            u32::try_from(self.maximum_queries).map_err(|_| AnnRecallError::ArithmeticOverflow)?;
        let maximum_total_result_ids = u32::try_from(self.maximum_total_result_ids)
            .map_err(|_| AnnRecallError::ArithmeticOverflow)?;
        let mut bytes = Vec::with_capacity(45);
        bytes.extend_from_slice(&PROFILE_DESCRIPTOR_MAGIC);
        push_u16(&mut bytes, PROFILE_DESCRIPTOR_VERSION);
        push_u32(&mut bytes, top_k);
        push_u32(&mut bytes, maximum_queries);
        push_u32(&mut bytes, maximum_total_result_ids);
        bytes.push(self.confidence_exponent);
        push_u64(&mut bytes, self.candidate_recall_threshold_units);
        push_u64(&mut bytes, self.rebuild_recall_threshold_units);
        bytes.push(self.assumptions.sample_design as u8);
        bytes.push(u8::from(self.assumptions.exact_baseline_complete));
        bytes.push(u8::from(self.assumptions.authorization_domain_fixed));
        bytes.push(u8::from(self.assumptions.candidate_policy_fixed));
        Ok(bytes)
    }

    fn verify_identity<V: AnnRecallProfileIdentityVerifier + ?Sized>(
        self,
        verifier: &V,
    ) -> Result<(), AnnRecallError> {
        let canonical_profile = self.try_canonical_bytes()?;
        if !verifier.verify_ann_recall_profile_oid(self.profile_oid, &canonical_profile) {
            return Err(AnnRecallError::ProfileIdentityUnverified {
                claimed: self.profile_oid,
            });
        }
        Ok(())
    }

    /// Registered identity for this exact profile.
    #[must_use]
    pub const fn profile_oid(self) -> ObjectId {
        self.profile_oid
    }

    /// Exact result-list length for every baseline and candidate observation.
    #[must_use]
    pub const fn top_k(self) -> usize {
        self.top_k
    }

    /// Maximum retained query observations.
    #[must_use]
    pub const fn maximum_queries(self) -> usize {
        self.maximum_queries
    }

    /// Maximum retained baseline plus candidate result identities.
    #[must_use]
    pub const fn maximum_total_result_ids(self) -> usize {
        self.maximum_total_result_ids
    }

    /// `q` in the declared failure probability `2^-q`.
    #[must_use]
    pub const fn confidence_exponent(self) -> u8 {
        self.confidence_exponent
    }

    /// Candidate gate numerator over [`RECALL_SCALE`].
    #[must_use]
    pub const fn candidate_recall_threshold_units(self) -> u64 {
        self.candidate_recall_threshold_units
    }

    /// Rebuild gate numerator over [`RECALL_SCALE`].
    #[must_use]
    pub const fn rebuild_recall_threshold_units(self) -> u64 {
        self.rebuild_recall_threshold_units
    }

    /// Disclosed interval assumptions.
    #[must_use]
    pub const fn assumptions(self) -> AnnRecallAssumptions {
        self.assumptions
    }
}

/// Observation-side identity binding checked against the immutable ledger.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AnnRecallBinding {
    monitor_oid: ObjectId,
    profile_oid: ObjectId,
    authorized_population_oid: ObjectId,
    snapshot_oid: ObjectId,
    authority_domain_oid: ObjectId,
    sample_key_oid: ObjectId,
    sample_design_oid: ObjectId,
    exact_baseline_oid: ObjectId,
    candidate_policy_oid: ObjectId,
    regime_epoch: u64,
}

impl AnnRecallBinding {
    /// Constructs the binding carried by one measured query.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub const fn new(
        monitor_oid: ObjectId,
        profile_oid: ObjectId,
        authorized_population_oid: ObjectId,
        snapshot_oid: ObjectId,
        authority_domain_oid: ObjectId,
        sample_key_oid: ObjectId,
        sample_design_oid: ObjectId,
        exact_baseline_oid: ObjectId,
        candidate_policy_oid: ObjectId,
        regime_epoch: u64,
    ) -> Self {
        Self {
            monitor_oid,
            profile_oid,
            authorized_population_oid,
            snapshot_oid,
            authority_domain_oid,
            sample_key_oid,
            sample_design_oid,
            exact_baseline_oid,
            candidate_policy_oid,
            regime_epoch,
        }
    }

    const fn from_identity(identity: AnnRecallIdentity) -> Self {
        Self {
            monitor_oid: identity.monitor_oid,
            profile_oid: identity.profile_oid,
            authorized_population_oid: identity.authorized_population_oid,
            snapshot_oid: identity.snapshot_oid,
            authority_domain_oid: identity.authority_domain_oid,
            sample_key_oid: identity.sample_key_oid,
            sample_design_oid: identity.sample_design_oid,
            exact_baseline_oid: identity.exact_baseline_oid,
            candidate_policy_oid: identity.candidate_policy_oid,
            regime_epoch: identity.regime_epoch,
        }
    }
}

/// Canonical result list being validated.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecallResultKind {
    /// Exact top-k baseline.
    ExactBaseline = 1,
    /// Candidate ANN top-k result.
    Candidate = 2,
}

/// One keyed query and its complete exact and candidate top-k results.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnRecallObservation {
    sequence: u64,
    sample_member_oid: ObjectId,
    query_oid: ObjectId,
    binding: AnnRecallBinding,
    exact_baseline_top_k: Vec<ObjectId>,
    candidate_top_k: Vec<ObjectId>,
}

impl AnnRecallObservation {
    /// Constructs an observation after requiring both result lists to be
    /// non-empty, sorted, unique, and within the absolute top-k ceiling.
    pub fn try_new(
        sequence: u64,
        sample_member_oid: ObjectId,
        query_oid: ObjectId,
        binding: AnnRecallBinding,
        exact_baseline_top_k: Vec<ObjectId>,
        candidate_top_k: Vec<ObjectId>,
    ) -> Result<Self, AnnRecallError> {
        validate_result_list(
            sequence,
            RecallResultKind::ExactBaseline,
            &exact_baseline_top_k,
        )?;
        validate_result_list(sequence, RecallResultKind::Candidate, &candidate_top_k)?;
        Ok(Self {
            sequence,
            sample_member_oid,
            query_oid,
            binding,
            exact_baseline_top_k,
            candidate_top_k,
        })
    }

    /// Exact stream sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Stable keyed-sample member identity.
    #[must_use]
    pub const fn sample_member_oid(&self) -> ObjectId {
        self.sample_member_oid
    }

    /// Query payload identity.
    #[must_use]
    pub const fn query_oid(&self) -> ObjectId {
        self.query_oid
    }

    /// Full observation-side identity binding.
    #[must_use]
    pub const fn binding(&self) -> AnnRecallBinding {
        self.binding
    }

    /// Canonically sorted exact top-k result identities.
    #[must_use]
    pub fn exact_baseline_top_k(&self) -> &[ObjectId] {
        &self.exact_baseline_top_k
    }

    /// Canonically sorted candidate top-k result identities.
    #[must_use]
    pub fn candidate_top_k(&self) -> &[ObjectId] {
        &self.candidate_top_k
    }
}

/// Exact cumulative recall ratio.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ExactRecall {
    intersection_hits: u64,
    baseline_results: u64,
}

impl ExactRecall {
    /// Number of candidate identities also present in their exact baselines.
    #[must_use]
    pub const fn intersection_hits(self) -> u64 {
        self.intersection_hits
    }

    /// Number of exact-baseline identities used as the recall denominator.
    #[must_use]
    pub const fn baseline_results(self) -> u64 {
        self.baseline_results
    }
}

/// Outward-rounded deterministic confidence interval.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RecallConfidenceInterval {
    scale: u64,
    point_estimate_units: u64,
    lower_units: u64,
    upper_units: u64,
    radius_units: u64,
    failure_probability_power_of_two_exponent: u8,
    query_observations: u64,
}

impl RecallConfidenceInterval {
    /// Fixed denominator shared by every interval component.
    #[must_use]
    pub const fn scale(self) -> u64 {
        self.scale
    }

    /// Floor-rounded empirical recall numerator over [`RECALL_SCALE`].
    #[must_use]
    pub const fn point_estimate_units(self) -> u64 {
        self.point_estimate_units
    }

    /// Conservative lower endpoint numerator over [`RECALL_SCALE`].
    #[must_use]
    pub const fn lower_units(self) -> u64 {
        self.lower_units
    }

    /// Conservative upper endpoint numerator over [`RECALL_SCALE`].
    #[must_use]
    pub const fn upper_units(self) -> u64 {
        self.upper_units
    }

    /// Outward-rounded Hoeffding radius numerator over [`RECALL_SCALE`].
    #[must_use]
    pub const fn radius_units(self) -> u64 {
        self.radius_units
    }

    /// `q` in the failure-probability upper bound `2^-q`.
    #[must_use]
    pub const fn failure_probability_power_of_two_exponent(self) -> u8 {
        self.failure_probability_power_of_two_exponent
    }

    /// Number of bounded per-query recall observations in the interval.
    #[must_use]
    pub const fn query_observations(self) -> u64 {
        self.query_observations
    }
}

/// Closed action emitted by recall evidence.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AnnRecallAction {
    /// Run the evaluated candidate ANN policy.
    Candidate = 1,
    /// Run the immutable pinned exact fallback.
    PinnedFallback = 2,
    /// Run the immutable rebuild policy.
    Rebuild = 3,
}

/// Deterministic explanation for the current decision state.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AnnRecallActionReason {
    /// The fixed sample window has not completed, so no action exists yet.
    IncompleteWindow = 1,
    /// One or more interval assumptions are unsupported.
    UnsupportedAssumptions = 2,
    /// The completed interval overlaps a decision boundary.
    StatisticallyInconclusive = 3,
    /// The completed lower bound meets the candidate recall gate.
    CandidateRecallSatisfied = 4,
    /// The upper bound is below the rebuild threshold after enough queries.
    RecallDriftDetected = 5,
}

/// Replayable `FG-CAL-03` report for the current ledger prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnnRecallEvidence {
    identity: AnnRecallIdentity,
    profile: AnnRecallProfile,
    query_observations: u64,
    exact_baseline_results: u64,
    candidate_results: u64,
    intersection_hits: u64,
    complete: bool,
    exact_recall: ExactRecall,
    confidence_interval: RecallConfidenceInterval,
    assumptions_supported: bool,
    action: Option<AnnRecallAction>,
    action_reason: AnnRecallActionReason,
}

impl AnnRecallEvidence {
    /// Complete immutable evidence identity.
    #[must_use]
    pub const fn identity(&self) -> AnnRecallIdentity {
        self.identity
    }

    /// Explicit resource, confidence, action, and assumption profile.
    #[must_use]
    pub const fn profile(&self) -> AnnRecallProfile {
        self.profile
    }

    /// Accepted keyed query count.
    #[must_use]
    pub const fn query_observations(&self) -> u64 {
        self.query_observations
    }

    /// Exact-baseline result identities compared.
    #[must_use]
    pub const fn exact_baseline_results(&self) -> u64 {
        self.exact_baseline_results
    }

    /// Candidate result identities compared.
    #[must_use]
    pub const fn candidate_results(&self) -> u64 {
        self.candidate_results
    }

    /// Candidate identities intersecting their corresponding exact baseline.
    #[must_use]
    pub const fn intersection_hits(&self) -> u64 {
        self.intersection_hits
    }

    /// Whether the fixed sequence window is complete.
    #[must_use]
    pub const fn complete(&self) -> bool {
        self.complete
    }

    /// Exact cumulative hit ratio.
    #[must_use]
    pub const fn exact_recall(&self) -> ExactRecall {
        self.exact_recall
    }

    /// Deterministic outward-rounded confidence interval.
    #[must_use]
    pub const fn confidence_interval(&self) -> RecallConfidenceInterval {
        self.confidence_interval
    }

    /// Whether every disclosed assumption is supported by this interval.
    #[must_use]
    pub const fn assumptions_supported(&self) -> bool {
        self.assumptions_supported
    }

    /// Complete disclosed sampling, baseline, authority, and policy
    /// assumptions.
    #[must_use]
    pub const fn assumptions(&self) -> AnnRecallAssumptions {
        self.profile.assumptions
    }

    /// Terminal candidate, fallback, or rebuild action.
    ///
    /// A decision exists only after the fixed sample window is complete.
    #[must_use]
    pub const fn action(&self) -> Option<AnnRecallAction> {
        self.action
    }

    /// Deterministic explanation for the pending or terminal decision state.
    #[must_use]
    pub const fn action_reason(&self) -> AnnRecallActionReason {
        self.action_reason
    }

    /// Policy selected by the terminal action, if the window is complete.
    #[must_use]
    pub const fn selected_policy_oid(&self) -> Option<ObjectId> {
        match self.action {
            Some(AnnRecallAction::Candidate) => Some(self.identity.candidate_policy_oid),
            Some(AnnRecallAction::PinnedFallback) => Some(self.identity.fallback_policy_oid),
            Some(AnnRecallAction::Rebuild) => Some(self.identity.rebuild_policy_oid),
            None => None,
        }
    }
}

/// A bounded recall ledger. Mutable state deliberately does not implement
/// `Clone`, preventing accidental branch-and-append histories.
#[derive(Debug)]
pub struct AnnRecallLedger {
    identity: AnnRecallIdentity,
    profile: AnnRecallProfile,
    observations: Vec<AnnRecallObservation>,
    sample_members: BTreeSet<ObjectId>,
    total_result_ids: usize,
    exact_baseline_results: u64,
    candidate_results: u64,
    intersection_hits: u64,
}

impl AnnRecallLedger {
    /// Constructs an empty ledger after authoritatively binding the complete
    /// canonical profile descriptor to its claimed OID.
    pub fn try_new_verified<V: AnnRecallProfileIdentityVerifier + ?Sized>(
        identity: AnnRecallIdentity,
        profile: AnnRecallProfile,
        verifier: &V,
    ) -> Result<Self, AnnRecallError> {
        profile.verify_identity(verifier)?;
        Self::try_new(identity, profile)
    }

    /// Constructs an empty ledger after validating profile identity and the
    /// complete window's resource envelope.
    pub(crate) fn try_new(
        identity: AnnRecallIdentity,
        profile: AnnRecallProfile,
    ) -> Result<Self, AnnRecallError> {
        if identity.profile_oid != profile.profile_oid {
            return Err(AnnRecallError::ProfileIdentityMismatch {
                expected: identity.profile_oid,
                actual: profile.profile_oid,
            });
        }
        let window_queries = usize::try_from(identity.window.query_capacity).map_err(|_| {
            AnnRecallError::WindowQueryCountUnrepresentable {
                actual: identity.window.query_capacity,
            }
        })?;
        if window_queries > profile.maximum_queries {
            return Err(AnnRecallError::WindowExceedsQueryLimit {
                window_queries,
                maximum: profile.maximum_queries,
            });
        }
        let result_ids_per_query = profile
            .top_k
            .checked_mul(2)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        let required_result_ids = window_queries
            .checked_mul(result_ids_per_query)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        if required_result_ids > profile.maximum_total_result_ids {
            return Err(AnnRecallError::ResultIdLimitCannotCoverWindow {
                required: required_result_ids,
                maximum: profile.maximum_total_result_ids,
            });
        }
        Ok(Self {
            identity,
            profile,
            observations: Vec::new(),
            sample_members: BTreeSet::new(),
            total_result_ids: 0,
            exact_baseline_results: 0,
            candidate_results: 0,
            intersection_hits: 0,
        })
    }

    /// Complete immutable ledger identity.
    #[must_use]
    pub const fn identity(&self) -> AnnRecallIdentity {
        self.identity
    }

    /// Explicit immutable profile.
    #[must_use]
    pub const fn profile(&self) -> AnnRecallProfile {
        self.profile
    }

    /// Accepted observations in exact sequence order.
    #[must_use]
    pub fn observations(&self) -> &[AnnRecallObservation] {
        &self.observations
    }

    /// Whether the fixed sequence window has been fully consumed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        u64::try_from(self.observations.len())
            .is_ok_and(|count| count == self.identity.window.query_capacity)
    }

    /// Accepts exactly the next query observation.
    ///
    /// All identity, sequence, sample uniqueness, list-length, resource,
    /// intersection, and arithmetic checks complete before retained state is
    /// changed. Any error therefore leaves the ledger byte-for-byte
    /// observationally unchanged.
    pub fn record(&mut self, observation: AnnRecallObservation) -> Result<(), AnnRecallError> {
        if self.is_complete() {
            return Err(AnnRecallError::WindowAlreadyComplete);
        }

        let accepted = u64::try_from(self.observations.len())
            .map_err(|_| AnnRecallError::ArithmeticOverflow)?;
        let expected_sequence = self
            .identity
            .window
            .first_sequence
            .checked_add(accepted)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        if observation.sequence != expected_sequence {
            return Err(AnnRecallError::UnexpectedSequence {
                expected: expected_sequence,
                actual: observation.sequence,
            });
        }
        if observation.sequence > self.identity.window.last_sequence {
            return Err(AnnRecallError::SequenceOutsideWindow {
                first: self.identity.window.first_sequence,
                last: self.identity.window.last_sequence,
                actual: observation.sequence,
            });
        }

        validate_binding(self.identity, observation.binding)?;

        if self.sample_members.contains(&observation.sample_member_oid) {
            return Err(AnnRecallError::DuplicateSampleMember {
                sample_member_oid: observation.sample_member_oid,
            });
        }

        let baseline_len = observation.exact_baseline_top_k.len();
        if baseline_len != self.profile.top_k {
            return Err(AnnRecallError::UnexpectedTopKLength {
                sequence: observation.sequence,
                kind: RecallResultKind::ExactBaseline,
                expected: self.profile.top_k,
                actual: baseline_len,
            });
        }
        let candidate_len = observation.candidate_top_k.len();
        if candidate_len != self.profile.top_k {
            return Err(AnnRecallError::UnexpectedTopKLength {
                sequence: observation.sequence,
                kind: RecallResultKind::Candidate,
                expected: self.profile.top_k,
                actual: candidate_len,
            });
        }

        let incoming_result_ids = baseline_len
            .checked_add(candidate_len)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        let new_total_result_ids = self
            .total_result_ids
            .checked_add(incoming_result_ids)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        if new_total_result_ids > self.profile.maximum_total_result_ids {
            return Err(AnnRecallError::ResultIdLimitExceeded {
                current: self.total_result_ids,
                incoming: incoming_result_ids,
                maximum: self.profile.maximum_total_result_ids,
            });
        }

        let hits = sorted_intersection_count(
            &observation.exact_baseline_top_k,
            &observation.candidate_top_k,
        )?;
        let baseline_increment =
            u64::try_from(baseline_len).map_err(|_| AnnRecallError::ArithmeticOverflow)?;
        let candidate_increment =
            u64::try_from(candidate_len).map_err(|_| AnnRecallError::ArithmeticOverflow)?;
        let new_exact_baseline_results = self
            .exact_baseline_results
            .checked_add(baseline_increment)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        let new_candidate_results = self
            .candidate_results
            .checked_add(candidate_increment)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        let new_intersection_hits = self
            .intersection_hits
            .checked_add(hits)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;

        self.observations
            .try_reserve(1)
            .map_err(|_| AnnRecallError::HistoryAllocationFailed)?;

        if !self.sample_members.insert(observation.sample_member_oid) {
            return Err(AnnRecallError::DuplicateSampleMember {
                sample_member_oid: observation.sample_member_oid,
            });
        }
        self.total_result_ids = new_total_result_ids;
        self.exact_baseline_results = new_exact_baseline_results;
        self.candidate_results = new_candidate_results;
        self.intersection_hits = new_intersection_hits;
        self.observations.push(observation);
        Ok(())
    }

    /// Produces deterministic evidence for the accepted prefix.
    pub fn evidence(&self) -> Result<AnnRecallEvidence, AnnRecallError> {
        let query_observations = u64::try_from(self.observations.len())
            .map_err(|_| AnnRecallError::ArithmeticOverflow)?;
        let confidence_interval = confidence_interval(
            self.intersection_hits,
            self.exact_baseline_results,
            query_observations,
            self.profile.confidence_exponent,
        )?;
        let assumptions_supported = self.profile.assumptions.supported();
        let complete = self.is_complete();

        let (action, action_reason) = if !complete {
            (None, AnnRecallActionReason::IncompleteWindow)
        } else if !assumptions_supported {
            (
                Some(AnnRecallAction::PinnedFallback),
                AnnRecallActionReason::UnsupportedAssumptions,
            )
        } else if confidence_interval.upper_units < self.profile.rebuild_recall_threshold_units {
            (
                Some(AnnRecallAction::Rebuild),
                AnnRecallActionReason::RecallDriftDetected,
            )
        } else if confidence_interval.lower_units >= self.profile.candidate_recall_threshold_units {
            (
                Some(AnnRecallAction::Candidate),
                AnnRecallActionReason::CandidateRecallSatisfied,
            )
        } else {
            (
                Some(AnnRecallAction::PinnedFallback),
                AnnRecallActionReason::StatisticallyInconclusive,
            )
        };

        Ok(AnnRecallEvidence {
            identity: self.identity,
            profile: self.profile,
            query_observations,
            exact_baseline_results: self.exact_baseline_results,
            candidate_results: self.candidate_results,
            intersection_hits: self.intersection_hits,
            complete,
            exact_recall: ExactRecall {
                intersection_hits: self.intersection_hits,
                baseline_results: self.exact_baseline_results,
            },
            confidence_interval,
            assumptions_supported,
            action,
            action_reason,
        })
    }

    /// Encodes the immutable trial configuration and every accepted query
    /// input into one self-contained canonical replay stream.
    ///
    /// Derived counters and evidence are intentionally absent: decoding
    /// reconstructs them through [`Self::record`] so replay exercises the
    /// production validation and decision path.
    pub fn to_canonical_replay_bytes(&self) -> Result<Vec<u8>, AnnRecallReplayCodecError> {
        let record_count = self.observations.len();
        let encoded_len = replay_encoded_len(record_count, self.profile.top_k)?;
        let top_k = u32::try_from(self.profile.top_k)
            .map_err(|_| AnnRecallReplayCodecError::IntegerWidth)?;
        let maximum_queries = u32::try_from(self.profile.maximum_queries)
            .map_err(|_| AnnRecallReplayCodecError::IntegerWidth)?;
        let maximum_total_result_ids = u32::try_from(self.profile.maximum_total_result_ids)
            .map_err(|_| AnnRecallReplayCodecError::IntegerWidth)?;
        let encoded_record_count =
            u32::try_from(record_count).map_err(|_| AnnRecallReplayCodecError::IntegerWidth)?;

        let mut bytes = Vec::new();
        bytes.try_reserve_exact(encoded_len).map_err(|_| {
            AnnRecallReplayCodecError::AllocationFailed {
                requested_elements: encoded_len,
            }
        })?;
        bytes.extend_from_slice(&REPLAY_MAGIC);
        push_u16(&mut bytes, ANN_RECALL_REPLAY_VERSION);
        push_u16(&mut bytes, REPLAY_RESERVED);
        encode_identity(&mut bytes, self.identity);
        encode_profile(
            &mut bytes,
            self.profile,
            top_k,
            maximum_queries,
            maximum_total_result_ids,
        );
        bytes.extend_from_slice(&REPLAY_PROFILE_RESERVED);
        push_u32(&mut bytes, encoded_record_count);
        debug_assert_eq!(bytes.len(), REPLAY_HEADER_BYTES);

        for observation in &self.observations {
            push_u64(&mut bytes, observation.sequence);
            push_oid(&mut bytes, observation.sample_member_oid);
            push_oid(&mut bytes, observation.query_oid);
            for oid in &observation.exact_baseline_top_k {
                push_oid(&mut bytes, *oid);
            }
            for oid in &observation.candidate_top_k {
                push_oid(&mut bytes, *oid);
            }
        }
        debug_assert_eq!(bytes.len(), encoded_len);
        Ok(bytes)
    }

    /// Reconstructs a ledger from its canonical input stream.
    ///
    /// The decoded identity and profile must exactly equal caller-owned
    /// trusted values. All encoded observations are then accepted through the
    /// normal ledger path rather than restoring derived counters directly.
    pub fn from_canonical_replay_bytes<V: AnnRecallProfileIdentityVerifier + ?Sized>(
        bytes: &[u8],
        limits: AnnRecallReplayDecodeLimits,
        expected_identity: AnnRecallIdentity,
        expected_profile: AnnRecallProfile,
        verifier: &V,
    ) -> Result<Self, AnnRecallReplayCodecError> {
        if bytes.len() > limits.max_encoded_bytes {
            return Err(AnnRecallReplayCodecError::EncodedBytesLimit {
                actual: bytes.len(),
                maximum: limits.max_encoded_bytes,
            });
        }
        if bytes.len() < REPLAY_HEADER_BYTES {
            return Err(AnnRecallReplayCodecError::Truncated {
                needed: REPLAY_HEADER_BYTES,
                actual: bytes.len(),
            });
        }

        let mut reader = AnnRecallReplayReader::new(bytes);
        if reader.read_exact(REPLAY_MAGIC.len())? != REPLAY_MAGIC {
            return Err(AnnRecallReplayCodecError::Magic);
        }
        let version = reader.read_u16()?;
        if version != ANN_RECALL_REPLAY_VERSION {
            return Err(AnnRecallReplayCodecError::Version { version });
        }
        let reserved = reader.read_u16()?;
        if reserved != REPLAY_RESERVED {
            return Err(AnnRecallReplayCodecError::Reserved { reserved });
        }

        let identity = decode_identity(&mut reader)?;
        let profile = decode_profile(&mut reader)?;
        let profile_reserved = reader.read_exact(REPLAY_PROFILE_RESERVED.len())?;
        if profile_reserved != REPLAY_PROFILE_RESERVED {
            return Err(AnnRecallReplayCodecError::ProfileReserved);
        }
        let record_count = usize::try_from(reader.read_u32()?)
            .map_err(|_| AnnRecallReplayCodecError::IntegerWidth)?;
        debug_assert_eq!(reader.position(), REPLAY_HEADER_BYTES);

        if identity != expected_identity {
            return Err(AnnRecallReplayCodecError::IdentityMismatch);
        }
        if profile != expected_profile {
            return Err(AnnRecallReplayCodecError::ProfileMismatch);
        }
        expected_profile
            .verify_identity(verifier)
            .map_err(AnnRecallReplayCodecError::Ledger)?;
        validate_replay_limits(profile, record_count, limits)?;

        let expected_len = replay_encoded_len(record_count, profile.top_k)?;
        if bytes.len() != expected_len {
            return Err(AnnRecallReplayCodecError::Length {
                expected: expected_len,
                actual: bytes.len(),
            });
        }

        let binding = AnnRecallBinding::from_identity(identity);
        let mut ledger =
            Self::try_new(identity, profile).map_err(AnnRecallReplayCodecError::Ledger)?;
        for _ in 0..record_count {
            let sequence = reader.read_u64()?;
            let sample_member_oid = reader.read_oid()?;
            let query_oid = reader.read_oid()?;
            let exact_baseline_top_k = reader.read_oid_vec(profile.top_k)?;
            let candidate_top_k = reader.read_oid_vec(profile.top_k)?;
            let observation = AnnRecallObservation::try_new(
                sequence,
                sample_member_oid,
                query_oid,
                binding,
                exact_baseline_top_k,
                candidate_top_k,
            )
            .map_err(AnnRecallReplayCodecError::Ledger)?;
            ledger
                .record(observation)
                .map_err(AnnRecallReplayCodecError::Ledger)?;
        }
        debug_assert_eq!(reader.position(), bytes.len());
        Ok(ledger)
    }
}

/// Identity component checked while accepting an observation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecallBindingField {
    /// Monitor implementation/configuration.
    Monitor,
    /// Statistical/resource profile.
    Profile,
    /// Authorized query population.
    AuthorizedPopulation,
    /// Pinned snapshot.
    Snapshot,
    /// Pinned authority domain.
    AuthorityDomain,
    /// Keyed sample.
    SampleKey,
    /// Sample design.
    SampleDesign,
    /// Exact baseline.
    ExactBaseline,
    /// Candidate policy.
    CandidatePolicy,
}

/// One of the three closed policies.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecallPolicyKind {
    /// Candidate ANN policy.
    Candidate,
    /// Pinned exact fallback.
    PinnedFallback,
    /// Rebuild action.
    Rebuild,
}

/// Fixed-point threshold being validated.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum RecallThresholdKind {
    /// Candidate-selection threshold.
    Candidate,
    /// Rebuild threshold.
    Rebuild,
}

/// Construction, validation, resource, and arithmetic failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnnRecallError {
    /// Inclusive sequence bounds were reversed.
    ReversedWindow {
        /// Inclusive first sequence.
        first: u64,
        /// Inclusive last sequence.
        last: u64,
    },
    /// Inclusive window length overflowed.
    WindowLengthOverflow {
        /// Inclusive first sequence.
        first: u64,
        /// Inclusive last sequence.
        last: u64,
    },
    /// Two closed policy roles used the same identity.
    PolicyIdentityCollision {
        /// First role.
        first: RecallPolicyKind,
        /// Second role.
        second: RecallPolicyKind,
    },
    /// Top-k was zero.
    ZeroTopK,
    /// Top-k exceeded the absolute ceiling.
    TopKTooLarge {
        /// Requested top-k.
        actual: usize,
        /// Absolute maximum.
        maximum: usize,
    },
    /// Maximum retained query count was zero.
    ZeroQueryLimit,
    /// Query limit exceeded the absolute ceiling.
    QueryLimitTooLarge {
        /// Requested limit.
        actual: usize,
        /// Absolute maximum.
        maximum: usize,
    },
    /// Result-identity limit was zero.
    ZeroResultIdLimit,
    /// Result-identity limit exceeded the absolute ceiling.
    ResultIdLimitTooLarge {
        /// Requested limit.
        actual: usize,
        /// Absolute maximum.
        maximum: usize,
    },
    /// Result limit cannot hold one complete baseline/candidate pair.
    ResultIdLimitCannotCoverOneQuery {
        /// Identities required.
        required: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Confidence exponent was outside the supported range.
    InvalidConfidenceExponent {
        /// Requested exponent.
        actual: u8,
        /// Inclusive minimum.
        minimum: u8,
        /// Inclusive maximum.
        maximum: u8,
    },
    /// A recall threshold exceeded one.
    RecallThresholdAboveOne {
        /// Threshold role.
        field: RecallThresholdKind,
        /// Requested fixed-point numerator.
        actual: u64,
    },
    /// Rebuild threshold exceeded the candidate threshold.
    RebuildThresholdAboveCandidate {
        /// Rebuild threshold.
        rebuild: u64,
        /// Candidate threshold.
        candidate: u64,
    },
    /// Profile OID disagreed with the immutable identity.
    ProfileIdentityMismatch {
        /// Identity-bound profile.
        expected: ObjectId,
        /// Supplied profile.
        actual: ObjectId,
    },
    /// The profile authority rejected the claimed OID for the complete
    /// canonical descriptor.
    ProfileIdentityUnverified {
        /// Rejected profile identity.
        claimed: ObjectId,
    },
    /// Window query count was not representable on this platform.
    WindowQueryCountUnrepresentable {
        /// Requested query count.
        actual: u64,
    },
    /// Complete window exceeded the query limit.
    WindowExceedsQueryLimit {
        /// Window query count.
        window_queries: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Result limit cannot hold the complete window.
    ResultIdLimitCannotCoverWindow {
        /// Identities required.
        required: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// A result list was empty.
    EmptyResultList {
        /// Source sequence.
        sequence: u64,
        /// Result-list role.
        kind: RecallResultKind,
    },
    /// A result list exceeded the absolute top-k ceiling.
    ResultListTooLarge {
        /// Source sequence.
        sequence: u64,
        /// Result-list role.
        kind: RecallResultKind,
        /// Actual length.
        actual: usize,
        /// Absolute maximum.
        maximum: usize,
    },
    /// A result identity was duplicated.
    DuplicateResult {
        /// Source sequence.
        sequence: u64,
        /// Result-list role.
        kind: RecallResultKind,
        /// Duplicate index.
        index: usize,
        /// Duplicated identity.
        object_oid: ObjectId,
    },
    /// A result list was not in canonical ascending identity order.
    ResultsOutOfOrder {
        /// Source sequence.
        sequence: u64,
        /// Result-list role.
        kind: RecallResultKind,
        /// First invalid index.
        index: usize,
        /// Previous identity.
        previous: ObjectId,
        /// Current identity.
        current: ObjectId,
    },
    /// The ledger's fixed window was already complete.
    WindowAlreadyComplete,
    /// Observation did not carry the exact next sequence.
    UnexpectedSequence {
        /// Required sequence.
        expected: u64,
        /// Supplied sequence.
        actual: u64,
    },
    /// Observation sequence exceeded the declared window.
    SequenceOutsideWindow {
        /// Inclusive first sequence.
        first: u64,
        /// Inclusive last sequence.
        last: u64,
        /// Supplied sequence.
        actual: u64,
    },
    /// Observation identity disagreed with the ledger identity.
    BindingMismatch {
        /// Mismatched component.
        field: RecallBindingField,
        /// Immutable ledger value.
        expected: ObjectId,
        /// Observation value.
        actual: ObjectId,
    },
    /// Observation regime disagreed with the immutable regime.
    RegimeEpochMismatch {
        /// Immutable ledger epoch.
        expected: u64,
        /// Observation epoch.
        actual: u64,
    },
    /// A keyed sample member appeared more than once.
    DuplicateSampleMember {
        /// Repeated sample member.
        sample_member_oid: ObjectId,
    },
    /// Result list length differed from the exact configured top-k.
    UnexpectedTopKLength {
        /// Source sequence.
        sequence: u64,
        /// Result-list role.
        kind: RecallResultKind,
        /// Required length.
        expected: usize,
        /// Supplied length.
        actual: usize,
    },
    /// Accepting an observation would exceed the result-identity limit.
    ResultIdLimitExceeded {
        /// Currently retained identities.
        current: usize,
        /// Incoming identities.
        incoming: usize,
        /// Configured maximum.
        maximum: usize,
    },
    /// Space for one history entry could not be reserved.
    HistoryAllocationFailed,
    /// Checked arithmetic could not represent the exact result.
    ArithmeticOverflow,
}

impl fmt::Display for AnnRecallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for AnnRecallError {}

/// Strict canonical replay-stream failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnnRecallReplayCodecError {
    /// The complete input exceeded the caller-owned byte budget.
    EncodedBytesLimit { actual: usize, maximum: usize },
    /// The input ended before a required field.
    Truncated { needed: usize, actual: usize },
    /// The stream magic was not canonical.
    Magic,
    /// The stream version is unsupported.
    Version { version: u16 },
    /// A reserved header field was nonzero.
    Reserved { reserved: u16 },
    /// Reserved profile bytes were nonzero.
    ProfileReserved,
    /// An enum tag was outside the closed sample-design vocabulary.
    UnknownSampleDesign { tag: u8 },
    /// A boolean field was not canonically encoded as zero or one.
    NonCanonicalBoolean { field: &'static str, value: u8 },
    /// A platform or durable integer width could not represent a value.
    IntegerWidth,
    /// Length or inventory arithmetic overflowed.
    ArithmeticOverflow,
    /// The encoded top-k exceeded caller-owned admission.
    TopKLimit { actual: usize, maximum: usize },
    /// The encoded query profile or count exceeded caller-owned admission.
    QueryLimit {
        profile_maximum: usize,
        record_count: usize,
        caller_maximum: usize,
    },
    /// The encoded result inventory exceeded caller-owned admission.
    ResultIdLimit {
        profile_maximum: usize,
        concrete_inventory: usize,
        caller_maximum: usize,
    },
    /// The concrete observation count exceeded the encoded profile.
    ObservationCountExceedsProfile {
        record_count: usize,
        profile_maximum: usize,
    },
    /// Exact framing length disagreed with the declared inventory.
    Length { expected: usize, actual: usize },
    /// The decoded immutable trial identity disagreed with trusted state.
    IdentityMismatch,
    /// The decoded immutable resource/statistical profile disagreed with
    /// trusted state.
    ProfileMismatch,
    /// Admitted storage could not be reserved.
    AllocationFailed { requested_elements: usize },
    /// Reconstructing the ledger rejected a decoded domain value.
    Ledger(AnnRecallError),
}

impl fmt::Display for AnnRecallReplayCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for AnnRecallReplayCodecError {}

impl From<AnnRecallError> for AnnRecallReplayCodecError {
    fn from(error: AnnRecallError) -> Self {
        Self::Ledger(error)
    }
}

fn encode_identity(bytes: &mut Vec<u8>, identity: AnnRecallIdentity) {
    push_oid(bytes, identity.monitor_oid);
    push_oid(bytes, identity.profile_oid);
    push_oid(bytes, identity.authorized_population_oid);
    push_oid(bytes, identity.snapshot_oid);
    push_oid(bytes, identity.authority_domain_oid);
    push_oid(bytes, identity.sample_key_oid);
    push_oid(bytes, identity.sample_design_oid);
    push_oid(bytes, identity.exact_baseline_oid);
    push_oid(bytes, identity.candidate_policy_oid);
    push_oid(bytes, identity.fallback_policy_oid);
    push_oid(bytes, identity.rebuild_policy_oid);
    push_u64(bytes, identity.window.first_sequence);
    push_u64(bytes, identity.window.last_sequence);
    push_u64(bytes, identity.regime_epoch);
}

fn decode_identity(
    reader: &mut AnnRecallReplayReader<'_>,
) -> Result<AnnRecallIdentity, AnnRecallReplayCodecError> {
    let monitor_oid = reader.read_oid()?;
    let profile_oid = reader.read_oid()?;
    let authorized_population_oid = reader.read_oid()?;
    let snapshot_oid = reader.read_oid()?;
    let authority_domain_oid = reader.read_oid()?;
    let sample_key_oid = reader.read_oid()?;
    let sample_design_oid = reader.read_oid()?;
    let exact_baseline_oid = reader.read_oid()?;
    let candidate_policy_oid = reader.read_oid()?;
    let fallback_policy_oid = reader.read_oid()?;
    let rebuild_policy_oid = reader.read_oid()?;
    let first_sequence = reader.read_u64()?;
    let last_sequence = reader.read_u64()?;
    let regime_epoch = reader.read_u64()?;
    let window = AnnRecallWindow::try_new(first_sequence, last_sequence)
        .map_err(AnnRecallReplayCodecError::Ledger)?;
    AnnRecallIdentity::try_new(
        monitor_oid,
        profile_oid,
        authorized_population_oid,
        snapshot_oid,
        authority_domain_oid,
        sample_key_oid,
        sample_design_oid,
        exact_baseline_oid,
        candidate_policy_oid,
        fallback_policy_oid,
        rebuild_policy_oid,
        window,
        regime_epoch,
    )
    .map_err(AnnRecallReplayCodecError::Ledger)
}

fn encode_profile(
    bytes: &mut Vec<u8>,
    profile: AnnRecallProfile,
    top_k: u32,
    maximum_queries: u32,
    maximum_total_result_ids: u32,
) {
    push_oid(bytes, profile.profile_oid);
    push_u32(bytes, top_k);
    push_u32(bytes, maximum_queries);
    push_u32(bytes, maximum_total_result_ids);
    bytes.push(profile.confidence_exponent);
    push_u64(bytes, profile.candidate_recall_threshold_units);
    push_u64(bytes, profile.rebuild_recall_threshold_units);
    bytes.push(profile.assumptions.sample_design as u8);
    bytes.push(u8::from(profile.assumptions.exact_baseline_complete));
    bytes.push(u8::from(profile.assumptions.authorization_domain_fixed));
    bytes.push(u8::from(profile.assumptions.candidate_policy_fixed));
}

fn decode_profile(
    reader: &mut AnnRecallReplayReader<'_>,
) -> Result<AnnRecallProfile, AnnRecallReplayCodecError> {
    let profile_oid = reader.read_oid()?;
    let top_k =
        usize::try_from(reader.read_u32()?).map_err(|_| AnnRecallReplayCodecError::IntegerWidth)?;
    let maximum_queries =
        usize::try_from(reader.read_u32()?).map_err(|_| AnnRecallReplayCodecError::IntegerWidth)?;
    let maximum_total_result_ids =
        usize::try_from(reader.read_u32()?).map_err(|_| AnnRecallReplayCodecError::IntegerWidth)?;
    let confidence_exponent = reader.read_u8()?;
    let candidate_recall_threshold_units = reader.read_u64()?;
    let rebuild_recall_threshold_units = reader.read_u64()?;
    let sample_design = QuerySampleDesign::try_from_tag(reader.read_u8()?)?;
    let exact_baseline_complete = reader.read_bool("exact_baseline_complete")?;
    let authorization_domain_fixed = reader.read_bool("authorization_domain_fixed")?;
    let candidate_policy_fixed = reader.read_bool("candidate_policy_fixed")?;
    let assumptions = AnnRecallAssumptions::new(
        sample_design,
        exact_baseline_complete,
        authorization_domain_fixed,
        candidate_policy_fixed,
    );
    AnnRecallProfile::try_new(
        profile_oid,
        top_k,
        maximum_queries,
        maximum_total_result_ids,
        confidence_exponent,
        candidate_recall_threshold_units,
        rebuild_recall_threshold_units,
        assumptions,
    )
    .map_err(AnnRecallReplayCodecError::Ledger)
}

fn validate_replay_limits(
    profile: AnnRecallProfile,
    record_count: usize,
    limits: AnnRecallReplayDecodeLimits,
) -> Result<(), AnnRecallReplayCodecError> {
    if profile.top_k > limits.max_top_k {
        return Err(AnnRecallReplayCodecError::TopKLimit {
            actual: profile.top_k,
            maximum: limits.max_top_k,
        });
    }
    if profile.maximum_queries > limits.max_queries {
        return Err(AnnRecallReplayCodecError::QueryLimit {
            profile_maximum: profile.maximum_queries,
            record_count,
            caller_maximum: limits.max_queries,
        });
    }
    if record_count > profile.maximum_queries {
        return Err(AnnRecallReplayCodecError::ObservationCountExceedsProfile {
            record_count,
            profile_maximum: profile.maximum_queries,
        });
    }
    if record_count > limits.max_queries {
        return Err(AnnRecallReplayCodecError::QueryLimit {
            profile_maximum: profile.maximum_queries,
            record_count,
            caller_maximum: limits.max_queries,
        });
    }

    let concrete_inventory = record_count
        .checked_mul(profile.top_k)
        .and_then(|count| count.checked_mul(2))
        .ok_or(AnnRecallReplayCodecError::ArithmeticOverflow)?;
    if profile.maximum_total_result_ids > limits.max_total_result_ids
        || concrete_inventory > limits.max_total_result_ids
    {
        return Err(AnnRecallReplayCodecError::ResultIdLimit {
            profile_maximum: profile.maximum_total_result_ids,
            concrete_inventory,
            caller_maximum: limits.max_total_result_ids,
        });
    }
    if concrete_inventory > profile.maximum_total_result_ids {
        return Err(AnnRecallReplayCodecError::ResultIdLimit {
            profile_maximum: profile.maximum_total_result_ids,
            concrete_inventory,
            caller_maximum: limits.max_total_result_ids,
        });
    }
    Ok(())
}

fn replay_encoded_len(
    record_count: usize,
    top_k: usize,
) -> Result<usize, AnnRecallReplayCodecError> {
    let result_bytes = top_k
        .checked_mul(2)
        .and_then(|count| count.checked_mul(OBJECT_ID_BYTES))
        .ok_or(AnnRecallReplayCodecError::ArithmeticOverflow)?;
    let observation_bytes = REPLAY_OBSERVATION_FIXED_BYTES
        .checked_add(result_bytes)
        .ok_or(AnnRecallReplayCodecError::ArithmeticOverflow)?;
    record_count
        .checked_mul(observation_bytes)
        .and_then(|payload| REPLAY_HEADER_BYTES.checked_add(payload))
        .ok_or(AnnRecallReplayCodecError::ArithmeticOverflow)
}

fn push_oid(bytes: &mut Vec<u8>, oid: ObjectId) {
    bytes.extend_from_slice(oid.as_bytes());
}

fn push_u16(bytes: &mut Vec<u8>, value: u16) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

struct AnnRecallReplayReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> AnnRecallReplayReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    const fn position(&self) -> usize {
        self.offset
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], AnnRecallReplayCodecError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(AnnRecallReplayCodecError::ArithmeticOverflow)?;
        let value =
            self.bytes
                .get(self.offset..end)
                .ok_or(AnnRecallReplayCodecError::Truncated {
                    needed: end,
                    actual: self.bytes.len(),
                })?;
        self.offset = end;
        Ok(value)
    }

    fn read_u8(&mut self) -> Result<u8, AnnRecallReplayCodecError> {
        Ok(self.read_exact(1)?[0])
    }

    fn read_bool(&mut self, field: &'static str) -> Result<bool, AnnRecallReplayCodecError> {
        let value = self.read_u8()?;
        match value {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(AnnRecallReplayCodecError::NonCanonicalBoolean { field, value }),
        }
    }

    fn read_u16(&mut self) -> Result<u16, AnnRecallReplayCodecError> {
        let mut value = [0_u8; 2];
        value.copy_from_slice(self.read_exact(2)?);
        Ok(u16::from_le_bytes(value))
    }

    fn read_u32(&mut self) -> Result<u32, AnnRecallReplayCodecError> {
        let mut value = [0_u8; 4];
        value.copy_from_slice(self.read_exact(4)?);
        Ok(u32::from_le_bytes(value))
    }

    fn read_u64(&mut self) -> Result<u64, AnnRecallReplayCodecError> {
        let mut value = [0_u8; 8];
        value.copy_from_slice(self.read_exact(8)?);
        Ok(u64::from_le_bytes(value))
    }

    fn read_oid(&mut self) -> Result<ObjectId, AnnRecallReplayCodecError> {
        let mut value = [0_u8; OBJECT_ID_BYTES];
        value.copy_from_slice(self.read_exact(OBJECT_ID_BYTES)?);
        Ok(ObjectId(value))
    }

    fn read_oid_vec(&mut self, len: usize) -> Result<Vec<ObjectId>, AnnRecallReplayCodecError> {
        let mut values = Vec::new();
        values
            .try_reserve_exact(len)
            .map_err(|_| AnnRecallReplayCodecError::AllocationFailed {
                requested_elements: len,
            })?;
        for _ in 0..len {
            values.push(self.read_oid()?);
        }
        Ok(values)
    }
}

fn validate_result_list(
    sequence: u64,
    kind: RecallResultKind,
    results: &[ObjectId],
) -> Result<(), AnnRecallError> {
    if results.is_empty() {
        return Err(AnnRecallError::EmptyResultList { sequence, kind });
    }
    if results.len() > MAX_RECALL_TOP_K {
        return Err(AnnRecallError::ResultListTooLarge {
            sequence,
            kind,
            actual: results.len(),
            maximum: MAX_RECALL_TOP_K,
        });
    }
    for (offset, adjacent) in results.windows(2).enumerate() {
        let previous = adjacent[0];
        let current = adjacent[1];
        if previous == current {
            return Err(AnnRecallError::DuplicateResult {
                sequence,
                kind,
                index: offset + 1,
                object_oid: current,
            });
        }
        if previous > current {
            return Err(AnnRecallError::ResultsOutOfOrder {
                sequence,
                kind,
                index: offset + 1,
                previous,
                current,
            });
        }
    }
    Ok(())
}

fn validate_binding(
    identity: AnnRecallIdentity,
    binding: AnnRecallBinding,
) -> Result<(), AnnRecallError> {
    check_binding(
        RecallBindingField::Monitor,
        identity.monitor_oid,
        binding.monitor_oid,
    )?;
    check_binding(
        RecallBindingField::Profile,
        identity.profile_oid,
        binding.profile_oid,
    )?;
    check_binding(
        RecallBindingField::AuthorizedPopulation,
        identity.authorized_population_oid,
        binding.authorized_population_oid,
    )?;
    check_binding(
        RecallBindingField::Snapshot,
        identity.snapshot_oid,
        binding.snapshot_oid,
    )?;
    check_binding(
        RecallBindingField::AuthorityDomain,
        identity.authority_domain_oid,
        binding.authority_domain_oid,
    )?;
    check_binding(
        RecallBindingField::SampleKey,
        identity.sample_key_oid,
        binding.sample_key_oid,
    )?;
    check_binding(
        RecallBindingField::SampleDesign,
        identity.sample_design_oid,
        binding.sample_design_oid,
    )?;
    check_binding(
        RecallBindingField::ExactBaseline,
        identity.exact_baseline_oid,
        binding.exact_baseline_oid,
    )?;
    check_binding(
        RecallBindingField::CandidatePolicy,
        identity.candidate_policy_oid,
        binding.candidate_policy_oid,
    )?;
    if identity.regime_epoch != binding.regime_epoch {
        return Err(AnnRecallError::RegimeEpochMismatch {
            expected: identity.regime_epoch,
            actual: binding.regime_epoch,
        });
    }
    Ok(())
}

fn check_binding(
    field: RecallBindingField,
    expected: ObjectId,
    actual: ObjectId,
) -> Result<(), AnnRecallError> {
    if expected != actual {
        return Err(AnnRecallError::BindingMismatch {
            field,
            expected,
            actual,
        });
    }
    Ok(())
}

fn sorted_intersection_count(
    baseline: &[ObjectId],
    candidate: &[ObjectId],
) -> Result<u64, AnnRecallError> {
    let mut baseline_index = 0;
    let mut candidate_index = 0;
    let mut hits = 0_u64;
    while baseline_index < baseline.len() && candidate_index < candidate.len() {
        match baseline[baseline_index].cmp(&candidate[candidate_index]) {
            core::cmp::Ordering::Less => baseline_index += 1,
            core::cmp::Ordering::Greater => candidate_index += 1,
            core::cmp::Ordering::Equal => {
                hits = hits
                    .checked_add(1)
                    .ok_or(AnnRecallError::ArithmeticOverflow)?;
                baseline_index += 1;
                candidate_index += 1;
            }
        }
    }
    Ok(hits)
}

fn confidence_interval(
    hits: u64,
    baseline_results: u64,
    queries: u64,
    confidence_exponent: u8,
) -> Result<RecallConfidenceInterval, AnnRecallError> {
    if queries == 0 || baseline_results == 0 {
        return Ok(RecallConfidenceInterval {
            scale: RECALL_SCALE,
            point_estimate_units: 0,
            lower_units: 0,
            upper_units: RECALL_SCALE,
            radius_units: RECALL_SCALE,
            failure_probability_power_of_two_exponent: confidence_exponent,
            query_observations: queries,
        });
    }

    let scaled_hits = u128::from(hits)
        .checked_mul(u128::from(RECALL_SCALE))
        .ok_or(AnnRecallError::ArithmeticOverflow)?;
    let denominator = u128::from(baseline_results);
    let point_floor = scaled_hits / denominator;
    let point_ceil = ceil_div(scaled_hits, denominator)?;

    let radius_numerator = u128::from(confidence_exponent)
        .checked_add(1)
        .and_then(|factor| factor.checked_mul(u128::from(RECALL_SCALE)))
        .and_then(|value| value.checked_mul(u128::from(RECALL_SCALE)))
        .ok_or(AnnRecallError::ArithmeticOverflow)?;
    let radius_denominator = u128::from(queries)
        .checked_mul(2)
        .ok_or(AnnRecallError::ArithmeticOverflow)?;
    let squared_radius_ceiling = ceil_div(radius_numerator, radius_denominator)?;
    let raw_radius = ceil_sqrt(squared_radius_ceiling);
    let bounded_radius = raw_radius.min(u128::from(RECALL_SCALE));

    let lower = point_floor.saturating_sub(bounded_radius);
    let upper = point_ceil
        .checked_add(bounded_radius)
        .ok_or(AnnRecallError::ArithmeticOverflow)?
        .min(u128::from(RECALL_SCALE));

    Ok(RecallConfidenceInterval {
        scale: RECALL_SCALE,
        point_estimate_units: u64::try_from(point_floor)
            .map_err(|_| AnnRecallError::ArithmeticOverflow)?,
        lower_units: u64::try_from(lower).map_err(|_| AnnRecallError::ArithmeticOverflow)?,
        upper_units: u64::try_from(upper).map_err(|_| AnnRecallError::ArithmeticOverflow)?,
        radius_units: u64::try_from(bounded_radius)
            .map_err(|_| AnnRecallError::ArithmeticOverflow)?,
        failure_probability_power_of_two_exponent: confidence_exponent,
        query_observations: queries,
    })
}

fn ceil_div(numerator: u128, denominator: u128) -> Result<u128, AnnRecallError> {
    let adjusted = numerator
        .checked_add(
            denominator
                .checked_sub(1)
                .ok_or(AnnRecallError::ArithmeticOverflow)?,
        )
        .ok_or(AnnRecallError::ArithmeticOverflow)?;
    Ok(adjusted / denominator)
}

fn ceil_sqrt(value: u128) -> u128 {
    if value <= 1 {
        return value;
    }
    let mut low = 1_u128;
    let mut high = value.min(u128::from(u64::MAX) + 1);
    while low < high {
        let midpoint = low + (high - low) / 2;
        if midpoint > value / midpoint {
            high = midpoint;
        } else if midpoint * midpoint == value {
            return midpoint;
        } else {
            low = midpoint + 1;
        }
    }
    low
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(value: u8) -> ObjectId {
        ObjectId([value; 32])
    }

    struct TestProfileVerifier;

    impl AnnRecallProfileIdentityVerifier for TestProfileVerifier {
        fn verify_ann_recall_profile_oid(
            &self,
            claimed_oid: ObjectId,
            _canonical_profile: &[u8],
        ) -> bool {
            claimed_oid == oid(2)
        }
    }

    struct HashProfileVerifier;

    impl AnnRecallProfileIdentityVerifier for HashProfileVerifier {
        fn verify_ann_recall_profile_oid(
            &self,
            claimed_oid: ObjectId,
            canonical_profile: &[u8],
        ) -> bool {
            claimed_oid == ObjectId(asupersync::atp::object::compute_hash(canonical_profile))
        }
    }

    fn supported_assumptions() -> AnnRecallAssumptions {
        AnnRecallAssumptions::new(
            QuerySampleDesign::KeyedUniformWithoutReplacement,
            true,
            true,
            true,
        )
    }

    fn identity(query_count: u64) -> Result<AnnRecallIdentity, AnnRecallError> {
        let last = 99_u64
            .checked_add(query_count)
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        AnnRecallIdentity::try_new(
            oid(1),
            oid(2),
            oid(3),
            oid(4),
            oid(5),
            oid(6),
            oid(7),
            oid(8),
            oid(9),
            oid(10),
            oid(11),
            AnnRecallWindow::try_new(100, last)?,
            12,
        )
    }

    fn profile(
        query_count: usize,
        top_k: usize,
        candidate_threshold: u64,
        rebuild_threshold: u64,
        assumptions: AnnRecallAssumptions,
    ) -> Result<AnnRecallProfile, AnnRecallError> {
        let result_limit = query_count
            .checked_mul(top_k)
            .and_then(|value| value.checked_mul(2))
            .ok_or(AnnRecallError::ArithmeticOverflow)?;
        AnnRecallProfile::try_new(
            oid(2),
            top_k,
            query_count,
            result_limit,
            1,
            candidate_threshold,
            rebuild_threshold,
            assumptions,
        )
    }

    fn binding() -> AnnRecallBinding {
        AnnRecallBinding::new(
            oid(1),
            oid(2),
            oid(3),
            oid(4),
            oid(5),
            oid(6),
            oid(7),
            oid(8),
            oid(9),
            12,
        )
    }

    fn observation(
        sequence: u64,
        sample: u8,
        baseline: &[u8],
        candidate: &[u8],
    ) -> Result<AnnRecallObservation, AnnRecallError> {
        AnnRecallObservation::try_new(
            sequence,
            oid(sample),
            oid(sample.saturating_add(64)),
            binding(),
            baseline.iter().copied().map(oid).collect(),
            candidate.iter().copied().map(oid).collect(),
        )
    }

    #[test]
    fn computes_known_recall_from_result_intersections() -> Result<(), AnnRecallError> {
        let mut ledger =
            AnnRecallLedger::try_new(identity(2)?, profile(2, 4, 0, 0, supported_assumptions())?)?;
        ledger.record(observation(100, 20, &[1, 2, 3, 4], &[1, 2, 8, 9])?)?;
        ledger.record(observation(101, 21, &[5, 6, 7, 8], &[5, 6, 7, 9])?)?;

        let evidence = ledger.evidence()?;
        assert_eq!(evidence.query_observations(), 2);
        assert_eq!(evidence.exact_baseline_results(), 8);
        assert_eq!(evidence.candidate_results(), 8);
        assert_eq!(evidence.intersection_hits(), 5);
        assert_eq!(evidence.exact_recall().intersection_hits(), 5);
        assert_eq!(evidence.exact_recall().baseline_results(), 8);
        assert_eq!(
            evidence.confidence_interval().point_estimate_units(),
            625_000_000
        );
        assert_eq!(evidence.action(), Some(AnnRecallAction::Candidate));
        assert_eq!(evidence.selected_policy_oid(), Some(oid(9)));
        Ok(())
    }

    #[test]
    fn clear_drift_selects_rebuild_not_fallback() -> Result<(), AnnRecallError> {
        let mut ledger = AnnRecallLedger::try_new(
            identity(4)?,
            profile(4, 2, 900_000_000, 800_000_000, supported_assumptions())?,
        )?;
        for offset in 0_u8..4 {
            ledger.record(observation(
                100 + u64::from(offset),
                20 + offset,
                &[1, 2],
                &[8, 9],
            )?)?;
        }
        let evidence = ledger.evidence()?;
        assert_eq!(evidence.action(), Some(AnnRecallAction::Rebuild));
        assert_eq!(
            evidence.action_reason(),
            AnnRecallActionReason::RecallDriftDetected
        );
        assert_eq!(evidence.selected_policy_oid(), Some(oid(11)));
        Ok(())
    }

    #[test]
    fn tight_lower_bound_selects_candidate_policy() -> Result<(), AnnRecallError> {
        let query_count = 128_usize;
        let mut ledger = AnnRecallLedger::try_new(
            identity(u64::try_from(query_count).map_err(|_| AnnRecallError::ArithmeticOverflow)?)?,
            profile(
                query_count,
                1,
                900_000_000,
                800_000_000,
                supported_assumptions(),
            )?,
        )?;
        for offset in 0_u8..128 {
            ledger.record(observation(
                100 + u64::from(offset),
                offset.saturating_add(20),
                &[1],
                &[1],
            )?)?;
        }
        let evidence = ledger.evidence()?;
        assert!(
            evidence.confidence_interval().lower_units()
                >= evidence.profile().candidate_recall_threshold_units()
        );
        assert_eq!(evidence.action(), Some(AnnRecallAction::Candidate));
        assert_eq!(
            evidence.action_reason(),
            AnnRecallActionReason::CandidateRecallSatisfied
        );
        Ok(())
    }

    #[test]
    fn overlapping_interval_selects_distinct_inconclusive_fallback() -> Result<(), AnnRecallError> {
        let mut ledger = AnnRecallLedger::try_new(
            identity(2)?,
            profile(2, 2, 950_000_000, 100_000_000, supported_assumptions())?,
        )?;
        ledger.record(observation(100, 20, &[1, 2], &[1, 2])?)?;
        ledger.record(observation(101, 21, &[3, 4], &[3, 4])?)?;
        let evidence = ledger.evidence()?;
        assert_eq!(evidence.action(), Some(AnnRecallAction::PinnedFallback));
        assert_eq!(
            evidence.action_reason(),
            AnnRecallActionReason::StatisticallyInconclusive
        );
        assert_eq!(evidence.selected_policy_oid(), Some(oid(10)));
        Ok(())
    }

    #[test]
    fn unsupported_assumptions_fail_closed() -> Result<(), AnnRecallError> {
        let unsupported =
            AnnRecallAssumptions::new(QuerySampleDesign::UnspecifiedDependence, true, true, true);
        let mut ledger = AnnRecallLedger::try_new(identity(1)?, profile(1, 2, 0, 0, unsupported)?)?;
        ledger.record(observation(100, 20, &[1, 2], &[1, 2])?)?;
        let evidence = ledger.evidence()?;
        assert!(!evidence.assumptions_supported());
        assert_eq!(evidence.action(), Some(AnnRecallAction::PinnedFallback));
        assert_eq!(
            evidence.action_reason(),
            AnnRecallActionReason::UnsupportedAssumptions
        );
        Ok(())
    }

    #[test]
    fn duplicate_and_out_of_order_results_are_rejected() -> Result<(), AnnRecallError> {
        let duplicate = AnnRecallObservation::try_new(
            100,
            oid(20),
            oid(80),
            binding(),
            vec![oid(1), oid(1)],
            vec![oid(2), oid(3)],
        );
        assert!(matches!(
            duplicate,
            Err(AnnRecallError::DuplicateResult {
                kind: RecallResultKind::ExactBaseline,
                index: 1,
                ..
            })
        ));

        let out_of_order = AnnRecallObservation::try_new(
            100,
            oid(20),
            oid(80),
            binding(),
            vec![oid(1), oid(2)],
            vec![oid(4), oid(3)],
        );
        assert!(matches!(
            out_of_order,
            Err(AnnRecallError::ResultsOutOfOrder {
                kind: RecallResultKind::Candidate,
                index: 1,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn profile_and_window_bounds_are_enforced() -> Result<(), AnnRecallError> {
        assert!(matches!(
            AnnRecallProfile::try_new(
                oid(2),
                MAX_RECALL_TOP_K + 1,
                1,
                2,
                1,
                RECALL_SCALE,
                0,
                supported_assumptions(),
            ),
            Err(AnnRecallError::TopKTooLarge { .. })
        ));

        let too_small = AnnRecallProfile::try_new(
            oid(2),
            2,
            2,
            4,
            1,
            RECALL_SCALE,
            0,
            supported_assumptions(),
        )?;
        assert!(matches!(
            AnnRecallLedger::try_new(identity(2)?, too_small),
            Err(AnnRecallError::ResultIdLimitCannotCoverWindow { .. })
        ));
        Ok(())
    }

    #[test]
    fn profile_identity_is_bound_before_any_history_exists() -> Result<(), AnnRecallError> {
        let wrong_profile = AnnRecallProfile::try_new(
            oid(99),
            1,
            1,
            2,
            1,
            RECALL_SCALE,
            0,
            supported_assumptions(),
        )?;
        assert!(matches!(
            AnnRecallLedger::try_new(identity(1)?, wrong_profile),
            Err(AnnRecallError::ProfileIdentityMismatch {
                expected,
                actual
            }) if expected == oid(2) && actual == oid(99)
        ));
        Ok(())
    }

    #[test]
    fn profile_identity_covers_every_assumption_before_ledger_allocation()
    -> Result<(), AnnRecallError> {
        let assumptions = supported_assumptions();
        let descriptor = AnnRecallProfile::try_canonical_descriptor_bytes(
            2,
            1,
            4,
            1,
            RECALL_SCALE,
            0,
            assumptions,
        )?;
        let profile_oid = ObjectId(asupersync::atp::object::compute_hash(&descriptor));
        let profile = AnnRecallProfile::try_new_verified(
            profile_oid,
            2,
            1,
            4,
            1,
            RECALL_SCALE,
            0,
            assumptions,
            &HashProfileVerifier,
        )?;
        let mut trial_identity = identity(1)?;
        trial_identity.profile_oid = profile_oid;
        let _ = AnnRecallLedger::try_new_verified(trial_identity, profile, &HashProfileVerifier)?;

        let changed_assumptions = AnnRecallAssumptions::new(
            QuerySampleDesign::KeyedUniformWithoutReplacement,
            false,
            true,
            true,
        );
        let changed_descriptor = AnnRecallProfile::try_canonical_descriptor_bytes(
            2,
            1,
            4,
            1,
            RECALL_SCALE,
            0,
            changed_assumptions,
        )?;
        assert_ne!(descriptor, changed_descriptor);
        assert!(matches!(
            AnnRecallProfile::try_new_verified(
                profile_oid,
                2,
                1,
                4,
                1,
                RECALL_SCALE,
                0,
                changed_assumptions,
                &HashProfileVerifier,
            ),
            Err(AnnRecallError::ProfileIdentityUnverified { claimed })
                if claimed == profile_oid
        ));
        Ok(())
    }

    #[test]
    fn replaying_identical_inputs_is_bit_identical() -> Result<(), AnnRecallError> {
        let run = || -> Result<AnnRecallEvidence, AnnRecallError> {
            let mut ledger = AnnRecallLedger::try_new(
                identity(2)?,
                profile(2, 3, 0, 0, supported_assumptions())?,
            )?;
            ledger.record(observation(100, 20, &[1, 2, 3], &[1, 3, 8])?)?;
            ledger.record(observation(101, 21, &[4, 5, 6], &[4, 5, 9])?)?;
            ledger.evidence()
        };
        assert_eq!(run()?, run()?);
        Ok(())
    }

    #[test]
    fn canonical_replay_reconstructs_evidence_through_validated_inputs()
    -> Result<(), AnnRecallReplayCodecError> {
        let trial_identity = identity(2)?;
        let trial_profile = profile(2, 3, 0, 0, supported_assumptions())?;
        let mut ledger = AnnRecallLedger::try_new(trial_identity, trial_profile)?;
        ledger.record(observation(100, 20, &[1, 2, 3], &[1, 3, 8])?)?;
        ledger.record(observation(101, 21, &[4, 5, 6], &[4, 5, 9])?)?;
        let expected_evidence = ledger.evidence()?;

        let encoded = ledger.to_canonical_replay_bytes()?;
        let limits = AnnRecallReplayDecodeLimits::new(encoded.len(), 3, 2, 12);
        let replayed = AnnRecallLedger::from_canonical_replay_bytes(
            &encoded,
            limits,
            trial_identity,
            trial_profile,
            &TestProfileVerifier,
        )?;

        assert_eq!(replayed.evidence()?, expected_evidence);
        assert_eq!(replayed.observations(), ledger.observations());
        assert_eq!(replayed.to_canonical_replay_bytes()?, encoded);
        Ok(())
    }

    #[test]
    fn canonical_replay_requires_trusted_identity_profile_and_external_limits()
    -> Result<(), AnnRecallReplayCodecError> {
        let trial_identity = identity(2)?;
        let trial_profile = profile(2, 3, 0, 0, supported_assumptions())?;
        let mut ledger = AnnRecallLedger::try_new(trial_identity, trial_profile)?;
        ledger.record(observation(100, 20, &[1, 2, 3], &[1, 3, 8])?)?;
        let encoded = ledger.to_canonical_replay_bytes()?;
        let admitted = AnnRecallReplayDecodeLimits::new(encoded.len(), 3, 2, 12);

        assert!(matches!(
            AnnRecallLedger::from_canonical_replay_bytes(
                &encoded,
                admitted,
                identity(3)?,
                trial_profile,
                &TestProfileVerifier,
            ),
            Err(AnnRecallReplayCodecError::IdentityMismatch)
        ));
        let different_profile = profile(2, 3, 1, 0, supported_assumptions())?;
        assert!(matches!(
            AnnRecallLedger::from_canonical_replay_bytes(
                &encoded,
                admitted,
                trial_identity,
                different_profile,
                &TestProfileVerifier,
            ),
            Err(AnnRecallReplayCodecError::ProfileMismatch)
        ));
        assert!(matches!(
            AnnRecallLedger::from_canonical_replay_bytes(
                &encoded,
                AnnRecallReplayDecodeLimits::new(encoded.len(), 2, 2, 12),
                trial_identity,
                trial_profile,
                &TestProfileVerifier,
            ),
            Err(AnnRecallReplayCodecError::TopKLimit {
                actual: 3,
                maximum: 2,
            })
        ));
        assert!(matches!(
            AnnRecallLedger::from_canonical_replay_bytes(
                &encoded,
                AnnRecallReplayDecodeLimits::new(encoded.len() - 1, 3, 2, 12),
                trial_identity,
                trial_profile,
                &TestProfileVerifier,
            ),
            Err(AnnRecallReplayCodecError::EncodedBytesLimit {
                actual,
                maximum,
            }) if actual == encoded.len() && maximum == encoded.len() - 1
        ));
        Ok(())
    }

    #[test]
    fn canonical_replay_preflights_inventory_and_rejects_noncanonical_fields()
    -> Result<(), AnnRecallReplayCodecError> {
        let trial_identity = identity(2)?;
        let trial_profile = profile(2, 2, 0, 0, supported_assumptions())?;
        let mut ledger = AnnRecallLedger::try_new(trial_identity, trial_profile)?;
        ledger.record(observation(100, 20, &[1, 2], &[1, 2])?)?;
        let encoded = ledger.to_canonical_replay_bytes()?;
        let limits = AnnRecallReplayDecodeLimits::new(encoded.len(), 2, 2, 8);

        let truncated = &encoded[..encoded.len() - 1];
        assert!(matches!(
            AnnRecallLedger::from_canonical_replay_bytes(
                truncated,
                limits,
                trial_identity,
                trial_profile,
                &TestProfileVerifier,
            ),
            Err(AnnRecallReplayCodecError::Length {
                expected,
                actual,
            }) if expected == encoded.len() && actual == encoded.len() - 1
        ));

        let mut oversized_count = encoded.clone();
        oversized_count[456..460].copy_from_slice(&3_u32.to_le_bytes());
        assert!(matches!(
            AnnRecallLedger::from_canonical_replay_bytes(
                &oversized_count,
                limits,
                trial_identity,
                trial_profile,
                &TestProfileVerifier,
            ),
            Err(AnnRecallReplayCodecError::ObservationCountExceedsProfile {
                record_count: 3,
                profile_maximum: 2,
            })
        ));

        let mut noncanonical_bool = encoded;
        noncanonical_bool[450] = 2;
        assert!(matches!(
            AnnRecallLedger::from_canonical_replay_bytes(
                &noncanonical_bool,
                limits,
                trial_identity,
                trial_profile,
                &TestProfileVerifier,
            ),
            Err(AnnRecallReplayCodecError::NonCanonicalBoolean {
                field: "exact_baseline_complete",
                value: 2,
            })
        ));
        Ok(())
    }

    #[test]
    fn identity_and_sequence_failures_are_atomic() -> Result<(), AnnRecallError> {
        let mut ledger =
            AnnRecallLedger::try_new(identity(2)?, profile(2, 2, 0, 0, supported_assumptions())?)?;
        ledger.record(observation(100, 20, &[1, 2], &[1, 2])?)?;
        let before = ledger.evidence()?;

        let bad_sequence = observation(103, 21, &[3, 4], &[3, 4])?;
        assert!(matches!(
            ledger.record(bad_sequence),
            Err(AnnRecallError::UnexpectedSequence {
                expected: 101,
                actual: 103
            })
        ));
        assert_eq!(ledger.evidence()?, before);
        assert_eq!(ledger.observations().len(), 1);

        let wrong_binding = AnnRecallBinding::new(
            oid(99),
            oid(2),
            oid(3),
            oid(4),
            oid(5),
            oid(6),
            oid(7),
            oid(8),
            oid(9),
            12,
        );
        let wrong_identity = AnnRecallObservation::try_new(
            101,
            oid(21),
            oid(81),
            wrong_binding,
            vec![oid(3), oid(4)],
            vec![oid(3), oid(4)],
        )?;
        assert!(matches!(
            ledger.record(wrong_identity),
            Err(AnnRecallError::BindingMismatch {
                field: RecallBindingField::Monitor,
                ..
            })
        ));
        assert_eq!(ledger.evidence()?, before);
        assert_eq!(ledger.observations().len(), 1);
        Ok(())
    }

    #[test]
    fn duplicate_sample_member_failure_is_atomic() -> Result<(), AnnRecallError> {
        let mut ledger =
            AnnRecallLedger::try_new(identity(3)?, profile(3, 2, 0, 0, supported_assumptions())?)?;
        ledger.record(observation(100, 20, &[1, 2], &[1, 2])?)?;
        ledger.record(observation(101, 21, &[3, 4], &[3, 4])?)?;
        let before = ledger.evidence()?;
        let duplicate = observation(102, 20, &[5, 6], &[5, 6])?;
        assert!(matches!(
            ledger.record(duplicate),
            Err(AnnRecallError::DuplicateSampleMember { .. })
        ));
        assert_eq!(ledger.evidence()?, before);
        assert_eq!(ledger.sample_members.len(), 2);
        assert_eq!(ledger.observations().len(), 2);

        ledger.record(observation(102, 22, &[5, 6], &[5, 6])?)?;
        assert_eq!(ledger.sample_members.len(), 3);
        assert_eq!(ledger.observations().len(), 3);
        assert!(ledger.is_complete());
        Ok(())
    }

    #[test]
    fn interval_counts_queries_not_neighbor_slots() -> Result<(), AnnRecallError> {
        let mut ledger =
            AnnRecallLedger::try_new(identity(1)?, profile(1, 4, 0, 0, supported_assumptions())?)?;
        ledger.record(observation(100, 20, &[1, 2, 3, 4], &[1, 2, 3, 4])?)?;
        let interval = ledger.evidence()?.confidence_interval();
        assert_eq!(interval.query_observations(), 1);
        assert_eq!(interval.radius_units(), RECALL_SCALE);
        assert_eq!(interval.lower_units(), 0);
        assert_eq!(interval.upper_units(), RECALL_SCALE);
        assert_eq!(interval.failure_probability_power_of_two_exponent(), 1);
        Ok(())
    }

    #[test]
    fn incomplete_window_has_no_action_until_terminal_gate() -> Result<(), AnnRecallError> {
        let mut ledger =
            AnnRecallLedger::try_new(identity(2)?, profile(2, 2, 0, 0, supported_assumptions())?)?;
        ledger.record(observation(100, 20, &[1, 2], &[1, 2])?)?;
        let evidence = ledger.evidence()?;
        assert!(!evidence.complete());
        assert_eq!(evidence.action(), None);
        assert_eq!(evidence.selected_policy_oid(), None);
        assert_eq!(
            evidence.action_reason(),
            AnnRecallActionReason::IncompleteWindow
        );
        Ok(())
    }

    #[test]
    fn low_recall_prefix_cannot_rebuild_before_terminal_candidate_decision()
    -> Result<(), AnnRecallError> {
        let query_count = 128_usize;
        let mut ledger = AnnRecallLedger::try_new(
            identity(u64::try_from(query_count).map_err(|_| AnnRecallError::ArithmeticOverflow)?)?,
            profile(
                query_count,
                1,
                750_000_000,
                500_000_000,
                supported_assumptions(),
            )?,
        )?;

        for offset in 0_u8..16 {
            ledger.record(observation(
                100 + u64::from(offset),
                20 + offset,
                &[1],
                &[2],
            )?)?;
        }
        let prefix = ledger.evidence()?;
        assert_eq!(prefix.confidence_interval().upper_units(), 250_000_000);
        assert!(
            prefix.confidence_interval().upper_units()
                < prefix.profile().rebuild_recall_threshold_units()
        );
        assert_eq!(prefix.action(), None);
        assert_eq!(
            prefix.action_reason(),
            AnnRecallActionReason::IncompleteWindow
        );

        for offset in 16_u8..128 {
            ledger.record(observation(
                100 + u64::from(offset),
                20 + offset,
                &[1],
                &[1],
            )?)?;
        }
        let terminal = ledger.evidence()?;
        assert!(terminal.complete());
        assert_eq!(terminal.confidence_interval().lower_units(), 786_611_652);
        assert_eq!(terminal.action(), Some(AnnRecallAction::Candidate));
        assert_eq!(
            terminal.action_reason(),
            AnnRecallActionReason::CandidateRecallSatisfied
        );
        assert_eq!(terminal.selected_policy_oid(), Some(oid(9)));

        assert!(matches!(
            ledger.record(observation(228, 200, &[1], &[1])?),
            Err(AnnRecallError::WindowAlreadyComplete)
        ));
        assert_eq!(ledger.evidence()?, terminal);
        Ok(())
    }
}
