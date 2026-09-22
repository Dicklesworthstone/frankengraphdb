use super::*;
use crate::{CrashPoint, DatabaseKeys, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}
fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}
async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut seed = WriteBatch::new(RelationId(1));
    for id in 1..=4 {
        seed.create_vertex(VId(id), vec![], vec![(P, int(0))]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![(P, int(10))]);
    db.write(cx, seed).await.unwrap();
    let mut other = WriteBatch::new(RelationId(2));
    other.add_edge(EId(20), VId(2), VId(3), vec![(P, int(20))]);
    db.write(cx, other).await.unwrap();
    db
}
fn dependent_program() -> Vec<WriteBatch> {
    let mut first = WriteBatch::new(RelationId(9));
    first.create_vertex(VId(5), vec![LabelId(1)], vec![(P, int(1))]); // 1
    first.add_edge(EId(50), VId(1), VId(5), vec![(P, int(10))]); // 2
    let mut second = WriteBatch::new(RelationId(2));
    second.set_vertex_property(VId(5), P, Some(int(2))); // 3
    second.create_vertex(VId(6), vec![], vec![]); // 4, not a leading prefix
    second.add_edge(EId(60), VId(5), VId(6), vec![]); // 5
    let mut last = WriteBatch::new(RelationId(1));
    last.compare_and_set_vertex_property(
        VId(5),
        P,
        Some(int(2)),
        int(3),
        WriteMismatchPolicy::AbortWrite,
    ); // 6
    // An identity-addressed mutation must follow edge 50 to relation 9.
    last.compare_and_set_edge_property(
        EId(50),
        P,
        Some(int(10)),
        int(11),
        WriteMismatchPolicy::AbortWrite,
    ); // 7
    last.add_edge(EId(70), VId(6), VId(2), vec![]); // 8
    vec![first, second, last]
}

#[test]
fn dependent_relations_keep_source_order_one_publication_and_global_birth_ordinals() {
    let ((), report) = run_async_under_lab(0x6f72_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        // This really closes a previously refused dependency, rather than
        // merely adding another spelling for independent relation groups.
        assert!(db.prepare_atomic_writes(dependent_program()).is_err());
        let prepared = db.prepare_ordered_writes(dependent_program()).unwrap();
        assert_eq!(prepared.basis(), basis);
        let births: BTreeMap<_, _> = prepared
            .template
            .coordinate_entries()
            .iter()
            .flat_map(|coordinate| &coordinate.rows)
            .filter_map(|row| match row {
                DeltaRow::CreateVertex {
                    vid, birth_ordinal, ..
                } => Some((ElementId::Vertex(*vid), *birth_ordinal)),
                DeltaRow::CreateEdge {
                    eid, birth_ordinal, ..
                } => Some((ElementId::Edge(*eid), *birth_ordinal)),
                _ => None,
            })
            .collect();
        assert_eq!(
            births,
            BTreeMap::from([
                (ElementId::Vertex(VId(5)), 1),
                (ElementId::Vertex(VId(6)), 4),
                (ElementId::Edge(EId(50)), 2),
                (ElementId::Edge(EId(60)), 5),
                (ElementId::Edge(EId(70)), 8),
            ])
        );
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertex(VId(5)).unwrap().is_none());
        let seq = db.commit_prepared(&cx, prepared).await.unwrap();
        assert_eq!(seq, CommitSeq(basis.0 + 1));
        assert_eq!(db.delta_since(basis).unwrap().count(), 1);
        assert_eq!(db.vertex(VId(5)).unwrap().unwrap().props, vec![(P, int(3))]);
        assert_eq!(db.edge(EId(50)).unwrap().unwrap().props, vec![(P, int(11))]);
        for (eid, relation) in [(50, 9), (60, 2), (70, 1)] {
            let edge = db.edge(EId(eid)).unwrap().unwrap();
            assert_eq!(edge.entry.relation, RelationId(relation));
            assert_eq!(edge.entry.created_at, seq);
        }
        assert!(pinned.vertex(VId(5)).unwrap().is_none());
        assert!(db.vertex_at(VId(5), basis).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ensure_is_relation_local_and_noop_visits_still_count() {
    let ((), report) = run_async_under_lab(0x6f72_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut first = WriteBatch::new(RelationId(9));
        first.ensure_vertex(VId(1), vec![], vec![]); // raw visit 1, no effect
        first.ensure_edge_by_triple(EId(90), VId(1), VId(2), vec![]); // 2
        let mut second = WriteBatch::new(RelationId(2));
        second.ensure_edge_by_triple(EId(91), VId(1), VId(2), vec![]); // 3
        second.create_vertex(VId(5), vec![], vec![]); // 4
        let mut last = WriteBatch::new(RelationId(9));
        last.ensure_edge_by_triple(EId(92), VId(1), VId(2), vec![]); // 5, no effect
        last.add_edge(EId(93), VId(5), VId(2), vec![]); // 6
        db.write_ordered(&cx, vec![first, second, last])
            .await
            .unwrap();
        assert_eq!(db.vertex(VId(5)).unwrap().unwrap().birth_ordinal, 4);
        assert_eq!(
            db.edge(EId(90)).unwrap().unwrap().entry.relation,
            RelationId(9)
        );
        assert_eq!(
            db.edge(EId(91)).unwrap().unwrap().entry.relation,
            RelationId(2)
        );
        assert!(db.edge(EId(92)).unwrap().is_none());
        assert!(db.edge(EId(93)).unwrap().is_some());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cross_relation_cascades_absorb_updates_explicit_deletes_and_transient_creations_once() {
    let ((), report) = run_async_under_lab(0x6f72_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for delete_both in [false, true] {
            let mut db = seeded(&cx).await;
            let basis = db.frontier().unwrap();
            let mut first = WriteBatch::new(RelationId(9));
            first.create_vertex(VId(5), vec![], vec![]);
            first.add_edge(EId(30), VId(5), VId(2), vec![]);
            first.set_edge_property(EId(20), P, Some(int(21)));
            let mut second = WriteBatch::new(RelationId(1));
            second.delete_edge(EId(10));
            second.set_vertex_property(VId(2), P, Some(int(7)));
            second.add_edge(EId(40), VId(4), VId(5), vec![]);
            let mut last = WriteBatch::new(RelationId(2));
            last.delete_vertex(VId(2));
            if delete_both {
                last.delete_vertex(VId(1));
            }
            last.delete_edge_if_present(EId(30));
            let prepared = db
                .prepare_ordered_writes(vec![first, second, last])
                .unwrap();
            let mut cascaded = Vec::new();
            for coordinate in prepared.template.coordinate_entries() {
                for row in &coordinate.rows {
                    if let DeltaRow::DeleteVertex {
                        sorted_retired_incident_edges,
                        ..
                    } = row
                    {
                        cascaded.extend_from_slice(sorted_retired_incident_edges);
                    }
                    assert!(!matches!(row, DeltaRow::CreateEdge { eid: EId(30), .. }));
                    assert!(!matches!(
                        row,
                        DeltaRow::Property {
                            elem: ElementId::Edge(EId(20)),
                            ..
                        }
                    ));
                }
            }
            cascaded.sort();
            assert_eq!(cascaded, vec![EId(10), EId(20)]);
            db.commit_prepared(&cx, prepared).await.unwrap();
            assert!(db.vertex(VId(2)).unwrap().is_none());
            assert_eq!(db.vertex(VId(1)).unwrap().is_none(), delete_both);
            assert_eq!(
                db.edges()
                    .unwrap()
                    .iter()
                    .map(|edge| edge.entry.eid)
                    .collect::<Vec<_>>(),
                vec![EId(40)]
            );
            assert_eq!(db.edges_at(basis).unwrap().len(), 2);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_dependencies_are_not_hoisted_and_any_refusal_leaves_the_database_unchanged() {
    let ((), report) = run_async_under_lab(0x6f72_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let basis = db.frontier().unwrap();
        let original = db.vertices().unwrap();
        let original_edges = db.edges().unwrap();
        let mut early = WriteBatch::new(RelationId(9));
        early.add_edge(EId(99), VId(1), VId(5), vec![]);
        let mut late = WriteBatch::new(RelationId(1));
        late.create_vertex(VId(5), vec![], vec![]);
        assert!(matches!(
            db.prepare_ordered_writes(vec![early, late]),
            Err(WriteTxnError::Write(WriteError::DanglingEndpoint { .. }))
        ));
        let mut program = dependent_program();
        program.last_mut().unwrap().compare_and_set_vertex_property(
            VId(5),
            P,
            Some(int(999)),
            int(4),
            WriteMismatchPolicy::AbortWrite,
        );
        assert!(matches!(
            db.write_ordered(&cx, program).await,
            Err(WriteTxnError::Write(WriteError::CompareAndSetMismatch(_)))
        ));
        let mut first = WriteBatch::new(RelationId(9));
        first.add_edge(EId(99), VId(1), VId(2), vec![]);
        first.delete_edge(EId(99));
        let mut second = WriteBatch::new(RelationId(1));
        second.add_edge(EId(99), VId(3), VId(4), vec![]);
        assert!(matches!(
            db.prepare_ordered_writes(vec![first, second]),
            Err(WriteTxnError::AtomicRelationConflict {
                element: ElementId::Edge(EId(99)),
                ..
            })
        ));
        assert!(matches!(
            db.prepare_ordered_writes(vec![]),
            Err(WriteTxnError::Write(WriteError::EmptyBatch))
        ));
        let mut program = dependent_program();
        program.push(WriteBatch::new(RelationId(2)));
        assert!(matches!(
            db.prepare_ordered_writes(program),
            Err(WriteTxnError::Write(WriteError::EmptyBatch))
        ));
        assert_eq!(db.frontier().unwrap(), basis);
        assert_eq!(db.vertices().unwrap(), original);
        assert_eq!(db.edges().unwrap(), original_edges);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn normalized_noop_guards_and_ensure_aliases_remain_external_conflict_dependencies() {
    let ((), report) = run_async_under_lab(0x6f72_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for mode in 0..3 {
            let mut db = seeded(&cx).await;
            let mut first = WriteBatch::new(RelationId(1));
            first.ensure_edge_by_triple(EId(99), VId(1), VId(2), vec![]);
            first.compare_and_set_vertex_property(
                VId(1),
                P,
                Some(int(0)),
                int(7),
                WriteMismatchPolicy::AbortWrite,
            );
            let mut second = WriteBatch::new(RelationId(2));
            second.set_vertex_property(VId(1), P, Some(int(0))); // net no-op
            second.create_vertex(VId(5), vec![], vec![]);
            let prepared = db.prepare_ordered_writes(vec![first, second]).unwrap();
            let mut winner = WriteBatch::new(RelationId(9));
            match mode {
                0 => {
                    winner.set_vertex_property(VId(1), P, Some(int(1)));
                }
                1 => {
                    winner.delete_edge(EId(10));
                }
                _ => {
                    winner.set_vertex_property(VId(4), P, Some(int(4)));
                }
            }
            db.write(&cx, winner).await.unwrap();
            let advanced = db.frontier().unwrap();
            let result = db.commit_prepared(&cx, prepared).await;
            if mode < 2 {
                assert!(matches!(result, Err(WriteError::FirstCommitterWins { .. })));
                assert_eq!(db.frontier().unwrap(), advanced);
                assert!(db.vertex(VId(5)).unwrap().is_none());
            } else {
                assert_eq!(result.unwrap(), CommitSeq(advanced.0 + 1));
                assert!(db.vertex(VId(5)).unwrap().is_some());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn single_relation_and_batch_splitting_preserve_exact_canonical_bytes() {
    let ((), report) = run_async_under_lab(0x6f72_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let program = dependent_program();
        let mut combined = WriteBatch::new(RelationId(9));
        let one_relation: Vec<_> = program
            .iter()
            .map(|batch| WriteBatch {
                relation: RelationId(9),
                rows: batch.rows.clone(),
            })
            .collect();
        for batch in &one_relation {
            combined.extend(batch.clone()).unwrap();
        }
        let expected = db.prepare_write(combined).unwrap();
        let actual = db.prepare_ordered_writes(one_relation).unwrap();
        assert_eq!(
            expected.template.canonical_bytes().unwrap(),
            actual.template.canonical_bytes().unwrap()
        );
        let split: Vec<_> = program
            .iter()
            .flat_map(|batch| {
                batch.rows.iter().map(move |row| WriteBatch {
                    relation: batch.relation,
                    rows: vec![row.clone()],
                })
            })
            .collect();
        let whole = db.prepare_ordered_writes(program).unwrap();
        let split = db.prepare_ordered_writes(split).unwrap();
        assert_eq!(
            whole.template.canonical_bytes().unwrap(),
            split.template.canonical_bytes().unwrap()
        );
        assert_eq!(whole.dependencies.elements, split.dependencies.elements);
        assert_eq!(whole.dependencies.adjacency, split.dependencies.adjacency);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn only_final_cross_relation_vertex_content_faces_storage_admission() {
    let ((), report) = run_async_under_lab(0x6f72_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut first = WriteBatch::new(RelationId(9));
        first.set_vertex_property(
            VId(1),
            P,
            Some(CanonicalScalar::bytes(vec![1; 8000]).unwrap()),
        );
        first.set_vertex_property(
            VId(1),
            PropertyKeyId(2),
            Some(CanonicalScalar::bytes(vec![2; 9000]).unwrap()),
        );
        let mut last = WriteBatch::new(RelationId(1));
        last.set_vertex_property(VId(1), P, Some(int(3)));
        last.set_vertex_property(VId(1), PropertyKeyId(2), None);
        db.write_ordered(&cx, vec![first.clone(), last])
            .await
            .unwrap();
        assert_eq!(db.vertex(VId(1)).unwrap().unwrap().props, vec![(P, int(3))]);
        let before = db.frontier().unwrap();
        let mut other = WriteBatch::new(RelationId(1));
        other.create_vertex(VId(5), vec![], vec![]);
        assert!(matches!(
            db.write_ordered(&cx, vec![first, other]).await,
            Err(WriteTxnError::Write(
                WriteError::VertexStorageAdmission { .. }
            ))
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertex(VId(5)).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordered_programs_recover_all_or_none_at_the_existing_marker_boundary() {
    let ((), report) = run_async_under_lab(0x6f72_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for (point, committed) in [
            (Some(CrashPoint::BeforeCapsule), false),
            (Some(CrashPoint::AfterD1), false),
            (Some(CrashPoint::AfterMarkerBeforeD2), true),
            (None, true),
        ] {
            let mut db = seeded(&cx).await;
            let basis = db.frontier().unwrap();
            let view = db.read_session().unwrap();
            let vfs = db.vfs.clone();
            let path = db.path().to_path_buf();
            let prepared = db.prepare_ordered_writes(dependent_program()).unwrap();
            let result = db.commit_prepared_with_crash(&cx, prepared, point).await;
            assert_eq!(result.is_ok(), point.is_none());
            if matches!(point, Some(CrashPoint::AfterMarkerBeforeD2)) {
                assert!(matches!(
                    result,
                    Err(WriteError::CommitOutcomeUnknown { .. })
                ));
                assert!(db.frontier().is_err());
            }
            drop(db);
            // MemVfs retains the written marker in the ambiguous case. This
            // is a surviving-bytes recovery test, not a power-loss simulation.
            let mut reopened = Database::open_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .unwrap();
            assert_eq!(
                reopened.frontier().unwrap(),
                CommitSeq(basis.0 + u64::from(committed))
            );
            for vid in [5, 6] {
                assert_eq!(reopened.vertex(VId(vid)).unwrap().is_some(), committed);
            }
            for eid in [50, 60, 70] {
                assert_eq!(reopened.edge(EId(eid)).unwrap().is_some(), committed);
            }
            assert_eq!(
                reopened.delta_since(basis).unwrap().count(),
                usize::from(committed)
            );
            assert!(view.vertex(VId(5)).unwrap().is_none());
            if committed {
                let vertices = reopened.vertices().unwrap();
                let edges = reopened.edges().unwrap();
                let versions = reopened.element_versions().unwrap().clone();
                reopened.compact(&cx).await.unwrap();
                assert_eq!(reopened.vertices().unwrap(), vertices);
                assert_eq!(reopened.edges().unwrap(), edges);
                drop(reopened);
                let reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
                    .await
                    .unwrap();
                assert_eq!(reopened.vertices().unwrap(), vertices);
                assert_eq!(reopened.edges().unwrap(), edges);
                assert_eq!(reopened.element_versions().unwrap(), &versions);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
