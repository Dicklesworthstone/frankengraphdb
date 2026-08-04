//! **The first differential Strata has ever had a subject for.**
//!
//! Everything in `fgdb-strata` up to now is format law: canonical bytes, fail-closed
//! decoders, identity, ranges. All of it is checkable without any notion of what a
//! graph MEANS. This file is the point where the tier acquires a semantic
//! counterpart — the merged read across blocks must return exactly what
//! `fgdb-reference`, the deliberately simple oracle, says the same delta stream
//! implies.
//!
//! **WHY THIS FILE IS IN `fgdb-sim` AND NOT IN EITHER OF THEM.** §15.2 forbids
//! `fgdb-reference` from importing any engine crate, "so the differential cannot be
//! quietly gutted by code sharing". `fgdb-strata` depending on the oracle would
//! invert the layers and create the same hazard from the other side. The
//! verification layer is the one place both are legitimately visible, which is
//! exactly what §15 says it is for.
//!
//! **THE TWO SIDES SHARE NO CODE**, and that is what makes agreement evidence
//! rather than tautology. The oracle folds `DeltaRow`s into `BTreeMap`s of vertices
//! and edges and answers `neighbours` by scanning them. Strata encodes
//! `AdjacencyEntry`s into canonical bytes, splits them across immutable blocks, and
//! answers by merging versions across those blocks under a tombstone-supersede
//! rule. The only thing in common is the history they are both told about.
//!
//! **THE DIFFERENTIAL IS SWEPT, NOT PROBED.** Every fixture is checked at EVERY
//! sequence from before its first commit to past its last, for every source vertex.
//! A differential that samples one instant agrees with almost anything: the
//! interesting disagreements live at interval boundaries, which is precisely where
//! a single well-chosen probe is least likely to look.

mod common;

use common::{REL, Step, assert_agrees, build};
use fgdb_strata::root::merge_neighbours;
use fgdb_types::{CommitSeq, VId};

/// THE BASE CASE: a history with no deletions, split across two blocks.
///
/// The oracle knows nothing about blocks, so the split must not be observable.
#[test]
fn a_split_history_agrees_with_the_oracle() {
    let history = [
        (1u64, Step::CreateVertex(1)),
        (1, Step::CreateVertex(2)),
        (1, Step::CreateVertex(3)),
        (
            2,
            Step::AddEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
        (
            3,
            Step::AddEdge {
                eid: 11,
                src: 1,
                dst: 3,
            },
        ),
        (
            4,
            Step::AddEdge {
                eid: 12,
                src: 2,
                dst: 3,
            },
        ),
    ];
    // Split after the first edge, so the two edges of vertex 1 land in DIFFERENT
    // blocks — the case a single-block scan could never exercise.
    let (graph, blocks) = build(&history, &[3]);
    assert!(blocks.len() >= 2, "the fixture must actually split");
    assert_agrees(&graph, &blocks, &[1, 2, 3], 4);
}

/// Trigger A from fgdb-0trr: two live parallel EIds with equal topology may sit
/// in different blocks. They are distinct edges, not overlapping versions, while
/// the neighbour result remains the destination set.
#[test]
fn parallel_edges_across_blocks_agree_with_the_oracle() {
    let history = [
        (1u64, Step::CreateVertex(1)),
        (1, Step::CreateVertex(2)),
        (
            2,
            Step::AddEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
        (
            3,
            Step::AddEdge {
                eid: 20,
                src: 1,
                dst: 2,
            },
        ),
    ];
    let (graph, blocks) = build(&history, &[2]);
    assert!(
        blocks.len() >= 2,
        "the parallel EIds must cross a block cut"
    );
    assert_eq!(graph.neighbours(VId(1), REL), vec![VId(2)]);
    assert_eq!(
        merge_neighbours(&blocks, VId(1), REL, CommitSeq(3)).expect("merges"),
        vec![VId(2)]
    );
    assert_agrees(&graph, &blocks, &[1, 2], 3);
}

/// Trigger B from fgdb-0trr: retiring one of two parallel EIds must not tombstone
/// their shared topology key and erase the still-live peer. Prove both the
/// same-block fold and the cross-block tombstone path.
#[test]
fn retiring_one_parallel_edge_keeps_its_peer_visible() {
    let history = [
        (1u64, Step::CreateVertex(1)),
        (1, Step::CreateVertex(2)),
        (
            2,
            Step::AddEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
        (
            2,
            Step::AddEdge {
                eid: 20,
                src: 1,
                dst: 2,
            },
        ),
        (4, Step::DeleteEdge(10)),
    ];

    for cuts in [&[][..], &[3][..]] {
        let (graph, blocks) = build(&history, cuts);
        assert_eq!(graph.neighbours(VId(1), REL), vec![VId(2)]);
        assert_eq!(
            merge_neighbours(&blocks, VId(1), REL, CommitSeq(4)).expect("merges"),
            vec![VId(2)],
            "cut set {cuts:?} erased the surviving EId"
        );
        assert_agrees(&graph, &blocks, &[1, 2], 4);
    }
}

/// A CROSS-BLOCK RETIREMENT: the edge is created in one block and deleted in a
/// later one, which is the case an immutable block cannot express alone.
#[test]
fn a_cross_block_deletion_agrees_with_the_oracle() {
    let history = [
        (1u64, Step::CreateVertex(1)),
        (1, Step::CreateVertex(2)),
        (1, Step::CreateVertex(3)),
        (
            2,
            Step::AddEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
        (
            3,
            Step::AddEdge {
                eid: 11,
                src: 1,
                dst: 3,
            },
        ),
        (5, Step::DeleteEdge(10)),
    ];
    let (graph, blocks) = build(&history, &[4]);
    assert!(blocks.len() >= 2);
    assert_eq!(
        graph.neighbours(VId(1), REL),
        vec![VId(3)],
        "the oracle must have dropped the deleted edge, or this proves nothing"
    );
    assert_agrees(&graph, &blocks, &[1, 2, 3], 5);
}

/// A RE-CREATION after a deletion, across three blocks.
///
/// The case that caught the merge keying on `dst` alone: the newer version must
/// not erase the older one, and the present must still be right.
#[test]
fn a_re_created_edge_agrees_with_the_oracle() {
    let history = [
        (1u64, Step::CreateVertex(1)),
        (1, Step::CreateVertex(2)),
        (
            2,
            Step::AddEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
        (4, Step::DeleteEdge(10)),
        (
            6,
            Step::AddEdge {
                eid: 20,
                src: 1,
                dst: 2,
            },
        ),
    ];
    let (graph, blocks) = build(&history, &[2, 3]);
    assert!(
        blocks.len() >= 3,
        "creation, deletion and re-creation apart"
    );
    assert_eq!(graph.neighbours(VId(1), REL), vec![VId(2)]);
    assert_agrees(&graph, &blocks, &[1, 2], 6);

    // And the OLD version still answers at a sequence when it was live — the
    // property a key-keyed merge silently loses and the oracle cannot check,
    // because it holds current state rather than history.
    assert_eq!(
        merge_neighbours(&blocks, VId(1), REL, CommitSeq(3)).expect("merges"),
        vec![VId(2)]
    );
    assert_eq!(
        merge_neighbours(&blocks, VId(1), REL, CommitSeq(5)).expect("merges"),
        Vec::<VId>::new(),
        "and the gap between the two versions is empty"
    );
}

/// THE SPLIT IS NOT OBSERVABLE: the same history under every block boundary gives
/// the same answers.
///
/// The strongest form of the claim, and the one that would catch a merge whose
/// correctness depended on how the writer happened to batch. Two blocks agreeing
/// with the oracle proves the pair; every split agreeing proves the rule.
#[test]
fn every_block_boundary_gives_the_same_answers() {
    let history = [
        (1u64, Step::CreateVertex(1)),
        (1, Step::CreateVertex(2)),
        (1, Step::CreateVertex(3)),
        (
            2,
            Step::AddEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
        (
            3,
            Step::AddEdge {
                eid: 11,
                src: 1,
                dst: 3,
            },
        ),
        (4, Step::DeleteEdge(10)),
        (
            5,
            Step::AddEdge {
                eid: 12,
                src: 1,
                dst: 2,
            },
        ),
    ];
    let (graph, unsplit) = build(&history, &[]);
    let reference: Vec<Vec<VId>> = (1..=6)
        .map(|as_of| merge_neighbours(&unsplit, VId(1), REL, CommitSeq(as_of)).expect("merges"))
        .collect();

    for boundary in 0..history.len() {
        let (_, blocks) = build(&history, &[boundary]);
        let answers: Vec<Vec<VId>> = (1..=6)
            .map(|as_of| merge_neighbours(&blocks, VId(1), REL, CommitSeq(as_of)).expect("merges"))
            .collect();
        assert_eq!(
            answers, reference,
            "splitting after step {boundary} changed the answers"
        );
    }
    assert_agrees(&graph, &unsplit, &[1, 2, 3], 5);
}

/// A vertex with no edges answers empty on both sides, and a source the history
/// never mentions does too.
#[test]
fn empty_adjacencies_agree() {
    let history = [
        (1u64, Step::CreateVertex(1)),
        (1, Step::CreateVertex(2)),
        (
            2,
            Step::AddEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
    ];
    let (graph, blocks) = build(&history, &[]);
    assert_eq!(graph.neighbours(VId(2), REL), Vec::<VId>::new());
    assert_eq!(
        merge_neighbours(&blocks, VId(2), REL, CommitSeq(9)).expect("merges"),
        Vec::<VId>::new()
    );
    assert_eq!(
        merge_neighbours(&blocks, VId(99), REL, CommitSeq(9)).expect("merges"),
        Vec::<VId>::new(),
        "a source the history never mentions is empty, not an error"
    );
}

/// A VERTEX DELETION AND ITS CASCADE agree with the oracle.
///
/// The oracle removes the vertex and every incident edge in one row; Strata must
/// retire each cascaded edge as its own adjacency entry, in whichever block the
/// writer is filling. Added after mutation showed the differential could not see a
/// writer that ignored the declared cascade entirely.
#[test]
fn a_vertex_deletion_cascade_agrees_with_the_oracle() {
    let history = [
        (1u64, Step::CreateVertex(1)),
        (1, Step::CreateVertex(2)),
        (1, Step::CreateVertex(3)),
        (
            2,
            Step::AddEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
        (
            3,
            Step::AddEdge {
                eid: 11,
                src: 1,
                dst: 3,
            },
        ),
        (
            4,
            Step::AddEdge {
                eid: 12,
                src: 2,
                dst: 3,
            },
        ),
        (6, Step::DeleteVertex(1)),
    ];
    // Split so the creations and the cascade land in DIFFERENT blocks — the
    // cross-block case, which is the one an immutable block cannot express alone.
    let (graph, blocks) = build(&history, &[4]);
    assert!(blocks.len() >= 2, "the cascade must be in a later block");
    assert_eq!(
        graph.neighbours(VId(1), REL),
        Vec::<VId>::new(),
        "the oracle dropped the vertex and its edges"
    );
    assert_eq!(
        graph.neighbours(VId(2), REL),
        vec![VId(3)],
        "and kept the unrelated one, or this proves nothing"
    );
    assert_agrees(&graph, &blocks, &[1, 2, 3], 6);

    // Before the deletion the cascaded edges are still there — the cascade is a
    // retirement at a sequence, not an erasure.
    assert_eq!(
        merge_neighbours(&blocks, VId(1), REL, CommitSeq(5)).expect("merges"),
        vec![VId(2), VId(3)]
    );
}
