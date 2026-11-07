use super::*;
use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0x6c; 32],
        DatabaseSecurityNamespaceId([0x6d; 32]),
        [0x6e; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut seed = WriteBatch::new(R);
    for id in 1..=4 {
        seed.create_vertex(VId(id), vec![LabelId(1)], vec![(P, int(0))]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![(P, int(10))]);
    db.write(cx, seed).await.unwrap();
    db
}

fn combined(staged: &[WriteBatch]) -> WriteBatch {
    let mut result = WriteBatch::new(R);
    for batch in staged {
        result.extend(batch.clone()).unwrap();
    }
    result
}

fn program() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.ensure_vertex(VId(1), vec![], vec![]); // No-op consumes birth visit 1.
    batch.create_vertex(VId(5), vec![], vec![(P, int(1))]);
    batch.create_vertex(VId(6), vec![], vec![(P, int(6))]);
    batch.add_edge(EId(50), VId(5), VId(6), vec![(P, int(50))]);
    batch.set_vertex_property(VId(1), P, Some(int(7)));
    batch.set_edge_property(EId(10), P, Some(int(11)));
    batch.set_vertex_label(VId(1), LabelId(2), true);
    batch.create_vertex(VId(100), vec![], vec![(P, int(100))]);
    batch.set_vertex_property(VId(5), P, Some(int(2)));
    batch.compare_and_set_edge_property(
        EId(50),
        P,
        Some(int(50)),
        int(51),
        WriteMismatchPolicy::AbortWrite,
    );
    batch.set_edge_property(EId(10), P, None);
    // Both effects on this existing vertex disappear from the net.
    batch.set_vertex_property(VId(1), P, Some(int(0)));
    batch.set_vertex_label(VId(1), LabelId(2), false);
    batch.ensure_vertex(VId(6), vec![], vec![(P, int(-1))]);
    batch.create_vertex(VId(7), vec![], vec![(P, int(7))]);
    batch.add_edge(EId(70), VId(6), VId(7), vec![]);
    batch.compare_and_set_vertex_property(
        VId(2),
        P,
        Some(int(99)),
        int(9),
        WriteMismatchPolicy::NoOp,
    );
    batch.compare_and_set_vertex_property(
        VId(5),
        P,
        Some(int(2)),
        int(3),
        WriteMismatchPolicy::AbortWrite,
    );
    batch.compare_and_set_edge_property(
        EId(50),
        P,
        Some(int(51)),
        int(52),
        WriteMismatchPolicy::AbortWrite,
    );
    batch.set_vertex_property(VId(6), P, Some(CanonicalScalar::Null));
    batch.compare_and_set_vertex_property(VId(6), P, None, int(6), WriteMismatchPolicy::AbortWrite);
    batch.set_edge_property(EId(70), Q, Some(int(5)));
    batch.set_vertex_label(VId(7), LabelId(3), true);
    batch.ensure_vertex(VId(8), vec![], vec![]);
    batch.add_edge(EId(80), VId(8), VId(5), vec![]);
    batch.set_vertex_property(VId(1), Q, Some(int(9)));
    batch.set_vertex_property(VId(1), Q, None);
    batch.compare_and_set_edge_property(EId(10), P, None, int(12), WriteMismatchPolicy::AbortWrite);
    batch.set_vertex_property(VId(5), P, None);
    batch.compare_and_set_vertex_property(VId(5), P, None, int(4), WriteMismatchPolicy::AbortWrite);
    batch
}

#[test]
fn every_incremental_cut_matches_full_native_preparation() {
    let ((), report) = run_async_under_lab(0x6972_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let basis = db.frontier().unwrap();
        let raw = program();
        for width in [1, 2, 3, 7, 13] {
            let mut staged = Vec::new();
            let mut previous: Option<PreparedWrite> = None;
            for rows in raw.rows.chunks(width) {
                let batch = WriteBatch {
                    relation: R,
                    rows: rows.to_vec(),
                };
                let prepared = match previous.as_ref() {
                    Some(previous) => {
                        let cached = db
                            .try_extend_prepared_write(previous, &staged, &batch)
                            .unwrap()
                            .expect("the mixed append/SET/CAS prefix must take the incremental lane");
                        // Withhold the optimization's raw-input proof and run
                        // the original raw scan against the same exact prefix.
                        let mut uncached = previous.clone();
                        uncached.raw_input = None;
                        let rescanned = db
                            .try_extend_prepared_write(&uncached, &staged, &batch)
                            .unwrap()
                            .unwrap();
                        assert_eq!(cached.template, rescanned.template);
                        assert_eq!(cached.raw_input, rescanned.raw_input);
                        cached
                    }
                    None => db.prepare_write(batch.clone()).unwrap(),
                };
                staged.push(batch);
                let full = db.prepare_write(combined(&staged)).unwrap();
                assert_eq!(
                    prepared.template.canonical_bytes(),
                    full.template.canonical_bytes(),
                    "width {width}, prefix {}",
                    staged.len(),
                );
                assert_eq!(prepared.dependencies.elements, full.dependencies.elements);
                assert_eq!(prepared.dependencies.adjacency, full.dependencies.adjacency);
                assert_eq!(prepared.replay_row_allowance, full.replay_row_allowance);
                assert_eq!(prepared.raw_input, full.raw_input);
                assert_eq!(
                    prepared.raw_input.unwrap().visits,
                    staged.iter().map(|batch| batch.rows.len() as u64).sum::<u64>(),
                    "no-op visits accumulate independently of the smaller seed"
                );
                assert_eq!(prepared.basis(), basis);
                previous = Some(prepared);
            }
        }
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertex(VId(5)).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pure_create_chunks_keep_the_native_creation_lane_and_raw_birth_visits() {
    let ((), report) = run_async_under_lab(0x6972_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut staged = Vec::new();
        let mut previous = None;
        for id in 20..36 {
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(id), vec![], vec![(P, int(id as i64))]);
            batch.add_edge(EId(id), VId(id), VId(1), vec![]);
            let prepared = match previous.as_ref() {
                Some(previous) => db
                    .try_extend_prepared_write(previous, &staged, &batch)
                    .unwrap()
                    .unwrap(),
                None => db.prepare_write(batch.clone()).unwrap(),
            };
            staged.push(batch);
            let full = db.prepare_write(combined(&staged)).unwrap();
            assert_eq!(
                prepared.template.canonical_bytes(),
                full.template.canonical_bytes()
            );
            previous = Some(prepared);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn incremental_collection_prefixes_keep_bytes_births_and_format_downgrades() {
    let ((), report) = run_async_under_lab(0x6972_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let collection = fgdb_types::CanonicalPropertyValue::List(
            fgdb_types::CanonicalList::try_new(vec![
                int(5).into(),
                CanonicalScalar::Date(fgdb_types::CanonicalDate::from_civil(2026, 11, 6).unwrap())
                    .into(),
            ])
            .unwrap(),
        );
        let mut prefix = WriteBatch::new(R);
        prefix.ensure_vertex(VId(1), vec![], vec![]);
        prefix.create_vertex_values(
            VId(5),
            vec![LabelId(2), LabelId(1)],
            vec![(Q, collection), (P, int(5).into())],
        );
        prefix.create_vertex(VId(6), vec![], vec![(P, int(6))]);
        prefix.add_edge(EId(50), VId(5), VId(6), vec![(P, int(50))]);
        let mut staged = vec![prefix.clone()];
        let mut previous = db.prepare_write(prefix).unwrap();
        assert_eq!(
            previous.template.format(),
            fgdb_delta_types::DELTA_FORMAT_V3
        );
        let frozen = previous.template.clone();
        let frozen_bytes = frozen.canonical_bytes().unwrap();
        let mut unrelated = WriteBatch::new(R);
        unrelated.create_vertex(VId(7), vec![], vec![(P, int(7))]);
        let mut remove_collection = WriteBatch::new(R);
        remove_collection.set_vertex_property_value(VId(5), Q, None);
        for (suffix, format) in [
            (unrelated, fgdb_delta_types::DELTA_FORMAT_V3),
            (remove_collection, fgdb_delta_types::DELTA_FORMAT_V1),
        ] {
            let before = previous.template.canonical_bytes().unwrap();
            let next = db
                .try_extend_prepared_write(&previous, &staged, &suffix)
                .unwrap()
                .expect("the native collection prefix supports incremental extension");
            staged.push(suffix);
            let full = db.prepare_write(combined(&staged)).unwrap();
            assert_eq!(
                next.template.canonical_bytes(),
                full.template.canonical_bytes()
            );
            assert_eq!(next.template.format(), format);
            assert_eq!(next.dependencies.elements, full.dependencies.elements);
            assert_eq!(next.dependencies.adjacency, full.dependencies.adjacency);
            assert_eq!(next.replay_row_allowance, full.replay_row_allowance);
            assert_eq!(previous.template.canonical_bytes().unwrap(), before);
            assert_eq!(frozen.canonical_bytes().unwrap(), frozen_bytes);
            previous = next;
        }
        let births: Vec<_> = previous
            .template
            .rows()
            .filter_map(|row| match row {
                DeltaRow::CreateVertex { birth_ordinal, .. }
                | DeltaRow::CreateEdge { birth_ordinal, .. } => Some(*birth_ordinal),
                _ => None,
            })
            .collect();
        assert_eq!(births, vec![2, 3, 5, 4]);
        assert!(db.vertex(VId(5)).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refused_suffix_preserves_identity_and_cas_errors() {
    let ((), report) = run_async_under_lab(0x6972_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(5), vec![], vec![(P, int(1))]);
        prefix.create_vertex(VId(6), vec![], vec![]);
        prefix.add_edge(EId(50), VId(5), VId(6), vec![(P, int(5))]);
        let previous = db.prepare_write(prefix.clone()).unwrap();
        for case in 0..4 {
            let mut suffix = WriteBatch::new(R);
            match case {
                0 => {
                    suffix.create_vertex(VId(5), vec![], vec![]);
                }
                1 => {
                    suffix.add_edge(EId(50), VId(1), VId(2), vec![]);
                }
                2 => {
                    suffix.add_edge(EId(60), VId(5), VId(999), vec![]);
                }
                _ => {
                    suffix.compare_and_set_edge_property(
                        EId(50),
                        P,
                        Some(int(99)),
                        int(6),
                        WriteMismatchPolicy::AbortWrite,
                    );
                }
            }
            let incremental = db
                .try_extend_prepared_write(&previous, &[prefix.clone()], &suffix)
                .unwrap_err();
            let expected = db
                .prepare_write(combined(&[prefix.clone(), suffix]))
                .unwrap_err();
            assert_eq!(
                format!("{incremental:?}"),
                format!("{:?}", WriteTxnError::Write(expected))
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn raw_retirements_and_edge_ensure_always_choose_full_preparation() {
    let ((), report) = run_async_under_lab(0x6972_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        for case in 0..3 {
            let mut prefix = WriteBatch::new(R);
            match case {
                0 => {
                    prefix.create_vertex(VId(5), vec![], vec![]);
                    prefix.delete_vertex(VId(5));
                }
                1 => {
                    prefix.add_edge(EId(50), VId(1), VId(2), vec![]);
                    prefix.delete_edge(EId(50));
                }
                _ => {
                    prefix.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
                }
            }
            let previous = db.prepare_write(prefix.clone()).unwrap();
            assert!(previous.template.rows().next().is_none());
            let mut suffix = WriteBatch::new(R);
            suffix.create_vertex(VId(5), vec![], vec![]);
            assert!(
                db.try_extend_prepared_write(&previous, &[prefix], &suffix)
                    .unwrap()
                    .is_none()
            );
        }
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(5), vec![], vec![]);
        let previous = db.prepare_write(prefix.clone()).unwrap();
        for edge_ensure in [false, true] {
            let mut suffix = WriteBatch::new(R);
            if edge_ensure {
                suffix.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
            } else {
                suffix.delete_vertex(VId(5));
            }
            assert!(
                db.try_extend_prepared_write(&previous, &[prefix.clone()], &suffix)
                    .unwrap()
                    .is_none()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authority_and_withheld_replay_are_preserved_with_noop_dependencies() {
    let ((), report) = run_async_under_lab(0x6972_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut prefix = WriteBatch::new(R);
        prefix.compare_and_set_vertex_property(
            VId(1),
            P,
            Some(int(99)),
            int(5),
            WriteMismatchPolicy::NoOp,
        );
        prefix.ensure_vertex(VId(2), vec![], vec![]);
        prefix.create_vertex(VId(5), vec![], vec![]);
        let mut previous = db.prepare_write(prefix.clone()).unwrap();
        let authority = fgdb_crypto::Digest([0xa1; 32]);
        previous.authorization_decision_digest = Some(authority);
        previous.replay_row_allowance = None;
        let mut suffix = WriteBatch::new(R);
        suffix.create_vertex(VId(6), vec![], vec![]);
        let extended = db
            .try_extend_prepared_write(&previous, &[prefix], &suffix)
            .unwrap()
            .unwrap();
        assert_eq!(extended.authorization_decision_digest, Some(authority));
        assert_eq!(extended.replay_row_allowance, None);
        assert!(
            extended
                .dependencies
                .elements
                .contains(&ElementId::Vertex(VId(1)))
        );
        assert!(
            extended
                .dependencies
                .elements
                .contains(&ElementId::Vertex(VId(2)))
        );
        assert!(extended.template.rows().any(|row| matches!(
            row,
            DeltaRow::CreateVertex {
                vid: VId(6),
                birth_ordinal: 4,
                ..
            }
        )));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn savepoint_rollback_discards_raw_retirements_and_restores_original_birth_visits() {
    let ((), report) = run_async_under_lab(0x6972_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seeded(&cx).await;
        let basis = db.frontier().unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.ensure_vertex(VId(1), vec![], vec![]);
        prefix.create_vertex(VId(5), vec![LabelId(2)], vec![(P, int(5))]);
        prefix.create_vertex(VId(6), vec![], vec![(P, int(6))]);
        prefix.add_edge(EId(50), VId(5), VId(6), vec![(P, int(50))]);
        prefix.set_vertex_property(VId(1), Q, Some(int(9)));
        prefix.set_vertex_property(VId(1), Q, None);
        let mut txn = db.begin(&txcx).unwrap();
        txn.write(&mut db, prefix.clone()).unwrap();
        txn.savepoint(&db, "supported").unwrap();
        let saved = txn.staged_effect_digest().unwrap();

        let mut retired = WriteBatch::new(R);
        retired.create_vertex(VId(70), vec![], vec![]);
        retired.delete_vertex(VId(70));
        txn.write(&mut db, retired).unwrap();
        assert_eq!(txn.staged_effect_digest().unwrap(), saved);
        let mut reuse = WriteBatch::new(R);
        reuse.create_vertex(VId(70), vec![], vec![]);
        assert!(matches!(
            txn.write(&mut db, reuse),
            Err(WriteTxnError::Write(WriteError::IdentitySpent {
                elem: ElementId::Vertex(VId(70))
            }))
        ));
        assert_eq!(txn.staged_effect_digest().unwrap(), saved);

        let mut suffix = WriteBatch::new(R);
        suffix.ensure_vertex(VId(2), vec![], vec![]);
        suffix.set_edge_property(EId(50), P, Some(int(51)));
        suffix.set_vertex_label(VId(5), LabelId(3), true);
        suffix.create_vertex(VId(70), vec![], vec![(Q, int(70))]);
        let full_input = combined(&[prefix, suffix.clone()]);
        let full = db.prepare_write(full_input.clone()).unwrap();
        let mut oracle = db.begin(&txcx).unwrap();
        oracle.write(&mut db, full_input).unwrap();

        for _ in 0..2 {
            txn.rollback_to_savepoint(&db, "supported").unwrap();
            assert_eq!(txn.staged_effect_digest().unwrap(), saved);
            txn.write(&mut db, suffix.clone()).unwrap();
            assert_eq!(
                txn.vertex(&db, VId(70)).unwrap().unwrap().birth_ordinal,
                10,
                "discarded visits must not survive rollback, but prefix no-ops must"
            );
            assert_eq!(
                txn.staged_effect_digest().unwrap(),
                oracle.staged_effect_digest().unwrap()
            );
            assert_eq!(txn.vertices(&db).unwrap(), oracle.vertices(&db).unwrap());
            assert_eq!(txn.edges(&db).unwrap(), oracle.edges(&db).unwrap());
        }
        oracle.abort();
        txn.commit(&mut db, &cx).await.unwrap();
        let committed = db.delta_since(basis).unwrap().next().unwrap();
        assert_eq!(
            committed.coordinate_entries(),
            full.template.coordinate_entries()
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn erased_mixed_relation_effects_cannot_establish_a_single_raw_relation() {
    let ((), report) = run_async_under_lab(0x6972_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = seeded(&cx).await;
        let mut first = WriteBatch::new(R);
        first.set_edge_property(EId(10), P, Some(int(11)));
        let mut second = WriteBatch::new(RelationId(9));
        second.set_edge_property(EId(10), P, Some(int(10)));
        let prefix = vec![first, second];
        let prepared = db.prepare_ordered_writes(prefix.clone()).unwrap();
        assert!(prepared.raw_input.is_none());
        assert_eq!(prepared.template.coordinate_entries().len(), 1);
        assert_eq!(prepared.template.rows().count(), 0);

        let mut suffix = WriteBatch::new(R);
        suffix.create_vertex(VId(5), vec![], vec![(P, int(5))]);
        suffix.add_edge(EId(50), VId(5), VId(1), vec![(P, int(50))]);
        assert!(
            db.try_extend_prepared_write(&prepared, &prefix, &suffix)
                .unwrap()
                .is_none(),
            "the empty normalized coordinate cannot erase mixed raw ownership"
        );
        let mut txn = db.begin(&txcx).unwrap();
        txn.write_ordered(&mut db, prefix.clone()).unwrap();
        txn.write(&mut db, suffix.clone()).unwrap();
        assert_eq!(txn.vertex(&db, VId(5)).unwrap().unwrap().birth_ordinal, 3);

        let mut foreign = WriteBatch::new(RelationId(7));
        foreign.set_edge_property(EId(50), P, Some(int(51)));
        txn.write(&mut db, foreign.clone()).unwrap();
        let mut complete = prefix;
        complete.extend([suffix, foreign]);
        let mut oracle = db.begin(&txcx).unwrap();
        oracle.write_ordered(&mut db, complete).unwrap();
        assert_eq!(
            txn.staged_effect_digest().unwrap(),
            oracle.staged_effect_digest().unwrap()
        );
        assert_eq!(txn.vertices(&db).unwrap(), oracle.vertices(&db).unwrap());
        assert_eq!(txn.edges(&db).unwrap(), oracle.edges(&db).unwrap());
        assert_eq!(txn.edge(&db, EId(50)).unwrap().unwrap().entry.relation, R);
        txn.abort();
        oracle.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_seed_ranges_match_a_full_scan_with_bounded_polls_and_refusals() {
    fn canonical(rows: Vec<DeltaRow>) -> crate::LogicalDeltaTemplate {
        crate::LogicalDeltaTemplate::build(
            crate::intent_semantics_oid(),
            [0; 32],
            vec![crate::CoordinateEntry {
                graph: crate::GRAPH,
                branch: crate::BRANCH,
                relation: R,
                schema_epoch: crate::SchemaEpoch(0),
                schema_transition: None,
                rows,
            }],
        )
        .unwrap()
    }

    let existing_vertex = VId(1_u128 << 127);
    let existing_edge = EId((1_u128 << 96) + 1);
    let requested = BTreeSet::from([
        ElementId::Vertex(existing_vertex),
        ElementId::Edge(existing_edge),
        ElementId::Edge(EId(u128::MAX)),
    ]);
    for unrelated in [32_usize, 512, 8_192] {
        let mut rows = Vec::new();
        for (vid, birth_ordinal) in [(VId(0), 1), (VId(u128::MAX), 2)] {
            rows.push(DeltaRow::CreateVertex {
                vid,
                birth_ordinal,
                labels: vec![],
                props: vec![],
                valid_time: None,
            });
        }
        rows.push(DeltaRow::CreateEdge {
            eid: EId(u128::MAX),
            birth_ordinal: 3,
            src: VId(0),
            relation: R,
            dst: VId(u128::MAX),
            canonical_key: None,
            props: vec![],
            valid_time: None,
        });
        for field in [0, u64::MAX] {
            rows.push(DeltaRow::LabelMembership {
                vid: existing_vertex,
                label: LabelId(field),
                before: false,
                after: true,
            });
            for elem in [
                ElementId::Vertex(existing_vertex),
                ElementId::Edge(existing_edge),
            ] {
                rows.push(DeltaRow::Property {
                    elem,
                    property: PropertyKeyId(field),
                    before: Some(int(1).into()),
                    after: Some(int(2).into()),
                });
            }
        }
        for id in 1..=unrelated {
            rows.push(DeltaRow::CreateVertex {
                vid: VId(id as u128),
                birth_ordinal: id as u64 + 3,
                labels: vec![],
                props: vec![],
                valid_time: None,
            });
            rows.push(DeltaRow::Property {
                elem: ElementId::Vertex(VId((1_u128 << 64) + id as u128)),
                property: P,
                before: Some(int(0).into()),
                after: Some(int(1).into()),
            });
        }
        // The durable encoder supplies the order; neither the fixture nor
        // the independent full-scan oracle uses the new binary selector.
        rows.reverse();
        let template = canonical(rows);
        let rows = &template.coordinate_entries()[0].rows;
        let expected_needed: BTreeSet<_> = requested
            .iter()
            .copied()
            .chain([
                ElementId::Vertex(VId(0)),
                ElementId::Vertex(VId(u128::MAX)),
            ])
            .collect();
        let expected: Vec<_> = rows
            .iter()
            .filter(|row| {
                let element = match row {
                    DeltaRow::CreateVertex { vid, .. }
                    | DeltaRow::LabelMembership { vid, .. } => ElementId::Vertex(*vid),
                    DeltaRow::CreateEdge { eid, .. } => ElementId::Edge(*eid),
                    DeltaRow::Property { elem, .. } => *elem,
                    _ => panic!("the positive fixture contains only supported families"),
                };
                expected_needed.contains(&element)
            })
            .collect();
        assert_eq!(expected.len(), 9);
        let mut needed = requested.clone();
        let mut polls = 0_usize;
        let selected = prefix_seed_rows(rows, &mut needed, &mut || {
            polls += 1;
            Ok::<(), usize>(())
        })
        .unwrap()
        .unwrap();
        assert_eq!(selected, expected);
        assert_eq!(needed, expected_needed);
        // Fixed requested identities and fixed output: each search comparison
        // polls, so a linear scan of the growing unrelated prefix fails this
        // logarithmic work bound without relying on wall-clock measurements.
        let depth = rows.len().ilog2() as usize + 1;
        assert!(
            polls <= 40 * depth + 40,
            "{} rows used {polls} polls",
            rows.len()
        );

        for stop in [1, 2, 5, polls / 2, polls] {
            let mut observed = 0_usize;
            let result = prefix_seed_rows(rows, &mut requested.clone(), &mut || {
                observed += 1;
                if observed == stop {
                    Err(stop)
                } else {
                    Ok(())
                }
            });
            assert_eq!(result, Err(stop));
            assert_eq!(observed, stop, "no polling continues after a refusal");
        }
        for unsupported in [
            DeltaRow::DeleteVertex {
                vid: VId((1_u128 << 120) + 5),
                before_version: fgdb_types::ObjectId([0x31; 32]),
                sorted_retired_incident_edges: vec![],
            },
            DeltaRow::DeleteEdge {
                eid: EId(5),
                before_version: fgdb_types::ObjectId([0x32; 32]),
            },
            DeltaRow::ValidTime {
                elem: ElementId::Vertex(VId(5)),
                contract_id: fgdb_types::ObjectId([0x33; 32]),
                before: None,
                after: Some(fgdb_delta_types::ValidTimePeriod {
                    start_micros: 10,
                    end_micros: Some(20),
                }),
            },
        ] {
            let mut refused = rows.to_vec();
            refused.push(unsupported);
            let refused = canonical(refused);
            assert!(
                prefix_seed_rows(
                    &refused.coordinate_entries()[0].rows,
                    &mut requested.clone(),
                    &mut || Ok::<(), usize>(()),
                )
                .unwrap()
                .is_none(),
                "an unrelated unsupported family must refuse before seeding"
            );
        }
    }
}
