//! Laws of snapshot isolation — including, deliberately, what it FAILS to
//! prevent.
//!
//! §15 asks for transaction-anomaly oracles: executable statements of which
//! histories an isolation level admits, so a real transaction manager can be
//! differentially tested against a program rather than against an argument. That
//! makes the negative laws as load-bearing as the positive ones.
//!
//! **THE MOST IMPORTANT TEST IN THIS FILE IS
//! `write_skew_is_admitted_under_snapshot_isolation`.** It builds a history where
//! two transactions each read a two-element invariant, each write a *different*
//! element, both commit, and the invariant is then false — and it asserts that
//! this is what happens. Doctrine 7 forbids "snapshot isolation quietly labeled
//! ACID"; an executable demonstration of the exact gap is what keeps it un-quiet.
//! When SSI lands, that same history must be REFUSED, and this law is what will
//! have to be rewritten to say so. The pair is the specification of what SSI
//! buys, which is the only honest way to state it in advance.
//!
//! The positive rule is FIRST COMMITTER WINS — not first writer and not last
//! writer. Two transactions from one snapshot writing one element: the first to
//! commit succeeds, the second is refused. Refusing both would lose work no rule
//! requires losing, and admitting both is lost update.
//!
//! **WHAT WOULD MAKE THESE VACUOUS.** A conflict rule keyed on the branch rather
//! than the element would refuse everything and pass every "is refused" law here
//! while making the database serial; the disjoint-writes and other-branch laws
//! exist to catch that. A rule keyed on nothing would admit everything and pass
//! every "both commit" law; the lost-update laws catch that. Neither direction is
//! safe to leave untested.

use fgdb_delta_types::{
    DeltaRow, ElementId, EscrowDomainId, LabelId, OperationKey, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_reference::intents::{Intent, MismatchPolicy, Statement};
use fgdb_reference::txn::{Transaction, TxnError, TxnOutcome};
use fgdb_reference::{ConflictKey, ReferenceDatabase, collect_conflict_keys};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, ObjectId, VId};
use std::collections::BTreeSet;

const GRAPH: GraphId = GraphId(1);
const MAIN: BranchId = BranchId(1);
const OTHER: BranchId = BranchId(2);
const NESTED: BranchId = BranchId(3);
const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);
const SEMANTICS: ObjectId = ObjectId([0x11; 32]);

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}

fn create(vid: u128, value: i64) -> Statement {
    Statement::new(vec![Intent::CreateVertex {
        vid: VId(vid),
        labels: vec![LABEL],
        props: vec![(PROP, int(value))],
    }])
}

fn set(vid: u128, value: i64) -> Statement {
    Statement::new(vec![Intent::SetProp {
        elem: ElementId::Vertex(VId(vid)),
        name: PROP,
        value: int(value),
    }])
}

/// Commit a transaction that is expected to succeed, at `seq`.
fn commit_ok(db: &mut ReferenceDatabase, txn: Transaction, seq: u64) -> (CommitSeq, usize, usize) {
    txn.commit(db, REL, SEMANTICS, CommitSeq(seq))
        .expect("commit should not error")
        .committed_parts()
        .expect("commit should have succeeded")
}

fn prop_of(db: &ReferenceDatabase, branch: BranchId, vid: u128) -> Option<i64> {
    match db
        .graph(GRAPH, branch)?
        .vertex(VId(vid))?
        .props
        .get(&PROP)?
    {
        CanonicalScalar::Int(value) => Some(*value),
        _ => None,
    }
}

/// Seed a coordinate at sequence 1, through the ordinary transaction path.
///
/// Every law below therefore starts from a database that a transaction built,
/// which is the only way to know the transaction path can build one.
fn seed(db: &mut ReferenceDatabase, vertices: &[(u128, i64)]) {
    let statements: Vec<Statement> = vertices
        .iter()
        .map(|(vid, value)| create(*vid, *value))
        .collect();
    let mut txn = Transaction::begin_genesis(db, GRAPH, MAIN).expect("genesis begin");
    txn.execute(&statements).expect("executes");
    commit_ok(db, txn, 1);
}

/// Two vertices, both at 0, committed at sequence 1.
fn seeded() -> ReferenceDatabase {
    let mut db = ReferenceDatabase::new();
    seed(&mut db, &[(1, 0), (2, 0)]);
    db
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A transaction reads at its snapshot, not at the present. The concurrent
/// commit lands *after* the transaction began and must be invisible to it.
#[test]
fn a_transaction_reads_at_its_snapshot_not_the_present() {
    let mut db = seeded();
    let reader = Transaction::begin(&db, GRAPH, MAIN).expect("begin");

    let mut writer = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    writer.execute(&[set(1, 99)]).expect("executes");
    commit_ok(&mut db, writer, 2);

    assert_eq!(prop_of(&db, MAIN, 1), Some(99), "the write landed");
    assert_eq!(
        reader
            .workspace()
            .vertex(VId(1))
            .and_then(|v| v.props.get(&PROP))
            .cloned(),
        Some(int(0)),
        "the reader must still see its snapshot"
    );
}

/// A transaction sees its own writes across separate `execute` calls — the
/// property without which the second statement of any transaction is wrong.
#[test]
fn a_transaction_sees_its_own_writes() {
    let db = seeded();
    let mut txn = Transaction::begin(&db, GRAPH, MAIN).expect("begin");

    txn.execute(&[set(1, 7)]).expect("executes");
    assert_eq!(
        txn.workspace()
            .vertex(VId(1))
            .and_then(|v| v.props.get(&PROP))
            .cloned(),
        Some(int(7))
    );

    // A CompareAndSet against the value this transaction just wrote must match.
    // If the workspace were the basis, this would see 0 and fail.
    txn.execute(&[Statement::new(vec![Intent::CompareAndSet {
        elem: ElementId::Vertex(VId(1)),
        name: PROP,
        expected: Some(int(7)),
        value: int(8),
        mismatch: MismatchPolicy::StatementError,
    }])])
    .expect("executes");
    assert_eq!(txn.statement_failures(), 0, "the guard saw its own write");
    assert_eq!(
        txn.workspace()
            .vertex(VId(1))
            .and_then(|v| v.props.get(&PROP))
            .cloned(),
        Some(int(8))
    );
}

/// Read skew is impossible under SI: two reads separated by a concurrent commit
/// that touched both elements still agree with each other.
#[test]
fn read_skew_cannot_happen() {
    let mut db = seeded();
    let reader = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let first = reader.workspace().vertex(VId(1)).cloned();

    let mut writer = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    writer.execute(&[set(1, 50), set(2, 50)]).expect("executes");
    commit_ok(&mut db, writer, 2);

    assert_eq!(
        reader.workspace().vertex(VId(1)).cloned(),
        first,
        "the first element did not move under the reader"
    );
    assert_eq!(
        reader
            .workspace()
            .vertex(VId(2))
            .and_then(|v| v.props.get(&PROP))
            .cloned(),
        Some(int(0)),
        "and the second is read from the same snapshot, not from the present"
    );
}

// ---------------------------------------------------------------------------
// First committer wins
// ---------------------------------------------------------------------------

/// LOST UPDATE IS REFUSED. Both transactions read 0 and write; only one may land.
#[test]
fn a_lost_update_is_refused() {
    let mut db = seeded();
    let mut first = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut second = Transaction::begin(&db, GRAPH, MAIN).expect("begin");

    first.execute(&[set(1, 10)]).expect("executes");
    second.execute(&[set(1, 20)]).expect("executes");

    commit_ok(&mut db, first, 2);
    let outcome = second
        .commit(&mut db, REL, SEMANTICS, CommitSeq(3))
        .expect("commit should not error");

    assert_eq!(
        outcome,
        TxnOutcome::Conflicted {
            conflicts: vec![ConflictKey::Element(ElementId::Vertex(VId(1)))],
        }
    );
}

/// The FIRST committer wins, not the last. Asserted on the resulting value,
/// because "one of them was refused" is also true of a rule that keeps the wrong
/// one.
#[test]
fn the_first_committer_wins_not_the_last() {
    let mut db = seeded();
    let mut first = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut second = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    first.execute(&[set(1, 10)]).expect("executes");
    second.execute(&[set(1, 20)]).expect("executes");

    commit_ok(&mut db, first, 2);
    let refused = second
        .commit(&mut db, REL, SEMANTICS, CommitSeq(3))
        .expect("no error");

    assert!(!refused.is_committed());
    assert_eq!(
        prop_of(&db, MAIN, 1),
        Some(10),
        "the surviving value is the first committer's"
    );
    assert_eq!(db.applied_through(GRAPH, MAIN), Some(CommitSeq(2)));
}

/// A refused transaction leaves the database exactly as it was — not a partial
/// application, not a consumed sequence.
#[test]
fn a_conflicted_transaction_writes_nothing() {
    let mut db = seeded();
    let mut first = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut second = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    first.execute(&[set(1, 10)]).expect("executes");
    second.execute(&[set(1, 20), set(2, 30)]).expect("executes");

    commit_ok(&mut db, first, 2);
    let before = db.clone();
    let refused = second
        .commit(&mut db, REL, SEMANTICS, CommitSeq(3))
        .expect("no error");

    assert!(refused.conflicts().is_some());
    assert_eq!(db, before, "a conflicted commit is not a partial commit");
    assert_eq!(
        prop_of(&db, MAIN, 2),
        Some(0),
        "including the part of it that did NOT conflict"
    );
}

/// Disjoint writes do not conflict. Without this law a rule that refuses every
/// concurrent transaction would pass every refusal law above.
#[test]
fn disjoint_writes_do_not_conflict() {
    let mut db = seeded();
    let mut first = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut second = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    first.execute(&[set(1, 10)]).expect("executes");
    second.execute(&[set(2, 20)]).expect("executes");

    commit_ok(&mut db, first, 2);
    commit_ok(&mut db, second, 3);

    assert_eq!(prop_of(&db, MAIN, 1), Some(10));
    assert_eq!(prop_of(&db, MAIN, 2), Some(20));
}

/// A concurrent commit on a DIFFERENT branch never conflicts — B6's
/// branch-per-agent isolation, stated as a concurrency-control property rather
/// than only as a visibility one.
#[test]
fn a_concurrent_commit_on_another_branch_never_conflicts() {
    let mut db = seeded();
    db.fork_branch(GRAPH, MAIN, OTHER).expect("forks");

    let mut mine = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut theirs = Transaction::begin(&db, GRAPH, OTHER).expect("begin");
    // The SAME element on both branches.
    mine.execute(&[set(1, 10)]).expect("executes");
    theirs.execute(&[set(1, 20)]).expect("executes");

    commit_ok(&mut db, theirs, 2);
    commit_ok(&mut db, mine, 3);

    assert_eq!(prop_of(&db, MAIN, 1), Some(10));
    assert_eq!(prop_of(&db, OTHER, 1), Some(20));
}

// ---------------------------------------------------------------------------
// THE DOCUMENTED DIVERGENCE
// ---------------------------------------------------------------------------

/// WRITE SKEW IS ADMITTED. This is not a bug in the oracle; it is what snapshot
/// isolation is.
///
/// The invariant is "at least one of v1, v2 is nonzero". Both transactions read
/// both vertices, each finds the invariant satisfied, and each zeroes the OTHER
/// one — writing disjoint elements, so the write-write rule sees no conflict.
/// Both commit and the invariant is false afterwards.
///
/// Asserted on the violated invariant and not merely on "both committed",
/// because the anomaly is the point: an implementation that admitted both
/// commits while somehow preserving the invariant would be a different (and
/// better) system, and this law should notice.
#[test]
fn write_skew_is_admitted_under_snapshot_isolation() {
    let mut db = ReferenceDatabase::new();
    seed(&mut db, &[(1, 1), (2, 1)]);

    let mut t1 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    let mut t2 = Transaction::begin(&db, GRAPH, MAIN).expect("begin");

    // Each reads the pair and finds the invariant holds.
    assert_eq!(read(t1.workspace(), 1), Some(1));
    assert_eq!(read(t1.workspace(), 2), Some(1));
    assert_eq!(read(t2.workspace(), 1), Some(1));
    assert_eq!(read(t2.workspace(), 2), Some(1));

    t1.execute(&[set(1, 0)]).expect("executes");
    t2.execute(&[set(2, 0)]).expect("executes");

    commit_ok(&mut db, t1, 2);
    commit_ok(&mut db, t2, 3);

    assert_eq!(prop_of(&db, MAIN, 1), Some(0));
    assert_eq!(prop_of(&db, MAIN, 2), Some(0));
    // The invariant both transactions relied on is now false, and no serial
    // order of the two produces this state. SSI must refuse this history; SI
    // does not, and pretending otherwise is what doctrine 7 forbids.
}

// ---------------------------------------------------------------------------
// Abort, emptiness, and the order of the checks
// ---------------------------------------------------------------------------

/// An aborted transaction is reported as ABORTED even when its writes would also
/// have conflicted. The check order is observable exactly here: reporting a
/// conflict would blame a concurrent writer for a guard this transaction chose.
#[test]
fn an_aborted_transaction_is_not_reported_as_a_conflict() {
    let mut db = seeded();
    let mut doomed = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    // Write the element first, so there IS a conflicting write set...
    doomed.execute(&[set(1, 10)]).expect("executes");
    // ...then abort on a guard that cannot hold.
    doomed
        .execute(&[Statement::new(vec![Intent::CompareAndSet {
            elem: ElementId::Vertex(VId(1)),
            name: PROP,
            expected: Some(int(12345)),
            value: int(0),
            mismatch: MismatchPolicy::TxnAbort,
        }])])
        .expect("executes");
    assert!(doomed.is_aborted());

    // A concurrent commit touches the same element, so a conflict genuinely
    // exists by the time this transaction tries to commit.
    let mut other = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    other.execute(&[set(1, 99)]).expect("executes");
    commit_ok(&mut db, other, 2);

    let outcome = doomed
        .commit(&mut db, REL, SEMANTICS, CommitSeq(3))
        .expect("no error");
    assert_eq!(outcome, TxnOutcome::Aborted { statement: 0 });
    assert_eq!(prop_of(&db, MAIN, 1), Some(99));
}

/// Statements issued after an abort are refused, not silently ignored: a caller
/// still issuing work has misunderstood something, and silence would confirm it.
#[test]
fn a_transaction_cannot_execute_after_it_aborted() {
    let db = seeded();
    let mut txn = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    txn.execute(&[Statement::new(vec![Intent::CompareAndSet {
        elem: ElementId::Vertex(VId(1)),
        name: PROP,
        expected: Some(int(4242)),
        value: int(0),
        mismatch: MismatchPolicy::TxnAbort,
    }])])
    .expect("executes");

    assert_eq!(
        txn.execute(&[set(1, 1)]),
        Err(TxnError::AlreadyAborted { statement: 0 })
    );
}

/// A transaction with no effects commits nothing and is NOT an abort. A caller
/// deciding whether to retry needs to tell "nothing to do" from "refused".
#[test]
fn a_transaction_with_no_effects_is_not_an_abort() {
    let mut db = seeded();
    let mut txn = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    // A no-op write plus a NoOp guard that fails: zero effects, one no-op.
    txn.execute(&[
        set(1, 0),
        Statement::new(vec![Intent::CompareAndSet {
            elem: ElementId::Vertex(VId(1)),
            name: PROP,
            expected: Some(int(777)),
            value: int(1),
            mismatch: MismatchPolicy::NoOp,
        }]),
    ])
    .expect("executes");

    let outcome = txn
        .commit(&mut db, REL, SEMANTICS, CommitSeq(2))
        .expect("no error");
    assert_eq!(
        outcome,
        TxnOutcome::NothingToCommit {
            statement_failures: 0
        }
    );
    assert_eq!(db.applied_through(GRAPH, MAIN), Some(CommitSeq(1)));
}

/// A statement error still commits the surviving statements, and the failure
/// count is reported rather than swallowed.
#[test]
fn a_statement_error_still_commits_the_rest() {
    let mut db = seeded();
    let mut txn = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    txn.execute(&[
        set(1, 5),
        Statement::new(vec![Intent::CompareAndSet {
            elem: ElementId::Vertex(VId(1)),
            name: PROP,
            expected: Some(int(999)),
            value: int(0),
            mismatch: MismatchPolicy::StatementError,
        }]),
        set(2, 6),
    ])
    .expect("executes");

    let (_, effects, failures) = commit_ok(&mut db, txn, 2);
    assert_eq!((effects, failures), (2, 1));
    assert_eq!(prop_of(&db, MAIN, 1), Some(5));
    assert_eq!(prop_of(&db, MAIN, 2), Some(6));
}

// ---------------------------------------------------------------------------
// Historical transactions
// ---------------------------------------------------------------------------

/// A transaction may begin at a HISTORICAL snapshot, and then everything
/// committed since is a potential conflict. That is the honest consequence of
/// first-committer-wins: reading old state is free, writing from it is not.
#[test]
fn a_transaction_begun_in_the_past_conflicts_with_everything_since() {
    let mut db = seeded();
    let mut advance = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    advance.execute(&[set(1, 42)]).expect("executes");
    commit_ok(&mut db, advance, 2);

    let old = db
        .snapshot_at(GRAPH, MAIN, CommitSeq(1))
        .expect("historical snapshot");
    let mut stale = Transaction::begin_at(&db, old).expect("begin");
    assert_eq!(read(stale.workspace(), 1), Some(0), "reads the old value");

    stale.execute(&[set(1, 1)]).expect("executes");
    let outcome = stale
        .commit(&mut db, REL, SEMANTICS, CommitSeq(3))
        .expect("no error");
    assert_eq!(
        outcome,
        TxnOutcome::Conflicted {
            conflicts: vec![ConflictKey::Element(ElementId::Vertex(VId(1)))],
        }
    );

    // But a write to an element nobody touched since is fine.
    let old = db
        .snapshot_at(GRAPH, MAIN, CommitSeq(1))
        .expect("historical snapshot");
    let mut stale_ok = Transaction::begin_at(&db, old).expect("begin");
    stale_ok.execute(&[set(2, 3)]).expect("executes");
    assert!(commit_ok(&mut db, stale_ok, 3).1 > 0);
}

// ---------------------------------------------------------------------------
// The conflict-key partition is total
// ---------------------------------------------------------------------------

/// EVERY row family names at least one conflict key.
///
/// `collect_conflict_keys` has no wildcard arm, so a new `DeltaRow` variant is a
/// compile error — but an arm that inserted nothing would compile fine and
/// silently let two conflicting writes both commit. This is the guard for that:
/// a family whose rows name nothing cannot be detected by reading the match.
#[test]
fn every_row_family_names_a_conflict_key() {
    let rows: Vec<(&str, DeltaRow)> = vec![
        (
            "CreateVertex",
            DeltaRow::CreateVertex {
                vid: VId(1),
                birth_ordinal: 1,
                labels: vec![],
                props: vec![],
                valid_time: None,
            },
        ),
        (
            "CreateEdge",
            DeltaRow::CreateEdge {
                eid: EId(1),
                birth_ordinal: 1,
                src: VId(1),
                relation: REL,
                dst: VId(2),
                canonical_key: None,
                props: vec![],
                valid_time: None,
            },
        ),
        (
            "DeleteVertex",
            DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0; 32]),
                sorted_retired_incident_edges: vec![EId(9)],
            },
        ),
        (
            "DeleteEdge",
            DeltaRow::DeleteEdge {
                eid: EId(1),
                before_version: ObjectId([0; 32]),
            },
        ),
        (
            "LabelMembership",
            DeltaRow::LabelMembership {
                vid: VId(1),
                label: LABEL,
                before: false,
                after: true,
            },
        ),
        (
            "Property",
            DeltaRow::Property {
                elem: ElementId::Vertex(VId(1)),
                property: PROP,
                before: None,
                after: Some(int(1)),
            },
        ),
        (
            "ValidTime",
            DeltaRow::ValidTime {
                elem: ElementId::Vertex(VId(1)),
                contract_id: ObjectId([0; 32]),
                before: None,
                after: None,
            },
        ),
        (
            "Counter",
            DeltaRow::Counter {
                operation_key: operation_key(),
                elem: ElementId::Vertex(VId(1)),
                property: PROP,
                algebra_profile: ObjectId([0; 32]),
                delta: 1,
                before: 0,
                after: 1,
            },
        ),
        (
            "Escrow",
            DeltaRow::Escrow {
                domain_id: EscrowDomainId(1),
                epoch: 0,
                operation_key: operation_key(),
                subject: ElementId::Vertex(VId(1)),
                subject_property: None,
                delta: -1,
                before_value: 1,
                after_value: 0,
            },
        ),
        (
            "Sketch",
            DeltaRow::Sketch {
                operation_key: operation_key(),
                sketch_profile_oid: ObjectId([0; 32]),
                before_state_digest: [0; 32],
                after_state_oid: ObjectId([1; 32]),
            },
        ),
        (
            "Schema",
            DeltaRow::Schema {
                transition_oid: ObjectId([0; 32]),
                before_epoch: SchemaEpoch(0),
                after_epoch: SchemaEpoch(1),
            },
        ),
        (
            "Constraint",
            DeltaRow::Constraint {
                before_schema_root: ObjectId([0; 32]),
                after_schema_root: ObjectId([1; 32]),
                before_constraint_root: ObjectId([0; 32]),
                after_constraint_root: ObjectId([1; 32]),
            },
        ),
    ];

    let mut silent: Vec<&str> = Vec::new();
    for (name, row) in &rows {
        let mut keys = BTreeSet::new();
        collect_conflict_keys(row, &mut keys);
        if keys.is_empty() {
            silent.push(name);
        }
    }
    assert!(
        silent.is_empty(),
        "these families name no conflict key, so two concurrent writes to them \
         would both commit: {silent:?}"
    );
    assert_eq!(rows.len(), 12, "a new row family must be added here too");

    // AND NO ROW MAY NAME CoordinateExistence. It is a claim about the coordinate
    // rather than about its contents — only a genesis transaction asserts it. A
    // row that emitted it would make every ordinary commit collide with every
    // genesis claim, which is a phantom conflict rather than a missed one, so it
    // would show up as unexplained refusals rather than as lost updates.
    for (name, row) in &rows {
        let mut keys = BTreeSet::new();
        collect_conflict_keys(row, &mut keys);
        assert!(
            !keys.contains(&ConflictKey::CoordinateExistence),
            "{name} names coordinate existence, which no row may"
        );
    }
}

/// The escrow row names BOTH its domain and its subject, since it is checked
/// against both.
#[test]
fn an_escrow_row_names_its_domain_and_its_subject() {
    let mut keys = BTreeSet::new();
    collect_conflict_keys(
        &DeltaRow::Escrow {
            domain_id: EscrowDomainId(7),
            epoch: 0,
            operation_key: operation_key(),
            subject: ElementId::Vertex(VId(3)),
            subject_property: None,
            delta: -1,
            before_value: 1,
            after_value: 0,
        },
        &mut keys,
    );
    assert_eq!(
        keys,
        BTreeSet::from([
            ConflictKey::Escrow(EscrowDomainId(7)),
            ConflictKey::Element(ElementId::Vertex(VId(3))),
        ])
    );
}

fn operation_key() -> OperationKey {
    OperationKey([0x5a; 32])
}

fn read(graph: &fgdb_reference::ReferenceGraph, vid: u128) -> Option<i64> {
    match graph.vertex(VId(vid))?.props.get(&PROP)? {
        CanonicalScalar::Int(value) => Some(*value),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Genesis
// ---------------------------------------------------------------------------

/// The first transaction on a branch creates it, and it is recorded as Genesis —
/// so the whole database is reachable through the transaction path rather than
/// needing a template applied behind its back.
#[test]
fn a_genesis_transaction_creates_the_branch() {
    let mut db = ReferenceDatabase::new();
    assert_eq!(db.coordinate_count(), 0);

    let mut txn = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis begin");
    assert_eq!(txn.workspace().vertex_count(), 0, "genesis reads empty");
    txn.execute(&[create(1, 5)]).expect("executes");
    let (seq, effects, _) = commit_ok(&mut db, txn, 1);

    assert_eq!((seq, effects), (CommitSeq(1), 1));
    assert_eq!(
        db.branch_origin(GRAPH, MAIN),
        Some(fgdb_reference::BranchOrigin::Genesis)
    );
    assert_eq!(prop_of(&db, MAIN, 1), Some(5));
}

/// A genesis transaction on a branch that ALREADY exists is refused. The
/// permissive reading is the dangerous one: it would let a mistyped branch name
/// read as a legitimately new branch, and — worse — hand back an empty workspace
/// for a populated coordinate, so the transaction's every before-image would be
/// computed against nothing.
#[test]
fn a_genesis_transaction_on_an_existing_branch_is_refused() {
    let db = seeded();
    // `.err()` rather than comparing the Result: `Transaction` is deliberately
    // neither `Clone` nor `PartialEq`, so there is nothing to compare an `Ok`
    // against — which is the type doing its job.
    assert_eq!(
        Transaction::begin_genesis(&db, GRAPH, MAIN).err(),
        Some(TxnError::Snapshot(
            fgdb_reference::SnapshotError::CoordinateAlreadyExists {
                graph: GRAPH,
                branch: MAIN,
            }
        ))
    );
    // And the ordinary path is the one that works on an existing branch.
    assert!(Transaction::begin(&db, GRAPH, MAIN).is_ok());
}

// ---------------------------------------------------------------------------
// Two defects another pane found in 4f860e9, and the laws that close them
// ---------------------------------------------------------------------------

/// fgdb-reference-historical-fork-conflict-lineage-re6w, the exact history filed.
///
/// A child forked at boundary 2, and a transaction reading the child AS OF 1. The
/// parent's sequence-2 write is VISIBLE to the child through its lineage and
/// occurred after the transaction's basis, but it is absent from the child's own
/// history — so a conflict check consulting only the coordinate's own records
/// reported "disjoint", and the stale before-image then failed at apply time as
/// `TxnError::Apply`. A concurrency outcome wearing the label of an internal
/// contradiction, which is the worst kind of wrong answer: it tells the caller its
/// effects are malformed when in fact it lost a race.
#[test]
fn a_historical_child_transaction_conflicts_with_its_inherited_lineage() {
    let mut db = ReferenceDatabase::new();
    seed(&mut db, &[(1, 0)]);
    let mut advance = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    advance.execute(&[set(1, 1)]).expect("executes");
    commit_ok(&mut db, advance, 2);
    db.fork_branch(GRAPH, MAIN, OTHER).expect("forks at 2");

    let stale = db
        .snapshot_at(GRAPH, OTHER, CommitSeq(1))
        .expect("a historical child snapshot is legal");
    let mut txn = Transaction::begin_at(&db, stale).expect("begin");
    assert_eq!(
        read(txn.workspace(), 1),
        Some(0),
        "it reads the value as of sequence 1"
    );
    txn.execute(&[set(1, 2)]).expect("executes");

    let before = db.clone();
    let outcome = txn
        .commit(&mut db, REL, SEMANTICS, CommitSeq(3))
        .expect("a lost race is an outcome, not an error");
    assert_eq!(
        outcome,
        TxnOutcome::Conflicted {
            conflicts: vec![ConflictKey::Element(ElementId::Vertex(VId(1)))],
        },
        "the inherited sequence-2 write is a conflict"
    );
    assert_eq!(db, before, "and nothing moved");
}

/// The other side of the same window: a child transaction at the CURRENT boundary
/// must NOT conflict with the parent's post-boundary commits.
///
/// Without this law the fix above could be "consult the parent's whole history",
/// which would make every child transaction conflict with a parent that kept
/// working — turning branch isolation into branch serialization. The parent's
/// later commits are not conflicts for the child because they are not visible to
/// it at all.
#[test]
fn a_child_transaction_does_not_conflict_with_the_parents_post_fork_commits() {
    let mut db = ReferenceDatabase::new();
    seed(&mut db, &[(1, 0), (2, 0)]);
    db.fork_branch(GRAPH, MAIN, OTHER).expect("forks at 1");

    // The parent writes the SAME element the child is about to write.
    let mut parent = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    parent.execute(&[set(1, 10)]).expect("executes");
    commit_ok(&mut db, parent, 2);

    let mut child = Transaction::begin(&db, GRAPH, OTHER).expect("begin");
    child.execute(&[set(1, 20)]).expect("executes");
    let (seq, effects, _) = commit_ok(&mut db, child, 3);
    assert_eq!((seq, effects), (CommitSeq(3), 1));
    assert_eq!(prop_of(&db, OTHER, 1), Some(20));
    assert_eq!(
        prop_of(&db, MAIN, 1),
        Some(10),
        "and the parent is untouched"
    );
}

/// Nested forks: the window is per-ancestor, so a grandchild reading below the
/// middle boundary conflicts with the MIDDLE ancestor's inherited writes.
#[test]
fn a_grandchild_conflicts_with_the_middle_ancestors_window() {
    let mut db = ReferenceDatabase::new();
    seed(&mut db, &[(1, 0)]);
    db.fork_branch(GRAPH, MAIN, OTHER).expect("forks at 1");
    let mut middle = Transaction::begin(&db, GRAPH, OTHER).expect("begin");
    middle.execute(&[set(1, 5)]).expect("executes");
    commit_ok(&mut db, middle, 2);
    db.fork_branch(GRAPH, OTHER, NESTED).expect("forks at 2");

    // The grandchild reads as of 1 — below the middle branch's sequence-2 write,
    // which it nonetheless inherits.
    let stale = db
        .snapshot_at(GRAPH, NESTED, CommitSeq(1))
        .expect("historical grandchild snapshot");
    let mut txn = Transaction::begin_at(&db, stale).expect("begin");
    assert_eq!(read(txn.workspace(), 1), Some(0));
    txn.execute(&[set(1, 9)]).expect("executes");
    let outcome = txn
        .commit(&mut db, REL, SEMANTICS, CommitSeq(3))
        .expect("outcome");
    assert!(
        outcome.conflicts().is_some(),
        "the middle ancestor's inherited write is a conflict: {outcome:?}"
    );
}

/// fgdb-reference-genesis-transaction-race-dfk3.
///
/// Two transactions both find the branch absent and both claim to be its first
/// write. Their EFFECTS are disjoint, so no element key catches them — and the
/// loser computed every before-image against a state that no longer exists by the
/// time it commits. Coordinate existence is therefore its own conflict domain: a
/// claim about the coordinate rather than about its contents.
#[test]
fn concurrent_genesis_transactions_cannot_both_create_the_branch() {
    let mut db = ReferenceDatabase::new();
    let mut first = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    let mut second = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    // DISJOINT effects: nothing an element-grained rule could catch.
    first.execute(&[create(1, 1)]).expect("executes");
    second.execute(&[create(2, 2)]).expect("executes");

    let (seq, _, _) = commit_ok(&mut db, first, 1);
    assert_eq!(seq, CommitSeq(1));

    let before = db.clone();
    let outcome = second
        .commit(&mut db, REL, SEMANTICS, CommitSeq(2))
        .expect("outcome");
    assert_eq!(
        outcome,
        TxnOutcome::Conflicted {
            conflicts: vec![ConflictKey::CoordinateExistence],
        },
        "the loser's claim to be the first write has to be able to lose"
    );
    assert_eq!(db, before, "and it appends nothing to the winner's branch");
    assert!(prop_of(&db, MAIN, 2).is_none());
}

/// CONTROL for the law above: an UNCONTESTED genesis transaction still commits.
///
/// Without it, the existence key could be refusing every genesis transaction and
/// the law above would pass for the wrong reason — and nothing would work.
#[test]
fn an_uncontested_genesis_transaction_still_commits() {
    let mut db = ReferenceDatabase::new();
    let mut txn = Transaction::begin_genesis(&db, GRAPH, MAIN).expect("genesis");
    txn.execute(&[create(1, 1)]).expect("executes");
    let (seq, effects, _) = commit_ok(&mut db, txn, 1);
    assert_eq!((seq, effects), (CommitSeq(1), 1));
    assert_eq!(prop_of(&db, MAIN, 1), Some(1));

    // And a second, ORDINARY transaction on the now-existing branch commits too:
    // the existence key must not linger as a permanent conflict.
    let mut next = Transaction::begin(&db, GRAPH, MAIN).expect("begin");
    next.execute(&[create(2, 2)]).expect("executes");
    assert_eq!(commit_ok(&mut db, next, 2).0, CommitSeq(2));
}
