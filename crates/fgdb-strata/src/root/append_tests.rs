//! An authenticated-prefix append must produce exactly the same V5 root and
//! named objects as full validation/encoding, including both segment levels.

use super::*;
use std::collections::BTreeMap;

const KEY: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn reference_id(family: u8, at: usize) -> ObjectId {
    let mut bytes = [family; 32];
    bytes[..8].copy_from_slice(&(at as u64).to_be_bytes());
    ObjectId(bytes)
}

fn root(blocks: usize, patches: usize) -> PartitionRoot {
    PartitionRoot {
        graph: GraphId(u128::MAX - 1),
        branch: BranchId(u128::MAX - 2),
        partition: u64::MAX,
        published_at: CommitSeq(blocks.max(patches) as u64 + 1),
        blocks: (0..blocks)
            .map(|at| BlockRef {
                block_id: reference_id(1, at),
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(at as u64 + 1),
            })
            .collect(),
        vertex_patches: (0..patches)
            .map(|at| PatchRef {
                patch_id: reference_id(2, at),
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(at as u64 + 1),
            })
            .collect(),
    }
}

#[test]
fn append_encoding_matches_cold_v5_at_both_segment_boundaries() {
    let leaf = SEGMENT_REFS;
    let index = INDEX_COVERS;
    for (before_blocks, before_patches, after_blocks, after_patches) in [
        (0, 0, 0, 0),
        (0, 0, 1, 1),
        (leaf - 1, 0, leaf, 0),
        (leaf, leaf - 1, leaf + 1, leaf),
        (index - 1, index - 1, index, index),
        (index, index, index + 1, index + 3),
        (index + 1, leaf * 2 - 1, index * 2 + leaf + 1, leaf * 2 + 1),
    ] {
        let prior = root(before_blocks, before_patches);
        let next = root(after_blocks, after_patches);
        let mut cache = SegmentCache::default();
        let base = encode_root_v5(&prior, &KEY, NAMESPACE, &mut cache).unwrap();
        let append = encode_root_v5_append(
            (&next).into(),
            (before_blocks, before_patches),
            &KEY,
            NAMESPACE,
            &mut cache,
        )
        .unwrap();
        let cold = encode_root_v5(&next, &KEY, NAMESPACE, &mut SegmentCache::default()).unwrap();
        assert_eq!(
            append.bytes, cold.bytes,
            "{before_blocks}/{before_patches} -> {after_blocks}/{after_patches}"
        );
        assert_eq!(
            base.new_segments
                .into_iter()
                .chain(append.new_segments)
                .collect::<BTreeMap<_, _>>(),
            cold.new_segments.into_iter().collect::<BTreeMap<_, _>>(),
            "carried and appended leaf objects are the cold encoder's exact objects"
        );
        assert_eq!(
            base.new_index_segments
                .into_iter()
                .chain(append.new_index_segments)
                .collect::<BTreeMap<_, _>>(),
            cold.new_index_segments
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            "carried and appended index objects are the cold encoder's exact objects"
        );
    }
}

#[test]
fn append_checks_each_structural_law_at_the_carried_boundary() {
    let mut lawful = root(2, 2);
    lawful.published_at = CommitSeq(8);
    lawful.blocks[0].last_seq = CommitSeq(4);
    lawful.blocks[1].last_seq = CommitSeq(6);
    lawful.vertex_patches[0].last_seq = CommitSeq(4);
    lawful.vertex_patches[1].last_seq = CommitSeq(6);
    let cases: [fn(&mut PartitionRoot); 8] = [
        |root| root.blocks[1].last_seq = CommitSeq(3),
        |root| root.blocks[1].first_seq = CommitSeq(0),
        |root| root.blocks[1].first_seq = CommitSeq(7),
        |root| root.blocks[1].last_seq = CommitSeq(9),
        |root| root.vertex_patches[1].last_seq = CommitSeq(3),
        |root| root.vertex_patches[1].first_seq = CommitSeq(0),
        |root| root.vertex_patches[1].first_seq = CommitSeq(7),
        |root| root.vertex_patches[1].last_seq = CommitSeq(9),
    ];
    for corrupt in cases {
        let mut candidate = lawful.clone();
        corrupt(&mut candidate);
        let expected = validate_root(&candidate).expect_err("the full path refuses");
        let mut cache = SegmentCache::default();
        let found = encode_root_v5_append((&candidate).into(), (1, 1), &KEY, NAMESPACE, &mut cache)
            .expect_err("the append path refuses");
        assert_eq!(found, expected, "same refusal and global reference ordinal");
    }
}

fn descriptors(root: &PartitionRoot) -> descriptor::RootDescriptors {
    use descriptor::{BlockDescriptor, PatchDescriptor, RootDescriptors};
    let edge = crate::AdjacencyEntry {
        eid: EId(u128::MAX),
        src: VId(u128::MAX - 1),
        relation: fgdb_delta_types::RelationId(u64::MAX),
        dst: VId(u128::MAX - 2),
        created_at: CommitSeq(1),
        retired_at: None,
    };
    let block_bytes = crate::encode_block(root.partition, None, &[edge]).unwrap();
    let vertex = crate::vertex::VertexRow {
        vid: VId(u128::MAX),
        birth_ordinal: u64::MAX,
        created_at: CommitSeq(1),
        retired_at: None,
        labels: vec![],
        props: vec![],
    };
    let patch_bytes = crate::vertex::encode_patch(std::slice::from_ref(&vertex)).unwrap();
    RootDescriptors {
        blocks: root
            .blocks
            .iter()
            .enumerate()
            .map(|(at, reference)| {
                // The codec law exercises a hosted-property reference as well as
                // its canonical absence. This helper is not a publication proof.
                BlockDescriptor::admitted(
                    reference.block_id,
                    &block_bytes,
                    &[edge],
                    (at % 2 == 0).then_some((reference_id(3, at), b"hosted".as_slice())),
                )
                .unwrap()
            })
            .collect(),
        patches: root
            .vertex_patches
            .iter()
            .map(|reference| {
                PatchDescriptor::admitted(
                    reference.patch_id,
                    &patch_bytes,
                    std::slice::from_ref(&vertex),
                )
                .unwrap()
            })
            .collect(),
    }
}

#[test]
fn described_append_matches_cold_v6_and_roundtrips_every_metadata_family() {
    for (before, after) in [
        (0, 0),
        (0, 1),
        (255, 256),
        (256, 257),
        (4095, 4096),
        (4096, 4097),
    ] {
        let prior = root(before, before);
        let next = root(after, after);
        let prior_descriptors = descriptors(&prior);
        let next_descriptors = descriptors(&next);
        let mut cache = SegmentCache::default();
        let base = encode_root_v6_append(
            (&prior).into(),
            (0, 0),
            &prior_descriptors,
            (&KEY, NAMESPACE),
            &mut cache,
        )
        .unwrap();
        let append = encode_root_v6_append(
            (&next).into(),
            (before, before),
            &next_descriptors,
            (&KEY, NAMESPACE),
            &mut cache,
        )
        .unwrap();
        let cold = encode_root_v6_append(
            (&next).into(),
            (0, 0),
            &next_descriptors,
            (&KEY, NAMESPACE),
            &mut SegmentCache::default(),
        )
        .unwrap();
        assert_eq!(append.bytes, cold.bytes, "{before} -> {after}");
        assert_eq!(
            base.new_segments
                .into_iter()
                .chain(append.new_segments)
                .collect::<BTreeMap<_, _>>(),
            cold.new_segments
                .iter()
                .cloned()
                .collect::<BTreeMap<_, _>>()
        );
        assert_eq!(
            base.new_index_segments
                .into_iter()
                .chain(append.new_index_segments)
                .collect::<BTreeMap<_, _>>(),
            cold.new_index_segments
                .into_iter()
                .collect::<BTreeMap<_, _>>()
        );
        let RootFrame::V6(frame) = decode_root_frame(&cold.bytes).unwrap() else {
            panic!("described root version")
        };
        let tail = after / SEGMENT_REFS * SEGMENT_REFS;
        assert_eq!(frame.descriptors.blocks, next_descriptors.blocks[tail..]);
        assert_eq!(frame.descriptors.patches, next_descriptors.patches[tail..]);
        assert_eq!(
            frame.membership.coordinate(),
            (next.graph, next.branch, next.partition)
        );
        for (_, bytes) in cold.new_segments {
            let (segment, _, recovered) =
                crate::root_segment::decode_described_segment(&bytes).unwrap();
            let first =
                u64::from_be_bytes(segment.entries[0].id.0[..8].try_into().unwrap()) as usize;
            match segment.class {
                SegmentClass::Blocks => {
                    assert_eq!(bytes.len(), crate::root_segment::MAX_SEGMENT_BYTES);
                    assert_eq!(
                        recovered.blocks,
                        next_descriptors.blocks[first..first + SEGMENT_REFS]
                    );
                    assert!(recovered.patches.is_empty());
                }
                SegmentClass::VertexPatches => {
                    assert_eq!(
                        recovered.patches,
                        next_descriptors.patches[first..first + SEGMENT_REFS]
                    );
                    assert!(recovered.blocks.is_empty());
                }
            }
        }
    }
}

#[test]
fn described_metadata_refuses_damage_noncanonical_fields_and_wrong_membership() {
    let membership = root(SEGMENT_REFS + 1, 1);
    let descriptions = descriptors(&membership);
    let encoded = encode_root_v6_append(
        (&membership).into(),
        (0, 0),
        &descriptions,
        (&KEY, NAMESPACE),
        &mut SegmentCache::default(),
    )
    .unwrap();
    for position in [OFF_PUBLISHED, encoded.bytes.len() - 1] {
        let mut damaged = encoded.bytes.clone();
        damaged[position] ^= 1;
        assert!(decode_root_frame(&damaged).is_err());
    }
    let mut trailing = encoded.bytes.clone();
    trailing.push(0);
    assert!(decode_root_frame(&trailing).is_err());
    assert!(decode_root_frame(&encoded.bytes[..encoded.bytes.len() - 1]).is_err());
    let mut leaf = encoded.new_segments[0].1.clone();
    *leaf.last_mut().unwrap() ^= 1;
    assert!(crate::root_segment::decode_described_segment(&leaf).is_err());

    let mut wrong = descriptions.clone();
    wrong.blocks.swap(0, 1);
    assert!(matches!(
        encode_root_v6_append(
            (&membership).into(),
            (0, 0),
            &wrong,
            (&KEY, NAMESPACE),
            &mut SegmentCache::default()
        ),
        Err(RootError::InvalidDescriptor)
    ));
    let reference = [SegmentEntry::from(membership.blocks[1])];
    let mut bytes = Vec::new();
    descriptions
        .encode_range(SegmentClass::Blocks, 1..2, &mut bytes)
        .unwrap();
    assert_eq!(bytes.len(), descriptor::BLOCK_BYTES);
    // These checks invoke the scalar decoder directly, past the outer
    // digest guard, so recomputing a digest cannot legalize malformed fields.
    for corrupt in [
        |bytes: &mut Vec<u8>| bytes[..4].fill(0),
        |bytes: &mut Vec<u8>| bytes[36..40].fill(0),
        |bytes: &mut Vec<u8>| bytes[64..80].fill(0),
        |bytes: &mut Vec<u8>| bytes[136] = 2,
        |bytes: &mut Vec<u8>| bytes[137] = 1,
    ] {
        let mut malformed = bytes.clone();
        corrupt(&mut malformed);
        assert!(descriptor::decode(SegmentClass::Blocks, &reference, &malformed).is_err());
    }
}

#[test]
fn upgrading_a_legacy_segment_cache_reseals_described_leaves() {
    let root = root(SEGMENT_REFS, SEGMENT_REFS);
    let descriptions = descriptors(&root);
    let mut cache = SegmentCache::default();
    let legacy = encode_root_v5(&root, &KEY, NAMESPACE, &mut cache).unwrap();
    let upgraded = encode_root_v6_append(
        (&root).into(),
        (0, 0),
        &descriptions,
        (&KEY, NAMESPACE),
        &mut cache,
    )
    .unwrap();
    let cold = encode_root_v6_append(
        (&root).into(),
        (0, 0),
        &descriptions,
        (&KEY, NAMESPACE),
        &mut SegmentCache::default(),
    )
    .unwrap();
    assert_eq!(upgraded.bytes, cold.bytes);
    assert_eq!(upgraded.new_segments, cold.new_segments);
    assert_eq!(upgraded.new_segments.len(), 2);
    assert_ne!(upgraded.new_segments[0].0, legacy.new_segments[0].0);
    assert!(matches!(
        decode_root_frame(&legacy.bytes),
        Ok(RootFrame::V5(_))
    ));
}
