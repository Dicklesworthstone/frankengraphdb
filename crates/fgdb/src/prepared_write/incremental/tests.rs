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
                    Some(previous) => db
                        .try_extend_prepared_write(previous, &staged, &batch)
                        .unwrap()
                        .expect("the mixed append/SET/CAS prefix must take the incremental lane"),
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
