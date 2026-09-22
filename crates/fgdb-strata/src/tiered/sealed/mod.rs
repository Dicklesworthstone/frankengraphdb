//! Immutable, reloadable inline/CSR adjacency images anchored to admitted roots.
//!
//! `BlockStore::seal_partition` uses the REAL authenticated Tier-D reopen and
//! compaction paths. Small descriptors use Tier I; larger descriptors use EF
//! neighbors, EF CSR offsets, identity-coded EIDs, visibility spans and aligned
//! property sidecars. Parallel edges and retained versions remain incidences.
//!
//! The source-root receipt is separate from the resident image: an anchor does
//! not pin its adjacency bytes. Reload authenticates the complete image against
//! that receipt before interpreting any length. Scalar visibility is valid only
//! within that exact source lineage and the admitted [floor, publication] cut.
//!
//! This is a derived adjacency image, not a newly registered AdjRunSegment kind
//! or a second durable graph authority. The existing manifest still roots Tier D.
//! Restart must recover source authority there; no public API deserializes a
//! self-asserted anchor. Publishing registered run refs, marker-aware cross-branch
//! attachment, origin-order metadata, and automatic maintenance are separate
//! integration work. In particular, content-version starts are NOT relabeled as
//! immutable OriginBirthOrder values. Forking an image handle shares its bytes,
//! but does not authorize the image under an unrelated branch.

mod image;
mod incoming;
pub use incoming::{
    IncomingIndexLimits, IncomingIndexStats, SealedIncomingCursor, SealedIncomingIndex,
};
#[cfg(test)]
mod tests;
mod wire;

use super::inline::InlineError;
use crate::edge_props::EdgePropertyPatchError;
use crate::store::{BlockStore, StoreError};
use crate::{AdjacencyEntry, BlockError, PartitionRootVersion};
use asupersync::fs::Vfs;
use fgdb_codec::ef_payload::EfPayloadError;
use fgdb_codec::elias_fano::EliasFanoError;
use fgdb_codec::identity::IdentityColumnError;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{
    BranchId, CanonicalScalar, CanonicalScalarResolver, CommitSeq, GraphId, QueryCx, VId,
};
use image::{Image, Row};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedLimits {
    pub max_rows: usize,
    pub max_incidences: usize,
    pub max_image_bytes: usize,
    /// Canonical property-row bytes, not an allocator/RSS measure.
    pub max_property_bytes: usize,
}

impl Default for SealedLimits {
    fn default() -> Self {
        Self {
            max_rows: 1_000_000,
            max_incidences: 1_000_000,
            max_image_bytes: 256 * 1024 * 1024,
            max_property_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Debug)]
pub enum SealedError {
    Store(Box<StoreError>),
    History(crate::root::RootError),
    Entry(BlockError),
    Inline(InlineError),
    Identity(IdentityColumnError),
    EliasFano(EliasFanoError),
    Payload(EfPayloadError),
    Property(EdgePropertyPatchError),
    Interrupted(Box<asupersync::error::Error>),
    Limit {
        resource: &'static str,
        requested: usize,
        limit: usize,
    },
    SizeOverflow,
    AllocationFailed,
    InvalidFormat,
    NonCanonical,
    Truncated,
    TrailingBytes,
    InvalidFloor,
    SnapshotOutsideAnchor {
        requested: CommitSeq,
        floor: CommitSeq,
        publication: CommitSeq,
    },
    ImageMismatch,
}

impl core::fmt::Display for SealedError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Limit {
                resource,
                requested,
                limit,
            } => write!(
                f,
                "ResourceExhausted: sealed {resource} needs {requested}, limit {limit}"
            ),
            Self::Interrupted(_) => write!(f, "sealed adjacency operation interrupted"),
            other => write!(f, "sealed adjacency: {other:?}"),
        }
    }
}

impl std::error::Error for SealedError {}

/// Coordinates proven by the source store, not parsed from image bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedScope {
    pub source_root: PartitionRootVersion,
    pub graph: GraphId,
    pub branch: BranchId,
    pub partition: u64,
    pub floor: CommitSeq,
    pub publication: CommitSeq,
}

/// A small, opaque proof of one derived image's source and exact bytes.
/// Keeping this does NOT keep Image, EF arrays, or decoded properties resident.
/// It is not a GC lease; the owner retains whatever durable source lease its
/// manifest/read-view protocol requires. No public constructor accepts a hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedAnchor {
    scope: SealedScope,
    fingerprint: [u8; 32],
    encoded_bytes: usize,
}

impl SealedAnchor {
    pub const fn scope(self) -> SealedScope {
        self.scope
    }

    pub const fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }

    pub(super) fn authorize(self, as_of: CommitSeq) -> Result<(), SealedError> {
        if as_of < self.scope.floor || as_of > self.scope.publication {
            return Err(SealedError::SnapshotOutsideAnchor {
                requested: as_of,
                floor: self.scope.floor,
                publication: self.scope.publication,
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowStorageKind {
    Inline,
    SealedCsr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SealedStats {
    pub rows: usize,
    pub inline_rows: usize,
    pub sealed_rows: usize,
    pub incidences: usize,
    pub property_rows: usize,
    pub encoded_bytes: usize,
}

/// An immutable adjacency generation. Cloning is O(1); it neither copies EF
/// payloads nor repeats source admission. This is an adjacency view, not a full
/// vertex catalog or proof of vertex endpoint visibility for a graph query.
#[derive(Clone)]
pub struct SealedPartition {
    anchor: SealedAnchor,
    image: Arc<Image>,
}

impl core::fmt::Debug for SealedPartition {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SealedPartition")
            .field("scope", &self.anchor.scope)
            .field("stats", &self.stats())
            .finish()
    }
}

fn fingerprint(bytes: &[u8]) -> [u8; 32] {
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(b"fgdb.strata.sealed-adjacency-image.v1");
    hash.update(bytes);
    hash.finalize().0
}

fn check_limit(resource: &'static str, requested: usize, limit: usize) -> Result<(), SealedError> {
    if requested > limit {
        Err(SealedError::Limit {
            resource,
            requested,
            limit,
        })
    } else {
        Ok(())
    }
}

impl<V: Vfs> BlockStore<V> {
    /// Authenticate a real stored partition, apply its established history and
    /// property precedence, and seal an immutable adjacency image. The original
    /// root/blocks remain untouched. Source traversal uses `RootReadLimits`'s
    /// default source-byte, reference and vertex-version ceilings; raw edge
    /// statements additionally obey `limits.max_incidences` BEFORE collection.
    /// Use `seal_partition_with_source_limits` to supply another source profile.
    /// Reopen/compaction retain admitted adjacency in RAM, not external storage.
    pub async fn seal_partition(
        &self,
        cx: &QueryCx,
        root_id: PartitionRootVersion,
        floor: CommitSeq,
        limits: SealedLimits,
    ) -> Result<SealedPartition, SealedError> {
        let source = crate::store::RootReadLimits {
            max_incidences: limits.max_incidences,
            ..crate::store::RootReadLimits::default()
        };
        self.seal_partition_with_source_limits(cx, root_id, floor, limits, source)
            .await
    }

    /// Bound the authenticated input separately from the sealed output. Root
    /// bytes are capped before decoding; a future floor is rejected before any
    /// payload I/O. All referenced blocks, properties and vertex histories are
    /// still verified. No second vertex-patch collection is kept, although the
    /// existing validator retains per-version payloads during admission.
    ///
    /// A refusal may have read/decoded one format-bounded source object before
    /// learning its exact byte/row charge. Refused rows never enter the retained
    /// adjacency or history maps, and no partial image or anchor escapes. These
    /// source limits bound populations, not allocator overhead or process RSS.
    pub async fn seal_partition_with_source_limits(
        &self,
        cx: &QueryCx,
        root_id: PartitionRootVersion,
        floor: CommitSeq,
        limits: SealedLimits,
        mut source: crate::store::RootReadLimits,
    ) -> Result<SealedPartition, SealedError> {
        // Preserve the existing source-incidence contract even when a caller
        // offers a looser explicit source profile than the image allowance.
        source.max_incidences = source.max_incidences.min(limits.max_incidences);
        let (root, blocks, properties) = self
            .reopen_adjacency_bounded(cx, root_id, floor, source)
            .await
            .map_err(sealed_source_error)?;
        cx.checkpoint().map_err(SealedError::Interrupted)?;
        let compacted = crate::compact::compact_with_props(&blocks, &properties, floor)
            .map_err(SealedError::History)?;
        drop(blocks);
        drop(properties);
        let scope = SealedScope {
            source_root: root_id,
            graph: root.graph,
            branch: root.branch,
            partition: root.partition,
            floor,
            publication: root.published_at,
        };
        let mut checkpoint = || cx.checkpoint().map_err(SealedError::Interrupted);
        let image = image::build(compacted, limits, &mut checkpoint)?;
        SealedPartition::finish(scope, image, limits, &mut checkpoint)
    }
}

fn sealed_source_error(error: crate::store::RootReadError) -> SealedError {
    use crate::store::RootReadError;
    match error {
        RootReadError::Store(error) => SealedError::Store(error),
        RootReadError::Interrupted(error) => SealedError::Interrupted(error),
        RootReadError::Limit { resource, requested, limit } => {
            SealedError::Limit { resource, requested, limit }
        }
        RootReadError::BeyondPublication { .. } => SealedError::InvalidFloor,
        RootReadError::SizeOverflow => SealedError::SizeOverflow,
        RootReadError::AllocationFailed => SealedError::AllocationFailed,
    }
}

impl SealedPartition {
    fn finish(
        scope: SealedScope,
        image: Image,
        limits: SealedLimits,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<Self, SealedError> {
        image.validate(scope, limits, checkpoint)?;
        let bytes = wire::encode(&image, limits, checkpoint)?;
        let anchor = SealedAnchor {
            scope,
            fingerprint: fingerprint(&bytes),
            encoded_bytes: bytes.len(),
        };
        checkpoint()?;
        Ok(Self {
            anchor,
            image: Arc::new(image),
        })
    }

    pub const fn anchor(&self) -> SealedAnchor {
        self.anchor
    }

    pub fn shares_image_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.image, &other.image)
    }

    pub fn stats(&self) -> SealedStats {
        let inline_rows = self
            .image
            .rows
            .iter()
            .filter(|row| row.kind() == RowStorageKind::Inline)
            .count();
        SealedStats {
            rows: self.image.rows.len(),
            inline_rows,
            sealed_rows: self.image.rows.len() - inline_rows,
            incidences: self.image.incidences,
            property_rows: self.image.properties.len(),
            encoded_bytes: self.anchor.encoded_bytes,
        }
    }

    pub fn storage_kind(&self, src: VId, relation: RelationId) -> Option<RowStorageKind> {
        self.image.find_row(src, relation).map(Row::kind)
    }

    /// Produce canonical bytes for a derived-object cache/archive. These bytes
    /// carry no claim to be a registered authoritative graph object.
    pub fn encode(&self, cx: &QueryCx, limits: SealedLimits) -> Result<Vec<u8>, SealedError> {
        wire::encode(&self.image, limits, &mut || {
            cx.checkpoint().map_err(SealedError::Interrupted)
        })
    }

    /// Reload under a previously earned, opaque source receipt. Integrity is
    /// checked BEFORE any caller-controlled counts or property payloads decode.
    pub fn reload(
        cx: &QueryCx,
        anchor: SealedAnchor,
        bytes: &[u8],
        limits: SealedLimits,
        resolver: Option<&dyn CanonicalScalarResolver>,
    ) -> Result<Self, SealedError> {
        Self::reload_inner(anchor, bytes, limits, resolver, &mut || {
            cx.checkpoint().map_err(SealedError::Interrupted)
        })
    }

    fn reload_inner(
        anchor: SealedAnchor,
        bytes: &[u8],
        limits: SealedLimits,
        resolver: Option<&dyn CanonicalScalarResolver>,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<Self, SealedError> {
        checkpoint()?;
        check_limit("image bytes", bytes.len(), limits.max_image_bytes)?;
        if bytes.len() != anchor.encoded_bytes || fingerprint(bytes) != anchor.fingerprint {
            return Err(SealedError::ImageMismatch);
        }
        let image = wire::decode(bytes, limits, resolver, checkpoint)?;
        image.validate(anchor.scope, limits, checkpoint)?;
        checkpoint()?;
        Ok(Self {
            anchor,
            image: Arc::new(image),
        })
    }

    /// Stream retained edge incidences visible at an authorized scalar cut.
    /// A destination lower bound seeks inside EF directly for large rows.
    pub fn row_from<'a>(
        &'a self,
        cx: &QueryCx,
        src: VId,
        relation: RelationId,
        as_of: CommitSeq,
        lower_bound: Option<VId>,
    ) -> Result<SealedCursor<'a>, SealedError> {
        cx.checkpoint().map_err(SealedError::Interrupted)?;
        self.anchor.authorize(as_of)?;
        let row = self.image.find_row(src, relation);
        let position = match (row, lower_bound) {
            (Some(row), Some(destination)) => row.lower_bound(&self.image, destination),
            _ => 0,
        };
        Ok(SealedCursor {
            image: &self.image,
            row,
            position,
            as_of,
            finished: false,
        })
    }

    pub fn row<'a>(
        &'a self,
        cx: &QueryCx,
        src: VId,
        relation: RelationId,
        as_of: CommitSeq,
    ) -> Result<SealedCursor<'a>, SealedError> {
        self.row_from(cx, src, relation, as_of, None)
    }
}

#[derive(Debug)]
pub struct SealedEdge<'a> {
    pub entry: AdjacencyEntry,
    pub properties: &'a [(PropertyKeyId, CanonicalScalar)],
}

/// Explicit fallible pull protocol: cancellation is an error, never clean EOF.
/// A failed cursor stays failed/finished and cannot resume after skipping rows.
pub struct SealedCursor<'a> {
    image: &'a Image,
    row: Option<&'a Row>,
    position: usize,
    as_of: CommitSeq,
    finished: bool,
}

impl<'a> SealedCursor<'a> {
    pub fn next(&mut self, cx: &QueryCx) -> Result<Option<SealedEdge<'a>>, SealedError> {
        self.next_inner(&mut || cx.checkpoint().map_err(SealedError::Interrupted))
    }

    fn next_inner(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
    ) -> Result<Option<SealedEdge<'a>>, SealedError> {
        if self.finished {
            return Ok(None);
        }
        if let Err(error) = checkpoint() {
            self.finished = true;
            return Err(error);
        }
        let Some(row) = self.row else {
            self.finished = true;
            return Ok(None);
        };
        while self.position < row.len() {
            if self.position % 64 == 0 {
                if let Err(error) = checkpoint() {
                    self.finished = true;
                    return Err(error);
                }
            }
            let (entry, locator) = row
                .incidence(self.image, self.position)
                .expect("sealed admission validated every incidence");
            self.position += 1;
            if entry.visible_at(self.as_of) {
                let properties = if locator == 0 {
                    &[][..]
                } else {
                    self.image.properties[locator as usize - 1].as_slice()
                };
                return Ok(Some(SealedEdge { entry, properties }));
            }
        }
        self.finished = true;
        Ok(None)
    }
}
