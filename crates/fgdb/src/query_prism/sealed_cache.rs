//! Owner-private reuse of completed compressed projections. No result, live
//! permit, unverified source claim or partial build is retained here.
//!
//! Construction allowances are part of the exact key. A warm request therefore
//! cannot borrow a looser source/image/projection admission. Physical build
//! checkpoints and I/O disappear on a hit; live entry/return checks and all
//! kernel/result admissions remain the caller's responsibility. These owner
//! APIs carry no capability charge transcript and are not authorized hosts.

use super::cache::Request as ProjectionRequest;
use super::{RootReadLimits, SealedGraphView, SealedLimits};
use crate::ReadCacheUsage;
use crate::gql_exec::AdmissionUsage;
use crate::query::read_cache::live::{CACHE_ENTRIES, LiveCache};
use fgdb_gql::GqlQueryPolicy;
use fgdb_strata::tiered::memory::{MemoryCharge, MemoryPool};
use fgdb_strata::tiered::sealed::SealedAnchor;
use fgdb_types::QueryCx;

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
    /// remaining allowances must rebuild and reach their own first refusal.
    /// Standalone preparation has no GQL admission profile.
    pub(crate) query_policy: Option<GqlQueryPolicy>,
}

#[derive(Clone)]
pub(crate) struct Retained {
    pub(crate) graph: SealedGraphView,
    pub(crate) query_usage: Option<AdmissionUsage>,
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
            std::mem::size_of::<Option<AdmissionUsage>>()
                + std::mem::size_of::<Option<GqlQueryPolicy>>()
        } else {
            0
        };
        self.graph.charged_resident_bytes().checked_add(extra)
    }
}

pub(crate) type SealedProjectionCache = LiveCache<crate::Snapshot, Request, Retained>;

struct BufferedEntry {
    request: Request,
    graph: SealedGraphView,
    charge: MemoryCharge,
}

/// The buffered view is exclusively borrowed by each preparation; it requires
/// no mutex or process-global owner. Retained values use a separate explicit
/// pool, never the source extent pool. Source admission, caller-held graph
/// clones and active build peaks remain governed by their existing allowances.
pub(crate) struct BufferedCache {
    pool: MemoryPool,
    entries: [Option<BufferedEntry>; CACHE_ENTRIES],
}

impl BufferedCache {
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
        cx.checkpoint()?;
        // Explicit host policy replacement releases this cache's old ownership.
        // Any caller-held graph clones remain valid under their own allowance.
        *self = Self::new(pool);
        Ok(self.usage())
    }

    pub(crate) fn lookup(
        &self,
        cx: &QueryCx,
        request: Request,
    ) -> Result<Option<SealedGraphView>, Box<asupersync::error::Error>> {
        cx.checkpoint()?;
        let graph = self
            .entries
            .iter()
            .flatten()
            .find_map(|entry| (entry.request == request).then(|| entry.graph.clone()));
        cx.checkpoint()?;
        Ok(graph)
    }

    pub(crate) fn remember(
        &mut self,
        cx: &QueryCx,
        request: Request,
        graph: &SealedGraphView,
    ) -> Result<(), Box<asupersync::error::Error>> {
        cx.checkpoint()?;
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
        // Transfer an already-completed caller-owned image into retained-cache
        // ownership only after admission. A full/disabled pool merely declines
        // this optional extra owner; it never evicts a live entry or a frame.
        let Ok(charge) = self.pool.reserve(cx, graph.charged_resident_bytes()) else {
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
