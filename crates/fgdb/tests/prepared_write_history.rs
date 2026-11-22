//! Direct PreparedWrite callers must not depend on a transaction wrapper or
//! the lifetime of a resettable coordinator validator for conflict safety.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, DatabaseState, MemVfs, WriteBatch, WriteError, WriteMismatchPolicy,
};
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}

fn seed_batch() -> WriteBatch {
    let mut seed = WriteBatch::new(R);
    for vid in [VId(1), VId(2), VId(3), VId(8)] {
        seed.create_vertex(vid, vec![], vec![(P, CanonicalScalar::Int(0))]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.expect("database");
    db.write(cx, seed_batch()).await.expect("seed");
    db
}

async fn reset_with_unrelated_writes(db: &mut Database<MemVfs>, cx: &CommitCx) {
    for value in 1..=3 {
        let mut write = WriteBatch::new(R);
        write.set_vertex_property(VId(8), P, Some(CanonicalScalar::Int(value)));
        db.write(cx, write)
            .await
            .expect("intervening ordinary write");
    }
}

fn assert_conflict(result: Result<CommitSeq, WriteError>) {
    assert!(matches!(
        result,
        Err(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-01",
            ..
        })
    ));
}

#[test]
fn direct_prepared_writes_reject_conflicts_hidden_by_multiple_validator_resets() {
    let ((), report) = run_async_under_lab(0xfc20_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for case in 0..6 {
            let mut db = seeded(&cx).await;
            let mut staged = WriteBatch::new(R);
            let mut winner = WriteBatch::new(R);
            match case {
                0 => {
                    staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
                    winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(10)));
                }
                1 => {
                    staged.compare_and_set_vertex_property(
                        VId(1),
                        P,
                        Some(CanonicalScalar::Int(99)),
                        CanonicalScalar::Int(5),
                        WriteMismatchPolicy::NoOp,
                    );
                    winner.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
                }
                2 => {
                    staged.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
                    winner.delete_edge(EId(10));
                }
                3 => {
                    staged.delete_vertex(VId(1));
                    winner.add_edge(EId(11), VId(2), VId(1), vec![]);
                }
                4 => {
                    staged.add_edge(EId(20), VId(2), VId(3), vec![]);
                    winner.delete_vertex(VId(3));
                }
                _ => {
                    staged.ensure_edge_by_triple(EId(21), VId(2), VId(3), vec![]);
                    winner.add_edge(EId(22), VId(2), VId(3), vec![]);
                }
            }
            staged.create_vertex(VId(99), vec![], vec![]);
            let prepared = db.prepare_write(staged).expect("prepare once");
            db.write(&cx, winner).await.expect("winning write");
            reset_with_unrelated_writes(&mut db, &cx).await;
            let frontier = db.frontier().expect("frontier before losing commit");
            assert_conflict(db.commit_prepared(&cx, prepared).await);
            assert_eq!(
                db.frontier().expect("no lost-write marker"),
                frontier,
                "case {case}"
            );
            assert!(matches!(db.state(), DatabaseState::Healthy { .. }));
            assert!(
                db.vertex(VId(99))
                    .expect("no partial publication")
                    .is_none()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn commits_at_or_before_the_basis_are_not_conflicts() {
    let ((), report) = run_async_under_lab(0xfc20_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut before = WriteBatch::new(R);
        before.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
        db.write(&cx, before)
            .await
            .expect("change before the basis");
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
        let prepared = db
            .prepare_write(staged)
            .expect("prepare after observing the change");
        reset_with_unrelated_writes(&mut db, &cx).await;
        db.commit_prepared(&cx, prepared)
            .await
            .expect("disjoint suffix");
        assert_eq!(
            db.vertex(VId(1))
                .expect("vertex read")
                .expect("vertex")
                .props,
            vec![(P, CanonicalScalar::Int(5).into())]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_parallel_edge_creates_remain_committable() {
    let ((), report) = run_async_under_lab(0xfc20_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut first = WriteBatch::new(R);
        first.add_edge(EId(31), VId(1), VId(2), vec![]);
        let mut second = WriteBatch::new(R);
        second.add_edge(EId(32), VId(1), VId(2), vec![]);
        let first = db.prepare_write(first).expect("first prepare");
        let second = db.prepare_write(second).expect("same-basis second prepare");
        assert_eq!(first.basis(), second.basis());
        db.commit_prepared(&cx, first).await.expect("first edge");
        db.commit_prepared(&cx, second)
            .await
            .expect("unconstrained parallel edge");
        assert!(db.edge(EId(31)).expect("first read").is_some());
        assert!(db.edge(EId(32)).expect("second read").is_some());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn restored_property_bytes_do_not_erase_an_intervening_write() {
    let ((), report) = run_async_under_lab(0xfc20_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let mut db = seeded(&cx).await;
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
        let prepared = db.prepare_write(staged).expect("prepare against zero");
        for value in [1, 0] {
            let mut update = WriteBatch::new(R);
            update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(value)));
            db.write(&cx, update).await.expect("change and restore");
        }
        reset_with_unrelated_writes(&mut db, &cx).await;
        assert_conflict(db.commit_prepared(&cx, prepared).await);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn lazy_open_prepared_validation_keeps_latest_writers_guards_and_alias_precedence() {
    let ((), report) = run_async_under_lab(0xfc20_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        for case in 0..4 {
            let vfs = MemVfs::new().expect("vfs");
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
                .await
                .expect("database");
            db.write(&cx, seed_batch()).await.expect("seed");
            drop(db);
            let mut db = Database::open_with_vfs(&cx, vfs, &path, keys())
                .await
                .expect("lazy reopen");
            assert!(db.delta_index().unwrap().is_empty());
            assert_eq!(
                db.delta_index().unwrap().retained_after_commit_seq(),
                CommitSeq(1)
            );
            let pinned = db.pinned_read_view().expect("pin original generation");
            let mut staged = WriteBatch::new(R);
            let mut first = WriteBatch::new(R);
            let mut second = WriteBatch::new(R);
            let mut third = WriteBatch::new(R);
            let expected_detail = match case {
                0 => {
                    staged.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(5)));
                    staged.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(5)));
                    first.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(10)));
                    second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
                    third.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                    // Canonical dependency order chooses vertex 1, even though
                    // vertex 2 changed earlier. Reverting bytes is still a write.
                    format!(
                        "prepared dependency {:?} changed at {:?}; draft {:?} loses first-committer-wins",
                        ElementId::Vertex(VId(1)),
                        CommitSeq(4),
                        CommitSeq(5),
                    )
                }
                1 => {
                    staged.compare_and_set_vertex_property(
                        VId(1),
                        P,
                        Some(CanonicalScalar::Int(99)),
                        CanonicalScalar::Int(5),
                        WriteMismatchPolicy::NoOp,
                    );
                    first.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
                    second.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
                    third.ensure_vertex(VId(8), vec![], vec![]);
                    // The normalized guard has no effect row but retains its
                    // observed target through an unrelated empty global tick.
                    format!(
                        "prepared dependency {:?} changed at {:?}; draft {:?} loses first-committer-wins",
                        ElementId::Vertex(VId(1)),
                        CommitSeq(3),
                        CommitSeq(5),
                    )
                }
                2 => {
                    staged.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
                    first.delete_edge(EId(10));
                    second.add_edge(EId(11), VId(1), VId(2), vec![]);
                    third.ensure_vertex(VId(8), vec![], vec![]);
                    // The aliased identity's deletion wins over a later
                    // insertion into the same witnessed adjacency.
                    format!(
                        "prepared dependency {:?} changed at {:?}; draft {:?} loses first-committer-wins",
                        ElementId::Edge(EId(10)),
                        CommitSeq(2),
                        CommitSeq(5),
                    )
                }
                _ => {
                    staged.delete_vertex(VId(1));
                    first.add_edge(EId(11), VId(2), VId(1), vec![]);
                    second.add_edge(EId(12), VId(1), VId(3), vec![]);
                    third.ensure_vertex(VId(8), vec![], vec![]);
                    format!(
                        "prepared adjacency at {:?} gained an edge at {:?}; draft {:?} must be prepared again",
                        VId(1),
                        CommitSeq(3),
                        CommitSeq(5),
                    )
                }
            };
            staged.create_vertex(VId(99), vec![], vec![]);
            let prepared = db.prepare_write(staged).expect("same-owner preparation");
            for (index, batch) in [first, second, third].into_iter().enumerate() {
                assert_eq!(
                    db.write(&cx, batch).await.expect("intervening batch"),
                    CommitSeq(index as u64 + 2),
                );
            }
            let window = db.delta_index().unwrap().clone();
            assert_eq!(window.len(), 3);
            assert_eq!(window.retained_after_commit_seq(), CommitSeq(1));
            let error = db
                .commit_prepared(&cx, prepared)
                .await
                .expect_err("conflicting historical preparation");
            let WriteError::FirstCommitterWins { law, detail } = error else {
                panic!("case {case}: expected FCW rejection, got {error:?}");
            };
            assert_eq!(law, "FG-LAW-FCW-01", "case {case}");
            assert_eq!(detail, expected_detail, "case {case}");
            assert_eq!(db.delta_index().unwrap(), &window);
            assert_eq!(db.frontier().unwrap(), CommitSeq(4));
            assert!(matches!(db.state(), DatabaseState::Healthy { .. }));
            assert!(db.vertex(VId(99)).unwrap().is_none());
            assert_eq!(pinned.frontier(), CommitSeq(1));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
