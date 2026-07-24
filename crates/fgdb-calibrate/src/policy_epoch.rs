//! Stream-sequenced decision-policy epochs.
//!
//! A [`DecisionPolicyEpoch`] is the immutable logical-state record that binds
//! one adaptive policy table to its deterministic fallback and the
//! statistical evidence used to promote it. Statistical evidence remains
//! assumption-bearing evidence: this module deliberately offers no path that
//! turns it into an invariant justification.

use std::fmt;

use crate::regime::{RegimePolicySelection, RegimeSignalEvidence, RegimeSignalStatus};
use fgdb_claim::EvidenceClaim;
use fgdb_evidence::{EvidenceEnvelope, FallbackBehavior};
use fgdb_types::ObjectId;

/// Domain separator for the version-1 canonical epoch encoding.
pub const DECISION_POLICY_EPOCH_ENCODING_DOMAIN: &[u8] = b"fgdb:decision-policy-epoch";

/// Current canonical encoding version.
pub const DECISION_POLICY_EPOCH_ENCODING_VERSION: u16 = 1;

/// Maximum canonical policy-identity length.
pub const MAX_POLICY_ID_BYTES: usize = 256;

/// Maximum number of evidence envelopes that one epoch may retain.
///
/// Promotion evidence is an audit inventory, not an unbounded event log.
/// The fixed limit keeps construction, validation, and canonical encoding
/// memory-bounded while leaving ample room for independent evidence streams.
pub const MAX_EVIDENCE_REFS: usize = 4_096;

const RECORD_TAG: u8 = 0x01;
const FIELD_COUNT: u16 = 8;
const FIELD_HEADER_BYTES: usize = 1 + 4;
const OBJECT_ID_BYTES: usize = 32;

const POLICY_ID_FIELD_TAG: u8 = 0x01;
const VERSION_FIELD_TAG: u8 = 0x02;
const SCOPE_FIELD_TAG: u8 = 0x03;
const LOGICAL_EFFECT_CLASS_FIELD_TAG: u8 = 0x04;
const PINNED_TABLE_OID_FIELD_TAG: u8 = 0x05;
const FALLBACK_OID_FIELD_TAG: u8 = 0x06;
const EVIDENCE_REFS_FIELD_TAG: u8 = 0x07;
const PREVIOUS_EPOCH_OID_FIELD_TAG: u8 = 0x08;

/// The exact three-way logical-effect classification for adaptive decisions.
///
/// Discriminants are stable canonical encoding tags.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LogicalEffectClass {
    /// Cache, prefetch, equivalent-kernel, and local-pacing choices whose
    /// observable answer and canonical state remain unchanged.
    AnswerPreservingPhysical = 0x01,
    /// Approximate generation, candidate, fusion, or stopping choices that
    /// can affect an answer within its explicitly bound answer contract.
    AnswerAffectingExecution = 0x02,
    /// Publication, watermark, retention, security, or configuration choices
    /// that alter canonical logical state.
    CanonicalStateAffecting = 0x03,
}

impl LogicalEffectClass {
    const fn canonical_tag(self) -> u8 {
        self as u8
    }
}

/// OID-backed scope of one decision policy.
///
/// Scope interpretation belongs to the object identified by `scope_oid`.
/// Keeping this type opaque avoids manufacturing an undocumented local scope
/// vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DecisionPolicyScope {
    scope_oid: ObjectId,
}

impl DecisionPolicyScope {
    /// Binds a decision policy to an already-defined scope object.
    #[must_use]
    pub const fn new(scope_oid: ObjectId) -> Self {
        Self { scope_oid }
    }

    /// Returns the identity of the bound scope object.
    #[must_use]
    pub const fn scope_oid(self) -> ObjectId {
        self.scope_oid
    }
}

/// Construction, transition, evidence-binding, or encoding failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionPolicyEpochError {
    /// A stable policy identity cannot be empty.
    EmptyPolicyId,
    /// A policy identity exceeded the canonical bound.
    PolicyIdTooLong { actual: usize, maximum: usize },
    /// A policy identity contained a byte outside printable canonical ASCII.
    NonCanonicalPolicyId { offset: usize },
    /// Space for the owned policy identity could not be reserved.
    PolicyIdAllocationFailed,
    /// Evidence references were not in strictly increasing OID order.
    EvidenceRefsOutOfOrder {
        index: usize,
        previous: ObjectId,
        current: ObjectId,
    },
    /// An evidence OID occurred more than once.
    DuplicateEvidenceRef {
        index: usize,
        evidence_oid: ObjectId,
    },
    /// An epoch exceeded the bounded promotion-evidence inventory.
    TooManyEvidenceRefs { actual: usize, maximum: usize },
    /// Space for the immutable evidence-reference list could not be reserved.
    EvidenceRefsAllocationFailed { count: usize },
    /// Promotion evidence was supplied without a predecessor binding.
    EvidenceWithoutPredecessor,
    /// A candidate successor did not name its predecessor.
    MissingPredecessor,
    /// A candidate successor did not carry promotion evidence.
    MissingPromotionEvidence,
    /// The predecessor version cannot be incremented.
    VersionExhausted { predecessor_version: u64 },
    /// A successor version was not exactly one greater than its predecessor.
    NonConsecutiveVersion {
        predecessor_version: u64,
        successor_version: u64,
    },
    /// The stable policy identity changed across a promotion.
    PolicyIdChanged,
    /// The OID-backed scope changed across a promotion.
    ScopeChanged,
    /// The logical-effect class changed across a promotion.
    LogicalEffectClassChanged,
    /// The successor did not name the exact supplied predecessor OID.
    PreviousEpochOidMismatch {
        expected: ObjectId,
        actual: Option<ObjectId>,
    },
    /// The deterministic fallback changed across a promotion.
    PinnedFallbackChanged {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// A promotion tried to select its own deterministic fallback as the
    /// candidate table, which carries no promotion decision.
    CandidateEqualsFallback { policy_oid: ObjectId },
    /// The supplied envelopes were not a one-for-one witness list.
    EvidenceEnvelopeCountMismatch { referenced: usize, supplied: usize },
    /// An envelope did not have the OID named at the corresponding canonical
    /// reference position.
    EvidenceEnvelopeRefMismatch {
        index: usize,
        expected: ObjectId,
        actual: ObjectId,
    },
    /// Promotion accepts only assumption-bearing statistical claims.
    EvidenceClaimMustBeStatistical {
        index: usize,
        evidence_oid: ObjectId,
    },
    /// An envelope was selected under a different pinned policy table.
    EvidenceSelectionPolicyMismatch {
        index: usize,
        expected: ObjectId,
        actual: ObjectId,
    },
    /// An envelope did not bind the epoch's deterministic fallback. `None`
    /// denotes an envelope configured to fail closed instead of naming the
    /// pinned fallback policy.
    EvidenceFallbackMismatch {
        index: usize,
        expected: ObjectId,
        actual: Option<ObjectId>,
    },
    /// A fallback successor may follow only an evidence-bearing candidate
    /// promotion, never a root epoch.
    FallbackRequiresPromotedPredecessor,
    /// A fallback successor cannot follow an epoch that already selects the
    /// pinned fallback.
    FallbackPredecessorAlreadyUsesPinnedFallback { policy_oid: ObjectId },
    /// A fallback successor must select the predecessor's immutable pinned
    /// fallback as its active policy table.
    FallbackTransitionMustSelectPinnedFallback {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// The distinguished regime-change evidence OID was not present in the
    /// successor's exact canonical evidence inventory.
    RegimeEvidenceRefMissing { evidence_oid: ObjectId },
    /// The supplied regime evidence did not report a detected change.
    RegimeEvidenceMustReportChange { actual: RegimeSignalStatus },
    /// The supplied regime evidence did not select its pinned fallback.
    RegimeEvidenceMustSelectFallback { actual: RegimePolicySelection },
    /// The regime evidence observed a different active candidate.
    RegimeEvidenceCandidateMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// The regime evidence named a different deterministic fallback.
    RegimeEvidenceFallbackMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// The regime evidence and its referenced envelope named different regime
    /// epochs.
    RegimeEvidenceEpochMismatch { expected: u64, actual: u64 },
    /// Detected regime evidence must retain the exact sequence that first
    /// selected the fallback.
    RegimeEvidenceMissingFallbackSequence,
    /// Detected regime evidence must identify its latest accepted source
    /// sequence.
    RegimeEvidenceMissingThroughSequence,
    /// A regime-change envelope must carry its exact source-stream window.
    RegimeEvidenceWindowMissing,
    /// A half-open evidence-window end could not represent the inclusive
    /// through-sequence.
    RegimeEvidenceWindowEndOverflow { through_sequence: u64 },
    /// The envelope window did not exactly cover the typed regime evidence
    /// prefix.
    RegimeEvidenceWindowMismatch {
        expected_start: u64,
        expected_end: u64,
        actual_start: u64,
        actual_end: u64,
    },
    /// The first fallback-selection sequence was outside the exact envelope
    /// window.
    RegimeFallbackSequenceOutsideWindow {
        fallback_sequence: u64,
        window_start: u64,
        window_end: u64,
    },
    /// A canonical record ended before the declared component was complete.
    CanonicalTruncated {
        offset: usize,
        needed: usize,
        remaining: usize,
    },
    /// The domain-length prefix did not name the one registered domain.
    CanonicalDomainLengthMismatch { expected: usize, actual: usize },
    /// The domain separator bytes did not match this record type.
    CanonicalDomainMismatch,
    /// The record used an unsupported canonical encoding version.
    UnsupportedCanonicalVersion { actual: u16 },
    /// The top-level record tag was not the registered epoch tag.
    UnexpectedCanonicalRecordTag { actual: u8 },
    /// The record did not declare exactly the eight normative fields.
    UnexpectedCanonicalFieldCount { actual: u16 },
    /// A field was absent, reordered, or substituted.
    UnexpectedCanonicalFieldTag {
        index: usize,
        expected: u8,
        actual: u8,
    },
    /// A field payload did not have its unique canonical length.
    CanonicalFieldLengthMismatch {
        field_tag: u8,
        expected: usize,
        actual: usize,
    },
    /// The logical-effect discriminant was outside the closed three-way
    /// vocabulary.
    InvalidLogicalEffectClassTag { actual: u8 },
    /// The predecessor option used a presence tag other than zero or one.
    InvalidPreviousEpochPresenceTag { actual: u8 },
    /// Bytes remained after all eight canonical fields.
    TrailingCanonicalBytes { count: usize },
    /// A root-only decoder was given a successor record.
    ExpectedRootEpoch,
    /// A variable-length component cannot be represented by the canonical
    /// length prefix.
    CanonicalComponentTooLong { field_tag: u8, length: usize },
    /// Canonical length arithmetic overflowed.
    CanonicalLengthOverflow,
    /// Space for the complete canonical record could not be reserved.
    CanonicalAllocationFailed { requested: usize },
}

impl fmt::Display for DecisionPolicyEpochError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPolicyId => formatter.write_str("policy_id must not be empty"),
            Self::PolicyIdTooLong { actual, maximum } => write!(
                formatter,
                "policy_id is {actual} bytes; maximum is {maximum}"
            ),
            Self::NonCanonicalPolicyId { offset } => {
                write!(
                    formatter,
                    "policy_id contains a non-canonical byte at offset {offset}"
                )
            }
            Self::PolicyIdAllocationFailed => formatter.write_str("could not allocate policy_id"),
            Self::EvidenceRefsOutOfOrder {
                index,
                previous,
                current,
            } => write!(
                formatter,
                "evidence_refs[{index}] ({current:?}) sorts before evidence_refs[{}] ({previous:?})",
                index.saturating_sub(1)
            ),
            Self::DuplicateEvidenceRef {
                index,
                evidence_oid,
            } => write!(
                formatter,
                "evidence_refs[{index}] duplicates {evidence_oid:?}"
            ),
            Self::TooManyEvidenceRefs { actual, maximum } => write!(
                formatter,
                "epoch has {actual} evidence references; maximum is {maximum}"
            ),
            Self::EvidenceRefsAllocationFailed { count } => {
                write!(
                    formatter,
                    "could not allocate {count} immutable evidence references"
                )
            }
            Self::EvidenceWithoutPredecessor => {
                formatter.write_str("promotion evidence requires previous_epoch_oid")
            }
            Self::MissingPredecessor => {
                formatter.write_str("promotion requires previous_epoch_oid")
            }
            Self::MissingPromotionEvidence => {
                formatter.write_str("promotion requires at least one evidence reference")
            }
            Self::VersionExhausted {
                predecessor_version,
            } => write!(
                formatter,
                "predecessor version {predecessor_version} cannot be incremented"
            ),
            Self::NonConsecutiveVersion {
                predecessor_version,
                successor_version,
            } => write!(
                formatter,
                "successor version {successor_version} does not immediately follow predecessor version {predecessor_version}"
            ),
            Self::PolicyIdChanged => formatter.write_str("policy_id changed across promotion"),
            Self::ScopeChanged => formatter.write_str("scope changed across promotion"),
            Self::LogicalEffectClassChanged => {
                formatter.write_str("logical_effect_class changed across promotion")
            }
            Self::PreviousEpochOidMismatch { expected, actual } => write!(
                formatter,
                "previous_epoch_oid mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::PinnedFallbackChanged { expected, actual } => write!(
                formatter,
                "pinned fallback changed: expected {expected:?}, got {actual:?}"
            ),
            Self::CandidateEqualsFallback { policy_oid } => write!(
                formatter,
                "promoted policy table {policy_oid:?} equals its deterministic fallback"
            ),
            Self::EvidenceEnvelopeCountMismatch {
                referenced,
                supplied,
            } => write!(
                formatter,
                "epoch references {referenced} evidence envelopes but {supplied} were supplied"
            ),
            Self::EvidenceEnvelopeRefMismatch {
                index,
                expected,
                actual,
            } => write!(
                formatter,
                "evidence envelope {index} has OID {actual:?}; expected {expected:?}"
            ),
            Self::EvidenceClaimMustBeStatistical {
                index,
                evidence_oid,
            } => write!(
                formatter,
                "evidence envelope {index} ({evidence_oid:?}) is not a StatisticalClaim"
            ),
            Self::EvidenceSelectionPolicyMismatch {
                index,
                expected,
                actual,
            } => write!(
                formatter,
                "evidence envelope {index} selected policy {actual:?}; expected pinned table {expected:?}"
            ),
            Self::EvidenceFallbackMismatch {
                index,
                expected,
                actual,
            } => write!(
                formatter,
                "evidence envelope {index} fallback {actual:?}; expected deterministic policy {expected:?}"
            ),
            Self::FallbackRequiresPromotedPredecessor => formatter
                .write_str("fallback transition requires an evidence-bearing promoted predecessor"),
            Self::FallbackPredecessorAlreadyUsesPinnedFallback { policy_oid } => write!(
                formatter,
                "fallback transition predecessor already selects pinned fallback {policy_oid:?}"
            ),
            Self::FallbackTransitionMustSelectPinnedFallback { expected, actual } => write!(
                formatter,
                "fallback transition selected {actual:?}; expected pinned fallback {expected:?}"
            ),
            Self::RegimeEvidenceRefMissing { evidence_oid } => write!(
                formatter,
                "regime-change evidence {evidence_oid:?} is absent from the canonical evidence inventory"
            ),
            Self::RegimeEvidenceMustReportChange { actual } => write!(
                formatter,
                "regime evidence status {actual:?} does not report a detected change"
            ),
            Self::RegimeEvidenceMustSelectFallback { actual } => write!(
                formatter,
                "regime evidence selection {actual:?} does not select the pinned fallback"
            ),
            Self::RegimeEvidenceCandidateMismatch { expected, actual } => write!(
                formatter,
                "regime evidence candidate {actual:?} does not match predecessor candidate {expected:?}"
            ),
            Self::RegimeEvidenceFallbackMismatch { expected, actual } => write!(
                formatter,
                "regime evidence fallback {actual:?} does not match pinned fallback {expected:?}"
            ),
            Self::RegimeEvidenceEpochMismatch { expected, actual } => write!(
                formatter,
                "regime evidence epoch {actual} does not match envelope epoch {expected}"
            ),
            Self::RegimeEvidenceMissingFallbackSequence => {
                formatter.write_str("detected regime evidence has no fallback-selection sequence")
            }
            Self::RegimeEvidenceMissingThroughSequence => {
                formatter.write_str("detected regime evidence has no through-sequence")
            }
            Self::RegimeEvidenceWindowMissing => {
                formatter.write_str("regime-change evidence envelope has no calibration window")
            }
            Self::RegimeEvidenceWindowEndOverflow { through_sequence } => write!(
                formatter,
                "regime evidence through-sequence {through_sequence} has no half-open window end"
            ),
            Self::RegimeEvidenceWindowMismatch {
                expected_start,
                expected_end,
                actual_start,
                actual_end,
            } => write!(
                formatter,
                "regime evidence window [{actual_start}, {actual_end}) does not match [{expected_start}, {expected_end})"
            ),
            Self::RegimeFallbackSequenceOutsideWindow {
                fallback_sequence,
                window_start,
                window_end,
            } => write!(
                formatter,
                "fallback sequence {fallback_sequence} is outside regime evidence window [{window_start}, {window_end})"
            ),
            Self::CanonicalTruncated {
                offset,
                needed,
                remaining,
            } => write!(
                formatter,
                "canonical epoch is truncated at byte {offset}: need {needed} bytes, have {remaining}"
            ),
            Self::CanonicalDomainLengthMismatch { expected, actual } => write!(
                formatter,
                "canonical domain length is {actual}; expected {expected}"
            ),
            Self::CanonicalDomainMismatch => {
                formatter.write_str("canonical epoch domain separator mismatch")
            }
            Self::UnsupportedCanonicalVersion { actual } => write!(
                formatter,
                "unsupported canonical epoch encoding version {actual}"
            ),
            Self::UnexpectedCanonicalRecordTag { actual } => write!(
                formatter,
                "canonical epoch record tag is {actual:#04x}; expected {RECORD_TAG:#04x}"
            ),
            Self::UnexpectedCanonicalFieldCount { actual } => write!(
                formatter,
                "canonical epoch declares {actual} fields; expected {FIELD_COUNT}"
            ),
            Self::UnexpectedCanonicalFieldTag {
                index,
                expected,
                actual,
            } => write!(
                formatter,
                "canonical epoch field {index} has tag {actual:#04x}; expected {expected:#04x}"
            ),
            Self::CanonicalFieldLengthMismatch {
                field_tag,
                expected,
                actual,
            } => write!(
                formatter,
                "canonical epoch field {field_tag:#04x} has length {actual}; expected {expected}"
            ),
            Self::InvalidLogicalEffectClassTag { actual } => write!(
                formatter,
                "logical_effect_class has unknown canonical tag {actual:#04x}"
            ),
            Self::InvalidPreviousEpochPresenceTag { actual } => write!(
                formatter,
                "previous_epoch_oid has invalid presence tag {actual:#04x}"
            ),
            Self::TrailingCanonicalBytes { count } => {
                write!(formatter, "canonical epoch has {count} trailing bytes")
            }
            Self::ExpectedRootEpoch => {
                formatter.write_str("canonical record is a successor, not a root epoch")
            }
            Self::CanonicalComponentTooLong { field_tag, length } => write!(
                formatter,
                "canonical field tag {field_tag:#04x} has unrepresentable length {length}"
            ),
            Self::CanonicalLengthOverflow => formatter.write_str("canonical epoch length overflow"),
            Self::CanonicalAllocationFailed { requested } => write!(
                formatter,
                "could not reserve {requested} bytes for canonical epoch encoding"
            ),
        }
    }
}

impl std::error::Error for DecisionPolicyEpochError {}

/// Immutable, stream-sequenced adaptive-policy state.
///
/// Every normative field is private and exposed only through read-only
/// accessors. A change creates and validates a successor; it never mutates an
/// existing epoch.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DecisionPolicyEpoch {
    policy_id: String,
    version: u64,
    scope: DecisionPolicyScope,
    logical_effect_class: LogicalEffectClass,
    pinned_table_oid: ObjectId,
    fallback_oid: ObjectId,
    evidence_refs: Vec<ObjectId>,
    previous_epoch_oid: Option<ObjectId>,
}

impl DecisionPolicyEpoch {
    /// Constructs a root epoch with no predecessor or promotion evidence.
    ///
    /// Every later epoch must be constructed by
    /// [`try_promote`](Self::try_promote) or
    /// [`try_revert_to_fallback`](Self::try_revert_to_fallback), each of which
    /// validates the complete predecessor and evidence binding before
    /// returning it.
    pub fn try_root(
        policy_id: &str,
        version: u64,
        scope: DecisionPolicyScope,
        logical_effect_class: LogicalEffectClass,
        pinned_table_oid: ObjectId,
        fallback_oid: ObjectId,
    ) -> Result<Self, DecisionPolicyEpochError> {
        Self::try_from_parts(
            policy_id,
            version,
            scope,
            logical_effect_class,
            pinned_table_oid,
            fallback_oid,
            &[],
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn try_from_parts(
        policy_id: &str,
        version: u64,
        scope: DecisionPolicyScope,
        logical_effect_class: LogicalEffectClass,
        pinned_table_oid: ObjectId,
        fallback_oid: ObjectId,
        evidence_refs: &[ObjectId],
        previous_epoch_oid: Option<ObjectId>,
    ) -> Result<Self, DecisionPolicyEpochError> {
        validate_policy_id(policy_id)?;
        if evidence_refs.len() > MAX_EVIDENCE_REFS {
            return Err(DecisionPolicyEpochError::TooManyEvidenceRefs {
                actual: evidence_refs.len(),
                maximum: MAX_EVIDENCE_REFS,
            });
        }
        validate_evidence_ref_order(evidence_refs)?;
        match (previous_epoch_oid, evidence_refs.is_empty()) {
            (None, false) => {
                return Err(DecisionPolicyEpochError::EvidenceWithoutPredecessor);
            }
            (Some(_), true) => {
                return Err(DecisionPolicyEpochError::MissingPromotionEvidence);
            }
            _ => {}
        }

        let policy_id = copy_policy_id(policy_id)?;
        let evidence_refs = copy_evidence_refs(evidence_refs)?;
        Ok(Self {
            policy_id,
            version,
            scope,
            logical_effect_class,
            pinned_table_oid,
            fallback_oid,
            evidence_refs,
            previous_epoch_oid,
        })
    }

    /// Constructs and fully validates the next policy promotion.
    ///
    /// The policy identity, scope, logical-effect class, and fallback are
    /// inherited exactly. Only the pinned candidate table, promotion
    /// evidence, version, and predecessor binding advance.
    pub fn try_promote(
        predecessor: &Self,
        predecessor_oid: ObjectId,
        pinned_table_oid: ObjectId,
        evidence_refs: &[ObjectId],
        evidence_envelopes: &[EvidenceEnvelope],
    ) -> Result<Self, DecisionPolicyEpochError> {
        if pinned_table_oid == predecessor.fallback_oid {
            return Err(DecisionPolicyEpochError::CandidateEqualsFallback {
                policy_oid: pinned_table_oid,
            });
        }
        let version = predecessor.version.checked_add(1).ok_or(
            DecisionPolicyEpochError::VersionExhausted {
                predecessor_version: predecessor.version,
            },
        )?;
        let candidate = Self::try_from_parts(
            predecessor.policy_id(),
            version,
            predecessor.scope,
            predecessor.logical_effect_class,
            pinned_table_oid,
            predecessor.fallback_oid,
            evidence_refs,
            Some(predecessor_oid),
        )?;
        candidate.validate_promotion_from(predecessor, predecessor_oid, evidence_envelopes)?;
        Ok(candidate)
    }

    /// Constructs the next epoch that explicitly selects the immutable pinned
    /// fallback after a typed regime-change receipt.
    ///
    /// This is a new successor, never a mutation or version rollback. The
    /// predecessor must itself be an evidence-bearing candidate promotion.
    /// `regime_evidence_oid` identifies which entry in the complete envelope
    /// inventory is the typed regime signal checked by this transition.
    #[allow(clippy::too_many_arguments)]
    pub fn try_revert_to_fallback(
        predecessor: &Self,
        predecessor_oid: ObjectId,
        evidence_refs: &[ObjectId],
        evidence_envelopes: &[EvidenceEnvelope],
        regime_evidence_oid: ObjectId,
        regime_evidence: &RegimeSignalEvidence,
    ) -> Result<Self, DecisionPolicyEpochError> {
        validate_fallback_predecessor(predecessor)?;
        let version = predecessor.version.checked_add(1).ok_or(
            DecisionPolicyEpochError::VersionExhausted {
                predecessor_version: predecessor.version,
            },
        )?;
        let fallback = Self::try_from_parts(
            predecessor.policy_id(),
            version,
            predecessor.scope,
            predecessor.logical_effect_class,
            predecessor.fallback_oid,
            predecessor.fallback_oid,
            evidence_refs,
            Some(predecessor_oid),
        )?;
        fallback.validate_fallback_from(
            predecessor,
            predecessor_oid,
            evidence_envelopes,
            regime_evidence_oid,
            regime_evidence,
        )?;
        Ok(fallback)
    }

    /// Strictly decodes a canonical root epoch.
    ///
    /// Successor bytes are rejected here rather than exposing a raw decoder
    /// that could manufacture an epoch without checking its predecessor and
    /// evidence. Use
    /// [`try_promoted_from_canonical_bytes`](Self::try_promoted_from_canonical_bytes)
    /// for successor records.
    pub fn try_root_from_canonical_bytes(encoded: &[u8]) -> Result<Self, DecisionPolicyEpochError> {
        let epoch = Self::decode_canonical_bytes(encoded)?;
        if epoch.previous_epoch_oid.is_some() || !epoch.evidence_refs.is_empty() {
            return Err(DecisionPolicyEpochError::ExpectedRootEpoch);
        }
        Ok(epoch)
    }

    /// Strictly decodes and fully validates one canonical successor epoch.
    ///
    /// The decoder requires the exact predecessor identity and the referenced
    /// envelopes so decoding cannot bypass the same transition laws enforced
    /// by [`try_promote`](Self::try_promote).
    pub fn try_promoted_from_canonical_bytes(
        encoded: &[u8],
        predecessor: &Self,
        predecessor_oid: ObjectId,
        evidence_envelopes: &[EvidenceEnvelope],
    ) -> Result<Self, DecisionPolicyEpochError> {
        let epoch = Self::decode_canonical_bytes(encoded)?;
        epoch.validate_promotion_from(predecessor, predecessor_oid, evidence_envelopes)?;
        Ok(epoch)
    }

    /// Strictly decodes and validates one canonical fallback successor.
    ///
    /// The exact predecessor, complete envelope inventory, distinguished
    /// regime evidence OID, and typed regime evidence are mandatory. Raw
    /// successor decoding remains private, so canonical bytes cannot bypass
    /// the same transition laws as
    /// [`try_revert_to_fallback`](Self::try_revert_to_fallback).
    #[allow(clippy::too_many_arguments)]
    pub fn try_fallback_from_canonical_bytes(
        encoded: &[u8],
        predecessor: &Self,
        predecessor_oid: ObjectId,
        evidence_envelopes: &[EvidenceEnvelope],
        regime_evidence_oid: ObjectId,
        regime_evidence: &RegimeSignalEvidence,
    ) -> Result<Self, DecisionPolicyEpochError> {
        let epoch = Self::decode_canonical_bytes(encoded)?;
        epoch.validate_fallback_from(
            predecessor,
            predecessor_oid,
            evidence_envelopes,
            regime_evidence_oid,
            regime_evidence,
        )?;
        Ok(epoch)
    }

    fn decode_canonical_bytes(encoded: &[u8]) -> Result<Self, DecisionPolicyEpochError> {
        let mut reader = CanonicalReader::new(encoded);

        let domain_len = usize::from(reader.read_u16()?);
        if domain_len != DECISION_POLICY_EPOCH_ENCODING_DOMAIN.len() {
            return Err(DecisionPolicyEpochError::CanonicalDomainLengthMismatch {
                expected: DECISION_POLICY_EPOCH_ENCODING_DOMAIN.len(),
                actual: domain_len,
            });
        }
        if reader.read_exact(domain_len)? != DECISION_POLICY_EPOCH_ENCODING_DOMAIN {
            return Err(DecisionPolicyEpochError::CanonicalDomainMismatch);
        }

        let encoding_version = reader.read_u16()?;
        if encoding_version != DECISION_POLICY_EPOCH_ENCODING_VERSION {
            return Err(DecisionPolicyEpochError::UnsupportedCanonicalVersion {
                actual: encoding_version,
            });
        }
        let record_tag = reader.read_u8()?;
        if record_tag != RECORD_TAG {
            return Err(DecisionPolicyEpochError::UnexpectedCanonicalRecordTag {
                actual: record_tag,
            });
        }
        let field_count = reader.read_u16()?;
        if field_count != FIELD_COUNT {
            return Err(DecisionPolicyEpochError::UnexpectedCanonicalFieldCount {
                actual: field_count,
            });
        }

        let policy_id_len = reader.read_field_length(0, POLICY_ID_FIELD_TAG)?;
        if policy_id_len > MAX_POLICY_ID_BYTES {
            return Err(DecisionPolicyEpochError::PolicyIdTooLong {
                actual: policy_id_len,
                maximum: MAX_POLICY_ID_BYTES,
            });
        }
        let policy_id_bytes = reader.read_exact(policy_id_len)?;
        if let Some((offset, _)) = policy_id_bytes
            .iter()
            .enumerate()
            .find(|(_, byte)| !(0x21_u8..=0x7e).contains(*byte))
        {
            return Err(DecisionPolicyEpochError::NonCanonicalPolicyId { offset });
        }
        let policy_id = std::str::from_utf8(policy_id_bytes).map_err(|error| {
            DecisionPolicyEpochError::NonCanonicalPolicyId {
                offset: error.valid_up_to(),
            }
        })?;

        let version_len = reader.read_field_length(1, VERSION_FIELD_TAG)?;
        expect_field_length(VERSION_FIELD_TAG, version_len, 8)?;
        let version = reader.read_u64()?;

        let scope_len = reader.read_field_length(2, SCOPE_FIELD_TAG)?;
        expect_field_length(SCOPE_FIELD_TAG, scope_len, OBJECT_ID_BYTES)?;
        let scope = DecisionPolicyScope::new(ObjectId(reader.read_array::<OBJECT_ID_BYTES>()?));

        let effect_len = reader.read_field_length(3, LOGICAL_EFFECT_CLASS_FIELD_TAG)?;
        expect_field_length(LOGICAL_EFFECT_CLASS_FIELD_TAG, effect_len, 1)?;
        let logical_effect_class = match reader.read_u8()? {
            0x01 => LogicalEffectClass::AnswerPreservingPhysical,
            0x02 => LogicalEffectClass::AnswerAffectingExecution,
            0x03 => LogicalEffectClass::CanonicalStateAffecting,
            actual => {
                return Err(DecisionPolicyEpochError::InvalidLogicalEffectClassTag { actual });
            }
        };

        let pinned_table_len = reader.read_field_length(4, PINNED_TABLE_OID_FIELD_TAG)?;
        expect_field_length(
            PINNED_TABLE_OID_FIELD_TAG,
            pinned_table_len,
            OBJECT_ID_BYTES,
        )?;
        let pinned_table_oid = ObjectId(reader.read_array::<OBJECT_ID_BYTES>()?);

        let fallback_len = reader.read_field_length(5, FALLBACK_OID_FIELD_TAG)?;
        expect_field_length(FALLBACK_OID_FIELD_TAG, fallback_len, OBJECT_ID_BYTES)?;
        let fallback_oid = ObjectId(reader.read_array::<OBJECT_ID_BYTES>()?);

        let evidence_payload_len = reader.read_field_length(6, EVIDENCE_REFS_FIELD_TAG)?;
        if evidence_payload_len < 4 {
            return Err(DecisionPolicyEpochError::CanonicalFieldLengthMismatch {
                field_tag: EVIDENCE_REFS_FIELD_TAG,
                expected: 4,
                actual: evidence_payload_len,
            });
        }
        let evidence_count = usize::try_from(reader.read_u32()?)
            .map_err(|_| DecisionPolicyEpochError::CanonicalLengthOverflow)?;
        if evidence_count > MAX_EVIDENCE_REFS {
            return Err(DecisionPolicyEpochError::TooManyEvidenceRefs {
                actual: evidence_count,
                maximum: MAX_EVIDENCE_REFS,
            });
        }
        let expected_evidence_payload_len = evidence_count
            .checked_mul(OBJECT_ID_BYTES)
            .and_then(|length| length.checked_add(4))
            .ok_or(DecisionPolicyEpochError::CanonicalLengthOverflow)?;
        expect_field_length(
            EVIDENCE_REFS_FIELD_TAG,
            evidence_payload_len,
            expected_evidence_payload_len,
        )?;
        let mut evidence_refs = Vec::new();
        evidence_refs
            .try_reserve_exact(evidence_count)
            .map_err(|_| DecisionPolicyEpochError::EvidenceRefsAllocationFailed {
                count: evidence_count,
            })?;
        for _ in 0..evidence_count {
            evidence_refs.push(ObjectId(reader.read_array::<OBJECT_ID_BYTES>()?));
        }

        let previous_payload_len = reader.read_field_length(7, PREVIOUS_EPOCH_OID_FIELD_TAG)?;
        if previous_payload_len == 0 {
            return Err(DecisionPolicyEpochError::CanonicalFieldLengthMismatch {
                field_tag: PREVIOUS_EPOCH_OID_FIELD_TAG,
                expected: 1,
                actual: 0,
            });
        }
        let previous_presence = reader.read_u8()?;
        let previous_epoch_oid = match previous_presence {
            0 => {
                expect_field_length(PREVIOUS_EPOCH_OID_FIELD_TAG, previous_payload_len, 1)?;
                None
            }
            1 => {
                expect_field_length(
                    PREVIOUS_EPOCH_OID_FIELD_TAG,
                    previous_payload_len,
                    1 + OBJECT_ID_BYTES,
                )?;
                Some(ObjectId(reader.read_array::<OBJECT_ID_BYTES>()?))
            }
            actual => {
                return Err(DecisionPolicyEpochError::InvalidPreviousEpochPresenceTag { actual });
            }
        };

        let trailing = reader.remaining();
        if trailing != 0 {
            return Err(DecisionPolicyEpochError::TrailingCanonicalBytes { count: trailing });
        }

        Self::try_from_parts(
            policy_id,
            version,
            scope,
            logical_effect_class,
            pinned_table_oid,
            fallback_oid,
            &evidence_refs,
            previous_epoch_oid,
        )
    }

    /// Returns the stable adaptive-policy identity.
    #[must_use]
    pub fn policy_id(&self) -> &str {
        &self.policy_id
    }

    /// Returns the stream-sequenced policy version.
    #[must_use]
    pub const fn version(&self) -> u64 {
        self.version
    }

    /// Returns the OID-backed policy scope.
    #[must_use]
    pub const fn scope(&self) -> DecisionPolicyScope {
        self.scope
    }

    /// Returns the exact logical-effect classification.
    #[must_use]
    pub const fn logical_effect_class(&self) -> LogicalEffectClass {
        self.logical_effect_class
    }

    /// Returns the immutable identity of the selected policy table.
    #[must_use]
    pub const fn pinned_table_oid(&self) -> ObjectId {
        self.pinned_table_oid
    }

    /// Returns the immutable deterministic fallback policy identity.
    #[must_use]
    pub const fn fallback_oid(&self) -> ObjectId {
        self.fallback_oid
    }

    /// Returns the canonical sorted-unique evidence references.
    #[must_use]
    pub fn evidence_refs(&self) -> &[ObjectId] {
        &self.evidence_refs
    }

    /// Returns the exact predecessor identity, if this is not a root epoch.
    #[must_use]
    pub const fn previous_epoch_oid(&self) -> Option<ObjectId> {
        self.previous_epoch_oid
    }

    /// Validates every cross-epoch promotion law and its evidence bindings.
    pub fn validate_promotion_from(
        &self,
        predecessor: &Self,
        predecessor_oid: ObjectId,
        evidence_envelopes: &[EvidenceEnvelope],
    ) -> Result<(), DecisionPolicyEpochError> {
        if self.previous_epoch_oid.is_none() {
            return Err(DecisionPolicyEpochError::MissingPredecessor);
        }
        if self.evidence_refs.is_empty() {
            return Err(DecisionPolicyEpochError::MissingPromotionEvidence);
        }
        self.validate_successor_identity_from(predecessor, predecessor_oid)?;
        if self.pinned_table_oid == self.fallback_oid {
            return Err(DecisionPolicyEpochError::CandidateEqualsFallback {
                policy_oid: self.pinned_table_oid,
            });
        }

        self.validate_statistical_evidence(evidence_envelopes)
    }

    /// Validates one explicit successor transition from a promoted candidate
    /// to its immutable pinned fallback.
    #[allow(clippy::too_many_arguments)]
    pub fn validate_fallback_from(
        &self,
        predecessor: &Self,
        predecessor_oid: ObjectId,
        evidence_envelopes: &[EvidenceEnvelope],
        regime_evidence_oid: ObjectId,
        regime_evidence: &RegimeSignalEvidence,
    ) -> Result<(), DecisionPolicyEpochError> {
        if self.previous_epoch_oid.is_none() {
            return Err(DecisionPolicyEpochError::MissingPredecessor);
        }
        if self.evidence_refs.is_empty() {
            return Err(DecisionPolicyEpochError::MissingPromotionEvidence);
        }
        self.validate_successor_identity_from(predecessor, predecessor_oid)?;
        validate_fallback_predecessor(predecessor)?;
        if self.pinned_table_oid != self.fallback_oid {
            return Err(
                DecisionPolicyEpochError::FallbackTransitionMustSelectPinnedFallback {
                    expected: self.fallback_oid,
                    actual: self.pinned_table_oid,
                },
            );
        }
        self.validate_statistical_evidence(evidence_envelopes)?;
        self.validate_regime_fallback_evidence(
            predecessor,
            evidence_envelopes,
            regime_evidence_oid,
            regime_evidence,
        )
    }

    fn validate_successor_identity_from(
        &self,
        predecessor: &Self,
        predecessor_oid: ObjectId,
    ) -> Result<(), DecisionPolicyEpochError> {
        let expected_version = predecessor.version.checked_add(1).ok_or(
            DecisionPolicyEpochError::VersionExhausted {
                predecessor_version: predecessor.version,
            },
        )?;
        if self.version != expected_version {
            return Err(DecisionPolicyEpochError::NonConsecutiveVersion {
                predecessor_version: predecessor.version,
                successor_version: self.version,
            });
        }
        if self.policy_id != predecessor.policy_id {
            return Err(DecisionPolicyEpochError::PolicyIdChanged);
        }
        if self.scope != predecessor.scope {
            return Err(DecisionPolicyEpochError::ScopeChanged);
        }
        if self.logical_effect_class != predecessor.logical_effect_class {
            return Err(DecisionPolicyEpochError::LogicalEffectClassChanged);
        }
        if self.previous_epoch_oid != Some(predecessor_oid) {
            return Err(DecisionPolicyEpochError::PreviousEpochOidMismatch {
                expected: predecessor_oid,
                actual: self.previous_epoch_oid,
            });
        }
        if self.fallback_oid != predecessor.fallback_oid {
            return Err(DecisionPolicyEpochError::PinnedFallbackChanged {
                expected: predecessor.fallback_oid,
                actual: self.fallback_oid,
            });
        }
        Ok(())
    }

    fn validate_regime_fallback_evidence(
        &self,
        predecessor: &Self,
        evidence_envelopes: &[EvidenceEnvelope],
        regime_evidence_oid: ObjectId,
        regime_evidence: &RegimeSignalEvidence,
    ) -> Result<(), DecisionPolicyEpochError> {
        let evidence_index = self
            .evidence_refs
            .binary_search(&regime_evidence_oid)
            .map_err(|_| DecisionPolicyEpochError::RegimeEvidenceRefMissing {
                evidence_oid: regime_evidence_oid,
            })?;
        let envelope = evidence_envelopes.get(evidence_index).ok_or(
            DecisionPolicyEpochError::EvidenceEnvelopeCountMismatch {
                referenced: self.evidence_refs.len(),
                supplied: evidence_envelopes.len(),
            },
        )?;

        if regime_evidence.status() != RegimeSignalStatus::ChangeDetected {
            return Err(DecisionPolicyEpochError::RegimeEvidenceMustReportChange {
                actual: regime_evidence.status(),
            });
        }
        if regime_evidence.selection() != RegimePolicySelection::PinnedFallback {
            return Err(DecisionPolicyEpochError::RegimeEvidenceMustSelectFallback {
                actual: regime_evidence.selection(),
            });
        }
        let identity = regime_evidence.identity();
        let actual_candidate = identity.candidate_decision_oid();
        if actual_candidate != predecessor.pinned_table_oid {
            return Err(DecisionPolicyEpochError::RegimeEvidenceCandidateMismatch {
                expected: predecessor.pinned_table_oid,
                actual: actual_candidate,
            });
        }
        let actual_fallback = identity.pinned_fallback_oid();
        if actual_fallback != predecessor.fallback_oid {
            return Err(DecisionPolicyEpochError::RegimeEvidenceFallbackMismatch {
                expected: predecessor.fallback_oid,
                actual: actual_fallback,
            });
        }
        if envelope.regime_epoch() != identity.regime_epoch() {
            return Err(DecisionPolicyEpochError::RegimeEvidenceEpochMismatch {
                expected: identity.regime_epoch(),
                actual: envelope.regime_epoch(),
            });
        }
        let fallback_sequence = regime_evidence
            .fallback_sequence()
            .ok_or(DecisionPolicyEpochError::RegimeEvidenceMissingFallbackSequence)?;
        let through_sequence = regime_evidence
            .through_sequence()
            .ok_or(DecisionPolicyEpochError::RegimeEvidenceMissingThroughSequence)?;
        let expected_start = identity.window().first();
        let expected_end = through_sequence.checked_add(1).ok_or(
            DecisionPolicyEpochError::RegimeEvidenceWindowEndOverflow { through_sequence },
        )?;
        let window = envelope
            .calibration_window()
            .ok_or(DecisionPolicyEpochError::RegimeEvidenceWindowMissing)?;
        if window.start_seq != expected_start || window.end_seq != expected_end {
            return Err(DecisionPolicyEpochError::RegimeEvidenceWindowMismatch {
                expected_start,
                expected_end,
                actual_start: window.start_seq,
                actual_end: window.end_seq,
            });
        }
        if fallback_sequence < window.start_seq || fallback_sequence >= window.end_seq {
            return Err(
                DecisionPolicyEpochError::RegimeFallbackSequenceOutsideWindow {
                    fallback_sequence,
                    window_start: window.start_seq,
                    window_end: window.end_seq,
                },
            );
        }
        Ok(())
    }

    /// Proves that the supplied envelopes are the exact referenced
    /// statistical evidence and bind both this table and this fallback.
    ///
    /// This method accepts only [`EvidenceClaim::StatisticalClaim`]. It
    /// intentionally returns no claim-lattice justification, so successful
    /// promotion evidence cannot be reused as an invariant substitute.
    pub fn validate_statistical_evidence(
        &self,
        evidence_envelopes: &[EvidenceEnvelope],
    ) -> Result<(), DecisionPolicyEpochError> {
        if evidence_envelopes.len() != self.evidence_refs.len() {
            return Err(DecisionPolicyEpochError::EvidenceEnvelopeCountMismatch {
                referenced: self.evidence_refs.len(),
                supplied: evidence_envelopes.len(),
            });
        }

        for (index, (expected_oid, envelope)) in self
            .evidence_refs
            .iter()
            .zip(evidence_envelopes.iter())
            .enumerate()
        {
            let actual_oid = envelope.evidence_oid();
            if actual_oid != *expected_oid {
                return Err(DecisionPolicyEpochError::EvidenceEnvelopeRefMismatch {
                    index,
                    expected: *expected_oid,
                    actual: actual_oid,
                });
            }
            if !matches!(envelope.claim(), EvidenceClaim::StatisticalClaim { .. }) {
                return Err(DecisionPolicyEpochError::EvidenceClaimMustBeStatistical {
                    index,
                    evidence_oid: actual_oid,
                });
            }
            let selection_policy_oid = envelope.selection_policy_oid();
            if selection_policy_oid != self.pinned_table_oid {
                return Err(DecisionPolicyEpochError::EvidenceSelectionPolicyMismatch {
                    index,
                    expected: self.pinned_table_oid,
                    actual: selection_policy_oid,
                });
            }
            match envelope.fallback() {
                FallbackBehavior::DeterministicPolicy { policy_oid }
                    if policy_oid == self.fallback_oid => {}
                FallbackBehavior::DeterministicPolicy { policy_oid } => {
                    return Err(DecisionPolicyEpochError::EvidenceFallbackMismatch {
                        index,
                        expected: self.fallback_oid,
                        actual: Some(policy_oid),
                    });
                }
                FallbackBehavior::FailClosed => {
                    return Err(DecisionPolicyEpochError::EvidenceFallbackMismatch {
                        index,
                        expected: self.fallback_oid,
                        actual: None,
                    });
                }
            }
        }
        Ok(())
    }

    /// Encodes the epoch in the unique version-1 canonical byte form.
    ///
    /// The record contains an explicit domain length and domain separator,
    /// encoding version, record tag, field count, and a tag/length header for
    /// each of the eight normative fields.
    pub fn try_to_canonical_bytes(&self) -> Result<Vec<u8>, DecisionPolicyEpochError> {
        let domain_len = component_len_u16(DECISION_POLICY_EPOCH_ENCODING_DOMAIN.len(), 0)?;
        let policy_id_len = component_len_u32(self.policy_id.len(), POLICY_ID_FIELD_TAG)?;
        let evidence_count = component_len_u32(self.evidence_refs.len(), EVIDENCE_REFS_FIELD_TAG)?;
        let evidence_oid_bytes = self
            .evidence_refs
            .len()
            .checked_mul(OBJECT_ID_BYTES)
            .ok_or(DecisionPolicyEpochError::CanonicalLengthOverflow)?;
        let evidence_payload_len = 4usize
            .checked_add(evidence_oid_bytes)
            .ok_or(DecisionPolicyEpochError::CanonicalLengthOverflow)?;
        let evidence_payload_len_u32 =
            component_len_u32(evidence_payload_len, EVIDENCE_REFS_FIELD_TAG)?;
        let previous_payload_len = if self.previous_epoch_oid.is_some() {
            1usize
                .checked_add(OBJECT_ID_BYTES)
                .ok_or(DecisionPolicyEpochError::CanonicalLengthOverflow)?
        } else {
            1
        };
        let previous_payload_len_u32 =
            component_len_u32(previous_payload_len, PREVIOUS_EPOCH_OID_FIELD_TAG)?;

        let prefix_len = 2usize
            .checked_add(DECISION_POLICY_EPOCH_ENCODING_DOMAIN.len())
            .and_then(|length| length.checked_add(2))
            .and_then(|length| length.checked_add(1))
            .and_then(|length| length.checked_add(2))
            .ok_or(DecisionPolicyEpochError::CanonicalLengthOverflow)?;
        let mut total_len = prefix_len;
        total_len = checked_field_total(total_len, self.policy_id.len())?;
        total_len = checked_field_total(total_len, 8)?;
        total_len = checked_field_total(total_len, OBJECT_ID_BYTES)?;
        total_len = checked_field_total(total_len, 1)?;
        total_len = checked_field_total(total_len, OBJECT_ID_BYTES)?;
        total_len = checked_field_total(total_len, OBJECT_ID_BYTES)?;
        total_len = checked_field_total(total_len, evidence_payload_len)?;
        total_len = checked_field_total(total_len, previous_payload_len)?;

        let mut encoded = Vec::new();
        encoded.try_reserve_exact(total_len).map_err(|_| {
            DecisionPolicyEpochError::CanonicalAllocationFailed {
                requested: total_len,
            }
        })?;

        encoded.extend_from_slice(&domain_len.to_le_bytes());
        encoded.extend_from_slice(DECISION_POLICY_EPOCH_ENCODING_DOMAIN);
        encoded.extend_from_slice(&DECISION_POLICY_EPOCH_ENCODING_VERSION.to_le_bytes());
        encoded.push(RECORD_TAG);
        encoded.extend_from_slice(&FIELD_COUNT.to_le_bytes());

        write_field_header(&mut encoded, POLICY_ID_FIELD_TAG, policy_id_len);
        encoded.extend_from_slice(self.policy_id.as_bytes());

        write_field_header(&mut encoded, VERSION_FIELD_TAG, 8);
        encoded.extend_from_slice(&self.version.to_le_bytes());

        write_field_header(&mut encoded, SCOPE_FIELD_TAG, OBJECT_ID_BYTES as u32);
        encoded.extend_from_slice(self.scope.scope_oid.as_bytes());

        write_field_header(&mut encoded, LOGICAL_EFFECT_CLASS_FIELD_TAG, 1);
        encoded.push(self.logical_effect_class.canonical_tag());

        write_field_header(
            &mut encoded,
            PINNED_TABLE_OID_FIELD_TAG,
            OBJECT_ID_BYTES as u32,
        );
        encoded.extend_from_slice(self.pinned_table_oid.as_bytes());

        write_field_header(&mut encoded, FALLBACK_OID_FIELD_TAG, OBJECT_ID_BYTES as u32);
        encoded.extend_from_slice(self.fallback_oid.as_bytes());

        write_field_header(
            &mut encoded,
            EVIDENCE_REFS_FIELD_TAG,
            evidence_payload_len_u32,
        );
        encoded.extend_from_slice(&evidence_count.to_le_bytes());
        for evidence_oid in &self.evidence_refs {
            encoded.extend_from_slice(evidence_oid.as_bytes());
        }

        write_field_header(
            &mut encoded,
            PREVIOUS_EPOCH_OID_FIELD_TAG,
            previous_payload_len_u32,
        );
        match self.previous_epoch_oid {
            Some(previous_epoch_oid) => {
                encoded.push(1);
                encoded.extend_from_slice(previous_epoch_oid.as_bytes());
            }
            None => encoded.push(0),
        }

        Ok(encoded)
    }
}

fn validate_fallback_predecessor(
    predecessor: &DecisionPolicyEpoch,
) -> Result<(), DecisionPolicyEpochError> {
    if predecessor.previous_epoch_oid.is_none() || predecessor.evidence_refs.is_empty() {
        return Err(DecisionPolicyEpochError::FallbackRequiresPromotedPredecessor);
    }
    if predecessor.pinned_table_oid == predecessor.fallback_oid {
        return Err(
            DecisionPolicyEpochError::FallbackPredecessorAlreadyUsesPinnedFallback {
                policy_oid: predecessor.fallback_oid,
            },
        );
    }
    Ok(())
}

fn validate_policy_id(policy_id: &str) -> Result<(), DecisionPolicyEpochError> {
    if policy_id.is_empty() {
        return Err(DecisionPolicyEpochError::EmptyPolicyId);
    }
    if policy_id.len() > MAX_POLICY_ID_BYTES {
        return Err(DecisionPolicyEpochError::PolicyIdTooLong {
            actual: policy_id.len(),
            maximum: MAX_POLICY_ID_BYTES,
        });
    }
    if let Some((offset, _)) = policy_id
        .as_bytes()
        .iter()
        .enumerate()
        .find(|(_, byte)| !(0x21_u8..=0x7e).contains(*byte))
    {
        return Err(DecisionPolicyEpochError::NonCanonicalPolicyId { offset });
    }
    Ok(())
}

fn copy_policy_id(policy_id: &str) -> Result<String, DecisionPolicyEpochError> {
    let mut owned = String::new();
    owned
        .try_reserve_exact(policy_id.len())
        .map_err(|_| DecisionPolicyEpochError::PolicyIdAllocationFailed)?;
    owned.push_str(policy_id);
    Ok(owned)
}

fn validate_evidence_ref_order(evidence_refs: &[ObjectId]) -> Result<(), DecisionPolicyEpochError> {
    let Some(mut previous) = evidence_refs.first() else {
        return Ok(());
    };
    for (index, current) in evidence_refs.iter().enumerate().skip(1) {
        if current == previous {
            return Err(DecisionPolicyEpochError::DuplicateEvidenceRef {
                index,
                evidence_oid: *current,
            });
        }
        if current < previous {
            return Err(DecisionPolicyEpochError::EvidenceRefsOutOfOrder {
                index,
                previous: *previous,
                current: *current,
            });
        }
        previous = current;
    }
    Ok(())
}

fn copy_evidence_refs(
    evidence_refs: &[ObjectId],
) -> Result<Vec<ObjectId>, DecisionPolicyEpochError> {
    let mut owned = Vec::new();
    owned.try_reserve_exact(evidence_refs.len()).map_err(|_| {
        DecisionPolicyEpochError::EvidenceRefsAllocationFailed {
            count: evidence_refs.len(),
        }
    })?;
    owned.extend_from_slice(evidence_refs);
    Ok(owned)
}

fn component_len_u16(length: usize, field_tag: u8) -> Result<u16, DecisionPolicyEpochError> {
    u16::try_from(length)
        .map_err(|_| DecisionPolicyEpochError::CanonicalComponentTooLong { field_tag, length })
}

fn component_len_u32(length: usize, field_tag: u8) -> Result<u32, DecisionPolicyEpochError> {
    u32::try_from(length)
        .map_err(|_| DecisionPolicyEpochError::CanonicalComponentTooLong { field_tag, length })
}

fn checked_field_total(
    current: usize,
    payload_len: usize,
) -> Result<usize, DecisionPolicyEpochError> {
    current
        .checked_add(FIELD_HEADER_BYTES)
        .and_then(|length| length.checked_add(payload_len))
        .ok_or(DecisionPolicyEpochError::CanonicalLengthOverflow)
}

fn write_field_header(encoded: &mut Vec<u8>, tag: u8, payload_len: u32) {
    encoded.push(tag);
    encoded.extend_from_slice(&payload_len.to_le_bytes());
}

fn expect_field_length(
    field_tag: u8,
    actual: usize,
    expected: usize,
) -> Result<(), DecisionPolicyEpochError> {
    if actual != expected {
        return Err(DecisionPolicyEpochError::CanonicalFieldLengthMismatch {
            field_tag,
            expected,
            actual,
        });
    }
    Ok(())
}

struct CanonicalReader<'a> {
    encoded: &'a [u8],
    offset: usize,
}

impl<'a> CanonicalReader<'a> {
    const fn new(encoded: &'a [u8]) -> Self {
        Self { encoded, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.encoded.len().saturating_sub(self.offset)
    }

    fn read_exact(&mut self, length: usize) -> Result<&'a [u8], DecisionPolicyEpochError> {
        let remaining = self.remaining();
        if remaining < length {
            return Err(DecisionPolicyEpochError::CanonicalTruncated {
                offset: self.offset,
                needed: length,
                remaining,
            });
        }
        let end = self
            .offset
            .checked_add(length)
            .ok_or(DecisionPolicyEpochError::CanonicalLengthOverflow)?;
        let bytes = self.encoded.get(self.offset..end).ok_or(
            DecisionPolicyEpochError::CanonicalTruncated {
                offset: self.offset,
                needed: length,
                remaining,
            },
        )?;
        self.offset = end;
        Ok(bytes)
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], DecisionPolicyEpochError> {
        let offset = self.offset;
        let bytes = self.read_exact(N)?;
        <[u8; N]>::try_from(bytes).map_err(|_| DecisionPolicyEpochError::CanonicalTruncated {
            offset,
            needed: N,
            remaining: bytes.len(),
        })
    }

    fn read_u8(&mut self) -> Result<u8, DecisionPolicyEpochError> {
        let [value] = self.read_array::<1>()?;
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16, DecisionPolicyEpochError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32, DecisionPolicyEpochError> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64, DecisionPolicyEpochError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    fn read_field_length(
        &mut self,
        index: usize,
        expected_tag: u8,
    ) -> Result<usize, DecisionPolicyEpochError> {
        let actual_tag = self.read_u8()?;
        if actual_tag != expected_tag {
            return Err(DecisionPolicyEpochError::UnexpectedCanonicalFieldTag {
                index,
                expected: expected_tag,
                actual: actual_tag,
            });
        }
        usize::try_from(self.read_u32()?)
            .map_err(|_| DecisionPolicyEpochError::CanonicalLengthOverflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regime::{
        COMBINED_REGIME_SIGNAL_ID, COMBINED_REGIME_SIGNAL_VERSION, CusumConfig, MetricSample,
        PageHinkleyConfig, RegimeSequenceWindow, RegimeSignalIdentity, RegimeSignalMonitor,
        RegimeSignalProfile, RuntimeMetricSeries, SequencedRegimeSample,
    };
    use asupersync::runtime::changepoint::ChangeDirection;
    use fgdb_claim::StatisticalErrorControl;
    use fgdb_evidence::CalibrationWindow;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    fn scope(fill: u8) -> DecisionPolicyScope {
        DecisionPolicyScope::new(oid(fill))
    }

    fn replace_test_bytes(encoded: &mut [u8], offset: usize, replacement: &[u8]) -> TestResult {
        let end = offset.checked_add(replacement.len()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "test mutation offset overflowed",
            )
        })?;
        let target = encoded.get_mut(offset..end).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "test mutation was outside the canonical record",
            )
        })?;
        target.copy_from_slice(replacement);
        Ok(())
    }

    fn replace_test_byte(encoded: &mut [u8], offset: usize, replacement: u8) -> TestResult {
        replace_test_bytes(encoded, offset, &[replacement])
    }

    fn root_with(
        policy_id: &str,
        version: u64,
        policy_scope: DecisionPolicyScope,
        effect: LogicalEffectClass,
        table_oid: ObjectId,
        fallback_oid: ObjectId,
    ) -> Result<DecisionPolicyEpoch, DecisionPolicyEpochError> {
        DecisionPolicyEpoch::try_root(
            policy_id,
            version,
            policy_scope,
            effect,
            table_oid,
            fallback_oid,
        )
    }

    fn statistical_claim() -> EvidenceClaim {
        EvidenceClaim::StatisticalClaim {
            population: "named-policy-stream".into(),
            sampling_rule: "every-stream-event".into(),
            error_control: StatisticalErrorControl::try_alpha(0.01).unwrap(),
            power_or_effective_sample_size: "n_eff=4096".into(),
            assumptions: vec!["registered-null-and-filtration".into()],
        }
    }

    fn statistical_envelope(
        evidence_oid: ObjectId,
        table_oid: ObjectId,
        fallback_oid: ObjectId,
    ) -> EvidenceEnvelope {
        EvidenceEnvelope::new(
            statistical_claim(),
            evidence_oid,
            table_oid,
            None,
            7,
            FallbackBehavior::DeterministicPolicy {
                policy_oid: fallback_oid,
            },
        )
    }

    fn promoted_candidate_epoch() -> TestResult<DecisionPolicyEpoch> {
        let root = root_with(
            "policy:fallback-transition",
            40,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(20),
            oid(90),
        )?;
        Ok(DecisionPolicyEpoch::try_promote(
            &root,
            oid(80),
            oid(30),
            &[oid(10)],
            &[statistical_envelope(oid(10), oid(30), oid(90))],
        )?)
    }

    fn regime_evidence_for_samples(
        candidate_oid: ObjectId,
        fallback_oid: ObjectId,
        samples: &[i64],
    ) -> TestResult<RegimeSignalEvidence> {
        let profile_oid = oid(61);
        let profile = RegimeSignalProfile::try_new(
            profile_oid,
            RuntimeMetricSeries::Custom(17),
            PageHinkleyConfig {
                tolerance: MetricSample::from_micro_units(0),
                threshold: 10 * MetricSample::SCALE,
                reset_after_detection: true,
            },
            CusumConfig {
                baseline: MetricSample::from_units(10),
                drift: MetricSample::from_micro_units(0),
                threshold: 100 * MetricSample::SCALE,
                direction: ChangeDirection::Increase,
                reset_after_detection: true,
            },
            CusumConfig {
                baseline: MetricSample::from_units(10),
                drift: MetricSample::from_micro_units(0),
                threshold: 100 * MetricSample::SCALE,
                direction: ChangeDirection::Decrease,
                reset_after_detection: true,
            },
            8,
            4,
        )?;
        let identity = RegimeSignalIdentity::try_new(
            oid(60),
            oid(62),
            profile_oid,
            COMBINED_REGIME_SIGNAL_ID,
            COMBINED_REGIME_SIGNAL_VERSION,
            RegimeSequenceWindow::try_new(60, 67)?,
            7,
            candidate_oid,
            fallback_oid,
        )?;
        let mut monitor = RegimeSignalMonitor::try_new(identity.clone(), profile.clone())?;
        let mut latest = None;
        for (offset, units) in samples.iter().copied().enumerate() {
            let sequence = 60_u64
                .checked_add(u64::try_from(offset)?)
                .ok_or_else(|| std::io::Error::other("regime test sequence overflowed"))?;
            latest = Some(
                monitor
                    .observe(SequencedRegimeSample::new(
                        identity.clone(),
                        profile.clone(),
                        sequence,
                        MetricSample::from_units(units),
                    ))?
                    .evidence,
            );
        }
        latest.ok_or_else(|| std::io::Error::other("regime test produced no evidence").into())
    }

    fn detected_regime_evidence(
        candidate_oid: ObjectId,
        fallback_oid: ObjectId,
    ) -> TestResult<RegimeSignalEvidence> {
        regime_evidence_for_samples(candidate_oid, fallback_oid, &[10_i64, 10, 10, 10, 10, 30])
    }

    fn regime_fallback_envelope(
        evidence_oid: ObjectId,
        fallback_oid: ObjectId,
    ) -> TestResult<EvidenceEnvelope> {
        Ok(EvidenceEnvelope::new(
            statistical_claim(),
            evidence_oid,
            fallback_oid,
            Some(CalibrationWindow::new(60, 66)?),
            7,
            FallbackBehavior::DeterministicPolicy {
                policy_oid: fallback_oid,
            },
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn candidate_with(
        _predecessor: &DecisionPolicyEpoch,
        predecessor_oid: ObjectId,
        policy_id: &str,
        version: u64,
        policy_scope: DecisionPolicyScope,
        effect: LogicalEffectClass,
        fallback_oid: ObjectId,
        evidence_oid: ObjectId,
    ) -> Result<DecisionPolicyEpoch, DecisionPolicyEpochError> {
        DecisionPolicyEpoch::try_from_parts(
            policy_id,
            version,
            policy_scope,
            effect,
            oid(30),
            fallback_oid,
            &[evidence_oid],
            Some(predecessor_oid),
        )
    }

    #[test]
    fn logical_effect_vocabulary_has_exact_stable_tags() {
        assert_eq!(
            LogicalEffectClass::AnswerPreservingPhysical.canonical_tag(),
            1
        );
        assert_eq!(
            LogicalEffectClass::AnswerAffectingExecution.canonical_tag(),
            2
        );
        assert_eq!(
            LogicalEffectClass::CanonicalStateAffecting.canonical_tag(),
            3
        );
    }

    #[test]
    fn policy_id_profile_rejects_empty_long_and_noncanonical_values() {
        assert_eq!(
            DecisionPolicyEpoch::try_root(
                "",
                0,
                scope(1),
                LogicalEffectClass::AnswerPreservingPhysical,
                oid(2),
                oid(3),
            ),
            Err(DecisionPolicyEpochError::EmptyPolicyId)
        );
        let too_long = "x".repeat(MAX_POLICY_ID_BYTES + 1);
        assert_eq!(
            DecisionPolicyEpoch::try_root(
                &too_long,
                0,
                scope(1),
                LogicalEffectClass::AnswerPreservingPhysical,
                oid(2),
                oid(3),
            ),
            Err(DecisionPolicyEpochError::PolicyIdTooLong {
                actual: MAX_POLICY_ID_BYTES + 1,
                maximum: MAX_POLICY_ID_BYTES,
            })
        );
        assert_eq!(
            DecisionPolicyEpoch::try_root(
                "policy id",
                0,
                scope(1),
                LogicalEffectClass::AnswerPreservingPhysical,
                oid(2),
                oid(3),
            ),
            Err(DecisionPolicyEpochError::NonCanonicalPolicyId { offset: 6 })
        );
    }

    #[test]
    fn evidence_references_must_be_sorted_unique() {
        assert_eq!(
            DecisionPolicyEpoch::try_from_parts(
                "policy:a",
                2,
                scope(1),
                LogicalEffectClass::AnswerAffectingExecution,
                oid(2),
                oid(3),
                &[oid(5), oid(4)],
                Some(oid(8)),
            ),
            Err(DecisionPolicyEpochError::EvidenceRefsOutOfOrder {
                index: 1,
                previous: oid(5),
                current: oid(4),
            })
        );
        assert_eq!(
            DecisionPolicyEpoch::try_from_parts(
                "policy:a",
                2,
                scope(1),
                LogicalEffectClass::AnswerAffectingExecution,
                oid(2),
                oid(3),
                &[oid(5), oid(5)],
                Some(oid(8)),
            ),
            Err(DecisionPolicyEpochError::DuplicateEvidenceRef {
                index: 1,
                evidence_oid: oid(5),
            })
        );
    }

    #[test]
    fn evidence_inventory_is_resource_bounded_before_copying() {
        let oversized = vec![oid(5); MAX_EVIDENCE_REFS + 1];
        assert_eq!(
            DecisionPolicyEpoch::try_from_parts(
                "policy:a",
                2,
                scope(1),
                LogicalEffectClass::AnswerAffectingExecution,
                oid(2),
                oid(3),
                &oversized,
                Some(oid(8)),
            ),
            Err(DecisionPolicyEpochError::TooManyEvidenceRefs {
                actual: MAX_EVIDENCE_REFS + 1,
                maximum: MAX_EVIDENCE_REFS,
            })
        );
    }

    #[test]
    fn predecessor_and_evidence_are_both_required_for_promotion_shape() {
        assert_eq!(
            DecisionPolicyEpoch::try_from_parts(
                "policy:a",
                2,
                scope(1),
                LogicalEffectClass::AnswerAffectingExecution,
                oid(2),
                oid(3),
                &[oid(5)],
                None,
            ),
            Err(DecisionPolicyEpochError::EvidenceWithoutPredecessor)
        );
        assert_eq!(
            DecisionPolicyEpoch::try_from_parts(
                "policy:a",
                2,
                scope(1),
                LogicalEffectClass::AnswerAffectingExecution,
                oid(2),
                oid(3),
                &[],
                Some(oid(8)),
            ),
            Err(DecisionPolicyEpochError::MissingPromotionEvidence)
        );
    }

    #[test]
    fn valid_promotion_inherits_identity_scope_effect_and_fallback() -> TestResult {
        let predecessor = root_with(
            "policy:a",
            41,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(20),
            oid(90),
        )?;
        let evidence_refs = [oid(10), oid(11)];
        let envelopes = [
            statistical_envelope(oid(10), oid(30), oid(90)),
            statistical_envelope(oid(11), oid(30), oid(90)),
        ];
        let promoted = DecisionPolicyEpoch::try_promote(
            &predecessor,
            oid(80),
            oid(30),
            &evidence_refs,
            &envelopes,
        )?;

        assert_eq!(promoted.policy_id(), "policy:a");
        assert_eq!(promoted.version(), 42);
        assert_eq!(promoted.scope(), scope(1));
        assert_eq!(
            promoted.logical_effect_class(),
            LogicalEffectClass::AnswerAffectingExecution
        );
        assert_eq!(promoted.pinned_table_oid(), oid(30));
        assert_eq!(promoted.fallback_oid(), oid(90));
        assert_eq!(promoted.evidence_refs(), evidence_refs);
        assert_eq!(promoted.previous_epoch_oid(), Some(oid(80)));
        Ok(())
    }

    #[test]
    fn promotion_rejects_candidate_fallback_aliasing() -> TestResult {
        let predecessor = root_with(
            "policy:a",
            4,
            scope(1),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(20),
            oid(90),
        )?;
        assert_eq!(
            DecisionPolicyEpoch::try_promote(
                &predecessor,
                oid(80),
                oid(90),
                &[oid(10)],
                &[statistical_envelope(oid(10), oid(90), oid(90))],
            ),
            Err(DecisionPolicyEpochError::CandidateEqualsFallback {
                policy_oid: oid(90),
            })
        );
        Ok(())
    }

    #[test]
    fn promotion_rejects_exhausted_and_nonconsecutive_versions() -> TestResult {
        let exhausted = root_with(
            "policy:a",
            u64::MAX,
            scope(1),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(20),
            oid(90),
        )?;
        assert_eq!(
            DecisionPolicyEpoch::try_promote(
                &exhausted,
                oid(80),
                oid(30),
                &[oid(10)],
                &[statistical_envelope(oid(10), oid(30), oid(90))],
            ),
            Err(DecisionPolicyEpochError::VersionExhausted {
                predecessor_version: u64::MAX,
            })
        );

        let predecessor = root_with(
            "policy:a",
            4,
            scope(1),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(20),
            oid(90),
        )?;
        let candidate = candidate_with(
            &predecessor,
            oid(80),
            "policy:a",
            6,
            scope(1),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(90),
            oid(10),
        )?;
        assert_eq!(
            candidate.validate_promotion_from(
                &predecessor,
                oid(80),
                &[statistical_envelope(oid(10), oid(30), oid(90))],
            ),
            Err(DecisionPolicyEpochError::NonConsecutiveVersion {
                predecessor_version: 4,
                successor_version: 6,
            })
        );
        Ok(())
    }

    #[test]
    fn promotion_rejects_policy_scope_and_effect_substitution() -> TestResult {
        let predecessor = root_with(
            "policy:a",
            4,
            scope(1),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(20),
            oid(90),
        )?;
        let envelope = [statistical_envelope(oid(10), oid(30), oid(90))];

        let changed_policy = candidate_with(
            &predecessor,
            oid(80),
            "policy:b",
            5,
            scope(1),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(90),
            oid(10),
        )?;
        assert_eq!(
            changed_policy.validate_promotion_from(&predecessor, oid(80), &envelope),
            Err(DecisionPolicyEpochError::PolicyIdChanged)
        );

        let changed_scope = candidate_with(
            &predecessor,
            oid(80),
            "policy:a",
            5,
            scope(2),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(90),
            oid(10),
        )?;
        assert_eq!(
            changed_scope.validate_promotion_from(&predecessor, oid(80), &envelope),
            Err(DecisionPolicyEpochError::ScopeChanged)
        );

        let changed_effect = candidate_with(
            &predecessor,
            oid(80),
            "policy:a",
            5,
            scope(1),
            LogicalEffectClass::CanonicalStateAffecting,
            oid(90),
            oid(10),
        )?;
        assert_eq!(
            changed_effect.validate_promotion_from(&predecessor, oid(80), &envelope),
            Err(DecisionPolicyEpochError::LogicalEffectClassChanged)
        );
        Ok(())
    }

    #[test]
    fn promotion_requires_exact_predecessor_oid_and_pinned_fallback() -> TestResult {
        let predecessor = root_with(
            "policy:a",
            4,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(20),
            oid(90),
        )?;
        let envelope = [statistical_envelope(oid(10), oid(30), oid(90))];

        let wrong_previous = candidate_with(
            &predecessor,
            oid(81),
            "policy:a",
            5,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(90),
            oid(10),
        )?;
        assert_eq!(
            wrong_previous.validate_promotion_from(&predecessor, oid(80), &envelope),
            Err(DecisionPolicyEpochError::PreviousEpochOidMismatch {
                expected: oid(80),
                actual: Some(oid(81)),
            })
        );

        let changed_fallback = candidate_with(
            &predecessor,
            oid(80),
            "policy:a",
            5,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(91),
            oid(10),
        )?;
        assert_eq!(
            changed_fallback.validate_promotion_from(&predecessor, oid(80), &envelope),
            Err(DecisionPolicyEpochError::PinnedFallbackChanged {
                expected: oid(90),
                actual: oid(91),
            })
        );
        Ok(())
    }

    #[test]
    fn root_record_cannot_validate_as_a_promotion() -> TestResult {
        let predecessor = root_with(
            "policy:a",
            4,
            scope(1),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(20),
            oid(90),
        )?;
        let candidate = root_with(
            "policy:a",
            5,
            scope(1),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(30),
            oid(90),
        )?;
        assert_eq!(
            candidate.validate_promotion_from(&predecessor, oid(80), &[]),
            Err(DecisionPolicyEpochError::MissingPredecessor)
        );
        Ok(())
    }

    #[test]
    fn evidence_list_must_match_envelopes_exactly_and_in_order() -> TestResult {
        let epoch = DecisionPolicyEpoch::try_from_parts(
            "policy:a",
            5,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(30),
            oid(90),
            &[oid(10), oid(11)],
            Some(oid(80)),
        )?;
        assert_eq!(
            epoch
                .validate_statistical_evidence(&[statistical_envelope(oid(10), oid(30), oid(90),)]),
            Err(DecisionPolicyEpochError::EvidenceEnvelopeCountMismatch {
                referenced: 2,
                supplied: 1,
            })
        );
        assert_eq!(
            epoch.validate_statistical_evidence(&[
                statistical_envelope(oid(11), oid(30), oid(90)),
                statistical_envelope(oid(10), oid(30), oid(90)),
            ]),
            Err(DecisionPolicyEpochError::EvidenceEnvelopeRefMismatch {
                index: 0,
                expected: oid(10),
                actual: oid(11),
            })
        );
        Ok(())
    }

    #[test]
    fn nonstatistical_claims_cannot_enter_promotion_evidence() -> TestResult {
        let epoch = DecisionPolicyEpoch::try_from_parts(
            "policy:a",
            5,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(30),
            oid(90),
            &[oid(10)],
            Some(oid(80)),
        )?;
        let invariant = EvidenceEnvelope::new(
            EvidenceClaim::SafetyInvariant {
                invariant_id: "FG-INV-20".into(),
            },
            oid(10),
            oid(30),
            None,
            7,
            FallbackBehavior::DeterministicPolicy {
                policy_oid: oid(90),
            },
        );

        assert_eq!(
            epoch.validate_statistical_evidence(&[invariant]),
            Err(DecisionPolicyEpochError::EvidenceClaimMustBeStatistical {
                index: 0,
                evidence_oid: oid(10),
            })
        );
        Ok(())
    }

    #[test]
    fn evidence_must_bind_exact_table_and_deterministic_fallback() -> TestResult {
        let epoch = DecisionPolicyEpoch::try_from_parts(
            "policy:a",
            5,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(30),
            oid(90),
            &[oid(10)],
            Some(oid(80)),
        )?;
        assert_eq!(
            epoch
                .validate_statistical_evidence(&[statistical_envelope(oid(10), oid(31), oid(90),)]),
            Err(DecisionPolicyEpochError::EvidenceSelectionPolicyMismatch {
                index: 0,
                expected: oid(30),
                actual: oid(31),
            })
        );
        assert_eq!(
            epoch
                .validate_statistical_evidence(&[statistical_envelope(oid(10), oid(30), oid(91),)]),
            Err(DecisionPolicyEpochError::EvidenceFallbackMismatch {
                index: 0,
                expected: oid(90),
                actual: Some(oid(91)),
            })
        );
        let fail_closed = EvidenceEnvelope::new(
            statistical_claim(),
            oid(10),
            oid(30),
            None,
            7,
            FallbackBehavior::FailClosed,
        );
        assert_eq!(
            epoch.validate_statistical_evidence(&[fail_closed]),
            Err(DecisionPolicyEpochError::EvidenceFallbackMismatch {
                index: 0,
                expected: oid(90),
                actual: None,
            })
        );
        Ok(())
    }

    #[test]
    fn canonical_bytes_are_deterministic_and_pin_the_field_layout() -> TestResult {
        let first = DecisionPolicyEpoch::try_from_parts(
            "p",
            7,
            scope(1),
            LogicalEffectClass::CanonicalStateAffecting,
            oid(2),
            oid(3),
            &[oid(4)],
            Some(oid(5)),
        )?;
        let replay = DecisionPolicyEpoch::try_from_parts(
            "p",
            7,
            scope(1),
            LogicalEffectClass::CanonicalStateAffecting,
            oid(2),
            oid(3),
            &[oid(4)],
            Some(oid(5)),
        )?;
        let first_bytes = first.try_to_canonical_bytes()?;
        let replay_bytes = replay.try_to_canonical_bytes()?;
        assert_eq!(first_bytes, replay_bytes);

        let mut expected = Vec::new();
        expected.extend_from_slice(&26u16.to_le_bytes());
        expected.extend_from_slice(b"fgdb:decision-policy-epoch");
        expected.extend_from_slice(&1u16.to_le_bytes());
        expected.push(0x01);
        expected.extend_from_slice(&8u16.to_le_bytes());
        expected.extend_from_slice(&[0x01, 1, 0, 0, 0, b'p']);
        expected.extend_from_slice(&[0x02, 8, 0, 0, 0]);
        expected.extend_from_slice(&7u64.to_le_bytes());
        expected.extend_from_slice(&[0x03, 32, 0, 0, 0]);
        expected.extend_from_slice(&[1; 32]);
        expected.extend_from_slice(&[0x04, 1, 0, 0, 0, 0x03]);
        expected.extend_from_slice(&[0x05, 32, 0, 0, 0]);
        expected.extend_from_slice(&[2; 32]);
        expected.extend_from_slice(&[0x06, 32, 0, 0, 0]);
        expected.extend_from_slice(&[3; 32]);
        expected.extend_from_slice(&[0x07, 36, 0, 0, 0]);
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&[4; 32]);
        expected.extend_from_slice(&[0x08, 33, 0, 0, 0, 1]);
        expected.extend_from_slice(&[5; 32]);
        assert_eq!(first_bytes, expected);
        Ok(())
    }

    #[test]
    fn canonical_decoders_round_trip_without_bypassing_promotion_laws() -> TestResult {
        let predecessor = root_with(
            "policy:a",
            41,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(20),
            oid(90),
        )?;
        let root_bytes = predecessor.try_to_canonical_bytes()?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&root_bytes)?,
            predecessor
        );

        let evidence_refs = [oid(10), oid(11)];
        let envelopes = [
            statistical_envelope(oid(10), oid(30), oid(90)),
            statistical_envelope(oid(11), oid(30), oid(90)),
        ];
        let promoted = DecisionPolicyEpoch::try_promote(
            &predecessor,
            oid(80),
            oid(30),
            &evidence_refs,
            &envelopes,
        )?;
        let promoted_bytes = promoted.try_to_canonical_bytes()?;

        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&promoted_bytes),
            Err(DecisionPolicyEpochError::ExpectedRootEpoch)
        );
        assert_eq!(
            DecisionPolicyEpoch::try_promoted_from_canonical_bytes(
                &promoted_bytes,
                &predecessor,
                oid(80),
                &envelopes,
            )?,
            promoted
        );
        assert_eq!(
            DecisionPolicyEpoch::try_promoted_from_canonical_bytes(
                &promoted_bytes,
                &predecessor,
                oid(81),
                &envelopes,
            ),
            Err(DecisionPolicyEpochError::PreviousEpochOidMismatch {
                expected: oid(81),
                actual: Some(oid(80)),
            })
        );
        Ok(())
    }

    #[test]
    fn canonical_decoder_rejects_every_truncated_prefix_and_trailing_bytes() -> TestResult {
        let epoch = root_with(
            "p",
            7,
            scope(1),
            LogicalEffectClass::CanonicalStateAffecting,
            oid(2),
            oid(3),
        )?;
        let encoded = epoch.try_to_canonical_bytes()?;
        for end in 0..encoded.len() {
            let truncated = encoded.get(..end).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "test truncation was outside the canonical record",
                )
            })?;
            assert!(
                DecisionPolicyEpoch::try_root_from_canonical_bytes(truncated).is_err(),
                "truncated canonical record ending at byte {end} was accepted"
            );
        }

        let mut with_trailing = encoded;
        with_trailing.push(0);
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&with_trailing),
            Err(DecisionPolicyEpochError::TrailingCanonicalBytes { count: 1 })
        );
        Ok(())
    }

    #[test]
    fn canonical_decoder_rejects_noncanonical_header_tag_and_length_variants() -> TestResult {
        let epoch = root_with(
            "p",
            7,
            scope(1),
            LogicalEffectClass::CanonicalStateAffecting,
            oid(2),
            oid(3),
        )?;
        let encoded = epoch.try_to_canonical_bytes()?;
        let version_offset = 2 + DECISION_POLICY_EPOCH_ENCODING_DOMAIN.len();
        let record_tag_offset = version_offset + 2;
        let field_count_offset = record_tag_offset + 1;
        let first_field_offset = field_count_offset + 2;
        let version_field_offset = first_field_offset + FIELD_HEADER_BYTES + 1;
        let effect_payload_offset = version_field_offset
            + FIELD_HEADER_BYTES
            + 8
            + FIELD_HEADER_BYTES
            + OBJECT_ID_BYTES
            + FIELD_HEADER_BYTES;
        let policy_id_payload_offset = first_field_offset + FIELD_HEADER_BYTES;

        let mut wrong_domain_len = encoded.clone();
        replace_test_bytes(&mut wrong_domain_len, 0, &25u16.to_le_bytes())?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&wrong_domain_len),
            Err(DecisionPolicyEpochError::CanonicalDomainLengthMismatch {
                expected: DECISION_POLICY_EPOCH_ENCODING_DOMAIN.len(),
                actual: 25,
            })
        );

        let mut wrong_domain = encoded.clone();
        replace_test_byte(&mut wrong_domain, 2, b'x')?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&wrong_domain),
            Err(DecisionPolicyEpochError::CanonicalDomainMismatch)
        );

        let mut wrong_version = encoded.clone();
        replace_test_bytes(&mut wrong_version, version_offset, &2u16.to_le_bytes())?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&wrong_version),
            Err(DecisionPolicyEpochError::UnsupportedCanonicalVersion { actual: 2 })
        );

        let mut wrong_record_tag = encoded.clone();
        replace_test_byte(&mut wrong_record_tag, record_tag_offset, 0xff)?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&wrong_record_tag),
            Err(DecisionPolicyEpochError::UnexpectedCanonicalRecordTag { actual: 0xff })
        );

        let mut wrong_field_count = encoded.clone();
        replace_test_bytes(
            &mut wrong_field_count,
            field_count_offset,
            &7u16.to_le_bytes(),
        )?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&wrong_field_count),
            Err(DecisionPolicyEpochError::UnexpectedCanonicalFieldCount { actual: 7 })
        );

        let mut wrong_field_tag = encoded.clone();
        replace_test_byte(&mut wrong_field_tag, first_field_offset, VERSION_FIELD_TAG)?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&wrong_field_tag),
            Err(DecisionPolicyEpochError::UnexpectedCanonicalFieldTag {
                index: 0,
                expected: POLICY_ID_FIELD_TAG,
                actual: VERSION_FIELD_TAG,
            })
        );

        let mut noncanonical_policy_id = encoded.clone();
        replace_test_byte(&mut noncanonical_policy_id, policy_id_payload_offset, b' ')?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&noncanonical_policy_id),
            Err(DecisionPolicyEpochError::NonCanonicalPolicyId { offset: 0 })
        );

        let mut wrong_version_len = encoded.clone();
        replace_test_bytes(
            &mut wrong_version_len,
            version_field_offset + 1,
            &7u32.to_le_bytes(),
        )?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&wrong_version_len),
            Err(DecisionPolicyEpochError::CanonicalFieldLengthMismatch {
                field_tag: VERSION_FIELD_TAG,
                expected: 8,
                actual: 7,
            })
        );

        let mut wrong_effect = encoded.clone();
        replace_test_byte(&mut wrong_effect, effect_payload_offset, 0xff)?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&wrong_effect),
            Err(DecisionPolicyEpochError::InvalidLogicalEffectClassTag { actual: 0xff })
        );

        let mut wrong_presence = encoded;
        let previous_presence_offset = wrong_presence.len().checked_sub(1).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "canonical test record unexpectedly empty",
            )
        })?;
        replace_test_byte(&mut wrong_presence, previous_presence_offset, 2)?;
        assert_eq!(
            DecisionPolicyEpoch::try_root_from_canonical_bytes(&wrong_presence),
            Err(DecisionPolicyEpochError::InvalidPreviousEpochPresenceTag { actual: 2 })
        );
        Ok(())
    }

    #[test]
    fn canonical_decoder_rejects_noncanonical_evidence_order() -> TestResult {
        let predecessor = root_with(
            "p",
            4,
            scope(1),
            LogicalEffectClass::AnswerAffectingExecution,
            oid(20),
            oid(90),
        )?;
        let evidence_refs = [oid(10), oid(11)];
        let envelopes = [
            statistical_envelope(oid(10), oid(30), oid(90)),
            statistical_envelope(oid(11), oid(30), oid(90)),
        ];
        let promoted = DecisionPolicyEpoch::try_promote(
            &predecessor,
            oid(80),
            oid(30),
            &evidence_refs,
            &envelopes,
        )?;
        let mut encoded = promoted.try_to_canonical_bytes()?;
        let prefix_len = 2 + DECISION_POLICY_EPOCH_ENCODING_DOMAIN.len() + 2 + 1 + 2;
        let fields_before_evidence = (FIELD_HEADER_BYTES + 1)
            + (FIELD_HEADER_BYTES + 8)
            + (FIELD_HEADER_BYTES + OBJECT_ID_BYTES)
            + (FIELD_HEADER_BYTES + 1)
            + (FIELD_HEADER_BYTES + OBJECT_ID_BYTES)
            + (FIELD_HEADER_BYTES + OBJECT_ID_BYTES);
        let first_evidence_oid_offset =
            prefix_len + fields_before_evidence + FIELD_HEADER_BYTES + 4;
        replace_test_bytes(&mut encoded, first_evidence_oid_offset, oid(12).as_bytes())?;

        assert_eq!(
            DecisionPolicyEpoch::try_promoted_from_canonical_bytes(
                &encoded,
                &predecessor,
                oid(80),
                &envelopes,
            ),
            Err(DecisionPolicyEpochError::EvidenceRefsOutOfOrder {
                index: 1,
                previous: oid(12),
                current: oid(11),
            })
        );
        Ok(())
    }

    #[test]
    fn field_storage_is_owned_and_access_is_read_only() -> TestResult {
        let policy_id = String::from("policy:owned");
        let evidence_refs = vec![oid(10), oid(11)];
        let epoch = DecisionPolicyEpoch::try_from_parts(
            &policy_id,
            9,
            scope(1),
            LogicalEffectClass::AnswerPreservingPhysical,
            oid(20),
            oid(90),
            &evidence_refs,
            Some(oid(80)),
        )?;
        let before = epoch.try_to_canonical_bytes()?;

        drop(policy_id);
        drop(evidence_refs);

        assert_eq!(epoch.policy_id(), "policy:owned");
        assert_eq!(epoch.evidence_refs(), &[oid(10), oid(11)]);
        assert_eq!(
            epoch.try_to_canonical_bytes(),
            Ok(before),
            "read-only access cannot alter canonical identity"
        );
        Ok(())
    }

    #[test]
    fn fallback_transition_is_consecutive_and_preserves_epoch_identity() -> TestResult {
        let predecessor = promoted_candidate_epoch()?;
        let regime_evidence = detected_regime_evidence(oid(30), oid(90))?;
        let envelopes = [regime_fallback_envelope(oid(70), oid(90))?];
        let fallback = DecisionPolicyEpoch::try_revert_to_fallback(
            &predecessor,
            oid(81),
            &[oid(70)],
            &envelopes,
            oid(70),
            &regime_evidence,
        )?;

        assert_eq!(fallback.policy_id(), predecessor.policy_id());
        assert_eq!(fallback.version(), predecessor.version() + 1);
        assert_eq!(fallback.scope(), predecessor.scope());
        assert_eq!(
            fallback.logical_effect_class(),
            predecessor.logical_effect_class()
        );
        assert_eq!(fallback.pinned_table_oid(), oid(90));
        assert_eq!(fallback.fallback_oid(), predecessor.fallback_oid());
        assert_eq!(fallback.evidence_refs(), &[oid(70)]);
        assert_eq!(fallback.previous_epoch_oid(), Some(oid(81)));
        Ok(())
    }

    #[test]
    fn fallback_transition_canonical_replay_cannot_bypass_laws() -> TestResult {
        let predecessor = promoted_candidate_epoch()?;
        let regime_evidence = detected_regime_evidence(oid(30), oid(90))?;
        let envelopes = [regime_fallback_envelope(oid(70), oid(90))?];
        let first = DecisionPolicyEpoch::try_revert_to_fallback(
            &predecessor,
            oid(81),
            &[oid(70)],
            &envelopes,
            oid(70),
            &regime_evidence,
        )?;
        let replay = DecisionPolicyEpoch::try_revert_to_fallback(
            &predecessor,
            oid(81),
            &[oid(70)],
            &envelopes,
            oid(70),
            &regime_evidence,
        )?;
        let first_bytes = first.try_to_canonical_bytes()?;
        let replay_bytes = replay.try_to_canonical_bytes()?;
        assert_eq!(first_bytes, replay_bytes);
        assert_eq!(
            DecisionPolicyEpoch::try_fallback_from_canonical_bytes(
                &first_bytes,
                &predecessor,
                oid(81),
                &envelopes,
                oid(70),
                &regime_evidence,
            )?,
            first
        );
        assert_eq!(
            DecisionPolicyEpoch::try_promoted_from_canonical_bytes(
                &first_bytes,
                &predecessor,
                oid(81),
                &envelopes,
            ),
            Err(DecisionPolicyEpochError::CandidateEqualsFallback {
                policy_oid: oid(90),
            })
        );
        Ok(())
    }

    #[test]
    fn fallback_transition_rejects_wrong_predecessor_and_regime_binding() -> TestResult {
        let predecessor = promoted_candidate_epoch()?;
        let regime_evidence = detected_regime_evidence(oid(30), oid(90))?;
        let envelopes = [regime_fallback_envelope(oid(70), oid(90))?];
        let fallback = DecisionPolicyEpoch::try_revert_to_fallback(
            &predecessor,
            oid(81),
            &[oid(70)],
            &envelopes,
            oid(70),
            &regime_evidence,
        )?;
        let bytes = fallback.try_to_canonical_bytes()?;
        assert_eq!(
            DecisionPolicyEpoch::try_fallback_from_canonical_bytes(
                &bytes,
                &predecessor,
                oid(82),
                &envelopes,
                oid(70),
                &regime_evidence,
            ),
            Err(DecisionPolicyEpochError::PreviousEpochOidMismatch {
                expected: oid(82),
                actual: Some(oid(81)),
            })
        );
        assert_eq!(
            DecisionPolicyEpoch::try_revert_to_fallback(
                &predecessor,
                oid(81),
                &[oid(70)],
                &envelopes,
                oid(71),
                &regime_evidence,
            ),
            Err(DecisionPolicyEpochError::RegimeEvidenceRefMissing {
                evidence_oid: oid(71),
            })
        );

        let quiet_regime_evidence =
            regime_evidence_for_samples(oid(30), oid(90), &[10_i64, 10, 10])?;
        assert_eq!(
            DecisionPolicyEpoch::try_revert_to_fallback(
                &predecessor,
                oid(81),
                &[oid(70)],
                &envelopes,
                oid(70),
                &quiet_regime_evidence,
            ),
            Err(DecisionPolicyEpochError::RegimeEvidenceMustReportChange {
                actual: RegimeSignalStatus::NoChangeDetected,
            })
        );

        let wrong_candidate_evidence = detected_regime_evidence(oid(31), oid(90))?;
        assert_eq!(
            DecisionPolicyEpoch::try_revert_to_fallback(
                &predecessor,
                oid(81),
                &[oid(70)],
                &envelopes,
                oid(70),
                &wrong_candidate_evidence,
            ),
            Err(DecisionPolicyEpochError::RegimeEvidenceCandidateMismatch {
                expected: oid(30),
                actual: oid(31),
            })
        );
        Ok(())
    }

    #[test]
    fn fallback_decoder_rejects_nonconsecutive_successor() -> TestResult {
        let predecessor = promoted_candidate_epoch()?;
        let regime_evidence = detected_regime_evidence(oid(30), oid(90))?;
        let envelopes = [regime_fallback_envelope(oid(70), oid(90))?];
        let nonconsecutive = DecisionPolicyEpoch::try_from_parts(
            predecessor.policy_id(),
            predecessor.version() + 2,
            predecessor.scope(),
            predecessor.logical_effect_class(),
            predecessor.fallback_oid(),
            predecessor.fallback_oid(),
            &[oid(70)],
            Some(oid(81)),
        )?;
        let bytes = nonconsecutive.try_to_canonical_bytes()?;
        assert_eq!(
            DecisionPolicyEpoch::try_fallback_from_canonical_bytes(
                &bytes,
                &predecessor,
                oid(81),
                &envelopes,
                oid(70),
                &regime_evidence,
            ),
            Err(DecisionPolicyEpochError::NonConsecutiveVersion {
                predecessor_version: predecessor.version(),
                successor_version: predecessor.version() + 2,
            })
        );
        Ok(())
    }

    #[test]
    fn fallback_alias_is_confined_to_validated_fallback_successors() -> TestResult {
        let predecessor = promoted_candidate_epoch()?;
        let regime_evidence = detected_regime_evidence(oid(30), oid(90))?;
        let envelopes = [regime_fallback_envelope(oid(70), oid(90))?];
        let candidate_root = root_with(
            "policy:fallback-transition",
            predecessor.version(),
            predecessor.scope(),
            predecessor.logical_effect_class(),
            oid(30),
            oid(90),
        )?;
        assert_eq!(
            DecisionPolicyEpoch::try_revert_to_fallback(
                &candidate_root,
                oid(81),
                &[oid(70)],
                &envelopes,
                oid(70),
                &regime_evidence,
            ),
            Err(DecisionPolicyEpochError::FallbackRequiresPromotedPredecessor)
        );

        let nonfallback_successor = DecisionPolicyEpoch::try_from_parts(
            predecessor.policy_id(),
            predecessor.version() + 1,
            predecessor.scope(),
            predecessor.logical_effect_class(),
            oid(31),
            predecessor.fallback_oid(),
            &[oid(70)],
            Some(oid(81)),
        )?;
        assert_eq!(
            nonfallback_successor.validate_fallback_from(
                &predecessor,
                oid(81),
                &envelopes,
                oid(70),
                &regime_evidence,
            ),
            Err(
                DecisionPolicyEpochError::FallbackTransitionMustSelectPinnedFallback {
                    expected: oid(90),
                    actual: oid(31),
                }
            )
        );

        let fallback = DecisionPolicyEpoch::try_revert_to_fallback(
            &predecessor,
            oid(81),
            &[oid(70)],
            &envelopes,
            oid(70),
            &regime_evidence,
        )?;
        assert_eq!(
            DecisionPolicyEpoch::try_revert_to_fallback(
                &fallback,
                oid(82),
                &[oid(71)],
                &[regime_fallback_envelope(oid(71), oid(90))?],
                oid(71),
                &regime_evidence,
            ),
            Err(
                DecisionPolicyEpochError::FallbackPredecessorAlreadyUsesPinnedFallback {
                    policy_oid: oid(90),
                }
            )
        );
        Ok(())
    }
}
