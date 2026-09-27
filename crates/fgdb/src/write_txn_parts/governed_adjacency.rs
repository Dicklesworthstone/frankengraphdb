// Resource-controlled adjacency over the existing exact-cut index and canonical
// effects. Ordinary and governed reads share every source and overlay decision.

use crate::gql_exec::source::SourceEvent as AdjacencySourceEvent;

enum AdjacencyReadEvent {
    Source(AdjacencySourceEvent),
    ResultRow,
}

impl WriteTxn {
    /// Read outgoing neighbours with one source/work/scratch/result allowance.
    ///
    /// The exact-cut index, canonical overlay, parallel-edge rule and topology
    /// witnesses are the same as neighbours(). Source records count matching
    /// live basis edges, before staged effects; result rows count distinct final
    /// neighbours. Staged creations consume work/scratch, not snapshot records.
    /// Every historical candidate, effect, cascade element, temporary entry,
    /// witness admission and output row crosses the shared control seam.
    /// Counters are per invocation and do not become cheaper on a repeated read.
    ///
    /// No partial result is returned. Refusal or unwind after traversal starts
    /// retains an allocation-free ALL-CHANGE witness: later validation may
    /// conservatively abort on unrelated writes rather than lose observations
    /// encoded by a resource failure. It never erases earlier witnesses, staged
    /// effects, savepoints or the pin. Wrong-owner, unhealthy and pre-traversal
    /// cancellation refusals do not install this witness. Rollback cannot clear
    /// it; normal terminal completion does. No database publication occurs here.
    ///
    /// Scratch counts logical entries (including reserved witness map/field
    /// slots), not allocator bytes or transaction-lifetime memory. B-tree
    /// updates remain synchronous; indexed history searches checkpoint each
    /// directory and version node. This raw embedded API adds neither
    /// authorization, spill nor a full SSI claim.
    pub fn neighbours_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        vertex: VId,
        relation: RelationId,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution, TxnGqlError<WriteTxnError>> {
        cx.with_restriction(|| {
            self.adjacency_governed_with_checkpoint(
                database,
                vertex,
                relation,
                false,
                policy,
                || cx.checkpoint(),
            )
        })
    }

    /// Incoming sibling of neighbours_governed, with the identical meter and
    /// cleanup law. The insertion domain is this relation's incoming face.
    pub fn in_neighbours_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        vertex: VId,
        relation: RelationId,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution, TxnGqlError<WriteTxnError>> {
        cx.with_restriction(|| {
            self.adjacency_governed_with_checkpoint(
                database,
                vertex,
                relation,
                true,
                policy,
                || cx.checkpoint(),
            )
        })
    }

    fn adjacency_governed_with_checkpoint<V: Vfs + Clone, C>(
        &self,
        database: &Database<V>,
        vertex: VId,
        relation: RelationId,
        incoming: bool,
        policy: fgdb_gql::GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<fgdb_gql::GqlQueryExecution, fgdb_gql::GqlQueryError<WriteTxnError, C>> {
        use fgdb_gql::{GlaExecutionStats, GqlBudgetDimension, GqlExecutionStats, GqlQueryError};
        let mut usage = crate::gql_exec::AdmissionUsage::default();
        let mut result_rows = 0_u64;
        let (value, snapshot_records) = self.adjacency_with_control(
            database,
            vertex,
            relation,
            incoming,
            &mut |event| {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                match event {
                    AdjacencyReadEvent::Source(event) => usage.observe(policy, event),
                    AdjacencyReadEvent::ResultRow => {
                        let next = result_rows
                            .checked_add(1)
                            .expect("resident result row count fits u64");
                        policy
                            .rows
                            .check(GqlBudgetDimension::ResultRows, next)
                            .map_err(GqlQueryError::Rows)?;
                        usage.observe(policy, AdjacencySourceEvent::Work)?;
                        result_rows = next;
                        Ok(())
                    }
                }
            },
            &GqlQueryError::Source,
        )?;
        usage.finish(
            policy,
            Ok(fgdb_gql::GqlQueryExecution {
                value,
                rows: GqlExecutionStats {
                    snapshot_records,
                    result_rows,
                },
                evaluator: GlaExecutionStats::default(),
            }),
        )
    }

    fn adjacency_basis_controlled<V: Vfs + Clone, E>(
        &self,
        database: &Database<V>,
        vertex: VId,
        relation: RelationId,
        incoming: bool,
        control: &mut impl FnMut(AdjacencySourceEvent) -> Result<(), E>,
        source_error: &impl Fn(WriteTxnError) -> E,
    ) -> Result<std::collections::BTreeMap<EId, VId>, E> {
        use fgdb_gql::{GlaExecutionEvent, algebra::GlaDirection};
        self.ensure_database(database).map_err(source_error)?;
        database
            .ensure_readable()
            .map_err(WriteTxnError::from)
            .map_err(source_error)?;
        let snapshot = &database.snapshot;
        snapshot
            .check_frontier(self.basis)
            .map_err(WriteTxnError::from)
            .map_err(source_error)?;
        let direction = if incoming {
            GlaDirection::Reverse
        } else {
            GlaDirection::Forward
        };
        let index = &snapshot.adjacency_index;
        let mut matching = std::collections::BTreeMap::new();
        let mut after = None;
        loop {
            let next = index.next_incident_edge(vertex, direction, after, &mut |event| {
                control(match event {
                    GlaExecutionEvent::ScratchEntry => AdjacencySourceEvent::ScratchEntry,
                    GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => {
                        AdjacencySourceEvent::Work
                    }
                })
            })?;
            let Some(eid) = next else { break };
            after = Some(eid);
            control(AdjacencySourceEvent::Work)?;
            let Some((block, row)) =
                index.statement_at_controlled(&snapshot.blocks, eid, self.basis, control)?
            else {
                continue;
            };
            let entry = &snapshot.blocks[block][row];
            let (anchor, neighbour) = if incoming {
                (entry.dst, entry.src)
            } else {
                (entry.src, entry.dst)
            };
            // A historical incidence entry is a candidate, never a second truth.
            if anchor == vertex && entry.relation == relation {
                control(AdjacencySourceEvent::SnapshotRecord)?;
                control(AdjacencySourceEvent::ScratchEntry)?;
                matching.insert(eid, neighbour);
            }
        }
        Ok(matching)
    }

    fn adjacency_with_control<V: Vfs + Clone, E>(
        &self,
        database: &Database<V>,
        vertex: VId,
        relation: RelationId,
        incoming: bool,
        control: &mut impl FnMut(AdjacencyReadEvent) -> Result<(), E>,
        source_error: &impl Fn(WriteTxnError) -> E,
    ) -> Result<(Vec<VId>, u64), E> {
        use AdjacencyReadEvent::{ResultRow, Source};
        use AdjacencySourceEvent::{ScratchEntry, Work};
        use fgdb_delta_types::DeltaRow;
        use std::collections::BTreeSet;

        // Do not install any observation for a foreign owner or unhealthy cut.
        self.ensure_database(database).map_err(source_error)?;
        database
            .ensure_readable()
            .map_err(WriteTxnError::from)
            .map_err(source_error)?;
        database
            .snapshot
            .check_frontier(self.basis)
            .map_err(WriteTxnError::from)
            .map_err(source_error)?;
        control(Source(Work))?;
        let mut attempt = ProjectionReadAttempt {
            reads: &self.point_reads,
            element: ElementId::Vertex(vertex),
            accepted: false,
        };
        let mut matching = self.adjacency_basis_controlled(
            database,
            vertex,
            relation,
            incoming,
            &mut |event| control(Source(event)),
            source_error,
        )?;
        let snapshot_records =
            u64::try_from(matching.len()).expect("resident source row count fits u64");
        let mut observed_edges = BTreeSet::new();
        for eid in matching.keys() {
            control(Source(ScratchEntry))?;
            observed_edges.insert(*eid);
        }
        // The sole canonical overlay body, shared with ordinary neighbours().
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                control(Source(Work))?;
                for effect in &coordinate.rows {
                    control(Source(Work))?;
                    match effect {
                        DeltaRow::CreateEdge {
                            eid,
                            src,
                            relation: edge_relation,
                            dst,
                            ..
                        } => {
                            let (anchor, neighbour) =
                                if incoming { (*dst, *src) } else { (*src, *dst) };
                            if anchor == vertex && *edge_relation == relation {
                                if !matching.contains_key(eid) {
                                    control(Source(ScratchEntry))?;
                                }
                                if !observed_edges.contains(eid) {
                                    control(Source(ScratchEntry))?;
                                }
                                matching.insert(*eid, neighbour);
                                observed_edges.insert(*eid);
                            }
                        }
                        DeltaRow::DeleteEdge { eid, .. } => {
                            matching.remove(eid);
                        }
                        DeltaRow::DeleteVertex {
                            sorted_retired_incident_edges,
                            ..
                        } => {
                            for eid in sorted_retired_incident_edges {
                                control(Source(Work))?;
                                matching.remove(eid);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        let mut distinct = BTreeSet::new();
        for neighbour in matching.into_values() {
            control(Source(Work))?;
            if !distinct.contains(&neighbour) {
                control(Source(ScratchEntry))?;
                distinct.insert(neighbour);
            }
        }
        let mut value = Vec::new();
        for neighbour in distinct {
            control(ResultRow)?;
            control(Source(ScratchEntry))?;
            value.push(neighbour);
        }
        self.point_reads.borrow_mut().record_adjacency_controlled(
            vertex,
            relation,
            incoming,
            observed_edges,
            &mut || {
                // Reserve a map entry plus its field slot even on warm reads.
                // Warm witnesses never discount this invocation's admission.
                control(Source(ScratchEntry))?;
                control(Source(ScratchEntry))
            },
        )?;
        control(Source(Work))?;
        attempt.accepted = true;
        Ok((value, snapshot_records))
    }
}

#[cfg(test)]
mod governed_adjacency_tests {
    use super::*;
    include!("governed_adjacency_tests.rs");
}
