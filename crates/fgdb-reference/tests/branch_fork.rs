//! Laws of branch forking — B1's git-style branches and B6's per-agent isolation.
//!
//! These pin the SEMANTICS the real engine must match, not the mechanism this
//! crate uses to get them. `fork_branch` materializes the parent's recorded
//! stream at the boundary, which is O(n); plan:451
//! requires the engine to add "only metadata and key wraps" for O(1) creation
//! and to have "reads select the branch head and follow explicit branch-parent
//! links atop structurally shared objects". Identical observable behaviour, and
//! a mechanism the engine may not use. Nothing here is evidence about fork cost.
//!
//! What the laws below actually constrain is the part that is easy to get wrong
//! in either implementation: **isolation is bidirectional**. A child must not
//! see the parent's post-fork writes, and the parent must not see the child's —
//! and the second direction is the one a structurally-shared implementation
//! breaks first, because the child's writes land in shared objects.

use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_reference::{BranchError, BranchOrigin, ReferenceDatabase};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, ObjectId, VId};

const GRAPH: GraphId = GraphId(1);
const MAIN: BranchId = BranchId(1);
const FEATURE: BranchId = BranchId(2);
const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::Text(fgdb_types::CanonicalText::new_ucs_basic(value).expect("bounded text"))
}

fn vertex(vid: u128, name: &str) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        labels: vec![LABEL],
        props: vec![(PROP, text(name))],
        valid_time: None,
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

/// Apply `rows` to one coordinate at `seq`.
///
/// The sequence is explicit at every call site because `apply_template` now
/// requires it and refuses one that does not advance — history is append-only.
fn apply_at(db: &mut ReferenceDatabase, branch: BranchId, seq: u64, rows: Vec<DeltaRow>) {
    let template = LogicalDeltaTemplate::build(
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
    .expect("template builds");
    db.apply_template(&template, CommitSeq(seq))
        .expect("applies");
}

fn name_on(db: &ReferenceDatabase, branch: BranchId, vid: u128) -> Option<CanonicalScalar> {
    db.graph(GRAPH, branch)?
        .vertex(VId(vid))?
        .props
        .get(&PROP)
        .cloned()
}

/// main with two vertices and an edge, ready to fork.
fn seeded() -> ReferenceDatabase {
    let mut db = ReferenceDatabase::new();
    apply_at(
        &mut db,
        MAIN,
        1,
        vec![vertex(1, "ada"), vertex(2, "grace"), edge(10, 1, 2)],
    );
    db
}

// ---------------------------------------------------------------------------
// Origin is a closed union
// ---------------------------------------------------------------------------

/// A coordinate that received writes without being forked is Genesis, and
/// Genesis carries no parent — structurally, since the variant has no fields.
/// plan:2000: "The Genesis tag forbids parent/head/boundary fields."
#[test]
fn an_unforked_branch_is_genesis_and_has_no_parent() {
    let db = seeded();
    assert_eq!(db.branch_origin(GRAPH, MAIN), Some(BranchOrigin::Genesis));
    assert_eq!(
        db.branch_origin(GRAPH, FEATURE),
        None,
        "a branch nobody wrote to or forked does not exist"
    );
}

/// A current-state fork records the parent's frontier as its boundary, DERIVED
/// rather than supplied. `fork_branch_at` is where a boundary may be chosen, and
/// the laws at the end of this file hold it to being used rather than stored.
#[test]
fn a_fork_records_its_parent() {
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("forks");
    assert_eq!(
        db.branch_origin(GRAPH, FEATURE),
        Some(BranchOrigin::Fork {
            parent_branch: MAIN,
            fork_boundary: CommitSeq(1),
        })
    );
    assert_eq!(
        db.branch_origin(GRAPH, MAIN),
        Some(BranchOrigin::Genesis),
        "forking a child does not rewrite the parent's origin"
    );
}

// ---------------------------------------------------------------------------
// Inheritance
// ---------------------------------------------------------------------------

/// The child begins as exactly the parent's state when `fork_branch` is called
/// — every vertex, edge and property, not merely the right counts.
#[test]
fn a_fork_inherits_the_parents_state_exactly() {
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("forks");

    let main = db.graph(GRAPH, MAIN).expect("main");
    let feature = db.graph(GRAPH, FEATURE).expect("feature");
    assert_eq!(
        main, feature,
        "at the fork point the two branches are indistinguishable"
    );
    assert_eq!(feature.vertex_count(), 2);
    assert_eq!(feature.edge_count(), 1);
    assert_eq!(name_on(&db, FEATURE, 1), Some(text("ada")));
    assert_eq!(
        feature.neighbours(VId(1), REL),
        vec![VId(2)],
        "inherited edges are traversable, not just present"
    );
}

// ---------------------------------------------------------------------------
// THE LAW: isolation is bidirectional
// ---------------------------------------------------------------------------

/// The child must not see the parent's post-fork writes.
#[test]
fn the_child_does_not_see_the_parents_later_writes() {
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("forks");

    apply_at(&mut db, MAIN, 2, vec![vertex(3, "alan")]);

    assert_eq!(db.graph(GRAPH, MAIN).expect("main").vertex_count(), 3);
    assert_eq!(
        db.graph(GRAPH, FEATURE).expect("feature").vertex_count(),
        2,
        "a write to main after the fork is not visible on feature"
    );
    assert!(
        db.graph(GRAPH, FEATURE)
            .expect("feature")
            .vertex(VId(3))
            .is_none()
    );
}

/// And the parent must not see the child's. THIS is the direction a
/// structurally-shared implementation breaks first, because the child's writes
/// land in objects the parent also reaches — so it is asserted separately rather
/// than assumed symmetric.
#[test]
fn the_parent_does_not_see_the_childs_writes() {
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("forks");

    apply_at(&mut db, FEATURE, 2, vec![vertex(4, "hopper")]);

    assert_eq!(db.graph(GRAPH, FEATURE).expect("feature").vertex_count(), 3);
    assert_eq!(
        db.graph(GRAPH, MAIN).expect("main").vertex_count(),
        2,
        "a write to feature is not visible on main"
    );
    assert!(
        db.graph(GRAPH, MAIN)
            .expect("main")
            .vertex(VId(4))
            .is_none()
    );
}

/// Divergence on the SAME identity: both branches may change vertex 1's name
/// independently, and neither observes the other's value. This is the case that
/// matters for branch-per-agent isolation — two agents editing the same entity.
#[test]
fn both_branches_may_diverge_on_the_same_identity() {
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("forks");

    apply_at(
        &mut db,
        MAIN,
        2,
        vec![DeltaRow::Property {
            elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
            property: PROP,
            before: Some(text("ada")),
            after: Some(text("ada-on-main")),
        }],
    );
    apply_at(
        &mut db,
        FEATURE,
        2,
        vec![DeltaRow::Property {
            elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
            property: PROP,
            before: Some(text("ada")),
            after: Some(text("ada-on-feature")),
        }],
    );

    assert_eq!(name_on(&db, MAIN, 1), Some(text("ada-on-main")));
    assert_eq!(name_on(&db, FEATURE, 1), Some(text("ada-on-feature")));
}

/// Both branches' before-images are checked against THEIR OWN state, so a row
/// valid on one branch can be invalid on the other. Without this, a divergent
/// branch would accept a row derived from a basis it never had.
#[test]
fn before_images_are_checked_per_branch() {
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("forks");
    apply_at(
        &mut db,
        MAIN,
        2,
        vec![DeltaRow::Property {
            elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
            property: PROP,
            before: Some(text("ada")),
            after: Some(text("ada-on-main")),
        }],
    );

    // main is now "ada-on-main"; a row asserting that before-image must FAIL on
    // feature, which still has "ada".
    let template = LogicalDeltaTemplate::build(
        ObjectId([0x11; 32]),
        [0x22; 32],
        vec![CoordinateEntry {
            graph: GRAPH,
            branch: FEATURE,
            relation: REL,
            schema_epoch: SchemaEpoch(0),
            schema_transition: None,
            rows: vec![DeltaRow::Property {
                elem: fgdb_delta_types::ElementId::Vertex(VId(1)),
                property: PROP,
                before: Some(text("ada-on-main")),
                after: Some(text("stolen")),
            }],
        }],
    )
    .expect("builds");
    assert!(
        db.apply_template(&template, CommitSeq(3)).is_err(),
        "a row whose basis is another branch's state must not apply here"
    );
    assert_eq!(
        name_on(&db, FEATURE, 1),
        Some(text("ada")),
        "and the refusal left feature untouched"
    );
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// HISTORY IS APPEND-ONLY. A template offered at a sequence the coordinate has
/// already passed is refused, in both the equal and the lower case. Re-applying
/// a sequence would either duplicate its effects or silently rewrite what that
/// sequence meant, and the commit stream this materializes is gap-free and
/// monotone by construction — so accepting one would model a stream that cannot
/// exist.
#[test]
fn a_sequence_that_does_not_advance_is_refused() {
    let mut db = seeded();
    assert_eq!(db.applied_through(GRAPH, MAIN), Some(CommitSeq(1)));

    for offered in [0u64, 1] {
        let template = LogicalDeltaTemplate::build(
            ObjectId([0x11; 32]),
            [0x22; 32],
            vec![CoordinateEntry {
                graph: GRAPH,
                branch: MAIN,
                relation: REL,
                schema_epoch: SchemaEpoch(0),
                schema_transition: None,
                rows: vec![vertex(50 + offered as u128, "late")],
            }],
        )
        .expect("builds");
        let result = db.apply_template(&template, CommitSeq(offered));
        assert!(
            matches!(
                result,
                Err(fgdb_reference::ApplyError::SequenceNotAdvancing { .. })
            ),
            "offering {offered} against applied_through 1 must be refused; got {result:?}"
        );
    }

    assert_eq!(
        db.applied_through(GRAPH, MAIN),
        Some(CommitSeq(1)),
        "a refusal does not move the frontier"
    );
    assert_eq!(
        db.graph(GRAPH, MAIN).expect("main").vertex_count(),
        2,
        "and applies nothing"
    );
}

/// A forked child inherits the parent's position, so its OWN next write must
/// advance past it. Without this, a child could re-apply the sequence range it
/// inherited and diverge from a history that never happened.
#[test]
fn a_child_must_advance_past_its_inherited_position() {
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("forks");
    assert_eq!(db.applied_through(GRAPH, FEATURE), Some(CommitSeq(1)));

    let template = LogicalDeltaTemplate::build(
        ObjectId([0x11; 32]),
        [0x22; 32],
        vec![CoordinateEntry {
            graph: GRAPH,
            branch: FEATURE,
            relation: REL,
            schema_epoch: SchemaEpoch(0),
            schema_transition: None,
            rows: vec![vertex(60, "replay")],
        }],
    )
    .expect("builds");
    assert!(
        db.apply_template(&template, CommitSeq(1)).is_err(),
        "the child may not re-apply the sequence it inherited"
    );

    // Advancing works.
    apply_at(&mut db, FEATURE, 2, vec![vertex(61, "fresh")]);
    assert_eq!(db.applied_through(GRAPH, FEATURE), Some(CommitSeq(2)));
    assert_eq!(
        db.applied_through(GRAPH, MAIN),
        Some(CommitSeq(1)),
        "and the parent's position is untouched"
    );
}

#[test]
fn forking_from_a_nonexistent_branch_is_refused() {
    let mut db = seeded();
    assert_eq!(
        db.fork_branch(GRAPH, BranchId(99), FEATURE),
        Err(BranchError::NoSuchParent {
            graph: GRAPH,
            parent: BranchId(99),
        })
    );
    assert_eq!(
        db.branch_origin(GRAPH, FEATURE),
        None,
        "a refused fork creates nothing"
    );
}

/// A branch is created once. Permitting a second fork onto a live branch would
/// silently replace its history — worse than refusing, because the caller would
/// believe the fork succeeded.
#[test]
fn forking_onto_an_existing_branch_is_refused() {
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("first fork");
    apply_at(&mut db, FEATURE, 2, vec![vertex(4, "hopper")]);
    let settled = db.clone();

    assert_eq!(
        db.fork_branch(GRAPH, MAIN, FEATURE),
        Err(BranchError::BranchExists {
            graph: GRAPH,
            branch: FEATURE,
        })
    );
    assert_eq!(db, settled, "the refusal changed nothing");

    // Also refused when the target exists only because it was written to.
    let mut written = ReferenceDatabase::new();
    apply_at(&mut written, MAIN, 1, vec![vertex(1, "ada")]);
    apply_at(&mut written, FEATURE, 1, vec![vertex(9, "independent")]);
    assert_eq!(
        written.fork_branch(GRAPH, MAIN, FEATURE),
        Err(BranchError::BranchExists {
            graph: GRAPH,
            branch: FEATURE,
        }),
        "a branch that exists by having been written to is still a live branch"
    );
}

#[test]
fn a_branch_cannot_fork_from_itself() {
    let mut db = seeded();
    assert_eq!(
        db.fork_branch(GRAPH, MAIN, MAIN),
        Err(BranchError::SelfFork { branch: MAIN })
    );
}

/// Forks chain: a fork of a fork inherits the intermediate state, and all three
/// stay isolated. A single-level implementation passes every test above.
#[test]
fn forks_chain_and_all_levels_stay_isolated() {
    const RELEASE: BranchId = BranchId(3);
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, FEATURE).expect("fork 1");
    apply_at(&mut db, FEATURE, 2, vec![vertex(4, "hopper")]);
    db.fork_branch(GRAPH, FEATURE, RELEASE).expect("fork 2");

    assert_eq!(
        db.graph(GRAPH, RELEASE).expect("release").vertex_count(),
        3,
        "release inherited feature's post-fork write"
    );
    assert_eq!(
        db.branch_origin(GRAPH, RELEASE),
        Some(BranchOrigin::Fork {
            parent_branch: FEATURE,
            // feature applied its post-fork write at 2, so that is where
            // release begins — derived from the chain, not chosen.
            fork_boundary: CommitSeq(2),
        })
    );

    apply_at(&mut db, RELEASE, 3, vec![vertex(5, "lovelace")]);
    assert_eq!(db.graph(GRAPH, MAIN).expect("m").vertex_count(), 2);
    assert_eq!(db.graph(GRAPH, FEATURE).expect("f").vertex_count(), 3);
    assert_eq!(db.graph(GRAPH, RELEASE).expect("r").vertex_count(), 4);
}

// ---------------------------------------------------------------------------
// Forking from a point in history (plan:2000 fork_boundary_logical_command_seq)
// ---------------------------------------------------------------------------

/// Distinct from the `SECOND_HISTORICAL` declared inside an earlier test's body, so these
/// laws do not depend on a name that is scoped to another test.
const HISTORICAL: BranchId = BranchId(4);
const SECOND_HISTORICAL: BranchId = BranchId(5);
const ABSENT_PARENT: BranchId = BranchId(98);
//
// A boundary parameter was a counterfeit once (fgdb-vyb0): the materializer had
// no history, so the value could only be stored, and a test asserting it round-
// tripped proved only that the number handed in was the number handed back. What
// makes it real now is that the boundary DECIDES THE CHILD'S STATE — every law
// below fails if the parameter is ignored, and none of them is a round-trip.

/// main at three sequences, so a fork can be taken from the middle of it.
///
/// seq 1: ada, grace, edge      seq 2: hopper      seq 3: lovelace
fn layered() -> ReferenceDatabase {
    let mut db = ReferenceDatabase::new();
    apply_at(
        &mut db,
        MAIN,
        1,
        vec![vertex(1, "ada"), vertex(2, "grace"), edge(10, 1, 2)],
    );
    apply_at(&mut db, MAIN, 2, vec![vertex(3, "hopper")]);
    apply_at(&mut db, MAIN, 3, vec![vertex(4, "lovelace")]);
    db
}

/// THE LAW THE PARAMETER EARNS ITS PLACE WITH: the child inherits the parent as
/// it stood at the boundary, not as it stands now.
#[test]
fn a_historical_fork_inherits_the_parent_at_the_boundary() {
    let mut db = layered();
    db.fork_branch_at(GRAPH, MAIN, HISTORICAL, CommitSeq(2))
        .expect("forks at 2");

    let child = db.graph(GRAPH, HISTORICAL).expect("feature exists");
    assert_eq!(child.vertex_count(), 3, "ada, grace, hopper — not lovelace");
    assert!(
        child.vertex(VId(4)).is_none(),
        "the vertex main added at 3 is above the boundary"
    );
    assert_eq!(
        db.graph(GRAPH, MAIN).expect("main").vertex_count(),
        4,
        "main still has all four — otherwise this law proves nothing"
    );
}

/// Two forks from the same parent at different boundaries are different
/// branches. The sharpest form of "the parameter is used": one call site, one
/// parent, one moment in time, two answers.
#[test]
fn two_boundaries_give_two_different_children() {
    let mut db = layered();
    db.fork_branch_at(GRAPH, MAIN, HISTORICAL, CommitSeq(1))
        .expect("forks at 1");
    db.fork_branch_at(GRAPH, MAIN, SECOND_HISTORICAL, CommitSeq(3))
        .expect("forks at 3");

    assert_eq!(db.graph(GRAPH, HISTORICAL).expect("f").vertex_count(), 2);
    assert_eq!(
        db.graph(GRAPH, SECOND_HISTORICAL)
            .expect("r")
            .vertex_count(),
        4
    );
}

/// A boundary of zero is legal and yields an empty child — a branch taken from
/// before the parent's first commit. Refusing zero would be arbitrary: the state
/// at zero is a state the parent genuinely had.
#[test]
fn a_fork_at_zero_is_an_empty_child() {
    let mut db = layered();
    db.fork_branch_at(GRAPH, MAIN, HISTORICAL, CommitSeq(0))
        .expect("forks at 0");

    let child = db.graph(GRAPH, HISTORICAL).expect("feature exists");
    assert_eq!(child.vertex_count(), 0);
    assert_eq!(child.edge_count(), 0);
    assert_eq!(db.applied_through(GRAPH, HISTORICAL), Some(CommitSeq(0)));
}

/// A boundary above the parent's frontier is REFUSED — a fork from the parent's
/// future. Clamping to the frontier would make the fork silently mean something
/// other than what was asked, and nothing afterwards would show the difference.
#[test]
fn a_fork_above_the_parents_frontier_is_refused() {
    let mut db = layered();
    assert_eq!(
        db.fork_branch_at(GRAPH, MAIN, HISTORICAL, CommitSeq(4)),
        Err(BranchError::BoundaryBeyondParentFrontier {
            graph: GRAPH,
            parent: MAIN,
            applied_through: CommitSeq(3),
            requested: CommitSeq(4),
        })
    );
    assert_eq!(
        db.branch_origin(GRAPH, HISTORICAL),
        None,
        "a refused fork must not have created the branch"
    );
    assert!(
        db.fork_branch_at(GRAPH, MAIN, HISTORICAL, CommitSeq(3))
            .is_ok(),
        "the frontier itself is a legal boundary"
    );
}

/// The recorded origin carries the boundary that was used, and the child's
/// frontier is that boundary — so its first commit must advance past it, not
/// past the parent's present.
#[test]
fn a_historical_fork_records_and_advances_from_its_boundary() {
    let mut db = layered();
    db.fork_branch_at(GRAPH, MAIN, HISTORICAL, CommitSeq(2))
        .expect("forks at 2");
    assert_eq!(
        db.branch_origin(GRAPH, HISTORICAL),
        Some(BranchOrigin::Fork {
            parent_branch: MAIN,
            fork_boundary: CommitSeq(2),
        })
    );
    assert_eq!(db.applied_through(GRAPH, HISTORICAL), Some(CommitSeq(2)));
    assert_eq!(
        db.recorded_commits(GRAPH, HISTORICAL),
        0,
        "the child owns no commits of its own yet"
    );

    apply_at(&mut db, HISTORICAL, 4, vec![vertex(5, "clarke")]);
    assert_eq!(db.graph(GRAPH, HISTORICAL).expect("f").vertex_count(), 4);
    assert_eq!(
        db.graph(GRAPH, MAIN).expect("m").vertex_count(),
        4,
        "and main is untouched by it"
    );
}

/// Historical reads of a historically-forked child are capped at its boundary
/// too, at every sequence — including sequences where the parent committed and
/// the child did not. The child's own frontier being higher than the boundary is
/// exactly the case where a single global filter would leak.
#[test]
fn a_historical_child_never_sees_the_parent_past_its_boundary() {
    let mut db = layered();
    db.fork_branch_at(GRAPH, MAIN, HISTORICAL, CommitSeq(1))
        .expect("forks at 1");
    apply_at(&mut db, HISTORICAL, 4, vec![vertex(9, "wilkes")]);

    let snapshot = db.snapshot(GRAPH, HISTORICAL).expect("mints");
    let observed = db.read(&snapshot).expect("reads");
    assert!(
        observed.vertex(VId(3)).is_none() && observed.vertex(VId(4)).is_none(),
        "main's commits at 2 and 3 are above the boundary of 1"
    );
    assert!(
        observed.vertex(VId(9)).is_some(),
        "the child's own write is in"
    );
    assert_eq!(observed.vertex_count(), 3);
}

/// The shape checks come first, so a self-fork or an existing branch is reported
/// as such whatever boundary was named — the boundary is not an excuse to
/// reclassify a malformed request.
#[test]
fn shape_refusals_outrank_the_boundary() {
    let mut db = layered();
    assert_eq!(
        db.fork_branch_at(GRAPH, MAIN, MAIN, CommitSeq(99)),
        Err(BranchError::SelfFork { branch: MAIN })
    );
    db.fork_branch_at(GRAPH, MAIN, HISTORICAL, CommitSeq(1))
        .expect("forks");
    assert_eq!(
        db.fork_branch_at(GRAPH, MAIN, HISTORICAL, CommitSeq(99)),
        Err(BranchError::BranchExists {
            graph: GRAPH,
            branch: HISTORICAL,
        })
    );
    assert_eq!(
        db.fork_branch_at(GRAPH, ABSENT_PARENT, SECOND_HISTORICAL, CommitSeq(1)),
        Err(BranchError::NoSuchParent {
            graph: GRAPH,
            parent: ABSENT_PARENT,
        })
    );
}
