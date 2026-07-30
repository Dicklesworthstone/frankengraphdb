//! Laws of the stream frontier — which commit sequences a history may use.
//!
//! `apply_template` used to compare a sequence only against each coordinate it
//! touched. That is sound for a coordinate with history and vacuous for one
//! without: a fresh coordinate has no `applied_through` to be compared with, so it
//! accepted ANY sequence. Gapped, globally reversed and zero-sequence histories
//! were all admitted, while a comment two lines up asserted the stream was
//! "gap-free and monotone by construction" (fgdb-reference-global-commit-frontier-pjqu).
//!
//! **WHY EXACT-NEXT AND NOT MERELY INCREASING.** The durable layer is stricter than
//! monotone: Chronicle's `MarkerChain` starts at 1 and demands the exact successor,
//! and `LocalDeltaBatchIndex` keeps one global frontier that rejects gaps and
//! duplicates alike. A gapped history therefore cannot come from the commit
//! stream — so an oracle that admits one can no longer REJECT, which is the
//! specific way an oracle stops being worth having. It is not there to agree with
//! the engine; it is there to disagree when the engine is wrong.
//!
//! **TWO FRONTIERS, TWO QUESTIONS.** The per-coordinate map stays: intervening
//! commits can touch other coordinates, so a branch's own frontier is genuinely
//! below the stream's, and both the fork boundary and the conflict window are
//! derived from the per-coordinate value. "What has this branch seen" and "where is
//! the stream" are different facts and neither substitutes for the other. The last
//! law here pins that they can differ.
//!
//! **WHAT WOULD MAKE THESE VACUOUS.** A rule that refused everything would pass
//! every refusal law in this file, so two of the laws are controls: an ordinary
//! sequential history is admitted, and one template may carry several coordinates
//! at ONE sequence — which is the whole reason a per-coordinate check existed in
//! the first place.

use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_reference::{ApplyError, ReferenceDatabase};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, GraphId, ObjectId, VId};

const GRAPH: GraphId = GraphId(1);
const A: BranchId = BranchId(1);
const B: BranchId = BranchId(2);
const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);

fn vertex(vid: u128) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        labels: vec![LABEL],
        props: vec![(PROP, CanonicalScalar::Int(vid as i64))],
        valid_time: None,
    }
}

fn entry(branch: BranchId, rows: Vec<DeltaRow>) -> CoordinateEntry {
    CoordinateEntry {
        graph: GRAPH,
        branch,
        relation: REL,
        schema_epoch: SchemaEpoch(0),
        schema_transition: None,
        rows,
    }
}

fn template(entries: Vec<CoordinateEntry>) -> LogicalDeltaTemplate {
    LogicalDeltaTemplate::build(ObjectId([0x11; 32]), [0x22; 32], entries).expect("template builds")
}

fn one(branch: BranchId, vid: u128) -> LogicalDeltaTemplate {
    template(vec![entry(branch, vec![vertex(vid)])])
}

/// A globally REVERSED pair: sequence 2 to one coordinate, then sequence 1 to a
/// coordinate that has never been written.
///
/// The reported case, and the one a per-coordinate rule cannot see at all: B has no
/// `applied_through`, so there was nothing for sequence 1 to fail against.
#[test]
fn a_globally_reversed_sequence_is_refused() {
    let mut db = ReferenceDatabase::new();
    // Even the FIRST commit must be the stream's next one, which is 1.
    assert_eq!(
        db.apply_template(&one(A, 1), CommitSeq(2)),
        Err(ApplyError::SequenceNotNext {
            expected: CommitSeq(1),
            offered: CommitSeq(2),
        })
    );

    db.apply_template(&one(A, 1), CommitSeq(1))
        .expect("applies");
    db.apply_template(&one(A, 2), CommitSeq(2))
        .expect("applies");
    assert_eq!(
        db.apply_template(&one(B, 3), CommitSeq(1)),
        Err(ApplyError::SequenceNotNext {
            expected: CommitSeq(3),
            offered: CommitSeq(1),
        }),
        "a fresh coordinate does not get to pick its own sequence"
    );
}

/// A GAP is refused, on a fresh coordinate and on one with history alike.
///
/// The forward direction the old per-coordinate rule could not express: sequence 5
/// against a coordinate at 1 strictly advances, so "does not advance" had nothing
/// to say about it.
#[test]
fn a_gap_is_refused() {
    let mut db = ReferenceDatabase::new();
    db.apply_template(&one(A, 1), CommitSeq(1))
        .expect("applies");

    for (branch, vid) in [(A, 10u128), (B, 11)] {
        assert_eq!(
            db.apply_template(&one(branch, vid), CommitSeq(5)),
            Err(ApplyError::SequenceNotNext {
                expected: CommitSeq(2),
                offered: CommitSeq(5),
            })
        );
    }
}

/// Sequence ZERO is refused. Chronicle's chain starts at 1, so zero names no
/// commit — it is the frontier of a database that has applied nothing.
#[test]
fn sequence_zero_is_refused() {
    let mut db = ReferenceDatabase::new();
    assert_eq!(db.replay_frontier(), CommitSeq(0));
    assert_eq!(
        db.apply_template(&one(A, 1), CommitSeq(0)),
        Err(ApplyError::SequenceNotNext {
            expected: CommitSeq(1),
            offered: CommitSeq(0),
        })
    );
}

/// An EMPTY template is refused rather than treated as a successful no-op.
///
/// It used to apply "successfully" while recording nothing: no sequence consumed,
/// no trace left — a commit that happened for no reason. That is exactly what the
/// write-path laws in `fgdb-sim` forbid one layer up ("not an empty capsule, not a
/// marker with no effects — nothing at all"), so admitting it here would let the
/// oracle bless a stream the engine must never produce.
#[test]
fn an_empty_template_is_refused() {
    let mut db = ReferenceDatabase::new();
    let empty = template(vec![]);
    assert_eq!(
        db.apply_template(&empty, CommitSeq(1)),
        Err(ApplyError::EmptyTemplate)
    );
    assert_eq!(
        db.replay_frontier(),
        CommitSeq(0),
        "and it consumed nothing"
    );
    assert_eq!(db.coordinate_count(), 0);

    // Refused at a legal sequence too, so the refusal is about emptiness and not
    // about the sequence.
    db.apply_template(&one(A, 1), CommitSeq(1))
        .expect("applies");
    assert_eq!(
        db.apply_template(&empty, CommitSeq(2)),
        Err(ApplyError::EmptyTemplate)
    );
}

/// CONTROL: an ordinary sequential history is admitted, across several
/// coordinates. Without this a rule that refused everything would pass every
/// refusal law above.
#[test]
fn a_sequential_history_across_coordinates_is_admitted() {
    let mut db = ReferenceDatabase::new();
    for (seq, branch, vid) in [(1u64, A, 1u128), (2, B, 2), (3, A, 3), (4, B, 4), (5, A, 5)] {
        db.apply_template(&one(branch, vid), CommitSeq(seq))
            .expect("a sequential history applies");
    }
    assert_eq!(db.replay_frontier(), CommitSeq(5));
    assert_eq!(db.coordinate_count(), 2);
    assert_eq!(db.applied_through(GRAPH, A), Some(CommitSeq(5)));
    assert_eq!(db.applied_through(GRAPH, B), Some(CommitSeq(4)));
}

/// CONTROL: ONE template may carry SEVERAL coordinates at one sequence.
///
/// This is the case the per-coordinate check exists for, and the one a naive
/// "one commit, one coordinate" frontier would break. A commit is a template, not
/// a coordinate — plan:397's batch inserts every coordinate it names in a single
/// transition.
#[test]
fn one_template_may_carry_several_coordinates_at_one_sequence() {
    let mut db = ReferenceDatabase::new();
    let both = template(vec![entry(A, vec![vertex(1)]), entry(B, vec![vertex(2)])]);
    db.apply_template(&both, CommitSeq(1)).expect("applies");

    assert_eq!(db.replay_frontier(), CommitSeq(1));
    assert_eq!(db.applied_through(GRAPH, A), Some(CommitSeq(1)));
    assert_eq!(
        db.applied_through(GRAPH, B),
        Some(CommitSeq(1)),
        "both coordinates advanced to the same sequence"
    );
    assert_eq!(db.coordinate_count(), 2);
}

/// A refusal moves NOTHING: not the stream frontier, not any coordinate, not the
/// history.
#[test]
fn a_refused_template_moves_neither_frontier() {
    let mut db = ReferenceDatabase::new();
    db.apply_template(&one(A, 1), CommitSeq(1))
        .expect("applies");
    let settled = db.clone();

    for (seq, kind) in [(0u64, "zero"), (1, "duplicate"), (7, "gap")] {
        let result = db.apply_template(&one(A, 90 + seq as u128), CommitSeq(seq));
        assert!(result.is_err(), "{kind} must be refused");
        assert_eq!(db, settled, "{kind} changed the database");
    }
    assert_eq!(db.replay_frontier(), CommitSeq(1));
    assert_eq!(db.recorded_commits(GRAPH, A), 1);
}

/// THE TWO FRONTIERS DIFFER, and both are needed.
///
/// A branch's own frontier sits below the stream's whenever another coordinate
/// committed in between. That gap is why the per-coordinate map cannot be replaced
/// by the stream frontier: the conflict window and the fork boundary are both
/// derived from what a BRANCH has seen, not from where the stream is.
#[test]
fn a_coordinates_frontier_sits_below_the_streams() {
    let mut db = ReferenceDatabase::new();
    db.apply_template(&one(A, 1), CommitSeq(1))
        .expect("applies");
    db.apply_template(&one(B, 2), CommitSeq(2))
        .expect("applies");
    db.apply_template(&one(B, 3), CommitSeq(3))
        .expect("applies");

    assert_eq!(db.replay_frontier(), CommitSeq(3));
    assert_eq!(
        db.applied_through(GRAPH, A),
        Some(CommitSeq(1)),
        "A has seen only its own commit, two behind the stream"
    );
    // And a fork from A records A's frontier, not the stream's — the value the
    // lineage cap depends on.
    let mut forked = db.clone();
    forked.fork_branch(GRAPH, A, BranchId(9)).expect("forks");
    assert_eq!(
        forked.applied_through(GRAPH, BranchId(9)),
        Some(CommitSeq(1)),
        "the fork boundary is the parent's frontier, not the stream's"
    );
}
