//! Fold filterless COUNT admission without retaining the statement's edge
//! table. Scope, MVCC winners, source polls and logical charges are the same
//! boundaries used by Tables. The ordinary aggregate owns the final result.

use super::*;
use fgdb_gql::GraphCountSource;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Input {
    pub(super) records: u64,
    pub(super) examined: u64,
    pub(super) matching: u64,
}

pub(super) fn admit(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    source: GraphCountSource,
    predicates: &PlannerPredicates,
    node: &mut impl FnMut() -> Result<(), Fault>,
    poll: &mut impl FnMut() -> Result<(), Fault>,
    control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
) -> Result<Input, Fault> {
    #[cfg(test)]
    SCANS.with(|scans| scans.set(scans.get() + 1));
    let edges = matches!(source, GraphCountSource::Relation(_));
    let reusable = predicates.sees_all_incidence() && predicates.sees_all_fields();
    // Preserve the existing unrestricted warm source for later statements,
    // including queries that need its actual records. Scoped sources never
    // populate or consume this shared core.
    let recording = reusable.then(|| RefCell::new(Recording::default()));
    let mut scan = |_| {
        if let Some(recording) = &recording {
            recording.borrow_mut().poll();
        }
        poll()
    };
    let mut tables = Tables::new();
    let mut examined = 0;
    let mut matching = 0;
    source::visit_vertex_coordinates(&snapshot.patches, at, &mut scan, |row, patch, row_at, _| {
        let labels = if edges {
            // An edge needs both endpoints admitted. Keep only these
            // vertices and their existing position index, never edges.
            tables.admit_vertex(row, predicates, node, control)?
        } else if let Some(labels) = visible_vertex_labels(row, predicates) {
            charge_vertex(labels, node, control)?;
            tables.records += 1;
            examined += 1;
            matching += 1;
            Some(labels)
        } else {
            None
        };
        if let (Some(labels), Some(recording)) = (labels, &recording) {
            recording
                .borrow_mut()
                .vertex(row.vid, patch, row_at, labels);
        }
        Ok(())
    })?;
    if let GraphCountSource::Relation(requested) = source {
        source::visit_edges_with_coordinates(
            snapshot,
            at,
            &mut scan,
            |edge, _, block, row_at, _| {
                let identity = (edge.eid, edge.src, edge.relation, edge.dst);
                if let Some(positions) =
                    tables.admit_edge_endpoints(identity, predicates, control)?
                {
                    examined += 1;
                    matching += u64::from(edge.relation == requested);
                    if let Some(recording) = &recording {
                        recording
                            .borrow_mut()
                            .edge(identity, block, row_at, positions);
                    }
                }
                Ok(())
            },
        )?;
    }
    if let Some(core) =
        recording.and_then(|recording| recording.into_inner().finish(tables.records))
    {
        let bytes = core.bytes();
        ADMITTED.remember(snapshot, AdmissionKey { at, edges }, Arc::new(core), bytes);
    }
    Ok(Input {
        records: tables.records,
        examined,
        matching,
    })
}
