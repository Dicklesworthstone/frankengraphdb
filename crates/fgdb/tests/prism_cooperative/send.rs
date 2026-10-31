//! Send is conditional on the supplied callbacks, not erased by Prism's live
//! control wrapper. These bounds are checked on the actual public futures.

use super::*;
use fgdb::{BufferLimits, BufferedReadLimits, MemoryPool};
use fgdb_prism::{SealedProjectionMask, SealedProjectionSpec};
use fgdb_strata::store::BlockStore;
use std::rc::Rc;

fn assert_send<F: Future + Send>(future: F) -> F {
    future
}

#[test]
fn unguarded_kernels_and_database_and_buffered_owners_are_send_with_identical_scores() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Reversed);
        let graph = projection(&db, &cx, opt).await;
        let call = FnxCallSpec::katz_centrality(KatzOptions::new(0.025, 1.0, 500, 1e-10).unwrap());
        let expected = call
            .execute_sealed(&cx, &graph, opt.execution_limits, memory())
            .unwrap();

        let collected = assert_send(call.execute_sealed_cooperative(
            &cx,
            &graph,
            opt.execution_limits,
            memory(),
            quantum(3),
            yield_now,
        ))
        .await
        .unwrap();
        compare_execution(&collected, &expected);
        let retained = assert_send(call.execute_sealed_cooperative_rows(
            &cx,
            &graph,
            opt.execution_limits,
            memory(),
            quantum(3),
            yield_now,
        ))
        .await
        .unwrap();
        compare_retained(retained, &collected, graph.node_count());

        let owned = assert_send(db.execute_fnx_sealed_cooperative(
            &cx,
            &call,
            opt,
            memory(),
            SealedLimits::default(),
            quantum(3),
        ))
        .await
        .unwrap();
        assert_eq!(owned.analytics.rows, collected.rows);
        assert_eq!(
            owned.analytics.certificate.result_digest,
            collected.certificate.result_digest
        );

        let mut buffered = db
            .buffered_read_view(
                &cx,
                MemoryPool::new(16 * 1024 * 1024, 0).unwrap(),
                BufferedReadLimits {
                    max_root_bytes: 64 * 1024,
                    max_source_bytes: 4 * 1024 * 1024,
                    max_blocks: 64,
                    max_vertex_patches: 64,
                    max_work: 1_000_000,
                    buffer: BufferLimits {
                        max_frames: 1,
                        max_ghost_entries: 2,
                        max_extent_bytes: 16 * 1024,
                    },
                },
            )
            .await
            .unwrap();
        let cold = assert_send(buffered.execute_fnx_sealed_cooperative(
            &cx,
            &call,
            opt,
            memory(),
            SealedLimits::default(),
            1000,
            quantum(3),
        ))
        .await
        .unwrap();
        assert_eq!(cold.analytics.rows, collected.rows);
        assert_eq!(
            cold.analytics.certificate.result_digest,
            collected.certificate.result_digest
        );

        // Neither local guards nor local yield futures acquired an accidental
        // Send/Sync API bound while making the unguarded futures portable.
        let guards = Rc::new(Cell::new(0));
        let yields = Rc::new(Cell::new(0));
        let local = call
            .execute_sealed_cooperative_with_checkpoint(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(3),
                || {
                    let local = Rc::clone(&yields);
                    async move {
                        local.set(local.get() + 1);
                        yield_now().await;
                        assert!(local.get() > 0); // Keep Rc live across suspension.
                    }
                },
                || {
                    guards.set(guards.get() + 1);
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(local, collected);
        assert!(guards.get() > 0 && yields.get() > 0);
        let retained_local = call
            .execute_sealed_cooperative_rows_with_checkpoint(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(3),
                yield_now,
                || {
                    guards.set(guards.get() + 1);
                    Ok(())
                },
            )
            .await
            .unwrap();
        compare_retained(retained_local, &collected, graph.node_count());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn unguarded_projection_builder_is_send_and_guarded_builder_keeps_local_callbacks() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let namespace = DatabaseSecurityNamespaceId([0x62; 32]);
        let keys = DatabaseKeys::new([0x61; 32], namespace, [0x63; 32]);
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys)
            .await
            .unwrap();
        let vertices = [VId(1), VId(1u128 << 80), VId(u128::MAX)];
        let mut batch = WriteBatch::new(RelationId(1));
        for vertex in vertices {
            batch.create_vertex(vertex, vec![LabelId(1)], vec![]);
        }
        for (id, source, target) in [(1, 0, 1), (2, 1, 2)] {
            batch.add_edge(
                EId(id),
                vertices[source],
                vertices[target],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(2))],
            );
        }
        let at = db.write(&commit, batch).await.unwrap();
        let store = BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            [0x61; 32],
            namespace,
        )
        .await
        .unwrap();
        let image = store
            .seal_relation(
                &cx,
                db.read_session().unwrap().partition_root(),
                at,
                RelationId(1),
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let opt = options(Directedness::Undirected);
        let config = SealedProjectionSpec {
            as_of: at,
            selection: opt.selection,
            projection: opt.projection,
        };
        let expected =
            SealedGraphView::build(&cx, &image, &vertices, config, opt.projection_limits).unwrap();
        let actual = assert_send(SealedGraphView::build_cooperative(
            &cx,
            &image,
            &vertices,
            config,
            opt.projection_limits,
            quantum(3),
            yield_now,
        ))
        .await
        .unwrap();
        assert_eq!(actual.binding(), expected.binding());
        assert_eq!(actual.digest(), expected.digest());
        assert_eq!(actual.vertex_ids(), expected.vertex_ids());
        assert_eq!(
            actual.adjacency_entry_count(),
            expected.adjacency_entry_count()
        );
        let call = FnxCallSpec::katz_centrality(KatzOptions::default());
        let expected_scores = call
            .execute_sealed(&cx, &expected, opt.execution_limits, memory())
            .unwrap();
        let actual_scores = call
            .execute_sealed(&cx, &actual, opt.execution_limits, memory())
            .unwrap();
        assert_eq!(actual_scores, expected_scores);

        let guards = Rc::new(Cell::new(0));
        let guarded = SealedGraphView::build_cooperative_with_checkpoint(
            &cx,
            &image,
            &vertices,
            config,
            SealedProjectionMask::UNMASKED,
            opt.projection_limits,
            quantum(3),
            yield_now,
            || {
                guards.set(guards.get() + 1);
                Ok(())
            },
        )
        .await
        .unwrap();
        assert!(guards.get() > 0);
        assert_eq!(guarded.digest(), expected.digest());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
