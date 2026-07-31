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

use fgdb_delta_types::{DeltaRow, LabelId, PropertyKeyId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_strata::root::merge_neighbours;
use fgdb_strata::writer::BlockWriter;
use fgdb_strata::{AdjacencyEntry, decode_block};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};

const REL: RelationId = RelationId(1);
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const K_OID: [u8; 32] = [0x5a; 32];
const KEYS: (&[u8; 32], DatabaseSecurityNamespaceId) =
    (&K_OID, DatabaseSecurityNamespaceId([0x77; 32]));
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);

/// One step of a history: what happened, and at which commit sequence.
#[derive(Clone, Copy, Debug)]
enum Step {
    CreateVertex(u128),
    AddEdge {
        eid: u128,
        src: u128,
        dst: u128,
    },
    DeleteEdge {
        eid: u128,
        src: u128,
        dst: u128,
    },
    /// Retire a vertex and everything hanging off it.
    ///
    /// Added because MUTATION FOUND THIS MISSING: making the writer ignore a
    /// declared cascade left the differential green while reddening the writer's
    /// own laws. A differential that never deletes a vertex cannot see a cascade
    /// defect, however carefully it sweeps everything else.
    DeleteVertex(u128),
}

/// Build both sides from ONE history, so neither can be tuned to the other.
///
/// **THE STRATA SIDE IS THE REAL WRITER**, not a fixture that mimics it. An
/// earlier version of this file hand-built `AdjacencyEntry`s, which meant the
/// differential was checking my understanding of what the writer should do rather
/// than what it does — and it silently deduplicated a key's second version, hiding
/// the forced-seal constraint entirely. Driving `BlockWriter` means the agreement
/// is evidence about the code that will actually run.
///
/// `seal_after` names the step indices where the caller cuts a block. The writer
/// may seal MORE often than that — a key's second version forces one — and that is
/// the point: the oracle has no notion of blocks, so no cut may be observable.
fn build(
    history: &[(u64, Step)],
    seal_after: &[usize],
) -> (ReferenceGraph, Vec<Vec<AdjacencyEntry>>) {
    let mut graph = ReferenceGraph::new();
    let mut writer = BlockWriter::new(GRAPH, BRANCH, 0);

    for (index, (seq, step)) in history.iter().enumerate() {
        let row = match *step {
            Step::CreateVertex(vid) => DeltaRow::CreateVertex {
                vid: VId(vid),
                birth_ordinal: vid as u64,
                labels: vec![LABEL],
                props: vec![(PROP, fgdb_types::CanonicalScalar::Int(vid as i64))],
                valid_time: None,
            },
            Step::AddEdge { eid, src, dst } => DeltaRow::CreateEdge {
                eid: EId(eid),
                birth_ordinal: eid as u64,
                src: VId(src),
                relation: REL,
                dst: VId(dst),
                canonical_key: None,
                props: vec![],
                valid_time: None,
            },
            Step::DeleteVertex(vid) => {
                // THE CASCADE IMAGE IS READ FROM THE ORACLE, like the before-image
                // beside it: the materializer checks it for EQUALITY with the
                // actual incident set, so a fixture that guessed would be refused
                // — and one that guessed right by luck would be testing nothing.
                let before_version = graph
                    .element_version(fgdb_delta_types::ElementId::Vertex(VId(vid)))
                    .expect("a deletion names a live vertex");
                DeltaRow::DeleteVertex {
                    vid: VId(vid),
                    before_version,
                    sorted_retired_incident_edges: graph.incident_edges(VId(vid)),
                }
            }
            Step::DeleteEdge { eid, .. } => {
                // THE BEFORE-IMAGE IS READ FROM THE ORACLE, not invented. Its
                // delete refuses a version that disagrees with materialized state
                // — that check is the delta stream's self-verification and this
                // fixture has no business bypassing it with a placeholder.
                let before_version = graph
                    .element_version(fgdb_delta_types::ElementId::Edge(EId(eid)))
                    .expect("a delete names a live edge");
                DeltaRow::DeleteEdge {
                    eid: EId(eid),
                    before_version,
                }
            }
        };

        // ONE row, BOTH sides. Neither can see a history the other did not.
        graph.apply_row(&row).expect("the oracle accepts the row");
        writer
            .apply(KEYS, CommitSeq(*seq), &row)
            .expect("the writer accepts the row");

        if seal_after.contains(&index) {
            writer.seal(KEYS).expect("seals");
        }
    }

    let (_, sealed) = writer
        .publish(KEYS, CommitSeq(u64::MAX / 2))
        .expect("publishes");
    // Decoded from the sealed BYTES, so the differential runs against what the
    // writer actually wrote rather than against its in-memory intent.
    let blocks = sealed
        .iter()
        .map(|block| decode_block(&block.bytes).expect("a sealed block decodes"))
        .collect();
    (graph, blocks)
}

/// Sweep every sequence and every source, asserting the two sides agree.
///
/// `last` is the highest sequence the history reaches; the sweep runs one past it
/// so the tail is covered too.
fn assert_agrees(
    graph: &ReferenceGraph,
    blocks: &[Vec<AdjacencyEntry>],
    sources: &[u128],
    last: u64,
) {
    for source in sources {
        let expected = graph.neighbours(VId(*source), REL);
        for as_of in 1..=last + 1 {
            let actual = merge_neighbours(blocks, VId(*source), REL, CommitSeq(as_of))
                .expect("the merge must not report a corrupt history");
            if as_of >= last {
                // At and past the final sequence both sides describe the same
                // present, which is the only instant the oracle can speak about:
                // ReferenceGraph holds current state, not history.
                assert_eq!(
                    actual, expected,
                    "source {source} disagrees at {as_of}: strata {actual:?} vs oracle {expected:?}"
                );
            }
        }
    }
}

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
        (
            5,
            Step::DeleteEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
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
        (
            4,
            Step::DeleteEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
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
        (
            4,
            Step::DeleteEdge {
                eid: 10,
                src: 1,
                dst: 2,
            },
        ),
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
