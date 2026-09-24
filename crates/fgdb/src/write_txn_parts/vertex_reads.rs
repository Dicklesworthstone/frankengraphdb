impl WriteTxn {
    /// Read one vertex from the pinned durable basis plus the canonical staged
    /// effects. This performs no preparation or publication.
    pub fn vertex<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        vid: VId,
    ) -> Result<Option<VertexRow>, WriteTxnError> {
        self.ensure_database(database)?;

        let live = database.frontier()?;
        let overlay = if live == self.basis {
            database.vertex(vid)?
        } else {
            database.vertex_at(vid, self.basis)?
        };
        Ok(self.vertex_over_basis(vid, overlay))
    }

    /// Point and bulk reads apply precisely the net effects that commit will
    /// publish, just as edge reads do. Reinterpreting raw intents would invent
    /// transient creations or assign the wrong birth ordinal after grouping
    /// independent relation prefixes. Before commit, created_at is the basis
    /// placeholder; birth ordinals and content already match the prepared net.
    fn vertex_over_basis(&self, vid: VId, mut overlay: Option<VertexRow>) -> Option<VertexRow> {
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for effect in &coordinate.rows {
                    self.apply_vertex_effect(vid, &mut overlay, effect);
                }
            }
        }
        // Negative reads remain observations even when NENF erased a create
        // and its delete, or an ensure emitted no effect.
        self.read_set.borrow_mut().insert(ElementId::Vertex(vid));
        overlay
    }

    /// The single-row canonical interpreter shared by point and bulk reads.
    /// Bulk scans dispatch by identity first, instead of replaying the complete
    /// template once for every vertex in the snapshot.
    fn apply_vertex_effect(
        &self,
        vid: VId,
        overlay: &mut Option<VertexRow>,
        effect: &fgdb_delta_types::DeltaRow,
    ) {
        use fgdb_delta_types::DeltaRow;
        match effect {
            DeltaRow::CreateVertex {
                vid: row_vid,
                birth_ordinal,
                labels,
                props,
                ..
            } if *row_vid == vid => {
                *overlay = Some(VertexRow {
                    vid,
                    birth_ordinal: *birth_ordinal,
                    created_at: self.basis,
                    retired_at: None,
                    labels: labels.clone(),
                    props: props.clone(),
                });
            }
            DeltaRow::DeleteVertex { vid: row_vid, .. } if *row_vid == vid => {
                *overlay = None;
            }
            DeltaRow::LabelMembership {
                vid: row_vid,
                label,
                after,
                ..
            } if *row_vid == vid => {
                if let Some(row) = overlay.as_mut() {
                    match row.labels.binary_search(label) {
                        Ok(at) if !after => {
                            row.labels.remove(at);
                        }
                        Err(at) if *after => row.labels.insert(at, *label),
                        Ok(_) | Err(_) => {}
                    }
                }
            }
            DeltaRow::Property {
                elem: ElementId::Vertex(row_vid),
                property,
                after,
                ..
            } if *row_vid == vid => {
                if let Some(row) = overlay.as_mut() {
                    Self::overlay_property(&mut row.props, *property, after.as_ref());
                }
            }
            _ => {}
        }
    }

    /// Read every vertex from the pinned basis through canonical staged effects,
    /// sorted by vertex identity. Empty results retain an insertion witness.
    pub fn vertices<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
    ) -> Result<Vec<VertexRow>, WriteTxnError> {
        self.vertices_for_scan(database, None)
    }

    /// Read the same complete overlay for evaluation and budget accounting,
    /// but scope insertion dependencies to a node plan's required label.
    /// Existing rows remain conservatively observed, including filtered rows.
    fn vertices_for_scan<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        label: Option<LabelId>,
    ) -> Result<Vec<VertexRow>, WriteTxnError> {
        self.ensure_database(database)?;

        let mut basis: std::collections::BTreeMap<VId, VertexRow> = database
            .vertices_at(self.basis)?
            .into_iter()
            .map(|row| (row.vid, row))
            .collect();
        if let Some(label) = label {
            self.scanned_vertex_labels.borrow_mut().insert(label);
        } else {
            self.scanned_vertices.set(true);
        }
        {
            let mut observations = self.read_set.borrow_mut();
            // Observe before removing rows. A deleted row and a normalized
            // absent identity remain dependencies even though neither is output.
            observations.extend(basis.keys().copied().map(ElementId::Vertex));
            for pending in self.staged.iter().flat_map(|batch| &batch.rows) {
                match pending {
                    PendingRow::Vertex { vid, .. } | PendingRow::DeleteVertex { vid, .. } => {
                        observations.insert(ElementId::Vertex(*vid));
                    }
                    PendingRow::Edge { .. }
                    | PendingRow::DeleteEdge { .. }
                    | PendingRow::SetLabel { .. }
                    | PendingRow::SetEdgeProperty { .. }
                    | PendingRow::SetProperty { .. }
                    | PendingRow::CompareAndSet { .. } => {}
                }
            }
        }
        if let Some(prepared) = &self.prepared {
            for coordinate in prepared.template.coordinate_entries() {
                for effect in &coordinate.rows {
                    use fgdb_delta_types::DeltaRow;
                    let vid = match effect {
                        DeltaRow::CreateVertex { vid, .. }
                        | DeltaRow::DeleteVertex { vid, .. }
                        | DeltaRow::LabelMembership { vid, .. }
                        | DeltaRow::Property { elem: ElementId::Vertex(vid), .. } => *vid,
                        _ => continue,
                    };
                    let mut overlay = basis.remove(&vid);
                    self.apply_vertex_effect(vid, &mut overlay, effect);
                    if let Some(row) = overlay {
                        basis.insert(vid, row);
                    }
                }
            }
        }
        self.read_set.borrow_mut().extend(basis.keys().copied().map(ElementId::Vertex));
        // BTreeMap already supplies canonical identity order. Each net effect
        // is visited once; no per-vertex replay or additional sort is needed.
        Ok(basis.into_values().collect())
    }
}

#[cfg(test)]
mod vertex_overlay_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn keys() -> crate::DatabaseKeys {
        crate::DatabaseKeys::new(
            [0xa1; 32],
            DatabaseSecurityNamespaceId([0xa2; 32]),
            [0xa3; 32],
        )
    }

    #[test]
    fn bulk_vertex_overlay_matches_point_reads_and_committed_contents() {
        let ((), report) = run_async_under_lab(0xa91c_0011, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for vid in (1..=128).rev() {
                seed.create_vertex(VId(vid), vec![LabelId(1)],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(10))]);
            }
            db.write(&commit, seed).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let mut changes = WriteBatch::new(RelationId(1));
            for vid in 1..=128 {
                match vid % 4 {
                    0 => { changes.delete_vertex(VId(vid)); }
                    1 => {
                        changes.set_vertex_property(VId(vid), PropertyKeyId(1),
                            Some(CanonicalScalar::Int(20)));
                        changes.set_vertex_label(VId(vid), LabelId(2), true);
                    }
                    2 => {
                        changes.set_vertex_property(VId(vid), PropertyKeyId(1), None);
                        changes.set_vertex_label(VId(vid), LabelId(1), false);
                    }
                    _ => { changes.ensure_vertex(VId(vid), vec![LabelId(99)], vec![]); }
                }
            }
            for vid in (200..232).rev() {
                changes.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
            }
            changes.create_vertex(VId(999), vec![], vec![]);
            changes.delete_vertex(VId(999));
            txn.write(&mut db, changes).unwrap();
            let rows = txn.vertices(&db).unwrap();
            assert_eq!(rows.len(), 128);
            assert!(rows.windows(2).all(|pair| pair[0].vid < pair[1].vid));
            for row in &rows {
                let point = txn.vertex(&db, row.vid).unwrap().unwrap();
                assert_eq!(point.birth_ordinal, row.birth_ordinal);
                assert_eq!(point.created_at, row.created_at);
                assert_eq!(point.retired_at, row.retired_at);
                assert_eq!(point.labels, row.labels);
                assert_eq!(point.props, row.props);
                if row.vid.0 <= 128 {
                    match row.vid.0 % 4 {
                        1 => {
                            assert_eq!(row.labels, vec![LabelId(1), LabelId(2)]);
                            assert_eq!(row.props, vec![(PropertyKeyId(1), CanonicalScalar::Int(20))]);
                        }
                        2 => { assert!(row.labels.is_empty()); assert!(row.props.is_empty()); }
                        3 => {
                            assert_eq!(row.labels, vec![LabelId(1)]);
                            assert_eq!(row.props, vec![(PropertyKeyId(1), CanonicalScalar::Int(10))]);
                        }
                        _ => panic!("deleted vertex was materialized"),
                    }
                } else {
                    assert_eq!(row.labels, vec![LabelId(3)]);
                }
            }
            assert!(txn.vertex(&db, VId(999)).unwrap().is_none());
            assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(999))));
            txn.commit(&mut db, &commit).await.unwrap();
            assert_eq!(db.vertices_at(db.frontier().unwrap()).unwrap().len(), rows.len());
            for row in rows {
                let committed = db.vertex(row.vid).unwrap().unwrap();
                assert_eq!(committed.birth_ordinal, row.birth_ordinal);
                assert_eq!(committed.labels, row.labels);
                assert_eq!(committed.props, row.props);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn bulk_scan_keeps_normalized_negative_reads_after_savepoint_rollback() {
        let ((), report) = run_async_under_lab(0xa91c_0012, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.savepoint(&db, "before-read").unwrap();
            let mut missing = WriteBatch::new(RelationId(1));
            missing.delete_vertex_if_present(VId(50));
            txn.write(&mut db, missing).unwrap();
            assert!(txn.vertices_for_scan(&db, Some(LabelId(9))).unwrap().is_empty());
            assert!(!txn.scanned_vertices.get());
            assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(50))));
            txn.rollback_to_savepoint(&db, "before-read").unwrap();
            let mut winner = WriteBatch::new(RelationId(1));
            // Not a label-9 phantom: the retained negative identity read must
            // reject this insertion even after its no-op preparation is gone.
            winner.create_vertex(VId(50), vec![LabelId(7)], vec![]);
            db.write(&commit, winner).await.unwrap();
            assert!(matches!(txn.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn label_scoped_bulk_scan_does_not_become_an_all_vertex_phantom_read() {
        for matching_label in [false, true] {
            let ((), report) = run_async_under_lab(0xa91c_0013, move |root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let txcx = contexts.txn();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(RelationId(1));
                seed.create_vertex(VId(1), vec![LabelId(9)], vec![]);
                db.write(&commit, seed).await.unwrap();
                let mut txn = db.begin(&txcx).unwrap();
                assert_eq!(txn.vertices_for_scan(&db, Some(LabelId(9))).unwrap().len(), 1);
                assert!(!txn.scanned_vertices.get());
                let mut winner = WriteBatch::new(RelationId(1));
                winner.create_vertex(VId(2),
                    vec![LabelId(if matching_label { 9 } else { 7 })], vec![]);
                db.write(&commit, winner).await.unwrap();
                let result = txn.finish(&mut db, &commit).await;
                if matching_label {
                    assert!(matches!(result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01", ..
                        }))));
                } else {
                    assert!(matches!(result, Ok(EmbeddedTxnCompletion::ReadClosed { .. })));
                }
            });
            assert!(report.lab_test_passed(), "{report:?}");
        }
    }
}
