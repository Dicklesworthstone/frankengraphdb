//! Evidence envelopes (§15.0).
//!
//! An [`EvidenceEnvelope`] binds an [`EvidenceClaim`](fgdb_claim::EvidenceClaim)
//! to an **immutable evidence identity**: the content address of the evidence
//! body plus the declared context that makes the claim auditable —
//! selection policy, calibration window, regime epoch, and the mandatory
//! deterministic fallback. Per the adaptive-decision contract, every field
//! here is an immutable declared identity: an envelope is never edited, only
//! superseded by a new envelope with a new identity.
//!
//! Interpretation (e-processes, conformal calibration, SPRT) belongs to
//! Sextant (`fgdb-verif-sextant`); enforcement of the claim lattice is
//! `fgdb-claim`'s. This crate only makes the binding well-typed and applies
//! the lattice at the envelope boundary: [`EvidenceEnvelope::justify`]
//! refuses to let an envelope back a registry row stronger than its claim
//! kind allows.

#![forbid(unsafe_code)]

use std::borrow::Borrow;

use fgdb_claim::{EvidenceClaim, Justification, LatticeViolation, RegistryClaimClass};
use fgdb_types::ObjectId;

/// Stable version of the replay-class vocabulary carried by completeness
/// grades. A vocabulary change is an explicit versioned contract change.
pub const REPLAY_CLASS_VOCABULARY_VERSION: u16 = 1;

/// Closed replay evidence-class vocabulary for the W1 replay contract.
///
/// Discriminants, [`ReplayClass::ALL`], and [`ReplayClass::as_str`] use the same
/// stable bytewise order. `CryptoEntropy` names evidence that policy forbids
/// recording. It may occur in omitted/redacted diagnostics, but never in a
/// reproduced/present or later-supplyable set. Other secret-bearing classes
/// classify authenticated evidence references, never secret bytes; disclosure
/// policy is enforced by the replay projection layer.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ReplayClass {
    BoundQuery = 0,
    CompilationTarget = 1,
    CpuFeatureContract = 2,
    CryptoEntropy = 3,
    DecisionCards = 4,
    DerivedGenerationSnapshots = 5,
    DifferentialPrivacySeed = 6,
    Evidence = 7,
    ExecutableBinary = 8,
    ExecutionSchedule = 9,
    ExecutionSeed = 10,
    KernelProfileRegistry = 11,
    KeyMaterial = 12,
    LanguageProfile = 13,
    LogicalState = 14,
    MediatedExternalInputs = 15,
    MediatedNondeterminism = 16,
    NormalizedQuery = 17,
    NumericProfile = 18,
    PlatformAbi = 19,
    Policies = 20,
    ReplayAuthoritySnapshot = 21,
    ReproducibleBuildClosure = 22,
    RoleBearingBindingSet = 23,
    RuntimeAllocatorConfiguration = 24,
    RustToolchain = 25,
    ScalarProfile = 26,
    SemanticProfile = 27,
    SourceTree = 28,
    StructuralControlFlow = 29,
    StructuralDataShape = 30,
    StructuralReplayProjection = 31,
    TypedParameters = 32,
    UdfModuleSet = 33,
    VmJitCompilerArtifacts = 34,
    WallClock = 35,
}

impl ReplayClass {
    pub const ALL: [Self; 36] = [
        Self::BoundQuery,
        Self::CompilationTarget,
        Self::CpuFeatureContract,
        Self::CryptoEntropy,
        Self::DecisionCards,
        Self::DerivedGenerationSnapshots,
        Self::DifferentialPrivacySeed,
        Self::Evidence,
        Self::ExecutableBinary,
        Self::ExecutionSchedule,
        Self::ExecutionSeed,
        Self::KernelProfileRegistry,
        Self::KeyMaterial,
        Self::LanguageProfile,
        Self::LogicalState,
        Self::MediatedExternalInputs,
        Self::MediatedNondeterminism,
        Self::NormalizedQuery,
        Self::NumericProfile,
        Self::PlatformAbi,
        Self::Policies,
        Self::ReplayAuthoritySnapshot,
        Self::ReproducibleBuildClosure,
        Self::RoleBearingBindingSet,
        Self::RuntimeAllocatorConfiguration,
        Self::RustToolchain,
        Self::ScalarProfile,
        Self::SemanticProfile,
        Self::SourceTree,
        Self::StructuralControlFlow,
        Self::StructuralDataShape,
        Self::StructuralReplayProjection,
        Self::TypedParameters,
        Self::UdfModuleSet,
        Self::VmJitCompilerArtifacts,
        Self::WallClock,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BoundQuery => "bound_query",
            Self::CompilationTarget => "compilation_target",
            Self::CpuFeatureContract => "cpu_feature_contract",
            Self::CryptoEntropy => "crypto_entropy",
            Self::DecisionCards => "decision_cards",
            Self::DerivedGenerationSnapshots => "derived_generation_snapshots",
            Self::DifferentialPrivacySeed => "differential_privacy_seed",
            Self::Evidence => "evidence",
            Self::ExecutableBinary => "executable_binary",
            Self::ExecutionSchedule => "execution_schedule",
            Self::ExecutionSeed => "execution_seed",
            Self::KernelProfileRegistry => "kernel_profile_registry",
            Self::KeyMaterial => "key_material",
            Self::LanguageProfile => "language_profile",
            Self::LogicalState => "logical_state",
            Self::MediatedExternalInputs => "mediated_external_inputs",
            Self::MediatedNondeterminism => "mediated_nondeterminism",
            Self::NormalizedQuery => "normalized_query",
            Self::NumericProfile => "numeric_profile",
            Self::PlatformAbi => "platform_abi",
            Self::Policies => "policies",
            Self::ReplayAuthoritySnapshot => "replay_authority_snapshot",
            Self::ReproducibleBuildClosure => "reproducible_build_closure",
            Self::RoleBearingBindingSet => "role_bearing_binding_set",
            Self::RuntimeAllocatorConfiguration => "runtime_allocator_configuration",
            Self::RustToolchain => "rust_toolchain",
            Self::ScalarProfile => "scalar_profile",
            Self::SemanticProfile => "semantic_profile",
            Self::SourceTree => "source_tree",
            Self::StructuralControlFlow => "structural_control_flow",
            Self::StructuralDataShape => "structural_data_shape",
            Self::StructuralReplayProjection => "structural_replay_projection",
            Self::TypedParameters => "typed_parameters",
            Self::UdfModuleSet => "udf_module_set",
            Self::VmJitCompilerArtifacts => "vm_jit_compiler_artifacts",
            Self::WallClock => "wall_clock",
        }
    }

    /// Whether policy permits replay evidence of this class to be present.
    pub const fn may_be_present(self) -> bool {
        !matches!(self, Self::CryptoEntropy)
    }

    const fn bit(self) -> u64 {
        1_u64 << self as u8
    }
}

const _: () = assert!(ReplayClass::ALL.len() <= u64::BITS as usize);

impl std::fmt::Display for ReplayClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Allocation-free set of replay classes.
///
/// The closed 36-class universe makes union total: two valid sets cannot
/// produce an unrepresentable result.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ReplayClassSet {
    bits: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplayClassUse {
    Present,
    MissingArtifact,
    AbsenceDiagnostic,
}

impl ReplayClassSet {
    const EMPTY: Self = Self { bits: 0 };

    fn from_strictly_ordered<I>(
        list: &'static str,
        classes: I,
        usage: ReplayClassUse,
    ) -> Result<Self, ReplayCompletenessError>
    where
        I: IntoIterator,
        I::Item: Borrow<ReplayClass>,
    {
        let mut set = Self::EMPTY;
        let mut previous = None;
        for (index, item) in classes.into_iter().enumerate() {
            let class = *item.borrow();
            if let Some(previous_class) = previous
                && previous_class >= class
            {
                let kind = if previous_class == class {
                    ClassOrderViolation::Duplicate
                } else {
                    ClassOrderViolation::OutOfOrder
                };
                return Err(ReplayCompletenessError::ClassesNotStrictlySorted {
                    list,
                    previous_index: index - 1,
                    index,
                    previous: previous_class,
                    current: class,
                    kind,
                });
            }
            if usage == ReplayClassUse::Present && !class.may_be_present() {
                return Err(ReplayCompletenessError::AbsenceOnlyClassCannotBePresent {
                    list,
                    index,
                    class,
                });
            }
            if usage == ReplayClassUse::MissingArtifact && !class.may_be_present() {
                return Err(ReplayCompletenessError::AbsenceOnlyClassCannotBeSupplied {
                    list,
                    index,
                    class,
                });
            }
            set.bits |= class.bit();
            previous = Some(class);
        }
        Ok(set)
    }

    pub const fn len(self) -> usize {
        self.bits.count_ones() as usize
    }

    pub const fn is_empty(self) -> bool {
        self.bits == 0
    }

    /// Iterates in the versioned canonical order defined by [`ReplayClass::ALL`].
    pub const fn iter(self) -> ReplayClassIter {
        ReplayClassIter {
            remaining: self.bits,
        }
    }

    pub const fn contains(self, class: ReplayClass) -> bool {
        self.bits & class.bit() != 0
    }

    const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }

    const fn intersection(self, other: Self) -> Self {
        Self {
            bits: self.bits & other.bits,
        }
    }

    const fn difference(self, other: Self) -> Self {
        Self {
            bits: self.bits & !other.bits,
        }
    }
}

/// Allocation-free iterator over a [`ReplayClassSet`].
#[derive(Clone, Copy, Debug)]
pub struct ReplayClassIter {
    remaining: u64,
}

impl Iterator for ReplayClassIter {
    type Item = ReplayClass;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let index = self.remaining.trailing_zeros() as usize;
        self.remaining &= self.remaining - 1;
        ReplayClass::ALL.get(index).copied()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.len();
        (len, Some(len))
    }
}

impl DoubleEndedIterator for ReplayClassIter {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let index = (u64::BITS - 1 - self.remaining.leading_zeros()) as usize;
        self.remaining &= !(1_u64 << index);
        ReplayClass::ALL.get(index).copied()
    }
}

impl ExactSizeIterator for ReplayClassIter {
    fn len(&self) -> usize {
        self.remaining.count_ones() as usize
    }
}

impl std::iter::FusedIterator for ReplayClassIter {}

/// Why a completeness value could not be constructed without normalization or
/// information loss.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ReplayCompletenessError {
    EmptyClassSet {
        list: &'static str,
    },
    ClassesNotStrictlySorted {
        list: &'static str,
        previous_index: usize,
        index: usize,
        previous: ReplayClass,
        current: ReplayClass,
        kind: ClassOrderViolation,
    },
    AbsenceOnlyClassCannotBePresent {
        list: &'static str,
        index: usize,
        class: ReplayClass,
    },
    AbsenceOnlyClassCannotBeSupplied {
        list: &'static str,
        index: usize,
        class: ReplayClass,
    },
    StructuralClassOverlap {
        class: ReplayClass,
    },
}

/// The two ways an input list can violate strict canonical ordering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClassOrderViolation {
    Duplicate,
    OutOfOrder,
}

impl std::fmt::Display for ReplayCompletenessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyClassSet { list } => {
                write!(f, "{list} must contain at least one class")
            }
            Self::ClassesNotStrictlySorted {
                list,
                previous_index,
                index,
                previous,
                current,
                kind,
            } => write!(
                f,
                "{list}[{previous_index}]={previous} and {list}[{index}]={current} violate strict ordering ({kind:?})"
            ),
            Self::AbsenceOnlyClassCannotBePresent { list, index, class } => write!(
                f,
                "{list}[{index}]={class} is an absence-only diagnostic class and cannot be reproduced or present"
            ),
            Self::AbsenceOnlyClassCannotBeSupplied { list, index, class } => write!(
                f,
                "{list}[{index}]={class} is an absence-only diagnostic class and cannot be supplied later"
            ),
            Self::StructuralClassOverlap { class } => write!(
                f,
                "structural replay class {class} is both reproduced and omitted"
            ),
        }
    }
}

impl std::error::Error for ReplayCompletenessError {}

/// Validated payload of the `StructuralReplay` completeness arm.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StructuralReplayCompleteness {
    reproduced_classes: ReplayClassSet,
    omitted_classes: ReplayClassSet,
}

impl StructuralReplayCompleteness {
    pub fn reproduced_classes(&self) -> &ReplayClassSet {
        &self.reproduced_classes
    }

    pub fn omitted_classes(&self) -> &ReplayClassSet {
        &self.omitted_classes
    }
}

/// Validated payload of the `VerifiableIfArtifactsSupplied` arm.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct MissingArtifactCompleteness {
    missing_classes: ReplayClassSet,
}

impl MissingArtifactCompleteness {
    pub fn missing_classes(&self) -> &ReplayClassSet {
        &self.missing_classes
    }
}

/// Validated payload of the `AuditOnly` completeness arm.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AuditOnlyCompleteness {
    missing_or_redacted_classes: ReplayClassSet,
}

impl AuditOnlyCompleteness {
    pub fn missing_or_redacted_classes(&self) -> &ReplayClassSet {
        &self.missing_or_redacted_classes
    }
}

/// The exact four production-incident replay grades from §15.1.
///
/// Payloads have private fields and are obtainable only through shape-validating
/// constructors. This declaration is not proof of slot completeness: the
/// versioned `ReplaySlotSchema` owned by the replay subsystem must derive and
/// verify it against the required slot matrix before it is trusted or encoded.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ReplayCompleteness {
    Replayable,
    StructuralReplay(StructuralReplayCompleteness),
    VerifiableIfArtifactsSupplied(MissingArtifactCompleteness),
    AuditOnly(AuditOnlyCompleteness),
}

impl ReplayCompleteness {
    pub const fn replayable() -> Self {
        Self::Replayable
    }

    pub fn structural_replay<R, O>(
        reproduced_classes: R,
        omitted_classes: O,
    ) -> Result<Self, ReplayCompletenessError>
    where
        R: IntoIterator,
        R::Item: Borrow<ReplayClass>,
        O: IntoIterator,
        O::Item: Borrow<ReplayClass>,
    {
        let reproduced_classes = ReplayClassSet::from_strictly_ordered(
            "reproduced_classes",
            reproduced_classes,
            ReplayClassUse::Present,
        )?;
        let omitted_classes = ReplayClassSet::from_strictly_ordered(
            "omitted_classes",
            omitted_classes,
            ReplayClassUse::AbsenceDiagnostic,
        )?;

        if reproduced_classes.is_empty() {
            return Err(ReplayCompletenessError::EmptyClassSet {
                list: "reproduced_classes",
            });
        }
        if omitted_classes.is_empty() {
            return Err(ReplayCompletenessError::EmptyClassSet {
                list: "omitted_classes",
            });
        }

        if let Some(class) = reproduced_classes
            .intersection(omitted_classes)
            .iter()
            .next()
        {
            return Err(ReplayCompletenessError::StructuralClassOverlap { class });
        }

        Ok(Self::StructuralReplay(StructuralReplayCompleteness {
            reproduced_classes,
            omitted_classes,
        }))
    }

    pub fn verifiable_if_artifacts_supplied<I>(
        missing_classes: I,
    ) -> Result<Self, ReplayCompletenessError>
    where
        I: IntoIterator,
        I::Item: Borrow<ReplayClass>,
    {
        let missing_classes = ReplayClassSet::from_strictly_ordered(
            "missing_classes",
            missing_classes,
            ReplayClassUse::MissingArtifact,
        )?;
        if missing_classes.is_empty() {
            return Err(ReplayCompletenessError::EmptyClassSet {
                list: "missing_classes",
            });
        }
        Ok(Self::VerifiableIfArtifactsSupplied(
            MissingArtifactCompleteness { missing_classes },
        ))
    }

    pub fn audit_only<I>(missing_or_redacted_classes: I) -> Result<Self, ReplayCompletenessError>
    where
        I: IntoIterator,
        I::Item: Borrow<ReplayClass>,
    {
        let missing_or_redacted_classes = ReplayClassSet::from_strictly_ordered(
            "missing_or_redacted_classes",
            missing_or_redacted_classes,
            ReplayClassUse::AbsenceDiagnostic,
        )?;
        if missing_or_redacted_classes.is_empty() {
            return Err(ReplayCompletenessError::EmptyClassSet {
                list: "missing_or_redacted_classes",
            });
        }
        Ok(Self::AuditOnly(AuditOnlyCompleteness {
            missing_or_redacted_classes,
        }))
    }

    pub fn reproduced_classes(&self) -> Option<&ReplayClassSet> {
        match self {
            Self::StructuralReplay(value) => Some(value.reproduced_classes()),
            _ => None,
        }
    }

    pub fn omitted_classes(&self) -> Option<&ReplayClassSet> {
        match self {
            Self::StructuralReplay(value) => Some(value.omitted_classes()),
            _ => None,
        }
    }

    pub fn missing_classes(&self) -> Option<&ReplayClassSet> {
        match self {
            Self::VerifiableIfArtifactsSupplied(value) => Some(value.missing_classes()),
            _ => None,
        }
    }

    pub fn missing_or_redacted_classes(&self) -> Option<&ReplayClassSet> {
        match self {
            Self::AuditOnly(value) => Some(value.missing_or_redacted_classes()),
            _ => None,
        }
    }

    /// Total, conservative meet of two independently derived summaries.
    ///
    /// Same-grade gaps union. Two structural grades retain only jointly
    /// reproduced classes and union only actual omissions; a one-sided
    /// reproduction that neither input omits is dropped from the joint
    /// guarantee rather than mislabeled as missing. A structural omission can
    /// be either unavailable or redacted, so meeting structural replay with a
    /// later-supplyable grade must conservatively produce `AuditOnly`; treating
    /// every structural omission as supplyable would overclaim. This operation
    /// is commutative, associative, and idempotent.
    pub fn meet(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Replayable, value) | (value, Self::Replayable) => *value,
            (Self::StructuralReplay(left), Self::StructuralReplay(right)) => {
                let reproduced_classes = left
                    .reproduced_classes
                    .intersection(right.reproduced_classes);
                let omitted_classes = left
                    .omitted_classes
                    .union(right.omitted_classes)
                    .difference(reproduced_classes);
                if reproduced_classes.is_empty() {
                    return Self::AuditOnly(AuditOnlyCompleteness {
                        missing_or_redacted_classes: omitted_classes,
                    });
                }
                Self::StructuralReplay(StructuralReplayCompleteness {
                    reproduced_classes,
                    omitted_classes,
                })
            }
            (
                Self::VerifiableIfArtifactsSupplied(left),
                Self::VerifiableIfArtifactsSupplied(right),
            ) => Self::VerifiableIfArtifactsSupplied(MissingArtifactCompleteness {
                missing_classes: left.missing_classes.union(right.missing_classes),
            }),
            (Self::AuditOnly(left), Self::AuditOnly(right)) => {
                Self::AuditOnly(AuditOnlyCompleteness {
                    missing_or_redacted_classes: left
                        .missing_or_redacted_classes
                        .union(right.missing_or_redacted_classes),
                })
            }
            (Self::StructuralReplay(_), Self::VerifiableIfArtifactsSupplied(_))
            | (Self::VerifiableIfArtifactsSupplied(_), Self::StructuralReplay(_)) => {
                Self::AuditOnly(AuditOnlyCompleteness {
                    missing_or_redacted_classes: self.actual_gaps().union(other.actual_gaps()),
                })
            }
            (Self::AuditOnly(_), _) | (_, Self::AuditOnly(_)) => {
                Self::AuditOnly(AuditOnlyCompleteness {
                    missing_or_redacted_classes: self.actual_gaps().union(other.actual_gaps()),
                })
            }
        }
    }

    /// Applies a redaction conservatively. Any actual redaction drops the
    /// result to `AuditOnly`; an empty redaction set is an identity operation.
    pub fn weaken_for_redaction<I>(
        &self,
        redacted_classes: I,
    ) -> Result<Self, ReplayCompletenessError>
    where
        I: IntoIterator,
        I::Item: Borrow<ReplayClass>,
    {
        let redacted_classes = ReplayClassSet::from_strictly_ordered(
            "redacted_classes",
            redacted_classes,
            ReplayClassUse::AbsenceDiagnostic,
        )?;
        if redacted_classes.is_empty() {
            return Ok(*self);
        }
        let gaps = self.actual_gaps();
        Ok(Self::AuditOnly(AuditOnlyCompleteness {
            missing_or_redacted_classes: gaps.union(redacted_classes),
        }))
    }

    fn actual_gaps(&self) -> ReplayClassSet {
        match self {
            Self::Replayable => ReplayClassSet::EMPTY,
            Self::StructuralReplay(value) => value.omitted_classes,
            Self::VerifiableIfArtifactsSupplied(value) => value.missing_classes,
            Self::AuditOnly(value) => value.missing_or_redacted_classes,
        }
    }
}

/// Closed, half-open sample window the evidence was computed over, in commit
/// sequences of the subject database (`[start_seq, end_seq)`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CalibrationWindow {
    pub start_seq: u64,
    pub end_seq: u64,
}

impl CalibrationWindow {
    /// Rejects empty/inverted windows instead of normalizing them.
    pub fn new(start_seq: u64, end_seq: u64) -> Result<Self, InvalidWindow> {
        if start_seq >= end_seq {
            return Err(InvalidWindow { start_seq, end_seq });
        }
        Ok(CalibrationWindow { start_seq, end_seq })
    }
}

/// Typed rejection of an empty or inverted calibration window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct InvalidWindow {
    pub start_seq: u64,
    pub end_seq: u64,
}

impl std::fmt::Display for InvalidWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "calibration window [{}, {}) is empty or inverted",
            self.start_seq, self.end_seq
        )
    }
}

impl std::error::Error for InvalidWindow {}

/// What consumers must do when this evidence is absent, stale, or its regime
/// epoch has rolled: the deterministic fallback is part of the evidence
/// identity, never an ambient runtime choice (adaptive-decision contract —
/// "no adaptive controller ships without its conservative deterministic
/// fallback").
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FallbackBehavior {
    /// Fall back to the named pinned deterministic policy.
    DeterministicPolicy { policy_oid: ObjectId },
    /// Refuse the guarded action entirely.
    FailClosed,
}

/// Immutable binding of one evidence claim to its identity and declared
/// context. Fields are read-only after construction (no setters, no `mut`
/// accessors) — supersession means a new envelope.
#[derive(Clone, PartialEq, Debug)]
pub struct EvidenceEnvelope {
    claim: EvidenceClaim,
    evidence_oid: ObjectId,
    selection_policy_oid: ObjectId,
    calibration_window: Option<CalibrationWindow>,
    regime_epoch: u64,
    fallback: FallbackBehavior,
}

impl EvidenceEnvelope {
    pub fn new(
        claim: EvidenceClaim,
        evidence_oid: ObjectId,
        selection_policy_oid: ObjectId,
        calibration_window: Option<CalibrationWindow>,
        regime_epoch: u64,
        fallback: FallbackBehavior,
    ) -> Self {
        EvidenceEnvelope {
            claim,
            evidence_oid,
            selection_policy_oid,
            calibration_window,
            regime_epoch,
            fallback,
        }
    }

    pub fn claim(&self) -> &EvidenceClaim {
        &self.claim
    }
    pub fn evidence_oid(&self) -> ObjectId {
        self.evidence_oid
    }
    pub fn selection_policy_oid(&self) -> ObjectId {
        self.selection_policy_oid
    }
    pub fn calibration_window(&self) -> Option<CalibrationWindow> {
        self.calibration_window
    }
    pub fn regime_epoch(&self) -> u64 {
        self.regime_epoch
    }
    pub fn fallback(&self) -> FallbackBehavior {
        self.fallback
    }

    /// The lattice at the envelope boundary: may this envelope back a
    /// registry row of class `target`? Statistical/empirical envelopes can
    /// never back invariants — a typed rejection, not a warning.
    pub fn justify(&self, target: RegistryClaimClass) -> Result<Justification, LatticeViolation> {
        self.claim.max_registry_class().try_justify(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_claim::{RefinementStatus, StatisticalErrorControl};

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    fn class_strings(classes: &ReplayClassSet) -> Vec<&'static str> {
        classes.iter().map(ReplayClass::as_str).collect()
    }

    #[test]
    fn replay_completeness_has_exactly_four_validated_arms() {
        let replayable = ReplayCompleteness::replayable();
        assert!(matches!(replayable, ReplayCompleteness::Replayable));
        assert!(replayable.reproduced_classes().is_none());
        assert!(replayable.omitted_classes().is_none());
        assert!(replayable.missing_classes().is_none());
        assert!(replayable.missing_or_redacted_classes().is_none());

        let structural = ReplayCompleteness::structural_replay(
            [
                ReplayClass::LogicalState,
                ReplayClass::StructuralControlFlow,
                ReplayClass::StructuralDataShape,
            ],
            [ReplayClass::ExecutionSchedule, ReplayClass::WallClock],
        )
        .unwrap();
        assert_eq!(
            class_strings(structural.reproduced_classes().unwrap()),
            [
                "logical_state",
                "structural_control_flow",
                "structural_data_shape"
            ]
        );
        assert_eq!(
            class_strings(structural.omitted_classes().unwrap()),
            ["execution_schedule", "wall_clock"]
        );

        let verifiable = ReplayCompleteness::verifiable_if_artifacts_supplied([
            ReplayClass::ExecutableBinary,
            ReplayClass::RustToolchain,
        ])
        .unwrap();
        assert_eq!(
            class_strings(verifiable.missing_classes().unwrap()),
            ["executable_binary", "rust_toolchain"]
        );

        let audit =
            ReplayCompleteness::audit_only([ReplayClass::KeyMaterial, ReplayClass::UdfModuleSet])
                .unwrap();
        assert_eq!(
            class_strings(audit.missing_or_redacted_classes().unwrap()),
            ["key_material", "udf_module_set"]
        );
    }

    #[test]
    fn replay_class_vocabulary_is_versioned_complete_and_canonically_ordered() {
        const EXPECTED_NAMES: [&str; 36] = [
            "bound_query",
            "compilation_target",
            "cpu_feature_contract",
            "crypto_entropy",
            "decision_cards",
            "derived_generation_snapshots",
            "differential_privacy_seed",
            "evidence",
            "executable_binary",
            "execution_schedule",
            "execution_seed",
            "kernel_profile_registry",
            "key_material",
            "language_profile",
            "logical_state",
            "mediated_external_inputs",
            "mediated_nondeterminism",
            "normalized_query",
            "numeric_profile",
            "platform_abi",
            "policies",
            "replay_authority_snapshot",
            "reproducible_build_closure",
            "role_bearing_binding_set",
            "runtime_allocator_configuration",
            "rust_toolchain",
            "scalar_profile",
            "semantic_profile",
            "source_tree",
            "structural_control_flow",
            "structural_data_shape",
            "structural_replay_projection",
            "typed_parameters",
            "udf_module_set",
            "vm_jit_compiler_artifacts",
            "wall_clock",
        ];

        assert_eq!(REPLAY_CLASS_VOCABULARY_VERSION, 1);
        assert_eq!(ReplayClass::ALL.len(), 36);
        assert_eq!(ReplayClass::ALL.map(ReplayClass::as_str), EXPECTED_NAMES);
        for (index, class) in ReplayClass::ALL.into_iter().enumerate() {
            assert_eq!(class as usize, index);
        }
        for pair in ReplayClass::ALL.windows(2) {
            assert!(pair[0] < pair[1]);
            assert!(pair[0].as_str() < pair[1].as_str());
        }

        let all = ReplayCompleteness::audit_only(ReplayClass::ALL).unwrap();
        let all_set = all.missing_or_redacted_classes().unwrap();
        assert_eq!(all_set.len(), ReplayClass::ALL.len());
        assert_eq!(all_set.iter().collect::<Vec<_>>(), ReplayClass::ALL);
        assert_eq!(all_set.iter().next_back(), Some(ReplayClass::WallClock));
    }

    #[test]
    fn class_lists_reject_duplicate_and_out_of_order_input() {
        let duplicate =
            ReplayCompleteness::audit_only([ReplayClass::LogicalState, ReplayClass::LogicalState])
                .unwrap_err();
        assert!(matches!(
            duplicate,
            ReplayCompletenessError::ClassesNotStrictlySorted {
                kind: ClassOrderViolation::Duplicate,
                ..
            }
        ));

        let unsorted =
            ReplayCompleteness::audit_only([ReplayClass::WallClock, ReplayClass::LogicalState])
                .unwrap_err();
        assert!(matches!(
            unsorted,
            ReplayCompletenessError::ClassesNotStrictlySorted {
                kind: ClassOrderViolation::OutOfOrder,
                ..
            }
        ));
    }

    #[test]
    fn weaker_grade_payloads_cannot_alias_stronger_grades_with_empty_sets() {
        assert_eq!(
            ReplayCompleteness::structural_replay(
                std::iter::empty::<ReplayClass>(),
                [ReplayClass::WallClock]
            ),
            Err(ReplayCompletenessError::EmptyClassSet {
                list: "reproduced_classes"
            })
        );
        assert_eq!(
            ReplayCompleteness::structural_replay(
                [ReplayClass::StructuralControlFlow],
                std::iter::empty::<ReplayClass>()
            ),
            Err(ReplayCompletenessError::EmptyClassSet {
                list: "omitted_classes"
            })
        );
        assert_eq!(
            ReplayCompleteness::verifiable_if_artifacts_supplied(std::iter::empty::<ReplayClass>()),
            Err(ReplayCompletenessError::EmptyClassSet {
                list: "missing_classes"
            })
        );
        assert_eq!(
            ReplayCompleteness::audit_only(std::iter::empty::<ReplayClass>()),
            Err(ReplayCompletenessError::EmptyClassSet {
                list: "missing_or_redacted_classes"
            })
        );
    }

    #[test]
    fn structural_sets_must_be_disjoint() {
        let error = ReplayCompleteness::structural_replay(
            [ReplayClass::LogicalState, ReplayClass::StructuralDataShape],
            [ReplayClass::StructuralDataShape, ReplayClass::WallClock],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ReplayCompletenessError::StructuralClassOverlap {
                class: ReplayClass::StructuralDataShape
            }
        ));
    }

    #[test]
    fn crypto_entropy_is_absence_only_but_remains_an_honest_diagnostic() {
        let error = ReplayCompleteness::structural_replay(
            [ReplayClass::CryptoEntropy],
            [ReplayClass::WallClock],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ReplayCompletenessError::AbsenceOnlyClassCannotBePresent {
                list: "reproduced_classes",
                index: 0,
                class: ReplayClass::CryptoEntropy,
            }
        ));

        let structural = ReplayCompleteness::structural_replay(
            [ReplayClass::StructuralControlFlow],
            [ReplayClass::CryptoEntropy],
        )
        .unwrap();
        assert!(
            structural
                .omitted_classes()
                .unwrap()
                .contains(ReplayClass::CryptoEntropy)
        );
        assert!(matches!(
            ReplayCompleteness::verifiable_if_artifacts_supplied([ReplayClass::CryptoEntropy]),
            Err(ReplayCompletenessError::AbsenceOnlyClassCannotBeSupplied {
                list: "missing_classes",
                index: 0,
                class: ReplayClass::CryptoEntropy,
            })
        ));
        let audit = ReplayCompleteness::audit_only([ReplayClass::CryptoEntropy]).unwrap();
        assert!(
            audit
                .missing_or_redacted_classes()
                .unwrap()
                .contains(ReplayClass::CryptoEntropy)
        );
    }

    #[test]
    fn protected_secret_references_are_distinct_from_exported_secret_bytes() {
        let structural = ReplayCompleteness::structural_replay(
            [
                ReplayClass::DifferentialPrivacySeed,
                ReplayClass::KeyMaterial,
            ],
            [ReplayClass::WallClock],
        )
        .unwrap();
        assert_eq!(structural.reproduced_classes().unwrap().len(), 2);

        let verifiable = ReplayCompleteness::verifiable_if_artifacts_supplied([
            ReplayClass::DifferentialPrivacySeed,
            ReplayClass::KeyMaterial,
        ])
        .unwrap();
        assert_eq!(verifiable.missing_classes().unwrap().len(), 2);
    }

    #[test]
    fn redaction_is_total_after_order_validation_and_uses_actual_gaps() {
        let replayable = ReplayCompleteness::replayable();
        let redacted = replayable
            .weaken_for_redaction([ReplayClass::KeyMaterial])
            .unwrap();
        assert!(matches!(redacted, ReplayCompleteness::AuditOnly(_)));
        assert_eq!(
            class_strings(redacted.missing_or_redacted_classes().unwrap()),
            ["key_material"]
        );

        let structural = ReplayCompleteness::structural_replay(
            [ReplayClass::LogicalState],
            [ReplayClass::WallClock],
        )
        .unwrap();
        let redacted = structural
            .weaken_for_redaction([ReplayClass::KeyMaterial])
            .unwrap();
        assert_eq!(
            class_strings(redacted.missing_or_redacted_classes().unwrap()),
            ["key_material", "wall_clock"]
        );

        let verifiable =
            ReplayCompleteness::verifiable_if_artifacts_supplied([ReplayClass::ExecutableBinary])
                .unwrap();
        let redacted = verifiable
            .weaken_for_redaction([ReplayClass::CryptoEntropy])
            .unwrap();
        assert_eq!(
            class_strings(redacted.missing_or_redacted_classes().unwrap()),
            ["crypto_entropy", "executable_binary"]
        );

        let audit = ReplayCompleteness::audit_only([ReplayClass::DifferentialPrivacySeed]).unwrap();
        let redacted = audit
            .weaken_for_redaction([ReplayClass::KeyMaterial])
            .unwrap();
        assert_eq!(
            class_strings(redacted.missing_or_redacted_classes().unwrap()),
            ["differential_privacy_seed", "key_material"]
        );

        assert_eq!(
            replayable.weaken_for_redaction(std::iter::empty::<ReplayClass>()),
            Ok(replayable)
        );
    }

    #[test]
    fn structural_meet_keeps_only_shared_reproduced_classes() {
        let left = ReplayCompleteness::structural_replay(
            [
                ReplayClass::LogicalState,
                ReplayClass::StructuralControlFlow,
            ],
            [ReplayClass::ExecutionSchedule, ReplayClass::WallClock],
        )
        .unwrap();
        let right = ReplayCompleteness::structural_replay(
            [ReplayClass::LogicalState, ReplayClass::StructuralDataShape],
            [
                ReplayClass::MediatedExternalInputs,
                ReplayClass::StructuralControlFlow,
            ],
        )
        .unwrap();
        let meet = left.meet(&right);
        assert_eq!(
            class_strings(meet.reproduced_classes().unwrap()),
            ["logical_state"]
        );
        assert_eq!(
            class_strings(meet.omitted_classes().unwrap()),
            [
                "execution_schedule",
                "mediated_external_inputs",
                "structural_control_flow",
                "wall_clock"
            ]
        );
        assert!(
            !meet
                .omitted_classes()
                .unwrap()
                .contains(ReplayClass::StructuralDataShape)
        );
    }

    #[test]
    fn structural_meet_with_no_shared_reproduction_downgrades_to_audit() {
        let left = ReplayCompleteness::structural_replay(
            [ReplayClass::StructuralControlFlow],
            [ReplayClass::WallClock],
        )
        .unwrap();
        let right = ReplayCompleteness::structural_replay(
            [ReplayClass::StructuralDataShape],
            [ReplayClass::ExecutionSchedule],
        )
        .unwrap();
        let meet = left.meet(&right);
        assert!(matches!(meet, ReplayCompleteness::AuditOnly(_)));
        assert_eq!(
            class_strings(meet.missing_or_redacted_classes().unwrap()),
            ["execution_schedule", "wall_clock"]
        );
    }

    #[test]
    fn meet_is_commutative_associative_and_idempotent() {
        let values = [
            ReplayCompleteness::replayable(),
            ReplayCompleteness::structural_replay(
                [
                    ReplayClass::LogicalState,
                    ReplayClass::StructuralControlFlow,
                ],
                [ReplayClass::ExecutionSchedule],
            )
            .unwrap(),
            ReplayCompleteness::structural_replay(
                [ReplayClass::LogicalState, ReplayClass::StructuralDataShape],
                [ReplayClass::WallClock],
            )
            .unwrap(),
            ReplayCompleteness::structural_replay(
                [ReplayClass::StructuralControlFlow],
                [ReplayClass::RuntimeAllocatorConfiguration],
            )
            .unwrap(),
            ReplayCompleteness::structural_replay(
                [ReplayClass::LogicalState],
                [ReplayClass::StructuralControlFlow],
            )
            .unwrap(),
            ReplayCompleteness::structural_replay(
                [ReplayClass::StructuralControlFlow],
                [ReplayClass::LogicalState],
            )
            .unwrap(),
            ReplayCompleteness::verifiable_if_artifacts_supplied([ReplayClass::ExecutableBinary])
                .unwrap(),
            ReplayCompleteness::verifiable_if_artifacts_supplied([ReplayClass::LogicalState])
                .unwrap(),
            ReplayCompleteness::verifiable_if_artifacts_supplied([ReplayClass::RustToolchain])
                .unwrap(),
            ReplayCompleteness::audit_only([ReplayClass::KeyMaterial]).unwrap(),
            ReplayCompleteness::audit_only(ReplayClass::ALL).unwrap(),
        ];

        for left in &values {
            assert_eq!(left.meet(left), *left);
            for right in &values {
                assert_eq!(left.meet(right), right.meet(left));
                for third in &values {
                    let left_associated = left.meet(right).meet(third);
                    let right_associated = left.meet(&right.meet(third));
                    assert_eq!(left_associated, right_associated);
                }
            }
        }
    }

    #[test]
    fn mixed_meet_is_conservative_and_carries_only_actual_gaps() {
        let structural = ReplayCompleteness::structural_replay(
            [
                ReplayClass::LogicalState,
                ReplayClass::StructuralControlFlow,
            ],
            [ReplayClass::WallClock],
        )
        .unwrap();
        let verifiable =
            ReplayCompleteness::verifiable_if_artifacts_supplied([ReplayClass::ExecutableBinary])
                .unwrap();
        let meet = structural.meet(&verifiable);
        assert!(matches!(meet, ReplayCompleteness::AuditOnly(_)));
        assert_eq!(
            class_strings(meet.missing_or_redacted_classes().unwrap()),
            ["executable_binary", "wall_clock"]
        );

        let audit = ReplayCompleteness::audit_only([ReplayClass::KeyMaterial]).unwrap();
        let meet = structural.meet(&audit);
        assert_eq!(
            class_strings(meet.missing_or_redacted_classes().unwrap()),
            ["key_material", "wall_clock"]
        );
        assert!(
            !meet
                .missing_or_redacted_classes()
                .unwrap()
                .contains(ReplayClass::LogicalState)
        );
        assert!(
            !meet
                .missing_or_redacted_classes()
                .unwrap()
                .contains(ReplayClass::StructuralControlFlow)
        );
    }

    #[test]
    fn full_universe_meet_and_redaction_remain_total() {
        let all = ReplayCompleteness::audit_only(ReplayClass::ALL).unwrap();
        let verifiable = ReplayCompleteness::verifiable_if_artifacts_supplied(
            ReplayClass::ALL
                .into_iter()
                .filter(|class| class.may_be_present()),
        )
        .unwrap();
        assert_eq!(all.meet(&verifiable), all);
        assert_eq!(
            all.weaken_for_redaction([ReplayClass::CryptoEntropy]),
            Ok(all)
        );
    }

    fn statistical_claim() -> EvidenceClaim {
        EvidenceClaim::StatisticalClaim {
            population: "hedged reads on fixture L".into(),
            sampling_rule: "every admission".into(),
            error_control: StatisticalErrorControl::try_alpha(0.01).unwrap(),
            power_or_effective_sample_size: "n_eff=52_000".into(),
            assumptions: vec!["per-epoch exchangeability".into()],
        }
    }

    #[test]
    fn windows_reject_empty_and_inverted() {
        assert!(CalibrationWindow::new(10, 20).is_ok());
        assert_eq!(
            CalibrationWindow::new(20, 10).unwrap_err(),
            InvalidWindow {
                start_seq: 20,
                end_seq: 10
            }
        );
        let err = CalibrationWindow::new(5, 5).unwrap_err();
        assert_eq!(
            err.to_string(),
            "calibration window [5, 5) is empty or inverted"
        );
    }

    #[test]
    fn envelope_binds_immutable_declared_context() {
        let window = CalibrationWindow::new(100, 42_000).unwrap();
        let env = EvidenceEnvelope::new(
            statistical_claim(),
            oid(1),
            oid(2),
            Some(window),
            7,
            FallbackBehavior::DeterministicPolicy { policy_oid: oid(3) },
        );
        assert_eq!(env.evidence_oid(), oid(1));
        assert_eq!(env.selection_policy_oid(), oid(2));
        assert_eq!(env.calibration_window(), Some(window));
        assert_eq!(env.regime_epoch(), 7);
        assert_eq!(
            env.fallback(),
            FallbackBehavior::DeterministicPolicy { policy_oid: oid(3) }
        );
    }

    #[test]
    fn statistical_envelope_cannot_back_an_invariant_row() {
        let env = EvidenceEnvelope::new(
            statistical_claim(),
            oid(1),
            oid(2),
            None,
            1,
            FallbackBehavior::FailClosed,
        );
        // Fine at its own level and below…
        assert!(env.justify(RegistryClaimClass::Statistical).is_ok());
        assert!(env.justify(RegistryClaimClass::Slo).is_ok());
        // …typed rejection above it.
        let err = env.justify(RegistryClaimClass::Invariant).unwrap_err();
        assert_eq!(err.evidence, RegistryClaimClass::Statistical);
        assert_eq!(err.target, RegistryClaimClass::Invariant);
    }

    #[test]
    fn refined_formal_envelope_backs_proof_but_not_invariant() {
        let env = EvidenceEnvelope::new(
            EvidenceClaim::FormalModelClaim {
                model_name: "block-level SSI safety (Lean)".into(),
                abstraction_boundary: "block granularity, no I/O model".into(),
                checked_bounds: None,
                refinement_status: RefinementStatus::RefinedToImplementation,
            },
            oid(4),
            oid(5),
            None,
            1,
            FallbackBehavior::FailClosed,
        );
        assert!(env.justify(RegistryClaimClass::Proof).is_ok());
        assert!(env.justify(RegistryClaimClass::Invariant).is_err());
    }
}
