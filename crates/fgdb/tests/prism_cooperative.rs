//! Production Chronicle -> Strata -> Prism pulls, paused inside raw history.
//! Decoded projections are independent output oracles, not source substitutes.
//!
//! Use the foundation's registered current-thread driver for awaited kernels.
//! Its generic unregistered-root runner treats repeated self-wakes as spinning
//! and adds sleep backoff. Keep actual yield futures, the explicit poll/wake
//! assertions, all cancellation cuts and the existing deadline unchanged.

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

#[path = "prism_cooperative/clustering.rs"]
mod clustering;
#[path = "prism_cooperative/connectivity.rs"]
mod connectivity;
#[path = "prism_cooperative/core.rs"]
mod core_numbers;
#[path = "prism_cooperative/dijkstra.rs"]
mod dijkstra;
#[path = "prism_cooperative/eigenvector.rs"]
mod eigenvector;

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
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
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
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
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
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
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
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
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

#[test]
fn fixed_iteration_pagerank_and_cdlp_reach_public_and_prepared_authorized_calls() {
    use asupersync::lab::run_async_under_lab;
    use asupersync::security::key::AuthKey;
    use fgdb::{QueryResult, QueryValue};
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
    use fgdb_prism::{FnxReadError, FnxValue};
    use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

    const BRANCH: &str = "host-analytics-branch";
    let ((), report) = run_async_under_lab(0x5ec0_3103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = small_database(&c.commit(), 0, false).await;
        let mut batch = WriteBatch::new(RelationId(1));
        for id in [1, 2, 3, u128::MAX] {
            let labels = if id == 1 {
                vec![LabelId(1), LabelId(99)]
            } else {
                vec![LabelId(1)]
            };
            batch.create_vertex(VId(id), labels, vec![]);
        }
        for (eid, source, target, weight) in [
            (1, 1, 2, 2),
            (2, 2, 3, 3),
            (3, 3, 1, 4),
            (4, 1, 2, 5),
            (5, 2, 2, 1),
        ] {
            batch.add_edge(
                EId(eid),
                VId(source),
                VId(target),
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(weight)),
                    (PropertyKeyId(2), CanonicalScalar::Bool(true)),
                ],
            );
        }
        batch.create_vertex(VId(99), vec![LabelId(99)], vec![]);
        for (eid, source, target) in [(90, 1, 99), (91, 99, 3)] {
            batch.add_edge(
                EId(eid),
                VId(source),
                VId(target),
                vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
            );
        }
        db.write(&c.commit(), batch).await.unwrap();
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(
            EId(200),
            VId(1),
            VId(3),
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))],
        );
        db.write(&c.commit(), other).await.unwrap();

        let issuer = Authority::new(
            AuthKey::from_seed(313),
            DatabaseSecurityNamespaceId([0x92; 32]),
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            BRANCH,
            1000,
            QueryLimits {
                max_nodes: 1000,
                max_work: 1_000_000,
                max_rows: 1000,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([RelationId(1)]);
        grant.properties = Scope::only([PropertyKeyId(1)]);
        let token = issuer.issue_at(&grant, 100).unwrap();
        let mut native_options = options(Directedness::Directed);
        // Native calls rely on the signed mask, while GQL below supplies the
        // same induced graph explicitly as its projection selection.
        native_options.selection.vertex_label = None;
        native_options.selection.relation = None;
        native_options.projection.parallel_edges = ParallelEdgePolicy::Minimum;
        native_options.source_limits = FnxSourceLimits {
            max_work_units: 100_000,
            max_scratch_entries: 10_000,
            max_staging_bytes: 1 << 20,
        };
        native_options.projection_limits = ProjectionLimits {
            max_vertices: 100,
            max_input_edges: 100,
            max_adjacency_entries: 200,
            max_workspace_bytes: 1 << 20,
        };
        native_options.execution_limits = FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 100,
            max_estimated_work: 1 << 26,
        };
        let symbols = |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "weight") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        };
        let policy = GqlQueryPolicy::new(100_000, 10_000, 1_000_000, 100_000);
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy, || 100)
            .unwrap();
        let text = "CALL fnx.pagerank(iterations => $steps, alpha => 0.85, \
                    weighted => true, label => 'Visible', relation => 'R', \
                    weight => 'weight', parallel => 'min', self_loops => 'keep') \
                    YIELD vertex,score RETURN vertex,score ORDER BY vertex";
        let parameters = GqlParameters::new().with_int64("steps", 2).unwrap();
        let prepared = session.prepare(&cx, text, &parameters).unwrap();
        for steps in [1, 2, 5] {
            // An independent four-state transition matrix: the induced
            // graph is 1 -> 2, 2 -> {2,3} with weights {1,3}, 3 -> 1,
            // and one isolate. The parallel 1 -> 2 edges reduce to min(2,5).
            // This scalar oracle never calls Prism or inspects its CSR.
            let mut expected = [0.25_f64; 4];
            for _ in 0..steps {
                let initial = (1.0 - 0.85) / 4.0 + 0.85 * expected[3] / 4.0;
                expected = [
                    initial + 0.85 * expected[2],
                    initial + 0.85 * expected[0] + 0.85 * expected[1] / 4.0,
                    initial + 0.85 * expected[1] * 3.0 / 4.0,
                    initial,
                ];
            }
            let standalone = format!(
                "CALL fnx.pagerank(iterations => {steps}, alpha => 0.85, weighted => true) \
                 YIELD vertex,score"
            );
            let actual = db
                .call_fnx_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &standalone,
                    &FnxParameters::new(),
                    native_options,
                    || 100,
                )
                .unwrap();
            assert_eq!(actual.len(), 4);
            for ((row, id), expected) in actual.iter().zip([1, 2, 3, u128::MAX]).zip(expected) {
                let [FnxValue::Vertex(vertex), FnxValue::Score(score)] = row.as_slice() else {
                    panic!("unexpected fixed PageRank row: {row:?}");
                };
                assert_eq!(*vertex, VId(id));
                assert!(
                    (score - expected).abs() < 1e-14,
                    "{steps}: {score} != {expected}"
                );
            }
            let parameters = GqlParameters::new().with_int64("steps", steps).unwrap();
            for result in [
                db.query(&cx, text, &parameters, symbols, policy).unwrap(),
                session.execute(&cx, &prepared, &parameters).unwrap(),
            ] {
                let QueryResult::Rows { rows, .. } = result else {
                    panic!("PageRank returned a write receipt");
                };
                let expected_rows: Vec<_> = actual
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|value| {
                                QueryValue::Value(match value {
                                    FnxValue::Vertex(vertex) => GraphValue::Vertex(*vertex),
                                    FnxValue::Score(value) => {
                                        GraphValue::Scalar(CanonicalScalar::Float(
                                            fgdb_types::CanonicalF64::new(*value),
                                        ))
                                    }
                                    _ => panic!("unexpected PageRank output domain"),
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect();
                assert_eq!(
                    rows, expected_rows,
                    "prepared fixed iteration count {steps}"
                );
            }
        }

        // CDLP uses the same signed population, with an explicitly simple,
        // loop-free graph. Removing the loop and collapsing the parallel pair
        // leaves a directed three-cycle and the full-width isolated identity.
        let mut community_options = native_options;
        community_options.selection.weight = FnxWeightSpec::Unit;
        community_options.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
        community_options.projection.self_loops = SelfLoopPolicy::Drop;
        let community_text = "CALL fnx.cdlp(max_iter => $rounds, label => 'Visible', \
                              relation => 'R', parallel => 'collapse', self_loops => 'drop') \
                              YIELD vertex,component AS community \
                              RETURN vertex,community ORDER BY vertex";
        let community_parameters = GqlParameters::new().with_int64("rounds", 2).unwrap();
        let community_prepared = session
            .prepare(&cx, community_text, &community_parameters)
            .unwrap();
        let ids = [1_u128, 2, 3, u128::MAX];
        for rounds in [0, 1, 2, 5] {
            // Every cycle vertex has two distinct neighbors. Equal-frequency
            // ties take the smaller previous-round label; this independent
            // transition neither calls Prism nor reads a projection.
            let mut expected = ids;
            for _ in 0..rounds {
                expected = [
                    expected[1].min(expected[2]),
                    expected[0].min(expected[2]),
                    expected[0].min(expected[1]),
                    expected[3],
                ];
            }
            let standalone = format!("CALL fnx.cdlp(max_iter => {rounds}) YIELD vertex,component");
            let actual = db
                .call_fnx_authorized(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    &standalone,
                    &FnxParameters::new(),
                    community_options,
                    || 100,
                )
                .unwrap();
            assert_eq!(actual.len(), ids.len());
            for ((row, id), label) in actual.iter().zip(ids).zip(expected) {
                assert_eq!(
                    row.as_slice(),
                    &[FnxValue::Vertex(VId(id)), FnxValue::Vertex(VId(label))],
                    "CDLP round {rounds}"
                );
            }
            let parameters = GqlParameters::new().with_int64("rounds", rounds).unwrap();
            let expected_rows: Vec<_> = ids
                .into_iter()
                .zip(expected)
                .map(|(id, label)| {
                    vec![
                        QueryValue::Value(GraphValue::Vertex(VId(id))),
                        QueryValue::Value(GraphValue::Vertex(VId(label))),
                    ]
                })
                .collect();
            for result in [
                db.query(&cx, community_text, &parameters, symbols, policy)
                    .unwrap(),
                session
                    .execute(&cx, &community_prepared, &parameters)
                    .unwrap(),
            ] {
                let QueryResult::Rows { rows, .. } = result else {
                    panic!("CDLP returned a write receipt");
                };
                assert_eq!(rows, expected_rows, "prepared CDLP round {rounds}");
            }
        }
        let mut community_limited = community_options;
        community_limited.execution_limits.max_iterations = 0;
        assert!(matches!(
            db.call_fnx_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "CALL fnx.cdlp(max_iter => 1)",
                &FnxParameters::new(),
                community_limited,
                || 100,
            ),
            Err(FnxReadError::Execution(FnxExecutionError::LimitExceeded {
                resource: "iterations",
                limit: 0,
                requested: 1,
            }))
        ));
        for arguments in ["", "max_iter => -1", "max_iter => 1.5", "max_iter => NULL"] {
            let text = format!("CALL fnx.cdlp({arguments}) YIELD vertex RETURN vertex LIMIT 0");
            assert!(
                db.query(&cx, &text, &GqlParameters::new(), symbols, policy)
                    .is_err(),
                "{text}"
            );
        }

        // The exact count is still admitted against native limits. It never
        // becomes an instruction to bypass the host's maximum iteration cap.
        let mut limited = native_options;
        limited.execution_limits.max_iterations = 1;
        assert!(matches!(
            db.call_fnx_authorized(
                &cx,
                &issuer,
                &token,
                BRANCH,
                "CALL fnx.pagerank(iterations => 2)",
                &FnxParameters::new(),
                limited,
                || 100,
            ),
            Err(FnxReadError::Execution(FnxExecutionError::LimitExceeded {
                resource: "iterations",
                limit: 1,
                requested: 2,
            }))
        ));
        for arguments in [
            "iterations => 0",
            "iterations => 2, max_iter => 100",
            "iterations => 2, tol => 0.001",
            "iterations => 2, args => []",
        ] {
            let text = format!("CALL fnx.pagerank({arguments}) YIELD vertex RETURN vertex LIMIT 0");
            assert!(
                db.query(&cx, &text, &GqlParameters::new(), symbols, policy)
                    .is_err(),
                "{text}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn fixed_iteration_pagerank_keeps_decoded_and_compressed_bits_at_saved_and_current_cuts() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
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
                for weighted in [false, true] {
                    let call = FnxCallSpec::pagerank(
                        PageRankOptions::fixed_iterations(0.85, 2, weighted).unwrap(),
                    );
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
                    assert_eq!(expected.certificate.call_digest, call.digest());
                    assert_eq!(
                        expected.certificate.witness.algorithm,
                        "pagerank_fixed_iterations"
                    );
                    assert_eq!(
                        expected.certificate.witness.nodes_touched,
                        2 * graph.node_count()
                    );
                    assert_eq!(
                        expected.certificate.witness.edges_scanned,
                        2 * graph.adjacency_entry_count()
                    );
                    assert_eq!(
                        expected.rows.len(),
                        4,
                        "excluded history must not expand the population"
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
                        assert_eq!(actual.certificate.call_digest, call.digest());
                        if q == 1 {
                            assert!(pending > 2 * graph.node_count());
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

#[test]
fn fixed_iteration_pagerank_observes_every_cooperative_cancellation_and_live_guard() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let clock = runtime.request_cx_with_budget(Budget::INFINITE);
    let root = runtime.request_cx_with_budget(
        Budget::INFINITE.with_timeout(clock.now(), std::time::Duration::from_secs(600)),
    );
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Directed);
        let graph = projection(&db, &cx, opt).await;
        let root_before = db.read_session().unwrap().partition_root();
        let call = FnxCallSpec::pagerank(PageRankOptions::fixed_iterations(0.85, 2, true).unwrap());
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
        let (expected, pending) = drive(
            call.execute_sealed_cooperative(
                &controlled,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(7),
                yield_now,
            ),
            &probe,
            7,
        );
        let expected = expected.unwrap();
        assert!(pending > 2);
        assert_eq!(expected.certificate.witness.nodes_touched, 6);
        compare_execution(
            &expected,
            &call
                .execute_sealed(&cx, &graph, opt.execution_limits, memory())
                .unwrap(),
        );
        for stop in 1..=probe.calls() {
            let refusal = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&refusal));
            let (actual, _) = drive(
                call.execute_sealed_cooperative(
                    &controlled,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(7),
                    yield_now,
                ),
                &refusal,
                7,
            );
            assert!(
                matches!(actual, Err(FnxSealedExecutionError::Cancelled(_))),
                "cut={stop}"
            );
            assert_eq!(refusal.calls(), stop);
        }
        let mut guards = 0;
        assert_eq!(
            call.execute_sealed_cooperative_with_checkpoint(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(7),
                yield_now,
                || {
                    guards += 1;
                    Ok(())
                },
            )
            .await
            .unwrap(),
            expected
        );
        for stop in 1..=guards {
            let mut seen = 0;
            let result = call
                .execute_sealed_cooperative_with_checkpoint(
                    &cx,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(7),
                    yield_now,
                    || {
                        seen += 1;
                        if seen == stop {
                            Err(SealedProjectionError::Guard(Box::new(LiveRefusal(stop))))
                        } else {
                            Ok(())
                        }
                    },
                )
                .await;
            live_refusal(result.unwrap_err(), stop);
            assert_eq!(seen, stop);
        }
        assert!(matches!(
            call.execute_sealed_cooperative(
                &cx,
                &graph,
                FnxExecutionLimits {
                    max_iterations: 1,
                    ..opt.execution_limits
                },
                memory(),
                quantum(1),
                || -> std::future::Ready<()> {
                    panic!("iteration admission must precede yielding")
                },
            )
            .await,
            Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::LimitExceeded {
                    resource: "iterations",
                    ..
                }
            ))
        ));
        let huge = FnxCallSpec::pagerank(
            PageRankOptions::fixed_iterations(0.85, usize::MAX, true).unwrap(),
        );
        assert!(matches!(
            huge.execute_sealed_cooperative(
                &cx,
                &graph,
                FnxExecutionLimits {
                    max_iterations: usize::MAX,
                    ..opt.execution_limits
                },
                memory(),
                quantum(1),
                || -> std::future::Ready<()> { panic!("overflow admission must precede yielding") },
            )
            .await,
            Err(FnxSealedExecutionError::Execution(
                FnxExecutionError::SizeOverflow
            ))
        ));
        assert_eq!(
            call.execute_sealed_cooperative(
                &cx,
                &graph,
                opt.execution_limits,
                memory(),
                quantum(1),
                yield_now,
            )
            .await
            .unwrap(),
            expected
        );
        assert_eq!(db.read_session().unwrap().partition_root(), root_before);
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
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Directed);
        let graph = projection(&db, &cx, opt).await;
        for call in [FnxCallSpec::single_source_shortest_path_length(VId(0), None),
            FnxCallSpec::pagerank(PageRankOptions::default()),
            FnxCallSpec::pagerank(PageRankOptions::fixed_iterations(0.85, 2, true).unwrap())] {
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
        for call in [FnxCallSpec::triangles(),
            FnxCallSpec::clustering_coefficient(), degree_call()] {
            assert!(call.supports_cooperative_sealed_execution());
            // Supported kernels still reject incompatible directed projections
            // at preflight, rather than yielding or silently changing semantics.
            let expected = call.execute_sealed(&cx, &graph,
                opt.execution_limits, memory()).unwrap_err();
            let actual = call.execute_sealed_cooperative(&cx, &graph,
                opt.execution_limits, memory(), quantum(1),
                || -> std::future::Ready<()> { panic!("invalid projection must not yield") })
                .await.unwrap_err();
            assert_eq!(actual.to_string(), expected.to_string());
        }
        let absent = FnxCallSpec::single_source_shortest_path_length(VId(u128::MAX), None);
        assert!(matches!(absent.execute_sealed_cooperative(&cx, &graph, opt.execution_limits, memory(),
            quantum(1), || -> std::future::Ready<()> { panic!("preflight must not yield") }).await,
            Err(FnxSealedExecutionError::Execution(FnxExecutionError::UnknownSource(VId(u128::MAX))))));
    });
}

#[test]
fn cooperative_numeric_refusal_nonconvergence_and_empty_or_isolated_populations_are_exact() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
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
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        let root_before = db.read_session().unwrap().partition_root();
        for call in [
            FnxCallSpec::single_source_shortest_path_length(VId(0), None),
            FnxCallSpec::pagerank(PageRankOptions::default()),
            FnxCallSpec::triangles(),
            FnxCallSpec::clustering_coefficient(),
            degree_call(),
            core_call(),
            eigenvector_call(),
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
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
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
            "CALL fnx.pagerank(iterations => 2, weighted => true) YIELD score AS rank,vertex AS id",
            "CALL fnx.single_source_dijkstra_path_length($source,NULL,true) YIELD distance AS cost,vertex AS id",
            "CALL fnx.triangles() YIELD triangles AS total,vertex AS id",
            "CALL fnx.clustering_coefficient() YIELD score AS coefficient,vertex AS id",
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
        assert!(dijkstra.supports_cooperative_sealed_execution());
        assert!(matches!(db.execute_fnx_sealed_cooperative(&cx, &dijkstra, forbidden,
            memory(), SealedLimits::default(), quantum(1)).await,
            Err(FnxSealedReadError::Input(fgdb_prism::FnxReadError::SourceLimit { .. }))));
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
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    // A stall guard, not a performance bound. This test once looped through
    // ~9,300 refusing checkpoints for over 30 minutes and stalled a whole test
    // census. Under a deadline, a runaway loop fails at its next checkpoint.
    let clock = runtime.request_cx_with_budget(Budget::INFINITE);
    let root = runtime.request_cx_with_budget(
        Budget::INFINITE.with_timeout(clock.now(), std::time::Duration::from_secs(600)),
    );
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        for n in [0, 3] {
            let db = small_database(&contexts.commit(), n, false).await;
            let opt = options(Directedness::Undirected);
            let graph = projection(&db, &cx, opt).await;
            // The stop loop below refuses at EVERY checkpoint, so its cost is
            // quadratic in the checkpoint count. Default PageRank tolerance on
            // this graph passes 9,313 checkpoints (~43M re-executed steps; the
            // test ran >30 min). A loose tolerance still converges genuinely
            // and exercises every checkpoint kind, over far fewer iterations.
            let pagerank = PageRankOptions::new(0.85, 100, 0.5, true).unwrap();
            let mut calls = vec![
                FnxCallSpec::pagerank(pagerank),
                FnxCallSpec::triangles(),
                FnxCallSpec::clustering_coefficient(),
                degree_call(),
                core_call(),
                eigenvector_call(),
            ];
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
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let db = small_database(&contexts.commit(), 3, false).await;
        let opt = options(Directedness::Undirected);
        let graph = projection(&db, &cx, opt).await;
        let root_before = db.read_session().unwrap().partition_root();
        for (call, quantum_size) in [
            (
                FnxCallSpec::single_source_shortest_path_length(VId(0), None),
                3,
            ),
            (FnxCallSpec::pagerank(PageRankOptions::default()), 3),
            (FnxCallSpec::triangles(), 3),
            (FnxCallSpec::clustering_coefficient(), 3),
            // Directory-only degree execution has fewer ticks than a scan.
            // Keep at least four revocation opportunities for this fixture.
            (degree_call(), 2),
            (core_call(), 3),
            (eigenvector_call(), 3),
        ] {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let measured = cx.with_checkpoint_probe(Arc::clone(&probe));
            let (expected, suspensions) = drive(
                call.execute_sealed_cooperative_with_checkpoint(
                    &measured,
                    &graph,
                    opt.execution_limits,
                    memory(),
                    quantum(quantum_size),
                    yield_now,
                    || Ok(()),
                ),
                &probe,
                quantum_size,
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
                    quantum(quantum_size),
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

fn degree_call() -> FnxCallSpec {
    FnxCallSpec::bind(
        "CALL fnx.degree_centrality() YIELD score AS rank,vertex AS id",
        &FnxParameters::new(),
    )
    .unwrap()
}

fn core_call() -> FnxCallSpec {
    FnxCallSpec::bind(
        "CALL fnx.core_number() YIELD core AS level,vertex AS id",
        &FnxParameters::new(),
    )
    .unwrap()
}

fn eigenvector_call() -> FnxCallSpec {
    FnxCallSpec::bind(
        "CALL fnx.eigenvector_centrality() YIELD score AS rank,vertex AS id",
        &FnxParameters::new(),
    )
    .unwrap()
}

#[test]
fn cooperative_degree_directory_yields_over_history_and_scalar_only_populations() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        for n in [4, 600] {
            let (db, original) = if n == 4 {
                database(&contexts.commit()).await
            } else {
                let db = small_database(&contexts.commit(), n, false).await;
                let at = db.frontier().unwrap();
                (db, at)
            };
            let mut cuts = vec![original];
            if db.frontier().unwrap() != original {
                cuts.push(db.frontier().unwrap());
            }
            for at in cuts {
                let mut opt = options(Directedness::Undirected);
                opt.as_of = Some(at);
                opt.projection.self_loops = SelfLoopPolicy::Drop;
                let graph = projection(&db, &cx, opt).await;
                assert_eq!(graph.node_count(), n);
                let call = degree_call();
                let columns = call.outputs().len() * size_of::<String>()
                    + call
                        .outputs()
                        .iter()
                        .map(|column| column.name.len())
                        .sum::<usize>();
                let bytes = columns
                    + n * (size_of::<Vec<fgdb_prism::FnxValue>>()
                        + call.outputs().len() * size_of::<fgdb_prism::FnxValue>());
                let exact_limits = FnxExecutionLimits {
                    max_iterations: 0,
                    max_result_rows: n,
                    max_estimated_work: n + 2,
                };
                let exact_memory = FnxMemoryLimits {
                    max_kernel_workspace_bytes: n * size_of::<f64>(),
                    max_result_bytes: bytes,
                };
                let sync = call
                    .execute_sealed(&cx, &graph, exact_limits, exact_memory)
                    .unwrap();
                let decoded = db
                    .read_session()
                    .unwrap()
                    .execute_fnx(&cx, &call, opt)
                    .unwrap();
                assert_eq!(sync.rows, decoded.analytics.rows);
                assert_eq!(
                    sync.certificate.result_digest,
                    decoded.analytics.certificate.result_digest
                );
                let mut previous = None;
                for q in [1, 7, 257] {
                    let probe = Arc::new(SimulationCheckpointProbe::new(None));
                    let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
                    let (actual, pending) = drive(
                        call.execute_sealed_cooperative(
                            &controlled,
                            &graph,
                            exact_limits,
                            exact_memory,
                            quantum(q),
                            yield_now,
                        ),
                        &probe,
                        q,
                    );
                    let actual = actual.unwrap();
                    compare_execution(&actual, &sync);
                    assert_eq!(actual.certificate.witness.edges_scanned, 0);
                    assert_eq!(
                        actual.certificate.execution_kernel,
                        "fgdb-prism/sealed-degree-directory-cooperative-v1"
                    );
                    if q == 1 {
                        assert!(
                            pending >= n * 2,
                            "degree reads and result rows must both yield"
                        );
                    }
                    if let Some(expected) = &previous {
                        assert_eq!(&actual, expected);
                    }
                    previous = Some(actual);
                }
                let prepared = previous.unwrap();
                let hosted = db
                    .execute_fnx_sealed_cooperative(
                        &cx,
                        &call,
                        opt,
                        exact_memory,
                        SealedLimits::default(),
                        quantum(7),
                    )
                    .await
                    .unwrap();
                assert_eq!(hosted.analytics, prepared);
                for resource in [
                    "result rows",
                    "estimated work",
                    "kernel workspace bytes",
                    "result bytes",
                ] {
                    let mut limits = exact_limits;
                    let mut memory = exact_memory;
                    match resource {
                        "result rows" => limits.max_result_rows -= 1,
                        "estimated work" => limits.max_estimated_work -= 1,
                        "kernel workspace bytes" => memory.max_kernel_workspace_bytes -= 1,
                        "result bytes" => memory.max_result_bytes -= 1,
                        _ => unreachable!(),
                    }
                    assert!(
                        matches!(
                            call.execute_sealed_cooperative(
                                &cx, &graph, limits, memory, quantum(1),
                                || -> std::future::Ready<()> {
                                    panic!("degree admission refusal must precede yielding")
                                },
                            ).await,
                            Err(FnxSealedExecutionError::Execution(
                                FnxExecutionError::LimitExceeded { resource: observed, .. }
                            )) if observed == resource
                        ),
                        "{resource}"
                    );
                }
            }
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
