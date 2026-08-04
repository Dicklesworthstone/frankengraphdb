//! Compaction: fewer blocks, same answers.
//!
//! A partition accumulates immutable blocks, and later blocks may restate an old
//! edge version to retire it. Reads merge across all of them, so the cost of a
//! read grows with write history. Compaction is what stops that.
//!
//! A block requires strictly ascending UNIQUE `(src, relation, dst, eid)` keys.
//! Parallel EIds are freely packable, while cross-block statements of the SAME
//! immutable EId collapse by last-block-wins supersede. A different topology or
//! `created_at` for that EId is identity reuse and is refused, not packed as a
//! second version. Consolidation is separate from retention: only the supplied
//! floor licenses dropping an interval entirely.
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
use crate::root::{RootError, collapse_edge_history};
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
/// Blocks are consumed in order and later statements of one edge version supersede
/// earlier ones exactly as [`crate::root::merge_neighbours`] does — compaction must
/// not invent a second precedence rule, or a compacted partition could answer
/// differently from the one it replaced.
///
/// Returns a typed [`RootError`] instead of emitting replacement blocks when an
/// input block is malformed, one EId has incompatible births, or a retirement is
/// reversed or retimed.
pub fn compact(blocks: &[Vec<AdjacencyEntry>], floor: CommitSeq) -> Result<Compaction, RootError> {
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
) -> Result<Compaction, RootError> {
    // Use the exact validation and last-block-wins rule the read path uses. A
    // compactor must not turn allocator corruption into a new lawful-looking
    // root merely because the conflicting births occupy different blocks.
    let (entries, superseded) = collapse_edge_history(blocks)?;

    // DROP BY OBSERVABILITY, NOT BY AGE. A version whose life ended at or before
    // the floor can never be seen again; one still live at the floor must stay
    // however old its creation is.
    let before_floor = entries.len();
    let retained: Vec<AdjacencyEntry> = entries
        .into_values()
        .filter(|entry| entry.retired_at.is_none_or(|r| r.0 > floor.0))
        .collect();
    let dropped = before_floor - retained.len();

    let packed = pack_retained(retained, max_entries);
    Ok(Compaction {
        blocks: packed,
        dropped,
        superseded,
    })
}

fn pack_retained(
    mut retained: Vec<AdjacencyEntry>,
    max_entries: usize,
) -> Vec<Vec<AdjacencyEntry>> {
    if retained.is_empty() {
        return Vec::new();
    }
    debug_assert!(max_entries > 0, "the durable block capacity is nonzero");

    // V3's durable unit is per descriptor.  Capacity is therefore a lower
    // bound within each `(src, relation)` family, never permission to merge two
    // descriptors merely because a block has spare rows.
    let mut by_descriptor: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for entry in retained.drain(..) {
        by_descriptor
            .entry((entry.src, entry.relation))
            .or_default()
            .push(entry);
    }
    let mut packed = Vec::new();
    for entries in by_descriptor.values_mut() {
        entries.sort_by_key(|entry| (entry.dst, entry.eid));
        packed.extend(entries.chunks(max_entries).map(<[AdjacencyEntry]>::to_vec));
    }

    // A root's list is publication order, witnessed by nondecreasing `last_seq`.
    // Canonical key packing does not necessarily preserve publication order.
    // Supersede has already collapsed every duplicate statement above, so
    // reordering these blocks cannot change last-wins precedence. Sort by the
    // truthful span before a root is allowed to name the result; canonical chunk
    // order is the deterministic tie breaker because `sort_by_key` is stable.
    packed.sort_by_key(|block| {
        crate::root::span_of(block)
            .map(|(first_seq, last_seq)| (last_seq, first_seq))
            // `retained` was proven non-empty and `chunks` never yields an empty
            // chunk, so the fallback is unreachable. Keeping the ordering total
            // avoids a production panic if that construction is ever refactored.
            .unwrap_or((CommitSeq(0), CommitSeq(0)))
    });
    packed
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::RelationId;
    use fgdb_types::{EId, VId};
    use std::collections::BTreeSet;

    fn entry(dst: u128) -> AdjacencyEntry {
        version(dst, 1, None)
    }

    fn version(dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
        AdjacencyEntry {
            src: VId(1),
            relation: RelationId(1),
            dst: VId(dst),
            eid: EId(dst),
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
                .map(|entry| (entry.src, entry.relation, entry.dst, entry.eid))
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

        let result =
            compact_with_limit(&before, CommitSeq(1), 2).expect("distinct edge identities compact");

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
    fn capacity_packing_is_a_fixed_point() {
        let before = vec![
            vec![
                version(2, 1, Some(8)),
                version(3, 1, None),
                version(4, 2, None),
            ],
            vec![version(5, 4, None), version(6, 4, None)],
            vec![version(7, 7, None), version(8, 7, None)],
        ];

        let once =
            compact_with_limit(&before, CommitSeq(1), 2).expect("distinct edge identities compact");

        assert_eq!(
            once.blocks.len(),
            4,
            "seven entries need four two-entry blocks"
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

        let twice = compact_with_limit(&once.blocks, CommitSeq(1), 2)
            .expect("the compacted history remains valid");
        assert_eq!(
            twice.blocks, once.blocks,
            "capacity-aware compaction is not a fixed point"
        );
        assert_eq!(twice.dropped, 0);
        assert_eq!(twice.superseded, 0);
    }

    #[test]
    fn small_capacities_are_minimal_lawful_and_deterministic() {
        for cardinality in 1u64..=12 {
            let retained = (1..=cardinality)
                .map(|eid| version(u128::from(eid), (eid * 3) % 7 + 1, None))
                .collect::<Vec<_>>();

            for max_entries in 1usize..=4 {
                let expected_blocks = retained.len().div_ceil(max_entries);
                let packed = pack_retained(retained.clone(), max_entries);

                assert_eq!(
                    packed.len(),
                    expected_blocks,
                    "wrong minimum for {cardinality} identities at capacity {max_entries}"
                );
                assert_packing_laws(&packed, max_entries);
                assert_eq!(
                    pack_retained(retained.clone(), max_entries),
                    packed,
                    "packing is not deterministic for {cardinality} identities at capacity \
                     {max_entries}"
                );
            }
        }
    }
}
