use asupersync::lab::run_async_under_lab;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::{CAPSULE_DIR, COMMIT_LOG_NAME, CommitCoordinator, CommitError};
use fgdb_chronicle::marker::{CommitMarker, EffectSource, HeadUpdate};
use fgdb_chronicle::validate::{
    CommitDraft, CommitValidator, PassThroughValidator, ValidationRejection,
};
use fgdb_crypto::Digest;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, GraphId, MarkerRef, ObjectId};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-fcw-seam-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create isolated Chronicle test directory");
    dir
}

fn keys() -> CapsuleKeys {
    CapsuleKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
        0x0274,
        CapsuleProfile::balanced(),
    )
}

fn digest(seed: u8) -> Digest {
    Digest([seed; 32])
}

fn marker_for(
    commit_seq: u64,
    capsule_ref: ObjectId,
    expected_previous: Option<MarkerRef>,
) -> CommitMarker {
    CommitMarker {
        logical_command_seq: commit_seq * 10,
        commit_seq,
        effect_source: EffectSource::Local {
            capsule_ref,
            logical_delta_template_digest: digest(commit_seq as u8),
        },
        prev_global: None,
        head_updates: vec![HeadUpdate {
            graph: GRAPH,
            branch: BRANCH,
            expected_previous,
        }],
        merge_record_oid: None,
        coordinate_schema_transition_digest: digest(0x31),
        topology_epoch: 1,
        policy_epoch: 2,
        revocation_index: 3,
        txn_token: [0x44; 16],
        commit_hlc: 1_000 + commit_seq,
        final_effect_digest: digest(0x51),
        authorization_decision_digest: digest(0x52),
        resource_effect_digest: digest(0x53),
        payload_availability_certificate_oid: None,
        flags: 0,
    }
}

async fn commit_plaintext(
    coordinator: &mut CommitCoordinator,
    cx: &CommitCx,
    plaintext: &[u8],
) -> Result<MarkerRef, CommitError> {
    let expected_previous = coordinator.chain().head(GRAPH, BRANCH);
    coordinator
        .commit(cx, plaintext, |commit_seq, capsule_ref| {
            marker_for(commit_seq, capsule_ref, expected_previous)
        })
        .await
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
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn capsule_names(dir: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(dir.join(CAPSULE_DIR))
        .expect("read capsule directory")
        .map(|entry| entry.expect("read capsule entry").file_name())
        .collect();
    names.sort();
    names
}

fn log_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join(COMMIT_LOG_NAME)).unwrap_or_default()
}

/// Observations one validator wants to share with the test body.
type SharedValidatorObservations = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

#[derive(Debug)]
struct RejectChosenPlaintext {
    rejected: Vec<u8>,
    seen: SharedValidatorObservations,
}

impl CommitValidator for RejectChosenPlaintext {
    fn validate(&mut self, draft: &CommitDraft<'_>) -> Result<(), ValidationRejection> {
        self.seen
            .lock()
            .expect("validator observation lock")
            .push((draft.commit_seq.0, draft.capsule_plaintext.to_vec()));
        if draft.capsule_plaintext == self.rejected {
            return Err(ValidationRejection {
                law: "fixture:reject-chosen-plaintext",
                detail: "chosen capsule plaintext was refused".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
struct AcceptTwiceThenReject {
    seen_sequences: Arc<Mutex<Vec<u64>>>,
}

impl CommitValidator for AcceptTwiceThenReject {
    fn validate(&mut self, draft: &CommitDraft<'_>) -> Result<(), ValidationRejection> {
        let mut seen = self
            .seen_sequences
            .lock()
            .expect("stateful validator observation lock");
        seen.push(draft.commit_seq.0);
        if seen.len() > 2 {
            return Err(ValidationRejection {
                law: "fixture:accept-two",
                detail: "stateful fixture refuses its third draft".to_string(),
            });
        }
        Ok(())
    }
}

#[test]
fn bare_open_installs_pass_through_validator() {
    under_lab(0xfc_01, |cx| async move {
        let dir = scratch_dir("default-pass-through");
        let mut coordinator = CommitCoordinator::open(&cx, &dir, keys())
            .await
            .expect("open bare coordinator");

        let debug = format!("{coordinator:?}");
        assert!(
            debug.contains("validator: PassThroughValidator"),
            "bare open must retain the PassThroughValidator instance: {debug}"
        );

        let committed = commit_plaintext(&mut coordinator, &cx, b"default accepts")
            .await
            .expect("default pass-through commit");
        assert_eq!(committed.commit_seq.0, 1);
    });
}

#[test]
fn rejection_is_trace_free_and_pass_through_reuses_the_sequence() {
    under_lab(0xfc_02, |cx| async move {
        let dir = scratch_dir("rejection-is-free");
        let mut coordinator = CommitCoordinator::open(&cx, &dir, keys())
            .await
            .expect("open coordinator");
        let rejected_plaintext = b"reject this capsule".to_vec();
        let seen = Arc::new(Mutex::new(Vec::new()));
        coordinator.set_validator(Box::new(RejectChosenPlaintext {
            rejected: rejected_plaintext.clone(),
            seen: seen.clone(),
        }));

        let seq_before = coordinator.next_commit_seq().expect("read next sequence");
        let head_before = coordinator.chain().head(GRAPH, BRANCH);
        let chain_value_before = coordinator.chain().chain_value();
        let capsules_before = capsule_names(&dir);
        let log_before = log_bytes(&dir);

        let rejected = commit_plaintext(&mut coordinator, &cx, &rejected_plaintext).await;
        assert!(
            matches!(
                rejected,
                Err(CommitError::Rejected(ref rejection))
                    if rejection.law == "fixture:reject-chosen-plaintext"
            ),
            "chosen plaintext must surface its typed validation rejection: {rejected:?}"
        );
        assert_eq!(
            *seen.lock().expect("validator observation lock"),
            vec![(seq_before.0, rejected_plaintext)]
        );
        assert_eq!(
            coordinator
                .next_commit_seq()
                .expect("sequence remains readable"),
            seq_before
        );
        assert_eq!(coordinator.chain().head(GRAPH, BRANCH), head_before);
        assert_eq!(coordinator.chain().chain_value(), chain_value_before);
        assert_eq!(
            capsule_names(&dir),
            capsules_before,
            "rejection wrote a capsule"
        );
        assert_eq!(log_bytes(&dir), log_before, "rejection wrote a marker");
        assert!(!coordinator.is_poisoned(), "rejection poisoned coordinator");

        coordinator.set_validator(Box::new(PassThroughValidator));
        let committed = commit_plaintext(&mut coordinator, &cx, b"different accepted capsule")
            .await
            .expect("pass-through commit after rejection");
        assert_eq!(
            committed.commit_seq, seq_before,
            "the accepted commit must consume the sequence the rejection left free"
        );
    });
}

#[test]
fn stateful_validator_accepts_twice_then_refuses_without_durable_residue() {
    under_lab(0xfc_03, |cx| async move {
        let dir = scratch_dir("two-accepts-then-refuse");
        let mut coordinator = CommitCoordinator::open(&cx, &dir, keys())
            .await
            .expect("open coordinator");
        let seen_sequences = Arc::new(Mutex::new(Vec::new()));
        coordinator.set_validator(Box::new(AcceptTwiceThenReject {
            seen_sequences: seen_sequences.clone(),
        }));

        commit_plaintext(&mut coordinator, &cx, b"accepted one")
            .await
            .expect("first stateful acceptance");
        commit_plaintext(&mut coordinator, &cx, b"accepted two")
            .await
            .expect("second stateful acceptance");

        let seq_before_refusal = coordinator.next_commit_seq().expect("read next sequence");
        let head_before_refusal = coordinator.chain().head(GRAPH, BRANCH);
        let chain_value_before_refusal = coordinator.chain().chain_value();
        let capsules_before_refusal = capsule_names(&dir);
        let log_before_refusal = log_bytes(&dir);

        let refused = commit_plaintext(&mut coordinator, &cx, b"refused three").await;
        assert!(
            matches!(
                refused,
                Err(CommitError::Rejected(ref rejection))
                    if rejection.law == "fixture:accept-two"
            ),
            "third stateful verdict must be a typed rejection: {refused:?}"
        );
        assert_eq!(
            *seen_sequences
                .lock()
                .expect("stateful validator observation lock"),
            vec![1, 2, 3],
            "both accepts and the refusal must traverse the same stateful validator"
        );
        assert_eq!(
            coordinator
                .next_commit_seq()
                .expect("sequence remains readable"),
            seq_before_refusal
        );
        assert_eq!(coordinator.chain().head(GRAPH, BRANCH), head_before_refusal);
        assert_eq!(
            coordinator.chain().chain_value(),
            chain_value_before_refusal
        );
        assert_eq!(capsule_names(&dir), capsules_before_refusal);
        assert_eq!(log_bytes(&dir), log_before_refusal);
        assert!(!coordinator.is_poisoned());
    });
}
