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

    // Pack into as few blocks as the one-version-per-key rule allows. Entries
    // arrive sorted by (key, created_at), so successive entries sharing a key are
    // adjacent: the Nth version of a key goes into the Nth block, which is the
    // minimum any packing can achieve.
    let mut packed: Vec<Vec<AdjacencyEntry>> = Vec::new();
    let mut previous_key = None;
    let mut depth = 0usize;
    for entry in retained {
        let key = (entry.src, entry.relation, entry.dst);
        if previous_key == Some(key) {
            depth += 1;
        } else {
            depth = 0;
            previous_key = Some(key);
        }
        if packed.len() == depth {
            packed.push(Vec::new());
        }
        packed[depth].push(entry);
    }

    // Each block's entries must be canonically ordered; the pack above preserves
    // key order within a block because it appends in key order.
    for block in &mut packed {
        block.sort_by_key(|e| (e.src, e.relation, e.dst));
    }

    // A root's list is publication order, witnessed by nondecreasing `last_seq`.
    // Packing by per-key version depth does not preserve that order: one key's
    // third version may be older than another key's second. Supersede has already
    // collapsed every duplicate statement of a version above, so reordering these
    // blocks cannot change last-wins precedence. Sort by the truthful span before a
    // root is allowed to name the result; the depth order is the deterministic tie
    // breaker because `sort_by_key` is stable.
    packed.sort_by_key(|block| {
        crate::root::span_of(block)
            .map(|(first_seq, last_seq)| (last_seq, first_seq))
            // Packing creates a block only while inserting its first entry, so the
            // fallback is unreachable. Keeping the ordering total here avoids a
            // production panic if that construction is ever refactored incorrectly.
            .unwrap_or((CommitSeq(0), CommitSeq(0)))
    });
    Compaction {
        blocks: packed,
        dropped,
        superseded,
    }
}
