//! Laws of snapshot visibility — `FOR SYSTEM_TIME AS OF <sequence>`, and the
//! §15 SI oracle it makes writable.
//!
//! **WHY THIS FILE EXISTS.** Until now the oracle could say how far a branch had
//! been advanced but not what it looked like earlier, so §15's SI oracle ("no
//! read sees `seq > snapshot.high`") had no subject: with one mutable state per
//! coordinate, every read trivially sees the latest sequence and the assertion
//! is unwritable rather than satisfied. B1 claims MVCC versions, time-travel,
//! replication and branches are *the same mechanism* — an append-only commit
//! stream — and a historical read is the fold of that stream truncated at a
//! sequence. These laws hold the implementation to that claim.
//!
//! **THE TWO DIMENSIONS ARE INDEPENDENT.** System time (which commits we have
//! folded) and valid time (which facts the folded state says were true when) are
//! orthogonal, and `read` returns a whole `ReferenceGraph` precisely so they
//! compose without a bitemporal variant of every accessor. The last law here
//! asks the same valid-time question against two system-time bases and gets
//! different answers, which is the bitemporal distinction — what we believed at
//! S about what was true at T.
//!
//! **WHAT WOULD MAKE THESE VACUOUS.** A read that silently clamped an
//! out-of-range sequence to the frontier would satisfy every "as of" law by
//! measuring the present, so the out-of-range read is a refusal. And a fold that
//! filtered by one global `seq <= high` instead of capping each ancestor at its
//! own fork boundary would pass every single-branch law here and leak the
//! parent's post-fork commits into the child; the two branch laws are shaped to
//! discriminate exactly that.

use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, ElementId, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch, ValidTimePeriod,
};
use fgdb_reference::{ApplyError, ReferenceDatabase, ReferenceGraph, SnapshotError};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, ObjectId, VId};

const GRAPH: GraphId = GraphId(1);
const MAIN: BranchId = BranchId(1);
const FEATURE: BranchId = BranchId(2);
const NESTED: BranchId = BranchId(3);
const ABSENT: BranchId = BranchId(99);
const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const OTHER_LABEL: LabelId = LabelId(11);
const PROP: PropertyKeyId = PropertyKeyId(100);

fn period(start: i64, end: Option<i64>) -> ValidTimePeriod {
    ValidTimePeriod {
        start_micros: start,
        end_micros: end,
    }
}

fn vertex(vid: u128) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        labels: vec![LABEL],
        props: vec![(PROP, CanonicalScalar::Int(vid as i64))],
        valid_time: None,
    }
}

fn timed_vertex(vid: u128, valid: ValidTimePeriod) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        labels: vec![LABEL],
        props: vec![(PROP, CanonicalScalar::Int(vid as i64))],
        valid_time: Some(valid),
    }
}

fn edge(eid: u128, src: u128, dst: u128) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: eid as u64,
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}

fn set_prop(vid: u128, before: Option<i64>, after: Option<i64>) -> DeltaRow {
    DeltaRow::Property {
        elem: ElementId::Vertex(VId(vid)),
        property: PROP,
        before: before.map(CanonicalScalar::Int),
        after: after.map(CanonicalScalar::Int),
    }
}

fn template(branch: BranchId, rows: Vec<DeltaRow>) -> LogicalDeltaTemplate {
    LogicalDeltaTemplate::build(
        ObjectId([0x11; 32]),
        [0x22; 32],
        vec![CoordinateEntry {
            graph: GRAPH,
            branch,
            relation: REL,
            schema_epoch: SchemaEpoch(0),
            schema_transition: None,
            rows,
        }],
    )
    .expect("template builds")
}

fn apply_at(db: &mut ReferenceDatabase, branch: BranchId, seq: u64, rows: Vec<DeltaRow>) {
    db.apply_template(&template(branch, rows), CommitSeq(seq))
        .expect("applies");
}

/// Read a coordinate as of a sequence. `expect` rather than a `panic!` arm: the
/// error's `Debug` carries the refusal, and a `panic!` in a test file moves the
/// workspace's UBS panic class.
fn read_at(db: &ReferenceDatabase, branch: BranchId, seq: u64) -> ReferenceGraph {
    let snapshot = db
        .snapshot_at(GRAPH, branch, CommitSeq(seq))
        .expect("snapshot should mint at or below the frontier");
    db.read(&snapshot).expect("a minted snapshot should read")
}

/// Which of the vertex ids these laws use are present, ascending.
fn vids(graph: &ReferenceGraph) -> Vec<u128> {
    (1..=32u128)
        .filter(|vid| graph.vertex(VId(*vid)).is_some())
        .collect()
}

fn prop_of(graph: &ReferenceGraph, vid: u128) -> Option<i64> {
    match graph.vertex(VId(vid))?.props.get(&PROP)? {
        CanonicalScalar::Int(value) => Some(*value),
        _ => None,
    }
}

/// A single branch with a rich stream: creates, edges, a property overwrite, a
/// label flip, deletes, and a valid-time change. Frontier is sequence 7.
fn rich_history() -> ReferenceDatabase {
    let mut db = ReferenceDatabase::new();
    apply_at(&mut db, MAIN, 1, vec![vertex(1), vertex(2), vertex(3)]);
    apply_at(&mut db, MAIN, 2, vec![edge(11, 1, 2), edge(12, 2, 3)]);
    apply_at(&mut db, MAIN, 3, vec![set_prop(1, Some(1), Some(1_000))]);
    apply_at(
        &mut db,
        MAIN,
        4,
        vec![DeltaRow::LabelMembership {
            vid: VId(2),
            label: OTHER_LABEL,
            before: false,
            after: true,
        }],
    );
    let edge_version = db
        .graph(GRAPH, MAIN)
        .expect("main exists")
        .edge(EId(11))
        .expect("edge exists")
        .version;
    apply_at(
        &mut db,
        MAIN,
        5,
        vec![DeltaRow::DeleteEdge {
            eid: EId(11),
            before_version: edge_version,
        }],
    );
    let vertex_version = db
        .graph(GRAPH, MAIN)
        .expect("main exists")
        .vertex(VId(1))
        .expect("vertex exists")
        .version;
    apply_at(
        &mut db,
        MAIN,
        6,
        vec![DeltaRow::DeleteVertex {
            vid: VId(1),
            before_version: vertex_version,
            sorted_retired_incident_edges: vec![],
        }],
    );
    apply_at(
        &mut db,
        MAIN,
        7,
        vec![DeltaRow::ValidTime {
            elem: ElementId::Vertex(VId(2)),
            contract_id: ObjectId([0x55; 32]),
            before: None,
            after: Some(period(100, Some(200))),
        }],
    );
    db
}

/// THE FAITHFULNESS LAW, and the load-bearing one in this file.
///
/// A snapshot at the frontier must reconstruct the live graph exactly — every
/// vertex, edge, property, label, valid-time period, counter and applied
/// operation key. It says the recorded stream and the materialized state are two
/// views of one fact rather than two facts kept in step by hand. Every other law
/// here reads a *derived* state; if the fold and the materializer could disagree
/// at the frontier, none of those readings would mean anything.
#[test]
fn a_snapshot_at_the_frontier_reconstructs_the_live_graph() {
    let db = rich_history();
    let live = db.graph(GRAPH, MAIN).expect("main exists");
    let folded = read_at(&db, MAIN, 7);
    assert_eq!(
        &folded, live,
        "folding the recorded stream must reproduce the materialized state"
    );
}

/// The `snapshot()` position is read-your-own-writes: the frontier, not behind it.
#[test]
fn a_snapshot_taken_now_sees_every_commit_so_far() {
    let db = rich_history();
    let snapshot = db.snapshot(GRAPH, MAIN).expect("mints");
    assert_eq!(snapshot.high(), CommitSeq(7));
    assert_eq!(
        Some(&db.read(&snapshot).expect("reads")),
        db.graph(GRAPH, MAIN)
    );
}

/// THE SI ORACLE LAW: no read sees a sequence above the snapshot.
#[test]
fn a_snapshot_does_not_see_commits_above_its_high() {
    let mut db = rich_history();
    let snapshot = db.snapshot(GRAPH, MAIN).expect("mints");

    apply_at(&mut db, MAIN, 8, vec![vertex(20)]);
    apply_at(&mut db, MAIN, 9, vec![vertex(21)]);

    let observed = db.read(&snapshot).expect("reads");
    assert!(
        observed.vertex(VId(20)).is_none() && observed.vertex(VId(21)).is_none(),
        "a snapshot at {:?} observed a later commit: {:?}",
        snapshot.high(),
        vids(&observed)
    );
    assert_eq!(observed.vertex_count(), 2, "vertices 2 and 3 survive to 7");
    assert!(
        db.graph(GRAPH, MAIN)
            .expect("main")
            .vertex(VId(21))
            .is_some(),
        "the live graph must have advanced — otherwise this law is vacuous"
    );
}

/// The same snapshot re-read after arbitrary later commits yields an identical
/// graph. Stability is the property a reader actually depends on: a snapshot
/// that drifted would let two reads inside one transaction disagree.
#[test]
fn a_snapshot_is_stable_across_later_commits() {
    let mut db = rich_history();
    let snapshot = db.snapshot(GRAPH, MAIN).expect("mints");
    let first = db.read(&snapshot).expect("reads");

    for (offset, vid) in (0..6u64).zip(30..36u128) {
        apply_at(&mut db, MAIN, 8 + offset, vec![vertex(vid)]);
    }

    assert_eq!(
        first,
        db.read(&snapshot).expect("re-reads"),
        "six later commits changed what an earlier snapshot observes"
    );
}

/// TIME TRAVEL PROPER: an overwritten value is still readable at the sequence
/// where it held. This is the law that a materializer without history cannot
/// pass at all — it would answer 1000 at every sequence.
#[test]
fn an_overwritten_property_is_readable_at_the_earlier_sequence() {
    let db = rich_history();
    assert_eq!(prop_of(&read_at(&db, MAIN, 2), 1), Some(1));
    assert_eq!(prop_of(&read_at(&db, MAIN, 3), 1), Some(1_000));
}

/// A deleted element is absent from the present and present in the past. In an
/// append-only stream deletion is a record, not an erasure.
#[test]
fn a_deleted_vertex_is_still_there_before_its_deletion() {
    let db = rich_history();
    let before = read_at(&db, MAIN, 5);
    let after = read_at(&db, MAIN, 6);
    assert!(before.vertex(VId(1)).is_some(), "v1 lives through 5");
    assert!(after.vertex(VId(1)).is_none(), "v1 is retired at 6");
    assert!(
        before.edge(EId(11)).is_none() && read_at(&db, MAIN, 4).edge(EId(11)).is_some(),
        "the edge deleted at 5 must be visible at 4 and gone at 5"
    );
}

/// Sequence zero names the state before the coordinate's first commit.
#[test]
fn a_snapshot_at_zero_is_empty() {
    let db = rich_history();
    let empty = read_at(&db, MAIN, 0);
    assert_eq!(empty.vertex_count(), 0);
    assert_eq!(empty.edge_count(), 0);
    assert_eq!(empty, ReferenceGraph::new());
}

/// Reading above the frontier is REFUSED, not clamped. Clamping is the
/// dangerous alternative: it answers with the present while the caller believes
/// it received a historical state, which makes every "as of a later sequence"
/// assertion pass for the wrong reason.
#[test]
fn a_read_of_the_future_is_refused() {
    let db = rich_history();
    assert_eq!(
        db.snapshot_at(GRAPH, MAIN, CommitSeq(8)),
        Err(SnapshotError::BeyondFrontier {
            graph: GRAPH,
            branch: MAIN,
            applied_through: CommitSeq(7),
            requested: CommitSeq(8),
        })
    );
    assert!(
        db.snapshot_at(GRAPH, MAIN, CommitSeq(7)).is_ok(),
        "the frontier itself is readable"
    );
}

/// A coordinate that does not exist is distinguishable from one that is empty.
#[test]
fn a_snapshot_of_a_nonexistent_coordinate_is_refused() {
    let db = rich_history();
    let expected = Err(SnapshotError::NoSuchCoordinate {
        graph: GRAPH,
        branch: ABSENT,
    });
    assert_eq!(db.snapshot(GRAPH, ABSENT), expected);
    assert_eq!(db.snapshot_at(GRAPH, ABSENT, CommitSeq(0)), expected);
}

/// A refused template must leave nothing in the stream.
///
/// `apply_template` is all-or-nothing over the materialized state; the history
/// has to be inside that same guarantee. If a rejected template were recorded,
/// the frontier fold would apply effects that were never committed and the
/// faithfulness law would be the only thing standing between that and a silent
/// wrong answer — at a *later* sequence than the one that caused it.
#[test]
fn a_refused_template_leaves_no_trace_in_the_history() {
    let mut db = rich_history();
    let recorded = db.recorded_commits(GRAPH, MAIN);
    let before = db.graph(GRAPH, MAIN).cloned().expect("main exists");

    // v2's property is 2; declaring 999 as the before-image is a refusal.
    let refused = db.apply_template(
        &template(MAIN, vec![set_prop(2, Some(999), Some(0))]),
        CommitSeq(8),
    );
    assert!(matches!(
        refused,
        Err(ApplyError::PropertyBeforeMismatch { .. })
    ));

    assert_eq!(db.recorded_commits(GRAPH, MAIN), recorded, "stream grew");
    assert_eq!(db.applied_through(GRAPH, MAIN), Some(CommitSeq(7)));
    assert_eq!(db.graph(GRAPH, MAIN), Some(&before));
    assert_eq!(
        read_at(&db, MAIN, 7),
        before,
        "the frontier fold must still reproduce the live graph"
    );
}

/// main:1,2 → fork feature → feature:3, main:4, feature:5 → fork nested →
/// nested:6, feature:7, nested:8.
///
/// Every ancestor therefore has a commit ABOVE its fork boundary and BELOW the
/// child's frontier, which is what makes the per-ancestor cap observable: a
/// single global `seq <= high` filter would leak v4 and v7 into nested.
fn forked_history() -> ReferenceDatabase {
    let mut db = ReferenceDatabase::new();
    apply_at(&mut db, MAIN, 1, vec![vertex(1)]);
    apply_at(&mut db, MAIN, 2, vec![vertex(2)]);
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("forks");
    apply_at(&mut db, FEATURE, 3, vec![vertex(3)]);
    apply_at(&mut db, MAIN, 4, vec![vertex(4)]);
    apply_at(&mut db, FEATURE, 5, vec![vertex(5)]);
    db.fork_branch(GRAPH, FEATURE, NESTED).expect("forks");
    apply_at(&mut db, NESTED, 6, vec![vertex(6)]);
    apply_at(&mut db, FEATURE, 7, vec![vertex(7)]);
    apply_at(&mut db, NESTED, 8, vec![vertex(8)]);
    db
}

/// A child's read before its fork boundary IS its parent's read at that
/// sequence. The child inherited the parent's history, so the two are the same
/// question — the git semantics, and B1's claim that a branch is not a separate
/// history but a continuation of one.
#[test]
fn a_child_read_before_the_fork_equals_the_parent_read() {
    let db = forked_history();
    for seq in 0..=2 {
        assert_eq!(
            read_at(&db, FEATURE, seq),
            read_at(&db, MAIN, seq),
            "feature and main must agree at sequence {seq}, at or below the fork boundary"
        );
    }
    assert_eq!(vids(&read_at(&db, FEATURE, 2)), vec![1, 2]);
}

/// A child must not inherit the parent's post-fork commits, at any sequence.
///
/// THE DISCRIMINATOR for the per-ancestor cap: v4 landed on main at sequence 4,
/// above the fork boundary of 2 and below feature's frontier of 7. A fold that
/// filtered by one global bound would include it.
#[test]
fn a_child_never_sees_the_parents_post_fork_commits() {
    let db = forked_history();
    assert_eq!(vids(&read_at(&db, FEATURE, 5)), vec![1, 2, 3, 5]);
    assert_eq!(vids(&read_at(&db, FEATURE, 7)), vec![1, 2, 3, 5, 7]);
    assert!(
        read_at(&db, MAIN, 4).vertex(VId(4)).is_some(),
        "v4 must exist on main — otherwise its absence on feature proves nothing"
    );
}

/// The parent's own history is unaffected by the child's commits, at every
/// sequence including ones the child wrote at. The direction a
/// structurally-shared implementation breaks first.
#[test]
fn a_parent_never_sees_the_childs_commits() {
    let db = forked_history();
    assert_eq!(vids(&read_at(&db, MAIN, 4)), vec![1, 2, 4]);
    assert_eq!(
        db.applied_through(GRAPH, MAIN),
        Some(CommitSeq(4)),
        "main's frontier must not be advanced by its children"
    );
}

/// Each ancestor is capped by its OWN boundary, not by the nearest one.
///
/// Reading nested at 8 must exclude v4 (main, above main→feature's boundary of
/// 2) and v7 (feature, above feature→nested's boundary of 5). Two different
/// caps, two different ancestors, one read.
#[test]
fn every_ancestor_is_capped_by_its_own_fork_boundary() {
    let db = forked_history();
    assert_eq!(vids(&read_at(&db, NESTED, 8)), vec![1, 2, 3, 5, 6, 8]);
    assert_eq!(vids(&read_at(&db, NESTED, 3)), vec![1, 2, 3]);
    assert_eq!(vids(&read_at(&db, NESTED, 2)), vec![1, 2]);
    assert!(
        read_at(&db, FEATURE, 7).vertex(VId(7)).is_some(),
        "v7 must exist on feature — otherwise its absence on nested proves nothing"
    );
}

/// A fork shares history by link rather than copying it.
///
/// An assertion about the MECHANISM, not the answers: the child owns zero
/// records while holding its parent's entire inherited state, so time-travel
/// reads across a fork cost one metadata row. This is the one dimension where
/// this crate matches plan:451's O(1) branch creation instead of merely matching
/// its semantics — worth pinning so a later "simplification" to copying the
/// parent's stream is visible as a change.
#[test]
fn a_fork_shares_history_rather_than_copying_it() {
    let mut db = ReferenceDatabase::new();
    apply_at(&mut db, MAIN, 1, vec![vertex(1)]);
    apply_at(&mut db, MAIN, 2, vec![vertex(2)]);
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("forks");

    assert_eq!(db.recorded_commits(GRAPH, MAIN), 2);
    assert_eq!(
        db.recorded_commits(GRAPH, FEATURE),
        0,
        "the fork copied the parent's stream"
    );
    assert_eq!(
        db.graph(GRAPH, FEATURE).expect("feature").vertex_count(),
        2,
        "while inheriting all of its state"
    );

    apply_at(&mut db, FEATURE, 3, vec![vertex(3)]);
    assert_eq!(db.recorded_commits(GRAPH, FEATURE), 1);
    assert_eq!(db.recorded_commits(GRAPH, MAIN), 2, "unchanged");
}

/// BITEMPORAL: system time and valid time are independent.
///
/// The same valid-time question — "was v1 live at instant 300?" — gets different
/// answers depending on which commits we have folded, because sequence 2 revised
/// the period. That is the distinction between *when a fact was true* and *when
/// the database was told*, and it works here without a bitemporal accessor
/// because `read` hands back a whole graph that the ordinary valid-time
/// selectors then interrogate.
#[test]
fn system_time_and_valid_time_are_independent_dimensions() {
    let mut db = ReferenceDatabase::new();
    apply_at(
        &mut db,
        MAIN,
        1,
        vec![timed_vertex(1, period(100, Some(200)))],
    );
    apply_at(
        &mut db,
        MAIN,
        2,
        vec![DeltaRow::ValidTime {
            elem: ElementId::Vertex(VId(1)),
            contract_id: ObjectId([0x55; 32]),
            before: Some(period(100, Some(200))),
            after: Some(period(100, Some(500))),
        }],
    );

    assert!(
        !read_at(&db, MAIN, 1).vertex_live_at(VId(1), 300),
        "as we understood it at sequence 1, v1 had already ended by 300"
    );
    assert!(
        read_at(&db, MAIN, 2).vertex_live_at(VId(1), 300),
        "sequence 2 revised the period, so the same instant is now covered"
    );
    // The revision is about valid time only: the vertex exists in both folds.
    assert!(
        read_at(&db, MAIN, 1).vertex(VId(1)).is_some()
            && read_at(&db, MAIN, 2).vertex(VId(1)).is_some()
    );
    assert!(
        read_at(&db, MAIN, 1).vertex_live_at(VId(1), 150)
            && read_at(&db, MAIN, 2).vertex_live_at(VId(1), 150),
        "an instant inside both periods must be live under both bases"
    );
}
