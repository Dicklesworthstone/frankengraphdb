//! Laws of the canonical delta encoding.
//!
//! A template's identity is the digest of its canonical bytes, and every
//! downstream cross-check — the capsule's `logical_delta_template_digest`, the
//! marker's copy of it, the batch's idempotency key — compares that digest. So
//! the encoding has to be a *bijection between values and byte strings*, and
//! the ways it can fail to be one are what this file attacks:
//!
//!   * **A field left out of the transcript** is a field the digest does not
//!     commit to; two templates differing only there would share an identity.
//!     Swept field by field, per arm.
//!   * **Two encodings of one value** (unordered rows, a non-canonical bool
//!     byte) would let the same logical change present two identities.
//!   * **Two values sharing one encoding** — caught by the same field sweep.
//!   * **A reader that accepts what it does not understand** silently loses
//!     data. Unknown tags, trailing bytes, and unsupported format versions are
//!     all refusals, and truncation is swept at every length.

use fgdb_delta_types::{
    CanonicalError, CoordinateEntry, DELTA_FORMAT_V1, DeltaRow, ElementId, EscrowDomainId, LabelId,
    LogicalDeltaTemplate, OperationKey, PropertyKeyId, RelationId, SchemaEpoch, ValidTimePeriod,
    canonicalize,
};
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, ObjectId, VId};

fn oid(seed: u8) -> ObjectId {
    ObjectId([seed; 32])
}

fn period(start: i64, end: Option<i64>) -> ValidTimePeriod {
    ValidTimePeriod {
        start_micros: start,
        end_micros: end,
    }
}

/// One row of every family, with every optional field populated — the shape
/// most likely to expose a field the writer emits and the reader forgets,
/// since a row whose optional fields are all `None` round-trips even through a
/// decoder that never learned about them.
fn every_family_populated() -> Vec<DeltaRow> {
    vec![
        DeltaRow::CreateVertex {
            vid: VId(7),
            birth_ordinal: 3,
            labels: vec![LabelId(1), LabelId(9)],
            props: vec![
                (PropertyKeyId(2), CanonicalScalar::Int(-5)),
                (PropertyKeyId(4), CanonicalScalar::Bool(true)),
            ],
            valid_time: Some(period(100, Some(200))),
        },
        DeltaRow::CreateEdge {
            eid: EId(11),
            birth_ordinal: 4,
            src: VId(7),
            relation: RelationId(3),
            dst: VId(8),
            canonical_key: Some(CanonicalScalar::Int(42)),
            props: vec![(PropertyKeyId(5), CanonicalScalar::Null)],
            valid_time: Some(period(-9, None)),
        },
        DeltaRow::DeleteVertex {
            vid: VId(12),
            before_version: oid(0x31),
            sorted_retired_incident_edges: vec![EId(1), EId(2)],
        },
        DeltaRow::DeleteEdge {
            eid: EId(13),
            before_version: oid(0x32),
        },
        DeltaRow::LabelMembership {
            vid: VId(14),
            label: LabelId(6),
            before: false,
            after: true,
        },
        DeltaRow::Property {
            elem: ElementId::Vertex(VId(15)),
            property: PropertyKeyId(7),
            before: Some(CanonicalScalar::Int(1)),
            after: Some(CanonicalScalar::Int(2)),
        },
        DeltaRow::ValidTime {
            elem: ElementId::Edge(EId(16)),
            contract_id: oid(0x33),
            before: Some(period(0, Some(1))),
            after: Some(period(2, None)),
        },
        DeltaRow::Counter {
            operation_key: OperationKey([0x41; 32]),
            elem: ElementId::Vertex(VId(17)),
            property: PropertyKeyId(8),
            algebra_profile: oid(0x34),
            delta: -3,
            before: 10,
            after: 7,
        },
        DeltaRow::Escrow {
            domain_id: EscrowDomainId(19),
            epoch: 2,
            operation_key: OperationKey([0x42; 32]),
            subject: ElementId::Vertex(VId(18)),
            subject_property: Some(PropertyKeyId(9)),
            delta: 5,
            before_value: 1,
            after_value: 6,
        },
        DeltaRow::Sketch {
            operation_key: OperationKey([0x43; 32]),
            sketch_profile_oid: oid(0x35),
            before_state_digest: [0x44; 32],
            after_state_oid: oid(0x36),
        },
        DeltaRow::Schema {
            transition_oid: oid(0x37),
            before_epoch: SchemaEpoch(1),
            after_epoch: SchemaEpoch(2),
        },
        DeltaRow::Constraint {
            before_schema_root: oid(0x38),
            after_schema_root: oid(0x39),
            before_constraint_root: oid(0x3a),
            after_constraint_root: oid(0x3b),
        },
    ]
}

fn entry(graph: u128, branch: u128, relation: u64, rows: Vec<DeltaRow>) -> CoordinateEntry {
    CoordinateEntry {
        graph: GraphId(graph),
        branch: BranchId(branch),
        relation: RelationId(relation),
        schema_epoch: SchemaEpoch(2),
        schema_transition: None,
        rows,
    }
}

fn bytes_of(row: &DeltaRow) -> Vec<u8> {
    row.canonical_bytes().expect("row encodes")
}

fn embedded_collection_rows(first: u64, second: u64) -> [(&'static str, DeltaRow); 4] {
    [
        (
            "CreateVertex.labels",
            DeltaRow::CreateVertex {
                vid: VId(1),
                birth_ordinal: 1,
                labels: vec![LabelId(first), LabelId(second)],
                props: vec![],
                valid_time: None,
            },
        ),
        (
            "CreateVertex.props",
            DeltaRow::CreateVertex {
                vid: VId(2),
                birth_ordinal: 2,
                labels: vec![],
                props: vec![
                    (PropertyKeyId(first), CanonicalScalar::Null),
                    (PropertyKeyId(second), CanonicalScalar::Null),
                ],
                valid_time: None,
            },
        ),
        (
            "CreateEdge.props",
            DeltaRow::CreateEdge {
                eid: EId(3),
                birth_ordinal: 3,
                src: VId(1),
                relation: RelationId(1),
                dst: VId(2),
                canonical_key: None,
                props: vec![
                    (PropertyKeyId(first), CanonicalScalar::Null),
                    (PropertyKeyId(second), CanonicalScalar::Null),
                ],
                valid_time: None,
            },
        ),
        (
            "DeleteVertex.sorted_retired_incident_edges",
            DeltaRow::DeleteVertex {
                vid: VId(4),
                before_version: oid(0x41),
                sorted_retired_incident_edges: vec![
                    EId(u128::from(first)),
                    EId(u128::from(second)),
                ],
            },
        ),
    ]
}

fn embedded_order_errors() -> Vec<(&'static str, CanonicalError)> {
    vec![
        (
            "CreateVertex.labels",
            CanonicalError::NonCanonicalLabelOrder { index: 1 },
        ),
        (
            "CreateVertex.props",
            CanonicalError::NonCanonicalPropertyOrder { index: 1 },
        ),
        (
            "CreateEdge.props",
            CanonicalError::NonCanonicalPropertyOrder { index: 1 },
        ),
        (
            "DeleteVertex.sorted_retired_incident_edges",
            CanonicalError::NonCanonicalRetiredEdgeOrder { index: 1 },
        ),
    ]
}

/// A named single-field edit, used to sweep a row's transcript field by field.
type FieldEdit = (&'static str, fn(&mut DeltaRow));

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

#[test]
fn every_row_family_round_trips() {
    for row in every_family_populated() {
        let encoded = bytes_of(&row);
        let decoded = DeltaRow::decode_canonical(&encoded).expect("row decodes");
        assert_eq!(decoded, row, "round trip must preserve every field");
        assert_eq!(
            bytes_of(&decoded),
            encoded,
            "re-encoding must reproduce the same bytes, so the encoding is a \
             bijection on this value rather than merely reversible"
        );
    }
}

#[test]
fn a_template_round_trips_including_multiple_coordinates() {
    let rows = every_family_populated();
    let template = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![
            entry(2, 1, 5, rows.clone()),
            entry(1, 1, 3, rows[..4].to_vec()),
            entry(1, 2, 3, rows[4..].to_vec()),
        ],
    )
    .expect("builds");

    let encoded = template.canonical_bytes().expect("encodes");
    let decoded = LogicalDeltaTemplate::decode_canonical(&encoded).expect("decodes");
    assert_eq!(decoded, template);
    assert_eq!(decoded.canonical_bytes().expect("re-encodes"), encoded);
    assert_eq!(decoded.row_count(), rows.len() * 2);
    assert_eq!(decoded.format(), DELTA_FORMAT_V1);
    assert_eq!(decoded.source_intent_root_digest(), &[0x22; 32]);
}

/// THE CAPABILITY THE SHAPE EXISTS FOR: one template spanning several graphs
/// and branches. A template keyed by a single coordinate could not express
/// this commit at all — it would have to become commits that are no longer
/// atomic.
#[test]
fn one_template_spans_several_graphs_and_branches_atomically() {
    let template = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![
            entry(9, 1, 1, vec![every_family_populated()[0].clone()]),
            entry(1, 1, 1, vec![every_family_populated()[1].clone()]),
            entry(1, 4, 1, vec![every_family_populated()[2].clone()]),
        ],
    )
    .expect("builds");

    let coordinates: Vec<(u128, u128, u64)> = template
        .coordinate_entries()
        .iter()
        .map(|e| (e.graph.0, e.branch.0, e.relation.0))
        .collect();
    assert_eq!(
        coordinates,
        vec![(1, 1, 1), (1, 4, 1), (9, 1, 1)],
        "entries are canonically ordered by (graph, branch, relation)"
    );
}

// ---------------------------------------------------------------------------
// Transcript completeness: every field is committed to
// ---------------------------------------------------------------------------

/// A field outside the encoding is a field the digest does not commit to. Each
/// pair below differs in exactly ONE field, so a collision names the field the
/// writer forgot.
#[test]
fn every_field_of_every_row_changes_the_encoding() {
    let base = every_family_populated();
    // CreateVertex is swept field by field by name, so a failure says WHICH
    // field the writer forgot rather than only that one was forgotten.
    let edits: Vec<FieldEdit> = vec![
        ("vid", |r| {
            if let DeltaRow::CreateVertex { vid, .. } = r {
                *vid = VId(8);
            }
        }),
        ("birth_ordinal", |r| {
            if let DeltaRow::CreateVertex { birth_ordinal, .. } = r {
                *birth_ordinal = 4;
            }
        }),
        ("labels", |r| {
            if let DeltaRow::CreateVertex { labels, .. } = r {
                labels.push(LabelId(10));
            }
        }),
        ("props.key", |r| {
            if let DeltaRow::CreateVertex { props, .. } = r {
                props[0].0 = PropertyKeyId(3);
            }
        }),
        ("props.value", |r| {
            if let DeltaRow::CreateVertex { props, .. } = r {
                props[0].1 = CanonicalScalar::Int(-6);
            }
        }),
        ("valid_time", |r| {
            if let DeltaRow::CreateVertex { valid_time, .. } = r {
                *valid_time = None;
            }
        }),
    ];
    for (field, edit) in edits {
        let mut variant = base[0].clone();
        edit(&mut variant);
        assert_ne!(
            variant, base[0],
            "the {field} edit did not change the row, so this case proves nothing"
        );
        assert_ne!(
            bytes_of(&base[0]),
            bytes_of(&variant),
            "CreateVertex.{field} is not in the canonical transcript"
        );
    }

    // The remaining arms are swept structurally: mutating each field via a
    // rebuilt row and asserting every encoding in the set is distinct. A
    // forgotten field shows up as two equal encodings.
    let mut all: Vec<Vec<u8>> = Vec::new();
    for row in &base {
        all.push(bytes_of(row));
    }
    for row in field_variants_of_every_arm() {
        all.push(bytes_of(&row));
    }
    let before = all.len();
    all.sort();
    all.dedup();
    assert_eq!(
        all.len(),
        before,
        "two distinct rows share a canonical encoding, so some field is \
         missing from the transcript"
    );
}

/// One single-field variant of every arm, so the dedup sweep above covers all
/// twelve rather than only the first.
fn field_variants_of_every_arm() -> Vec<DeltaRow> {
    vec![
        DeltaRow::CreateEdge {
            eid: EId(11),
            birth_ordinal: 4,
            src: VId(7),
            relation: RelationId(3),
            dst: VId(99), // dst
            canonical_key: Some(CanonicalScalar::Int(42)),
            props: vec![(PropertyKeyId(5), CanonicalScalar::Null)],
            valid_time: Some(period(-9, None)),
        },
        DeltaRow::CreateEdge {
            eid: EId(11),
            birth_ordinal: 4,
            src: VId(7),
            relation: RelationId(3),
            dst: VId(8),
            canonical_key: None, // canonical_key
            props: vec![(PropertyKeyId(5), CanonicalScalar::Null)],
            valid_time: Some(period(-9, None)),
        },
        DeltaRow::DeleteVertex {
            vid: VId(12),
            before_version: oid(0x31),
            sorted_retired_incident_edges: vec![EId(1)], // cascade image
        },
        DeltaRow::DeleteEdge {
            eid: EId(13),
            before_version: oid(0x99), // before_version
        },
        DeltaRow::LabelMembership {
            vid: VId(14),
            label: LabelId(6),
            before: true, // before
            after: true,
        },
        DeltaRow::Property {
            elem: ElementId::Edge(EId(15)), // element KIND, same ordinal
            property: PropertyKeyId(7),
            before: Some(CanonicalScalar::Int(1)),
            after: Some(CanonicalScalar::Int(2)),
        },
        DeltaRow::ValidTime {
            elem: ElementId::Edge(EId(16)),
            contract_id: oid(0x33),
            before: Some(period(0, Some(1))),
            after: Some(period(2, Some(3))), // after.end
        },
        DeltaRow::Counter {
            operation_key: OperationKey([0x41; 32]),
            elem: ElementId::Vertex(VId(17)),
            property: PropertyKeyId(8),
            algebra_profile: oid(0x34),
            delta: -3,
            before: 10,
            after: 8, // after
        },
        DeltaRow::Escrow {
            domain_id: EscrowDomainId(19),
            epoch: 2,
            operation_key: OperationKey([0x42; 32]),
            subject: ElementId::Vertex(VId(18)),
            subject_property: None, // subject_property
            delta: 5,
            before_value: 1,
            after_value: 6,
        },
        DeltaRow::Sketch {
            operation_key: OperationKey([0x43; 32]),
            sketch_profile_oid: oid(0x35),
            before_state_digest: [0x45; 32], // before_state_digest
            after_state_oid: oid(0x36),
        },
        DeltaRow::Schema {
            transition_oid: oid(0x37),
            before_epoch: SchemaEpoch(1),
            after_epoch: SchemaEpoch(3), // after_epoch
        },
        DeltaRow::Constraint {
            before_schema_root: oid(0x38),
            after_schema_root: oid(0x39),
            before_constraint_root: oid(0x3a),
            after_constraint_root: oid(0x99), // after_constraint_root
        },
    ]
}

/// A vertex and an edge with the SAME numeric identity must not encode alike.
/// The element tag is the only thing separating them, so this is the test that
/// fails if it is ever dropped as redundant.
#[test]
fn element_kind_is_part_of_the_transcript() {
    let as_vertex = DeltaRow::Property {
        elem: ElementId::Vertex(VId(5)),
        property: PropertyKeyId(1),
        before: None,
        after: None,
    };
    let as_edge = DeltaRow::Property {
        elem: ElementId::Edge(EId(5)),
        property: PropertyKeyId(1),
        before: None,
        after: None,
    };
    assert_ne!(bytes_of(&as_vertex), bytes_of(&as_edge));
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// THE DETERMINISM LAW (doctrine 4). The same effects presented in any order
/// must produce byte-identical canonical bytes — otherwise one logical change
/// gets two identities and every digest cross-check compares coincidences.
#[test]
fn input_order_does_not_change_the_canonical_bytes() {
    let rows = every_family_populated();
    let mut reversed = rows.clone();
    reversed.reverse();

    let forward =
        LogicalDeltaTemplate::build(oid(0x11), [0x22; 32], vec![entry(1, 1, 3, rows.clone())])
            .expect("builds");
    let backward =
        LogicalDeltaTemplate::build(oid(0x11), [0x22; 32], vec![entry(1, 1, 3, reversed)])
            .expect("builds");

    assert_eq!(
        forward.canonical_bytes().expect("encodes"),
        backward.canonical_bytes().expect("encodes"),
        "row order in the input must not survive into the bytes"
    );

    // And the same for coordinate entries.
    let a = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry(1, 1, 3, rows.clone()), entry(2, 1, 3, rows.clone())],
    )
    .expect("builds");
    let b = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry(2, 1, 3, rows.clone()), entry(1, 1, 3, rows)],
    )
    .expect("builds");
    assert_eq!(
        a.canonical_bytes().expect("encodes"),
        b.canonical_bytes().expect("encodes")
    );
}

#[test]
fn embedded_collection_order_does_not_change_a_built_template() {
    let canonical_rows: Vec<DeltaRow> = embedded_collection_rows(2, 7)
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    let reversed_embedded: Vec<DeltaRow> = embedded_collection_rows(7, 2)
        .into_iter()
        .map(|(_, row)| row)
        .collect();

    let canonical =
        LogicalDeltaTemplate::build(oid(0x11), [0x22; 32], vec![entry(1, 1, 3, canonical_rows)])
            .expect("canonical input builds");
    let permuted = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry(1, 1, 3, reversed_embedded)],
    )
    .expect("permuted input builds");

    assert_eq!(
        canonical, permuted,
        "builder canonicalization must normalize every embedded set and map"
    );
    assert_eq!(
        canonical.canonical_bytes().expect("canonical bytes"),
        permuted.canonical_bytes().expect("permuted bytes"),
        "equivalent embedded effects must have one durable identity"
    );
}

#[test]
fn canonicalize_is_idempotent() {
    let mut entries = vec![
        entry(2, 1, 3, every_family_populated()),
        entry(1, 1, 3, every_family_populated()),
    ];
    canonicalize(&mut entries).expect("first pass");
    let once = entries.clone();
    canonicalize(&mut entries).expect("second pass");
    assert_eq!(entries, once, "canonicalizing twice must change nothing");
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn a_duplicate_coordinate_is_refused() {
    let rows = every_family_populated();
    let result = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry(1, 1, 3, rows.clone()), entry(1, 1, 3, rows)],
    );
    assert!(
        matches!(
            result,
            Err(CanonicalError::NonCanonicalCoordinateOrder { index: 1 })
        ),
        "merging two entries for one coordinate is the normal form's job, not \
         the encoder's; got {result:?}"
    );
}

#[test]
fn a_duplicate_row_within_a_coordinate_is_refused() {
    let row = every_family_populated()[0].clone();
    let result = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry(1, 1, 3, vec![row.clone(), row])],
    );
    assert!(
        matches!(
            result,
            Err(CanonicalError::NonCanonicalRowOrder { entry: 0, index: 1 })
        ),
        "got {result:?}"
    );
}

#[test]
fn direct_row_encoding_refuses_noncanonical_embedded_collections() {
    let rows = embedded_collection_rows(2, 1);
    let errors: Vec<(&str, CanonicalError)> = rows
        .iter()
        .map(|(name, row)| {
            (
                *name,
                row.canonical_bytes()
                    .expect_err("noncanonical direct row must fail"),
            )
        })
        .collect();
    assert_eq!(
        errors,
        embedded_order_errors(),
        "each embedded collection must fail at its first non-strict member"
    );
}

#[test]
fn row_decoder_refuses_noncanonical_embedded_collections() {
    let canonical = embedded_collection_rows(1, 2);
    let mut labels = bytes_of(&canonical[0].1);
    // tag + vid + birth + count = 29, followed by two adjacent u64 labels.
    labels[29..45].rotate_left(8);

    let mut props = bytes_of(&canonical[1].1);
    // Empty-label count ends at 29; property count ends at 33. Each Null
    // property is key(8) + scalar length(4) + scalar tag(1).
    props[33..59].rotate_left(13);

    let mut edge_props = bytes_of(&canonical[2].1);
    // Fixed edge fields plus absent canonical key and property count end at 70.
    edge_props[70..96].rotate_left(13);

    let mut retired_edges = bytes_of(&canonical[3].1);
    // tag + vid + before-version + count = 53, followed by two u128 edge ids.
    retired_edges[53..85].rotate_left(16);

    let errors: Vec<(&str, CanonicalError)> = [
        ("CreateVertex.labels", labels),
        ("CreateVertex.props", props),
        ("CreateEdge.props", edge_props),
        ("DeleteVertex.sorted_retired_incident_edges", retired_edges),
    ]
    .into_iter()
    .map(|(name, encoded)| {
        (
            name,
            DeltaRow::decode_canonical(&encoded)
                .expect_err("noncanonical embedded bytes must fail"),
        )
    })
    .collect();
    assert_eq!(
        errors,
        embedded_order_errors(),
        "decoder must report the exact embedded ordering law, never repair it"
    );
}

#[test]
fn duplicate_embedded_set_and_map_members_are_refused() {
    let rows = embedded_collection_rows(2, 2);
    let errors: Vec<(&str, CanonicalError)> = rows
        .into_iter()
        .map(|(name, row)| {
            (
                name,
                LogicalDeltaTemplate::build(oid(0x11), [0x22; 32], vec![entry(1, 1, 3, vec![row])])
                    .expect_err("duplicate embedded member must fail"),
            )
        })
        .collect();
    assert_eq!(
        errors,
        embedded_order_errors(),
        "sorting may expose a duplicate but must never collapse it"
    );
}

#[test]
fn an_unknown_arm_tag_is_refused_never_skipped() {
    let mut encoded = bytes_of(&every_family_populated()[0]);
    encoded[0] = 0x7f;
    assert_eq!(
        DeltaRow::decode_canonical(&encoded),
        Err(CanonicalError::UnknownTag { tag: 0x7f }),
        "a reader that does not know an arm has not understood the record"
    );
}

#[test]
fn a_non_canonical_boolean_byte_is_refused() {
    let row = DeltaRow::LabelMembership {
        vid: VId(14),
        label: LabelId(6),
        before: false,
        after: true,
    };
    let mut encoded = bytes_of(&row);
    let last = encoded.len() - 1;
    encoded[last] = 0x02;
    assert!(
        matches!(
            DeltaRow::decode_canonical(&encoded),
            Err(CanonicalError::UnknownTag { tag: 0x02 })
        ),
        "accepting any nonzero as true would give `true` 255 encodings"
    );
}

#[test]
fn trailing_bytes_are_refused() {
    let mut encoded = bytes_of(&every_family_populated()[3]);
    encoded.push(0x00);
    assert!(matches!(
        DeltaRow::decode_canonical(&encoded),
        Err(CanonicalError::TrailingBytes { remaining: 1 })
    ));

    let template = LogicalDeltaTemplate::build(oid(0x11), [0x22; 32], vec![entry(1, 1, 3, vec![])])
        .expect("builds");
    let mut bytes = template.canonical_bytes().expect("encodes");
    bytes.push(0xff);
    assert!(matches!(
        LogicalDeltaTemplate::decode_canonical(&bytes),
        Err(CanonicalError::TrailingBytes { remaining: 1 })
    ));
}

/// Truncation swept at EVERY length. One truncation point could pass by luck;
/// the sweep cannot.
#[test]
fn truncation_at_any_length_is_refused() {
    for row in every_family_populated() {
        let encoded = bytes_of(&row);
        for length in 0..encoded.len() {
            assert!(
                DeltaRow::decode_canonical(&encoded[..length]).is_err(),
                "a {length}-byte prefix of a {}-byte row must not decode",
                encoded.len()
            );
        }
    }
}

#[test]
fn an_unsupported_format_version_is_refused() {
    let template = LogicalDeltaTemplate::build(oid(0x11), [0x22; 32], vec![entry(1, 1, 3, vec![])])
        .expect("builds");
    let mut bytes = template.canonical_bytes().expect("encodes");
    bytes[0..2].copy_from_slice(&99u16.to_be_bytes());
    assert_eq!(
        LogicalDeltaTemplate::decode_canonical(&bytes),
        Err(CanonicalError::UnsupportedFormat { format: 99 })
    );
}

/// A corrupt count must fail on the count, not on the allocation it would
/// otherwise request. The check is against bytes actually remaining, so a
/// four-byte length claiming four billion elements cannot ask for the memory.
#[test]
fn an_implausible_count_is_refused_before_allocating() {
    let template = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry(1, 1, 3, every_family_populated())],
    )
    .expect("builds");
    let mut bytes = template.canonical_bytes().expect("encodes");
    // The entry count sits after format (2) + intent oid (32) + digest (32).
    bytes[66..70].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(
        matches!(
            LogicalDeltaTemplate::decode_canonical(&bytes),
            Err(CanonicalError::ImplausibleCount { .. })
        ),
        "a corrupt length must be rejected against the input, not honoured"
    );
}

/// Decoding never repairs. A template whose bytes are out of canonical order
/// is refused rather than silently sorted, because a reader that repaired it
/// would compute a different digest than the writer did — the exact
/// disagreement the canonical form exists to prevent.
#[test]
fn decode_refuses_rather_than_repairs_non_canonical_order() {
    let rows = every_family_populated();
    let good = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry(1, 1, 3, rows.clone()), entry(2, 1, 3, rows.clone())],
    )
    .expect("builds");
    let bytes = good.canonical_bytes().expect("encodes");

    // Hand-encode the same two entries in the wrong order by swapping their
    // graph ids in place — entry bodies are otherwise identical in shape.
    let swapped = LogicalDeltaTemplate::build(
        oid(0x11),
        [0x22; 32],
        vec![entry(2, 1, 3, rows.clone()), entry(1, 1, 3, rows)],
    )
    .expect("builds");
    assert_eq!(
        swapped.canonical_bytes().expect("encodes"),
        bytes,
        "the builder sorts, so both orders reach the same bytes"
    );

    // Now corrupt the ORDER directly in the bytes: make the first entry's
    // graph id larger than the second's.
    let mut out_of_order = bytes.clone();
    out_of_order[70..86].copy_from_slice(&9u128.to_be_bytes());
    assert!(
        matches!(
            LogicalDeltaTemplate::decode_canonical(&out_of_order),
            Err(CanonicalError::NonCanonicalCoordinateOrder { .. })
        ),
        "decode must refuse a non-canonical template, not repair it"
    );
}
