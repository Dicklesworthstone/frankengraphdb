//! **Durability versus semantics: the end-to-end differential.**
//!
//! Chronicle proves bytes survive a crash without asking what they mean. The
//! reference oracle proves rows materialize without asking whether those rows
//! were ever durable. Each is only plausible alone. This file asserts the
//! sentence neither can state:
//!
//! > The graph you get after a crash is **exactly** the graph implied by the
//! > commits that reached D2.
//!
//! Both directions matter and fail differently. A superset means an orphan
//! capsule — bytes that are on disk, readable, and decodable — leaked into
//! state that no commit acknowledged. A subset means an acknowledged commit was
//! lost. The first is the more dangerous, because everything about the orphan
//! looks valid; the only thing wrong with it is that no marker names it.
//!
//! Everything runs under asupersync's lab runtime with a real `CommitCx`.

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::commit::{CommitCoordinator, CrashPoint};
use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_reference::ReferenceDatabase;
use fgdb_sim::{PreparedCapsule, ReplayError, commit_capsule, materialize, prepare_capsule};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, ObjectId, VId};
use std::path::{Path, PathBuf};

const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const REL_KNOWS: RelationId = RelationId(1);
const LABEL_PERSON: LabelId = LabelId(10);
const PROP_NAME: PropertyKeyId = PropertyKeyId(100);

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-sim-e2e-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::Text(fgdb_types::CanonicalText::new_ucs_basic(value).expect("bounded text"))
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

fn template_of(rows: Vec<DeltaRow>) -> LogicalDeltaTemplate {
    LogicalDeltaTemplate::build(
        ObjectId([0x11; 32]),
        [0x22; 32],
        vec![CoordinateEntry {
            graph: GRAPH,
            branch: BRANCH,
            relation: REL_KNOWS,
            schema_epoch: SchemaEpoch(0),
            schema_transition: None,
            rows,
        }],
    )
    .expect("template builds")
}

fn capsule_of(rows: Vec<DeltaRow>) -> PreparedCapsule {
    prepare_capsule(&K_OID, NAMESPACE, &template_of(rows)).expect("capsule prepares")
}

fn person(vid: u128, ordinal: u64, name: &str) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: ordinal,
        labels: vec![LABEL_PERSON],
        props: vec![(PROP_NAME, text(name))],
        valid_time: None,
    }
}

fn knows(eid: u128, ordinal: u64, src: u128, dst: u128) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: ordinal,
        src: VId(src),
        relation: REL_KNOWS,
        dst: VId(dst),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}

/// The three commits every test below builds on: two people, then a third, then
/// the edges between them. Split across commits on purpose — a single commit
/// would not exercise the chain at all.
fn three_commits() -> [PreparedCapsule; 3] {
    [
        capsule_of(vec![person(1, 1, "ada"), person(2, 2, "grace")]),
        capsule_of(vec![person(3, 3, "alan")]),
        capsule_of(vec![knows(10, 4, 1, 2), knows(11, 5, 1, 3)]),
    ]
}

/// `(vertices, edges)` after N of the three commits, or `None` when no
/// coordinate should exist at all. A table rather than a match arm per case, so
/// the "how many commits landed" question has exactly one answer per N and a
/// test cannot quietly assert against a case nobody defined.
const EXPECTED_AFTER: [Option<(usize, usize)>; 4] =
    [None, Some((2, 0)), Some((3, 0)), Some((3, 2))];

fn expect_graph_after(database: &ReferenceDatabase, commits: usize) {
    let expected = EXPECTED_AFTER
        .get(commits)
        .copied()
        .expect("an expectation is defined for this many commits");
    let graph = database.graph(GRAPH, BRANCH);
    match expected {
        None => assert!(
            graph.is_none(),
            "no committed template means no coordinate at all"
        ),
        Some((vertices, edges)) => {
            let g = graph.expect("coordinate exists");
            assert_eq!(
                g.vertex_count(),
                vertices,
                "vertices after {commits} commits"
            );
            assert_eq!(g.edge_count(), edges, "edges after {commits} commits");
        }
    }
    if commits == 3 {
        assert_eq!(
            graph.expect("coordinate").neighbours(VId(1), REL_KNOWS),
            vec![VId(2), VId(3)],
            "ada knows grace and alan"
        );
    }
}

// ---------------------------------------------------------------------------
// The milestone
// ---------------------------------------------------------------------------

/// A graph survives the process that made it. The coordinator is dropped and
/// the directory is reopened from its path alone — a restart, as the files see
/// it — and the graph is rebuilt from durable bytes with nothing carried over
/// in memory.
#[test]
fn a_graph_committed_to_disk_is_rebuilt_after_a_restart() {
    let dir = scratch_dir("restart");
    under_lab(1, move |cx| {
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(&dir).expect("open");
        for capsule in &capsules {
            commit_capsule(&mut coordinator, cx, capsule, vec![]).expect("commit");
        }
        drop(coordinator);

        let reopened = CommitCoordinator::open(&dir).expect("reopen");
        assert_eq!(reopened.chain().len(), 3);
        let database = materialize(&reopened).expect("materializes");
        expect_graph_after(&database, 3);

        // The property survived the whole round trip: encode, seal into a
        // capsule, fsync, recover, decode, materialize.
        assert_eq!(
            database
                .graph(GRAPH, BRANCH)
                .expect("coordinate")
                .vertex(VId(1))
                .expect("ada")
                .props
                .get(&PROP_NAME),
            Some(&text("ada"))
        );
    });
}

/// Materializing is a function of the durable bytes alone.
#[test]
fn materializing_twice_yields_identical_state() {
    let dir = scratch_dir("deterministic");
    under_lab(2, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir).expect("open");
        for capsule in &three_commits() {
            commit_capsule(&mut coordinator, cx, capsule, vec![]).expect("commit");
        }
        assert_eq!(
            materialize(&coordinator).expect("first"),
            materialize(&coordinator).expect("second")
        );
    });
}

// ---------------------------------------------------------------------------
// THE LAW: the graph equals the committed prefix, at every crash instant
// ---------------------------------------------------------------------------

/// Crash at each protocol instant during the THIRD commit and assert the
/// recovered graph is exactly the two-commit graph.
///
/// The `AfterD1` case is the sharp one: the third capsule's bytes are durable,
/// readable, and would decode into a perfectly valid template — the ONLY thing
/// wrong with them is that no marker names them. A replay that walked capsules
/// instead of markers would silently include those edges and every assertion
/// about vertex counts would still pass.
#[test]
fn a_crash_at_any_instant_materializes_exactly_the_committed_prefix() {
    for (index, point) in [
        CrashPoint::BeforeCapsule,
        CrashPoint::AfterCapsuleBeforeD1,
        CrashPoint::AfterD1,
    ]
    .into_iter()
    .enumerate()
    {
        let dir = scratch_dir(&format!("prefix-{index}"));
        under_lab(10 + index as u64, move |cx| {
            let capsules = three_commits();
            let mut coordinator = CommitCoordinator::open(&dir).expect("open");
            commit_capsule(&mut coordinator, cx, &capsules[0], vec![]).expect("commit 1");
            commit_capsule(&mut coordinator, cx, &capsules[1], vec![]).expect("commit 2");

            let third = capsules[2].clone();
            let crashed = coordinator.commit_with_crash(
                cx,
                third.object_id,
                &third.bytes,
                |seq| fgdb_sim::marker_for_capsule(seq, &third, vec![]),
                Some(point),
            );
            assert!(crashed.is_err(), "{point:?} must not report success");
            drop(coordinator);

            let reopened = CommitCoordinator::open(&dir).expect("reopen after crash");
            let database = materialize(&reopened).expect("materializes");

            expect_graph_after(&database, 2);

            // The capsule for the crashed commit may be sitting right there,
            // whole and decodable, and it must still contribute nothing.
            let capsule_durable = point != CrashPoint::BeforeCapsule;
            assert_eq!(
                reopened.capsule_exists(third.object_id),
                capsule_durable,
                "{point:?}: capsule presence"
            );
            if capsule_durable {
                let orphan_bytes = reopened.read_capsule(third.object_id).expect("readable");
                assert!(
                    LogicalDeltaTemplate::decode_canonical(&orphan_bytes).is_ok(),
                    "{point:?}: the orphan decodes cleanly — being unusable is not \
                     what keeps it out of the graph; being unnamed by any marker is"
                );
                assert_eq!(
                    reopened.orphan_capsules().expect("scan"),
                    vec![third.object_id]
                );
            }
        });
    }
}

/// A torn tail from an interrupted D2 removes that commit's effects from the
/// graph, and the next commit reuses its sequence — history has no hole.
#[test]
fn a_torn_tail_removes_its_effects_and_the_sequence_is_reused() {
    let dir = scratch_dir("torn");
    under_lab(20, move |cx| {
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(&dir).expect("open");
        commit_capsule(&mut coordinator, cx, &capsules[0], vec![]).expect("commit 1");
        commit_capsule(&mut coordinator, cx, &capsules[1], vec![]).expect("commit 2");
        let committed_len = log_len(&dir);

        let third = capsules[2].clone();
        let _ = coordinator.commit_with_crash(
            cx,
            third.object_id,
            &third.bytes,
            |seq| fgdb_sim::marker_for_capsule(seq, &third, vec![]),
            Some(CrashPoint::AfterMarkerBeforeD2),
        );
        drop(coordinator);

        // The un-fsynced tail was lost, as a crash before a barrier may do.
        let written = log_len(&dir);
        assert!(written > committed_len);
        CommitCoordinator::tear_log_tail_for_test(&dir, (written - committed_len) as u64 - 4)
            .expect("tear");

        let mut reopened = CommitCoordinator::open(&dir).expect("reopen");
        assert_eq!(reopened.chain().len(), 2);
        expect_graph_after(&materialize(&reopened).expect("materializes"), 2);

        // And the database is still writable: committing the third template
        // again lands at the sequence the torn one abandoned.
        assert_eq!(reopened.next_commit_seq(), 3);
        commit_capsule(&mut reopened, cx, &capsules[2], vec![]).expect("recommit");
        expect_graph_after(&materialize(&reopened).expect("materializes"), 3);
    });
}

fn log_len(dir: &Path) -> usize {
    std::fs::read(dir.join(fgdb_chronicle::commit::COMMIT_LOG_NAME))
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The digest cross-check has teeth
// ---------------------------------------------------------------------------

/// FG-INV-09's shape at the semantic layer: a capsule whose bytes were altered
/// after commit must be REFUSED, not materialized.
///
/// The corruption used here is deliberately one that still decodes into a valid
/// template — a different graph, not a broken file. Corrupting the bytes into
/// garbage would be caught by the decoder and would prove only that the decoder
/// works; this proves the digest is what stands between a rewritten capsule and
/// silently different graph state.
#[test]
fn a_rewritten_capsule_is_refused_rather_than_materialized() {
    let dir = scratch_dir("rewritten");
    under_lab(30, move |cx| {
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(&dir).expect("open");
        for capsule in &capsules {
            commit_capsule(&mut coordinator, cx, capsule, vec![]).expect("commit");
        }
        drop(coordinator);

        // Swap the FIRST capsule's bytes for a different, perfectly valid
        // template: same coordinate, different people.
        let impostor = capsule_of(vec![person(1, 1, "mallory"), person(2, 2, "eve")]);
        assert!(
            LogicalDeltaTemplate::decode_canonical(&impostor.bytes).is_ok(),
            "the replacement must be valid, or this tests the decoder instead"
        );
        let path = dir
            .join(fgdb_chronicle::commit::CAPSULE_DIR)
            .join(format!("{}.capsule", hex(&capsules[0].object_id.0)));
        std::fs::write(&path, &impostor.bytes).expect("rewrite capsule");

        let reopened = CommitCoordinator::open(&dir).expect("reopen");
        let result = materialize(&reopened);
        assert!(
            matches!(
                result,
                Err(ReplayError::TemplateDigestMismatch { commit_seq: 1, .. })
            ),
            "a rewritten capsule must fail closed and name the commit; got {result:?}"
        );
    });
}

/// A capsule deleted out from under a committed marker is unrecoverable, and
/// says so. This is NOT the orphan case: the marker exists, so the commit
/// happened, so the bytes were durable before it was written.
#[test]
fn a_missing_capsule_under_a_committed_marker_fails_closed() {
    let dir = scratch_dir("missing");
    under_lab(31, move |cx| {
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(&dir).expect("open");
        for capsule in &capsules {
            commit_capsule(&mut coordinator, cx, capsule, vec![]).expect("commit");
        }
        drop(coordinator);

        let path = dir
            .join(fgdb_chronicle::commit::CAPSULE_DIR)
            .join(format!("{}.capsule", hex(&capsules[1].object_id.0)));
        std::fs::remove_file(&path).expect("remove the second capsule");

        let reopened = CommitCoordinator::open(&dir).expect("reopen");
        let result = materialize(&reopened);
        assert!(
            matches!(
                result,
                Err(ReplayError::MissingCapsule { commit_seq: 2, .. })
            ),
            "got {result:?}"
        );
    });
}

fn hex(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble"));
        out.push(char::from_digit(u32::from(byte & 0xf), 16).expect("nibble"));
    }
    out
}

/// The identity is over the bytes, so two templates with the same effects in
/// different input orders are the SAME capsule — canonicalization reaching all
/// the way to the object id rather than stopping at the encoder.
#[test]
fn input_order_does_not_change_a_capsules_identity() {
    let forward = capsule_of(vec![person(1, 1, "ada"), person(2, 2, "grace")]);
    let backward = capsule_of(vec![person(2, 2, "grace"), person(1, 1, "ada")]);
    assert_eq!(forward.object_id, backward.object_id);
    assert_eq!(forward.template_digest, backward.template_digest);
    assert_eq!(forward.bytes, backward.bytes);
}
