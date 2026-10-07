//! The public cold reader uses the real Chronicle/Strata checkpoint and Vfs.
//! Resident views are an independent oracle; explicit expected values also
//! distinguish historical property changes, deletion and parallel incidence.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedOpenError, BufferedReadError, BufferedReadLimits, Database, DatabaseKeys,
    DerivedPublicationStage, EmbeddedReadView, MemVfs, MemoryPool, WriteBatch, WriteError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const PROPERTY: PropertyKeyId = PropertyKeyId(1);
const RELATION: RelationId = RelationId(7);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}

fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 4 * 1024 * 1024,
        max_blocks: 512,
        max_vertex_patches: 512,
        max_work: 100_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}

fn pool() -> MemoryPool {
    MemoryPool::new(16 * 1024 * 1024, 0).unwrap()
}

fn vertex_batch(vid: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RELATION);
    batch.create_vertex(
        VId(vid),
        vec![LabelId(1)],
        vec![(PROPERTY, CanonicalScalar::Int(value))],
    );
    batch
}

async fn history(cx: &CommitCx) -> (MemVfs, EmbeddedReadView) {
    let vfs = MemVfs::new().unwrap();
    let path = vfs.database_dir();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), &path, keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(RELATION);
    for vid in 1..=4 {
        first.create_vertex(
            VId(vid),
            vec![LabelId(1)],
            vec![(PROPERTY, CanonicalScalar::Int(vid as i64))],
        );
    }
    for (eid, src, dst, value) in [
        (101, 1, 2, 7),
        (102, 1, 3, 8),
        (103, 4, 1, 9),
        (104, 1, 2, 10),
    ] {
        first.add_edge(
            EId(eid),
            VId(src),
            VId(dst),
            vec![(PROPERTY, CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, first).await.unwrap(), CommitSeq(1));
    let mut second = WriteBatch::new(RELATION);
    second.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(11)));
    second.set_edge_property(EId(101), PROPERTY, Some(CanonicalScalar::Int(17)));
    second.delete_vertex(VId(3));
    assert_eq!(db.write(cx, second).await.unwrap(), CommitSeq(2));
    let mut third = WriteBatch::new(RelationId(8));
    third.add_edge(
        EId(105),
        VId(1),
        VId(4),
        vec![(PROPERTY, CanonicalScalar::Int(20))],
    );
    assert_eq!(db.write(cx, third).await.unwrap(), CommitSeq(3));
    (vfs, db.read_session().unwrap())
}

#[test]
fn buffered_reads_match_resident_history_and_stay_on_their_root_through_eviction_and_compaction() {
    let ((), report) = run_async_under_lab(0x6275_6601, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, reference) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        assert_eq!(view.frontier(), CommitSeq(3));
        assert_eq!(view.partition_root(), reference.partition_root());
        assert_eq!(view.manifest(), reference.manifest());
        assert_eq!(
            view.buffer_stats().misses,
            0,
            "admission leaves a cold cache"
        );

        for seq in 0..=3 {
            let cut = CommitSeq(seq);
            for vid in [1, 2, 3, 4, 999] {
                let expected = reference.vertex_at(VId(vid), cut).unwrap();
                let actual = view.vertex_at(&cx, VId(vid), cut).await.unwrap();
                assert_eq!(
                    actual.as_deref(),
                    expected.as_ref(),
                    "vertex {vid} at {seq}"
                );
            }
            for eid in [101, 102, 103, 104, 105, 999] {
                let expected = reference.edge_at(EId(eid), cut).unwrap();
                let actual = view.edge_at(&cx, EId(eid), cut).await.unwrap();
                assert_eq!(
                    actual.as_ref().map(|row| (&row.entry, &row.props)),
                    expected.as_ref().map(|row| (&row.entry, &row.props)),
                    "edge {eid} at {seq}"
                );
            }
            for vertex in [VId(1), VId(3), VId(4)] {
                for incoming in [false, true] {
                    for relation in [None, Some(RELATION), Some(RelationId(8))] {
                        let mut expected: Vec<_> = reference
                            .edges_at(cut)
                            .unwrap()
                            .into_iter()
                            .map(|row| row.entry)
                            .filter(|row| {
                                (if incoming { row.dst } else { row.src }) == vertex
                                    && relation.is_none_or(|wanted| row.relation == wanted)
                            })
                            .collect();
                        expected
                            .sort_unstable_by_key(|row| (row.src, row.relation, row.dst, row.eid));
                        let actual = view
                            .adjacency_at(&cx, vertex, relation, incoming, cut, 32)
                            .await
                            .unwrap();
                        assert_eq!(actual.as_ref(), &expected);
                    }
                }
            }
        }
        assert_eq!(
            view.vertex_at(&cx, VId(1), CommitSeq(1))
                .await
                .unwrap()
                .unwrap()
                .props,
            [(PROPERTY, CanonicalScalar::Int(1))]
        );
        assert_eq!(
            view.edge_at(&cx, EId(101), CommitSeq(1))
                .await
                .unwrap()
                .unwrap()
                .props,
            [(PROPERTY, CanonicalScalar::Int(7))]
        );
        assert!(view.vertex(&cx, VId(3)).await.unwrap().is_none());
        assert!(view.edge(&cx, EId(102)).await.unwrap().is_none());
        let incidence = view.adjacency(&cx, VId(1), None, 32).await.unwrap();
        assert_eq!(
            incidence.iter().map(|row| row.eid).collect::<Vec<_>>(),
            [EId(101), EId(104), EId(105)]
        );
        drop(incidence);
        assert!(view.buffer_stats().misses > 1);
        assert!(view.buffer_stats().evictions > 0);
        assert!(pool.used() <= pool.limit());

        // The cold view owns no writer lease. A successor and compaction use
        // the real writer while this view continues to refault its old objects.
        let mut writer = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        let mut change = WriteBatch::new(RELATION);
        change.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(99)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(4));
        writer.compact(&commit).await.unwrap();
        drop(writer);
        let answer = view.vertex(&cx, VId(1)).await.unwrap().unwrap();
        assert_eq!(answer.props, [(PROPERTY, CanonicalScalar::Int(11))]);
        assert_eq!(view.frontier(), CommitSeq(3));
        drop(view);
        assert!(pool.used() > 0, "a retained answer keeps its own charge");
        assert_eq!(answer.props, [(PROPERTY, CanonicalScalar::Int(11))]);
        drop(answer);
        assert_eq!(
            pool.used(),
            0,
            "all view and answer reservations are refunded"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn buffered_limits_refuse_whole_opens_or_reads_and_refund_every_reservation() {
    let ((), report) = run_async_under_lab(0x6275_6602, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        for selected_limits in [
            BufferedReadLimits {
                max_root_bytes: 1,
                ..limits()
            },
            BufferedReadLimits {
                max_blocks: 0,
                ..limits()
            },
            BufferedReadLimits {
                max_vertex_patches: 0,
                ..limits()
            },
            BufferedReadLimits {
                max_source_bytes: 0,
                ..limits()
            },
            BufferedReadLimits {
                max_work: 0,
                ..limits()
            },
            BufferedReadLimits {
                max_root_bytes: usize::MAX,
                ..limits()
            },
        ] {
            let pool = pool();
            assert!(
                Database::open_buffered_read_view_with_vfs(
                    &commit,
                    vfs.clone(),
                    &path,
                    keys(),
                    pool.clone(),
                    selected_limits,
                )
                .await
                .is_err(),
                "{selected_limits:?}"
            );
            assert_eq!(pool.used(), 0, "failed admission retains no decoded state");
        }
        let empty_pool = MemoryPool::new(0, 0).unwrap();
        assert!(matches!(
            Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                &path,
                keys(),
                empty_pool.clone(),
                limits(),
            )
            .await,
            Err(BufferedOpenError::Memory(_))
        ));
        assert_eq!(empty_pool.used(), 0);

        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        assert!(matches!(
            view.adjacency(&cx, VId(1), None, 0).await,
            Err(BufferedReadError::Limit {
                resource: "buffered adjacency identities",
                ..
            })
        ));
        let before = pool.used();
        assert!(matches!(
            view.vertex_at(&cx, VId(1), CommitSeq(4)).await,
            Err(BufferedReadError::BeyondPublication {
                requested: CommitSeq(4),
                publication: CommitSeq(3),
            })
        ));
        assert_eq!(pool.used(), before);
        let occupied = pool.reserve(&cx, pool.available()).unwrap();
        assert!(view.vertex(&cx, VId(1)).await.is_err());
        drop(occupied);
        assert!(view.vertex(&cx, VId(1)).await.unwrap().is_some());
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_lagging_checkpoint_requires_explicit_recovery_before_buffered_open() {
    let ((), report) = run_async_under_lab(0x6275_6603, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, vertex_batch(1, 7)).await.unwrap();
        assert!(matches!(
            db.write_with_publication_failure(
                &commit,
                vertex_batch(2, 9),
                DerivedPublicationStage::PublishPartitionRoot,
            )
            .await,
            Err(WriteError::CommittedNeedsRecovery { .. })
        ));
        drop(db);
        let pool = pool();
        assert!(matches!(
            Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                &path,
                keys(),
                pool.clone(),
                limits(),
            )
            .await,
            Err(BufferedOpenError::RecoveryRequired)
        ));
        assert_eq!(pool.used(), 0);

        // Recovery is an explicit writable operation, never an implicit
        // full-generation allocation hidden behind a buffered read request.
        let recovered = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        drop(recovered);
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        assert_eq!(view.frontier(), CommitSeq(2));
        let vertex = view.vertex(&cx, VId(2)).await.unwrap().unwrap();
        assert_eq!(vertex.props, [(PROPERTY, CanonicalScalar::Int(9))]);
        drop(vertex);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
