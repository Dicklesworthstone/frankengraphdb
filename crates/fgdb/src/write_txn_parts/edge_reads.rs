impl WriteTxn {
    /// Read one edge from the pinned durable basis plus this transaction's
    /// staged create/delete overlay, without publishing the transaction.
    pub fn edge<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        eid: EId,
    ) -> Result<Option<EdgeRecord>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut overlay = database.edge_at(eid, self.basis)?;
        let mut observed_sources = std::collections::BTreeSet::new();
        let mut deleted_vertices = std::collections::BTreeSet::new();
        if let Some(record) = &overlay {
            observed_sources.insert(record.entry.src);
        }

        for batch in &self.staged {
            for pending in &batch.rows {
                match pending {
                    PendingRow::Edge {
                        eid: row_eid,
                        src,
                        dst,
                        props,
                        ensure: _,
                    } if *row_eid == eid => {
                        let mut props = props.clone();
                        crate::sort_write_props(&mut props);
                        observed_sources.insert(*src);
                        overlay = Some(EdgeRecord {
                            entry: AdjacencyEntry {
                                src: *src,
                                relation: batch.relation,
                                dst: *dst,
                                eid,
                                created_at: self.basis,
                                retired_at: None,
                            },
                            props,
                        });
                    }
                    PendingRow::DeleteEdge { eid: row_eid, .. } if *row_eid == eid => {
                        overlay = None;
                    }
                    PendingRow::SetEdgeProperty {
                        eid: row_eid,
                        key,
                        value,
                    } if *row_eid == eid => {
                        if let Some(record) = overlay.as_mut() {
                            Self::overlay_property(&mut record.props, *key, value.as_ref());
                        }
                    }
                    PendingRow::CompareAndSet {
                        elem: ElementId::Edge(row_eid),
                        key,
                        expected,
                        value,
                        ..
                    } if *row_eid == eid => {
                        if let Some(record) = overlay.as_mut() {
                            let actual = record
                                .props
                                .binary_search_by_key(key, |(property, _)| *property)
                                .ok()
                                .map(|at| &record.props[at].1);
                            if actual == expected.as_deref() {
                                Self::overlay_property(
                                    &mut record.props,
                                    *key,
                                    Some(value.as_ref()),
                                );
                            }
                        }
                    }
                    PendingRow::DeleteVertex { vid, .. } => {
                        if overlay.as_ref().is_some_and(|record| {
                            record.entry.src == *vid || record.entry.dst == *vid
                        }) {
                            deleted_vertices.insert(*vid);
                            overlay = None;
                        }
                    }
                    PendingRow::Vertex { .. }
                    | PendingRow::Edge { .. }
                    | PendingRow::DeleteEdge { .. }
                    | PendingRow::SetLabel { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetProperty { .. }
                    | PendingRow::CompareAndSet { .. } => {}
                }
            }
        }

        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Edge(eid));
        read_set.extend(observed_sources.into_iter().map(ElementId::Vertex));
        read_set.extend(deleted_vertices.into_iter().map(ElementId::Vertex));
        Ok(overlay)
    }

    /// Read every edge from the pinned basis through this transaction's
    /// staged row-order overlay, sorted by edge identity.
    pub fn edges<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Vec<EdgeRecord>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut eids: std::collections::BTreeSet<EId> = database
            .edges_at(self.basis)?
            .into_iter()
            .map(|record| record.entry.eid)
            .collect();
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            match pending {
                PendingRow::Edge { eid, .. }
                | PendingRow::DeleteEdge { eid, .. }
                | PendingRow::SetEdgeProperty { eid, .. }
                | PendingRow::CompareAndSet {
                    elem: ElementId::Edge(eid),
                    ..
                } => {
                    eids.insert(*eid);
                }
                PendingRow::Vertex { .. }
                | PendingRow::DeleteVertex { .. }
                | PendingRow::SetLabel { .. }
                | PendingRow::SetProperty { .. }
                | PendingRow::CompareAndSet { .. } => {}
            }
        }

        let mut rows = Vec::new();
        for eid in eids {
            if let Some(record) = self.edge(database, eid)? {
                rows.push(record);
            }
        }
        rows.sort_by_key(|record| record.entry.eid);

        let mut read_set = self.read_set.borrow_mut();
        read_set.extend(rows.iter().map(|record| ElementId::Edge(record.entry.eid)));
        read_set.extend(
            rows.iter()
                .map(|record| ElementId::Vertex(record.entry.src)),
        );
        drop(read_set);
        self.match_expansions.borrow_mut().extend(
            rows.iter()
                .map(|record| (record.entry.src, record.entry.relation)),
        );
        Ok(rows)
    }

    /// Read the pinned neighbours of one relation through staged edge
    /// creates and deletes. Destinations retain the database API's sorted,
    /// deduplicated result shape even when parallel edges exist.
    pub fn neighbours<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        src: VId,
        relation: RelationId,
    ) -> Result<Vec<VId>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut destinations: std::collections::BTreeSet<VId> = database
            .neighbours_at(src, relation, self.basis)?
            .into_iter()
            .collect();
        let mut matching_edges: std::collections::BTreeMap<EId, VId> = database
            .edges_at(self.basis)?
            .into_iter()
            .filter_map(|record| {
                (record.entry.src == src && record.entry.relation == relation)
                    .then_some((record.entry.eid, record.entry.dst))
            })
            .collect();
        let mut observed_edges: std::collections::BTreeSet<EId> =
            matching_edges.keys().copied().collect();
        let mut deleted_vertices = std::collections::BTreeSet::new();

        for batch in &self.staged {
            for pending in &batch.rows {
                match pending {
                    PendingRow::Edge {
                        eid,
                        src: edge_src,
                        dst,
                        ensure,
                        ..
                    } if *edge_src == src && batch.relation == relation => {
                        if !ensure || !destinations.contains(dst) {
                            matching_edges.insert(*eid, *dst);
                            destinations.insert(*dst);
                            observed_edges.insert(*eid);
                        }
                    }
                    PendingRow::DeleteEdge { eid, .. } => {
                        if let Some(dst) = matching_edges.remove(eid)
                            && !matching_edges.values().any(|other| *other == dst)
                        {
                            destinations.remove(&dst);
                        }
                    }
                    PendingRow::DeleteVertex { vid, .. } => {
                        let affected = if *vid == src {
                            matching_edges.clear();
                            destinations.clear();
                            true
                        } else if matching_edges.values().any(|dst| *dst == *vid) {
                            matching_edges.retain(|_, dst| *dst != *vid);
                            destinations.remove(vid);
                            true
                        } else {
                            false
                        };
                        if affected {
                            deleted_vertices.insert(*vid);
                        }
                    }
                    PendingRow::Vertex { .. }
                    | PendingRow::Edge { .. }
                    | PendingRow::SetLabel { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetProperty { .. }
                    | PendingRow::CompareAndSet { .. } => {}
                }
            }
        }

        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Vertex(src));
        read_set.extend(observed_edges.into_iter().map(ElementId::Edge));
        read_set.extend(deleted_vertices.into_iter().map(ElementId::Vertex));
        drop(read_set);
        self.match_expansions.borrow_mut().insert((src, relation));
        Ok(destinations.into_iter().collect())
    }

    /// Read the pinned incoming neighbours of one relation through staged
    /// edge creates, edge deletes, and vertex-delete cascades.
    pub fn in_neighbours<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        dst: VId,
        relation: RelationId,
    ) -> Result<Vec<VId>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }

        let mut sources: std::collections::BTreeSet<VId> = database
            .in_neighbours_at(dst, relation, self.basis)?
            .into_iter()
            .collect();
        let mut matching_edges: std::collections::BTreeMap<EId, VId> = database
            .edges_at(self.basis)?
            .into_iter()
            .filter_map(|record| {
                (record.entry.dst == dst && record.entry.relation == relation)
                    .then_some((record.entry.eid, record.entry.src))
            })
            .collect();
        let mut observed_edges: std::collections::BTreeSet<EId> =
            matching_edges.keys().copied().collect();
        let mut deleted_sources = std::collections::BTreeSet::new();

        for batch in &self.staged {
            for pending in &batch.rows {
                match pending {
                    PendingRow::Edge {
                        eid,
                        src,
                        dst: edge_dst,
                        ensure,
                        ..
                    } if *edge_dst == dst && batch.relation == relation => {
                        if !ensure || !sources.contains(src) {
                            matching_edges.insert(*eid, *src);
                            sources.insert(*src);
                            observed_edges.insert(*eid);
                        }
                    }
                    PendingRow::DeleteEdge { eid, .. } => {
                        if let Some(src) = matching_edges.remove(eid)
                            && !matching_edges.values().any(|other| *other == src)
                        {
                            sources.remove(&src);
                        }
                    }
                    PendingRow::DeleteVertex { vid, .. } => {
                        if *vid == dst {
                            matching_edges.clear();
                            sources.clear();
                        } else if matching_edges.values().any(|src| *src == *vid) {
                            matching_edges.retain(|_, src| *src != *vid);
                            sources.remove(vid);
                            deleted_sources.insert(*vid);
                        }
                    }
                    PendingRow::Vertex { .. }
                    | PendingRow::Edge { .. }
                    | PendingRow::SetLabel { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetProperty { .. }
                    | PendingRow::CompareAndSet { .. } => {}
                }
            }
        }

        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Vertex(dst));
        read_set.extend(observed_edges.into_iter().map(ElementId::Edge));
        read_set.extend(deleted_sources.into_iter().map(ElementId::Vertex));
        Ok(sources.into_iter().collect())
    }

}
