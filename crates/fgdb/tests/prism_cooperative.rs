//! Production Chronicle -> Strata -> Prism pulls, paused inside raw history.
//! Decoded projections are independent output oracles, not source substitutes.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_prism::{
    Directedness, FnxExecutionLimits, FnxReadOptions, FnxSelection, FnxSourceLimits,
    FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy, ProjectionLimits,
    ProjectionSpec, SealedGraphView, SelfLoopPolicy,
};
use fgdb_strata::tiered::sealed::{SealedLimits, SealedScanBudget, SealedScanStep};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, QueryCx, VId,
};
use std::sync::Arc;

fn options(direction: Directedness) -> FnxReadOptions {
    FnxReadOptions {
        as_of: None,
        selection: FnxSelection {
            vertex_label: Some(LabelId(1)), relation: Some(RelationId(1)),
            weight: FnxWeightSpec::Property {
                key: PropertyKeyId(1), missing: MissingWeightPolicy::Reject,
            },
        },
        projection: ProjectionSpec {
            directedness: direction, parallel_edges: ParallelEdgePolicy::Sum,
            self_loops: SelfLoopPolicy::Keep,
        },
        source_limits: FnxSourceLimits {
            max_work_units: 1_000_000, max_scratch_entries: 100_000,
            max_staging_bytes: 1 << 24,
        },
        projection_limits: ProjectionLimits {
            max_vertices: 1000, max_input_edges: 10_000,
            max_adjacency_entries: 10_000, max_workspace_bytes: 1 << 26,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 1000, max_result_rows: 1000, max_estimated_work: 1 << 28,
        },
    }
}

async fn database(cx: &CommitCx) -> (Database<MemVfs>, CommitSeq) {
    let keys = DatabaseKeys::new(
        [0x81; 32], DatabaseSecurityNamespaceId([0x82; 32]), [0x83; 32],
    );
    let mut db = Database::<MemVfs>::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in [0, 1, 2, u128::MAX] {
        batch.create_vertex(VId(id), vec![LabelId(1)], vec![]);
    }
    batch.create_vertex(VId(99), vec![LabelId(2)], vec![]);
    for (id, source, target, weight) in [
        (1, 0, 1, 9_007_199_254_740_992i64), (2, 1, 0, 1), (3, 1, 0, 1),
        (4, 0, 0, 7), (5, 1, 2, 3), (6, 2, u128::MAX, 5),
    ] {
        batch.add_edge(EId(id), VId(source), VId(target),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))]);
    }
    // A long excluded group must consume raw fuel but never inspect Bool weights.
    // It also crosses the incoming index's 256-entry chunk boundaries.
    for id in 100..800 {
        batch.add_edge(EId(id), VId(99), VId(0),
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))]);
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
    db: &Database<MemVfs>, cx: &QueryCx, options: FnxReadOptions,
) -> SealedGraphView {
    db.prism_sealed_projection_at(
        cx, options.as_of, options.selection, options.projection,
        options.projection_limits, options.source_limits, SealedLimits::default(),
    ).await.unwrap()
}

fn row(
    graph: &SealedGraphView, cx: &QueryCx, source: usize, lower: Option<VId>,
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
        assert!(matches!(cursor.next_budgeted(cx, &mut SealedScanBudget::new(0)).unwrap(),
            SealedScanStep::Yield));
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
            SealedScanStep::End => { ended = true; break; }
        }
    }
    assert!(ended, "bounded pulls must converge even with quantum one");
    assert!(matches!(cursor.next_budgeted(cx, &mut SealedScanBudget::new(0)).unwrap(),
        SealedScanStep::End));
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
            for direction in [Directedness::Directed, Directedness::Reversed, Directedness::Undirected] {
                for reduction in [ParallelEdgePolicy::Sum, ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum, ParallelEdgePolicy::CollapseUnit] {
                    for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                        let mut opt = options(direction);
                        opt.as_of = Some(at);
                        opt.projection.parallel_edges = reduction;
                        opt.projection.self_loops = loops;
                        let graph = projection(&db, &cx, opt).await;
                        let decoded = view.prism_projection_at(
                            &cx, at, opt.selection, opt.projection,
                            opt.projection_limits, opt.source_limits,
                        ).unwrap();
                        for source in 0..graph.node_count() {
                            let (targets, weights) = decoded.projected_row(source).unwrap();
                            for lower in [None, Some(VId(1)), Some(VId(u128::MAX))] {
                                let expected: Vec<_> = targets.iter().zip(weights)
                                    .map(|(&v, &w)| (decoded.vertex_id(v).unwrap(), w.to_bits()))
                                    .filter(|(v, _)| lower.is_none_or(|low| *v >= low)).collect();
                                let baseline = row(&graph, &cx, source, lower, usize::MAX);
                                assert_eq!(baseline.0, expected);
                                for quantum in [1, 2, 3, 7, 64, 255, 256, 257] {
                                    let actual = row(&graph, &cx, source, lower, quantum);
                                    assert_eq!(actual.0, expected, "{direction:?}, q={quantum}");
                                    assert_eq!(actual.1, baseline.1,
                                        "pausing must neither repeat nor skip raw work");
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
        assert!(small.2 > 700, "hidden endpoints/history must yield without producing neighbors");
        assert_eq!(small.0, expected.0);
        let big = 9_007_199_254_740_992f64;
        assert_ne!(((big + 1.0) + 1.0).to_bits(), (big + (1.0 + 1.0)).to_bits());
        assert_eq!(expected.0, vec![(VId(0), 7.0f64.to_bits()), (VId(1), big.to_bits())]);
        let mut cursor = graph.neighbor_cursor(&cx, 0).unwrap();
        assert!(matches!(cursor.next_budgeted(&cx, &mut SealedScanBudget::new(1)).unwrap(),
            SealedScanStep::Yield));
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
        while !matches!(cursor.next_budgeted(&measured, &mut SealedScanBudget::new(7)).unwrap(),
            SealedScanStep::End) {}
        let total = observe.calls();
        assert!(total > 700);
        for stop in 1..=total {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let controlled = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut cursor = graph.neighbor_cursor(&cx, 0).unwrap();
            loop {
                match cursor.next_budgeted(&controlled, &mut SealedScanBudget::new(7)) {
                    Ok(SealedScanStep::End) => panic!("cancelled scan cannot report successful EOF"),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            assert_eq!(probe.calls(), stop);
            assert!(matches!(cursor.next_budgeted(&cx, &mut SealedScanBudget::new(7)).unwrap(),
                SealedScanStep::End));
        }
        let mut cursor = graph.neighbor_cursor(&cx, 0).unwrap();
        assert!(matches!(cursor.next_budgeted(&cx, &mut SealedScanBudget::new(1)).unwrap(),
            SealedScanStep::Yield));
        let cancelled = cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
        assert!(cursor.next_budgeted(&cancelled, &mut SealedScanBudget::new(0)).is_err());
        assert!(cursor.next(&cx).unwrap().is_none());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
