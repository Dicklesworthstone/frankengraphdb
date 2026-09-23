//! Real stored-source coverage of incoming and merged compressed analytics.
//! No test constructs a source anchor or substitutes an in-memory edge oracle
//! for the production Chronicle -> Strata -> Prism path.

use super::*;
use fgdb_prism::{FnxGraphKind, GraphView, SnapshotGraphView};

const DIRECTIONS: [Directedness; 3] = [
    Directedness::Directed,
    Directedness::Reversed,
    Directedness::Undirected,
];

fn projected(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    image: &SealedPartition,
    opt: FnxReadOptions,
) -> SealedGraphView {
    view.prism_sealed_projection_at(
        cx, image, opt.as_of.unwrap_or(view.frontier()), opt.selection,
        opt.projection, opt.projection_limits, opt.source_limits,
    ).unwrap()
}

fn cursor_row(
    graph: &SealedGraphView,
    cx: &QueryCx,
    source: usize,
    lower: Option<VId>,
) -> Vec<(VId, u64)> {
    let mut cursor = graph.neighbor_cursor_from(cx, source, lower).unwrap();
    let mut row = Vec::new();
    while let Some((target, weight)) = cursor.next(cx).unwrap() {
        row.push((graph.vertex_id(target).unwrap(), weight.to_bits()));
    }
    assert!(cursor.next(cx).unwrap().is_none());
    row
}

fn assert_projection(
    view: &EmbeddedReadView,
    cx: &QueryCx,
    image: &SealedPartition,
    opt: FnxReadOptions,
) -> (SealedGraphView, SnapshotGraphView) {
    let graph = projected(view, cx, image, opt);
    let decoded = view.prism_projection_at(
        cx, opt.as_of.unwrap_or(view.frontier()), opt.selection,
        opt.projection, opt.projection_limits, opt.source_limits,
    ).unwrap();
    assert_eq!(graph.vertex_ids(), decoded.vertex_ids());
    assert_eq!(graph.edge_count(), decoded.edge_count());
    let mut arcs = 0;
    for source in 0..graph.node_count() {
        let (targets, weights) = decoded.projected_row(source).unwrap();
        let expected: Vec<_> = targets.iter().zip(weights).map(|(&target, &weight)| {
            (decoded.vertex_id(target).unwrap(), weight.to_bits())
        }).collect();
        assert_eq!(cursor_row(&graph, cx, source, None), expected,
            "source={source} laws={:?}", opt.projection);
        assert_eq!(graph.degree(source), Some(expected.len()));
        arcs += expected.len();
    }
    assert_eq!(graph.adjacency_entry_count(), arcs);
    assert!(graph.shares_storage_with(image));
    assert_eq!(graph.incoming_index_stats().is_some(),
        opt.projection.directedness != Directedness::Directed);
    (graph, decoded)
}

#[test]
fn actual_mixed_tiers_support_all_directions_reductions_and_registered_calls() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
        let mut batch = fixture();
        batch.add_edge(EId(90), VId(2), VId(1),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(3))]);
        batch.add_edge(EId(1001), VId(3), VId(3),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(2))]);
        db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let image = db.store.seal_partition(&query, view.partition_root(), view.frontier(),
            SealedLimits::default()).await.unwrap();
        assert_eq!(image.storage_kind(VId(1), RelationId(1)), Some(RowStorageKind::SealedCsr));
        for direction in DIRECTIONS {
            for reduction in [ParallelEdgePolicy::CollapseUnit, ParallelEdgePolicy::Minimum,
                ParallelEdgePolicy::Maximum, ParallelEdgePolicy::Sum] {
                for loops in [SelfLoopPolicy::Keep, SelfLoopPolicy::Drop] {
                    let mut opt = options();
                    opt.projection = ProjectionSpec {
                        directedness: direction, parallel_edges: reduction, self_loops: loops,
                    };
                    let (graph, decoded) = assert_projection(&view, &query, &image, opt);
                    // 16 selected non-loops plus one loop; the excluded Bool edge
                    // is never selected. The sealed input count includes dropped loops.
                    assert_eq!(graph.input_edge_count(), 17);
                    let component = if direction == Directedness::Undirected {
                        "CALL fnx.connected_components() YIELD component AS group_id,vertex"
                    } else {
                        "CALL fnx.strongly_connected_components() YIELD component AS group_id,vertex"
                    };
                    for text in [DIJKSTRA,
                        "CALL fnx.single_source_shortest_path_length($source) YIELD distance,vertex",
                        "CALL fnx.pagerank(0.85,1000,1e-9,true) YIELD score AS rank,vertex",
                        component] {
                        let call = FnxCallSpec::bind(text, &parameters()).unwrap();
                        let expected = call.execute(&decoded, opt.execution_limits,
                            || query.checkpoint()).unwrap();
                        let actual = view.call_fnx_sealed(&query, &image, text,
                            &parameters(), opt, memory()).unwrap();
                        assert_eq!(actual.analytics.rows, expected.rows, "{direction:?} {text}");
                        assert_eq!(actual.analytics.columns, expected.columns);
                        assert_eq!(actual.analytics.certificate.result_digest,
                            expected.certificate.result_digest);
                        assert_eq!(actual.analytics.certificate.edges, graph.edge_count());
                        assert_eq!(actual.analytics.certificate.adapter, AdapterPath::CompressedCursor);
                    }
                    if direction != Directedness::Undirected {
                        let call = FnxCallSpec::weakly_connected_components();
                        assert_eq!(call.execute_sealed(&query, &graph, opt.execution_limits,
                            memory()).unwrap().rows,
                            call.execute(&decoded, opt.execution_limits,
                                || query.checkpoint()).unwrap().rows);
                    }
                }
            }
            let mut opt = options();
            opt.projection.directedness = direction;
            let expected = view.execute_fnx_sealed(&query, &image, &dijkstra_call(), opt,
                memory()).unwrap();
            assert_eq!(db.call_fnx_sealed(&query, DIJKSTRA, &parameters(), opt,
                memory(), SealedLimits::default()).await.unwrap(), expected);
        }
    });
}

#[test]
fn reciprocal_incidence_eid_order_and_loop_ownership_are_observable() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for vertex in [VId(1), VId(2), VId(u128::MAX)] {
            batch.create_vertex(vertex, vec![LabelId(1)], vec![]);
        }
        let big = 9_007_199_254_740_992i64;
        for (eid, source, target, weight) in [(1, 1, 2, big), (2, 2, 1, 1),
            (3, 2, 1, 1), (4, 1, 1, 7)] {
            batch.add_edge(EId(eid), VId(source), VId(target),
                vec![(PropertyKeyId(1), CanonicalScalar::Int(weight))]);
        }
        db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let image = db.store.seal_partition(&query, view.partition_root(), view.frontier(),
            SealedLimits::default()).await.unwrap();
        let mut opt = options();
        opt.projection.directedness = Directedness::Undirected;
        opt.projection.parallel_edges = ParallelEdgePolicy::Sum;
        let (graph, _) = assert_projection(&view, &query, &image, opt);
        let canonical = (big as f64 + 1.0) + 1.0;
        let regrouped = big as f64 + (1.0 + 1.0);
        assert_ne!(canonical.to_bits(), regrouped.to_bits(), "negative control is discriminating");
        assert_eq!(cursor_row(&graph, &query, 0, None),
            vec![(VId(1), 7.0f64.to_bits()), (VId(2), canonical.to_bits())]);
        assert_eq!(cursor_row(&graph, &query, 1, None), vec![(VId(1), canonical.to_bits())]);
        assert_eq!((graph.input_edge_count(), graph.edge_count(), graph.adjacency_entry_count()),
            (4, 2, 3));
        opt.projection.self_loops = SelfLoopPolicy::Drop;
        opt.projection.directedness = Directedness::Reversed;
        let (reversed, _) = assert_projection(&view, &query, &image, opt);
        assert_eq!(cursor_row(&reversed, &query, 0, None), vec![(VId(2), 2.0f64.to_bits())]);
        assert_eq!(cursor_row(&reversed, &query, 1, None), vec![(VId(1), (big as f64).to_bits())]);
        opt.projection.directedness = Directedness::Undirected;
        opt.projection.parallel_edges = ParallelEdgePolicy::Reject;
        assert!(matches!(view.prism_sealed_projection_at(&query, &image, view.frontier(),
            opt.selection, opt.projection, opt.projection_limits, opt.source_limits),
            Err(SealedReadError::Projection(SealedProjectionError::Projection(
                ProjectionError::ParallelEdge { source: VId(1), target: VId(2) })))));
    });
}

#[test]
fn every_three_vertex_simple_directed_topology_agrees_after_compressed_projection() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let query = contexts.query();
        let commit = contexts.commit();
        let vertices = [VId(0), VId(1 << 80), VId(u128::MAX)];
        let pairs = [(0, 1), (0, 2), (1, 0), (1, 2), (2, 0), (2, 1)];
        for mask in 0..64 {
            let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for vertex in vertices { batch.create_vertex(vertex, vec![LabelId(1)], vec![]); }
            for (bit, &(source, target)) in pairs.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    batch.add_edge(EId(bit as u128), vertices[source], vertices[target],
                        vec![(PropertyKeyId(1), CanonicalScalar::Int((bit % 3) as i64))]);
                }
            }
            db.write(&commit, batch).await.unwrap();
            let view = db.read_session().unwrap();
            let image = db.store.seal_partition(&query, view.partition_root(), view.frontier(),
                SealedLimits::default()).await.unwrap();
            for direction in DIRECTIONS {
                let mut opt = options();
                opt.projection.directedness = direction;
                let (graph, decoded) = assert_projection(&view, &query, &image, opt);
                for source in vertices {
                    let call = FnxCallSpec::single_source_shortest_path_length(source, None);
                    assert_eq!(call.execute_sealed(&query, &graph, opt.execution_limits,
                        memory()).unwrap().rows,
                        call.execute(&decoded, opt.execution_limits,
                            || query.checkpoint()).unwrap().rows, "mask={mask} {direction:?}");
                }
                let call = if direction == Directedness::Undirected {
                    FnxCallSpec::connected_components()
                } else { FnxCallSpec::strongly_connected_components() };
                assert_eq!(call.execute_sealed(&query, &graph, opt.execution_limits,
                    memory()).unwrap().rows,
                    call.execute(&decoded, opt.execution_limits, || query.checkpoint()).unwrap().rows);
            }
        }
    });
}

#[test]
fn incoming_indices_preserve_historical_seeks_full_width_neighbors_and_owned_pins() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
        let mut batch = small_fixture();
        batch.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]);
        batch.add_edge(EId(3), VId(u128::MAX), VId(2),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(9))]);
        db.write(&commit, batch).await.unwrap();
        let old = db.frontier().unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(5)));
        update.delete_edge(EId(3));
        db.write(&commit, update).await.unwrap();
        let view = db.read_session().unwrap();
        let image = db.store.seal_partition(&query, view.partition_root(), old,
            SealedLimits::default()).await.unwrap();
        let bytes = image.encode(&query, SealedLimits::default()).unwrap();
        let reloaded = SealedPartition::reload(&query, image.anchor(), &bytes,
            SealedLimits::default(), None).unwrap();
        let mut retained = Vec::new();
        for direction in [Directedness::Reversed, Directedness::Undirected] {
            for at in [old, view.frontier()] {
                let mut opt = options();
                opt.as_of = Some(at);
                opt.projection.directedness = direction;
                let (graph, _) = assert_projection(&view, &query, &reloaded, opt);
                for source in 0..graph.node_count() {
                    let all = cursor_row(&graph, &query, source, None);
                    for lower in [VId(0), VId(2), VId(3), VId(u128::MAX)] {
                        let expected: Vec<_> = all.iter().copied().filter(|(id, _)| *id >= lower).collect();
                        assert_eq!(cursor_row(&graph, &query, source, Some(lower)), expected);
                    }
                }
                if direction == Directedness::Reversed {
                    let row = cursor_row(&graph, &query, graph.vertex_ordinal(VId(2)).unwrap(), None);
                    assert_eq!(row, if at == old {
                        vec![(VId(1), 2.0f64.to_bits()), (VId(u128::MAX), 9.0f64.to_bits())]
                    } else { vec![(VId(1), 5.0f64.to_bits())] });
                }
                let call = FnxCallSpec::single_source_shortest_path_length(VId(2), None);
                let expected = call.execute_sealed(&query, &graph, opt.execution_limits, memory()).unwrap();
                retained.push((graph, call, opt.execution_limits, expected));
            }
        }
        drop(db);
        drop(view);
        drop(image);
        drop(reloaded);
        for (graph, call, limits, expected) in retained {
            assert_eq!(call.execute_sealed(&query, &graph.clone(), limits, memory()).unwrap(), expected);
        }
    });
}

#[test]
fn direction_specific_admission_counts_construction_workspace_and_both_incidence_faces() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, small_fixture()).await.unwrap();
        let view = db.read_session().unwrap();
        let image = db.store.seal_partition(&query, view.partition_root(), view.frontier(),
            SealedLimits::default()).await.unwrap();
        let mut opt = options();
        opt.projection_limits.max_workspace_bytes = 3 * (std::mem::size_of::<VId>()
            + std::mem::size_of::<usize>());
        let forward = projected(&view, &query, &image, opt);
        assert_eq!(forward.charged_workspace_bytes(), opt.projection_limits.max_workspace_bytes);
        opt.projection.directedness = Directedness::Reversed;
        assert!(matches!(view.prism_sealed_projection_at(&query, &image, view.frontier(),
            opt.selection, opt.projection, opt.projection_limits, opt.source_limits),
            Err(SealedReadError::Projection(SealedProjectionError::Read(SealedError::Limit {
                resource: "incoming workspace bytes", .. })))));
        for direction in [Directedness::Reversed, Directedness::Undirected] {
            let mut opt = options();
            opt.projection.directedness = direction;
            let graph = projected(&view, &query, &image, opt);
            let index = graph.incoming_index_stats().unwrap();
            assert_eq!(graph.charged_workspace_bytes(), forward.charged_workspace_bytes()
                + index.charged_workspace_bytes);
            assert!(index.charged_workspace_bytes > index.charged_resident_bytes);
            opt.projection_limits.max_workspace_bytes = graph.charged_workspace_bytes();
            projected(&view, &query, &image, opt);
            opt.projection_limits.max_workspace_bytes -= 1;
            assert!(view.prism_sealed_projection_at(&query, &image, view.frontier(), opt.selection,
                opt.projection, opt.projection_limits, opt.source_limits).is_err());
            let call = FnxCallSpec::single_source_shortest_path_length(VId(3), None);
            let result = call.execute_sealed(&query, &graph, opt.execution_limits, memory()).unwrap();
            let exact = FnxExecutionLimits { max_estimated_work: result.certificate.estimated_work,
                ..opt.execution_limits };
            assert_eq!(call.execute_sealed(&query, &graph, exact, memory()).unwrap(), result);
            assert!(matches!(call.execute_sealed(&query, &graph, FnxExecutionLimits {
                max_estimated_work: exact.max_estimated_work - 1, ..exact }, memory()),
                Err(FnxSealedExecutionError::Execution(FnxExecutionError::LimitExceeded {
                    resource: "estimated work", .. }))));
        }
        opt = options();
        opt.projection.directedness = Directedness::Undirected;
        opt.projection_limits.max_adjacency_entries = 3;
        assert!(matches!(view.prism_sealed_projection_at(&query, &image, view.frontier(),
            opt.selection, opt.projection, opt.projection_limits, opt.source_limits),
            Err(SealedReadError::Projection(SealedProjectionError::Projection(
                ProjectionError::LimitExceeded { resource: "adjacency entries", observed: 4, .. })))));
        opt.projection_limits.max_adjacency_entries = 4;
        let graph = projected(&view, &query, &image, opt);
        assert_eq!((graph.edge_count(), graph.adjacency_entry_count()), (2, 4));
        opt.source_limits.max_work_units = 0;
        assert!(matches!(db.execute_fnx_sealed(&query, &FnxCallSpec::strongly_connected_components(),
            opt, memory(), SealedLimits::default()).await,
            Err(SealedReadError::Execution(FnxSealedExecutionError::Execution(
                FnxExecutionError::GraphKind { required: FnxGraphKind::Directed })))));
        opt.projection.directedness = Directedness::Reversed;
        assert!(matches!(db.execute_fnx_sealed(&query, &FnxCallSpec::connected_components(),
            opt, memory(), SealedLimits::default()).await,
            Err(SealedReadError::Execution(FnxSealedExecutionError::Execution(
                FnxExecutionError::GraphKind { required: FnxGraphKind::Undirected })))));
    });
}

#[test]
fn incoming_endpoint_masks_and_loop_discard_precede_weight_observation() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
        let mut batch = small_fixture();
        batch.create_vertex(VId(99), vec![LabelId(2)], vec![]);
        batch.add_edge(EId(3), VId(99), VId(2),
            vec![(PropertyKeyId(1), CanonicalScalar::Bool(true))]);
        batch.add_edge(EId(4), VId(2), VId(2), vec![]);
        db.write(&commit, batch).await.unwrap();
        for direction in [Directedness::Reversed, Directedness::Undirected] {
            let mut opt = options();
            opt.projection.directedness = direction;
            opt.projection.self_loops = SelfLoopPolicy::Drop;
            let graph = db.prism_sealed_projection_at(&query, None, opt.selection,
                opt.projection, opt.projection_limits, opt.source_limits,
                SealedLimits::default()).await.unwrap();
            assert!(!cursor_row(&graph, &query, graph.vertex_ordinal(VId(2)).unwrap(), None).is_empty());
            opt.selection.vertex_label = None;
            assert!(matches!(db.prism_sealed_projection_at(&query, None, opt.selection,
                opt.projection, opt.projection_limits, opt.source_limits,
                SealedLimits::default()).await,
                Err(SealedReadError::Projection(SealedProjectionError::Weight {
                    edge: EId(3), reason: FnxWeightError::NotNumeric }))));
            opt.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
            opt.projection.self_loops = SelfLoopPolicy::Keep;
            db.prism_sealed_projection_at(&query, None, opt.selection, opt.projection,
                opt.projection_limits, opt.source_limits, SealedLimits::default()).await.unwrap();
            opt.projection.self_loops = SelfLoopPolicy::Reject;
            assert!(matches!(db.prism_sealed_projection_at(&query, None, opt.selection,
                opt.projection, opt.projection_limits, opt.source_limits,
                SealedLimits::default()).await,
                Err(SealedReadError::Projection(SealedProjectionError::Projection(
                    ProjectionError::SelfLoop(EId(4)))))));
        }
    });
}

#[test]
fn every_direction_build_cursor_and_connected_call_checkpoint_is_terminal() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let query = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::<MemVfs>::open_memory(&commit, keys()).await.unwrap();
        let mut batch = small_fixture();
        batch.add_edge(EId(3), VId(3), VId(1),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(4))]);
        db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let image = db.store.seal_partition(&query, view.partition_root(), view.frontier(),
            SealedLimits::default()).await.unwrap();
        for direction in [Directedness::Reversed, Directedness::Undirected] {
            let mut opt = options();
            opt.projection.directedness = direction;
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = query.with_checkpoint_probe(Arc::clone(&probe));
            let graph = projected(&view, &observed, &image, opt);
            let count = probe.calls();
            assert!(count > 20);
            for stop in 1..=count {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
                assert!(view.prism_sealed_projection_at(&controlled, &image, view.frontier(),
                    opt.selection, opt.projection, opt.projection_limits, opt.source_limits).is_err());
                assert_eq!(probe.calls(), stop);
            }
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = query.with_checkpoint_probe(Arc::clone(&probe));
            cursor_row(&graph, &observed, 0, None);
            for stop in 1..=probe.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
                if let Ok(mut cursor) = graph.neighbor_cursor(&controlled, 0) {
                    loop {
                        match cursor.next(&controlled) {
                            Ok(Some(_)) => {}
                            Err(_) => break,
                            Ok(None) => panic!("interruption became clean EOF at {stop}"),
                        }
                    }
                    assert!(cursor.next(&query).unwrap().is_none(), "a failed cursor cannot resume");
                }
                assert_eq!(probe.calls(), stop);
            }
            let call = if direction == Directedness::Undirected {
                FnxCallSpec::connected_components()
            } else { FnxCallSpec::strongly_connected_components() };
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = query.with_checkpoint_probe(Arc::clone(&probe));
            let expected = call.execute_sealed(&observed, &graph, opt.execution_limits, memory()).unwrap();
            for stop in 1..=probe.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let controlled = query.with_checkpoint_probe(Arc::clone(&probe));
                assert!(matches!(call.execute_sealed(&controlled, &graph, opt.execution_limits, memory()),
                    Err(FnxSealedExecutionError::Cancelled(_))));
                assert_eq!(probe.calls(), stop);
            }
            assert_eq!(call.execute_sealed(&query, &graph, opt.execution_limits, memory()).unwrap(), expected);
        }
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
}
