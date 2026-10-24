//! A sealed image from an opaque, already-authenticated buffered generation.
//! The caller supplies no root claim or decoded histories to this constructor.

use super::*;
use crate::store::{BufferedPartition, BufferedReadError};
use crate::tiered::buffer::BufferError;

fn buffered_error(error: BufferedReadError) -> SealedError {
    match error {
        BufferedReadError::Interrupted(error)
        | BufferedReadError::Buffer(BufferError::Cancelled(error)) => {
            SealedError::Interrupted(error)
        }
        BufferedReadError::BeyondPublication { .. } => SealedError::InvalidFloor,
        BufferedReadError::SizeOverflow => SealedError::SizeOverflow,
        BufferedReadError::Limit {
            resource,
            requested,
            limit,
        } => SealedError::Limit {
            resource,
            requested,
            limit,
        },
        other => SealedError::BufferedSource(Box::new(other)),
    }
}

impl<V: Vfs> BufferedPartition<V> {
    /// Seal one relation from this already-admitted immutable generation.
    /// Initial buffered admission proved the complete root and its histories;
    /// subsequent faults still authenticate every selected topology/property
    /// extent. No root reopen, second vertex scan, or writer lease is needed.
    /// Source blocks keep their publication order and their MemoryPool charges
    /// until the ordinary compactor has consumed them. Other relations are not
    /// faulted again, even when the selected relation is empty.
    ///
    /// `max_source_incidences` bounds selected RAW incidences before payload
    /// faults, independently of the compacted image's `limits.max_incidences`.
    /// Input metadata, decode and retained blocks use the buffered pool and one
    /// cumulative source-work allowance. The existing in-core compactor and
    /// image encoder have their separate population/image bounds; this is NOT
    /// a spillable compactor or a whole-operation MemoryPool/RSS guarantee.
    /// Cancellation or any limit failure yields no image and refunds all
    /// transient source reservations. The original reader remains usable.
    ///
    /// The result is the same derived image and opaque root/relation receipt as
    /// BlockStore::seal_relation, not persisted Tier R or a new durable authority.
    pub async fn seal_relation(
        &mut self,
        cx: &QueryCx,
        floor: CommitSeq,
        relation: RelationId,
        limits: SealedLimits,
        max_source_incidences: usize,
    ) -> Result<SealedPartition, SealedError> {
        cx.with_restriction_async(async {
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            let root = self.root();
            let scope = SealedScope {
                source_root: self.root_id(),
                graph: root.graph,
                branch: root.branch,
                partition: root.partition,
                floor,
                publication: root.published_at,
            };
            let source = self
                .sealed_relation_source(cx, floor, relation, max_source_incidences)
                .await
                .map_err(buffered_error)?;
            cx.checkpoint().map_err(SealedError::Interrupted)?;
            let compacted =
                crate::compact::compact_with_props(&source.blocks, &source.properties, floor)
                    .map_err(SealedError::History)?;
            drop(source);
            let mut checkpoint = || cx.checkpoint().map_err(SealedError::Interrupted);
            checkpoint()?;
            let image = image::build(compacted, limits, &mut checkpoint)?;
            SealedPartition::finish_scoped(scope, Some(relation), image, limits, &mut checkpoint)
        })
        .await
    }
}
