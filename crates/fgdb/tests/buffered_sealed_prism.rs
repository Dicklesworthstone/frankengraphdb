//! The public cold reader feeds the real sealed image and native kernel.
//! Hand-enumerated triangle counts guard the resident comparison against a
//! shared wrong answer. Full-width identities and an isolate are deliberate.
#![recursion_limit = "256"]

use asupersync::fs::Vfs;
use asupersync::lab::run_async_under_lab;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{
    BufferLimits, BufferedReadError, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys,
    MemVfs, MemoryPool, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaSelection};
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadError, FnxReadOptions,
    FnxReadResult, FnxSealedExecutionError, FnxSealedReadError, FnxValue, ParallelEdgePolicy,
    SealedGraphView, SealedProjectionError, SelfLoopPolicy,
};
use fgdb_strata::store::{BlockStore, RootReadLimits};
use fgdb_strata::tiered::sealed::{SealedError, SealedLimits};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::sync::Arc;
use std::{
    future::Future,
    num::NonZeroUsize,
    task::{Context, Poll, Waker},
};

const R: RelationId = RelationId(7);
const L: LabelId = LabelId(10);
const P: PropertyKeyId = PropertyKeyId(9);
const BASE: u128 = 1_u128 << 100;
const TRIANGLES: &str = "CALL fnx.triangles() YIELD vertex AS id,triangles AS n";
const RAW: usize = 1000;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xe1; 32],
        DatabaseSecurityNamespaceId([0xe2; 32]),
        [0xe3; 32],
    )
}

fn pool() -> MemoryPool {
    MemoryPool::new(16 * 1024 * 1024, 0).unwrap()
}

fn limits() -> BufferedReadLimits {
    BufferedReadLimits {
        max_root_bytes: 64 * 1024,
        max_source_bytes: 4 * 1024 * 1024,
        max_blocks: 512,
        max_vertex_patches: 512,
        max_work: 1_000_000,
        buffer: BufferLimits {
            max_frames: 1,
            max_ghost_entries: 2,
            max_extent_bytes: 16 * 1024,
        },
    }
}

fn options() -> FnxReadOptions {
    let mut options = FnxReadOptions::whole_graph(None);
    options.selection.vertex_labels = Some(L).into();
    options.selection.relations = Some(R).into();
    options.projection.directedness = Directedness::Undirected;
    options.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
    options.projection.self_loops = SelfLoopPolicy::Drop;
    options.source_limits.max_work_units = 100_000;
    options.source_limits.max_scratch_entries = 1000;
    options.source_limits.max_staging_bytes = 16 * 1024;
    options.projection_limits.max_vertices = 100;
    options.projection_limits.max_input_edges = 100;
    options.projection_limits.max_adjacency_entries = 200;
    options.projection_limits.max_workspace_bytes = 256 * 1024;
    options.execution_limits.max_result_rows = 100;
    options.execution_limits.max_estimated_work = 100_000;
    options
}

fn memory() -> FnxMemoryLimits {
    FnxMemoryLimits {
        max_kernel_workspace_bytes: 256 * 1024,
        max_result_bytes: 64 * 1024,
    }
}

fn sealing() -> SealedLimits {
    SealedLimits {
        max_rows: 100,
        max_incidences: 100,
        max_image_bytes: 256 * 1024,
        max_property_bytes: 64 * 1024,
    }
}

fn call() -> FnxCallSpec {
    FnxCallSpec::bind(TRIANGLES, &FnxParameters::new()).unwrap()
}

fn expected(vertices: &[u128], triangle: bool) -> Vec<Vec<FnxValue>> {
    vertices
        .iter()
        .map(|&id| {
            vec![
                FnxValue::Vertex(VId(id)),
                FnxValue::Integer(u64::from(triangle && id != u128::MAX)),
            ]
        })
        .collect()
}

async fn cold(
    contexts: &PurposeContexts,
    vfs: &MemVfs,
    pool: MemoryPool,
) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        pool,
        limits(),
    )
    .await
    .unwrap()
}

async fn seed(contexts: &PurposeContexts, vfs: &MemVfs) -> Database<MemVfs> {
    let commit = contexts.commit();
    let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for id in [BASE + 1, BASE + 2, BASE + 3, u128::MAX] {
        batch.create_vertex(VId(id), vec![L], vec![]);
    }
    for (eid, source, target) in [(1, 1, 2), (2, 2, 3), (3, 3, 1), (4, 1, 2)] {
        batch.add_edge(
            EId(BASE + eid),
            VId(BASE + source),
            VId(BASE + target),
            vec![],
        );
    }
    assert_eq!(db.write(&commit, batch).await.unwrap(), CommitSeq(1));
    let mut unrelated = WriteBatch::new(RelationId(8));
    unrelated.add_edge(
        EId(u128::MAX),
        VId(BASE + 1),
        VId(u128::MAX),
        vec![(P, CanonicalScalar::Bool(false))],
    );
    assert_eq!(db.write(&commit, unrelated).await.unwrap(), CommitSeq(2));
    db
}

async fn durable_projection(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    options: FnxReadOptions,
) -> SealedGraphView {
    view.prism_sealed_projection_at(
        cx,
        options.as_of.unwrap_or(view.frontier()),
        options.selection,
        options.projection,
        options.projection_limits,
        options.source_limits,
        sealing(),
        RAW,
    )
    .await
    .unwrap()
}

#[test]
fn durable_buffered_runs_keep_native_and_carried_provenance_across_history_and_reopen() {
    let ((), report) = run_async_under_lab(0x6273_0120, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let source_root = db.partition_root().unwrap();
        for relation in [R, RelationId(99)] {
            db.seal_relation(
                &commit,
                &cx,
                relation,
                CommitSeq(2),
                sealing(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        }
        let native_root = db.partition_root().unwrap();
        let pinned_pool = pool();
        let mut pinned = cold(&contexts, &vfs, pinned_pool.clone()).await;
        let native = durable_projection(&mut pinned, &cx, options()).await;
        let image = native.unmasked_source().unwrap();
        assert!(image.has_durable_incoming());
        assert_eq!(image.anchor().scope().source_root, source_root);
        assert_eq!(image.anchor().scope().floor, CommitSeq(2));
        assert_eq!(image.anchor().scope().publication, CommitSeq(2));
        assert_eq!(image.anchor().verified_through(), CommitSeq(2));
        assert_eq!(image.anchor().admitted_envelope_root(), Some(native_root));
        let triangle = expected(&[BASE + 1, BASE + 2, BASE + 3, u128::MAX], true);
        assert_eq!(
            call()
                .execute_sealed(&cx, &native, options().execution_limits, memory())
                .unwrap()
                .rows,
            triangle
        );

        // An unrelated commit carries both runs and advances their coverage,
        // while their physical source, floor and publication remain original.
        let mut c3 = WriteBatch::new(RelationId(8));
        c3.set_vertex_property(VId(BASE + 1), P, Some(CanonicalScalar::Int(55)));
        assert_eq!(db.write(&commit, c3).await.unwrap(), CommitSeq(3));
        let carried_root = db.partition_root().unwrap();
        let mut carried = cold(&contexts, &vfs, pool()).await;
        let graph = durable_projection(&mut carried, &cx, options()).await;
        let image = graph.unmasked_source().unwrap();
        assert!(image.has_durable_incoming());
        assert_eq!(image.anchor().scope().source_root, source_root);
        assert_eq!(image.anchor().scope().publication, CommitSeq(2));
        assert_eq!(image.anchor().verified_through(), CommitSeq(3));
        assert_eq!(image.anchor().admitted_envelope_root(), Some(carried_root));
        assert_eq!(
            call()
                .execute_sealed(&cx, &graph, options().execution_limits, memory())
                .unwrap()
                .rows,
            triangle
        );

        // R now requires the persisted base plus a delta. Deletion, property
        // replacement and an added edge must survive physical consolidation.
        let mut c4 = WriteBatch::new(R);
        c4.delete_edge(EId(BASE + 3));
        c4.set_edge_property(EId(BASE + 1), P, Some(CanonicalScalar::Int(7)));
        c4.add_edge(EId(BASE + 5), VId(BASE + 2), VId(u128::MAX), vec![]);
        assert_eq!(db.write(&commit, c4).await.unwrap(), CommitSeq(4));
        let before_compact = db.partition_root().unwrap();
        db.compact(&commit).await.unwrap();
        assert_ne!(db.partition_root().unwrap(), before_compact);
        let mut current = cold(&contexts, &vfs, pool()).await;
        for cut in [
            CommitSeq::ORIGIN,
            CommitSeq(1),
            CommitSeq(2),
            CommitSeq(3),
            CommitSeq(4),
        ] {
            let mut opt = options();
            opt.as_of = Some(cut);
            let actual = current
                .execute_fnx_sealed(&cx, &call(), opt, memory(), sealing(), RAW)
                .await
                .unwrap();
            let literal = if cut == CommitSeq::ORIGIN {
                Vec::new()
            } else {
                expected(
                    &[BASE + 1, BASE + 2, BASE + 3, u128::MAX],
                    cut < CommitSeq(4),
                )
            };
            assert_eq!(actual.analytics.rows, literal, "cut {cut:?}");
            let resident = db
                .call_fnx(&cx, TRIANGLES, &FnxParameters::new(), opt)
                .unwrap();
            assert_eq!(actual.analytics.rows, resident.analytics.rows);
            assert_eq!(
                actual.analytics.certificate.snapshot,
                resident.analytics.certificate.snapshot
            );
        }
        let overlay = durable_projection(&mut current, &cx, options()).await;
        assert!(overlay.unmasked_source().unwrap().has_durable_incoming());
        let mut empty = options();
        empty.selection.relations = Some(RelationId(99)).into();
        let empty_graph = durable_projection(&mut current, &cx, empty).await;
        let empty_source = empty_graph.unmasked_source().unwrap();
        assert_eq!(empty_source.anchor().scope().source_root, source_root);
        assert_eq!(empty_source.anchor().scope().publication, CommitSeq(2));
        assert_eq!(empty_source.anchor().verified_through(), CommitSeq(4));
        assert_eq!(
            call()
                .execute_sealed(&cx, &empty_graph, empty.execution_limits, memory())
                .unwrap()
                .rows,
            expected(&[BASE + 1, BASE + 2, BASE + 3, u128::MAX], false)
        );
        drop(db);
        assert_eq!(
            pinned
                .execute_fnx_sealed(&cx, &call(), options(), memory(), sealing(), RAW)
                .await
                .unwrap()
                .analytics
                .rows,
            triangle
        );
        drop(pinned);
        assert_eq!(pinned_pool.used(), 0);
        let db = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut reopened = cold(&contexts, &vfs, pool()).await;
        let actual = reopened
            .execute_fnx_sealed(&cx, &call(), options(), memory(), sealing(), RAW)
            .await
            .unwrap();
        assert_eq!(
            actual.analytics.rows,
            expected(&[BASE + 1, BASE + 2, BASE + 3, u128::MAX], false)
        );
        let resident = db
            .call_fnx(&cx, TRIANGLES, &FnxParameters::new(), options())
            .unwrap();
        assert_eq!(actual.analytics.rows, resident.analytics.rows);
        assert_eq!(
            actual.analytics.certificate.snapshot,
            resident.analytics.certificate.snapshot
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn admitted_buffered_runs_do_not_reseal_old_blocks_or_hide_attached_run_damage() {
    let ((), report) = run_async_under_lab(0x6273_0121, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for stale in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let mut db = seed(&contexts, &vfs).await;
            // The retained run contains four incidences, but its selected
            // Tier-D history also contains the replaced property's statements.
            let mut update = WriteBatch::new(R);
            update.set_edge_property(EId(BASE + 1), P, Some(CanonicalScalar::Int(5)));
            assert_eq!(db.write(&commit, update).await.unwrap(), CommitSeq(3));
            let run = db
                .seal_relation(
                    &commit,
                    &cx,
                    R,
                    CommitSeq(3),
                    sealing(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
            if stale {
                let mut suffix = WriteBatch::new(R);
                suffix.delete_edge(EId(BASE + 3));
                suffix.add_edge(EId(BASE + 5), VId(BASE + 2), VId(u128::MAX), vec![]);
                assert_eq!(db.write(&commit, suffix).await.unwrap(), CommitSeq(4));
            }
            let store = BlockStore::open_existing_for_read_with_vfs(
                &commit,
                vfs.clone(),
                vfs.database_dir(),
                [0xe1; 32],
                DatabaseSecurityNamespaceId([0xe2; 32]),
            )
            .await
            .unwrap();
            let root = store
                .get_root(&commit, db.partition_root().unwrap())
                .await
                .unwrap();
            let mut old_block = None;
            for reference in root.blocks {
                if reference.last_seq > CommitSeq(3) {
                    continue;
                }
                let bytes = store
                    .get_bytes(&commit, fgdb_strata::DeltaBlockVersion(reference.block_id))
                    .await
                    .unwrap();
                if fgdb_strata::decode_block(&bytes)
                    .unwrap()
                    .iter()
                    .any(|entry| entry.relation == R)
                {
                    old_block = Some(store.path(reference.block_id));
                    break;
                }
            }
            let old_block = old_block.expect("the sealed prefix has a Tier-D source block");
            let original_block = vfs.read(&old_block).await.unwrap();
            let run_path = store.path(run.0);
            let original_run = vfs.read(&run_path).await.unwrap();
            let source_pool = pool();
            // Full source authentication must complete before this fault.
            let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
            view.set_sealed_read_cache_pool(&cx, MemoryPool::new(0, 0).unwrap())
                .unwrap();
            vfs.write(&old_block, b"damaged redundant pre-seal Tier-D block")
                .await
                .unwrap();
            let actual = view
                .execute_fnx_sealed(&cx, &call(), options(), memory(), sealing(), RAW)
                .await
                .unwrap();
            let literal = expected(&[BASE + 1, BASE + 2, BASE + 3, u128::MAX], !stale);
            assert_eq!(actual.analytics.rows, literal);
            // A fresh reader still validates every source family. Run reuse
            // does not authorize skipping initial complete-source admission.
            assert!(
                Database::open_buffered_read_view_with_vfs(
                    &commit,
                    vfs.clone(),
                    vfs.database_dir(),
                    keys(),
                    pool(),
                    limits(),
                )
                .await
                .is_err()
            );
            vfs.write(&old_block, &original_block).await.unwrap();

            vfs.write(&run_path, b"damaged selected durable adjacency run")
                .await
                .unwrap();
            // Admission charges raw history before touching run bytes; the
            // smaller compacted image cannot excuse those source records.
            let error = view
                .execute_fnx_sealed(&cx, &call(), options(), memory(), sealing(), 4)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    FnxSealedReadError::Seal(SealedError::Limit { limit: 4, .. })
                ),
                "{error:?}"
            );
            let error = view
                .execute_fnx_sealed(&cx, &call(), options(), memory(), sealing(), RAW)
                .await
                .unwrap_err();
            assert!(matches!(error, FnxSealedReadError::Seal(_)), "{error:?}");
            // Healthy Tier-D bytes cannot replace an attached damaged run.
            // Restoring the original object keeps this same reader usable.
            vfs.write(&run_path, &original_run).await.unwrap();
            assert_eq!(
                view.execute_fnx_sealed(&cx, &call(), options(), memory(), sealing(), RAW)
                    .await
                    .unwrap()
                    .analytics
                    .rows,
                literal
            );
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn durable_buffered_run_admission_preserves_limits_cancellation_and_reservation_refunds() {
    let ((), report) = run_async_under_lab(0x6273_0122, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for stale in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let mut db = seed(&contexts, &vfs).await;
            db.seal_relation(
                &commit,
                &cx,
                R,
                CommitSeq(1),
                sealing(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
            if stale {
                // Three independent source families force simultaneous suffix
                // reservations in the bounded, in-memory overlay constructor.
                let mut suffix = WriteBatch::new(R);
                suffix.set_edge_property(EId(BASE + 1), P, Some(CanonicalScalar::Int(5)));
                suffix.delete_edge(EId(BASE + 3));
                suffix.add_edge(EId(BASE + 5), VId(BASE + 2), VId(u128::MAX), vec![]);
                assert_eq!(db.write(&commit, suffix).await.unwrap(), CommitSeq(3));
            }
            let source_pool = pool();
            let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
            view.set_sealed_read_cache_pool(&cx, MemoryPool::new(0, 0).unwrap())
                .unwrap();
            let procedure = call();
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
            let expected = view
                .execute_fnx_sealed(&observed, &procedure, options(), memory(), sealing(), RAW)
                .await
                .unwrap();
            let checkpoints = probe.calls();
            assert!(checkpoints > 1);

            if stale {
                let pressure = source_pool
                    .reserve(&cx, source_pool.available() - 3 * 1024 * 1024)
                    .unwrap();
                let held = source_pool.used();
                let error = view
                    .execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW)
                    .await
                    .unwrap_err();
                let FnxSealedReadError::Seal(SealedError::BufferedSource(error)) = error else {
                    panic!("suffix memory admission must preserve its source error: {error:?}");
                };
                assert!(matches!(
                    *error,
                    BufferedReadError::Buffer(fgdb_strata::tiered::buffer::BufferError::Memory(
                        fgdb::MemoryError::ResourceExhausted { .. }
                    ))
                ));
                assert!(
                    source_pool.used() <= held,
                    "failed suffix leaked its reservations"
                );
                drop(pressure);
                assert_eq!(
                    view.execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW)
                        .await
                        .unwrap(),
                    expected
                );
            }
            for dimension in 0..6 {
                let mut opt = options();
                let mut image = sealing();
                let mut kernel = memory();
                let mut raw = RAW;
                match dimension {
                    0 => raw = 0,
                    1 => image.max_image_bytes = 0,
                    2 => image.max_incidences = 0,
                    3 => opt.source_limits.max_work_units = 0,
                    4 => kernel.max_result_bytes = 0,
                    5 => opt.execution_limits.max_result_rows = 0,
                    _ => unreachable!(),
                }
                assert!(
                    view.execute_fnx_sealed(&cx, &procedure, opt, kernel, image, raw)
                        .await
                        .is_err(),
                    "persisted source dimension {dimension}, stale={stale}"
                );
                assert_eq!(
                    view.execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW)
                        .await
                        .unwrap(),
                    expected
                );
            }
            drop(view);
            assert_eq!(source_pool.used(), 0);

            for stop in 1..=checkpoints {
                let source_pool = pool();
                let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
                view.set_sealed_read_cache_pool(&cx, MemoryPool::new(0, 0).unwrap())
                    .unwrap();
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
                assert!(
                    view.execute_fnx_sealed(
                        &interrupted,
                        &procedure,
                        options(),
                        memory(),
                        sealing(),
                        RAW,
                    )
                    .await
                    .is_err(),
                    "persisted source checkpoint {stop}, stale={stale}"
                );
                assert_eq!(probe.calls(), stop);
                assert_eq!(
                    view.execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW)
                        .await
                        .unwrap(),
                    expected
                );
                drop(view);
                assert_eq!(source_pool.used(), 0, "checkpoint {stop}, stale={stale}");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sealed_label_unions_preserve_any_membership_without_widening_relation_storage() {
    let ((), report) = run_async_under_lab(0x6273_0110, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let mut labels = WriteBatch::new(R);
        labels.set_vertex_label(VId(BASE + 3), L, false);
        labels.set_vertex_label(VId(BASE + 3), LabelId(11), true);
        db.write(&contexts.commit(), labels).await.unwrap();
        let mut view = cold(&contexts, &vfs, pool()).await;
        let procedure = call();
        let mut opt = options();
        opt.selection.vertex_labels = SchemaSelection::any_of(&[LabelId(11), L, L]).unwrap();
        let actual = view
            .execute_fnx_sealed(&cx, &procedure, opt, memory(), sealing(), RAW)
            .await
            .unwrap();
        let triangle = expected(&[BASE + 1, BASE + 2, BASE + 3, u128::MAX], true);
        assert_eq!(actual.analytics.rows, triangle);
        assert_eq!(
            actual,
            db.execute_fnx_sealed(&cx, &procedure, opt, memory(), sealing())
                .await
                .unwrap()
        );
        assert_eq!(
            db.call_fnx(&cx, TRIANGLES, &FnxParameters::new(), opt)
                .unwrap()
                .analytics
                .rows,
            triangle
        );
        let narrower = view
            .execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW)
            .await
            .unwrap();
        assert_eq!(
            narrower.analytics.rows,
            expected(&[BASE + 1, BASE + 2, u128::MAX], false)
        );
        // One physical sealed image represents one relation. A union must
        // refuse before reads instead of silently selecting All or one member.
        opt.selection.relations = SchemaSelection::any_of(&[R, RelationId(8)]).unwrap();
        let before = view.buffer_stats();
        assert!(matches!(
            view.execute_fnx_sealed(&cx, &procedure, opt, memory(), sealing(), RAW)
                .await,
            Err(FnxSealedReadError::Projection(
                SealedProjectionError::RelationRequired
            ))
        ));
        assert_eq!(view.buffer_stats(), before);
        assert!(matches!(
            db.execute_fnx_sealed(&cx, &procedure, opt, memory(), sealing())
                .await,
            Err(FnxSealedReadError::Projection(
                SealedProjectionError::RelationRequired
            ))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compressed_cold_calls_coexist_with_a_writer_and_keep_their_generation_after_reopen() {
    let ((), report) = run_async_under_lab(0x6273_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        // The writer remains alive: another writable open cannot serve this call.
        let source_pool = pool();
        let mut old = cold(&contexts, &vfs, source_pool.clone()).await;
        let old_root = old.partition_root();
        let procedure = call();
        let result = old
            .execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW)
            .await
            .unwrap();
        let triangle = expected(&[BASE + 1, BASE + 2, BASE + 3, u128::MAX], true);
        assert_eq!(result.analytics.rows, triangle);
        assert_eq!(
            result,
            db.execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(),)
                .await
                .unwrap()
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
        let graph = old
            .prism_sealed_projection_at(
                &cx,
                CommitSeq(2),
                options().selection,
                options().projection,
                options().projection_limits,
                options().source_limits,
                sealing(),
                RAW,
            )
            .await
            .unwrap();

        let mut change = WriteBatch::new(R);
        change.set_vertex_label(VId(BASE + 3), L, false);
        change.delete_edge(EId(BASE + 3));
        assert_eq!(db.write(&commit, change).await.unwrap(), CommitSeq(3));
        db.compact(&commit).await.unwrap();
        let current_pool = pool();
        let mut current = cold(&contexts, &vfs, current_pool.clone()).await;
        assert_ne!(current.partition_root(), old_root);
        for cut in [CommitSeq(2), CommitSeq(3)] {
            let mut opt = options();
            opt.as_of = Some(cut);
            let result = current
                .execute_fnx_sealed(&cx, &procedure, opt, memory(), sealing(), RAW)
                .await
                .unwrap();
            assert_eq!(
                result,
                db.execute_fnx_sealed(&cx, &procedure, opt, memory(), sealing(),)
                    .await
                    .unwrap()
            );
            assert_eq!(result.analytics.certificate.snapshot.as_of, cut);
            assert_eq!(
                result.analytics.rows,
                if cut == CommitSeq(2) {
                    triangle.clone()
                } else {
                    expected(&[BASE + 1, BASE + 2, u128::MAX], false)
                }
            );
        }
        drop(db);
        assert_eq!(
            old.execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW,)
                .await
                .unwrap()
                .analytics
                .rows,
            triangle
        );
        drop(old);
        assert_eq!(source_pool.used(), 0);
        // The owned compressed image holds neither the reader nor its source pool.
        assert_eq!(
            procedure
                .execute_sealed(&cx, &graph, options().execution_limits, memory(),)
                .unwrap()
                .rows,
            triangle
        );
        drop(current);
        assert_eq!(current_pool.used(), 0);
        let mut reopened = cold(&contexts, &vfs, pool()).await;
        assert_eq!(
            reopened
                .execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW,)
                .await
                .unwrap()
                .analytics
                .rows,
            expected(&[BASE + 1, BASE + 2, u128::MAX], false)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sealed_preflight_and_future_cuts_refuse_before_faults_and_empty_relations_keep_isolates() {
    let ((), report) = run_async_under_lab(0x6273_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _writer = seed(&contexts, &vfs).await;
        let mut view = cold(&contexts, &vfs, pool()).await;
        let procedure = call();
        let before = view.buffer_stats();
        let mut bad = options();
        bad.projection.directedness = Directedness::Directed;
        assert!(matches!(
            view.execute_fnx_sealed(&cx, &procedure, bad, memory(), sealing(), RAW,)
                .await,
            Err(FnxSealedReadError::Execution(_))
        ));
        bad = options();
        bad.selection.relations = None.into();
        assert!(matches!(
            view.execute_fnx_sealed(&cx, &procedure, bad, memory(), sealing(), RAW,)
                .await,
            Err(FnxSealedReadError::Projection(
                SealedProjectionError::RelationRequired
            ))
        ));
        bad = options();
        bad.as_of = Some(CommitSeq(3));
        assert!(matches!(
            view.execute_fnx_sealed(&cx, &procedure, bad, memory(), sealing(), RAW,)
                .await,
            Err(FnxSealedReadError::Input(FnxReadError::Read(
                BufferedReadError::BeyondPublication { .. }
            )))
        ));
        assert_eq!(view.buffer_stats(), before);
        let mut empty = options();
        empty.selection.relations = Some(RelationId(99)).into();
        let result = view
            .execute_fnx_sealed(&cx, &procedure, empty, memory(), sealing(), 0)
            .await
            .unwrap();
        assert_eq!(
            result.analytics.rows,
            expected(&[BASE + 1, BASE + 2, BASE + 3, u128::MAX], false)
        );
        let mut origin = options();
        origin.as_of = Some(CommitSeq::ORIGIN);
        assert!(
            view.execute_fnx_sealed(&cx, &procedure, origin, memory(), sealing(), RAW,)
                .await
                .unwrap()
                .analytics
                .rows
                .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_image_projection_kernel_and_delivery_limits_all_remain_active() {
    let ((), report) = run_async_under_lab(0x6273_0103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let writer = seed(&contexts, &vfs).await;
        let source_pool = pool();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        let procedure = call();
        for dimension in 0..10 {
            let mut opt = options();
            let mut image = sealing();
            let mut kernel = memory();
            let mut raw = RAW;
            match dimension {
                0 => opt.source_limits.max_work_units = 0,
                1 => opt.source_limits.max_scratch_entries = 0,
                2 => opt.source_limits.max_staging_bytes = 0,
                3 => raw = 0,
                4 => image.max_image_bytes = 0,
                5 => image.max_incidences = 0,
                6 => opt.projection_limits.max_vertices = 0,
                7 => kernel.max_kernel_workspace_bytes = 0,
                8 => kernel.max_result_bytes = 0,
                9 => opt.execution_limits.max_result_rows = 0,
                _ => unreachable!(),
            }
            assert!(
                view.execute_fnx_sealed(&cx, &procedure, opt, kernel, image, raw,)
                    .await
                    .is_err(),
                "dimension {dimension}"
            );
            // Same view after each refusal: a blanket or poisoned-reader error
            // cannot satisfy these limits tests.
            assert_eq!(
                view.execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW,)
                    .await
                    .unwrap()
                    .analytics
                    .rows,
                expected(&[BASE + 1, BASE + 2, BASE + 3, u128::MAX], true)
            );
        }
        // Raw history admission is separate from the compacted image ceiling.
        assert!(matches!(
            view.execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), 1,)
                .await,
            Err(FnxSealedReadError::Seal(SealedError::Limit { .. }))
        ));
        assert_eq!(writer.frontier().unwrap(), CommitSeq(2));
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_reached_checkpoint_can_cancel_without_losing_the_reader_or_source_reservations() {
    let ((), report) = run_async_under_lab(0x6273_0104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let writer = seed(&contexts, &vfs).await;
        let procedure = call();
        let mut measured = cold(&contexts, &vfs, pool()).await;
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
        let expected = measured
            .execute_fnx_sealed(&observed, &procedure, options(), memory(), sealing(), RAW)
            .await
            .unwrap();
        let checkpoints = probe.calls();
        assert!(checkpoints > 1);
        drop(measured);
        for stop in 1..=checkpoints {
            let source_pool = pool();
            let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
            let control = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&control));
            assert!(
                view.execute_fnx_sealed(
                    &interrupted,
                    &procedure,
                    options(),
                    memory(),
                    sealing(),
                    RAW,
                )
                .await
                .is_err(),
                "checkpoint {stop}"
            );
            assert_eq!(control.calls(), stop);
            assert_eq!(
                view.execute_fnx_sealed(&cx, &procedure, options(), memory(), sealing(), RAW,)
                    .await
                    .unwrap(),
                expected
            );
            drop(view);
            assert_eq!(source_pool.used(), 0, "checkpoint {stop}");
        }
        assert_eq!(writer.frontier().unwrap(), CommitSeq(2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn same_answer(actual: &FnxReadResult, expected: &FnxReadResult) {
    assert_eq!(actual.selection, expected.selection);
    assert_eq!(actual.analytics.columns, expected.analytics.columns);
    assert_eq!(actual.analytics.rows, expected.analytics.rows);
    let actual = &actual.analytics.certificate;
    let expected = &expected.analytics.certificate;
    assert_eq!(actual.result_digest, expected.result_digest);
    assert_eq!(actual.projection_digest, expected.projection_digest);
    assert_eq!(actual.snapshot, expected.snapshot);
    assert_eq!(actual.estimated_work, expected.estimated_work);
    assert_eq!(
        actual.kernel_workspace_bytes,
        expected.kernel_workspace_bytes
    );
}

#[test]
fn cooperative_buffered_calls_preserve_historical_answers_across_quanta() {
    // The existing cooperative kernel future is non-Send. Its registered
    // current-thread driver executes real yield futures without an unsafe
    // Send shim or the unregistered-root driver's self-wake backoff.
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut writer = seed(&contexts, &vfs).await;
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(BASE + 3));
        writer.write(&contexts.commit(), change).await.unwrap();
        writer.compact(&contexts.commit()).await.unwrap();
        let mut view = cold(&contexts, &vfs, pool()).await;
        for text in [
            TRIANGLES,
            "CALL fnx.betweenness_centrality() YIELD vertex,score",
            "CALL fnx.load_centrality() YIELD vertex,score",
            "CALL fnx.average_neighbor_degree() YIELD vertex,score",
            "CALL fnx.label_propagation_communities() YIELD vertex,component",
            "CALL fnx.louvain_communities() YIELD vertex,component",
            "CALL fnx.pagerank(iterations => 4, weighted => false) YIELD vertex,score",
            "CALL fnx.in_degree_centrality() YIELD vertex,score",
            "CALL fnx.out_degree_centrality() YIELD vertex,score",
            "CALL fnx.cdlp(max_iter => 0) YIELD vertex,component",
            "CALL fnx.cdlp(max_iter => 1) YIELD vertex,component",
            "CALL fnx.cdlp(max_iter => 2) YIELD vertex,component",
            "CALL fnx.cdlp(max_iter => 5) YIELD vertex,component",
        ] {
            for cut in [CommitSeq(2), CommitSeq(3)] {
                let mut opt = options();
                let directed_degree = text.contains("degree_centrality");
                if directed_degree {
                    opt.projection.directedness = Directedness::Directed;
                }
                opt.as_of = Some(cut);
                let procedure = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
                let synchronous = view
                    .execute_fnx_sealed(&cx, &procedure, opt, memory(), sealing(), RAW)
                    .await
                    .unwrap();
                let decoded = writer.execute_fnx(&cx, &procedure, opt).unwrap();
                assert_eq!(synchronous.analytics.rows, decoded.analytics.rows);
                if let fgdb_prism::FnxAlgorithm::Cdlp { max_iter } = procedure.algorithm() {
                    // Independent labels for the triangle (cut 2), then the
                    // path after deleting its closing edge (cut 3). Each round
                    // reads the preceding round; the isolated vertex keeps
                    // its own full-width identity.
                    let labels = if max_iter == 0 {
                        [1, 2, 3]
                    } else if cut == CommitSeq(2) {
                        if max_iter == 1 { [2, 1, 1] } else { [1, 1, 1] }
                    } else if max_iter % 2 == 1 {
                        [2, 1, 2]
                    } else {
                        [1, 2, 1]
                    };
                    let expected: Vec<_> = [BASE + 1, BASE + 2, BASE + 3, u128::MAX]
                        .into_iter()
                        .zip(labels.map(|id| BASE + id).into_iter().chain([u128::MAX]))
                        .map(|(vertex, component)| {
                            vec![
                                FnxValue::Vertex(VId(vertex)),
                                FnxValue::Vertex(VId(component)),
                            ]
                        })
                        .collect();
                    assert_eq!(synchronous.analytics.rows, expected);
                }
                if directed_degree {
                    let incoming = text.contains("in_degree");
                    let counts = if cut == CommitSeq(2) {
                        [1, 1, 1, 0]
                    } else if incoming {
                        [0, 1, 1, 0]
                    } else {
                        [1, 1, 0, 0]
                    };
                    let expected: Vec<_> = [BASE + 1, BASE + 2, BASE + 3, u128::MAX]
                        .into_iter()
                        .zip(counts)
                        .map(|(id, count)| {
                            vec![
                                FnxValue::Vertex(VId(id)),
                                FnxValue::Score(count as f64 / 3.0),
                            ]
                        })
                        .collect();
                    assert_eq!(synchronous.analytics.rows, expected);
                    // Four scores are the entire kernel allocation when the
                    // projection retains its optional incoming-degree table.
                    // Outgoing degree uses the mandatory row directory.
                    let bytes = 4 * std::mem::size_of::<f64>();
                    assert_eq!(synchronous.analytics.certificate.kernel_workspace_bytes, bytes);
                    let exact = FnxMemoryLimits { max_kernel_workspace_bytes: bytes, ..memory() };
                    let bounded = view.execute_fnx_sealed(&cx, &procedure, opt, exact, sealing(), RAW).await.unwrap();
                    assert_eq!(bounded.analytics.rows, expected);
                    let below = FnxMemoryLimits { max_kernel_workspace_bytes: bytes - 1, ..memory() };
                    assert!(matches!(
                        view.execute_fnx_sealed(&cx, &procedure, opt, below, sealing(), RAW).await,
                        Err(FnxSealedReadError::Execution(FnxSealedExecutionError::Execution(
                            fgdb_prism::FnxExecutionError::LimitExceeded { resource: "kernel workspace bytes", requested, limit }
                        ))) if requested == bytes && limit == bytes - 1
                    ));
                    if incoming {
                        let fallback_bytes =
                            4 * (std::mem::size_of::<f64>() + std::mem::size_of::<usize>());
                        // The original 64/63-byte allowances both admit the
                        // cached path; its exact boundary is now 32/31.
                        for admitted in [fallback_bytes, fallback_bytes - 1] {
                            let answer = view.execute_fnx_sealed(
                                &cx, &procedure, opt,
                                FnxMemoryLimits { max_kernel_workspace_bytes: admitted, ..memory() },
                                sealing(), RAW,
                            ).await.unwrap();
                            assert_eq!(answer.analytics.rows, expected);
                        }
                        // Leave exactly the mandatory four identities and
                        // outgoing degrees in the projection budget. There
                        // is no spare backing for the optional incoming table.
                        let mut fallback_options = opt;
                        fallback_options.projection_limits.max_workspace_bytes =
                            4 * (std::mem::size_of::<VId>() + std::mem::size_of::<usize>());
                        let mut fallback = cold(&contexts, &vfs, pool()).await;
                        fallback.set_sealed_read_cache_pool(
                            &cx, MemoryPool::new(0, 0).unwrap(),
                        ).unwrap();
                        let exact = FnxMemoryLimits {
                            max_kernel_workspace_bytes: fallback_bytes, ..memory()
                        };
                        let answer = fallback.execute_fnx_sealed(
                            &cx, &procedure, fallback_options, exact, sealing(), RAW,
                        ).await.unwrap();
                        assert_eq!(answer.analytics.rows, expected);
                        assert_eq!(answer.analytics.certificate.kernel_workspace_bytes, fallback_bytes);
                        let below = FnxMemoryLimits {
                            max_kernel_workspace_bytes: fallback_bytes - 1, ..memory()
                        };
                        assert!(matches!(
                            fallback.execute_fnx_sealed(
                                &cx, &procedure, fallback_options, below, sealing(), RAW,
                            ).await,
                            Err(FnxSealedReadError::Execution(FnxSealedExecutionError::Execution(
                                fgdb_prism::FnxExecutionError::LimitExceeded {
                                    resource: "kernel workspace bytes", requested, limit
                                }
                            ))) if requested == fallback_bytes && limit == fallback_bytes - 1
                        ));
                        for q in [1, 7, 257] {
                            let answer = fallback.execute_fnx_sealed_cooperative(
                                &cx, &procedure, fallback_options, exact, sealing(), RAW,
                                NonZeroUsize::new(q).unwrap(),
                            ).await.unwrap();
                            assert_eq!(answer.analytics.rows, expected);
                            assert_eq!(answer.analytics.certificate.kernel_workspace_bytes, fallback_bytes);
                            assert!(matches!(
                                fallback.execute_fnx_sealed_cooperative(
                                    &cx, &procedure, fallback_options, below, sealing(), RAW,
                                    NonZeroUsize::new(q).unwrap(),
                                ).await,
                                Err(FnxSealedReadError::Execution(FnxSealedExecutionError::Execution(
                                    fgdb_prism::FnxExecutionError::LimitExceeded {
                                        resource: "kernel workspace bytes", requested, limit
                                    }
                                ))) if requested == fallback_bytes && limit == fallback_bytes - 1
                            ));
                        }
                    }
                }
                assert_eq!(
                    synchronous.analytics.certificate.result_digest,
                    decoded.analytics.certificate.result_digest
                );
                if text == TRIANGLES {
                    assert_eq!(
                        synchronous.analytics.rows,
                        expected(
                            &[BASE + 1, BASE + 2, BASE + 3, u128::MAX],
                            cut == CommitSeq(2)
                        )
                    );
                }
                for q in [1, 7, 257] {
                    let parameters = FnxParameters::new();
                    let mut future = Box::pin(view.call_fnx_sealed_cooperative(
                        &cx,
                        text,
                        &parameters,
                        opt,
                        memory(),
                        sealing(),
                        RAW,
                        NonZeroUsize::new(q).unwrap(),
                    ));
                    let mut pending = 0;
                    let actual = std::future::poll_fn(|task| {
                        let result = future.as_mut().poll(task);
                        if result.is_pending() {
                            pending += 1;
                        }
                        result
                    })
                    .await
                    .unwrap();
                    drop(future);
                    same_answer(&actual, &synchronous);
                    if directed_degree {
                        let bytes = 4 * std::mem::size_of::<f64>();
                        let below = FnxMemoryLimits { max_kernel_workspace_bytes: bytes - 1, ..memory() };
                        assert!(matches!(
                            view.execute_fnx_sealed_cooperative(&cx, &procedure, opt, below, sealing(), RAW, NonZeroUsize::new(q).unwrap()).await,
                            Err(FnxSealedReadError::Execution(FnxSealedExecutionError::Execution(
                                fgdb_prism::FnxExecutionError::LimitExceeded { resource: "kernel workspace bytes", requested, limit }
                            ))) if requested == bytes && limit == bytes - 1
                        ));
                    }
                    assert_ne!(
                        actual.analytics.certificate.execution_kernel,
                        synchronous.analytics.certificate.execution_kernel
                    );
                    if q == 1 {
                        assert!(pending > 0, "the actual future must yield: {text}");
                    }
                }
            }
        }
        assert_eq!(writer.frontier().unwrap(), CommitSeq(3));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn cooperative_buffered_preflight_and_limits_remain_active_after_a_warm_build() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _writer = seed(&contexts, &vfs).await;
        let mut view = cold(&contexts, &vfs, pool()).await;
        let q = NonZeroUsize::new(1).unwrap();
        let before = view.buffer_stats();
        // Every currently registered algorithm has a cooperative kernel.
        // Malformed arguments still refuse before any source access.
        assert!(matches!(
            view.call_fnx_sealed_cooperative(
                &cx,
                "CALL fnx.cdlp(max_iter => -1) YIELD vertex,component",
                &FnxParameters::new(),
                options(),
                memory(),
                sealing(),
                RAW,
                q,
            )
            .await,
            Err(FnxSealedReadError::Input(FnxReadError::Bind(_)))
        ));
        let mut directed = options();
        directed.projection.directedness = Directedness::Directed;
        assert!(
            view.execute_fnx_sealed_cooperative(
                &cx,
                &call(),
                directed,
                memory(),
                sealing(),
                RAW,
                q,
            )
            .await
            .is_err()
        );
        let mut future = options();
        future.as_of = Some(CommitSeq(3));
        assert!(
            view.execute_fnx_sealed_cooperative(&cx, &call(), future, memory(), sealing(), RAW, q,)
                .await
                .is_err()
        );
        assert_eq!(view.buffer_stats(), before);

        let procedure = call();
        let good = view
            .execute_fnx_sealed_cooperative(&cx, &procedure, options(), memory(), sealing(), RAW, q)
            .await
            .unwrap();
        for dimension in 0..10 {
            let mut opt = options();
            let mut image = sealing();
            let mut kernel = memory();
            let mut raw = RAW;
            match dimension {
                0 => opt.source_limits.max_work_units = 0,
                1 => opt.source_limits.max_scratch_entries = 0,
                2 => opt.source_limits.max_staging_bytes = 0,
                3 => raw = 0,
                4 => image.max_image_bytes = 0,
                5 => image.max_incidences = 0,
                6 => opt.projection_limits.max_vertices = 0,
                7 => kernel.max_kernel_workspace_bytes = 0,
                8 => kernel.max_result_bytes = 0,
                9 => opt.execution_limits.max_result_rows = 0,
                _ => unreachable!(),
            }
            assert!(
                view.execute_fnx_sealed_cooperative(&cx, &procedure, opt, kernel, image, raw, q,)
                    .await
                    .is_err(),
                "dimension {dimension}"
            );
            assert_eq!(
                view.execute_fnx_sealed_cooperative(
                    &cx,
                    &procedure,
                    options(),
                    memory(),
                    sealing(),
                    RAW,
                    q,
                )
                .await
                .unwrap(),
                good
            );
        }
        let mut empty = options();
        empty.selection.relations = Some(RelationId(99)).into();
        assert_eq!(
            view.execute_fnx_sealed_cooperative(&cx, &procedure, empty, memory(), sealing(), 0, q,)
                .await
                .unwrap()
                .analytics
                .rows,
            expected(&[BASE + 1, BASE + 2, BASE + 3, u128::MAX], false)
        );
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn cooperative_buffered_cancellation_and_every_warm_suspend_release_the_reader() {
    for (text, direction) in [
        (
            "CALL fnx.betweenness_centrality() YIELD vertex,score",
            Directedness::Undirected,
        ),
        (
            "CALL fnx.load_centrality() YIELD vertex,score",
            Directedness::Undirected,
        ),
        (
            "CALL fnx.average_neighbor_degree() YIELD vertex,score",
            Directedness::Undirected,
        ),
        (
            "CALL fnx.label_propagation_communities() YIELD vertex,component",
            Directedness::Undirected,
        ),
        (
            "CALL fnx.louvain_communities() YIELD vertex,component",
            Directedness::Undirected,
        ),
        (
            "CALL fnx.in_degree_centrality() YIELD vertex,score",
            Directedness::Directed,
        ),
        (
            "CALL fnx.out_degree_centrality() YIELD vertex,score",
            Directedness::Directed,
        ),
    ] {
        let runtime = RuntimeBuilder::current_thread().build().unwrap();
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        runtime.block_on(async {
            let cx = contexts.query();
            let vfs = MemVfs::new().unwrap();
            let writer = seed(&contexts, &vfs).await;
            let source_pool = pool();
            let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
            let procedure = FnxCallSpec::bind(text, &FnxParameters::new()).unwrap();
            let mut opt = options();
            opt.projection.directedness = direction;
            let q = NonZeroUsize::new(1).unwrap();
            let expected = view
                .execute_fnx_sealed_cooperative(&cx, &procedure, opt, memory(), sealing(), RAW, q)
                .await
                .unwrap();
            let source_bytes = source_pool.used();
            let observe = Arc::new(SimulationCheckpointProbe::new(None));
            let measured = cx.with_checkpoint_probe(Arc::clone(&observe));
            let mut pending = 0;
            {
                let mut future = Box::pin(view.execute_fnx_sealed_cooperative(
                    &measured,
                    &procedure,
                    opt,
                    memory(),
                    sealing(),
                    RAW,
                    q,
                ));
                let actual = std::future::poll_fn(|task| {
                    let result = future.as_mut().poll(task);
                    if result.is_pending() {
                        pending += 1;
                    }
                    result
                })
                .await
                .unwrap();
                assert_eq!(actual, expected);
            }
            assert!(pending > 0);
            for stop in 1..=observe.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let cancelled = cx.with_checkpoint_probe(Arc::clone(&probe));
                assert!(
                    view.execute_fnx_sealed_cooperative(
                        &cancelled,
                        &procedure,
                        opt,
                        memory(),
                        sealing(),
                        RAW,
                        q,
                    )
                    .await
                    .is_err(),
                    "checkpoint {stop}"
                );
                assert_eq!(probe.calls(), stop);
                assert_eq!(source_pool.used(), source_bytes);
            }
            // Preparation is already cached. These futures have no source I/O;
            // each Pending is the real cooperative kernel's yield primitive.
            for stop in 0..=pending {
                let mut task = Context::from_waker(Waker::noop());
                let mut future = Box::pin(view.execute_fnx_sealed_cooperative(
                    &cx,
                    &procedure,
                    opt,
                    memory(),
                    sealing(),
                    RAW,
                    q,
                ));
                for _ in 0..stop {
                    assert!(matches!(future.as_mut().poll(&mut task), Poll::Pending));
                }
                drop(future);
                assert_eq!(source_pool.used(), source_bytes);
                assert_eq!(
                    view.execute_fnx_sealed_cooperative(
                        &cx,
                        &procedure,
                        opt,
                        memory(),
                        sealing(),
                        RAW,
                        q,
                    )
                    .await
                    .unwrap(),
                    expected
                );
            }
            drop(view);
            assert_eq!(source_pool.used(), 0);
            assert_eq!(writer.frontier().unwrap(), CommitSeq(2));
            assert_eq!(contexts.outstanding_obligations(), 0);
        });
    }
}
