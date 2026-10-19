//! Seed-directed graph retrieval over the admitted snapshot's adjacency index.
//! The graph lane retains selected vertices and BFS state, never an edge copy.
//! Text/vector construction and exact candidate fusion remain the native paths.

use super::{BeaconRegistry, Cancel, Meter, Options, Scan, SharedWork, build_selected_optional};
use crate::gql_exec::source::SourceEvent;
use crate::{Database, EmbeddedReadView, ReadError, Snapshot, VertexRow};
use asupersync::fs::Vfs;
use fgdb_beacon::expansion::{
    ExpansionDirection, ExpansionGraph, ExpansionLimits, ExpansionNeighbors, ExpansionSpec,
};
use fgdb_beacon::read::{ReadError as Error, Search};
use fgdb_beacon::{BeaconError, GraphHybridHit, GraphHybridQuery, WorkControl};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GlaExecutionEvent, algebra::GlaDirection};
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::cell::{Cell, RefCell};

struct ScratchWork<'a, W> {
    work: &'a RefCell<W>,
    used: &'a Cell<usize>,
    limit: usize,
}

impl<W: WorkControl> WorkControl for ScratchWork<'_, W> {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        self.work.borrow_mut().charge(units)
    }

    fn reserve_scratch(&mut self, entries: usize) -> Result<(), BeaconError> {
        self.work.borrow_mut().charge(entries)?;
        let next = self
            .used
            .get()
            .checked_add(entries)
            .ok_or(BeaconError::ResourceLimit {
                resource: "indexed graph scratch entries",
                limit: self.limit,
            })?;
        if next > self.limit {
            return Err(BeaconError::ResourceLimit {
                resource: "indexed graph scratch entries",
                limit: self.limit,
            });
        }
        self.used.set(next);
        Ok(())
    }
}

struct IndexedNeighbors<'s, 'g, 'c, R, A> {
    snapshot: &'s Snapshot,
    at: CommitSeq,
    selected: &'g ExpansionGraph,
    relation: Option<RelationId>,
    direction: GlaDirection,
    limits: ExpansionLimits,
    scan: Scan<'c>,
    relation_allowed: R,
    admit_edge: A,
    admitted: usize,
    scratch: &'g Cell<usize>,
}

impl<R, A> IndexedNeighbors<'_, '_, '_, R, A>
where
    R: FnMut(RelationId) -> bool,
    A: FnMut() -> Result<(), BeaconError>,
{
    fn reserve_source(&mut self) -> Result<(), BeaconError> {
        let next = self
            .scratch
            .get()
            .checked_add(1)
            .ok_or(BeaconError::ResourceLimit {
                resource: "indexed graph scratch entries",
                limit: self.limits.max_source_scratch,
            })?;
        if next > self.limits.max_source_scratch {
            return Err(BeaconError::ResourceLimit {
                resource: "indexed graph scratch entries",
                limit: self.limits.max_source_scratch,
            });
        }
        self.scratch.set(next);
        Ok(())
    }

    fn source_event(
        &mut self,
        event: SourceEvent,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        match &mut self.scan {
            Scan::Metered => {
                work.charge(1)?;
                if event == SourceEvent::ScratchEntry {
                    self.reserve_source()?;
                }
                Ok(())
            }
            // No capability-observable charge for invisible historical rows,
            // private index shape or rejected relation/endpoint candidates.
            Scan::Unmetered(poll) => poll(),
        }
    }
}

impl<R, A> ExpansionNeighbors for IndexedNeighbors<'_, '_, '_, R, A>
where
    R: FnMut(RelationId) -> bool,
    A: FnMut() -> Result<(), BeaconError>,
{
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        if self
            .relation
            .is_some_and(|relation| !(self.relation_allowed)(relation))
        {
            return Ok(());
        }
        let snapshot = self.snapshot;
        let direction = self.direction;
        let mut after = None;
        loop {
            let next = snapshot.adjacency_index().next_incident_edge(
                vertex,
                direction,
                after,
                &mut |event| {
                    self.source_event(
                        match event {
                            GlaExecutionEvent::ScratchEntry => SourceEvent::ScratchEntry,
                            GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => {
                                SourceEvent::Work
                            }
                        },
                        work,
                    )
                },
            )?;
            let Some(eid) = next else { break };
            // Ordered successors handle zero and u128::MAX without arithmetic.
            after = Some(eid);
            self.source_event(SourceEvent::Work, work)?;
            let Some((block, row)) =
                snapshot
                    .adjacency_index()
                    .statement_at(&snapshot.blocks, eid, self.at)
            else {
                continue;
            };
            let edge = &snapshot.blocks[block][row];
            let neighbour = match direction {
                GlaDirection::Forward if edge.src == vertex => edge.dst,
                GlaDirection::Reverse if edge.dst == vertex => edge.src,
                GlaDirection::Undirected if edge.src == vertex => edge.dst,
                GlaDirection::Undirected if edge.dst == vertex => edge.src,
                _ => continue,
            };
            // Winner selection MUST precede every filter. The index is a
            // historical superset, not permission to resurrect an older edge.
            if self
                .relation
                .is_some_and(|relation| relation != edge.relation)
                || !(self.relation_allowed)(edge.relation)
                || !self.selected.contains(edge.src)
                || !self.selected.contains(edge.dst)
            {
                continue;
            }
            work.charge(1)?;
            if self.admitted >= self.limits.max_input_edges {
                return Err(BeaconError::ResourceLimit {
                    resource: "indexed expansion incidence visits",
                    limit: self.limits.max_input_edges,
                });
            }
            self.reserve_source()?;
            (self.admit_edge)()?;
            self.admitted += 1;
            // Parallel edges are distinct admissions. A self-loop is visited
            // once at its anchor; an undirected edge may be met at both ends.
            // No arc, edge property, or per-edge visited set is allocated.
            visit(neighbour, work)?;
        }
        Ok(())
    }
}

/// One meter and one selected corpus serve all modalities. The source path is
/// explicit, not an adaptive race, eager fallback or repeated query execution.
/// Scoped callers supply the same original-label admission and property masks
/// as the ordinary Beacon adapter, plus a poll-only hidden-history control.
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

/// Restrict result endpoints while retaining the complete selected corpus and
/// its seed-directed transit. The same predicate precedes every lane's top-k.
/// A graph-only request prepares no text/vector index and reads no properties;
/// its topology uses the same historical successor source as an indexed hybrid.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_filtered(
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
    eligible: impl Fn(VId) -> bool,
) -> Result<Vec<GraphHybridHit>, BeaconError> {
    evaluate_with_registry(
        snapshot, at, options, query, expansion, work, None, scan, admit, label_allowed,
        property_allowed, relation_allowed, admit_edge, eligible,
    )
}

/// Only a privileged Database supplies its own registry. Capability-scoped
/// execution always enters evaluate_filtered and builds the admitted corpus.
#[allow(clippy::too_many_arguments)]
pub(crate) fn evaluate_privileged(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: GraphHybridQuery<'_>,
    expansion: ExpansionSpec<'_, RelationId>,
    work: &RefCell<impl WorkControl>,
    registry: &BeaconRegistry,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    eligible: impl Fn(VId) -> bool,
) -> Result<Vec<GraphHybridHit>, BeaconError> {
    evaluate_with_registry(
        snapshot,
        at,
        options,
        query,
        expansion,
        work,
        Some(registry),
        Scan::Metered,
        admit,
        |_| true,
        |_| true,
        |_| true,
        || Ok(()),
        eligible,
    )
}

#[allow(clippy::too_many_arguments)]
fn evaluate_with_registry(
    snapshot: &Snapshot,
    at: CommitSeq,
    options: &Options,
    query: GraphHybridQuery<'_>,
    expansion: ExpansionSpec<'_, RelationId>,
    work: &RefCell<impl WorkControl>,
    registry: Option<&BeaconRegistry>,
    mut scan: Scan<'_>,
    admit: impl FnMut(&VertexRow) -> Result<bool, BeaconError>,
    label_allowed: impl FnMut(LabelId) -> bool,
    property_allowed: impl FnMut(PropertyKeyId) -> bool,
    relation_allowed: impl FnMut(RelationId) -> bool,
    admit_edge: impl FnMut() -> Result<(), BeaconError>,
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
    let mut selected = if query.graph_enabled() {
        if expansion.seeds.len() > expansion.limits.max_seed_ids {
            return Err(BeaconError::ResourceLimit {
                resource: "expansion seed IDs",
                limit: expansion.limits.max_seed_ids,
            });
        }
        Some(ExpansionGraph::new(expansion.limits))
    } else {
        // Disabled lanes cannot consume seeds, topology, or graph allowances.
        None
    };
    let reused = match (registry, config.as_ref()) {
        // Seeded graph requests keep their existing selected-source build.
        (Some(registry), Some(config)) if !query.graph_enabled() => registry.lookup_exact(
            at,
            snapshot.frontier,
            options,
            config,
            Search::Hybrid(query.source_query()),
            &mut SharedWork(work),
        )?,
        _ => None,
    };
    let index = if let Some(index) = reused {
        // No graph lane needs a vertex directory. The generation already owns
        // the exact current corpus and its complete live BM25 statistics.
        Some(index)
    } else {
        let vertex_scan = match &mut scan {
            Scan::Metered => Scan::Metered,
            Scan::Unmetered(poll) => Scan::Unmetered(&mut **poll),
        };
        build_selected_optional(
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
                if let Some(graph) = &mut selected {
                    graph.insert_vertex(row.vid, &mut SharedWork(work))?;
                }
                Ok(())
            },
        )?
        .map(|index| index.snapshot())
    };
    let scratch = Cell::new(0usize);
    let graph_hits = if let Some(graph) = selected {
        let mut source = IndexedNeighbors {
            snapshot,
            at,
            selected: &graph,
            relation: expansion.relation,
            direction: match expansion.direction {
                ExpansionDirection::Outgoing => GlaDirection::Forward,
                ExpansionDirection::Incoming => GlaDirection::Reverse,
                ExpansionDirection::Undirected => GlaDirection::Undirected,
            },
            limits: expansion.limits,
            scan,
            relation_allowed,
            admit_edge,
            admitted: 0,
            scratch: &scratch,
        };
        let mut kernel = ScratchWork {
            work,
            used: &scratch,
            limit: expansion.limits.max_source_scratch,
        };
        graph.expand_from_source_filtered(
            expansion.seeds,
            expansion.max_hops,
            expansion.include_seeds,
            query.graph_candidates,
            &mut source,
            |id, _| Ok(eligible(id)),
            &mut kernel,
        )?
    } else {
        Vec::new()
    };
    // Index-free and indexed execution share the exact fusion owner. None
    // is selected before source access, never a fallback after index failure.
    let mut kernel = ScratchWork {
        work,
        used: &scratch,
        // Fusion state joins the graph's one allowance only while the lane
        // runs: a disabled lane neither inspects seeds nor enforces graph
        // limits, so text/vector fusion is charged work but never refused by
        // the graph's scratch cap.
        limit: if query.graph_enabled() {
            expansion.limits.max_source_scratch
        } else {
            usize::MAX
        },
    };
    let rows = match index {
        Some(index) => index.hybrid_search_graph_filtered(
            query,
            &graph_hits,
            eligible,
            &mut kernel,
        )?,
        None => query.fuse_graph_only(&graph_hits, eligible, &mut kernel)?,
    };
    work.borrow_mut().charge(1)?;
    Ok(rows)
}

impl<V: Vfs + Clone> Database<V> {
    /// Graph/text/vector retrieval with seed-directed indexed expansion.
    /// Pins one admitted generation; every modality uses the same exact cut.
    /// The graph lane reads incident histories only for reached vertices below
    /// max_hops, instead of constructing a whole selected edge graph. The
    /// existing BFS and exact candidate-fusion implementations own semantics.
    ///
    /// This is a privileged API. Capability holders require the authorized
    /// sibling; neither an index nor a raw snapshot is an authorization grant.
    /// No partial rows escape on source, work, incidence or visited refusal.
    /// No edge properties are read and no per-query adjacency copy is built.
    /// Text/vector corpus preparation still scans selected vertex history;
    /// decoded source/index and visited state remain resident, not spill-backed.
    /// With the graph lane disabled, compatible healthy maintained text/exact-
    /// vector lanes skip source projection and index construction.
    /// GraphHybridQuery::graph_only skips property projection and index creation,
    /// but keeps the selected vertex directory and all source/vertex limits.
    ///
    /// In this explicit physical profile max_input_edges bounds ADMITTED
    /// INCIDENCE VISITS, not all graph edges: parallel EIds count separately,
    /// self-loops once per anchor, undirected edges at most once per expanded
    /// endpoint. max_source_scratch bounds native source scratch events plus
    /// one unit per admitted incidence (logical admission units, not bytes).
    /// Unreached and hop-boundary neighborhoods spend neither allowance. The
    /// legacy whole-graph API retains its whole-graph edge admission profile.
    /// ANN and candidate-limited fusion keep their original approximation scope.
    pub fn beacon_search_graph_indexed(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
    ) -> Result<Vec<GraphHybridHit>, Error<ReadError, Cancel>> {
        self.beacon_search_graph_indexed_filtered(cx, options, query, expansion, |_| true)
    }

    /// Restrict the result endpoints of indexed hybrid retrieval before each
    /// candidate limit. Rejected vertices still contribute corpus statistics
    /// and may carry graph paths to eligible endpoints. The predicate is a
    /// stable, query-local result rule, not an authorization boundary.
    pub fn beacon_search_graph_indexed_filtered(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, Error<ReadError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let current = self.frontier().map_err(Error::Read)?;
            let at = options.as_of.unwrap_or(current);
            self.snapshot.check_frontier(at).map_err(Error::Read)?;
            let work = RefCell::new(Meter::new(options.policy.max_work_units, |_| {
                cx.checkpoint()
            }));
            let result = evaluate_privileged(
                &self.snapshot,
                at,
                options,
                query,
                expansion,
                &work,
                &self.maintained_beacon,
                |_| Ok(true),
                eligible,
            );
            work.into_inner().finish(result)
        })
    }
}

impl EmbeddedReadView {
    /// Indexed sibling over this already-pinned generation. Later writes,
    /// compaction, writer fencing or dropping cannot mix topology with newer
    /// text/vector values. A historical selector beyond this view refuses.
    pub fn beacon_search_graph_indexed(
        &self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
    ) -> Result<Vec<GraphHybridHit>, Error<ReadError, Cancel>> {
        self.beacon_search_graph_indexed_filtered(cx, options, query, expansion, |_| true)
    }

    /// Endpoint-filtered indexed retrieval at this view's exact pinned cut.
    /// Eligibility does not remove transit vertices or change corpus scores.
    pub fn beacon_search_graph_indexed_filtered(
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
#[path = "indexed/graph_only_tests.rs"]
mod graph_only_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_beacon::{
        DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
        WorkBudget,
    };
    use fgdb_types::{
        CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    };
    use std::cell::Cell;

    const R: RelationId = RelationId(1);
    const HIGH: VId = VId(u128::MAX);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
            [0xb3; 32],
        )
    }
    fn options() -> Options {
        let mut value = Options::text(PropertyKeyId(1));
        value.vertex_label = Some(LabelId(1));
        value.projection.vector = vec![PropertyKeyId(2)];
        value.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
        value
    }
    fn query() -> GraphHybridQuery<'static> {
        GraphHybridQuery {
            retrieval: ExactHybridQuery {
                vector: &[0.0],
                text: "graph",
                k: 3,
                vector_candidates: 4,
                text_candidates: 4,
                vector_mode: VectorSearch::Exact,
                text_mode: TextMatch::Any,
                profile: ExactRrfProfile::new(60, 1, 1).unwrap(),
            },
            graph_candidates: 4,
            graph_weight: 100,
        }
    }
    fn spec(
        seeds: &[VId],
        direction: ExpansionDirection,
        hops: u32,
    ) -> ExpansionSpec<'_, RelationId> {
        ExpansionSpec {
            seeds,
            relation: Some(R),
            direction,
            max_hops: hops,
            include_seeds: false,
            limits: ExpansionLimits::default(),
        }
    }
    async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, disconnected: usize) {
        let mut batch = WriteBatch::new(R);
        for (id, x) in [(VId(0), 0), (VId(1), 1), (HIGH, 2), (VId(9), 3)] {
            batch.create_vertex(
                id,
                vec![LabelId(1)],
                vec![
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text("graph").unwrap(),
                    ),
                    (PropertyKeyId(2), CanonicalScalar::Int(x)),
                ],
            );
        }
        for (eid, from, to) in [
            (0, VId(0), VId(1)),
            (1, VId(0), VId(1)),
            (2, VId(1), HIGH),
            (3, HIGH, VId(0)),
            (4, VId(1), VId(1)),
        ] {
            batch.add_edge(EId(eid), from, to, vec![]);
        }
        for eid in 0..disconnected {
            batch.add_edge(EId(100 + eid as u128), VId(9), VId(9), vec![]);
        }
        db.write(cx, batch).await.unwrap();
    }

    #[test]
    fn filtered_indexed_lanes_keep_transit_and_bound_only_reached_incidence() {
        let ((), report) = run_async_under_lab(0xbeac_3501, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit(), 128).await;
            let seeds = [VId(0)];
            let mut query = query();
            query.retrieval.k = 1;
            query.retrieval.vector_candidates = 1;
            query.retrieval.text_candidates = 1;
            query.graph_candidates = 1;
            let mut expansion = spec(&seeds, ExpansionDirection::Outgoing, 2);
            let expected = db
                .beacon_search_graph_filtered(&c.query(), &options(), query, expansion, |id| {
                    id == HIGH
                })
                .unwrap();
            assert_eq!(expected.len(), 1);
            assert_eq!(expected[0].id, HIGH);
            assert_eq!(expected[0].graph_hops, Some(2));
            assert_eq!(expected[0].vector_rank.unwrap().get(), 1);
            assert_eq!(expected[0].text_rank.unwrap().get(), 1);
            assert_eq!(expected[0].graph_rank.unwrap().get(), 1);
            // Two parallel 0->1 edges, 1->HIGH and the 1->1 loop: only four
            // reached incidences. The 128 disconnected loops spend no slots.
            expansion.limits.max_input_edges = 4;
            assert_eq!(
                db.beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &options(),
                    query,
                    expansion,
                    |id| id == HIGH,
                )
                .unwrap(),
                expected
            );
            assert_ne!(
                db.beacon_search_graph_indexed(&c.query(), &options(), query, expansion,)
                    .unwrap()[0]
                    .id,
                HIGH
            );
            // A whole-graph retry would violate this physical source limit.
            assert!(
                db.beacon_search_graph_filtered(&c.query(), &options(), query, expansion, |id| id
                    == HIGH,)
                    .is_err()
            );
            assert!(
                db.beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &options(),
                    query,
                    expansion,
                    |_| false,
                )
                .unwrap()
                .is_empty()
            );
            expansion.limits.max_input_edges = 3;
            assert!(
                db.beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &options(),
                    query,
                    expansion,
                    |id| id == HIGH,
                )
                .is_err()
            );
            assert_eq!(c.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn filtered_indexed_reads_pin_topology_and_scores_to_the_same_history() {
        let ((), report) = run_async_under_lab(0xbeac_3502, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit(), 0).await;
            let basis = db.frontier().unwrap();
            let pinned = db.read_session().unwrap();
            let seeds = [VId(0)];
            let expansion = spec(&seeds, ExpansionDirection::Outgoing, 2);
            let before = pinned
                .beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &options(),
                    query(),
                    expansion,
                    |id| id == HIGH,
                )
                .unwrap();
            let mut change = WriteBatch::new(R);
            change.delete_edge(EId(2));
            change.set_vertex_property(HIGH, PropertyKeyId(2), Some(CanonicalScalar::Int(20)));
            db.write(&c.commit(), change).await.unwrap();
            let after = db
                .beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &options(),
                    query(),
                    expansion,
                    |id| id == HIGH,
                )
                .unwrap();
            assert_ne!(after, before);
            assert_eq!(after[0].graph_hops, None);
            assert_eq!(
                pinned
                    .beacon_search_graph_indexed_filtered(
                        &c.query(),
                        &options(),
                        query(),
                        expansion,
                        |id| id == HIGH,
                    )
                    .unwrap(),
                before
            );
            let mut historic = options();
            historic.as_of = Some(basis);
            assert_eq!(
                db.beacon_search_graph_indexed_filtered(
                    &c.query(),
                    &historic,
                    query(),
                    expansion,
                    |id| id == HIGH,
                )
                .unwrap(),
                before
            );
            assert_eq!(c.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn indexed_fusion_matches_whole_graph_across_directions_windows_and_reopen() {
        let ((), report) = run_async_under_lab(0xbeac_3101, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&c.commit(), vfs.clone(), &path, keys())
                .await
                .unwrap();
            seed(&mut db, &c.commit(), 8).await;
            let seeds = [VId(0), VId(0), VId(999)];
            for direction in [
                ExpansionDirection::Outgoing,
                ExpansionDirection::Incoming,
                ExpansionDirection::Undirected,
            ] {
                for hops in 0..=4 {
                    for include in [false, true] {
                        let mut expansion = spec(&seeds, direction, hops);
                        expansion.include_seeds = include;
                        let expected = db
                            .beacon_search_graph(&c.query(), &options(), query(), expansion)
                            .unwrap();
                        let actual = db
                            .beacon_search_graph_indexed(&c.query(), &options(), query(), expansion)
                            .unwrap();
                        assert_eq!(actual, expected);
                    }
                }
            }
            let expected = db
                .beacon_search_graph_indexed(
                    &c.query(),
                    &options(),
                    query(),
                    spec(&seeds, ExpansionDirection::Outgoing, 2),
                )
                .unwrap();
            drop(db);
            let db = Database::open_with_vfs(&c.commit(), vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(
                db.beacon_search_graph_indexed(
                    &c.query(),
                    &options(),
                    query(),
                    spec(&seeds, ExpansionDirection::Outgoing, 2)
                )
                .unwrap(),
                expected
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn incidence_limit_counts_only_reached_neighborhoods_not_disconnected_edges() {
        let ((), report) = run_async_under_lab(0xbeac_3102, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit(), 128).await;
            let seeds = [VId(0)];
            let mut expansion = spec(&seeds, ExpansionDirection::Outgoing, 1);
            expansion.limits.max_input_edges = 2;
            // The two reached incidences plus the BFS, ranking and fusion
            // state the seed-directed profile retains (0b24aa88): far below
            // what the materialized profile's 128 disconnected edges need.
            expansion.limits.max_source_scratch =
                crate::query::beacon::graph::least_source_scratch(expansion, |spec| {
                    db.beacon_search_graph_indexed(&c.query(), &options(), query(), spec)
                        .is_ok()
                });
            assert!(expansion.limits.max_source_scratch >= 2);
            let expected = db
                .beacon_search_graph(
                    &c.query(),
                    &options(),
                    query(),
                    spec(&seeds, ExpansionDirection::Outgoing, 1),
                )
                .unwrap();
            assert_eq!(
                db.beacon_search_graph_indexed(&c.query(), &options(), query(), expansion)
                    .unwrap(),
                expected
            );
            assert!(
                db.beacon_search_graph(&c.query(), &options(), query(), expansion)
                    .is_err()
            );
            expansion.limits.max_input_edges = 1;
            assert!(matches!(
                db.beacon_search_graph_indexed(&c.query(), &options(), query(), expansion),
                Err(Error::Index(BeaconError::ResourceLimit {
                    resource: "indexed expansion incidence visits",
                    limit: 1,
                }))
            ));
            expansion.limits.max_input_edges = 2;
            expansion.limits.max_source_scratch = 1;
            // One entry cannot even hold the seed's own BFS state, which the
            // shared allowance (0b24aa88) reserves before any incidence.
            assert!(matches!(
                db.beacon_search_graph_indexed(&c.query(), &options(), query(), expansion),
                Err(Error::Index(BeaconError::ResourceLimit {
                    resource: "indexed graph scratch entries",
                    limit: 1,
                }))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn historical_winners_and_pins_never_resurrect_retired_or_newer_edges() {
        let ((), report) = run_async_under_lab(0xbeac_3103, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit(), 0).await;
            let at = db.frontier().unwrap();
            let pinned = db.read_session().unwrap();
            let seeds = [VId(0)];
            let expansion = spec(&seeds, ExpansionDirection::Outgoing, 2);
            let old = pinned
                .beacon_search_graph_indexed(&c.query(), &options(), query(), expansion)
                .unwrap();
            let mut edit = WriteBatch::new(R);
            edit.delete_edge(EId(2));
            edit.set_edge_property(EId(0), PropertyKeyId(99), Some(CanonicalScalar::Int(42)));
            edit.add_edge(EId(u128::MAX), VId(0), VId(9), vec![]);
            db.write(&c.commit(), edit).await.unwrap();
            let current = db
                .beacon_search_graph(&c.query(), &options(), query(), expansion)
                .unwrap();
            assert_eq!(
                db.beacon_search_graph_indexed(&c.query(), &options(), query(), expansion)
                    .unwrap(),
                current
            );
            assert_ne!(current, old);
            let mut historic = options();
            historic.as_of = Some(at);
            assert_eq!(
                db.beacon_search_graph_indexed(&c.query(), &historic, query(), expansion)
                    .unwrap(),
                old
            );
            historic.as_of = Some(CommitSeq(db.frontier().unwrap().0 + 1));
            assert!(matches!(
                pinned.beacon_search_graph_indexed(&c.query(), &historic, query(), expansion),
                Err(Error::Read(ReadError::BeyondFrontier { .. }))
            ));
            drop(db);
            assert_eq!(
                pinned
                    .beacon_search_graph_indexed(&c.query(), &options(), query(), expansion)
                    .unwrap(),
                old
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn selected_label_and_relation_constrain_transit_before_expansion() {
        let ((), report) = run_async_under_lab(0xbeac_3104, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit(), 0).await;
            let mut edit = WriteBatch::new(RelationId(2));
            edit.set_vertex_label(VId(1), LabelId(1), false);
            edit.add_edge(EId(50), VId(0), HIGH, vec![]);
            db.write(&c.commit(), edit).await.unwrap();
            let seeds = [VId(0)];
            for relation in [Some(R), Some(RelationId(2)), None] {
                let mut expansion = spec(&seeds, ExpansionDirection::Outgoing, 2);
                expansion.relation = relation;
                assert_eq!(
                    db.beacon_search_graph_indexed(&c.query(), &options(), query(), expansion)
                        .unwrap(),
                    db.beacon_search_graph(&c.query(), &options(), query(), expansion)
                        .unwrap()
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn admission_counts_parallel_self_loop_and_undirected_incidence_exactly() {
        let ((), report) = run_async_under_lab(0xbeac_3105, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
            seed(&mut db, &c.commit(), 40).await;
            let at = db.frontier().unwrap();
            let seeds = [VId(0)];
            for (direction, hops, expected) in [
                (ExpansionDirection::Outgoing, 1, 2),
                (ExpansionDirection::Outgoing, 2, 4),
                (ExpansionDirection::Incoming, 1, 1),
                (ExpansionDirection::Undirected, 1, 3),
                (ExpansionDirection::Undirected, 2, 9),
            ] {
                let count = Cell::new(0);
                let work = RefCell::new(WorkBudget::new(1_000_000));
                evaluate(
                    &db.snapshot,
                    at,
                    &options(),
                    query(),
                    spec(&seeds, direction, hops),
                    &work,
                    Scan::Metered,
                    |_| Ok(true),
                    |_| true,
                    |_| true,
                    |_| true,
                    || {
                        count.set(count.get() + 1);
                        Ok(())
                    },
                )
                .unwrap();
                assert_eq!(count.get(), expected, "{direction:?}, hops {hops}");
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
