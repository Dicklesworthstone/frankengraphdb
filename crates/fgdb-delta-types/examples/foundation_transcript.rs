//! `foundation_types_e2e` transcript: one deterministic pass over every
//! foundation crate (bead `fgdb-w1-foundation-types-tjk`).
//!
//! The output is a line-oriented transcript that must be byte-identical
//! across runs (`scripts/w1_foundation_types_e2e.sh` runs it twice and
//! `cmp`s). It exercises, in order: every canonical scalar variant under
//! `STRICT_PORTABLE` (round trip + typed malformed rejections), a `ZWeight`
//! promotion across the `i128` boundary into `fgdb-bigint` and back, every
//! delta-row arm into a `LogicalDeltaTemplate` and — via a committed marker —
//! an ordered `LogicalDeltaBatch`, one evidence envelope per §15.0 claim
//! kind, a scripted claim-lattice violation producing the typed rejection,
//! and a resource-admission loop ending in a typed ceiling rejection.
//! The final line is an FNV-1a digest of the whole transcript (std-only,
//! stable across processes and toolchains, unlike `DefaultHasher`).
//!
//! Lab-runtime (fgdb-sim) integration replaces this direct harness when
//! `fgdb-verif-sim` lands; determinism-under-two-seeds is what this stage
//! can honestly assert.

use fgdb_bigint::{BigInt, LimbLimit};
use fgdb_claim::{EvidenceClaim, RefinementStatus, RegistryClaimClass, class, justify};
use fgdb_delta_types::{
    CommittedMarker, DeltaRow, DeltaTemplateKey, ElementId, EscrowDomainId, LabelId,
    LogicalDeltaBatch, LogicalDeltaTemplate, OperationKey, PropertyKeyId, RelationId, SchemaEpoch,
    ValidTimePeriod,
};
use fgdb_evidence::{CalibrationWindow, EvidenceEnvelope, FallbackBehavior};
use fgdb_resource::{ResourceCeiling, ResourceVector};
use fgdb_types::{
    BranchId, CanonicalF64, CanonicalScalar, CommitSeq, EId, GraphId, MarkerRef, ObjectId, VId,
};

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
    let mut t = Transcript {
        digest: Fnv1a::new(),
    };
    t.emit("== foundation_types_e2e transcript v1 ==");

    // 1. Canonical scalars: every variant, encode/decode round trip.
    let scalars = [
        CanonicalScalar::Null,
        CanonicalScalar::Bool(true),
        CanonicalScalar::Int(-41999),
        CanonicalScalar::Float(CanonicalF64::new(-0.0)),
        CanonicalScalar::Float(CanonicalF64::new(f64::NAN)),
        CanonicalScalar::Float(CanonicalF64::new(2.5)),
        CanonicalScalar::Text("frankengraphdb".into()),
        CanonicalScalar::Bytes(vec![0xF0, 0x0D]),
    ];
    for s in &scalars {
        let enc = s.encode();
        let back = CanonicalScalar::decode(&enc).expect("round trip");
        assert_eq!(&back, s);
        t.emit(&format!("scalar {:?} encodes {}", s, hex(&enc)));
    }
    // Typed malformed rejections.
    let bad_float = {
        let mut e = vec![0x03];
        e.extend_from_slice(&0x7FF0_0000_0000_0001u64.to_le_bytes());
        e
    };
    t.emit(&format!(
        "scalar reject non-canonical float: {}",
        CanonicalScalar::decode(&bad_float).unwrap_err()
    ));
    let mut huge = vec![0x05];
    huge.extend_from_slice(&u64::MAX.to_le_bytes());
    t.emit(&format!(
        "scalar reject oversized declared length: {}",
        CanonicalScalar::decode(&huge).unwrap_err()
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
            props: vec![(PropertyKeyId(1), CanonicalScalar::Text("Ada".into()))],
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
                alpha: 0.01,
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

    let digest = t.digest.0;
    println!("transcript fnv1a: {digest:016x}");
}
