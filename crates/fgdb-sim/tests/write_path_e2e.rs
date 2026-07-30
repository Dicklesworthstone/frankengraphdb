//! **The whole write path, end to end.**
//!
//! ```text
//!   statements of intents
//!        │  finalization against current state  (fgdb-reference::intents)
//!        ▼
//!   canonical effects
//!        │  canonical encoding                 (fgdb-delta-types)
//!        ▼
//!   LogicalDeltaTemplate
//!        │  seal + erasure-code                 (fgdb-chronicle::capsule)
//!        ▼
//!   capsule  ──▶ D1 ──▶ marker ──▶ D2           (fgdb-chronicle::commit)
//!        │  crash, restart, recover
//!        ▼
//!   materialized graph                          (fgdb-reference)
//! ```
//!
//! Every stage has its own laws elsewhere. What this file adds is that they
//! COMPOSE: a user-level transaction produces exactly the graph it implied,
//! after surviving a crash it never knew about.
//!
//! THE LAW THAT NEEDS THIS FILE TO EXIST: **an aborted transaction writes
//! nothing durable.** Not an empty capsule, not a marker with no effects —
//! nothing. It is easy to build the capsule before evaluating the guard and let
//! the commit proceed with an empty effect set, which leaves the stream carrying
//! a commit that happened for no reason and inflates every sequence after it.
//! No single-layer test can catch that, because each layer behaves correctly in
//! isolation.

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CommitCoordinator, CrashPoint};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_reference::intents::{Intent, MismatchPolicy, Statement};
use fgdb_sim::{TransactionCommit, commit_transaction, replay};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, ObjectId, VId};
use std::path::PathBuf;

const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const REL: RelationId = RelationId(1);
const LABEL: LabelId = LabelId(10);
const NAME: PropertyKeyId = PropertyKeyId(100);
const INTENT_SEMANTICS: ObjectId = ObjectId([0x11; 32]);

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
    let dir = std::env::temp_dir().join(format!("fgdb-writepath-{}-{name}", std::process::id()));
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
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    output
}

fn text(v: &str) -> CanonicalScalar {
    CanonicalScalar::Text(fgdb_types::CanonicalText::new_ucs_basic(v).expect("bounded"))
}

fn create(vid: u128, name: &str) -> Intent {
    Intent::CreateVertex {
        vid: VId(vid),
        labels: vec![LABEL],
        props: vec![(NAME, text(name))],
    }
}

fn add_edge(eid: u128, src: u128, dst: u128) -> Intent {
    Intent::AddEdge {
        eid: EId(eid),
        src: VId(src),
        etype: REL,
        dst: VId(dst),
        props: vec![],
    }
}

fn cas(vid: u128, expected: &str, value: &str, policy: MismatchPolicy) -> Intent {
    Intent::CompareAndSet {
        elem: ElementId::Vertex(VId(vid)),
        name: NAME,
        expected: Some(text(expected)),
        value: text(value),
        mismatch: policy,
    }
}

/// Commit one transaction against the graph the durable stream currently
/// implies. Reading the basis back from the stream on every call is the point:
/// finalization must run against committed state, not against whatever the test
/// happens to be holding.
fn commit_txn(
    coordinator: &mut CommitCoordinator,
    cx: &CommitCx,
    statements: &[Statement],
) -> TransactionCommit {
    let basis = replay(cx, coordinator)
        .expect("the stream replays")
        .database
        .graph(GRAPH, BRANCH)
        .cloned()
        .unwrap_or_else(ReferenceGraph::new);
    commit_transaction(
        coordinator,
        cx,
        &basis,
        statements,
        (GRAPH, BRANCH, REL),
        INTENT_SEMANTICS,
    )
    .expect("commit path")
}

fn graph_of(cx: &CommitCx, coordinator: &CommitCoordinator) -> ReferenceGraph {
    replay(cx, coordinator)
        .expect("replays")
        .database
        .graph(GRAPH, BRANCH)
        .cloned()
        .unwrap_or_else(ReferenceGraph::new)
}

// ---------------------------------------------------------------------------
// THE ARC
// ---------------------------------------------------------------------------

/// A user transaction becomes a durable graph, and survives a restart.
#[test]
fn intents_become_a_durable_graph() {
    let dir = scratch_dir("arc");
    under_lab(1, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");

        let first = commit_txn(
            &mut coordinator,
            cx,
            &[Statement::new(vec![
                create(1, "ada"),
                create(2, "grace"),
                add_edge(10, 1, 2),
            ])],
        );
        assert!(matches!(
            first,
            TransactionCommit::Committed { effects: 3, .. }
        ));

        // A second transaction finalized against the FIRST one's committed
        // state: the CAS expects what the stream says, not what the test knows.
        let second = commit_txn(
            &mut coordinator,
            cx,
            &[Statement::new(vec![cas(
                1,
                "ada",
                "ada-lovelace",
                MismatchPolicy::StatementError,
            )])],
        );
        assert!(matches!(
            second,
            TransactionCommit::Committed { effects: 1, .. }
        ));
        drop(coordinator);

        // Restart: nothing in memory, everything from disk.
        let reopened = CommitCoordinator::open(&dir, keys()).expect("reopen");
        let graph = graph_of(cx, &reopened);
        assert_eq!(graph.vertex_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        assert_eq!(
            graph.vertex(VId(1)).expect("ada").props.get(&NAME),
            Some(&text("ada-lovelace")),
            "the CAS from transaction 2 is durable"
        );
        assert_eq!(graph.neighbours(VId(1), REL), vec![VId(2)]);
        assert_eq!(reopened.chain().len(), 2, "two commits, two markers");
    });
}

// ---------------------------------------------------------------------------
// THE LAW: an aborted transaction writes nothing
// ---------------------------------------------------------------------------

/// A `TxnAbort` guard that fires must leave the durable stream completely
/// untouched — no marker, no capsule, no sequence consumed. An implementation
/// that sealed the capsule before evaluating the guard would leave a commit in
/// the stream that happened for no reason.
#[test]
fn an_aborted_transaction_writes_nothing_durable() {
    let dir = scratch_dir("abort");
    under_lab(2, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_txn(
            &mut coordinator,
            cx,
            &[Statement::new(vec![create(1, "ada")])],
        );
        let markers_before = coordinator.chain().len();
        let seq_before = coordinator.next_commit_seq();
        let capsules_before = std::fs::read_dir(dir.join(fgdb_chronicle::commit::CAPSULE_DIR))
            .expect("capsule dir")
            .count();

        let aborted = commit_txn(
            &mut coordinator,
            cx,
            &[
                // This statement would succeed on its own.
                Statement::new(vec![create(2, "grace")]),
                // And this guard aborts the whole transaction.
                Statement::new(vec![cas(1, "WRONG", "never", MismatchPolicy::TxnAbort)]),
            ],
        );
        assert!(
            matches!(
                aborted,
                TransactionCommit::NothingToCommit { aborted: true }
            ),
            "got {aborted:?}"
        );

        assert_eq!(
            coordinator.chain().len(),
            markers_before,
            "no marker was written"
        );
        assert_eq!(
            coordinator.next_commit_seq(),
            seq_before,
            "no sequence was consumed"
        );
        assert_eq!(
            std::fs::read_dir(dir.join(fgdb_chronicle::commit::CAPSULE_DIR))
                .expect("capsule dir")
                .count(),
            capsules_before,
            "no capsule was sealed — not even an orphan"
        );

        // And the graph is exactly what the first transaction left, so the
        // aborted statement that WOULD have succeeded left no trace either.
        let graph = graph_of(cx, &coordinator);
        assert_eq!(graph.vertex_count(), 1);
        assert!(graph.vertex(VId(2)).is_none());
    });
}

/// A transaction that finalizes to zero effects for ordinary reasons — every
/// write was a no-op — also writes nothing. There is nothing to record, so
/// nothing is recorded, and this is NOT an abort.
#[test]
fn a_transaction_with_no_effects_writes_nothing_and_is_not_an_abort() {
    let dir = scratch_dir("empty");
    under_lab(3, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_txn(
            &mut coordinator,
            cx,
            &[Statement::new(vec![create(1, "ada")])],
        );
        let before = coordinator.chain().len();

        // Setting the name to what it already is, plus a NoOp guard that fails.
        let empty = commit_txn(
            &mut coordinator,
            cx,
            &[Statement::new(vec![
                Intent::SetProp {
                    elem: ElementId::Vertex(VId(1)),
                    name: NAME,
                    value: text("ada"),
                },
                cas(1, "WRONG", "never", MismatchPolicy::NoOp),
            ])],
        );
        assert!(
            matches!(empty, TransactionCommit::NothingToCommit { aborted: false }),
            "no effects is distinct from aborted; got {empty:?}"
        );
        assert_eq!(coordinator.chain().len(), before);
    });
}

/// A statement error does NOT stop the transaction from committing: the
/// surviving statements' effects are durable and the failure count is reported.
/// This is the case that distinguishes StatementError from TxnAbort all the way
/// through to disk.
#[test]
fn a_statement_error_still_commits_the_surviving_statements() {
    let dir = scratch_dir("stmt-error");
    under_lab(4, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_txn(
            &mut coordinator,
            cx,
            &[Statement::new(vec![create(1, "ada")])],
        );

        let mixed = commit_txn(
            &mut coordinator,
            cx,
            &[
                Statement::new(vec![create(2, "grace")]),
                Statement::new(vec![cas(
                    1,
                    "WRONG",
                    "never",
                    MismatchPolicy::StatementError,
                )]),
                Statement::new(vec![create(3, "alan")]),
            ],
        );
        let (effects, statement_failures) =
            mixed.committed_counts().expect("the transaction committed");
        assert_eq!(effects, 2, "statements 0 and 2 produced effects");
        assert_eq!(statement_failures, 1, "statement 1 failed");

        let graph = graph_of(cx, &coordinator);
        assert_eq!(graph.vertex_count(), 3, "ada, grace and alan are durable");
        assert_eq!(
            graph.vertex(VId(1)).expect("ada").props.get(&NAME),
            Some(&text("ada")),
            "and the failed CAS changed nothing"
        );
    });
}

// ---------------------------------------------------------------------------
// Crash composition
// ---------------------------------------------------------------------------

/// A crash during a transaction's commit leaves the previous transactions'
/// graph, and the next transaction finalizes against THAT — so a crash cannot
/// produce a graph that no sequence of transactions could have produced.
#[test]
fn a_crash_mid_transaction_leaves_a_reachable_graph() {
    let dir = scratch_dir("crash");
    under_lab(5, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_txn(
            &mut coordinator,
            cx,
            &[Statement::new(vec![create(1, "ada")])],
        );

        // Build the next transaction's capsule the same way commit_transaction
        // would, then crash after D1 so the capsule is durable and unnamed.
        let basis = graph_of(cx, &coordinator);
        let outcome =
            fgdb_reference::intents::evaluate(&basis, &[Statement::new(vec![create(2, "grace")])]);
        let (effects, _) = outcome.committed_parts().expect("committed");
        let template = fgdb_delta_types::LogicalDeltaTemplate::build(
            INTENT_SEMANTICS,
            [0u8; 32],
            vec![fgdb_delta_types::CoordinateEntry {
                graph: GRAPH,
                branch: BRANCH,
                relation: REL,
                schema_epoch: fgdb_delta_types::SchemaEpoch(0),
                schema_transition: None,
                rows: effects.to_vec(),
            }],
        )
        .expect("builds");
        let capsule = fgdb_sim::prepare_capsule(&K_OID, NAMESPACE, &template).expect("prepares");
        let crashed = coordinator.commit_with_crash(
            cx,
            &capsule.bytes,
            |seq, oid| fgdb_sim::marker_for_capsule(seq, oid, &capsule, vec![]),
            Some(CrashPoint::AfterD1),
        );
        assert!(crashed.is_err());
        drop(coordinator);

        let reopened = CommitCoordinator::open(&dir, keys()).expect("reopen");
        let graph = graph_of(cx, &reopened);
        assert_eq!(
            graph.vertex_count(),
            1,
            "the crashed transaction contributed nothing"
        );
        assert!(
            reopened.capsule_exists(capsule.object_id),
            "though its capsule is durable — the orphan is real"
        );
        assert_eq!(
            reopened.orphan_capsules().expect("scan"),
            vec![capsule.object_id]
        );

        // And a fresh transaction now finalizes against the recovered graph and
        // commits normally, so the crash did not wedge the database.
        let mut reopened = reopened;
        let next = commit_txn(
            &mut reopened,
            cx,
            &[Statement::new(vec![create(3, "alan")])],
        );
        assert!(matches!(next, TransactionCommit::Committed { .. }));
        assert_eq!(graph_of(cx, &reopened).vertex_count(), 2);
    });
}
