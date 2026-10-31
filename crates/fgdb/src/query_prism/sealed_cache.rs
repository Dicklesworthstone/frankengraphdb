//! Owner-private reuse of completed decoded and compressed projections. No result, live
//! permit, unverified source claim or partial build is retained here.
//!
//! Standalone construction allowances remain exact keys. Composed resident and
//! buffered queries may reuse the same image/projection when their complete measured
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
    /// A query retains its complete measured construction admission. A resident
    /// or buffered query may reuse under different remaining allowances only
    /// when every construction counter
    /// fits; otherwise it rebuilds and reaches its own first refusal.
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
    fn retained_bytes(&self) -> Option<usize>;
}

impl RetainedProjection for SealedGraphView {
    fn retained_bytes(&self) -> Option<usize> {
        Some(self.charged_resident_bytes())
    }
}

impl RetainedProjection for SnapshotGraphView {
    fn retained_bytes(&self) -> Option<usize> {
        Some(self.charged_resident_bytes())
    }
}

impl RetainedProjection for Retained {
    fn retained_bytes(&self) -> Option<usize> {
        self.charged_bytes()
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

pub(crate) type BufferedSealedCache = BufferedCache<Request, Retained>;
pub(crate) type BufferedProjectionCache = BufferedCache<ProjectionRequest, SnapshotGraphView>;

impl BufferedSealedCache {
    /// Reuse only completed construction whose exact counters fit this caller.
    /// Root, cut, selection, graph laws and physical source bounds remain exact.
    pub(crate) fn lookup_query(
        &self,
        cx: &QueryCx,
        request: Request,
    ) -> Result<Option<Retained>, Box<asupersync::error::Error>> {
        self.lookup_controlled_by(
            |cached, retained| buffered_query_compatible(*cached, request, retained.query_usage),
            || cx.checkpoint(),
        )
    }
}

fn buffered_query_compatible(cached: Request, request: Request, usage: Option<QueryUsage>) -> bool {
    let (Some(policy), Some(usage)) = (request.query_policy, usage) else {
        return false;
    };
    if cached.query_policy.is_none() || !matches!(request.source, Source::Buffered { .. }) {
        return false;
    }
    // Normalize only the same three allowance profiles as the resident query
    // cache. Equality retains staging, projection, sealing and raw-source caps,
    // along with every current or future source and projection identity field.
    let mut compatible = cached;
    compatible.query_policy = request.query_policy;
    compatible.projection.source_limits.max_work_units =
        request.projection.source_limits.max_work_units;
    compatible.projection.source_limits.max_scratch_entries =
        request.projection.source_limits.max_scratch_entries;
    compatible == request && usage.fits(policy, request.projection.source_limits)
}

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
        checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<Graph>, E> {
        self.lookup_controlled_by(|cached, _| cached == &request, checkpoint)
    }

    fn lookup_controlled_by<E>(
        &self,
        mut matches: impl FnMut(&Key, &Graph) -> bool,
        mut checkpoint: impl FnMut() -> Result<(), E>,
    ) -> Result<Option<Graph>, E> {
        checkpoint()?;
        let graph =
            self.entries.iter().flatten().find_map(|entry| {
                matches(&entry.request, &entry.graph).then(|| entry.graph.clone())
            });
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
        let Some(bytes) = graph.retained_bytes() else {
            return Ok(());
        };
        let Ok(charge) = reserve(&self.pool, bytes) else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use fgdb_prism::{
        Directedness, FnxReadOptions, FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy,
        SelfLoopPolicy, SnapshotBinding,
    };
    use fgdb_types::{CommitSeq, ObjectId};

    fn request() -> Request {
        let mut options = FnxReadOptions::whole_graph(Some(CommitSeq(2)));
        options.selection.relation = Some(RelationId(1));
        options.source_limits.max_work_units = 100;
        options.source_limits.max_scratch_entries = 100;
        options.projection.directedness = Directedness::Directed;
        options.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        options.projection.self_loops = SelfLoopPolicy::Drop;
        Request {
            projection: ProjectionRequest {
                binding: SnapshotBinding {
                    root: ObjectId([1; 32]),
                    as_of: CommitSeq(2),
                },
                selection: options.selection,
                projection: options.projection,
                projection_limits: options.projection_limits,
                source_limits: options.source_limits,
            },
            source: Source::Buffered {
                sealing: SealedLimits {
                    max_rows: 100,
                    max_incidences: 100,
                    max_image_bytes: 1 << 20,
                    max_property_bytes: 1 << 16,
                },
                max_source_incidences: 100,
            },
            query_policy: Some(GqlQueryPolicy::new(100, 100, 100, 100)),
        }
    }

    fn usage() -> QueryUsage {
        let mut admission = AdmissionUsage::default();
        admission.absorb(3, 16, 5);
        QueryUsage {
            admission,
            source_work: 11,
            source_scratch: 5,
        }
    }

    #[test]
    fn buffered_query_reuse_requires_every_measured_construction_allowance() {
        let cached = request();
        let mut exact = cached;
        exact.query_policy = Some(GqlQueryPolicy::new(3, 1, 16, 5));
        exact.projection.source_limits.max_work_units = 11;
        exact.projection.source_limits.max_scratch_entries = 5;
        assert!(buffered_query_compatible(cached, exact, Some(usage())));
        assert!(buffered_query_compatible(exact, cached, Some(usage())));
        for dimension in 0..5 {
            let mut short = exact;
            match dimension {
                0 => short.query_policy = Some(GqlQueryPolicy::new(2, 1, 16, 5)),
                1 => short.query_policy = Some(GqlQueryPolicy::new(3, 1, 15, 5)),
                2 => short.query_policy = Some(GqlQueryPolicy::new(3, 1, 16, 4)),
                3 => short.projection.source_limits.max_work_units = 10,
                4 => short.projection.source_limits.max_scratch_entries = 4,
                _ => unreachable!(),
            }
            assert!(
                !buffered_query_compatible(cached, short, Some(usage())),
                "underfit construction must rebuild at dimension {dimension}",
            );
        }
        assert!(!buffered_query_compatible(cached, exact, None));
        let mut standalone = cached;
        standalone.query_policy = None;
        assert!(!buffered_query_compatible(standalone, exact, Some(usage())));
        assert!(!buffered_query_compatible(
            cached,
            standalone,
            Some(usage())
        ));
    }

    #[test]
    fn buffered_query_reuse_keeps_identity_and_physical_admissions_exact() {
        let cached = request();
        for dimension in 0..18 {
            let mut other = cached;
            other.query_policy = Some(GqlQueryPolicy::new(90, 90, 90, 90));
            match dimension {
                0 => other.projection.binding.root = ObjectId([2; 32]),
                1 => other.projection.binding.as_of = CommitSeq(1),
                2 => other.projection.selection.vertex_label = Some(LabelId(1)),
                3 => other.projection.selection.relation = Some(RelationId(2)),
                4 => {
                    other.projection.selection.weight = FnxWeightSpec::Property {
                        key: PropertyKeyId(1),
                        missing: MissingWeightPolicy::Reject,
                    };
                }
                5 => other.projection.projection.directedness = Directedness::Reversed,
                6 => other.projection.projection.parallel_edges = ParallelEdgePolicy::Reject,
                7 => other.projection.projection.self_loops = SelfLoopPolicy::Keep,
                8 => other.projection.projection_limits.max_vertices -= 1,
                9 => other.projection.projection_limits.max_input_edges -= 1,
                10 => other.projection.projection_limits.max_adjacency_entries -= 1,
                11 => other.projection.projection_limits.max_workspace_bytes -= 1,
                12 => other.projection.source_limits.max_staging_bytes -= 1,
                13..=17 => {
                    let Source::Buffered {
                        sealing,
                        max_source_incidences,
                    } = &mut other.source
                    else {
                        unreachable!();
                    };
                    match dimension {
                        13 => *max_source_incidences -= 1,
                        14 => sealing.max_rows -= 1,
                        15 => sealing.max_incidences -= 1,
                        16 => sealing.max_image_bytes -= 1,
                        17 => sealing.max_property_bytes -= 1,
                        _ => unreachable!(),
                    }
                }
                _ => unreachable!(),
            }
            assert!(
                !buffered_query_compatible(cached, other, Some(usage())),
                "identity or physical admission changed at dimension {dimension}",
            );
        }
    }
}
