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

#[cfg(test)]
mod bulk_vertex_overlay_tests {
    use super::{WriteTxn, WriteTxnError};
    use crate::{Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteError};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

    const PROPERTY: PropertyKeyId = PropertyKeyId(1);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
    }

    // Point reads retain the full-net traversal, independent of bulk routing.
    // Compare the entire rows and exact observations, not just returned IDs.
    fn compare_with_points(
        transaction: &WriteTxn,
        database: &Database<MemVfs>,
        ids: &[VId],
        label: Option<LabelId>,
    ) -> Vec<VertexRow> {
        let before = transaction.read_set.borrow().clone();
        let mut expected: Vec<_> = ids
            .iter()
            .filter_map(|vid| transaction.vertex(database, *vid).expect("point read"))
            .collect();
        expected.sort_by_key(|row| row.vid);
        let observed = transaction.read_set.borrow().clone();
        *transaction.read_set.borrow_mut() = before;
        let actual = transaction
            .vertices_for_scan(database, label)
            .expect("bulk read");
        assert_eq!(actual, expected);
        assert_eq!(&*transaction.read_set.borrow(), &observed);
        actual
    }

    #[test]
    fn mixed_bulk_effects_match_points_at_the_pinned_basis() {
        let ((), report) = run_async_under_lab(0x7a_71, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            let mut database = Database::open_memory(&commit, keys())
                .await
                .expect("open");
            let mut seed = WriteBatch::new(RelationId(1));
            for vid in [VId(1), VId(2), VId(3)] {
                seed.create_vertex(
                    vid,
                    vec![LabelId(10), LabelId(20)],
                    vec![(PROPERTY, CanonicalScalar::Int(10))],
                );
            }
            // An edge and vertex share their numeric identity, but not a kind.
            seed.add_edge(
                EId(2),
                VId(1),
                VId(2),
                vec![(PROPERTY, CanonicalScalar::Int(1))],
            );
            database.write(&commit, seed).await.expect("seed");
            let mut transaction = database.begin(&txn_cx).expect("begin");
            let mut staged = WriteBatch::new(RelationId(1));
            staged.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(77)));
            staged.set_vertex_property(VId(2), PROPERTY, None);
            staged.set_vertex_label(VId(1), LabelId(10), false);
            staged.set_vertex_label(VId(2), LabelId(30), true);
            staged.delete_vertex(VId(3));
            staged.create_vertex(
                VId(4),
                vec![LabelId(20)],
                vec![(PROPERTY, CanonicalScalar::Int(40))],
            );
            staged.create_vertex(VId(5), vec![], vec![]);
            staged.delete_vertex(VId(5));
            staged.delete_vertex_if_present(VId(6));
            staged.ensure_vertex(VId(1), vec![LabelId(99)], vec![]);
            staged.set_edge_property(EId(2), PROPERTY, Some(CanonicalScalar::Int(999)));
            transaction.write(&mut database, staged).expect("stage");

            let ids = [VId(1), VId(2), VId(3), VId(4), VId(5), VId(6)];
            let rows = compare_with_points(&transaction, &database, &ids, Some(LabelId(20)));
            assert_eq!(
                rows.iter().map(|row| row.vid).collect::<Vec<_>>(),
                vec![VId(1), VId(2), VId(4)]
            );
            assert_eq!(rows[0].labels, vec![LabelId(20)]);
            assert_eq!(rows[0].props, vec![(PROPERTY, CanonicalScalar::Int(77))]);
            assert_eq!(rows[1].labels, vec![LabelId(10), LabelId(20), LabelId(30)]);
            assert!(rows[1].props.is_empty());
            assert_eq!(rows[2].created_at, transaction.basis());
            assert!(!transaction.scanned_vertices.get());
            assert!(
                transaction
                    .scanned_vertex_labels
                    .borrow()
                    .contains(&LabelId(20))
            );
            assert!(
                transaction
                    .read_set
                    .borrow()
                    .contains(&ElementId::Vertex(VId(5)))
            );
            assert!(
                transaction
                    .read_set
                    .borrow()
                    .contains(&ElementId::Vertex(VId(6)))
            );

            let mut advance = WriteBatch::new(RelationId(1));
            advance.create_vertex(VId(99), vec![LabelId(20)], vec![]);
            database.write(&commit, advance).await.expect("advance");
            assert_eq!(
                compare_with_points(&transaction, &database, &ids, None),
                rows
            );
            assert!(transaction.scanned_vertices.get());
            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "lab run failed: {report:?}");
    }

    #[test]
    fn relation_groups_keep_birth_ordinals_and_committed_contents() {
        let ((), report) = run_async_under_lab(0x7a_72, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            let mut database = Database::open_memory(&commit, keys())
                .await
                .expect("open");
            let mut transaction = database.begin(&txn_cx).expect("begin");
            let mut first = WriteBatch::new(RelationId(9));
            first.create_vertex(VId(20), vec![LabelId(2)], vec![]);
            first.create_vertex(VId(10), vec![LabelId(1)], vec![]);
            let mut second = WriteBatch::new(RelationId(1));
            second.create_vertex(VId(30), vec![LabelId(3)], vec![]);
            second.add_edge(EId(30), VId(20), VId(30), vec![]);
            let mut third = WriteBatch::new(RelationId(9));
            third.set_vertex_property(VId(20), PROPERTY, Some(CanonicalScalar::Int(42)));
            transaction
                .write_ordered(&mut database, vec![first, second, third])
                .expect("ordered relation groups");
            let mut rows = compare_with_points(
                &transaction,
                &database,
                &[VId(10), VId(20), VId(30)],
                None,
            );
            assert_eq!(rows.len(), 3);
            assert_eq!(rows[1].props, vec![(PROPERTY, CanonicalScalar::Int(42))]);
            let committed_at = transaction
                .commit(&mut database, &commit)
                .await
                .expect("commit");
            for row in &mut rows {
                row.created_at = committed_at;
            }
            assert_eq!(database.vertices().expect("durable vertices"), rows);
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "lab run failed: {report:?}");
    }

    #[test]
    fn normalized_away_identity_remains_a_negative_read_witness() {
        let ((), report) = run_async_under_lab(0x7a_73, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            let mut database = Database::open_memory(&commit, keys())
                .await
                .expect("open");
            let mut transaction = database.begin(&txn_cx).expect("begin");
            let mut staged = WriteBatch::new(RelationId(1));
            staged.create_vertex(VId(9), vec![], vec![]);
            staged.delete_vertex(VId(9));
            transaction.write(&mut database, staged).expect("stage no-op");
            assert!(
                transaction
                    .vertices_for_scan(&database, Some(LabelId(77)))
                    .expect("empty scan")
                    .is_empty()
            );
            assert!(
                transaction
                    .read_set
                    .borrow()
                    .contains(&ElementId::Vertex(VId(9)))
            );
            assert!(!transaction.scanned_vertices.get());

            // No label-77 insertion occurs. READ-01 must therefore come from
            // the absent identity, not the label or full-scan insertion guard.
            let mut concurrent = WriteBatch::new(RelationId(1));
            concurrent.create_vertex(VId(9), vec![], vec![]);
            database
                .write(&commit, concurrent)
                .await
                .expect("concurrent create");
            assert!(matches!(
                transaction.finish(&mut database, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law, .. }))
                    if law == "FG-LAW-FCW-READ-01"
            ));
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "lab run failed: {report:?}");
    }
}
