//! Laws of compaction — fewer blocks, same answers.
//!
//! **THE LOAD-BEARING LAW IS ANSWER-PRESERVATION**, swept across every sequence at
//! or above the floor rather than probed. Everything else here — fewer blocks,
//! fewer entries — is a benefit that is only worth having if the answers did not
//! move, and a compactor that merged aggressively while changing one answer at one
//! sequence would look like a win in every metric anyone thinks to measure.
//!
//! **DROPPING IS BY OBSERVABILITY, NOT BY AGE.** A version still live at the floor
//! must stay however old its creation is. Dropping by age is the classic MVCC bug
//! and it silently empties a graph whose edges were all created long ago, so it
//! gets its own law with a deliberately ancient live edge.
//!
//! EIds are permanently spent. A later block may restate one exact birth to add a
//! retirement, but a changed topology or `created_at` is malformed history and
//! compaction must refuse it rather than minting plausible replacement blocks.

use fgdb_delta_types::RelationId;
use fgdb_strata::compact::compact;
use fgdb_strata::root::{
    BlockRef, EdgeBirth, EdgeIdentityConflict, PartitionRoot, RootError, encode_root,
    merge_neighbours, span_of,
};
use fgdb_strata::{AdjacencyEntry, block_id, encode_block};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};

const REL: RelationId = RelationId(1);
const K_OID: [u8; 32] = [0x5a; 32];

fn entry(src: u128, dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    edge(
        src.wrapping_mul(1_000_000).wrapping_add(dst),
        src,
        dst,
        created,
        retired,
    )
}

fn edge(eid: u128, src: u128, dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        eid: EId(eid),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn identity_mismatch(expected: AdjacencyEntry, found: AdjacencyEntry) -> RootError {
    let birth = |entry: AdjacencyEntry| EdgeBirth {
        src: entry.src,
        relation: entry.relation,
        dst: entry.dst,
        created_at: entry.created_at,
    };
    RootError::EdgeIdentityMismatch {
        eid: expected.eid,
        conflict: Box::new(EdgeIdentityConflict {
            expected: birth(expected),
            found: birth(found),
        }),
    }
}

/// Assert the compacted blocks answer identically to the originals at every
/// sequence from `floor` to `last`, for every source mentioned.
fn assert_answers_preserved(
    before: &[Vec<AdjacencyEntry>],
    after: &[Vec<AdjacencyEntry>],
    floor: CommitSeq,
    last: u64,
    sources: &[u128],
) {
    for source in sources {
        for as_of in floor.0..=last {
            let original = merge_neighbours(before, VId(*source), REL, CommitSeq(as_of))
                .expect("the original merges");
            let compacted = merge_neighbours(after, VId(*source), REL, CommitSeq(as_of))
                .expect("the compaction merges");
            assert_eq!(
                original, compacted,
                "source {source} disagrees at {as_of} after compaction"
            );
        }
    }
}

/// Every compacted block must still be a LAWFUL block — canonical order, unique
/// keys — or the compactor has produced something the encoder would refuse.
fn assert_blocks_are_lawful(blocks: &[Vec<AdjacencyEntry>]) {
    let mut published_at = CommitSeq(1);
    let mut references = Vec::with_capacity(blocks.len());
    for (index, block) in blocks.iter().enumerate() {
        let encoded = encode_block(block);
        assert!(
            encoded.is_ok(),
            "compacted block {index} is not encodable: {encoded:?}"
        );
        let bytes = encoded.expect("the assertion checked the encoding result");
        let (first_seq, last_seq) = span_of(block).expect("compacted blocks are non-empty");
        published_at = CommitSeq(published_at.0.max(last_seq.0));
        references.push(BlockRef {
            block_id: block_id(&K_OID, DatabaseSecurityNamespaceId([0x77; 32]), &bytes),
            first_seq,
            last_seq,
        });
    }
    let root = PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(1),
        partition: 0,
        published_at,
        blocks: references,
    };
    encode_root(&root).expect("compacted blocks form a lawful ordered root");
}

/// Blocks with disjoint descriptors remain descriptor-local, and answers do not move.
#[test]
fn disjoint_descriptors_remain_separate_blocks() {
    let before = vec![
        vec![entry(1, 2, 1, None)],
        vec![entry(1, 3, 2, None)],
        vec![entry(2, 3, 3, None)],
    ];
    let result = compact(&before, CommitSeq(1)).expect("valid history compacts");
    assert_eq!(
        result.blocks.len(),
        2,
        "the V3 descriptor boundary is stronger than spare block capacity"
    );
    assert_eq!(result.dropped, 0);
    assert_blocks_are_lawful(&result.blocks);
    assert_answers_preserved(&before, &result.blocks, CommitSeq(1), 5, &[1, 2]);
}

/// Compaction supersedes statements per EId, never per topology. A tombstone for
/// one parallel edge cannot collapse or retire another EId at the same destination.
#[test]
fn compaction_preserves_parallel_edge_identities() {
    let before = vec![
        vec![edge(10, 1, 2, 1, None)],
        vec![edge(20, 1, 2, 2, None)],
        vec![edge(10, 1, 2, 1, Some(4))],
    ];
    let result = compact(&before, CommitSeq(2)).expect("valid history compacts");
    assert_eq!(result.superseded, 1, "only e10's creation was restated");
    assert_eq!(result.dropped, 0);
    assert_eq!(result.blocks.len(), 1, "distinct EIds may share one block");
    assert_eq!(
        result.blocks[0]
            .iter()
            .map(|entry| entry.eid)
            .collect::<Vec<_>>(),
        vec![EId(10), EId(20)],
        "neither stable identity was collapsed by equal topology"
    );
    assert_eq!(
        merge_neighbours(&result.blocks, VId(1), REL, CommitSeq(4)).expect("merges"),
        vec![VId(2)],
        "e20 remains visible after e10 retires"
    );
}

/// Compaction validates the same immutable EId birth as the read merge. It must
/// not turn either topology drift or a later reuse into canonical output blocks.
#[test]
fn compaction_refuses_eid_history_corruption() {
    let topology_drift = vec![
        vec![edge(10, 1, 2, 1, None)],
        vec![edge(10, 1, 3, 1, Some(4))],
    ];
    assert_eq!(
        compact(&topology_drift, CommitSeq(2)),
        Err(identity_mismatch(
            edge(10, 1, 2, 1, None),
            edge(10, 1, 3, 1, Some(4)),
        ))
    );

    let rebirth = vec![
        vec![edge(10, 1, 2, 1, Some(3))],
        vec![edge(10, 1, 2, 5, None)],
    ];
    assert_eq!(
        compact(&rebirth, CommitSeq(4)),
        Err(identity_mismatch(
            edge(10, 1, 2, 1, Some(3)),
            edge(10, 1, 2, 5, None),
        ))
    );

    let resurrection = vec![
        vec![edge(10, 1, 2, 1, Some(3))],
        vec![edge(10, 1, 2, 1, None)],
    ];
    assert_eq!(
        compact(&resurrection, CommitSeq(4)),
        Err(RootError::EdgeRetirementMismatch {
            eid: EId(10),
            expected: Some(CommitSeq(3)),
            found: None,
        })
    );
}

/// **A RETIRED VERSION BELOW THE FLOOR IS DROPPED, AND THAT IS WHAT LETS TWO
/// BLOCKS BECOME ONE.**
///
/// The old and replacement edges have distinct EIds. Dropping the identity whose
/// life ended below the floor reclaims exactly that unobservable history.
#[test]
fn a_version_retired_below_the_floor_is_dropped_and_the_blocks_merge() {
    let before = vec![
        vec![edge(10, 1, 2, 1, Some(4))],
        vec![edge(20, 1, 2, 6, None), edge(30, 1, 3, 6, None)],
    ];
    let result = compact(&before, CommitSeq(5)).expect("valid history compacts");
    assert_eq!(
        result.dropped, 1,
        "the version that ended at 4 is unobservable"
    );
    assert_eq!(
        result.blocks.len(),
        1,
        "the retained identities fit in one block"
    );
    assert_blocks_are_lawful(&result.blocks);
    assert_answers_preserved(&before, &result.blocks, CommitSeq(5), 9, &[1]);
}

/// **DROPPING IS BY OBSERVABILITY, NOT BY AGE.**
///
/// An edge created at sequence 1 and never retired is still live at a floor of
/// 1000. A compactor that dropped by age would empty a graph whose edges were all
/// created long ago — the classic MVCC bug, and one that looks like a very
/// effective compaction right up until every read returns nothing.
#[test]
fn an_ancient_but_live_version_is_never_dropped() {
    let before = vec![vec![entry(1, 2, 1, None), entry(1, 3, 2, Some(3))]];
    let result = compact(&before, CommitSeq(1_000)).expect("valid history compacts");
    assert_eq!(result.dropped, 1, "only the retired one goes");
    assert_eq!(
        merge_neighbours(&result.blocks, VId(1), REL, CommitSeq(1_000)).expect("merges"),
        vec![VId(2)],
        "the ancient live edge survives an enormous floor"
    );
}

/// A version retired EXACTLY AT the floor is dropped; one retired just after is
/// kept. The boundary is checked on both sides in one test, because two separate
/// tests can both pass while the real threshold sits somewhere else entirely.
#[test]
fn the_floor_is_the_exact_drop_boundary() {
    let at = vec![vec![entry(1, 2, 1, Some(5))]];
    let past = vec![vec![entry(1, 2, 1, Some(6))]];
    assert_eq!(
        compact(&at, CommitSeq(5))
            .expect("valid history compacts")
            .dropped,
        1,
        "retired AT the floor is unobservable — intervals are half-open"
    );
    assert_eq!(
        compact(&past, CommitSeq(5))
            .expect("valid history compacts")
            .dropped,
        0,
        "retired after the floor is still visible at the floor"
    );
}

/// A replacement topology uses a fresh EId. Both identities remain observable
/// above a low floor and can share one canonical block because their durable keys
/// are distinct.
#[test]
fn fresh_identity_after_retirement_packs_with_its_predecessor() {
    let before = vec![
        vec![edge(10, 1, 2, 1, Some(4)), edge(11, 1, 3, 1, None)],
        vec![edge(20, 1, 2, 6, None)],
    ];
    // A floor of 2 leaves the old identity observable (it lives until 4).
    let result = compact(&before, CommitSeq(2)).expect("valid history compacts");
    assert_eq!(result.dropped, 0);
    assert_eq!(
        result.blocks.len(),
        1,
        "fresh EIds are distinct keys and fit in one block"
    );
    assert_blocks_are_lawful(&result.blocks);
    assert_answers_preserved(&before, &result.blocks, CommitSeq(2), 9, &[1]);
}

/// Compaction PRESERVES ANSWERS across creations, retirements and fresh-identity
/// topology replacements, swept at every sequence at or above the floor.
///
/// The composite case: if any of the drop rule, the packing, or the supersede
/// precedence is wrong, one of these sequences disagrees.
#[test]
fn a_mixed_history_compacts_without_moving_any_answer() {
    let before = vec![
        vec![edge(10, 1, 2, 1, Some(4)), edge(11, 1, 3, 2, None)],
        vec![edge(20, 1, 2, 5, Some(8)), edge(21, 2, 3, 5, None)],
        vec![edge(30, 1, 2, 9, None), edge(31, 1, 4, 9, None)],
    ];
    for floor in [1u64, 5, 8, 9] {
        let result = compact(&before, CommitSeq(floor)).expect("valid history compacts");
        assert_blocks_are_lawful(&result.blocks);
        assert_answers_preserved(&before, &result.blocks, CommitSeq(floor), 12, &[1, 2]);
        assert!(
            result.blocks.len() <= before.len(),
            "compaction must not increase the block count at floor {floor}"
        );
    }
}

/// A HIGHER FLOOR DROPS AT LEAST AS MUCH as a lower one — the drop rule is
/// monotone in the floor.
///
/// Without this, a compactor could drop MORE at a lower floor, which would mean it
/// is not dropping by observability at all; the property is cheap to state and
/// would catch an inverted comparison that every single-floor law tolerates.
#[test]
fn dropping_is_monotone_in_the_floor() {
    let before = vec![vec![
        entry(1, 2, 1, Some(3)),
        entry(1, 3, 1, Some(7)),
        entry(1, 4, 1, None),
    ]];
    let mut previous = 0usize;
    for floor in [1u64, 3, 5, 7, 9, 100] {
        let dropped = compact(&before, CommitSeq(floor))
            .expect("valid history compacts")
            .dropped;
        assert!(
            dropped >= previous,
            "floor {floor} dropped {dropped}, fewer than a lower floor's {previous}"
        );
        previous = dropped;
    }
    assert_eq!(previous, 2, "and the live edge is never dropped");
}

/// Compacting an already-compact partition is a fixed point: it changes nothing.
#[test]
fn compaction_is_idempotent() {
    let before = vec![
        vec![edge(10, 1, 2, 1, Some(4))],
        vec![edge(20, 1, 2, 6, None), edge(30, 1, 3, 6, None)],
    ];
    let once = compact(&before, CommitSeq(5)).expect("valid history compacts");
    let twice = compact(&once.blocks, CommitSeq(5)).expect("compacted history stays valid");
    assert_eq!(
        twice.blocks, once.blocks,
        "a second pass changed the blocks"
    );
    assert_eq!(twice.dropped, 0, "and had nothing left to drop");
}

/// Compacting nothing produces nothing, rather than an empty block.
#[test]
fn compacting_nothing_produces_nothing() {
    let result = compact(&[], CommitSeq(1)).expect("empty history compacts");
    assert!(result.blocks.is_empty() && result.dropped == 0);
    // And a partition whose every version is below the floor compacts away
    // entirely — an empty partition is a legitimate state, not an empty block.
    let all_dead = vec![vec![entry(1, 2, 1, Some(2))]];
    let result = compact(&all_dead, CommitSeq(5)).expect("valid history compacts");
    assert_eq!(result.dropped, 1);
    assert!(
        result.blocks.is_empty(),
        "nothing observable remains, so there is no block to write"
    );
}

/// **COMPACTION MUST USE THE SAME SUPERSEDE PRECEDENCE AS THE MERGE: last block
/// wins for one version.**
///
/// The cross-block retirement is the case: an early block says the version is
/// live, a later block restates the SAME version (same `created_at`) with its
/// retirement. Compaction must keep the later statement — a compactor that took
/// the first would resurrect a retired edge, and the compacted partition would
/// answer differently from the one it replaced.
///
/// Added after mutation: reversing the precedence left every other law in this
/// file green, because no other fixture restates one version across two blocks.
/// A merge rule can only be tested by a history that exercises the merge.
#[test]
fn compaction_supersedes_by_last_block_like_the_merge_does() {
    let before = vec![
        vec![entry(1, 2, 1, None), entry(1, 3, 1, None)],
        // The SAME version of (1,REL,2) — same created_at — now retired.
        vec![entry(1, 2, 1, Some(6))],
    ];
    let result = compact(&before, CommitSeq(2)).expect("valid history compacts");
    assert_eq!(result.dropped, 0, "nothing ended below the floor");
    assert_eq!(
        result.superseded, 1,
        "one entry was collapsed by supersede, which is NOT a floor drop"
    );
    assert_blocks_are_lawful(&result.blocks);

    assert_eq!(
        merge_neighbours(&result.blocks, VId(1), REL, CommitSeq(7)).expect("merges"),
        vec![VId(3)],
        "the retirement stated by the LATER block must survive compaction"
    );
    // And it still agrees with the uncompacted partition everywhere.
    assert_answers_preserved(&before, &result.blocks, CommitSeq(2), 9, &[1]);

    // The retired version also compacts away once the floor passes it, which the
    // first-wins bug could not do either — it would keep resurrecting the live
    // statement instead.
    let later = compact(&before, CommitSeq(6)).expect("valid history compacts");
    assert_eq!(later.dropped, 1);
    assert_eq!(
        merge_neighbours(&later.blocks, VId(1), REL, CommitSeq(6)).expect("merges"),
        vec![VId(3)]
    );
}

/// Compaction emits entries in canonical key order and its finished blocks in
/// nondecreasing upper-frontier order before a partition root can name them.
#[test]
fn compacted_entries_have_a_rootable_publication_order() {
    let before = vec![
        vec![edge(10, 1, 2, 1, Some(10)), edge(20, 2, 3, 2, Some(1_000))],
        vec![edge(30, 1, 2, 20, Some(30)), edge(40, 2, 3, 1_001, None)],
        vec![edge(50, 1, 2, 40, None)],
    ];
    let result = compact(&before, CommitSeq(1)).expect("valid history compacts");
    assert_eq!(
        result.blocks.len(),
        2,
        "five identities span two V3 descriptors"
    );
    assert_blocks_are_lawful(&result.blocks);

    let upper_frontiers = result
        .blocks
        .iter()
        .map(|block| span_of(block).expect("non-empty").1.0)
        .collect::<Vec<_>>();
    assert!(
        upper_frontiers.windows(2).all(|pair| pair[0] <= pair[1]),
        "publication frontiers regressed: {upper_frontiers:?}"
    );
    assert_answers_preserved(&before, &result.blocks, CommitSeq(1), 1_002, &[1, 2]);
}
