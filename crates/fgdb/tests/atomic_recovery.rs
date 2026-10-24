//! Compound writes use the actual marker boundary and ordinary recovery.
//! The unflushed-marker case explicitly models surviving bytes; truncating
//! its trailer models a torn tail. This is not a new power-loss VFS oracle.

use asupersync::lab::run_async_under_lab;
use fgdb::{CrashPoint, Database, DatabaseKeys, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::path::Path;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
    )
}

async fn seeded(cx: &CommitCx, path: &Path) -> Database {
    let mut db = Database::create(cx, path, keys()).await.unwrap();
    let mut seed = WriteBatch::new(R);
    for id in 1..=3 {
        seed.create_vertex(VId(id), vec![], vec![]);
    }
    db.write(cx, seed).await.unwrap();
    db
}

fn path_edges() -> Vec<WriteBatch> {
    let mut r = WriteBatch::new(R);
    r.add_edge(EId(10), VId(1), VId(2), vec![]);
    let mut s = WriteBatch::new(S);
    s.add_edge(EId(20), VId(2), VId(3), vec![]);
    vec![s, r]
}

#[test]
fn marker_boundary_recovers_both_relations_or_neither_including_torn_tail() {
    let ((), report) = run_async_under_lab(0xa703_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for (name, point, tear, committed) in [
            ("before", Some(CrashPoint::BeforeCapsule), false, false),
            (
                "capsule",
                Some(CrashPoint::AfterCapsuleBeforeD1),
                false,
                false,
            ),
            ("d1", Some(CrashPoint::AfterD1), false, false),
            (
                "marker-survived",
                Some(CrashPoint::AfterMarkerBeforeD2),
                false,
                true,
            ),
            (
                "marker-torn",
                Some(CrashPoint::AfterMarkerBeforeD2),
                true,
                false,
            ),
            (
                "marker-synced",
                Some(CrashPoint::AfterMarkerFileSyncBeforeDirectorySync),
                false,
                true,
            ),
            ("complete", None, false, true),
        ] {
            let path = std::env::temp_dir().join(format!(
                "fgdb-atomic-recovery-{}-{name}",
                std::process::id()
            ));
            let mut db = seeded(&cx, &path).await;
            let basis = db.frontier().unwrap();
            let pinned = db.read_session().unwrap();
            let prepared = db.prepare_atomic_writes(path_edges()).unwrap();
            let result = db.commit_prepared_with_crash(&cx, prepared, point).await;
            assert_eq!(
                result.is_ok(),
                point.is_none(),
                "crash point must be reached: {name}"
            );
            if matches!(
                point,
                Some(
                    CrashPoint::AfterMarkerBeforeD2
                        | CrashPoint::AfterMarkerFileSyncBeforeDirectorySync
                )
            ) {
                assert!(matches!(
                    result,
                    Err(WriteError::CommitOutcomeUnknown { .. })
                ));
                assert!(db.frontier().is_err());
            }
            assert!(pinned.edges().unwrap().is_empty());
            drop(db);
            if tear {
                fgdb_chronicle::CommitCoordinator::<asupersync::fs::UnixVfs>::tear_log_tail_for_test(&path, 1).unwrap();
            }
            let mut reopened = Database::open(&cx, &path, keys()).await.unwrap();
            reopened.ensure_delta_window(&cx, basis).await.unwrap();
            let expected_seq = CommitSeq(basis.0 + u64::from(committed));
            assert_eq!(reopened.frontier().unwrap(), expected_seq, "{name}");
            assert_eq!(
                reopened.edge(EId(10)).unwrap().is_some(),
                committed,
                "{name}"
            );
            assert_eq!(
                reopened.edge(EId(20)).unwrap().is_some(),
                committed,
                "{name}"
            );
            assert_eq!(
                reopened.delta_since(basis).unwrap().count(),
                usize::from(committed)
            );
            let vertices = reopened.vertices().unwrap();
            let edges = reopened.edges().unwrap();
            let versions = reopened.element_versions().unwrap().clone();
            drop(reopened);
            let rebuilt = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
            assert_eq!(rebuilt.frontier().unwrap(), expected_seq);
            assert_eq!(rebuilt.vertices().unwrap(), vertices);
            assert_eq!(rebuilt.edges().unwrap(), edges);
            assert_eq!(rebuilt.element_versions().unwrap(), &versions);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_birth_ordinals_versions_and_history_survive_compaction_and_rebuild() {
    let ((), report) = run_async_under_lab(0xa703_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let path = std::env::temp_dir().join(format!("fgdb-atomic-compact-{}", std::process::id()));
        let mut db = seeded(&cx, &path).await;
        let mut r = WriteBatch::new(R);
        r.create_vertex(VId(4), vec![], vec![(P, CanonicalScalar::Int(4))]);
        r.add_edge(EId(40), VId(4), VId(1), vec![]);
        let mut s = WriteBatch::new(S);
        s.create_vertex(VId(5), vec![], vec![(P, CanonicalScalar::Int(5))]);
        s.add_edge(EId(50), VId(5), VId(3), vec![]);
        let created = db.write_atomic(&cx, vec![s, r]).await.unwrap();
        assert_eq!(db.vertex(VId(4)).unwrap().unwrap().birth_ordinal, 1);
        assert_eq!(db.vertex(VId(5)).unwrap().unwrap().birth_ordinal, 3);
        let pinned = db.read_session().unwrap();
        let mut r = WriteBatch::new(R);
        r.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(44)));
        let mut s = WriteBatch::new(S);
        s.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(55)));
        let updated = db.write_atomic(&cx, vec![r, s]).await.unwrap();
        db.compact(&cx).await.unwrap();
        let versions = db.element_versions().unwrap().clone();
        let historical = db.vertices_at(created).unwrap();
        let current = db.vertices().unwrap();
        // A later generation knows the retirement time of an older row;
        // compare snapshot-visible content rather than that future metadata.
        let visible = |rows: Vec<fgdb::VertexRow>| {
            rows.into_iter()
                .map(|row| (row.vid, row.birth_ordinal, row.labels, row.props))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            visible(historical.clone()),
            visible(pinned.vertices().unwrap())
        );
        assert_ne!(historical, current);
        drop(db);
        let reopened = Database::open(&cx, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), updated);
        assert_eq!(reopened.vertices_at(created).unwrap(), historical);
        assert_eq!(reopened.vertices().unwrap(), current);
        assert_eq!(reopened.element_versions().unwrap(), &versions);
        drop(reopened);
        let mut rebuilt = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
        assert_eq!(rebuilt.vertices_at(created).unwrap(), historical);
        assert_eq!(rebuilt.vertices().unwrap(), current);
        assert_eq!(rebuilt.element_versions().unwrap(), &versions);
        let mut r = WriteBatch::new(R);
        r.delete_vertex(VId(4));
        let mut s = WriteBatch::new(S);
        s.delete_vertex(VId(5));
        rebuilt.write_atomic(&cx, vec![s, r]).await.unwrap();
        assert!(rebuilt.edge(EId(40)).unwrap().is_none());
        assert!(rebuilt.edge(EId(50)).unwrap().is_none());
        assert_eq!(pinned.edges().unwrap().len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn oversized_group_cannot_publish_an_independently_valid_group() {
    let ((), report) = run_async_under_lab(0xa703_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(&cx, seed).await.unwrap();
        let before = db.frontier().unwrap();
        let mut r = WriteBatch::new(R);
        r.create_vertex(VId(99), vec![], vec![]);
        let mut s = WriteBatch::new(S);
        s.set_vertex_property(
            VId(1),
            P,
            Some(CanonicalScalar::bytes(vec![0x41; 8000]).unwrap()),
        );
        s.set_vertex_property(
            VId(1),
            PropertyKeyId(2),
            Some(CanonicalScalar::bytes(vec![0x42; 9000]).unwrap()),
        );
        assert!(matches!(
            db.write_atomic(&cx, vec![r, s]).await,
            Err(WriteTxnError::Write(
                WriteError::VertexStorageAdmission { .. }
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert!(db.vertex(VId(1)).unwrap().unwrap().props.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[cfg(not(target_arch = "wasm32"))]
mod capsule_recovery {
    use super::*;
    use fgdb::{
        CAPSULE_OBJECT_KIND, CapsuleRecoveryError, DatabaseState, DerivedPublicationStage,
        OpenError,
    };
    use fgdb_chronicle::commit::restore::CapsuleRestoreError;
    use fgdb_chronicle::commit::{CAPSULE_DIR, COMMIT_LOG_NAME, CommitCoordinator};
    use fgdb_chronicle::identity::{
        CipherDescriptor, EncodingDescriptor, IdentifiedObject, VerificationOutcome,
    };
    use fgdb_chronicle::marker::EffectSource;
    use fgdb_chronicle::symbolize::{RecoveryTarget, encode_object, source_symbol_count};
    use fgdb_chronicle::transfer::{BondedPull, DonorId, PullLimits, VerifiedObject};
    use fgdb_types::ObjectId;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn directory(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "fgdb-capsule-recovery-{}-{epoch}-{}-{name}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn capsule_path(path: &Path, oid: ObjectId) -> PathBuf {
        let hex: String = oid.0.iter().map(|byte| format!("{byte:02x}")).collect();
        path.join(CAPSULE_DIR).join(format!("{hex}.capsule"))
    }

    // A real authenticated transfer, with a donor DEK and encoding different
    // from local capsule storage. No test constructor can forge VerifiedObject.
    fn authenticated(
        plaintext: &[u8],
        namespace: DatabaseSecurityNamespaceId,
        kind: u16,
    ) -> VerifiedObject {
        let keys = keys();
        let donor_dek = [0xb7; 32];
        let identified = IdentifiedObject::new(keys.k_oid(), namespace, kind, &[], plaintext);
        let protected = identified
            .protect(
                &donor_dek,
                CipherDescriptor {
                    object_kind: kind,
                    canonical_plaintext_len: plaintext.len() as u64,
                    codec_profile: 0,
                    compressed_len: plaintext.len() as u64,
                    data_crypto_profile: 1,
                    dek_id: [17; 16],
                    object_nonce: [29; 24],
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
            4,
            &donor_dek,
        )
        .unwrap();
        let mut pull = BondedPull::new(
            &encoding,
            RecoveryTarget {
                k_oid: keys.k_oid(),
                namespace,
                object_id: encoding.object_id(),
                canonical_header: &[],
                protected_len: len,
            },
            &donor_dek,
            &[DonorId(1), DonorId(2)],
            PullLimits::default(),
        )
        .unwrap();
        for request in pull.schedule(source_symbol_count(len, 512)).unwrap() {
            pull.accept(
                request.donor,
                &records[request.esi as usize],
                &mut Vec::new(),
            )
            .unwrap();
        }
        pull.try_recover(&mut Vec::new()).unwrap().unwrap()
    }

    async fn last_capsule(cx: &CommitCx, path: &Path) -> VerifiedObject {
        let coordinator = CommitCoordinator::open(cx, path, keys().capsule_keys())
            .await
            .unwrap();
        let EffectSource::Local { capsule_ref, .. } = &coordinator
            .chain()
            .entries()
            .last()
            .unwrap()
            .marker
            .effect_source;
        let oid = *capsule_ref;
        let bytes = coordinator
            .read_capsule(cx, oid, &mut Vec::new())
            .await
            .unwrap();
        let recovered = authenticated(&bytes, keys().namespace, CAPSULE_OBJECT_KIND);
        assert_eq!(recovered.object_id(), oid);
        assert_ne!(
            recovered.encoding().encoding_id().0,
            keys()
                .capsule_keys()
                .seal(&bytes)
                .unwrap()
                .descriptor
                .encoding_id,
        );
        recovered
    }

    #[test]
    fn authenticated_capsules_recover_the_required_suffix_and_normal_writes() {
        let ((), report) = run_async_under_lab(0xa703_0101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            for missing in [false, true] {
                let path = directory(if missing { "missing" } else { "damaged" });
                let mut db = seeded(&cx, &path).await;
                let basis = db.frontier().unwrap();
                let before = db.vertices().unwrap();
                let mut write = WriteBatch::new(R);
                write.create_vertex(VId(4), vec![], vec![(P, CanonicalScalar::Int(44))]);
                write.add_edge(EId(40), VId(3), VId(4), vec![]);
                assert!(
                    db.write_with_publication_failure(
                        &cx,
                        write,
                        DerivedPublicationStage::FoldCommittedTemplate,
                    )
                    .await
                    .is_err()
                );
                assert!(matches!(
                    db.state(),
                    DatabaseState::NeedsAuthoritativeRecovery(_)
                ));
                drop(db);
                let recovered = last_capsule(&cx, &path).await;
                let capsule = capsule_path(&path, recovered.object_id());
                let markers = std::fs::read(path.join(COMMIT_LOG_NAME)).unwrap();
                if missing {
                    std::fs::rename(&capsule, capsule.with_extension("lost-fixture")).unwrap();
                } else {
                    // The entire framed container is unusable: local FEC
                    // cannot supply the lost capsule from surviving symbols.
                    std::fs::write(&capsule, b"no surviving capsule symbols").unwrap();
                }
                assert!(Database::open(&cx, &path, keys()).await.is_err());
                assert!(
                    Database::open_restoring_capsules(&cx, &path, keys(), &[])
                        .await
                        .is_err(),
                    "capsule verification cannot replace ordinary suffix admission"
                );
                let objects = [recovered];
                let mut restored = if missing {
                    Database::open_restoring_capsules_with_vfs(
                        &cx,
                        asupersync::fs::UnixVfs::new(),
                        &path,
                        keys(),
                        &objects,
                    )
                    .await
                    .unwrap()
                } else {
                    Database::open_restoring_capsules(&cx, &path, keys(), &objects)
                        .await
                        .unwrap()
                };
                let committed = CommitSeq(basis.0 + 1);
                assert_eq!(restored.frontier().unwrap(), committed);
                assert_eq!(restored.vertices_at(basis).unwrap(), before);
                assert!(restored.vertex(VId(4)).unwrap().is_some());
                assert!(restored.edge(EId(40)).unwrap().is_some());
                restored.ensure_delta_window(&cx, basis).await.unwrap();
                assert_eq!(restored.delta_since(basis).unwrap().count(), 1);
                assert_eq!(std::fs::read(path.join(COMMIT_LOG_NAME)).unwrap(), markers);
                assert!(restored.crypto_verification_events().iter().any(|event| {
                    event.object_kind == CAPSULE_OBJECT_KIND
                        && event.outcome == VerificationOutcome::Accepted
                }));
                assert!(matches!(
                    Database::open(&cx, &path, keys()).await,
                    Err(OpenError::Commit(
                        fgdb_chronicle::CommitError::WriterAlreadyOpen
                    ))
                ));
                let mut next = WriteBatch::new(R);
                next.create_vertex(VId(5), vec![], vec![]);
                let next_seq = restored.write(&cx, next).await.unwrap();
                assert_eq!(next_seq, CommitSeq(committed.0 + 1));
                let vertices = restored.vertices().unwrap();
                let edges = restored.edges().unwrap();
                drop(restored);
                let reopened = Database::open(&cx, &path, keys()).await.unwrap();
                assert_eq!(reopened.frontier().unwrap(), next_seq);
                assert_eq!(reopened.vertices().unwrap(), vertices);
                assert_eq!(reopened.edges().unwrap(), edges);
                drop(reopened);
                let rebuilt = Database::open_rebuilding(&cx, &path, keys()).await.unwrap();
                assert_eq!(rebuilt.frontier().unwrap(), next_seq);
                assert_eq!(rebuilt.vertices().unwrap(), vertices);
                assert_eq!(rebuilt.edges().unwrap(), edges);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn capsule_recovery_preflights_the_whole_batch_and_excludes_other_writers() {
        let ((), report) = run_async_under_lab(0xa703_0102, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let path = directory("admission");
            drop(seeded(&cx, &path).await);
            let recovered = last_capsule(&cx, &path).await;
            let capsule = capsule_path(&path, recovered.object_id());
            let markers = std::fs::read(path.join(COMMIT_LOG_NAME)).unwrap();
            let held = Database::open(&cx, &path, keys()).await.unwrap();
            let damaged = b"unrecoverable committed capsule";
            std::fs::write(&capsule, damaged).unwrap();
            assert!(matches!(
                Database::open_restoring_capsules(
                    &cx,
                    &path,
                    keys(),
                    std::slice::from_ref(&recovered),
                )
                .await,
                Err(CapsuleRecoveryError::Open(OpenError::Commit(
                    fgdb_chronicle::CommitError::WriterAlreadyOpen
                )))
            ));
            assert_eq!(std::fs::read(&capsule).unwrap(), damaged);
            drop(held);

            let mut other_plaintext = recovered.plaintext().to_vec();
            other_plaintext.push(1);
            let invalid = [
                authenticated(
                    recovered.plaintext(),
                    DatabaseSecurityNamespaceId([0xee; 32]),
                    CAPSULE_OBJECT_KIND,
                ),
                authenticated(
                    recovered.plaintext(),
                    keys().namespace,
                    CAPSULE_OBJECT_KIND + 1,
                ),
                authenticated(&other_plaintext, keys().namespace, CAPSULE_OBJECT_KIND),
            ];
            for (case, invalid) in invalid.into_iter().enumerate() {
                let invalid_id = invalid.object_id();
                let objects = [
                    authenticated(recovered.plaintext(), keys().namespace, CAPSULE_OBJECT_KIND),
                    invalid,
                ];
                let error = Database::open_restoring_capsules(&cx, &path, keys(), &objects)
                    .await
                    .unwrap_err();
                let CapsuleRecoveryError::Capsule { object_id, source } = error else {
                    panic!("wrong admission error: {error:?}");
                };
                assert_eq!(object_id, invalid_id);
                assert!(matches!(
                    (case, source),
                    (0, CapsuleRestoreError::WrongNamespace)
                        | (1, CapsuleRestoreError::UnsupportedPayload)
                        | (2, CapsuleRestoreError::UnreferencedObject(_))
                ));
                assert_eq!(
                    std::fs::read(&capsule).unwrap(),
                    damaged,
                    "an invalid later object must refuse before the valid first repair"
                );
                assert!(!capsule_path(&path, invalid_id).exists());
                assert_eq!(std::fs::read(path.join(COMMIT_LOG_NAME)).unwrap(), markers);
            }
            let repaired = Database::open_restoring_capsules(
                &cx,
                &path,
                keys(),
                std::slice::from_ref(&recovered),
            )
            .await
            .unwrap();
            assert_eq!(repaired.frontier().unwrap(), CommitSeq::FIRST);
            assert_eq!(repaired.vertices().unwrap().len(), 3);
            // This root already covers the marker, so ordinary checkpoint
            // replay reads zero capsules. Restoration's verification evidence
            // still belongs to the returned database handle.
            assert!(!repaired.crypto_verification_events().is_empty());
            assert_ne!(std::fs::read(&capsule).unwrap(), damaged);
            assert_eq!(std::fs::read(path.join(COMMIT_LOG_NAME)).unwrap(), markers);
            drop(repaired);
            let reopened = Database::open_restoring_capsules(&cx, &path, keys(), &[])
                .await
                .unwrap();
            assert_eq!(reopened.vertices().unwrap().len(), 3);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn capsule_recovery_authenticates_the_database_before_replacing_any_bytes() {
        let ((), report) = run_async_under_lab(0xa703_0103, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.commit();
            let path = directory("wrong-key");
            drop(seeded(&cx, &path).await);
            let recovered = last_capsule(&cx, &path).await;
            let capsule = capsule_path(&path, recovered.object_id());
            let damaged = b"unrecoverable committed capsule";
            std::fs::write(&capsule, damaged).unwrap();
            for foreign_namespace in [false, true] {
                let correct = keys();
                let wrong = if foreign_namespace {
                    DatabaseKeys::new(
                        *correct.k_oid(),
                        DatabaseSecurityNamespaceId([0xee; 32]),
                        *correct.dek(),
                    )
                } else {
                    DatabaseKeys::new(*correct.k_oid(), correct.namespace, [0xee; 32])
                };
                let error = Database::open_restoring_capsules(
                    &cx,
                    &path,
                    wrong,
                    std::slice::from_ref(&recovered),
                )
                .await
                .unwrap_err();
                assert!(matches!(
                    (foreign_namespace, error),
                    (
                        false,
                        CapsuleRecoveryError::Open(OpenError::WrongDek { .. })
                    ) | (
                        true,
                        CapsuleRecoveryError::Open(OpenError::ForeignSlot { .. })
                    )
                ));
                assert_eq!(std::fs::read(&capsule).unwrap(), damaged);
            }
            let absent = directory("absent");
            assert!(matches!(
                Database::open_restoring_capsules(
                    &cx,
                    &absent,
                    keys(),
                    std::slice::from_ref(&recovered),
                )
                .await,
                Err(CapsuleRecoveryError::Open(OpenError::NotADatabase { .. }))
            ));
            assert!(!absent.exists());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
