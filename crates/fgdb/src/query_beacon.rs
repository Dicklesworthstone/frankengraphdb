//! Native snapshot -> bounded scalar projection -> Beacon search. Historical
//! winners are selected by the SAME visitor as GLA, before any label/property
//! projection. Privileged exact searches reuse matching maintained generations;
//! other reads build their admitted corpus per execution. Neither is a new
//! authority, a durable index descriptor, or an external-memory claim.

use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, EmbeddedReadView, ReadError, Snapshot, VertexRow};
use asupersync::fs::Vfs;
use fgdb_beacon::read::{ReadError as Error, ReadOptions, Rows, Search};
use fgdb_beacon::{BeaconError, BeaconIndex, IndexConfig, WorkBudget, WorkControl};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::cell::RefCell;

#[path = "query_beacon/demand.rs"]
pub(crate) mod demand;
#[path = "query_beacon/graph.rs"]
pub(crate) mod graph;
#[path = "query_beacon/indexed.rs"]
pub(crate) mod indexed;
#[path = "query_beacon/procedure.rs"]
pub(crate) mod procedure;
#[path = "query_beacon/resident.rs"]
mod resident;
pub use resident::{
    Error as ResidentIndexError, MAX_MAINTAINED_BEACON_INDEXES, MaintainedBeaconError,
    MaintainedBeaconFailure, MaintainedBeaconIndex, MaintainedBeaconStatus, PinnedIndex,
    RefreshReport, ResidentIndex,
};
pub(crate) use resident::{QueryIndexes as BeaconQueryIndexes, Registry as BeaconRegistry};

pub(crate) type Options = ReadOptions<PropertyKeyId, LabelId>;
type Cancel = Box<asupersync::error::Error>;

/// A typed interruption is retained outside Beacon's narrow WorkControl sum.
/// No later wrapper check may replace an expiry/cancellation with a generic
/// index refusal. One meter is shared sequentially by visitor and builder.
pub(crate) struct Meter<E, F> {
    budget: WorkBudget,
    gate: F,
    failure: Option<E>,
}
impl<E, F: FnMut(usize) -> Result<(), E>> Meter<E, F> {
    pub(crate) fn new(units: usize, gate: F) -> Self {
        Self {
            budget: WorkBudget::new(units),
            gate,
            failure: None,
        }
    }
    pub(crate) fn refuse(&mut self, error: E) -> BeaconError {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
        BeaconError::Cancelled
    }
    /// Preserve an adapter's typed failure after off-side work completes.
    /// Consuming the meter cannot resample the gate or revive its allowance.
    pub(crate) fn into_failure(self) -> Option<E> {
        self.failure
    }
    pub(crate) fn finish<R, T>(self, result: Result<T, BeaconError>) -> Result<T, Error<R, E>> {
        match self.failure {
            Some(error) => Err(Error::Interrupted(error)),
            None => result.map_err(Error::Index),
        }
    }
}
impl<E, F: FnMut(usize) -> Result<(), E>> WorkControl for Meter<E, F> {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        if self.failure.is_some() {
            return Err(BeaconError::Cancelled);
        }
        if let Err(error) = (self.gate)(units) {
            return Err(self.refuse(error));
        }
        self.budget.charge(units)
    }
}

pub(crate) struct SharedWork<'a, W>(pub(crate) &'a RefCell<W>);
impl<W: WorkControl> WorkControl for SharedWork<'_, W> {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        self.0.borrow_mut().charge(units)
    }
}

/// How the history walk is paid for. The privileged reader meters every
/// visited record and source scratch entry against its own budget. A Warden
/// reader walks unmetered, polling only cancellation and a retained refusal:
/// history it cannot see must not move any limit it can observe (FG-INV-20).
pub(crate) enum Scan<'a> {
    Metered,
    Unmetered(&'a mut dyn FnMut() -> Result<(), BeaconError>),
}

/// Private common preparation for privileged and Warden readers. Admitting a
/// row and a property are separate operations; neither callback sees copied
/// values. The iterator projects one borrowed winner per builder poll.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    config: IndexConfig,
    work: &RefCell<impl WorkControl>,
    scan: Scan<'_>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    property_allowed: impl FnMut(PropertyKeyId) -> bool,
) -> Result<BeaconIndex, BeaconError> {
    build_selected(
        snapshot,
        at,
        options,
        config,
        work,
        scan,
        admit,
        label_allowed,
        property_allowed,
        |_| Ok(()),
    )
}

/// Observe the exact selected corpus after historical, authorization, label
/// and staging admission. Graph retrieval uses this seam to admit transit
/// vertices without a second scan or a differently filtered source domain.
#[allow(clippy::too_many_arguments)]
fn build_selected(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    config: IndexConfig,
    work: &RefCell<impl WorkControl>,
    scan: Scan<'_>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    property_allowed: impl FnMut(PropertyKeyId) -> bool,
    selected: impl FnMut(&VertexRow) -> Result<(), BeaconError>,
) -> Result<BeaconIndex, BeaconError> {
    build_selected_optional(
        snapshot,
        at,
        options,
        Some(config),
        work,
        scan,
        admit,
        label_allowed,
        property_allowed,
        selected,
    )?
    .ok_or(BeaconError::Invariant(
        "requested search index was not constructed",
    ))
}

/// The same admitted vertex source with an explicitly optional index. None
/// means topology-only preparation: every source/label/staging check and the
/// selected callback still runs, but no property is projected and no text or
/// vector index is constructed. It is never a fallback after a build failure.
#[allow(clippy::too_many_arguments)]
fn build_selected_optional(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    config: Option<IndexConfig>,
    work: &RefCell<impl WorkControl>,
    mut scan: Scan<'_>,
    mut admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    mut label_allowed: impl FnMut(LabelId) -> bool,
    mut property_allowed: impl FnMut(PropertyKeyId) -> bool,
    mut selected: impl FnMut(&VertexRow) -> Result<(), BeaconError>,
) -> Result<Option<BeaconIndex>, BeaconError> {
    let max_documents = config
        .as_ref()
        .map_or(options.index.max_documents, |config| config.max_documents);
    let mut scratch = 0usize;
    let mut source_work = |event| match &mut scan {
        Scan::Metered => {
            work.borrow_mut().charge(1)?;
            if matches!(event, SourceEvent::ScratchEntry) {
                if scratch == options.policy.max_source_scratch {
                    return Err(BeaconError::ResourceLimit {
                        resource: "source scratch entries",
                        limit: options.policy.max_source_scratch,
                    });
                }
                scratch += 1;
            }
            Ok(())
        }
        Scan::Unmetered(poll) => poll(),
    };
    let mut rows = Vec::new();
    source::visit_vertices(&snapshot.patches, at, &mut source_work, |row, _| {
        if !admit(row)? {
            return Ok(());
        }
        // One unit for the admitted record plus one per label this reader may
        // see. The privileged reader sees every label and admits every row, so
        // its charge is unchanged; a Warden reader pays nothing for hidden ones.
        let labels = row
            .labels
            .iter()
            .filter(|&&label| label_allowed(label))
            .count();
        work.borrow_mut().charge(1 + labels)?;
        if options
            .vertex_label
            .is_some_and(|label| !label_allowed(label) || row.labels.binary_search(&label).is_err())
        {
            return Ok(());
        }
        let limit = options.policy.max_staging_rows.min(max_documents);
        if rows.len() == limit {
            return Err(BeaconError::ResourceLimit {
                resource: "staged vertices",
                limit,
            });
        }
        rows.try_reserve(1)
            .map_err(|_| BeaconError::ResourceLimit {
                resource: "vertex staging allocation",
                limit,
            })?;
        selected(row)?;
        rows.push(row);
        Ok(())
    })?;
    let Some(config) = config else {
        return Ok(None);
    };
    let projected = rows.into_iter().map(|row| {
        options.projection.project(
            row.vid,
            &config,
            |key| {
                // Do not inspect an unauthorized property's value or its type.
                if !property_allowed(key) {
                    return None;
                }
                row.props
                    .binary_search_by_key(&key, |(key, _)| *key)
                    .ok()
                    .map(|slot| &row.props[slot].1)
            },
            &mut SharedWork(work),
        )
    });
    BeaconIndex::try_build(config.clone(), projected, &mut SharedWork(work)).map(Some)
}

/// Result eligibility is independent of source authorization and projection.
/// Keep the admitted corpus (and its BM25 statistics/ANN topology) unchanged;
/// each lane applies the stable predicate before selecting its candidate depth.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_filtered(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: Search<'_>,
    work: &RefCell<impl WorkControl>,
    scan: Scan<'_>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    property_allowed: impl FnMut(PropertyKeyId) -> bool,
    eligible: impl Fn(VId) -> bool,
) -> Result<Rows, BeaconError> {
    work.borrow_mut().charge(1)?;
    let config = options.config_for(query)?;
    query.validate(&config, &mut SharedWork(work))?;
    let index = build(
        snapshot,
        at,
        options,
        config,
        work,
        scan,
        admit,
        label_allowed,
        property_allowed,
    )?;
    let rows = query.execute_filtered(&index.snapshot(), eligible, &mut SharedWork(work))?;
    // Even empty and native zero-k paths cannot skip final live admission.
    work.borrow_mut().charge(1)?;
    Ok(rows)
}

/// Privileged exact retrieval may use a matching immutable maintained pin.
/// Capability readers deliberately use evaluate_filtered instead: their
/// corpus, statistics and topology must be built after source authorization.
fn evaluate_privileged_filtered(
    snapshot: &Snapshot,
    indexes: &BeaconQueryIndexes,
    at: CommitSeq,
    options: &Options,
    query: Search<'_>,
    work: &RefCell<impl WorkControl>,
    eligible: impl Fn(VId) -> bool,
) -> Result<Rows, BeaconError> {
    work.borrow_mut().charge(1)?;
    let config = options.config_for(query)?;
    query.validate(&config, &mut SharedWork(work))?;
    let index = match indexes.lookup_exact(
        at,
        snapshot.frontier,
        options,
        &config,
        query,
        &mut SharedWork(work),
    )? {
        Some(index) => index,
        None => build(
            snapshot,
            at,
            options,
            config,
            work,
            Scan::Metered,
            |_| Ok(true),
            |_| true,
            |_| true,
        )?
        .snapshot(),
    };
    let rows = query.execute_filtered(&index, eligible, &mut SharedWork(work))?;
    // Reuse cannot bypass final live admission, even for an empty answer.
    work.borrow_mut().charge(1)?;
    Ok(rows)
}

impl<V: Vfs + Clone> Database<V> {
    /// Search one admitted database generation. None selects its frontier;
    /// as_of selects that generation's historical winners, including updates,
    /// label changes and tombstones. Exact retrieval reuses a healthy maintained
    /// index only at the identical cut and projection/configuration. Historical
    /// mismatches, unavailable indexes and ANN use ordinary source construction.
    /// This is a privileged API, like read_session; use beacon_search_authorized
    /// when serving capability holders. No graph/value/clock is fabricated.
    pub fn beacon_search(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: Search<'_>,
    ) -> Result<Rows, Error<ReadError, Cancel>> {
        self.beacon_search_filtered(cx, options, query, |_| true)
    }

    /// Search eligible results before native candidate limits and final top-k.
    /// The predicate must be stable, bounded host-side computation over IDs,
    /// not I/O or a capability check. It does not narrow the source corpus or
    /// change BM25 statistics; rejected vertices can still route ANN search.
    /// Use a pinned read view when deriving eligibility from database state.
    /// This remains privileged, resident retrieval; ANN remains approximate.
    pub fn beacon_search_filtered(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: Search<'_>,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Rows, Error<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let current = self.frontier().map_err(Error::Read)?;
            let at = options.as_of.unwrap_or(current);
            self.snapshot.check_frontier(at).map_err(Error::Read)?;
            let work = RefCell::new(Meter::new(options.policy.max_work_units, |_| {
                cx.checkpoint()
            }));
            let result = evaluate_privileged_filtered(
                &self.snapshot,
                self.maintained_beacon.query_indexes(),
                at,
                options,
                query,
                &work,
                eligible,
            );
            work.into_inner().finish(result)
        })
    }
}

impl EmbeddedReadView {
    /// Reuse this pinned generation, not the writer's current head. A later
    /// writer publication/drop cannot change these results. Matching maintained
    /// pins avoid per-call construction for text and exact vector retrieval.
    /// Exactness is over explicitly projected f32 coordinates; ANN retains its
    /// ordinary per-call generation. Both paths remain resident.
    pub fn beacon_search(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: Search<'_>,
    ) -> Result<Rows, Error<ReadError, Cancel>> {
        self.beacon_search_filtered(cx, options, query, |_| true)
    }

    /// Filter source candidates at this view's pinned historical cut, never
    /// by filtering an already-truncated result. Derive any data-dependent
    /// eligibility from this same view and `options.as_of`. The host predicate
    /// is not an authorization boundary and must be stable and bounded.
    /// One meter covers any required construction, search and final publication.
    /// A maintained pin needs no source staging; query/result limits and live
    /// cancellation still apply, including when every endpoint is rejected.
    pub fn beacon_search_filtered(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: Search<'_>,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Rows, Error<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let at = options.as_of.unwrap_or(self.frontier());
            self.snapshot.check_frontier(at).map_err(Error::Read)?;
            let work = RefCell::new(Meter::new(options.policy.max_work_units, |_| {
                cx.checkpoint()
            }));
            let result = evaluate_privileged_filtered(
                &self.snapshot,
                &self.beacon_indexes,
                at,
                options,
                query,
                &work,
                eligible,
            );
            work.into_inner().finish(result)
        })
    }
}

#[cfg(test)]
mod filtered_tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_beacon::{
        DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
    };
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

    const SECOND: VId = VId((1_u128 << 100) + 2);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x41; 32],
            DatabaseSecurityNamespaceId([0x42; 32]),
            [0x43; 32],
        )
    }

    fn initial() -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        for (id, coordinate) in [(VId(1), 0), (SECOND, 1)] {
            batch.create_vertex(
                id,
                vec![],
                vec![
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text("red").unwrap(),
                    ),
                    (PropertyKeyId(2), CanonicalScalar::Int(coordinate)),
                ],
            );
        }
        batch
    }

    fn options() -> Options {
        let mut options = Options::text(PropertyKeyId(1));
        options.projection.vector = vec![PropertyKeyId(2)];
        options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
        options
    }

    fn queries() -> [Search<'static>; 3] {
        [
            Search::Text {
                query: "red",
                k: 1,
                mode: TextMatch::Any,
            },
            Search::Vector {
                query: &[0.0],
                k: 1,
                mode: VectorSearch::Exact,
            },
            Search::Hybrid(ExactHybridQuery {
                vector: &[0.0],
                text: "red",
                k: 1,
                vector_candidates: 1,
                text_candidates: 1,
                vector_mode: VectorSearch::Exact,
                text_mode: TextMatch::Any,
                profile: ExactRrfProfile::default(),
            }),
        ]
    }

    fn ids(rows: &Rows) -> Vec<VId> {
        match rows {
            Rows::Text(rows) => rows.iter().map(|row| row.id).collect(),
            Rows::Vector(rows) => rows.iter().map(|row| row.id).collect(),
            Rows::Hybrid(rows) => rows.iter().map(|row| row.id).collect(),
        }
    }

    #[test]
    fn filtering_refills_each_native_lane_before_candidate_depth_and_top_k() {
        let ((), report) = run_async_under_lab(0xbeac_3101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, initial()).await.unwrap();
            for query in queries() {
                let baseline = db.beacon_search(&cx, &options(), query).unwrap();
                assert_eq!(ids(&baseline), [VId(1)]);
                let rows = db
                    .beacon_search_filtered(&cx, &options(), query, |id| id == SECOND)
                    .unwrap();
                assert_eq!(ids(&rows), [SECOND]);
                if let Rows::Hybrid(hits) = rows {
                    assert_eq!(hits[0].vector_rank.unwrap().get(), 1);
                    assert_eq!(hits[0].text_rank.unwrap().get(), 1);
                    assert_eq!(hits[0].score.numerator(), 2);
                    assert_eq!(hits[0].score.denominator(), 61);
                }
                assert!(
                    db.beacon_search_filtered(&cx, &options(), query, |_| false)
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(
                    db.beacon_search_filtered(&cx, &options(), query, |_| true)
                        .unwrap(),
                    baseline
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn filtered_reads_keep_retained_history_through_delete_compaction_and_writer_drop() {
        let ((), report) = run_async_under_lab(0xbeac_3102, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = db.write(&commit, initial()).await.unwrap();
            let pinned = db.read_session().unwrap();
            let query = queries()[2];
            let before = pinned
                .beacon_search_filtered(&cx, &options(), query, |id| id == SECOND)
                .unwrap();
            let mut changes = WriteBatch::new(RelationId(1));
            changes.delete_vertex(SECOND);
            db.write(&commit, changes).await.unwrap();
            assert!(
                db.beacon_search_filtered(&cx, &options(), query, |id| id == SECOND)
                    .unwrap()
                    .is_empty()
            );
            db.compact(&commit).await.unwrap();
            let mut historical = options();
            historical.as_of = Some(at);
            assert_eq!(
                db.beacon_search_filtered(&cx, &historical, query, |id| id == SECOND)
                    .unwrap(),
                before
            );
            historical.as_of = Some(CommitSeq(at.0 + 1));
            assert!(matches!(
                pinned.beacon_search_filtered(&cx, &historical, query, |_| false),
                Err(Error::Read(ReadError::BeyondFrontier { .. }))
            ));
            drop(db);
            assert_eq!(
                pinned
                    .beacon_search_filtered(&cx, &options(), query, |id| id == SECOND)
                    .unwrap(),
                before
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn reject_all_never_bypasses_source_limits_or_any_work_checkpoint() {
        #[derive(Default)]
        struct Cut {
            calls: usize,
            stop: Option<usize>,
        }
        impl WorkControl for Cut {
            fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
                self.calls += 1;
                if self.stop == Some(self.calls) {
                    Err(BeaconError::Cancelled)
                } else {
                    Ok(())
                }
            }
        }
        let ((), report) = run_async_under_lab(0xbeac_3103, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = db.write(&commit, initial()).await.unwrap();
            let manifest = db.manifest().unwrap();
            for query in queries() {
                let run = |stop| {
                    let work = RefCell::new(Cut {
                        stop,
                        ..Cut::default()
                    });
                    let result = evaluate_filtered(
                        &db.snapshot,
                        at,
                        &options(),
                        query,
                        &work,
                        Scan::Metered,
                        |_| Ok(true),
                        |_| true,
                        |_| true,
                        |_| false,
                    );
                    (result, work.into_inner().calls)
                };
                let (rows, calls) = run(None);
                assert!(rows.unwrap().is_empty());
                assert!(calls > 2);
                for stop in 1..=calls {
                    let (result, seen) = run(Some(stop));
                    assert!(matches!(result, Err(BeaconError::Cancelled)), "cut {stop}");
                    assert_eq!(seen, stop);
                }
                let mut limited = options();
                limited.policy.max_staging_rows = 0;
                assert!(matches!(
                    db.beacon_search_filtered(&cx, &limited, query, |_| false),
                    Err(Error::Index(BeaconError::ResourceLimit {
                        resource: "staged vertices",
                        ..
                    }))
                ));
            }
            assert_eq!(db.frontier().unwrap(), at);
            assert_eq!(db.manifest().unwrap(), manifest);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
