//! `foundation_types_e2e` transcript: one deterministic pass over every
//! foundation crate (bead `fgdb-w1-foundation-types-tjk`).
//!
//! The output is a line-oriented transcript that must be byte-identical
//! across lab seeds (`scripts/w1_foundation_types_e2e.sh` runs it with two
//! seeds and `cmp`s the results). It exercises, in order: every canonical scalar variant under
//! `STRICT_PORTABLE` (round trip + typed malformed rejections), a `ZWeight`
//! promotion across the `i128` boundary into `fgdb-bigint` and back, every
//! delta-row arm into a `LogicalDeltaTemplate` and — via a committed marker —
//! an ordered `LogicalDeltaBatch`, one evidence envelope per §15.0 claim
//! kind, a scripted claim-lattice violation producing the typed rejection,
//! and a resource-admission loop ending in a typed ceiling rejection.
//! The final line is an FNV-1a digest of the whole transcript (std-only,
//! stable across processes and toolchains, unlike `DefaultHasher`).
//!
//! The harness runs directly under asupersync's lab runtime. `fgdb-verif-sim`
//! will later supply the database-specific VFS, chaos, and DPOR campaign;
//! determinism and quiescence under two seeds are what this stage asserts.

use asupersync::lab::run_async_under_lab;
use fgdb_bigint::{BigInt, LimbLimit};
use fgdb_claim::{
    EvidenceClaim, RefinementStatus, RegistryClaimClass, StatisticalErrorControl, class, justify,
};
use fgdb_delta_types::{
    CommittedMarker, DeltaRow, DeltaTemplateKey, ElementId, EscrowDomainId, LabelId,
    LogicalDeltaBatch, LogicalDeltaTemplate, OperationKey, PropertyKeyId, RelationId, SchemaEpoch,
    ValidTimePeriod,
};
use fgdb_evidence::{
    CalibrationWindow, EvidenceEnvelope, FallbackBehavior, ReplayClass, ReplayCompleteness,
};
use fgdb_resource::{ResourceCeiling, ResourceVector};
use fgdb_types::{
    BranchId, CanonicalDecimal, CanonicalF64, CanonicalScalar, CanonicalText, CanonicalTimestamp,
    CollationResolver, CollationResolverError, CommitSeq, EId, GraphId, MAX_DECIMAL_COEFFICIENT,
    MarkerRef, NonBinaryTextBinding, ObjectId, ObligationId, ObligationResolution, ObligationStage,
    PurposeContexts, TzdbResolver, VId,
};

const ZONED_FIXTURE_INSTANT: i128 = 1_735_689_600_123_456_789;
const DEFAULT_LAB_SEED: u64 = 0x000F_00D4_1999;

struct FoundationResolver {
    available: bool,
}

impl CollationResolver for FoundationResolver {
    fn artifact_available(&self, object_id: &ObjectId) -> bool {
        self.available && [oid(0x31), oid(0x32), oid(0x33), oid(0x34)].contains(object_id)
    }

    fn canonical_sort_key_len(
        &self,
        _: &NonBinaryTextBinding,
        text: &str,
    ) -> Result<usize, CollationResolverError> {
        text.len()
            .checked_add(4)
            .ok_or(CollationResolverError::new(1))
    }

    fn write_canonical_sort_key(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
        output: &mut [u8],
    ) -> Result<usize, CollationResolverError> {
        let expected = self.canonical_sort_key_len(binding, text)?;
        if output.len() != expected {
            return Err(CollationResolverError::new(2));
        }
        output[..4].copy_from_slice(&[
            binding.unicode_data_oid.as_bytes()[0],
            binding.normalization_oid.as_bytes()[0],
            binding.segmentation_oid.as_bytes()[0],
            binding.collation_oid.as_bytes()[0],
        ]);
        output[4..].copy_from_slice(text.as_bytes());
        Ok(expected)
    }

    fn canonical_sort_key_matches(
        &self,
        binding: &NonBinaryTextBinding,
        text: &str,
        candidate: &[u8],
    ) -> Result<bool, CollationResolverError> {
        let prefix = [
            binding.unicode_data_oid.as_bytes()[0],
            binding.normalization_oid.as_bytes()[0],
            binding.segmentation_oid.as_bytes()[0],
            binding.collation_oid.as_bytes()[0],
        ];
        Ok(candidate.len() == text.len() + prefix.len()
            && candidate.starts_with(&prefix)
            && &candidate[prefix.len()..] == text.as_bytes())
    }
}

impl TzdbResolver for FoundationResolver {
    fn contains_tzdb(&self, tzdb_oid: &ObjectId) -> bool {
        self.available && tzdb_oid == &oid(0x40)
    }

    fn canonical_utc_offset_seconds(
        &self,
        tzdb_oid: &ObjectId,
        zone_identifier: &str,
        instant_utc_nanos: i128,
    ) -> Option<i32> {
        (tzdb_oid == &oid(0x40)
            && zone_identifier == "America/New_York"
            && instant_utc_nanos == ZONED_FIXTURE_INSTANT)
            .then_some(-5 * 60 * 60)
    }
}

const AVAILABLE_RESOLVER: FoundationResolver = FoundationResolver { available: true };
const MISSING_RESOLVER: FoundationResolver = FoundationResolver { available: false };

/// FNV-1a over the transcript bytes: deterministic across processes.
struct Fnv1a(u64);
impl Fnv1a {
    fn new() -> Self {
        Fnv1a(0xCBF2_9CE4_8422_2325)
    }
    fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
}

struct Transcript {
    digest: Fnv1a,
}

impl Transcript {
    fn emit(&mut self, line: &str) {
        println!("{line}");
        self.digest.update(line.as_bytes());
        self.digest.update(b"\n");
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn oid(fill: u8) -> ObjectId {
    ObjectId([fill; 32])
}

fn main() {
    let seed = match std::env::args().nth(1) {
        Some(value) => value.parse::<u64>().expect("lab seed must be a u64"),
        None => DEFAULT_LAB_SEED,
    };
    let ((), report) = run_async_under_lab(seed, |root| async move {
        run_transcript(&root);
    });
    assert!(
        report.quiescent,
        "foundation lab run did not quiesce: {report:?}"
    );
    assert!(
        report.oracle_report.total > 0,
        "foundation lab run produced no oracle coverage: {report:?}"
    );
    assert!(
        report.oracle_report.all_passed(),
        "foundation lab oracle failed: {report:?}"
    );
    for invariant in ["obligation_leak", "quiescence"] {
        let entry = report.oracle_report.entry(invariant);
        assert!(
            entry.is_some(),
            "foundation lab report omitted {invariant}: {report:?}"
        );
        if let Some(entry) = entry {
            assert!(
                entry.passed,
                "foundation lab oracle {invariant} failed: {report:?}"
            );
        }
    }
    assert!(
        report.invariant_violations.is_empty(),
        "foundation lab invariant violation: {report:?}"
    );
}

fn run_transcript(root: &asupersync::Cx) {
    let mut t = Transcript {
        digest: Fnv1a::new(),
    };
    t.emit("== foundation_types_e2e transcript v4 ==");

    // 1. Canonical scalars: every variant, encode/decode round trip.
    let text_binding = NonBinaryTextBinding::new(oid(0x31), oid(0x32), oid(0x33), oid(0x34));
    let pinned_text = CanonicalText::new_non_binary("Straße", text_binding, &AVAILABLE_RESOLVER)
        .expect("pinned text artifacts available");
    let zoned_timestamp = CanonicalTimestamp::zoned(
        ZONED_FIXTURE_INSTANT,
        -5 * 60 * 60,
        "America/New_York",
        oid(0x40),
        &AVAILABLE_RESOLVER,
    )
    .expect("zoned timestamp fixture");
    let pinned_text_scalar = CanonicalScalar::Text(pinned_text);
    let zoned_timestamp_scalar = CanonicalScalar::Timestamp(zoned_timestamp);
    let scalars = vec![
        CanonicalScalar::Null,
        CanonicalScalar::Bool(true),
        CanonicalScalar::Int(-41999),
        CanonicalScalar::Decimal(
            CanonicalDecimal::from_scaled_half_even(12_345, 2).expect("123.45 decimal"),
        ),
        CanonicalScalar::Float(CanonicalF64::new(-0.0)),
        CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
        CanonicalScalar::Float(CanonicalF64::new(2.5)),
        CanonicalScalar::ucs_basic_text("frankengraphdb").expect("bounded UCS_BASIC text"),
        pinned_text_scalar.clone(),
        CanonicalScalar::Timestamp(
            CanonicalTimestamp::offset_only(42_000_000_000, 90 * 60)
                .expect("offset-only timestamp fixture"),
        ),
        zoned_timestamp_scalar.clone(),
        CanonicalScalar::bytes(vec![0xF0, 0x0D]).expect("bounded byte scalar"),
    ];
    for s in &scalars {
        let enc = s.encode().expect("bounded scalar encoding");
        let back =
            CanonicalScalar::decode_with_resolver(&enc, &AVAILABLE_RESOLVER).expect("round trip");
        assert_eq!(&back, s);
        t.emit(&format!("scalar {:?} encodes {}", s, hex(&enc)));
    }
    let pinned_text_encoding = pinned_text_scalar.encode().expect("pinned text encoding");
    t.emit(&format!(
        "scalar reject absent collation resolver: {}",
        CanonicalScalar::decode(&pinned_text_encoding).unwrap_err()
    ));
    t.emit(&format!(
        "scalar reject missing collation artifact: {}",
        CanonicalScalar::decode_with_resolver(&pinned_text_encoding, &MISSING_RESOLVER)
            .unwrap_err()
    ));
    let mut forged_text_encoding = pinned_text_encoding.clone();
    forged_text_encoding[1 + 1 + 4 * 32] ^= 1;
    t.emit(&format!(
        "scalar reject forged collation sort key: {}",
        CanonicalScalar::decode_with_resolver(&forged_text_encoding, &AVAILABLE_RESOLVER)
            .unwrap_err()
    ));
    let zoned_timestamp_encoding = zoned_timestamp_scalar
        .encode()
        .expect("zoned timestamp encoding");
    t.emit(&format!(
        "scalar reject absent tzdb resolver: {}",
        CanonicalScalar::decode(&zoned_timestamp_encoding).unwrap_err()
    ));
    t.emit(&format!(
        "scalar reject missing tzdb artifact: {}",
        CanonicalScalar::decode_with_resolver(&zoned_timestamp_encoding, &MISSING_RESOLVER)
            .unwrap_err()
    ));
    t.emit(&format!(
        "timestamp reject tzdb offset mismatch: {}",
        CanonicalTimestamp::zoned(
            ZONED_FIXTURE_INSTANT,
            -4 * 60 * 60,
            "America/New_York",
            oid(0x40),
            &AVAILABLE_RESOLVER,
        )
        .unwrap_err()
    ));
    let half_even = CanonicalDecimal::from_scaled_half_even(25, 19)
        .expect("2.5 units at scale 18 rounds to even 2");
    t.emit(&format!(
        "decimal half-even boundary: source=25e-19 coefficient={}",
        half_even.coefficient()
    ));
    let decimal_max = CanonicalDecimal::from_coefficient(MAX_DECIMAL_COEFFICIENT)
        .expect("profile maximum is canonical");
    let decimal_one = CanonicalDecimal::from_coefficient(1).expect("one coefficient is canonical");
    t.emit(&format!(
        "decimal reject profile overflow: {}",
        decimal_max.checked_add(decimal_one).unwrap_err()
    ));
    // Typed malformed rejections.
    let bad_float = {
        let mut e = vec![0x04];
        e.extend_from_slice(&0xFFF0_0000_0000_0001u64.to_be_bytes());
        e
    };
    t.emit(&format!(
        "scalar reject non-canonical float: {}",
        CanonicalScalar::decode(&bad_float).unwrap_err()
    ));
    let mut malformed_bytes = vec![0x07];
    malformed_bytes.extend_from_slice(&[0; 8]);
    malformed_bytes.push(0xF6);
    t.emit(&format!(
        "scalar reject invalid memcomparable marker: {}",
        CanonicalScalar::decode(&malformed_bytes).unwrap_err()
    ));

    // 2. ZWeight promotion across the i128 boundary and back.
    let limit = LimbLimit::new(4);
    let fast = i128::MAX; // the checked-i128 fast path saturates here...
    assert!(fast.checked_add(1).is_none());
    let promoted = BigInt::from_i128(fast)
        .checked_add(&BigInt::from_i128(1), limit)
        .expect("promotion add");
    t.emit(&format!(
        "zweight promoted past i128: sign={:?} limbs_le={:x?} (limb_count={}, demotes={:?})",
        promoted.sign(),
        promoted.magnitude_limbs_le(),
        promoted.limb_count(),
        promoted.to_i128()
    ));
    let demoted = promoted
        .checked_sub(&BigInt::from_i128(1), limit)
        .expect("demotion sub")
        .to_i128();
    assert_eq!(demoted, Some(i128::MAX));
    t.emit(&format!("zweight demoted back: {demoted:?}"));

    // 3. Every delta-row arm -> template -> committed marker -> ordered batch.
    let rows = vec![
        DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![LabelId(7)],
            props: vec![(
                PropertyKeyId(1),
                CanonicalScalar::ucs_basic_text("Ada").expect("bounded UCS_BASIC text"),
            )],
            valid_time: None,
        },
        DeltaRow::CreateEdge {
            eid: EId(1),
            birth_ordinal: 2,
            src: VId(1),
            relation: RelationId(3),
            dst: VId(2),
            canonical_key: None,
            props: vec![],
            valid_time: Some(ValidTimePeriod {
                start_micros: 0,
                end_micros: None,
            }),
        },
        DeltaRow::DeleteVertex {
            vid: VId(9),
            before_version: oid(2),
            sorted_retired_incident_edges: vec![EId(4)],
        },
        DeltaRow::DeleteEdge {
            eid: EId(4),
            before_version: oid(3),
        },
        DeltaRow::LabelMembership {
            vid: VId(1),
            label: LabelId(2),
            before: false,
            after: true,
        },
        DeltaRow::Property {
            elem: ElementId::Vertex(VId(1)),
            property: PropertyKeyId(2),
            before: None,
            after: Some(CanonicalScalar::Int(1815)),
        },
        DeltaRow::ValidTime {
            elem: ElementId::Edge(EId(1)),
            contract_id: oid(4),
            before: None,
            after: Some(ValidTimePeriod {
                start_micros: 10,
                end_micros: Some(20),
            }),
        },
        DeltaRow::Counter {
            operation_key: OperationKey([5; 32]),
            elem: ElementId::Vertex(VId(1)),
            property: PropertyKeyId(3),
            algebra_profile: oid(6),
            delta: 5,
            before: 10,
            after: 15,
        },
        DeltaRow::Escrow {
            domain_id: EscrowDomainId(1),
            epoch: 1,
            operation_key: OperationKey([7; 32]),
            subject: ElementId::Vertex(VId(1)),
            subject_property: None,
            delta: -3,
            before_value: 10,
            after_value: 7,
        },
        DeltaRow::Sketch {
            operation_key: OperationKey([8; 32]),
            sketch_profile_oid: oid(9),
            before_state_digest: [0; 32],
            after_state_oid: oid(10),
        },
        DeltaRow::Schema {
            transition_oid: oid(11),
            before_epoch: SchemaEpoch(2),
            after_epoch: SchemaEpoch(3),
        },
        DeltaRow::Constraint {
            before_schema_root: oid(12),
            after_schema_root: oid(13),
            before_constraint_root: oid(14),
            after_constraint_root: oid(15),
        },
    ];
    let families: Vec<String> = rows.iter().map(|r| format!("{:?}", r.family())).collect();
    t.emit(&format!("delta families: {}", families.join(",")));
    let template = LogicalDeltaTemplate::new(
        DeltaTemplateKey {
            graph: GraphId(1),
            branch: BranchId(1),
            relation: RelationId(3),
            schema_epoch: SchemaEpoch(2),
            intent_semantics_oid: oid(0x11),
        },
        rows,
    );
    let marker = MarkerRef {
        marker_oid: oid(0xAA),
        commit_seq: CommitSeq(41999),
    };
    let batch = LogicalDeltaBatch::order(template, CommittedMarker::attest(marker));
    t.emit(&format!(
        "ordered batch at commit_seq {:?} with {} rows",
        batch.commit_seq(),
        batch.template().rows().len()
    ));

    // 4. One envelope per §15.0 claim kind, with the lattice at the boundary.
    let claims: Vec<(&str, EvidenceClaim)> = vec![
        (
            "safety",
            EvidenceClaim::SafetyInvariant {
                invariant_id: "FG-INV-12".into(),
            },
        ),
        (
            "formal-refined",
            EvidenceClaim::FormalModelClaim {
                model_name: "MVCC visibility (Lean)".into(),
                abstraction_boundary: "block-level".into(),
                checked_bounds: None,
                refinement_status: RefinementStatus::RefinedToImplementation,
            },
        ),
        (
            "statistical",
            EvidenceClaim::StatisticalClaim {
                population: "fixture L admissions".into(),
                sampling_rule: "every admission".into(),
                error_control: StatisticalErrorControl::try_alpha(0.01)
                    .expect("fixture alpha is valid"),
                power_or_effective_sample_size: "n_eff=52_000".into(),
                assumptions: vec!["per-epoch exchangeability".into()],
            },
        ),
        (
            "config-model",
            EvidenceClaim::ConfigurationModelClaim {
                model_version: "cost-v1".into(),
                fitted_inputs: vec!["nvme7".into()],
                sensitivity: "low".into(),
                validity_domain: "single node".into(),
            },
        ),
        (
            "empirical-gate",
            EvidenceClaim::EmpiricalGate {
                fixture: "ldbc-snb-sf100".into(),
                machine_profile: "ref-32c-256g".into(),
                sample_count: 30,
                variance_budget: "cv<=0.03".into(),
                comparison_rule: "p99<=baseline*1.05".into(),
            },
        ),
    ];
    for (tag, claim) in claims {
        let env = EvidenceEnvelope::new(
            claim,
            oid(0x20),
            oid(0x21),
            Some(CalibrationWindow::new(100, 42_000).expect("window")),
            7,
            FallbackBehavior::FailClosed,
        );
        t.emit(&format!(
            "envelope {tag}: max class {} routes to {:?}",
            env.claim().max_registry_class().name(),
            env.claim().max_registry_class().registry_route()
        ));
        if tag == "statistical" {
            // The scripted lattice violation: must be a typed rejection.
            let violation = env.justify(RegistryClaimClass::Invariant).unwrap_err();
            t.emit(&format!("lattice violation (typed): {violation}"));
        }
    }
    // The statically checked twin (compiles because proof >= statistical).
    let j = justify::<class::Proof, class::Statistical>();
    t.emit(&format!(
        "static justification: {} => {}",
        j.evidence().name(),
        j.target().name()
    ));

    // 5. Resource admission loop ending in a typed ceiling rejection.
    let ceiling = ResourceCeiling::new(ResourceVector {
        cpu_micros: 10,
        memory_bytes: 1000,
        io_bytes: 1000,
        io_ops: 1000,
        network_bytes: 1000,
    });
    let step = ResourceVector {
        cpu_micros: 3,
        memory_bytes: 10,
        io_bytes: 10,
        io_ops: 10,
        network_bytes: 10,
    };
    let mut used = ResourceVector::ZERO;
    let mut admitted = 0;
    let rejection = loop {
        let next = used.checked_add(step).expect("accumulate");
        match ceiling.admit(next) {
            Ok(_) => {
                used = next;
                admitted += 1;
            }
            Err(e) => break e,
        }
    };
    t.emit(&format!(
        "admitted {admitted} steps; rejection (typed): {rejection}"
    ));

    // 6. Shared replay grading and narrowed-Cx obligations under the lab.
    let replayable = ReplayCompleteness::replayable();
    let structural = ReplayCompleteness::structural_replay(
        [
            ReplayClass::LogicalState,
            ReplayClass::StructuralControlFlow,
        ],
        [ReplayClass::MediatedExternalInputs],
    )
    .expect("canonical structural replay fixture");
    let verifiable = ReplayCompleteness::verifiable_if_artifacts_supplied([
        ReplayClass::ExecutableBinary,
        ReplayClass::LogicalState,
    ])
    .expect("canonical missing-artifact fixture");
    let audit = structural
        .weaken_for_redaction([ReplayClass::KeyMaterial])
        .expect("bounded redaction fixture");
    t.emit(&format!(
        "replay grades: replayable={}; structural reproduced={} omitted={}; verifiable missing={}; audit missing_or_redacted={}",
        matches!(replayable, ReplayCompleteness::Replayable),
        structural.reproduced_classes().map_or(0, |set| set.len()),
        structural.omitted_classes().map_or(0, |set| set.len()),
        verifiable.missing_classes().map_or(0, |set| set.len()),
        audit
            .missing_or_redacted_classes()
            .map_or(0, |set| set.len()),
    ));

    let contexts = PurposeContexts::narrow_runtime_root(root);
    let obligation = contexts
        .commit()
        .reserve_prepared_bytes(
            ObligationId::new(0x0004_1999).expect("nonzero obligation fixture"),
            std::num::NonZeroU64::new(4096).expect("nonzero byte fixture"),
        )
        .expect("live lab context");
    let receipt = obligation
        .transfer()
        .expect("transfer boundary remains live")
        .publish()
        .expect("publication boundary remains live")
        .cleanup()
        .expect("cleanup boundary remains live")
        .complete()
        .expect("completion boundary remains live");
    let stages: Vec<String> = receipt
        .events()
        .map(|event| format!("{:?}", event.stage()))
        .collect();
    assert_eq!(contexts.outstanding_obligations(), 0);
    assert_eq!(receipt.resolution(), ObligationResolution::Discharged);
    assert_eq!(
        receipt.events().last().map(|event| event.stage()),
        Some(ObligationStage::Resolution)
    );
    t.emit(&format!(
        "narrowed cx obligation: role={:?} kind={:?} units={} resolution={:?} stages={}",
        receipt.role(),
        receipt.kind(),
        receipt.units(),
        receipt.resolution(),
        stages.join(","),
    ));
    t.emit(&format!(
        "merge capabilities: spawn={} time={} random={} io={} remote={}",
        contexts.merge_eval().capabilities().spawn,
        contexts.merge_eval().capabilities().time,
        contexts.merge_eval().capabilities().random,
        contexts.merge_eval().capabilities().io,
        contexts.merge_eval().capabilities().remote,
    ));

    let digest = t.digest.0;
    println!("transcript fnv1a: {digest:016x}");
}
