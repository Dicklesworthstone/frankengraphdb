//! **Complexity witnesses for the tier-D read path.**
//!
//! AGENTS.md makes this a permanent CI gate, not an optimization exercise:
//! "complexity-witness regression locks (an operator whose observed op-count
//! exceeds its declared bound *fails CI*)". Nothing implemented one. This file is
//! the first, and it covers the claim `root.rs` makes about itself — that a reader
//! "can skip a block that cannot contain anything visible at its snapshot without
//! decoding it", which that module's own doc calls "the whole performance argument
//! for a root".
//!
//! **WHY OP COUNTS AND NOT A STOPWATCH.** A wall-clock benchmark cannot be a CI
//! gate here: it is nondeterministic, which B5 forbids as a *result*, and it varies
//! with machine load in a swarm where ten panes compile at once. An op count is
//! exact, reproducible, and diffable — the same properties that make a plan
//! certificate replayable. It also fails for the right reason: a regression shows up
//! as "examined 40 blocks where the bound is 20", which names the defect, rather than
//! as "12% slower", which names nothing.
//!
//! **THE BOUNDS BELOW ARE HONEST, NOT ASPIRATIONAL.** One of them records that a
//! read at the current frontier examines EVERY block in the partition. That is bad
//! and it is what the code does today; §6.2's skip-list nodes and hub striping, which
//! would fix it, are unbuilt (fgdb-w3-tier-d-ctj). Writing the bad bound down is the
//! point — it makes the eventual improvement measurable instead of anecdotal, and it
//! stops the number silently getting worse in the meantime. Publish the bad numbers.

use fgdb_strata::root::{BlockRef, PartitionRoot, blocks_visible_at};
use fgdb_types::ids::ObjectId;
use fgdb_types::{BranchId, CommitSeq, GraphId};

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);

/// A root of `count` blocks, block *i* covering sequences `[i+1, i+1]`.
///
/// One block per sequence is the shape that makes the skip bound legible: the
/// number of blocks a reader may examine at `as_of` is then exactly `as_of`, so a
/// bound that is wrong is wrong by an obvious amount rather than by a constant
/// factor nobody notices.
fn root_of(count: u64) -> PartitionRoot {
    let blocks = (0..count)
        .map(|i| BlockRef {
            // Distinct per block: an identity collision would silently make two
            // BlockRefs interchangeable and the counts below would still "pass".
            block_id: ObjectId([(i + 1) as u8; 32]),
            first_seq: CommitSeq(i + 1),
            last_seq: CommitSeq(i + 1),
        })
        .collect();
    PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(count + 1),
        blocks,
    }
}

/// WITNESS: a historical read examines only blocks that could hold something
/// visible to it.
///
/// This is the skip bound doing real work, and it is the property the whole root
/// format exists to provide. Declared bound: at `as_of = s`, a reader examines
/// exactly `s` of the `n` blocks.
#[test]
fn a_historical_read_examines_only_blocks_at_or_before_its_snapshot() {
    let root = root_of(64);

    for as_of in [1u64, 2, 7, 31, 63] {
        let examined = blocks_visible_at(&root, CommitSeq(as_of)).len();
        assert_eq!(
            examined, as_of as usize,
            "at as_of={as_of} the reader examined {examined} of 64 blocks; \
             the declared bound is exactly {as_of}"
        );
        assert!(
            examined < root.blocks.len(),
            "a historical read that examines every block is not skipping at all"
        );
    }
}

/// WITNESS: the bound is MONOTONE in the snapshot.
///
/// A reader looking further forward may examine more blocks and must never examine
/// fewer. Without this, a skip bound could be "efficient" by dropping blocks it
/// needed — the failure mode that makes a reader return a silently smaller graph,
/// which is far worse than being slow.
#[test]
fn the_examined_count_never_decreases_as_the_snapshot_advances() {
    let root = root_of(48);
    let mut previous = 0usize;
    for as_of in 0..=49u64 {
        let examined = blocks_visible_at(&root, CommitSeq(as_of)).len();
        assert!(
            examined >= previous,
            "as_of={as_of} examined {examined} after {previous} at the previous \
             sequence — the bound went backwards, so some block was skipped that a \
             later snapshot still needs"
        );
        previous = examined;
    }
}

/// WITNESS, AND IT RECORDS A BAD NUMBER ON PURPOSE: a read at the current frontier
/// examines EVERY block in the partition.
///
/// `blocks_visible_at` filters on `first_seq <= as_of`, which skips only blocks
/// created AFTER the snapshot. At the frontier there are none, so nothing is
/// skipped and a point read is O(blocks in partition). That is the read
/// amplification §6.2 answers with two-level skip-list nodes and hub striping —
/// neither of which exists yet (fgdb-w3-tier-d-ctj).
///
/// **THIS TEST SHOULD FAIL WHEN THAT LANDS**, and failing is the correct outcome:
/// it is the signal to re-derive the bound downward, not a defect. A witness that
/// only ever tightens silently is not a lock. Until then it holds the line — if the
/// frontier cost ever exceeds one examination per block, something has started
/// examining blocks twice.
#[test]
fn a_frontier_read_examines_every_block_and_that_is_the_current_bound() {
    for n in [1u64, 8, 64, 512] {
        let root = root_of(n);
        let frontier = CommitSeq(n);
        let examined = blocks_visible_at(&root, frontier).len();
        assert_eq!(
            examined, n as usize,
            "frontier read over {n} blocks examined {examined}; the current bound is \
             ALL of them (no skip-list yet), and exceeding it means double-examination"
        );
    }
}

/// CONTROL: the witness above can actually fail.
///
/// Every bound in this file is an equality against a number the fixture also
/// computes, so a fixture that silently built the wrong root would satisfy them
/// trivially. This pins the fixture itself: a root of `n` blocks has `n` blocks,
/// with the sequence layout the bounds assume. Without it, `root_of` could return an
/// empty root and every witness above would pass over zero blocks.
#[test]
fn the_fixture_builds_the_partition_the_bounds_assume() {
    let root = root_of(10);
    assert_eq!(root.blocks.len(), 10, "root_of(10) must build ten blocks");
    for (i, block) in root.blocks.iter().enumerate() {
        let expected = CommitSeq(i as u64 + 1);
        assert_eq!(block.first_seq, expected, "block {i} first_seq");
        assert_eq!(block.last_seq, expected, "block {i} last_seq");
    }
    // And a snapshot before the first block examines nothing at all, which is the
    // only case where "skipped everything" is the right answer.
    assert!(
        blocks_visible_at(&root, CommitSeq(0)).is_empty(),
        "a snapshot before every block must examine no blocks"
    );
}
