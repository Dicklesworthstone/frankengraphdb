use super::*;
use crate::{
    Directedness, ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
    SelfLoopPolicy, SnapshotBinding,
};
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use fnx_classes::Graph;
use std::collections::VecDeque;
use std::convert::Infallible;

struct Unstoppable;
impl FnxMeter<Infallible> for Unstoppable {
    fn unit(&mut self) -> Result<(), Infallible> {
        panic!("native arithmetic must not recharge logical work")
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
            weight: if at % 2 == 0 { -3.0 } else { 11.0 },
        })
        .collect();
    SnapshotGraphView::build(
        SnapshotBinding {
            root: ObjectId([29; 32]),
            as_of: CommitSeq(53),
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

fn copied(graph: &SnapshotGraphView) -> Graph {
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
    copy
}

fn bits(output: &KernelOutput) -> Vec<u64> {
    let KernelValues::Scores(scores) = &output.values else {
        panic!("distance centrality returns scores")
    };
    scores.iter().map(|score| score.to_bits()).collect()
}

/// Independent all-pairs distance table. A VecDeque FIFO derives the scores;
/// physical batch work is reconstructed from distance-level SETS, never from
/// the production bit operations, frontier buffers, or scheduler.
fn oracle<const HARMONIC: bool>(
    graph: &SnapshotGraphView,
    width: usize,
) -> (Vec<u64>, ComplexityWitness) {
    let n = graph.node_count();
    let mut distances = Vec::new();
    let mut scores = Vec::new();
    let mut witness = ComplexityWitness {
        algorithm: if HARMONIC {
            "harmonic_centrality"
        } else {
            "closeness_centrality"
        }
        .to_owned(),
        complexity_claim: "O(|V| * (|V| + |E|))".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    for source in 0..n {
        let mut distance = vec![usize::MAX; n];
        let mut queue = VecDeque::from([source]);
        distance[source] = 0;
        let mut reached = 0usize;
        let mut sum = 0usize;
        let mut harmonic = 0.0;
        if width == 0 {
            witness.queue_peak = witness.queue_peak.max(1);
        }
        while let Some(vertex) = queue.pop_front() {
            reached += 1;
            sum += distance[vertex];
            if distance[vertex] != 0 {
                harmonic += 1.0 / distance[vertex] as f64;
            }
            let row = graph.neighbors_indices(vertex).unwrap();
            if width == 0 {
                witness.edges_scanned += row.len();
            }
            for &target in row {
                if distance[target] == usize::MAX {
                    distance[target] = distance[vertex] + 1;
                    queue.push_back(target);
                    if width == 0 {
                        witness.queue_peak = witness.queue_peak.max(queue.len());
                    }
                }
            }
        }
        witness.nodes_touched += reached;
        let value = if HARMONIC {
            harmonic
        } else if reached <= 1 {
            0.0
        } else {
            let mut value = (reached - 1) as f64 / sum as f64;
            if n > 1 {
                value *= (reached - 1) as f64 / (n - 1) as f64;
            }
            value
        };
        scores.push(value.to_bits());
        distances.push(distance);
    }
    if width != 0 && n != 0 {
        for sources in distances.chunks(width * 64) {
            let mut present = vec![false; n * n];
            for source in sources {
                for (vertex, &distance) in source.iter().enumerate() {
                    if distance != usize::MAX {
                        present[distance * n + vertex] = true;
                    }
                }
            }
            for level in present.chunks(n) {
                let mut count = 0;
                for (vertex, &present) in level.iter().enumerate() {
                    if present {
                        count += 1;
                        witness.edges_scanned += graph.neighbors_indices(vertex).unwrap().len();
                    }
                }
                witness.queue_peak = witness.queue_peak.max(count);
            }
        }
    }
    (scores, witness)
}

fn assert_oracle<const W: usize, const HARMONIC: bool>(graph: &SnapshotGraphView, threads: usize) {
    let expected = oracle::<HARMONIC>(graph, W);
    let (actual, workspace) =
        run_scheduled::<W, HARMONIC, Infallible>(graph, &mut Unstoppable, threads).unwrap();
    assert_eq!(bits(&actual), expected.0, "lanes={W}, threads={threads}");
    assert_eq!(actual.witness, expected.1, "lanes={W}, threads={threads}");
    assert_eq!(actual.row_count, graph.node_count());
    assert!(workspace >= graph.node_count() * size_of::<f64>());
}

#[test]
fn all_small_topologies_match_real_pinned_fnx_bits_and_full_witnesses() {
    let mut topologies = 0;
    for n in 0..=5 {
        let pairs: Vec<_> = (0..n)
            .flat_map(|source| (source + 1..n).map(move |target| (source, target)))
            .collect();
        for mask in 0..1usize << pairs.len() {
            let edges: Vec<_> = pairs
                .iter()
                .enumerate()
                .filter(|(at, _)| mask & (1 << at) != 0)
                .map(|(_, &pair)| pair)
                .collect();
            let graph = graph(n, &edges);
            let reference = copied(&graph);
            let close = fnx_algorithms::closeness_centrality(&reference);
            let harm = fnx_algorithms::harmonic_centrality(&reference);
            for (actual, expected, witness) in [
                (
                    run::<false, Infallible>(&graph, &mut Unstoppable)
                        .unwrap()
                        .0,
                    close.scores,
                    close.witness,
                ),
                (
                    run::<true, Infallible>(&graph, &mut Unstoppable).unwrap().0,
                    harm.scores,
                    harm.witness,
                ),
            ] {
                assert_eq!(
                    bits(&actual),
                    expected
                        .iter()
                        .map(|score| score.score.to_bits())
                        .collect::<Vec<_>>()
                );
                assert_eq!(actual.witness, witness);
            }
            assert_oracle::<1, false>(&graph, 1);
            assert_oracle::<1, true>(&graph, 1);
            topologies += 1;
        }
    }
    assert_eq!(topologies, 1100);
}

#[test]
fn source_and_batch_boundaries_match_an_independent_distance_set_witness() {
    for n in [1, 63, 64, 65, 129, 499, 500, 513] {
        let edges: Vec<_> = (0..n)
            .flat_map(|source| {
                (source + 1..n)
                    .filter(move |&target| {
                        target < n - 1 && (source == 0 || (source * 7 + target * 11) % 31 == 0)
                    })
                    .map(move |target| (source, target))
            })
            .collect();
        let graph = graph(n, &edges);
        assert_oracle::<0, false>(&graph, 1);
        assert_oracle::<0, true>(&graph, 4);
        assert_oracle::<1, false>(&graph, 4);
        assert_oracle::<1, true>(&graph, 1);
        assert_oracle::<3, false>(&graph, 2);
        assert_oracle::<3, true>(&graph, 4);
        assert_oracle::<8, false>(&graph, 4);
        assert_oracle::<8, true>(&graph, 1);
    }
    // Distances 1..n-1 exercise the harmonic addend order and the scalar
    // fallback. The graph also has an unreachable component and an isolate.
    let graph = graph(
        131,
        &(0..127)
            .filter(|&source| source != 79)
            .map(|source| (source, source + 1))
            .collect::<Vec<_>>(),
    );
    assert_oracle::<0, false>(&graph, 4);
    assert_oracle::<0, true>(&graph, 4);
    assert_oracle::<2, false>(&graph, 4);
    assert_oracle::<2, true>(&graph, 4);
    assert_oracle::<8, true>(&graph, 1);
}

#[test]
fn lane_policy_is_stable_and_declines_disconnected_and_high_diameter_inputs() {
    for n in [499, 500, 513, 4097] {
        let star = graph(n, &(1..n).map(|target| (0, target)).collect::<Vec<_>>());
        let path = graph(
            n,
            &(1..n)
                .map(|target| (target - 1, target))
                .collect::<Vec<_>>(),
        );
        let disconnected = graph(n, &(1..n - 1).map(|target| (0, target)).collect::<Vec<_>>());
        let expected = if n < PARALLEL_SOURCES {
            8
        } else {
            chunk_width(n)
        };
        assert_eq!(lane_width(&star, &mut Unstoppable).unwrap().0, expected);
        assert_eq!(
            lane_width(&path, &mut Unstoppable).unwrap().0,
            if n < PARALLEL_SOURCES { 8 } else { 0 }
        );
        assert_eq!(
            lane_width(&disconnected, &mut Unstoppable).unwrap().0,
            if n < PARALLEL_SOURCES { 8 } else { 0 }
        );
    }
    let graph = graph(513, &(1..513).map(|target| (0, target)).collect::<Vec<_>>());
    let reference = copied(&graph);
    let close = fnx_algorithms::closeness_centrality(&reference);
    let harm = fnx_algorithms::harmonic_centrality(&reference);
    for (actual, expected, nodes) in [
        (
            run::<false, Infallible>(&graph, &mut Unstoppable)
                .unwrap()
                .0,
            close.scores,
            close.witness.nodes_touched,
        ),
        (
            run::<true, Infallible>(&graph, &mut Unstoppable).unwrap().0,
            harm.scores,
            harm.witness.nodes_touched,
        ),
    ] {
        // FNX Auto's batch width depends on the Rayon pool. The fixed native
        // policy deliberately does not promise its physical witness matches
        // that unrelated configuration. The independent law above covers it.
        assert_eq!(
            bits(&actual),
            expected
                .iter()
                .map(|score| score.score.to_bits())
                .collect::<Vec<_>>()
        );
        assert_eq!(actual.witness.nodes_touched, nodes);
    }
}

#[test]
fn replicated_scratch_chooser_respects_budget_boundaries_without_changing_batches() {
    fn boundaries<const W: usize, const HARMONIC: bool>() {
        // Count requested storage independently by its concrete arrays: FIFO
        // uses distance+queue; batching uses three W-word rows and two queues.
        let per_vertex = if W == 0 {
            size_of::<usize>() + size_of::<usize>()
        } else {
            size_of::<u64>() * W * 3 + size_of::<usize>() * 2
        };
        let fixed = size_of::<Worker<W, HARMONIC>>()
            + if W == 0 {
                0
            } else {
                64 * W * size_of::<usize>() * if HARMONIC { 1 } else { 2 }
            };
        for count in 2..=MAX_GATHER_THREADS {
            // Last n at which exactly count full workers fit. No large graph
            // or allocation is needed to exercise the overflow/budget policy.
            let last_n = (WORKER_SCRATCH_TARGET / count - fixed) / per_vertex;
            let admitted =
                worker_count::<W, HARMONIC, Infallible>(last_n).unwrap();
            let declined =
                worker_count::<W, HARMONIC, Infallible>(last_n + 1).unwrap();
            assert!(admitted >= count);
            assert!(declined < count);
            assert!(admitted * (last_n * per_vertex + fixed) <= WORKER_SCRATCH_TARGET);
        }
        let oversized = WORKER_SCRATCH_TARGET / per_vertex + 1;
        assert_eq!(
            worker_count::<W, HARMONIC, Infallible>(oversized).unwrap(),
            1
        );
        assert_eq!(worker_count::<W, HARMONIC, Infallible>(0).unwrap(), 1);
        assert!(matches!(
            worker_count::<W, HARMONIC, Infallible>(usize::MAX),
            Err(FnxExecutionError::SizeOverflow)
        ));
    }
    boundaries::<0, false>();
    boundaries::<1, true>();
    boundaries::<8, false>();
    boundaries::<8, true>();

    let graph = graph(513, &(1..513).map(|target| (0, target)).collect::<Vec<_>>());
    for available in [1, 2, 4, 8] {
        assert_eq!(
            worker_count::<1, false, Infallible>(513).unwrap(),
            POLICY_THREADS
        );
        assert_oracle::<1, false>(&graph, available);
        assert_oracle::<1, true>(&graph, available);
    }
}

#[test]
fn physical_threads_preserve_exact_workspace_for_scalar_and_bit_parallel_lanes() {
    fn check<const W: usize, const HARMONIC: bool>(graph: &SnapshotGraphView) {
        let n = graph.node_count();
        let logical = worker_count::<W, HARMONIC, Infallible>(n).unwrap();
        let width = (64 * W).max(1);
        let batches = n.div_ceil(width);
        // Count concrete retained arrays independently of Worker::bytes:
        // final and partitioned scores, worker descriptors, replicated BFS
        // rows/queues, and only the actual live per-batch counters.
        let mut expected_workspace = if n == 0 {
            0
        } else {
            2 * n * size_of::<f64>()
                + logical * size_of::<Worker<W, HARMONIC>>()
                + logical * n * (3 * size_of::<[u64; W]>() + 2 * size_of::<usize>())
        };
        let mut first_batch = 0;
        if W != 0 && n != 0 {
            for worker in 0..logical {
                let count = batches / logical + usize::from(worker < batches % logical);
                let first = first_batch * width;
                first_batch += count;
                let sources = (first_batch * width).min(n) - first;
                expected_workspace += width.min(sources)
                    * size_of::<usize>()
                    * if HARMONIC { 1 } else { 2 };
            }
        }
        let expected = oracle::<HARMONIC>(graph, W);
        for threads in [1, 2, 8] {
            let (actual, workspace) =
                run_scheduled::<W, HARMONIC, Infallible>(graph, &mut Unstoppable, threads)
                    .unwrap();
            assert_eq!(bits(&actual), expected.0);
            assert_eq!(actual.witness, expected.1);
            assert_eq!(workspace, expected_workspace, "W={W}, threads={threads}");
        }
    }
    check::<0, false>(&graph(0, &[]));
    check::<8, true>(&graph(0, &[]));
    let edges: Vec<_> = (1..512)
        .filter(|v| v % 97 != 0)
        .map(|v| (v - 1, v))
        .collect();
    let graph = graph(513, &edges);
    check::<0, false>(&graph);
    check::<0, true>(&graph);
    check::<1, false>(&graph);
    check::<1, true>(&graph);
    check::<8, false>(&graph);
    check::<8, true>(&graph);
}

#[test]
fn harmonic_repeats_level_addends_instead_of_multiplying_counts() {
    // 97 vertices are exactly distance three from source zero. Repeated 1/3
    // additions differ from multiplying 97 by 1/3 on this floating profile.
    let mut edges = vec![(0, 1), (1, 2)];
    edges.extend((3..100).map(|target| (2, target)));
    let graph = graph(100, &edges);
    let expected = oracle::<true>(&graph, 2).0;
    assert_ne!(
        f64::from_bits(expected[0]).to_bits(),
        (1.0_f64 + 0.5 + 97.0 * (1.0 / 3.0)).to_bits()
    );
    for threads in [1, 2, 4, 8] {
        assert_oracle::<2, true>(&graph, threads);
    }
}

#[test]
fn cancellation_stops_inside_a_hub_before_the_next_edge_batch() {
    let n = 8194;
    let graph = graph(n, &(1..n).map(|target| (0, target)).collect::<Vec<_>>());
    let mut worker = Worker::<1, true>::new(n, 0, 1, &mut Unstoppable).unwrap();
    // Two n-row clears, one source initialization, one queue pop, then a
    // strict edge prefix; the next poll must stop within the first hub row.
    let before_edges = 2 * n + 2;
    let admitted = before_edges.div_ceil(ARITHMETIC_RUN) * ARITHMETIC_RUN;
    let stop = admitted / ARITHMETIC_RUN + 1;
    let mut polls = 0;
    let result = worker.run(&graph, &mut || {
        polls += 1;
        polls != stop
    });
    assert_eq!(result, Err(WorkerError::Stopped));
    assert_eq!(polls, stop);
    assert_eq!(worker.edges_scanned, admitted - before_edges);
    assert_eq!(worker.next_queue.len(), admitted - before_edges);
    assert!(worker.edges_scanned > 0 && worker.edges_scanned < n - 1);
    assert_eq!(worker.scores, [0.0]);
}

#[test]
fn parallel_distance_workers_join_and_keep_non_send_control_on_the_parent() {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let n = 8194;
    let graph = graph(n, &(1..n).map(|target| (0, target)).collect::<Vec<_>>());
    let mut workers: Vec<_> = (0..2)
        .map(|source| Worker::<1, true>::new(n, source, 1, &mut Unstoppable).unwrap())
        .collect();
    let requests = [AtomicUsize::new(0), AtomicUsize::new(0)];
    let returned = AtomicUsize::new(0);
    let error = Rc::new(Cell::new(0));
    let parent = std::thread::current().id();
    let before_edges = 2 * n + 2;
    let admitted = before_edges.div_ceil(ARITHMETIC_RUN) * ARITHMETIC_RUN;
    let stop = admitted / ARITHMETIC_RUN + 1;
    struct Host<'a> {
        requests: &'a [AtomicUsize; 2],
        error: Rc<Cell<usize>>,
        parent: std::thread::ThreadId,
        stop: usize,
    }
    impl FnxMeter<Rc<Cell<usize>>> for Host<'_> {
        fn unit(&mut self) -> Result<(), Rc<Cell<usize>>> {
            panic!("parallel arithmetic must not charge work")
        }
        fn poll(&mut self) -> Result<(), Rc<Cell<usize>>> {
            assert_eq!(std::thread::current().id(), self.parent);
            if self.requests[0].load(Ordering::Acquire) >= self.stop {
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
            stop,
        },
        |low, group, poll| {
            for (offset, worker) in group.iter_mut().enumerate() {
                worker.error = worker
                    .run(&graph, &mut || {
                        requests[low + offset].fetch_add(1, Ordering::Release);
                        poll()
                    })
                    .err();
                returned.fetch_add(1, Ordering::Release);
            }
        },
    );
    assert!(
        matches!(result, Err(FnxExecutionError::Cancelled(observed)) if Rc::ptr_eq(&observed, &error))
    );
    assert_eq!(error.get(), 1);
    assert_eq!(returned.load(Ordering::Acquire), 2);
    assert_eq!(requests[0].load(Ordering::Acquire), stop);
    assert_eq!(workers[0].edges_scanned, admitted - before_edges);
    assert_eq!(workers[0].error, Some(WorkerError::Stopped));
}

#[test]
fn serial_paths_and_probe_propagate_every_live_refusal_without_another_call() {
    struct Cut {
        calls: usize,
        stop: usize,
    }
    impl FnxMeter<usize> for Cut {
        fn unit(&mut self) -> Result<(), usize> {
            panic!("live arithmetic only")
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
    let small = graph(73, &(1..73).map(|target| (0, target)).collect::<Vec<_>>());
    let large = graph(
        500,
        &(1..500)
            .map(|target| (target - 1, target))
            .collect::<Vec<_>>(),
    );
    let grouped = graph(513, &[]);
    for arm in 0..4 {
        let invoke = |cut: &mut Cut| -> Result<(), FnxExecutionError<usize>> {
            match arm {
                0 => run_scheduled::<0, false, usize>(&small, cut, 1).map(|_| ()),
                1 => run_scheduled::<2, true, usize>(&small, cut, 1).map(|_| ()),
                2 => run_scheduled::<1, true, usize>(&grouped, cut, 1).map(|_| ()),
                _ => lane_width(&large, cut).map(|_| ()),
            }
        };
        let mut complete = Cut {
            calls: 0,
            stop: usize::MAX,
        };
        invoke(&mut complete).unwrap();
        for stop in 1..=complete.calls {
            let mut cut = Cut { calls: 0, stop };
            assert!(
                matches!(invoke(&mut cut), Err(FnxExecutionError::Cancelled(at)) if at == stop)
            );
            assert_eq!(cut.calls, stop);
        }
    }
}
