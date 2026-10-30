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
    /// One source family's raw history remains charged until the ordinary
    /// compactor consumes it, then drops before the next family is loaded.
    /// Publication precedence stays unchanged inside each family. Other
    /// relations are not faulted again, even when the selection is empty.
    ///
    /// `max_source_incidences` bounds ALL selected RAW incidences before payload
    /// faults, independently of the compacted image's `limits.max_incidences`.
    /// Charged routing metadata is linear in selected block count; decoded
    /// input residency depends on the largest source history. Metadata sorting,
    /// decode and retained inputs share one cumulative source-work allowance.
    /// Compacted output populations and properties are admitted cumulatively
    /// while each family is encoded, before another family is read. Completed
    /// families retain final columns and provisional compressed destination IDs,
    /// not decoded topology. The global dictionary is merged from those columns
    /// and translated one row at a time without another source payload read.
    /// An output refusal may stop before later families are read; no partial
    /// image is exposed.
    ///
    /// The existing in-core compactor and image encoder retain their separate
    /// population/image bounds; this is NOT a spillable compactor or a
    /// whole-operation MemoryPool/RSS guarantee. Cancellation or any limit
    /// failure refunds all transient source reservations and leaves the reader
    /// usable. A single large source history may still exceed source admission.
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
        self.seal_relation_with_source_admission(
            cx,
            floor,
            relation,
            limits,
            max_source_incidences,
            |_| Ok(()),
            |error| error,
        )
        .await
    }

    /// Seal the same authenticated source with an additional owner-level
    /// admission for its complete selected RAW incidence population. The count
    /// comes from the existing descriptor admission, before compaction can
    /// discard superseded or pre-floor histories. No selected payload is faulted
    /// and no image builder is allocated until `admit` succeeds.
    ///
    /// The callback runs once, after the ordinary source count/work/routing
    /// bounds succeed. It may return the host's original typed refusal without
    /// wrapping or flattening it; `map_error` preserves storage causes in that
    /// same host error domain. Successful admission does not replace live
    /// cancellation or the source, compactor and image limits.
    #[allow(clippy::too_many_arguments)]
    pub async fn seal_relation_with_source_admission<E: Send>(
        &mut self,
        cx: &QueryCx,
        floor: CommitSeq,
        relation: RelationId,
        limits: SealedLimits,
        max_source_incidences: usize,
        admit: impl FnOnce(usize) -> Result<(), E> + Send,
        map_error: fn(SealedError) -> E,
    ) -> Result<SealedPartition, E> {
        cx.with_restriction_async(async {
            cx.checkpoint()
                .map_err(SealedError::Interrupted)
                .map_err(map_error)?;
            let root = self.root();
            let scope = SealedScope {
                source_root: self.root_id(),
                graph: root.graph,
                branch: root.branch,
                partition: root.partition,
                floor,
                publication: root.published_at,
            };
            let mut sources = self
                .sealed_relation_sources(cx, floor, relation, max_source_incidences)
                .map_err(buffered_error)
                .map_err(map_error)?;
            admit(sources.incidences)?;
            let mut builder = image::ImageBuilder::new(limits).map_err(map_error)?;
            let mut checkpoint = || cx.checkpoint().map_err(SealedError::Interrupted);
            while let Some(source) = self
                .next_sealed_relation_source(cx, &mut sources)
                .await
                .map_err(buffered_error)
                .map_err(map_error)?
            {
                checkpoint().map_err(map_error)?;
                let family =
                    crate::compact::compact_with_props(&source.blocks, &source.properties, floor)
                        .map_err(SealedError::History)
                        .map_err(map_error)?;
                drop(source);
                checkpoint().map_err(map_error)?;
                // Complete root admission fixed each EId's source/relation.
                // Sources arrive in canonical order; publication precedence
                // inside a source was resolved by the ordinary compactor.
                // The builder consumes and drops this family's decoded output.
                builder.push(family, &mut checkpoint).map_err(map_error)?;
            }
            drop(sources);
            checkpoint().map_err(map_error)?;
            let image = builder.finish(&mut checkpoint).map_err(map_error)?;
            SealedPartition::finish_scoped(scope, Some(relation), image, limits, &mut checkpoint)
                .map_err(map_error)
        })
        .await
    }
}
