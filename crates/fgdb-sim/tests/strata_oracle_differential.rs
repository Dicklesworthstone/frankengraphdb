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
use fgdb_strata::{AdjacencyEntry, decode_block, encode_block};
use fgdb_types::{CommitSeq, EId, VId};

const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);

/// One step of a history: what happened, and at which commit sequence.
#[derive(Clone, Copy, Debug)]
enum Step {
    CreateVertex(u128),
    AddEdge { eid: u128, src: u128, dst: u128 },
    DeleteEdge { eid: u128, src: u128, dst: u128 },
}

/// Build both sides from ONE history, so neither can be tuned to the other.
///
/// Returns the oracle's graph and the Strata blocks. The `split` argument says
/// where block boundaries fall, which is the whole point: the oracle has no notion
/// of blocks, so any split must produce the same answers.
fn build(history: &[(u64, Step)], split: &[usize]) -> (ReferenceGraph, Vec<Vec<AdjacencyEntry>>) {
    let mut graph = ReferenceGraph::new();
    let mut blocks: Vec<Vec<AdjacencyEntry>> = Vec::new();
    let mut pending: Vec<AdjacencyEntry> = Vec::new();
    // Live edges, so a delete can name the version it retires on the Strata side
    // and the before-image the oracle demands on its own.
    let mut live: Vec<(u128, u128, u128, u64)> = Vec::new();

    for (index, (seq, step)) in history.iter().enumerate() {
        match *step {
            Step::CreateVertex(vid) => {
                graph
                    .apply_row(&DeltaRow::CreateVertex {
                        vid: VId(vid),
                        birth_ordinal: vid as u64,
                        labels: vec![LABEL],
                        props: vec![(PROP, fgdb_types::CanonicalScalar::Int(vid as i64))],
                        valid_time: None,
                    })
                    .expect("vertex applies");
            }
            Step::AddEdge { eid, src, dst } => {
                graph
                    .apply_row(&DeltaRow::CreateEdge {
                        eid: EId(eid),
                        birth_ordinal: eid as u64,
                        src: VId(src),
                        relation: REL,
                        dst: VId(dst),
                        canonical_key: None,
                        props: vec![],
                        valid_time: None,
                    })
                    .expect("edge applies");
                let next = AdjacencyEntry {
                    src: VId(src),
                    relation: REL,
                    dst: VId(dst),
                    created_at: CommitSeq(*seq),
                    retired_at: None,
                };
                if would_collide(&pending, &next) {
                    blocks.push(seal(&mut pending));
                }
                pending.push(next);
                live.push((eid, src, dst, *seq));
            }
            Step::DeleteEdge { eid, src, dst } => {
                // THE BEFORE-IMAGE IS READ FROM THE ORACLE, not invented. Its
                // delete refuses a version that disagrees with materialized state
                // — that check is the delta stream's self-verification and this
                // fixture has no business bypassing it with a placeholder.
                let before_version = graph
                    .element_version(fgdb_delta_types::ElementId::Edge(EId(eid)))
                    .expect("a delete names a live edge");
                graph
                    .apply_row(&DeltaRow::DeleteEdge {
                        eid: EId(eid),
                        before_version,
                    })
                    .expect("delete applies");
                let created = live
                    .iter()
                    .find(|(e, _, _, _)| *e == eid)
                    .map(|(_, _, _, c)| *c)
                    .expect("a delete names a live edge");
                // THE TOMBSTONE: a later block restates the version it retires.
                // The oracle needs no such record — it simply removes the edge —
                // which is the asymmetry this differential exists to check.
                let next = AdjacencyEntry {
                    src: VId(src),
                    relation: REL,
                    dst: VId(dst),
                    created_at: CommitSeq(created),
                    retired_at: Some(CommitSeq(*seq)),
                };
                if would_collide(&pending, &next) {
                    blocks.push(seal(&mut pending));
                }
                pending.push(next);
                live.retain(|(e, _, _, _)| *e != eid);
            }
        }
        if split.contains(&index) {
            blocks.push(seal(&mut pending));
        }
    }
    if !pending.is_empty() {
        blocks.push(seal(&mut pending));
    }
    (graph, blocks)
}

/// Canonicalize a pending run into a block, through the real encoder.
///
/// Encoded and decoded rather than handed over as a `Vec`, so the differential runs
/// against bytes that actually round-tripped — if the format could not carry the
/// history, that must surface here rather than being bypassed.
///
/// It does NOT deduplicate. An earlier version did, and that quietly hid the
/// constraint below: two versions of one key silently became one, and the fixture
/// disagreed with itself depending on where blocks were cut.
fn seal(pending: &mut Vec<AdjacencyEntry>) -> Vec<AdjacencyEntry> {
    pending.sort_by_key(|e| (e.src, e.relation, e.dst));
    let bytes = encode_block(pending).expect("the history must be encodable");
    pending.clear();
    decode_block(&bytes).expect("and decodable")
}

/// **A KEY'S SECOND VERSION FORCES A BLOCK BOUNDARY**, and the differential is
/// what found it.
///
/// A block requires strictly ascending unique `(src, relation, dst)` keys, because
/// two entries for one key inside one block would leave it unable to say which is
/// current — that is a merge, and a block does not merge. So a writer that retires
/// a key and re-creates it CANNOT put both versions in the same block: it must seal
/// first. That is a real constraint the format imposes on the writer, discovered
/// here rather than declared, and a fixture that deduplicated instead would have
/// hidden it while producing plausible answers.
fn would_collide(pending: &[AdjacencyEntry], next: &AdjacencyEntry) -> bool {
    pending
        .iter()
        .any(|e| (e.src, e.relation, e.dst) == (next.src, next.relation, next.dst))
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
