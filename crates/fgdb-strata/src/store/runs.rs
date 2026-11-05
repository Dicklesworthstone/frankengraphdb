//! Durable compressed runs are members of the selected immutable root.
//! Metadata hints avoid an extra legacy-root read, but never supply a run or
//! its anchor: every selected FGRN object earns identity and shape admission.

use super::{BlockStore, PublishReceipts, StoreError, StoredObjectKind};
use crate::PartitionRootVersion;
use crate::root::{
    RootFrame,
    runs::{AdjRunRef, decode_root_runs, encode_root_runs},
};
use crate::tiered::sealed::{
    AdjRunError, AdjRunSegment, AdjRunSegmentVersion, MAX_ADJ_RUN_BYTES, SealedLimits, SealedScope,
    adj_run_id, read_adj_run,
};
use asupersync::fs::Vfs;
use fgdb_crypto::Digest;
use fgdb_delta_types::RelationId;
use fgdb_types::{BranchId, CommitCx, CommitSeq, GraphId, QueryCx, StorageReadCx};

fn malformed(error: AdjRunError) -> StoreError {
    StoreError::MalformedAdjRun(Box::new(error))
}

fn coordinate(bytes: &[u8]) -> Result<(GraphId, BranchId, u64, CommitSeq), StoreError> {
    let tuple = match crate::root::decode_root_frame(bytes).map_err(StoreError::MalformedRoot)? {
        RootFrame::V3(root) => (root.graph, root.branch, root.partition, root.published_at),
        RootFrame::V4(frame) => (
            frame.graph,
            frame.branch,
            frame.partition,
            frame.published_at,
        ),
        RootFrame::V5(frame) => (
            frame.graph,
            frame.branch,
            frame.partition,
            frame.published_at,
        ),
        RootFrame::V6(frame) => {
            let frame = frame.membership;
            (
                frame.graph,
                frame.branch,
                frame.partition,
                frame.published_at,
            )
        }
    };
    Ok(tuple)
}

impl<V: Vfs> BlockStore<V> {
    pub(super) fn note_root_run_presence(&self, root: PartitionRootVersion, present: bool) {
        if let Ok(mut hint) = self.root_run_presence.lock() {
            *hint = Some((root, present));
        }
    }

    /// Retain the selected root's exact run inventory before a later integrity
    /// sweep. These references carry no permission to decode an image or retire
    /// a generation; a selected run still needs full scope admission on use.
    pub async fn published_adj_run_refs(
        &self,
        cx: &impl StorageReadCx,
        root: PartitionRootVersion,
        max_root_bytes: usize,
    ) -> Result<Vec<AdjRunRef>, StoreError> {
        if self
            .root_run_presence
            .lock()
            .ok()
            .is_some_and(|hint| *hint == Some((root, false)))
        {
            return Ok(Vec::new());
        }
        let bytes = self
            .read_metadata_bytes(
                cx,
                root.0,
                StoredObjectKind::Root,
                max_root_bytes.min(crate::root::MAX_ENCODED_ROOT_BYTES) as u64,
            )
            .await?;
        match decode_root_runs(&bytes).map_err(StoreError::MalformedRoot)? {
            Some(attached) => {
                self.note_root_run_presence(root, true);
                Ok(attached.runs)
            }
            None => {
                crate::root::decode_root_frame(&bytes).map_err(StoreError::MalformedRoot)?;
                self.note_root_run_presence(root, false);
                Ok(Vec::new())
            }
        }
    }

    /// Re-read an admitted run's complete physical bytes for integrity sweep,
    /// with its registered keyed identity and format allocation ceiling.
    pub async fn get_adj_run_bytes(
        &self,
        cx: &impl StorageReadCx,
        run: AdjRunSegmentVersion,
    ) -> Result<Vec<u8>, StoreError> {
        self.read_metadata_bytes(
            cx,
            run.0,
            StoredObjectKind::AdjRun,
            MAX_ADJ_RUN_BYTES as u64,
        )
        .await
    }

    /// The exact Tier-D source of a maintenance seal. Existing attachments
    /// are flattened to their embedded base; they never become a new lineage.
    /// The original base object remains retained in this additive profile.
    pub async fn get_root_run_source(
        &self,
        cx: &impl StorageReadCx,
        root: PartitionRootVersion,
        max_root_bytes: usize,
    ) -> Result<PartitionRootVersion, StoreError> {
        let bytes = self
            .read_metadata_bytes(
                cx,
                root.0,
                StoredObjectKind::Root,
                max_root_bytes.min(crate::root::MAX_ENCODED_ROOT_BYTES) as u64,
            )
            .await?;
        match decode_root_runs(&bytes).map_err(StoreError::MalformedRoot)? {
            Some(attached) => {
                self.note_root_run_presence(root, true);
                Ok(PartitionRootVersion(crate::root::root_id(
                    self.k_oid.expose(),
                    self.namespace,
                    attached.base_frame,
                )))
            }
            None => {
                crate::root::decode_root_frame(&bytes).map_err(StoreError::MalformedRoot)?;
                self.note_root_run_presence(root, false);
                Ok(root)
            }
        }
    }

    /// Open a selected root's registered run without reconstructing compressed
    /// adjacency from Tier D. Missing membership, legacy roots and cuts below
    /// the run's retained floor return `None`; an attached but damaged object
    /// refuses. A failed run read must never silently trigger resealing.
    #[allow(clippy::too_many_arguments)]
    pub async fn get_published_adj_run(
        &self,
        cx: &QueryCx,
        root: PartitionRootVersion,
        relation: RelationId,
        as_of: CommitSeq,
        limits: SealedLimits,
        max_root_bytes: usize,
    ) -> Result<Option<AdjRunSegment>, StoreError> {
        cx.checkpoint().map_err(|error| {
            malformed(crate::tiered::sealed::SealedError::Interrupted(error).into())
        })?;
        // One immutable-ID-tagged hint, populated only after frame admission.
        // A miss (including poisoned locking) simply takes the ordinary read.
        let known_legacy = self
            .root_run_presence
            .lock()
            .ok()
            .is_some_and(|hint| *hint == Some((root, false)));
        if known_legacy {
            return Ok(None);
        }
        let bytes = self
            .read_metadata_bytes(
                cx,
                root.0,
                StoredObjectKind::Root,
                max_root_bytes.min(crate::root::MAX_ENCODED_ROOT_BYTES) as u64,
            )
            .await?;
        let Some(attached) = decode_root_runs(&bytes).map_err(StoreError::MalformedRoot)? else {
            crate::root::decode_root_frame(&bytes).map_err(StoreError::MalformedRoot)?;
            self.note_root_run_presence(root, false);
            return Ok(None);
        };
        self.note_root_run_presence(root, true);
        let Some(reference) = attached.runs.iter().find(|run| run.relation == relation) else {
            return Ok(None);
        };
        if as_of < reference.floor {
            return Ok(None);
        }
        let (graph, branch, partition, publication) = coordinate(attached.base_frame)?;
        if as_of > publication {
            return Err(malformed(AdjRunError::ScopeMismatch));
        }
        let scope = SealedScope {
            source_root: PartitionRootVersion(crate::root::root_id(
                self.k_oid.expose(),
                self.namespace,
                attached.base_frame,
            )),
            graph,
            branch,
            partition,
            floor: reference.floor,
            publication,
        };
        let id = reference.run;
        self.read_object_proven(
            cx,
            id,
            limits.max_image_bytes.min(MAX_ADJ_RUN_BYTES) as u64,
            |bytes| async move {
                read_adj_run(
                    cx,
                    id,
                    scope,
                    relation,
                    self.k_oid.expose(),
                    self.namespace,
                    &bytes,
                    limits,
                    self.decode_resolver(),
                )
                .map_err(malformed)
            },
        )
        .await
        .map(Some)
    }

    /// Publish a run and a replacement root/manifest at the same semantic
    /// frontier. The run is derived from an admitted source anchor; its exact
    /// base bytes and stable scope must agree with the selected predecessor.
    /// All three immutable objects use staged durable readback and the shared
    /// directory barrier before any caller can advance a root slot.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_adj_run(
        &self,
        cx: &CommitCx,
        query_cx: &QueryCx,
        selected: PartitionRootVersion,
        run: &AdjRunSegment,
        limits: SealedLimits,
        published_chain_hash: Digest,
    ) -> Result<
        (
            PartitionRootVersion,
            AdjRunSegmentVersion,
            crate::manifest::ManifestVersion,
            usize,
        ),
        StoreError,
    > {
        let selected_bytes = self.get_root_bytes(cx, selected).await?;
        let attached = decode_root_runs(&selected_bytes).map_err(StoreError::MalformedRoot)?;
        let base = attached
            .as_ref()
            .map_or(selected_bytes.as_slice(), |root| root.base_frame);
        let source_root = PartitionRootVersion(crate::root::root_id(
            self.k_oid.expose(),
            self.namespace,
            base,
        ));
        let (graph, branch, partition, publication) = coordinate(base)?;
        let header = run.header();
        if header.scope.source_root != source_root
            || (
                header.scope.graph,
                header.scope.branch,
                header.scope.partition,
                header.scope.publication,
            ) != (graph, branch, partition, publication)
            || header.scope.floor > publication
        {
            return Err(malformed(AdjRunError::ScopeMismatch));
        }
        let run_bytes = run.encode(query_cx, limits).map_err(malformed)?;
        let run_id = adj_run_id(self.k_oid.expose(), self.namespace, &run_bytes);
        let mut refs = attached.map_or_else(Vec::new, |root| root.runs);
        let reference = AdjRunRef {
            relation: header.relation,
            run: run_id,
            floor: header.scope.floor,
        };
        match refs.binary_search_by_key(&header.relation, |reference| reference.relation) {
            Ok(at) => refs[at] = reference,
            Err(at) => {
                if refs.len() >= crate::root::runs::MAX_ROOT_RUNS {
                    return Err(StoreError::MalformedRoot(
                        crate::root::RootError::InvalidRunRoot,
                    ));
                }
                refs.insert(at, reference);
            }
        }
        let root_bytes = encode_root_runs(base, &refs).map_err(StoreError::MalformedRoot)?;
        let root = PartitionRootVersion(crate::root::root_id(
            self.k_oid.expose(),
            self.namespace,
            &root_bytes,
        ));
        let manifest_bytes = crate::manifest::encode_manifest(&[crate::manifest::ManifestRecord {
            graph,
            branch,
            partition,
            root,
            published_chain_hash,
        }])
        .map_err(StoreError::MalformedManifest)?;
        let manifest = crate::manifest::ManifestVersion(crate::manifest::manifest_id(
            self.k_oid.expose(),
            self.namespace,
            &manifest_bytes,
        ));
        let mut receipts = PublishReceipts::default();
        let mut batch = self.publication_batch(cx, &mut receipts, None)?;
        batch
            .put_object(cx, StoredObjectKind::AdjRun, run_id, &run_bytes)
            .await?;
        batch.flush(cx).await?;
        batch
            .put_object(cx, StoredObjectKind::Root, root.0, &root_bytes)
            .await?;
        batch
            .put_object(cx, StoredObjectKind::Manifest, manifest.0, &manifest_bytes)
            .await?;
        batch.finish(cx).await?;
        self.note_root_run_presence(root, true);
        Ok((
            root,
            AdjRunSegmentVersion(run_id),
            manifest,
            manifest_bytes.len(),
        ))
    }
}
