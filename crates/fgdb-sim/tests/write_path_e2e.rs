//! **Reference-effects-to-Chronicle composition fixture.**
//!
//! ```text
//!   fixture statements + caller-supplied reference basis
//!        │  reference evaluation              (fgdb-reference::intents)
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
//! COMPOSE for a verification fixture: reference effects remain byte-identical
//! through Chronicle durability and replay.
//!
//! **SCOPE BOUNDARY:** this is not the database transaction write path. It
//! deliberately lacks authenticated/current basis selection, coordinate-head
//! CAS visibility, final certification, SSI, authorization, resources,
//! constraints/escrow, audit ownership, durable transaction outcome/idempotency,
//! and same-transition live delta-index publication. An abort or empty
//! reference effect set appends no graph marker in this fixture; the real
//! control/outcome plane still owes durable terminal state. Nothing in this file
//! is evidence that those production laws are implemented.

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CommitCoordinator, CrashPoint};
use fgdb_delta_types::{
    CoordinateEntry, ElementId, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_reference::ReferenceGraph;
use fgdb_reference::intents::{Intent, MismatchPolicy, Statement, evaluate};
use fgdb_sim::{commit_capsule, prepare_capsule, replay};
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
const SOURCE_INTENT_ROOT_DIGEST: [u8; 32] = [0x22; 32];
const SCHEMA_EPOCH: SchemaEpoch = SchemaEpoch(7);

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

/// Local result vocabulary for this integration fixture.
///
/// Keeping it in the test is load-bearing: exposing this as a library
/// transaction result would falsely imply that the production lifecycle and
/// outcome planes had run.
#[derive(Debug)]
enum ReferenceEffectFixtureResult {
    MarkerAppended {
        effects: usize,
        statement_failures: usize,
    },
    NoGraphMarker {
        aborted: bool,
        reported_failures: usize,
    },
}

impl ReferenceEffectFixtureResult {
    fn appended_counts(&self) -> Option<(usize, usize)> {
        match self {
            Self::MarkerAppended {
                effects,
                statement_failures,
            } => Some((*effects, *statement_failures)),
            Self::NoGraphMarker { .. } => None,
        }
    }
}

/// Feed one reference evaluation into the durability fixture.
///
/// This test deliberately derives the caller basis from replay so the
/// composition scenario is meaningful. The convention is local to this file
/// and must not be cited as a production current-head guarantee.
fn append_fixture(
    coordinator: &mut CommitCoordinator,
    cx: &CommitCx,
    statements: &[Statement],
) -> ReferenceEffectFixtureResult {
    let basis = replay(cx, coordinator)
        .expect("the stream replays")
        .database
        .graph(GRAPH, BRANCH)
        .cloned()
        .unwrap_or_else(ReferenceGraph::new);
    let outcome = evaluate(&basis, statements);
    let Some((effects, statement_failures)) = outcome.committed_parts() else {
        return ReferenceEffectFixtureResult::NoGraphMarker {
            aborted: true,
            reported_failures: 1,
        };
    };
    if effects.is_empty() {
        return ReferenceEffectFixtureResult::NoGraphMarker {
            aborted: false,
            reported_failures: statement_failures.len(),
        };
    }

    let template = LogicalDeltaTemplate::build(
        INTENT_SEMANTICS,
        SOURCE_INTENT_ROOT_DIGEST,
        vec![CoordinateEntry {
            graph: GRAPH,
            branch: BRANCH,
            relation: REL,
            schema_epoch: SCHEMA_EPOCH,
            schema_transition: None,
            rows: effects.to_vec(),
        }],
    )
    .expect("reference effects are canonical");
    let capsule = prepare_capsule(
        &coordinator.keys().k_oid,
        coordinator.keys().namespace,
        &template,
    )
    .expect("canonical template seals");

    // Empty by design for this fixture: it proves effect-byte durability, not
    // coordinate-head visibility. A real write path must supply and validate
    // the exact head CAS rather than cite this helper as evidence.
    let _marker = commit_capsule(coordinator, cx, &capsule, vec![]).expect("capsule commits");
    ReferenceEffectFixtureResult::MarkerAppended {
        effects: effects.len(),
        statement_failures: statement_failures.len(),
    }
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

/// Reference effects become a durable graph image and survive a restart.
#[test]
fn reference_effects_round_trip_through_chronicle() {
    let dir = scratch_dir("arc");
    under_lab(1, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");

        let first = append_fixture(
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
            ReferenceEffectFixtureResult::MarkerAppended { effects: 3, .. }
        ));

        // The wrapper supplies the first append's replayed state as the second
        // evaluation basis. That is a fixture convention, not library-enforced
        // current-head authority.
        let second = append_fixture(
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
            ReferenceEffectFixtureResult::MarkerAppended { effects: 1, .. }
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
// THE NARROW LAW: abort appends no graph marker in this fixture
// ---------------------------------------------------------------------------

/// A `TxnAbort` guard appends no GRAPH-EFFECT marker/capsule/sequence in this
/// fixture. This does not model the durable control/outcome record a real
/// transaction still owes.
#[test]
fn an_aborted_reference_evaluation_appends_no_graph_marker() {
    let dir = scratch_dir("abort");
    under_lab(2, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        append_fixture(
            &mut coordinator,
            cx,
            &[Statement::new(vec![create(1, "ada")])],
        );
        let markers_before = coordinator.chain().len();
        let seq_before = coordinator.next_commit_seq();
        let capsules_before = std::fs::read_dir(dir.join(fgdb_chronicle::commit::CAPSULE_DIR))
            .expect("capsule dir")
            .count();

        let aborted = append_fixture(
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
                ReferenceEffectFixtureResult::NoGraphMarker {
                    aborted: true,
                    reported_failures: 1
                }
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

/// An ordinary empty reference effect set likewise appends no GRAPH marker and
/// remains distinguishable from an abort. A production outcome plane is out of
/// scope and may still need durable success/idempotency state.
#[test]
fn an_empty_reference_effect_set_appends_no_graph_marker_and_is_not_an_abort() {
    let dir = scratch_dir("empty");
    under_lab(3, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        append_fixture(
            &mut coordinator,
            cx,
            &[Statement::new(vec![create(1, "ada")])],
        );
        let before = coordinator.chain().len();

        // Setting the name to what it already is, plus a NoOp guard that fails.
        let empty = append_fixture(
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
            matches!(
                empty,
                ReferenceEffectFixtureResult::NoGraphMarker {
                    aborted: false,
                    reported_failures: 0
                }
            ),
            "no effects is distinct from aborted; got {empty:?}"
        );
        assert_eq!(coordinator.chain().len(), before);
    });
}

/// A statement error does not stop this fixture from appending the surviving
/// reference effects. The count is an in-memory observation only; this test does
/// not claim a durable transaction outcome.
#[test]
fn a_statement_error_still_appends_the_surviving_reference_effects() {
    let dir = scratch_dir("stmt-error");
    under_lab(4, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        append_fixture(
            &mut coordinator,
            cx,
            &[Statement::new(vec![create(1, "ada")])],
        );

        let mixed = append_fixture(
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
        let (effects, statement_failures) = mixed
            .appended_counts()
            .expect("a graph marker was appended");
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

/// A crash during one fixture append leaves the prior committed prefix, and a
/// later fixture evaluation can use that replayed prefix as its caller basis.
#[test]
fn a_crash_mid_fixture_append_leaves_the_committed_prefix_replayable() {
    let dir = scratch_dir("crash");
    under_lab(5, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        append_fixture(
            &mut coordinator,
            cx,
            &[Statement::new(vec![create(1, "ada")])],
        );

        // Build the next fixture capsule the same way
        // append_fixture would, then crash after D1 so the capsule is durable
        // and unnamed.
        let basis = graph_of(cx, &coordinator);
        let outcome =
            fgdb_reference::intents::evaluate(&basis, &[Statement::new(vec![create(2, "grace")])]);
        let (effects, _) = outcome.committed_parts().expect("committed");
        let template = fgdb_delta_types::LogicalDeltaTemplate::build(
            INTENT_SEMANTICS,
            SOURCE_INTENT_ROOT_DIGEST,
            vec![fgdb_delta_types::CoordinateEntry {
                graph: GRAPH,
                branch: BRANCH,
                relation: REL,
                schema_epoch: SCHEMA_EPOCH,
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

        // And a fresh fixture evaluation against the recovered graph can append
        // normally, so the composition harness is not wedged.
        let mut reopened = reopened;
        let next = append_fixture(
            &mut reopened,
            cx,
            &[Statement::new(vec![create(3, "alan")])],
        );
        assert!(matches!(
            next,
            ReferenceEffectFixtureResult::MarkerAppended { .. }
        ));
        assert_eq!(graph_of(cx, &reopened).vertex_count(), 2);
    });
}
