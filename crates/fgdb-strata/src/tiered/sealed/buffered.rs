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
    /// before appending another family. An output refusal may therefore stop
    /// before later families are read. No partial image is exposed.
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
            let mut sources = self
                .sealed_relation_sources(cx, floor, relation, max_source_incidences)
                .map_err(buffered_error)?;
            let mut compacted = crate::compact::Compaction {
                blocks: Vec::new(),
                block_props: Vec::new(),
                dropped: 0,
                superseded: 0,
            };
            let mut incidences = 0usize;
            let mut descriptors = 0usize;
            let mut property_bytes = 0usize;
            let mut checkpoint = || cx.checkpoint().map_err(SealedError::Interrupted);
            while let Some(source) = self
                .next_sealed_relation_source(cx, &mut sources)
                .await
                .map_err(buffered_error)?
            {
                checkpoint()?;
                let mut family =
                    crate::compact::compact_with_props(&source.blocks, &source.properties, floor)
                        .map_err(SealedError::History)?;
                drop(source);
                checkpoint()?;
                // Complete root admission fixed each EId's source/relation.
                // Only independent families move here. The image builder
                // restores canonical adjacency order before encoding.
                for block in &family.blocks {
                    checkpoint()?;
                    incidences = incidences
                        .checked_add(block.len())
                        .ok_or(SealedError::SizeOverflow)?;
                }
                check_limit("incidences", incidences, limits.max_incidences)?;
                u32::try_from(incidences).map_err(|_| SealedError::SizeOverflow)?;
                descriptors = descriptors
                    .checked_add(usize::from(!family.blocks.is_empty()))
                    .ok_or(SealedError::SizeOverflow)?;
                check_limit("descriptors", descriptors, limits.max_rows)?;
                for props in family.block_props.iter().flatten() {
                    for row in &props.rows {
                        checkpoint()?;
                        if row.is_empty() {
                            continue;
                        }
                        let bytes = usize::try_from(
                            crate::edge_props::admitted_row_bytes(row)
                                .map_err(SealedError::Property)?,
                        )
                        .map_err(|_| SealedError::SizeOverflow)?;
                        property_bytes = property_bytes
                            .checked_add(bytes)
                            .ok_or(SealedError::SizeOverflow)?;
                        check_limit("property bytes", property_bytes, limits.max_property_bytes)?;
                    }
                }
                compacted.dropped = compacted
                    .dropped
                    .checked_add(family.dropped)
                    .ok_or(SealedError::SizeOverflow)?;
                compacted.superseded = compacted
                    .superseded
                    .checked_add(family.superseded)
                    .ok_or(SealedError::SizeOverflow)?;
                compacted
                    .blocks
                    .try_reserve(family.blocks.len())
                    .map_err(|_| SealedError::AllocationFailed)?;
                compacted
                    .block_props
                    .try_reserve(family.block_props.len())
                    .map_err(|_| SealedError::AllocationFailed)?;
                compacted.blocks.append(&mut family.blocks);
                compacted.block_props.append(&mut family.block_props);
            }
            drop(sources);
            checkpoint()?;
            let image = image::build(compacted, limits, &mut checkpoint)?;
            SealedPartition::finish_scoped(scope, Some(relation), image, limits, &mut checkpoint)
        })
        .await
    }
}
