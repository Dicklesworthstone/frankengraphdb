//! Explicit, writer-owned materialization of an authenticated compressed run.
//! It changes physical publication only. Chronicle and graph semantics remain
//! authoritative, and a cancelled/ambiguous root-slot swap fences the handle.

use crate::*;
use fgdb_strata::store::RootReadLimits;
use fgdb_strata::tiered::sealed::{
    AdjRunError, AdjRunSegment, AdjRunSegmentVersion, SealedError, SealedLimits,
};
use fgdb_types::QueryCx;

/// A seal never publishes a partial origin proof, source admission or run.
#[derive(Debug)]
pub enum SealPublicationError {
    Publication(RebuildError),
    Origin(Box<crate::OriginBirthError>),
    Source(SealedError),
    Run(AdjRunError),
}

impl core::fmt::Display for SealPublicationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Publication(error) => error.fmt(f),
            Self::Origin(error) => error.fmt(f),
            Self::Source(error) => error.fmt(f),
            Self::Run(error) => error.fmt(f),
        }
    }
}

impl core::error::Error for SealPublicationError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Publication(error) => Some(error),
            Self::Origin(error) => Some(error.as_ref()),
            Self::Source(error) => Some(error),
            Self::Run(error) => Some(error),
        }
    }
}

impl From<StoreError> for SealPublicationError {
    fn from(error: StoreError) -> Self {
        Self::Publication(RebuildError::Store(error))
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Persist one relation as a registered compressed adjacency run and
    /// publish its authenticated root membership for subsequent process opens.
    /// Existing attachments for other relations at this exact generation are
    /// retained. Repeating a seal is content-addressed and idempotent in graph
    /// semantics; it does not create a Chronicle commit or advance the frontier.
    ///
    /// This profile retains the complete Tier-D source. Queries in
    /// `[floor, verified_through]` reopen the run directly; earlier cuts and
    /// cuts after an unsealed edge mutation on `relation` use the retained
    /// Tier-D history, while commits that do not mutate `relation`'s edges
    /// carry its attachment forward across subsequent roots.
    ///
    /// Source admission, compressed output and retained origin cardinality
    /// have explicit limits. Origins are reconstructed from original committed
    /// creation effects, including when later property versions have replaced
    /// the current edge row. This operation holds this handle's existing writer
    /// lease throughout and never grants retention/deletion authority.
    #[allow(clippy::too_many_arguments)]
    pub async fn seal_relation(
        &mut self,
        cx: &CommitCx,
        query_cx: &QueryCx,
        relation: RelationId,
        floor: CommitSeq,
        sealing: SealedLimits,
        source_limits: RootReadLimits,
    ) -> Result<AdjRunSegmentVersion, SealPublicationError> {
        if !matches!(self.state, DatabaseState::Healthy { .. }) {
            return Err(SealPublicationError::Publication(
                RebuildError::HandleNotHealthy(self.state),
            ));
        }
        let next_generation = next_slot_generation(self.slot_generation)
            .map_err(RebuildError::from)
            .map_err(SealPublicationError::Publication)?;
        cx.checkpoint()
            .map_err(RebuildError::Interrupted)
            .map_err(SealPublicationError::Publication)?;
        let source_root = self
            .store
            .get_root_run_source(cx, self.snapshot.root, source_limits.max_root_bytes)
            .await?;
        let seal: SendFuture<'_, _> = Box::pin(self.store.seal_relation_with_source_limits(
            query_cx,
            source_root,
            floor,
            relation,
            sealing,
            source_limits,
        ));
        let partition = seal.await.map_err(SealPublicationError::Source)?;
        let edge_ids = partition
            .stable_edge_ids(query_cx, sealing.max_incidences)
            .map_err(SealPublicationError::Source)?;
        let frontier = self.snapshot.frontier;
        let origins = crate::origin_birth::reconstruct(
            cx,
            &self.coordinator,
            &self.keys,
            crate::origin_birth::EdgeOriginRequest {
                graph: GRAPH,
                branch: BRANCH,
                through: frontier,
                edge_ids: &edge_ids,
                max_origins: sealing.max_incidences,
            },
            &mut self.crypto_verification_events,
        )
        .await
        .map_err(|error| SealPublicationError::Origin(Box::new(error)))?;
        let run = AdjRunSegment::from_sealed(query_cx, &partition, &origins, sealing)
            .map_err(SealPublicationError::Run)?;
        let published_chain_hash = chain_commitment_at(self.coordinator.chain(), frontier)
            .expect("a healthy seal's frontier is on its recovered chain");
        let (root, run_id, manifest, manifest_len) = self
            .store
            .publish_adj_run(
                cx,
                query_cx,
                self.snapshot.root,
                &run,
                sealing,
                published_chain_hash,
            )
            .await?;
        // Fully stage the replacement before the externally visible swap. The
        // immutable graph payloads and their indexes are shared with old views.
        let previous_root = self.snapshot.root;
        let mut snapshot = (*self.snapshot).clone();
        snapshot.root = root;
        snapshot.manifest = manifest;
        if self
            .attached_runs
            .values()
            .all(|entry| entry.resolved_source_root(source_root.0) == source_root.0)
        {
            snapshot.sealed_source_root = Some(source_root);
        }
        let mut attached_runs = self.attached_runs.clone();
        attached_runs.insert(
            relation,
            fgdb_strata::root::runs::AdjRunRef::carried(
                relation,
                run_id.0,
                source_root.0,
                floor,
                frontier,
                frontier,
            ),
        );
        query_cx
            .checkpoint()
            .map_err(SealedError::Interrupted)
            .map_err(SealPublicationError::Source)?;
        cx.checkpoint()
            .map_err(RebuildError::Interrupted)
            .map_err(SealPublicationError::Publication)?;
        self.retain_preparation_anchor();
        self.state = DatabaseState::NeedsAuthoritativeRecovery(RecoveryRequired {
            durable_frontier: frontier,
            published_frontier: frontier,
            failed_stage: DerivedPublicationStage::PublishRootSlot,
        });
        self.slot_store
            .publish_evidenced(
                cx,
                &spine_slot(&self.keys, next_generation, manifest, manifest_len as u64),
            )
            .await
            .map_err(RebuildError::Slot)
            .map_err(SealPublicationError::Publication)?;
        // No suspension may separate successful publication from this swap.
        self.slot_generation = next_generation;
        self.receipts.rebind_root_binding(previous_root, root);
        self.receipts.rebind_root_binding(source_root, root);
        self.snapshot = Arc::new(snapshot);
        self.attached_runs = attached_runs;
        self.state = DatabaseState::Healthy {
            published_frontier: frontier,
        };
        Ok(run_id)
    }
}
