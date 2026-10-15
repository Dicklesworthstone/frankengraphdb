//! Commit-fed, opened-database-local Beacon registrations.
//!
//! The database owns every live registration and publishes it only after the
//! authoritative graph generation is healthy. Maintenance refusal fences that
//! registration; it never changes the outcome of an already durable commit.

use super::{
    Cancel, Database, Error, Meter, Options, PinnedIndex, RefreshReport, ResidentIndex, Snapshot,
    complete,
};
use asupersync::fs::Vfs;
use fgdb_beacon::expansion::ExpansionSpec;
use fgdb_beacon::read::{ReadPolicy, Rows, Search};
use fgdb_beacon::{BeaconError, GraphHybridHit, GraphHybridQuery, WorkControl};
use fgdb_delta_types::{LogicalDeltaBatch, RelationId};
use fgdb_types::{CommitCx, CommitSeq, QueryCx, VId};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::Arc;

/// Maximum live registrations in one opened database. Vacant slots are reused
/// without retaining index data or aliasing old handles. Each registered index
/// additionally retains its own configured document/text/vector/segment caps.
/// This count is not a byte-memory quota or a bound on caller-held old pins.
pub const MAX_MAINTAINED_BEACON_INDEXES: usize = 64;

/// Opaque identity of one database-owned registration, with no standalone
/// search or mutation authority. Cloning it does not pin an index generation.
/// Removing a registration invalidates every clone, even if its slot is reused.
#[derive(Clone)]
pub struct MaintainedBeaconIndex {
    owner: Arc<()>,
    registration: Arc<()>,
    slot: usize,
}

impl core::fmt::Debug for MaintainedBeaconIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MaintainedBeaconIndex([REDACTED])")
    }
}

/// First refused committed tick. Its cause is retained until explicit rebuild
/// or removal; later writes do not overwrite it with a secondary delta gap.
#[derive(Clone, Debug)]
pub struct MaintainedBeaconFailure {
    pub at: CommitSeq,
    pub cause: Arc<Error>,
}

/// Privileged maintenance diagnostics, without reading or rebuilding the
/// corpus. A healthy current generation has no failure and equal sequences.
#[derive(Clone, Debug)]
pub struct MaintainedBeaconStatus {
    pub source_sequence: CommitSeq,
    pub database_sequence: CommitSeq,
    pub last_refresh: RefreshReport,
    pub failure: Option<MaintainedBeaconFailure>,
}

#[derive(Debug)]
pub enum MaintainedBeaconError {
    Resident(Error),
    ForeignHandle,
    UnknownHandle,
    RegistrationLimit {
        limit: usize,
    },
    /// Registration cannot reinterpret an explicitly historical source as a
    /// current generation. Use the ordinary resident API for historical builds.
    NotCurrent {
        requested: CommitSeq,
        current: CommitSeq,
    },
    Stale {
        source: CommitSeq,
        current: CommitSeq,
    },
    Unavailable {
        source: CommitSeq,
        current: CommitSeq,
        failure: MaintainedBeaconFailure,
    },
}

impl From<Error> for MaintainedBeaconError {
    fn from(error: Error) -> Self {
        Self::Resident(error)
    }
}

impl core::fmt::Display for MaintainedBeaconError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Resident(error) => error.fmt(f),
            Self::ForeignHandle => {
                f.write_str("Beacon registration belongs to another opened database")
            }
            Self::UnknownHandle => f.write_str("unknown or removed Beacon registration"),
            Self::RegistrationLimit { limit } => {
                write!(f, "Beacon registration limit exceeded ({limit})")
            }
            Self::NotCurrent { requested, current } => write!(
                f,
                "Beacon registration requires current sequence {current:?}, requested {requested:?}"
            ),
            Self::Stale { source, current } => write!(
                f,
                "Beacon registration at {source:?} is stale for current sequence {current:?}"
            ),
            Self::Unavailable {
                source,
                current,
                failure,
            } => write!(
                f,
                "Beacon registration at {source:?} is unavailable for {current:?}; maintenance at {:?}: {}",
                failure.at, failure.cause
            ),
        }
    }
}

impl core::error::Error for MaintainedBeaconError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Resident(error) => Some(error),
            Self::Unavailable { failure, .. } => Some(failure.cause.as_ref()),
            _ => None,
        }
    }
}

struct Entry {
    registration: Arc<()>,
    resident: ResidentIndex,
    maintenance: ReadPolicy,
    last_refresh: RefreshReport,
    failure: Option<MaintainedBeaconFailure>,
}

/// Bounded slots, rather than an append-only history of removed registrations.
#[derive(Default)]
pub(crate) struct Registry {
    entries: Vec<Option<Entry>>,
}

impl Registry {
    pub(crate) fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    fn vacant_slot(&self) -> Result<usize, MaintainedBeaconError> {
        if let Some(slot) = self.entries.iter().position(Option::is_none) {
            return Ok(slot);
        }
        if self.entries.len() == MAX_MAINTAINED_BEACON_INDEXES {
            return Err(MaintainedBeaconError::RegistrationLimit {
                limit: MAX_MAINTAINED_BEACON_INDEXES,
            });
        }
        Ok(self.entries.len())
    }

    fn entry(
        &self,
        owner: &Arc<()>,
        handle: &MaintainedBeaconIndex,
    ) -> Result<&Entry, MaintainedBeaconError> {
        if !Arc::ptr_eq(owner, &handle.owner) {
            return Err(MaintainedBeaconError::ForeignHandle);
        }
        self.entries
            .get(handle.slot)
            .and_then(Option::as_ref)
            .filter(|entry| Arc::ptr_eq(&entry.registration, &handle.registration))
            .ok_or(MaintainedBeaconError::UnknownHandle)
    }

    /// Called only after the database publishes its complete healthy snapshot.
    /// Every healthy sibling gets its own maintenance allowance. No failure is
    /// propagated into the durable write result, and no stale data is relabeled.
    pub(crate) fn publish(
        &mut self,
        cx: &CommitCx,
        snapshot: &Arc<Snapshot>,
        batch: &LogicalDeltaBatch,
    ) {
        for entry in self.entries.iter_mut().flatten() {
            if entry.failure.is_some() {
                continue;
            }
            let through = batch.commit_seq();
            // A same-cut physical republish has no new logical effect. Its old
            // pin still answers the identical cut without changing topology.
            if entry.resident.source_sequence == through && snapshot.frontier == through {
                continue;
            }
            let work = RefCell::new(Meter::new(entry.maintenance.max_work_units, |_| {
                cx.checkpoint()
            }));
            let result = (|| {
                work.borrow_mut().charge(1)?;
                let after = entry.resident.source_sequence;
                if snapshot.frontier != through || after.0.checked_add(1) != Some(through.0) {
                    return Err(Error::IncompleteDelta { after, through });
                }
                let mut affected = BTreeSet::new();
                let mut scratch = 0;
                entry.resident.collect_affected(
                    batch,
                    &mut affected,
                    &mut scratch,
                    entry.maintenance,
                    &work,
                )?;
                entry.resident.apply_affected(
                    snapshot,
                    through,
                    affected,
                    scratch,
                    entry.maintenance,
                    &work,
                )
            })();
            match complete(work.into_inner(), result) {
                Ok(report) => entry.last_refresh = report,
                Err(error) => {
                    entry.failure = Some(MaintainedBeaconFailure {
                        at: through,
                        cause: Arc::new(error),
                    });
                }
            }
        }
    }
}

fn checkpoint(cx: &QueryCx) -> Result<(), MaintainedBeaconError> {
    cx.checkpoint()
        .map_err(|error: Cancel| Error::Interrupted(error).into())
}

impl<V: Vfs + Clone> Database<V> {
    /// Build and register a current text/vector generation. Every subsequent
    /// successfully published commit feeds this definition synchronously from
    /// its canonical delta; applications do not need to call refresh.
    ///
    /// `options.policy` admits the initial build. `maintenance` is a separate
    /// per-commit allowance, including no-effect commits. A failed tick fences
    /// only this index; it never aborts the graph commit. Rebuild is explicit.
    /// At most MAX_MAINTAINED_BEACON_INDEXES registrations are retained. Each
    /// preserves its configured Beacon resource bounds; no spill is provided.
    ///
    /// This privileged, opened-handle-local registry is not a durable catalog,
    /// subscription or authority grant. Reopening starts with no registrations.
    pub fn register_beacon_index(
        &mut self,
        cx: &QueryCx,
        options: &Options,
        maintenance: ReadPolicy,
    ) -> Result<MaintainedBeaconIndex, MaintainedBeaconError> {
        self.register_beacon_internal(cx, options, maintenance, false)
    }

    /// Register the same maintained retrieval lanes together with their native
    /// graph source. Edge-only and no-effect ticks advance the coherent source
    /// pin and sequence without rebuilding unchanged text/vector segments.
    pub fn register_beacon_graph_index(
        &mut self,
        cx: &QueryCx,
        options: &Options,
        maintenance: ReadPolicy,
    ) -> Result<MaintainedBeaconIndex, MaintainedBeaconError> {
        self.register_beacon_internal(cx, options, maintenance, true)
    }

    fn register_beacon_internal(
        &mut self,
        cx: &QueryCx,
        options: &Options,
        maintenance: ReadPolicy,
        graph: bool,
    ) -> Result<MaintainedBeaconIndex, MaintainedBeaconError> {
        cx.with_restriction(|| {
            checkpoint(cx)?;
            let current = self.frontier().map_err(Error::Source)?;
            if let Some(requested) = options.as_of
                && requested != current
            {
                return Err(MaintainedBeaconError::NotCurrent { requested, current });
            }
            let slot = self.maintained_beacon.vacant_slot()?;
            // Reserve the bounded slot before building index data. A failed
            // registration can retain vector capacity but never a live entry.
            if slot == self.maintained_beacon.entries.len() {
                self.maintained_beacon.entries.try_reserve(1).map_err(|_| {
                    Error::Index(BeaconError::ResourceLimit {
                        resource: "Beacon registration allocation",
                        limit: MAX_MAINTAINED_BEACON_INDEXES,
                    })
                })?;
            }
            let resident = self.prepare_beacon_index_internal(cx, options, graph)?;
            let registration = Arc::new(());
            let handle = MaintainedBeaconIndex {
                owner: Arc::clone(&self.handle_owner),
                registration: Arc::clone(&registration),
                slot,
            };
            let entry = Entry {
                registration,
                resident,
                maintenance,
                last_refresh: RefreshReport {
                    from: current,
                    through: current,
                    commits: 0,
                    touched_vertices: 0,
                },
                failure: None,
            };
            checkpoint(cx)?;
            if slot == self.maintained_beacon.entries.len() {
                self.maintained_beacon.entries.push(Some(entry));
            } else {
                self.maintained_beacon.entries[slot] = Some(entry);
            }
            Ok(handle)
        })
    }

    /// Return maintenance state even when this registration is fenced. The
    /// database itself must still be readable; this never blesses an ambiguous
    /// durable outcome or reads the retained corpus for diagnostics.
    pub fn beacon_index_status(
        &self,
        cx: &QueryCx,
        handle: &MaintainedBeaconIndex,
    ) -> Result<MaintainedBeaconStatus, MaintainedBeaconError> {
        cx.with_restriction(|| {
            checkpoint(cx)?;
            let current = self.frontier().map_err(Error::Source)?;
            let entry = self.maintained_beacon.entry(&self.handle_owner, handle)?;
            let status = MaintainedBeaconStatus {
                source_sequence: entry.resident.source_sequence,
                database_sequence: current,
                last_refresh: entry.last_refresh,
                failure: entry.failure.clone(),
            };
            checkpoint(cx)?;
            Ok(status)
        })
    }

    /// Pin only a healthy generation at the database's current publication.
    /// A refused or skipped tick never returns an older answer as current.
    /// The returned PinnedIndex explicitly remains at that sequence after any
    /// later commit, rebuild, unregister, compaction or database drop.
    pub fn beacon_index_snapshot(
        &self,
        cx: &QueryCx,
        handle: &MaintainedBeaconIndex,
    ) -> Result<PinnedIndex, MaintainedBeaconError> {
        cx.with_restriction(|| {
            checkpoint(cx)?;
            let current = self.frontier().map_err(Error::Source)?;
            let entry = self.maintained_beacon.entry(&self.handle_owner, handle)?;
            let source = entry.resident.source_sequence;
            if let Some(failure) = &entry.failure {
                return Err(MaintainedBeaconError::Unavailable {
                    source,
                    current,
                    failure: failure.clone(),
                });
            }
            if source != current {
                return Err(MaintainedBeaconError::Stale { source, current });
            }
            let pin = entry.resident.snapshot();
            checkpoint(cx)?;
            Ok(pin)
        })
    }

    /// Search one current registered generation with a fresh read allowance.
    pub fn search_beacon_index(
        &self,
        cx: &QueryCx,
        handle: &MaintainedBeaconIndex,
        query: Search<'_>,
        policy: ReadPolicy,
    ) -> Result<Rows, MaintainedBeaconError> {
        self.beacon_index_snapshot(cx, handle)?
            .search(cx, query, policy)
            .map_err(Into::into)
    }

    /// Restrict a current maintained generation's endpoints before candidate
    /// limits. Owner, health and exact-frontier checks precede the predicate;
    /// reject-all cannot hide a failed maintenance tick or select a stale pin.
    /// No implicit rebuild occurs. This API remains privileged, as does
    /// [`PinnedIndex::search_filtered`]; eligibility grants no capability.
    pub fn search_beacon_index_filtered(
        &self,
        cx: &QueryCx,
        handle: &MaintainedBeaconIndex,
        query: Search<'_>,
        policy: ReadPolicy,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Rows, MaintainedBeaconError> {
        self.beacon_index_snapshot(cx, handle)?
            .search_filtered(cx, query, policy, eligible)
            .map_err(Into::into)
    }

    /// Graph/text/vector search over one current, coherent registered pin.
    pub fn search_beacon_graph_index(
        &self,
        cx: &QueryCx,
        handle: &MaintainedBeaconIndex,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
    ) -> Result<Vec<GraphHybridHit>, MaintainedBeaconError> {
        self.beacon_index_snapshot(cx, handle)?
            .search_graph(cx, query, expansion, policy)
            .map_err(Into::into)
    }

    /// Explicitly rebuild the same frozen definition at the current published
    /// cut, optionally changing its build and future maintenance allowances.
    /// A new approximate topology is a new generation; old pins are unchanged.
    /// Failure leaves the previous generation, fence and policies untouched.
    pub fn rebuild_beacon_index(
        &mut self,
        cx: &QueryCx,
        handle: &MaintainedBeaconIndex,
        build: ReadPolicy,
        maintenance: ReadPolicy,
    ) -> Result<CommitSeq, MaintainedBeaconError> {
        cx.with_restriction(|| {
            checkpoint(cx)?;
            let current = self.frontier().map_err(Error::Source)?;
            let entry = self.maintained_beacon.entry(&self.handle_owner, handle)?;
            let mut options = (*entry.resident.definition).clone();
            options.as_of = Some(current);
            options.policy = build;
            let graph = entry.resident.graph_source.is_some();
            let resident = self.prepare_beacon_index_internal(cx, &options, graph)?;
            let replacement = Entry {
                registration: Arc::clone(&entry.registration),
                resident,
                maintenance,
                last_refresh: RefreshReport {
                    from: current,
                    through: current,
                    commits: 0,
                    touched_vertices: 0,
                },
                failure: None,
            };
            checkpoint(cx)?;
            self.maintained_beacon.entries[handle.slot] = Some(replacement);
            Ok(current)
        })
    }

    /// Release this registration and invalidate all copies of its handle.
    /// Caller-held PinnedIndexes retain only their explicitly historical state.
    /// Removal needs no readable graph, so a fenced database can still release
    /// resident memory. A foreign/removed handle never removes another entry.
    pub fn unregister_beacon_index(
        &mut self,
        cx: &QueryCx,
        handle: &MaintainedBeaconIndex,
    ) -> Result<(), MaintainedBeaconError> {
        cx.with_restriction(|| {
            checkpoint(cx)?;
            self.maintained_beacon.entry(&self.handle_owner, handle)?;
            checkpoint(cx)?;
            self.maintained_beacon.entries[handle.slot] = None;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

    #[test]
    fn delivery_is_contiguous_idempotent_and_first_failure_is_retained() {
        let ((), report) = run_async_under_lab(0xbeac_1301, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let mut db = Database::open_memory(
                &commit,
                DatabaseKeys::new(
                    [0x61; 32],
                    DatabaseSecurityNamespaceId([0x62; 32]),
                    [0x63; 32],
                ),
            )
            .await
            .unwrap();
            let document = |id| {
                let mut batch = WriteBatch::new(RelationId(1));
                batch.create_vertex(
                    VId(id),
                    vec![],
                    vec![(
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text("graph").unwrap(),
                    )],
                );
                batch
            };
            let first = db.write(&commit, document(1)).await.unwrap();
            let original = db
                .prepare_beacon_index(&query, &Options::text(PropertyKeyId(1)))
                .unwrap();
            let pin = original.snapshot();
            let make_registry = |resident| Registry {
                entries: vec![Some(Entry {
                    registration: Arc::new(()),
                    resident,
                    maintenance: ReadPolicy::default(),
                    last_refresh: RefreshReport {
                        from: first,
                        through: first,
                        commits: 0,
                        touched_vertices: 0,
                    },
                    failure: None,
                })],
            };
            // These isolated registries let the test deliver real committed
            // batches out of order without changing the product commit hook.
            let mut complete = make_registry(original.clone());
            let mut gapped = make_registry(original);
            let second = db.write(&commit, document(2)).await.unwrap();
            let second_snapshot = Arc::clone(&db.snapshot);
            let second_batch = second_snapshot.delta_index.get(second).unwrap();
            complete.publish(&commit, &second_snapshot, second_batch);
            let entry = complete.entries[0].as_ref().unwrap();
            assert_eq!(entry.resident.source_sequence(), second);
            assert_eq!(entry.resident.snapshot().stats().documents, 2);
            assert_eq!(entry.last_refresh.touched_vertices, 1);
            let accepted = entry.last_refresh;
            complete.entries[0]
                .as_mut()
                .unwrap()
                .maintenance
                .max_work_units = 0;
            complete.publish(&commit, &second_snapshot, second_batch);
            let entry = complete.entries[0].as_ref().unwrap();
            assert!(entry.failure.is_none());
            assert_eq!(entry.last_refresh, accepted);

            let third = db.write(&commit, document(3)).await.unwrap();
            let third_batch = db.snapshot.delta_index.get(third).unwrap();
            gapped.publish(&commit, &db.snapshot, third_batch);
            let entry = gapped.entries[0].as_ref().unwrap();
            assert_eq!(entry.resident.source_sequence(), first);
            assert_eq!(entry.resident.snapshot().stats().documents, 1);
            let failure = entry.failure.as_ref().unwrap();
            assert_eq!(failure.at, third);
            assert!(matches!(
                failure.cause.as_ref(),
                Error::IncompleteDelta { after, through } if *after == first && *through == third
            ));
            let cause = Arc::clone(&failure.cause);
            // A later delivery of the missing tick cannot silently revive the
            // registration or replace its first actionable refusal.
            gapped.publish(&commit, &second_snapshot, second_batch);
            let entry = gapped.entries[0].as_ref().unwrap();
            assert!(Arc::ptr_eq(&cause, &entry.failure.as_ref().unwrap().cause));
            assert_eq!(entry.resident.source_sequence(), first);
            assert_eq!(pin.source_sequence(), first);
            assert_eq!(pin.stats().documents, 1);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
