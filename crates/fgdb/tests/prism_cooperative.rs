//! Production Chronicle -> Strata -> Prism pulls, paused inside raw history.
//! Decoded projections are independent output oracles, not source substitutes.

use asupersync::runtime::yield_now::yield_now;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    Directedness, FnxAlgorithm, FnxArgument, FnxCallSpec, FnxExecutionError, FnxExecutionLimits,
    FnxMemoryLimits, FnxParameters, FnxReadOptions, FnxResult, FnxSealedExecutionError,
    FnxSealedReadError, FnxSelection, FnxSourceLimits, FnxWeightSpec, MissingWeightPolicy,
    PageRankOptions, ParallelEdgePolicy, ProjectionLimits, ProjectionSpec, SealedGraphView,
    SealedProjectionError, SelfLoopPolicy,
};
use fgdb_strata::tiered::sealed::{SealedLimits, SealedScanBudget, SealedScanStep};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use std::cell::Cell;
use std::future::Future;
use std::mem::size_of;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

#[path = "prism_cooperative/connectivity.rs"]
mod connectivity;

fn options(direction: Directedness) -> FnxReadOptions {
    FnxReadOptions {
        as_of: None,
        selection: FnxSelection {
            vertex_label: Some(LabelId(1)),
            relation: Some(RelationId(1)),
            weight: FnxWeightSpec::Property {
                key: PropertyKeyId(1),
                missing: MissingWeightPolicy::Reject,
            },
        },
        projection: ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        source_limits: FnxSourceLimits {
            max_work_units: 1_000_000,
            max_scratch_entries: 100_000,
            max_staging_bytes: 1 << 24,
        },
        projection_limits: ProjectionLimits {
            max_vertices: 1000,
            max_input_edges: 10_000,
            max_adjacency_entries: 10_000,
            max_workspace_bytes: 1 << 26,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 1000,
            max_estimated_work: 1 << 28,
        },
    }
}

async fn database(cx: &CommitCx) -> (Database<MemVfs>, CommitSeq) {
    let keys = DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    );
    let mut db = Database::<MemVfs>::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in [0, 1, 2, u128::MAX] {
        batch.create_vertex(VId(id), vec![LabelId(1)], vec![]);
    }
    batch.create_vertex(VId(99), vec![LabelId(2)], vec![]);
    for (id, source, target, weight) in [
        (1, 0, 1, 9_007_199_254_740_992i64),
        (2, 1, 0, 1),
        (3, 1, 0, 1),
        (4, 0, 0, 7),
        (5, 1, 2, 3),
        (6, 2, u128::MAX, 5),
    ] {
        batch.add_edge(
            EId(id),
            VId(source),
            VId(target),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))],
        );
    }
    // A long excluded group must consume raw fuel but never inspect Bool weights.
    // It also crosses the incoming index's 256-entry chunk boundaries.
    for id in 100..800 {
        batch.add_edge(
            EId(id),
            VId(99),
            VId(0),
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
        );
    }
    db.write(cx, batch).await.unwrap();
    let original = db.frontier().unwrap();
    for weight in 10..30 {
        let mut change = WriteBatch::new(RelationId(1));
        change.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(weight)));
        db.write(cx, change).await.unwrap();
    }
    (db, original)
}

async fn projection(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    options: FnxReadOptions,
) -> SealedGraphView {
    db.prism_sealed_projection_at(
        cx,
        options.as_of,
        options.selection,
        options.projection,
        options.projection_limits,
        options.source_limits,
        SealedLimits::default(),
    )
    .await
    .unwrap()
}

fn row(
    graph: &SealedGraphView,
    cx: &QueryCx,
    source: usize,
    lower: Option<VId>,
    quantum: usize,
) -> (Vec<(VId, u64)>, usize, usize) {
    let mut cursor = graph.neighbor_cursor_from(cx, source, lower).unwrap();
    let mut values = Vec::new();
    let mut spent = 0;
    let mut yields = 0;
    let mut ended = false;
    // A bound independent of the quantum catches a cursor that forgets its
    // progress whenever one face or a parallel reduction pauses.
    for _ in 0..(graph.scan_incidence_bound() * 8 + graph.node_count() * 4 + 32) {
        assert!(matches!(
            cursor
                .next_budgeted(cx, &mut SealedScanBudget::new(0))
                .unwrap(),
            SealedScanStep::Yield
        ));
        let mut fuel = SealedScanBudget::new(quantum);
        let result = cursor.next_budgeted(cx, &mut fuel).unwrap();
        assert!(fuel.remaining() <= quantum);
        spent += quantum - fuel.remaining();
        match result {
            SealedScanStep::Item((target, weight)) => {
                values.push((graph.vertex_id(target).unwrap(), weight.to_bits()));
            }
            SealedScanStep::Yield => {
                assert_eq!(fuel.remaining(), 0);
                yields += 1;
            }
            SealedScanStep::End => {
                ended = true;
                break;
            }
        }
    }
    assert!(ended, "bounded pulls must converge even with quantum one");
    assert!(matches!(
        cursor
            .next_budgeted(cx, &mut SealedScanBudget::new(0))
            .unwrap(),
        SealedScanStep::End
    ));
    assert!(cursor.next(cx).unwrap().is_none());
    (values, spent, yields)
}

#[test]
fn raw_history_faces_and_parallel_sums_resume_bit_exactly_at_every_small_quantum() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, original) = database(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for at in [original, view.frontier()] {
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
                    for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                        let mut opt = options(direction);
                        opt.as_of = Some(at);
                        opt.projection.parallel_edges = reduction;
                        opt.projection.self_loops = loops;
                        let graph = projection(&db, &cx, opt).await;
                        let decoded = view
                            .prism_projection_at(
                                &cx,
                                at,
                                opt.selection,
                                opt.projection,
                                opt.projection_limits,
                                opt.source_limits,
                            )
                            .unwrap();
                        for source in 0..graph.node_count() {
                            let (targets, weights) = decoded.projected_row(source).unwrap();
                            for lower in [None, Some(VId(1)), Some(VId(u128::MAX))] {
                                let expected: Vec<_> = targets
                                    .iter()
                                    .zip(weights)
                                    .map(|(&v, &w)| (decoded.vertex_id(v).unwrap(), w.to_bits()))
                                    .filter(|(v, _)| lower.is_none_or(|low| *v >= low))
                                    .collect();
                                let baseline = row(&graph, &cx, source, lower, usize::MAX);
                                assert_eq!(baseline.0, expected);
                                for quantum in [1, 2, 3, 7, 64, 255, 256, 257] {
                                    let actual = row(&graph, &cx, source, lower, quantum);
                                    assert_eq!(actual.0, expected, "{direction:?}, q={quantum}");
                                    assert_eq!(
                                        actual.1, baseline.1,
                                        "pausing must neither repeat nor skip raw work"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn a_pause_keeps_partial_groups_private_and_sync_resume_uses_the_same_positions() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, original) = database(&contexts.commit()).await;
        let mut opt = options(Directedness::Undirected);
        opt.as_of = Some(original);
        let graph = projection(&db, &cx, opt).await;
        let expected = row(&graph, &cx, 0, None, usize::MAX);
        let small = row(&graph, &cx, 0, None, 1);
        assert!(
            small.2 > 700,
            "hidden endpoints/history must yield without producing neighbors"
        );
        assert_eq!(small.0, expected.0);
        let big = 9_007_199_254_740_992f64;
        assert_ne!(((big + 1.0) + 1.0).to_bits(), (big + (1.0 + 1.0)).to_bits());
        assert_eq!(
            expected.0,
            vec![(VId(0), 7.0f64.to_bits()), (VId(1), big.to_bits())]
        );
        let mut cursor = graph.neighbor_cursor(&cx, 0).unwrap();
        assert!(matches!(
            cursor
                .next_budgeted(&cx, &mut SealedScanBudget::new(1))
                .unwrap(),
            SealedScanStep::Yield
        ));
        drop(db); // paused borrows refer only to the retained immutable projection
        let mut mixed = Vec::new();
        while let Some((target, weight)) = cursor.next(&cx).unwrap() {
            mixed.push((graph.vertex_id(target).unwrap(), weight.to_bits()));
        }
        assert_eq!(mixed, expected.0);
    });
}

#[test]
fn cancellation_after_every_resumable_checkpoint_fuses_even_with_zero_fuel() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, _) = database(&contexts.commit()).await;
        let graph = projection(&db, &cx, options(Directedness::Undirected)).await;
        let observe = Arc::new(SimulationCheckpointProbe::new(None));
        let measured = cx.with_checkpoint_probe(Arc::clone(&observe));
        let mut cursor = graph.neighbor_cursor(&cx, 0).unwrap();
        while !matches!(
            cursor
                .next_budgeted(&measured, &mut SealedScanBudget::new(7))
                .unwrap(),
            SealedScanStep::End
        ) {}
        let total = observe.calls();
        assert!(total > 700);
        for stop in 1..=total {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut cursor = graph.neighbor_cursor(&cx, 0).unwrap();
            loop {
                match cursor.next_budgeted(&controlled, &mut SealedScanBudget::new(7)) {
                    Ok(SealedScanStep::End) => {
                        panic!("cancelled scan cannot report successful EOF")
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            assert_eq!(probe.calls(), stop);
            assert!(matches!(
                cursor
                    .next_budgeted(&cx, &mut SealedScanBudget::new(7))
                    .unwrap(),
                SealedScanStep::End
            ));
        }
        let mut cursor = graph.neighbor_cursor(&cx, 0).unwrap();
        assert!(matches!(
            cursor
                .next_budgeted(&cx, &mut SealedScanBudget::new(1))
                .unwrap(),
            SealedScanStep::Yield
        ));
        let cancelled = cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
        assert!(
            cursor
                .next_budgeted(&cancelled, &mut SealedScanBudget::new(0))
                .is_err()
        );
        assert!(cursor.next(&cx).unwrap().is_none());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

fn memory() -> FnxMemoryLimits {
    FnxMemoryLimits {
        max_kernel_workspace_bytes: 1 << 24,
        max_result_bytes: 1 << 24,
    }
}
fn quantum(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
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

// Poll the actual Rust future and actual foundation yield, not a simulated
// kernel. Another actor could run at each Pending; no wall-clock timing enters
// the test. Only cooperative prepared calls (no I/O) are driven this way.
fn drive<F: Future>(future: F, probe: &SimulationCheckpointProbe, q: usize) -> (F::Output, usize) {
    let wakes = Arc::new(Wakes::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut task = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    let mut pending = 0;
    loop {
        let before = probe.calls();
        let result = future.as_mut().poll(&mut task);
        // Conservative checkpoint-count control, NOT an instruction/time bound.
        // A hidden-history or output-population unbounded loop violates it.
        assert!(
            probe.calls() - before <= q.saturating_mul(32).saturating_add(128),
            "one poll crossed an unbounded series of checkpointed steps"
        );
        match result {
            Poll::Ready(result) => {
                assert_eq!(wakes.0.load(Ordering::Relaxed), pending);
                return (result, pending);
            }
            Poll::Pending => {
                pending += 1;
                assert_eq!(
                    wakes.0.load(Ordering::Relaxed),
                    pending,
                    "each yield must wake the executor exactly once"
                );
                assert!(pending < 10_000_000, "resume must make progress");
            }
        }
    }
}

fn compare_execution(actual: &FnxResult, expected: &FnxResult) {
    assert_eq!(actual.columns, expected.columns);
    assert_eq!(actual.rows, expected.rows);
    // This also discriminates floating-point bit differences and output tags.
    assert_eq!(
        actual.certificate.result_digest,
        expected.certificate.result_digest
    );
    assert_eq!(actual.certificate.witness, expected.certificate.witness);
    assert_eq!(
        actual.certificate.estimated_work,
        expected.certificate.estimated_work
    );
    assert_eq!(
        actual.certificate.kernel_workspace_bytes,
        expected.certificate.kernel_workspace_bytes
    );
    assert_eq!(actual.certificate.snapshot, expected.certificate.snapshot);
    assert_eq!(
        actual.certificate.projection_digest,
        expected.certificate.projection_digest
    );
    assert_ne!(
        actual.certificate.execution_kernel,
        expected.certificate.execution_kernel
    );
}

#[test]
fn cooperative_bfs_and_pagerank_preserve_rows_scalar_bits_and_certificates_across_quanta() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, original) = database(&contexts.commit()).await;
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            for at in [original, db.frontier().unwrap()] {
                let mut opt = options(direction);
                opt.as_of = Some(at);
                let graph = projection(&db, &cx, opt).await;
                let view = db.read_session().unwrap();
                let decoded = view
                    .prism_projection_at(
                        &cx,
                        at,
                        opt.selection,
                        opt.projection,
                        opt.projection_limits,
                        opt.source_limits,
                    )
                    .unwrap();
                let mut calls = vec![];
                for weighted in [false, true] {
                    calls.push(FnxCallSpec::pagerank(
                        PageRankOptions::new(0.85, 1000, 1e-9, weighted).unwrap(),
                    ));
                }
                for source in [VId(0), VId(u128::MAX)] {
                    for cutoff in [None, Some(0), Some(1)] {
                        calls.push(FnxCallSpec::single_source_shortest_path_length(
                            source, cutoff,
                        ));
                    }
                }
                for call in calls {
                    let expected = call
                        .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                        .unwrap();
                    let independent = call
                        .execute(&decoded, opt.execution_limits, || cx.checkpoint())
                        .unwrap();
                    assert_eq!(expected.rows, independent.rows);
                    assert_eq!(
                        expected.certificate.result_digest,
                        independent.certificate.result_digest
                    );
                    let mut previous = None;
                    for q in [1, 7, 257] {
                        let probe = Arc::new(SimulationCheckpointProbe::new(None));
                        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                        let (actual, pending) = drive(
                            call.execute_sealed_cooperative(
                                &controlled,
                                &graph,
                                opt.execution_limits,
                                memory(),
                                quantum(q),
                                yield_now,
                            ),
                            &probe,
                            q,
                        );
                        let actual = actual.unwrap();
                        compare_execution(&actual, &expected);
                        if q == 1 {
                            assert!(pending > graph.node_count());
                        }
                        if let Some(previous) = &previous {
                            assert_eq!(&actual, previous);
                        }
                        previous = Some(actual);
                    }
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

async fn small_database(cx: &CommitCx, n: usize, negative: bool) -> Database<MemVfs> {
    let mut db = Database::<MemVfs>::open_memory(
        cx,
        DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        ),
    )
    .await
    .unwrap();
    if n == 0 {
        return db;
    }
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 0..n {
        batch.create_vertex(VId(id as u128), vec![LabelId(1)], vec![]);
    }
    if n == 3 {
        for (id, s, t) in [(1, 0, 1), (2, 1, 2)] {
            batch.add_edge(
                EId(id),
                VId(s),
                VId(t),
                vec![(
                    PropertyKeyId(1),
                    CanonicalScalar::Int(if negative { -1 } else { 2 }),
                )],
            );
        }
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn cooperative_admission_has_no_sync_fallback_and_uses_exact_independent_limits() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Directed);
        let graph = projection(&db, &cx, opt).await;
        for call in [FnxCallSpec::single_source_shortest_path_length(VId(0), None),
            FnxCallSpec::pagerank(PageRankOptions::default())] {
            let expected = call.execute_sealed_cooperative(&cx, &graph,
                opt.execution_limits, memory(), quantum(7), yield_now).await.unwrap();
            let columns = call.outputs().len() * size_of::<String>()
                + call.outputs().iter().map(|column| column.name.len()).sum::<usize>();
            let bytes = columns + 3 * (size_of::<Vec<fgdb_prism::FnxValue>>()
                + call.outputs().len() * size_of::<fgdb_prism::FnxValue>());
            let exact = FnxExecutionLimits {
                max_iterations: call.options().map_or(0, PageRankOptions::max_iter),
                max_result_rows: 3, max_estimated_work: expected.certificate.estimated_work,
            };
            let mem = FnxMemoryLimits { max_kernel_workspace_bytes: expected.certificate.kernel_workspace_bytes,
                max_result_bytes: bytes };
            assert_eq!(call.execute_sealed_cooperative(&cx, &graph, exact, mem,
                quantum(1), yield_now).await.unwrap(), expected);
            for (resource, limits, memory) in [
                ("estimated work", FnxExecutionLimits { max_estimated_work: exact.max_estimated_work - 1, ..exact }, mem),
                ("kernel workspace bytes", exact, FnxMemoryLimits { max_kernel_workspace_bytes: mem.max_kernel_workspace_bytes - 1, ..mem }),
                ("result rows", FnxExecutionLimits { max_result_rows: 2, ..exact }, mem),
                ("result bytes", exact, FnxMemoryLimits { max_result_bytes: bytes - 1, ..mem }),
            ] {
                assert!(matches!(call.execute_sealed_cooperative(&cx, &graph, limits, memory,
                    quantum(1), yield_now).await,
                    Err(FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded { resource: actual, .. }))
                        if actual == resource));
            }
        }
        let cutoff = FnxCallSpec::single_source_shortest_path_length(VId(0), Some(0));
        assert_eq!(cutoff.execute_sealed_cooperative(&cx, &graph,
            FnxExecutionLimits { max_result_rows: 1, ..opt.execution_limits }, memory(),
            quantum(1), yield_now).await.unwrap().rows.len(), 1);
        for call in [FnxCallSpec::strongly_connected_components(), FnxCallSpec::triangles(),
            FnxCallSpec::clustering_coefficient()] {
            assert!(!call.supports_cooperative_sealed_execution());
            let mut forbidden = opt; forbidden.source_limits.max_work_units = 0;
            assert!(matches!(db.execute_fnx_sealed_cooperative(&cx, &call, forbidden, memory(),
                SealedLimits { max_image_bytes: 0, ..SealedLimits::default() }, quantum(1)).await,
                Err(FnxSealedReadError::Execution(FnxSealedExecutionError::UnsupportedCooperativeAlgorithm(_)))));
        }
        let absent = FnxCallSpec::single_source_shortest_path_length(VId(u128::MAX), None);
        assert!(matches!(absent.execute_sealed_cooperative(&cx, &graph, opt.execution_limits, memory(),
            quantum(1), || -> std::future::Ready<()> { panic!("preflight must not yield") }).await,
            Err(FnxSealedExecutionError::Execution(FnxExecutionError::UnknownSource(VId(u128::MAX))))));
    });
}

#[test]
fn cooperative_numeric_refusal_nonconvergence_and_empty_or_isolated_populations_are_exact() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let opt = options(Directedness::Directed);
        for negative in [false, true] {
            let db = small_database(&contexts.commit(), 3, negative).await;
            let graph = projection(&db, &cx, opt).await;
            let call = FnxCallSpec::pagerank(PageRankOptions::new(0.85, 1, 1e-30, true).unwrap());
            let error = call
                .execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(1),
                    yield_now,
                )
                .await
                .unwrap_err();
            if negative {
                assert!(matches!(
                    error,
                    FnxSealedExecutionError::Execution(FnxExecutionError::NegativeWeight)
                ));
            } else {
                assert!(matches!(
                    error,
                    FnxSealedExecutionError::Execution(FnxExecutionError::NotConverged {
                        max_iterations: 1,
                        ..
                    })
                ));
            }
            let unweighted =
                FnxCallSpec::pagerank(PageRankOptions::new(0.85, 1000, 1e-9, false).unwrap());
            compare_execution(
                &unweighted
                    .execute_sealed_cooperative(
                        &cx,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(1),
                        yield_now,
                    )
                    .await
                    .unwrap(),
                &unweighted
                    .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                    .unwrap(),
            );
        }
        for n in [0, 512] {
            let db = small_database(&contexts.commit(), n, false).await;
            let graph = projection(&db, &cx, opt).await;
            let call = FnxCallSpec::pagerank(PageRankOptions::default());
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let (result, yields) = drive(
                call.execute_sealed_cooperative(
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(1),
                    yield_now,
                ),
                &probe,
                1,
            );
            assert_eq!(result.unwrap().rows.len(), n);
            if n != 0 {
                // No edges can produce these yields: scalar initialization,
                // convergence and result conversion must themselves cooperate.
                assert!(yields >= n * 5);
                let call = FnxCallSpec::single_source_shortest_path_length(VId(0), Some(0));
                let (result, yields) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        FnxExecutionLimits {
                            max_result_rows: 1,
                            ..opt.execution_limits
                        },
                        memory(),
                        quantum(1),
                        yield_now,
                    ),
                    &probe,
                    1,
                );
                assert_eq!(result.unwrap().rows.len(), 1);
                assert!(
                    yields >= n * 2,
                    "initialization AND sparse result encoding must yield"
                );
            }
        }
    });
}

#[test]
fn every_cooperative_checkpoint_cancels_and_every_suspension_can_be_dropped_without_leaks() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Directed);
        let graph = projection(&db, &cx, opt).await;
        let root_before = db.read_session().unwrap().partition_root();
        for call in [
            FnxCallSpec::single_source_shortest_path_length(VId(0), None),
            FnxCallSpec::pagerank(PageRankOptions::default()),
        ] {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let (expected, yields) = drive(
                call.execute_sealed_cooperative(
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                ),
                &probe,
                3,
            );
            let expected = expected.unwrap();
            let total = probe.calls();
            for stop in 1..=total {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let (actual, _) = drive(
                    call.execute_sealed_cooperative(
                        &controlled,
                        &graph,
                        opt.execution_limits,
                        memory(),
                        quantum(3),
                        yield_now,
                    ),
                    &probe,
                    3,
                );
                assert!(
                    matches!(actual, Err(FnxSealedExecutionError::Cancelled(_))),
                    "cut={stop}"
                );
                assert_eq!(probe.calls(), stop);
            }
            let wakes = Arc::new(Wakes::default());
            let waker = Waker::from(wakes);
            let mut task = Context::from_waker(&waker);
            for stop in 0..=yields {
                let mut future = Box::pin(call.execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                ));
                for _ in 0..stop {
                    assert!(future.as_mut().poll(&mut task).is_pending());
                }
                drop(future); // includes never-polled and final-result-build suspensions
                assert_eq!(contexts.outstanding_obligations(), 0);
                assert_eq!(db.read_session().unwrap().partition_root(), root_before);
            }
            assert_eq!(
                call.execute_sealed_cooperative(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(7),
                    yield_now
                )
                .await
                .unwrap(),
                expected
            );
        }
    });
}

#[test]
fn public_cooperative_calls_share_exact_source_preparation_and_keep_old_entrypoints_synchronous() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let (db, at) = database(&contexts.commit()).await;
        let mut opt = options(Directedness::Undirected); opt.as_of = Some(at);
        let graph = projection(&db, &cx, opt).await;
        let params: FnxParameters = [("source".to_owned(), FnxArgument::Vertex(VId(0)))].into_iter().collect();
        for text in [
            "CALL fnx.single_source_shortest_path_length($source) YIELD distance AS hops,vertex AS id",
            "CALL fnx.pagerank(0.85,1000,1e-9,true) YIELD score AS rank,vertex AS id",
        ] {
            let call = FnxCallSpec::bind(text, &params).unwrap();
            let prepared = call.execute_sealed_cooperative(&cx, &graph, opt.execution_limits,
                memory(), quantum(7), yield_now).await.unwrap();
            let hosted = db.call_fnx_sealed_cooperative(&cx, text, &params, opt, memory(),
                SealedLimits::default(), quantum(13)).await.unwrap();
            assert_eq!(hosted.analytics, prepared);
            assert_eq!(hosted.selection, opt.selection);
            assert_eq!(db.execute_fnx_sealed_cooperative(&cx, &call, opt, memory(),
                SealedLimits::default(), quantum(1)).await.unwrap(), hosted);
            let old = db.call_fnx_sealed(&cx, text, &params, opt, memory(),
                SealedLimits::default()).await.unwrap();
            assert_eq!(old.analytics, call.execute_sealed(&cx, &graph, opt.execution_limits, memory()).unwrap());
            compare_execution(&hosted.analytics, &old.analytics);
            let mut future_opt = opt; future_opt.as_of = Some(CommitSeq(u64::MAX));
            assert!(matches!(db.execute_fnx_sealed_cooperative(&cx, &call, future_opt,
                memory(), SealedLimits::default(), quantum(1)).await,
                Err(FnxSealedReadError::Input(fgdb_prism::FnxReadError::Read(_)))));
        }
        let mut forbidden = opt; forbidden.source_limits.max_work_units = 0;
        assert!(matches!(db.call_fnx_sealed_cooperative(&cx, "CALL fnx.unknown()", &params,
            forbidden, memory(), SealedLimits::default(), quantum(1)).await,
            Err(FnxSealedReadError::Input(fgdb_prism::FnxReadError::Bind(_)))));
        let dijkstra = FnxCallSpec::bind("CALL fnx.single_source_dijkstra_path_length($source)", &params).unwrap();
        assert!(matches!(db.execute_fnx_sealed_cooperative(&cx, &dijkstra, forbidden,
            memory(), SealedLimits::default(), quantum(1)).await,
            Err(FnxSealedReadError::Execution(FnxSealedExecutionError::UnsupportedCooperativeAlgorithm(
                FnxAlgorithm::SingleSourceDijkstraPathLength(_))))));
    });
}

#[derive(Debug, PartialEq)]
struct LiveRefusal(usize);
impl std::fmt::Display for LiveRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "live execution refused at {}", self.0)
    }
}
impl std::error::Error for LiveRefusal {}

fn live_refusal(error: FnxSealedExecutionError, expected: usize) {
    match error {
        FnxSealedExecutionError::Source(SealedProjectionError::Guard(cause)) => {
            assert_eq!(
                cause.downcast_ref::<LiveRefusal>(),
                Some(&LiveRefusal(expected))
            );
        }
        other => panic!("live guard lost its typed cause: {other}"),
    }
}

#[test]
fn cooperative_live_guards_span_every_checkpoint_without_changing_successful_results() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        for n in [0, 3] {
            let db = small_database(&contexts.commit(), n, false).await;
            let opt = options(Directedness::Undirected);
            let graph = projection(&db, &cx, opt).await;
            let mut calls = vec![FnxCallSpec::pagerank(PageRankOptions::default())];
            if n != 0 {
                calls.push(FnxCallSpec::single_source_shortest_path_length(
                    VId(0),
                    None,
                ));
            }
            for call in calls {
                for q in [1, 7] {
                    let mut count = 0;
                    let expected = call
                        .execute_sealed_cooperative_with_checkpoint(
                            &cx,
                            &graph,
                            opt.execution_limits,
                            memory(),
                            quantum(q),
                            yield_now,
                            || {
                                count += 1;
                                Ok(())
                            },
                        )
                        .await
                        .unwrap();
                    assert!(count > n);
                    assert_eq!(
                        call.execute_sealed_cooperative(
                            &cx,
                            &graph,
                            opt.execution_limits,
                            memory(),
                            quantum(q),
                            yield_now
                        )
                        .await
                        .unwrap(),
                        expected
                    );
                    // Includes initialization, source opens, raw pulls, scalar
                    // passes, before/after yields, row conversion and delivery.
                    for stop in 1..=count {
                        let mut seen = 0;
                        let actual = call
                            .execute_sealed_cooperative_with_checkpoint(
                                &cx,
                                &graph,
                                opt.execution_limits,
                                memory(),
                                quantum(q),
                                yield_now,
                                || {
                                    seen += 1;
                                    if seen == stop {
                                        Err(SealedProjectionError::Guard(Box::new(LiveRefusal(
                                            stop,
                                        ))))
                                    } else {
                                        Ok(())
                                    }
                                },
                            )
                            .await;
                        live_refusal(actual.unwrap_err(), stop);
                        assert_eq!(seen, stop, "no control work after a refusal");
                    }
                    let cancelled =
                        cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
                    assert!(matches!(
                        call.execute_sealed_cooperative_with_checkpoint(
                            &cancelled,
                            &graph,
                            opt.execution_limits,
                            memory(),
                            quantum(q),
                            yield_now,
                            || panic!("runtime cancellation must precede the additional guard"),
                        )
                        .await,
                        Err(FnxSealedExecutionError::Cancelled(_))
                    ));
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}

#[test]
fn revocation_during_each_suspension_precedes_any_resumed_source_or_result_work() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Directed);
        let graph = projection(&db, &cx, opt).await;
        let root_before = db.read_session().unwrap().partition_root();
        for call in [
            FnxCallSpec::single_source_shortest_path_length(VId(0), None),
            FnxCallSpec::pagerank(PageRankOptions::default()),
        ] {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let measured = cx.with_checkpoint_probe(Arc::clone(&probe));
            let (expected, suspensions) = drive(
                call.execute_sealed_cooperative_with_checkpoint(
                    &measured,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                    || Ok(()),
                ),
                &probe,
                3,
            );
            let expected = expected.unwrap();
            assert!(suspensions > 3);
            for stop in 1..=suspensions {
                let revoked = Cell::new(false);
                let guards = Cell::new(0usize);
                let probe = Arc::new(SimulationCheckpointProbe::new(None));
                let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                let waker = Waker::from(Arc::new(Wakes::default()));
                let mut task = Context::from_waker(&waker);
                let mut future = Box::pin(call.execute_sealed_cooperative_with_checkpoint(
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(3),
                    yield_now,
                    || {
                        guards.set(guards.get() + 1);
                        if revoked.get() {
                            Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop))))
                        } else {
                            Ok(())
                        }
                    },
                ));
                for _ in 0..stop {
                    assert!(future.as_mut().poll(&mut task).is_pending());
                }
                let before = probe.calls();
                let before_guards = guards.get();
                revoked.set(true);
                let Poll::Ready(result) = future.as_mut().poll(&mut task) else {
                    panic!("revoked execution must not schedule another scan slice");
                };
                live_refusal(result.unwrap_err(), stop);
                assert_eq!(probe.calls(), before + 1);
                assert_eq!(guards.get(), before_guards + 1);
                drop(future);
                assert_eq!(db.read_session().unwrap().partition_root(), root_before);
                assert_eq!(contexts.outstanding_obligations(), 0);
            }
            // The immutable projection has no stored guard/permit or partial
            // execution state; a separately admitted execution is independent.
            assert_eq!(
                call.execute_sealed_cooperative_with_checkpoint(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(7),
                    yield_now,
                    || Ok(())
                )
                .await
                .unwrap(),
                expected
            );
        }
    });
}
