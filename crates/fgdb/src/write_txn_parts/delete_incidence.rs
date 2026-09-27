// Non-detaching DELETE proves only its selected incidence. The admitted native
// snapshot index and canonical net effects remain the topology authorities.

use crate::gql_exec::source::SourceEvent as DeleteSourceEvent;

type DeleteAdmissionFault<C> =
    fgdb_gql::GqlQueryError<fgdb_gql::GraphDeleteError<WriteTxnError>, C>;

struct DeleteAdmission<Checkpoint> {
    policy: fgdb_gql::GraphDeletePolicy,
    stats: fgdb_gql::GraphDeleteStats,
    checkpoint: Checkpoint,
}

impl<Checkpoint> DeleteAdmission<Checkpoint> {
    fn observe<C>(&mut self, event: DeleteSourceEvent) -> Result<(), DeleteAdmissionFault<C>>
    where
        Checkpoint: FnMut() -> Result<(), C>,
    {
        use fgdb_gql::{GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlQueryError};
        (self.checkpoint)().map_err(GqlQueryError::Interrupted)?;
        let mut next = self.stats;
        if event == DeleteSourceEvent::SnapshotRecord {
            next.selection.snapshot_records = next
                .selection
                .snapshot_records
                .checked_add(1)
                .ok_or(GqlQueryError::Source(
                    fgdb_gql::GraphDeleteError::InvalidSourceStatistics,
                ))?;
            self.policy
                .query
                .rows
                .check(GqlBudgetDimension::SnapshotRecords, next.selection.snapshot_records)
                .map_err(GqlQueryError::Rows)?;
        }
        let work = u128::from(next.evaluator.work_units) + 1;
        let scratch = u128::from(next.evaluator.scratch_entries)
            + u128::from(event == DeleteSourceEvent::ScratchEntry);
        for (observed, limit, dimension) in [
            (work, self.policy.query.evaluator.max_work_units, GlaLimitDimension::WorkUnits),
            (scratch, self.policy.query.evaluator.max_scratch_entries, GlaLimitDimension::ScratchEntries),
        ] {
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension, limit, observed }));
            }
        }
        next.evaluator.work_units = work as u64;
        next.evaluator.scratch_entries = scratch as u64;
        self.stats = next;
        Ok(())
    }
}

impl WriteTxn {
    // One completion body after the real GQL collector freezes sorted targets.
    // The test seam changes only interruption, not source or staging behavior.
    fn stage_delete_targets_controlled<V: Vfs + Clone, C>(
        &mut self,
        database: &mut Database<V>,
        relation: RelationId,
        stats: fgdb_gql::GraphDeleteStats,
        (targets, edge_targets): (Vec<VId>, Vec<EId>),
        policy: fgdb_gql::GraphDeletePolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<WithAffectedIds<fgdb_gql::GraphDeleteStats>, DeleteAdmissionFault<C>> {
        use fgdb_gql::{GqlQueryError, GraphDeleteError};
        let source = |error| GqlQueryError::Source(GraphDeleteError::Source(error));
        self.ensure_database(database).map_err(source)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(source)?;
        if live != self.basis {
            return Err(source(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live }));
        }
        if targets.is_empty() && edge_targets.is_empty() {
            return Ok((stats, targets, edge_targets));
        }
        let mut admission = DeleteAdmission { policy, stats, checkpoint };
        self.prove_delete_incidence(database, &targets, &edge_targets, &mut |event| {
            admission.observe(event)
        })?;
        let mut batch = WriteBatch::new(relation);
        // Explicit edges precede vertices. Native preparation still derives the
        // cascade and before-images; this proof never manufactures a delta row.
        for edge in &edge_targets {
            admission.observe(DeleteSourceEvent::ScratchEntry)?;
            batch.delete_edge(*edge);
        }
        for vertex in &targets {
            admission.observe(DeleteSourceEvent::ScratchEntry)?;
            batch.delete_vertex(*vertex);
        }
        admission.observe(DeleteSourceEvent::Work)?;
        self.write(database, batch).map_err(source)?;
        // No fallible work follows ordinary atomic staging.
        Ok((admission.stats, targets, edge_targets))
    }

    fn prove_delete_incidence<V: Vfs + Clone, C>(
        &self,
        database: &Database<V>,
        targets: &[VId],
        edge_targets: &[EId],
        event: &mut impl FnMut(DeleteSourceEvent) -> Result<(), DeleteAdmissionFault<C>>,
    ) -> Result<(), DeleteAdmissionFault<C>> {
        use fgdb_delta_types::DeltaRow;
        use fgdb_gql::algebra::GlaDirection;
        use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GraphDeleteError};
        use std::collections::{BTreeMap, BTreeSet};
        let source = |error| GqlQueryError::Source(GraphDeleteError::Source(error));
        self.ensure_database(database).map_err(source)?;
        database.ensure_readable().map_err(WriteTxnError::from).map_err(source)?;
        database.snapshot.check_frontier(self.basis)
            .map_err(WriteTxnError::from).map_err(source)?;
        let Some(&anchor) = targets.first() else { return Ok(()); };
        // Health/ownership and cancellation before traversal reveal no graph
        // content. Once traversal starts, even a refused budget can reveal it.
        event(DeleteSourceEvent::Work)?;
        let mut attempt = AdjacencyReadAttempt {
            reads: &self.point_reads,
            anchor,
            accepted: false,
        };
        // Reuse the allocation-free refusal witness. A quota failure/unwind
        // cannot lose a partially observed topology when a precise set is full.
        // Success needs every relation and direction at each target; a broad
        // vertex witness supplies that insertion domain, including an empty one.
        for &vertex in targets {
            event(DeleteSourceEvent::ScratchEntry)?;
            self.read_set.borrow_mut().insert(ElementId::Vertex(vertex));
        }
        let touches = |src, dst| targets.binary_search(&src).is_ok()
            || targets.binary_search(&dst).is_ok();
        let incident = || GqlQueryError::Source(GraphDeleteError::IncidentRelationships);
        // Only net topology edits are owned. Do not copy edge/vertex properties,
        // replay raw ENSURE/CAS instructions, or clone the durable edge map.
        let mut overlay = BTreeMap::<EId, Option<(VId, VId)>>::new();
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                event(DeleteSourceEvent::Work)?;
                for effect in &coordinate.rows {
                    event(DeleteSourceEvent::Work)?;
                    let (eid, value) = match effect {
                        DeltaRow::CreateEdge { eid, src, dst, .. } => {
                            (*eid, Some((*src, *dst)))
                        }
                        DeltaRow::DeleteEdge { eid, .. } => (*eid, None),
                        DeltaRow::DeleteVertex { sorted_retired_incident_edges, .. } => {
                            for &eid in sorted_retired_incident_edges {
                                event(DeleteSourceEvent::ScratchEntry)?;
                                overlay.insert(eid, None);
                            }
                            continue;
                        }
                        _ => continue,
                    };
                    event(DeleteSourceEvent::ScratchEntry)?;
                    overlay.insert(eid, value);
                }
            }
        }
        let snapshot = &database.snapshot;
        let index = &snapshot.adjacency_index;
        let mut seen = BTreeSet::new();
        for &vertex in targets {
            event(DeleteSourceEvent::Work)?;
            let mut after = None;
            while let Some(eid) = index.next_incident_edge(
                vertex, GlaDirection::Undirected, after, &mut |kind| event(match kind {
                    GlaExecutionEvent::ScratchEntry => DeleteSourceEvent::ScratchEntry,
                    GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => DeleteSourceEvent::Work,
                }),
            )? {
                // Strict successor handles EId(0) and EId(u128::MAX) without
                // arithmetic. Historical candidates are never visible rows.
                after = Some(eid);
                event(DeleteSourceEvent::Work)?;
                let Some((block, row)) = index.statement_at(&snapshot.blocks, eid, self.basis) else {
                    continue;
                };
                let edge = &snapshot.blocks[block][row];
                if (edge.src != vertex && edge.dst != vertex) || seen.contains(&eid) {
                    continue;
                }
                event(DeleteSourceEvent::SnapshotRecord)?;
                event(DeleteSourceEvent::ScratchEntry)?;
                seen.insert(eid);
                event(DeleteSourceEvent::ScratchEntry)?;
                // Retain even a staged tombstone: rollback cannot erase the
                // proof's observation of the original edge or its lifetime.
                self.read_set.borrow_mut().insert(ElementId::Edge(eid));
                let final_edge = overlay.get(&eid).copied().unwrap_or(Some((edge.src, edge.dst)));
                if final_edge.is_some_and(|(src, dst)| touches(src, dst))
                    && edge_targets.binary_search(&eid).is_err()
                {
                    return Err(incident());
                }
            }
        }
        // Staged creations need not occur in the durable incidence index.
        // The same sorted canonical overlay supplies them; no source-row charge
        // is invented for a row which has never appeared in the snapshot.
        for (eid, endpoints) in overlay {
            event(DeleteSourceEvent::Work)?;
            if endpoints.is_some_and(|(src, dst)| touches(src, dst)) {
                event(DeleteSourceEvent::ScratchEntry)?;
                self.read_set.borrow_mut().insert(ElementId::Edge(eid));
                if edge_targets.binary_search(&eid).is_err() {
                    return Err(incident());
                }
            }
        }
        event(DeleteSourceEvent::Work)?;
        attempt.accepted = true;
        Ok(())
    }
}

#[cfg(test)]
mod delete_incidence_tests {
    include!("delete_incidence_tests.rs");
}
