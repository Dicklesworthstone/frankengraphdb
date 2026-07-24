//! The claim-type constitution (§1 constraint 11, §15.0) as a type system.
//!
//! Two closed vocabularies live here, matching `registries/constitution.toml`
//! byte-for-byte in names:
//!
//! 1. **Registry claim classes** — `invariant | proof | bounded_model |
//!    statistical | slo | benchmark`, with the lattice law: *a weaker claim
//!    class may inform policy but may not enforce or justify a stronger
//!    one*. At the type level ([`justify`], [`Justified`]) an illegal
//!    justification is a **compile error**; at the value level
//!    ([`RegistryClaimClass::try_justify`]) it is a typed rejection carrying
//!    both classes — never a boolean.
//! 2. **Evidence claim classes** (§15.0) — [`EvidenceClaim`]: the five
//!    envelope-level kinds (`SafetyInvariant`, `FormalModelClaim`,
//!    `StatisticalClaim`, `ConfigurationModelClaim`, `EmpiricalGate`) with
//!    their mandatory declared context.
//!
//! The routing law is also here ([`RegistryClaimClass::registry_route`]):
//! `invariants.toml` carries only exact safety/liveness invariants;
//! statistical and empirical claims go to `evidence.toml` / `slo.toml`.

#![forbid(unsafe_code)]

/// The six registry claim classes, strongest first — the declaration order
/// of `registries/constitution.toml`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RegistryClaimClass {
    /// Must hold on every execution; failure is a correctness defect.
    Invariant,
    /// Theorem within a named formal model.
    Proof,
    /// Checked only within declared bounds.
    BoundedModel,
    /// Confidence statement under named assumptions.
    Statistical,
    /// Empirical operational target.
    Slo,
    /// Measured on a pinned fixture under a pinned benchmark manifest.
    Benchmark,
}

impl RegistryClaimClass {
    /// Registry spelling (`constitution.toml` `[[claim_class]].name`).
    pub const fn name(self) -> &'static str {
        match self {
            RegistryClaimClass::Invariant => "invariant",
            RegistryClaimClass::Proof => "proof",
            RegistryClaimClass::BoundedModel => "bounded_model",
            RegistryClaimClass::Statistical => "statistical",
            RegistryClaimClass::Slo => "slo",
            RegistryClaimClass::Benchmark => "benchmark",
        }
    }

    /// Lattice strength; higher is stronger. Private on purpose: consumers
    /// speak in [`try_justify`](Self::try_justify), not raw ranks.
    const fn strength(self) -> u8 {
        match self {
            RegistryClaimClass::Invariant => 5,
            RegistryClaimClass::Proof => 4,
            RegistryClaimClass::BoundedModel => 3,
            RegistryClaimClass::Statistical => 2,
            RegistryClaimClass::Slo => 1,
            RegistryClaimClass::Benchmark => 0,
        }
    }

    /// The lattice law, value form: evidence of class `self` may justify a
    /// claim of class `target` only if `self` is at least as strong.
    pub fn try_justify(
        self,
        target: RegistryClaimClass,
    ) -> Result<Justification, LatticeViolation> {
        if self.strength() >= target.strength() {
            Ok(Justification {
                evidence: self,
                target,
            })
        } else {
            Err(LatticeViolation {
                evidence: self,
                target,
            })
        }
    }

    /// The routing law: which registry file may carry a row of this class.
    pub const fn registry_route(self) -> RegistryRoute {
        match self {
            RegistryClaimClass::Invariant => RegistryRoute::Invariants,
            RegistryClaimClass::Slo => RegistryRoute::Slo,
            RegistryClaimClass::Proof
            | RegistryClaimClass::BoundedModel
            | RegistryClaimClass::Statistical
            | RegistryClaimClass::Benchmark => RegistryRoute::Evidence,
        }
    }
}

/// Registry file a claim class routes to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RegistryRoute {
    /// `registries/invariants.toml` — exact safety/liveness only.
    Invariants,
    /// `registries/evidence.toml`.
    Evidence,
    /// `registries/slo.toml`.
    Slo,
}

/// Proof-of-legality token for one justification edge; constructible only
/// through [`RegistryClaimClass::try_justify`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Justification {
    evidence: RegistryClaimClass,
    target: RegistryClaimClass,
}

impl Justification {
    pub const fn evidence(self) -> RegistryClaimClass {
        self.evidence
    }
    pub const fn target(self) -> RegistryClaimClass {
        self.target
    }
}

/// Typed rejection of an illegal justification (weaker evidence, stronger
/// claim). Carries both ends so the diagnostic can be reconstructed from
/// logs alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LatticeViolation {
    pub evidence: RegistryClaimClass,
    pub target: RegistryClaimClass,
}

impl std::fmt::Display for LatticeViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "claim-lattice violation: evidence of class {} cannot justify a claim of class {}",
            self.evidence.name(),
            self.target.name()
        )
    }
}

impl std::error::Error for LatticeViolation {}

/// Type-level claim classes. Each marker names one registry class; the
/// [`AtLeastAsStrongAs`] relation is implemented for exactly the legal pairs,
/// so an illegal [`justify`] call fails to *compile*.
pub mod class {
    /// Marker for one registry claim class.
    pub trait Class {
        const CLASS: super::RegistryClaimClass;
    }

    macro_rules! classes {
        ($($ty:ident => $variant:ident),+ $(,)?) => {
            $(
                #[derive(Debug, Clone, Copy, PartialEq, Eq)]
                pub struct $ty;
                impl Class for $ty {
                    const CLASS: super::RegistryClaimClass =
                        super::RegistryClaimClass::$variant;
                }
            )+
        };
    }

    classes! {
        Invariant => Invariant,
        Proof => Proof,
        BoundedModel => BoundedModel,
        Statistical => Statistical,
        Slo => Slo,
        Benchmark => Benchmark,
    }

    /// `E: AtLeastAsStrongAs<T>` holds exactly when evidence of class `E`
    /// may justify a claim of class `T` under the lattice law.
    pub trait AtLeastAsStrongAs<Target: Class>: Class {}

    macro_rules! justifies {
        ($e:ident => [$($t:ident),+ $(,)?]) => {
            $(impl AtLeastAsStrongAs<$t> for $e {})+
        };
    }

    justifies!(Invariant => [Invariant, Proof, BoundedModel, Statistical, Slo, Benchmark]);
    justifies!(Proof => [Proof, BoundedModel, Statistical, Slo, Benchmark]);
    justifies!(BoundedModel => [BoundedModel, Statistical, Slo, Benchmark]);
    justifies!(Statistical => [Statistical, Slo, Benchmark]);
    justifies!(Slo => [Slo, Benchmark]);
    justifies!(Benchmark => [Benchmark]);
}

/// A compile-time-legal justification edge.
///
/// ```compile_fail
/// // The lattice law as a compile error: statistical evidence can never
/// // justify an invariant claim.
/// use fgdb_claim::{class, justify};
/// let _ = justify::<class::Statistical, class::Invariant>();
/// ```
///
/// ```
/// // Equal or stronger evidence is fine.
/// use fgdb_claim::{class, justify};
/// let j = justify::<class::Proof, class::Statistical>();
/// assert_eq!(j.evidence(), fgdb_claim::RegistryClaimClass::Proof);
/// ```
pub fn justify<E, T>() -> Justification
where
    E: class::AtLeastAsStrongAs<T>,
    T: class::Class,
{
    Justification {
        evidence: E::CLASS,
        target: T::CLASS,
    }
}

/// Marker alias: a statically checked justification value.
pub type Justified = Justification;

/// A validated statistical type-I error probability.
///
/// The value is always finite and lies strictly inside `(0, 1)`. Keeping the
/// field private prevents a statistical claim from carrying a sentinel,
/// `NaN`, or an unchecked probability.
#[derive(Clone, Copy, PartialEq, PartialOrd, Debug)]
pub struct StatisticalAlpha(f64);

impl StatisticalAlpha {
    /// Validates a finite type-I error probability in the open unit interval.
    pub fn try_new(value: f64) -> Result<Self, InvalidStatisticalAlpha> {
        if !value.is_finite() {
            return Err(InvalidStatisticalAlpha::NonFinite {
                supplied_bits: value.to_bits(),
            });
        }
        if value <= 0.0 || value >= 1.0 {
            return Err(InvalidStatisticalAlpha::OutsideOpenUnitInterval {
                supplied_bits: value.to_bits(),
            });
        }
        Ok(Self(value))
    }

    /// Returns the validated probability.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// Whether a statistical claim declares type-I error control.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum StatisticalErrorControl {
    /// The claim is evaluated at this validated type-I error probability.
    Alpha(StatisticalAlpha),
    /// Type-I error probability does not apply to the claim being made.
    NotApplicable,
}

impl StatisticalErrorControl {
    /// Validates and wraps an applicable type-I error probability.
    pub fn try_alpha(value: f64) -> Result<Self, InvalidStatisticalAlpha> {
        StatisticalAlpha::try_new(value).map(Self::Alpha)
    }

    /// Returns the declared alpha, or `None` when alpha does not apply.
    #[must_use]
    pub const fn alpha(self) -> Option<StatisticalAlpha> {
        match self {
            Self::Alpha(alpha) => Some(alpha),
            Self::NotApplicable => None,
        }
    }
}

/// Typed rejection of an invalid statistical alpha.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InvalidStatisticalAlpha {
    /// The supplied value was a NaN or infinity.
    NonFinite {
        /// Exact IEEE-754 payload supplied by the caller.
        supplied_bits: u64,
    },
    /// The supplied finite value was not strictly between zero and one.
    OutsideOpenUnitInterval {
        /// Exact IEEE-754 payload supplied by the caller.
        supplied_bits: u64,
    },
}

impl InvalidStatisticalAlpha {
    /// Recovers the rejected IEEE-754 value for diagnostics.
    #[must_use]
    pub const fn supplied(self) -> f64 {
        match self {
            Self::NonFinite { supplied_bits } | Self::OutsideOpenUnitInterval { supplied_bits } => {
                f64::from_bits(supplied_bits)
            }
        }
    }
}

impl std::fmt::Display for InvalidStatisticalAlpha {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonFinite { supplied_bits } => write!(
                formatter,
                "statistical alpha 0x{supplied_bits:016x} is not finite"
            ),
            Self::OutsideOpenUnitInterval { supplied_bits } => write!(
                formatter,
                "statistical alpha {} is outside the open interval (0, 1)",
                f64::from_bits(*supplied_bits)
            ),
        }
    }
}

impl std::error::Error for InvalidStatisticalAlpha {}

/// §15.0 evidence claim classes: what kind of statement an evidence envelope
/// makes, with the context each kind must declare. String/OID fields are the
/// immutable declared identities; interpretation belongs to `fgdb-evidence`
/// and Sextant (`verif-sextant`).
#[derive(Clone, PartialEq, Debug)]
pub enum EvidenceClaim {
    /// Must hold on every execution; failure is a correctness defect.
    SafetyInvariant {
        /// Stable invariant ID (`FG-INV-…`).
        invariant_id: String,
    },
    /// Theorem or model-check result inside a named abstraction.
    FormalModelClaim {
        model_name: String,
        abstraction_boundary: String,
        checked_bounds: Option<String>,
        refinement_status: RefinementStatus,
    },
    /// Confidence statement under named assumptions.
    StatisticalClaim {
        population: String,
        sampling_rule: String,
        error_control: StatisticalErrorControl,
        power_or_effective_sample_size: String,
        assumptions: Vec<String>,
    },
    /// Fitted configuration model with a validity domain.
    ConfigurationModelClaim {
        model_version: String,
        fitted_inputs: Vec<String>,
        sensitivity: String,
        validity_domain: String,
    },
    /// Measured gate on a pinned fixture.
    EmpiricalGate {
        fixture: String,
        machine_profile: String,
        sample_count: u64,
        variance_budget: String,
        comparison_rule: String,
    },
}

/// Whether a formal claim has been refined down to the running code.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RefinementStatus {
    /// Proven about the model only; no mechanical link to the implementation.
    ModelOnly,
    /// The model is mechanically refined to (extracted into) the checked code.
    RefinedToImplementation,
}

impl EvidenceClaim {
    /// The strongest registry class an envelope of this kind may justify.
    /// The mapping is intentionally conservative: statistical or empirical
    /// evidence caps at its own level (never `invariant`/`proof`).
    pub fn max_registry_class(&self) -> RegistryClaimClass {
        match self {
            EvidenceClaim::SafetyInvariant { .. } => RegistryClaimClass::Invariant,
            EvidenceClaim::FormalModelClaim {
                refinement_status, ..
            } => match refinement_status {
                RefinementStatus::RefinedToImplementation => RegistryClaimClass::Proof,
                RefinementStatus::ModelOnly => RegistryClaimClass::BoundedModel,
            },
            EvidenceClaim::StatisticalClaim { .. } => RegistryClaimClass::Statistical,
            EvidenceClaim::ConfigurationModelClaim { .. } => RegistryClaimClass::Statistical,
            EvidenceClaim::EmpiricalGate { .. } => RegistryClaimClass::Benchmark,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [RegistryClaimClass; 6] = [
        RegistryClaimClass::Invariant,
        RegistryClaimClass::Proof,
        RegistryClaimClass::BoundedModel,
        RegistryClaimClass::Statistical,
        RegistryClaimClass::Slo,
        RegistryClaimClass::Benchmark,
    ];

    #[test]
    fn names_match_the_constitution_registry() {
        let names: Vec<&str> = ALL.iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            [
                "invariant",
                "proof",
                "bounded_model",
                "statistical",
                "slo",
                "benchmark"
            ]
        );
    }

    #[test]
    fn lattice_is_reflexive_antisymmetric_and_downward_only() {
        for (i, &e) in ALL.iter().enumerate() {
            for (j, &t) in ALL.iter().enumerate() {
                let legal = e.try_justify(t).is_ok();
                // Declaration order is strongest-first, so evidence at index
                // i may justify targets at index >= i.
                assert_eq!(legal, i <= j, "evidence={e:?} target={t:?}");
                if !legal {
                    let err = e.try_justify(t).unwrap_err();
                    assert_eq!((err.evidence, err.target), (e, t));
                    assert!(err.to_string().contains(e.name()));
                    assert!(err.to_string().contains(t.name()));
                }
            }
        }
    }

    #[test]
    fn every_weaker_to_stronger_edge_is_rejected() {
        // The exact negative required by the bead: constructing an invariant
        // from statistical evidence is a typed rejection (and the type-level
        // twin is the compile_fail doctest on `justify`).
        let err = RegistryClaimClass::Statistical
            .try_justify(RegistryClaimClass::Invariant)
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "claim-lattice violation: evidence of class statistical cannot justify a claim of class invariant",
        );
        assert!(
            RegistryClaimClass::Benchmark
                .try_justify(RegistryClaimClass::Slo)
                .is_err()
        );
    }

    #[test]
    fn routing_law_matches_the_constitution() {
        assert_eq!(
            RegistryClaimClass::Invariant.registry_route(),
            RegistryRoute::Invariants
        );
        assert_eq!(RegistryClaimClass::Slo.registry_route(), RegistryRoute::Slo);
        for c in [
            RegistryClaimClass::Proof,
            RegistryClaimClass::BoundedModel,
            RegistryClaimClass::Statistical,
            RegistryClaimClass::Benchmark,
        ] {
            assert_eq!(c.registry_route(), RegistryRoute::Evidence, "{c:?}");
        }
    }

    #[test]
    fn type_level_lattice_agrees_with_value_level() {
        // Spot-check the statically legal edges agree with try_justify.
        let j = justify::<class::Invariant, class::Benchmark>();
        assert_eq!(j.evidence(), RegistryClaimClass::Invariant);
        assert_eq!(j.target(), RegistryClaimClass::Benchmark);
        let j = justify::<class::Slo, class::Slo>();
        assert!(j.evidence().try_justify(j.target()).is_ok());
    }

    #[test]
    fn evidence_claims_cap_their_registry_class() {
        let stat = EvidenceClaim::StatisticalClaim {
            population: "all commits on fixture L".into(),
            sampling_rule: "every commit".into(),
            error_control: StatisticalErrorControl::try_alpha(0.05).unwrap(),
            power_or_effective_sample_size: "n=10_000".into(),
            assumptions: vec!["stationarity within a policy epoch".into()],
        };
        // Statistical evidence may justify statistical or weaker...
        assert!(
            stat.max_registry_class()
                .try_justify(RegistryClaimClass::Slo)
                .is_ok()
        );
        // ...but never an invariant.
        assert!(
            stat.max_registry_class()
                .try_justify(RegistryClaimClass::Invariant)
                .is_err()
        );

        let model_only = EvidenceClaim::FormalModelClaim {
            model_name: "two-fsync commit (TLA+)".into(),
            abstraction_boundary: "single node, crash-stop".into(),
            checked_bounds: Some("3 writers, 5 crashes".into()),
            refinement_status: RefinementStatus::ModelOnly,
        };
        assert_eq!(
            model_only.max_registry_class(),
            RegistryClaimClass::BoundedModel
        );

        let refined = EvidenceClaim::FormalModelClaim {
            model_name: "MVCC visibility (Lean)".into(),
            abstraction_boundary: "block-level".into(),
            checked_bounds: None,
            refinement_status: RefinementStatus::RefinedToImplementation,
        };
        assert_eq!(refined.max_registry_class(), RegistryClaimClass::Proof);

        let gate = EvidenceClaim::EmpiricalGate {
            fixture: "ldbc-snb-sf100".into(),
            machine_profile: "ref-32c-256g-nvme7".into(),
            sample_count: 30,
            variance_budget: "cv<=0.03".into(),
            comparison_rule: "p99 <= baseline*1.05".into(),
        };
        assert_eq!(gate.max_registry_class(), RegistryClaimClass::Benchmark);
    }

    #[test]
    fn statistical_alpha_rejects_sentinels_and_invalid_probabilities() {
        for invalid in [
            0.0,
            -0.0,
            -0.01,
            1.0,
            1.01,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ] {
            assert!(
                StatisticalErrorControl::try_alpha(invalid).is_err(),
                "accepted invalid alpha with bits 0x{:016x}",
                invalid.to_bits()
            );
        }

        let alpha = StatisticalErrorControl::try_alpha(0.05).unwrap();
        assert_eq!(alpha.alpha().map(StatisticalAlpha::get), Some(0.05));
        assert_eq!(StatisticalErrorControl::NotApplicable.alpha(), None);
        assert!(matches!(
            StatisticalErrorControl::try_alpha(f64::NAN),
            Err(InvalidStatisticalAlpha::NonFinite { .. })
        ));
        assert!(matches!(
            StatisticalErrorControl::try_alpha(0.0),
            Err(InvalidStatisticalAlpha::OutsideOpenUnitInterval { .. })
        ));
    }
}
