impl WriteTxn {
    /// Read the pinned durable edge plus the exact prepared net effects.
    pub fn edge<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        eid: EId,
    ) -> Result<Option<EdgeRecord>, WriteTxnError> {
        self.ensure_database(database)?;
        let overlay = database.edge_at(eid, self.basis)?;
        Ok(self.edge_over_basis(eid, overlay))
    }

    /// Preparation already resolved ensure-by-triple, conditional no-ops and
    /// row-order semantics. Applying raw PendingRow::Edge as an unconditional
    /// create would invent unused ensure aliases or overwrite existing props.
    /// Point and bulk reads therefore overlay the same canonical net template
    /// that commit will publish, never a second interpretation of intentions.
    fn edge_over_basis(&self, eid: EId, mut overlay: Option<EdgeRecord>) -> Option<EdgeRecord> {
        let mut observed_sources = std::collections::BTreeSet::new();
        let mut deleted_vertices = std::collections::BTreeSet::new();
        if let Some(record) = &overlay {
            observed_sources.insert(record.entry.src);
        }
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for row in &coordinate.rows {
                    match row {
                        fgdb_delta_types::DeltaRow::CreateEdge {
                            eid: row_eid, src, relation, dst, props, ..
                        } if *row_eid == eid => {
                            observed_sources.insert(*src);
                            overlay = Some(EdgeRecord {
                                entry: AdjacencyEntry {
                                    src: *src,
                                    relation: *relation,
                                    dst: *dst,
                                    eid,
                                    created_at: self.basis,
                                    retired_at: None,
                                },
                                props: props.clone(),
                            });
                        }
                        fgdb_delta_types::DeltaRow::DeleteEdge { eid: row_eid, .. }
                            if *row_eid == eid =>
                        {
                            overlay = None;
                        }
                        fgdb_delta_types::DeltaRow::Property {
                            elem: ElementId::Edge(row_eid), property, after, ..
                        } if *row_eid == eid => {
                            if let Some(record) = overlay.as_mut() {
                                Self::overlay_property(&mut record.props, *property, after.as_ref());
                            }
                        }
                        fgdb_delta_types::DeltaRow::DeleteVertex {
                            vid, sorted_retired_incident_edges, ..
                        } if sorted_retired_incident_edges.binary_search(&eid).is_ok() => {
                            deleted_vertices.insert(*vid);
                            overlay = None;
                        }
                        _ => {}
                    }
                }
            }
        }
        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Edge(eid));
        read_set.extend(observed_sources.into_iter().map(ElementId::Vertex));
        read_set.extend(deleted_vertices.into_iter().map(ElementId::Vertex));
        overlay
    }

    /// Read all pinned edges with their prepared net effects. Empty scans
    /// retain an insertion witness even when every observed row is deleted.
    pub fn edges<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Vec<EdgeRecord>, WriteTxnError> {
        self.ensure_database(database)?;
        let mut basis: std::collections::BTreeMap<EId, EdgeRecord> = database
            .edges_at(self.basis)?
            .into_iter()
            .map(|record| (record.entry.eid, record))
            .collect();
        self.scanned_edges.set(true);
        let mut eids: std::collections::BTreeSet<EId> = basis.keys().copied().collect();
        for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
            match pending {
                PendingRow::Edge { eid, .. }
                | PendingRow::DeleteEdge { eid, .. }
                | PendingRow::SetEdgeProperty { eid, .. }
                | PendingRow::CompareAndSet { elem: ElementId::Edge(eid), .. } => {
                    // Retain absent identities as negative-read dependencies,
                    // but only actual prepared creations can materialize rows.
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
            if let Some(record) = self.edge_over_basis(eid, basis.remove(&eid)) {
                rows.push(record);
            }
        }
        rows.sort_by_key(|record| record.entry.eid);
        let mut read_set = self.read_set.borrow_mut();
        read_set.extend(rows.iter().map(|record| ElementId::Edge(record.entry.eid)));
        read_set.extend(rows.iter().map(|record| ElementId::Vertex(record.entry.src)));
        drop(read_set);
        self.match_expansions.borrow_mut().extend(
            rows.iter().map(|record| (record.entry.src, record.entry.relation)),
        );
        Ok(rows)
    }

    /// Read outgoing neighbours from the pinned basis plus prepared net
    /// effects. Parallel edges keep a neighbour live until its last edge is
    /// removed; the returned vertex identities are sorted and deduplicated.
    pub fn neighbours<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        src: VId,
        relation: RelationId,
    ) -> Result<Vec<VId>, WriteTxnError> {
        self.adjacency_neighbours(database, src, relation, false)
    }

    /// Read incoming neighbours through the same canonical overlay as outgoing
    /// reads, including engine-derived vertex-delete cascades.
    pub fn in_neighbours<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        dst: VId,
        relation: RelationId,
    ) -> Result<Vec<VId>, WriteTxnError> {
        self.adjacency_neighbours(database, dst, relation, true)
    }

    fn adjacency_neighbours<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vertex: VId,
        relation: RelationId,
        incoming: bool,
    ) -> Result<Vec<VId>, WriteTxnError> {
        self.ensure_database(database)?;
        // Do not call edges(): that would turn a local expansion into a global
        // edge-scan conflict witness. The endpoint read below also detects a
        // previously empty incoming adjacency through adjacency_endpoints.
        let mut matching = std::collections::BTreeMap::new();
        for record in database.edges_at(self.basis)? {
            let entry = record.entry;
            let (anchor, neighbour) = if incoming {
                (entry.dst, entry.src)
            } else {
                (entry.src, entry.dst)
            };
            if anchor == vertex && entry.relation == relation {
                matching.insert(entry.eid, neighbour);
            }
        }
        let mut observed_edges: std::collections::BTreeSet<EId> =
            matching.keys().copied().collect();
        let mut deleted_vertices = std::collections::BTreeSet::new();
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for effect in &coordinate.rows {
                    match effect {
                        fgdb_delta_types::DeltaRow::CreateEdge {
                            eid, src, relation: edge_relation, dst, ..
                        } => {
                            let (anchor, neighbour) = if incoming {
                                (*dst, *src)
                            } else {
                                (*src, *dst)
                            };
                            if anchor == vertex && *edge_relation == relation {
                                matching.insert(*eid, neighbour);
                                observed_edges.insert(*eid);
                            }
                        }
                        fgdb_delta_types::DeltaRow::DeleteEdge { eid, .. } => {
                            matching.remove(eid);
                        }
                        fgdb_delta_types::DeltaRow::DeleteVertex {
                            vid, sorted_retired_incident_edges, ..
                        } => {
                            // Apply only the authoritative cascade image. Do
                            // not rescan every surviving edge for each delete.
                            for eid in sorted_retired_incident_edges {
                                if matching.remove(eid).is_some() {
                                    deleted_vertices.insert(*vid);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Vertex(vertex));
        read_set.extend(observed_edges.into_iter().map(ElementId::Edge));
        read_set.extend(deleted_vertices.into_iter().map(ElementId::Vertex));
        drop(read_set);
        if !incoming {
            self.match_expansions.borrow_mut().insert((vertex, relation));
        }
        // Deduplicate once, after applying all edge identities. In particular,
        // deletion of one parallel edge never removes a surviving neighbour.
        Ok(matching.into_values().collect::<std::collections::BTreeSet<_>>()
            .into_iter().collect())
    }
}

#[cfg(test)]
mod adjacency_overlay_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn keys() -> crate::DatabaseKeys {
        crate::DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        )
    }

    #[test]
    fn both_directions_match_committed_net_effects_and_relation_scope() {
        let ((), report) = run_async_under_lab(0xa91c_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for vid in 1..=4 {
                seed.create_vertex(VId(vid), vec![], vec![]);
            }
            for (eid, src, dst) in [
                (10, 1, 2), (11, 1, 2), (12, 2, 1),
                (13, 1, 1), (14, 3, 1), (15, 1, 3),
            ] {
                seed.add_edge(EId(eid), VId(src), VId(dst), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let mut other_relation = WriteBatch::new(RelationId(2));
            other_relation.add_edge(EId(90), VId(1), VId(4), vec![]);
            db.write(&commit, other_relation).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let mut changes = WriteBatch::new(RelationId(1));
            changes.ensure_edge_by_triple(EId(100), VId(1), VId(2), vec![]);
            changes.delete_edge(EId(10));
            changes.delete_edge(EId(11));
            changes.ensure_edge_by_triple(EId(101), VId(1), VId(2), vec![]);
            changes.add_edge(EId(102), VId(1), VId(4), vec![]);
            changes.delete_edge(EId(102));
            changes.delete_vertex(VId(3));
            changes.add_edge(EId(103), VId(4), VId(1), vec![]);
            changes.delete_edge_if_present(EId(999));
            txn.write(&mut db, changes).unwrap();
            assert!(txn.edge(&db, EId(100)).unwrap().is_none());
            assert!(txn.edge(&db, EId(102)).unwrap().is_none());
            assert!(txn.edge(&db, EId(101)).unwrap().is_some());
            let outgoing = txn.neighbours(&db, VId(1), RelationId(1)).unwrap();
            let incoming = txn.in_neighbours(&db, VId(1), RelationId(1)).unwrap();
            assert_eq!(outgoing, vec![VId(1), VId(2)]);
            assert_eq!(incoming, vec![VId(1), VId(2), VId(4)]);
            assert_eq!(txn.neighbours(&db, VId(1), RelationId(2)).unwrap(), vec![VId(4)]);
            assert_eq!(txn.in_neighbours(&db, VId(4), RelationId(2)).unwrap(), vec![VId(1)]);
            txn.commit(&mut db, &commit).await.unwrap();
            assert_eq!(db.neighbours(VId(1), RelationId(1)).unwrap(), outgoing);
            assert_eq!(db.in_neighbours(VId(1), RelationId(1)).unwrap(), incoming);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn parallel_edges_survive_partial_deletion_and_savepoint_rollback() {
        let ((), report) = run_async_under_lab(0xa91c_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(VId(1), vec![], vec![]);
            seed.create_vertex(VId(2), vec![], vec![]);
            for eid in 10..266 {
                seed.add_edge(EId(eid), VId(1), VId(2), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let mut partial = WriteBatch::new(RelationId(1));
            for eid in 10..265 {
                partial.delete_edge(EId(eid));
            }
            txn.write(&mut db, partial).unwrap();
            txn.savepoint(&db, "last-edge").unwrap();
            assert_eq!(txn.neighbours(&db, VId(1), RelationId(1)).unwrap(), vec![VId(2)]);
            assert_eq!(txn.in_neighbours(&db, VId(2), RelationId(1)).unwrap(), vec![VId(1)]);
            let mut last = WriteBatch::new(RelationId(1));
            last.delete_edge(EId(265));
            txn.write(&mut db, last).unwrap();
            assert!(txn.neighbours(&db, VId(1), RelationId(1)).unwrap().is_empty());
            assert!(txn.in_neighbours(&db, VId(2), RelationId(1)).unwrap().is_empty());
            txn.rollback_to_savepoint(&db, "last-edge").unwrap();
            assert_eq!(txn.neighbours(&db, VId(1), RelationId(1)).unwrap(), vec![VId(2)]);
            assert_eq!(txn.in_neighbours(&db, VId(2), RelationId(1)).unwrap(), vec![VId(1)]);
            txn.commit(&mut db, &commit).await.unwrap();
            assert_eq!(db.neighbours(VId(1), RelationId(1)).unwrap(), vec![VId(2)]);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn empty_adjacency_reads_detect_phantoms_without_global_scan_witnesses() {
        for incoming in [false, true] {
            for touches_anchor in [false, true] {
                let ((), report) = run_async_under_lab(0xa91c_0003, |root| async move {
                    let contexts = PurposeContexts::narrow_runtime_root(&root);
                    let commit = contexts.commit();
                    let txcx = contexts.txn();
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    let mut seed = WriteBatch::new(RelationId(1));
                    for vid in 1..=4 {
                        seed.create_vertex(VId(vid), vec![], vec![]);
                    }
                    db.write(&commit, seed).await.unwrap();
                    let mut txn = db.begin(&txcx).unwrap();
                    assert!(txn.adjacency_neighbours(&db, VId(1), RelationId(1), incoming)
                        .unwrap().is_empty());
                    assert!(!txn.scanned_edges.get());
                    assert_eq!(*txn.read_set.borrow(),
                        [ElementId::Vertex(VId(1))].into_iter().collect());
                    let (src, dst) = if touches_anchor {
                        if incoming { (2, 1) } else { (1, 2) }
                    } else {
                        (3, 4)
                    };
                    let mut winner = WriteBatch::new(RelationId(1));
                    winner.add_edge(EId(20), VId(src), VId(dst), vec![]);
                    db.write(&commit, winner).await.unwrap();
                    // Reads still answer the pinned basis, not the winner.
                    assert!(txn.adjacency_neighbours(&db, VId(1), RelationId(1), incoming)
                        .unwrap().is_empty());
                    let completion = txn.finish(&mut db, &commit).await;
                    if touches_anchor {
                        assert!(matches!(completion,
                            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                                law: "FG-LAW-FCW-READ-01", ..
                            }))));
                    } else {
                        assert!(matches!(completion, Ok(EmbeddedTxnCompletion::ReadClosed { .. })));
                    }
                });
                assert!(report.lab_test_passed(), "{report:?}");
            }
        }
    }
}
