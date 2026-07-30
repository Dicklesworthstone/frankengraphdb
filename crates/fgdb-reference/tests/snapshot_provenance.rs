//! Laws binding a snapshot to the history that minted it.
//!
//! A `Snapshot` was a bare `(graph, branch, high)` triple, and a value with no
//! provenance is freely transferable. So `B.read(&snapshot_from_A)` answered — with
//! B's state — and with equal frontiers but divergent histories nothing
//! distinguished the two databases at all. The same hole reached the write side:
//! `begin_at(db_a, snapshot_a)` then `commit(db_b, ..)` could commit A's effects
//! into B whenever the before-images happened to line up
//! (fgdb-reference-snapshot-provenance-9bvm).
//!
//! **A WRONG ANSWER, NOT A CRASH**, which is why this is worth a file. A refusal is
//! recoverable and legible; silently substituting one database's state for
//! another's is neither, and every downstream assertion built on it inherits the
//! substitution.
//!
//! **AUTHORITY, HISTORY, AND HEAD ARE DISTINCT.** An opaque authority distinguishes
//! independent databases even when both are empty or have replayed identical bytes.
//! A true `ReferenceDatabase::clone` shares that authority, so it remains useful;
//! the stream-prefix digest then binds the history through `high`, and a separate
//! exact-lineage digest binds the branch head selected from that stream. None of
//! the three can stand in for the other two.
//!
//! Keyed by sequence rather than one running digest, because a snapshot must stay
//! valid as its own database advances — that is an existing law, and a single
//! current digest would invalidate every outstanding snapshot on the next commit.

use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, ElementId, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_reference::txn::{Transaction, TxnError};
use fgdb_reference::{ReferenceDatabase, SnapshotError};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, DatabaseId, GraphId, ObjectId, VId};

const GRAPH: GraphId = GraphId(1);
const MAIN: BranchId = BranchId(1);
const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);
const SEMANTICS: ObjectId = ObjectId([0x11; 32]);

fn vertex(vid: u128, value: i64) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        labels: vec![LABEL],
        props: vec![(PROP, CanonicalScalar::Int(value))],
        valid_time: None,
    }
}

fn set(vid: u128, before: i64, after: i64) -> DeltaRow {
    DeltaRow::Property {
        elem: ElementId::Vertex(VId(vid)),
        property: PROP,
        before: Some(CanonicalScalar::Int(before)),
        after: Some(CanonicalScalar::Int(after)),
    }
}

fn apply(db: &mut ReferenceDatabase, seq: u64, rows: Vec<DeltaRow>) {
    let template = LogicalDeltaTemplate::build(
        SEMANTICS,
        [0x22; 32],
        vec![CoordinateEntry {
            graph: GRAPH,
            branch: MAIN,
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

/// A database advanced through `seq` with v1 counting up.
fn advanced(through: u64) -> ReferenceDatabase {
    advance(ReferenceDatabase::new(), through)
}

fn advance(mut db: ReferenceDatabase, through: u64) -> ReferenceDatabase {
    apply(&mut db, 1, vec![vertex(1, 0)]);
    for seq in 2..=through {
        apply(&mut db, seq, vec![set(1, seq as i64 - 2, seq as i64 - 1)]);
    }
    db
}

/// Same authority and stream, but the same child ID selects different parent
/// heads. This is the shape a stream-only provenance basis cannot distinguish.
fn divergent_child_clones() -> (ReferenceDatabase, ReferenceDatabase, BranchId) {
    let alt = BranchId(2);
    let child = BranchId(3);
    let template = LogicalDeltaTemplate::build(
        SEMANTICS,
        [0x22; 32],
        vec![
            CoordinateEntry {
                graph: GRAPH,
                branch: MAIN,
                relation: REL,
                schema_epoch: SchemaEpoch(0),
                schema_transition: None,
                rows: vec![vertex(1, 11)],
            },
            CoordinateEntry {
                graph: GRAPH,
                branch: alt,
                relation: REL,
                schema_epoch: SchemaEpoch(0),
                schema_transition: None,
                rows: vec![vertex(2, 22)],
            },
        ],
    )
    .expect("template builds");

    let mut a = ReferenceDatabase::new();
    a.apply_template(&template, CommitSeq(1))
        .expect("a applies");
    let mut b = a.clone();
    a.fork_branch_at(GRAPH, MAIN, child, CommitSeq(1))
        .expect("a forks child from main");
    b.fork_branch_at(GRAPH, alt, child, CommitSeq(1))
        .expect("b forks child from alt");
    (a, b, child)
}

/// A snapshot from a LONGER history is refused by a database that has not reached
/// that sequence. The reported case: B answered with its own state instead.
#[test]
fn a_snapshot_from_a_longer_history_is_refused() {
    let mut a = advanced(1);
    let b = a.clone();
    apply(&mut a, 2, vec![set(1, 0, 1)]);

    let from_a = a.snapshot(GRAPH, MAIN).expect("mints");
    assert_eq!(from_a.high(), CommitSeq(2));
    assert_eq!(
        b.read(&from_a),
        Err(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: MAIN,
            high: CommitSeq(2),
        })
    );
    // And it still reads correctly against the database that minted it.
    assert!(a.read(&from_a).is_ok());
}

/// EQUAL FRONTIERS, DIVERGENT HISTORIES — the case a frontier comparison cannot
/// see at all, and the reason the binding is a digest rather than a number.
#[test]
fn equal_frontiers_with_divergent_histories_are_refused() {
    let mut a = advanced(1);
    let mut b = a.clone();
    // Same sequence, different content.
    apply(&mut a, 2, vec![vertex(50, 7)]);
    apply(&mut b, 2, vec![vertex(60, 7)]);

    assert_eq!(a.replay_frontier(), b.replay_frontier(), "frontiers agree");
    let from_a = a.snapshot(GRAPH, MAIN).expect("mints");
    assert_eq!(
        b.read(&from_a),
        Err(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: MAIN,
            high: CommitSeq(2),
        }),
        "identical frontiers do not make two histories the same history"
    );
}

/// CONTROL: a true clone shares authority and accepts the snapshot while its
/// selected history and head still match.
///
/// Without this law, an implementation that generates a fresh authority during
/// `Clone` — or simply refuses every value except against the exact object address
/// that minted it — would pass the negative laws while making an intentional
/// database clone unusable.
#[test]
fn an_exact_clone_with_the_same_history_is_accepted() {
    let a = advanced(3);
    let b = a.clone();

    let from_a = a.snapshot(GRAPH, MAIN).expect("mints");
    let observed = b.read(&from_a).expect("the same history is the same basis");
    assert_eq!(
        observed,
        a.read(&from_a).expect("reads"),
        "and it observes the same graph"
    );
}

/// Equal content is not database authority.
///
/// This is the same-state control the empty/genesis case makes load-bearing:
/// digesting bytes alone cannot distinguish two independently authoritative
/// databases after an identical replay.
#[test]
fn an_independent_database_with_the_same_history_is_refused() {
    let a = advanced(3);
    let b = advanced(3);
    assert_eq!(
        a.prefix_digest(CommitSeq(3)),
        b.prefix_digest(CommitSeq(3)),
        "the histories really do have identical content"
    );

    let from_a = a.snapshot(GRAPH, MAIN).expect("mints");
    assert_eq!(
        b.read(&from_a),
        Err(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: MAIN,
            high: CommitSeq(3),
        }),
        "content equality does not confer authority"
    );
}

/// Durable recovery may materialize several values for one persisted database ID.
/// Those values share authority, while a different ID remains foreign even after
/// an identical replay.
#[test]
fn a_persisted_database_id_survives_replay_without_aliasing_another_database() {
    let database_id = DatabaseId([0x44; 16]);
    let a = advance(ReferenceDatabase::with_database_id(database_id), 3);
    let same_replay = advance(ReferenceDatabase::with_database_id(database_id), 3);
    let other = advance(
        ReferenceDatabase::with_database_id(DatabaseId([0x55; 16])),
        3,
    );

    let snapshot = a.snapshot(GRAPH, MAIN).expect("mints");
    assert_eq!(
        same_replay.read(&snapshot),
        a.read(&snapshot),
        "one durable database ID survives independent materialization"
    );
    assert_eq!(
        other.read(&snapshot),
        Err(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: MAIN,
            high: CommitSeq(3),
        }),
        "identical bytes under another database ID do not confer authority"
    );
}

/// A snapshot stays valid as ITS OWN database advances.
///
/// The reason the digest is per-sequence rather than one running value: a single
/// current digest would invalidate every outstanding snapshot on the next commit,
/// breaking the stability law that snapshot isolation rests on.
#[test]
fn a_snapshot_survives_later_commits_on_its_own_database() {
    let mut db = advanced(2);
    let snapshot = db.snapshot(GRAPH, MAIN).expect("mints");
    let before = db.read(&snapshot).expect("reads");

    for seq in 3..=6 {
        apply(&mut db, seq, vec![set(1, seq as i64 - 2, seq as i64 - 1)]);
    }
    assert_eq!(db.replay_frontier(), CommitSeq(6));
    assert_eq!(
        db.read(&snapshot).expect("still reads"),
        before,
        "four later commits did not invalidate an outstanding snapshot"
    );
}

/// THE WRITE-SIDE HALF: a transaction begun on one database cannot commit into
/// another.
///
/// Sharper than the read case, because the effects would be DURABLE. The two
/// databases here share a prefix and the before-image lines up, so nothing about
/// the effects themselves is detectably wrong — which is exactly the situation a
/// content check has to catch and an effect-level check cannot.
#[test]
fn a_transaction_cannot_commit_into_a_different_database() {
    let a = advanced(2);
    let mut b = advanced(2);

    let mut txn = Transaction::begin(&a, GRAPH, MAIN).expect("begins on a");
    txn.execute(&[fgdb_reference::intents::Statement::new(vec![
        fgdb_reference::intents::Intent::CreateVertex {
            vid: VId(99),
            labels: vec![LABEL],
            props: vec![],
        },
    ])])
    .expect("executes");

    let settled = b.clone();
    let result = txn.commit(&mut b, REL, SEMANTICS, CommitSeq(3));
    assert_eq!(
        result,
        Err(TxnError::Snapshot(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: MAIN,
            high: CommitSeq(2),
        }))
    );
    assert_eq!(b, settled, "and nothing was written into b");
}

/// A genesis snapshot is accepted by a true clone and refused by an independent
/// empty database.
///
/// It names the empty stream, so history and lineage digests are necessarily equal
/// everywhere. Only the issuing authority can close this case. The same-authority
/// stale-creation race remains a transaction conflict rather than a provenance
/// error, and the law pins both distinctions.
#[test]
fn a_genesis_snapshot_is_authority_bound_and_still_guarded() {
    let empty_a = ReferenceDatabase::new();
    let genesis = empty_a.genesis_snapshot(GRAPH, MAIN).expect("mints");

    // Accepted by a true clone: it shares the issuing authority and empty basis.
    let empty_clone = empty_a.clone();
    assert!(empty_clone.read(&genesis).is_ok());

    // Refused by an independently authoritative empty database, with no effects.
    let mut independent = ReferenceDatabase::new();
    assert_eq!(
        independent.read(&genesis),
        Err(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: MAIN,
            high: CommitSeq(0),
        })
    );
    let mut foreign_txn =
        Transaction::begin_at(&empty_a, genesis.clone()).expect("begins on issuer");
    foreign_txn
        .execute(&[fgdb_reference::intents::Statement::new(vec![
            fgdb_reference::intents::Intent::CreateVertex {
                vid: VId(8),
                labels: vec![LABEL],
                props: vec![],
            },
        ])])
        .expect("executes");
    let independent_before = independent.clone();
    assert_eq!(
        foreign_txn.commit(&mut independent, REL, SEMANTICS, CommitSeq(1)),
        Err(TxnError::Snapshot(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: MAIN,
            high: CommitSeq(0),
        }))
    );
    assert_eq!(
        independent, independent_before,
        "foreign genesis refusal is side-effect-free"
    );

    // A same-authority clone that has ADVANCED refuses the commit — and MEASURED rather
    // than predicted: the guard that fires is the coordinate-existence conflict
    // key, not the stream frontier. It runs first because the branch genuinely came
    // into existence after this transaction's basis, which is the more specific
    // answer of the two: "someone else created this branch" rather than "your
    // sequence is wrong".
    let mut advanced_b = empty_a.clone();
    apply(&mut advanced_b, 1, vec![vertex(1, 0)]);
    apply(&mut advanced_b, 2, vec![set(1, 0, 1)]);
    let mut txn = Transaction::begin_at(&empty_a, genesis).expect("begins");
    txn.execute(&[fgdb_reference::intents::Statement::new(vec![
        fgdb_reference::intents::Intent::CreateVertex {
            vid: VId(7),
            labels: vec![LABEL],
            props: vec![],
        },
    ])])
    .expect("executes");
    let settled = advanced_b.clone();
    let result = txn
        .commit(&mut advanced_b, REL, SEMANTICS, CommitSeq(1))
        .expect("a lost race is an outcome, not an error");
    assert_eq!(
        result,
        fgdb_reference::txn::TxnOutcome::Conflicted {
            conflicts: vec![fgdb_reference::ConflictKey::CoordinateExistence],
        },
        "a stale genesis claim loses to the branch that already exists"
    );
    assert_eq!(advanced_b, settled, "and nothing was written");
}

/// The prefix digest is a function of the whole stream up to a sequence, so it
/// differs at the FIRST point two histories diverge and stays different after.
#[test]
fn the_prefix_digest_diverges_at_the_first_difference_and_stays_diverged() {
    let mut a = advanced(1);
    let mut b = advanced(1);
    assert_eq!(
        a.prefix_digest(CommitSeq(1)),
        b.prefix_digest(CommitSeq(1)),
        "identical first commits agree"
    );

    apply(&mut a, 2, vec![vertex(50, 1)]);
    apply(&mut b, 2, vec![vertex(51, 1)]);
    assert_ne!(a.prefix_digest(CommitSeq(2)), b.prefix_digest(CommitSeq(2)));

    // Now give them IDENTICAL third commits: the prefixes must stay apart, because
    // the digest folds the previous prefix rather than only the latest template.
    apply(&mut a, 3, vec![vertex(70, 1)]);
    apply(&mut b, 3, vec![vertex(70, 1)]);
    assert_ne!(
        a.prefix_digest(CommitSeq(3)),
        b.prefix_digest(CommitSeq(3)),
        "an identical later commit must not re-converge two divergent histories"
    );
    assert_eq!(
        a.prefix_digest(CommitSeq(1)),
        b.prefix_digest(CommitSeq(1)),
        "while the shared prefix still agrees"
    );
}

/// Sequence zero is the empty stream and every database agrees on it; an unknown
/// sequence is represented out of band rather than by a digest-shaped sentinel.
#[test]
fn the_empty_stream_is_shared_and_an_unknown_sequence_is_not() {
    let db = advanced(2);
    assert_eq!(
        db.prefix_digest(CommitSeq(0)),
        ReferenceDatabase::new().prefix_digest(CommitSeq(0)),
        "the empty stream is the same everywhere"
    );
    assert_eq!(
        db.prefix_digest(CommitSeq(9)),
        None,
        "a sequence this database never applied has no digest"
    );
}

/// A stream-prefix digest is not a complete provenance identity while branch
/// lineage can change outside `apply_template`.
#[test]
fn equal_stream_prefix_with_divergent_fork_lineage_is_refused() {
    let (a, b, child) = divergent_child_clones();

    assert_eq!(
        a.prefix_digest(CommitSeq(1)),
        b.prefix_digest(CommitSeq(1)),
        "fork lineage is absent from the stream-only basis"
    );
    let from_a = a.snapshot(GRAPH, child).expect("a mints child snapshot");
    assert_eq!(
        b.read(&from_a),
        Err(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: child,
            high: CommitSeq(1),
        }),
        "the receiving database must not substitute its divergent child lineage"
    );
}

/// Sequence zero does not erase the selected head of an EXISTING branch.
///
/// Both reads happen to materialize an empty graph, but the capability names a
/// branch ancestry, not merely its current rows. Treating every high-zero snapshot
/// as a genesis claim would let a transaction prepared for one child head commit
/// through another after a same-authority clone diverged.
#[test]
fn an_existing_sequence_zero_snapshot_is_bound_to_its_selected_head() {
    let (a, b, child) = divergent_child_clones();

    let from_a = a
        .snapshot_at(GRAPH, child, CommitSeq(0))
        .expect("a mints historical child snapshot");
    assert_eq!(
        b.read(&from_a),
        Err(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: child,
            high: CommitSeq(0),
        }),
        "empty visible rows do not erase the selected branch head"
    );
}

/// A selected head includes its raw fork boundary, even when the snapshot's
/// historical cut lies below both candidate heads.
///
/// If provenance hashed only effective read caps, boundary 1 and boundary 2 both
/// collapse to cap 1 for this snapshot. The visible rows agree there; the branch
/// heads do not.
#[test]
fn same_parent_with_a_different_fork_boundary_is_refused() {
    let child = BranchId(3);
    let mut a = advanced(2);
    let mut b = a.clone();
    a.fork_branch_at(GRAPH, MAIN, child, CommitSeq(1))
        .expect("a forks at one");
    b.fork_branch_at(GRAPH, MAIN, child, CommitSeq(2))
        .expect("b forks at two");

    let from_a = a.snapshot(GRAPH, child).expect("a mints child snapshot");
    assert_eq!(from_a.high(), CommitSeq(1));
    assert_eq!(
        b.read(&from_a),
        Err(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: child,
            high: CommitSeq(1),
        }),
        "an equal read cap is not an equal selected head"
    );
}

/// The same divergent-lineage law applies before a transaction can mutate the
/// receiving clone.
#[test]
fn divergent_fork_lineage_refuses_transaction_commit_without_side_effects() {
    let (a, mut b, child) = divergent_child_clones();

    let mut txn = Transaction::begin(&a, GRAPH, child).expect("begins on a child");
    txn.execute(&[fgdb_reference::intents::Statement::new(vec![
        fgdb_reference::intents::Intent::CreateVertex {
            vid: VId(99),
            labels: vec![LABEL],
            props: vec![],
        },
    ])])
    .expect("executes");

    let settled = b.clone();
    assert_eq!(
        txn.commit(&mut b, REL, SEMANTICS, CommitSeq(2)),
        Err(TxnError::Snapshot(SnapshotError::ForeignSnapshot {
            graph: GRAPH,
            branch: child,
            high: CommitSeq(1),
        }))
    );
    assert_eq!(b, settled, "the receiving child was not mutated");
}
