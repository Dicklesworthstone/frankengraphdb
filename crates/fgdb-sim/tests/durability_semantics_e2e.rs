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
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CommitCoordinator, CrashPoint};
use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_reference::{ReferenceDatabase, SnapshotError};
use fgdb_sim::{
    PreparedCapsule, ReplayError, commit_capsule, materialize, prepare_capsule, replay,
};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, ObjectId, VId};
use std::path::{Path, PathBuf};

const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const REL_KNOWS: RelationId = RelationId(1);
const LABEL_PERSON: LabelId = LabelId(10);
const PROP_NAME: PropertyKeyId = PropertyKeyId(100);

/// The capsule keys every coordinator here opens under. They MUST agree with
/// what `prepare_capsule` uses, or the identity the store derives and the one
/// the caller computed would differ — which is exactly what
/// `a_prepared_capsule_agrees_with_the_stores_derived_identity` checks.
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
        // lab_test_passed() covers ALL THREE channels — quiescence, the full
        // 24-oracle suite, and the mirrored invariant list (fresh-eyes I3).
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
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
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        for capsule in &capsules {
            commit_capsule(&mut coordinator, cx, capsule, vec![]).expect("commit");
        }
        drop(coordinator);

        let reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");
        assert_eq!(reopened.chain().len(), 3);
        let database = materialize(cx, &reopened).expect("materializes");
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
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        for capsule in &three_commits() {
            commit_capsule(&mut coordinator, cx, capsule, vec![]).expect("commit");
        }
        let first = materialize(cx, &coordinator).expect("first");
        let second = materialize(cx, &coordinator).expect("second");
        assert_eq!(first, second);

        let snapshot = first.snapshot(GRAPH, BRANCH).expect("snapshot");
        assert_eq!(
            second.read(&snapshot).expect("same authority"),
            first.read(&snapshot).expect("minting authority")
        );
    });
}

/// The replay authority includes the database directory, not just capsule keys.
///
/// Operators commonly configure several databases with the same key material.
/// Equal empty content does not make those databases one authority: a genesis
/// capability minted for one directory must not be spendable against another.
#[test]
fn independent_directories_do_not_share_snapshot_authority() {
    let first_dir = scratch_dir("authority-first");
    let second_dir = scratch_dir("authority-second");
    under_lab(3, move |cx| {
        let first_coordinator =
            CommitCoordinator::open(cx, &first_dir, keys()).expect("open first database");
        let second_coordinator =
            CommitCoordinator::open(cx, &second_dir, keys()).expect("open second database");
        let first = materialize(cx, &first_coordinator).expect("materialize first database");
        let second = materialize(cx, &second_coordinator).expect("materialize second database");
        let snapshot = first
            .genesis_snapshot(GRAPH, BRANCH)
            .expect("first genesis snapshot");

        assert_eq!(
            second.read(&snapshot),
            Err(SnapshotError::ForeignSnapshot {
                graph: GRAPH,
                branch: BRANCH,
                high: fgdb_types::CommitSeq(0),
            })
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
            let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
            commit_capsule(&mut coordinator, cx, &capsules[0], vec![]).expect("commit 1");
            commit_capsule(&mut coordinator, cx, &capsules[1], vec![]).expect("commit 2");

            let third = capsules[2].clone();
            let crashed = coordinator.commit_with_crash(
                cx,
                &third.bytes,
                |seq, oid| fgdb_sim::marker_for_capsule(seq, oid, &third, vec![]),
                Some(point),
            );
            assert!(crashed.is_err(), "{point:?} must not report success");
            drop(coordinator);

            let reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen after crash");
            let database = materialize(cx, &reopened).expect("materializes");

            expect_graph_after(&database, 2);

            // The capsule for the crashed commit may be sitting right there,
            // whole and decodable, and it must still contribute nothing.
            let capsule_durable = point != CrashPoint::BeforeCapsule;
            assert_eq!(
                reopened.capsule_exists(cx, third.object_id),
                capsule_durable,
                "{point:?}: capsule presence"
            );
            if capsule_durable {
                let orphan_bytes = reopened
                    .read_capsule(cx, third.object_id)
                    .expect("readable");
                assert!(
                    LogicalDeltaTemplate::decode_canonical(&orphan_bytes).is_ok(),
                    "{point:?}: the orphan decodes cleanly — being unusable is not \
                     what keeps it out of the graph; being unnamed by any marker is"
                );
                assert_eq!(
                    reopened.orphan_capsules(cx).expect("scan"),
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
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        commit_capsule(&mut coordinator, cx, &capsules[0], vec![]).expect("commit 1");
        commit_capsule(&mut coordinator, cx, &capsules[1], vec![]).expect("commit 2");
        let committed_len = log_len(&dir);

        let third = capsules[2].clone();
        let _ = coordinator.commit_with_crash(
            cx,
            &third.bytes,
            |seq, oid| fgdb_sim::marker_for_capsule(seq, oid, &third, vec![]),
            Some(CrashPoint::AfterMarkerBeforeD2),
        );
        drop(coordinator);

        // The un-fsynced tail was lost, as a crash before a barrier may do.
        let written = log_len(&dir);
        assert!(written > committed_len);
        CommitCoordinator::tear_log_tail_for_test(&dir, (written - committed_len) as u64 - 4)
            .expect("tear");

        let mut reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");
        assert_eq!(reopened.chain().len(), 2);
        expect_graph_after(&materialize(cx, &reopened).expect("materializes"), 2);

        // And the database is still writable: committing the third template
        // again lands at the sequence the torn one abandoned.
        assert_eq!(reopened.next_commit_seq(), Ok(CommitSeq(3)));
        commit_capsule(&mut reopened, cx, &capsules[2], vec![]).expect("recommit");
        expect_graph_after(&materialize(cx, &reopened).expect("materializes"), 3);
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

/// A capsule rewritten on disk is REFUSED, not materialized.
///
/// WHICH LAYER CATCHES IT CHANGED when capsules became erasure-coded, and the
/// test says so rather than pretending otherwise. The file is now a container
/// whose recovery must reproduce the ObjectId the MARKER names, and the
/// ObjectId is the keyed hash of the plaintext — so any substituted content is
/// rejected by content-addressing before the template digest is ever consulted.
/// The impostor here is a properly SEALED container for different content, not
/// garbage, so this measures that law and not the container parser.
#[test]
fn a_rewritten_capsule_is_refused_rather_than_materialized() {
    let dir = scratch_dir("rewritten");
    under_lab(30, move |cx| {
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        for capsule in &capsules {
            commit_capsule(&mut coordinator, cx, capsule, vec![]).expect("commit");
        }
        drop(coordinator);

        // A valid template for a different graph, sealed exactly as the store
        // would seal it.
        let impostor = capsule_of(vec![person(1, 1, "mallory"), person(2, 2, "eve")]);
        let sealed = keys().seal(&impostor.bytes).expect("seals");
        let path = dir
            .join(fgdb_chronicle::commit::CAPSULE_DIR)
            .join(format!("{}.capsule", hex(&capsules[0].object_id.0)));
        std::fs::write(&path, fgdb_chronicle::capsule::encode_container(&sealed))
            .expect("rewrite capsule");

        let reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");
        let result = materialize(cx, &reopened);
        assert!(
            result.is_err(),
            "a substituted capsule must never materialize; got {result:?}"
        );

        // Nothing partial was applied on the way to failing.
        assert!(
            reopened.read_capsule(cx, capsules[0].object_id).is_err(),
            "the substituted container must not recover under the committed identity"
        );
    });
}

/// The template digest is still load-bearing, on the case that is now the
/// reachable one: a MARKER that declares a digest its capsule does not have.
///
/// On-disk tampering can no longer reach this check — content-addressing fires
/// first — but the marker's declared digest is supplied by the apply path, so a
/// caller that computed it from the wrong bytes is a live failure mode, and it
/// is the one that would otherwise let the commit stream and the effect set
/// disagree silently.
#[test]
fn a_marker_declaring_the_wrong_template_digest_is_refused() {
    let dir = scratch_dir("wrong-digest");
    under_lab(31, move |cx| {
        let capsule = three_commits()[0].clone();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        coordinator
            .commit(cx, &capsule.bytes, |seq, oid| {
                let mut marker = fgdb_sim::marker_for_capsule(seq, oid, &capsule, vec![]);
                marker.effect_source = fgdb_chronicle::marker::EffectSource::Local {
                    capsule_ref: oid,
                    // A digest of something else entirely.
                    logical_delta_template_digest: fgdb_crypto::Digest([0xab; 32]),
                };
                marker
            })
            .expect("the commit itself is well formed");
        drop(coordinator);

        let reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");
        let result = materialize(cx, &reopened);
        assert!(
            matches!(
                result,
                Err(ReplayError::TemplateDigestMismatch { commit_seq: 1, .. })
            ),
            "the declared digest must be checked against the recovered bytes; got {result:?}"
        );
    });
}

/// Replay takes the semantic command position from the durable marker, not from
/// the transaction commit sequence. The two domains deliberately use different
/// gaps here so substituting `commit_seq` in the replay path turns both frontier
/// assertions red.
#[test]
fn replay_preserves_the_markers_independent_logical_command_sequence() {
    let dir = scratch_dir("logical-command-sequence");
    under_lab(37, move |cx| {
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");

        for (capsule, logical_command_seq) in [(&capsules[0], 10_u64), (&capsules[1], 25_u64)] {
            coordinator
                .commit(cx, &capsule.bytes, |commit_seq, capsule_oid| {
                    let mut marker =
                        fgdb_sim::marker_for_capsule(commit_seq, capsule_oid, capsule, vec![]);
                    marker.logical_command_seq = logical_command_seq;
                    marker
                })
                .expect("commit with an independent logical command position");
        }
        drop(coordinator);

        let reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");
        let recovered = replay(cx, &reopened).expect("replay");
        assert_eq!(
            recovered.database.replay_frontier(),
            fgdb_types::CommitSeq(2)
        );
        assert_eq!(
            recovered.database.logical_command_frontier(),
            fgdb_types::LogicalCommandSeq(25)
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
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        for capsule in &capsules {
            commit_capsule(&mut coordinator, cx, capsule, vec![]).expect("commit");
        }
        drop(coordinator);

        let path = dir
            .join(fgdb_chronicle::commit::CAPSULE_DIR)
            .join(format!("{}.capsule", hex(&capsules[1].object_id.0)));
        std::fs::remove_file(&path).expect("remove the second capsule");

        let reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");
        let result = materialize(cx, &reopened);
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

/// TWO INDEPENDENT COMPUTATIONS OF ONE IDENTITY MUST AGREE. `prepare_capsule`
/// derives the object id from the template bytes; the coordinator derives it
/// again from the bytes it actually seals. Nothing forces them to match except
/// that both are the §5.1 transcript over the same inputs — so if they ever
/// diverge, a commit would name an object the store did not write, and every
/// recovery would fail on an identity mismatch it could not explain.
#[test]
fn a_prepared_capsule_agrees_with_the_stores_derived_identity() {
    let dir = scratch_dir("identity-agreement");
    under_lab(40, move |cx| {
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");

        for capsule in &capsules {
            assert_eq!(
                coordinator.capsule_id(&capsule.bytes),
                capsule.object_id,
                "the store and the caller must derive the same identity"
            );
        }

        // And the identity the commit path hands back is that same value.
        let expected = capsules[0].object_id;
        let mut observed = None;
        coordinator
            .commit(cx, &capsules[0].bytes, |seq, oid| {
                observed = Some(oid);
                fgdb_sim::marker_for_capsule(seq, oid, &capsules[0], vec![])
            })
            .expect("commit");
        assert_eq!(observed, Some(expected));
        assert!(coordinator.capsule_exists(cx, expected));
    });
}

/// A capsule now round-trips through erasure coding, not raw bytes: what comes
/// back out of `read_capsule` is the decoded plaintext, byte-for-byte.
#[test]
fn a_committed_capsule_recovers_its_exact_plaintext_through_the_codec() {
    let dir = scratch_dir("codec-round-trip");
    under_lab(41, move |cx| {
        let capsule = three_commits()[0].clone();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        commit_capsule(&mut coordinator, cx, &capsule, vec![]).expect("commit");
        drop(coordinator);

        let reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");
        let recovered = reopened
            .read_capsule(cx, capsule.object_id)
            .expect("recovers through the codec");
        assert_eq!(recovered, capsule.bytes);

        // The file on disk is the CONTAINER, not the plaintext — otherwise
        // "erasure coded" would be a claim with nothing behind it.
        let path = dir
            .join(fgdb_chronicle::commit::CAPSULE_DIR)
            .join(format!("{}.capsule", hex(&capsule.object_id.0)));
        let raw = std::fs::read(&path).expect("read the container");
        assert_ne!(raw, capsule.bytes, "the stored bytes are not the plaintext");
        assert!(
            raw.len() > capsule.bytes.len(),
            "coded bytes carry repair overhead"
        );
    });
}

/// THE DELTA WINDOW TRACKS THE COMMITTED PREFIX, and does so across a crash.
///
/// plan:397 requires apply to insert the batch and advance the frontier in the
/// SAME transition as the commit, so after recovery the two must agree exactly:
/// the frontier is the last committed sequence, the window is gap-free, and
/// every retained batch names the marker it came from. A frontier that ran
/// ahead would claim deltas nobody committed; one that lagged would hide
/// committed effects from every downstream consumer of the stream.
#[test]
fn the_delta_frontier_equals_the_committed_prefix_after_a_crash() {
    for (index_case, point) in [
        CrashPoint::BeforeCapsule,
        CrashPoint::AfterCapsuleBeforeD1,
        CrashPoint::AfterD1,
    ]
    .into_iter()
    .enumerate()
    {
        let dir = scratch_dir(&format!("frontier-{index_case}"));
        under_lab(50 + index_case as u64, move |cx| {
            let capsules = three_commits();
            let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
            commit_capsule(&mut coordinator, cx, &capsules[0], vec![]).expect("commit 1");
            commit_capsule(&mut coordinator, cx, &capsules[1], vec![]).expect("commit 2");

            let third = capsules[2].clone();
            let _ = coordinator.commit_with_crash(
                cx,
                &third.bytes,
                |seq, oid| fgdb_sim::marker_for_capsule(seq, oid, &third, vec![]),
                Some(point),
            );
            drop(coordinator);

            let reopened = CommitCoordinator::open(cx, &dir, keys()).expect("reopen");
            let replayed = replay(cx, &reopened).expect("replays");

            // The graph and the window agree about how far history got.
            expect_graph_after(&replayed.database, 2);
            assert_eq!(
                replayed.index.frontier(),
                fgdb_types::CommitSeq(2),
                "{point:?}: the frontier is the last COMMITTED sequence"
            );
            assert_eq!(replayed.index.len(), 2);
            assert_eq!(
                replayed.index.verify(),
                Ok(()),
                "{point:?}: the window is gap-free and exact"
            );

            // Every retained batch names the marker it came from, at its own
            // sequence — the wrong-marker mode plan:397 names.
            for seq in 1..=2u64 {
                let batch = replayed
                    .index
                    .get(fgdb_types::CommitSeq(seq))
                    .expect("retained");
                assert_eq!(batch.commit_seq(), fgdb_types::CommitSeq(seq));
                assert_eq!(
                    batch.commit_marker_identity().commit_seq,
                    fgdb_types::CommitSeq(seq)
                );
                assert_eq!(batch.frontier(), fgdb_types::CommitSeq(seq));
            }

            // The crashed commit contributed no batch, exactly as it
            // contributed no graph state.
            assert!(replayed.index.get(fgdb_types::CommitSeq(3)).is_none());
        });
    }
}

/// The window and the graph are built from the same walk, so they cannot
/// disagree about which commits exist.
#[test]
fn the_window_and_the_graph_cover_the_same_commits() {
    let dir = scratch_dir("window-graph-agree");
    under_lab(60, move |cx| {
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys()).expect("open");
        for capsule in &three_commits() {
            commit_capsule(&mut coordinator, cx, capsule, vec![]).expect("commit");
        }
        let replayed = replay(cx, &coordinator).expect("replays");

        assert_eq!(replayed.index.frontier(), fgdb_types::CommitSeq(3));
        assert_eq!(replayed.index.len(), 3);
        assert_eq!(
            replayed.index.frontier().0 as usize,
            coordinator.chain().len(),
            "the frontier and the marker chain describe the same history length"
        );
        expect_graph_after(&replayed.database, 3);
    });
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
