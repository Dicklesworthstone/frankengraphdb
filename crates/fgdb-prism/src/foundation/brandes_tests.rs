use super::*;
use crate::{
    Directedness, FnxCallSpec, FnxExecutionLimits, FnxParameters, ParallelEdgePolicy,
    ProjectionEdge, ProjectionLimits, ProjectionSpec, SelfLoopPolicy, SnapshotBinding,
};
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use fnx_classes::Graph;
use std::convert::Infallible;

struct Unstoppable;
impl FnxMeter<Infallible> for Unstoppable {
    fn unit(&mut self) -> Result<(), Infallible> {
        panic!("native arithmetic must not charge logical work")
    }
    fn poll(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}

fn graph(n: usize, edges: &[(usize, usize)]) -> SnapshotGraphView {
    let id = |at: usize| VId((1u128 << 110) + ((at as u128) << 64) + 17);
    let vertices: Vec<_> = (0..n).rev().map(id).collect();
    let edges: Vec<_> = edges
        .iter()
        .rev()
        .enumerate()
        .map(|(at, &(source, target))| ProjectionEdge {
            eid: EId(at as u128 + 1),
            source: id(target),
            target: id(source),
            // This registered procedure is unweighted.
            weight: if at % 2 == 0 { -3.0 } else { 11.0 },
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([17; 32]),
            as_of: CommitSeq(29),
        },
        &vertices,
        &edges,
        ProjectionSpec {
            directedness: Directedness::Undirected,
            parallel_edges: ParallelEdgePolicy::Reject,
            self_loops: SelfLoopPolicy::Keep,
        },
        ProjectionLimits {
            max_vertices: n,
            max_input_edges: edges.len(),
            max_adjacency_entries: edges.len() * 2,
            max_workspace_bytes: 1 << 28,
        },
    )
    .unwrap()
}

fn reference(graph: &SnapshotGraphView) -> fnx_algorithms::BetweennessCentralityResult {
    let mut copy = Graph::strict();
    let names = graph.nodes_ordered();
    for &name in &names {
        copy.add_node(name);
    }
    for source in 0..graph.node_count() {
        for &target in graph.neighbors_indices(source).unwrap() {
            if source < target {
                copy.add_edge(names[source], names[target]).unwrap();
            }
        }
    }
    fnx_algorithms::betweenness_centrality(&copy)
}

fn bits(output: &KernelOutput) -> Vec<u64> {
    let KernelValues::Scores(scores) = &output.values else {
        panic!("Brandes must return scores");
    };
    scores.iter().map(|score| score.to_bits()).collect()
}

fn largest_component(graph: &SnapshotGraphView) -> usize {
    let mut unseen = vec![true; graph.node_count()];
    let mut largest = 0;
    for source in 0..graph.node_count() {
        if !unseen[source] {
            continue;
        }
        unseen[source] = false;
        let mut queue = std::collections::VecDeque::from([source]);
        let mut reached = 0;
        while let Some(vertex) = queue.pop_front() {
            reached += 1;
            for &neighbor in graph.neighbors_indices(vertex).unwrap() {
                if unseen[neighbor] {
                    unseen[neighbor] = false;
                    queue.push_back(neighbor);
                }
            }
        }
        largest = largest.max(reached);
    }
    largest
}

fn assert_reference(graph: &SnapshotGraphView, schedules: &[(usize, usize)]) {
    let expected = reference(graph);
    let expected_bits: Vec<_> = expected
        .scores
        .iter()
        .map(|score| score.score.to_bits())
        .collect();
    for &(threads, window) in schedules {
        let (actual, workspace) =
            run_with_schedule(graph, &mut Unstoppable, threads, window).unwrap();
        assert_eq!(
            bits(&actual),
            expected_bits,
            "threads {threads}, window {window}"
        );
        assert_eq!(actual.row_count, graph.node_count());
        assert_eq!(actual.witness, expected.witness);
        let n = graph.node_count();
        let window = window.clamp(1, n.max(1));
        let logical_workers = window.min(MAX_GATHER_THREADS);
        // Count each retained allocation independently: output, the four
        // shared component directory vectors, descriptors, sigma/distance/
        // queue per worker, and component-sized scalar result windows.
        let expected_workspace = if n == 0 {
            0
        } else {
            n * size_of::<f64>()
                + (4 * n + 1) * size_of::<usize>()
                + logical_workers
                    * (size_of::<Worker>() + n * (size_of::<f64>() + 2 * size_of::<usize>()))
                + window * largest_component(graph) * size_of::<f64>()
        };
        assert_eq!(workspace, expected_workspace);
    }
}

#[test]
fn all_small_topologies_match_the_exact_pinned_brandes_bits_and_witness() {
    for n in 0..=5 {
        let pairs: Vec<_> = (0..n)
            .flat_map(|source| (source + 1..n).map(move |target| (source, target)))
            .collect();
        for mask in 0..1usize << pairs.len() {
            let edges: Vec<_> = pairs
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, &pair)| pair)
                .collect();
            assert_reference(&graph(n, &edges), &[(1, 1)]);
        }
    }
}

#[test]
fn worker_counts_and_result_windows_preserve_parallel_fnx_source_reduction() {
    for n in [31, 499, 500, 513] {
        let path: Vec<_> = (1..n).map(|v| (v - 1, v)).collect();
        let hub: Vec<_> = (1..n).map(|v| (0, v)).collect();
        let mut mixed = Vec::new();
        for source in 0..n - 1 {
            for gap in [1, 3, 7] {
                let target = source + gap;
                if target < n - 1 && source / 97 == target / 97 {
                    mixed.push((source, target));
                }
            }
        }
        for edges in [path, hub, mixed] {
            assert_reference(&graph(n, &edges), &[(1, 1), (2, 7), (4, 37), (8, n)]);
        }
    }
}

#[test]
fn production_policy_replays_workspace_bits_and_witness_across_cpu_counts() {
    assert_eq!(source_window::<Infallible>(0).unwrap(), 1);
    assert_eq!(source_window::<Infallible>(499).unwrap(), 1);
    assert_eq!(source_window::<Infallible>(500).unwrap(), 500);
    for n in [513, 2896, 2897, 10_000, 1_000_000] {
        let window = source_window::<Infallible>(n).unwrap();
        assert!((1..=n).contains(&window));
        assert!(window * n * size_of::<f64>() <= DELTA_WINDOW_BYTES);
    }
    assert!(matches!(
        source_window::<Infallible>(usize::MAX),
        Err(FnxExecutionError::SizeOverflow)
    ));

    for n in [0usize, 1, 499, 500, 513] {
        let edges: Vec<_> = (1..n.saturating_sub(1)).map(|v| (0, v)).collect();
        let graph = graph(n, &edges);
        let expected = run_with_threads(&graph, &mut Unstoppable, 1).unwrap();
        for threads in [2, 8] {
            let (actual, workspace) = run_with_threads(&graph, &mut Unstoppable, threads).unwrap();
            assert_eq!(bits(&actual), bits(&expected.0));
            assert_eq!(actual.witness, expected.0.witness);
            assert_eq!(workspace, expected.1, "n={n}, physical threads={threads}");
        }
        let window = source_window::<Infallible>(n).unwrap();
        assert_reference(&graph, &[(1, window), (2, window), (8, window)]);
    }
}

/// Independent predecessor-list formulation. Unlike the native kernel it
/// records every predecessor during BFS and owns a separate reverse stack.
fn source_delta(graph: &SnapshotGraphView, source: usize) -> Vec<f64> {
    use std::collections::VecDeque;
    let n = graph.node_count();
    let mut predecessors = vec![Vec::new(); n];
    let mut sigma = vec![0.0; n];
    let mut distance = vec![None; n];
    let mut dependency = vec![0.0; n];
    let mut stack = Vec::new();
    let mut queue = VecDeque::from([source]);
    sigma[source] = 1.0;
    distance[source] = Some(0);
    while let Some(vertex) = queue.pop_front() {
        stack.push(vertex);
        let next_distance = distance[vertex].unwrap() + 1;
        let sigma_vertex = sigma[vertex];
        for &neighbor in graph.neighbors_indices(vertex).unwrap() {
            if distance[neighbor].is_none() {
                distance[neighbor] = Some(next_distance);
                queue.push_back(neighbor);
            }
            if distance[neighbor] == Some(next_distance) {
                sigma[neighbor] += sigma_vertex;
                predecessors[neighbor].push(vertex);
            }
        }
    }
    while let Some(vertex) = stack.pop() {
        let sigma_vertex = sigma[vertex];
        let delta_vertex = dependency[vertex];
        for &predecessor in &predecessors[vertex] {
            dependency[predecessor] += (sigma[predecessor] / sigma_vertex) * (1.0 + delta_vertex);
        }
    }
    dependency[source] = 0.0;
    dependency
}

#[test]
fn csr_predecessors_match_each_source_before_any_score_reduction() {
    let n = 83;
    let mut edges = Vec::new();
    for source in 0..n {
        for target in source + 1..n {
            if target == source + 1 || (source * 17 + target * 31) % 19 < 3 {
                edges.push((source, target));
            }
        }
    }
    let graph = graph(n, &edges);
    let components = Components::new(&graph, &mut Unstoppable).unwrap();
    let mut worker = Worker::new(n, 1, components.largest, &mut Unstoppable).unwrap();
    for source in 0..n {
        worker.first_source = source;
        worker.sources = 1;
        worker.run(&graph, &components, &mut || true).unwrap();
        let actual: Vec<_> = worker.deltas.iter().map(|score| score.to_bits()).collect();
        let expected: Vec<_> = source_delta(&graph, source)
            .iter()
            .map(|score| score.to_bits())
            .collect();
        assert_eq!(actual, expected, "source {source}");
    }
}

#[test]
fn sparse_windows_preserve_source_order_across_disconnected_components_and_reuse() {
    // Components are interleaved in canonical vertex order. Reusing the same
    // worker across windows therefore switches repeatedly between two paths,
    // an isolated edge and isolated vertices instead of hiding stale state in
    // a component-contiguous source schedule.
    let n = 41;
    let graph = graph(n, &[(0, 7), (7, 14), (2, 9), (9, 16), (16, 23), (1, 8)]);
    let components = Components::new(&graph, &mut Unstoppable).unwrap();
    assert_eq!(components.largest, 4);
    assert!(!components.connected());
    let mut worker = Worker::new(n, 3, components.largest, &mut Unstoppable).unwrap();
    let capacity = worker.deltas.capacity();
    assert_eq!(capacity, 12);
    for first_source in (0..n).step_by(3) {
        worker.first_source = first_source;
        worker.sources = (n - first_source).min(3);
        worker.run(&graph, &components, &mut || true).unwrap();
        let mut actual = vec![0.0; n];
        let mut consumed = 0;
        for source in first_source..first_source + worker.sources {
            let component = components.range(source).unwrap();
            let end = consumed + component.len();
            for (&vertex, &value) in components.vertices[component]
                .iter()
                .zip(&worker.deltas[consumed..end])
            {
                actual[vertex] += value;
            }
            consumed = end;
        }
        assert_eq!(consumed, worker.deltas.len());
        let mut expected = vec![0.0; n];
        for source in first_source..first_source + worker.sources {
            for (score, dependency) in expected.iter_mut().zip(source_delta(&graph, source)) {
                *score += dependency;
            }
        }
        assert_eq!(
            actual.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            expected.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            "window begins at source {first_source}"
        );
        assert_eq!(worker.deltas.capacity(), capacity);
    }
    assert_reference(&graph, &[(1, 1), (1, 3), (2, 7), (8, n)]);
}

#[test]
fn isolates_and_small_components_do_not_clear_or_reduce_the_full_population_per_source() {
    let n = 8193;
    let pairs: Vec<_> = (0..n - 2)
        .step_by(3)
        .flat_map(|source| [(source, source + 1), (source + 1, source + 2)])
        .collect();
    for edges in [Vec::new(), pairs] {
        let graph = graph(n, &edges);
        let components = Components::new(&graph, &mut Unstoppable).unwrap();
        let expected_component = if edges.is_empty() { 1 } else { 3 };
        assert_eq!(components.largest, expected_component);
        assert_eq!(
            components.bytes::<Infallible>().unwrap(),
            (4 * n + 1) * size_of::<usize>()
        );
        let mut worker = Worker::new(n, 37, components.largest, &mut Unstoppable).unwrap();
        assert_eq!(worker.deltas.capacity(), 37 * expected_component);
        let mut nodes = 0;
        let mut scans = 0;
        let mut scalar_slots = 0;
        for first_source in (0..n).step_by(37) {
            worker.first_source = first_source;
            worker.sources = (n - first_source).min(37);
            let mut polls = 0;
            worker
                .run(&graph, &components, &mut || {
                    polls += 1;
                    true
                })
                .unwrap();
            // At most three reached vertices per source: initialization,
            // BFS, dependencies, reached-only reset and compact result capture
            // all fit in the same bounded arithmetic run for this window.
            // Three full V clears per source exceeded 200 polls per window.
            assert_eq!(polls, 2, "source {first_source}");
            nodes += worker.nodes_touched;
            scans += worker.edges_scanned;
            assert_eq!(worker.deltas.len(), worker.sources * expected_component);
            scalar_slots += worker.deltas.len();
        }
        if edges.is_empty() {
            assert_eq!(nodes, n);
            assert_eq!(scans, 0);
            assert_eq!(scalar_slots, n);
        } else {
            assert_eq!(nodes, 3 * n);
            assert_eq!(scans, 4 * n);
            assert_eq!(scalar_slots, 3 * n);
        }
    }
}

#[test]
fn non_associative_path_counts_keep_fnx_sigma_and_dependency_bits() {
    let width = 12;
    let layers = 27;
    let n = 1 + width * layers;
    let mut edges: Vec<_> = (1..=width).map(|target| (0, target)).collect();
    for layer in 1..layers {
        for left in 0..width {
            for right in 0..width {
                if (left * 13 + right * 7 + layer) % 11 < 9 {
                    edges.push((1 + (layer - 1) * width + left, 1 + layer * width + right));
                }
            }
        }
    }
    let graph = graph(n, &edges);
    let components = Components::new(&graph, &mut Unstoppable).unwrap();
    let mut worker = Worker::new(n, 1, components.largest, &mut Unstoppable).unwrap();
    worker.sources = 1;
    worker.run(&graph, &components, &mut || true).unwrap();
    assert!(
        worker
            .sigma
            .iter()
            .any(|&sigma| sigma > (1u64 << 53) as f64)
    );
    assert!(worker.sigma.iter().all(|sigma| sigma.is_finite()));
    let expected: Vec<_> = source_delta(&graph, 0)
        .iter()
        .map(|score| score.to_bits())
        .collect();
    assert_eq!(
        worker
            .deltas
            .iter()
            .map(|score| score.to_bits())
            .collect::<Vec<_>>(),
        expected
    );
    assert_reference(&graph, &[(1, 1), (4, 29), (8, n)]);
}

#[test]
fn cancellation_inside_a_hub_stops_at_the_admitted_scalar_edge_prefix() {
    let n = 8194;
    let graph = graph(n, &(1..n).map(|target| (0, target)).collect::<Vec<_>>());
    let components = Components::new(&graph, &mut Unstoppable).unwrap();
    let mut worker = Worker::new(n, 1, components.largest, &mut Unstoppable).unwrap();
    worker.sources = 1;
    // Three n-entry resets, one source setup and one FIFO pop precede the
    // first hub edge. Refuse the next live window after entering that row.
    let before_hub = 3 * n + 2;
    let accepted = (before_hub / ARITHMETIC_RUN + 1) * ARITHMETIC_RUN;
    let edges = accepted - before_hub;
    assert!(edges > 0 && edges < n - 1);
    let stop = accepted / ARITHMETIC_RUN + 1;
    let mut polls = 0;
    let result = worker.run(&graph, &components, &mut || {
        polls += 1;
        polls != stop
    });
    assert_eq!(result, Err(WorkerError::Stopped));
    assert_eq!(polls, stop);
    assert_eq!(worker.edges_scanned, edges);
    assert_eq!(worker.queue, (0..=edges).collect::<Vec<_>>());
    assert!(worker.sigma[..=edges].iter().all(|&sigma| sigma == 1.0));
    assert!(worker.sigma[edges + 1..].iter().all(|&sigma| sigma == 0.0));
    assert!(worker.deltas.iter().all(|&delta| delta == 0.0));
}

#[test]
fn parallel_brandes_joins_after_live_refusal_with_a_non_send_host_error() {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let n = 8194;
    let graph = graph(n, &(1..n).map(|target| (0, target)).collect::<Vec<_>>());
    let components = Components::new(&graph, &mut Unstoppable).unwrap();
    let mut workers: Vec<_> = (0..2)
        .map(|source| {
            let mut worker = Worker::new(n, 1, components.largest, &mut Unstoppable).unwrap();
            worker.first_source = source;
            worker.sources = 1;
            worker
        })
        .collect();
    let requests = [AtomicUsize::new(0), AtomicUsize::new(0)];
    let returned = AtomicUsize::new(0);
    let error = Rc::new(Cell::new(0));
    let parent = std::thread::current().id();
    struct Host<'a> {
        requests: &'a [AtomicUsize; 2],
        error: Rc<Cell<usize>>,
        parent: std::thread::ThreadId,
    }
    impl FnxMeter<Rc<Cell<usize>>> for Host<'_> {
        fn unit(&mut self) -> Result<(), Rc<Cell<usize>>> {
            panic!("parallel arithmetic must not charge work")
        }
        fn poll(&mut self) -> Result<(), Rc<Cell<usize>>> {
            assert_eq!(std::thread::current().id(), self.parent);
            if self.requests[0].load(Ordering::Acquire) >= 8 {
                self.error.set(1);
                Err(self.error.clone())
            } else {
                Ok(())
            }
        }
    }
    let result = arithmetic_rows(
        &mut workers,
        &[1, 2],
        &mut Host {
            requests: &requests,
            error: error.clone(),
            parent,
        },
        |low, group, poll| {
            for (offset, worker) in group.iter_mut().enumerate() {
                worker.error = worker
                    .run(&graph, &components, &mut || {
                        requests[low + offset].fetch_add(1, Ordering::Release);
                        poll()
                    })
                    .err();
                returned.fetch_add(1, Ordering::Release);
            }
        },
    );
    assert!(matches!(
        result,
        Err(FnxExecutionError::Cancelled(observed)) if Rc::ptr_eq(&observed, &error)
    ));
    assert_eq!(error.get(), 1);
    assert_eq!(returned.load(Ordering::Acquire), 2);
    assert_eq!(requests[0].load(Ordering::Acquire), 8);
    assert_eq!(workers[0].queue.len(), 4089);
    assert_eq!(workers[0].error, Some(WorkerError::Stopped));
}

#[test]
fn overflowing_shortest_path_counts_keep_the_finite_result_refusal() {
    for layers in [1024, 1025, 1030] {
        let n = 2 * layers + 1;
        let mut edges = vec![(0, 1), (0, 2)];
        for layer in 1..layers {
            for left in [2 * layer - 1, 2 * layer] {
                for right in [2 * layer + 1, 2 * layer + 2] {
                    edges.push((left, right));
                }
            }
        }
        let graph = graph(n, &edges);
        let components = Components::new(&graph, &mut Unstoppable).unwrap();
        let mut worker = Worker::new(n, 1, components.largest, &mut Unstoppable).unwrap();
        worker.sources = 1;
        if layers == 1024 {
            worker.run(&graph, &components, &mut || true).unwrap();
            assert!(worker.sigma.iter().all(|count| count.is_finite()));
            assert_eq!(worker.sigma[n - 1].to_bits(), 2.0f64.powi(1023).to_bits());
            assert_eq!(worker.deltas[0].to_bits(), 0.0f64.to_bits());
            // Each vertex lies on half the paths to each of the two vertices
            // in every later layer: its exact dependency is layers - layer.
            for layer in 1..=layers {
                let expected = (layers - layer) as f64;
                for vertex in [2 * layer - 1, 2 * layer] {
                    assert_eq!(worker.deltas[vertex].to_bits(), expected.to_bits());
                }
            }
            continue;
        }

        assert_eq!(
            worker.run(&graph, &components, &mut || true),
            Err(WorkerError::InvalidNumericResult)
        );
        if layers == 1025 {
            // Only the final layer overflows. The unchecked predecessor
            // oracle demonstrates why a final finite-score check misses it:
            // every dependency is finite, but the first layer loses one.
            let unchecked = source_delta(&graph, 0);
            assert!(unchecked.iter().all(|score| score.is_finite()));
            assert_eq!(unchecked[1].to_bits(), 1023.0f64.to_bits());
            assert_ne!(unchecked[1].to_bits(), 1024.0f64.to_bits());
            let call =
                FnxCallSpec::bind("CALL fnx.betweenness_centrality()", &FnxParameters::new())
                    .unwrap();
            assert!(matches!(
                call.execute(
                    &graph,
                    FnxExecutionLimits {
                        max_iterations: 0,
                        max_result_rows: n,
                        max_estimated_work: (n + 1) * (n + 2 * edges.len()),
                    },
                    || Ok::<(), Infallible>(()),
                ),
                Err(FnxExecutionError::InvalidNumericResult)
            ));
        } else {
            // Preserve the existing deeper-overflow oracle: consecutive
            // infinite path counts also make its final scores nonfinite.
            let expected = reference(&graph);
            assert!(expected.scores.iter().any(|score| !score.score.is_finite()));
        }
        for (threads, window) in [(1, 1), (4, 31)] {
            assert!(matches!(
                run_with_schedule(&graph, &mut Unstoppable, threads, window),
                Err(FnxExecutionError::InvalidNumericResult)
            ));
        }
    }
}

#[test]
fn serial_brandes_propagates_every_live_refusal_without_another_control_call() {
    let graph = graph(
        17,
        &(0..17)
            .flat_map(|source| {
                (source + 1..17)
                    .filter(move |&target| (source * 7 + target) % 3 != 0)
                    .map(move |target| (source, target))
            })
            .collect::<Vec<_>>(),
    );
    struct Cut {
        calls: usize,
        stop: usize,
    }
    impl FnxMeter<usize> for Cut {
        fn unit(&mut self) -> Result<(), usize> {
            panic!("uncharged live arithmetic only")
        }
        fn poll(&mut self) -> Result<(), usize> {
            assert!(self.calls < self.stop);
            self.calls += 1;
            if self.calls == self.stop {
                Err(self.calls)
            } else {
                Ok(())
            }
        }
    }
    let mut complete = Cut {
        calls: 0,
        stop: usize::MAX,
    };
    run_with_schedule(&graph, &mut complete, 1, 3).unwrap();
    for stop in 1..=complete.calls {
        let mut cut = Cut { calls: 0, stop };
        let result = run_with_schedule(&graph, &mut cut, 1, 3);
        assert!(matches!(result, Err(FnxExecutionError::Cancelled(at)) if at == stop));
        assert_eq!(cut.calls, stop);
    }
}

#[test]
fn directed_and_reversed_brandes_match_pinned_bits_and_weak_source_windows() {
    let cases: &[(usize, &[(usize, usize)])] = &[
        (0, &[]),
        (1, &[]),
        (6, &[(0, 1), (1, 2), (3, 2), (2, 4), (4, 1)]),
        (7, &[(2, 0), (2, 1), (3, 1), (4, 5)]),
        (6, &[(0, 1), (0, 2), (1, 3), (2, 3), (3, 4)]),
        (
            513,
            &[(4, 1), (4, 2), (2, 3), (3, 1), (6, 7), (9, 8), (20, 21)],
        ),
    ];
    for &(n, pairs) in cases {
        for direction in [Directedness::Directed, Directedness::Reversed] {
            let id = |at: usize| VId((1u128 << 110) + ((at as u128) << 64) + 17);
            let vertices: Vec<_> = (0..n).rev().map(id).collect();
            let edges: Vec<_> = pairs
                .iter()
                .enumerate()
                .map(|(at, &(s, t))| ProjectionEdge {
                    eid: EId(at as u128 + 1),
                    source: id(s),
                    target: id(t),
                    weight: if at % 2 == 0 { -3.0 } else { 11.0 },
                })
                .collect();
            let graph = SnapshotGraphView::build(
                SnapshotBinding {
                    root: ObjectId([19; 32]),
                    as_of: CommitSeq(31),
                },
                &vertices,
                &edges,
                ProjectionSpec {
                    directedness: direction,
                    parallel_edges: ParallelEdgePolicy::Reject,
                    self_loops: SelfLoopPolicy::Keep,
                },
                ProjectionLimits {
                    max_vertices: n,
                    max_input_edges: edges.len(),
                    max_adjacency_entries: edges.len(),
                    max_workspace_bytes: 1 << 28,
                },
            )
            .unwrap();
            let names = graph.nodes_ordered();
            let mut oracle = fnx_classes::digraph::DiGraph::strict();
            for &name in &names {
                oracle.add_node(name);
            }
            for source in 0..n {
                for &target in graph.neighbors_indices(source).unwrap() {
                    oracle.add_edge(names[source], names[target]).unwrap();
                }
            }
            let expected = fnx_algorithms::betweenness_centrality_directed(&oracle);
            let expected_bits: Vec<_> = expected
                .scores
                .iter()
                .map(|score| score.score.to_bits())
                .collect();
            for (threads, window) in [(1, 1), (1, 7), (2, 7), (4, 17)] {
                let (actual, _) =
                    run_with_schedule(&graph, &mut Unstoppable, threads, window).unwrap();
                assert_eq!(
                    bits(&actual),
                    expected_bits,
                    "{direction:?}, n={n}, threads={threads}, window={window}"
                );
                assert_eq!(actual.witness, expected.witness);
            }
            let work = (n.min(edges.len() + 1) + 1) * (n + edges.len()) + edges.len();
            let limits = FnxExecutionLimits {
                max_iterations: 0,
                max_result_rows: n,
                max_estimated_work: work,
            };
            let call =
                FnxCallSpec::bind("CALL fnx.betweenness_centrality()", &FnxParameters::new())
                    .unwrap();
            let result = call
                .execute(&graph, limits, || Ok::<(), Infallible>(()))
                .unwrap();
            assert_eq!(result.certificate.estimated_work, work);
            if work != 0 {
                assert!(matches!(
                    call.execute(&graph, FnxExecutionLimits { max_estimated_work: work - 1, ..limits }, || Ok::<(), Infallible>(())),
                    Err(FnxExecutionError::LimitExceeded { resource: "estimated work", requested, .. }) if requested == work
                ));
            }
        }
    }
}
