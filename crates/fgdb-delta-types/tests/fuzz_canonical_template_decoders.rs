//! Deterministic mutation campaign for the two `decode_canonical` APIs.
//!
//! Default: 50,000 actually changed byte strings, each sent to BOTH decoders,
//! plus byte-preserving replacement replays. `FGDB_TEMPLATE_FUZZ_CASES` can
//! increase (never decrease) that floor. The debug campaign has a 90-second
//! elapsed budget; `FGDB_TEMPLATE_FUZZ_SECONDS` can increase it for longer runs.
//! Each input, including successful decode/encode/decode checks, must return
//! within one second. These are observed wall bounds, not preemptive timeouts;
//! the test runner still needs an external timeout to interrupt a hung decoder.
//!
//! Memory method: cap every generated input at 4096 bytes, retain only the
//! small encoder corpus and one mutant, and inspect allocation guards in
//! canonical.rs. Reader::count checks declared * minimum against remaining
//! bytes BEFORE Vec::with_capacity: labels /8, retired edges /16, properties
//! /12, rows /5, coordinates /53. Scalar and row byte lengths use count(1)
//! followed by checked slicing. Nested scalar decoding enforces its own profile
//! and present-input bounds. Thus no forged u32 count directly requests an
//! unbounded allocation here. This is bounded-input plus source-audited length
//! guards, NOT allocator instrumentation or an RSS measurement; elapsed time
//! alone proves nothing about memory. Mutants are not retained between calls.

use std::time::{Duration, Instant};

use fgdb_delta_types::{
    CanonicalError, CoordinateEntry, DeltaRow, ElementId, EscrowDomainId, LabelId,
    LogicalDeltaTemplate, OperationKey, PropertyKeyId, RelationId, SchemaEpoch, ValidTimePeriod,
};
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, ObjectId, VId};

const MAX_INPUT: usize = 4096;
const INPUT_BUDGET: Duration = Duration::from_secs(1);

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn index(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

struct Seed {
    bytes: Vec<u8>,
    row_tag: usize,
    counts: Vec<usize>,
}

fn rows() -> Vec<DeltaRow> {
    let scalars = [
        CanonicalScalar::Null,
        CanonicalScalar::Bool(false),
        CanonicalScalar::Bool(true),
        CanonicalScalar::Int(i64::MIN),
        CanonicalScalar::Int(i64::MAX),
        CanonicalScalar::ucs_basic_text("a\0é").unwrap(),
        CanonicalScalar::bytes(vec![0, 1, 127, 255]).unwrap(),
    ];
    let mut rows = Vec::new();
    for (index, scalar) in scalars.iter().enumerate() {
        let n = index as u64;
        let populated = index % 2 == 0;
        let elem = if populated {
            ElementId::Vertex(VId(n as u128 + 1))
        } else {
            ElementId::Edge(EId(n as u128 + 1))
        };
        let period = ValidTimePeriod {
            start_micros: -100 - n as i64,
            end_micros: if populated {
                Some(100 + n as i64)
            } else {
                None
            },
        };
        let props = vec![
            (PropertyKeyId(1), scalar.clone()),
            (PropertyKeyId(9), CanonicalScalar::Int(-(n as i64))),
        ];
        rows.extend([
            DeltaRow::CreateVertex {
                vid: VId(n as u128 + 1),
                birth_ordinal: n,
                labels: vec![LabelId(1), LabelId(9)],
                props: props.clone(),
                valid_time: populated.then_some(period),
            },
            DeltaRow::CreateEdge {
                eid: EId(n as u128 + 1),
                birth_ordinal: n,
                src: VId(1),
                relation: RelationId(4),
                dst: VId(2),
                canonical_key: populated.then(|| scalar.clone()),
                props,
                valid_time: Some(period),
            },
            DeltaRow::DeleteVertex {
                vid: VId(n as u128 + 1),
                before_version: ObjectId([index as u8; 32]),
                sorted_retired_incident_edges: vec![EId(1), EId(10)],
            },
            DeltaRow::DeleteEdge {
                eid: EId(n as u128 + 1),
                before_version: ObjectId([index as u8; 32]),
            },
            DeltaRow::LabelMembership {
                vid: VId(n as u128 + 1),
                label: LabelId(n),
                before: populated,
                after: !populated,
            },
            DeltaRow::Property {
                elem,
                property: PropertyKeyId(n),
                before: populated.then(|| scalar.clone()),
                after: Some(scalar.clone()),
            },
            DeltaRow::ValidTime {
                elem,
                contract_id: ObjectId([4; 32]),
                before: populated.then_some(period),
                after: Some(ValidTimePeriod {
                    start_micros: 0,
                    end_micros: None,
                }),
            },
            DeltaRow::Counter {
                operation_key: OperationKey([index as u8; 32]),
                elem,
                property: PropertyKeyId(n),
                algebra_profile: ObjectId([5; 32]),
                delta: -7,
                before: 100 + n as i128,
                after: 93 + n as i128,
            },
            DeltaRow::Escrow {
                domain_id: EscrowDomainId(n as u128),
                epoch: n,
                operation_key: OperationKey([index as u8; 32]),
                subject: elem,
                subject_property: populated.then_some(PropertyKeyId(n)),
                delta: 1,
                before_value: n as i128,
                after_value: n as i128 + 1,
            },
            DeltaRow::Sketch {
                operation_key: OperationKey([index as u8; 32]),
                sketch_profile_oid: ObjectId([6; 32]),
                before_state_digest: [index as u8; 32],
                after_state_oid: ObjectId([7; 32]),
            },
            DeltaRow::Schema {
                transition_oid: ObjectId([index as u8; 32]),
                before_epoch: SchemaEpoch(n),
                after_epoch: SchemaEpoch(n + 1),
            },
            DeltaRow::Constraint {
                before_schema_root: ObjectId([index as u8; 32]),
                after_schema_root: ObjectId([8; 32]),
                before_constraint_root: ObjectId([9; 32]),
                after_constraint_root: ObjectId([10; 32]),
            },
        ]);
    }
    rows
}

fn seeds() -> Vec<Seed> {
    let mut seeds = Vec::new();
    let mut variants = [false; 12];
    for (index, row) in rows().into_iter().enumerate() {
        let bytes = row.canonical_bytes().unwrap();
        variants[usize::from(bytes[0] - 1)] = true;
        let decoded = DeltaRow::decode_canonical(&bytes).unwrap();
        assert_eq!(decoded, row);
        assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
        let counts = match bytes[0] {
            1 => vec![25, 45], // label count, then property count after two labels
            3 => vec![49],     // retired-edge count
            _ => Vec::new(),
        };
        let template = LogicalDeltaTemplate::build(
            ObjectId([11; 32]),
            [index as u8; 32],
            vec![CoordinateEntry {
                graph: GraphId(index as u128 + 1),
                branch: BranchId(2),
                relation: RelationId(3),
                schema_epoch: SchemaEpoch(4),
                schema_transition: None,
                rows: vec![row],
            }],
        )
        .unwrap();
        let template_bytes = template.canonical_bytes().unwrap();
        let decoded = LogicalDeltaTemplate::decode_canonical(&template_bytes).unwrap();
        assert_eq!(decoded, template);
        assert_eq!(decoded.canonical_bytes().unwrap(), template_bytes);
        // V1 header: 2 + 32 + 32 + 4; coordinate: 16 + 16 + 8 + 8 + 1 + 4;
        // the following four-byte row length puts the nested row tag at 127.
        assert_eq!(&template_bytes[127..], bytes.as_slice());
        let mut template_counts = vec![66, 119, 123];
        template_counts.extend(counts.iter().map(|offset| 127 + offset));
        seeds.push(Seed {
            bytes,
            row_tag: 0,
            counts,
        });
        seeds.push(Seed {
            bytes: template_bytes,
            row_tag: 127,
            counts: template_counts,
        });
    }
    assert!(variants.into_iter().all(|covered| covered));
    assert!(seeds.iter().all(|seed| seed.bytes.len() < MAX_INPUT / 2));
    seeds
}

fn mutate(seed: &Seed, op: usize, rng: &mut Rng) -> Vec<u8> {
    let mut bytes = seed.bytes.clone();
    match op {
        0 => {
            let at = rng.index(bytes.len());
            bytes[at] ^= 1 << rng.index(8);
        }
        1 => {
            let at = rng.index(bytes.len());
            bytes[at] = rng.next() as u8;
        }
        2 => bytes.truncate(rng.index(bytes.len())),
        3 => {
            let at = if seed.counts.is_empty() {
                1 + rng.index(bytes.len() - 4)
            } else {
                seed.counts[rng.index(seed.counts.len())]
            };
            let inflated = [u32::MAX, 0x8000_0000, 65_536][rng.index(3)];
            bytes[at..at + 4].copy_from_slice(&inflated.to_be_bytes());
        }
        4 => {
            let start = rng.index(bytes.len());
            let len = (bytes.len() - start).min(1 + rng.index(64));
            let copy = bytes[start..start + len].to_vec();
            bytes.splice(start..start, copy);
        }
        5 => {
            let len = 1 + rng.index(64);
            for _ in 0..len {
                bytes.push(rng.next() as u8);
            }
        }
        6 => {
            // A REAL property tag with an invalid nested element tag. For a
            // template, keep framing consistent so the inner reader is reached.
            bytes.truncate(seed.row_tag + 2);
            bytes[seed.row_tag] = 6;
            bytes[seed.row_tag + 1] = 0xff;
            if seed.row_tag != 0 {
                bytes[seed.row_tag - 4..seed.row_tag].copy_from_slice(&2u32.to_be_bytes());
            }
        }
        7 => {
            // Replacement's byte-preserving member replays valid encoder seeds.
            let at = rng.index(bytes.len());
            bytes[at] = seed.bytes[at];
        }
        _ => unreachable!("only eight mutation operators"),
    }
    assert!(bytes.len() <= MAX_INPUT);
    bytes
}

#[derive(Debug, Default)]
struct Counts {
    ok: usize,
    err: usize,
    changed_ok: usize,
    post_tag: usize,
}

fn exercise(bytes: &[u8], changed: bool, row: &mut Counts, template: &mut Counts) {
    let started = Instant::now();
    match DeltaRow::decode_canonical(bytes) {
        Ok(value) => {
            row.ok += 1;
            row.changed_ok += usize::from(changed);
            let canonical = value.canonical_bytes().expect("successful row must encode");
            let again = DeltaRow::decode_canonical(&canonical).expect("canonical row must decode");
            assert_eq!(again, value);
            assert_eq!(again.canonical_bytes().unwrap(), canonical);
        }
        Err(error) => {
            row.err += 1;
            if bytes.starts_with(&[6, 0xff]) {
                assert_eq!(error, CanonicalError::UnknownTag { tag: 0xff });
                row.post_tag += 1;
            }
        }
    }
    match LogicalDeltaTemplate::decode_canonical(bytes) {
        Ok(value) => {
            template.ok += 1;
            template.changed_ok += usize::from(changed);
            let canonical = value
                .canonical_bytes()
                .expect("successful template must encode");
            let again = LogicalDeltaTemplate::decode_canonical(&canonical)
                .expect("canonical template must decode");
            assert_eq!(again, value);
            assert_eq!(again.canonical_bytes().unwrap(), canonical);
        }
        Err(error) => {
            template.err += 1;
            if bytes.len() == 129
                && bytes.starts_with(&[0, 1])
                && bytes[123..129] == [0, 0, 0, 2, 6, 0xff]
            {
                assert_eq!(error, CanonicalError::UnknownTag { tag: 0xff });
                template.post_tag += 1;
            }
        }
    }
    assert!(
        started.elapsed() <= INPUT_BUDGET,
        "per-input wall budget exceeded"
    );
}

#[test]
fn canonical_template_decoders_mutation_campaign() {
    let target = std::env::var("FGDB_TEMPLATE_FUZZ_CASES")
        .map(|value| {
            value
                .parse::<usize>()
                .expect("FGDB_TEMPLATE_FUZZ_CASES must be an integer")
        })
        .unwrap_or(50_000)
        .max(50_000);
    let seconds = std::env::var("FGDB_TEMPLATE_FUZZ_SECONDS")
        .map(|value| {
            value
                .parse::<u64>()
                .expect("FGDB_TEMPLATE_FUZZ_SECONDS must be an integer")
        })
        .unwrap_or(90)
        .max(90);
    let started = Instant::now();
    let seeds = seeds();
    let mut rng = Rng(0x7465_6d70_6c61_7465);
    let mut row = Counts::default();
    let mut template = Counts::default();
    let mut executed = 0usize;
    let mut replayed = 0usize;
    let mut attempts = 0usize;
    let mut changed_by_op = [0usize; 8];
    while executed < target {
        let op = attempts % 8;
        let seed = &seeds[(attempts / 8) % seeds.len()];
        let bytes = mutate(seed, op, &mut rng);
        let changed = bytes != seed.bytes;
        exercise(&bytes, changed, &mut row, &mut template);
        if changed {
            executed += 1; // exactly once AFTER this changed input hit both APIs
            changed_by_op[op] += 1;
        } else {
            replayed += 1;
        }
        attempts += 1;
        assert!(
            started.elapsed() <= Duration::from_secs(seconds),
            "campaign wall budget exceeded"
        );
    }
    assert!(
        executed >= 50_000,
        "actually executed changed inputs: {executed}"
    );
    assert!(replayed > 0);
    assert!(changed_by_op[..7].iter().all(|count| *count > 0));
    for counts in [&row, &template] {
        assert_eq!(counts.ok + counts.err, executed + replayed);
        assert!(counts.ok > 0 && counts.err > 0, "{counts:?}");
        assert!(
            counts.changed_ok > 0,
            "only unchanged seeds decoded: {counts:?}"
        );
        assert!(counts.post_tag > 0, "no nested-tag witness: {counts:?}");
    }
    eprintln!(
        "changed={executed} replayed={replayed} operators={changed_by_op:?} row={row:?} template={template:?} elapsed={:?}",
        started.elapsed()
    );
}
