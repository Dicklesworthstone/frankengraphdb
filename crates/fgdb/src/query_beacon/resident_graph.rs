//! Reusable graph/text/vector retrieval from one coherent resident generation.
//! Membership and incidence are demanded from the retained native indexes;
//! no per-query graph directory, text tokenization or vector build is needed.

use super::*;
use fgdb_beacon::expansion::{
    ExpansionDirection, ExpansionMembership, ExpansionNeighbors, ExpansionSpec,
    expand_from_membership_filtered,
};
use fgdb_beacon::{GraphHybridHit, GraphHybridQuery};
use fgdb_delta_types::RelationId;
use fgdb_gql::{GlaExecutionEvent, algebra::GlaDirection};
use fgdb_types::VId;
use std::cell::Cell;

impl<V: Vfs + Clone> Database<V> {
    /// Build reusable text/vector lanes AND retain their native graph source.
    ///
    /// Shares prepare_beacon_index's builder, projection and admission meter.
    /// The opt-in source pin enables search_graph without consulting a writer,
    /// including when options.as_of selects a historical cut. This retains an
    /// entire decoded immutable generation, not just the query neighborhood.
    /// Ordinary prepare_beacon_index keeps its existing two-lane retention.
    ///
    /// Refresh remains explicit: it advances the search lanes, graph source
    /// and sequence together after final acceptance. Edge-only commits need no
    /// text/vector rebuild. Old snapshots remain coherent after refresh,
    /// compaction, writer fencing or dropping. No durable subscription, graph
    /// authority, capability grant, spill or ANN-equivalence claim is added.
    pub fn prepare_beacon_graph_index(
        &self,
        cx: &QueryCx,
        options: &Options,
    ) -> Result<ResidentIndex, Error> {
        self.prepare_beacon_index_internal(cx, options, true)
    }

    /// Filter a current commit-maintained generation before all candidate
    /// limits. The registry's ordinary owner, health and exact-frontier gate
    /// runs before any predicate call. A failed maintenance tick cannot fall
    /// back to an older pin or rebuild implicitly. This remains privileged;
    /// eligibility never grants graph or property authority.
    #[allow(clippy::too_many_arguments)]
    pub fn search_beacon_graph_index_filtered(
        &self,
        cx: &QueryCx,
        handle: &MaintainedBeaconIndex,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, MaintainedBeaconError> {
        self.beacon_index_snapshot(cx, handle)?
            .search_graph_filtered(cx, query, expansion, policy, eligible)
            .map_err(Into::into)
    }
}

impl ResidentIndex {
    /// Whether this generation retained the native source needed for graph
    /// retrieval. False never causes implicit database access or index rebuild.
    #[must_use]
    pub fn has_graph_source(&self) -> bool {
        self.graph_source.is_some()
    }

    /// Fuse this generation's reused search lanes with native graph expansion.
    /// See PinnedIndex::search_graph for the source, limits and authority contract.
    pub fn search_graph(
        &self,
        cx: &QueryCx,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        self.snapshot().search_graph(cx, query, expansion, policy)
    }

    /// Reuse this generation while filtering result endpoints BEFORE each
    /// source's candidate limit. No source corpus or graph is rebuilt. See
    /// [`PinnedIndex::search_graph_filtered`] for the predicate contract.
    pub fn search_graph_filtered(
        &self,
        cx: &QueryCx,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        self.snapshot()
            .search_graph_filtered(cx, query, expansion, policy, eligible)
    }
}

impl PinnedIndex {
    #[must_use]
    pub fn has_graph_source(&self) -> bool {
        self.graph_source.is_some()
    }

    /// Search all three modalities at source_sequence(), with one allowance.
    /// The source and text/vector generations are paired only by the database;
    /// callers cannot inject a snapshot, document list or source sequence.
    /// Existing vertices without either indexed property can still be transit
    /// vertices and graph hits. Frozen label selection applies to seeds and
    /// BOTH edge endpoints; no property is reprojected during graph traversal.
    ///
    /// Membership uses the native per-vertex history lookup. Incidence uses
    /// the same exact-cut adjacency index as indexed Beacon reads. Complete
    /// bounded BFS and exact candidate fusion use the shared Beacon kernels.
    /// No vertex directory, edge graph, tokenization, corpus statistics or ANN
    /// topology is rebuilt. Each call has fresh work/result/visit allowances;
    /// candidate truncation never hides a later source or control refusal.
    ///
    /// For this demand-driven profile max_vertices AND max_visited_vertices
    /// bound retained visits, not the full selected population. max_input_edges
    /// counts admitted incidence visits (parallel IDs separately; undirected
    /// edges at each expanded endpoint). Source admission counts one per vertex
    /// membership probe and admitted incidence, plus native scratch events;
    /// its cap is min(policy.max_source_scratch, expansion.max_source_scratch).
    /// max_staging_rows is unused: there is no document staging on a search.
    /// These are logical limits, not byte-memory, lifetime retention or spill.
    /// Individual index-history lookups remain synchronous.
    ///
    /// A graph-enabled query on a two-lane-only preparation refuses Disabled;
    /// a disabled graph lane reads no seeds or graph source. Privileged API:
    /// never give this index to Warden clients. Approximate vector search and
    /// candidate-limited fusion keep their original approximation boundaries.
    pub fn search_graph(
        &self,
        cx: &QueryCx,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let work = RefCell::new(Meter::new(policy.max_work_units, |_| cx.checkpoint()));
            let result = self.search_graph_with_work(cx, query, expansion, policy, &work);
            complete(work.into_inner(), result)
        })
    }

    fn search_graph_with_work(
        &self,
        cx: &QueryCx,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
        work: &RefCell<impl WorkControl>,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        self.search_graph_filtered_with_work(cx, query, expansion, policy, |_| true, work)
    }

    /// Filter endpoints without rebuilding documents, ANN topology, corpus
    /// statistics or graph adjacency. Rejected endpoints remain available for
    /// transit and ANN routing; frozen source selection is unchanged. Apply
    /// the predicate before graph, vector and text candidate limits, not to a
    /// truncated final result. Graph-only eligible vertices need no modality.
    ///
    /// The host must supply a stable, bounded, side-effect-free ID predicate.
    /// Derive graph-dependent eligibility at this index's source_sequence(),
    /// not at a writer's newer head. This is NOT an authorization predicate;
    /// the resident index remains privileged, and ANN remains approximate.
    /// Rejected endpoints still count against visit and incidence limits;
    /// reject-all cannot conceal a source, work or final-delivery refusal.
    pub fn search_graph_filtered(
        &self,
        cx: &QueryCx,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(Error::Interrupted)?;
            let work = RefCell::new(Meter::new(policy.max_work_units, |_| cx.checkpoint()));
            let result =
                self.search_graph_filtered_with_work(cx, query, expansion, policy, eligible, &work);
            complete(work.into_inner(), result)
        })
    }

    // Both entrypoints and refusal-cut tests exercise this single execution
    // body; the unfiltered request uses the constant-true result predicate.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_graph_filtered_with_work(
        &self,
        cx: &QueryCx,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        policy: ReadPolicy,
        eligible: impl Fn(VId) -> bool,
        work: &RefCell<impl WorkControl>,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        work.borrow_mut().charge(1)?;
        query.candidate_depths()?;
        if query.retrieval.k > policy.max_result_rows {
            return Err(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: policy.max_result_rows,
            }
            .into());
        }
        let request = Search::Hybrid(query.source_query());
        let (vector, text) = request.lanes();
        if vector && self.definition.index.vector.is_none() {
            return Err(BeaconError::Disabled("vector").into());
        }
        if text && self.definition.index.text.is_none() {
            return Err(BeaconError::Disabled("text").into());
        }
        request.validate(&self.definition.index, &mut SharedWork(work))?;
        let graph = if query.graph_enabled() {
            let view = self
                .graph_source
                .as_ref()
                .ok_or(BeaconError::Disabled("resident graph source"))?;
            // The view can contain a newer head when preparation/refresh chose
            // history. EVERY lookup uses source_sequence, never that newer head.
            let source = view
                .vertex_scan_source(cx, self.source_sequence)
                .map_err(Error::Source)?;
            let budget = SourceBudget {
                used: Cell::new(0),
                limit: policy
                    .max_source_scratch
                    .min(expansion.limits.max_source_scratch),
            };
            let membership = Membership {
                source,
                label: self.definition.vertex_label,
                budget: &budget,
                failure: RefCell::new(None),
            };
            let mut neighbors = Neighbors {
                view,
                at: self.source_sequence,
                membership: &membership,
                budget: &budget,
                relation: expansion.relation,
                direction: match expansion.direction {
                    ExpansionDirection::Outgoing => GlaDirection::Forward,
                    ExpansionDirection::Incoming => GlaDirection::Reverse,
                    ExpansionDirection::Undirected => GlaDirection::Undirected,
                },
                admitted: 0,
                limit: expansion.limits.max_input_edges,
            };
            let result = expand_from_membership_filtered(
                expansion.seeds,
                expansion.max_hops,
                expansion.include_seeds,
                query.graph_candidates,
                expansion.limits,
                &membership,
                &mut neighbors,
                |id, _| Ok(eligible(id)),
                &mut SharedWork(work),
            );
            // The narrow Beacon source interface never flattens a native read
            // failure into a successful omission or a permanent generic error.
            if let Some(error) = membership.failure.borrow_mut().take() {
                return Err(Error::Source(error));
            }
            result?
        } else {
            Vec::new()
        };
        let rows = self.index.hybrid_search_graph_filtered(
            query,
            &graph,
            eligible,
            &mut SharedWork(work),
        )?;
        work.borrow_mut().charge(1)?;
        Ok(rows)
    }
}

struct SourceBudget {
    used: Cell<usize>,
    limit: usize,
}
impl SourceBudget {
    fn reserve(&self) -> Result<(), BeaconError> {
        let used = self.used.get();
        if used >= self.limit {
            return Err(BeaconError::ResourceLimit {
                resource: "resident graph source admissions",
                limit: self.limit,
            });
        }
        self.used.set(used + 1);
        Ok(())
    }
}

struct Membership<'a, S> {
    source: S,
    label: Option<LabelId>,
    budget: &'a SourceBudget,
    failure: RefCell<Option<ReadError>>,
}
impl<S: VertexScanSource<Error = ReadError>> ExpansionMembership for Membership<'_, S> {
    fn contains_vertex(&self, id: VId, work: &mut dyn WorkControl) -> Result<bool, BeaconError> {
        if self.failure.borrow().is_some() {
            return Err(BeaconError::Invariant("resident graph source refused"));
        }
        work.charge(1)?;
        self.budget.reserve()?;
        let row = self.source.vertex(id, &mut |event| {
            work.charge(1)?;
            if matches!(event, VertexScanEvent::ScratchEntry) {
                self.budget.reserve()?;
            }
            Ok::<_, BeaconError>(())
        });
        let row = match row {
            Ok(row) => row,
            Err(VertexScanSourceError::Control(error)) => return Err(error),
            Err(VertexScanSourceError::Source(error)) => {
                *self.failure.borrow_mut() = Some(error);
                return Err(BeaconError::Invariant("resident graph source refused"));
            }
        };
        let Some(row) = row else { return Ok(false) };
        work.charge(row.labels.len())?;
        Ok(self
            .label
            .is_none_or(|label| row.labels.binary_search(&label).is_ok()))
    }
}

// A thin native-index adapter for the membership-driven kernel. No edge bag,
// graph interpreter or cache of caller-asserted topology is introduced.
struct Neighbors<'a, M> {
    view: &'a EmbeddedReadView,
    at: CommitSeq,
    membership: &'a M,
    budget: &'a SourceBudget,
    relation: Option<RelationId>,
    direction: GlaDirection,
    admitted: usize,
    limit: usize,
}
impl<M: ExpansionMembership> ExpansionNeighbors for Neighbors<'_, M> {
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        let snapshot = &self.view.snapshot;
        let mut after = None;
        loop {
            let next = snapshot.adjacency_index().next_incident_edge(
                vertex,
                self.direction,
                after,
                &mut |event| {
                    work.charge(1)?;
                    if matches!(event, GlaExecutionEvent::ScratchEntry) {
                        self.budget.reserve()?;
                    }
                    Ok::<_, BeaconError>(())
                },
            )?;
            let Some(eid) = next else { break };
            after = Some(eid);
            work.charge(1)?;
            let Some((block, row)) =
                snapshot
                    .adjacency_index()
                    .statement_at(&snapshot.blocks, eid, self.at)
            else {
                continue;
            };
            let edge = &snapshot.blocks[block][row];
            let neighbor = match self.direction {
                GlaDirection::Forward if edge.src == vertex => edge.dst,
                GlaDirection::Reverse if edge.dst == vertex => edge.src,
                GlaDirection::Undirected if edge.src == vertex => edge.dst,
                GlaDirection::Undirected if edge.dst == vertex => edge.src,
                _ => continue,
            };
            if self
                .relation
                .is_some_and(|relation| relation != edge.relation)
                || !self.membership.contains_vertex(edge.src, work)?
                || !self.membership.contains_vertex(edge.dst, work)?
            {
                continue;
            }
            work.charge(1)?;
            if self.admitted >= self.limit {
                return Err(BeaconError::ResourceLimit {
                    resource: "indexed expansion incidence visits",
                    limit: self.limit,
                });
            }
            self.budget.reserve()?;
            self.admitted += 1;
            visit(neighbor, work)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "resident_graph_tests.rs"]
mod tests;

#[cfg(test)]
mod filtered_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_beacon::expansion::ExpansionLimits;
    use fgdb_beacon::{
        DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
    };
    use fgdb_types::{
        CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    };

    const R: RelationId = RelationId(1);
    const HIGH: VId = VId(u128::MAX);

    fn options() -> Options {
        let mut options = Options::text(PropertyKeyId(1));
        options.vertex_label = Some(LabelId(1));
        options.projection.vector = vec![PropertyKeyId(2)];
        options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
        options.index.max_segments = 1;
        options
    }
    fn query() -> GraphHybridQuery<'static> {
        GraphHybridQuery {
            retrieval: ExactHybridQuery {
                vector: &[0.0],
                text: "graph",
                k: 1,
                vector_candidates: 1,
                text_candidates: 1,
                vector_mode: VectorSearch::Exact,
                text_mode: TextMatch::Any,
                profile: ExactRrfProfile::default(),
            },
            graph_candidates: 1,
            graph_weight: 100,
        }
    }
    fn expansion() -> ExpansionSpec<'static, RelationId> {
        ExpansionSpec {
            seeds: &[VId(1)],
            relation: Some(R),
            direction: ExpansionDirection::Outgoing,
            max_hops: 2,
            include_seeds: false,
            limits: ExpansionLimits::default(),
        }
    }
    fn policy() -> ReadPolicy {
        ReadPolicy {
            max_staging_rows: 0,
            ..ReadPolicy::default()
        }
    }
    async fn database(cx: &CommitCx, disconnected: bool) -> Database<MemVfs> {
        let keys = DatabaseKeys::new(
            [0xe1; 32],
            DatabaseSecurityNamespaceId([0xe2; 32]),
            [0xe3; 32],
        );
        let mut db = Database::open_memory(cx, keys).await.unwrap();
        let mut batch = WriteBatch::new(R);
        let mut vertices = vec![(VId(1), 0), (VId(2), 1), (HIGH, 2)];
        if disconnected {
            for id in 100..132 {
                vertices.push((VId(id), id as i64));
            }
        }
        for (id, coordinate) in vertices {
            batch.create_vertex(
                id,
                vec![LabelId(1)],
                vec![
                    (
                        PropertyKeyId(1),
                        CanonicalScalar::ucs_basic_text("graph").unwrap(),
                    ),
                    (PropertyKeyId(2), CanonicalScalar::Int(coordinate)),
                ],
            );
        }
        batch.create_vertex(VId(4), vec![LabelId(1)], vec![]);
        if disconnected {
            for id in 101..132 {
                batch.add_edge(EId(id), VId(id - 1), VId(id), vec![]);
            }
        }
        batch.add_edge(EId(1), VId(1), VId(2), vec![]);
        batch.add_edge(EId(2), VId(2), HIGH, vec![]);
        batch.add_edge(EId(3), HIGH, VId(4), vec![]);
        db.write(cx, batch).await.unwrap();
        db
    }

    #[test]
    fn filtered_resident_search_refills_all_lanes_without_building_a_corpus() {
        let ((), report) = run_async_under_lab(0xbeac_5201, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let db = database(&c.commit(), true).await;
            let resident = db
                .prepare_beacon_graph_index(&c.query(), &options())
                .unwrap();
            let pinned = resident.snapshot();
            let stats = pinned.stats();
            let expected = db
                .beacon_search_graph_filtered(&c.query(), &options(), query(), expansion(), |id| {
                    id == HIGH
                })
                .unwrap();
            let mut local = expansion();
            local.limits.max_vertices = 3;
            local.limits.max_visited_vertices = 3;
            local.limits.max_input_edges = 2;
            for _ in 0..3 {
                let rows = resident
                    .search_graph_filtered(&c.query(), query(), local, policy(), |id| id == HIGH)
                    .unwrap();
                assert_eq!(rows, expected);
                assert_eq!(rows[0].id, HIGH);
                assert_eq!(rows[0].graph_hops, Some(2));
                assert_eq!(rows[0].graph_rank.unwrap().get(), 1);
                assert_eq!(rows[0].vector_rank.unwrap().get(), 1);
                assert_eq!(rows[0].text_rank.unwrap().get(), 1);
                assert_eq!(
                    (rows[0].score.numerator(), rows[0].score.denominator()),
                    (102, 61)
                );
                assert_eq!(pinned.stats(), stats);
            }
            assert_eq!(
                pinned
                    .search_graph(&c.query(), query(), local, policy())
                    .unwrap()[0]
                    .id,
                VId(2)
            );
            let mut graph_only = expansion();
            graph_only.max_hops = 3;
            let rows = pinned
                .search_graph_filtered(&c.query(), query(), graph_only, policy(), |id| id == VId(4))
                .unwrap();
            assert_eq!(rows[0].id, VId(4));
            assert_eq!(rows[0].graph_hops, Some(3));
            assert_eq!(rows[0].text_rank, None);
            assert_eq!(rows[0].vector_rank, None);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn filtered_refresh_and_old_pins_keep_topology_and_documents_at_the_same_cut() {
        let ((), report) = run_async_under_lab(0xbeac_5202, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = database(&c.commit(), false).await;
            let mut resident = db
                .prepare_beacon_graph_index(&c.query(), &options())
                .unwrap();
            let old = resident.snapshot();
            let eligible = |id| id == HIGH || id == VId(4);
            let before = old
                .search_graph_filtered(&c.query(), query(), expansion(), policy(), eligible)
                .unwrap();
            let mut change = WriteBatch::new(R);
            change.delete_edge(EId(2));
            change.add_edge(EId(4), VId(2), VId(4), vec![]);
            change.set_vertex_property(HIGH, PropertyKeyId(2), Some(CanonicalScalar::Int(7)));
            let second = db.write(&c.commit(), change).await.unwrap();
            let mut change = WriteBatch::new(R);
            change.delete_vertex(VId(4));
            change.add_edge(EId(5), VId(1), HIGH, vec![]);
            db.write(&c.commit(), change).await.unwrap();
            let mut historical = options();
            historical.as_of = Some(second);
            let expected = db
                .beacon_search_graph_filtered(
                    &c.query(),
                    &historical,
                    query(),
                    expansion(),
                    eligible,
                )
                .unwrap();
            resident
                .refresh(&c.query(), &db, Some(second), ReadPolicy::default())
                .unwrap();
            let middle = resident.snapshot();
            assert_eq!(middle.source_sequence(), second);
            assert_eq!(
                middle
                    .search_graph_filtered(&c.query(), query(), expansion(), policy(), eligible)
                    .unwrap(),
                expected
            );
            assert_eq!(expected[0].id, VId(4));
            assert_eq!(before[0].id, HIGH);
            db.compact(&c.commit()).await.unwrap();
            drop(db);
            assert_eq!(
                old.search_graph_filtered(&c.query(), query(), expansion(), policy(), eligible)
                    .unwrap(),
                before
            );
            assert_eq!(
                middle
                    .search_graph_filtered(&c.query(), query(), expansion(), policy(), eligible)
                    .unwrap(),
                expected
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn result_filtering_cannot_hide_limits_or_enable_an_unprepared_graph() {
        let ((), report) = run_async_under_lab(0xbeac_5203, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let db = database(&c.commit(), false).await;
            let resident = db
                .prepare_beacon_graph_index(&c.query(), &options())
                .unwrap();
            let pinned = resident.snapshot();
            for accept in [false, true] {
                let mut limited = expansion();
                limited.limits.max_visited_vertices = 2;
                assert!(matches!(
                    pinned
                        .search_graph_filtered(&c.query(), query(), limited, policy(), |_| accept),
                    Err(Error::Index(BeaconError::ResourceLimit {
                        resource: "expansion visited vertices",
                        limit: 2
                    }))
                ));
                let mut denied = policy();
                denied.max_result_rows = 0;
                assert!(matches!(
                    pinned.search_graph_filtered(&c.query(), query(), expansion(), denied, |_| {
                        accept
                    }),
                    Err(Error::Index(BeaconError::ResourceLimit {
                        resource: "result rows",
                        limit: 0
                    }))
                ));
            }
            let plain = db.prepare_beacon_index(&c.query(), &options()).unwrap();
            assert!(matches!(
                plain.search_graph_filtered(&c.query(), query(), expansion(), policy(), |_| true),
                Err(Error::Index(BeaconError::Disabled("resident graph source")))
            ));
            let mut two_lanes = query();
            two_lanes.graph_weight = 0;
            let mut disabled = expansion();
            disabled.limits.max_seed_ids = 0;
            disabled.limits.max_source_scratch = 0;
            let rows = plain
                .search_graph_filtered(&c.query(), two_lanes, disabled, policy(), |id| id == HIGH)
                .unwrap();
            assert_eq!(rows[0].id, HIGH);
            assert_eq!(rows[0].graph_rank, None);
            assert!(
                pinned
                    .search_graph_filtered(&c.query(), query(), expansion(), policy(), |_| false)
                    .unwrap()
                    .is_empty()
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn filtered_search_cancellation_at_every_checkpoint_preserves_the_index() {
        #[derive(Default)]
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
        let ((), report) = run_async_under_lab(0xbeac_5204, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let db = database(&c.commit(), false).await;
            let pinned = db
                .prepare_beacon_graph_index(&c.query(), &options())
                .unwrap()
                .snapshot();
            let before = pinned.stats();
            let at = pinned.source_sequence();
            let run = |stop| {
                let work = RefCell::new(Cut { calls: 0, stop });
                let result = pinned.search_graph_filtered_with_work(
                    &c.query(),
                    query(),
                    expansion(),
                    policy(),
                    |id| id == HIGH,
                    &work,
                );
                (result, work.into_inner().calls)
            };
            let (expected, calls) = run(usize::MAX);
            let expected = expected.unwrap();
            assert_eq!(expected[0].id, HIGH);
            for stop in 1..=calls {
                let (result, seen) = run(stop);
                assert!(
                    matches!(result, Err(Error::Index(BeaconError::Cancelled))),
                    "cut {stop}"
                );
                assert_eq!(seen, stop);
            }
            assert_eq!(pinned.source_sequence(), at);
            assert_eq!(pinned.stats(), before);
            assert_eq!(run(usize::MAX).0.unwrap(), expected);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn maintained_filtered_search_checks_freshness_before_predicates_and_keeps_old_pins() {
        let ((), report) = run_async_under_lab(0xbeac_5205, |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = database(&c.commit(), false).await;
            let handle = db
                .register_beacon_graph_index(&c.query(), &options(), ReadPolicy::default())
                .unwrap();
            let denied = ReadPolicy {
                max_work_units: 0,
                ..ReadPolicy::default()
            };
            let fenced = db
                .register_beacon_graph_index(&c.query(), &options(), denied)
                .unwrap();
            let old = db.beacon_index_snapshot(&c.query(), &handle).unwrap();
            let eligible = |id| id == HIGH || id == VId(4);
            let before = db
                .search_beacon_graph_index_filtered(
                    &c.query(),
                    &handle,
                    query(),
                    expansion(),
                    policy(),
                    eligible,
                )
                .unwrap();
            assert_eq!(before[0].id, HIGH);
            let mut change = WriteBatch::new(R);
            change.delete_edge(EId(2));
            change.add_edge(EId(4), VId(2), VId(4), vec![]);
            let second = db.write(&c.commit(), change).await.unwrap();
            let after = db
                .search_beacon_graph_index_filtered(
                    &c.query(),
                    &handle,
                    query(),
                    expansion(),
                    policy(),
                    eligible,
                )
                .unwrap();
            assert_eq!(after[0].id, VId(4));
            assert_eq!(
                db.beacon_index_snapshot(&c.query(), &handle)
                    .unwrap()
                    .source_sequence(),
                second
            );
            assert_eq!(
                old.search_graph_filtered(&c.query(), query(), expansion(), policy(), eligible)
                    .unwrap(),
                before
            );
            let called = Cell::new(false);
            let refused = db.search_beacon_graph_index_filtered(
                &c.query(),
                &fenced,
                query(),
                expansion(),
                policy(),
                |_| {
                    called.set(true);
                    false
                },
            );
            assert!(matches!(
                refused,
                Err(MaintainedBeaconError::Unavailable { .. })
            ));
            assert!(!called.get());
            db.unregister_beacon_index(&c.query(), &handle).unwrap();
            assert!(
                db.search_beacon_graph_index_filtered(
                    &c.query(),
                    &handle,
                    query(),
                    expansion(),
                    policy(),
                    eligible,
                )
                .is_err()
            );
            drop(db);
            assert_eq!(
                old.search_graph_filtered(&c.query(), query(), expansion(), policy(), eligible)
                    .unwrap(),
                before
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
