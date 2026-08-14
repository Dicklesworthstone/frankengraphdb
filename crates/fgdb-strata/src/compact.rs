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
use crate::edge_props::{BlockProps, EdgePropertyRow, MAX_PROPERTY_PATCH_ROWS};
use crate::root::{RootError, collapse_edge_history};
use fgdb_types::CommitSeq;
use std::collections::BTreeMap;

/// The result of compacting a partition's blocks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Compaction {
    /// The compacted blocks, ordered by nondecreasing upper sequence frontier —
    /// fewer than went in, unless nothing could be dropped.
    pub blocks: Vec<Vec<AdjacencyEntry>>,
    /// Each compacted block's hosted property column, parallel to `blocks` —
    /// the same shape [`crate::store::BlockStore::reopen`] answers. A retained
    /// entry keeps the row its winning statement carried; a block none of
    /// whose entries own a row hosts nothing.
    pub block_props: Vec<Option<BlockProps>>,
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
///
/// **THIS IS THE PROPERTYLESS FACE.** A partition whose blocks host property
/// patches must compact through [`compact_with_props`], or the replacement
/// blocks silently shed their rows — this face cannot see them to carry them.
pub fn compact(blocks: &[Vec<AdjacencyEntry>], floor: CommitSeq) -> Result<Compaction, RootError> {
    compact_with_props(blocks, &vec![None; blocks.len()], floor)
}

/// [`compact`], carrying each retained entry's property row into the
/// replacement blocks (fgdb-yqor).
///
/// `block_props` is the per-block column [`crate::store::BlockStore::reopen`]
/// answers, parallel to `blocks`. The winning statement's row travels with its
/// entry — a tombstone restated the row, so last-block-wins over props is the
/// same rule the read path applies — and packing cuts a block early when its
/// propertied entries would overflow one hosted patch, exactly as the writer's
/// seal does.
///
/// **THE ENCODING CALLER OWNS THE CHAIN RESTART** (V6, fgdb-4391): a repacked
/// generation's blocks link `None` at each family's first block, and when
/// packing splits one family into several blocks the caller must link each to
/// its predecessor in emission order, or root admission refuses the chain.
pub fn compact_with_props(
    blocks: &[Vec<AdjacencyEntry>],
    block_props: &[Option<BlockProps>],
    floor: CommitSeq,
) -> Result<Compaction, RootError> {
    compact_with_limit(
        blocks,
        block_props,
        floor,
        usize::try_from(crate::MAX_BLOCK_ENTRIES).unwrap_or(usize::MAX),
        usize::try_from(MAX_PROPERTY_PATCH_ROWS).unwrap_or(usize::MAX),
    )
}

fn compact_with_limit(
    blocks: &[Vec<AdjacencyEntry>],
    block_props: &[Option<BlockProps>],
    floor: CommitSeq,
    max_entries: usize,
    max_patch_rows: usize,
) -> Result<Compaction, RootError> {
    if blocks.len() != block_props.len() {
        return Err(RootError::BlockPropsArity {
            blocks: blocks.len(),
            props: block_props.len(),
        });
    }

    // Use the exact validation and last-block-wins rule the read path uses. A
    // compactor must not turn allocator corruption into a new lawful-looking
    // root merely because the conflicting births occupy different blocks.
    let (entries, superseded) = collapse_edge_history(blocks)?;

    // The winning statement's row, by the same forward last-block-wins pass
    // the collapse applied to the entries themselves — shared with the
    // whole-graph scan so the two cannot drift apart.
    let mut winning_props = crate::root::winning_edge_rows(blocks, block_props);

    // DROP BY OBSERVABILITY, NOT BY AGE. A version whose life ended at or before
    // the floor can never be seen again; one still live at the floor must stay
    // however old its creation is.
    let before_floor = entries.len();
    let retained: Vec<(AdjacencyEntry, EdgePropertyRow)> = entries
        .into_iter()
        .filter(|(_, entry)| entry.retired_at.is_none_or(|r| r.0 > floor.0))
        .map(|(key, entry)| {
            let row = winning_props.remove(&key).unwrap_or_default();
            (entry, row)
        })
        .collect();
    let dropped = before_floor - retained.len();

    let (packed, packed_props) = pack_retained(retained, max_entries, max_patch_rows);
    Ok(Compaction {
        blocks: packed,
        block_props: packed_props,
        dropped,
        superseded,
    })
}

/// Consolidate a vertex patch publication history: fewer patches, the same
/// answer at every sequence (fgdb-ge6a, the vertex half of [`compact`]).
///
/// Later patches restate a statement `(vid, created_at)` to add its
/// retirement, and the later statement wins — exactly the block model, per
/// statement. The floor licenses dropping nothing here until the transaction
/// layer owns snapshot tracking, so this collapses RESTATEMENTS only; the
/// result repacks in canonical `(vid, created_at)` order at the format's
/// row ceiling, which [`crate::vertex::validate_succession`]'s chain laws
/// admit because collapse preserves every surviving statement byte-for-byte.
pub fn compact_vertex_patches(
    patches: &[Vec<crate::vertex::VertexRow>],
    floor: CommitSeq,
) -> (Vec<Vec<crate::vertex::VertexRow>>, usize) {
    let mut statements: BTreeMap<(fgdb_types::VId, CommitSeq), crate::vertex::VertexRow> =
        BTreeMap::new();
    let mut seen = 0usize;
    for rows in patches {
        for row in rows {
            seen += 1;
            statements.insert((row.vid, row.created_at), row.clone());
        }
    }
    let retained: Vec<crate::vertex::VertexRow> = statements
        .into_values()
        .filter(|row| row.retired_at.is_none_or(|r| r.0 > floor.0))
        .collect();
    let superseded = seen - retained.len();
    let ceiling = usize::try_from(crate::vertex::MAX_PATCH_ROWS).unwrap_or(usize::MAX);
    let mut packed: Vec<Vec<crate::vertex::VertexRow>> = retained
        .chunks(ceiling)
        .map(<[crate::vertex::VertexRow]>::to_vec)
        .collect();
    // A root's patch list is publication order, witnessed by nondecreasing
    // frontiers — canonical row packing does not preserve it, so sort by the
    // truthful span exactly as the block packer does.
    packed.sort_by_key(|rows| {
        crate::vertex::span_of_rows(rows)
            .map(|(first_seq, last_seq)| (last_seq, first_seq))
            .unwrap_or((CommitSeq(0), CommitSeq(0)))
    });
    (packed, superseded)
}

type PackedBlocks = (Vec<Vec<AdjacencyEntry>>, Vec<Option<BlockProps>>);

fn pack_retained(
    mut retained: Vec<(AdjacencyEntry, EdgePropertyRow)>,
    max_entries: usize,
    max_patch_rows: usize,
) -> PackedBlocks {
    if retained.is_empty() {
        return (Vec::new(), Vec::new());
    }
    debug_assert!(max_entries > 0, "the durable block capacity is nonzero");
    debug_assert!(max_patch_rows > 0, "the hosted patch capacity is nonzero");

    // V3's durable unit is per descriptor.  Capacity is therefore a lower
    // bound within each `(src, relation)` family, never permission to merge two
    // descriptors merely because a block has spare rows.
    let mut by_descriptor: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for pair in retained.drain(..) {
        by_descriptor
            .entry((pair.0.src, pair.0.relation))
            .or_default()
            .push(pair);
    }
    let mut packed: Vec<(Vec<AdjacencyEntry>, Option<BlockProps>)> = Vec::new();
    for pairs in by_descriptor.values_mut() {
        pairs.sort_by_key(|(entry, _)| (entry.dst, entry.eid, entry.created_at));
        // Cut a block at entry capacity OR at the hosted patch's row ceiling,
        // whichever binds first — a block hosts at most one patch, so packing
        // past the row ceiling would emit an unencodable block.
        let mut chunk: Vec<(AdjacencyEntry, EdgePropertyRow)> = Vec::new();
        let mut propertied = 0usize;
        for pair in pairs.drain(..) {
            if chunk.len() == max_entries || (!pair.1.is_empty() && propertied == max_patch_rows) {
                packed.push(seal_chunk(std::mem::take(&mut chunk)));
                propertied = 0;
            }
            propertied += usize::from(!pair.1.is_empty());
            chunk.push(pair);
        }
        if !chunk.is_empty() {
            packed.push(seal_chunk(chunk));
        }
    }

    // A root's list is publication order, witnessed by nondecreasing `last_seq`.
    // Canonical key packing does not necessarily preserve publication order.
    // Supersede has already collapsed every duplicate statement above, so
    // reordering these blocks cannot change last-wins precedence. Sort by the
    // truthful span before a root is allowed to name the result; canonical chunk
    // order is the deterministic tie breaker because `sort_by_key` is stable.
    packed.sort_by_key(|(block, _)| {
        crate::root::span_of(block)
            .map(|(first_seq, last_seq)| (last_seq, first_seq))
            // `retained` was proven non-empty and the packer emits no empty
            // chunk, so the fallback is unreachable. Keeping the ordering total
            // avoids a production panic if that construction is ever refactored.
            .unwrap_or((CommitSeq(0), CommitSeq(0)))
    });
    packed.into_iter().unzip()
}

/// One packed chunk becomes a block and, when any entry owns a row, the
/// hosted-patch column beside it — locators are 1-based in entry order, the
/// bijection [`crate::edge_props::validate_block_patch_consistency`] demands.
fn seal_chunk(
    chunk: Vec<(AdjacencyEntry, EdgePropertyRow)>,
) -> (Vec<AdjacencyEntry>, Option<BlockProps>) {
    let mut entries = Vec::with_capacity(chunk.len());
    let mut locators = Vec::with_capacity(chunk.len());
    let mut rows = Vec::new();
    for (entry, row) in chunk {
        if row.is_empty() {
            locators.push(0);
        } else {
            rows.push(row);
            locators.push(
                u8::try_from(rows.len()).expect("the packer cut at the hosted patch's row ceiling"),
            );
        }
        entries.push(entry);
    }
    let props = if rows.is_empty() {
        None
    } else {
        Some(BlockProps { locators, rows })
    };
    (entries, props)
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
            crate::encode_block(0, None, block).expect("every compacted block remains encodable");
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

        let result = compact_with_limit(&before, &[None], CommitSeq(1), 2, 2)
            .expect("distinct edge identities compact");

        assert_eq!(
            result.blocks.len(),
            3,
            "five entries need three two-entry blocks"
        );
        assert_eq!(result.dropped, 0);
        assert_eq!(result.superseded, 0);
        assert!(
            result.block_props.iter().all(Option::is_none),
            "propertyless input packs propertyless blocks"
        );
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
        let no_props = vec![None; before.len()];

        let once = compact_with_limit(&before, &no_props, CommitSeq(1), 2, 2)
            .expect("distinct edge identities compact");

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

        let twice = compact_with_limit(&once.blocks, &once.block_props, CommitSeq(1), 2, 2)
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
                .map(|eid| {
                    (
                        version(u128::from(eid), (eid * 3) % 7 + 1, None),
                        Vec::new(),
                    )
                })
                .collect::<Vec<_>>();

            for max_entries in 1usize..=4 {
                let expected_blocks = retained.len().div_ceil(max_entries);
                let (packed, packed_props) =
                    pack_retained(retained.clone(), max_entries, max_entries);

                assert_eq!(
                    packed.len(),
                    expected_blocks,
                    "wrong minimum for {cardinality} identities at capacity {max_entries}"
                );
                assert_eq!(packed_props.len(), packed.len());
                assert_packing_laws(&packed, max_entries);
                assert_eq!(
                    pack_retained(retained.clone(), max_entries, max_entries),
                    (packed, packed_props),
                    "packing is not deterministic for {cardinality} identities at capacity \
                     {max_entries}"
                );
            }
        }
    }

    #[test]
    fn the_hosted_patch_row_ceiling_cuts_a_block_early() {
        use fgdb_delta_types::PropertyKeyId;
        use fgdb_types::CanonicalScalar;
        let row = |seed: u64| vec![(PropertyKeyId(seed), CanonicalScalar::Int(seed as i64))];

        // Five entries, three propertied, entry capacity 5, row ceiling 2: the
        // third propertied entry cannot join the first block's patch.
        let retained: Vec<(AdjacencyEntry, crate::edge_props::EdgePropertyRow)> = (1..=5u128)
            .map(|dst| {
                let props = if dst % 2 == 1 {
                    row(dst as u64)
                } else {
                    Vec::new()
                };
                (entry(dst), props)
            })
            .collect();
        let (packed, packed_props) = pack_retained(retained, 5, 2);

        assert_eq!(
            packed.len(),
            2,
            "the row ceiling, not entry capacity, forces the second block"
        );
        assert_packing_laws(&packed, 5);
        for (block, props) in packed.iter().zip(&packed_props) {
            let props = props
                .as_ref()
                .expect("every packed block here carries at least one propertied entry");
            assert_eq!(props.locators.len(), block.len());
            crate::edge_props::validate_block_patch_consistency(&props.locators, props.rows.len())
                .expect("the packed column satisfies the joint bijection law");
            assert!(props.rows.len() <= 2, "a chunk exceeded the row ceiling");
            // Every propertied entry still owns its exact row.
            for (index, entry) in block.iter().enumerate() {
                let expected = if entry.dst.0 % 2 == 1 {
                    row(entry.dst.0 as u64)
                } else {
                    Vec::new()
                };
                assert_eq!(props.props_of(index), expected);
            }
        }
    }

    /// 256 same-family propertied entries must pack into two blocks and
    /// each block must encode: a full 255-row locator column is lawful
    /// (fgdb-hc04).
    #[test]
    fn a_full_property_patch_column_packs_and_encodes() {
        use fgdb_delta_types::PropertyKeyId;
        use fgdb_types::CanonicalScalar;
        let count = usize::try_from(MAX_PROPERTY_PATCH_ROWS).expect("fits") + 1;
        let retained: Vec<(AdjacencyEntry, EdgePropertyRow)> = (1..=count as u128)
            .map(|dst| {
                (
                    entry(dst),
                    vec![(PropertyKeyId(dst as u64), CanonicalScalar::Int(dst as i64))],
                )
            })
            .collect();
        let (packed, packed_props) = pack_retained(
            retained,
            usize::try_from(crate::MAX_BLOCK_ENTRIES).expect("fits"),
            usize::try_from(MAX_PROPERTY_PATCH_ROWS).expect("fits"),
        );
        assert_eq!(
            packed.len(),
            2,
            "256 propertied entries split at the 255-row ceiling"
        );
        assert_eq!(packed_props.len(), 2);
        for (block, props) in packed.iter().zip(&packed_props) {
            let props = props
                .as_ref()
                .expect("every packed block here is propertied");
            crate::edge_props::validate_block_patch_consistency(&props.locators, props.rows.len())
                .expect("the packed column satisfies the joint bijection law");
            crate::encode_block_with_properties(
                0,
                None,
                block,
                fgdb_types::ids::ObjectId([0xab; 32]),
                &props.locators,
                &props.rows,
            )
            .expect("a packed chunk at the locator ceiling must encode");
        }
        let total: usize = packed.iter().map(Vec::len).sum();
        assert_eq!(total, count, "every retained edge survived the split");
    }
}
