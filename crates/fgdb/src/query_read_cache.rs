//! Read acceleration belongs to one opened database, across its generations.
//! Entries identify an exact snapshot allocation and retain only a weak pin.

use super::authorized::{ADMITTED_BYTES, AdmittedCache, AuthorizedProjectionCache};
use super::prism::{PROJECTION_CACHE_BYTES, ProjectionCache};
use crate::{Database, EmbeddedReadView, Snapshot};
use asupersync::fs::Vfs;
use fgdb_types::QueryCx;

#[path = "query_read_cache/live.rs"]
pub(crate) mod live;
use live::LiveCache;

/// Independent retained-memory allowances for an opened database's read
/// accelerators. Independently opening a database creates a fresh owner with
/// these defaults; commits, compaction, authoritative recovery of a consumed
/// handle and every pinned view keep the existing owner and policy.
///
/// These bound charged vector capacities and reserved lazy indexes, not process
/// RSS. Source snapshots, active callers' clones, concurrent builders and
/// allocator metadata are outside the retained-cache allowance. Each cache
/// additionally keeps at most eight entries. Zero explicitly disables retention
/// for that cache; it does not disable the underlying query functionality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadCacheLimits {
    pub projection_bytes: usize,
    pub admission_bytes: usize,
    pub authorized_projection_bytes: usize,
}

impl Default for ReadCacheLimits {
    fn default() -> Self {
        Self {
            projection_bytes: PROJECTION_CACHE_BYTES,
            admission_bytes: ADMITTED_BYTES,
            authorized_projection_bytes: PROJECTION_CACHE_BYTES,
        }
    }
}

/// Retained state for one read accelerator after reclaiming dead snapshots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadCacheUsage {
    pub max_bytes: usize,
    pub retained_bytes: usize,
    pub entries: usize,
}

/// Trusted-host diagnostics for the entire opened database, including entries
/// belonging to older generations that still have a live reader.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReadCacheStatus {
    pub projections: ReadCacheUsage,
    pub admissions: ReadCacheUsage,
    pub authorized_projections: ReadCacheUsage,
}

#[derive(Debug)]
pub(crate) struct ReadCaches {
    pub(crate) projections: ProjectionCache<Snapshot>,
    pub(crate) admitted: AdmittedCache,
    pub(crate) authorized: AuthorizedProjectionCache,
}

impl Default for ReadCaches {
    fn default() -> Self {
        let limits = ReadCacheLimits::default();
        Self {
            projections: LiveCache::new(limits.projection_bytes),
            admitted: LiveCache::new(limits.admission_bytes),
            authorized: LiveCache::new(limits.authorized_projection_bytes),
        }
    }
}

impl ReadCaches {
    /// Acquire the three short metadata sections together. If any is busy,
    /// release every acquired guard BEFORE invoking the caller's checkpoint.
    /// Thus a policy update is all-or-nothing even when cancellation races it.
    fn inspect(
        &self,
        cx: &QueryCx,
        limits: Option<ReadCacheLimits>,
    ) -> Result<ReadCacheStatus, Box<asupersync::error::Error>> {
        cx.checkpoint()?;
        loop {
            let status = (|| {
                let mut projections = self.projections.try_lock()?;
                let mut admitted = self.admitted.try_lock()?;
                let mut authorized = self.authorized.try_lock()?;
                let (old_projections, old_admitted, old_authorized) = match limits {
                    Some(limits) => (
                        self.projections
                            .set_limit(&mut projections, limits.projection_bytes),
                        self.admitted
                            .set_limit(&mut admitted, limits.admission_bytes),
                        self.authorized
                            .set_limit(&mut authorized, limits.authorized_projection_bytes),
                    ),
                    None => (
                        LiveCache::prune(&mut projections),
                        LiveCache::prune(&mut admitted),
                        LiveCache::prune(&mut authorized),
                    ),
                };
                let status = ReadCacheStatus {
                    projections: self.projections.usage(&projections),
                    admissions: self.admitted.usage(&admitted),
                    authorized_projections: self.authorized.usage(&authorized),
                };
                drop(authorized);
                drop(admitted);
                drop(projections);
                // Potentially large vectors are released outside every lock.
                drop(old_authorized);
                drop(old_admitted);
                drop(old_projections);
                Some(status)
            })();
            if let Some(status) = status {
                return Ok(status);
            }
            cx.checkpoint()?;
            std::thread::yield_now();
        }
    }
}

impl<V: Vfs> Database<V> {
    /// Inspect this opened database's retained read acceleration. Other
    /// databases cannot consume its entries or byte allowances.
    pub fn read_cache_status(
        &self,
        cx: &QueryCx,
    ) -> Result<ReadCacheStatus, Box<asupersync::error::Error>> {
        self.snapshot.read_caches.inspect(cx, None)
    }

    /// Set explicit host retention policy for all generations and pinned views
    /// of this opened database. A lower limit releases higher occupied slots
    /// until the retained total fits. Existing caller-held values remain valid; later
    /// queries build and replay through their normal correctness boundaries.
    pub fn set_read_cache_limits(
        &self,
        cx: &QueryCx,
        limits: ReadCacheLimits,
    ) -> Result<ReadCacheStatus, Box<asupersync::error::Error>> {
        self.snapshot.read_caches.inspect(cx, Some(limits))
    }
}

impl EmbeddedReadView {
    /// Inspect the cache owner shared by this view and its opened database,
    /// including older or newer live generations of that same owner.
    pub fn read_cache_status(
        &self,
        cx: &QueryCx,
    ) -> Result<ReadCacheStatus, Box<asupersync::error::Error>> {
        self.snapshot.read_caches.inspect(cx, None)
    }

    /// Configure retention for a read-only opened view, or for the owner shared
    /// with this view's writer and other generations. Like the raw view itself,
    /// this is trusted-host authority, not an authorized-session operation.
    pub fn set_read_cache_limits(
        &self,
        cx: &QueryCx,
        limits: ReadCacheLimits,
    ) -> Result<ReadCacheStatus, Box<asupersync::error::Error>> {
        self.snapshot.read_caches.inspect(cx, Some(limits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::PurposeContexts;
    use fgdb_types::context::SimulationCheckpointProbe;
    use std::sync::Arc;

    #[test]
    fn cancelling_a_contended_policy_update_keeps_every_previous_limit() {
        let ((), report) = run_async_under_lab(0x6dca_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cache = ReadCaches::default();
            let held = cache.admitted.try_lock().unwrap();
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(2)));
            let cx = contexts.query().with_checkpoint_probe(Arc::clone(&probe));
            assert!(
                cache
                    .inspect(
                        &cx,
                        Some(ReadCacheLimits {
                            projection_bytes: 1,
                            admission_bytes: 2,
                            authorized_projection_bytes: 3,
                        })
                    )
                    .is_err()
            );
            assert_eq!(probe.calls(), 2);
            assert!(cache.projections.try_lock().is_some());
            let defaults = ReadCacheLimits::default();
            assert_eq!(cache.projections.max_bytes(), defaults.projection_bytes);
            assert_eq!(cache.admitted.max_bytes(), defaults.admission_bytes);
            assert_eq!(
                cache.authorized.max_bytes(),
                defaults.authorized_projection_bytes
            );
            drop(held);
            let updated = cache
                .inspect(
                    &contexts.query(),
                    Some(ReadCacheLimits {
                        projection_bytes: 1,
                        admission_bytes: 2,
                        authorized_projection_bytes: 3,
                    }),
                )
                .unwrap();
            assert_eq!(updated.projections.max_bytes, 1);
            assert_eq!(updated.admissions.max_bytes, 2);
            assert_eq!(updated.authorized_projections.max_bytes, 3);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
