//! Weighted distances from immutable compressed rows, with O(|V|) workspace.
//! The indexed queue is shared with the decoded primitive; graph traversal is
//! not. Complete immutable projection admission can discharge the nonnegative
//! weight check once for every later source query. Uncertified sources retain
//! full validation. One cursor is live at a time and neither validation nor
//! relaxation collects adjacency. Failures and cancellation discard all state.

use super::{
    Cursor, Error, ExecutionError, KernelOutput, KernelValues, Result, ResultAdmission, Rows, add,
    mul, reserve,
};
use crate::shortest_path::{HeapError, IndexedHeap};
use crate::{ComplexityWitness, DijkstraOptions};
use std::convert::Infallible;

impl From<HeapError<Error>> for Error {
    fn from(error: HeapError<Error>) -> Self {
        match error {
            // Do not reclassify a typed source/cancellation failure or wrap it
            // in a successful-prefix/EOF result at the shared queue boundary.
            HeapError::Cancelled(error) => error,
            HeapError::SizeOverflow => ExecutionError::SizeOverflow.into(),
            HeapError::AllocationFailed => ExecutionError::AllocationFailed.into(),
            HeapError::InvalidOrdinal => ExecutionError::InvalidUpstreamResult.into(),
        }
    }
}

/// Exact unit distances and their next candidates are representable through
/// n == 2^53. Larger populations retain the general weighted queue.
pub(super) fn uses_unit_fifo(n: usize, unit_weights: bool) -> bool {
    unit_weights && n as u128 <= 1u128 << 53
}

pub(super) fn workspace(n: usize, unit: bool) -> Result<usize> {
    if unit {
        mul(n, size_of::<Option<f64>>() + size_of::<usize>())
    } else {
        crate::shortest_path::workspace_bytes::<Infallible>(n).map_err(Into::into)
    }
}

/// Preserve the conservative two-pass admission allowance, even when immutable
/// weight evidence eliminates the validation pass. Heap work uses reduced
/// edges separately; a heavily reduced multigraph must not conceal the cost of
/// its raw incidences. This is a bound, not the observed cursor visit count.
pub(super) fn work(n: usize, edges: usize, pass: usize, unit: bool) -> Result<usize> {
    let queue = if unit {
        // Population/result passes and constant-time discovery/queue work.
        // Raw history still contributes BOTH complete compressed-row passes.
        add(mul(n, 5)?, mul(edges, 2)?)?
    } else {
        crate::shortest_path::estimated_work::<Infallible>(n, edges)?
    };
    add(mul(pass, 2)?, queue)
}

pub(super) fn run(
    graph: &impl Rows,
    source: usize,
    options: DijkstraOptions,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    checkpoint()?;
    let n = graph.node_count();
    if source >= n {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    admission.rows(1)?;
    let unit = uses_unit_fifo(n, graph.has_unit_weights());
    // The admitted sealed graph has already inspected every final weight.
    // Reopening ALL compressed rows for each source repeats that work, even
    // for an isolate or a tiny cutoff. Only immutable admission evidence can
    // omit it: unknown sources and projections containing a negative weight
    // still validate the ENTIRE selection before allocating any queue.
    if !graph.has_nonnegative_weights() {
        for node in 0..n {
            checkpoint()?;
            let mut row = graph.open(node)?;
            while let Some((target, weight)) = row.next()? {
                checkpoint()?;
                if target >= n {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                validate_weight(weight)?;
                if unit && weight != 1.0 {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
            }
        }
    }
    if unit {
        return run_unit_fifo(graph, source, options.cutoff(), admission, checkpoint);
    }
    let mut distances = reserve(n)?;
    let mut overflowed = reserve(n)?;
    let mut heap = IndexedHeap::new(n, options.comparison(), checkpoint)?;
    for _ in 0..n {
        checkpoint()?;
        distances.push(None);
        overflowed.push(false);
    }
    heap.offer(source, 0.0, checkpoint)?;
    let mut discovered = 1usize;
    let mut witness = ComplexityWitness {
        algorithm: "single_source_dijkstra_compressed_indexed_heap".to_owned(),
        complexity_claim: "O(|V| log(1+H) + H log(1+|V|) + (|V|+|E|) log(1+|V|)) compressed row visits and queue work".to_owned(),
        nodes_touched: 0, edges_scanned: 0, queue_peak: 1,
    };
    loop {
        checkpoint()?;
        let Some(entry) = heap.pop(checkpoint)? else {
            break;
        };
        distances[entry.node] = Some(entry.cost);
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        let mut row = graph.open(entry.node)?;
        while let Some((target, weight)) = row.next()? {
            checkpoint()?;
            witness.edges_scanned = add(witness.edges_scanned, 1)?;
            validate_weight(weight)?;
            if distances
                .get(target)
                .ok_or(ExecutionError::InvalidUpstreamResult)?
                .is_some()
            {
                continue;
            }
            let candidate = entry.cost + weight;
            if !candidate.is_finite() {
                // Do not confuse an overflowing alternative with an
                // unrepresentable shortest path. A later finite route wins.
                // Any overflow exceeds a finite inclusive cost cutoff.
                if options.cutoff().is_none() {
                    overflowed[target] = true;
                }
                continue;
            }
            if options.cutoff().is_some_and(|cutoff| candidate > cutoff) {
                continue;
            }
            if !heap
                .contains(target)
                .ok_or(ExecutionError::InvalidUpstreamResult)?
            {
                let next = add(discovered, 1)?;
                admission.rows(next)?; // BEFORE the queue can retain this row
                discovered = next;
            }
            heap.offer(target, candidate, checkpoint)?;
            witness.queue_peak = witness.queue_peak.max(heap.len());
        }
        // In contrast to hop-cutoff BFS, a vertex at the cost cutoff MUST
        // still expand: zero-weight edges can extend the admitted closure.
    }
    for node in 0..n {
        checkpoint()?;
        if overflowed[node] && distances[node].is_none() {
            return Err(ExecutionError::InvalidNumericResult.into());
        }
    }
    if witness.nodes_touched != discovered {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    Ok(KernelOutput {
        values: KernelValues::WeightedDistances(distances),
        row_count: discovered,
        witness,
    })
}

/// Shared bounded state for synchronous and cooperative unit traversal.
/// First discovery is final, and canonical rows plus FIFO preserve the
/// indexed heap's cost/offer-sequence order under both comparison policies.
/// The drivers checkpoint before each initialization, pop and incidence step.
pub(super) struct UnitTraversal {
    population: usize,
    distances: Vec<Option<f64>>,
    queue: Vec<usize>,
    head: usize,
    witness: ComplexityWitness,
}

impl UnitTraversal {
    pub(super) fn new(n: usize) -> Result<Self> {
        Ok(Self {
            population: n,
            distances: reserve(n)?,
            queue: reserve(n)?,
            head: 0,
            witness: ComplexityWitness {
                algorithm: "single_source_dijkstra_compressed_unit_fifo".to_owned(),
                complexity_claim: "O(|V| log(1+H) + H log(1+|V|) + |V| + |E|) compressed row visits and unit queue work".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 1,
            },
        })
    }

    pub(super) fn initialize_step(&mut self) -> bool {
        if self.distances.len() < self.population {
            self.distances.push(None);
        }
        self.distances.len() == self.population
    }

    pub(super) fn start(&mut self, source: usize) -> Result<()> {
        if self.distances.len() != self.population || !self.queue.is_empty() {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        *self
            .distances
            .get_mut(source)
            .ok_or(ExecutionError::InvalidUpstreamResult)? = Some(0.0);
        self.queue.push(source);
        Ok(())
    }

    pub(super) fn pop(&mut self) -> Result<Option<(usize, f64)>> {
        let Some(&source) = self.queue.get(self.head) else {
            return Ok(None);
        };
        let cost = self.distances[source].ok_or(ExecutionError::InvalidUpstreamResult)?;
        self.head += 1;
        self.witness.nodes_touched = add(self.witness.nodes_touched, 1)?;
        Ok(Some((source, cost)))
    }

    pub(super) fn visit(
        &mut self,
        target: usize,
        weight: f64,
        cost: f64,
        cutoff: Option<f64>,
        admission: &ResultAdmission,
    ) -> Result<()> {
        self.witness.edges_scanned = add(self.witness.edges_scanned, 1)?;
        validate_weight(weight)?;
        if weight != 1.0 {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        let distance = self
            .distances
            .get_mut(target)
            .ok_or(ExecutionError::InvalidUpstreamResult)?;
        if distance.is_some() {
            return Ok(());
        }
        let candidate = cost + 1.0;
        if cutoff.is_some_and(|limit| candidate > limit) {
            return Ok(());
        }
        admission.rows(add(self.queue.len(), 1)?)?;
        *distance = Some(candidate);
        self.queue.push(target);
        self.witness.queue_peak = self.witness.queue_peak.max(self.queue.len() - self.head);
        Ok(())
    }

    pub(super) fn finish(self) -> Result<KernelOutput> {
        if self.head != self.queue.len() || self.witness.nodes_touched != self.queue.len() {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        Ok(KernelOutput {
            values: KernelValues::WeightedDistances(self.distances),
            row_count: self.queue.len(),
            witness: self.witness,
        })
    }
}

fn run_unit_fifo(
    graph: &impl Rows,
    source: usize,
    cutoff: Option<f64>,
    admission: &ResultAdmission,
    checkpoint: &mut impl FnMut() -> Result<()>,
) -> Result<KernelOutput> {
    checkpoint()?;
    let mut traversal = UnitTraversal::new(graph.node_count())?;
    loop {
        checkpoint()?;
        if traversal.initialize_step() {
            break;
        }
    }
    checkpoint()?;
    traversal.start(source)?;
    loop {
        checkpoint()?;
        let Some((node, cost)) = traversal.pop()? else {
            break;
        };
        let mut row = graph.open(node)?;
        while let Some((target, weight)) = row.next()? {
            checkpoint()?;
            traversal.visit(target, weight, cost, cutoff, admission)?;
        }
        // Scan cutoff rows too, retaining source failures and edge witnesses.
        // Unit weights simply cannot discover another vertex beyond the bound.
    }
    traversal.finish()
}

fn validate_weight(weight: f64) -> Result<()> {
    if !weight.is_finite() {
        return Err(ExecutionError::InvalidNumericResult.into());
    }
    if weight < 0.0 {
        return Err(ExecutionError::NegativeWeight.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DijkstraComparison, Directedness, FnxAlgorithm, FnxArgument, FnxCallSpec,
        FnxExecutionLimits, FnxMemoryLimits, FnxParameters, FnxValue, GraphView,
        ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
        SealedProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
    };
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
    use std::cell::Cell;

    struct Fixture {
        view: SnapshotGraphView,
        calls: Cell<usize>,
        fail_at: usize,
    }
    struct TestCursor<'a> {
        fixture: &'a Fixture,
        row: std::iter::Zip<std::slice::Iter<'a, usize>, std::slice::Iter<'a, f64>>,
    }
    impl Fixture {
        fn new(n: usize, edges: &[(usize, usize, f64)]) -> Self {
            let vertices: Vec<_> = (1..=n)
                .map(|i| VId(u128::MAX - n as u128 + i as u128))
                .collect();
            let edges: Vec<_> = edges
                .iter()
                .enumerate()
                .map(|(eid, &(source, target, weight))| ProjectionEdge {
                    eid: EId(eid as u128),
                    source: vertices[source],
                    target: vertices[target],
                    weight,
                })
                .collect();
            let view = SnapshotGraphView::build(
                SnapshotBinding {
                    root: ObjectId([71; 32]),
                    as_of: CommitSeq(2),
                },
                &vertices,
                &edges,
                ProjectionSpec {
                    directedness: Directedness::Directed,
                    parallel_edges: ParallelEdgePolicy::Minimum,
                    self_loops: SelfLoopPolicy::Keep,
                },
                ProjectionLimits {
                    max_vertices: n,
                    max_input_edges: edges.len(),
                    max_adjacency_entries: edges.len(),
                    max_workspace_bytes: 1 << 24,
                },
            )
            .unwrap();
            Self {
                view,
                calls: Cell::new(0),
                fail_at: usize::MAX,
            }
        }
        fn step(&self) -> Result<()> {
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() == self.fail_at {
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                    usize::MAX,
                )))
            } else {
                Ok(())
            }
        }
    }
    impl Cursor for TestCursor<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.fixture.step()?;
            Ok(self.row.next().map(|(&target, &weight)| (target, weight)))
        }
    }
    impl Rows for Fixture {
        type Cursor<'a>
            = TestCursor<'a>
        where
            Self: 'a;
        fn node_count(&self) -> usize {
            self.view.node_count()
        }
        fn degree(&self, node: usize) -> Option<usize> {
            self.view.neighbors_indices(node).map(<[usize]>::len)
        }
        fn has_unit_weights(&self) -> bool {
            self.view.has_unit_weights()
        }
        fn open(&self, node: usize) -> Result<Self::Cursor<'_>> {
            self.step()?;
            let (targets, weights) = self
                .view
                .projected_row(node)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            Ok(TestCursor {
                fixture: self,
                row: targets.iter().zip(weights),
            })
        }
    }
    fn limits(n: usize) -> FnxExecutionLimits {
        FnxExecutionLimits {
            max_iterations: 0,
            max_result_rows: n,
            max_estimated_work: usize::MAX,
        }
    }
    fn memory() -> FnxMemoryLimits {
        FnxMemoryLimits {
            max_kernel_workspace_bytes: usize::MAX,
            max_result_bytes: usize::MAX,
        }
    }
    fn bound(source: VId, cutoff: Option<f64>, strict: bool) -> (FnxCallSpec, DijkstraOptions) {
        let parameters: FnxParameters = [
            ("s".to_owned(), FnxArgument::Vertex(source)),
            (
                "c".to_owned(),
                cutoff.map_or(FnxArgument::Null, FnxArgument::Float),
            ),
            ("strict".to_owned(), FnxArgument::Boolean(strict)),
        ]
        .into_iter()
        .collect();
        let call = FnxCallSpec::bind(
            "CALL fnx.single_source_dijkstra_path_length($s,$c,$strict) YIELD vertex,distance",
            &parameters,
        )
        .unwrap();
        assert!(call.supports_sealed_execution());
        let FnxAlgorithm::SingleSourceDijkstraPathLength(options) = call.algorithm() else {
            panic!("Dijkstra");
        };
        (call, options)
    }
    fn values(output: &KernelOutput) -> &[Option<f64>] {
        let KernelValues::WeightedDistances(values) = &output.values else {
            panic!("weighted distances");
        };
        values
    }
    fn execute(fixture: &Fixture, cutoff: Option<f64>, strict: bool) -> Result<KernelOutput> {
        let (call, options) = bound(fixture.view.vertex_id(0).unwrap(), cutoff, strict);
        let admission = ResultAdmission::new(&call, limits(fixture.node_count()), memory())?;
        run(fixture, 0, options, &admission, &mut || Ok(()))
    }

    struct Uncertified<'a>(&'a Fixture);
    impl Rows for Uncertified<'_> {
        type Cursor<'a>
            = TestCursor<'a>
        where
            Self: 'a;
        fn node_count(&self) -> usize {
            self.0.node_count()
        }
        fn degree(&self, node: usize) -> Option<usize> {
            self.0.degree(node)
        }
        fn open(&self, node: usize) -> Result<Self::Cursor<'_>> {
            self.0.open(node)
        }
    }

    // Model complete immutable admission independently of the SSSP driver.
    // The actual compressed constructor is exercised by prism_cooperative.
    struct Nonnegative<'a>(&'a Fixture);
    impl<'a> Nonnegative<'a> {
        fn new(fixture: &'a Fixture) -> Self {
            for source in 0..fixture.node_count() {
                let (_, weights) = fixture.view.projected_row(source).unwrap();
                assert!(
                    weights
                        .iter()
                        .all(|weight| weight.is_finite() && *weight >= 0.0)
                );
            }
            Self(fixture)
        }
    }
    impl Rows for Nonnegative<'_> {
        type Cursor<'a>
            = TestCursor<'a>
        where
            Self: 'a;
        fn node_count(&self) -> usize {
            self.0.node_count()
        }
        fn degree(&self, node: usize) -> Option<usize> {
            self.0.degree(node)
        }
        fn has_unit_weights(&self) -> bool {
            self.0.has_unit_weights()
        }
        fn has_nonnegative_weights(&self) -> bool {
            true
        }
        fn open(&self, node: usize) -> Result<Self::Cursor<'_>> {
            self.0.open(node)
        }
    }

    #[test]
    fn every_small_unit_topology_matches_heap_order_witnesses_and_independent_bfs() {
        for n in 1..=3 {
            for mask in 0usize..(1 << (n * n)) {
                let edges: Vec<_> = (0..n * n)
                    .filter(|bit| mask & (1 << bit) != 0)
                    .map(|bit| (bit / n, bit % n, 1.0))
                    .collect();
                let fixture = Fixture::new(n, &edges);
                for source in 0..n {
                    let mut oracle = vec![None; n];
                    let mut queue = std::collections::VecDeque::from([source]);
                    oracle[source] = Some(0usize);
                    while let Some(node) = queue.pop_front() {
                        for &target in fixture.view.neighbors_indices(node).unwrap() {
                            if oracle[target].is_none() {
                                oracle[target] = Some(oracle[node].unwrap() + 1);
                                queue.push_back(target);
                            }
                        }
                    }
                    for cutoff in [None, Some(-0.0), Some(0.5), Some(1.0), Some(2.0)] {
                        for strict in [false, true] {
                            let (call, options) =
                                bound(fixture.view.vertex_id(source).unwrap(), cutoff, strict);
                            let admission =
                                ResultAdmission::new(&call, limits(n), memory()).unwrap();
                            fixture.calls.set(0);
                            let actual =
                                run(&fixture, source, options, &admission, &mut || Ok(())).unwrap();
                            let visits = fixture.calls.get();
                            fixture.calls.set(0);
                            let heap = run(
                                &Uncertified(&fixture),
                                source,
                                options,
                                &admission,
                                &mut || Ok(()),
                            )
                            .unwrap();
                            assert_eq!(fixture.calls.get(), visits);
                            assert_eq!(values(&actual), values(&heap));
                            assert_eq!(actual.row_count, heap.row_count);
                            assert_eq!(actual.witness.nodes_touched, heap.witness.nodes_touched);
                            assert_eq!(actual.witness.edges_scanned, heap.witness.edges_scanned);
                            assert_eq!(actual.witness.queue_peak, heap.witness.queue_peak);
                            assert_eq!(
                                actual.witness.algorithm,
                                "single_source_dijkstra_compressed_unit_fifo"
                            );
                            let expected: Vec<_> = oracle
                                .iter()
                                .map(|&hops| {
                                    hops.map(|hops| hops as f64)
                                        .filter(|&distance| cutoff.is_none_or(|c| distance <= c))
                                })
                                .collect();
                            assert_eq!(values(&actual), expected);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn every_unit_checkpoint_and_cursor_failure_discards_the_private_fifo() {
        let mut fixture = Fixture::new(
            7,
            &[
                (0, 0, 1.0),
                (0, 1, 1.0),
                (0, 2, 1.0),
                (1, 3, 1.0),
                (2, 3, 1.0),
                (3, 4, 1.0),
                (5, 6, 1.0),
            ],
        );
        let (call, options) = bound(fixture.view.vertex_id(0).unwrap(), Some(1.0), true);
        let admission = ResultAdmission::new(&call, limits(7), memory()).unwrap();
        let mut count = 0;
        let expected = run(&fixture, 0, options, &admission, &mut || {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            values(&expected),
            &[Some(0.0), Some(1.0), Some(1.0), None, None, None, None]
        );
        // Includes edges from both cutoff vertices and the unreachable pair
        // during validation; no cutoff or reachability shortcut hides reads.
        assert_eq!(expected.witness.edges_scanned, 5);
        for stop in 1..=count {
            let mut seen = 0;
            let result = run(&fixture, 0, options, &admission, &mut || {
                seen += 1;
                if seen == stop {
                    Err(Error::Cancelled(std::io::Error::other("unit stop").into()))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(Error::Cancelled(_))));
            assert_eq!(seen, stop);
        }
        fixture.calls.set(0);
        run(&fixture, 0, options, &admission, &mut || Ok(())).unwrap();
        let operations = fixture.calls.get();
        for fail_at in 1..=operations {
            fixture.calls.set(0);
            fixture.fail_at = fail_at;
            assert!(matches!(
                run(&fixture, 0, options, &admission, &mut || Ok(())),
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                    usize::MAX
                )))
            ));
            assert_eq!(fixture.calls.get(), fail_at);
        }
        assert!(uses_unit_fifo(0, true));
        assert!(!uses_unit_fifo(7, false));
        if let Ok(limit) = usize::try_from(1u128 << 53) {
            assert!(uses_unit_fifo(limit, true));
            assert!(!uses_unit_fifo(limit + 1, true));
        }
        assert_eq!(
            workspace(7, true).unwrap(),
            7 * (size_of::<Option<f64>>() + size_of::<usize>())
        );
        assert!(workspace(7, true).unwrap() < workspace(7, false).unwrap());
        let pass = super::super::pass_work(7, 7).unwrap();
        assert!(work(7, 7, pass, true).unwrap() < work(7, 7, pass, false).unwrap());
    }

    #[test]
    fn all_weighted_three_node_topologies_match_decoded_calls_and_dense_oracle() {
        for mask in 0u16..512 {
            let edges: Vec<_> = (0..9)
                .filter(|bit| mask & (1 << bit) != 0)
                .map(|bit| (bit / 3, bit % 3, [0.0, 0.5, 3.0, 7.0][bit % 4]))
                .collect();
            let fixture = Fixture::new(3, &edges);
            let mut dense = [[f64::INFINITY; 3]; 3];
            for (source, row) in dense.iter_mut().enumerate() {
                row[source] = 0.0;
                let (targets, weights) = fixture.view.projected_row(source).unwrap();
                for (&target, &weight) in targets.iter().zip(weights) {
                    row[target] = row[target].min(weight);
                }
            }
            for via in 0..3 {
                for source in 0..3 {
                    for target in 0..3 {
                        dense[source][target] =
                            dense[source][target].min(dense[source][via] + dense[via][target]);
                    }
                }
            }
            for (source, expected) in dense.iter().enumerate() {
                for cutoff in [None, Some(0.0), Some(0.5), Some(3.0)] {
                    for strict in [false, true] {
                        let (call, options) =
                            bound(fixture.view.vertex_id(source).unwrap(), cutoff, strict);
                        let admission = ResultAdmission::new(&call, limits(3), memory()).unwrap();
                        fixture.calls.set(0);
                        let output =
                            run(&fixture, source, options, &admission, &mut || Ok(())).unwrap();
                        let with_validation = fixture.calls.get();
                        fixture.calls.set(0);
                        let admitted = run(
                            &Nonnegative::new(&fixture),
                            source,
                            options,
                            &admission,
                            &mut || Ok(()),
                        )
                        .unwrap();
                        assert_eq!(values(&admitted), values(&output));
                        assert_eq!(admitted.row_count, output.row_count);
                        assert_eq!(admitted.witness, output.witness);
                        // One open and EOF per vertex, plus every reduced arc,
                        // disappear. Reachable relaxation visits are unchanged.
                        assert_eq!(fixture.calls.get() + 2 * 3 + edges.len(), with_validation);
                        let oracle = call
                            .execute(&fixture.view, limits(3), || Ok::<(), Infallible>(()))
                            .unwrap();
                        let actual = values(&output);
                        let expected: Vec<_> = expected
                            .iter()
                            .copied()
                            .map(|cost| {
                                (cost.is_finite() && cutoff.is_none_or(|cutoff| cost <= cutoff))
                                    .then_some(cost)
                            })
                            .collect();
                        assert_eq!(
                            actual
                                .iter()
                                .map(|x| x.map(f64::to_bits))
                                .collect::<Vec<_>>(),
                            expected
                                .iter()
                                .map(|x| x.map(f64::to_bits))
                                .collect::<Vec<_>>()
                        );
                        let rows: Vec<_> = actual
                            .iter()
                            .enumerate()
                            .filter_map(|(index, cost)| {
                                cost.map(|cost| {
                                    vec![
                                        FnxValue::Vertex(fixture.view.vertex_id(index).unwrap()),
                                        FnxValue::Float(cost),
                                    ]
                                })
                            })
                            .collect();
                        assert_eq!(
                            rows, oracle.rows,
                            "mask={mask} source={source} cutoff={cutoff:?} strict={strict}"
                        );
                        assert_eq!(output.row_count, rows.len());
                        assert_eq!(
                            output.witness.nodes_touched,
                            oracle.certificate.witness.nodes_touched
                        );
                        assert_eq!(
                            output.witness.edges_scanned,
                            oracle.certificate.witness.edges_scanned
                        );
                        assert_eq!(
                            output.witness.queue_peak,
                            oracle.certificate.witness.queue_peak
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn admitted_weights_still_interrupt_each_queue_and_reachable_cursor_step() {
        for weight in [0.0, 1.0, 2.5] {
            let mut fixture = Fixture::new(
                5,
                &[
                    (0, 1, weight),
                    (1, 2, weight),
                    (2, 0, weight),
                    (3, 4, weight),
                ],
            );
            let (call, options) = bound(fixture.view.vertex_id(0).unwrap(), None, true);
            let admission = ResultAdmission::new(&call, limits(5), memory()).unwrap();
            let mut checkpoints = 0;
            let expected = run(
                &Nonnegative::new(&fixture),
                0,
                options,
                &admission,
                &mut || {
                    checkpoints += 1;
                    Ok(())
                },
            )
            .unwrap();
            let visits = fixture.calls.get();
            assert_eq!(expected.row_count, 3);
            assert_eq!(expected.witness.edges_scanned, 3);
            assert_eq!(visits, 2 * 3 + 3, "only reached rows are opened");
            for stop in 1..=checkpoints {
                let mut seen = 0;
                let result = run(
                    &Nonnegative::new(&fixture),
                    0,
                    options,
                    &admission,
                    &mut || {
                        seen += 1;
                        if seen == stop {
                            Err(Error::Cancelled(
                                std::io::Error::other("admitted stop").into(),
                            ))
                        } else {
                            Ok(())
                        }
                    },
                );
                assert!(matches!(result, Err(Error::Cancelled(_))));
                assert_eq!(seen, stop);
            }
            for stop in 1..=visits {
                fixture.calls.set(0);
                fixture.fail_at = stop;
                assert!(matches!(
                    run(
                        &Nonnegative::new(&fixture),
                        0,
                        options,
                        &admission,
                        &mut || Ok(())
                    ),
                    Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                        usize::MAX
                    )))
                ));
                assert_eq!(fixture.calls.get(), stop);
            }
        }
    }

    #[test]
    fn every_checkpoint_and_source_failure_discards_weighted_results() {
        let mut fixture = Fixture::new(
            7,
            &[
                (0, 1, 20.0),
                (0, 2, 15.0),
                (0, 3, 10.0),
                (0, 4, 5.0),
                (4, 1, 1.0),
                (4, 2, 2.0),
                (1, 3, 0.0),
                (3, 5, 1.0),
                (5, 6, 0.0),
            ],
        );
        let (call, options) = bound(fixture.view.vertex_id(0).unwrap(), None, false);
        let admission = ResultAdmission::new(&call, limits(7), memory()).unwrap();
        let mut count = 0;
        run(&fixture, 0, options, &admission, &mut || {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert!(count > 3 * fixture.node_count() + 9);
        for stop in 1..=count {
            let mut seen = 0;
            let result = run(&fixture, 0, options, &admission, &mut || {
                seen += 1;
                if seen == stop {
                    Err(Error::Cancelled(
                        std::io::Error::other("Dijkstra stop").into(),
                    ))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(Error::Cancelled(_))));
            assert_eq!(seen, stop);
        }
        fixture.calls.set(0);
        run(&fixture, 0, options, &admission, &mut || Ok(())).unwrap();
        let operations = fixture.calls.get();
        assert_eq!(operations, 2 * (2 * 7 + 9)); // two passes: opens, edges, EOFs
        for fail_at in 1..=operations {
            fixture.calls.set(0);
            fixture.fail_at = fail_at;
            assert!(matches!(
                run(&fixture, 0, options, &admission, &mut || Ok(())),
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                    usize::MAX
                )))
            ));
            assert_eq!(fixture.calls.get(), fail_at);
        }
    }

    #[test]
    fn cutoff_retains_zero_cost_closure_and_numeric_profiles_are_distinct() {
        let fixture = Fixture::new(
            5,
            &[
                (0, 1, 20.0),
                (0, 1, 2.0),
                (1, 2, 0.0),
                (2, 1, 0.0),
                (2, 3, 0.5),
            ],
        );
        assert_eq!(
            values(&execute(&fixture, Some(2.0), true).unwrap()),
            &[Some(0.0), Some(2.0), Some(2.0), None, None]
        );
        let fixture = Fixture::new(4, &[(0, 1, 0.0), (1, 2, 0.0), (2, 0, 0.0), (2, 3, 0.5)]);
        assert_eq!(
            values(&execute(&fixture, Some(0.0), false).unwrap()),
            &[Some(0.0), Some(0.0), Some(0.0), None]
        );
        let fixture = Fixture::new(3, &[(0, 1, 1.0), (0, 2, 0.5), (2, 1, 0.5 - 5e-13)]);
        assert_eq!(
            values(&execute(&fixture, None, false).unwrap())[1],
            Some(1.0)
        );
        assert!(values(&execute(&fixture, None, true).unwrap())[1].unwrap() < 1.0);
        let (_, loose) = bound(VId(1), None, false);
        let (_, strict) = bound(VId(1), None, true);
        assert_eq!(loose.comparison(), DijkstraComparison::FnxEpsilon);
        assert_eq!(strict.comparison(), DijkstraComparison::Strict);
    }

    #[test]
    fn whole_projection_validation_and_overflow_do_not_hide_reachable_vertices() {
        let fixture = Fixture::new(4, &[(2, 3, -1.0)]);
        assert!(matches!(
            execute(&fixture, Some(0.0), true),
            Err(Error::Execution(ExecutionError::NegativeWeight))
        ));
        let mut edges = vec![(0, 1, f64::MAX * 0.75), (1, 3, f64::MAX * 0.75)];
        let fixture = Fixture::new(4, &edges);
        assert!(matches!(
            execute(&fixture, None, true),
            Err(Error::Execution(ExecutionError::InvalidNumericResult))
        ));
        assert_eq!(
            execute(&fixture, Some(f64::MAX), true).unwrap().row_count,
            2
        );
        edges.extend([(0, 2, f64::MAX * 0.875), (2, 3, 0.0)]);
        let fixture = Fixture::new(4, &edges);
        assert_eq!(
            values(&execute(&fixture, None, true).unwrap())[3],
            Some(f64::MAX * 0.875)
        );

        struct Malformed {
            target: usize,
            weight: f64,
        }
        struct One(Option<(usize, f64)>);
        impl Cursor for One {
            fn next(&mut self) -> Result<Option<(usize, f64)>> {
                Ok(self.0.take())
            }
        }
        impl Rows for Malformed {
            type Cursor<'a>
                = One
            where
                Self: 'a;
            fn node_count(&self) -> usize {
                2
            }
            fn degree(&self, _: usize) -> Option<usize> {
                Some(1)
            }
            fn open(&self, _: usize) -> Result<One> {
                Ok(One(Some((self.target, self.weight))))
            }
        }
        let (call, options) = bound(VId(0), Some(0.0), true);
        let admission = ResultAdmission::new(&call, limits(2), memory()).unwrap();
        for weight in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                run(
                    &Malformed { target: 1, weight },
                    0,
                    options,
                    &admission,
                    &mut || Ok(())
                ),
                Err(Error::Execution(ExecutionError::InvalidNumericResult))
            ));
        }
        assert!(matches!(
            run(
                &Malformed {
                    target: usize::MAX,
                    weight: 1.0
                },
                0,
                options,
                &admission,
                &mut || Ok(())
            ),
            Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
        ));
    }

    #[test]
    fn result_admission_uses_discovery_and_history_and_workspace_are_checked() {
        let fixture = Fixture::new(20, &[]);
        let (call, options) = bound(fixture.view.vertex_id(0).unwrap(), None, true);
        let mut admission = ResultAdmission::new(&call, limits(1), memory()).unwrap();
        admission.max_bytes = admission.column_bytes + admission.row_bytes;
        assert_eq!(
            run(&fixture, 0, options, &admission, &mut || Ok(()))
                .unwrap()
                .row_count,
            1
        );
        let mut fixture = Fixture::new(20, &[(0, 1, 0.0)]);
        fixture.fail_at = 1;
        admission.max_rows = 0;
        assert!(matches!(
            run(&fixture, 0, options, &admission, &mut || Ok(())),
            Err(Error::Execution(ExecutionError::LimitExceeded {
                resource: "result rows",
                ..
            }))
        ));
        assert_eq!(fixture.calls.get(), 0);
        assert!(matches!(
            run(&fixture, 20, options, &admission, &mut || Ok(())),
            Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
        ));
        fixture.fail_at = usize::MAX;
        admission.max_rows = 1;
        assert!(matches!(
            run(&fixture, 0, options, &admission, &mut || Ok(())),
            Err(Error::Execution(ExecutionError::LimitExceeded {
                resource: "result rows",
                ..
            }))
        ));
        admission.max_rows = 2;
        assert!(matches!(
            run(&fixture, 0, options, &admission, &mut || Ok(())),
            Err(Error::Execution(ExecutionError::LimitExceeded {
                resource: "result bytes",
                ..
            }))
        ));
        admission.max_bytes += admission.row_bytes;
        assert_eq!(
            run(&fixture, 0, options, &admission, &mut || Ok(()))
                .unwrap()
                .row_count,
            2
        );
        for unit in [false, true] {
            assert_eq!(workspace(0, unit).unwrap(), 0);
            assert!(workspace(usize::MAX, unit).is_err());
            assert!(work(usize::MAX, usize::MAX, usize::MAX, unit).is_err());
            assert!(
                work(3, 2, super::super::pass_work(3, 1000).unwrap(), unit).unwrap()
                    > work(3, 2, super::super::pass_work(3, 2).unwrap(), unit).unwrap()
            );
        }
    }

    #[test]
    fn implicit_deep_paths_and_hubs_do_not_materialize_adjacency_or_duplicate_queue_entries() {
        struct Implicit {
            n: usize,
            hub: bool,
        }
        struct Row {
            next: usize,
            end: usize,
            weight: Option<f64>,
        }
        impl Cursor for Row {
            fn next(&mut self) -> Result<Option<(usize, f64)>> {
                if self.next == self.end {
                    return Ok(None);
                }
                let target = self.next;
                self.next += 1;
                Ok(Some((
                    target,
                    self.weight.unwrap_or((self.end - target) as f64),
                )))
            }
        }
        impl Rows for Implicit {
            type Cursor<'a>
                = Row
            where
                Self: 'a;
            fn node_count(&self) -> usize {
                self.n
            }
            fn degree(&self, node: usize) -> Option<usize> {
                if node >= self.n {
                    return None;
                }
                Some(if self.hub {
                    if node == 0 { self.n - 1 } else { 0 }
                } else {
                    usize::from(node + 1 < self.n)
                })
            }
            fn open(&self, node: usize) -> Result<Row> {
                if node >= self.n {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                Ok(if self.hub && node == 0 {
                    Row {
                        next: 1,
                        end: self.n,
                        weight: None,
                    }
                } else if !self.hub && node + 1 < self.n {
                    Row {
                        next: node + 1,
                        end: node + 2,
                        weight: Some(0.5),
                    }
                } else {
                    Row {
                        next: 0,
                        end: 0,
                        weight: None,
                    }
                })
            }
        }
        let n = 20_000;
        let (call, options) = bound(VId(0), None, true);
        let admission = ResultAdmission::new(&call, limits(n), memory()).unwrap();
        for hub in [false, true] {
            let output = run(&Implicit { n, hub }, 0, options, &admission, &mut || Ok(())).unwrap();
            assert_eq!(output.row_count, n);
            assert_eq!(output.witness.edges_scanned, n - 1);
            assert_eq!(output.witness.queue_peak, if hub { n - 1 } else { 1 });
            for (node, &cost) in values(&output).iter().enumerate() {
                let expected = if node == 0 {
                    0.0
                } else if hub {
                    (n - node) as f64
                } else {
                    node as f64 * 0.5
                };
                assert_eq!(cost, Some(expected));
            }
        }
    }
}
