//! Bounded reuse of projections from an exact, still-live snapshot allocation.
//!
//! This is an in-process sidecar, not graph storage or a persistent index. A
//! weak owner prevents an entry from pinning an entire database generation;
//! its allocation identity prevents reuse across independently opened handles,
//! even when their root bytes agree. Live entries are never evicted to admit
//! another request. Full, contended or poisoned caches take the ordinary build
//! path. No lock is held while checking cancellation, visiting data or building.
//!
//! An entry may carry an `Extra` beside its graph: the authorized analytics
//! path keeps the live charge sequence its build made, which every reuse
//! spends again (fgdb-0tbrz). That sidecar is its own static, so it has its
//! own entries and an allowance of the same size.
//!
//! `LiveCache` is the same sidecar for any key and value: authorized
//! full-scan admission keeps its admitted records under it (fgdb-61p1n).

use super::{
    EmbeddedReadView, Error, FnxSelection, FnxSourceLimits, ProjectionLimits, ProjectionSpec,
    QueryCx, SnapshotBinding, SnapshotGraphView,
};
use std::borrow::Borrow;
use std::sync::{Arc, Mutex, Weak};

const CACHE_ENTRIES: usize = 8;
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
static CACHE: ProjectionCache<crate::Snapshot> = ProjectionCache::new(CACHE_BYTES);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) binding: SnapshotBinding,
    pub(crate) selection: FnxSelection,
    pub(crate) projection: ProjectionSpec,
    pub(crate) projection_limits: ProjectionLimits,
    pub(crate) source_limits: FnxSourceLimits,
}

struct Entry<Owner, Key, Value> {
    owner: Weak<Owner>,
    key: Key,
    value: Value,
    bytes: usize,
}

/// A bounded sidecar of values derived from still-live snapshot allocations,
/// with the weak-owner, never-evict and never-wait rules in the module docs.
pub(crate) struct LiveCache<Owner, Key, Value> {
    entries: Mutex<[Option<Entry<Owner, Key, Value>>; CACHE_ENTRIES]>,
    max_bytes: usize,
}

/// Projections by request, with an optional `Extra` beside each graph.
pub(crate) type ProjectionCache<Owner, Extra = ()> =
    LiveCache<Owner, Request, (SnapshotGraphView, Extra)>;

impl<Owner, Key: PartialEq, Value: Clone> LiveCache<Owner, Key, Value> {
    pub(crate) const fn new(max_bytes: usize) -> Self {
        Self {
            entries: Mutex::new([const { None }; CACHE_ENTRIES]),
            max_bytes,
        }
    }

    fn prune(entries: &mut [Option<Entry<Owner, Key, Value>>; CACHE_ENTRIES]) {
        for entry in entries {
            if entry
                .as_ref()
                .is_some_and(|entry| entry.owner.strong_count() == 0)
            {
                *entry = None;
            }
        }
    }

    pub(crate) fn lookup(&self, owner: &Arc<Owner>, key: impl Borrow<Key>) -> Option<Value> {
        // Observing a cache must not wait behind another query or escape the
        // caller's cancellation contract through a blocking cache mutex.
        let mut entries = self.entries.try_lock().ok()?;
        Self::prune(&mut entries);
        let owner = Arc::downgrade(owner);
        entries.iter().flatten().find_map(|entry| {
            (entry.owner.ptr_eq(&owner) && &entry.key == key.borrow()).then(|| entry.value.clone())
        })
    }

    /// `bytes` is the retained size charged for `value`.
    pub(crate) fn remember(&self, owner: &Arc<Owner>, key: Key, value: Value, bytes: usize) {
        if bytes > self.max_bytes {
            return;
        }
        let Ok(mut entries) = self.entries.try_lock() else {
            return;
        };
        Self::prune(&mut entries);
        let owner = Arc::downgrade(owner);
        // Two concurrent misses may both complete. Keep the first complete
        // admission, never reserve a slot for an in-flight/failed build.
        if entries
            .iter()
            .flatten()
            .any(|entry| entry.owner.ptr_eq(&owner) && entry.key == key)
        {
            return;
        }
        let total = entries
            .iter()
            .flatten()
            .try_fold(bytes, |total, entry| total.checked_add(entry.bytes));
        if total.is_none_or(|total| total > self.max_bytes) {
            return;
        }
        if let Some(slot) = entries.iter_mut().find(|entry| entry.is_none()) {
            *slot = Some(Entry {
                owner,
                key,
                value,
                bytes,
            });
        }
    }
}

impl<Owner, Extra: Clone> ProjectionCache<Owner, Extra> {
    /// `extra_bytes` is the retained size of `extra`, charged with the graph.
    pub(crate) fn remember_graph(
        &self,
        owner: &Arc<Owner>,
        request: Request,
        graph: &SnapshotGraphView,
        extra: Extra,
        extra_bytes: usize,
    ) {
        if graph.binding() != request.binding || graph.spec() != request.projection {
            return;
        }
        let Some(bytes) = graph.charged_resident_bytes().checked_add(extra_bytes) else {
            return;
        };
        self.remember(owner, request, (graph.clone(), extra), bytes);
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
        if let Some((graph, ())) = self.lookup(owner, request) {
            checkpoint()?;
            return Ok((graph, true));
        }
        let graph = build()?;
        checkpoint()?;
        self.remember_graph(owner, request, &graph, (), 0);
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
    CACHE
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
    fn a_contended_cache_never_waits_or_holds_a_lock_during_build() {
        let cache = ProjectionCache::new(CACHE_BYTES);
        let owner = Arc::new(());
        let builds = Cell::new(0);
        let retained = acquire(&cache, &owner, request(), &builds).0;
        let guard = cache.entries.lock().unwrap();
        let (uncached, hit) = acquire(&cache, &owner, request(), &builds);
        assert!(!hit);
        assert!(!retained.shares_cache_with(&uncached));
        assert_eq!(retained.digest(), uncached.digest());
        drop(guard);
        assert!(acquire(&cache, &owner, request(), &builds).1);
        let mut changed = request();
        changed.binding.as_of = CommitSeq(0);
        let (_, hit) = cache
            .get_or_build(
                &owner,
                changed,
                || Ok::<_, &'static str>(()),
                || {
                    assert!(cache.entries.try_lock().is_ok());
                    Ok(graph(changed))
                },
            )
            .unwrap();
        assert!(!hit);
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
