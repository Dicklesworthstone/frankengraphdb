//! Graph/text/vector retrieval from one admitted native historical cut.
//! The selected vertex corpus is shared by projection and graph traversal;
//! historical edges are admitted only when BOTH endpoints belong to it.

use super::{Cancel, Meter, Options, Scan, SharedWork, build_selected_optional};
use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, EmbeddedReadView, ReadError, Snapshot, VertexRow};
use asupersync::fs::Vfs;
use fgdb_beacon::expansion::{ExpansionGraph, ExpansionSpec};
use fgdb_beacon::read::{ReadError as Error, Search};
use fgdb_beacon::{BeaconError, GraphHybridHit, GraphHybridQuery, WorkControl};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::cell::{Cell, RefCell};

/// One cumulative logical scratch allowance for visible graph-source state and
/// the expansion/fusion kernel. Admissions happen before retention and are not
/// refunded when a heap slot or temporary entry is later released.
struct ScratchWork<'a, W> {
    work: &'a RefCell<W>,
    used: &'a Cell<usize>,
    limit: usize,
}

fn reserve_scratch(used: &Cell<usize>, limit: usize, entries: usize) -> Result<(), BeaconError> {
    let next = used
        .get()
        .checked_add(entries)
        .ok_or(BeaconError::ResourceLimit {
            resource: "graph query scratch entries",
            limit,
        })?;
    if next > limit {
        return Err(BeaconError::ResourceLimit {
            resource: "graph query scratch entries",
            limit,
        });
    }
    used.set(next);
    Ok(())
}

impl<W: WorkControl> WorkControl for ScratchWork<'_, W> {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        self.work.borrow_mut().charge(units)
    }

    fn reserve_scratch(&mut self, entries: usize) -> Result<(), BeaconError> {
        self.work.borrow_mut().charge(entries)?;
        reserve_scratch(self.used, self.limit, entries)
    }
}

/// Private common path for privileged and capability-governed consumers. No
/// caller-supplied graph or hit list can assert that it belongs to this cut.
/// `Scan` also covers the edge history walk: hidden history must not influence
/// a capability holder's resource allowance. Visible graph admission, BFS and
/// fusion all use the SAME meter as projection and both retrieval lanes.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: GraphHybridQuery<'_>,
    expansion: ExpansionSpec<'_, RelationId>,
    work: &RefCell<impl WorkControl>,
    scan: Scan<'_>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    property_allowed: impl FnMut(PropertyKeyId) -> bool,
    relation_allowed: impl FnMut(RelationId) -> bool,
) -> Result<Vec<GraphHybridHit>, BeaconError> {
    evaluate_with_edge_admission(
        snapshot,
        at,
        options,
        query,
        expansion,
        work,
        scan,
        admit,
        label_allowed,
        property_allowed,
        relation_allowed,
        || Ok(()),
    )
}

/// The same execution body with an admitted-edge event for a host's combined
/// vertex/edge allowance. The callback runs AFTER historical winner selection,
/// relation scope and BOTH selected endpoint checks, before retaining an arc.
/// It sees no hidden candidate, raw row, identity or property. Since
/// 2903b02e the session path runs its own evaluator, so only `evaluate`
/// reaches this body.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_with_edge_admission(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: GraphHybridQuery<'_>,
    expansion: ExpansionSpec<'_, RelationId>,
    work: &RefCell<impl WorkControl>,
    scan: Scan<'_>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    property_allowed: impl FnMut(PropertyKeyId) -> bool,
    relation_allowed: impl FnMut(RelationId) -> bool,
    admit_edge: impl FnMut() -> Result<(), BeaconError>,
) -> Result<Vec<GraphHybridHit>, BeaconError> {
    evaluate_filtered(
        snapshot,
        at,
        options,
        query,
        expansion,
        work,
        scan,
        admit,
        label_allowed,
        property_allowed,
        relation_allowed,
        admit_edge,
        |_| true,
    )
}

/// Filter result endpoints, never the admitted corpus or graph transit nodes.
/// The same stable predicate participates in graph candidate selection and in
/// text/vector candidate selection before any rank or final top-k is assigned.
/// Security callbacks still run before corpus construction and edge admission.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_filtered(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: GraphHybridQuery<'_>,
    expansion: ExpansionSpec<'_, RelationId>,
    work: &RefCell<impl WorkControl>,
    mut scan: Scan<'_>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    property_allowed: impl FnMut(PropertyKeyId) -> bool,
    mut relation_allowed: impl FnMut(RelationId) -> bool,
    mut admit_edge: impl FnMut() -> Result<(), BeaconError>,
    eligible: impl Fn(VId) -> bool,
) -> Result<Vec<GraphHybridHit>, BeaconError> {
    work.borrow_mut().charge(1)?;
    let config = if query.is_graph_only() {
        options.validate_graph_only(query)?;
        None
    } else {
        let config = options.config_for_graph(query)?;
        Search::Hybrid(query.source_query()).validate(&config, &mut SharedWork(work))?;
        Some(config)
    };
    let mut graph = if query.graph_enabled() {
        if expansion.seeds.len() > expansion.limits.max_seed_ids {
            return Err(BeaconError::ResourceLimit {
                resource: "expansion seed IDs",
                limit: expansion.limits.max_seed_ids,
            });
        }
        Some(ExpansionGraph::new(expansion.limits))
    } else {
        // A disabled lane neither inspects seeds nor enforces graph limits.
        None
    };
    let vertex_scan = match &mut scan {
        Scan::Metered => Scan::Metered,
        Scan::Unmetered(poll) => Scan::Unmetered(&mut **poll),
    };
    let index = build_selected_optional(
        snapshot,
        at,
        options,
        config,
        work,
        vertex_scan,
        admit,
        label_allowed,
        property_allowed,
        |row| {
            if let Some(graph) = &mut graph {
                graph.insert_vertex(row.vid, &mut SharedWork(work))?;
            }
            Ok(())
        },
    )?;
    let scratch = Cell::new(0usize);
    let graph_hits = if let Some(mut graph) = graph {
        // Neither an empty seed set nor a zero-hop query can consume an edge.
        // In particular include_seeds with zero hops works at a zero edge cap.
        if expansion.max_hops != 0
            && !expansion.seeds.is_empty()
            && expansion.relation.is_none_or(&mut relation_allowed)
        {
            let metered = matches!(&scan, Scan::Metered);
            let mut control = |event| match &mut scan {
                Scan::Metered => {
                    work.borrow_mut().charge(1)?;
                    if matches!(event, SourceEvent::ScratchEntry) {
                        reserve_scratch(&scratch, expansion.limits.max_source_scratch, 1)?;
                    }
                    Ok(())
                }
                Scan::Unmetered(poll) => poll(),
            };
            // This visitor selects authoritative retirements/content successors
            // before filtering. No earlier visible edge can be resurrected.
            // Edge properties/weights are not read or copied for unit-hop BFS.
            source::visit_edges(&snapshot.blocks, at, &mut control, |edge, _| {
                if expansion
                    .relation
                    .is_none_or(|relation| relation == edge.relation)
                    && relation_allowed(edge.relation)
                    && graph.contains(edge.src)
                    && graph.contains(edge.dst)
                {
                    admit_edge()?;
                    // Scoped history is poll-only. Its observable scratch
                    // allowance counts only admitted edge winners, after
                    // relation and BOTH endpoint checks, never hidden history.
                    if !metered {
                        work.borrow_mut().charge(1)?;
                        reserve_scratch(&scratch, expansion.limits.max_source_scratch, 1)?;
                    }
                    // Charges each admitted logical edge before parallel-edge
                    // collapse, and admits direction-specific arcs atomically.
                    graph.insert_edge(
                        edge.src,
                        edge.dst,
                        expansion.direction,
                        &mut SharedWork(work),
                    )?;
                }
                Ok(())
            })?;
        }
        let mut kernel = ScratchWork {
            work,
            used: &scratch,
            limit: expansion.limits.max_source_scratch,
        };
        graph.expand_filtered(
            expansion.seeds,
            expansion.max_hops,
            expansion.include_seeds,
            query.graph_candidates,
            |id, _| Ok(eligible(id)),
            &mut kernel,
        )?
    } else {
        Vec::new()
    };
    // Topology-only queries retain the same source and traversal, without
    // building or searching a dummy index. Both branches use one fusion owner.
    let mut kernel = ScratchWork {
        work,
        used: &scratch,
        limit: expansion.limits.max_source_scratch,
    };
    let rows = match index {
        Some(index) => index.snapshot().hybrid_search_graph_filtered(
            query,
            &graph_hits,
            eligible,
            &mut kernel,
        )?,
        None => query.fuse_graph_only(&graph_hits, eligible, &mut kernel)?,
    };
    // No partial rows escape, even on empty/zero-k paths or final refusal.
    work.borrow_mut().charge(1)?;
    Ok(rows)
}

impl<V: Vfs + Clone> Database<V> {
    /// Retrieve with text, vector and graph rank contributions from one native
    /// cut. `options.as_of` selects history; None selects the healthy frontier.
    /// Graph seeds, relation, direction, hop bound and resident limits are
    /// explicit. Selected labels constrain transit vertices as well as hits.
    /// `GraphHybridQuery::graph_only` reads no text/vector property or index.
    ///
    /// Privileged embedded API, not a capability grant. Construction is
    /// per-execution and resident, with no durable index or spill guarantee.
    /// Exact RRF arithmetic does not make ANN or truncated candidates exact.
    pub fn beacon_search_graph(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
    ) -> Result<Vec<GraphHybridHit>, Error<ReadError, Cancel>> {
        self.beacon_search_graph_filtered(cx, options, query, expansion, |_| true)
    }

    /// Select result endpoints before all three modality limits and final
    /// fusion. A rejected result vertex remains a valid transit vertex; only
    /// source selection determines which vertices can be traversed. The host
    /// predicate must be stable, bounded computation over IDs, not I/O or an
    /// authorization check. Derive data-dependent filters from a pinned view.
    /// This is a privileged resident API, with the same approximation bounds
    /// as `beacon_search_graph`, not a secure-view or exhaustive-search claim.
    pub fn beacon_search_graph_filtered(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, Error<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            self.read_session()
                .map_err(Error::Read)?
                .beacon_search_graph_filtered(cx, options, query, expansion, eligible)
        })
    }
}

impl EmbeddedReadView {
    /// Search only this pinned generation. New writes, compaction or dropping
    /// the writer cannot mix newer topology with the retained text/vector cut.
    /// Missing and out-of-selection seeds are absent, never invented vertices.
    pub fn beacon_search_graph(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
    ) -> Result<Vec<GraphHybridHit>, Error<ReadError, Cancel>> {
        self.beacon_search_graph_filtered(cx, options, query, expansion, |_| true)
    }

    /// Endpoint filtering at this pinned generation and selected history.
    /// Derive any graph-dependent predicate from this same view/as_of cut.
    /// Rejected endpoints retain their transit role, and all existing work,
    /// staging, graph and final-delivery checks remain effective. Reject-all
    /// cannot bypass a late source failure or a visited-vertex limit.
    pub fn beacon_search_graph_filtered(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, Error<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let at = options.as_of.unwrap_or(self.frontier());
            self.snapshot.check_frontier(at).map_err(Error::Read)?;
            let work = RefCell::new(Meter::new(options.policy.max_work_units, |_| {
                cx.checkpoint()
            }));
            let result = evaluate_filtered(
                &self.snapshot,
                at,
                options,
                query,
                expansion,
                &work,
                Scan::Metered,
                |_| Ok(true),
                |_| true,
                |_| true,
                |_| true,
                || Ok(()),
                eligible,
            );
            work.into_inner().finish(result)
        })
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod retrieval_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_beacon::expansion::{ExpansionDirection as Direction, ExpansionLimits};
    use fgdb_beacon::{
        DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
    };
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

    const HIGH: VId = VId((1_u128 << 100) + 3);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x31; 32],
            DatabaseSecurityNamespaceId([0x32; 32]),
            [0x33; 32],
        )
    }

    fn options() -> Options {
        let mut options = Options::text(PropertyKeyId(1));
        options.vertex_label = Some(LabelId(1));
        options.projection.vector = vec![PropertyKeyId(2)];
        options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
        options
    }

    fn query() -> GraphHybridQuery<'static> {
        GraphHybridQuery {
            retrieval: ExactHybridQuery {
                vector: &[0.0],
                text: "graph",
                k: 1,
                vector_candidates: 3,
                text_candidates: 0,
                vector_mode: VectorSearch::Exact,
                text_mode: TextMatch::Any,
                profile: ExactRrfProfile::new(60, 1, 0).unwrap(),
            },
            graph_candidates: 10,
            graph_weight: 100,
        }
    }

    fn expansion(seeds: &[VId]) -> ExpansionSpec<'_, RelationId> {
        ExpansionSpec {
            seeds,
            relation: Some(RelationId(1)),
            direction: Direction::Outgoing,
            max_hops: 2,
            include_seeds: false,
            limits: ExpansionLimits::default(),
        }
    }

    fn initial() -> WriteBatch {
        let mut batch = WriteBatch::new(RelationId(1));
        for (id, x, words) in [
            (VId(1), 0, "graph"),
            (VId(2), 5, "storage"),
            (HIGH, 10, "graph graph"),
        ] {
            batch.create_vertex(
                id,
                vec![LabelId(1)],
                vec![
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text(words).unwrap(),
                    ),
                    (PropertyKeyId(2), CanonicalScalar::Int(x)),
                ],
            );
        }
        batch.create_vertex(VId(4), vec![LabelId(1)], vec![]);
        for (eid, src, dst) in [
            (1, VId(1), VId(2)),
            (2, VId(1), VId(2)),
            (3, VId(2), HIGH),
            (4, HIGH, VId(1)),
            (5, VId(2), VId(2)),
        ] {
            batch.add_edge(EId(eid), src, dst, vec![]);
        }
        batch
    }

    fn graph_population(
        db: &Database<MemVfs>,
        cx: &QueryCx,
        options: &Options,
        expansion: ExpansionSpec<'_, RelationId>,
    ) -> Vec<(VId, u32)> {
        let mut q = query();
        q.retrieval.k = 10;
        let mut rows = db
            .beacon_search_graph(cx, options, q, expansion)
            .unwrap()
            .into_iter()
            .filter_map(|hit| hit.graph_hops.map(|hops| (hit.id, hops)))
            .collect::<Vec<_>>();
        rows.sort_by_key(|(id, hops)| (*hops, *id));
        rows
    }

    #[test]
    fn native_third_lane_promotes_base_losers_and_adds_graph_only_results() {
        let ((), report) = run_async_under_lab(0xbeac_2101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, initial()).await.unwrap();
            let options = options();
            let mut spec = expansion(&[VId(2)]);
            spec.max_hops = 1;
            let hits = db
                .beacon_search_graph(&cx, &options, query(), spec)
                .unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].id, HIGH);
            assert_eq!(hits[0].vector_rank.unwrap().get(), 3);
            assert_eq!(hits[0].graph_rank.unwrap().get(), 1);
            assert_eq!(hits[0].graph_hops, Some(1));
            // Independent arithmetic: 1/63 + 100/61 = 6361/3843.
            assert_eq!(
                (hits[0].score.numerator(), hits[0].score.denominator()),
                (6361, 3843)
            );
            let mut edge = WriteBatch::new(RelationId(2));
            edge.add_edge(EId(6), VId(1), VId(4), vec![]);
            db.write(&commit, edge).await.unwrap();
            let mut q = query();
            q.retrieval.k = 4; // Larger than the two-lane candidate population.
            spec.seeds = &[VId(1)];
            spec.relation = Some(RelationId(2));
            let hits = db.beacon_search_graph(&cx, &options, q, spec).unwrap();
            assert_eq!(hits.len(), 4);
            assert_eq!(hits[0].id, VId(4));
            assert_eq!(hits[0].vector_rank, None);
            assert_eq!(hits[0].text_rank, None);
            assert_eq!(hits[0].graph_hops, Some(1));
            assert_eq!(
                (hits[0].score.numerator(), hits[0].score.denominator()),
                (100, 61)
            );
            q.retrieval.text_candidates = 3;
            q.retrieval.profile = ExactRrfProfile::new(60, 1, 1).unwrap();
            let hits = db.beacon_search_graph(&cx, &options, q, spec).unwrap();
            assert!(
                hits.iter()
                    .any(|h| h.text_rank.is_some() && h.vector_rank.is_some())
            );
            assert_eq!(hits[0].id, VId(4));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn historical_retirements_parallel_edges_and_label_transit_share_one_cut() {
        let ((), report) = run_async_under_lab(0xbeac_2102, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = db.write(&commit, initial()).await.unwrap();
            let pinned = db.read_session().unwrap();
            let options = options();
            let spec = expansion(&[VId(1)]);
            let expected = vec![(VId(2), 1), (HIGH, 2)];
            assert_eq!(graph_population(&db, &cx, &options, spec), expected);
            let before = pinned
                .beacon_search_graph(&cx, &options, query(), spec)
                .unwrap();
            let mut changes = WriteBatch::new(RelationId(1));
            changes.delete_edge(EId(1));
            db.write(&commit, changes).await.unwrap();
            assert_eq!(graph_population(&db, &cx, &options, spec), expected);
            let mut changes = WriteBatch::new(RelationId(1));
            changes.set_vertex_label(VId(2), LabelId(1), false);
            changes.set_vertex_property(HIGH, PropertyKeyId(2), Some(CanonicalScalar::Int(1)));
            db.write(&commit, changes).await.unwrap();
            assert!(
                graph_population(&db, &cx, &options, spec).is_empty(),
                "excluded vertex cannot be a bridge"
            );
            let mut historical = options.clone();
            historical.as_of = Some(at);
            assert_eq!(
                db.beacon_search_graph(&cx, &historical, query(), spec)
                    .unwrap(),
                before
            );
            let mut changes = WriteBatch::new(RelationId(1));
            changes.delete_vertex(VId(2));
            changes.add_edge(EId(7), VId(1), HIGH, vec![]);
            db.write(&commit, changes).await.unwrap();
            assert_eq!(graph_population(&db, &cx, &options, spec), vec![(HIGH, 1)]);
            db.compact(&commit).await.unwrap();
            assert_eq!(
                db.beacon_search_graph(&cx, &historical, query(), spec)
                    .unwrap(),
                before
            );
            drop(db);
            assert_eq!(
                pinned
                    .beacon_search_graph(&cx, &options, query(), spec)
                    .unwrap(),
                before
            );
            historical.as_of = Some(CommitSeq(at.0 + 1));
            assert!(matches!(
                pinned.beacon_search_graph(&cx, &historical, query(), spec),
                Err(Error::Read(ReadError::BeyondFrontier { .. }))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn directions_seed_sets_zero_hops_and_relation_selection_use_real_topology() {
        let ((), report) = run_async_under_lab(0xbeac_2103, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, initial()).await.unwrap();
            let options = options();
            let mut spec = expansion(&[VId(2), VId(2), VId(999)]);
            spec.max_hops = 1;
            for (direction, expected) in [
                (Direction::Outgoing, vec![(HIGH, 1)]),
                (Direction::Incoming, vec![(VId(1), 1)]),
                (Direction::Undirected, vec![(VId(1), 1), (HIGH, 1)]),
            ] {
                spec.direction = direction;
                assert_eq!(graph_population(&db, &cx, &options, spec), expected);
            }
            spec.max_hops = 0;
            spec.limits.max_input_edges = 0;
            spec.limits.max_source_scratch = 0;
            spec.include_seeds = true;
            assert_eq!(
                graph_population(&db, &cx, &options, spec),
                vec![(VId(2), 0)]
            );
            spec.include_seeds = false;
            assert!(graph_population(&db, &cx, &options, spec).is_empty());
            spec = expansion(&[]);
            spec.limits.max_source_scratch = 0;
            assert!(graph_population(&db, &cx, &options, spec).is_empty());
            spec = expansion(&[VId(1)]);
            spec.relation = Some(RelationId(999));
            assert!(graph_population(&db, &cx, &options, spec).is_empty());
            spec.relation = None;
            assert_eq!(
                graph_population(&db, &cx, &options, spec),
                vec![(VId(2), 1), (HIGH, 2)]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn independent_graph_limits_refuse_instead_of_delivering_partial_top_k() {
        let ((), report) = run_async_under_lab(0xbeac_2104, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = db.write(&commit, initial()).await.unwrap();
            let options = options();
            for (limits, resource) in [
                (
                    ExpansionLimits {
                        max_vertices: 3,
                        ..ExpansionLimits::default()
                    },
                    "expansion vertices",
                ),
                (
                    ExpansionLimits {
                        max_input_edges: 1,
                        ..ExpansionLimits::default()
                    },
                    "expansion input edges",
                ),
                (
                    ExpansionLimits {
                        max_visited_vertices: 1,
                        ..ExpansionLimits::default()
                    },
                    "expansion visited vertices",
                ),
                (
                    ExpansionLimits {
                        max_seed_ids: 0,
                        ..ExpansionLimits::default()
                    },
                    "expansion seed IDs",
                ),
                (
                    ExpansionLimits {
                        max_source_scratch: 0,
                        ..ExpansionLimits::default()
                    },
                    "expansion source scratch entries",
                ),
            ] {
                let mut spec = expansion(&[VId(1)]);
                spec.limits = limits;
                assert!(
                    matches!(db.beacon_search_graph(&cx, &options, query(), spec),
                    Err(Error::Index(BeaconError::ResourceLimit { resource: actual, .. })) if actual == resource),
                    "{resource}"
                );
                assert_eq!(db.frontier().unwrap(), at);
            }
            let mut spec = expansion(&[VId(1), VId(999)]);
            spec.limits = ExpansionLimits {
                max_vertices: 0,
                max_input_edges: 0,
                max_visited_vertices: 0,
                max_seed_ids: 0,
                max_source_scratch: 0,
            };
            for (weight, candidates) in [(0, 10), (100, 0)] {
                let q = GraphHybridQuery {
                    graph_weight: weight,
                    graph_candidates: candidates,
                    ..query()
                };
                let hits = db.beacon_search_graph(&cx, &options, q, spec).unwrap();
                assert_eq!(hits[0].id, VId(1));
                assert!(hits[0].graph_rank.is_none());
            }
            let mut denied = options.clone();
            denied.policy.max_result_rows = 0;
            assert!(matches!(
                db.beacon_search_graph(&cx, &denied, query(), expansion(&[VId(1)])),
                Err(Error::Index(BeaconError::ResourceLimit {
                    resource: "result rows",
                    ..
                }))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn every_source_projection_expansion_fusion_and_final_checkpoint_can_refuse() {
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
        let ((), report) = run_async_under_lab(0xbeac_2105, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = db.write(&commit, initial()).await.unwrap();
            let manifest = db.manifest().unwrap();
            let options = options();
            for k in [0, 1] {
                let mut q = query();
                q.retrieval.k = k;
                let run = |stop| {
                    let work = RefCell::new(Cut {
                        stop,
                        ..Cut::default()
                    });
                    let result = evaluate(
                        &db.snapshot,
                        at,
                        &options,
                        q,
                        expansion(&[VId(1)]),
                        &work,
                        Scan::Metered,
                        |_| Ok(true),
                        |_| true,
                        |_| true,
                        |_| true,
                    );
                    (result, work.into_inner().calls)
                };
                let (expected, calls) = run(None);
                let expected = expected.unwrap();
                assert_eq!(expected.len(), k);
                assert!(calls > 2);
                for stop in 1..=calls {
                    let (result, visited) = run(Some(stop));
                    assert!(
                        matches!(result, Err(BeaconError::Cancelled)),
                        "k={k} stop={stop}"
                    );
                    assert_eq!(visited, stop);
                }
                assert_eq!(
                    db.beacon_search_graph(&cx, &options, q, expansion(&[VId(1)]))
                        .unwrap(),
                    expected
                );
            }
            assert_eq!(db.frontier().unwrap(), at);
            assert_eq!(db.manifest().unwrap(), manifest);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn endpoint_filter_refills_all_lanes_without_removing_transit_vertices() {
        let ((), report) = run_async_under_lab(0xbeac_3201, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, initial()).await.unwrap();
            let mut q = query();
            q.retrieval.vector_candidates = 1;
            q.retrieval.text_candidates = 1;
            q.retrieval.profile = ExactRrfProfile::default();
            q.graph_candidates = 1;
            let spec = expansion(&[VId(1)]);
            let hits = db
                .beacon_search_graph_filtered(&cx, &options(), q, spec, |id| id == HIGH)
                .unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].id, HIGH);
            assert_eq!(hits[0].graph_hops, Some(2));
            assert_eq!(hits[0].graph_rank.unwrap().get(), 1);
            assert_eq!(hits[0].vector_rank.unwrap().get(), 1);
            assert_eq!(hits[0].text_rank.unwrap().get(), 1);
            assert_eq!(hits[0].score.numerator(), 102);
            assert_eq!(hits[0].score.denominator(), 61);
            assert_eq!(
                db.beacon_search_graph_filtered(&cx, &options(), q, spec, |_| true)
                    .unwrap(),
                db.beacon_search_graph(&cx, &options(), q, spec).unwrap()
            );
            assert!(
                db.beacon_search_graph_filtered(&cx, &options(), q, spec, |_| false)
                    .unwrap()
                    .is_empty()
            );
            let mut denied = spec;
            denied.limits.max_visited_vertices = 2;
            assert!(matches!(
                db.beacon_search_graph_filtered(&cx, &options(), q, denied, |_| false),
                Err(Error::Index(BeaconError::ResourceLimit {
                    resource: "expansion visited vertices",
                    limit: 2,
                }))
            ));
            q.graph_weight = 0;
            denied.limits.max_vertices = 0;
            let hits = db
                .beacon_search_graph_filtered(&cx, &options(), q, denied, |id| id == HIGH)
                .unwrap();
            assert_eq!(hits[0].id, HIGH);
            assert_eq!(hits[0].graph_rank, None);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn endpoint_filter_uses_one_historical_cut_through_transit_deletion_and_compaction() {
        let ((), report) = run_async_under_lab(0xbeac_3202, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = db.write(&commit, initial()).await.unwrap();
            let pinned = db.read_session().unwrap();
            let spec = expansion(&[VId(1)]);
            let before = pinned
                .beacon_search_graph_filtered(&cx, &options(), query(), spec, |id| id == HIGH)
                .unwrap();
            assert_eq!(before[0].graph_hops, Some(2));
            let mut changes = WriteBatch::new(RelationId(1));
            changes.delete_vertex(VId(2));
            db.write(&commit, changes).await.unwrap();
            let live = db
                .beacon_search_graph_filtered(&cx, &options(), query(), spec, |id| id == HIGH)
                .unwrap();
            assert_eq!(live[0].id, HIGH);
            assert_eq!(live[0].graph_rank, None);
            db.compact(&commit).await.unwrap();
            let mut historical = options();
            historical.as_of = Some(at);
            assert_eq!(
                db.beacon_search_graph_filtered(&cx, &historical, query(), spec, |id| id == HIGH)
                    .unwrap(),
                before
            );
            drop(db);
            assert_eq!(
                pinned
                    .beacon_search_graph_filtered(&cx, &options(), query(), spec, |id| id == HIGH)
                    .unwrap(),
                before
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn filtered_graph_refuses_every_interruption_without_publishing_a_prefix() {
        struct Cut {
            calls: usize,
            stop: usize,
        }
        impl WorkControl for Cut {
            fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
                self.calls += 1;
                if self.calls == self.stop {
                    Err(BeaconError::Cancelled)
                } else {
                    Ok(())
                }
            }
        }
        let ((), report) = run_async_under_lab(0xbeac_3203, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = db.write(&commit, initial()).await.unwrap();
            for accept in [false, true] {
                let run = |stop| {
                    let work = RefCell::new(Cut { calls: 0, stop });
                    let result = evaluate_filtered(
                        &db.snapshot,
                        at,
                        &options(),
                        query(),
                        expansion(&[VId(1)]),
                        &work,
                        Scan::Metered,
                        |_| Ok(true),
                        |_| true,
                        |_| true,
                        |_| true,
                        || Ok(()),
                        |id| accept && id == HIGH,
                    );
                    (result, work.into_inner().calls)
                };
                let (rows, calls) = run(usize::MAX);
                assert_eq!(rows.unwrap().len(), usize::from(accept));
                assert!(calls > 2);
                for stop in 1..=calls {
                    let (result, seen) = run(stop);
                    assert!(matches!(result, Err(BeaconError::Cancelled)), "cut {stop}");
                    assert_eq!(seen, stop);
                }
            }
            assert_eq!(db.frontier().unwrap(), at);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn graph_only_reads_no_properties_or_index_and_keeps_history_and_transit() {
        let ((), report) = run_async_under_lab(0xbeac_3601, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = db.write(&commit, initial()).await.unwrap();
            let pinned = db.read_session().unwrap();
            let mut opt = options();
            // A real no-index request must not enable a dummy lane to pass.
            opt.index.text = None;
            opt.index.vector = None;
            assert!(opt.index.validate().is_err());
            let q = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
            let spec = expansion(&[VId(1)]);
            let before = db
                .beacon_search_graph_filtered(&cx, &opt, q, spec, |id| id == HIGH)
                .unwrap();
            assert_eq!(before.len(), 1);
            assert_eq!(before[0].id, HIGH);
            assert_eq!(before[0].graph_hops, Some(2));
            assert_eq!(before[0].graph_rank.unwrap().get(), 1);
            assert_eq!(
                (before[0].score.numerator(), before[0].score.denominator()),
                (100, 61)
            );
            assert_eq!(before[0].vector_rank, None);
            assert_eq!(before[0].text_rank, None);
            let work = RefCell::new(fgdb_beacon::WorkBudget::new(1_000_000));
            assert_eq!(
                evaluate_filtered(
                    &db.snapshot,
                    at,
                    &opt,
                    q,
                    spec,
                    &work,
                    Scan::Metered,
                    |_| Ok(true),
                    |_| true,
                    |_| panic!("graph-only read requested a property"),
                    |_| true,
                    || Ok(()),
                    |id| id == HIGH,
                )
                .unwrap(),
                before
            );
            // Excluding a transit source is unlike rejecting it as a result.
            assert!(
                evaluate_filtered(
                    &db.snapshot,
                    at,
                    &opt,
                    q,
                    spec,
                    &work,
                    Scan::Metered,
                    |row| Ok(row.vid != VId(2)),
                    |_| true,
                    |_| false,
                    |_| true,
                    || Ok(()),
                    |id| id == HIGH,
                )
                .unwrap()
                .is_empty()
            );
            let mut changes = WriteBatch::new(RelationId(1));
            changes.delete_vertex(VId(2));
            db.write(&commit, changes).await.unwrap();
            assert!(
                db.beacon_search_graph_filtered(&cx, &opt, q, spec, |id| id == HIGH)
                    .unwrap()
                    .is_empty()
            );
            opt.as_of = Some(at);
            assert_eq!(
                db.beacon_search_graph_filtered(&cx, &opt, q, spec, |id| id == HIGH)
                    .unwrap(),
                before
            );
            db.compact(&commit).await.unwrap();
            assert_eq!(
                db.beacon_search_graph_filtered(&cx, &opt, q, spec, |id| id == HIGH)
                    .unwrap(),
                before
            );
            opt.as_of = None;
            drop(db);
            assert_eq!(
                pinned
                    .beacon_search_graph_filtered(&cx, &opt, q, spec, |id| id == HIGH)
                    .unwrap(),
                before
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn graph_only_zero_hop_seeds_need_no_edges_and_refuse_source_limits() {
        let ((), report) = run_async_under_lab(0xbeac_3602, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = db.write(&commit, initial()).await.unwrap();
            let q = GraphHybridQuery::graph_only(2, 2, 1, 60).unwrap();
            let mut spec = expansion(&[HIGH, VId(4), VId(4), VId(999)]);
            spec.max_hops = 0;
            spec.include_seeds = true;
            spec.limits.max_input_edges = 0;
            spec.limits.max_source_scratch = 0;
            let rows = db.beacon_search_graph(&cx, &options(), q, spec).unwrap();
            assert_eq!(
                rows.iter()
                    .map(|row| (row.id, row.graph_hops))
                    .collect::<Vec<_>>(),
                [(VId(4), Some(0)), (HIGH, Some(0))]
            );
            let mut opt = options();
            opt.policy.max_staging_rows = 0;
            assert!(matches!(
                db.beacon_search_graph_filtered(&cx, &opt, q, spec, |_| false),
                Err(Error::Index(BeaconError::ResourceLimit {
                    resource: "staged vertices",
                    ..
                }))
            ));
            let work = RefCell::new(fgdb_beacon::WorkBudget::new(1_000_000));
            evaluate_filtered(
                &db.snapshot,
                at,
                &options(),
                q,
                spec,
                &work,
                Scan::Metered,
                |_| Ok(true),
                |_| true,
                |_| false,
                |_| true,
                || panic!("zero-hop request admitted an edge"),
                |_| true,
            )
            .unwrap();
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn graph_only_reject_all_retains_every_cancellation_checkpoint() {
        struct Cut {
            calls: usize,
            stop: usize,
        }
        impl WorkControl for Cut {
            fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
                self.calls += 1;
                if self.calls == self.stop {
                    Err(BeaconError::Cancelled)
                } else {
                    Ok(())
                }
            }
        }
        let ((), report) = run_async_under_lab(0xbeac_3603, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&contexts.commit(), keys())
                .await
                .unwrap();
            let at = db.write(&contexts.commit(), initial()).await.unwrap();
            for k in [0, 1] {
                let q = GraphHybridQuery::graph_only(k, 1, 1, 60).unwrap();
                let run = |stop| {
                    let work = RefCell::new(Cut { calls: 0, stop });
                    let result = evaluate_filtered(
                        &db.snapshot,
                        at,
                        &options(),
                        q,
                        expansion(&[VId(1)]),
                        &work,
                        Scan::Metered,
                        |_| Ok(true),
                        |_| true,
                        |_| false,
                        |_| true,
                        || Ok(()),
                        |_| false,
                    );
                    (result, work.into_inner().calls)
                };
                let (result, calls) = run(usize::MAX);
                assert!(result.unwrap().is_empty());
                assert!(calls > 2);
                for stop in 1..=calls {
                    let (result, seen) = run(stop);
                    assert!(
                        matches!(result, Err(BeaconError::Cancelled)),
                        "cut {stop}, k={k}"
                    );
                    assert_eq!(seen, stop);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
    #[test]
    fn zero_hop_materialized_kernel_scratch_uses_the_expansion_source_allowance() {
        let ((), report) = run_async_under_lab(0xbeac_2110, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            db.write(&commit, initial()).await.unwrap();
            let options = options();
            let q = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
            let seeds = [VId(1)];
            let mut spec = expansion(&seeds);
            spec.max_hops = 0;
            spec.include_seeds = true;
            spec.limits.max_input_edges = 0;
            spec.limits.max_source_scratch = 0;
            assert!(
                db.beacon_search_graph_filtered(&cx, &options, q, spec, |_| true)
                    .is_err(),
                "zero-hop BFS/ranking state must not bypass the shared scratch cap"
            );
            spec.limits.max_source_scratch = 32;
            let rows = db
                .beacon_search_graph_filtered(&cx, &options, q, spec, |_| true)
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id, VId(1));
            assert_eq!(rows[0].graph_hops, Some(0));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
