//! Graph-only retrieval from the capability-visible staged closure. The
//! canonical overlay owns changed rows; seeded access borrows the pinned
//! history and does not scan the complete vertex or edge population.

use super::*;
use fgdb_beacon::expansion::{
    ExpansionMembership, ExpansionNeighbors, expand_from_membership_filtered,
};
use fgdb_delta_types::LabelId;
use fgdb_gql::algebra::{EdgeRelation, GlaDirection};
use std::collections::BTreeMap;

trait DemandMeter: WorkControl {
    fn poll_hidden(&mut self) -> Result<(), BeaconError>;
    fn admit_vertex(&mut self) -> Result<(), BeaconError>;
    fn admit_edge(&mut self) -> Result<(), BeaconError>;
}

impl<Clock: FnMut() -> u64> DemandMeter for HybridMeter<'_, '_, '_, '_, Clock> {
    fn poll_hidden(&mut self) -> Result<(), BeaconError> {
        HybridMeter::poll_hidden(self)
    }

    fn admit_vertex(&mut self) -> Result<(), BeaconError> {
        HybridMeter::admit_vertex(self)
    }

    fn admit_edge(&mut self) -> Result<(), BeaconError> {
        self.observe(SourceEvent::SnapshotRecord)
    }
}

struct Membership<'a, 'overlay, V: Vfs + Clone, W> {
    overlay: &'a OverlayRows<'overlay, V>,
    scope: &'a PlannerPredicates,
    label: Option<LabelId>,
    visible: RefCell<BTreeMap<VId, bool>>,
    limit: usize,
    meter: &'a RefCell<W>,
}

impl<V: Vfs + Clone, W: DemandMeter> Membership<'_, '_, V, W> {
    /// Resolve authority before ANY observable charge. Hidden/absent
    /// identities never enter the visible cache; user label rejection is
    /// retained only after the visible probe consumes its ordinary allowance.
    fn admitted(&self, id: VId) -> Result<Option<(bool, usize)>, BeaconError> {
        self.meter.borrow_mut().poll_hidden()?;
        if let Some(selected) = self.visible.borrow().get(&id).copied() {
            return Ok(Some((selected, 0)));
        }
        let row = self
            .overlay
            .seeded_vertex(id, &mut |_| self.meter.borrow_mut().poll_hidden())?;
        let Some(row) = row else { return Ok(None) };
        if !self.scope.allows_vertex(&row.labels) {
            return Ok(None);
        }
        let selected = self.label.is_none_or(|label| {
            self.scope.allows_label(label) && row.labels.binary_search(&label).is_ok()
        });
        let labels = row
            .labels
            .iter()
            .filter(|&&label| self.scope.allows_label(label))
            .count();
        Ok(Some((selected, labels)))
    }

    fn retain(
        &self,
        id: VId,
        selected: bool,
        labels: usize,
        work: &mut dyn WorkControl,
    ) -> Result<bool, BeaconError> {
        let entries = self.visible.borrow().len();
        work.charge(1 + entries.checked_ilog2().unwrap_or(0) as usize)?;
        if let Some(selected) = self.visible.borrow().get(&id).copied() {
            return Ok(selected);
        }
        self.meter.borrow_mut().admit_vertex()?;
        work.charge(labels)?;
        if entries >= self.limit {
            return Err(BeaconError::ResourceLimit {
                resource: "authorized staged demand vertices",
                limit: self.limit,
            });
        }
        work.reserve_scratch(1)?;
        self.visible.borrow_mut().insert(id, selected);
        Ok(selected)
    }
}

impl<V: Vfs + Clone, W: DemandMeter> ExpansionMembership for Membership<'_, '_, V, W> {
    fn contains_vertex(&self, id: VId, work: &mut dyn WorkControl) -> Result<bool, BeaconError> {
        let Some((selected, labels)) = self.admitted(id)? else {
            return Ok(false);
        };
        self.retain(id, selected, labels, work)
    }
}

struct Neighbors<'a, 'source, 'overlay, V: Vfs + Clone, W> {
    membership: &'a Membership<'source, 'overlay, V, W>,
    relation: EdgeRelation,
    direction: GlaDirection,
    enabled: bool,
    admitted: usize,
    limit: usize,
}

impl<V: Vfs + Clone, W: DemandMeter> ExpansionNeighbors for Neighbors<'_, '_, '_, V, W> {
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        if !self.enabled {
            return Ok(());
        }
        let membership = self.membership;
        let direction = self.direction;
        membership
            .overlay
            .visit_seeded_incidence(
                vertex,
                &self.relation,
                direction,
                &mut |_| membership.meter.borrow_mut().poll_hidden(),
                |(_, src, relation, dst), _, _| {
                    if !membership.scope.allows_relation(relation) {
                        return Ok(());
                    }
                    let next = match direction {
                        GlaDirection::Forward if src == vertex => dst,
                        GlaDirection::Reverse if dst == vertex => src,
                        GlaDirection::Undirected if src == vertex => dst,
                        GlaDirection::Undirected if dst == vertex => src,
                        _ => return Ok(()),
                    };
                    // BFS admitted the anchor through this same immutable
                    // membership. Mask the other endpoint before billing the
                    // edge OR touching its visible cache: a hidden bridge must
                    // neither extend the closure nor consume its allowance.
                    let Some((selected, labels)) = membership.admitted(next)? else {
                        return Ok(());
                    };
                    if !membership.retain(next, selected, labels, work)? {
                        return Ok(());
                    }
                    membership.meter.borrow_mut().admit_edge()?;
                    if self.admitted >= self.limit {
                        return Err(BeaconError::ResourceLimit {
                            resource: "authorized staged demand incidence visits",
                            limit: self.limit,
                        });
                    }
                    // Count admitted physical incidences, including parallel
                    // EIds and each expanded endpoint of an undirected edge.
                    // The canonical source offers a self-loop only once.
                    work.reserve_scratch(1)?;
                    self.admitted += 1;
                    visit(next, work)
                },
            )
            .expect("enabled demand retains the canonical edge overlay")
    }
}

/// One signed/native allowance covers membership, reached incidences, BFS,
/// eligibility, fusion and output. No full corpus or projected property index
/// is built. The existing sparse overlay may still own changed-row payloads;
/// host cursors and conservative read witnesses are not a process-memory cap.
#[allow(clippy::too_many_arguments)]
pub(super) fn evaluate<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    cx: &QueryCx,
    call: &PreparedProcedureCall,
    search: &crate::query::HybridSearch<'_>,
    eligible: Option<&[GraphValue]>,
    scope: &PlannerPredicates,
    policy: GqlQueryPolicy,
    execution: &RefCell<&mut Execution<'_, '_, Clock>>,
) -> Selected<GraphValueRow> {
    let options = search.options();
    let query = search.query();
    let expansion = search.expansion();
    let reads_edges = query.graph_enabled()
        && expansion.max_hops != 0
        && !expansion.seeds.is_empty()
        && expansion
            .relation
            .is_none_or(|relation| scope.allows_relation(relation));
    // Seeded row access retains observed identities, but an empty adjacency
    // also conveys an insertion gap. Preserve the established allocation-free
    // ALL-CHANGE witness on SUCCESS as well as refusal/negative probes. This
    // deliberately remains conservative rather than claiming precise SSI.
    let _attempt = ProjectionReadAttempt {
        reads: &transaction.point_reads,
        element: ElementId::Vertex(expansion.seeds.first().copied().unwrap_or(VId(0))),
        accepted: false,
    };
    with_overlay(
        transaction,
        database,
        cx,
        reads_edges,
        true,
        execution,
        |overlay| {
            let overlay = overlay.expect("demand retains even an empty canonical overlay");
            let usage = RefCell::new(AdmissionUsage::default());
            let work = RefCell::new(HybridMeter {
                meter: ProcedureMeter {
                    cx,
                    execution,
                    usage: &usage,
                    policy,
                },
                scratch: 0,
                scratch_limit: options
                    .policy
                    .max_source_scratch
                    .min(expansion.limits.max_source_scratch),
                failure: None,
                refusal: None,
            });
            let result = (|| {
                work.borrow_mut().charge(1)?;
                options.validate_graph_only(query)?;
                let filter = crate::query::EndpointFilter::prepare_with_admission(
                    eligible,
                    query.retrieval.k,
                    policy.evaluator.max_scratch_entries,
                    &mut HybridWork(&work),
                    || work.borrow_mut().reserve_scratch(1),
                )?;
                let membership = Membership {
                    overlay,
                    scope,
                    label: options.vertex_label,
                    visible: RefCell::new(BTreeMap::new()),
                    limit: options
                        .policy
                        .max_staging_rows
                        .min(options.index.max_documents)
                        .min(expansion.limits.max_vertices),
                    meter: &work,
                };
                let mut neighbors = Neighbors {
                    membership: &membership,
                    relation: expansion
                        .relation
                        .map_or(EdgeRelation::Any, EdgeRelation::One),
                    direction: match expansion.direction {
                        ExpansionDirection::Outgoing => GlaDirection::Forward,
                        ExpansionDirection::Incoming => GlaDirection::Reverse,
                        ExpansionDirection::Undirected => GlaDirection::Undirected,
                    },
                    enabled: reads_edges,
                    admitted: 0,
                    limit: expansion.limits.max_input_edges,
                };
                let graph_hits = if query.graph_enabled() {
                    expand_from_membership_filtered(
                        expansion.seeds,
                        expansion.max_hops,
                        expansion.include_seeds,
                        query.graph_candidates,
                        expansion.limits,
                        &membership,
                        &mut neighbors,
                        |id, _| Ok(filter.admits(id, &work)),
                        &mut HybridWork(&work),
                    )?
                } else {
                    Vec::new()
                };
                let hits = filter.finish(query.fuse_graph_only(
                    &graph_hits,
                    |id| filter.admits(id, &work),
                    &mut HybridWork(&work),
                ))?;
                for _ in &hits {
                    work.borrow_mut()
                        .reserve_scratch(1 + call.outputs().len())?;
                }
                work.borrow_mut().charge(1)?;
                Ok(hits)
            })();
            let HybridMeter {
                mut meter, failure, ..
            } = work.into_inner();
            if let Some(error) = failure {
                return Err(error);
            }
            let hits = result.map_err(|error| {
                crate::query::hybrid_refusal(crate::query::HybridCallError::Index(error))
                    .map_source(WriteTxnError::Gql)
            })?;
            let value = search.rows(hits);
            meter.poll()?;
            let usage = usage.into_inner();
            usage.finish(
                policy,
                Ok(GqlQueryExecution {
                    rows: GqlExecutionStats {
                        snapshot_records: usage.snapshot_records(),
                        result_rows: value.len() as u64,
                    },
                    evaluator: GlaExecutionStats::default(),
                    value,
                }),
            )
        },
    )
}
