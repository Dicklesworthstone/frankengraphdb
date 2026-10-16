//! Graph-only reads of a transaction overlay without a whole vertex directory.
//! Prepared membership effects are borrowed once; native histories are resolved
//! only for probed identities. The indexed source still owns the edge merge.

use super::super::super::{Overlay, candidate};
use super::*;
use crate::write_txn::ProjectionReadAttempt;
use fgdb_beacon::WorkBudget;
use fgdb_beacon::expansion::{ExpansionMembership, expand_from_membership_filtered};
use fgdb_delta_types::LabelId;

struct Membership<'a, 'cx> {
    txn: &'a WriteTxn,
    snapshot: &'a Snapshot,
    work: &'a RefCell<Control<'cx>>,
    label: Option<LabelId>,
    overlay: BTreeMap<VId, Overlay<'a>>,
    // This is a privileged transaction source: absent and label-rejected
    // probes are cached too, and remain negative/positive read witnesses.
    cache: RefCell<BTreeMap<VId, bool>>,
    limit: usize,
}

fn observe(txn: &WriteTxn, id: VId, work: &RefCell<Control<'_>>) -> Result<(), BeaconError> {
    let element = ElementId::Vertex(id);
    work.borrow_mut().charge(1)?;
    if !txn.read_set.borrow().contains(&element) {
        work.borrow_mut().source(SourceEvent::ScratchEntry)?;
        txn.read_set.borrow_mut().insert(element);
    }
    Ok(())
}

impl<'a, 'cx> Membership<'a, 'cx> {
    fn prepare(
        txn: &'a WriteTxn,
        snapshot: &'a Snapshot,
        options: &Options,
        expansion: ExpansionSpec<'_, RelationId>,
        work: &'a RefCell<Control<'cx>>,
    ) -> Result<Self, BeaconError> {
        // Match the ordinary overlay's normalized-away identity observations.
        // Input spelling is NEVER used to reconstruct an effective vertex.
        for pending in txn.staged.iter().flat_map(|batch| &batch.rows) {
            work.borrow_mut().charge(1)?;
            if let PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. } = pending {
                observe(txn, *vid, work)?;
            }
        }
        let mut overlay = BTreeMap::new();
        if let Some(prepared) = &txn.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                work.borrow_mut().charge(1)?;
                if coordinate.schema_transition.is_some() {
                    return Err(BeaconError::InvalidQuery(
                        "transaction graph search requires an unchanged schema",
                    ));
                }
                for effect in &coordinate.rows {
                    work.borrow_mut().charge(1)?;
                    let id = match effect {
                        DeltaRow::CreateVertex { vid, .. }
                        | DeltaRow::DeleteVertex { vid, .. }
                        | DeltaRow::LabelMembership { vid, .. } => *vid,
                        // Properties cannot change graph-only membership.
                        // Edges, including cascades, use the shared indexed
                        // source's canonical net when adjacency is demanded.
                        DeltaRow::CreateEdge { .. }
                        | DeltaRow::DeleteEdge { .. }
                        | DeltaRow::Property { .. } => continue,
                        _ => {
                            return Err(BeaconError::InvalidQuery(
                                "transaction demand search has no membership law for this delta",
                            ));
                        }
                    };
                    observe(txn, id, work)?;
                    candidate(&mut overlay, id, work)?.apply(effect, work)?;
                }
            }
        }
        Ok(Self {
            txn,
            snapshot,
            work,
            label: options.vertex_label,
            overlay,
            cache: RefCell::new(BTreeMap::new()),
            limit: options
                .policy
                .max_staging_rows
                .min(options.index.max_documents)
                .min(expansion.limits.max_vertices),
        })
    }

    fn selected(&self, id: VId) -> Result<bool, BeaconError> {
        // Read the original basis, never the writer's latest row. Creation and
        // retirement in the prepared net override it; only labels are touched.
        self.work
            .borrow_mut()
            .charge(tree_work(self.overlay.len()))?;
        let overlay = self.overlay.get(&id);
        if overlay.is_some_and(|row| row.deleted) {
            return Ok(false);
        }
        let labels = if let Some((labels, _)) = overlay.and_then(|row| row.created) {
            labels
        } else {
            let row = source::find_vertex(self.snapshot, id, self.txn.basis, &mut |event| {
                self.work.borrow_mut().source(event)
            })?;
            let Some(row) = row else { return Ok(false) };
            &row.labels
        };
        self.work.borrow_mut().record()?;
        self.work.borrow_mut().charge(labels.len())?;
        let Some(label) = self.label else {
            return Ok(true);
        };
        if let Some(overlay) = overlay {
            self.work
                .borrow_mut()
                .charge(tree_work(overlay.labels.len()))?;
            if let Some(selected) = overlay.labels.get(&label) {
                return Ok(*selected);
            }
        }
        Ok(labels.binary_search(&label).is_ok())
    }
}

impl ExpansionMembership for Membership<'_, '_> {
    fn contains_vertex(&self, id: VId, work: &mut dyn WorkControl) -> Result<bool, BeaconError> {
        let (cached, count) = {
            let cache = self.cache.borrow();
            (cache.get(&id).copied(), cache.len())
        };
        work.charge(tree_work(count))?;
        if let Some(selected) = cached {
            return Ok(selected);
        }
        // Preserve both lifetime and requested label, including absence and
        // rejected transit. Do not observe properties or unexamined adjacency.
        // Raw/staged input dependencies in prepare() retain their broad law.
        observe_field(
            self.txn,
            ElementId::Vertex(id),
            PointReadField::VertexExistence,
            self.work,
        )?;
        if let Some(label) = self.label {
            observe_field(
                self.txn,
                ElementId::Vertex(id),
                PointReadField::Label(label),
                self.work,
            )?;
        }
        if count >= self.limit {
            return Err(BeaconError::ResourceLimit {
                resource: "transaction demand membership entries",
                limit: self.limit,
            });
        }
        let selected = self.selected(id)?;
        self.work.borrow_mut().source(SourceEvent::ScratchEntry)?;
        self.cache.borrow_mut().insert(id, selected);
        Ok(selected)
    }
}

impl Selection for Membership<'_, '_> {
    fn retain_staged(&self, _: VId, _: VId) -> bool {
        // Index only prepared incidences here, not their endpoint histories.
        // Unreached staged edges must not force vertex membership probes.
        true
    }

    fn contains_vertex(&self, id: VId, work: &mut dyn WorkControl) -> Result<bool, BeaconError> {
        ExpansionMembership::contains_vertex(self, id, work)
    }
}

impl WriteTxn {
    /// Graph-only retrieval from this transaction's basis plus prepared net,
    /// without scanning or retaining the full committed vertex population.
    /// Text/vector queries refuse; no modality or staged write is discarded.
    ///
    /// Membership metadata costs O(prepared membership effects + probed IDs).
    /// The same indexed edge merge handles ENSURE aliases, parallel EIds,
    /// tombstones and engine-derived cascades. Zero-hop/absent seeds do not
    /// demand an edge source. Properties are neither projected nor cloned.
    ///
    /// Every probed identity, including absent seeds and rejected transit,
    /// retains lifetime and selected-label dependencies. Every opened adjacency
    /// retains its direction/relation insertion gap, even when empty; every
    /// observed edge retains topology including cascade retirement. Unrelated
    /// properties, components and unexpanded last-hop edges do not conflict
    /// solely because this search ran. Prior broad reads are never narrowed.
    /// Refusal or unwind after source admission retains the existing ALL-CHANGE
    /// witness. Reads survive rollback and use the original-basis FCW validator
    /// for finish/refresh/rebase; this is not full SSI or a new certificate.
    ///
    /// max_staging_rows/max_documents/max_vertices cap cached membership
    /// probes, including absences. Work and source scratch cover preparation,
    /// probes, edge changes, witnesses, BFS and fusion without per-lane resets.
    /// The source cap is the minimum of read and expansion scratch limits.
    /// These are logical limits over decoded resident storage, not spill/RSS.
    /// The predicate is bounded, deterministic host computation, not authority.
    #[allow(clippy::too_many_arguments)]
    pub fn beacon_search_graph_demand_filtered<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        eligible: impl Fn(VId) -> bool,
    ) -> Result<Vec<GraphHybridHit>, ReadError<WriteTxnError, Cancel>> {
        self.beacon_search_graph_demand_observed(
            database,
            cx,
            options,
            query,
            expansion,
            None,
            |_| Ok(()),
            |id, _| eligible(id),
            |result| result,
        )
    }

    /// Native CALL shares the source, observer and first-refusal filter owner.
    #[allow(clippy::too_many_arguments)]
    pub(in super::super::super) fn beacon_search_graph_demand_observed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
        observer: Option<Observer<'_>>,
        prepare_filter: impl FnOnce(&RefCell<Control<'_>>) -> Result<(), BeaconError>,
        eligible: impl Fn(VId, &RefCell<Control<'_>>) -> bool,
        finish_filter: impl FnOnce(
            Result<Vec<GraphHybridHit>, BeaconError>,
        ) -> Result<Vec<GraphHybridHit>, BeaconError>,
    ) -> Result<Vec<GraphHybridHit>, ReadError<WriteTxnError, Cancel>> {
        cx.with_restriction(|| {
            cx.checkpoint().map_err(ReadError::Interrupted)?;
            self.ensure_database(database).map_err(ReadError::Read)?;
            if options.as_of.is_some_and(|at| at != self.basis) {
                return Err(ReadError::Index(BeaconError::InvalidQuery(
                    "transaction search must use the pinned transaction basis",
                )));
            }
            let view = database
                .read_session()
                .map_err(|error| ReadError::Read(error.into()))?;
            view.snapshot
                .check_frontier(self.basis)
                .map_err(|error| ReadError::Read(error.into()))?;
            let work = RefCell::new(Control {
                cx,
                budget: WorkBudget::new(options.policy.max_work_units),
                scratch: 0,
                limit: options
                    .policy
                    .max_source_scratch
                    .min(expansion.limits.max_source_scratch),
                interrupted: None,
                observer,
                refusal: None,
            });
            let result = (|| {
                work.borrow_mut().charge(1)?;
                options.validate_graph_only(query)?;
                if expansion.seeds.len() > expansion.limits.max_seed_ids {
                    return Err(BeaconError::ResourceLimit {
                        resource: "expansion seed IDs",
                        limit: expansion.limits.max_seed_ids,
                    });
                }
                prepare_filter(&work)?;
                let mut attempt = ProjectionReadAttempt {
                    reads: &self.point_reads,
                    element: ElementId::Vertex(expansion.seeds.first().copied().unwrap_or(VId(0))),
                    accepted: false,
                };
                let result = (|| {
                    let membership =
                        Membership::prepare(self, &view.snapshot, options, expansion, &work)?;
                    let mut neighbors = OverlayNeighbors {
                        txn: self,
                        snapshot: &view.snapshot,
                        selected: &membership,
                        observation: Observation::ReachedTopology,
                        relation: expansion.relation,
                        direction: match expansion.direction {
                            ExpansionDirection::Outgoing => GlaDirection::Forward,
                            ExpansionDirection::Incoming => GlaDirection::Reverse,
                            ExpansionDirection::Undirected => GlaDirection::Undirected,
                        },
                        edge_work: EdgeWork {
                            shared: &work,
                            scratch: Cell::new(0),
                            limit: expansion.limits.max_source_scratch,
                        },
                        changes: None,
                        admitted: 0,
                        limit: expansion.limits.max_input_edges,
                    };
                    let hits = expand_from_membership_filtered(
                        expansion.seeds,
                        expansion.max_hops,
                        expansion.include_seeds,
                        query.graph_candidates,
                        expansion.limits,
                        &membership,
                        &mut neighbors,
                        |id, _| Ok(eligible(id, &work)),
                        &mut Shared(&work),
                    )?;
                    let rows = query.fuse_graph_only(
                        &hits,
                        |id| eligible(id, &work),
                        &mut Shared(&work),
                    )?;
                    work.borrow_mut().charge(1)?;
                    Ok(rows)
                })();
                let result = finish_filter(result);
                attempt.accepted = result.is_ok() && work.borrow().refusal.is_none();
                result
            })();
            let control = work.into_inner();
            match control.interrupted {
                Some(error) => Err(ReadError::Interrupted(error)),
                None => match control.refusal {
                    Some(error) => Err(ReadError::Index(error)),
                    None => result.map_err(ReadError::Index),
                },
            }
        })
    }
}

#[cfg(test)]
#[path = "demand_tests.rs"]
mod tests;
