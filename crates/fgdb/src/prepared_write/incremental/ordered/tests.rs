use super::*;
use crate::{DatabaseKeys, MemVfs, WriteMismatchPolicy};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{
    CanonicalScalar, CanonicalTimestamp, CommitCx, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, VId,
};

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0x6f; 32],
        DatabaseSecurityNamespaceId([0x70; 32]),
        [0x71; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut seed = WriteBatch::new(RelationId(7));
    for id in 1..=4 {
        seed.create_vertex(VId(id), vec![LabelId(1)], vec![(P, int(0))]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![(P, int(10))]);
    db.write(cx, seed).await.unwrap();
    db
}

fn program() -> Vec<WriteBatch> {
    let mut first = WriteBatch::new(RelationId(9));
    first.ensure_vertex(VId(1), vec![], vec![(P, int(999))]);
    first.create_vertex(VId(5), vec![], vec![(P, int(5))]);
    let mut same_relation = WriteBatch::new(RelationId(9));
    same_relation.add_edge(EId(50), VId(1), VId(5), vec![(P, int(50))]);
    same_relation.set_vertex_property(VId(2), P, Some(int(7)));
    let mut second = WriteBatch::new(RelationId(2));
    second.create_vertex(VId(6), vec![], vec![(P, int(6))]);
    second.add_edge(EId(60), VId(5), VId(6), vec![(P, int(60))]);
    second.set_edge_property(EId(50), P, Some(int(51)));
    second.set_vertex_label(VId(5), LabelId(2), true);
    let mut third = WriteBatch::new(RelationId(4));
    third.set_vertex_property(VId(5), P, Some(int(8)));
    third.set_edge_property(EId(10), P, Some(int(11)));
    third.ensure_vertex(VId(5), vec![], vec![(P, int(999))]);
    third.compare_and_set_edge_property(
        EId(60),
        P,
        Some(int(60)),
        int(61),
        WriteMismatchPolicy::AbortWrite,
    );
    let mut fourth = WriteBatch::new(RelationId(1));
    fourth.set_vertex_property(VId(2), P, Some(int(0)));
    fourth.set_edge_property(EId(10), P, Some(int(10)));
    fourth.create_vertex(VId(100), vec![], vec![(P, int(100))]);
    fourth.create_vertex(VId(7), vec![], vec![]);
    fourth.add_edge(EId(70), VId(6), VId(7), vec![]);
    let mut fifth = WriteBatch::new(RelationId(6));
    fifth.compare_and_set_vertex_property(
        VId(5),
        P,
        Some(int(999)),
        int(9),
        WriteMismatchPolicy::NoOp,
    );
    fifth.compare_and_set_vertex_property(
        VId(5),
        P,
        Some(int(8)),
        int(9),
        WriteMismatchPolicy::AbortWrite,
    );
    fifth.set_vertex_property(VId(6), P, None);
    fifth.compare_and_set_edge_property(
        EId(50),
        P,
        Some(int(51)),
        int(52),
        WriteMismatchPolicy::AbortWrite,
    );
    let mut sixth = WriteBatch::new(RelationId(2));
    sixth.set_vertex_property(VId(5), P, Some(int(5)));
    sixth.ensure_vertex(VId(8), vec![], vec![(P, int(8))]);
    sixth.add_edge(EId(80), VId(8), VId(7), vec![]);
    sixth.set_vertex_property(VId(8), Q, Some(CanonicalScalar::Null));
    let mut last = WriteBatch::new(RelationId(3));
    last.set_edge_property(EId(80), P, Some(int(80)));
    last.set_vertex_label(VId(5), LabelId(2), false);
    last.compare_and_set_vertex_property(
        VId(6),
        P,
        None,
        CanonicalScalar::Null,
        WriteMismatchPolicy::AbortWrite,
    );
    last.compare_and_set_edge_property(
        EId(60),
        P,
        Some(int(61)),
        int(60),
        WriteMismatchPolicy::AbortWrite,
    );
    last.create_vertex(VId(9), vec![], vec![]);
    vec![
        first,
        same_relation,
        second,
        third,
        fourth,
        fifth,
        sixth,
        last,
    ]
}

fn assert_same(actual: &PreparedWrite, expected: &PreparedWrite) {
    assert_eq!(
        actual.template.canonical_bytes(),
        expected.template.canonical_bytes()
    );
    assert_eq!(actual.dependencies.elements, expected.dependencies.elements);
    assert_eq!(
        actual.dependencies.adjacency,
        expected.dependencies.adjacency
    );
    assert_eq!(actual.replay_row_allowance, expected.replay_row_allowance);
    assert_eq!(actual.basis(), expected.basis());
}

#[test]
fn every_ordered_incremental_cut_matches_complete_native_preparation() {
    let ((), report) = run_async_under_lab(0x6972_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let basis = db.frontier().unwrap();
        let raw = program();
        for width in [1, 2, 3, 5] {
            let mut staged = Vec::new();
            let mut previous = None;
            for suffix in raw.chunks(width) {
                let prepared = match previous.as_ref() {
                    Some(previous) => db
                        .try_extend_prepared_ordered_writes(previous, &staged, suffix)
                        .unwrap()
                        .expect("supported relation slices must reuse the admitted prefix"),
                    None => db.prepare_ordered_writes(suffix.to_vec()).unwrap(),
                };
                staged.extend_from_slice(suffix);
                let full = db.prepare_ordered_writes(staged.clone()).unwrap();
                assert_same(&prepared, &full);
                previous = Some(prepared);
            }
        }
        assert_eq!(db.frontier().unwrap(), basis);
        assert!(db.vertex(VId(5)).unwrap().is_none());
        assert_eq!(
            db.edge(EId(10)).unwrap().unwrap().props,
            vec![(P, int(10).into())]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn repeated_same_coordinate_appends_match_complete_preparation_without_changing_prefixes() {
    let ((), report) = run_async_under_lab(0x6972_0107, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = seeded(&contexts.commit()).await;
        let mut staged: Vec<_> = program().into_iter().take(3).collect();
        for (relation, first) in [(RelationId(9), 100), (RelationId(2), 200)] {
            let mut batch = WriteBatch::new(relation);
            for offset in 0..32 {
                batch.add_edge(EId(first + offset), VId(5), VId(6), vec![(P, int(1))]);
            }
            staged.push(batch);
        }
        let mut previous = db.prepare_ordered_writes(staged.clone()).unwrap();
        for step in 0..12 {
            let before = previous.template.canonical_bytes().unwrap();
            let mut low = WriteBatch::new(RelationId(2));
            low.set_vertex_property(VId(2), P, Some(int(if step % 2 == 0 { 0 } else { 7 })));
            low.set_vertex_label(VId(5), LabelId(2), step % 2 != 0);
            low.set_edge_property(EId(60), Q, (step % 2 == 0).then(|| int(step)));
            low.add_edge(EId(1_000 + step as u128), VId(5), VId(6), vec![]);
            let mut high = WriteBatch::new(RelationId(9));
            high.compare_and_set_edge_property(
                EId(50),
                P,
                Some(int(51 + step)),
                int(52 + step),
                WriteMismatchPolicy::AbortWrite,
            );
            // Reverse batch order as well as the coordinate being changed.
            let suffix = if step % 2 == 0 {
                vec![high, low]
            } else {
                vec![low, high]
            };
            let actual = db
                .try_extend_prepared_ordered_writes(&previous, &staged, &suffix)
                .unwrap()
                .unwrap();
            staged.extend(suffix);
            let expected = db.prepare_ordered_writes(staged.clone()).unwrap();
            assert_same(&actual, &expected);
            assert_eq!(previous.template.canonical_bytes().unwrap(), before);
            assert_eq!(
                actual
                    .template
                    .coordinate_entries()
                    .iter()
                    .map(|entry| entry.relation)
                    .collect::<Vec<_>>(),
                vec![RelationId(2), RelationId(9)]
            );
            previous = actual;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn same_coordinate_appends_keep_savepoint_and_refusal_atomicity() {
    let ((), report) = run_async_under_lab(0x6972_0108, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut transaction = db.begin(&contexts.txn()).unwrap();
        let prefix: Vec<_> = program().into_iter().take(3).collect();
        transaction.write_ordered(&mut db, prefix).unwrap();
        let before = transaction.staged_effect_digest().unwrap();
        let birth = transaction
            .vertex(&db, VId(5))
            .unwrap()
            .unwrap()
            .birth_ordinal;
        transaction.savepoint(&db, "mixed").unwrap();
        let mut low = WriteBatch::new(RelationId(2));
        low.create_vertex(VId(7), vec![], vec![(P, int(7))]);
        low.add_edge(EId(70), VId(5), VId(7), vec![]);
        let mut high = WriteBatch::new(RelationId(9));
        high.set_edge_property(EId(50), P, Some(int(99)));
        let suffix = vec![high, low];
        transaction.write_ordered(&mut db, suffix.clone()).unwrap();
        let changed = transaction.staged_effect_digest().unwrap();
        assert_ne!(changed, before);
        assert!(transaction.vertex(&db, VId(7)).unwrap().is_some());
        assert!(db.vertex(VId(7)).unwrap().is_none());
        let mut refused = WriteBatch::new(RelationId(2));
        refused.compare_and_set_edge_property(
            EId(50),
            P,
            Some(int(51)),
            int(100),
            WriteMismatchPolicy::AbortWrite,
        );
        assert!(transaction.write_ordered(&mut db, vec![refused]).is_err());
        assert_eq!(transaction.staged_effect_digest().unwrap(), changed);
        transaction.rollback_to_savepoint(&db, "mixed").unwrap();
        assert_eq!(transaction.staged_effect_digest().unwrap(), before);
        assert_eq!(
            transaction
                .vertex(&db, VId(5))
                .unwrap()
                .unwrap()
                .birth_ordinal,
            birth
        );
        assert!(transaction.vertex(&db, VId(7)).unwrap().is_none());
        transaction.write_ordered(&mut db, suffix).unwrap();
        assert_eq!(transaction.staged_effect_digest().unwrap(), changed);
        transaction.commit(&mut db, &cx).await.unwrap();
        assert!(db.vertex(VId(7)).unwrap().is_some());
        assert_eq!(
            db.edge(EId(50)).unwrap().unwrap().props,
            vec![(P, int(99).into())]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_new_lower_relation_moves_the_vertex_backbone_before_same_coordinate_reuse() {
    let ((), report) = run_async_under_lab(0x6972_0109, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = seeded(&contexts.commit()).await;
        let mut staged: Vec<_> = program().into_iter().take(3).collect();
        let previous = db.prepare_ordered_writes(staged.clone()).unwrap();
        let mut lower = WriteBatch::new(RelationId(1));
        lower.add_edge(EId(70), VId(5), VId(6), vec![(P, int(70))]);
        lower.set_vertex_property(VId(5), P, Some(int(50)));
        let moved = db
            .try_extend_prepared_ordered_writes(&previous, &staged, &[lower.clone()])
            .unwrap()
            .unwrap();
        staged.push(lower);
        assert_same(&moved, &db.prepare_ordered_writes(staged.clone()).unwrap());
        assert_eq!(
            moved.template.coordinate_entries()[0].relation,
            RelationId(1)
        );
        for coordinate in moved.template.coordinate_entries() {
            if coordinate.relation != RelationId(1) {
                assert!(
                    coordinate
                        .rows
                        .iter()
                        .all(|row| !vertex_effect(row).unwrap())
                );
            }
        }
        let mut later = WriteBatch::new(RelationId(9));
        later.compare_and_set_edge_property(
            EId(70),
            P,
            Some(int(70)),
            int(71),
            WriteMismatchPolicy::AbortWrite,
        );
        later.set_vertex_property(VId(5), P, Some(int(5)));
        let actual = db
            .try_extend_prepared_ordered_writes(&moved, &staged, &[later.clone()])
            .unwrap()
            .unwrap();
        staged.push(later);
        assert_same(&actual, &db.prepare_ordered_writes(staged).unwrap());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreign_edge_identity_moves_to_its_actual_coordinate_without_empty_slices() {
    let ((), report) = run_async_under_lab(0x6972_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = seeded(&contexts.commit()).await;
        let mut prefix = WriteBatch::new(RelationId(9));
        prefix.set_edge_property(EId(10), P, Some(int(11)));
        let previous = db.prepare_write(prefix.clone()).unwrap();
        let mut suffix = WriteBatch::new(RelationId(2));
        suffix.compare_and_set_edge_property(
            EId(10),
            P,
            Some(int(11)),
            int(12),
            WriteMismatchPolicy::AbortWrite,
        );
        let actual = db
            .try_extend_prepared_ordered_writes(&previous, &[prefix.clone()], &[suffix.clone()])
            .unwrap()
            .unwrap();
        let expected = db.prepare_ordered_writes(vec![prefix, suffix]).unwrap();
        assert_same(&actual, &expected);
        let [coordinate] = actual.template.coordinate_entries() else {
            panic!("only the edge's owning relation has evaluator input");
        };
        assert_eq!(coordinate.relation, RelationId(7));
        assert_eq!(actual.replay_row_allowance, Some(2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expansion_counts_original_noops_and_never_upgrades_withheld_replay() {
    let ((), report) = run_async_under_lab(0x6972_0103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = seeded(&contexts.commit()).await;
        let mut prefix = WriteBatch::new(RelationId(9));
        prefix.ensure_vertex(VId(1), vec![], vec![]);
        prefix.create_vertex(VId(5), vec![], vec![]);
        let mut previous = db.prepare_write(prefix.clone()).unwrap();
        let mut edge = WriteBatch::new(RelationId(2));
        edge.add_edge(EId(50), VId(1), VId(5), vec![]);
        let mut update = WriteBatch::new(RelationId(3));
        update.set_edge_property(EId(50), P, Some(int(1)));
        let suffix = [edge, update];
        let raw: Vec<_> = [prefix.clone()].into_iter().chain(suffix.clone()).collect();
        let expected = db.prepare_ordered_writes_bounded(raw.clone(), 8).unwrap();
        assert!(matches!(
            db.prepare_ordered_writes_bounded(raw, 7),
            Err(WriteTxnError::OrderedWriteBudgetExceeded {
                limit: 7,
                required: 8
            })
        ));
        let authority = fgdb_crypto::Digest([0xa2; 32]);
        previous.authorization_decision_digest = Some(authority);
        let actual = db
            .try_extend_prepared_ordered_writes(&previous, &[prefix.clone()], &suffix)
            .unwrap()
            .unwrap();
        assert_same(&actual, &expected);
        assert_eq!(actual.authorization_decision_digest, Some(authority));
        assert_eq!(actual.replay_row_allowance, Some(8));
        // Retaining an explicit two-row deficit is different from recomputing
        // an allowance from the template or silently granting full replay.
        previous.replay_row_allowance = Some(0);
        let reduced = db
            .try_extend_prepared_ordered_writes(&previous, &[prefix.clone()], &suffix)
            .unwrap()
            .unwrap();
        assert_eq!(reduced.replay_row_allowance, Some(6));
        previous.replay_row_allowance = None;
        let withheld = db
            .try_extend_prepared_ordered_writes(&previous, &[prefix], &suffix)
            .unwrap()
            .unwrap();
        assert_eq!(withheld.replay_row_allowance, None);
        assert_eq!(withheld.authorization_decision_digest, Some(authority));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn unavailable_value() -> CanonicalScalar {
    CanonicalScalar::Timestamp(
        CanonicalTimestamp::from_stored_zoned(
            0,
            0,
            "Etc/UTC",
            fgdb_types::ids::ObjectId([0x72; 32]),
        )
        .unwrap(),
    )
}

#[test]
fn refused_suffixes_keep_exact_native_errors_and_original_slice_precedence() {
    let ((), report) = run_async_under_lab(0x6972_0104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = seeded(&contexts.commit()).await;
        let prefix: Vec<_> = program().into_iter().take(3).collect();
        let previous = db.prepare_ordered_writes(prefix.clone()).unwrap();
        for case in 0..7 {
            let mut suffix = WriteBatch::new(RelationId(3));
            match case {
                0 => {
                    suffix.create_vertex(VId(5), vec![], vec![]);
                }
                1 => {
                    suffix.relation = RelationId(9);
                    suffix.add_edge(EId(50), VId(1), VId(2), vec![]);
                }
                2 => {
                    // Relation ownership refuses before the bad scalar.
                    suffix.add_edge(EId(50), VId(1), VId(2), vec![(P, unavailable_value())]);
                }
                3 => {
                    suffix.add_edge(EId(90), VId(5), VId(999), vec![]);
                }
                4 => {
                    suffix.compare_and_set_edge_property(
                        EId(60),
                        P,
                        Some(int(999)),
                        int(1),
                        WriteMismatchPolicy::AbortWrite,
                    );
                }
                5 => {
                    suffix.set_vertex_property(VId(999), P, Some(int(1)));
                }
                _ => {
                    suffix.add_edge(EId(90), VId(5), VId(6), vec![(P, unavailable_value())]);
                    suffix.set_edge_property(EId(90), P, Some(int(1)));
                }
            }
            let mut full = prefix.clone();
            full.push(suffix.clone());
            let expected = db.prepare_ordered_writes(full).unwrap_err();
            let actual = db
                .try_extend_prepared_ordered_writes(&previous, &prefix, &[suffix])
                .unwrap_err();
            assert_eq!(
                format!("{actual:?}"),
                format!("{expected:?}"),
                "case {case}"
            );
        }

        let mut low = WriteBatch::new(RelationId(1));
        low.create_vertex(VId(5), vec![], vec![(P, int(5))]);
        let previous = db.prepare_write(low.clone()).unwrap();
        let mut high = WriteBatch::new(RelationId(9));
        high.add_edge(EId(90), VId(5), VId(2), vec![(P, unavailable_value())]);
        high.compare_and_set_vertex_property(
            VId(5),
            P,
            Some(int(999)),
            int(6),
            WriteMismatchPolicy::AbortWrite,
        );
        let expected = db
            .prepare_ordered_writes(vec![low.clone(), high.clone()])
            .unwrap_err();
        assert!(matches!(
            &expected,
            WriteTxnError::Write(WriteError::CompareAndSetMismatch(_))
        ));
        let actual = db
            .try_extend_prepared_ordered_writes(&previous, &[low], &[high])
            .unwrap_err();
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn atomic_prefix_births_survive_ordered_reuse_and_replay_stays_withheld() {
    let ((), report) = run_async_under_lab(0x6972_0105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = seeded(&contexts.commit()).await;
        let mut high = WriteBatch::new(RelationId(9));
        high.create_vertex(VId(5), vec![], vec![(P, int(5))]);
        let mut low = WriteBatch::new(RelationId(2));
        low.create_vertex(VId(6), vec![], vec![(P, int(6))]);
        let prefix = [high, low];
        let previous = db.prepare_atomic_writes(prefix.to_vec()).unwrap();
        assert_eq!(previous.replay_row_allowance, None);
        let mut suffix = WriteBatch::new(RelationId(1));
        suffix.set_vertex_property(VId(5), P, Some(int(7)));
        suffix.add_edge(EId(50), VId(5), VId(6), vec![]);
        let actual = db
            .try_extend_prepared_ordered_writes(&previous, &prefix, &[suffix.clone()])
            .unwrap()
            .unwrap();
        let raw = prefix.into_iter().chain([suffix]).collect();
        let expected = db
            .prepare_ordered_writes(raw)
            .unwrap()
            .retain_birth_ordinals(Some(&previous))
            .unwrap();
        assert_eq!(
            actual.template.canonical_bytes(),
            expected.template.canonical_bytes()
        );
        assert_eq!(actual.dependencies.elements, expected.dependencies.elements);
        assert_eq!(actual.replay_row_allowance, None);
        let births: BTreeMap<_, _> = actual
            .template
            .rows()
            .filter_map(|row| match row {
                DeltaRow::CreateVertex {
                    vid, birth_ordinal, ..
                } => Some((*vid, *birth_ordinal)),
                _ => None,
            })
            .collect();
        assert_eq!(births, BTreeMap::from([(VId(5), 2), (VId(6), 1)]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retired_and_ensured_edge_histories_always_keep_the_full_native_path() {
    let ((), report) = run_async_under_lab(0x6972_0106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = seeded(&contexts.commit()).await;
        for case in 0..3 {
            let mut prefix = WriteBatch::new(RelationId(7));
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
            let mut suffix = WriteBatch::new(RelationId(2));
            suffix.create_vertex(VId(5), vec![], vec![]);
            assert!(
                db.try_extend_prepared_ordered_writes(&previous, &[prefix], &[suffix])
                    .unwrap()
                    .is_none()
            );
        }
        let prefix = program().into_iter().take(3).collect::<Vec<_>>();
        let previous = db.prepare_ordered_writes(prefix.clone()).unwrap();
        for case in 0..3 {
            let mut suffix = WriteBatch::new(RelationId(2));
            match case {
                0 => {
                    suffix.delete_vertex(VId(5));
                }
                1 => {
                    suffix.delete_edge(EId(50));
                }
                _ => {
                    suffix.ensure_edge_by_triple(EId(999), VId(5), VId(6), vec![]);
                }
            }
            assert!(
                db.try_extend_prepared_ordered_writes(&previous, &prefix, &[suffix])
                    .unwrap()
                    .is_none()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
