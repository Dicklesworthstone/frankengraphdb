//! Compaction: fewer blocks, same answers.
//!
//! A partition accumulates blocks — one per seal, and the writer is forced to seal
//! whenever a key gets a second version. Reads merge across all of them, so the
//! cost of a read grows with the write history. Compaction is what stops that.
//!
//! **THE FORMAT MAKES THIS HARDER THAN IT LOOKS, AND THAT IS THE INTERESTING
//! PART.** A block requires strictly ascending UNIQUE keys, so two versions of one
//! key cannot share a block. Merging blocks that each hold a version of the same
//! key is therefore not a packing problem — it is impossible without DROPPING one
//! of them. Which means:
//!
//! > **Compaction needs a retention floor precisely because a block cannot hold two
//! > versions of one key.** Without a floor there is nothing that licenses dropping
//! > a version, so a compactor with no floor can only merge blocks whose key sets
//! > are disjoint, which is the case that needed compacting least.
//!
//! That is not a limitation invented here; it falls out of the canonical-block rule
//! chosen in the first slice, and it is why `compact` takes a floor rather than
//! offering a floorless convenience that would quietly be useless.
//!
//! **THE FLOOR IS "NO READER CAN ASK BELOW THIS".** Every version whose life ended
//! at or before the floor is unobservable and may go. A version still live at the
//! floor must stay, however old its creation, because a reader AT the floor needs
//! it — dropping by age rather than by observability is the classic MVCC bug, and
//! it silently empties a graph whose edges were all created long ago.
//!
//! **WHAT IS DELIBERATELY ABSENT: the floor's PROVENANCE.** Deciding that no reader
//! can ask below a sequence is a snapshot-tracking question owned by the
//! transaction layer, and inventing a rule here would make that decision silently
//! and in the wrong place — the same reason the writer refuses to own sealing
//! policy. `compact` takes the floor as an argument and refuses to guess.

use crate::AdjacencyEntry;
use fgdb_types::CommitSeq;
use std::collections::BTreeMap;

/// The result of compacting a partition's blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compaction {
    /// The compacted blocks, ordered by nondecreasing upper sequence frontier —
    /// fewer than went in, unless nothing could be dropped.
    pub blocks: Vec<Vec<AdjacencyEntry>>,
    /// How many entries were dropped as unobservable at or below the floor.
    ///
    /// **ONLY FLOOR DROPS**, and the distinction was a defect before a law caught
    /// it. The first version computed `seen - retained`, which also counted
    /// entries collapsed by SUPERSEDE — a version restated across two blocks, where
    /// nothing was retired and nothing became unobservable. A caller auditing
    /// retention with that number would be told the floor had reclaimed something
    /// it had not touched.
    pub dropped: usize,
    /// How many entries were collapsed because a later block restated the same
    /// version — the cross-block retirement case.
    ///
    /// Reported rather than folded into `dropped` because the two answer different
    /// questions: this one is "how much did consolidation save", `dropped` is "how
    /// much did the retention floor reclaim", and only the second is a statement
    /// about what a reader can no longer see.
    pub superseded: usize,
}

/// Compact `blocks` under a retention floor.
///
/// Returns blocks that answer IDENTICALLY to the input for every sequence at or
/// above `floor`, using as few blocks as the format allows.
///
/// Blocks are consumed in order and later versions of a key supersede earlier ones
/// exactly as [`crate::root::merge_neighbours`] does — compaction must not invent a
/// second precedence rule, or a compacted partition could answer differently from
/// the one it replaced.
pub fn compact(blocks: &[Vec<AdjacencyEntry>], floor: CommitSeq) -> Compaction {
    compact_with_limit(
        blocks,
        floor,
        usize::try_from(crate::MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX),
    )
}

fn compact_with_limit(
    blocks: &[Vec<AdjacencyEntry>],
    floor: CommitSeq,
    max_entries: usize,
) -> Compaction {
    // Collapse to surviving VERSIONS first, keyed the same way the merge is:
    // (key, created_at) identifies a version, and the last block wins for one.
    let mut versions: BTreeMap<
        (
            (
                fgdb_types::VId,
                fgdb_delta_types::RelationId,
                fgdb_types::VId,
            ),
            u64,
        ),
        AdjacencyEntry,
    > = BTreeMap::new();
    let mut seen = 0usize;
    for block in blocks {
        for entry in block {
            seen += 1;
            versions.insert(
                ((entry.src, entry.relation, entry.dst), entry.created_at.0),
                *entry,
            );
        }
    }

    // DROP BY OBSERVABILITY, NOT BY AGE. A version whose life ended at or before
    // the floor can never be seen again; one still live at the floor must stay
    // however old its creation is.
    let superseded = seen - versions.len();
    let before_floor = versions.len();
    let retained: Vec<AdjacencyEntry> = versions
        .into_values()
        .filter(|entry| entry.retired_at.is_none_or(|r| r.0 > floor.0))
        .collect();
    let dropped = before_floor - retained.len();

    let packed = pack_retained(retained, max_entries);
    Compaction {
        blocks: packed,
        dropped,
        superseded,
    }
}

fn pack_retained(retained: Vec<AdjacencyEntry>, max_entries: usize) -> Vec<Vec<AdjacencyEntry>> {
    if retained.is_empty() {
        return Vec::new();
    }
    debug_assert!(max_entries > 0, "the durable block capacity is nonzero");

    // There are TWO independent lower bounds on the output block count:
    //
    // 1. every version of one key needs a different block; and
    // 2. no block may exceed the durable format's entry ceiling.
    //
    // Their maximum is achievable. Entries arrive sorted by (key, created_at),
    // so assigning them cyclically gives each key's adjacent versions distinct
    // blocks. The same cycle balances total cardinality to within one entry, so
    // the capacity lower bound is sufficient too. This is therefore the minimum
    // block count allowed by BOTH format laws, not merely by version depth.
    let mut max_versions_for_one_key = 0usize;
    let mut group_start = 0usize;
    while group_start < retained.len() {
        let key = (
            retained[group_start].src,
            retained[group_start].relation,
            retained[group_start].dst,
        );
        let mut group_end = group_start + 1;
        while group_end < retained.len()
            && (
                retained[group_end].src,
                retained[group_end].relation,
                retained[group_end].dst,
            ) == key
        {
            group_end += 1;
        }
        max_versions_for_one_key = max_versions_for_one_key.max(group_end - group_start);
        group_start = group_end;
    }

    let capacity_blocks = retained.len().div_ceil(max_entries);
    let block_count = max_versions_for_one_key.max(capacity_blocks);
    let base_len = retained.len() / block_count;
    let longer_blocks = retained.len() % block_count;
    let mut packed = (0..block_count)
        .map(|index| Vec::with_capacity(base_len + usize::from(index < longer_blocks)))
        .collect::<Vec<Vec<AdjacencyEntry>>>();

    let mut next_block = 0usize;
    for entry in retained {
        packed[next_block].push(entry);
        next_block += 1;
        if next_block == block_count {
            next_block = 0;
        }
    }

    // Each block's entries must be canonically ordered; the pack above preserves
    // key order within a block because it appends in key order.
    for block in &mut packed {
        block.sort_by_key(|e| (e.src, e.relation, e.dst));
    }

    // A root's list is publication order, witnessed by nondecreasing `last_seq`.
    // Cyclic capacity packing does not preserve that order: one key's third
    // version may be older than another key's second. Supersede has already
    // collapsed every duplicate statement of a version above, so reordering these
    // blocks cannot change last-wins precedence. Sort by the truthful span before a
    // root is allowed to name the result; the cyclic block order is the
    // deterministic tie breaker because `sort_by_key` is stable.
    packed.sort_by_key(|block| {
        crate::root::span_of(block)
            .map(|(first_seq, last_seq)| (last_seq, first_seq))
            // Packing creates a block only while inserting its first entry, so the
            // fallback is unreachable. Keeping the ordering total here avoids a
            // production panic if that construction is ever refactored incorrectly.
            .unwrap_or((CommitSeq(0), CommitSeq(0)))
    });
    packed
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::RelationId;
    use fgdb_types::VId;
    use std::collections::BTreeSet;

    fn entry(dst: u128) -> AdjacencyEntry {
        version(dst, 1, None)
    }

    fn version(dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
        AdjacencyEntry {
            src: VId(1),
            relation: RelationId(1),
            dst: VId(dst),
            created_at: CommitSeq(created),
            retired_at: retired.map(CommitSeq),
        }
    }

    fn assert_packing_laws(blocks: &[Vec<AdjacencyEntry>], max_entries: usize) {
        let mut upper_frontiers = Vec::with_capacity(blocks.len());
        for block in blocks {
            assert!(
                block.len() <= max_entries,
                "a compacted block has {} entries, above {max_entries}",
                block.len()
            );
            let keys = block
                .iter()
                .map(|entry| (entry.src, entry.relation, entry.dst))
                .collect::<BTreeSet<_>>();
            assert_eq!(
                keys.len(),
                block.len(),
                "one compacted block contains two versions of a key"
            );
            crate::encode_block(block).expect("every compacted block remains encodable");
            upper_frontiers.push(
                crate::root::span_of(block)
                    .expect("the packer emits no empty blocks")
                    .1,
            );
        }
        assert!(
            upper_frontiers
                .windows(2)
                .all(|pair| pair[0].0 <= pair[1].0),
            "compacted publication frontiers regress: {upper_frontiers:?}"
        );
    }

    #[test]
    fn capacity_is_part_of_the_minimum_block_count() {
        let before = vec![(1..=5).map(entry).collect()];

        let result = compact_with_limit(&before, CommitSeq(1), 2);

        assert_eq!(
            result.blocks.len(),
            3,
            "five entries need three two-entry blocks"
        );
        assert_eq!(result.dropped, 0);
        assert_eq!(result.superseded, 0);
        assert_packing_laws(&result.blocks, 2);
    }

    #[test]
    fn capacity_and_version_depth_share_one_minimal_packing() {
        let before = vec![
            vec![
                version(2, 1, Some(3)),
                version(3, 1, None),
                version(4, 1, None),
            ],
            vec![
                version(2, 4, Some(6)),
                version(5, 4, None),
                version(6, 4, None),
            ],
            vec![version(2, 7, None), version(7, 7, None)],
        ];

        let once = compact_with_limit(&before, CommitSeq(1), 2);

        assert_eq!(
            once.blocks.len(),
            4,
            "max(version depth 3, capacity lower bound 4) is exact"
        );
        assert_packing_laws(&once.blocks, 2);
        for as_of in 1..=9 {
            assert_eq!(
                crate::root::merge_neighbours(&before, VId(1), RelationId(1), CommitSeq(as_of),),
                crate::root::merge_neighbours(
                    &once.blocks,
                    VId(1),
                    RelationId(1),
                    CommitSeq(as_of),
                ),
                "capacity packing changed the answer at sequence {as_of}"
            );
        }

        let twice = compact_with_limit(&once.blocks, CommitSeq(1), 2);
        assert_eq!(
            twice.blocks, once.blocks,
            "capacity-aware compaction is not a fixed point"
        );
        assert_eq!(twice.dropped, 0);
        assert_eq!(twice.superseded, 0);
    }

    #[test]
    fn small_capacity_and_version_products_are_minimal_and_lawful() {
        for a in 0u8..=3 {
            for b in 0u8..=3 {
                for c in 0u8..=3 {
                    for d in 0u8..=3 {
                        let multiplicities = [a, b, c, d];
                        let mut retained = Vec::new();
                        for (key, count) in (1u128..).zip(multiplicities) {
                            for version_index in 0..count {
                                retained.push(version(key, u64::from(version_index) + 1, None));
                            }
                        }
                        if retained.is_empty() {
                            continue;
                        }

                        for max_entries in 1usize..=4 {
                            let expected_blocks = multiplicities
                                .into_iter()
                                .max()
                                .map(usize::from)
                                .unwrap_or(0)
                                .max(retained.len().div_ceil(max_entries));
                            let packed = pack_retained(retained.clone(), max_entries);

                            assert_eq!(
                                packed.len(),
                                expected_blocks,
                                "wrong minimum for {multiplicities:?} at capacity {max_entries}"
                            );
                            assert_packing_laws(&packed, max_entries);
                            assert_eq!(
                                pack_retained(retained.clone(), max_entries),
                                packed,
                                "packing is not deterministic for {multiplicities:?} at capacity \
                                 {max_entries}"
                            );
                        }
                    }
                }
            }
        }
    }
}
