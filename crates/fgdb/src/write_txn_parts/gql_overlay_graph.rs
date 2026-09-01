impl WriteTxn {
    fn overlay_graph<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<OverlayGraph, WriteTxnError> {
        let mut observed = std::collections::BTreeSet::new();
        let mut vertices = OverlayVertexSet::new();
        let mut edges: OverlayEdgeMap = database
            .edges_at(self.basis)?
            .into_iter()
            .map(|row| {
                observed.insert(ElementId::Edge(row.entry.eid));
                observed.insert(ElementId::Vertex(row.entry.src));
                observed.insert(ElementId::Vertex(row.entry.dst));
                vertices.insert(row.entry.src);
                vertices.insert(row.entry.dst);
                (
                    row.entry.eid,
                    (row.entry.src, row.entry.relation, row.entry.dst),
                )
            })
            .collect();

        for batch in &self.staged {
            for pending in &batch.rows {
                match pending {
                    PendingRow::Vertex { vid, .. } => {
                        observed.insert(ElementId::Vertex(*vid));
                        vertices.insert(*vid);
                    }
                    PendingRow::Edge {
                        eid,
                        src,
                        dst,
                        ensure,
                        ..
                    } => {
                        observed.insert(ElementId::Edge(*eid));
                        observed.insert(ElementId::Vertex(*src));
                        observed.insert(ElementId::Vertex(*dst));
                        vertices.insert(*src);
                        vertices.insert(*dst);
                        let triple = (*src, batch.relation, *dst);
                        if !ensure || !edges.values().any(|existing| *existing == triple) {
                            edges.insert(*eid, triple);
                        }
                    }
                    PendingRow::DeleteEdge { eid, .. } => {
                        observed.insert(ElementId::Edge(*eid));
                        edges.remove(eid);
                    }
                    PendingRow::DeleteVertex { vid, .. } => {
                        observed.insert(ElementId::Vertex(*vid));
                        vertices.remove(vid);
                        edges.retain(|_, (src, _, dst)| *src != *vid && *dst != *vid);
                    }
                    PendingRow::SetLabel { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetProperty { .. }
                    | PendingRow::CompareAndSet { .. } => {}
                }
            }
        }
        Ok(OverlayGraph {
            observed,
            vertices,
            edges,
        })
    }

    fn label_holders<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vertices: &OverlayVertexSet,
        label: Option<fgdb_delta_types::LabelId>,
    ) -> Result<Option<OverlayVertexSet>, WriteTxnError> {
        let Some(label) = label else {
            return Ok(None);
        };
        let mut holders = OverlayVertexSet::new();
        for vid in vertices.iter().copied() {
            if self
                .vertex(database, vid)?
                .is_some_and(|row| row.labels.contains(&label))
            {
                holders.insert(vid);
            }
        }
        Ok(Some(holders))
    }
}
