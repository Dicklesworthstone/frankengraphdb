//! The shared Strata-versus-oracle differential harness.
//!
//! **WHY THIS IS A MODULE AND NOT A COPY.** Two test binaries now drive the same
//! differential — [`strata_oracle_differential`] with hand-written fixtures and
//! [`generated_histories`] with model-generated ones. If each carried its own
//! `build`, a fix to one copy would silently leave the other checking a weaker
//! property, and a differential that is weaker than it looks is worse than none:
//! it converts "we tested that" from a fact into a belief. One definition, two
//! callers.
//!
//! Everything here is deliberately about MECHANISM, never about which histories
//! are interesting. Choosing histories is the caller's job, and the two callers
//! choose very differently.

// Cargo compiles this module SEPARATELY INTO EACH test binary that declares it,
// so every item unused by *that* binary is dead code there — `build` is unused by
// the generator, `try_build` by the fixtures. The allow is scoped to this module
// and buys nothing anywhere else; the alternative is splitting the harness along
// a line drawn by which caller happens to use what, which is not a real seam.
#![allow(dead_code)]

use fgdb_delta_types::{DeltaRow, LabelId, PropertyKeyId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_strata::root::merge_neighbours;
use fgdb_strata::writer::BlockWriter;
use fgdb_strata::{AdjacencyEntry, decode_block};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};

pub const REL: RelationId = RelationId(1);
pub const GRAPH: GraphId = GraphId(1);
pub const BRANCH: BranchId = BranchId(1);
pub const K_OID: [u8; 32] = [0x5a; 32];
pub const KEYS: (&[u8; 32], DatabaseSecurityNamespaceId) =
    (&K_OID, DatabaseSecurityNamespaceId([0x77; 32]));
pub const LABEL: LabelId = LabelId(10);
pub const PROP: PropertyKeyId = PropertyKeyId(100);

/// One step of a history: what happened, and at which commit sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    CreateVertex(u128),
    AddEdge {
        eid: u128,
        src: u128,
        dst: u128,
    },
    DeleteEdge(u128),
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
/// than what it does — and it silently deduplicated a parallel edge whose topology
/// matched another EId. Driving `BlockWriter` means the agreement
/// is evidence about the code that will actually run.
///
/// `seal_after` names the step indices where the caller cuts a block. The writer
/// may also seal at its hard entry ceiling. The oracle has no notion of blocks, so
/// no legal cut may be observable.
pub fn build(
    history: &[(u64, Step)],
    seal_after: &[usize],
) -> (ReferenceGraph, Vec<Vec<AdjacencyEntry>>) {
    try_build(history, seal_after).expect("both sides accept the history")
}

/// The fallible form, for a caller that GENERATES histories rather than writing
/// them out.
///
/// A hand-written fixture that is rejected is a bug in the fixture and should
/// panic loudly, which is what [`build`] does. A generated history that is
/// rejected means the GENERATOR proposed something unreachable, and the generator
/// must be able to see that as a value rather than as a process death — otherwise
/// the only way to discover a generator defect is a stack trace, and shrinking
/// (which re-runs candidate histories constantly) becomes impossible.
pub fn try_build(
    history: &[(u64, Step)],
    seal_after: &[usize],
) -> Result<(ReferenceGraph, Vec<Vec<AdjacencyEntry>>), String> {
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
                    .ok_or_else(|| format!("step {index}: DeleteVertex names a dead vertex"))?;
                DeltaRow::DeleteVertex {
                    vid: VId(vid),
                    before_version,
                    sorted_retired_incident_edges: graph.incident_edges(VId(vid)),
                }
            }
            Step::DeleteEdge(eid) => {
                // THE BEFORE-IMAGE IS READ FROM THE ORACLE, not invented. Its
                // delete refuses a version that disagrees with materialized state
                // — that check is the delta stream's self-verification and this
                // fixture has no business bypassing it with a placeholder.
                let before_version = graph
                    .element_version(fgdb_delta_types::ElementId::Edge(EId(eid)))
                    .ok_or_else(|| format!("step {index}: DeleteEdge names a dead edge"))?;
                DeltaRow::DeleteEdge {
                    eid: EId(eid),
                    before_version,
                }
            }
        };

        // ONE row, BOTH sides. Neither can see a history the other did not.
        graph
            .apply_row(&row)
            .map_err(|e| format!("step {index}: the oracle refused the row: {e}"))?;
        writer
            .apply(KEYS, CommitSeq(*seq), &row)
            .map_err(|e| format!("step {index}: the writer refused the row: {e:?}"))?;

        if seal_after.contains(&index) {
            writer
                .seal(KEYS)
                .map_err(|e| format!("step {index}: seal failed: {e:?}"))?;
        }
    }

    let (_, sealed) = writer
        .publish(KEYS, CommitSeq(u64::MAX / 2))
        .map_err(|e| format!("publish failed: {e:?}"))?;
    // Decoded from the sealed BYTES, so the differential runs against what the
    // writer actually wrote rather than against its in-memory intent.
    let blocks = sealed
        .iter()
        .map(|block| decode_block(&block.bytes).expect("a sealed block decodes"))
        .collect();
    Ok((graph, blocks))
}

/// Sweep every sequence and every source, asserting the two sides agree.
///
/// `last` is the highest sequence the history reaches; the sweep runs one past it
/// so the tail is covered too.
pub fn assert_agrees(
    graph: &ReferenceGraph,
    blocks: &[Vec<AdjacencyEntry>],
    sources: &[u128],
    last: u64,
) {
    assert_eq!(check_agrees(graph, blocks, sources, last), Ok(()));
}

/// The fallible form of [`assert_agrees`], for the shrinker.
///
/// Shrinking re-runs a candidate history for every removal it tries, and asks a
/// question a panic cannot answer: *does this smaller history still fail?* Only a
/// returned value can be branched on.
pub fn check_agrees(
    graph: &ReferenceGraph,
    blocks: &[Vec<AdjacencyEntry>],
    sources: &[u128],
    last: u64,
) -> Result<(), String> {
    for source in sources {
        let expected = graph.neighbours(VId(*source), REL);
        for as_of in 1..=last + 1 {
            let actual = merge_neighbours(blocks, VId(*source), REL, CommitSeq(as_of))
                .map_err(|e| format!("the merge reported a corrupt history at {as_of}: {e:?}"))?;
            if as_of >= last {
                // At and past the final sequence both sides describe the same
                // present, which is the only instant the oracle can speak about:
                // ReferenceGraph holds current state, not history.
                if actual != expected {
                    return Err(format!(
                        "source {source} disagrees at {as_of}: strata {actual:?} vs oracle {expected:?}"
                    ));
                }
            }
        }
    }
    Ok(())
}
