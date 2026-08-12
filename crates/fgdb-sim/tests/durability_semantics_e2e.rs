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

use asupersync::fs::UnixVfs;
use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CommitCoordinator, CommitError, CrashPoint};
use fgdb_delta_types::{
    CoordinateEntry, DeltaRow, LabelId, LogicalDeltaTemplate, PropertyKeyId, RelationId,
    SchemaEpoch,
};
use fgdb_reference::{ReferenceDatabase, SnapshotError};
use fgdb_sim::vfs::{FaultKind, FaultPlan, FaultVfs, Trigger};
use fgdb_sim::{
    PreparedCapsule, ReplayError, commit_capsule, materialize, prepare_capsule, replay,
};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, MarkerRef, ObjectId, VId};
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

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
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
    under_lab(1, move |cx| async move {
        let cx = &cx;
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");
        for capsule in &capsules {
            commit_capsule(&mut coordinator, cx, capsule, vec![])
                .await
                .expect("commit");
        }
        drop(coordinator);

        let reopened = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        assert_eq!(reopened.chain().len(), 3);
        let database = materialize(cx, &reopened).await.expect("materializes");
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
    under_lab(2, move |cx| async move {
        let cx = &cx;
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");
        for capsule in &three_commits() {
            commit_capsule(&mut coordinator, cx, capsule, vec![])
                .await
                .expect("commit");
        }
        let first = materialize(cx, &coordinator).await.expect("first");
        let second = materialize(cx, &coordinator).await.expect("second");
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
    under_lab(3, move |cx| async move {
        let cx = &cx;
        let first_coordinator = CommitCoordinator::open(cx, &first_dir, keys())
            .await
            .expect("open first database");
        let second_coordinator = CommitCoordinator::open(cx, &second_dir, keys())
            .await
            .expect("open second database");
        let first = materialize(cx, &first_coordinator)
            .await
            .expect("materialize first database");
        let second = materialize(cx, &second_coordinator)
            .await
            .expect("materialize second database");
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
        under_lab(10 + index as u64, move |cx| async move {
            let cx = &cx;
            let capsules = three_commits();
            let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("open");
            commit_capsule(&mut coordinator, cx, &capsules[0], vec![])
                .await
                .expect("commit 1");
            commit_capsule(&mut coordinator, cx, &capsules[1], vec![])
                .await
                .expect("commit 2");

            let third = capsules[2].clone();
            let crashed = coordinator
                .commit_with_crash(
                    cx,
                    &third.bytes,
                    |seq, oid| fgdb_sim::marker_for_capsule(seq, oid, &third, vec![]),
                    Some(point),
                )
                .await;
            assert!(crashed.is_err(), "{point:?} must not report success");
            drop(coordinator);

            let reopened = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("reopen after crash");
            let database = materialize(cx, &reopened).await.expect("materializes");

            expect_graph_after(&database, 2);

            // The capsule for the crashed commit may be sitting right there,
            // whole and decodable, and it must still contribute nothing.
            let capsule_durable = point != CrashPoint::BeforeCapsule;
            assert_eq!(
                reopened.capsule_exists(cx, third.object_id).await,
                capsule_durable,
                "{point:?}: capsule presence"
            );
            if capsule_durable {
                let orphan_bytes = reopened
                    .read_capsule(cx, third.object_id)
                    .await
                    .expect("readable");
                assert!(
                    LogicalDeltaTemplate::decode_canonical(&orphan_bytes).is_ok(),
                    "{point:?}: the orphan decodes cleanly — being unusable is not \
                     what keeps it out of the graph; being unnamed by any marker is"
                );
                assert_eq!(
                    reopened.orphan_capsules(cx).await.expect("scan"),
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
    under_lab(20, move |cx| async move {
        let cx = &cx;
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");
        commit_capsule(&mut coordinator, cx, &capsules[0], vec![])
            .await
            .expect("commit 1");
        commit_capsule(&mut coordinator, cx, &capsules[1], vec![])
            .await
            .expect("commit 2");
        let committed_len = log_len(&dir);

        let third = capsules[2].clone();
        let _ = coordinator
            .commit_with_crash(
                cx,
                &third.bytes,
                |seq, oid| fgdb_sim::marker_for_capsule(seq, oid, &third, vec![]),
                Some(CrashPoint::AfterMarkerBeforeD2),
            )
            .await;
        drop(coordinator);

        // The un-fsynced tail was lost, as a crash before a barrier may do.
        let written = log_len(&dir);
        assert!(written > committed_len);
        CommitCoordinator::<UnixVfs>::tear_log_tail_for_test(
            &dir,
            (written - committed_len) as u64 - 4,
        )
        .expect("tear");

        let mut reopened = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        assert_eq!(reopened.chain().len(), 2);
        expect_graph_after(&materialize(cx, &reopened).await.expect("materializes"), 2);

        // And the database is still writable: committing the third template
        // again lands at the sequence the torn one abandoned.
        assert_eq!(reopened.next_commit_seq(), Ok(CommitSeq(3)));
        commit_capsule(&mut reopened, cx, &capsules[2], vec![])
            .await
            .expect("recommit");
        expect_graph_after(&materialize(cx, &reopened).await.expect("materializes"), 3);
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
    under_lab(30, move |cx| async move {
        let cx = &cx;
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");
        for capsule in &capsules {
            commit_capsule(&mut coordinator, cx, capsule, vec![])
                .await
                .expect("commit");
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

        let reopened = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        let result = materialize(cx, &reopened).await;
        assert!(
            result.is_err(),
            "a substituted capsule must never materialize; got {result:?}"
        );

        // Nothing partial was applied on the way to failing.
        assert!(
            reopened
                .read_capsule(cx, capsules[0].object_id)
                .await
                .is_err(),
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
    under_lab(31, move |cx| async move {
        let cx = &cx;
        let capsule = three_commits()[0].clone();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");
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
            .await
            .expect("the commit itself is well formed");
        drop(coordinator);

        let reopened = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        let result = materialize(cx, &reopened).await;
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
    under_lab(37, move |cx| async move {
        let cx = &cx;
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");

        for (capsule, logical_command_seq) in [(&capsules[0], 10_u64), (&capsules[1], 25_u64)] {
            coordinator
                .commit(cx, &capsule.bytes, |commit_seq, capsule_oid| {
                    let mut marker =
                        fgdb_sim::marker_for_capsule(commit_seq, capsule_oid, capsule, vec![]);
                    marker.logical_command_seq = logical_command_seq;
                    marker
                })
                .await
                .expect("commit with an independent logical command position");
        }
        drop(coordinator);

        let reopened = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        let recovered = replay(cx, &reopened).await.expect("replay");
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
    under_lab(31, move |cx| async move {
        let cx = &cx;
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");
        for capsule in &capsules {
            commit_capsule(&mut coordinator, cx, capsule, vec![])
                .await
                .expect("commit");
        }
        drop(coordinator);

        let path = dir
            .join(fgdb_chronicle::commit::CAPSULE_DIR)
            .join(format!("{}.capsule", hex(&capsules[1].object_id.0)));
        std::fs::remove_file(&path).expect("remove the second capsule");

        let reopened = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        let result = materialize(cx, &reopened).await;
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
    under_lab(40, move |cx| async move {
        let cx = &cx;
        let capsules = three_commits();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");

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
            .await
            .expect("commit");
        assert_eq!(observed, Some(expected));
        assert!(coordinator.capsule_exists(cx, expected).await);
    });
}

/// A capsule now round-trips through erasure coding, not raw bytes: what comes
/// back out of `read_capsule` is the decoded plaintext, byte-for-byte.
#[test]
fn a_committed_capsule_recovers_its_exact_plaintext_through_the_codec() {
    let dir = scratch_dir("codec-round-trip");
    under_lab(41, move |cx| async move {
        let cx = &cx;
        let capsule = three_commits()[0].clone();
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");
        commit_capsule(&mut coordinator, cx, &capsule, vec![])
            .await
            .expect("commit");
        drop(coordinator);

        let reopened = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("reopen");
        let recovered = reopened
            .read_capsule(cx, capsule.object_id)
            .await
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
        under_lab(50 + index_case as u64, move |cx| async move {
            let cx = &cx;
            let capsules = three_commits();
            let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("open");
            commit_capsule(&mut coordinator, cx, &capsules[0], vec![])
                .await
                .expect("commit 1");
            commit_capsule(&mut coordinator, cx, &capsules[1], vec![])
                .await
                .expect("commit 2");

            let third = capsules[2].clone();
            let _ = coordinator
                .commit_with_crash(
                    cx,
                    &third.bytes,
                    |seq, oid| fgdb_sim::marker_for_capsule(seq, oid, &third, vec![]),
                    Some(point),
                )
                .await;
            drop(coordinator);

            let reopened = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("reopen");
            let replayed = replay(cx, &reopened).await.expect("replays");

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
    under_lab(60, move |cx| async move {
        let cx = &cx;
        let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
            .await
            .expect("open");
        for capsule in &three_commits() {
            commit_capsule(&mut coordinator, cx, capsule, vec![])
                .await
                .expect("commit");
        }
        let replayed = replay(cx, &coordinator).await.expect("replays");

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

// ---------------------------------------------------------------------------
// The crash matrix, re-expressed over the FaultVfs (bead fgdb-w14j, fgdb-1xtp
// step 3).
//
// `CrashPoint` models "the process stopped at instant X" by returning early
// from inside the commit path — a test-only enum arm threaded through a
// production signature. The tests below reach the same recovered states from
// the OUTSIDE, through real storage faults injected by `FaultVfs`, and then
// go where `CrashPoint` structurally cannot: a sync that lied, a flush that
// tore an interior sector, a disk that filled mid-barrier, a bit that rotted
// after landing.
//
// The instant-to-fault correspondence, stated so the equivalence is checkable
// rather than implied:
//
//   CrashPoint arm                      fault expression here
//   ---------------------------------   -----------------------------------
//   AfterMarkerBeforeD2 loss arm        ENOSPC at D2, then crash()
//   clean restart                       faultless plan, then crash()
//   (inexpressible by CrashPoint)       one transient fsync lie at D1 or D2:
//                                       reinforced before publication advances
//   (inexpressible by CrashPoint)       interior tear inside D2's flush
//   (inexpressible by CrashPoint)       bit rot in a durable capsule, healed
//
// The dirent-granular arms (AfterCapsuleFileSyncBeforeDirectorySync and kin)
// remain `CrashPoint`'s to express: the fault model deliberately does not
// represent dirents (see `fgdb_sim::vfs`'s not-modelled list), so NOTHING is
// removed from the CrashPoint matrix until each arm's recovered state is
// reachable this way and mutation-proven at least as strong — the bead's own
// instruction.
//
// Per-commit trigger arithmetic every plan below relies on: a faultless commit
// performs exactly TWO trigger-eligible syncs — D1 (the capsule file) and D2
// (the commit log). Each logical barrier has one reinforcement call, but the
// FaultVfs makes a clean-file sync ineligible. When the primary sync lies, its
// dirty bytes make the reinforcement eligible and the one-shot trigger does
// not fire again. Directory syncs carry no dirty sectors and consume no file-
// sync counts. Each test asserts the injected event's PATH, so drift fails
// loudly instead of silently faulting the wrong barrier.
// ---------------------------------------------------------------------------

fn capsule_disk_path(dir: &Path, oid: &ObjectId) -> PathBuf {
    dir.join(fgdb_chronicle::commit::CAPSULE_DIR)
        .join(format!("{}.capsule", hex(&oid.0)))
}

async fn open_faulted(cx: &CommitCx, vfs: &FaultVfs, dir: &Path) -> CommitCoordinator<FaultVfs> {
    CommitCoordinator::open_with_vfs(cx, vfs.clone(), dir, keys())
        .await
        .expect("open through the fault model")
}

fn commit_io_error_code(result: &Result<MarkerRef, CommitError>) -> Option<i32> {
    match result {
        Err(CommitError::Io(error)) => error.raw_os_error(),
        Ok(_) | Err(_) => None,
    }
}

fn torn_write_range(kind: FaultKind) -> Option<(u64, u64)> {
    match kind {
        FaultKind::TornWrite { start, end } => Some((start, end)),
        _ => None,
    }
}

async fn exercise_file_write_enospc(cx: &CommitCx, dir: &Path, write_ordinal: u32, d2_arm: bool) {
    let capsules = three_commits();
    let third = capsules[2].clone();
    let mut baseline = CommitCoordinator::open(cx, dir, keys())
        .await
        .expect("open baseline");
    commit_capsule(&mut baseline, cx, &capsules[0], vec![])
        .await
        .expect("commit 1");
    commit_capsule(&mut baseline, cx, &capsules[1], vec![])
        .await
        .expect("commit 2");
    drop(baseline);

    // A freshly opened fault model sees exactly two non-empty write calls in
    // this commit: the capsule container (D1's file action), then the framed
    // marker append (D2's file action). The event path below independently
    // pins which one the ordinal selected.
    let vfs = FaultVfs::unix(FaultPlan {
        write_enospc: Trigger::At(write_ordinal),
        ..FaultPlan::faultless()
    });
    let mut faulted = open_faulted(cx, &vfs, dir).await;
    let refused = commit_capsule(&mut faulted, cx, &third, vec![]).await;
    assert_eq!(
        commit_io_error_code(&refused),
        Some(28),
        "the selected file write must refuse with typed ENOSPC: {refused:?}"
    );
    assert_eq!(
        faulted.is_poisoned(),
        d2_arm,
        "a D1 write refusal is pre-marker and reusable; D2 begins after the coordinator's uncertainty fence"
    );
    let events = vfs.events();
    assert_eq!(
        events.len(),
        1,
        "exactly the planted write fault must fire: {events:?}"
    );
    assert!(
        matches!(events[0].kind, FaultKind::WriteEnospc { requested } if requested > 0),
        "the witness must be a non-empty write refusal: {:?}",
        events[0].kind
    );
    let expected_path = if d2_arm {
        dir.join(fgdb_chronicle::commit::COMMIT_LOG_NAME)
    } else {
        capsule_disk_path(dir, &third.object_id)
    };
    assert_eq!(
        events[0].path, expected_path,
        "the ordinal selected the wrong D1/D2 file action"
    );

    assert_eq!(
        faulted.chain().len(),
        2,
        "a refused write must never become an acknowledged commit"
    );
    assert_eq!(faulted.next_commit_seq(), Ok(CommitSeq(3)));
    expect_graph_after(
        &materialize(cx, &faulted)
            .await
            .expect("the committed prefix materializes"),
        2,
    );

    if !d2_arm {
        // `create_new` happened before the first D1 write accepted zero bytes,
        // so the pathname exists but no immutable object does. The same live
        // coordinator must be able to finish that exact capsule after the
        // one-shot fault is exhausted; accepting only a different object would
        // leave a canonical-name denial of service.
        let residue = capsule_disk_path(dir, &third.object_id);
        let residue_metadata = std::fs::metadata(&residue)
            .expect("the refused create_new write leaves its canonical pathname");
        assert!(residue_metadata.is_file());
        assert_eq!(
            residue_metadata.len(),
            0,
            "the planted pre-cache ENOSPC must leave zero accepted bytes"
        );
        assert!(
            !faulted.capsule_exists(cx, third.object_id).await,
            "an empty pre-D1 staging pathname is not a capsule"
        );
        assert_eq!(
            faulted.orphan_capsules(cx).await.expect("orphan scan"),
            Vec::<ObjectId>::new(),
            "an empty pre-D1 staging pathname is not an orphan capsule"
        );
        let marker = commit_capsule(&mut faulted, cx, &third, vec![])
            .await
            .expect("exact D1 retry completes the empty pre-object residue");
        assert_eq!(marker.commit_seq, CommitSeq(3));
    }

    vfs.crash().await.expect("process loss");
    drop(faulted);
    let recovered_vfs = FaultVfs::unix(FaultPlan::faultless());
    let mut recovered = open_faulted(cx, &recovered_vfs, dir).await;

    if d2_arm {
        assert_eq!(recovered.chain().len(), 2);
        assert_eq!(recovered.next_commit_seq(), Ok(CommitSeq(3)));
        // D1 completed before the D2 write was refused. Reusing the durable
        // capsule and the unconsumed sequence must publish the exact third
        // commit after recovery.
        commit_capsule(&mut recovered, cx, &third, vec![])
            .await
            .expect("retry after D2 write refusal");
        expect_graph_after(
            &materialize(cx, &recovered)
                .await
                .expect("retry materializes"),
            3,
        );
    } else {
        assert_eq!(recovered.chain().len(), 3);
        assert_eq!(recovered.next_commit_seq(), Ok(CommitSeq(4)));
        assert_eq!(
            recovered.orphan_capsules(cx).await.expect("orphan scan"),
            Vec::<ObjectId>::new(),
            "an exact retry turns the pre-D1 residue into the referenced immutable capsule"
        );
        expect_graph_after(
            &materialize(cx, &recovered)
                .await
                .expect("exact retry survives recovery"),
            3,
        );
    }
}

/// Commit all three fixtures through `coordinator`, asserting each ack.
async fn three_commits_through<V: asupersync::fs::Vfs>(
    coordinator: &mut CommitCoordinator<V>,
    cx: &CommitCx,
) {
    for capsule in &three_commits() {
        commit_capsule(coordinator, cx, capsule, vec![])
            .await
            .expect("commit through the fault model");
    }
}

/// THE CONTROL, and the transparency law in one: a faultless plan must be
/// byte-invisible. The same workload runs over `UnixVfs` and over a faultless
/// `FaultVfs`, and the durable artifacts must be identical — log bytes, the
/// capsule object set, and the materialized graph. Without this, every test
/// below could be measuring the write-back model instead of the fault it
/// injects.
#[test]
fn a_faultless_fault_model_is_byte_transparent_through_the_real_commit_path() {
    let plain_dir = scratch_dir("vfs-control-plain");
    let faulted_dir = scratch_dir("vfs-control-faulted");
    under_lab(70, move |cx| async move {
        let cx = &cx;
        let mut plain = CommitCoordinator::open(cx, &plain_dir, keys())
            .await
            .expect("open plain");
        three_commits_through(&mut plain, cx).await;
        drop(plain);

        let vfs = FaultVfs::unix(FaultPlan::faultless());
        let mut faulted = open_faulted(cx, &vfs, &faulted_dir).await;
        three_commits_through(&mut faulted, cx).await;
        drop(faulted);

        assert_eq!(vfs.events(), Vec::new(), "a faultless plan injects nothing");
        assert!(
            vfs.flushed_bytes() > 0,
            "zero flushed bytes would mean the workload never went through \
             the model, and every assertion here would be vacuous"
        );
        assert_eq!(
            std::fs::read(plain_dir.join(fgdb_chronicle::commit::COMMIT_LOG_NAME)).expect("plain"),
            std::fs::read(faulted_dir.join(fgdb_chronicle::commit::COMMIT_LOG_NAME))
                .expect("faulted"),
            "the durable log must be byte-identical through the fault model"
        );
        for capsule in &three_commits() {
            assert_eq!(
                std::fs::read(capsule_disk_path(&plain_dir, &capsule.object_id)).expect("plain"),
                std::fs::read(capsule_disk_path(&faulted_dir, &capsule.object_id))
                    .expect("faulted"),
                "every capsule object must be byte-identical through the fault model"
            );
        }

        let reopened = CommitCoordinator::open(cx, &faulted_dir, keys())
            .await
            .expect("reopen without the model");
        expect_graph_after(&materialize(cx, &reopened).await.expect("materializes"), 3);
    });
}

/// A D1 file write can fail before the capsule cache accepts any byte. No
/// marker is published, the committed graph remains the exact prior prefix,
/// and the abandoned sequence remains available for unrelated progress.
#[test]
fn d1_file_write_enospc_refuses_before_marker_publication_and_recovers_prefix() {
    let dir = scratch_dir("vfs-d1-write-enospc");
    under_lab(74, move |cx| async move {
        exercise_file_write_enospc(&cx, &dir, 1, false).await;
    });
}

/// A D2 file write can fail after D1 without acknowledging the commit. Reopen
/// observes the exact prior prefix and can reuse both the durable capsule and
/// the unconsumed commit sequence.
#[test]
fn d2_file_write_enospc_refuses_before_acknowledgement_and_recovers_prefix() {
    let dir = scratch_dir("vfs-d2-write-enospc");
    under_lab(75, move |cx| async move {
        exercise_file_write_enospc(&cx, &dir, 2, true).await;
    });
}

/// One transient fsync lie at D2 is absorbed before acknowledgement.
///
/// The primary sync returns success without writing, leaving the marker dirty;
/// the reinforcement sync on the same handle is honest and persists it. This
/// is not same-cache readback and not a claim about an indefinitely lying
/// device: it pins the exact one-fault guarantee the protocol implements.
#[test]
fn a_one_shot_d2_fsync_lie_is_reinforced_before_acknowledgement() {
    let dir = scratch_dir("vfs-d2-lie");
    under_lab(71, move |cx| async move {
        let cx = &cx;
        // Two eligible syncs per commit: the 6th is commit 3's D2.
        let vfs = FaultVfs::unix(FaultPlan {
            fsync_lie: Trigger::Nth(6),
            ..FaultPlan::faultless()
        });
        let mut coordinator = open_faulted(cx, &vfs, &dir).await;
        three_commits_through(&mut coordinator, cx).await; // commit 3 ACKS Ok
        drop(coordinator);

        let events = vfs.events();
        assert_eq!(events.len(), 1, "exactly the planned lie fired: {events:?}");
        assert!(
            matches!(events[0].kind, FaultKind::FsyncLie { unflushed_bytes } if unflushed_bytes > 0)
        );
        assert_eq!(
            events[0].path,
            dir.join(fgdb_chronicle::commit::COMMIT_LOG_NAME),
            "the 6th eligible sync must be the commit log's D2 — this pins the \
             per-commit trigger arithmetic the whole section relies on"
        );

        vfs.crash().await.expect("crash rollback");
        let reopened = open_faulted(cx, &vfs, &dir).await;
        assert_eq!(
            reopened.chain().len(),
            3,
            "the reinforced marker is durable"
        );
        assert_eq!(reopened.chain().verify(), Ok(()));
        assert_eq!(reopened.next_commit_seq(), Ok(CommitSeq(4)));
        assert_eq!(
            reopened.discarded_tail_bytes(),
            0,
            "the honest reinforcement persisted the complete frame"
        );
        assert_eq!(
            reopened.orphan_capsules(cx).await.expect("scan"),
            Vec::<ObjectId>::new(),
            "every durable capsule is named by its recovered marker"
        );
        expect_graph_after(&materialize(cx, &reopened).await.expect("materializes"), 3);
    });
}

/// One transient fsync lie at D1 is absorbed before a marker can name the
/// capsule. The reinforcement sees the still-dirty container and persists it;
/// D2 then publishes a marker over bytes recovery can actually open.
#[test]
fn a_one_shot_d1_fsync_lie_is_reinforced_before_marker_publication() {
    let dir = scratch_dir("vfs-d1-lie");
    under_lab(72, move |cx| async move {
        let cx = &cx;
        // The 5th eligible sync is commit 3's D1.
        let vfs = FaultVfs::unix(FaultPlan {
            fsync_lie: Trigger::Nth(5),
            ..FaultPlan::faultless()
        });
        let mut coordinator = open_faulted(cx, &vfs, &dir).await;
        three_commits_through(&mut coordinator, cx).await; // commit 3 ACKS Ok
        drop(coordinator);

        let third = three_commits()[2].clone();
        let events = vfs.events();
        assert_eq!(events.len(), 1, "exactly the planned lie fired: {events:?}");
        assert_eq!(
            events[0].path,
            capsule_disk_path(&dir, &third.object_id),
            "the 5th eligible sync must be commit 3's capsule D1"
        );

        vfs.crash().await.expect("crash rollback");
        let reopened = open_faulted(cx, &vfs, &dir).await;
        assert_eq!(
            reopened.chain().len(),
            3,
            "D1 reinforcement lets D2 publish the complete third commit"
        );
        assert!(reopened.capsule_exists(cx, third.object_id).await);
        expect_graph_after(&materialize(cx, &reopened).await.expect("materializes"), 3);
    });
}

/// ENOSPC at each barrier. At D1 the refusal is typed, pre-marker, and leaves
/// the coordinator usable; at D2 it poisons — the same taxonomy the
/// `CrashPoint` matrix pins for its instants, now produced by the disk itself.
/// In both arms recovery lands on the committed prefix and the database
/// resumes once space returns.
#[test]
fn enospc_at_either_barrier_refuses_typed_and_recovery_resumes() {
    // (budget_for, expect_poisoned): D1 = a budget the capsule flush already
    // exceeds; D2 = exactly the capsule plus a sliver, so the marker flush is
    // the one refused.
    for (index, d2_arm) in [false, true].into_iter().enumerate() {
        let dir = scratch_dir(&format!("vfs-enospc-{index}"));
        under_lab(73 + index as u64, move |cx| async move {
            let cx = &cx;
            let capsules = three_commits();
            let third = capsules[2].clone();

            // Land the first two commits honestly, then measure the third
            // capsule's container size from a twin directory — the container
            // is deterministic, so the twin's byte length is THIS run's too.
            let mut coordinator = CommitCoordinator::open(cx, &dir, keys())
                .await
                .expect("open");
            commit_capsule(&mut coordinator, cx, &capsules[0], vec![])
                .await
                .expect("commit 1");
            commit_capsule(&mut coordinator, cx, &capsules[1], vec![])
                .await
                .expect("commit 2");
            drop(coordinator);
            let twin_dir = scratch_dir(&format!("vfs-enospc-twin-{index}"));
            let mut twin = CommitCoordinator::open(cx, &twin_dir, keys())
                .await
                .expect("open twin");
            three_commits_through(&mut twin, cx).await;
            drop(twin);
            let capsule_len = std::fs::read(capsule_disk_path(&twin_dir, &third.object_id))
                .expect("twin capsule")
                .len() as u64;

            // A fresh-file flush writes exactly the container's bytes, so the
            // budget can be placed on either side of D1 to the byte.
            let budget = if d2_arm {
                capsule_len + 3
            } else {
                capsule_len - 1
            };
            let vfs = FaultVfs::unix(FaultPlan {
                space_budget: Some(budget),
                ..FaultPlan::faultless()
            });
            let mut faulted = open_faulted(cx, &vfs, &dir).await;
            let refused = commit_capsule(&mut faulted, cx, &third, vec![]).await;
            assert_eq!(
                commit_io_error_code(&refused),
                Some(28),
                "a full disk must surface as the kernel's typed ENOSPC: {refused:?}"
            );
            assert_eq!(
                faulted.is_poisoned(),
                d2_arm,
                "pre-marker refusals leave the coordinator usable; a refusal \
                 at D2 poisons, because the log may now disagree with memory"
            );
            let events = vfs.events();
            assert_eq!(events.len(), 1, "exactly the planned refusal: {events:?}");
            assert!(matches!(events[0].kind, FaultKind::OutOfSpace { .. }));
            let expected_path = if d2_arm {
                dir.join(fgdb_chronicle::commit::COMMIT_LOG_NAME)
            } else {
                capsule_disk_path(&dir, &third.object_id)
            };
            assert_eq!(events[0].path, expected_path);

            // Space returns (a faultless model over the same directory): the
            // committed prefix is intact and the abandoned sequence is reused.
            vfs.crash().await.expect("crash rollback");
            drop(faulted);
            let recovered_vfs = FaultVfs::unix(FaultPlan::faultless());
            let mut recovered = open_faulted(cx, &recovered_vfs, &dir).await;
            assert_eq!(recovered.chain().len(), 2);
            assert_eq!(recovered.next_commit_seq(), Ok(CommitSeq(3)));
            expect_graph_after(&materialize(cx, &recovered).await.expect("materializes"), 2);
            if d2_arm {
                // D1 landed the full container before D2 was refused, so the
                // recommit deduplicates against its own durable capsule.
                commit_capsule(&mut recovered, cx, &third, vec![])
                    .await
                    .expect("recommit once space returns");
            } else {
                // The refused D1 left the capsule path behind holding NONE of
                // its bytes. It is not an immutable object yet and no marker
                // names it, so the sole writer completes that exact creation
                // in place. Requiring external deletion here made transient
                // ENOSPC a permanent canonical-name denial of service.
                commit_capsule(&mut recovered, cx, &third, vec![])
                    .await
                    .expect("exact recommit completes the empty pre-D1 residue");
            }
            expect_graph_after(&materialize(cx, &recovered).await.expect("materializes"), 3);
        });
    }
}

/// An interior sector torn out of D2's flush: sectors landed on BOTH sides of
/// a hole. `tear_log_tail_for_test` can only truncate a suffix, so the
/// torn-tail rule's "missing bytes at the end versus wrong bytes in the
/// middle" discrimination had never faced this shape inside the REAL commit
/// path. It is corruption, not a tail: recovery must fail closed naming the
/// sequence, and must leave the damaged bytes in place as evidence.
#[test]
fn an_interior_tear_inside_the_d2_flush_is_corruption_not_a_tail() {
    let dir = scratch_dir("vfs-interior-tear");
    under_lab(75, move |cx| async move {
        let cx = &cx;
        // Small sectors so a single log entry spans enough of them for an
        // interior sector to exist (the tear's eligibility requirement — the
        // first and last sectors of the flush always land).
        let vfs = FaultVfs::unix(FaultPlan {
            sector_bytes: 64,
            torn_write: Trigger::Nth(6),
            ..FaultPlan::faultless()
        });
        let mut coordinator = open_faulted(cx, &vfs, &dir).await;
        three_commits_through(&mut coordinator, cx).await; // the tear is silent
        drop(coordinator);

        let log_path = dir.join(fgdb_chronicle::commit::COMMIT_LOG_NAME);
        let events = vfs.events();
        assert_eq!(
            events.len(),
            1,
            "exactly the planned tear fired: {events:?}"
        );
        assert_eq!(
            events[0].path, log_path,
            "the 6th eligible flush is commit 3's D2"
        );
        let range = torn_write_range(events[0].kind);
        assert!(
            range.is_some(),
            "expected a torn write, got {:?}",
            events[0].kind
        );
        let (start, end) = range.unwrap_or((0, 0));
        assert!(end > start, "the tear names the hole it made");

        vfs.crash().await.expect("crash rollback");
        let damaged = std::fs::read(&log_path).expect("durable log");
        let result = CommitCoordinator::open_with_vfs(
            cx,
            FaultVfs::unix(FaultPlan::faultless()),
            &dir,
            keys(),
        )
        .await;
        assert!(
            matches!(
                &result,
                Err(CommitError::CorruptLogEntry { commit_seq: 3 }
                    | CommitError::ChainDiverged { commit_seq: 3 })
            ),
            "an interior hole is damage inside a complete frame — corruption \
             naming sequence 3, never a discardable tail; got {result:?}"
        );
        assert_eq!(
            std::fs::read(&log_path).expect("durable log"),
            damaged,
            "fail-closed recovery must preserve the corruption as evidence"
        );
    });
}

/// A single bit of a durable capsule rots after landing, and the read HEALS:
/// the container is erasure-coded, the damaged symbol fails its MAC, is
/// dropped, and the plaintext recovers exactly. This is the assertion that
/// was structurally impossible before capsules were erasure-coded, and the
/// one `CrashPoint` could never make — nothing about a process stopping can
/// damage a byte that already landed.
///
/// The flip's location is seed-pinned into the symbol region: a flip in the
/// container header fails closed instead of healing (that direction is
/// covered by `a_rewritten_capsule_is_refused_rather_than_materialized`).
#[test]
fn a_flipped_bit_in_a_durable_capsule_heals_through_the_erasure_code() {
    let dir = scratch_dir("vfs-bit-heal");
    under_lab(76, move |cx| async move {
        let cx = &cx;
        // The 5th eligible flush-with-writes is commit 3's capsule D1.
        let vfs = FaultVfs::unix(FaultPlan {
            seed: 3,
            bit_flip: Trigger::Nth(5),
            ..FaultPlan::faultless()
        });
        let mut coordinator = open_faulted(cx, &vfs, &dir).await;
        three_commits_through(&mut coordinator, cx).await;
        drop(coordinator);

        let third = three_commits()[2].clone();
        let capsule_path = capsule_disk_path(&dir, &third.object_id);
        let events = vfs.events();
        assert_eq!(
            events.len(),
            1,
            "exactly the planned flip fired: {events:?}"
        );
        assert_eq!(events[0].path, capsule_path);
        assert!(matches!(events[0].kind, FaultKind::BitFlip { .. }));

        // The damage is REAL on the platter — one byte differs from the
        // deterministic container a twin run produces.
        let twin_dir = scratch_dir("vfs-bit-heal-twin");
        let mut twin = CommitCoordinator::open(cx, &twin_dir, keys())
            .await
            .expect("open twin");
        three_commits_through(&mut twin, cx).await;
        drop(twin);
        let pristine =
            std::fs::read(capsule_disk_path(&twin_dir, &third.object_id)).expect("twin capsule");
        let damaged = std::fs::read(&capsule_path).expect("damaged capsule");
        assert_eq!(damaged.len(), pristine.len());
        let differing = damaged
            .iter()
            .zip(&pristine)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(differing, 1, "exactly one byte differs — the flipped one");

        vfs.crash().await.expect("crash rollback");
        let reopened = open_faulted(cx, &vfs, &dir).await;
        assert_eq!(reopened.chain().len(), 3);
        assert_eq!(
            reopened
                .read_capsule(cx, third.object_id)
                .await
                .expect("the erasure code heals one rotted bit"),
            third.bytes,
            "healing must recover the EXACT plaintext, not merely decode"
        );
        expect_graph_after(&materialize(cx, &reopened).await.expect("materializes"), 3);
    });
}
