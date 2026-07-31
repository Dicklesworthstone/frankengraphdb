//! The crash-point matrix over the two-fsync commit protocol (plan §15).
//!
//! The protocol's whole claim is that a crash at ANY instant leaves the
//! database in a state recovery can name. This suite places a failure at each
//! named instant and asserts what survived — because "we fsync twice" is not a
//! durability argument, it is a description of two syscalls.
//!
//! ```text
//!   build capsule  ──▶  D1: capsule durable  ──▶  append marker  ──▶  D2: marker durable
//!        │                      │                       │                     │
//!   nothing written      orphan capsule,          orphan capsule,        COMMITTED
//!                        NOT committed            NOT committed
//! ```
//!
//! Two rules are under test throughout, and each has a way of being quietly
//! false:
//!
//!   * **The marker is the commit.** A capsule with no marker is bytes nobody
//!     referenced. The way this goes wrong is a recovery that treats capsule
//!     presence as evidence of a commit, so every crash test below checks the
//!     capsule is *present* AND *not committed* — a test that only checked
//!     "not committed" would pass against an implementation that never wrote
//!     the capsule at all.
//!   * **The torn-tail rule.** Missing bytes at the end of the log is a crash
//!     during D2; bytes that are present but wrong is damage. Discarding a
//!     torn tail is correct, discarding a *corrupt middle entry* would delete
//!     every commit after it while reporting success — so both directions are
//!     asserted, and the corruption cases deliberately sit in the middle of
//!     the log where a lenient reader would swallow them.
//!
//! Everything runs under asupersync's lab runtime with a real `CommitCx`: the
//! two barriers are exactly the instants a lab wants to control.

use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{
    CAPSULE_DIR, COMMIT_LOG_NAME, CommitCoordinator, CommitError, CrashPoint, MAX_ENTRY_BODY,
};
use fgdb_chronicle::marker::{CommitMarker, EffectSource, HeadUpdate, MarkerChain};
use fgdb_crypto::Digest;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::{BranchId, CommitSeq, GraphId, MarkerRef, ObjectId};
use std::path::{Path, PathBuf};

/// A per-test directory carrying the process id, so a neighbouring pane
/// running this same suite cannot observe or delete ours.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-commit-crash-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn digest(seed: u8) -> Digest {
    Digest([seed; 32])
}

/// The keys every coordinator in this file opens under. Fixed so a capsule's
/// identity is a function of its bytes alone across the whole suite.
fn keys() -> CapsuleKeys {
    CapsuleKeys {
        k_oid: [0x5a; 32],
        namespace: fgdb_types::ids::DatabaseSecurityNamespaceId([0x77; 32]),
        dek: [0x3c; 32],
        object_kind: 0x0274,
        profile: CapsuleProfile::balanced(),
    }
}

/// The identity capsule `seq`'s bytes will have. Derived, not invented: the
/// coordinator computes the same value from the same bytes, so a test can no
/// longer assert against an identity the store would never produce.
fn capsule_oid(seq: u64) -> ObjectId {
    keys().identify(&capsule_bytes(seq))
}

fn capsule_bytes(seq: u64) -> Vec<u8> {
    format!("capsule for commit {seq}").into_bytes()
}

/// A marker for `commit_seq`, advancing one branch head against `previous`.
fn marker_for(seq: u64, capsule: ObjectId, chain: &MarkerChain) -> CommitMarker {
    CommitMarker {
        logical_command_seq: seq * 10,
        commit_seq: seq,
        effect_source: EffectSource::Local {
            capsule_ref: capsule,
            logical_delta_template_digest: digest(seq as u8 + 1),
        },
        prev_global: None,
        head_updates: vec![HeadUpdate {
            graph: GraphId(1),
            branch: BranchId(1),
            expected_previous: chain.head(GraphId(1), BranchId(1)),
        }],
        merge_record_oid: None,
        coordinate_schema_transition_digest: digest(3),
        topology_epoch: 1,
        policy_epoch: 2,
        revocation_index: 3,
        txn_token: [7u8; 16],
        commit_hlc: 1_000 + seq,
        final_effect_digest: digest(seq as u8 + 4),
        authorization_decision_digest: digest(5),
        resource_effect_digest: digest(6),
        payload_availability_certificate_oid: None,
        flags: 0,
    }
}

/// Commit `seq` cleanly, asserting it succeeded.
fn commit_ok(coordinator: &mut CommitCoordinator, cx: &CommitCx, seq: u64) {
    let chain_snapshot = coordinator.chain().clone();
    coordinator
        .commit(cx, &capsule_bytes(seq), |allocated, oid| {
            assert_eq!(allocated, seq, "the coordinator allocates the sequence");
            assert_eq!(
                oid,
                capsule_oid(seq),
                "the derived identity is reproducible"
            );
            marker_for(allocated, oid, &chain_snapshot)
        })
        .expect("clean commit");
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

fn log_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join(COMMIT_LOG_NAME)).unwrap_or_default()
}

fn write_log(dir: &Path, bytes: &[u8]) {
    std::fs::write(dir.join(COMMIT_LOG_NAME), bytes).expect("rewrite log");
}

fn capsule_file_count(dir: &Path) -> usize {
    std::fs::read_dir(dir.join(CAPSULE_DIR))
        .expect("capsule dir")
        .count()
}

fn only_capsule_path(dir: &Path) -> PathBuf {
    let mut entries = std::fs::read_dir(dir.join(CAPSULE_DIR)).expect("capsule dir");
    let path = entries
        .next()
        .expect("one capsule")
        .expect("read capsule entry")
        .path();
    assert!(
        entries.next().is_none(),
        "the fixture must contain exactly one capsule"
    );
    path
}

// ---------------------------------------------------------------------------
// The canonical-bytes round trip. Recovery reads markers back from the log, so
// a marker that does not survive its own encoding is a commit that cannot be
// recovered — this is upstream of every law below.
// ---------------------------------------------------------------------------

/// A marker with EVERY optional field populated and several head updates: the
/// shape most likely to expose a field the encoder writes and the decoder
/// forgets, since a decoder that drops a trailing field still round-trips a
/// marker whose trailing fields are all defaults.
fn fully_populated_marker() -> CommitMarker {
    CommitMarker {
        logical_command_seq: 0x0123_4567_89ab_cdef,
        commit_seq: 42,
        effect_source: EffectSource::Local {
            capsule_ref: ObjectId([0x11; 32]),
            logical_delta_template_digest: Digest([0x22; 32]),
        },
        prev_global: Some(MarkerRef {
            marker_oid: ObjectId([0x33; 32]),
            commit_seq: CommitSeq(41),
        }),
        head_updates: vec![
            HeadUpdate {
                graph: GraphId(1),
                branch: BranchId(2),
                expected_previous: None,
            },
            HeadUpdate {
                graph: GraphId(1),
                branch: BranchId((1u128 << 96) | 3),
                expected_previous: Some(MarkerRef {
                    marker_oid: ObjectId([0x44; 32]),
                    commit_seq: CommitSeq(7),
                }),
            },
            HeadUpdate {
                graph: GraphId((1u128 << 100) | 2),
                branch: BranchId(1),
                expected_previous: None,
            },
        ],
        merge_record_oid: Some(ObjectId([0x55; 32])),
        coordinate_schema_transition_digest: Digest([0x66; 32]),
        topology_epoch: u64::MAX,
        policy_epoch: 0,
        revocation_index: 9_999,
        txn_token: [0x77; 16],
        commit_hlc: 0xfedc_ba98_7654_3210,
        final_effect_digest: Digest([0x88; 32]),
        authorization_decision_digest: Digest([0x99; 32]),
        resource_effect_digest: Digest([0xaa; 32]),
        payload_availability_certificate_oid: Some(ObjectId([0xbb; 32])),
        flags: u32::MAX,
    }
}

#[test]
fn a_marker_round_trips_through_its_canonical_bytes() {
    let marker = fully_populated_marker();
    let bytes = marker.canonical_bytes();
    let decoded = fgdb_chronicle::marker::decode_canonical(&bytes).expect("decodes");
    assert_eq!(decoded, marker, "every field must survive the round trip");

    // And the decoded marker re-encodes identically — so the encoding is a
    // bijection on this input, not merely a function the decoder can undo.
    assert_eq!(decoded.canonical_bytes(), bytes);
}

#[test]
fn a_truncated_marker_body_decodes_to_nothing_at_every_length() {
    let bytes = fully_populated_marker().canonical_bytes();
    for length in 0..bytes.len() {
        assert!(
            fgdb_chronicle::marker::decode_canonical(&bytes[..length]).is_none(),
            "a {length}-byte prefix of a {}-byte marker must not decode",
            bytes.len()
        );
    }
}

#[test]
fn trailing_bytes_after_a_marker_are_refused() {
    let mut bytes = fully_populated_marker().canonical_bytes();
    bytes.push(0x00);
    assert!(
        fgdb_chronicle::marker::decode_canonical(&bytes).is_none(),
        "a durable format must refuse bytes it does not understand rather than \
         silently ignore them"
    );
}

// ---------------------------------------------------------------------------
// The clean path.
// ---------------------------------------------------------------------------

/// THE MILESTONE: commits outlive the process that made them. The coordinator
/// is dropped and rebound from the path alone, which is what a restart looks
/// like from the file's point of view.
#[test]
fn committed_markers_survive_a_restart() {
    let dir = scratch_dir("restart");
    under_lab(1, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        for seq in 1..=5 {
            commit_ok(&mut coordinator, cx, seq);
        }
        let head_before = coordinator
            .chain()
            .head(GraphId(1), BranchId(1))
            .expect("branch head");
        drop(coordinator);

        let reopened = CommitCoordinator::open(&dir, keys()).expect("reopen");
        assert_eq!(reopened.chain().len(), 5);
        assert_eq!(reopened.next_commit_seq(), Ok(CommitSeq(6)));
        assert_eq!(reopened.discarded_tail_bytes(), 0);
        assert_eq!(reopened.chain().verify(), Ok(()));
        assert_eq!(
            reopened.chain().head(GraphId(1), BranchId(1)),
            Some(head_before),
            "branch heads are rebuilt from the log, not stored separately"
        );
        for (index, entry) in reopened.chain().entries().iter().enumerate() {
            assert_eq!(entry.marker.commit_seq, index as u64 + 1);
            assert!(
                reopened.capsule_exists(capsule_oid(entry.marker.commit_seq)),
                "a committed marker names a capsule that must be durable"
            );
        }
    });
}

#[test]
fn a_committed_capsule_is_never_an_orphan() {
    let dir = scratch_dir("no-orphans");
    under_lab(2, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        for seq in 1..=3 {
            commit_ok(&mut coordinator, cx, seq);
        }
        assert_eq!(
            coordinator.orphan_capsules().expect("scan"),
            Vec::new(),
            "every capsule here is named by a committed marker"
        );
        assert_eq!(capsule_file_count(&dir), 3);
    });
}

/// A content-addressed path is immutable even when its current bytes are
/// corrupt. Rewriting it in place is especially dangerous: an earlier durable
/// marker may already name that object, so a short rewrite or crash can destroy
/// a commit that completed before this coordinator existed.
#[test]
fn a_conflicting_existing_capsule_path_is_never_overwritten() {
    let dir = scratch_dir("capsule-path-conflict");
    under_lab(35, move |cx| {
        let plaintext = capsule_bytes(1);

        // Reach D1 once so the fixture obtains the exact path the coordinator
        // derives, but leave no marker. Then model pre-existing wrong bytes at
        // that path. The next commit must preserve them as evidence rather than
        // silently replacing them.
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        let chain_snapshot = coordinator.chain().clone();
        let interrupted = coordinator.commit_with_crash(
            cx,
            &plaintext,
            |seq, oid| marker_for(seq, oid, &chain_snapshot),
            Some(CrashPoint::AfterD1),
        );
        assert!(interrupted.is_err(), "the fixture stops before the marker");
        drop(coordinator);

        let path = only_capsule_path(&dir);
        let mut sentinel = std::fs::read(&path).expect("read valid container");
        assert!(!sentinel.is_empty(), "the encoded container is non-empty");
        let changed_at = sentinel.len() / 2;
        sentinel[changed_at] ^= 0x80;
        std::fs::write(&path, &sentinel).expect("seed same-length conflicting capsule bytes");

        let mut reopened = CommitCoordinator::open(&dir, keys()).expect("reopen");
        let chain_snapshot = reopened.chain().clone();
        let result = reopened.commit(cx, &plaintext, |seq, oid| {
            marker_for(seq, oid, &chain_snapshot)
        });
        assert!(
            matches!(
                result,
                Err(CommitError::CapsulePathConflict { capsule_oid: found })
                    if found == capsule_oid(1)
            ),
            "a collision must be typed and name the derived identity; got {result:?}"
        );
        assert_eq!(
            std::fs::read(&path).expect("read preserved capsule"),
            sentinel,
            "the existing path is immutable even when its bytes are wrong"
        );
        assert!(!reopened.is_poisoned());
        assert_eq!(reopened.next_commit_seq(), Ok(CommitSeq(1)));
        assert_eq!(reopened.chain().len(), 0);
        assert!(log_bytes(&dir).is_empty(), "no marker may be written");
    });
}

/// Deterministic sealing makes legitimate deduplication ordinary: two commits
/// may name the same capsule bytes. No-replace publication must therefore
/// distinguish an identical existing object from a conflicting one rather than
/// turning every `AlreadyExists` into an error.
#[test]
fn identical_payloads_reuse_one_capsule_and_both_markers_survive_restart() {
    let dir = scratch_dir("capsule-dedup");
    under_lab(36, move |cx| {
        let plaintext = capsule_bytes(1);
        let expected_oid = capsule_oid(1);
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");

        for expected_seq in 1..=2 {
            let chain_snapshot = coordinator.chain().clone();
            let marker_ref = coordinator
                .commit(cx, &plaintext, |seq, oid| {
                    assert_eq!(seq, expected_seq);
                    assert_eq!(oid, expected_oid);
                    marker_for(seq, oid, &chain_snapshot)
                })
                .expect("deduplicated commit");
            assert_eq!(marker_ref.commit_seq, CommitSeq(expected_seq));
        }

        assert_eq!(
            capsule_file_count(&dir),
            1,
            "identical deterministic containers share one immutable object"
        );
        assert_eq!(
            coordinator
                .read_capsule(expected_oid)
                .expect("read shared capsule"),
            plaintext
        );
        drop(coordinator);

        let reopened = CommitCoordinator::open(&dir, keys()).expect("reopen");
        assert_eq!(reopened.chain().len(), 2);
        assert_eq!(reopened.chain().verify(), Ok(()));
        assert_eq!(capsule_file_count(&dir), 1);
        assert_eq!(
            reopened
                .read_capsule(expected_oid)
                .expect("recover shared capsule"),
            plaintext
        );
    });
}

// ---------------------------------------------------------------------------
// The crash matrix.
// ---------------------------------------------------------------------------

/// Crash at each instant, then reopen and assert what recovery sees.
///
/// The committed prefix must be exactly the commits that reached D2, and the
/// interrupted commit must leave the sequence available — a crash consumes no
/// sequence number, or history would have a hole at the crash point.
#[test]
fn a_crash_at_any_instant_recovers_the_committed_prefix() {
    let points = [
        CrashPoint::BeforeCapsule,
        CrashPoint::AfterCapsuleBeforeD1,
        CrashPoint::AfterD1,
    ];
    for (index, point) in points.into_iter().enumerate() {
        let dir = scratch_dir(&format!("matrix-{index}"));
        under_lab(10 + index as u64, move |cx| {
            let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
            commit_ok(&mut coordinator, cx, 1);
            commit_ok(&mut coordinator, cx, 2);

            let chain_snapshot = coordinator.chain().clone();
            let crashed = coordinator.commit_with_crash(
                cx,
                &capsule_bytes(3),
                |seq, oid| marker_for(seq, oid, &chain_snapshot),
                Some(point),
            );
            assert!(crashed.is_err(), "{point:?} must not report success");

            // The process is gone; only the files speak now.
            drop(coordinator);
            let reopened = CommitCoordinator::open(&dir, keys()).expect("reopen after crash");

            assert_eq!(
                reopened.chain().len(),
                2,
                "{point:?}: only the commits that reached D2 are committed"
            );
            assert_eq!(reopened.chain().verify(), Ok(()));
            assert_eq!(
                reopened.next_commit_seq(),
                Ok(CommitSeq(3)),
                "{point:?}: the interrupted commit consumed no sequence"
            );
            assert_eq!(
                reopened.discarded_tail_bytes(),
                0,
                "{point:?}: no marker was written, so there is no tail to discard"
            );

            // THE RULE, stated as an assertion: the capsule may be on disk and
            // is STILL not a commit. Checking presence as well as
            // non-commitment is what makes this a test of the rule rather than
            // a test that nothing happened.
            let capsule_written = point != CrashPoint::BeforeCapsule;
            assert_eq!(
                reopened.capsule_exists(capsule_oid(3)),
                capsule_written,
                "{point:?}: capsule presence"
            );
            let orphans = reopened.orphan_capsules().expect("scan");
            if capsule_written {
                assert_eq!(
                    orphans,
                    vec![capsule_oid(3)],
                    "{point:?}: the capsule is an orphan — bytes nobody referenced"
                );
            } else {
                assert!(orphans.is_empty(), "{point:?}: nothing was written");
            }
        });
    }
}

/// A crash between the marker write and D2 has TWO legal outcomes, because
/// un-fsynced bytes may or may not survive. Both must be safe, so both are
/// tested: this arm is the one where the tail was torn away.
#[test]
fn a_torn_tail_from_an_interrupted_d2_is_discarded() {
    let dir = scratch_dir("torn-tail");
    under_lab(20, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_ok(&mut coordinator, cx, 1);
        commit_ok(&mut coordinator, cx, 2);
        let committed_len = log_bytes(&dir).len();

        let chain_snapshot = coordinator.chain().clone();
        let crashed = coordinator.commit_with_crash(
            cx,
            &capsule_bytes(3),
            |seq, oid| marker_for(seq, oid, &chain_snapshot),
            Some(CrashPoint::AfterMarkerBeforeD2),
        );
        assert!(crashed.is_err());

        // The coordinator can no longer speak for the log: the entry may or
        // may not be durable, and only the file knows.
        assert!(
            coordinator.is_poisoned(),
            "an interrupted commit must not leave a coordinator that keeps \
             allocating sequences it may already have written"
        );
        let refused = coordinator.commit_with_crash(
            cx,
            &capsule_bytes(4),
            |seq, oid| marker_for(seq, oid, &chain_snapshot),
            None,
        );
        assert!(
            matches!(refused, Err(CommitError::Poisoned)),
            "a poisoned coordinator must refuse, got {refused:?}"
        );
        drop(coordinator);

        // The crash tore the un-fsynced tail away, which is what a crash
        // before a barrier is entitled to do.
        let written = log_bytes(&dir).len();
        assert!(written > committed_len, "the marker bytes were written");
        CommitCoordinator::tear_log_tail_for_test(&dir, (written - committed_len) as u64 - 4)
            .expect("tear");

        let reopened = CommitCoordinator::open(&dir, keys()).expect("a torn tail is not an error");
        assert_eq!(
            reopened.chain().len(),
            2,
            "the interrupted commit is not a commit"
        );
        assert_eq!(
            reopened.next_commit_seq(),
            Ok(CommitSeq(3)),
            "its sequence is still free"
        );
        assert_eq!(reopened.chain().verify(), Ok(()));
        assert_eq!(
            reopened.discarded_tail_bytes(),
            4,
            "recovery reports what it dropped rather than swallowing it"
        );
        assert_eq!(
            reopened.orphan_capsules().expect("scan"),
            vec![capsule_oid(3)],
            "the capsule survives as an orphan"
        );
    });
}

/// Recovery must remove the physical torn suffix, not merely ignore it in
/// memory. Otherwise O_APPEND places the next successful marker after those
/// stale bytes, and the following restart stops at the same torn prefix before
/// it ever reaches the newly acknowledged commit.
#[test]
fn a_commit_after_torn_tail_recovery_survives_restart() {
    let dir = scratch_dir("torn-tail-then-append");
    under_lab(25, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_ok(&mut coordinator, cx, 1);
        commit_ok(&mut coordinator, cx, 2);
        let committed_len = log_bytes(&dir).len();

        let chain_snapshot = coordinator.chain().clone();
        let crashed = coordinator.commit_with_crash(
            cx,
            &capsule_bytes(3),
            |seq, oid| marker_for(seq, oid, &chain_snapshot),
            Some(CrashPoint::AfterMarkerBeforeD2),
        );
        assert!(crashed.is_err());
        drop(coordinator);

        let written = log_bytes(&dir).len();
        assert!(
            written > committed_len,
            "the interrupted marker was written"
        );
        CommitCoordinator::tear_log_tail_for_test(&dir, (written - committed_len) as u64 - 4)
            .expect("tear");

        let mut recovered = CommitCoordinator::open(&dir, keys()).expect("recover torn tail");
        assert_eq!(recovered.discarded_tail_bytes(), 4);
        commit_ok(&mut recovered, cx, 3);
        drop(recovered);

        let reopened =
            CommitCoordinator::open(&dir, keys()).expect("reopen after replacement commit");
        let sequences: Vec<u64> = reopened
            .chain()
            .entries()
            .iter()
            .map(|entry| entry.marker.commit_seq)
            .collect();
        assert_eq!(
            sequences,
            vec![1, 2, 3],
            "the replacement commit must remain reachable after another restart"
        );
        assert_eq!(reopened.discarded_tail_bytes(), 0);
        assert_eq!(reopened.chain().verify(), Ok(()));
    });
}

/// The other arm: the un-fsynced entry happened to survive intact. It is a
/// complete, chain-consistent entry, so recovery accepts it — the window where
/// a commit may or may not have happened is real, and both resolutions are
/// consistent.
#[test]
fn an_intact_unsynced_entry_recovers_as_committed() {
    let dir = scratch_dir("intact-tail");
    under_lab(21, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_ok(&mut coordinator, cx, 1);

        let chain_snapshot = coordinator.chain().clone();
        let _ = coordinator.commit_with_crash(
            cx,
            &capsule_bytes(2),
            |seq, oid| marker_for(seq, oid, &chain_snapshot),
            Some(CrashPoint::AfterMarkerBeforeD2),
        );
        drop(coordinator);

        let reopened = CommitCoordinator::open(&dir, keys()).expect("reopen");
        assert_eq!(reopened.chain().len(), 2);
        assert_eq!(reopened.chain().verify(), Ok(()));
        assert_eq!(reopened.discarded_tail_bytes(), 0);
        assert!(
            reopened.orphan_capsules().expect("scan").is_empty(),
            "the capsule is named by a recovered marker, so it is not an orphan"
        );
    });
}

/// Recovery must be able to make progress after a crash, not merely describe
/// one. The next commit lands at the sequence the crashed one abandoned.
#[test]
fn the_next_commit_after_a_crash_is_gap_free() {
    let dir = scratch_dir("gap-free");
    under_lab(22, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_ok(&mut coordinator, cx, 1);

        let chain_snapshot = coordinator.chain().clone();
        let _ = coordinator.commit_with_crash(
            cx,
            &capsule_bytes(2),
            |seq, oid| marker_for(seq, oid, &chain_snapshot),
            Some(CrashPoint::AfterD1),
        );
        drop(coordinator);

        let mut reopened = CommitCoordinator::open(&dir, keys()).expect("reopen");
        assert_eq!(reopened.next_commit_seq(), Ok(CommitSeq(2)));
        commit_ok(&mut reopened, cx, 2);
        commit_ok(&mut reopened, cx, 3);

        assert_eq!(reopened.chain().verify(), Ok(()));
        let sequences: Vec<u64> = reopened
            .chain()
            .entries()
            .iter()
            .map(|entry| entry.marker.commit_seq)
            .collect();
        assert_eq!(sequences, vec![1, 2, 3], "history has no hole at the crash");
    });
}

// ---------------------------------------------------------------------------
// Writer/reader symmetry. Anything the writer acknowledges must be inside the
// exact profile recovery accepts, and all cross-object references must agree
// before the first durable byte is written.
// ---------------------------------------------------------------------------

#[test]
fn a_marker_that_names_another_capsule_is_rejected_before_any_write() {
    let dir = scratch_dir("capsule-ref-mismatch");
    under_lab(26, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        let chain_snapshot = coordinator.chain().clone();
        let marker_capsule_ref = capsule_oid(99);
        let result = coordinator.commit(cx, &capsule_bytes(1), |seq, oid| {
            let mut marker = marker_for(seq, oid, &chain_snapshot);
            marker.effect_source = EffectSource::Local {
                capsule_ref: marker_capsule_ref,
                logical_delta_template_digest: digest(2),
            };
            marker
        });
        assert!(
            matches!(
                result,
                Err(CommitError::CapsuleRefMismatch {
                    capsule_oid: found_capsule_oid,
                    marker_capsule_ref: found_marker_ref,
                }) if found_capsule_oid == capsule_oid(1)
                    && found_marker_ref == marker_capsule_ref
            ),
            "the mismatch must be typed and preserve both identities; got {result:?}"
        );
        assert!(!coordinator.is_poisoned());
        assert_eq!(coordinator.next_commit_seq(), Ok(CommitSeq(1)));
        assert_eq!(capsule_file_count(&dir), 0, "no capsule may be written");
        assert!(log_bytes(&dir).is_empty(), "no marker may be written");

        commit_ok(&mut coordinator, cx, 1);
        assert_eq!(
            coordinator.chain().len(),
            1,
            "a pre-write refusal must leave the coordinator usable"
        );
    });
}

#[test]
fn a_marker_above_the_recovery_bound_is_rejected_before_any_write() {
    let dir = scratch_dir("marker-too-large");
    under_lab(27, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        let chain_snapshot = coordinator.chain().clone();
        let result = coordinator.commit(cx, &capsule_bytes(1), |seq, oid| {
            let mut marker = marker_for(seq, oid, &chain_snapshot);
            marker.head_updates = (0..4_000)
                .map(|branch| HeadUpdate {
                    graph: GraphId(7),
                    branch: BranchId(branch),
                    expected_previous: None,
                })
                .collect();
            marker
        });
        assert!(
            matches!(
                result,
                Err(CommitError::MarkerTooLarge {
                    body_len,
                    max_body_len: MAX_ENTRY_BODY,
                }) if body_len > MAX_ENTRY_BODY
            ),
            "the writer must enforce recovery's framing limit; got {result:?}"
        );
        assert!(!coordinator.is_poisoned());
        assert_eq!(coordinator.next_commit_seq(), Ok(CommitSeq(1)));
        assert_eq!(capsule_file_count(&dir), 0, "no capsule may be written");
        assert!(log_bytes(&dir).is_empty(), "no marker may be written");

        commit_ok(&mut coordinator, cx, 1);
        drop(coordinator);
        let reopened = CommitCoordinator::open(&dir, keys()).expect("reopen after valid commit");
        assert_eq!(reopened.chain().len(), 1);
        assert_eq!(reopened.chain().verify(), Ok(()));
    });
}

/// Repeated crashes must not accumulate damage. Ten crash/reopen cycles
/// interleaved with real commits, verified at every step.
#[test]
fn repeated_crashes_leave_a_verifiable_chain() {
    let dir = scratch_dir("repeated");
    under_lab(23, move |cx| {
        let points = [
            CrashPoint::BeforeCapsule,
            CrashPoint::AfterCapsuleBeforeD1,
            CrashPoint::AfterD1,
        ];
        // Derived, not hard-coded: only a crash that got past the capsule
        // write leaves an orphan, and stating that as arithmetic over the
        // actual schedule keeps the expectation honest if the schedule changes.
        let mut expected_orphans = 0usize;
        for round in 0..10u64 {
            let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
            let expected_seq = round + 1;
            assert_eq!(coordinator.next_commit_seq(), Ok(CommitSeq(expected_seq)));

            let point = points[(round % 3) as usize];
            if point != CrashPoint::BeforeCapsule {
                expected_orphans += 1;
            }
            let chain_snapshot = coordinator.chain().clone();
            // The marker names whatever the store derived, which is now the
            // only identity available — the override this used to carry became
            // a no-op once the coordinator started deriving the id itself.
            let _ = coordinator.commit_with_crash(
                cx,
                &capsule_bytes(1_000 + round),
                |seq, oid| marker_for(seq, oid, &chain_snapshot),
                Some(point),
            );
            drop(coordinator);

            let mut reopened = CommitCoordinator::open(&dir, keys()).expect("reopen");
            assert_eq!(reopened.next_commit_seq(), Ok(CommitSeq(expected_seq)));
            commit_ok(&mut reopened, cx, expected_seq);
            assert_eq!(reopened.chain().verify(), Ok(()));
        }

        let final_state = CommitCoordinator::open(&dir, keys()).expect("final open");
        assert_eq!(final_state.chain().len(), 10);
        assert_eq!(final_state.chain().verify(), Ok(()));
        assert_eq!(
            final_state.orphan_capsules().expect("scan").len(),
            expected_orphans,
            "every crash past the capsule write leaves exactly one orphan, and \
             none of them is ever mistaken for a commit"
        );
        assert!(
            expected_orphans > 0,
            "a zero here would make the assertion above vacuous"
        );
    });
}

// ---------------------------------------------------------------------------
// Corruption fails closed. These are the mirror of the torn-tail rule, and
// each one is placed in the MIDDLE of the log — the position where a reader
// that treats every malformed entry as a tail silently discards durable
// commits and reports success.
// ---------------------------------------------------------------------------

fn three_commit_log(dir: &Path, cx: &CommitCx) -> (Vec<u8>, usize) {
    let mut coordinator = CommitCoordinator::open(dir, keys()).expect("open");
    commit_ok(&mut coordinator, cx, 1);
    let first_len = log_bytes(dir).len();
    commit_ok(&mut coordinator, cx, 2);
    commit_ok(&mut coordinator, cx, 3);
    (log_bytes(dir), first_len)
}

#[test]
fn a_corrupt_middle_entry_fails_closed_instead_of_truncating_history() {
    let dir = scratch_dir("corrupt-magic");
    under_lab(30, move |cx| {
        let (mut bytes, first_len) = three_commit_log(&dir, cx);
        // Destroy the second entry's magic.
        bytes[first_len] ^= 0xff;
        write_log(&dir, &bytes);

        let result = CommitCoordinator::open(&dir, keys());
        assert!(
            matches!(result, Err(CommitError::CorruptLogEntry { commit_seq: 2 })),
            "damage to a durable entry must fail closed and name the position, \
             not silently return a shorter history; got {result:?}"
        );
    });
}

#[test]
fn a_middle_entry_with_an_oversized_length_is_corruption_not_a_tail() {
    let dir = scratch_dir("corrupt-length");
    under_lab(31, move |cx| {
        let (mut bytes, first_len) = three_commit_log(&dir, cx);
        // A length field damaged to a huge value is the case that reads
        // exactly like truncation: the entry claims more bytes than the file
        // holds. The bound on entry size is what tells them apart.
        bytes[first_len + 4..first_len + 8].copy_from_slice(&0xffff_ffffu32.to_be_bytes());
        write_log(&dir, &bytes);

        let result = CommitCoordinator::open(&dir, keys());
        assert!(
            matches!(result, Err(CommitError::CorruptLogEntry { commit_seq: 2 })),
            "an over-large length must not be mistaken for a torn tail; got {result:?}"
        );
    });
}

#[test]
fn a_bounded_length_increase_on_the_final_durable_entry_fails_closed() {
    let dir = scratch_dir("bounded-final-length");
    under_lab(37, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_ok(&mut coordinator, cx, 1);
        drop(coordinator);

        let mut bytes = log_bytes(&dir);
        let original_len = u32::from_be_bytes(bytes[4..8].try_into().expect("entry length field"));
        let added_bit = (0..u32::BITS)
            .find(|bit| {
                let mask = 1u32 << bit;
                original_len & mask == 0 && (original_len | mask) as usize <= MAX_ENTRY_BODY
            })
            .expect("fixture body has a bounded zero bit");
        let damaged_len = original_len | (1u32 << added_bit);
        assert!(
            damaged_len > original_len && (damaged_len as usize) <= MAX_ENTRY_BODY,
            "the causal mutation must increase exactly one bit while remaining in profile"
        );
        bytes[4..8].copy_from_slice(&damaged_len.to_be_bytes());
        write_log(&dir, &bytes);

        let result = CommitCoordinator::open(&dir, keys());
        assert!(
            matches!(result, Err(CommitError::CorruptLogEntry { commit_seq: 1 })),
            "a complete durable entry whose bounded length bit changed is corruption, \
             not an unfsynced torn tail; got {result:?}"
        );
        assert_eq!(
            log_bytes(&dir),
            bytes,
            "fail-closed recovery must not truncate the corruption evidence"
        );
    });
}

#[test]
fn a_corrupt_final_entry_trailer_length_fails_closed() {
    let dir = scratch_dir("corrupt-trailer-length");
    under_lab(38, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_ok(&mut coordinator, cx, 1);
        drop(coordinator);

        let mut bytes = log_bytes(&dir);
        let trailer_length_offset = bytes.len() - 8;
        bytes[trailer_length_offset] ^= 0x01;
        write_log(&dir, &bytes);

        let result = CommitCoordinator::open(&dir, keys());
        assert!(
            matches!(result, Err(CommitError::CorruptLogEntry { commit_seq: 1 })),
            "the duplicated length is part of the durable frame contract; got {result:?}"
        );
        assert_eq!(
            log_bytes(&dir),
            bytes,
            "recovery must preserve the corrupt frame as evidence"
        );
    });
}

#[test]
fn a_corrupt_final_entry_trailer_magic_fails_closed() {
    let dir = scratch_dir("corrupt-trailer-magic");
    under_lab(39, move |cx| {
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        commit_ok(&mut coordinator, cx, 1);
        drop(coordinator);

        let mut bytes = log_bytes(&dir);
        let final_byte = bytes.last_mut().expect("complete framed entry");
        *final_byte ^= 0x01;
        write_log(&dir, &bytes);

        let result = CommitCoordinator::open(&dir, keys());
        assert!(
            matches!(result, Err(CommitError::CorruptLogEntry { commit_seq: 1 })),
            "the end sentinel is part of the durable frame contract; got {result:?}"
        );
        assert_eq!(
            log_bytes(&dir),
            bytes,
            "recovery must preserve the corrupt frame as evidence"
        );
    });
}

#[test]
fn a_corrupt_marker_body_fails_closed() {
    let dir = scratch_dir("corrupt-body");
    under_lab(32, move |cx| {
        let (mut bytes, first_len) = three_commit_log(&dir, cx);
        // Flip the effect-source arm tag of the second entry to an unknown
        // value: the framing is intact, so only the body decoder can catch it.
        bytes[first_len + 8 + 16] = 0x7f;
        write_log(&dir, &bytes);

        let result = CommitCoordinator::open(&dir, keys());
        assert!(
            matches!(result, Err(CommitError::CorruptLogEntry { commit_seq: 2 })),
            "an unknown arm tag must be rejected, never skipped; got {result:?}"
        );
    });
}

#[test]
fn a_tampered_middle_entry_is_caught_by_the_chain_hash() {
    let dir = scratch_dir("tampered");
    under_lab(33, move |cx| {
        let (mut bytes, first_len) = three_commit_log(&dir, cx);
        // Change a field that still decodes — the commit_hlc — leaving a
        // perfectly well-formed entry whose stored chain hash no longer
        // matches. Framing checks cannot see this; the chain hash is the only
        // thing that can.
        let hlc_offset =
            first_len + 8 + 8 + 8 + 1 + 32 + 32 + 1 + 4 + 8 + 8 + 1 + 1 + 32 + 8 + 8 + 8 + 16;
        bytes[hlc_offset] ^= 0x01;
        write_log(&dir, &bytes);

        let result = CommitCoordinator::open(&dir, keys());
        assert!(
            matches!(result, Err(CommitError::ChainDiverged { commit_seq: 2 })),
            "a well-formed but tampered entry must be caught by the chain hash \
             and named by sequence; got {result:?}"
        );
    });
}

/// The truncation direction, swept: cutting the log at EVERY byte position
/// inside the final entry must recover the earlier commits and never error.
/// A single truncation point could pass by luck; the sweep cannot.
#[test]
fn truncation_anywhere_in_the_final_entry_recovers_the_prefix() {
    let dir = scratch_dir("truncation-sweep");
    under_lab(34, move |cx| {
        let (bytes, _) = three_commit_log(&dir, cx);
        let mut coordinator = CommitCoordinator::open(&dir, keys()).expect("open");
        let chain_snapshot = coordinator.chain().clone();
        let _ = coordinator.commit_with_crash(
            cx,
            &capsule_bytes(4),
            |seq, oid| marker_for(seq, oid, &chain_snapshot),
            Some(CrashPoint::AfterMarkerBeforeD2),
        );
        drop(coordinator);
        let full = log_bytes(&dir);
        let committed_len = bytes.len();

        // Failures are collected rather than raised, so a sweep reports EVERY
        // truncation point that misbehaves. Which positions fail is the whole
        // diagnostic — "the first one" would hide whether the rule is broken
        // at one offset or at all of them.
        let mut failures: Vec<String> = Vec::new();
        for cut in committed_len..full.len() {
            write_log(&dir, &full[..cut]);
            match CommitCoordinator::open(&dir, keys()) {
                Ok(reopened) => {
                    if reopened.chain().len() != 3 {
                        failures.push(format!(
                            "cut at {cut}: recovered {} committed entries, expected 3",
                            reopened.chain().len()
                        ));
                    }
                    if reopened.chain().verify() != Ok(()) {
                        failures.push(format!("cut at {cut}: recovered chain does not verify"));
                    }
                    if reopened.discarded_tail_bytes() != cut - committed_len {
                        failures.push(format!(
                            "cut at {cut}: reported {} discarded tail bytes, expected {}",
                            reopened.discarded_tail_bytes(),
                            cut - committed_len
                        ));
                    }
                }
                Err(error) => failures.push(format!("cut at {cut}: refused to open ({error})")),
            }
        }
        assert!(
            failures.is_empty(),
            "truncation inside the final entry must always recover the committed \
             prefix; {} of {} positions failed:\n{}",
            failures.len(),
            full.len() - committed_len,
            failures.join("\n")
        );
        assert!(
            full.len() > committed_len,
            "the sweep must cover at least one position, or it proves nothing"
        );
    });
}
