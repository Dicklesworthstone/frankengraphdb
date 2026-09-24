//! Input admission for authenticated root traversal, before graph collection.
//!
//! The ordinary and bounded paths share the same object, range, partition,
//! predecessor-chain and cross-object history checks. Quota refusal never mints
//! an admission token or returns a successfully validated prefix. Vertex history
//! still keeps the existing validator's per-version rows, but no second complete
//! collection of decoded vertex patches is retained by an adjacency consumer.

use super::{BlockProps, BlockStore, StoreError};
use asupersync::fs::Vfs;
use fgdb_types::{CommitSeq, QueryCx, StorageReadCx};

/// Limits on the SOURCE of a root traversal, not on its compacted output.
/// Repeated references/versions are charged on every actual visit. Counts bound
/// the existing history validators as well as retained rows; they are not an
/// allocator-byte quota or an external-memory implementation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RootReadLimits {
    /// Encoded root bytes. Enforced by the bounded file reader before decode.
    pub max_root_bytes: usize,
    /// Sum of encoded block, hosted-property and vertex-patch bytes, excluding
    /// the separately bounded root. A single format-bounded object is in hand
    /// when charged, but is not decoded or retained after a byte refusal.
    pub max_source_bytes: usize,
    pub max_blocks: usize,
    pub max_vertex_patches: usize,
    /// Raw adjacency statements, including restatements and invisible history.
    pub max_incidences: usize,
    /// Raw vertex versions, including history outside the requested cut.
    pub max_vertex_versions: usize,
}

impl Default for RootReadLimits {
    fn default() -> Self {
        Self {
            max_root_bytes: crate::root::MAX_ENCODED_ROOT_BYTES,
            max_source_bytes: 256 * 1024 * 1024,
            max_blocks: 1_000_000,
            max_vertex_patches: 1_000_000,
            max_incidences: 1_000_000,
            max_vertex_versions: 1_000_000,
        }
    }
}

#[derive(Debug)]
pub enum RootReadError {
    Store(Box<StoreError>),
    Interrupted(Box<asupersync::error::Error>),
    Limit {
        resource: &'static str,
        requested: usize,
        limit: usize,
    },
    BeyondPublication {
        requested: CommitSeq,
        publication: CommitSeq,
    },
    SizeOverflow,
    AllocationFailed,
}

impl From<StoreError> for RootReadError {
    fn from(error: StoreError) -> Self {
        Self::Store(Box::new(error))
    }
}

impl core::fmt::Display for RootReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Store(error) => error.fmt(f),
            Self::Interrupted(_) => f.write_str("root source traversal interrupted"),
            Self::Limit {
                resource,
                requested,
                limit,
            } => write!(
                f,
                "ResourceExhausted: {resource} needs at least {requested}, limit {limit}"
            ),
            Self::BeyondPublication {
                requested,
                publication,
            } => write!(
                f,
                "root source cut {} exceeds publication {}",
                requested.0, publication.0
            ),
            Self::SizeOverflow => f.write_str("root source accounting overflow"),
            Self::AllocationFailed => f.write_str("root source collection allocation failed"),
        }
    }
}

impl std::error::Error for RootReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error.as_ref()),
            Self::Interrupted(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

/// Only counts leave a verifier through this hook, never rows or authority.
/// Bytes are observed BEFORE their decoder; row counts BEFORE a history map
/// or retained collection can grow. An error stops the same verifier loop.
pub(super) enum RootReadEvent {
    /// Every lawful source object has at least one encoded byte. At an exact
    /// byte ceiling refuse before opening another object, not after its I/O.
    ObjectStart,
    SourceBytes(usize),
    Incidences(usize),
    VertexVersions(usize),
}

struct Admission {
    limits: RootReadLimits,
    bytes: usize,
    incidences: usize,
    vertex_versions: usize,
}

fn admit(resource: &'static str, requested: usize, limit: usize) -> Result<(), RootReadError> {
    if requested > limit {
        Err(RootReadError::Limit {
            resource,
            requested,
            limit,
        })
    } else {
        Ok(())
    }
}

impl Admission {
    fn new(
        limits: RootReadLimits,
        root: &crate::root::PartitionRoot,
    ) -> Result<Self, RootReadError> {
        // The root has already been structurally decoded/authenticated. These
        // ceilings apply before ANY referenced block or vertex patch is opened.
        admit("source blocks", root.blocks.len(), limits.max_blocks)?;
        admit(
            "source vertex patches",
            root.vertex_patches.len(),
            limits.max_vertex_patches,
        )?;
        Ok(Self {
            limits,
            bytes: 0,
            incidences: 0,
            vertex_versions: 0,
        })
    }

    fn observe(&mut self, event: RootReadEvent) -> Result<(), RootReadError> {
        let (counter, increment, maximum, resource) = match event {
            RootReadEvent::ObjectStart => {
                let minimum = self
                    .bytes
                    .checked_add(1)
                    .ok_or(RootReadError::SizeOverflow)?;
                return admit(
                    "source encoded bytes",
                    minimum,
                    self.limits.max_source_bytes,
                );
            }
            RootReadEvent::SourceBytes(bytes) => (
                &mut self.bytes,
                bytes,
                self.limits.max_source_bytes,
                "source encoded bytes",
            ),
            RootReadEvent::Incidences(rows) => (
                &mut self.incidences,
                rows,
                self.limits.max_incidences,
                "source incidences",
            ),
            RootReadEvent::VertexVersions(rows) => (
                &mut self.vertex_versions,
                rows,
                self.limits.max_vertex_versions,
                "source vertex versions",
            ),
        };
        let next = counter
            .checked_add(increment)
            .ok_or(RootReadError::SizeOverflow)?;
        admit(resource, next, maximum)?;
        *counter = next;
        Ok(())
    }
}

/// The same fully admitted adjacency returned by reopen, without retaining
/// vertex-patch payloads. This owned data is not an authorization/retention token.
pub type ReopenedAdjacency = (
    crate::root::PartitionRoot,
    Vec<Vec<crate::AdjacencyEntry>>,
    Vec<Option<BlockProps>>,
);

impl<V: Vfs> BlockStore<V> {
    /// Shared by ordinary root reads and source-budgeted traversal. A smaller
    /// caller limit cannot enlarge the format's canonical maximum.
    pub(super) async fn get_root_with_byte_limit(
        &self,
        cx: &impl StorageReadCx,
        id: crate::PartitionRootVersion,
        maximum: usize,
    ) -> Result<crate::root::PartitionRoot, StoreError> {
        let maximum = maximum.min(crate::root::MAX_ENCODED_ROOT_BYTES) as u64;
        let bytes = self.read_object_bytes(cx, id.0, maximum).await?;
        crate::root::read_root(self.k_oid.expose(), self.namespace, &bytes, id.0)
            .map_err(StoreError::MalformedRoot)
    }

    /// Authenticate a whole root under explicit source ceilings, retaining only
    /// its adjacency. No snapshot range skips an unproved block/patch, and no
    /// property, range, predecessor or identity check is weakened.
    ///
    /// The requested cut is validated after the bounded root read but before
    /// payload I/O. Source byte admission precedes decoding. A row-count refusal
    /// can have decoded at most ONE format-bounded object beyond the admitted
    /// prefix, but never grows a history map/collection with those refused rows.
    /// The vertex validator still owns rows for distinct content versions until
    /// admission completes; `max_vertex_versions` bounds that population. Object
    /// decoders and history maps are not async streaming/spill implementations
    /// or fallible-allocation RSS guarantees.
    pub async fn reopen_adjacency_bounded(
        &self,
        cx: &QueryCx,
        id: crate::PartitionRootVersion,
        requested_cut: CommitSeq,
        limits: RootReadLimits,
    ) -> Result<ReopenedAdjacency, RootReadError> {
        cx.checkpoint().map_err(RootReadError::Interrupted)?;
        let root = self
            .get_root_with_byte_limit(cx, id, limits.max_root_bytes)
            .await?;
        cx.checkpoint().map_err(RootReadError::Interrupted)?;
        if requested_cut > root.published_at {
            return Err(RootReadError::BeyondPublication {
                requested: requested_cut,
                publication: root.published_at,
            });
        }
        let mut admission = Admission::new(limits, &root)?;
        let mut observe = |event| {
            cx.checkpoint().map_err(RootReadError::Interrupted)?;
            admission.observe(event)
        };
        let resolved = self
            .inspect_root_blocks_observed(cx, &root, |_, _| true, &mut observe)
            .await?;
        // Still validate the complete vertex history. The validator retains its
        // existing owned version records, but no second patch collection is kept.
        drop(
            self.inspect_root_patches_observed(cx, &root, |_, _| false, &mut observe)
                .await?,
        );
        let mut blocks = Vec::new();
        let mut properties = Vec::new();
        blocks
            .try_reserve_exact(resolved.len())
            .map_err(|_| RootReadError::AllocationFailed)?;
        properties
            .try_reserve_exact(resolved.len())
            .map_err(|_| RootReadError::AllocationFailed)?;
        for (entries, props) in resolved {
            cx.checkpoint().map_err(RootReadError::Interrupted)?;
            blocks.push(entries);
            properties.push(props);
        }
        cx.checkpoint().map_err(RootReadError::Interrupted)?;
        Ok((root, blocks, properties))
    }
}

#[cfg(test)]
mod tests;
