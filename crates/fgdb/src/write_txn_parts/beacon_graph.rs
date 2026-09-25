//! Transaction-local graph/text/vector retrieval over the prepared net.
//! The graph is query scratch, not storage. Only endpoint identities are
//! copied; edge properties are irrelevant to unit-hop traversal.

use super::{Cancel, Control, Options, Shared, WriteTxn, WriteTxnError};
use crate::gql_exec::source::{self, SourceEvent};
use crate::{Database, PendingRow, Snapshot};
use asupersync::fs::Vfs;
use fgdb_beacon::expansion::{ExpansionGraph, ExpansionSpec};
use fgdb_beacon::read::{ReadError, Search};
use fgdb_beacon::{BeaconError, GraphHybridHit, GraphHybridQuery, WorkControl};
use fgdb_delta_types::{DeltaRow, ElementId, RelationId};
use fgdb_types::{EId, QueryCx, VId};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, btree_map::Entry};

type Incidence = (VId, RelationId, VId);

/// Edge history, overlay identities and new edge-read witnesses count against
/// BOTH the shared source allowance and the graph-specific scratch ceiling.
/// No per-lane reset of work or source scratch is possible.
struct EdgeWork<'a, 'cx> {
    shared: &'a RefCell<Control<'cx>>,
    scratch: Cell<usize>,
    limit: usize,
}

impl EdgeWork<'_, '_> {
    fn source(&self, event: SourceEvent) -> Result<(), BeaconError> {
        let scratch = matches!(event, SourceEvent::ScratchEntry);
        self.shared.borrow_mut().source(event)?;
        if scratch {
            let count = self.scratch.get();
            if count == self.limit {
                return Err(BeaconError::ResourceLimit {
                    resource: "transaction expansion source scratch entries",
                    limit: self.limit,
                });
            }
            self.scratch.set(count + 1);
        }
        Ok(())
    }

    fn observe(&self, txn: &WriteTxn, element: ElementId) -> Result<(), BeaconError> {
        self.source(SourceEvent::Work)?;
        if !txn.read_set.borrow().contains(&element) {
            self.source(SourceEvent::ScratchEntry)?;
            txn.read_set.borrow_mut().insert(element);
        }
        Ok(())
    }

    fn replace(
        &self,
        changes: &mut BTreeMap<EId, Option<Incidence>>,
        eid: EId,
        after: Option<Incidence>,
    ) -> Result<(), BeaconError> {
        self.source(SourceEvent::Work)?;
        match changes.entry(eid) {
            Entry::Occupied(mut entry) => {
                entry.insert(after);
            }
            Entry::Vacant(entry) => {
                self.source(SourceEvent::ScratchEntry)?;
                entry.insert(after);
            }
        }
        Ok(())
    }
}

impl WriteTxn {
    /// Fuse graph expansion, text and vectors against this transaction's
    /// pinned basis PLUS its canonical prepared writes, without committing.
    ///
    /// All lanes share one selected vertex corpus and allowance. Staged label
    /// changes constrain transit vertices as well as hits. Edges come from the
    /// historical native visitor plus the already-resolved prepared effects:
    /// ensure aliases are not fabricated, parallel EIds remain distinct and
    /// vertex deletion uses its engine-derived cascade. No edge properties
    /// are cloned or interpreted for this unit-hop graph lane.
    ///
    /// A nonzero-hop, nonempty-seed graph read retains a conservative global
    /// edge-scan witness, including on empty results or subsequent failure.
    /// Observed and normalized-away edge IDs remain read dependencies after
    /// savepoint rollback. This uses the existing FCW validator, not full SSI.
    /// Zero-hop/empty-seed and disabled graph lanes do not scan edges. A
    /// disabled graph lane consumes no graph-specific capacity.
    ///
    /// `as_of` must be absent or equal to the transaction basis. Later commits
    /// cannot change the result but can invalidate transaction completion.
    /// Errors return no prefix and leave the transaction open for retry/abort.
    ///
    /// Privileged embedded API, not a Warden grant. This is bounded resident
    /// execution, not spill, a maintained index or an exhaustive hybrid-answer
    /// certificate. ANN and candidate-limited fusion keep their usual scope.
    pub fn beacon_search_graph<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
    ) -> Result<Vec<GraphHybridHit>, ReadError<WriteTxnError, Cancel>> {
        let graph = RefCell::new(None);
        self.beacon_read(
            database,
            cx,
            options,
            |work| {
                let config = options.config_for_graph(query)?;
                Search::Hybrid(query.source_query()).validate(&config, &mut Shared(work))?;
                if query.graph_enabled() {
                    if expansion.seeds.len() > expansion.limits.max_seed_ids {
                        return Err(BeaconError::ResourceLimit {
                            resource: "expansion seed IDs",
                            limit: expansion.limits.max_seed_ids,
                        });
                    }
                    *graph.borrow_mut() = Some(ExpansionGraph::new(expansion.limits));
                }
                Ok(config)
            },
            |vid, work| {
                if let Some(graph) = graph.borrow_mut().as_mut() {
                    graph.insert_vertex(vid, &mut Shared(work))?;
                }
                Ok(())
            },
            |index, snapshot, work| {
                let hits = if let Some(mut graph) = graph.borrow_mut().take() {
                    if expansion.max_hops != 0 && !expansion.seeds.is_empty() {
                        self.beacon_graph_edges(snapshot, &mut graph, expansion, work)?;
                    }
                    graph.expand(
                        expansion.seeds,
                        expansion.max_hops,
                        expansion.include_seeds,
                        query.graph_candidates,
                        &mut Shared(work),
                    )?
                } else {
                    Vec::new()
                };
                // Do not truncate the two-lane result before graph ranks exist.
                index.hybrid_search_graph(query, &hits, &mut Shared(work))
            },
        )
    }

    fn beacon_graph_edges(
        &self,
        snapshot: &Snapshot,
        graph: &mut ExpansionGraph,
        expansion: ExpansionSpec<'_, RelationId>,
        work: &RefCell<Control<'_>>,
    ) -> Result<(), BeaconError> {
        work.borrow_mut().charge(1)?;
        // This path scans the whole edge source, not just one adjacency. Its
        // negative dependency is recorded BEFORE inspecting any source row.
        self.scanned_edges.set(true);
        let edge_work = EdgeWork {
            shared: work,
            scratch: Cell::new(0),
            limit: expansion.limits.max_source_scratch,
        };
        // Input spelling supplies negative-read dependencies ONLY. It must
        // never be interpreted as an unconditional insertion or deletion.
        for row in self.staged.iter().flat_map(|batch| &batch.rows) {
            edge_work.source(SourceEvent::Work)?;
            match row {
                PendingRow::Edge { eid, .. }
                | PendingRow::DeleteEdge { eid, .. }
                | PendingRow::SetEdgeProperty { eid, .. }
                | PendingRow::CompareAndSet { elem: ElementId::Edge(eid), .. } => {
                    edge_work.observe(self, ElementId::Edge(*eid))?;
                }
                _ => {}
            }
        }
        // Only changed identities are staged. Native winners stream directly
        // into the bounded graph, rather than cloning the entire edge table or
        // reapplying every intent once per edge.
        let mut changes = BTreeMap::new();
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                edge_work.source(SourceEvent::Work)?;
                if coordinate.schema_transition.is_some() {
                    return Err(BeaconError::InvalidQuery(
                        "transaction graph search requires an unchanged schema",
                    ));
                }
                for row in &coordinate.rows {
                    edge_work.source(SourceEvent::Work)?;
                    match row {
                        DeltaRow::CreateEdge { eid, src, relation, dst, .. } => {
                            edge_work.observe(self, ElementId::Edge(*eid))?;
                            edge_work.observe(self, ElementId::Vertex(*src))?;
                            edge_work.replace(&mut changes, *eid, Some((*src, *relation, *dst)))?;
                        }
                        DeltaRow::DeleteEdge { eid, .. } => {
                            edge_work.observe(self, ElementId::Edge(*eid))?;
                            edge_work.replace(&mut changes, *eid, None)?;
                        }
                        DeltaRow::DeleteVertex { vid, sorted_retired_incident_edges, .. } => {
                            edge_work.observe(self, ElementId::Vertex(*vid))?;
                            for &eid in sorted_retired_incident_edges {
                                edge_work.observe(self, ElementId::Edge(eid))?;
                                edge_work.replace(&mut changes, eid, None)?;
                            }
                        }
                        DeltaRow::Property { elem: ElementId::Edge(eid), .. } => {
                            edge_work.observe(self, ElementId::Edge(*eid))?;
                        }
                        DeltaRow::CreateVertex { .. }
                        | DeltaRow::LabelMembership { .. }
                        | DeltaRow::Property { elem: ElementId::Vertex(_), .. } => {}
                        _ => return Err(BeaconError::InvalidQuery(
                            "transaction graph search has no overlay law for this delta",
                        )),
                    }
                }
            }
        }
        let mut emit = |incidence: Incidence| -> Result<(), BeaconError> {
            edge_work.source(SourceEvent::Work)?;
            let (src, relation, dst) = incidence;
            if expansion.relation.is_none_or(|requested| requested == relation)
                && graph.contains(src)
                && graph.contains(dst)
            {
                graph.insert_edge(src, dst, expansion.direction, &mut Shared(work))?;
            }
            Ok(())
        };
        source::visit_edges(
            &snapshot.blocks,
            self.basis,
            &mut |event| edge_work.source(event),
            |edge, _| {
                edge_work.observe(self, ElementId::Edge(edge.eid))?;
                edge_work.observe(self, ElementId::Vertex(edge.src))?;
                let after = changes.remove(&edge.eid)
                    .unwrap_or(Some((edge.src, edge.relation, edge.dst)));
                if let Some(incidence) = after {
                    emit(incidence)?;
                }
                Ok(())
            },
        )?;
        for (_, after) in changes {
            edge_work.source(SourceEvent::Work)?;
            if let Some(incidence) = after {
                emit(incidence)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseKeys, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits};
    use fgdb_beacon::{
        DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
    };
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

    #[test]
    fn prepared_incidence_and_selected_corpus_match_committed_three_lane_search() {
        let ((), report) = run_async_under_lab(0xbeac_3101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query_cx = contexts.query();
            let keys = DatabaseKeys::new([0xd1; 32], DatabaseSecurityNamespaceId([0xd2; 32]), [0xd3; 32]);
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let label = LabelId(1);
            let text = PropertyKeyId(1);
            let x = PropertyKeyId(2);
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 1..=5 {
                seed.create_vertex(VId(id), vec![label], vec![
                    (text, CanonicalScalar::ucs_basic_text("graph").unwrap()),
                    (x, CanonicalScalar::Int(id as i64)),
                ]);
            }
            seed.create_vertex(VId(6), vec![label], vec![]);
            for (eid, src, dst) in [(10, 1, 2), (11, 1, 2), (12, 2, 3),
                (13, 3, 4), (14, 4, 1), (15, 1, 6)] {
                seed.add_edge(EId(eid), VId(src), VId(dst), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let mut changes = WriteBatch::new(RelationId(1));
            changes.ensure_edge_by_triple(EId(100), VId(1), VId(2), vec![]);
            changes.delete_edge(EId(10));
            changes.delete_vertex(VId(3));
            changes.add_edge(EId(20), VId(2), VId(5), vec![]);
            changes.add_edge(EId(21), VId(5), VId(4), vec![]);
            changes.set_vertex_label(VId(4), label, false);
            changes.set_vertex_property(VId(5), x, Some(CanonicalScalar::Int(0)));
            changes.create_vertex(VId(7), vec![label], vec![]);
            changes.add_edge(EId(22), VId(6), VId(7), vec![]);
            changes.add_edge(EId(23), VId(7), VId(5), vec![]);
            changes.add_edge(EId(30), VId(1), VId(5), vec![]);
            changes.delete_edge(EId(30));
            txn.write(&mut db, changes).unwrap();
            let mut options = Options::text(text);
            options.vertex_label = Some(label);
            options.projection.vector = vec![x];
            options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
            let query = GraphHybridQuery {
                retrieval: ExactHybridQuery {
                    vector: &[0.0], text: "graph", k: 8,
                    vector_candidates: 8, text_candidates: 8,
                    vector_mode: VectorSearch::Exact, text_mode: TextMatch::Any,
                    profile: ExactRrfProfile::default(),
                },
                graph_candidates: 8, graph_weight: 100,
            };
            let expansion = ExpansionSpec {
                seeds: &[VId(1)], relation: Some(RelationId(1)),
                direction: ExpansionDirection::Outgoing, max_hops: 4,
                include_seeds: false, limits: ExpansionLimits::default(),
            };
            let actual = txn.beacon_search_graph(&db, &query_cx, &options, query, expansion).unwrap();
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(txn.scanned_edges.get());
            assert!(txn.read_set.borrow().contains(&ElementId::Edge(EId(100))));
            assert!(txn.read_set.borrow().contains(&ElementId::Edge(EId(30))));
            for (id, hops) in [(2, 1), (6, 1), (7, 2), (5, 2)] {
                assert_eq!(actual.iter().find(|hit| hit.id == VId(id)).unwrap().graph_hops, Some(hops));
            }
            assert!(actual.iter().all(|hit| hit.id != VId(3) && hit.id != VId(4)));
            txn.commit(&mut db, &commit).await.unwrap();
            assert_eq!(db.beacon_search_graph(&query_cx, &options, query, expansion).unwrap(), actual);
            assert!(matches!(txn.beacon_search_graph(&db, &query_cx, &options, query, expansion),
                Err(ReadError::Read(WriteTxnError::Finished))));
            assert_eq!(contexts.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
