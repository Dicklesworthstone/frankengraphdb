//! Cold construction, actual suspend boundaries and publication through the
//! public owners. The Strata image comes from the real committed source walk.

use super::*;
use asupersync::runtime::yield_now::yield_now;
use fgdb_prism::{SealedProjectionError, SealedProjectionMask, SealedProjectionSpec};
use fgdb_strata::tiered::sealed::{SealedError, SealedPartition};
use std::cell::Cell;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

fn quantum(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn config(options: FnxReadOptions, cut: CommitSeq) -> SealedProjectionSpec {
    SealedProjectionSpec {
        as_of: cut,
        selection: options.selection,
        projection: options.projection,
    }
}

async fn source(
    contexts: &PurposeContexts,
    vfs: &MemVfs,
    long_hidden_group: bool,
) -> (Database<MemVfs>, SealedPartition) {
    let mut database = seed(&contexts.commit(), vfs).await;
    if long_hidden_group {
        let mut batch = WriteBatch::new(R);
        for id in 1000..1257 {
            batch.add_edge(
                EId(id),
                VId(1),
                VId(9),
                vec![(P, CanonicalScalar::Bool(true))],
            );
        }
        database.write(&contexts.commit(), batch).await.unwrap();
    }
    let store = BlockStore::open_existing_for_read_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        [0x72; 32],
        NS,
    )
    .await
    .unwrap();
    let image = store
        .seal_relation(
            &contexts.query(),
            database.read_session().unwrap().partition_root(),
            CommitSeq(1),
            R,
            SealedLimits::default(),
        )
        .await
        .unwrap();
    (database, image)
}

#[derive(Default)]
struct Wakes(AtomicUsize);
impl Wake for Wakes {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// Only the already-admitted projection constructor is polled here: no I/O
// future is busy-polled. Its actual runtime yield must return Pending and wake.
fn drive<F: Future>(future: F, probe: &SimulationCheckpointProbe, q: usize) -> (F::Output, usize) {
    let wakes = Arc::new(Wakes::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut task = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    let mut pending = 0;
    loop {
        let before = probe.calls();
        let result = future.as_mut().poll(&mut task);
        assert!(
            probe.calls() - before <= q.saturating_mul(32).saturating_add(128),
            "a poll performed an unbounded checkpointed source walk",
        );
        match result {
            Poll::Ready(result) => {
                assert_eq!(wakes.0.load(Ordering::Relaxed), pending);
                return (result, pending);
            }
            Poll::Pending => {
                pending += 1;
                assert_eq!(wakes.0.load(Ordering::Relaxed), pending);
                assert!(pending < 1_000_000, "resuming must advance construction");
            }
        }
    }
}

fn rows(graph: &SealedGraphView, cx: &QueryCx) -> Vec<Vec<(VId, u64)>> {
    (0..graph.node_count())
        .map(|source| {
            let mut cursor = graph.neighbor_cursor(cx, source).unwrap();
            let mut values = Vec::new();
            while let Some((target, weight)) = cursor.next(cx).unwrap() {
                values.push((graph.vertex_id(target).unwrap(), weight.to_bits()));
            }
            values
        })
        .collect()
}

fn same_graph(actual: &SealedGraphView, expected: &SealedGraphView, cx: &QueryCx) {
    assert_eq!(actual.binding(), expected.binding());
    assert_eq!(actual.digest(), expected.digest());
    assert_eq!(actual.vertex_ids(), expected.vertex_ids());
    assert_eq!(actual.edge_count(), expected.edge_count());
    assert_eq!(actual.input_edge_count(), expected.input_edge_count());
    assert_eq!(
        actual.adjacency_entry_count(),
        expected.adjacency_entry_count()
    );
    assert_eq!(
        actual.scan_incidence_bound(),
        expected.scan_incidence_bound()
    );
    assert_eq!(
        actual.incoming_index_stats(),
        expected.incoming_index_stats()
    );
    assert_eq!(
        actual.charged_workspace_bytes(),
        expected.charged_workspace_bytes()
    );
    assert_eq!(
        actual.charged_resident_bytes(),
        expected.charged_resident_bytes()
    );
    assert_eq!(rows(actual, cx), rows(expected, cx));
}

async fn counted<F: Future>(future: F) -> (F::Output, usize) {
    let mut future = std::pin::pin!(future);
    let mut pending = 0;
    let result = std::future::poll_fn(|task| {
        let result = future.as_mut().poll(task);
        if result.is_pending() {
            pending += 1;
        }
        result
    })
    .await;
    (result, pending)
}

async fn prepare_native(
    database: &Database<MemVfs>,
    query: &QueryCx,
    opt: FnxReadOptions,
) -> Result<SealedGraphView, FnxSealedReadError<fgdb::ReadError, Box<asupersync::error::Error>>> {
    database
        .prism_sealed_projection_cooperative_at(
            query,
            None,
            opt.selection,
            opt.projection,
            opt.projection_limits,
            opt.source_limits,
            SealedLimits::default(),
            RootReadLimits::default(),
            quantum(3),
        )
        .await
}

async fn drop_at<F: Future>(future: F, stop: usize) {
    let mut future = std::pin::pin!(future);
    let mut pending = 0;
    std::future::poll_fn(|task| {
        if pending == stop {
            return Poll::Ready(());
        }
        match future.as_mut().poll(task) {
            Poll::Ready(_) => panic!("the requested suspension must precede publication"),
            Poll::Pending => {
                pending += 1;
                if pending == stop {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }
    })
    .await;
    assert_eq!(pending, stop);
}

#[test]
fn cooperative_construction_preserves_history_reductions_masks_and_projection_identity() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (database, image) = source(&contexts, &vfs, true).await;
        let view = database.read_session().unwrap();
        let vertices = [VId(1), VId(2), VId(3), VId(u128::MAX)];
        for cut in [CommitSeq(1), view.frontier()] {
            for direction in [
                Directedness::Directed,
                Directedness::Reversed,
                Directedness::Undirected,
            ] {
                for reduction in [
                    ParallelEdgePolicy::Sum,
                    ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum,
                    ParallelEdgePolicy::CollapseUnit,
                ] {
                    let mut opt = options();
                    opt.projection.directedness = direction;
                    opt.projection.parallel_edges = reduction;
                    opt.projection.self_loops = SelfLoopPolicy::Keep;
                    opt.selection.weight = FnxWeightSpec::Property {
                        key: P,
                        missing: MissingWeightPolicy::Reject,
                    };
                    let config = config(opt, cut);
                    let immediate = SealedGraphView::build(
                        &cx,
                        &image,
                        &vertices,
                        config,
                        opt.projection_limits,
                    )
                    .unwrap();
                    let decoded = view
                        .prism_projection_at(
                            &cx,
                            cut,
                            opt.selection,
                            opt.projection,
                            opt.projection_limits,
                            opt.source_limits,
                        )
                        .unwrap();
                    let oracle: Vec<Vec<_>> = (0..vertices.len())
                        .map(|source| {
                            let (targets, weights) = decoded.projected_row(source).unwrap();
                            targets
                                .iter()
                                .zip(weights)
                                .map(|(&target, &weight)| {
                                    (decoded.vertex_id(target).unwrap(), weight.to_bits())
                                })
                                .collect()
                        })
                        .collect();
                    assert_eq!(rows(&immediate, &cx), oracle);
                    for q in [1, 7, 256] {
                        let probe = Arc::new(SimulationCheckpointProbe::new(None));
                        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                        let (result, pending) = drive(
                            SealedGraphView::build_cooperative(
                                &controlled,
                                &image,
                                &vertices,
                                config,
                                opt.projection_limits,
                                quantum(q),
                                yield_now,
                            ),
                            &probe,
                            q,
                        );
                        let actual = result.unwrap();
                        same_graph(&actual, &immediate, &cx);
                        assert!(actual.shares_storage_with(&image));
                        if q == 1 {
                            assert!(pending > 257, "hidden rows must consume suspension fuel");
                        }
                    }
                }
            }
        }
        for mask in [
            SealedProjectionMask {
                relation_visible: false,
                weight_property_visible: false,
            },
            SealedProjectionMask {
                relation_visible: true,
                weight_property_visible: false,
            },
        ] {
            let mut opt = options();
            opt.selection.weight = FnxWeightSpec::Property {
                key: P,
                missing: MissingWeightPolicy::Unit,
            };
            opt.projection.parallel_edges = ParallelEdgePolicy::Sum;
            let config = config(opt, view.frontier());
            let immediate = SealedGraphView::build_with_checkpoint(
                &cx,
                &image,
                &vertices,
                config,
                mask,
                opt.projection_limits,
                || Ok(()),
            )
            .unwrap();
            let actual = SealedGraphView::build_cooperative_with_checkpoint(
                &cx,
                &image,
                &vertices,
                config,
                mask,
                opt.projection_limits,
                quantum(1),
                yield_now,
                || Ok(()),
            )
            .await
            .unwrap();
            same_graph(&actual, &immediate, &cx);
            if !mask.relation_visible {
                assert_eq!(rows(&actual, &cx), vec![Vec::new(); vertices.len()]);
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn construction_cancellation_live_guards_limits_and_every_drop_preserve_the_source() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (database, image) = source(&contexts, &vfs, false).await;
        let anchor = image.anchor();
        let bytes = image.encode(&cx, SealedLimits::default()).unwrap();
        let vertices = [VId(1), VId(2), VId(3), VId(u128::MAX)];
        let opt = options();
        let config = config(opt, database.frontier().unwrap());
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (expected, suspensions) = drive(SealedGraphView::build_cooperative(
            &controlled, &image, &vertices, config, opt.projection_limits, quantum(3), yield_now,
        ), &probe, 3);
        let expected = expected.unwrap();
        assert!(suspensions > vertices.len());
        for stop in 1..=probe.calls() {
            let cancel = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&cancel));
            assert!(matches!(SealedGraphView::build_cooperative(
                &controlled, &image, &vertices, config, opt.projection_limits, quantum(3), yield_now,
            ).await, Err(SealedProjectionError::Read(SealedError::Interrupted(_)))));
            assert_eq!(cancel.calls(), stop);
        }
        let guards = Cell::new(0usize);
        SealedGraphView::build_cooperative_with_checkpoint(
            &cx, &image, &vertices, config, SealedProjectionMask::UNMASKED, opt.projection_limits,
            quantum(3), yield_now, || { guards.set(guards.get() + 1); Ok(()) },
        ).await.unwrap();
        for stop in 1..=guards.get() {
            let seen = Cell::new(0usize);
            let error = SealedGraphView::build_cooperative_with_checkpoint(
                &cx, &image, &vertices, config, SealedProjectionMask::UNMASKED, opt.projection_limits,
                quantum(3), yield_now, || {
                    seen.set(seen.get() + 1);
                    if seen.get() == stop {
                        Err(SealedProjectionError::Guard(Box::new(std::io::Error::other(stop.to_string()))))
                    } else { Ok(()) }
                },
            ).await.unwrap_err();
            assert!(matches!(error, SealedProjectionError::Guard(error) if error.to_string() == stop.to_string()));
            assert_eq!(seen.get(), stop);
        }
        for stop in 0..=suspensions {
            let mut future = Box::pin(SealedGraphView::build_cooperative(
                &cx, &image, &vertices, config, opt.projection_limits, quantum(3), yield_now,
            ));
            let mut task = Context::from_waker(Waker::noop());
            for _ in 0..stop { assert!(future.as_mut().poll(&mut task).is_pending()); }
            drop(future);
            assert_eq!(image.anchor(), anchor);
            assert_eq!(contexts.outstanding_obligations(), 0);
        }
        for dimension in 0..4 {
            let mut limits = opt.projection_limits;
            match dimension {
                0 => limits.max_vertices = 0,
                1 => limits.max_input_edges = 0,
                2 => limits.max_adjacency_entries = 0,
                3 => limits.max_workspace_bytes = 0,
                _ => unreachable!(),
            }
            let immediate = SealedGraphView::build(&cx, &image, &vertices, config, limits).unwrap_err();
            let cooperative = SealedGraphView::build_cooperative(
                &cx, &image, &vertices, config, limits, quantum(1), yield_now,
            ).await.unwrap_err();
            assert_eq!(format!("{cooperative:?}"), format!("{immediate:?}"));
        }
        let mut wrong = config;
        wrong.selection.relation = Some(RelationId(99));
        assert!(matches!(SealedGraphView::build_cooperative_with_checkpoint(
            &cx, &image, &[], wrong,
            SealedProjectionMask { relation_visible: false, weight_property_visible: false },
            opt.projection_limits, quantum(1), yield_now, || Ok(()),
        ).await, Err(SealedProjectionError::Read(SealedError::RelationOutsideAnchor { .. }))));
        let cancelled = cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
        let guards = Cell::new(0);
        assert!(SealedGraphView::build_cooperative_with_checkpoint(
            &cancelled, &image, &vertices, config, SealedProjectionMask::UNMASKED,
            opt.projection_limits, quantum(1), yield_now,
            || { guards.set(guards.get() + 1); Ok(()) },
        ).await.is_err());
        assert_eq!(guards.get(), 0, "original QueryCx is checked before the caller guard");
        assert_eq!(image.encode(&cx, SealedLimits::default()).unwrap(), bytes);
        same_graph(&SealedGraphView::build(&cx, &image, &vertices, config, opt.projection_limits).unwrap(), &expected, &cx);
    });
}

#[test]
fn public_database_and_read_only_open_use_cooperative_builds_and_share_completed_cache_entries() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let database = seed(&contexts.commit(), &vfs).await;
        let opt = options();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (first, pending) = counted(prepare_native(&database, &controlled, opt)).await;
        let first = first.unwrap();
        assert!(
            pending > 10,
            "the cold public constructor must suspend inside preparation"
        );
        assert!(first.shares_cache_with(&project(&database, &cx, opt).await));
        assert!(first.shares_cache_with(&prepare_native(&database, &cx, opt).await.unwrap()));
        assert_eq!(
            database
                .read_cache_status(&cx)
                .unwrap()
                .sealed_projections
                .entries,
            1
        );
        assert_eq!(
            call(TRIANGLES)
                .execute_sealed(&cx, &first, opt.execution_limits, memory())
                .unwrap()
                .rows,
            expected()
        );
        for stop in 0..=pending {
            cache_limit(&database, &cx, 0);
            cache_limit(&database, &cx, 1 << 26);
            drop_at(prepare_native(&database, &cx, opt), stop).await;
            let status = database.read_cache_status(&cx).unwrap().sealed_projections;
            assert_eq!(status.entries, 0);
            assert_eq!(status.retained_bytes, 0);
            assert_eq!(contexts.outstanding_obligations(), 0);
        }
        for stop in [1, probe.calls() / 2, probe.calls() - 1, probe.calls()] {
            let cancellation = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let cancelled = cx.with_checkpoint_probe(Arc::clone(&cancellation));
            assert!(prepare_native(&database, &cancelled, opt).await.is_err());
            assert_eq!(cancellation.calls(), stop);
            assert_eq!(
                database
                    .read_cache_status(&cx)
                    .unwrap()
                    .sealed_projections
                    .entries,
                0
            );
        }
        let read_only = Database::open_prism_sealed_projection_cooperative_with_vfs(
            &contexts.commit(),
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            opt,
            SealedLimits::default(),
            RootReadLimits::default(),
            quantum(1),
        )
        .await
        .unwrap();
        same_graph(&read_only, &first, &cx);
        let immediate = Database::open_prism_sealed_projection_with_vfs(
            &contexts.commit(),
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            opt,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        same_graph(&read_only, &immediate, &cx);
        assert_eq!(database.frontier().unwrap(), CommitSeq(3));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn buffered_cold_construction_drops_never_retain_partial_graphs_and_reader_remains_reusable() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _database = seed(&contexts.commit(), &vfs).await;
        let mut view = buffered(&contexts, &vfs).await;
        let opt = options();
        let cut = view.frontier();
        let retention = MemoryPool::new(1 << 26, 0).unwrap();
        view.set_sealed_read_cache_pool(&cx, retention.clone())
            .unwrap();
        let (first, pending) = counted(view.prism_sealed_projection_cooperative_at(
            &cx,
            cut,
            opt.selection,
            opt.projection,
            opt.projection_limits,
            opt.source_limits,
            SealedLimits::default(),
            RAW,
            quantum(3),
        ))
        .await;
        let first = first.unwrap();
        assert!(pending > 10);
        let source_activity = view.buffer_stats();
        assert!(first.shares_cache_with(&buffered_project(&mut view, &cx, opt).await));
        assert_eq!(view.buffer_stats(), source_activity);
        drop(view);
        assert_eq!(retention.used(), 0);
        for stop in 0..=pending {
            let mut view = buffered(&contexts, &vfs).await;
            let source_pool = view.memory_pool().clone();
            view.set_sealed_read_cache_pool(&cx, retention.clone())
                .unwrap();
            assert_eq!(retention.used(), 0);
            drop_at(
                view.prism_sealed_projection_cooperative_at(
                    &cx,
                    cut,
                    opt.selection,
                    opt.projection,
                    opt.projection_limits,
                    opt.source_limits,
                    SealedLimits::default(),
                    RAW,
                    quantum(3),
                ),
                stop,
            )
            .await;
            assert_eq!(view.sealed_read_cache_usage().entries, 0);
            assert_eq!(retention.used(), 0);
            assert_eq!(contexts.outstanding_obligations(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
        let mut view = buffered(&contexts, &vfs).await;
        view.set_sealed_read_cache_pool(&cx, retention.clone())
            .unwrap();
        let rebuilt = view
            .prism_sealed_projection_cooperative_at(
                &cx,
                cut,
                opt.selection,
                opt.projection,
                opt.projection_limits,
                opt.source_limits,
                SealedLimits::default(),
                RAW,
                quantum(1),
            )
            .await
            .unwrap();
        same_graph(&rebuilt, &first, &cx);
        assert_eq!(view.sealed_read_cache_usage().entries, 1);
        assert_eq!(retention.used(), rebuilt.charged_resident_bytes());
        assert_eq!(
            view.execute_fnx_sealed_cooperative(
                &cx,
                &call(TRIANGLES),
                opt,
                memory(),
                SealedLimits::default(),
                RAW,
                quantum(1),
            )
            .await
            .unwrap()
            .analytics
            .rows,
            expected()
        );
        drop(view);
        assert_eq!(retention.used(), 0);
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
