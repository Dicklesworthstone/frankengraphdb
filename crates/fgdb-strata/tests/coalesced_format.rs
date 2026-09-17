use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_strata::coalesced::{CoalescedBlock, decode_coalesced, encode_coalesced};
use fgdb_strata::edge_props::{decode_property_patch, encode_property_patch, property_patch_id};
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
    let propertied = encode_block_with_properties(
        41, predecessor, &entries, patch_id, &[0, 1], &rows,
    ).unwrap();
    let members = [
        CoalescedBlock { block_bytes: &plain, property_patch: None },
        CoalescedBlock { block_bytes: &propertied, property_patch: Some(&patch) },
    ];
    let encoded = encode_coalesced(&KEY, NS, &members).unwrap();
    let decoded = decode_coalesced(&KEY, NS, &encoded).unwrap();
    assert_eq!(decoded, members);
    assert_eq!(encode_coalesced(&KEY, NS, &decoded).unwrap(), encoded);
    assert_eq!(decode_block(decoded[0].block_bytes).unwrap(), plain_entries);
    assert_eq!(decode_block_with_properties(decoded[1].block_bytes).unwrap(),
        (entries.to_vec(), Some((patch_id, vec![0, 1]))));
    assert_eq!(decode_property_patch(decoded[1].property_patch.unwrap()).unwrap(), rows);
    assert_eq!(block_id(&KEY, NS, decoded[1].block_bytes), block_id(&KEY, NS, &propertied));
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
