//! Owner-private reuse of completed decoded and compressed projections. No result, live
//! permit, unverified source claim or partial build is retained here.
//!
//! Standalone and buffered construction allowances remain exact keys. Composed
//! queries may reuse the same image/projection when their complete measured
//! construction cost fits every fresh source/GQL counter allowance. Otherwise
//! the ordinary build reaches its original first refusal. Physical build
//! checkpoints and I/O disappear on a hit; live entry/return checks and all
//! kernel/result admissions remain the caller's responsibility. These owner
//! APIs carry no capability charge transcript and are not authorized hosts.

use super::cache::Request as ProjectionRequest;
use super::{FnxSourceLimits, RootReadLimits, SealedGraphView, SealedLimits, SnapshotGraphView};
use crate::ReadCacheUsage;
use crate::gql_exec::AdmissionUsage;
use crate::query::read_cache::live::{CACHE_ENTRIES, LiveCache};
use fgdb_gql::GqlQueryPolicy;
use fgdb_strata::tiered::memory::{MemoryCharge, MemoryError, MemoryPool};
use fgdb_strata::tiered::sealed::SealedAnchor;
use fgdb_types::QueryCx;
use std::sync::Arc;

pub(crate) const CACHE_BYTES: usize = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// Every root object was authenticated under these independent raw bounds.
    Root {
        sealing: SealedLimits,
        limits: RootReadLimits,
    },
    /// A caller supplied an opaque earned receipt; a different image/floor or
    /// relation scope must still undergo the ordinary source compatibility law.
    Admitted(SealedAnchor),
    /// The buffered owner already admitted its immutable root. Its own source
    /// pool/work limits never change; these are the remaining per-build bounds.
    Buffered {
        sealing: SealedLimits,
        max_source_incidences: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) projection: ProjectionRequest,
    pub(crate) source: Source,
    /// A query retains its complete measured construction admission. Different
    /// remaining allowances may reuse only when every construction counter
    /// fits; otherwise they rebuild and reach their own first refusal.
    /// Standalone preparation has no GQL admission profile.
    pub(crate) query_policy: Option<GqlQueryPolicy>,
}

/// Exact counters from a successful composed projection build, before any
/// kernel or result conversion. Construction only adds nonnegative amounts,
/// so fitting these complete totals proves that no construction prefix can
/// refuse under the new counter allowances. Staging and projection bounds
/// remain exact key dimensions, not estimates reconstructed from the output.
#[derive(Clone, Copy)]
pub(crate) struct QueryUsage {
    pub(crate) admission: AdmissionUsage,
    pub(crate) source_work: u64,
    pub(crate) source_scratch: u64,
}

impl QueryUsage {
    fn fits(self, policy: GqlQueryPolicy, limits: FnxSourceLimits) -> bool {
        self.source_work <= limits.max_work_units
            && self.source_scratch <= limits.max_scratch_entries
            && self.admission.absorbs(policy, 0, 0, 0)
    }
}

#[derive(Clone)]
pub(crate) struct Retained {
    pub(crate) graph: SealedGraphView,
    pub(crate) query_usage: Option<QueryUsage>,
}

impl Retained {
    pub(crate) fn standalone(graph: SealedGraphView) -> Self {
        Self {
            graph,
            query_usage: None,
        }
    }

    pub(crate) fn charged_bytes(&self) -> Option<usize> {
        let extra = if self.query_usage.is_some() {
            std::mem::size_of::<Option<QueryUsage>>()
                + std::mem::size_of::<Option<GqlQueryPolicy>>()
        } else {
            0
        };
        self.graph.charged_resident_bytes().checked_add(extra)
    }
}

pub(crate) type SealedProjectionCache = LiveCache<crate::Snapshot, Request, Retained>;

impl SealedProjectionCache {
    /// Composed queries charge the saved construction totals again. Only
    /// counter allowances may differ; owner, cut, earned anchor, selection,
    /// graph laws, projection bounds and staging bytes must still match.
    pub(crate) fn lookup_query_controlled<E>(
        &self,
        owner: &Arc<crate::Snapshot>,
        request: Request,
        poll: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<Option<Retained>, E> {
        self.lookup_controlled_by(
            owner,
            |cached, retained| {
                if *cached == request {
                    return true;
                }
                let (Some(policy), Some(usage)) = (request.query_policy, retained.query_usage)
                else {
                    return false;
                };
                if cached.query_policy.is_none() || !matches!(request.source, Source::Admitted(_)) {
                    return false;
                }
                // Normalize only the three counter profiles. Equality still
                // compares every other current or future request dimension.
                let mut compatible = *cached;
                compatible.query_policy = request.query_policy;
                compatible.projection.source_limits.max_work_units =
                    request.projection.source_limits.max_work_units;
                compatible.projection.source_limits.max_scratch_entries =
                    request.projection.source_limits.max_scratch_entries;
                compatible == request && usage.fits(policy, request.projection.source_limits)
            },
            poll,
        )
    }
}

pub(crate) trait RetainedProjection: Clone {
    fn retained_bytes(&self) -> usize;
}

impl RetainedProjection for SealedGraphView {
    fn retained_bytes(&self) -> usize {
        self.charged_resident_bytes()
    }
}

impl RetainedProjection for SnapshotGraphView {
    fn retained_bytes(&self) -> usize {
        self.charged_resident_bytes()
    }
}

struct BufferedEntry<Key, Graph> {
    request: Key,
    graph: Graph,
    charge: MemoryCharge,
}

/// The buffered view is exclusively borrowed by each preparation; it requires
/// no mutex or process-global owner. Retained values use a separate explicit
/// pool, never the source extent pool. Source admission, caller-held graph
/// clones and active build peaks remain governed by their existing allowances.
pub(crate) struct BufferedCache<Key, Graph> {
    pool: MemoryPool,
    entries: [Option<BufferedEntry<Key, Graph>>; CACHE_ENTRIES],
}

pub(crate) type BufferedSealedCache = BufferedCache<Request, SealedGraphView>;
pub(crate) type BufferedProjectionCache = BufferedCache<ProjectionRequest, SnapshotGraphView>;

impl<Key: PartialEq, Graph: RetainedProjection> BufferedCache<Key, Graph> {
    pub(crate) fn new(pool: MemoryPool) -> Self {
        Self {
            pool,
            entries: [const { None }; CACHE_ENTRIES],
        }
    }

    pub(crate) fn pool(&self) -> &MemoryPool {
        &self.pool
    }

    pub(crate) fn usage(&self) -> ReadCacheUsage {
        ReadCacheUsage {
            max_bytes: self.pool.effective_limit(),
            retained_bytes: self
                .entries
                .iter()
                .flatten()
                .map(|entry| entry.charge.bytes())
                .sum(),
            entries: self.entries.iter().flatten().count(),
        }
    }

    pub(crate) fn replace_pool(
        &mut self,
        cx: &QueryCx,
        pool: MemoryPool,
    ) -> Result<ReadCacheUsage, Box<asupersync::error::Error>> {
        self.replace_pool_controlled(pool, || cx.checkpoint())
    }

    fn replace_pool_controlled<E>(
        &mut self,
        pool: MemoryPool,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<ReadCacheUsage, E> {
        checkpoint()?;
        // Explicit host policy replacement releases this cache's old ownership.
        // Any caller-held graph clones remain valid under their own allowance.
        *self = Self::new(pool);
        Ok(self.usage())
    }

    pub(crate) fn lookup(
        &self,
        cx: &QueryCx,
        request: Key,
    ) -> Result<Option<Graph>, Box<asupersync::error::Error>> {
        self.lookup_controlled(request, || cx.checkpoint())
    }

    fn lookup_controlled<E>(
        &self,
        request: Key,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<Graph>, E> {
        checkpoint()?;
        let graph = self
            .entries
            .iter()
            .flatten()
            .find_map(|entry| (entry.request == request).then(|| entry.graph.clone()));
        checkpoint()?;
        Ok(graph)
    }

    pub(crate) fn remember(
        &mut self,
        cx: &QueryCx,
        request: Key,
        graph: &Graph,
    ) -> Result<(), Box<asupersync::error::Error>> {
        self.remember_controlled(
            request,
            graph,
            || cx.checkpoint(),
            |pool, bytes| pool.reserve(cx, bytes),
        )
    }

    fn remember_controlled<E>(
        &mut self,
        request: Key,
        graph: &Graph,
        mut checkpoint: impl FnMut() -> Result<(), E>,
        reserve: impl FnOnce(&MemoryPool, usize) -> Result<MemoryCharge, MemoryError>,
    ) -> Result<(), E> {
        checkpoint()?;
        if self
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.request == request)
        {
            return Ok(());
        }
        let Some(slot) = self.entries.iter_mut().find(|entry| entry.is_none()) else {
            return Ok(());
        };
        // Transfer an already-completed caller-owned graph into retained-cache
        // ownership only after admission. A full/disabled pool merely declines
        // this optional extra owner; it never evicts a live entry or a frame.
        let Ok(charge) = reserve(&self.pool, graph.retained_bytes()) else {
            return Ok(());
        };
        *slot = Some(BufferedEntry {
            request,
            graph: graph.clone(),
            charge,
        });
        Ok(())
    }
}
