//! Bounded reuse of projections from an exact, still-live snapshot allocation.
//!
//! This is an in-process sidecar, not graph storage or a persistent index. A
//! weak owner prevents an entry from pinning an entire database generation;
//! its allocation identity prevents reuse across independently opened handles,
//! even when their root bytes agree. Live entries are never evicted to admit
//! another request. Each opened database owns its own allowances. A full cache
//! takes the ordinary build path; contention waits through the caller's live
//! cancellation check, never choosing whether an entry is retained. No lock is
//! held while checking cancellation, visiting data, building or dropping entries.
//!
//! An entry may carry an `Extra` beside its graph: the authorized analytics
//! path keeps the live charge sequence its build made, which every reuse
//! spends again (fgdb-0tbrz). Each sidecar has its own entries and byte allowance.
//!
//! `LiveCache` is the same sidecar for any key and value: authorized
//! full-scan admission keeps its admitted records under it (fgdb-61p1n).

use super::{
    EmbeddedReadView, Error, FnxSelection, FnxSourceLimits, ProjectionLimits, ProjectionSpec,
    QueryCx, SnapshotBinding, SnapshotGraphView,
};
pub(crate) use crate::query::read_cache::live::LiveCache;
use std::sync::Arc;

#[cfg(test)]
use crate::query::read_cache::live::CACHE_ENTRIES;
// A sum of the vectors each kept graph holds, not a process RSS claim.
// Snapshot data, caller-held graphs, concurrent builders and allocator metadata
// remain outside this retained-cache allowance. It is the default projection
// admission, so whatever a default CALL may build can be kept: at 64 MiB no
// Tier-1 projection fit (a directed web-google one charges about 490 MB), and
// every PageRank and WCC rebuilt its own (fgdb-62mzm). An entry is charged what
// it keeps, not its build's peak: the peak also counts the freed edge staging,
// about half of it, and pokec's PageRank graph alone then filled the allowance,
// so every WCC beside it rebuilt for 75 s.
pub(crate) const CACHE_BYTES: usize =
    fgdb_prism::FnxReadOptions::DEFAULT_PROJECTION_LIMITS.max_workspace_bytes;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) binding: SnapshotBinding,
    pub(crate) selection: FnxSelection,
    pub(crate) projection: ProjectionSpec,
    pub(crate) projection_limits: ProjectionLimits,
    pub(crate) source_limits: FnxSourceLimits,
}

/// Projections by request, with an optional `Extra` beside each graph.
pub(crate) type ProjectionCache<Owner, Extra = ()> =
    LiveCache<Owner, Request, (SnapshotGraphView, Extra)>;

impl<Owner, Extra: Clone> ProjectionCache<Owner, Extra> {
    /// `extra_bytes` is the retained size of `extra`, charged with the graph.
    pub(crate) fn remember_graph<E>(
        &self,
        owner: &Arc<Owner>,
        request: Request,
        graph: &SnapshotGraphView,
        extra: Extra,
        extra_bytes: usize,
        poll: &mut impl FnMut() -> Result<(), E>,
    ) -> Result<(), E> {
        if graph.binding() != request.binding || graph.spec() != request.projection {
            return Ok(());
        }
        let Some(bytes) = graph.charged_resident_bytes().checked_add(extra_bytes) else {
            return Ok(());
        };
        self.remember(owner, request, (graph.clone(), extra), bytes, poll)
    }
}

impl<Owner> ProjectionCache<Owner> {
    /// The bool is a physical reuse witness for tests/benchmarks, not part of
    /// logical result identity. Neither failed builds nor failed final checks
    /// can enter the cache. Even a hit must pass both caller checkpoints.
    fn get_or_build<E>(
        &self,
        owner: &Arc<Owner>,
        request: Request,
        mut checkpoint: impl FnMut() -> Result<(), E>,
        build: impl FnOnce() -> Result<SnapshotGraphView, E>,
    ) -> Result<(SnapshotGraphView, bool), E> {
        checkpoint()?;
        if let Some((graph, ())) = self.lookup_controlled(owner, request, &mut checkpoint)? {
            checkpoint()?;
            return Ok((graph, true));
        }
        let graph = build()?;
        checkpoint()?;
        self.remember_graph(owner, request, &graph, (), 0, &mut checkpoint)?;
        Ok((graph, false))
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn projection(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    as_of: fgdb_types::CommitSeq,
    selection: FnxSelection,
    projection: ProjectionSpec,
    projection_limits: ProjectionLimits,
    source_limits: FnxSourceLimits,
) -> Result<SnapshotGraphView, Error> {
    // A cached old cut must never turn a future request into a successful read.
    cx.checkpoint().map_err(Error::Cancelled)?;
    view.snapshot.check_frontier(as_of).map_err(Error::Read)?;
    let request = Request {
        binding: SnapshotBinding {
            root: view.partition_root().0,
            as_of,
        },
        selection,
        projection,
        projection_limits,
        source_limits,
    };
    view.snapshot
        .read_caches
        .projections
        .get_or_build(
            &view.snapshot,
            request,
            || cx.checkpoint().map_err(Error::Cancelled),
            || {
                view.build_prism_projection_at(
                    cx,
                    as_of,
                    selection,
                    projection,
                    projection_limits,
                    source_limits,
                )
            },
        )
        .map(|(graph, _)| graph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
    use fgdb_prism::{
        Directedness, FnxReadOptions, FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy,
        ProjectionEdge, SelfLoopPolicy,
    };
    use fgdb_types::{CommitSeq, EId, ObjectId, VId};
    use std::cell::Cell;

    fn request() -> Request {
        let options = FnxReadOptions::whole_graph(Some(CommitSeq(1)));
        Request {
            binding: SnapshotBinding {
                root: ObjectId([1; 32]),
                as_of: CommitSeq(1),
            },
            selection: options.selection,
            projection: options.projection,
            projection_limits: options.projection_limits,
            source_limits: options.source_limits,
        }
    }

    fn graph(request: Request) -> SnapshotGraphView {
        SnapshotGraphView::build(
            request.binding,
            &[VId(1), VId(2)],
            &[ProjectionEdge {
                eid: EId(1),
                source: VId(1),
                target: VId(2),
                weight: 1.0,
            }],
            request.projection,
            request.projection_limits,
        )
        .unwrap()
    }

    fn acquire(
        cache: &ProjectionCache<()>,
        owner: &Arc<()>,
        request: Request,
        builds: &Cell<usize>,
    ) -> (SnapshotGraphView, bool) {
        cache
            .get_or_build(
                owner,
                request,
                || Ok::<_, &'static str>(()),
                || {
                    builds.set(builds.get() + 1);
                    Ok(graph(request))
                },
            )
            .unwrap()
    }

    #[test]
    fn a_hit_reuses_the_projection_without_invoking_the_builder() {
        let cache = ProjectionCache::new(CACHE_BYTES);
        let owner = Arc::new(());
        let builds = Cell::new(0);
        let (first, hit) = acquire(&cache, &owner, request(), &builds);
        assert!(!hit);
        let (second, hit) = acquire(&cache, &owner, request(), &builds);
        assert!(hit);
        assert_eq!(builds.get(), 1);
        assert!(first.shares_cache_with(&second));
        assert_eq!(first.digest(), second.digest());
    }

    #[test]
    fn independently_opened_owners_never_share_even_an_identical_root() {
        let cache = ProjectionCache::new(CACHE_BYTES);
        let first_owner = Arc::new(());
        let second_owner = Arc::new(());
        let builds = Cell::new(0);
        let first = acquire(&cache, &first_owner, request(), &builds).0;
        let (second, hit) = acquire(&cache, &second_owner, request(), &builds);
        assert!(!hit);
        assert!(!first.shares_cache_with(&second));
        assert_eq!(first.digest(), second.digest());
        assert_eq!(builds.get(), 2);
    }

    #[test]
    fn every_projection_and_admission_dimension_is_part_of_the_key() {
        let base = request();
        let mut variants = Vec::new();
        let mut other = base;
        other.binding.root = ObjectId([2; 32]);
        variants.push(other);
        other = base;
        other.binding.as_of = CommitSeq(0);
        variants.push(other);
        other = base;
        other.selection.vertex_label = Some(LabelId(1));
        variants.push(other);
        other = base;
        other.selection.relation = Some(RelationId(1));
        variants.push(other);
        other = base;
        other.selection.weight = FnxWeightSpec::Property {
            key: PropertyKeyId(1),
            missing: MissingWeightPolicy::Reject,
        };
        variants.push(other);
        other.selection.weight = FnxWeightSpec::Property {
            key: PropertyKeyId(1),
            missing: MissingWeightPolicy::Zero,
        };
        variants.push(other);
        other = base;
        other.projection.directedness = Directedness::Reversed;
        variants.push(other);
        other = base;
        other.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        variants.push(other);
        other = base;
        other.projection.self_loops = SelfLoopPolicy::Drop;
        variants.push(other);
        other = base;
        other.projection_limits.max_vertices -= 1;
        variants.push(other);
        other = base;
        other.projection_limits.max_input_edges -= 1;
        variants.push(other);
        other = base;
        other.projection_limits.max_adjacency_entries -= 1;
        variants.push(other);
        other = base;
        other.projection_limits.max_workspace_bytes -= 1;
        variants.push(other);
        other = base;
        other.source_limits.max_work_units -= 1;
        variants.push(other);
        other = base;
        other.source_limits.max_scratch_entries -= 1;
        variants.push(other);
        other = base;
        other.source_limits.max_staging_bytes -= 1;
        variants.push(other);
        for changed in variants {
            let cache = ProjectionCache::new(CACHE_BYTES);
            let owner = Arc::new(());
            let builds = Cell::new(0);
            acquire(&cache, &owner, base, &builds);
            assert!(!acquire(&cache, &owner, changed, &builds).1);
            assert_eq!(builds.get(), 2);
            assert!(acquire(&cache, &owner, base, &builds).1);
        }
    }

    #[test]
    fn cache_is_non_owning_and_reclaims_dead_generations() {
        let cache = ProjectionCache::new(CACHE_BYTES);
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let builds = Cell::new(0);
        let first = acquire(&cache, &owner, request(), &builds).0;
        assert_eq!(Arc::strong_count(&owner), 1);
        drop(owner);
        assert!(weak.upgrade().is_none());
        let replacement = Arc::new(());
        let (second, hit) = acquire(&cache, &replacement, request(), &builds);
        assert!(!hit);
        assert!(!first.shares_cache_with(&second));
        assert_eq!(cache.entries.lock().unwrap().iter().flatten().count(), 1);
    }

    #[test]
    fn full_and_oversized_caches_build_without_displacing_live_entries() {
        let owner = Arc::new(());
        let builds = Cell::new(0);
        let bytes = graph(request()).charged_resident_bytes();
        // An entry is charged what it keeps, below its build's peak.
        assert!(bytes < graph(request()).charged_workspace_bytes());
        let cache = ProjectionCache::new(bytes - 1);
        assert!(!acquire(&cache, &owner, request(), &builds).1);
        assert!(!acquire(&cache, &owner, request(), &builds).1);
        assert!(cache.entries.lock().unwrap().iter().all(Option::is_none));
        let cache = ProjectionCache::new(bytes * CACHE_ENTRIES);
        for sequence in 0..CACHE_ENTRIES {
            let mut key = request();
            key.binding.as_of = CommitSeq(sequence as u64);
            assert!(!acquire(&cache, &owner, key, &builds).1);
        }
        let mut extra = request();
        extra.binding.as_of = CommitSeq(CACHE_ENTRIES as u64);
        assert!(!acquire(&cache, &owner, extra, &builds).1);
        assert!(
            !acquire(&cache, &owner, request(), &builds)
                .0
                .vertex_ids()
                .is_empty()
        );
        assert!(!acquire(&cache, &owner, extra, &builds).1);
        assert!(acquire(&cache, &owner, request(), &builds).1);
        assert_eq!(
            cache.entries.lock().unwrap().iter().flatten().count(),
            CACHE_ENTRIES
        );
    }

    #[test]
    fn failures_and_final_cancellation_never_publish_a_cache_entry() {
        let cache = ProjectionCache::new(CACHE_BYTES);
        let owner = Arc::new(());
        let failed = cache.get_or_build(&owner, request(), || Ok(()), || Err("build"));
        assert_eq!(failed.unwrap_err(), "build");
        let mut checks = 0;
        let cancelled = cache.get_or_build(
            &owner,
            request(),
            || {
                checks += 1;
                if checks == 2 { Err("cancel") } else { Ok(()) }
            },
            || Ok(graph(request())),
        );
        assert_eq!(cancelled.unwrap_err(), "cancel");
        assert!(cache.entries.lock().unwrap().iter().all(Option::is_none));
        let builds = Cell::new(0);
        acquire(&cache, &owner, request(), &builds);
        for at in [1, 2] {
            let mut checks = 0;
            let cancelled = cache.get_or_build(
                &owner,
                request(),
                || {
                    checks += 1;
                    if checks == at { Err("cancel") } else { Ok(()) }
                },
                || Err("hit must not build"),
            );
            assert_eq!(cancelled.unwrap_err(), "cancel");
        }
        assert!(acquire(&cache, &owner, request(), &builds).1);
        assert_eq!(builds.get(), 1);
    }

    #[test]
    fn contention_preserves_hits_and_retention_and_checks_cancellation() {
        let cache = ProjectionCache::new(CACHE_BYTES);
        let owner = Arc::new(());
        let builds = Cell::new(0);
        let retained = acquire(&cache, &owner, request(), &builds).0;
        let mut guard = Some(cache.entries.lock().unwrap());
        let mut polls = 0;
        let (reused, hit) = cache
            .get_or_build(
                &owner,
                request(),
                || {
                    polls += 1;
                    if polls == 2 {
                        drop(guard.take());
                    }
                    Ok::<_, &'static str>(())
                },
                || Err("a contended hit must not rebuild"),
            )
            .unwrap();
        assert!(hit);
        assert!(retained.shares_cache_with(&reused));
        assert_eq!(polls, 3);
        assert_eq!(builds.get(), 1);

        let guard = cache.entries.lock().unwrap();
        let mut polls = 0;
        let cancelled = cache.lookup_controlled(&owner, request(), &mut || {
            polls += 1;
            Err("cancelled while waiting")
        });
        assert_eq!(cancelled.unwrap_err(), "cancelled while waiting");
        assert_eq!(polls, 1);
        drop(guard);

        let mut changed = request();
        changed.binding.as_of = CommitSeq(0);
        let graph = graph(changed);
        let mut guard = Some(cache.entries.lock().unwrap());
        let mut polls = 0;
        cache
            .remember_graph(&owner, changed, &graph, (), 0, &mut || {
                polls += 1;
                drop(guard.take());
                // The cache itself owns no guard while calling live control.
                assert!(cache.entries.try_lock().is_ok());
                Ok::<_, &'static str>(())
            })
            .unwrap();
        assert_eq!(polls, 1);
        assert!(acquire(&cache, &owner, changed, &builds).1);
    }

    #[test]
    fn builders_and_checkpoints_run_without_the_cache_lock() {
        let cache = ProjectionCache::new(CACHE_BYTES);
        let owner = Arc::new(());
        let changed = request();
        let (_, hit) = cache
            .get_or_build(
                &owner,
                changed,
                || {
                    assert!(cache.entries.try_lock().is_ok());
                    Ok::<_, &'static str>(())
                },
                || {
                    assert!(cache.entries.try_lock().is_ok());
                    Ok(graph(changed))
                },
            )
            .unwrap();
        assert!(!hit);
    }

    #[test]
    fn poisoned_metadata_recovers_without_disabling_complete_entries() {
        let cache = ProjectionCache::new(CACHE_BYTES);
        let owner = Arc::new(());
        let builds = Cell::new(0);
        let retained = acquire(&cache, &owner, request(), &builds).0;
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = cache.entries.lock().unwrap();
            panic!("interrupted metadata holder");
        }));
        assert!(poisoned.is_err());
        let (reused, hit) = acquire(&cache, &owner, request(), &builds);
        assert!(hit);
        assert!(retained.shares_cache_with(&reused));
        assert_eq!(builds.get(), 1);
        assert!(!cache.entries.is_poisoned());
    }

    #[test]
    fn copy_on_write_dissociates_a_cached_weak_snapshot_identity() {
        let cache = ProjectionCache::new(CACHE_BYTES);
        let mut owner = Arc::new(1_u64);
        let key = request();
        let first = cache
            .get_or_build(&owner, key, || Ok::<_, &'static str>(()), || Ok(graph(key)))
            .unwrap()
            .0;
        let old = Arc::downgrade(&owner);
        *Arc::make_mut(&mut owner) = 2;
        assert!(old.upgrade().is_none());
        let (second, hit) = cache
            .get_or_build(&owner, key, || Ok::<_, &'static str>(()), || Ok(graph(key)))
            .unwrap();
        assert!(!hit);
        assert!(!first.shares_cache_with(&second));
    }
}
