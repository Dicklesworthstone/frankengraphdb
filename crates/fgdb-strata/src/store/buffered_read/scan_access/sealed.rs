//! Reuse admitted run membership or transfer charged relation histories.
//! Only the pinned root envelope is reopened for run membership; no complete
//! history reopen, vertex payload scan, or uncharged source clone is needed.

use super::*;
use crate::tiered::sealed::{AdjRunSegment, SealedLimits};
use core::convert::Infallible;

/// Field order keeps the input allocations alive under their reservations.
/// Compaction/output workspace is separate from this buffered-source owner.
pub(crate) struct SealedSource {
    pub(crate) blocks: Vec<Vec<AdjacencyEntry>>,
    pub(crate) properties: Vec<Option<BlockProps>>,
    _charges: Vec<MemoryCharge>,
}

/// Charged routing metadata only. Within a source, publication ordinals stay
/// ascending; no decoded history from another family has to remain resident.
pub(crate) struct SealedSources {
    blocks: Vec<(VId, usize)>,
    next: usize,
    work: usize,
    /// The complete selected RAW population, before history compaction.
    pub(crate) incidences: usize,
    _charge: MemoryCharge,
}

fn source_error(error: BufferedScanError<Infallible>) -> BufferedReadError {
    match error {
        BufferedScanError::Read(error) => error,
        BufferedScanError::Control(never) => match never {},
    }
}

impl<V: Vfs> BufferedPartition<V> {
    /// Reuse only run membership authenticated by this pinned root. Root
    /// parsing keeps a source-pool reservation; the compressed run itself has
    /// the caller's separate image/population limits, like a freshly sealed
    /// image. The caller must admit the complete raw relation before this I/O.
    pub(crate) async fn published_sealed_relation(
        &mut self,
        cx: &QueryCx,
        as_of: CommitSeq,
        relation: RelationId,
        limits: SealedLimits,
    ) -> Result<Option<(AdjRunSegment, CommitSeq)>, BufferedReadError> {
        self.begin(cx, as_of)?;
        // A keyed, immutable legacy-root hint avoids adding root-read scratch
        // or I/O to the ordinary sealer. A missing hint earns the proof again.
        let known_legacy = self
            .store
            .root_run_presence
            .lock()
            .ok()
            .is_some_and(|hint| *hint == Some((self.root_id, false)));
        if known_legacy {
            return Ok(None);
        }
        let root_bytes = self
            .limits
            .max_root_bytes
            .min(crate::root::MAX_ENCODED_ROOT_BYTES);
        let bytes = root_bytes
            .checked_mul(16)
            .ok_or(BufferedReadError::SizeOverflow)?;
        let _root_workspace = self.reserve_scan_bytes(cx, bytes)?;
        if let Some(run) = self
            .store
            .get_published_adj_run(cx, self.root_id, relation, as_of, limits, root_bytes)
            .await?
        {
            let verified_through = run.partition().anchor().verified_through();
            return Ok(Some((run, verified_through)));
        }
        self.store
            .get_published_adj_run_post_seal(cx, self.root_id, relation, as_of, limits, root_bytes)
            .await
            .map_err(BufferedReadError::from)
    }

    /// Admit the complete raw population before faulting any payload. Sorting
    /// authenticated descriptors changes only the order of independent source
    /// families, never the publication precedence inside a family's history.
    pub(crate) fn sealed_relation_sources(
        &mut self,
        cx: &QueryCx,
        floor: CommitSeq,
        relation: RelationId,
        max_incidences: usize,
    ) -> Result<SealedSources, BufferedReadError> {
        self.begin(cx, floor)?;
        let maximum = self.limits.max_work;
        let mut work = 0;
        let selected = self.block_directory.relation_blocks(relation, &mut || {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, maximum)
        })?;
        let count = selected.len();
        let mut incidences = 0usize;
        let mut counting = selected;
        while let Some(at) = self.block_directory.next_relation(&mut counting, &mut || {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, maximum)
        })? {
            incidences = incidences
                .checked_add(self.blocks[at].rows)
                .ok_or(BufferedReadError::SizeOverflow)?;
            limit("sealed source incidences", incidences, max_incidences)?;
        }
        let bytes = count
            .checked_mul(size_of::<(VId, usize)>())
            .ok_or(BufferedReadError::SizeOverflow)?;
        let charge = self.reserve_scan_bytes(cx, bytes)?;
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(count)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        let allocated = blocks
            .capacity()
            .checked_mul(size_of::<(VId, usize)>())
            .ok_or(BufferedReadError::SizeOverflow)?;
        limit("sealed source routing bytes", allocated, bytes)?;
        let mut selected = selected;
        while let Some(at) = self.block_directory.next_relation(&mut selected, &mut || {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, maximum)
        })? {
            let summary = self.blocks[at].summary.ok_or(BufferError::InvalidLoad)?;
            blocks.push((summary.source, at));
        }
        // The in-place heap sort spends this same cumulative work allowance
        // and observes cancellation; it allocates no second routing vector.
        crate::tiered::edge_scan::sort(&mut blocks, &mut || {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut work, 1, maximum)
        })?;
        Ok(SealedSources {
            blocks,
            next: 0,
            work,
            incidences,
            _charge: charge,
        })
    }

    /// Load one complete source/relation history. The caller consumes it with
    /// the ordinary compactor and drops its charges before asking for another.
    /// Complete initial admission already proved EId topology across families.
    pub(crate) async fn next_sealed_relation_source(
        &mut self,
        cx: &QueryCx,
        sources: &mut SealedSources,
    ) -> Result<Option<SealedSource>, BufferedReadError> {
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        let maximum = self.limits.max_work;
        advance(&mut sources.work, 1, maximum)?;
        let start = sources.next;
        let Some(&(vertex, _)) = sources.blocks.get(start) else {
            return Ok(None);
        };
        let mut end = start;
        while sources
            .blocks
            .get(end)
            .is_some_and(|&(source, _)| source == vertex)
        {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut sources.work, 1, maximum)?;
            end += 1;
        }
        self.load_sealed_relation_source(cx, sources, start, end)
            .await
            .map(Some)
    }

    /// Retain only the admitted relation's post-horizon statements. Root
    /// block spans include retirements as well as births, so an old birth
    /// retired after sealing remains in this overlay. The raw source count
    /// was already admitted before this physical pruning.
    ///
    /// Unlike family-at-a-time legacy sealing, this retains the complete
    /// suffix until overlay construction finishes. Its owner vectors and
    /// every decoded block keep the ordinary source-pool reservations; an
    /// oversized suffix refuses rather than escaping the pool or resealing.
    pub(crate) async fn post_seal_relation_source(
        &mut self,
        cx: &QueryCx,
        sources: &mut SealedSources,
        verified_through: CommitSeq,
    ) -> Result<SealedSource, BufferedReadError> {
        let maximum = self.limits.max_work;
        let mut retained = 0;
        for position in sources.next..sources.blocks.len() {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut sources.work, 1, maximum)?;
            let route = sources.blocks[position];
            if self.root.blocks[route.1].last_seq > verified_through {
                sources.blocks[retained] = route;
                retained += 1;
            }
        }
        sources.blocks.truncate(retained);
        sources.next = 0;
        self.load_sealed_relation_source(cx, sources, 0, retained)
            .await
    }

    async fn load_sealed_relation_source(
        &mut self,
        cx: &QueryCx,
        sources: &mut SealedSources,
        start: usize,
        end: usize,
    ) -> Result<SealedSource, BufferedReadError> {
        let maximum = self.limits.max_work;
        let count = end - start;
        // Reserve the three owner vectors before any allocation or payload
        // fault. Decoded images keep their own per-block reservations.
        let bytes = count
            .checked_mul(
                size_of::<Vec<AdjacencyEntry>>()
                    + size_of::<Option<BlockProps>>()
                    + size_of::<MemoryCharge>(),
            )
            .and_then(|bytes| bytes.checked_add(size_of::<MemoryCharge>()))
            .ok_or(BufferedReadError::SizeOverflow)?;
        let charge = self.reserve_scan_bytes(cx, bytes)?;
        let mut source = SealedSource {
            blocks: Vec::new(),
            properties: Vec::new(),
            _charges: Vec::new(),
        };
        source
            .blocks
            .try_reserve_exact(count)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        source
            .properties
            .try_reserve_exact(count)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        let charge_count = count
            .checked_add(1)
            .ok_or(BufferedReadError::SizeOverflow)?;
        source
            ._charges
            .try_reserve_exact(charge_count)
            .map_err(|_| MemoryError::AllocationFailed { requested: bytes })?;
        let allocated = source
            .blocks
            .capacity()
            .checked_mul(size_of::<Vec<AdjacencyEntry>>())
            .and_then(|bytes| {
                source
                    .properties
                    .capacity()
                    .checked_mul(size_of::<Option<BlockProps>>())
                    .and_then(|extra| bytes.checked_add(extra))
            })
            .and_then(|bytes| {
                source
                    ._charges
                    .capacity()
                    .checked_mul(size_of::<MemoryCharge>())
                    .and_then(|extra| bytes.checked_add(extra))
            })
            .ok_or(BufferedReadError::SizeOverflow)?;
        limit("sealed source owner bytes", allocated, bytes)?;
        source._charges.push(charge);

        for position in start..end {
            cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
            advance(&mut sources.work, 1, maximum)?;
            let at = sources.blocks[position].1;
            let descriptor = self.blocks[at];
            advance(&mut sources.work, descriptor.rows, maximum)?;
            let charge = self.reserve_scan_workspace(cx)?;
            let mut observe = || -> Result<(), BufferedScanError<Infallible>> {
                cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
                advance(&mut sources.work, 1, maximum)?;
                Ok(())
            };
            let image = self
                .block_controlled(cx, at, Admission::ScanBypass, &mut observe)
                .await
                .map_err(source_error)?;
            let BufferedValue {
                value: (entries, properties),
                _charge,
            } = retained::block(BufferedValue::from_reserved(image, charge), &mut observe)
                .map_err(source_error)?;
            source.blocks.push(entries);
            source.properties.push(properties);
            source._charges.push(_charge);
        }
        cx.checkpoint().map_err(BufferedReadError::Interrupted)?;
        sources.next = end;
        Ok(source)
    }
}
