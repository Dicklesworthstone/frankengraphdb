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

use fgdb_claim::{EvidenceClaim, Justification, LatticeViolation, RegistryClaimClass};
use fgdb_types::ObjectId;

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
    use fgdb_claim::RefinementStatus;

    fn oid(fill: u8) -> ObjectId {
        ObjectId([fill; 32])
    }

    fn statistical_claim() -> EvidenceClaim {
        EvidenceClaim::StatisticalClaim {
            population: "hedged reads on fixture L".into(),
            sampling_rule: "every admission".into(),
            alpha: 0.01,
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
