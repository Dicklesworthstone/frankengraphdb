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
