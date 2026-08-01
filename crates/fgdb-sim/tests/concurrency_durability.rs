//! Where concurrency control meets durability: **the durable stream decides
//! which transactions are in the history.**
//!
//! `fgdb-reference` can now say which histories snapshot isolation admits
//! (`transaction_anomalies.rs`) and which of those SSI must refuse
//! (`ssi_oracle.rs`). Both reason over an in-memory `ReferenceDatabase`. Chronicle
//! separately guarantees that a commit exists exactly when its marker reached D2.
//! Neither layer can state the law that falls out of putting them together, and it
//! is the interesting one:
//!
//! > A transaction whose marker did not reach D2 is **not part of the history**.
//! > So an anomaly it participated in never happened, and the conflicts it held
//! > are released.
//!
//! Both halves are load-bearing and neither is obvious. A crash cannot leave a
//! non-serializable history behind by dropping the transaction that made it
//! serializable — dropping transactions only removes rw edges, so it can only
//! remove dangerous structures. And a crash must not leave *phantom* conflicts:
//! a transaction that was lost before D2 has to stop blocking later writers, or
//! every crash would permanently poison the elements the lost transaction touched.
//!
//! **THE CHECK ORDER IS THE POINT OF THE FIRST LAW.** The conflict decision is
//! made against the RECOVERED state before any capsule is sealed, so a refused
//! transaction leaves nothing on disk — not even an orphan. Sealing first and
//! deciding after is the easy mistake, and it is invisible at the semantic layer:
//! the graph comes out identical either way, because an orphan capsule no marker
//! names contributes nothing to a replay. What it costs is disk that grows with
//! every lost race, and the only test that can see it is one that asks whether
//! the bytes are there.
//!
//! Every fixture here drives the real `Transaction` type against the real
//! recovered database, and the durable authority stays with Chronicle: the
//! in-memory commit is a decision, and the marker is the commit.

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CommitCoordinator, CrashPoint};
use fgdb_delta_types::{
    CoordinateEntry, ElementId, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_reference::intents::{Intent, Statement};
use fgdb_reference::ssi::{TxnTrace, dangerous_structures};
use fgdb_reference::txn::{Transaction, TxnOutcome};
use fgdb_reference::{ReferenceDatabase, ReferenceGraph};
use fgdb_sim::{commit_capsule, prepare_capsule, replay};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, GraphId, ObjectId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const PROP: PropertyKeyId = PropertyKeyId(100);
const SCHEMA_EPOCH: SchemaEpoch = SchemaEpoch(0);
const INTENT_SEMANTICS: ObjectId = ObjectId([0x11; 32]);
const SOURCE_INTENT_ROOT_DIGEST: [u8; 32] = [0x22; 32];
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> CapsuleKeys {
    CapsuleKeys {
        k_oid: K_OID,
        namespace: NAMESPACE,
        dek: [0x3c; 32],
        object_kind: fgdb_sim::CAPSULE_OBJECT_KIND,
        profile: CapsuleProfile::balanced(),
    }
}

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-conc-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn under_lab<T: Send + 'static>(
    seed: u64,
    test: impl FnOnce(&CommitCx) -> T + Send + 'static,
) -> T {
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(&contexts.commit())
    });
    assert!(
        // lab_test_passed() covers ALL THREE channels — quiescence, the full
        // 24-oracle suite, and the mirrored invariant list — not merely the
        // 7 temporals mirrored into invariant_violations (fresh-eyes I3: an
        // oracle-only failure otherwise stayed green here).
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

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

/// The recovered database — the only authority a transaction may read.
fn recovered(cx: &CommitCx, coordinator: &CommitCoordinator) -> ReferenceDatabase {
    replay(cx, coordinator)
        .expect("the stream replays")
        .database
}

fn graph_of(cx: &CommitCx, coordinator: &CommitCoordinator) -> ReferenceGraph {
    recovered(cx, coordinator)
        .graph(GRAPH, BRANCH)
        .cloned()
        .unwrap_or_else(ReferenceGraph::new)
}

/// The sequence Chronicle will assign next, derived from the recovered stream.
fn next_seq(cx: &CommitCx, coordinator: &CommitCoordinator) -> CommitSeq {
    let applied = recovered(cx, coordinator)
        .applied_through(GRAPH, BRANCH)
        .map_or(0, |seq| seq.0);
    CommitSeq(applied + 1)
}

fn template_for(rows: Vec<fgdb_delta_types::DeltaRow>) -> LogicalDeltaTemplate {
    LogicalDeltaTemplate::build(
        INTENT_SEMANTICS,
        SOURCE_INTENT_ROOT_DIGEST,
        vec![CoordinateEntry {
            graph: GRAPH,
            branch: BRANCH,
            relation: REL,
            schema_epoch: SCHEMA_EPOCH,
            schema_transition: None,
            rows,
        }],
    )
    .expect("reference effects are canonical")
}

/// Begin a transaction against the recovered state, creating the coordinate on
/// the first call.
fn begin(db: &ReferenceDatabase) -> Transaction {
    Transaction::begin(db, GRAPH, BRANCH)
        .or_else(|_| Transaction::begin_genesis(db, GRAPH, BRANCH))
        .expect("begin against the recovered state")
}

/// What one durable transaction attempt did.
struct Attempt {
    outcome: TxnOutcome,
    trace: TxnTrace,
    /// The capsule that WOULD have been sealed, always computed so a refused
    /// attempt can be asked whether its bytes reached the disk.
    capsule_oid: Option<ObjectId>,
}

/// Run `txn` to completion: decide against the recovered state, and only then
/// seal and append.
///
/// The decision is made on a CLONE of the recovered database, which is then
/// discarded: the durable stream is re-derived by replay, so nothing in memory
/// can outlive a crash. Passing the live database would let the harness carry
/// state across a crash the database itself would not.
fn finish(
    coordinator: &mut CommitCoordinator,
    cx: &CommitCx,
    txn: Transaction,
    id: usize,
    crash: Option<CrashPoint>,
) -> Attempt {
    let trace = txn.trace(id);
    let seq = next_seq(cx, coordinator);
    let effects = txn.effects().to_vec();
    let mut decision_basis = recovered(cx, coordinator);
    // The logical command sequence MATCHES the commit sequence here, and that is
    // required rather than convenient: `marker_for_capsule` writes
    // `logical_command_seq: commit_seq` into the marker, and replay reads the
    // logical sequence back out of it. A fixture that passed a different value
    // would make the in-memory decision basis disagree with the durable stream it
    // is supposed to mirror.
    let outcome = txn
        .commit(
            &mut decision_basis,
            REL,
            INTENT_SEMANTICS,
            seq,
            fgdb_types::LogicalCommandSeq(seq.0),
        )
        .expect("commit decision");

    // PREPARED EVEN WHEN REFUSED, and that is not a wasted step: preparing is
    // pure — it computes bytes and their identity in memory and touches no disk —
    // so a refused attempt can name the exact object it WOULD have written and
    // the test can ask whether those bytes are there. Without the identity, "no
    // capsule was sealed" can only be asserted about the absence of a marker,
    // which the abort laws already cover.
    let template = template_for(effects);
    let capsule = prepare_capsule(&K_OID, NAMESPACE, &template).expect("canonical template seals");
    let capsule_oid = capsule.object_id;

    if !outcome.is_committed() {
        return Attempt {
            outcome,
            trace,
            capsule_oid: Some(capsule_oid),
        };
    }

    match crash {
        None => {
            commit_capsule(coordinator, cx, &capsule, vec![]).expect("capsule commits");
            Attempt {
                outcome,
                trace: trace.committed_at(seq),
                capsule_oid: Some(capsule_oid),
            }
        }
        Some(point) => {
            let crashed = coordinator.commit_with_crash(
                cx,
                &capsule.bytes,
                |s, oid| fgdb_sim::marker_for_capsule(s, oid, &capsule, vec![]),
                Some(point),
            );
            assert!(crashed.is_err(), "the injected crash must fail the commit");
            // NOT marked committed: the marker never reached D2, so this
            // transaction is not in the history however far the write got.
            Attempt {
                outcome,
                trace,
                capsule_oid: Some(capsule_oid),
            }
        }
    }
}

/// THE CHECK-ORDER LAW: a refused transaction leaves NOTHING on disk.
///
/// Two transactions read the same durable state and write the same element. The
/// second is refused, and the capsule it would have sealed is asked for by name —
/// which is the only assertion that can see the difference. Sealing before
/// deciding produces an identical graph, because an orphan no marker names
/// contributes nothing to a replay; what it produces is disk that grows with
/// every lost race.
#[test]
fn a_refused_transaction_seals_no_capsule() {
    let dir = scratch_dir("refused");
    under_lab(11, move |cx| {
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        let db = recovered(cx, &coordinator);
        let mut seed = begin(&db);
        seed.execute(&[create(1, 0), create(2, 0)])
            .expect("executes");
        assert!(
            finish(&mut coordinator, cx, seed, 0, None)
                .outcome
                .is_committed()
        );

        // Both read the same durable state.
        let db = recovered(cx, &coordinator);
        let mut first = begin(&db);
        let mut second = begin(&db);
        first.execute(&[set(1, 10)]).expect("executes");
        second.execute(&[set(1, 20)]).expect("executes");

        let won = finish(&mut coordinator, cx, first, 1, None);
        let lost = finish(&mut coordinator, cx, second, 2, None);
        assert!(won.outcome.is_committed());
        assert!(lost.outcome.conflicts().is_some(), "SI refuses the second");

        // THE ASSERTION THAT CAN SEE IT: the exact object the refused
        // transaction would have written is absent from the store.
        let would_have = lost.capsule_oid.expect("its capsule identity is known");
        assert!(
            !coordinator.capsule_exists(cx, would_have),
            "the refused transaction's bytes reached the disk"
        );
        assert!(
            coordinator.capsule_exists(cx, won.capsule_oid.expect("prepared")),
            "while the winner's did — otherwise the check above is vacuous"
        );
        assert_eq!(
            coordinator.orphan_capsules(cx).expect("scan"),
            vec![],
            "a refused transaction leaves no orphan"
        );
        assert_eq!(graph_of(cx, &coordinator).vertex_count(), 2);
        assert_eq!(
            recovered(cx, &coordinator).applied_through(GRAPH, BRANCH),
            Some(CommitSeq(2)),
            "one seed commit and one winner — the loser consumed no sequence"
        );
    });
}

/// THE MARQUEE LAW: durability decides membership in the SSI history.
///
/// A write-skew pair, both admitted by SI. The second one's marker never reaches
/// D2, so it is not in the history — and the anomaly it was half of never
/// happened. The full attempted history HAS a dangerous structure and the durable
/// history does NOT, from the same two traces, differing only in whether the
/// second is marked committed.
#[test]
fn durability_decides_membership_in_the_ssi_history() {
    let dir = scratch_dir("membership");
    under_lab(12, move |cx| {
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        let db = recovered(cx, &coordinator);
        let mut seed = begin(&db);
        seed.execute(&[create(1, 1), create(2, 1)])
            .expect("executes");
        finish(&mut coordinator, cx, seed, 0, None);

        // Write skew: each reads the invariant, each zeroes the other's element.
        let db = recovered(cx, &coordinator);
        let mut t1 = begin(&db);
        let mut t2 = begin(&db);
        for vid in [1, 2] {
            t1.read_property(ElementId::Vertex(VId(vid)), PROP);
            t2.read_property(ElementId::Vertex(VId(vid)), PROP);
        }
        t1.execute(&[set(1, 0)]).expect("executes");
        t2.execute(&[set(2, 0)]).expect("executes");

        let a = finish(&mut coordinator, cx, t1, 1, None);
        // t2's marker never reaches D2.
        let b = finish(&mut coordinator, cx, t2, 2, Some(CrashPoint::AfterD1));
        assert!(a.outcome.is_committed() && b.outcome.is_committed());
        assert_eq!(b.trace.commit_seq, None, "the lost commit is not committed");

        drop(coordinator);
        let reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");

        // The recovered graph carries only the first transaction's effect.
        let graph = graph_of(cx, &reopened);
        assert_eq!(prop_of(&graph, 1), Some(0), "t1 landed");
        assert_eq!(prop_of(&graph, 2), Some(1), "t2 did not");
        assert!(
            reopened.capsule_exists(cx, b.capsule_oid.expect("prepared")),
            "its capsule is durable — the orphan is real, and still not a commit"
        );

        let durable = vec![a.trace.clone(), b.trace.clone()];
        assert_eq!(
            dangerous_structures(&durable),
            vec![],
            "the anomaly needed both transactions, and only one is in the history"
        );

        // CONTROL: had the second marker reached D2, the very same traces form a
        // structure. So the all-clear above is about durability and not about the
        // history being harmless.
        let as_if = vec![a.trace, b.trace.committed_at(CommitSeq(3))];
        assert_eq!(
            dangerous_structures(&as_if).len(),
            1,
            "the anomaly is exactly one D2 away"
        );
    });
}

/// A crash RELEASES the conflicts of the transaction it lost.
///
/// The mirror of the law above, and the half that is easy to get wrong in the
/// other direction: a lost transaction must stop blocking later writers. If the
/// conflict rule consulted anything that survived the crash — a cached write set,
/// an orphan capsule's contents — every crash would permanently poison the
/// elements the lost transaction touched.
#[test]
fn a_crash_releases_the_lost_transactions_conflicts() {
    let dir = scratch_dir("release");
    under_lab(13, move |cx| {
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        let db = recovered(cx, &coordinator);
        let mut seed = begin(&db);
        seed.execute(&[create(1, 0)]).expect("executes");
        finish(&mut coordinator, cx, seed, 0, None);

        // A transaction writes v1 and is lost after D1.
        let db = recovered(cx, &coordinator);
        let mut lost = begin(&db);
        lost.execute(&[set(1, 50)]).expect("executes");
        let lost = finish(&mut coordinator, cx, lost, 1, Some(CrashPoint::AfterD1));
        assert!(lost.outcome.is_committed(), "it decided to commit");

        drop(coordinator);
        let mut reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");

        // A later transaction writes the SAME element. It must not conflict: the
        // write that would have conflicted is not in the history.
        let db = recovered(cx, &reopened);
        assert_eq!(
            db.applied_through(GRAPH, BRANCH),
            Some(CommitSeq(1)),
            "the frontier is where the surviving prefix ends"
        );
        let mut later = begin(&db);
        later.execute(&[set(1, 99)]).expect("executes");
        let later = finish(&mut reopened, cx, later, 2, None);
        assert!(
            later.outcome.is_committed(),
            "a lost transaction must not poison the element it touched: {:?}",
            later.outcome
        );
        assert_eq!(prop_of(&graph_of(cx, &reopened), 1), Some(99));
    });
}

/// The oracle's sequence and Chronicle's agree.
///
/// The conflict decision is made under a sequence predicted from the recovered
/// stream, and the marker is written under one the coordinator assigns. Nothing
/// forces those to be the same number, so it is asserted rather than assumed: a
/// drift would make every conflict check evaluate a window one commit wide of the
/// truth.
#[test]
fn the_predicted_sequence_matches_the_one_chronicle_assigns() {
    let dir = scratch_dir("seq");
    under_lab(14, move |cx| {
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        for round in 0..4u128 {
            let db = recovered(cx, &coordinator);
            let predicted = next_seq(cx, &coordinator);
            let mut txn = begin(&db);
            txn.execute(&[create(round + 1, round as i64)])
                .expect("executes");
            let attempt = finish(&mut coordinator, cx, txn, round as usize, None);
            let (seq, _, _) = attempt
                .outcome
                .committed_parts()
                .expect("each round commits");
            assert_eq!(seq, predicted);
            assert_eq!(
                recovered(cx, &coordinator).applied_through(GRAPH, BRANCH),
                Some(predicted),
                "and the durable stream agrees with both"
            );
        }
    });
}

/// Dangerous structures are MONOTONE under history restriction: a prefix has no
/// structure the whole history lacks.
///
/// This is the general property the membership law relies on — a crash truncates
/// the history, and truncation only removes rw edges. Pinned separately because
/// the membership law would still pass if this were false in some other case, and
/// the consequence of it being false is the worst one available: a crash that
/// manufactures an anomaly.
#[test]
fn dangerous_structures_are_monotone_under_restriction() {
    let dir = scratch_dir("monotone");
    under_lab(15, move |cx| {
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        let db = recovered(cx, &coordinator);
        let mut seed = begin(&db);
        seed.execute(&[create(1, 1), create(2, 1)])
            .expect("executes");
        finish(&mut coordinator, cx, seed, 0, None);

        let db = recovered(cx, &coordinator);
        let mut t1 = begin(&db);
        let mut t2 = begin(&db);
        for vid in [1, 2] {
            t1.read_property(ElementId::Vertex(VId(vid)), PROP);
            t2.read_property(ElementId::Vertex(VId(vid)), PROP);
        }
        t1.execute(&[set(1, 0)]).expect("executes");
        t2.execute(&[set(2, 0)]).expect("executes");
        let a = finish(&mut coordinator, cx, t1, 1, None);
        let b = finish(&mut coordinator, cx, t2, 2, None);

        let full = vec![a.trace, b.trace];
        let whole = dangerous_structures(&full);
        assert_eq!(whole.len(), 1, "the full history has the anomaly");
        for cut in 0..full.len() {
            let prefix = &full[..cut];
            for structure in dangerous_structures(prefix) {
                assert!(
                    whole.contains(&structure),
                    "restriction manufactured {structure:?}"
                );
            }
        }
    });
}

fn prop_of(graph: &ReferenceGraph, vid: u128) -> Option<i64> {
    match graph.vertex(VId(vid))?.props.get(&PROP)? {
        CanonicalScalar::Int(value) => Some(*value),
        _ => None,
    }
}
