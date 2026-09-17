//! Incremental derived-index publication versus fresh construction.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xac; 32],
        DatabaseSecurityNamespaceId([0x44; 32]),
        [0x19; 32],
    )
}

#[test]
fn maintained_indexes_equal_rebuild_after_every_publication() {
    for seed in [3_u64, 17, 91] {
        let ((), report) = run_async_under_lab(0xac40 + seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut initial = WriteBatch::new(R);
            for id in 0..8 {
                initial.create_vertex(
                    VId(id),
                    vec![],
                    vec![(P, CanonicalScalar::Int((id as u64 % seed) as i64))],
                );
            }
            initial.add_edge(EId(1), VId(0), VId(1), vec![]);
            initial.add_edge(EId(2), VId(1), VId(0), vec![]);
            initial.add_edge(EId(3), VId(0), VId(0), vec![]);
            let old = db.write(&commit, initial).await.unwrap();
            assert!(db.verify_snapshot_indexes().unwrap());
            let pinned = db.read_session().unwrap();
            let original = pinned.neighbours_at(VId(0), R, old).unwrap();
            for step in 1..=8_u64 {
                let mut batch = WriteBatch::new(R);
                batch.set_vertex_property(
                    VId(u128::from(step % 6)),
                    P,
                    Some(CanonicalScalar::Int((seed * step) as i64)),
                );
                batch.add_edge(
                    EId(u128::from(100 + step)),
                    VId(u128::from((step * seed) % 6)),
                    VId(0),
                    vec![],
                );
                if step == 2 {
                    batch.delete_edge(EId(1));
                }
                if step == 3 {
                    batch.set_edge_property(EId(2), P, Some(CanonicalScalar::Int(9)));
                }
                if step == 4 {
                    batch.delete_vertex(VId(7));
                }
                db.write(&commit, batch).await.unwrap();
                assert!(
                    db.verify_snapshot_indexes().unwrap(),
                    "seed={seed} step={step}"
                );
                assert_eq!(pinned.neighbours_at(VId(0), R, old).unwrap(), original);
                if step == 5 {
                    db.compact(&commit).await.unwrap();
                    assert!(
                        db.verify_snapshot_indexes().unwrap(),
                        "after replacement compaction"
                    );
                }
                if step == 6 {
                    drop(db);
                    db = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
                        .await
                        .unwrap();
                    assert!(db.verify_snapshot_indexes().unwrap(), "after reopen");
                }
            }
            assert_eq!(pinned.neighbours_at(VId(0), R, old).unwrap(), original);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[test]
fn constant_sized_commits_do_not_rebuild_growing_history() {
    let ((), report) = run_async_under_lab(0xac44, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut tenth = (0, 0);
        let mut last = (0, 0);
        // Keep a predecessor pinned throughout: deep COW clones are not an
        // acceptable substitute for path-copy maintenance.
        let pinned = db.read_session().unwrap();
        for step in 1..=400_u64 {
            let id = u128::from(step);
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(P, CanonicalScalar::Int(step as i64))],
            );
            batch.add_edge(EId(id), VId(id), VId(id), vec![]);
            db.write(&commit, batch).await.unwrap();
            let work = db.index_maintenance_work().unwrap();
            assert!(work.0 > 0 && work.1 > 0);
            // AVL height follows logarithmic key-domain depth, not linear
            // history. The fixed ceiling includes rows, encoded property
            // bytes, visited nodes and newly allocated path/rotation nodes.
            assert!(work.0 < 512 && work.1 < 512, "commit={step} work={work:?}");
            if step == 10 {
                tenth = work;
            }
            if step == 400 {
                last = work;
            }
        }
        assert!(
            last.0 <= tenth.0 * 3 && last.1 <= tenth.1 * 3,
            "10={tenth:?} 400={last:?}"
        );
        assert!(db.verify_snapshot_indexes().unwrap());
        drop(pinned);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
