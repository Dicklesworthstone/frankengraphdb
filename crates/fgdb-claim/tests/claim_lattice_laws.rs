//! Lattice and vocabulary laws for the claim-type constitution.
//!
//! This crate is unusual in the set: its primary instrument is the **compile
//! error**, not the assertion. All 15 forbidden justification directions
//! already fail to compile, one `compile_fail` block each, and the 21 legal
//! edges already compile — that coverage lives on [`fgdb_claim::justify`] and
//! is complete, so nothing here duplicates it.
//!
//! What this file adds is the part the doctests cannot state and the unit
//! tests do not reach:
//!
//! 1. **Type-level transitivity, proven by composition.** The value-level
//!    lattice derives legality from a private `strength()` rank, so it is
//!    transitive by arithmetic. The *type-level* lattice is a hand-written
//!    edge list in a macro, where transitivity is a property of the listing
//!    and a dropped edge is invisible to every existing test. `composes`
//!    below demands the composed edge exist, and it is instantiated at all 56
//!    ordered triples. A missing edge is a compile error, so this file
//!    building at all is the proof.
//! 2. **Marker identity.** Nothing pinned `class::Proof::CLASS ==
//!    RegistryClaimClass::Proof`. A one-word slip in the `classes!` macro
//!    would make a marker lie about which class it denotes while every legal
//!    edge still compiled and every value-level law still held.
//! 3. **Mutual distinctness.** Six classes, six names, six *justification
//!    profiles*. The profile is the observable proxy for the private strength
//!    rank: if two classes shared a rank their profiles would coincide.
//! 4. **The whole §15.0 evidence vocabulary.** Two of the five variants had
//!    no `max_registry_class` coverage at all.
//!
//! Deterministic and total: every law is swept over the entire closed
//! vocabulary rather than sampled. No clock, no entropy, no new dependencies.

use fgdb_claim::class::Class;
use fgdb_claim::{
    EvidenceClaim, InvalidStatisticalAlpha, RefinementStatus, RegistryClaimClass, RegistryRoute,
    StatisticalAlpha, StatisticalErrorControl, class, justify,
};

/// The closed vocabulary in declaration order, strongest first.
const ALL: [RegistryClaimClass; 6] = [
    RegistryClaimClass::Invariant,
    RegistryClaimClass::Proof,
    RegistryClaimClass::BoundedModel,
    RegistryClaimClass::Statistical,
    RegistryClaimClass::Slo,
    RegistryClaimClass::Benchmark,
];

// ------------------------------------------- type-level lattice laws -------

/// Composition witness: this signature is only satisfiable when the
/// type-level relation is transitive. `E >= M` and `M >= T` are the premises;
/// `E: AtLeastAsStrongAs<T>` is the conclusion the caller must already have.
/// Instantiating it at a triple whose composed edge is missing from the
/// `justifies!` list is a compile error, not a failed assertion.
fn composes<E, M, T>() -> (RegistryClaimClass, RegistryClaimClass)
where
    E: class::AtLeastAsStrongAs<M> + class::AtLeastAsStrongAs<T>,
    M: class::AtLeastAsStrongAs<T>,
    T: Class,
{
    (E::CLASS, T::CLASS)
}

#[test]
fn type_level_lattice_is_transitive_over_every_ordered_triple() {
    macro_rules! assert_composes {
        ($(($e:ty, $m:ty, $t:ty)),+ $(,)?) => {{
            let mut checked = 0usize;
            $(
                let (evidence, target) = composes::<$e, $m, $t>();
                // The composed edge must also be legal at the value level, so
                // the two lattices cannot drift apart on the diagonal.
                assert!(
                    evidence.try_justify(target).is_ok(),
                    "type-level composition {evidence:?} -> {target:?} is illegal \
                     at the value level"
                );
                checked += 1;
            )+
            checked
        }};
    }

    // All 56 triples with a >= b >= c. Enumerated, not sampled: a dropped
    // edge in the middle of the chain only shows up in the triples that
    // traverse it.
    let checked = assert_composes!(
        (class::Invariant, class::Invariant, class::Invariant),
        (class::Invariant, class::Invariant, class::Proof),
        (class::Invariant, class::Invariant, class::BoundedModel),
        (class::Invariant, class::Invariant, class::Statistical),
        (class::Invariant, class::Invariant, class::Slo),
        (class::Invariant, class::Invariant, class::Benchmark),
        (class::Invariant, class::Proof, class::Proof),
        (class::Invariant, class::Proof, class::BoundedModel),
        (class::Invariant, class::Proof, class::Statistical),
        (class::Invariant, class::Proof, class::Slo),
        (class::Invariant, class::Proof, class::Benchmark),
        (class::Invariant, class::BoundedModel, class::BoundedModel),
        (class::Invariant, class::BoundedModel, class::Statistical),
        (class::Invariant, class::BoundedModel, class::Slo),
        (class::Invariant, class::BoundedModel, class::Benchmark),
        (class::Invariant, class::Statistical, class::Statistical),
        (class::Invariant, class::Statistical, class::Slo),
        (class::Invariant, class::Statistical, class::Benchmark),
        (class::Invariant, class::Slo, class::Slo),
        (class::Invariant, class::Slo, class::Benchmark),
        (class::Invariant, class::Benchmark, class::Benchmark),
        (class::Proof, class::Proof, class::Proof),
        (class::Proof, class::Proof, class::BoundedModel),
        (class::Proof, class::Proof, class::Statistical),
        (class::Proof, class::Proof, class::Slo),
        (class::Proof, class::Proof, class::Benchmark),
        (class::Proof, class::BoundedModel, class::BoundedModel),
        (class::Proof, class::BoundedModel, class::Statistical),
        (class::Proof, class::BoundedModel, class::Slo),
        (class::Proof, class::BoundedModel, class::Benchmark),
        (class::Proof, class::Statistical, class::Statistical),
        (class::Proof, class::Statistical, class::Slo),
        (class::Proof, class::Statistical, class::Benchmark),
        (class::Proof, class::Slo, class::Slo),
        (class::Proof, class::Slo, class::Benchmark),
        (class::Proof, class::Benchmark, class::Benchmark),
        (
            class::BoundedModel,
            class::BoundedModel,
            class::BoundedModel
        ),
        (class::BoundedModel, class::BoundedModel, class::Statistical),
        (class::BoundedModel, class::BoundedModel, class::Slo),
        (class::BoundedModel, class::BoundedModel, class::Benchmark),
        (class::BoundedModel, class::Statistical, class::Statistical),
        (class::BoundedModel, class::Statistical, class::Slo),
        (class::BoundedModel, class::Statistical, class::Benchmark),
        (class::BoundedModel, class::Slo, class::Slo),
        (class::BoundedModel, class::Slo, class::Benchmark),
        (class::BoundedModel, class::Benchmark, class::Benchmark),
        (class::Statistical, class::Statistical, class::Statistical),
        (class::Statistical, class::Statistical, class::Slo),
        (class::Statistical, class::Statistical, class::Benchmark),
        (class::Statistical, class::Slo, class::Slo),
        (class::Statistical, class::Slo, class::Benchmark),
        (class::Statistical, class::Benchmark, class::Benchmark),
        (class::Slo, class::Slo, class::Slo),
        (class::Slo, class::Slo, class::Benchmark),
        (class::Slo, class::Benchmark, class::Benchmark),
        (class::Benchmark, class::Benchmark, class::Benchmark),
    );

    assert_eq!(
        checked, 56,
        "the ordered-triple sweep must cover every a >= b >= c triple"
    );
}

#[test]
fn type_level_markers_denote_the_class_they_are_named_for() {
    // A one-word slip in the `classes!` macro (`Proof => Invariant`) leaves
    // every legal edge compiling and every value-level law holding, while the
    // type-level lattice silently denotes the wrong class. Pin the mapping.
    assert_eq!(class::Invariant::CLASS, RegistryClaimClass::Invariant);
    assert_eq!(class::Proof::CLASS, RegistryClaimClass::Proof);
    assert_eq!(class::BoundedModel::CLASS, RegistryClaimClass::BoundedModel);
    assert_eq!(class::Statistical::CLASS, RegistryClaimClass::Statistical);
    assert_eq!(class::Slo::CLASS, RegistryClaimClass::Slo);
    assert_eq!(class::Benchmark::CLASS, RegistryClaimClass::Benchmark);

    // And the six markers denote six DISTINCT classes: no accidental aliasing.
    let denoted = [
        class::Invariant::CLASS,
        class::Proof::CLASS,
        class::BoundedModel::CLASS,
        class::Statistical::CLASS,
        class::Slo::CLASS,
        class::Benchmark::CLASS,
    ];
    for (left_index, left) in denoted.iter().enumerate() {
        for (right_index, right) in denoted.iter().enumerate() {
            assert_eq!(
                left == right,
                left_index == right_index,
                "markers {left_index} and {right_index} alias onto {left:?}"
            );
        }
    }
    assert_eq!(denoted, ALL, "markers must follow the declaration order");
}

#[test]
fn justify_carries_the_exact_ends_it_was_instantiated_with() {
    // The existing suite only checks that the produced justification is legal.
    // That survives a swap of the two ends on any reflexive edge, so pin the
    // identity on asymmetric edges where a swap is observable.
    let strong_to_weak = justify::<class::Invariant, class::Benchmark>();
    assert_eq!(strong_to_weak.evidence(), RegistryClaimClass::Invariant);
    assert_eq!(strong_to_weak.target(), RegistryClaimClass::Benchmark);

    let mid = justify::<class::Proof, class::Slo>();
    assert_eq!(mid.evidence(), RegistryClaimClass::Proof);
    assert_eq!(mid.target(), RegistryClaimClass::Slo);

    let reflexive = justify::<class::Statistical, class::Statistical>();
    assert_eq!(reflexive.evidence(), reflexive.target());
    assert_eq!(reflexive.evidence(), RegistryClaimClass::Statistical);
}

// ------------------------------------------ value-level lattice laws -------

#[test]
fn lattice_is_a_partial_order_over_every_triple() {
    // Reflexivity: every class justifies itself.
    for &class in &ALL {
        assert!(
            class.try_justify(class).is_ok(),
            "{class:?} must justify itself"
        );
    }

    // Antisymmetry: mutual justification implies identity. A comparator that
    // is inconsistent on ties admits two distinct classes justifying each
    // other, which no pairwise legality check would notice.
    for &evidence in &ALL {
        for &target in &ALL {
            let forward = evidence.try_justify(target).is_ok();
            let backward = target.try_justify(evidence).is_ok();
            if forward && backward {
                assert_eq!(
                    evidence, target,
                    "{evidence:?} and {target:?} justify each other but are distinct"
                );
            }
            assert!(
                forward || backward,
                "{evidence:?} and {target:?} are incomparable; the lattice is a total chain"
            );
        }
    }

    // Transitivity over all 216 triples, not the 36 pairs.
    let mut triples = 0usize;
    for &a in &ALL {
        for &b in &ALL {
            for &c in &ALL {
                triples += 1;
                if a.try_justify(b).is_ok() && b.try_justify(c).is_ok() {
                    assert!(
                        a.try_justify(c).is_ok(),
                        "transitivity broken: {a:?} >= {b:?} >= {c:?} but not {a:?} >= {c:?}"
                    );
                }
            }
        }
    }
    assert_eq!(triples, 216, "the triple sweep must be exhaustive");
}

#[test]
fn the_six_classes_are_mutually_distinct_in_every_observable() {
    // Distinct values.
    for (left_index, &left) in ALL.iter().enumerate() {
        for (right_index, &right) in ALL.iter().enumerate() {
            assert_eq!(
                left == right,
                left_index == right_index,
                "classes at {left_index} and {right_index} alias"
            );
        }
    }

    // Distinct registry spellings.
    let mut names: Vec<&str> = ALL.iter().map(|c| c.name()).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 6, "two classes share a registry spelling");

    // Distinct justification profiles. `strength()` is private, so the set of
    // targets a class can justify is the only observable proxy for its rank.
    // If two classes shared a rank, their profiles would be identical — which
    // is exactly what "no accidental aliasing" has to rule out.
    let profiles: Vec<(RegistryClaimClass, Vec<&str>)> = ALL
        .iter()
        .map(|&evidence| {
            let targets = ALL
                .iter()
                .filter(|&&target| evidence.try_justify(target).is_ok())
                .map(|t| t.name())
                .collect();
            (evidence, targets)
        })
        .collect();

    for (left_index, (left, left_profile)) in profiles.iter().enumerate() {
        for (right_index, (right, right_profile)) in profiles.iter().enumerate() {
            assert_eq!(
                left_profile == right_profile,
                left_index == right_index,
                "{left:?} and {right:?} have identical justification profiles, \
                 so they occupy the same lattice rank"
            );
        }
    }

    // The profiles are strictly nested, which is what makes the lattice a
    // chain: each class justifies exactly one more target than the next.
    for (index, (class, profile)) in profiles.iter().enumerate() {
        assert_eq!(
            profile.len(),
            6 - index,
            "{class:?} should justify exactly {} targets",
            6 - index
        );
    }
}

#[test]
fn a_legal_justification_carries_both_of_its_ends() {
    // The error path is already asserted upstream; the success path was not.
    // A swap here would make every accepted justification report backwards.
    for &evidence in &ALL {
        for &target in &ALL {
            if let Ok(justification) = evidence.try_justify(target) {
                assert_eq!(
                    justification.evidence(),
                    evidence,
                    "justification lost its evidence class"
                );
                assert_eq!(
                    justification.target(),
                    target,
                    "justification lost its target class"
                );
            }
        }
    }
}

#[test]
fn routing_law_is_total_and_its_routes_are_not_interchangeable() {
    for &class in &ALL {
        let route = class.registry_route();
        let expected = match class {
            RegistryClaimClass::Invariant => RegistryRoute::Invariants,
            RegistryClaimClass::Slo => RegistryRoute::Slo,
            _ => RegistryRoute::Evidence,
        };
        assert_eq!(route, expected, "routing law drifted for {class:?}");
    }

    // The three routes are distinct: `invariants.toml` carrying an empirical
    // claim is the exact failure the routing law exists to prevent.
    let routes = [
        RegistryRoute::Invariants,
        RegistryRoute::Evidence,
        RegistryRoute::Slo,
    ];
    for (left_index, left) in routes.iter().enumerate() {
        for (right_index, right) in routes.iter().enumerate() {
            assert_eq!(left == right, left_index == right_index);
        }
    }

    // Only the strongest class reaches `invariants.toml`, and only `slo`
    // reaches `slo.toml`. Everything else is evidence.
    let to_invariants: Vec<_> = ALL
        .iter()
        .filter(|c| c.registry_route() == RegistryRoute::Invariants)
        .collect();
    assert_eq!(to_invariants, [&RegistryClaimClass::Invariant]);
    let to_slo: Vec<_> = ALL
        .iter()
        .filter(|c| c.registry_route() == RegistryRoute::Slo)
        .collect();
    assert_eq!(to_slo, [&RegistryClaimClass::Slo]);
}

// ------------------------------------- section 15.0 evidence vocabulary ----

/// One instance of every §15.0 evidence claim kind, with both refinement
/// statuses of the formal variant. Fixed strings; nothing is sampled.
fn evidence_corpus() -> Vec<(EvidenceClaim, RegistryClaimClass)> {
    vec![
        (
            EvidenceClaim::SafetyInvariant {
                invariant_id: "FG-INV-17".into(),
            },
            RegistryClaimClass::Invariant,
        ),
        (
            EvidenceClaim::FormalModelClaim {
                model_name: "MVCC visibility (Lean)".into(),
                abstraction_boundary: "block-level".into(),
                checked_bounds: None,
                refinement_status: RefinementStatus::RefinedToImplementation,
            },
            RegistryClaimClass::Proof,
        ),
        (
            EvidenceClaim::FormalModelClaim {
                model_name: "two-fsync commit (TLA+)".into(),
                abstraction_boundary: "single node, crash-stop".into(),
                checked_bounds: Some("3 writers, 5 crashes".into()),
                refinement_status: RefinementStatus::ModelOnly,
            },
            RegistryClaimClass::BoundedModel,
        ),
        (
            EvidenceClaim::StatisticalClaim {
                population: "all commits on fixture L".into(),
                sampling_rule: "every commit".into(),
                error_control: StatisticalErrorControl::try_alpha(0.05).expect("alpha"),
                power_or_effective_sample_size: "n=10_000".into(),
                assumptions: vec!["stationarity within a policy epoch".into()],
            },
            RegistryClaimClass::Statistical,
        ),
        (
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "cost-model-v3".into(),
                fitted_inputs: vec!["nvme7 latency curve".into()],
                sensitivity: "±8% on p99".into(),
                validity_domain: "16..64 cores, sf<=300".into(),
            },
            RegistryClaimClass::Statistical,
        ),
        (
            EvidenceClaim::EmpiricalGate {
                fixture: "ldbc-snb-sf100".into(),
                machine_profile: "ref-32c-256g-nvme7".into(),
                sample_count: 30,
                variance_budget: "cv<=0.03".into(),
                comparison_rule: "p99 <= baseline*1.05".into(),
            },
            RegistryClaimClass::Benchmark,
        ),
    ]
}

#[test]
fn every_evidence_kind_caps_at_its_declared_registry_class() {
    let corpus = evidence_corpus();
    assert_eq!(
        corpus.len(),
        6,
        "all five variants, with both refinement statuses of the formal one"
    );

    for (claim, expected) in &corpus {
        assert_eq!(
            &claim.max_registry_class(),
            expected,
            "evidence kind caps at the wrong registry class: {claim:?}"
        );
    }
}

#[test]
fn no_evidence_kind_can_reach_above_its_cap() {
    // The constitutional point of the cap: statistical and empirical evidence
    // must never justify an invariant, whatever it claims about itself.
    for (claim, _) in evidence_corpus() {
        let cap = claim.max_registry_class();
        for &target in &ALL {
            let legal = cap.try_justify(target).is_ok();
            let strictly_stronger_than_cap = ALL.iter().position(|c| *c == target).expect("member")
                < ALL.iter().position(|c| *c == cap).expect("member");
            assert_eq!(
                legal, !strictly_stronger_than_cap,
                "{claim:?} capped at {cap:?} must not justify {target:?}"
            );
        }
    }

    // Named instances of the forbidden direction, stated outright.
    let empirical = RegistryClaimClass::Benchmark;
    for forbidden in [
        RegistryClaimClass::Invariant,
        RegistryClaimClass::Proof,
        RegistryClaimClass::BoundedModel,
        RegistryClaimClass::Statistical,
        RegistryClaimClass::Slo,
    ] {
        assert!(
            empirical.try_justify(forbidden).is_err(),
            "a benchmark measurement must not justify {forbidden:?}"
        );
    }
}

#[test]
fn statistical_and_configuration_claims_share_a_cap_by_design() {
    // These are the only two §15.0 kinds that map to one registry class. That
    // is deliberate — a fitted configuration model is a confidence statement —
    // so pin it as intentional rather than leaving it to look like a bug.
    let corpus = evidence_corpus();
    let caps: Vec<RegistryClaimClass> = corpus.iter().map(|(_, cap)| *cap).collect();
    let statistical_caps = caps
        .iter()
        .filter(|cap| **cap == RegistryClaimClass::Statistical)
        .count();
    assert_eq!(
        statistical_caps, 2,
        "exactly StatisticalClaim and ConfigurationModelClaim cap at statistical"
    );

    // Every other registry class is reached by exactly one evidence kind.
    for class in [
        RegistryClaimClass::Invariant,
        RegistryClaimClass::Proof,
        RegistryClaimClass::BoundedModel,
        RegistryClaimClass::Benchmark,
    ] {
        assert_eq!(
            caps.iter().filter(|cap| **cap == class).count(),
            1,
            "{class:?} must be reachable by exactly one evidence kind"
        );
    }

    // No evidence kind caps at `slo`: an SLO is a target, not evidence.
    assert_eq!(
        caps.iter()
            .filter(|cap| **cap == RegistryClaimClass::Slo)
            .count(),
        0,
        "no §15.0 evidence kind should cap at the slo class"
    );
}

// ------------------------------------------------ statistical alpha --------

#[test]
fn statistical_alpha_accepts_exactly_the_open_unit_interval() {
    // Boundary-heavy: the two representable values nearest each excluded
    // endpoint must be accepted, and the endpoints themselves rejected.
    for accepted in [
        f64::MIN_POSITIVE, // smallest positive normal
        5e-324,            // smallest positive subnormal
        0.05,
        0.5,
        1.0 - f64::EPSILON, // largest representable below one
    ] {
        let alpha = StatisticalAlpha::try_new(accepted)
            .unwrap_or_else(|e| panic!("rejected valid alpha {accepted:e}: {e:?}"));
        assert_eq!(
            alpha.get().to_bits(),
            accepted.to_bits(),
            "alpha must be carried through unmodified"
        );
    }

    for (rejected, expected_nonfinite) in [
        (0.0, false),
        (-0.0, false),
        (-f64::MIN_POSITIVE, false),
        (1.0, false),
        (f64::INFINITY, true),
        (f64::NEG_INFINITY, true),
        (f64::NAN, true),
    ] {
        let error = StatisticalAlpha::try_new(rejected).expect_err(&format!(
            "accepted invalid alpha 0x{:016x}",
            rejected.to_bits()
        ));
        assert_eq!(
            matches!(error, InvalidStatisticalAlpha::NonFinite { .. }),
            expected_nonfinite,
            "wrong rejection variant for 0x{:016x}",
            rejected.to_bits()
        );
        // The rejected payload is recoverable bit-for-bit, including NaN.
        assert_eq!(
            error.supplied().to_bits(),
            rejected.to_bits(),
            "rejection must carry the exact supplied bits"
        );
    }
}

#[test]
fn error_control_distinguishes_not_applicable_from_a_declared_alpha() {
    let declared = StatisticalErrorControl::try_alpha(0.01).expect("alpha");
    assert_eq!(
        declared.alpha().map(StatisticalAlpha::get),
        Some(0.01),
        "a declared alpha must be readable"
    );
    assert_eq!(
        StatisticalErrorControl::NotApplicable.alpha(),
        None,
        "not-applicable must not masquerade as a declared alpha"
    );
    assert_ne!(
        declared,
        StatisticalErrorControl::NotApplicable,
        "the two error-control states must be distinguishable"
    );
}
