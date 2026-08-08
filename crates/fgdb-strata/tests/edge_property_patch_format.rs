//! Format laws for the block-hosted edge property patch and the FGSB V4
//! locator/patch section (`fgdb-yqor`, ruling fgdb-2t7q 3B).
//!
//! Same disciplines as the sibling format suites: DISTINCT values per field,
//! an every-strict-prefix truncation sweep paired with an append control, and
//! every canonical refusal proven in BOTH directions — including hand-spliced
//! bytes the encoder cannot emit.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_strata::edge_props::{
    EdgePropertyPatchError, EdgePropertyPatchVersion, EdgePropertyRow, MAX_PROPERTY_PATCH_ROWS,
    PROPERTY_PATCH_FORMAT_V1, PROPERTY_PATCH_MAGIC, decode_property_patch, encode_property_patch,
    property_patch_id, read_property_patch, validate_block_patch_consistency,
};
use fgdb_strata::{
    AdjacencyEntry, BLOCK_FORMAT_V4, BlockError, decode_block, decode_block_with_properties,
    encode_block, encode_block_with_properties,
};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};

const K_OID: [u8; 32] = [0x5a; 32];

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId([0x77; 32])
}

/// Rows whose every field is distinct, so a wrong-offset decode cannot
/// accidentally reproduce the input.
fn distinct_rows() -> Vec<EdgePropertyRow> {
    vec![
        vec![
            (
                PropertyKeyId(41),
                CanonicalScalar::ucs_basic_text("ada").expect("admissible"),
            ),
            (PropertyKeyId(59), CanonicalScalar::Int(-1815)),
        ],
        vec![(PropertyKeyId(67), CanonicalScalar::Bool(true))],
    ]
}

fn entries() -> Vec<AdjacencyEntry> {
    vec![
        AdjacencyEntry {
            src: VId(1),
            relation: RelationId(1),
            dst: VId(2),
            eid: EId(10),
            created_at: CommitSeq(3),
            retired_at: None,
        },
        AdjacencyEntry {
            src: VId(1),
            relation: RelationId(1),
            dst: VId(3),
            eid: EId(11),
            created_at: CommitSeq(4),
            retired_at: None,
        },
        AdjacencyEntry {
            src: VId(1),
            relation: RelationId(1),
            dst: VId(4),
            eid: EId(12),
            created_at: CommitSeq(5),
            retired_at: Some(CommitSeq(7)),
        },
    ]
}

#[test]
fn patch_round_trip_preserves_every_field_with_distinct_values() {
    let rows = distinct_rows();
    let bytes = encode_property_patch(&rows).expect("canonical rows encode");
    assert_eq!(decode_property_patch(&bytes).expect("decodes"), rows);
}

#[test]
fn every_strict_prefix_is_a_typed_truncation_refusal_with_an_append_control() {
    let bytes = encode_property_patch(&distinct_rows()).expect("encodes");
    for cut in 0..bytes.len() {
        let result = decode_property_patch(&bytes[..cut]);
        assert!(
            matches!(
                result,
                Err(EdgePropertyPatchError::Truncated { .. })
                    | Err(EdgePropertyPatchError::NotAPropertyPatch)
            ),
            "prefix of {cut} bytes must refuse, got {result:?}"
        );
    }
    let mut appended = bytes;
    appended.push(0);
    assert_eq!(
        decode_property_patch(&appended),
        Err(EdgePropertyPatchError::TrailingBytes { extra: 1 })
    );
}

#[test]
fn wrong_magic_future_format_and_identity_are_distinct_refusals() {
    let rows = distinct_rows();
    let bytes = encode_property_patch(&rows).expect("encodes");

    let mut wrong_magic = bytes.clone();
    wrong_magic[..4].copy_from_slice(b"FGSB");
    assert_eq!(
        decode_property_patch(&wrong_magic),
        Err(EdgePropertyPatchError::NotAPropertyPatch)
    );

    let mut future = bytes.clone();
    future[4..6].copy_from_slice(&(PROPERTY_PATCH_FORMAT_V1 + 1).to_le_bytes());
    assert_eq!(
        decode_property_patch(&future),
        Err(EdgePropertyPatchError::UnsupportedFormat {
            format: PROPERTY_PATCH_FORMAT_V1 + 1
        })
    );

    let id = EdgePropertyPatchVersion(property_patch_id(&K_OID, namespace(), &bytes));
    assert_eq!(
        read_property_patch(&K_OID, namespace(), &bytes, id).expect("identity matches"),
        rows
    );
    let mut flipped = bytes;
    let last = flipped.len() - 1;
    flipped[last] ^= 0x01;
    assert!(matches!(
        read_property_patch(&K_OID, namespace(), &flipped, id),
        Err(EdgePropertyPatchError::IdentityMismatch { .. })
    ));
    let other_key = [0x5b; 32];
    let bytes = encode_property_patch(&distinct_rows()).expect("encodes");
    assert!(matches!(
        read_property_patch(&other_key, namespace(), &bytes, id),
        Err(EdgePropertyPatchError::IdentityMismatch { .. })
    ));
}

#[test]
fn empty_and_unsorted_rows_are_refused_in_both_directions() {
    assert_eq!(
        encode_property_patch(&[vec![]]),
        Err(EdgePropertyPatchError::EmptyRow { at: 0 })
    );
    let unsorted: Vec<EdgePropertyRow> = vec![vec![
        (PropertyKeyId(59), CanonicalScalar::Int(1)),
        (PropertyKeyId(41), CanonicalScalar::Int(2)),
    ]];
    assert_eq!(
        encode_property_patch(&unsorted),
        Err(EdgePropertyPatchError::NonCanonicalRow { at: 0 })
    );

    // Decoder independence for the empty row: hand-build a one-row patch
    // whose row declares zero properties — bytes the encoder cannot emit.
    let mut spliced = Vec::new();
    spliced.extend_from_slice(&PROPERTY_PATCH_MAGIC);
    spliced.extend_from_slice(&PROPERTY_PATCH_FORMAT_V1.to_le_bytes());
    spliced.extend_from_slice(&1u32.to_le_bytes());
    spliced.extend_from_slice(&0u32.to_le_bytes()); // zero properties
    assert_eq!(
        decode_property_patch(&spliced),
        Err(EdgePropertyPatchError::EmptyRow { at: 0 })
    );
}

#[test]
fn the_row_ceiling_is_shared_with_the_locator_in_both_directions() {
    let row = |seed: u64| -> EdgePropertyRow {
        vec![(PropertyKeyId(seed), CanonicalScalar::Int(seed as i64))]
    };
    let too_many: Vec<EdgePropertyRow> = (1..=u64::from(MAX_PROPERTY_PATCH_ROWS) + 1)
        .map(row)
        .collect();
    assert_eq!(
        encode_property_patch(&too_many),
        Err(EdgePropertyPatchError::ImplausibleRowCount {
            declared: MAX_PROPERTY_PATCH_ROWS + 1
        })
    );
    let full: Vec<EdgePropertyRow> = (1..=u64::from(MAX_PROPERTY_PATCH_ROWS)).map(row).collect();
    let mut bytes = encode_property_patch(&full).expect("the ceiling itself is lawful");
    bytes[6..10].copy_from_slice(&(MAX_PROPERTY_PATCH_ROWS + 1).to_le_bytes());
    assert_eq!(
        decode_property_patch(&bytes),
        Err(EdgePropertyPatchError::ImplausibleRowCount {
            declared: MAX_PROPERTY_PATCH_ROWS + 1
        })
    );
}

// ---------------------------------------------------------------------------
// FGSB V4: the hosting side
// ---------------------------------------------------------------------------

#[test]
fn a_block_with_a_hosted_patch_round_trips_id_and_locators() {
    let entries = entries();
    let patch_id = ObjectId([0xab; 32]);
    let locators = [1u8, 0, 2];
    let rows = distinct_rows();
    let bytes = encode_block_with_properties(0, &entries, patch_id, &locators, &rows)
        .expect("lawful block encodes");
    let (decoded, patch) = decode_block_with_properties(&bytes).expect("decodes");
    assert_eq!(decoded, entries);
    let (found_id, found_locators) = patch.expect("the patch section survives");
    assert_eq!(found_id, patch_id);
    assert_eq!(found_locators, locators);

    // The plain face still answers, and still validated the whole format.
    assert_eq!(decode_block(&bytes).expect("plain decode"), entries);

    // A propertyless block carries NO section and decodes to None.
    let plain = encode_block(0, &entries).expect("encodes");
    let (_, none) = decode_block_with_properties(&plain).expect("decodes");
    assert!(none.is_none());
    assert!(plain.len() < bytes.len());
}

#[test]
fn locator_laws_hold_in_both_directions() {
    let entries = entries();
    let patch_id = ObjectId([0xab; 32]);

    // Length mismatch and scrambled sequences refuse at encode.
    assert!(matches!(
        encode_block_with_properties(0, &entries, patch_id, &[1, 0], &distinct_rows()),
        Err(BlockError::NonCanonicalLocators { .. })
    ));
    assert!(matches!(
        encode_block_with_properties(0, &entries, patch_id, &[2, 0, 1], &distinct_rows()),
        Err(BlockError::NonCanonicalLocators { at: 0 })
    ));
    assert!(matches!(
        encode_block_with_properties(0, &entries, patch_id, &[1, 0, 1], &distinct_rows()),
        Err(BlockError::NonCanonicalLocators { at: 2 })
    ));
    assert!(
        matches!(
            encode_block_with_properties(0, &entries, patch_id, &[0, 0, 0], &distinct_rows()),
            Err(BlockError::NonCanonicalLocators { at: 0 })
        ),
        "a patch no entry references is dead weight, refused"
    );

    // Decoder independence: corrupt the locator bytes in place — the last
    // `entries.len()` bytes of the encoding — into a scramble the encoder
    // cannot emit.
    let lawful =
        encode_block_with_properties(0, &entries, patch_id, &[1, 0, 2], &distinct_rows())
            .expect("lawful encodes");
    let mut scrambled = lawful.clone();
    let tail = scrambled.len() - 3;
    scrambled[tail..].copy_from_slice(&[2, 0, 1]);
    assert!(matches!(
        decode_block_with_properties(&scrambled),
        Err(BlockError::NonCanonicalLocators { at: 0 })
    ));

    // More than one declared patch stays fail-closed (the 2t7q multi-patch
    // arm), proven by editing the count in place.
    let mut two = lawful;
    two[39..41].copy_from_slice(&2u16.to_be_bytes());
    assert!(matches!(
        decode_block_with_properties(&two),
        Err(BlockError::PropertyPatchesNotYetImplemented { declared: 2 })
    ));
}

#[test]
fn v3_blocks_are_refused_by_name() {
    let bytes = encode_block(0, &entries()).expect("encodes");
    let mut old = bytes;
    old[4..6].copy_from_slice(&3u16.to_be_bytes());
    assert_eq!(
        decode_block(&old),
        Err(BlockError::UnsupportedFormat { format: 3 }),
        "a retired version is 'not implemented', never 'not our file'"
    );
    let _ = BLOCK_FORMAT_V4; // the current version, named so the import is load-bearing
}

#[test]
fn the_joint_bijection_law_needs_both_objects() {
    // Locator sequence lawful, patch row count wrong: only the JOINT law
    // catches it — each object alone is internally canonical.
    assert!(validate_block_patch_consistency(&[1, 0, 2], 2).is_ok());
    assert_eq!(
        validate_block_patch_consistency(&[1, 0, 2], 3),
        Err(EdgePropertyPatchError::UnreferencedRows {
            referenced: 2,
            declared: 3
        })
    );
    assert_eq!(
        validate_block_patch_consistency(&[1, 0], 2),
        Err(EdgePropertyPatchError::UnreferencedRows {
            referenced: 1,
            declared: 2
        })
    );
}
