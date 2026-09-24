//! Reusable, database-derived resident Beacon generations (plan §10).
//!
//! A generation owns a frozen projection and an explicit source sequence.
//! Searching it never samples a newer database frontier or silently rebuilds
//! an approximate topology. Clones retain the same immutable generation.
//!
//! This is a privileged embedded API, like `Database::read_session`, NOT a
//! capability grant. Do not hand it to Warden clients: use the authorized
//! one-shot adapter for them. These resident objects are neither durable
//! IndexDefinition/DerivedIdentity records nor an AnswerContract certificate,
//! and they do not provide disk spill or an independently writable graph.

use super::{Meter, SharedWork, build};
use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, ReadError};
use asupersync::fs::Vfs;
use fgdb_beacon::read::{Projection, ReadOptions, ReadPolicy, Rows, Search};
use fgdb_beacon::{BeaconError, BeaconIndex, IndexMutation, IndexSnapshot, IndexStats, WorkControl};
use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId};
use fgdb_types::{CommitSeq, QueryCx};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

type Cancel = Box<asupersync::error::Error>;
pub type Options = ReadOptions<PropertyKeyId, LabelId>;

#[cfg(test)]
#[path = "resident_tests.rs"]
mod api_tests;

#[derive(Debug)]
pub enum Error {
    Source(ReadError),
    Interrupted(Cancel),
    Index(BeaconError),
    ForeignDatabase,
    BeforeSource {
        source: CommitSeq,
        requested: CommitSeq,
    },
    /// The current resident projection has no tailing law for this family or
    /// schema transition. Rebuild explicitly; never pretend it was irrelevant.
    UnsupportedDelta,
    IncompleteDelta {
        after: CommitSeq,
        through: CommitSeq,
    },
}

impl From<BeaconError> for Error {
    fn from(error: BeaconError) -> Self {
        Self::Index(error)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::Interrupted(error) => error.fmt(f),
            Self::Index(error) => error.fmt(f),
            Self::ForeignDatabase => f.write_str("Beacon index belongs to another opened database"),
            Self::BeforeSource { source, requested } => write!(
                f, "cannot refresh Beacon backwards from {source:?} to {requested:?}"
            ),
            Self::UnsupportedDelta => f.write_str("Beacon projection requires an explicit rebuild after this delta"),
            Self::IncompleteDelta { after, through } => write!(
                f, "Beacon refresh lacks a contiguous delta path after {after:?} through {through:?}"
            ),
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Interrupted(error) => Some(error.as_ref()),
            Self::Index(error) => Some(error),
            _ => None,
        }
    }
}

/// A database-derived resident index. Construction is the only public source
/// of this type: arbitrary documents cannot be paired with a database sequence.
/// The definition is private and owned, not a mutable caller-supplied catalog.
#[derive(Clone)]
pub struct ResidentIndex {
    owner: Arc<()>,
    definition: Arc<Options>,
    source_sequence: CommitSeq,
    index: BeaconIndex,
}

/// A cheap, immutable pin of both lanes, BM25 statistics, and ANN topology at
/// exactly `source_sequence()`. It survives later database writes and drops.
/// A pin does not promise that the corresponding Chronicle history is retained.
#[derive(Clone)]
pub struct PinnedIndex {
    definition: Arc<Options>,
    source_sequence: CommitSeq,
    index: IndexSnapshot,
}

/// One atomic refresh, including no-effect commits. `touched_vertices` counts
/// distinct affected identities, including deletions and vertices leaving the
/// selected label; it is not a scan/work count or a performance certificate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshReport {
    pub from: CommitSeq,
    pub through: CommitSeq,
    pub commits: u64,
    pub touched_vertices: usize,
}

impl core::fmt::Debug for ResidentIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ResidentBeaconIndex")
            .field("source_sequence", &self.source_sequence)
            .field("definition", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl core::fmt::Debug for PinnedIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PinnedBeaconIndex")
            .field("source_sequence", &self.source_sequence)
            .field("definition", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

fn complete<T>(
    meter: Meter<Cancel, impl FnMut(usize) -> Result<(), Cancel>>,
    result: Result<T, Error>,
) -> Result<T, Error> {
    match meter.into_failure() {
        Some(error) => Err(Error::Interrupted(error)),
        None => result,
    }
}

/// Freeze only explicitly enabled lanes. Preparing a two-lane index validates
/// both projections even when its first search will use just one of them.
/// The one-shot adapter's request-specific lane elision is unchanged.
fn definition(options: &Options, work: &mut impl WorkControl) -> Result<Options, BeaconError> {
    work.charge(1)?;
    options.index.validate()?;
    if options.index.text.is_some() && options.projection.text.is_none() {
        return Err(BeaconError::InvalidConfig("text index needs a text property"));
    }
    let vector = if let Some(vector) = &options.index.vector {
        if options.projection.vector.len() != vector.dimensions {
            return Err(BeaconError::InvalidConfig("vector properties must match dimensions"));
        }
        if vector.dimensions > options.index.max_vector_values {
            return Err(BeaconError::ResourceLimit {
                resource: "vector projection", limit: options.index.max_vector_values,
            });
        }
        work.charge(vector.dimensions)?;
        options.projection.vector.clone()
    } else {
        Vec::new()
    };
    Ok(Options {
        as_of: None,
        vertex_label: options.vertex_label,
        projection: Projection {
            text: options.index.text.as_ref().and(options.projection.text),
            vector,
        },
        index: options.index.clone(),
        policy: options.policy,
    })
}

fn search(
    index: &IndexSnapshot,
    definition: &Options,
    cx: &QueryCx,
    query: Search<'_>,
    policy: ReadPolicy,
) -> Result<Rows, Error> {
    cx.with_restriction(|| {
        cx.checkpoint().map_err(Error::Interrupted)?;
        let work = RefCell::new(Meter::new(policy.max_work_units, |_| cx.checkpoint()));
        let result = (|| {
            work.borrow_mut().charge(1)?;
            if query.k() > policy.max_result_rows {
                return Err(BeaconError::ResourceLimit {
                    resource: "result rows", limit: policy.max_result_rows,
                }.into());
            }
            let (vector, text) = query.lanes();
            if vector && definition.index.vector.is_none() {
                return Err(BeaconError::Disabled("vector").into());
            }
            if text && definition.index.text.is_none() {
                return Err(BeaconError::Disabled("text").into());
            }
            query.validate(&definition.index, &mut SharedWork(&work))?;
            let rows = query.execute(index, &mut SharedWork(&work))?;
            // Empty and zero-k searches still pass final live admission.
            work.borrow_mut().charge(1)?;
            Ok(rows)
        })();
        complete(work.into_inner(), result)
    })
}

impl ResidentIndex {
    #[must_use]
    pub fn source_sequence(&self) -> CommitSeq {
        self.source_sequence
    }

    #[must_use]
    pub fn snapshot(&self) -> PinnedIndex {
        PinnedIndex {
            definition: Arc::clone(&self.definition),
            source_sequence: self.source_sequence,
            index: self.index.snapshot(),
        }
    }

    /// Explicit in-process owner check; equal keys/sequence numbers or a reopen
    /// do not transfer an index to another opened database lifetime.
    #[must_use]
    pub fn belongs_to<V: Vfs>(&self, database: &Database<V>) -> bool {
        Arc::ptr_eq(&self.owner, &database.handle_owner)
    }

    pub fn search(&self, cx: &QueryCx, query: Search<'_>, policy: ReadPolicy) -> Result<Rows, Error> {
        search(&self.index.snapshot(), &self.definition, cx, query, policy)
    }

    /// Catch up through one explicitly selected native sequence (None = live).
    /// The complete retained Chronicle delta window identifies affected VIds;
    /// the SAME native historical-winner visitor as one-shot GQL/Beacon supplies
    /// their final rows. Repeated changes coalesce at the target cut, not at the
    /// writer's newer frontier. No intermediate search generation is claimed.
    ///
    /// Changed documents enter Beacon's existing segmented atomic apply path.
    /// Both lanes, corpus statistics and source_sequence advance together, only
    /// after final live admission. Errors, cancellation and unwinding leave the
    /// previous index and every pin unchanged. Retired history, a foreign owner,
    /// a future/backwards target, and unsupported deltas fail closed; no hidden
    /// rebuild or truncated tail is substituted. Explicit prepare rebuilds.
    ///
    /// This is synchronous, caller-driven resident maintenance, not a durable
    /// subscription or a commit hook. Changed IDs and borrowed winners are
    /// bounded; source winner selection can still visit the native patch table,
    /// and Beacon copies live metadata while sharing index segments. No O(delta)
    /// total-work, spill, full index-descriptor or ANN-equivalence claim is made.
    pub fn refresh<V: Vfs + Clone>(
        &mut self,
        cx: &QueryCx,
        database: &Database<V>,
        through: Option<CommitSeq>,
        policy: ReadPolicy,
    ) -> Result<RefreshReport, Error> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let work = RefCell::new(Meter::new(policy.max_work_units, |_| cx.checkpoint()));
            let result = self.refresh_with_work(database, through, policy, &work);
            complete(work.into_inner(), result)
        })
    }

    // Shared verbatim with refusal-cut tests. All effects before the final
    // assignments belong to local candidates, never to the retained index.
    fn refresh_with_work<V: Vfs + Clone>(
        &mut self,
        database: &Database<V>,
        through: Option<CommitSeq>,
        policy: ReadPolicy,
        work: &RefCell<impl WorkControl>,
    ) -> Result<RefreshReport, Error> {
        work.borrow_mut().charge(1)?;
        if !self.belongs_to(database) {
            return Err(Error::ForeignDatabase);
        }
        // Even a no-op cannot bless a handle fenced by an ambiguous write.
        let frontier = database.frontier().map_err(Error::Source)?;
        let through = through.unwrap_or(frontier);
        database.snapshot.check_frontier(through).map_err(Error::Source)?;
        let from = self.source_sequence;
        if through.0 < from.0 {
            return Err(Error::BeforeSource { source: from, requested: through });
        }
        let mut scratch = 0usize;
        let mut affected = BTreeSet::new();
        if through != from {
            let mut batches = database.delta_since(from).map_err(Error::Source)?;
            let mut after = from;
            // Do not even poll the first batch newer than the requested cut.
            while after.0 < through.0 {
                work.borrow_mut().charge(1)?;
                let batch = batches.next().ok_or(Error::IncompleteDelta { after, through })?;
                if after.0.checked_add(1) != Some(batch.commit_seq().0) {
                    return Err(Error::IncompleteDelta { after, through });
                }
                after = batch.commit_seq();
                for coordinate in batch.coordinate_entries() {
                    work.borrow_mut().charge(1)?;
                    if coordinate.schema_transition.is_some() {
                        return Err(Error::UnsupportedDelta);
                    }
                    for row in &coordinate.rows {
                        work.borrow_mut().charge(1)?;
                        let vid = match row {
                            DeltaRow::CreateVertex { vid, .. }
                            | DeltaRow::DeleteVertex { vid, .. }
                            | DeltaRow::LabelMembership { vid, .. }
                            | DeltaRow::Property { elem: ElementId::Vertex(vid), .. } => *vid,
                            DeltaRow::CreateEdge { .. }
                            | DeltaRow::DeleteEdge { .. }
                            | DeltaRow::Property { elem: ElementId::Edge(_), .. } => continue,
                            _ => return Err(Error::UnsupportedDelta),
                        };
                        if !affected.contains(&vid) {
                            let limit = policy.max_staging_rows.min(self.definition.index.max_batch_operations);
                            if affected.len() == limit {
                                return Err(BeaconError::ResourceLimit {
                                    resource: "refresh vertices", limit,
                                }.into());
                            }
                            reserve_scratch(&mut scratch, policy)?;
                            affected.insert(vid);
                        }
                    }
                }
            }
        }
        let report = RefreshReport {
            from,
            through,
            commits: through.0 - from.0,
            touched_vertices: affected.len(),
        };
        let mut next = self.index.clone();
        if !affected.is_empty() {
            let mut winners = BTreeMap::new();
            let mut control = |event| -> Result<(), BeaconError> {
                work.borrow_mut().charge(1)?;
                if matches!(event, SourceEvent::ScratchEntry) {
                    reserve_scratch(&mut scratch, policy)?;
                }
                Ok(())
            };
            source::visit_vertices(&database.snapshot.patches, through, &mut control, |row, control| {
                control(SourceEvent::Work)?;
                if affected.contains(&row.vid) {
                    work.borrow_mut().charge(row.labels.len())?;
                    if self.definition.vertex_label.is_none_or(|label| row.labels.binary_search(&label).is_ok()) {
                        control(SourceEvent::ScratchEntry)?;
                        winners.insert(row.vid, row);
                    }
                }
                Ok(())
            })?;
            let mutations = affected.into_iter().map(|vid| {
                work.borrow_mut().charge(1)?;
                match winners.get(&vid) {
                    Some(row) => self.definition.projection.project(
                        vid,
                        &self.definition.index,
                        |key| row.props.binary_search_by_key(&key, |(key, _)| *key)
                            .ok().map(|slot| &row.props[slot].1),
                        &mut SharedWork(work),
                    ).map(IndexMutation::Upsert),
                    // A retired row or a label exit removes BOTH lanes, even
                    // when another affected vertex also enters the corpus.
                    None => Ok(IndexMutation::Delete(vid)),
                }
            });
            next.try_apply_batch(mutations, &mut SharedWork(work))?;
        }
        // The native source sequence is distinct from Beacon's internal Arc
        // generation. Edge-only/no-op commits advance only the source sequence.
        work.borrow_mut().charge(1)?;
        self.index = next;
        self.source_sequence = through;
        Ok(report)
    }
}

fn reserve_scratch(scratch: &mut usize, policy: ReadPolicy) -> Result<(), BeaconError> {
    if *scratch == policy.max_source_scratch {
        return Err(BeaconError::ResourceLimit {
            resource: "refresh scratch entries", limit: policy.max_source_scratch,
        });
    }
    *scratch += 1;
    Ok(())
}

impl PinnedIndex {
    #[must_use]
    pub fn source_sequence(&self) -> CommitSeq {
        self.source_sequence
    }

    /// Privileged corpus counters, not a public capability-filtered diagnostic.
    #[must_use]
    pub fn stats(&self) -> IndexStats {
        self.index.stats()
    }

    /// Reuse this exact generation with a fresh execution allowance. Source
    /// scratch/staging allowances are unused: no graph scan or rebuild occurs.
    /// Exact vector mode remains exact only for the admitted f32 projection;
    /// approximate mode and candidate-limited fusion do not become exact.
    pub fn search(&self, cx: &QueryCx, query: Search<'_>, policy: ReadPolicy) -> Result<Rows, Error> {
        search(&self.index, &self.definition, cx, query, policy)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Build one reusable resident generation from the native historical-winner
    /// visitor. `options.as_of` selects the initial cut; None selects the live
    /// frontier. All enabled lanes share that cut, projection and source domain.
    /// No supplied document list, sequence assertion or alternate graph model
    /// can enter the resulting index. This method grants no Warden authority.
    pub fn prepare_beacon_index(&self, cx: &QueryCx, options: &Options) -> Result<ResidentIndex, Error> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let work = RefCell::new(Meter::new(options.policy.max_work_units, |_| cx.checkpoint()));
            let result = (|| {
                let definition = definition(options, &mut SharedWork(&work))?;
                let frontier = self.frontier().map_err(Error::Source)?;
                let at = options.as_of.unwrap_or(frontier);
                self.snapshot.check_frontier(at).map_err(Error::Source)?;
                let index = build(
                    &self.snapshot, at, &definition, definition.index.clone(), &work,
                    |_| Ok(true), |_| true, |_| true,
                )?;
                let prepared = ResidentIndex {
                    owner: Arc::clone(&self.handle_owner),
                    definition: Arc::new(definition),
                    source_sequence: at,
                    index,
                };
                work.borrow_mut().charge(1)?;
                Ok(prepared)
            })();
            complete(work.into_inner(), result)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, DatabaseState, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_beacon::{DistanceMetric, HnswConfig, TextMatch, VectorSearch};
    use fgdb_delta_types::{LocalDeltaBatchIndex, RelationId};
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x41; 32],
            DatabaseSecurityNamespaceId([0x42; 32]),
            [0x43; 32],
        )
    }

    fn options() -> Options {
        let mut options = Options::text(PropertyKeyId(1));
        options.projection.vector = vec![PropertyKeyId(2)];
        options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
        // A changed document forces the existing compactor, rather than only
        // exercising the cheaper path that adds a segment.
        options.index.max_segments = 1;
        options
    }

    fn document(id: u128) -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::ucs_basic_text("graph").unwrap()),
                (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
            ],
        );
        batch
    }

    fn searches() -> [Search<'static>; 2] {
        [
            Search::Text { query: "graph", k: 8, mode: TextMatch::Any },
            Search::Vector { query: &[0.0], k: 8, mode: VectorSearch::Exact },
        ]
    }

    #[derive(Default)]
    struct Cut {
        calls: usize,
        stop_at: Option<usize>,
        unwind: bool,
    }

    impl WorkControl for Cut {
        fn charge(&mut self, _units: usize) -> Result<(), BeaconError> {
            self.calls += 1;
            if self.stop_at == Some(self.calls) {
                assert!(!self.unwind, "injected resident-index work unwind");
                return Err(BeaconError::Cancelled);
            }
            Ok(())
        }
    }

    #[test]
    fn every_refresh_refusal_cut_preserves_both_lanes_and_allows_retry() {
        let ((), report) = run_async_under_lab(0xbeac_1101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, document(1)).await.unwrap();
            db.write(&commit, document(2)).await.unwrap();
            let definition = options();
            let original = db.prepare_beacon_index(&query, &definition).unwrap();
            let old = original.snapshot();
            let expected_old = searches().map(|search| {
                old.search(&query, search, ReadPolicy::default()).unwrap()
            });

            let mut changes = document(3);
            changes.delete_vertex(VId(2));
            changes.set_vertex_property(
                VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(9)),
            );
            let target = db.write(&commit, changes).await.unwrap();
            let expected_new = searches().map(|search| {
                db.beacon_search(&query, &definition, search).unwrap()
            });
            let trace = RefCell::new(Cut::default());
            let mut success = original.clone();
            success.refresh_with_work(&db, None, ReadPolicy::default(), &trace).unwrap();
            let calls = trace.into_inner().calls;
            assert!(calls > 2, "must reach index construction and publication");
            assert_eq!(success.source_sequence(), target);
            assert_eq!(success.snapshot().stats().segments, 1);
            for (search, expected) in searches().into_iter().zip(&expected_new) {
                assert_eq!(success.search(&query, search, ReadPolicy::default()).unwrap(), *expected);
            }

            // Includes the adapter's FINAL charge, after Beacon accepted the
            // off-side successor. A premature source or Arc assignment fails
            // at that cut even if every inner index failure was atomic.
            for stop_at in 1..=calls {
                let mut candidate = original.clone();
                let work = RefCell::new(Cut { stop_at: Some(stop_at), ..Cut::default() });
                assert!(matches!(
                    candidate.refresh_with_work(&db, None, ReadPolicy::default(), &work),
                    Err(Error::Index(BeaconError::Cancelled))
                ), "cut {stop_at}");
                assert_eq!(work.borrow().calls, stop_at);
                assert_eq!(candidate.source_sequence(), old.source_sequence());
                assert_eq!(candidate.snapshot().stats(), old.stats());
                for (search, expected) in searches().into_iter().zip(&expected_old) {
                    assert_eq!(candidate.search(&query, search, ReadPolicy::default()).unwrap(), *expected);
                }
                candidate.refresh(&query, &db, None, ReadPolicy::default()).unwrap();
                assert_eq!(candidate.source_sequence(), target);
                for (search, expected) in searches().into_iter().zip(&expected_new) {
                    assert_eq!(candidate.search(&query, search, ReadPolicy::default()).unwrap(), *expected);
                }
            }

            for stop_at in [1, calls / 2, calls - 1, calls] {
                let mut candidate = original.clone();
                let work = RefCell::new(Cut { stop_at: Some(stop_at), unwind: true, calls: 0 });
                let panic = catch_unwind(AssertUnwindSafe(|| {
                    candidate.refresh_with_work(&db, None, ReadPolicy::default(), &work)
                }));
                assert!(panic.is_err(), "unwind cut {stop_at}");
                assert_eq!(candidate.source_sequence(), old.source_sequence());
                assert_eq!(candidate.snapshot().stats(), old.stats());
                for (search, expected) in searches().into_iter().zip(&expected_old) {
                    assert_eq!(candidate.search(&query, search, ReadPolicy::default()).unwrap(), *expected);
                }
                candidate.refresh(&query, &db, None, ReadPolicy::default()).unwrap();
                assert_eq!(candidate.source_sequence(), target);
            }
            for (search, expected) in searches().into_iter().zip(&expected_old) {
                assert_eq!(old.search(&query, search, ReadPolicy::default()).unwrap(), *expected);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn retired_and_gapped_windows_never_publish_a_surviving_suffix() {
        let ((), report) = run_async_under_lab(0xbeac_1102, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let first = db.write(&commit, document(1)).await.unwrap();
            let original = db.prepare_beacon_index(&query, &options()).unwrap();
            let second = db.write(&commit, document(2)).await.unwrap();
            let mut boundary = db.prepare_beacon_index(&query, &options()).unwrap();
            let third = db.write(&commit, document(3)).await.unwrap();
            let authoritative = Arc::clone(&db.snapshot);

            // Test-only window surgery retains REAL committed payloads. It
            // models reader admission, not a production retention/GC command.
            Arc::make_mut(&mut db.snapshot).delta_index.retire_prefix(second).unwrap();
            let mut lagging = original.clone();
            assert!(matches!(lagging.refresh(&query, &db, None, ReadPolicy::default()),
                Err(Error::Source(ReadError::DeltaCursorRetired { .. }))));
            assert_eq!(lagging.source_sequence(), first);
            assert_eq!(lagging.snapshot().stats().documents, 1);
            boundary.refresh(&query, &db, None, ReadPolicy::default()).unwrap();
            assert_eq!(boundary.source_sequence(), third);
            assert_eq!(boundary.snapshot().stats().documents, 3);
            // Explicit rebuild remains usable when incremental catch-up is not.
            let rebuilt = db.prepare_beacon_index(&query, &options()).unwrap();
            assert_eq!(rebuilt.source_sequence(), third);
            assert_eq!(rebuilt.snapshot().stats().documents, 3);

            for (sequence, expected_after) in [(third, first), (second, second)] {
                let batch = authoritative.delta_index.get(sequence).unwrap().clone();
                Arc::make_mut(&mut db.snapshot).delta_index =
                    LocalDeltaBatchIndex::from_parts_for_test(first, third, vec![(sequence, batch)]);
                let mut candidate = original.clone();
                assert!(matches!(
                    candidate.refresh(&query, &db, None, ReadPolicy::default()),
                    Err(Error::IncompleteDelta { after, through })
                        if after == expected_after && through == third
                ));
                assert_eq!(candidate.source_sequence(), first);
                assert_eq!(candidate.snapshot().stats(), original.snapshot().stats());
            }
            db.snapshot = authoritative;
            lagging.refresh(&query, &db, None, ReadPolicy::default()).unwrap();
            assert_eq!(lagging.source_sequence(), third);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn unhealthy_owner_cannot_bless_even_an_empty_refresh() {
        let ((), report) = run_async_under_lab(0xbeac_1103, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let frontier = db.write(&commit, document(1)).await.unwrap();
            let mut resident = db.prepare_beacon_index(&query, &options()).unwrap();
            let pinned = resident.snapshot();
            let prior = db.state;
            // Drive the real read fence without claiming disk-fault coverage.
            db.state = DatabaseState::CommitOutcomeUnknown { published_frontier: frontier };
            assert!(matches!(resident.refresh(&query, &db, Some(frontier), ReadPolicy::default()),
                Err(Error::Source(ReadError::CommitOutcomeUnknown { .. }))));
            assert!(matches!(db.prepare_beacon_index(&query, &options()),
                Err(Error::Source(ReadError::CommitOutcomeUnknown { .. }))));
            assert_eq!(resident.source_sequence(), frontier);
            assert_eq!(resident.snapshot().stats(), pinned.stats());
            for search in searches() {
                assert_eq!(resident.search(&query, search, ReadPolicy::default()).unwrap(),
                    pinned.search(&query, search, ReadPolicy::default()).unwrap());
            }
            db.state = prior;
            assert_eq!(resident.refresh(&query, &db, None, ReadPolicy::default()).unwrap().commits, 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn unimplemented_delta_families_and_schema_transitions_require_explicit_rebuild() {
        let ((), report) = run_async_under_lab(0xbeac_1104, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let first = db.write(&commit, document(1)).await.unwrap();
            let original = db.prepare_beacon_index(&query, &options()).unwrap();
            let second = db.write(&commit, document(2)).await.unwrap();
            let third = db.write(&commit, document(3)).await.unwrap();
            let authoritative = Arc::clone(&db.snapshot);
            let source = authoritative.delta_index.get(third).unwrap();
            let oid = fgdb_types::ids::ObjectId([0x5c; 32]);
            for family in 0..4 {
                let mut coordinates = source.coordinate_entries().to_vec();
                let coordinate = &mut coordinates[0];
                match family {
                    0 => coordinate.schema_transition = Some(oid),
                    1 => coordinate.rows = vec![DeltaRow::Schema {
                        transition_oid: oid,
                        before_epoch: coordinate.schema_epoch,
                        after_epoch: fgdb_delta_types::SchemaEpoch(2),
                    }],
                    2 => coordinate.rows = vec![DeltaRow::ValidTime {
                        elem: ElementId::Vertex(VId(1)),
                        contract_id: oid,
                        before: None,
                        after: None,
                    }],
                    _ => coordinate.rows = vec![DeltaRow::Constraint {
                        before_schema_root: oid,
                        after_schema_root: oid,
                        before_constraint_root: oid,
                        after_constraint_root: oid,
                    }],
                }
                // Explicitly synthetic reader-admission fixture. This does
                // not attest that these mutated payloads were committed or
                // authenticate them with the retained real marker identity.
                let future = fgdb_delta_types::LogicalDeltaBatch::from_parts_for_test(
                    coordinates,
                    *source.source_template_digest(),
                    source.commit_marker_identity(),
                    third,
                    third,
                );
                let second_batch = authoritative.delta_index.get(second).unwrap().clone();
                Arc::make_mut(&mut db.snapshot).delta_index =
                    LocalDeltaBatchIndex::from_parts_for_test(
                        first, third, vec![(second, second_batch), (third, future)],
                    );
                let mut candidate = original.clone();
                // The unselected future batch must NOT block a supported cut.
                candidate.refresh(&query, &db, Some(second), ReadPolicy::default()).unwrap();
                let pinned = candidate.snapshot();
                assert_eq!(pinned.source_sequence(), second);
                assert_eq!(pinned.stats().documents, 2);
                assert!(matches!(candidate.refresh(&query, &db, None, ReadPolicy::default()),
                    Err(Error::UnsupportedDelta)));
                assert_eq!(candidate.source_sequence(), second);
                assert_eq!(candidate.snapshot().stats(), pinned.stats());
                for search in searches() {
                    assert_eq!(candidate.search(&query, search, ReadPolicy::default()).unwrap(),
                        pinned.search(&query, search, ReadPolicy::default()).unwrap());
                }
            }
            db.snapshot = authoritative;
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn extracting_the_first_interruption_does_not_resample_or_revive_the_meter() {
        let calls = std::cell::Cell::new(0);
        let mut meter = Meter::new(100, |_| {
            calls.set(calls.get() + 1);
            Err(37_u64)
        });
        assert!(matches!(meter.charge(1), Err(BeaconError::Cancelled)));
        assert!(matches!(meter.charge(0), Err(BeaconError::Cancelled)));
        assert_eq!(meter.into_failure(), Some(37));
        assert_eq!(calls.get(), 1);
    }
}
