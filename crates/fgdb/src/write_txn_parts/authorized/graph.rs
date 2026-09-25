//! Edge targets and engine-derived detach cascades for the authorized write path.
//! The identity candidate union is not a graph overlay: every candidate is
//! resolved through the same canonical native point read that commit uses.
use super::{Execution, Fields, denied, redacted, stage_native};
use crate::{Database, EdgeRecord, PendingRow, VertexRow, WriteTxn, WriteTxnError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_types::{EId, VId};
use fgdb_warden::{EdgeWriteImage, WriteEndpoint};
use std::collections::BTreeSet;

struct Edge {
    record: EdgeRecord,
    source: VertexRow,
    destination: VertexRow,
}
impl Edge {
    fn image(&self) -> EdgeWriteImage<'_> {
        EdgeWriteImage {
            id: self.record.entry.eid,
            relation: self.record.entry.relation,
            source: WriteEndpoint {
                id: self.source.vid,
                labels: &self.source.labels,
            },
            destination: WriteEndpoint {
                id: self.destination.vid,
                labels: &self.destination.labels,
            },
            properties: &self.record.props,
        }
    }
}

impl<Clock: FnMut() -> u64> Execution<'_, '_, Clock> {
    fn endpoint<V: Vfs + Clone>(
        &mut self,
        transaction: &WriteTxn,
        database: &Database<V>,
        vid: VId,
    ) -> Result<VertexRow, WriteTxnError> {
        let row = self
            .vertex(transaction, database, vid)?
            .ok_or_else(denied)?;
        self.check_endpoint(row)
    }

    fn check_endpoint(&mut self, row: VertexRow) -> Result<VertexRow, WriteTxnError> {
        // Endpoints are admitted, not mutated. Preserve all original fields.
        self.check_vertex(
            Some(&row),
            Some(&row),
            &Fields {
                labels: vec![],
                properties: vec![],
            },
        )?;
        Ok(row)
    }

    fn edge_record<V: Vfs + Clone>(
        &mut self,
        transaction: &WriteTxn,
        database: &Database<V>,
        eid: EId,
    ) -> Result<Option<EdgeRecord>, WriteTxnError> {
        self.checkpoint()?;
        transaction.edge(database, eid).map_err(redacted)
    }

    fn relation(&mut self, relation: RelationId) -> Result<(), WriteTxnError> {
        self.checkpoint()?;
        if !self.permit.predicates().allows_relation(relation) {
            return Err(denied());
        }
        Ok(())
    }

    fn edge_from_record<V: Vfs + Clone>(
        &mut self,
        transaction: &WriteTxn,
        database: &Database<V>,
        record: EdgeRecord,
    ) -> Result<Edge, WriteTxnError> {
        // Resolve scope with cancellation-only polling before billing any
        // relation or endpoint admission. Otherwise a hidden edge reaches a
        // different work/node limit than an absent EId (FG-INV-20). These are
        // the native pinned rows, not a synthetic or masked graph overlay.
        self.poll()?;
        if !self
            .permit
            .predicates()
            .allows_relation(record.entry.relation)
        {
            return Err(denied());
        }
        let source = transaction
            .vertex(database, record.entry.src)
            .map_err(redacted)?
            .ok_or_else(denied)?;
        self.poll()?;
        if !self.permit.predicates().allows_vertex(&source.labels) {
            return Err(denied());
        }
        let destination = transaction
            .vertex(database, record.entry.dst)
            .map_err(redacted)?
            .ok_or_else(denied)?;
        self.poll()?;
        if !self.permit.predicates().allows_vertex(&destination.labels) {
            return Err(denied());
        }
        // Charge the same admitted logical events, in the same order, as the
        // ordinary relation + two endpoint reads. Reuse the resolved rows so
        // admission does not add duplicate native reads or property clones.
        self.relation(record.entry.relation)?;
        self.checkpoint()?;
        let source = self.check_endpoint(source)?;
        self.checkpoint()?;
        let destination = self.check_endpoint(destination)?;
        Ok(Edge {
            record,
            source,
            destination,
        })
    }

    fn edge<V: Vfs + Clone>(
        &mut self,
        transaction: &WriteTxn,
        database: &Database<V>,
        eid: EId,
    ) -> Result<Option<Edge>, WriteTxnError> {
        self.edge_record(transaction, database, eid)?
            .map(|record| self.edge_from_record(transaction, database, record))
            .transpose()
    }

    fn check_edge(
        &mut self,
        before: Option<&Edge>,
        after: Option<&Edge>,
        properties: &[PropertyKeyId],
    ) -> Result<(), WriteTxnError> {
        self.checkpoint()?;
        self.permit
            .check_edge_write_at(
                (self.clock)(),
                before.map(Edge::image),
                after.map(Edge::image),
                properties,
            )
            .map_err(WriteTxnError::Authorization)
    }
}

fn incident_candidates<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &WriteTxn,
    database: &Database<V>,
    vid: VId,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<BTreeSet<EId>, WriteTxnError> {
    // Enumeration is unmetered (FG-INV-20, fgdb-4iiho): the live incidence
    // includes edges of relations, and to endpoints, the capability cannot
    // see, so charging per candidate here would let a holder count them by
    // bisecting MaxWork. Callers charge the candidates they actually process.
    execution.poll()?;
    // The API holds the exclusive database borrow for the entire transaction,
    // so the live writer still supplies this exact pinned basis. Add original
    // staged identity candidates, but never infer that an ensure alias exists.
    let mut ids = BTreeSet::new();
    for eid in database.writer.live_incident_edges(vid) {
        execution.poll()?;
        ids.insert(eid);
    }
    for batch in &transaction.staged {
        for row in &batch.rows {
            execution.poll()?;
            if let PendingRow::Edge { eid, src, dst, .. } = row
                && (*src == vid || *dst == vid)
            {
                ids.insert(*eid);
            }
        }
    }
    Ok(ids)
}

pub(super) fn stage_edge<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    relation: RelationId,
    row: PendingRow,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    let (eid, creates, deletes, mut fields) = match &row {
        PendingRow::Edge {
            eid,
            src,
            dst,
            props,
            ensure,
        } => {
            execution.relation(relation)?;
            let fields = execution.fields([], props.iter().map(|(key, _)| *key))?;
            execution.endpoint(transaction, database, *src)?;
            execution.endpoint(transaction, database, *dst)?;
            let mut target = *eid;
            if *ensure {
                // Native ensure-by-triple ignores the requested EId when ANY
                // live alias satisfies the triple. Authorize that actual edge,
                // not an invented edge under the unused requested identity.
                // Only an alias with this exact (src, relation, dst) can be
                // selected, and such an edge is visible: its relation and both
                // endpoints were admitted above. Every other incident edge is
                // resolved unmetered and skipped; only the selected alias is
                // charged, like any record read (FG-INV-20, fgdb-4iiho).
                for candidate in incident_candidates(transaction, database, *src, execution)? {
                    execution.poll()?;
                    if let Some(edge) = transaction.edge(database, candidate).map_err(redacted)?
                        && edge.entry.src == *src
                        && edge.entry.relation == relation
                        && edge.entry.dst == *dst
                    {
                        execution.checkpoint()?;
                        target = candidate;
                        break;
                    }
                }
            }
            (target, true, false, fields)
        }
        PendingRow::DeleteEdge { eid, .. } => {
            // A whole-edge delete erases every property. Refusing only when
            // a hidden property actually exists would disclose one bit about
            // the edge (FG-INV-20, fgdb-4iiho). Decide from the capability
            // before looking up any target, including an if-present delete.
            // Label/relation scopes remain legal: endpoints are not erased.
            if !execution.permit.predicates().sees_all_properties() {
                return Err(denied());
            }
            (*eid, false, true, execution.fields([], [])?)
        }
        PendingRow::SetEdgeProperty { eid, key, .. }
        | PendingRow::CompareAndSet {
            elem: ElementId::Edge(eid),
            key,
            ..
        } => (*eid, false, false, execution.fields([], [*key])?),
        _ => return Err(WriteTxnError::AuthorizedMutationRefused),
    };
    let before = execution.edge(transaction, database, eid)?;
    if let Some(before) = &before {
        if deletes {
            fields = execution.fields([], before.record.props.iter().map(|(key, _)| *key))?;
        }
        execution.check_edge(Some(before), Some(before), &fields.properties)?;
    } else if !creates {
        return Err(denied());
    }
    stage_native(transaction, database, relation, row, execution)?;
    let after = execution.edge(transaction, database, eid)?;
    execution.check_edge(before.as_ref(), after.as_ref(), &fields.properties)
}

pub(super) fn delete_vertex<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    relation: RelationId,
    row: PendingRow,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    let PendingRow::DeleteVertex { vid, .. } = &row else {
        return Err(WriteTxnError::AuthorizedMutationRefused);
    };
    // FG-INV-20 (fgdb-4iiho, owner ruling 2026-09-25): a vertex delete erases
    // every label and property and cascades to every incident edge. If the
    // capability could hide any of them, the outcome would reveal whether
    // hidden fields or incidence exist (or destroy data the holder cannot
    // observe). Such a capability is refused here, before any read or charge,
    // so the refusal depends on the capability alone, never on the data.
    let predicates = execution.permit.predicates();
    if !(predicates.sees_all_incidence() && predicates.sees_all_fields()) {
        return Err(denied());
    }
    let vid = *vid;
    let before = execution
        .vertex(transaction, database, vid)?
        .ok_or_else(denied)?;
    let fields = execution.fields(
        before.labels.iter().copied(),
        before.props.iter().map(|(key, _)| *key),
    )?;
    execution.check_vertex(Some(&before), None, &fields)?;
    let mut cascades = Vec::new();
    for eid in incident_candidates(transaction, database, vid, execution)? {
        let Some(record) = execution.edge_record(transaction, database, eid)? else {
            continue;
        };
        // An ignored ensure alias may name a live but unrelated edge. Its
        // ORIGINAL identity/endpoint record, not its input spelling, decides
        // incidence. Do not authorize or delete an unrelated hidden edge.
        if record.entry.src != vid && record.entry.dst != vid {
            continue;
        }
        let edge = execution.edge_from_record(transaction, database, record)?;
        let touched = execution.fields([], edge.record.props.iter().map(|(key, _)| *key))?;
        execution.check_edge(Some(&edge), None, &touched.properties)?;
        cascades.push(eid);
    }
    stage_native(transaction, database, relation, row, execution)?;
    let after = execution.vertex(transaction, database, vid)?;
    execution.check_vertex(Some(&before), after.as_ref(), &fields)?;
    if after.is_some() {
        return Err(WriteTxnError::AuthorizedMutationRefused);
    }
    // Confirm the native cascade actually removed every authorized incidence;
    // no missing-edge interpretation or synthetic post-image reaches commit.
    for eid in cascades {
        if execution.edge_record(transaction, database, eid)?.is_some() {
            return Err(WriteTxnError::AuthorizedMutationRefused);
        }
    }
    Ok(())
}
