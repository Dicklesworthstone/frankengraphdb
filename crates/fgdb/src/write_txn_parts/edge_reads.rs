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
        let mut observed_vertices = std::collections::BTreeSet::new();
        if let Some(record) = &overlay {
            observed_vertices.insert(record.entry.src);
        }
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for effect in &coordinate.rows {
                    if let Some(vertex) = self.apply_edge_effect(eid, &mut overlay, effect) {
                        observed_vertices.insert(vertex);
                    }
                }
            }
        }
        let mut read_set = self.read_set.borrow_mut();
        read_set.insert(ElementId::Edge(eid));
        read_set.extend(observed_vertices.into_iter().map(ElementId::Vertex));
        overlay
    }

    /// Apply a canonical row and return its source/cascade vertex witness.
    /// The witness is independent of whether the edge survives as an output.
    /// Bulk reads dispatch single-edge effects here by identity; cascades walk
    /// their explicit EIds once instead of testing every edge against each one.
    fn apply_edge_effect(
        &self,
        eid: EId,
        overlay: &mut Option<EdgeRecord>,
        effect: &fgdb_delta_types::DeltaRow,
    ) -> Option<VId> {
        use fgdb_delta_types::DeltaRow;
        match effect {
            DeltaRow::CreateEdge {
                eid: row_eid, src, relation, dst, props, ..
            } if *row_eid == eid => {
                *overlay = Some(EdgeRecord {
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
                Some(*src)
            }
            DeltaRow::DeleteEdge { eid: row_eid, .. } if *row_eid == eid => {
                *overlay = None;
                None
            }
            DeltaRow::Property {
                elem: ElementId::Edge(row_eid), property, after, ..
            } if *row_eid == eid => {
                if let Some(record) = overlay.as_mut() {
                    Self::overlay_property(&mut record.props, *property, after.as_ref());
                }
                None
            }
            DeltaRow::DeleteVertex {
                vid, sorted_retired_incident_edges, ..
            } if sorted_retired_incident_edges.binary_search(&eid).is_ok() => {
                *overlay = None;
                Some(*vid)
            }
            _ => None,
        }
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
        {
            let mut read_set = self.read_set.borrow_mut();
            // Capture baseline sources before removing any rows. Sources of
            // retired edges and negative identities remain read dependencies.
            read_set.extend(eids.iter().copied().map(ElementId::Edge));
            read_set.extend(basis.values().map(|record| ElementId::Vertex(record.entry.src)));
            if let Some(prepared) = &self.prepared {
                for coordinate in prepared.template.coordinate_entries() {
                    for effect in &coordinate.rows {
                        use fgdb_delta_types::DeltaRow;
                        let eid = match effect {
                            DeltaRow::CreateEdge { eid, .. }
                            | DeltaRow::DeleteEdge { eid, .. }
                            | DeltaRow::Property { elem: ElementId::Edge(eid), .. } => *eid,
                            DeltaRow::DeleteVertex {
                                vid, sorted_retired_incident_edges, ..
                            } => {
                                for eid in sorted_retired_incident_edges {
                                    // A preceding delete must not erase the
                                    // later cascade's observation of this ID.
                                    if eids.contains(eid) {
                                        read_set.insert(ElementId::Vertex(*vid));
                                    }
                                    basis.remove(eid);
                                }
                                continue;
                            }
                            _ => continue,
                        };
                        let mut overlay = basis.remove(&eid);
                        if let Some(vertex) = self.apply_edge_effect(eid, &mut overlay, effect) {
                            read_set.insert(ElementId::Vertex(vertex));
                        }
                        if let Some(record) = overlay {
                            basis.insert(eid, record);
                        }
                    }
                }
            }
            read_set.extend(basis.keys().copied().map(ElementId::Edge));
            read_set.extend(basis.values().map(|record| ElementId::Vertex(record.entry.src)));
        }
        self.match_expansions.borrow_mut().extend(
            basis.values().map(|record| (record.entry.src, record.entry.relation)),
        );
        // Identity order comes from the map. Neither effects nor cascade
        // images are replayed per output row, and no second sort is needed.
        Ok(basis.into_values().collect())
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

    /// Resolve only incident historical identities, borrowing topology from
    /// the admitted generation rather than cloning every edge and property.
    /// Keep EIds until after overlay application: neighbour deduplication here
    /// would lose the last-surviving-parallel-edge rule and read witnesses.
    fn adjacency_basis<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vertex: VId,
        relation: RelationId,
        incoming: bool,
        control: &mut impl FnMut(fgdb_gql::GlaExecutionEvent) -> Result<(), WriteTxnError>,
    ) -> Result<std::collections::BTreeMap<EId, VId>, WriteTxnError> {
        use fgdb_gql::algebra::GlaDirection;

        self.ensure_database(database)?;
        // Direct snapshot access must retain the health fence previously
        // enforced by edges_at(), even for empty or entirely staged adjacency.
        database.ensure_readable()?;
        let snapshot = &database.snapshot;
        snapshot.check_frontier(self.basis)?;
        let direction = if incoming {
            GlaDirection::Reverse
        } else {
            GlaDirection::Forward
        };
        let index = &snapshot.adjacency_index;
        let mut matching = std::collections::BTreeMap::new();
        let mut after = None;
        while let Some(eid) = index.next_incident_edge(vertex, direction, after, control)? {
            // Advance even for invisible or other-relation candidates. Strict
            // successors also handle zero and u128::MAX without arithmetic.
            after = Some(eid);
            let Some((block, row)) = index.statement_at(&snapshot.blocks, eid, self.basis) else {
                continue;
            };
            let entry = &snapshot.blocks[block][row];
            let (anchor, neighbour) = if incoming {
                (entry.dst, entry.src)
            } else {
                (entry.src, entry.dst)
            };
            // The index is a historical superset, never an authority beside
            // the exact-cut winner. In particular, do not resurrect retired
            // edges or admit creations newer than the pinned transaction.
            if anchor == vertex && entry.relation == relation {
                matching.insert(eid, neighbour);
            }
        }
        Ok(matching)
    }

    fn adjacency_neighbours<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vertex: VId,
        relation: RelationId,
        incoming: bool,
    ) -> Result<Vec<VId>, WriteTxnError> {
        // Do not call edges(): that would turn a local expansion into a global
        // edge-scan conflict witness. The endpoint read below also detects a
        // previously empty incoming adjacency through adjacency_endpoints.
        let mut matching = self.adjacency_basis(
            database, vertex, relation, incoming, &mut |_| Ok(()),
        )?;
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
                let ((), report) = run_async_under_lab(0xa91c_0003, move |root| async move {
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

    #[test]
    fn indexed_adjacency_preserves_pinned_history_and_local_dependencies() {
        use fgdb_delta_types::PropertyKeyId;

        let ((), report) = run_async_under_lab(0xa91c_0031, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for vid in 0..=130 {
                seed.create_vertex(VId(vid), vec![], vec![]);
            }
            for (eid, src, dst) in [
                (0, 0, 1), (7, 0, 1), (8, 1, 0), (u128::MAX, 0, 0),
            ] {
                seed.add_edge(EId(eid), VId(src), VId(dst), vec![]);
            }
            for vid in 2..130 {
                seed.add_edge(EId(1000 + vid), VId(vid), VId(vid + 1), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let mut other = WriteBatch::new(RelationId(2));
            other.add_edge(EId(90), VId(0), VId(2), vec![]);
            db.write(&commit, other).await.unwrap();
            let mut pinned = db.begin(&txcx).unwrap();
            let basis = pinned.basis();
            let mut changes = WriteBatch::new(RelationId(1));
            changes.delete_edge(EId(0));
            changes.delete_edge(EId(8));
            changes.set_edge_property(EId(7), PropertyKeyId(1),
                Some(CanonicalScalar::Int(42)));
            changes.add_edge(EId(40), VId(0), VId(2), vec![]);
            changes.add_edge(EId(41), VId(2), VId(0), vec![]);
            db.write(&commit, changes).await.unwrap();

            for incoming in [false, true] {
                let mut seek_work = 0;
                let indexed = pinned.adjacency_basis(
                    &db, VId(0), RelationId(1), incoming, &mut |_| {
                        seek_work += 1;
                        Ok(())
                    },
                ).unwrap();
                let expected: std::collections::BTreeMap<_, _> = db.edges_at(basis)
                    .unwrap().into_iter().filter_map(|record| {
                        let entry = record.entry;
                        let (anchor, neighbour) = if incoming {
                            (entry.dst, entry.src)
                        } else {
                            (entry.src, entry.dst)
                        };
                        (anchor == VId(0) && entry.relation == RelationId(1))
                            .then_some((entry.eid, neighbour))
                    }).collect();
                assert_eq!(indexed, expected);
                assert!(seek_work < 128, "unrelated history was scanned: {seek_work}");
                assert_eq!(pinned.adjacency_neighbours(&db, VId(0), RelationId(1), incoming)
                    .unwrap(), vec![VId(0), VId(1)]);
            }
            assert!(!pinned.scanned_edges.get());
            assert_eq!(*pinned.read_set.borrow(), [
                ElementId::Vertex(VId(0)), ElementId::Edge(EId(0)),
                ElementId::Edge(EId(7)), ElementId::Edge(EId(8)),
                ElementId::Edge(EId(u128::MAX)),
            ].into_iter().collect());

            let current = db.begin(&txcx).unwrap();
            assert_eq!(current.neighbours(&db, VId(0), RelationId(1)).unwrap(),
                vec![VId(0), VId(1), VId(2)]);
            assert_eq!(current.in_neighbours(&db, VId(0), RelationId(1)).unwrap(),
                vec![VId(0), VId(2)]);
            assert_eq!(current.neighbours(&db, VId(0), RelationId(2)).unwrap(), vec![VId(2)]);
            assert!(!current.read_set.borrow().contains(&ElementId::Edge(EId(0))));
            assert!(!current.read_set.borrow().contains(&ElementId::Edge(EId(8))));
            current.abort();
            assert!(matches!(pinned.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn indexed_adjacency_source_refusal_does_not_publish_partial_dependencies() {
        let ((), report) = run_async_under_lab(0xa91c_0032, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for vid in 0..4 {
                seed.create_vertex(VId(vid), vec![], vec![]);
            }
            for eid in 0..12 {
                seed.add_edge(EId(eid), VId(0), VId(eid % 4), vec![]);
            }
            db.write(&commit, seed).await.unwrap();
            let txn = db.begin(&txcx).unwrap();
            for incoming in [false, true] {
                let mut total = 0;
                let expected = txn.adjacency_basis(
                    &db, VId(0), RelationId(1), incoming, &mut |_| {
                        total += 1;
                        Ok(())
                    },
                ).unwrap();
                assert!(!expected.is_empty());
                for stop in 1..=total {
                    let mut calls = 0;
                    let result = txn.adjacency_basis(
                        &db, VId(0), RelationId(1), incoming, &mut |_| {
                            calls += 1;
                            if calls == stop {
                                // A deterministic sentinel for the fallible source seam.
                                Err(WriteTxnError::NoPreparedWrite)
                            } else {
                                Ok(())
                            }
                        },
                    );
                    assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)));
                    assert_eq!(calls, stop);
                    assert!(txn.read_set.borrow().is_empty());
                    assert!(!txn.scanned_edges.get());
                    assert_eq!(txn.adjacency_basis(
                        &db, VId(0), RelationId(1), incoming, &mut |_| Ok(()),
                    ).unwrap(), expected);
                }
            }
            txn.abort();
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn indexed_adjacency_preserves_real_commit_failure_fences() {
        use crate::{DatabaseState, DerivedPublicationStage};
        use fgdb_chronicle::commit::CrashPoint;

        let ((), report) = run_async_under_lab(0xa91c_0033, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            for (crash, publication_failure) in [
                (Some(CrashPoint::AfterMarkerBeforeD2), None),
                (None, Some(DerivedPublicationStage::FoldCommittedTemplate)),
            ] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(RelationId(1));
                for vid in 1..=3 {
                    seed.create_vertex(VId(vid), vec![], vec![]);
                }
                seed.add_edge(EId(10), VId(1), VId(2), vec![]);
                db.write(&commit, seed).await.unwrap();
                let view = db.read_session().unwrap();
                let mut txn = db.begin(&txcx).unwrap();
                txn.vertex(&db, VId(2)).unwrap();
                let mut staged = WriteBatch::new(RelationId(1));
                staged.add_edge(EId(20), VId(2), VId(3), vec![]);
                txn.write(&mut db, staged).unwrap();
                let expected_template = txn.prepared.as_ref().unwrap().template.clone();
                let expected_reads = txn.read_set.borrow().clone();
                let expected_expansions = txn.match_expansions.borrow().clone();
                let basis = txn.basis();

                let mut winner = WriteBatch::new(RelationId(1));
                winner.create_vertex(VId(4), vec![], vec![]);
                let prepared = db.prepare_write(winner).unwrap();
                // Enter actual Chronicle and derived-publication failure paths;
                // assigning a fake health flag would not exercise these fences.
                assert!(db.commit_template(
                    &commit, prepared.template, crash, publication_failure, None,
                ).await.is_err());
                let fence = db.state();
                let assert_fence = |error: WriteTxnError| match (fence, error) {
                    (
                        DatabaseState::CommitOutcomeUnknown { published_frontier },
                        WriteTxnError::Read(ReadError::CommitOutcomeUnknown {
                            published_frontier: observed,
                        }),
                    ) => {
                        assert_eq!(published_frontier, basis);
                        assert_eq!(observed, basis);
                        assert!(publication_failure.is_none());
                    }
                    (
                        DatabaseState::NeedsAuthoritativeRecovery(expected),
                        WriteTxnError::Read(ReadError::RecoveryRequired(observed)),
                    ) => {
                        assert_eq!(observed, expected);
                        assert_eq!(observed.published_frontier, basis);
                        assert_eq!(observed.durable_frontier, CommitSeq(basis.0 + 1));
                        assert!(publication_failure.is_some());
                    }
                    (state, error) => panic!("wrong read fence: {state:?}: {error:?}"),
                };
                for incoming in [false, true] {
                    // Cover stored rows, entirely staged incidence, and absence.
                    for anchor in [VId(1), VId(2), VId(3), VId(999)] {
                        let mut checkpoints = 0;
                        let result = txn.adjacency_basis(
                            &db, anchor, RelationId(1), incoming, &mut |_| {
                                checkpoints += 1;
                                Err(WriteTxnError::NoPreparedWrite)
                            },
                        );
                        assert_fence(result.unwrap_err());
                        assert_eq!(checkpoints, 0, "health refusal must precede index work");
                        let result = if incoming {
                            txn.in_neighbours(&db, anchor, RelationId(1))
                        } else {
                            txn.neighbours(&db, anchor, RelationId(1))
                        };
                        assert_fence(result.unwrap_err());
                    }
                }
                assert_eq!(*txn.read_set.borrow(), expected_reads);
                assert_eq!(*txn.match_expansions.borrow(), expected_expansions);
                assert!(!txn.scanned_edges.get());
                assert!(!txn.scanned_vertices.get());
                assert_eq!(txn.staged.len(), 1);
                assert_eq!(txn.prepared.as_ref().unwrap().template, expected_template);
                assert_eq!(txn.state(), EmbeddedTxnState::Active);
                assert_eq!(txcx.outstanding_obligations(), 1);
                assert_eq!(db.state(), fence);
                // A previously admitted immutable view remains independently
                // readable; this fix must not globally poison old generations.
                assert_eq!(view.neighbours(VId(1), RelationId(1)).unwrap(), vec![VId(2)]);
                assert_eq!(view.in_neighbours(VId(2), RelationId(1)).unwrap(), vec![VId(1)]);
                txn.abort();
                assert_eq!(txcx.outstanding_obligations(), 0);
                let recovered = db.recover_authoritatively(&commit).await.unwrap();
                assert_eq!(recovered.neighbours(VId(1), RelationId(1)).unwrap(), vec![VId(2)]);
                assert!(recovered.edge(EId(20)).unwrap().is_none());
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[cfg(test)]
mod edge_bulk_overlay_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn keys() -> crate::DatabaseKeys {
        crate::DatabaseKeys::new(
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
            [0xb3; 32],
        )
    }

    fn assert_contents(actual: &EdgeRecord, expected: &EdgeRecord) {
        assert_eq!(actual.entry.eid, expected.entry.eid);
        assert_eq!(actual.entry.src, expected.entry.src);
        assert_eq!(actual.entry.dst, expected.entry.dst);
        assert_eq!(actual.entry.relation, expected.entry.relation);
        assert_eq!(actual.entry.retired_at, expected.entry.retired_at);
        assert_eq!(actual.props, expected.props);
    }

    #[test]
    fn bulk_edge_overlay_matches_point_reads_and_committed_contents() {
        let ((), report) = run_async_under_lab(0xa91c_0021, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for vid in 1..=4 {
                seed.create_vertex(VId(vid), vec![], vec![]);
            }
            for eid in (10..138).rev() {
                seed.add_edge(EId(eid), VId(1), VId(2),
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(10))]);
            }
            seed.add_edge(EId(500), VId(3), VId(4), vec![]);
            seed.add_edge(EId(501), VId(4), VId(3), vec![]);
            seed.add_edge(EId(502), VId(3), VId(3), vec![]);
            db.write(&commit, seed).await.unwrap();
            let mut other = WriteBatch::new(RelationId(2));
            other.add_edge(EId(900), VId(1), VId(4), vec![]);
            db.write(&commit, other).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let mut changes = WriteBatch::new(RelationId(1));
            changes.ensure_edge_by_triple(EId(600), VId(1), VId(2), vec![]);
            for eid in 10..138 {
                match eid % 4 {
                    0 => { changes.delete_edge(EId(eid)); }
                    1 => {
                        changes.set_edge_property(EId(eid), PropertyKeyId(1),
                            Some(CanonicalScalar::Int(20)));
                        changes.set_edge_property(EId(eid), PropertyKeyId(2),
                            Some(CanonicalScalar::Int(30)));
                    }
                    2 => { changes.set_edge_property(EId(eid), PropertyKeyId(1), None); }
                    _ => {}
                }
            }
            for eid in (200..232).rev() {
                changes.add_edge(EId(eid), VId(2), VId(1), vec![]);
            }
            changes.add_edge(EId(700), VId(1), VId(4), vec![]);
            changes.delete_edge(EId(700));
            changes.delete_edge(EId(500));
            changes.delete_vertex(VId(3));
            changes.delete_edge_if_present(EId(999));
            txn.write(&mut db, changes).unwrap();
            let rows = txn.edges(&db).unwrap();
            let expected_ids: Vec<_> = (10..138).filter(|eid| eid % 4 != 0)
                .chain(200..232).chain([900]).map(EId).collect();
            assert_eq!(rows.iter().map(|row| row.entry.eid).collect::<Vec<_>>(), expected_ids);
            for row in &rows {
                let point = txn.edge(&db, row.entry.eid).unwrap().unwrap();
                assert_contents(&point, row);
                assert_eq!(point.entry.created_at, row.entry.created_at);
                if row.entry.eid.0 < 138 {
                    assert_eq!(row.entry.src, VId(1));
                    assert_eq!(row.entry.dst, VId(2));
                    assert_eq!(row.entry.relation, RelationId(1));
                    match row.entry.eid.0 % 4 {
                        1 => assert_eq!(row.props, vec![
                            (PropertyKeyId(1), CanonicalScalar::Int(20)),
                            (PropertyKeyId(2), CanonicalScalar::Int(30)),
                        ]),
                        2 => assert!(row.props.is_empty()),
                        3 => assert_eq!(row.props,
                            vec![(PropertyKeyId(1), CanonicalScalar::Int(10))]),
                        _ => panic!("deleted edge was materialized"),
                    }
                } else if row.entry.eid != EId(900) {
                    assert_eq!(row.entry.src, VId(2));
                    assert_eq!(row.entry.dst, VId(1));
                    assert_eq!(row.entry.created_at, txn.basis());
                }
            }
            for eid in [500, 501, 502, 600, 700, 999] {
                assert!(txn.read_set.borrow().contains(&ElementId::Edge(EId(eid))));
                assert!(txn.edge(&db, EId(eid)).unwrap().is_none());
            }
            let committed_at = txn.commit(&mut db, &commit).await.unwrap();
            let committed = db.edges_at(committed_at).unwrap();
            assert_eq!(committed.len(), rows.len());
            let committed: std::collections::BTreeMap<_, _> = committed.into_iter()
                .map(|row| (row.entry.eid, row)).collect();
            for row in rows {
                assert_contents(committed.get(&row.entry.eid).unwrap(), &row);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn bulk_edge_scan_retains_exact_point_dependencies_for_cascades_and_absences() {
        let ((), report) = run_async_under_lab(0xa91c_0022, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for vid in 1..=3 {
                seed.create_vertex(VId(vid), vec![], vec![]);
            }
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            seed.add_edge(EId(11), VId(3), VId(2), vec![]);
            db.write(&commit, seed).await.unwrap();
            let mut bulk = db.begin(&txcx).unwrap();
            let mut points = db.begin(&txcx).unwrap();
            bulk.savepoint(&db, "before").unwrap();
            let mut changes = WriteBatch::new(RelationId(1));
            changes.delete_edge(EId(10));
            changes.delete_vertex(VId(2));
            changes.delete_edge_if_present(EId(999));
            bulk.write(&mut db, changes.clone()).unwrap();
            points.write(&mut db, changes).unwrap();
            assert!(bulk.read_set.borrow().is_empty());
            assert!(points.read_set.borrow().is_empty());
            assert!(bulk.edges(&db).unwrap().is_empty());
            for eid in [10, 11, 999] {
                assert!(points.edge(&db, EId(eid)).unwrap().is_none());
            }
            let expected: std::collections::BTreeSet<_> = [
                ElementId::Edge(EId(10)), ElementId::Edge(EId(11)),
                ElementId::Edge(EId(999)), ElementId::Vertex(VId(1)),
                ElementId::Vertex(VId(2)), ElementId::Vertex(VId(3)),
            ].into_iter().collect();
            assert_eq!(*bulk.read_set.borrow(), expected);
            assert_eq!(*bulk.read_set.borrow(), *points.read_set.borrow());
            assert!(bulk.scanned_edges.get());
            assert!(!points.scanned_edges.get());
            points.abort();
            bulk.rollback_to_savepoint(&db, "before").unwrap();
            assert_eq!(*bulk.read_set.borrow(), expected);
            let mut winner = WriteBatch::new(RelationId(1));
            // No edge insertion: only the cascade target's retained point
            // witness can explain this read conflict after effects rewind.
            winner.set_vertex_property(VId(2), PropertyKeyId(1),
                Some(CanonicalScalar::Int(42)));
            db.write(&commit, winner).await.unwrap();
            assert!(matches!(bulk.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn bulk_edge_overlay_applies_every_atomic_relation_coordinate() {
        let ((), report) = run_async_under_lab(0xa91c_0023, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for vid in 1..=4 {
                seed.create_vertex(VId(vid), vec![], vec![]);
            }
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(&commit, seed).await.unwrap();
            let mut second = WriteBatch::new(RelationId(2));
            second.add_edge(EId(20), VId(3), VId(4), vec![]);
            db.write(&commit, second).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let mut first = WriteBatch::new(RelationId(1));
            first.delete_edge(EId(10));
            first.add_edge(EId(11), VId(2), VId(1),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(1))]);
            let mut second = WriteBatch::new(RelationId(2));
            second.delete_edge(EId(20));
            second.add_edge(EId(21), VId(4), VId(3),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(2))]);
            txn.write_atomic(&mut db, vec![second, first]).unwrap();
            let rows = txn.edges(&db).unwrap();
            assert_eq!(rows.iter().map(|row| (row.entry.eid, row.entry.relation))
                .collect::<Vec<_>>(), vec![(EId(11), RelationId(1)), (EId(21), RelationId(2))]);
            for row in &rows {
                assert_contents(&txn.edge(&db, row.entry.eid).unwrap().unwrap(), row);
                assert_eq!(row.entry.created_at, txn.basis());
            }
            let seq = txn.commit(&mut db, &commit).await.unwrap();
            for row in rows {
                let committed = db.edge_at(row.entry.eid, seq).unwrap().unwrap();
                assert_contents(&committed, &row);
            }
            assert!(db.edge_at(EId(10), seq).unwrap().is_none());
            assert!(db.edge_at(EId(20), seq).unwrap().is_none());
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
