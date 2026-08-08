//! Format laws for the Tier-D vertex row patch (`fgdb-3xoi`).
//!
//! Two decoder offset defects have already shipped in this subsystem because
//! equal field values made a wrong-offset read indistinguishable from a right
//! one, so the round-trip law here uses DISTINCT values in every field, the
//! truncation law walks EVERY strict prefix, and the truncation law is paired
//! with an append control — a marker test cannot see an append.

use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_strata::vertex::{
    MAX_PATCH_ROWS, VERTEX_PATCH_FORMAT_V1, VERTEX_PATCH_MAGIC, VertexPatchError,
    VertexPatchVersion, VertexRow, decode_patch, encode_patch, read_patch, vertex_patch_id,
};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, CommitSeq, VId};

const K_OID: [u8; 32] = [0x5a; 32];

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId([0x77; 32])
}

/// Rows whose every field is distinct from every other field's value, so a
/// wrong-offset decode cannot accidentally reproduce the input.
fn distinct_rows() -> Vec<VertexRow> {
    vec![
        VertexRow {
            vid: VId(101),
            birth_ordinal: 7,
            created_at: CommitSeq(3),
            retired_at: None,
            labels: vec![LabelId(11), LabelId(23)],
            props: vec![
                (
                    PropertyKeyId(41),
                    CanonicalScalar::ucs_basic_text("ada").expect("admissible text"),
                ),
                (PropertyKeyId(59), CanonicalScalar::Int(-1815)),
            ],
        },
        VertexRow {
            vid: VId(202),
            birth_ordinal: 13,
            created_at: CommitSeq(5),
            retired_at: Some(CommitSeq(9)),
            labels: vec![LabelId(31)],
            props: vec![(PropertyKeyId(67), CanonicalScalar::Bool(true))],
        },
        VertexRow {
            vid: VId(303),
            birth_ordinal: 17,
            created_at: CommitSeq(6),
            retired_at: None,
            labels: vec![],
            props: vec![],
        },
    ]
}

#[test]
fn round_trip_preserves_every_field_with_distinct_values() {
    let rows = distinct_rows();
    let bytes = encode_patch(&rows).expect("canonical rows encode");
    assert_eq!(decode_patch(&bytes).expect("bytes decode"), rows);
}

#[test]
fn same_rows_encode_to_the_same_bytes_and_identity() {
    let rows = distinct_rows();
    let first = encode_patch(&rows).expect("encodes");
    let second = encode_patch(&rows).expect("encodes");
    assert_eq!(first, second, "canonical means one byte string per value");
    assert_eq!(
        vertex_patch_id(&K_OID, namespace(), &first),
        vertex_patch_id(&K_OID, namespace(), &second),
    );
}

#[test]
fn every_strict_prefix_is_a_typed_truncation_refusal() {
    let bytes = encode_patch(&distinct_rows()).expect("encodes");
    for cut in 0..bytes.len() {
        let result = decode_patch(&bytes[..cut]);
        assert!(
            matches!(
                result,
                Err(VertexPatchError::Truncated { .. }) | Err(VertexPatchError::NotAVertexPatch)
            ),
            "prefix of {cut} bytes must refuse as truncated, got {result:?}"
        );
    }
}

#[test]
fn an_appended_byte_is_a_trailing_bytes_refusal() {
    // The control that pairs with the truncation sweep: a decoder that only
    // checked "enough bytes" would accept an append.
    let mut bytes = encode_patch(&distinct_rows()).expect("encodes");
    bytes.push(0);
    assert_eq!(
        decode_patch(&bytes),
        Err(VertexPatchError::TrailingBytes { extra: 1 })
    );
}

#[test]
fn wrong_magic_and_future_format_are_distinct_refusals() {
    let bytes = encode_patch(&distinct_rows()).expect("encodes");

    let mut wrong_magic = bytes.clone();
    wrong_magic[..4].copy_from_slice(b"FGSB");
    assert_eq!(
        decode_patch(&wrong_magic),
        Err(VertexPatchError::NotAVertexPatch),
        "a block is not a vertex patch"
    );

    let mut future = bytes;
    future[4..6].copy_from_slice(&(VERTEX_PATCH_FORMAT_V1 + 1).to_le_bytes());
    assert_eq!(
        decode_patch(&future),
        Err(VertexPatchError::UnsupportedFormat {
            format: VERTEX_PATCH_FORMAT_V1 + 1
        }),
        "a newer version of our file is not \"not our file\""
    );
}

#[test]
fn encoder_and_decoder_both_refuse_unsorted_rows() {
    let mut rows = distinct_rows();
    rows.swap(0, 1);
    assert_eq!(
        encode_patch(&rows),
        Err(VertexPatchError::NonCanonicalOrder { at: 1 })
    );

    // Decoder independence: the encoder will never emit descending rows, so
    // hand-build them — encode each row as its own lawful single-row patch,
    // then concatenate the row payloads under one header with the HIGHER vid
    // first.
    let descending_first = encode_patch(&rows[0..1]).expect("row alone encodes"); // VId(202)
    let descending_second = encode_patch(&rows[1..2]).expect("row alone encodes"); // VId(101)
    let mut spliced = Vec::new();
    spliced.extend_from_slice(&VERTEX_PATCH_MAGIC);
    spliced.extend_from_slice(&VERTEX_PATCH_FORMAT_V1.to_le_bytes());
    spliced.extend_from_slice(&2u32.to_le_bytes());
    spliced.extend_from_slice(&descending_first[10..]); // strip 4+2+4 header
    spliced.extend_from_slice(&descending_second[10..]);
    let decoded = decode_patch(&spliced);
    assert!(
        matches!(decoded, Err(VertexPatchError::NonCanonicalOrder { .. })),
        "decoder must independently refuse what the encoder refuses, got {decoded:?}"
    );
}

#[test]
fn canonical_shape_refusals_hold_in_both_directions() {
    let base = distinct_rows();

    let mut zero = base.clone();
    zero[0].created_at = CommitSeq(0);
    assert_eq!(
        encode_patch(&zero),
        Err(VertexPatchError::CreatedAtZero { at: 0 })
    );

    let mut inverted = base.clone();
    inverted[1].retired_at = Some(CommitSeq(5));
    assert_eq!(
        encode_patch(&inverted),
        Err(VertexPatchError::RetiredBeforeCreated {
            at: 1,
            created_at: CommitSeq(5),
            retired_at: CommitSeq(5),
        })
    );

    let mut labels = base.clone();
    labels[0].labels = vec![LabelId(23), LabelId(11)];
    assert_eq!(
        encode_patch(&labels),
        Err(VertexPatchError::NonCanonicalLabels { at: 0 })
    );

    let mut dup_prop = base.clone();
    dup_prop[0].props = vec![
        (PropertyKeyId(41), CanonicalScalar::Int(1)),
        (PropertyKeyId(41), CanonicalScalar::Int(2)),
    ];
    assert_eq!(
        encode_patch(&dup_prop),
        Err(VertexPatchError::NonCanonicalProps { at: 0 })
    );

    // Decoder side for one representative shape: encode a lawful patch, then
    // flip its label order bytes in place (two u64s swap), which preserves
    // length and framing exactly.
    let bytes = encode_patch(&base).expect("encodes");
    let labels_at = 4 + 2 + 4 + 16 + 8 + 8 + 8 + 4;
    let mut swapped = bytes.clone();
    swapped[labels_at..labels_at + 8].copy_from_slice(&23u64.to_le_bytes());
    swapped[labels_at + 8..labels_at + 16].copy_from_slice(&11u64.to_le_bytes());
    assert_eq!(
        decode_patch(&swapped),
        Err(VertexPatchError::NonCanonicalLabels { at: 0 })
    );
}

#[test]
fn the_row_ceiling_is_a_format_constant_in_both_directions() {
    let row = |vid: u128| VertexRow {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        created_at: CommitSeq(1),
        retired_at: None,
        labels: vec![],
        props: vec![],
    };
    let too_many: Vec<_> = (1..=u128::from(MAX_PATCH_ROWS) + 1).map(row).collect();
    assert_eq!(
        encode_patch(&too_many),
        Err(VertexPatchError::ImplausibleRowCount {
            declared: MAX_PATCH_ROWS + 1
        })
    );

    let full: Vec<_> = (1..=u128::from(MAX_PATCH_ROWS)).map(row).collect();
    let mut bytes = encode_patch(&full).expect("the ceiling itself is lawful");
    bytes[6..10].copy_from_slice(&(MAX_PATCH_ROWS + 1).to_le_bytes());
    assert_eq!(
        decode_patch(&bytes),
        Err(VertexPatchError::ImplausibleRowCount {
            declared: MAX_PATCH_ROWS + 1
        })
    );
}

#[test]
fn read_patch_refuses_bytes_that_are_not_the_named_patch() {
    let rows = distinct_rows();
    let bytes = encode_patch(&rows).expect("encodes");
    let id = VertexPatchVersion(vertex_patch_id(&K_OID, namespace(), &bytes));

    assert_eq!(
        read_patch(&K_OID, namespace(), &bytes, id).expect("identity matches"),
        rows
    );

    // A single flipped bit in a property value is well-formed damage: the
    // scalar may still decode, so ONLY the content address catches it.
    let mut flipped = bytes.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 0x01;
    let result = read_patch(&K_OID, namespace(), &flipped, id);
    assert!(
        matches!(result, Err(VertexPatchError::IdentityMismatch { .. })),
        "flipped bytes must fail the identity check, got {result:?}"
    );

    // And the same bytes under different keys are a different identity: the
    // §5.1 keyed-identity property, asserted here so a key mixup cannot read
    // another database's patch.
    let other_key = [0x5b; 32];
    let result = read_patch(&other_key, namespace(), &bytes, id);
    assert!(matches!(
        result,
        Err(VertexPatchError::IdentityMismatch { .. })
    ));
}

#[test]
fn visibility_is_the_shared_half_open_interval_rule() {
    let rows = distinct_rows();
    let retired = &rows[1]; // created 5, retired 9
    assert!(!retired.visible_at(CommitSeq(4)));
    assert!(retired.visible_at(CommitSeq(5)));
    assert!(retired.visible_at(CommitSeq(8)));
    assert!(!retired.visible_at(CommitSeq(9)), "half-open upper bound");
    let live = &rows[0]; // created 3, live
    assert!(live.visible_at(CommitSeq(u64::MAX)));
}
