//! Exact vertex-history admission without retaining the property history.
//!
//! The ordinary validator still owns every publication-order lifecycle law.
//! Its rows omit only labels and properties, whose exact equality is checked
//! against the first authenticated statement instead. The birth locator is
//! never a digest-only substitute for those values: a restatement refaults
//! and decodes the original patch through the same keyed root-reference check.

use super::*;

// The initial tree nodes and their unused slots must fit even for one entry;
// amortized per-version accounting alone would miss that minimum allocation.
const INITIAL_TREE_BYTES: usize = 4096;

pub(super) struct VertexAdmission<'a, V: Vfs> {
    births: BTreeMap<(VId, CommitSeq), (usize, usize)>,
    lifecycle: crate::root::VertexHistoryValidator,
    store: &'a BlockStore<V>,
    root: &'a PartitionRoot,
    pool: MemoryPool,
    // Both maps drop before their reservation. The initial node allowance
    // plus 512 bytes per version covers their fixed-size entries, tree nodes
    // and spare slots; neither map owns a label vector or property payload.
    _metadata: MemoryCharge,
}

impl<'a, V: Vfs + Clone> VertexAdmission<'a, V> {
    pub(super) fn new(
        cx: &impl BufferedReadCx,
        store: &'a BlockStore<V>,
        root: &'a PartitionRoot,
        pool: MemoryPool,
    ) -> Result<Self, BufferedReadError> {
        let metadata = pool.reserve(
            cx,
            if root.vertex_patches.is_empty() {
                0
            } else {
                INITIAL_TREE_BYTES
            },
        )?;
        Ok(Self {
            births: BTreeMap::new(),
            lifecycle: crate::root::VertexHistoryValidator::default(),
            store,
            root,
            pool,
            _metadata: metadata,
        })
    }

    pub(super) async fn observe_patch(
        &mut self,
        cx: &impl BufferedReadCx,
        patch_at: usize,
        rows: &VertexPatchRows,
        observe: &mut impl FnMut(RootReadEvent) -> Result<(), BufferedReadError>,
    ) -> Result<(), BufferedReadError> {
        // Keep at most one refaulted birth patch alongside the caller's
        // admitted incoming patch. Adjacent restatements of the same patch
        // reuse this image; unrelated histories are never scanned.
        let mut birth_patch: Option<(usize, BufferedValue<VertexPatchRows>)> = None;
        for (row_at, row) in rows.iter().enumerate() {
            cx.buffered_checkpoint()
                .map_err(BufferedReadError::Interrupted)?;
            let key = (row.vid, row.created_at);
            let birth = self.births.get(&key).copied();
            if let Some((birth_at, birth_row)) = birth {
                if birth_patch
                    .as_ref()
                    .is_none_or(|(cached_at, _)| *cached_at != birth_at)
                {
                    // Drop the previous image before reserving its
                    // replacement: no third decoded object can accumulate.
                    drop(birth_patch.take());
                    let charge = self.pool.reserve(cx, OBJECT_WORKSPACE_BYTES)?;
                    let (image, bytes) = self
                        .store
                        .resolve_root_patch_observed(
                            cx,
                            birth_at,
                            &self.root.vertex_patches[birth_at],
                            observe,
                        )
                        .await?;
                    drop(bytes);
                    birth_patch = Some((birth_at, BufferedValue::from_reserved(image, charge)));
                }
                let original = birth_patch
                    .as_ref()
                    .and_then(|(_, image)| image.get(birth_row))
                    .filter(|original| (original.vid, original.created_at) == key)
                    .ok_or(BufferedReadError::Buffer(BufferError::InvalidLoad))?;
                if original.labels != row.labels || original.props != row.props {
                    return Err(BufferedReadError::VertexHistoryConflict { vid: row.vid });
                }
            } else {
                // A restatement adds no map entry. Unique versions reserve
                // both maps before either BTreeMap can allocate a node.
                self._metadata.grow(cx, HISTORY_ENTRY_BYTES)?;
            }

            let statement = VertexRow {
                vid: row.vid,
                birth_ordinal: row.birth_ordinal,
                created_at: row.created_at,
                retired_at: row.retired_at,
                labels: Vec::new(),
                props: Vec::new(),
            };
            self.lifecycle
                .observe_patch(patch_at, core::slice::from_ref(&statement))
                .map_err(|error| match error {
                    crate::root::RootError::VertexIdentityMismatch { vid, conflict } => {
                        drop(conflict);
                        BufferedReadError::VertexHistoryConflict { vid }
                    }
                    error => StoreError::MalformedRoot(error).into(),
                })?;
            if birth.is_none() {
                self.births.insert(key, (patch_at, row_at));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::{
        BranchId, CanonicalScalar, DatabaseSecurityNamespaceId, GraphId, PurposeContexts,
    };

    #[test]
    fn birth_refault_admission_interruptions_and_corruption_refund_both_images() {
        let dir = std::env::temp_dir().join(format!("fgdb-birth-refault-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_, report) = run_async_under_lab(0xb0_11, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let store = BlockStore::open(
                &commit,
                &dir,
                [0x23; 32],
                DatabaseSecurityNamespaceId([0x24; 32]),
            )
            .await
            .unwrap();
            let first = VertexRow {
                vid: VId(1),
                birth_ordinal: 7,
                created_at: CommitSeq(1),
                retired_at: None,
                labels: vec![LabelId(9)],
                props: vec![(
                    PropertyKeyId(11),
                    CanonicalScalar::bytes(vec![0x45; 14_000]).unwrap(),
                )],
            };
            let first_bytes = crate::vertex::encode_patch(core::slice::from_ref(&first)).unwrap();
            let first_id = store.put_patch(&commit, &first_bytes).await.unwrap();
            let later = VertexRow {
                retired_at: Some(CommitSeq(2)),
                ..first
            };
            let later_id = store
                .put_patch(&commit, &crate::vertex::encode_patch(&[later]).unwrap())
                .await
                .unwrap();
            let root = PartitionRoot {
                graph: GraphId(1),
                branch: BranchId(1),
                partition: 0,
                published_at: CommitSeq(2),
                blocks: vec![],
                vertex_patches: vec![
                    crate::root::PatchRef {
                        patch_id: first_id.0,
                        first_seq: CommitSeq(1),
                        last_seq: CommitSeq(1),
                    },
                    crate::root::PatchRef {
                        patch_id: later_id.0,
                        first_seq: CommitSeq(1),
                        last_seq: CommitSeq(2),
                    },
                ],
            };

            let empty_root = PartitionRoot {
                vertex_patches: Vec::new(),
                ..root.clone()
            };
            let empty_pool = MemoryPool::new(0, 0).unwrap();
            let empty =
                VertexAdmission::new(&query, &store, &empty_root, empty_pool.clone()).unwrap();
            assert_eq!(empty_pool.used(), 0);
            drop(empty);

            for fail_at in 0..3 {
                let pool = MemoryPool::new(3 * 1024 * 1024, 0).unwrap();
                let mut history =
                    VertexAdmission::new(&query, &store, &root, pool.clone()).unwrap();
                {
                    let _incoming = pool.reserve(&query, OBJECT_WORKSPACE_BYTES).unwrap();
                    let rows = store.get_patch(&query, first_id).await.unwrap();
                    history
                        .observe_patch(&query, 0, &rows, &mut |_| Ok(()))
                        .await
                        .unwrap();
                }
                assert_eq!(pool.used(), INITIAL_TREE_BYTES + HISTORY_ENTRY_BYTES);
                {
                    let _incoming = pool.reserve(&query, OBJECT_WORKSPACE_BYTES).unwrap();
                    let rows = store.get_patch(&query, later_id).await.unwrap();
                    let mut events = 0;
                    let mut stop = |_| {
                        let at = events;
                        events += 1;
                        if at == fail_at {
                            Err(BufferedReadError::Interrupted(Box::new(
                                asupersync::error::Error::cancelled(
                                    &asupersync::types::CancelReason::user(
                                        "birth-refault interruption",
                                    ),
                                ),
                            )))
                        } else {
                            Ok(())
                        }
                    };
                    assert!(matches!(
                        history.observe_patch(&query, 1, &rows, &mut stop).await,
                        Err(BufferedReadError::Interrupted(_))
                    ));
                    assert_eq!(events, fail_at + 1);
                    assert_eq!(
                        pool.used(),
                        OBJECT_WORKSPACE_BYTES + INITIAL_TREE_BYTES + HISTORY_ENTRY_BYTES
                    );
                }
                drop(history);
                assert_eq!(pool.used(), 0);
            }

            // An incoming image may fit while the exact-comparison image does
            // not. Refuse before issuing its first object-read event.
            let pool = MemoryPool::new(
                2 * OBJECT_WORKSPACE_BYTES + INITIAL_TREE_BYTES + HISTORY_ENTRY_BYTES - 1,
                0,
            )
            .unwrap();
            let mut history = VertexAdmission::new(&query, &store, &root, pool.clone()).unwrap();
            {
                let _incoming = pool.reserve(&query, OBJECT_WORKSPACE_BYTES).unwrap();
                let rows = store.get_patch(&query, first_id).await.unwrap();
                history
                    .observe_patch(&query, 0, &rows, &mut |_| Ok(()))
                    .await
                    .unwrap();
            }
            {
                let _incoming = pool.reserve(&query, OBJECT_WORKSPACE_BYTES).unwrap();
                let rows = store.get_patch(&query, later_id).await.unwrap();
                let mut events = 0;
                assert!(matches!(
                    history
                        .observe_patch(&query, 1, &rows, &mut |_| {
                            events += 1;
                            Ok(())
                        })
                        .await,
                    Err(BufferedReadError::Memory(
                        MemoryError::ResourceExhausted { .. }
                    ))
                ));
                assert_eq!(events, 0);
            }
            drop(history);
            assert_eq!(pool.used(), 0);

            // Admission never trusts a birth locator after its object changes,
            // even though the first visit already authenticated those bytes.
            let pool = MemoryPool::new(3 * 1024 * 1024, 0).unwrap();
            let mut history = VertexAdmission::new(&query, &store, &root, pool.clone()).unwrap();
            {
                let _incoming = pool.reserve(&query, OBJECT_WORKSPACE_BYTES).unwrap();
                let rows = store.get_patch(&query, first_id).await.unwrap();
                history
                    .observe_patch(&query, 0, &rows, &mut |_| Ok(()))
                    .await
                    .unwrap();
            }
            let mut corrupt = first_bytes;
            corrupt[100] ^= 1;
            std::fs::write(store.path(first_id.0), corrupt).unwrap();
            {
                let _incoming = pool.reserve(&query, OBJECT_WORKSPACE_BYTES).unwrap();
                let rows = store.get_patch(&query, later_id).await.unwrap();
                assert!(matches!(
                    history
                        .observe_patch(&query, 1, &rows, &mut |_| Ok(()))
                        .await,
                    Err(BufferedReadError::Store(_))
                ));
                assert_eq!(
                    pool.used(),
                    OBJECT_WORKSPACE_BYTES + INITIAL_TREE_BYTES + HISTORY_ENTRY_BYTES
                );
            }
            drop(history);
            assert_eq!(pool.used(), 0);
        });
        assert!(report.invariant_violations.is_empty(), "{report:?}");
    }
}
