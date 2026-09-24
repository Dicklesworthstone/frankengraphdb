#![cfg(not(target_arch = "wasm32"))]

//! Actual bonded crypto/FEC and UnixVfs capsule publication/reopen. Crash hooks
//! stop the real storage path; they do not model power loss or truthful hardware.

use asupersync::runtime::RuntimeBuilder;
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile, encode_container};
use fgdb_chronicle::commit::restore::CapsuleRestoreError;
use fgdb_chronicle::commit::{CAPSULE_DIR, COMMIT_LOG_NAME, CommitCoordinator, CrashPoint};
use fgdb_chronicle::identity::{
    CipherDescriptor, CryptoVerificationEvent, CryptoVerificationSink, EncodingDescriptor,
    IdentifiedObject,
};
use fgdb_chronicle::marker::{CommitMarker, EffectSource};
use fgdb_chronicle::scrub::ScrubCrashPoint;
use fgdb_chronicle::symbolize::{RecoveryTarget, encode_object, source_symbol_count};
use fgdb_chronicle::transfer::{BondedPull, DonorId, PullLimits, VerifiedObject};
use fgdb_crypto::Digest;
use fgdb_types::context::PurposeContexts;
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, ObjectId};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const KEY: [u8; 32] = [0x59; 32];
const LOCAL_DEK: [u8; 32] = [0x63; 32];
const DONOR_DEK: [u8; 32] = [0x91; 32];
const KIND: u16 = 0x0274;
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> CapsuleKeys {
    CapsuleKeys::new(KEY, NS, LOCAL_DEK, KIND, CapsuleProfile::balanced())
}

fn directory() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "fgdb-restored-capsule-{}-{epoch}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    path
}

fn capsule_path(dir: &Path, oid: ObjectId) -> PathBuf {
    let hex: String = oid.0.iter().map(|byte| format!("{byte:02x}")).collect();
    dir.join(CAPSULE_DIR).join(format!("{hex}.capsule"))
}

fn marker(seq: u64, capsule_ref: ObjectId) -> CommitMarker {
    CommitMarker {
        logical_command_seq: seq,
        commit_seq: seq,
        effect_source: EffectSource::Local {
            capsule_ref,
            logical_delta_template_digest: Digest([3; 32]),
        },
        prev_global: None,
        head_updates: Vec::new(),
        merge_record_oid: None,
        coordinate_schema_transition_digest: Digest([4; 32]),
        topology_epoch: 1,
        policy_epoch: 1,
        revocation_index: 0,
        txn_token: [0; 16],
        commit_hlc: seq,
        final_effect_digest: Digest([5; 32]),
        authorization_decision_digest: Digest([6; 32]),
        resource_effect_digest: Digest([7; 32]),
        payload_availability_certificate_oid: None,
        flags: 0,
    }
}

/// A different physical encoding and encryption key, but the same logical
/// capsule. Every returned object came through actual MAC/FEC/AEAD/OID checks.
fn recovered_with(
    plaintext: &[u8],
    key: &[u8; 32],
    namespace: DatabaseSecurityNamespaceId,
    kind: u16,
    header: &[u8],
    codec: u16,
) -> VerifiedObject {
    let identified = IdentifiedObject::new(key, namespace, kind, header, plaintext);
    let protected = identified
        .protect(
            &DONOR_DEK,
            CipherDescriptor {
                object_kind: kind,
                canonical_plaintext_len: plaintext.len() as u64,
                codec_profile: codec,
                compressed_len: plaintext.len() as u64,
                data_crypto_profile: 1,
                dek_id: [11; 16],
                object_nonce: [19; 24],
                object_tag_len: 16,
            },
            plaintext,
        )
        .unwrap();
    let len = protected.protected_bytes().len();
    let encoding = protected.encode(EncodingDescriptor {
        fec_profile: 1,
        transfer_length: len as u64,
        oti_common: 0,
        oti_scheme: 0,
        symbol_size: 512,
        source_block_count: 1,
        symbol_auth_profile: 1,
    });
    let records = encode_object(
        &encoding,
        protected.protected_bytes(),
        kind,
        0,
        8,
        &DONOR_DEK,
    )
    .unwrap();
    let mut pull = BondedPull::new(
        &encoding,
        RecoveryTarget {
            k_oid: key,
            namespace,
            object_id: encoding.object_id(),
            canonical_header: header,
            protected_len: len,
        },
        &DONOR_DEK,
        &[DonorId(1), DonorId(2), DonorId(3)],
        PullLimits::default(),
    )
    .unwrap();
    let requests = pull.schedule(source_symbol_count(len, 512)).unwrap();
    let mut donors = std::collections::BTreeSet::new();
    for request in requests.into_iter().rev() {
        donors.insert(request.donor);
        pull.accept(
            request.donor,
            &records[request.esi as usize],
            &mut Vec::new(),
        )
        .unwrap();
    }
    if len > 1024 {
        assert_eq!(donors.len(), 3, "the fixture must actually combine donors");
    }
    pull.try_recover(&mut Vec::new()).unwrap().unwrap()
}

fn recovered(plaintext: &[u8]) -> VerifiedObject {
    recovered_with(plaintext, &KEY, NS, KIND, &[], 0)
}

#[test]
fn bonded_bytes_restore_missing_and_locally_unrecoverable_committed_capsules() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    runtime.block_on(async {
        let plaintext: Vec<_> = (0..4096).map(|i| (i % 251) as u8).collect();
        let object = recovered(&plaintext);
        let local = keys().seal(&plaintext).unwrap();
        assert_ne!(
            object.encoding().encoding_id().0,
            local.descriptor.encoding_id
        );
        for missing in [false, true] {
            let dir = directory();
            let mut owner = CommitCoordinator::open(&cx, &dir, keys()).await.unwrap();
            owner.commit(&cx, &plaintext, marker).await.unwrap();
            let oid = object.object_id();
            let path = capsule_path(&dir, oid);
            let before = std::fs::read(dir.join(COMMIT_LOG_NAME)).unwrap();
            let chain = owner.chain().chain_value();
            // Damage arrives from outside the storage implementation. Keeping
            // the old inode under another name is not restoration's mechanism.
            if missing {
                std::fs::rename(&path, path.with_extension("lost-fixture")).unwrap();
            } else {
                std::fs::write(&path, b"irrecoverable capsule").unwrap();
            }
            assert!(owner.read_capsule(&cx, oid, &mut Vec::new()).await.is_err());
            assert_eq!(
                owner
                    .scrub_capsules(&cx, CommitSeq(1), None, &mut Vec::new())
                    .await
                    .unwrap()
                    .lost
                    .len(),
                1
            );
            owner
                .restore_capsule(&cx, oid, &object, &mut Vec::new())
                .await
                .unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), encode_container(&local));
            assert_eq!(std::fs::read(dir.join(COMMIT_LOG_NAME)).unwrap(), before);
            assert_eq!(owner.chain().chain_value(), chain);
            assert_eq!(owner.next_commit_seq().unwrap(), CommitSeq(2));
            drop(owner);
            let mut reopened = CommitCoordinator::open(&cx, &dir, keys()).await.unwrap();
            assert_eq!(
                reopened
                    .read_capsule(&cx, oid, &mut Vec::new())
                    .await
                    .unwrap(),
                plaintext
            );
            // Identical retry closes durability again without a new staging file.
            let entries = std::fs::read_dir(dir.join(CAPSULE_DIR)).unwrap().count();
            reopened
                .restore_capsule(&cx, oid, &object, &mut Vec::new())
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_dir(dir.join(CAPSULE_DIR)).unwrap().count(),
                entries
            );
            assert_eq!(
                reopened
                    .scrub_capsules(&cx, CommitSeq(1), None, &mut Vec::new())
                    .await
                    .unwrap()
                    .clean,
                [oid]
            );
        }
    });
}

#[test]
fn invalid_objects_never_rewrite_a_capsule_or_acquire_publication_state() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    runtime.block_on(async {
        let dir = directory();
        let mut owner = CommitCoordinator::open(&cx, &dir, keys()).await.unwrap();
        let plaintext = vec![8; 2048];
        owner.commit(&cx, &plaintext, marker).await.unwrap();
        let valid = recovered(&plaintext);
        let oid = valid.object_id();
        let path = capsule_path(&dir, oid);
        let before = std::fs::read(&path).unwrap();
        let candidates = [
            recovered(b"unreferenced plaintext"),
            recovered_with(
                &plaintext,
                &KEY,
                DatabaseSecurityNamespaceId([2; 32]),
                KIND,
                &[],
                0,
            ),
            recovered_with(&plaintext, &KEY, NS, KIND + 1, &[], 0),
            recovered_with(&plaintext, &KEY, NS, KIND, &[], 1),
        ];
        for (index, candidate) in candidates.iter().enumerate() {
            let error = owner
                .restore_capsule(&cx, candidate.object_id(), candidate, &mut Vec::new())
                .await
                .unwrap_err();
            assert!(match index {
                0 => matches!(error, CapsuleRestoreError::UnreferencedObject(_)),
                1 => matches!(error, CapsuleRestoreError::WrongNamespace),
                _ => matches!(error, CapsuleRestoreError::UnsupportedPayload),
            });
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(!owner.is_poisoned());
        }
        assert!(matches!(
            owner
                .restore_capsule(&cx, ObjectId([0; 32]), &valid, &mut Vec::new())
                .await,
            Err(CapsuleRestoreError::WrongObject)
        ));
        assert_eq!(std::fs::read_dir(dir.join(CAPSULE_DIR)).unwrap().count(), 1);
        assert_eq!(owner.next_commit_seq().unwrap(), CommitSeq(2));
    });
}

#[test]
fn an_authentic_object_from_another_identity_key_cannot_repair_local_history() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    runtime.block_on(async {
        let dir = directory();
        let plaintext = vec![9; 2048];
        let mut owner = CommitCoordinator::open(&cx, &dir, keys()).await.unwrap();
        owner.commit(&cx, &plaintext, marker).await.unwrap();
        let object = recovered(&plaintext);
        let path = capsule_path(&dir, object.object_id());
        std::fs::write(&path, b"damaged").unwrap();
        drop(owner);
        let wrong = CapsuleKeys::new([0xaa; 32], NS, LOCAL_DEK, KIND, CapsuleProfile::balanced());
        let mut owner = CommitCoordinator::open(&cx, &dir, wrong).await.unwrap();
        assert!(matches!(
            owner
                .restore_capsule(&cx, object.object_id(), &object, &mut Vec::new())
                .await,
            Err(CapsuleRestoreError::LocalIdentityMismatch)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"damaged");
        assert!(!owner.is_poisoned());
    });
}

#[test]
fn every_scrub_publication_cut_is_old_or_exact_new_and_retry_uses_recovered_ownership() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    runtime.block_on(async {
        let plaintext = vec![17; 2048];
        let object = recovered(&plaintext);
        let expected = encode_container(&keys().seal(&plaintext).unwrap());
        for point in [
            ScrubCrashPoint::AfterTempCreate,
            ScrubCrashPoint::AfterTempWrite,
            ScrubCrashPoint::AfterTempFlush,
            ScrubCrashPoint::AfterTempFileSync,
            ScrubCrashPoint::AfterRename,
            ScrubCrashPoint::AfterDirectorySync,
        ] {
            let dir = directory();
            let mut owner = CommitCoordinator::open(&cx, &dir, keys()).await.unwrap();
            owner.commit(&cx, &plaintext, marker).await.unwrap();
            let log = std::fs::read(dir.join(COMMIT_LOG_NAME)).unwrap();
            let path = capsule_path(&dir, object.object_id());
            std::fs::write(&path, b"damaged").unwrap();
            assert!(
                owner
                    .restore_capsule_with_crash(
                        &cx,
                        object.object_id(),
                        &object,
                        &mut Vec::new(),
                        Some(point)
                    )
                    .await
                    .is_err(),
                "{point:?}"
            );
            assert!(owner.is_poisoned());
            assert!(matches!(
                owner
                    .restore_capsule(&cx, object.object_id(), &object, &mut Vec::new())
                    .await,
                Err(CapsuleRestoreError::RecoveryRequired)
            ));
            let after = std::fs::read(&path).unwrap();
            if matches!(
                point,
                ScrubCrashPoint::AfterRename | ScrubCrashPoint::AfterDirectorySync
            ) {
                assert_eq!(after, expected);
            } else {
                assert_eq!(after, b"damaged");
            }
            assert_eq!(std::fs::read(dir.join(COMMIT_LOG_NAME)).unwrap(), log);
            drop(owner);
            let mut owner = CommitCoordinator::open(&cx, &dir, keys()).await.unwrap();
            owner
                .restore_capsule(&cx, object.object_id(), &object, &mut Vec::new())
                .await
                .unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), expected);
            assert_eq!(std::fs::read(dir.join(COMMIT_LOG_NAME)).unwrap(), log);
            assert_eq!(
                owner
                    .read_capsule(&cx, object.object_id(), &mut Vec::new())
                    .await
                    .unwrap(),
                plaintext
            );
        }
    });
}

#[test]
fn nonregular_destinations_and_uncertain_commit_owners_refuse_repair() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    runtime.block_on(async {
        let plaintext = vec![4; 2048];
        let object = recovered(&plaintext);
        let dir = directory();
        let mut owner = CommitCoordinator::open(&cx, &dir, keys()).await.unwrap();
        owner.commit(&cx, &plaintext, marker).await.unwrap();
        let path = capsule_path(&dir, object.object_id());
        std::fs::rename(&path, path.with_extension("retained-fixture")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            owner
                .restore_capsule(&cx, object.object_id(), &object, &mut Vec::new())
                .await,
            Err(CapsuleRestoreError::NonRegularDestination)
        ));
        assert!(path.is_dir());
        assert!(!owner.is_poisoned());
        assert!(
            owner
                .commit_with_crash(
                    &cx,
                    b"uncertain",
                    marker,
                    Some(CrashPoint::AfterMarkerBeforeD2)
                )
                .await
                .is_err()
        );
        assert!(matches!(
            owner
                .restore_capsule(&cx, object.object_id(), &object, &mut Vec::new())
                .await,
            Err(CapsuleRestoreError::RecoveryRequired)
        ));
    });
}

#[test]
fn final_verifier_unwind_fences_a_completed_replacement() {
    struct PanicSink;
    impl CryptoVerificationSink for PanicSink {
        fn record(&mut self, _: CryptoVerificationEvent) {
            panic!("verification callback failure");
        }
    }
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    let dir = directory();
    let plaintext = vec![5; 2048];
    let object = recovered(&plaintext);
    let mut owner = runtime
        .block_on(CommitCoordinator::open(&cx, &dir, keys()))
        .unwrap();
    runtime
        .block_on(owner.commit(&cx, &plaintext, marker))
        .unwrap();
    std::fs::write(capsule_path(&dir, object.object_id()), b"damaged").unwrap();
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(owner.restore_capsule(
                &cx,
                object.object_id(),
                &object,
                &mut PanicSink,
            ))
        }))
        .is_err()
    );
    assert!(owner.is_poisoned());
    assert_eq!(
        std::fs::read(capsule_path(&dir, object.object_id())).unwrap(),
        encode_container(&keys().seal(&plaintext).unwrap())
    );
}
