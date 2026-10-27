//! The public cold reader feeds the real sealed image and native kernel.
//! Hand-enumerated triangle counts guard the resident comparison against a
//! shared wrong answer. Full-width identities and an isolate are deliberate.
#![recursion_limit = "256"]

use asupersync::lab::run_async_under_lab;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{
    BufferLimits, BufferedReadError, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys,
    MemVfs, MemoryPool, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadError, FnxReadOptions,
    FnxReadResult, FnxSealedExecutionError, FnxSealedReadError, FnxValue, ParallelEdgePolicy,
    SealedProjectionError, SelfLoopPolicy,
};
use fgdb_strata::tiered::sealed::{SealedError, SealedLimits};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
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
    options.selection.vertex_label = Some(L);
    options.selection.relation = Some(R);
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
        bad.selection.relation = None;
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
        empty.selection.relation = Some(RelationId(99));
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
            "CALL fnx.pagerank(iterations => 4, weighted => false) YIELD vertex,score",
            "CALL fnx.in_degree_centrality() YIELD vertex,score",
            "CALL fnx.out_degree_centrality() YIELD vertex,score",
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
                    let bytes = 4 * (std::mem::size_of::<f64>() + if incoming { std::mem::size_of::<usize>() } else { 0 });
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
                        let bytes = synchronous.analytics.certificate.kernel_workspace_bytes;
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
        let unsupported = FnxCallSpec::bind(
            "CALL fnx.cdlp(max_iter => 2) YIELD vertex,component",
            &FnxParameters::new(),
        )
        .unwrap();
        assert!(matches!(
            view.execute_fnx_sealed_cooperative(
                &cx,
                &unsupported,
                options(),
                memory(),
                sealing(),
                RAW,
                q,
            )
            .await,
            Err(FnxSealedReadError::Execution(
                FnxSealedExecutionError::UnsupportedCooperativeAlgorithm(_)
            ))
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
        empty.selection.relation = Some(RelationId(99));
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
