//! The append entry point may reuse only an exact authenticated publication.
//! Suffix admission and durable publication retain the full-root refusals.

use super::*;
use fgdb_strata::manifest::ManifestRecord;
use fgdb_strata::root::{PatchRef, encode_root, encode_root_v4, validate_root};
use fgdb_strata::store::RootAppend;
use fgdb_strata::vertex::{VertexRow, encode_patch};

fn vertex(vid: u128, created: u64) -> VertexRow {
    VertexRow {
        vid: VId(vid),
        birth_ordinal: vid as u64,
        created_at: CommitSeq(created),
        retired_at: None,
        labels: vec![],
        props: vec![],
    }
}

async fn base(cx: &CommitCx, store: &BlockStore) -> PartitionRoot {
    let bytes = encode_block(0, None, &[edge(10, 1, 2, 4, None)]).unwrap();
    let block = store.put(cx, &bytes).await.unwrap();
    let bytes = encode_patch(&[vertex(1, 4), vertex(2, 4)]).unwrap();
    let patch = store.put_patch(cx, &bytes).await.unwrap();
    PartitionRoot {
        graph: GraphId(1),
        branch: BranchId(1),
        partition: 0,
        published_at: CommitSeq(4),
        blocks: vec![BlockRef {
            block_id: block.0,
            first_seq: CommitSeq(4),
            last_seq: CommitSeq(4),
        }],
        vertex_patches: vec![PatchRef {
            patch_id: patch.0,
            first_seq: CommitSeq(4),
            last_seq: CommitSeq(4),
        }],
    }
}

fn append<'a>(
    basis: PartitionRootVersion,
    root: &PartitionRoot,
    blocks: &'a [BlockRef],
    vertex_patches: &'a [PatchRef],
) -> RootAppend<'a> {
    RootAppend {
        basis,
        graph: root.graph,
        branch: root.branch,
        partition: root.partition,
        published_at: CommitSeq(8),
        carried_blocks: root.blocks.len(),
        carried_patches: root.vertex_patches.len(),
        blocks,
        vertex_patches,
    }
}

fn records(root: &PartitionRoot, id: PartitionRootVersion) -> [ManifestRecord; 1] {
    [ManifestRecord {
        graph: root.graph,
        branch: root.branch,
        partition: root.partition,
        root: id,
        published_chain_hash: fgdb_crypto::Digest([0xa1; 32]),
    }]
}

#[test]
fn only_an_exact_basis_and_store_identity_can_reuse_the_append_memo() {
    let dir = scratch_dir("append-exact-basis");
    under_lab(0xa990, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE).await.unwrap();
        let root = base(&cx, &store).await;
        let mut receipts = PublishReceipts::new();
        let basis = store
            .put_root_verified(&cx, &root, &mut receipts)
            .await
            .unwrap();
        let request = append(basis, &root, &[], &[]);
        for mismatch in [
            RootAppend {
                basis: PartitionRootVersion(ObjectId([0x13; 32])),
                ..request
            },
            RootAppend {
                graph: GraphId(u128::MAX),
                ..request
            },
            RootAppend {
                branch: BranchId(u128::MAX),
                ..request
            },
            RootAppend {
                partition: 1,
                ..request
            },
            RootAppend {
                carried_blocks: 2,
                ..request
            },
            RootAppend {
                carried_patches: 2,
                ..request
            },
            RootAppend {
                published_at: CommitSeq(3),
                ..request
            },
        ] {
            assert!(
                store
                    .verify_root_append(&cx, mismatch, &mut receipts)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                receipts.verified_root_prefix(0),
                (1, 1),
                "a miss preserves the prior memo"
            );
        }
        for (at, (key, namespace)) in [
            (K_OID, DatabaseSecurityNamespaceId([0x78; 32])),
            ([0x5b; 32], NAMESPACE),
        ]
        .into_iter()
        .enumerate()
        {
            let other_dir = dir.join(format!("other-{at}"));
            std::fs::create_dir_all(&other_dir).unwrap();
            let other = BlockStore::open(&cx, &other_dir, key, namespace)
                .await
                .unwrap();
            assert!(
                other
                    .verify_root_append(&cx, request, &mut receipts)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(receipts.verified_root_prefix(0), (1, 1));
        }
        assert!(
            store
                .verify_root_append(&cx, request, &mut PublishReceipts::new())
                .await
                .unwrap()
                .is_none()
        );
        let verified = store
            .verify_root_append(&cx, request, &mut receipts)
            .await
            .unwrap()
            .expect("the exact basis admits a no-object publication");
        let mut next = root.clone();
        next.published_at = request.published_at;
        let cold = encode_root_v5(&next, &K_OID, NAMESPACE, &mut SegmentCache::default()).unwrap();
        assert_eq!(
            verified.id().0,
            derive_root_id(&K_OID, NAMESPACE, &cold.bytes)
        );
    });
}

#[test]
fn append_refuses_regression_and_false_spans_at_both_list_boundaries() {
    let dir = scratch_dir("append-boundaries");
    under_lab(0xa991, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE).await.unwrap();
        let root = base(&cx, &store).await;
        let added_block = store
            .put(
                &cx,
                &encode_block(0, None, &[edge(20, 3, 4, 6, None)]).unwrap(),
            )
            .await
            .unwrap();
        let added_patch = store
            .put_patch(&cx, &encode_patch(&[vertex(3, 6)]).unwrap())
            .await
            .unwrap();
        let block = BlockRef {
            block_id: added_block.0,
            first_seq: CommitSeq(6),
            last_seq: CommitSeq(6),
        };
        let patch = PatchRef {
            patch_id: added_patch.0,
            first_seq: CommitSeq(6),
            last_seq: CommitSeq(6),
        };
        let mut receipts = PublishReceipts::new();
        for (blocks, patches, shape_error) in [
            (
                vec![BlockRef {
                    first_seq: CommitSeq(3),
                    last_seq: CommitSeq(3),
                    ..block
                }],
                vec![],
                true,
            ),
            (
                vec![],
                vec![PatchRef {
                    first_seq: CommitSeq(3),
                    last_seq: CommitSeq(3),
                    ..patch
                }],
                true,
            ),
            (
                vec![BlockRef {
                    first_seq: CommitSeq(5),
                    ..block
                }],
                vec![],
                false,
            ),
            (
                vec![],
                vec![PatchRef {
                    first_seq: CommitSeq(5),
                    ..patch
                }],
                false,
            ),
        ] {
            let basis = store
                .put_root_verified(&cx, &root, &mut receipts)
                .await
                .unwrap();
            let request = append(basis, &root, &blocks, &patches);
            let refused = store
                .verify_root_append(&cx, request, &mut receipts)
                .await
                .expect_err("the suffix is unlawful");
            match refused {
                StoreError::MalformedRoot(error) if shape_error => {
                    let mut candidate = root.clone();
                    candidate.published_at = request.published_at;
                    candidate.blocks.extend(blocks);
                    candidate.vertex_patches.extend(patches);
                    assert_eq!(error, validate_root(&candidate).unwrap_err());
                }
                StoreError::MalformedRoot(RootError::BlockRangeMismatch { at: 1, .. })
                    if !blocks.is_empty() => {}
                StoreError::MalformedRoot(RootError::PatchRangeMismatch { at: 1, .. })
                    if !patches.is_empty() => {}
                other => panic!("wrong suffix refusal: {other:?}"),
            }
            assert_eq!(
                receipts.verified_root_prefix(0),
                (0, 0),
                "a refused append discards its memo"
            );
            assert!(
                store
                    .verify_root_append(&cx, append(basis, &root, &[], &[]), &mut receipts)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    });
}

#[test]
fn append_keeps_chain_history_and_appended_object_authentication() {
    let dir = scratch_dir("append-history");
    under_lab(0xa992, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE).await.unwrap();
        let root = base(&cx, &store).await;
        let mut receipts = PublishReceipts::new();
        let basis = store
            .put_root_verified(&cx, &root, &mut receipts)
            .await
            .unwrap();
        let unlinked = encode_block(0, None, &[edge(11, 1, 3, 6, None)]).unwrap();
        let unlinked = store
            .put_verified(&cx, &unlinked, None, &mut receipts)
            .await
            .unwrap();
        assert!(
            receipts.holds(unlinked),
            "the chain law must run even for an admitted object"
        );
        let suffix = [BlockRef {
            block_id: unlinked.0,
            first_seq: CommitSeq(6),
            last_seq: CommitSeq(6),
        }];
        assert!(
            matches!(store.verify_root_append(&cx, append(basis, &root, &suffix, &[]), &mut receipts).await,
            Err(StoreError::MalformedRoot(RootError::BlockChainMismatch { at: 1, declared: None, expected: Some(id) })) if id == root.blocks[0].block_id)
        );

        store
            .put_root_verified(&cx, &root, &mut receipts)
            .await
            .unwrap();
        let reused = encode_block(
            0,
            Some(DeltaBlockVersion(root.blocks[0].block_id)),
            &[edge(10, 1, 3, 6, None)],
        )
        .unwrap();
        let reused = store.put(&cx, &reused).await.unwrap();
        let suffix = [BlockRef {
            block_id: reused.0,
            first_seq: CommitSeq(6),
            last_seq: CommitSeq(6),
        }];
        let result = store
            .verify_root_append(&cx, append(basis, &root, &suffix, &[]), &mut receipts)
            .await
            .map(|verified| verified.expect("matching basis").id());
        assert_identity_conflict(
            result,
            EId(10),
            EdgeBirth {
                src: VId(1),
                relation: REL,
                dst: VId(2),
                created_at: CommitSeq(4),
            },
            EdgeBirth {
                src: VId(1),
                relation: REL,
                dst: VId(3),
                created_at: CommitSeq(6),
            },
        );

        store
            .put_root_verified(&cx, &root, &mut receipts)
            .await
            .unwrap();
        let reused = store
            .put_patch(&cx, &encode_patch(&[vertex(1, 6)]).unwrap())
            .await
            .unwrap();
        let suffix = [PatchRef {
            patch_id: reused.0,
            first_seq: CommitSeq(6),
            last_seq: CommitSeq(6),
        }];
        assert!(matches!(
            store
                .verify_root_append(&cx, append(basis, &root, &[], &suffix), &mut receipts)
                .await,
            Err(StoreError::MalformedRoot(
                RootError::VertexIdentityMismatch { vid: VId(1), .. }
            ))
        ));

        store
            .put_root_verified(&cx, &root, &mut receipts)
            .await
            .unwrap();
        let bytes = encode_block(0, None, &[edge(30, 7, 8, 6, None)]).unwrap();
        let damaged = store.put(&cx, &bytes).await.unwrap();
        std::fs::write(
            store.path(damaged.0),
            encode_block(0, None, &[edge(31, 7, 9, 6, None)]).unwrap(),
        )
        .unwrap();
        let suffix = [BlockRef {
            block_id: damaged.0,
            first_seq: CommitSeq(6),
            last_seq: CommitSeq(6),
        }];
        let refused = store
            .verify_root_append(&cx, append(basis, &root, &suffix, &[]), &mut receipts)
            .await;
        assert!(
            matches!(refused, Err(StoreError::RootBlockLoad { at: 1, error }) if matches!(*error, StoreError::IdentityMismatch { .. }))
        );
        assert_eq!(receipts.verified_root_prefix(0), (0, 0));
    });
}

#[test]
fn a_dropped_or_failed_append_never_restores_its_unpublished_memo() {
    let dir = scratch_dir("append-publication-failure");
    under_lab(0xa993, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE).await.unwrap();
        let root = base(&cx, &store).await;
        let mut receipts = PublishReceipts::new();
        let basis = store
            .put_root_verified(&cx, &root, &mut receipts)
            .await
            .unwrap();
        let added = store
            .put(
                &cx,
                &encode_block(0, None, &[edge(20, 3, 4, 6, None)]).unwrap(),
            )
            .await
            .unwrap();
        let suffix = [BlockRef {
            block_id: added.0,
            first_seq: CommitSeq(6),
            last_seq: CommitSeq(6),
        }];
        let request = append(basis, &root, &suffix, &[]);
        let verified = store
            .verify_root_append(&cx, request, &mut receipts)
            .await
            .unwrap()
            .unwrap();
        let candidate = verified.id();
        assert_eq!(receipts.verified_root_prefix(0), (0, 0));
        drop(verified);
        assert!(
            !store.path(candidate.0).exists(),
            "verification alone publishes nothing"
        );
        assert!(
            store
                .verify_root_append(&cx, request, &mut receipts)
                .await
                .unwrap()
                .is_none()
        );

        store
            .put_root_verified(&cx, &root, &mut receipts)
            .await
            .unwrap();
        let verified = store
            .verify_root_append(&cx, request, &mut receipts)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(verified.id(), candidate);
        std::fs::write(store.path(candidate.0), b"damaged existing root").unwrap();
        let refused = store
            .publish_root_and_manifest(&cx, verified, &records(&root, candidate), &mut receipts)
            .await;
        assert!(
            matches!(refused, Err(StoreError::DamagedExisting { expected, .. }) if expected == candidate.0)
        );
        assert_eq!(
            receipts.verified_root_prefix(0),
            (0, 0),
            "failed durable publication cannot reinstate the candidate"
        );
        assert!(
            store
                .verify_root_append(&cx, request, &mut receipts)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store.get_root(&cx, basis).await.unwrap(),
            root,
            "the previous root remains readable"
        );
    });
}

#[test]
fn a_reopened_legacy_root_binds_its_stored_identity_before_a_v5_append() {
    let dir = scratch_dir("append-legacy-root");
    under_lab(0xa994, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE).await.unwrap();
        let root = base(&cx, &store).await;
        let added = store
            .put(
                &cx,
                &encode_block(0, None, &[edge(20, 3, 4, 6, None)]).unwrap(),
            )
            .await
            .unwrap();
        let suffix = [BlockRef {
            block_id: added.0,
            first_seq: CommitSeq(6),
            last_seq: CommitSeq(6),
        }];
        let v3 = encode_root(&root).unwrap();
        let v4 = encode_root_v4(&root, &K_OID, NAMESPACE, &mut SegmentCache::default()).unwrap();
        assert!(v4.new_segments.is_empty());
        for bytes in [v3, v4.bytes] {
            let basis = plant_unadmitted_root(&store, &bytes);
            std::fs::File::open(store.path(basis.0))
                .unwrap()
                .sync_all()
                .unwrap();
            std::fs::File::open(dir.join(BLOCK_DIR))
                .unwrap()
                .sync_all()
                .unwrap();
            let admitted = store.reopen_sealed(&cx, basis).await.unwrap();
            let mut receipts = PublishReceipts::for_published_root(admitted.admission);
            let verified = store
                .verify_root_append(&cx, append(basis, &root, &suffix, &[]), &mut receipts)
                .await
                .unwrap()
                .expect("stored V3/V4 identity, not its V5 re-encoding");
            let mut next = root.clone();
            next.published_at = CommitSeq(8);
            next.blocks.extend(suffix);
            let cold =
                encode_root_v5(&next, &K_OID, NAMESPACE, &mut SegmentCache::default()).unwrap();
            let next_id = verified.id();
            assert_eq!(next_id.0, derive_root_id(&K_OID, NAMESPACE, &cold.bytes));
            store
                .publish_root_and_manifest(&cx, verified, &records(&next, next_id), &mut receipts)
                .await
                .unwrap();
            assert_eq!(
                store.get_root_bytes(&cx, next_id).await.unwrap(),
                cold.bytes
            );
            assert_eq!(store.reopen(&cx, next_id).await.unwrap().0, next);
        }
    });
}

#[test]
fn append_publishes_identical_v5_bytes_across_a_full_segment_and_reopen() {
    let dir = scratch_dir("append-segment");
    under_lab(0xa995, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE).await.unwrap();
        let grown = batch_of_blocks(&cx, &store, SEGMENT_REFS + 1).await;
        let mut root = grown.clone();
        root.blocks.truncate(SEGMENT_REFS - 1);
        let mut receipts = PublishReceipts::new();
        let mut basis = store
            .put_root_verified(&cx, &root, &mut receipts)
            .await
            .unwrap();
        for end in [SEGMENT_REFS, SEGMENT_REFS + 1] {
            let request = RootAppend {
                published_at: grown.published_at,
                ..append(basis, &root, &grown.blocks[root.blocks.len()..end], &[])
            };
            let verified = store
                .verify_root_append(&cx, request, &mut receipts)
                .await
                .unwrap()
                .expect("exact live/reopened prefix");
            root.blocks.extend_from_slice(request.blocks);
            basis = verified.id();
            let cold =
                encode_root_v5(&root, &K_OID, NAMESPACE, &mut SegmentCache::default()).unwrap();
            assert_eq!(basis.0, derive_root_id(&K_OID, NAMESPACE, &cold.bytes));
            store
                .publish_root_and_manifest(&cx, verified, &records(&root, basis), &mut receipts)
                .await
                .unwrap();
            assert_eq!(store.get_root_bytes(&cx, basis).await.unwrap(), cold.bytes);
            let reopened = store.reopen_sealed(&cx, basis).await.unwrap();
            assert_eq!(reopened.root, root);
            receipts = PublishReceipts::for_published_root(reopened.admission);
        }
        assert_eq!(root, grown);
    });
}
