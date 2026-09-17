use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_strata::coalesced::{
    COALESCED_ENTRY_FRAME_LEN, COALESCED_HEADER_LEN, COALESCED_MAGIC, CoalescedBlock,
    CoalescedError, MAX_COALESCED_BYTES, MAX_COALESCED_MEMBER_BYTES, decode_coalesced,
    encode_coalesced,
};
use fgdb_strata::edge_props::{
    EdgePropertyPatchError, decode_property_patch, encode_property_patch, property_patch_id,
};
use fgdb_strata::{
    AdjacencyEntry, BlockError, DeltaBlockVersion, block_id, decode_block,
    decode_block_with_properties, encode_block, encode_block_with_properties,
};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};

const KEY: [u8; 32] = [0x57; 32];
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x93; 32]);

fn entry(src: u128, dst: u128, relation: u64, created: u64) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(src),
        relation: RelationId(relation),
        dst: VId(dst),
        eid: EId(src * 100 + dst),
        created_at: CommitSeq(created),
        retired_at: Some(CommitSeq(29)),
    }
}

#[test]
fn two_families_and_properties_round_trip_without_changing_v7_members() {
    let plain_entries = [entry(3, 7, 11, 5)];
    let plain = encode_block(41, None, &plain_entries).unwrap();
    let entries = [entry(13, 17, 19, 2), entry(13, 23, 19, 3)];
    let rows = vec![vec![(PropertyKeyId(31), CanonicalScalar::Int(-37))]];
    let patch = encode_property_patch(&rows).unwrap();
    let patch_id = property_patch_id(&KEY, NS, &patch);
    let prior = encode_block(41, None, &[entry(13, 47, 19, 1)]).unwrap();
    let predecessor = Some(DeltaBlockVersion(block_id(&KEY, NS, &prior)));
    let propertied =
        encode_block_with_properties(41, predecessor, &entries, patch_id, &[0, 1], &rows).unwrap();
    let members = [
        CoalescedBlock {
            block_bytes: &plain,
            property_patch: None,
        },
        CoalescedBlock {
            block_bytes: &propertied,
            property_patch: Some(&patch),
        },
    ];
    let encoded = encode_coalesced(&KEY, NS, &members).unwrap();
    let decoded = decode_coalesced(&KEY, NS, &encoded).unwrap();
    assert_eq!(decoded, members);
    assert_eq!(encode_coalesced(&KEY, NS, &decoded).unwrap(), encoded);
    assert_eq!(decode_block(decoded[0].block_bytes).unwrap(), plain_entries);
    assert_eq!(
        decode_block_with_properties(decoded[1].block_bytes).unwrap(),
        (entries.to_vec(), Some((patch_id, vec![0, 1])))
    );
    assert_eq!(
        decode_property_patch(decoded[1].property_patch.unwrap()).unwrap(),
        rows
    );
    assert_eq!(
        block_id(&KEY, NS, decoded[1].block_bytes),
        block_id(&KEY, NS, &propertied)
    );
    assert_eq!(decode_block(&encoded), Err(BlockError::NotABlock));
    // The borrowed payload is a view into the frame, not a re-encoded allocation.
    let start = encoded.as_ptr() as usize;
    let end = start + encoded.len();
    for member in &decoded {
        assert!((start..end).contains(&(member.block_bytes.as_ptr() as usize)));
        if let Some(patch) = member.property_patch {
            assert!((start..end).contains(&(patch.as_ptr() as usize)));
        }
    }
}

// Independent framing permits malformed inputs the encoder rightly refuses.
fn frame(members: &[CoalescedBlock<'_>]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&COALESCED_MAGIC);
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&(members.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    for member in members {
        let patch = member.property_patch.unwrap_or_default();
        bytes.extend_from_slice(&(member.block_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(patch.len() as u32).to_le_bytes());
        bytes.extend_from_slice(member.block_bytes);
        bytes.extend_from_slice(patch);
    }
    set_total(&mut bytes);
    bytes
}

fn set_total(bytes: &mut [u8]) {
    let len = bytes.len() as u64;
    bytes[12..20].copy_from_slice(&len.to_le_bytes());
}

#[test]
fn framing_rejects_every_prefix_even_with_a_repaired_total_length() {
    let block = encode_block(41, None, &[entry(3, 7, 11, 5)]).unwrap();
    let rows = vec![vec![(PropertyKeyId(31), CanonicalScalar::Int(-37))]];
    let patch = encode_property_patch(&rows).unwrap();
    let propertied = encode_block_with_properties(
        41,
        None,
        &[entry(13, 17, 19, 2)],
        property_patch_id(&KEY, NS, &patch),
        &[1],
        &rows,
    )
    .unwrap();
    let encoded = encode_coalesced(
        &KEY,
        NS,
        &[
            CoalescedBlock {
                block_bytes: &block,
                property_patch: None,
            },
            CoalescedBlock {
                block_bytes: &propertied,
                property_patch: Some(&patch),
            },
        ],
    )
    .unwrap();
    for cut in 0..encoded.len() {
        let mut prefix = encoded[..cut].to_vec();
        assert!(decode_coalesced(&KEY, NS, &prefix).is_err(), "prefix {cut}");
        if cut >= COALESCED_HEADER_LEN {
            set_total(&mut prefix);
            assert!(
                decode_coalesced(&KEY, NS, &prefix).is_err(),
                "reframed prefix {cut}"
            );
        }
    }
    let mut appended = encoded.clone();
    appended.push(0);
    assert!(matches!(
        decode_coalesced(&KEY, NS, &appended),
        Err(CoalescedError::LengthMismatch { .. })
    ));
    set_total(&mut appended);
    assert_eq!(
        decode_coalesced(&KEY, NS, &appended),
        Err(CoalescedError::TrailingBytes { extra: 1 })
    );
}

#[test]
fn counts_lengths_versions_and_resource_bounds_fail_closed() {
    let block = encode_block(41, None, &[entry(3, 7, 11, 5)]).unwrap();
    let member = CoalescedBlock {
        block_bytes: &block,
        property_patch: None,
    };
    let encoded = encode_coalesced(&KEY, NS, &[member]).unwrap();
    for count in [0u32, 2, u32::MAX] {
        let mut bad = encoded.clone();
        bad[8..12].copy_from_slice(&count.to_le_bytes());
        assert!(
            decode_coalesced(&KEY, NS, &bad).is_err(),
            "member count {count}"
        );
    }
    assert_eq!(
        encode_coalesced(&KEY, NS, &[]),
        Err(CoalescedError::InvalidMemberCount { count: 0 })
    );
    for offset in [4, 6] {
        let mut bad = encoded.clone();
        bad[offset..offset + 2].copy_from_slice(&99u16.to_le_bytes());
        assert!(matches!(
            decode_coalesced(&KEY, NS, &bad),
            Err(CoalescedError::UnsupportedFormat { .. })
        ));
    }
    let mut bad_magic = encoded.clone();
    bad_magic[0] ^= 0xff;
    assert_eq!(
        decode_coalesced(&KEY, NS, &bad_magic),
        Err(CoalescedError::NotAContainer)
    );
    let mut bad_total = encoded.clone();
    bad_total[12..20].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(matches!(
        decode_coalesced(&KEY, NS, &bad_total),
        Err(CoalescedError::LengthMismatch { .. })
    ));
    for offset in [COALESCED_HEADER_LEN, COALESCED_HEADER_LEN + 4] {
        let mut bad = encoded.clone();
        bad[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode_coalesced(&KEY, NS, &bad),
            Err(CoalescedError::TooLarge { .. })
        ));
    }
    let mut missing_payload = encoded.clone();
    missing_payload[COALESCED_HEADER_LEN..COALESCED_HEADER_LEN + 4]
        .copy_from_slice(&((block.len() + 1) as u32).to_le_bytes());
    assert!(matches!(
        decode_coalesced(&KEY, NS, &missing_payload),
        Err(CoalescedError::Truncated { .. })
    ));
    let mut empty_block = encoded.clone();
    empty_block[COALESCED_HEADER_LEN..COALESCED_HEADER_LEN + 4]
        .copy_from_slice(&0u32.to_le_bytes());
    assert_eq!(
        decode_coalesced(&KEY, NS, &empty_block),
        Err(CoalescedError::EmptyBlock { at: 0 })
    );
    let huge = vec![0; MAX_COALESCED_MEMBER_BYTES + 1];
    assert!(matches!(
        encode_coalesced(
            &KEY,
            NS,
            &[CoalescedBlock {
                block_bytes: &huge,
                property_patch: None
            },]
        ),
        Err(CoalescedError::TooLarge { .. })
    ));
    assert!(matches!(
        encode_coalesced(
            &KEY,
            NS,
            &[CoalescedBlock {
                block_bytes: &block,
                property_patch: Some(&huge)
            },]
        ),
        Err(CoalescedError::TooLarge { .. })
    ));
    assert_eq!(
        decode_coalesced(&KEY, NS, &vec![0; MAX_COALESCED_BYTES + 1]),
        Err(CoalescedError::TooLarge {
            bytes: MAX_COALESCED_BYTES + 1,
            limit: MAX_COALESCED_BYTES
        })
    );
    // Each component fits; their aggregate does not. Bounds run before decoding.
    let component = vec![0; MAX_COALESCED_MEMBER_BYTES];
    let many = [CoalescedBlock {
        block_bytes: &component,
        property_patch: Some(&component),
    }; 5];
    assert!(matches!(
        encode_coalesced(&KEY, NS, &many),
        Err(CoalescedError::TooLarge { .. })
    ));
}

#[test]
fn invalid_inner_blocks_and_mixed_partitions_are_rejected_on_both_faces() {
    let good = encode_block(41, None, &[entry(3, 7, 11, 5)]).unwrap();
    let mut bad = good.clone();
    bad[0] ^= 0xff;
    let member = CoalescedBlock {
        block_bytes: &bad,
        property_patch: None,
    };
    assert_eq!(
        encode_coalesced(&KEY, NS, &[member]),
        Err(CoalescedError::Block {
            at: 0,
            error: BlockError::NotABlock
        })
    );
    assert_eq!(
        decode_coalesced(&KEY, NS, &frame(&[member])),
        Err(CoalescedError::Block {
            at: 0,
            error: BlockError::NotABlock
        })
    );
    let foreign = encode_block(43, None, &[entry(13, 17, 19, 2)]).unwrap();
    let members = [
        CoalescedBlock {
            block_bytes: &good,
            property_patch: None,
        },
        CoalescedBlock {
            block_bytes: &foreign,
            property_patch: None,
        },
    ];
    let expected = CoalescedError::PartitionMismatch {
        at: 1,
        expected: 41,
        found: 43,
    };
    assert_eq!(encode_coalesced(&KEY, NS, &members), Err(expected.clone()));
    assert_eq!(decode_coalesced(&KEY, NS, &frame(&members)), Err(expected));
}

#[test]
fn repeated_family_chunks_keep_publication_order_and_predecessor_bytes() {
    let first = encode_block(41, None, &[entry(3, 7, 11, 5)]).unwrap();
    let predecessor = DeltaBlockVersion(block_id(&KEY, NS, &first));
    let second = encode_block(41, Some(predecessor), &[entry(3, 13, 11, 2)]).unwrap();
    let members = [
        CoalescedBlock {
            block_bytes: &first,
            property_patch: None,
        },
        CoalescedBlock {
            block_bytes: &second,
            property_patch: None,
        },
    ];
    let encoded = encode_coalesced(&KEY, NS, &members).unwrap();
    assert_eq!(decode_coalesced(&KEY, NS, &encoded).unwrap(), members);
    let reversed = encode_coalesced(&KEY, NS, &[members[1], members[0]]).unwrap();
    assert_ne!(encoded, reversed, "codec must not sort publication order");
    assert_eq!(
        decode_coalesced(&KEY, NS, &reversed).unwrap(),
        [members[1], members[0]]
    );
}

#[test]
fn hosted_patch_presence_identity_bijection_and_joint_digest_are_enforced() {
    let entries = [entry(13, 17, 19, 2), entry(13, 23, 19, 3)];
    let rows = vec![vec![(PropertyKeyId(31), CanonicalScalar::Int(-37))]];
    let different_rows = vec![vec![(PropertyKeyId(31), CanonicalScalar::Int(-41))]];
    let patch = encode_property_patch(&rows).unwrap();
    let different_patch = encode_property_patch(&different_rows).unwrap();
    let id = property_patch_id(&KEY, NS, &patch);
    let good = encode_block_with_properties(41, None, &entries, id, &[0, 1], &rows).unwrap();
    let missing = CoalescedBlock {
        block_bytes: &good,
        property_patch: None,
    };
    assert_eq!(
        encode_coalesced(&KEY, NS, &[missing]),
        Err(CoalescedError::MissingPatch { at: 0 })
    );
    assert_eq!(
        decode_coalesced(&KEY, NS, &frame(&[missing])),
        Err(CoalescedError::MissingPatch { at: 0 })
    );
    let plain = encode_block(41, None, &entries).unwrap();
    let extra = CoalescedBlock {
        block_bytes: &plain,
        property_patch: Some(&patch),
    };
    assert_eq!(
        encode_coalesced(&KEY, NS, &[extra]),
        Err(CoalescedError::UnexpectedPatch { at: 0 })
    );
    assert_eq!(
        decode_coalesced(&KEY, NS, &frame(&[extra])),
        Err(CoalescedError::UnexpectedPatch { at: 0 })
    );
    assert_eq!(
        encode_coalesced(
            &KEY,
            NS,
            &[CoalescedBlock {
                block_bytes: &good,
                property_patch: Some(&[])
            },]
        ),
        Err(CoalescedError::EmptyPatch { at: 0 })
    );
    let substituted = CoalescedBlock {
        block_bytes: &good,
        property_patch: Some(&different_patch),
    };
    assert!(matches!(
        encode_coalesced(&KEY, NS, &[substituted]),
        Err(CoalescedError::Patch {
            error: EdgePropertyPatchError::IdentityMismatch { .. },
            ..
        })
    ));
    assert!(matches!(
        decode_coalesced(&KEY, NS, &frame(&[substituted])),
        Err(CoalescedError::Patch {
            error: EdgePropertyPatchError::IdentityMismatch { .. },
            ..
        })
    ));
    // Rebinding the patch identity is insufficient: the block still digests the old row.
    let rebound = encode_block_with_properties(
        41,
        None,
        &entries,
        property_patch_id(&KEY, NS, &different_patch),
        &[0, 1],
        &rows,
    )
    .unwrap();
    let bad_joint = CoalescedBlock {
        block_bytes: &rebound,
        property_patch: Some(&different_patch),
    };
    assert!(matches!(
        encode_coalesced(&KEY, NS, &[bad_joint]),
        Err(CoalescedError::Block {
            error: BlockError::LogicalDigestMismatch { .. },
            ..
        })
    ));
    assert!(matches!(
        decode_coalesced(&KEY, NS, &frame(&[bad_joint])),
        Err(CoalescedError::Block {
            error: BlockError::LogicalDigestMismatch { .. },
            ..
        })
    ));
    let two_rows = vec![rows[0].clone(), different_rows[0].clone()];
    let two_locators =
        encode_block_with_properties(41, None, &entries, id, &[1, 2], &two_rows).unwrap();
    let bad_bijection = CoalescedBlock {
        block_bytes: &two_locators,
        property_patch: Some(&patch),
    };
    assert!(matches!(
        encode_coalesced(&KEY, NS, &[bad_bijection]),
        Err(CoalescedError::Patch {
            error: EdgePropertyPatchError::UnreferencedRows { .. },
            ..
        })
    ));
    assert!(matches!(
        decode_coalesced(&KEY, NS, &frame(&[bad_bijection])),
        Err(CoalescedError::Patch {
            error: EdgePropertyPatchError::UnreferencedRows { .. },
            ..
        })
    ));
    let encoded = encode_coalesced(
        &KEY,
        NS,
        &[CoalescedBlock {
            block_bytes: &good,
            property_patch: Some(&patch),
        }],
    )
    .unwrap();
    let other_namespace = DatabaseSecurityNamespaceId([0x94; 32]);
    assert!(matches!(
        decode_coalesced(&KEY, other_namespace, &encoded),
        Err(CoalescedError::Patch {
            error: EdgePropertyPatchError::IdentityMismatch { .. },
            ..
        })
    ));
    assert!(matches!(
        decode_coalesced(&[0x58; 32], NS, &encoded),
        Err(CoalescedError::Patch {
            error: EdgePropertyPatchError::IdentityMismatch { .. },
            ..
        })
    ));
    // A damaged scalar patch whose new identity is correctly referenced must
    // still pass the patch decoder, not just an identity comparison.
    let malformed_patch = b"bad patch";
    let malformed_block = encode_block_with_properties(
        41,
        None,
        &entries,
        property_patch_id(&KEY, NS, malformed_patch),
        &[0, 1],
        &rows,
    )
    .unwrap();
    let malformed = CoalescedBlock {
        block_bytes: &malformed_block,
        property_patch: Some(malformed_patch),
    };
    assert!(matches!(
        encode_coalesced(&KEY, NS, &[malformed]),
        Err(CoalescedError::Patch {
            error: EdgePropertyPatchError::NotAPropertyPatch,
            ..
        })
    ));
    assert!(matches!(
        decode_coalesced(&KEY, NS, &frame(&[malformed])),
        Err(CoalescedError::Patch {
            error: EdgePropertyPatchError::NotAPropertyPatch,
            ..
        })
    ));
    // Corruption of the hosted bytes in an otherwise lawful envelope is refused.
    let mut mutated = encoded;
    let patch_offset = COALESCED_HEADER_LEN + COALESCED_ENTRY_FRAME_LEN + good.len();
    mutated[patch_offset] ^= 0xff;
    assert!(matches!(
        decode_coalesced(&KEY, NS, &mutated),
        Err(CoalescedError::Patch {
            error: EdgePropertyPatchError::IdentityMismatch { .. },
            ..
        })
    ));
    let mut bad_locator = good.clone();
    *bad_locator.last_mut().unwrap() = 2;
    let invalid = CoalescedBlock {
        block_bytes: &bad_locator,
        property_patch: Some(&patch),
    };
    assert_eq!(
        encode_coalesced(&KEY, NS, &[invalid]),
        Err(CoalescedError::Block {
            at: 0,
            error: BlockError::NonCanonicalLocators { at: 1 }
        })
    );
    assert_eq!(
        decode_coalesced(&KEY, NS, &frame(&[invalid])),
        Err(CoalescedError::Block {
            at: 0,
            error: BlockError::NonCanonicalLocators { at: 1 }
        })
    );
}
