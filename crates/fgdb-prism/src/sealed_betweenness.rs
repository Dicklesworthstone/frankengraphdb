//! Exact, source-ordered Brandes betweenness over compressed undirected rows.
//!
//! Keep one FIFO, distances, path counts, dependencies and scores. Predecessors
//! are the neighbors one level nearer the source, so neither adjacency nor
//! predecessor lists are copied. Sources and reverse FIFO dependencies retain
//! the decoded kernel's arithmetic order. Reset only reached vertices: an
//! isolated population does not perform a quadratic clear or zero reduction.
//! Refuse nonfinite shortest-path counts before dependency division: a finite
//! count divided by infinity would silently discard a real contribution even
//! when every final score remained finite.

use super::{
    ComplexityWitness, Cursor, ExecutionError, Finished, FnxCallSpec, FnxExecutionLimits,
    FnxMemoryLimits, KernelOutput, KernelValues, Result, ResultAdmission, Rows, SealedGraphView,
    SealedRows, add, admit, checkpoint, directional_pass_work, finish, mul,
    require_loop_free_degree, reserve,
};
use crate::sealed_control::Control;
use std::mem::size_of;

pub(super) fn workspace(n: usize) -> Result<usize> {
    mul(n, 2 * size_of::<usize>() + 3 * size_of::<f64>())
}

pub(super) fn work(n: usize, arcs: usize, pass: usize, columns: usize) -> Result<usize> {
    if n == 0 {
        return Ok(0);
    }
    // A connected component with r vertices needs at least r-1 distinct
    // undirected edges. Each row is therefore revisited from at most this
    // many sources. This also bounds reached-only resets and score additions;
    // isolated populations admit linear work rather than a fictitious V^2.
    // `pass` includes BOTH compressed faces, retained history and excluded
    // endpoint searches, not merely the reduced simple graph's arcs.
    let component = n.min(add(arcs / 2, 1)?);
    let per_component = add(mul(pass, 2)?, add(mul(arcs, 2)?, mul(n, 8)?)?)?;
    add(
        mul(component, per_component)?,
        add(mul(n, add(columns, 24)?)?, add(columns, 16)?)?,
    )
}

pub(super) fn witness() -> ComplexityWitness {
    ComplexityWitness {
        algorithm: "brandes_betweenness_compressed_fifo".to_owned(),
        complexity_claim: "O(V + C*(V*(1+log(1+H)) + H*(1+log(1+H)+log(1+V)))); C <= min(V,E+1) bounds component size, E is projected undirected edges and H is retained incidence count; O(V) kernel workspace".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    }
}

fn reserved<T>(
    n: usize,
    live: &mut usize,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<T>> {
    control()?;
    admit(
        "kernel workspace bytes",
        add(*live, mul(n, size_of::<T>())?)?,
        limit,
    )?;
    let values = reserve(n)?;
    let actual = add(*live, mul(values.capacity(), size_of::<T>())?)?;
    // Admit actual capacity before initializing it or allocating another
    // vector; try_reserve_exact need not return exactly the requested backing.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

fn next(
    row: &mut impl Cursor,
    source: usize,
    n: usize,
    previous: &mut Option<usize>,
    seen: &mut usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<Option<usize>> {
    control()?;
    let Some((target, _)) = row.next()? else {
        return Ok(None);
    };
    if target >= n || previous.is_some_and(|last| last >= target) {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    if target == source {
        return Err(ExecutionError::SelfLoopRefused.into());
    }
    *previous = Some(target);
    *seen = add(*seen, 1)?;
    Ok(Some(target))
}

fn check_degree(graph: &impl Rows, source: usize, seen: usize) -> Result<()> {
    if graph.degree(source) != Some(seen) {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    Ok(())
}

fn run(
    graph: &impl Rows,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    control()?;
    let n = graph.node_count();
    admit("kernel workspace bytes", workspace(n)?, limit)?;
    let mut live = 0;
    let mut distances = reserved(n, &mut live, limit, control)?;
    let mut queue = reserved(n, &mut live, limit, control)?;
    let mut sigma = reserved(n, &mut live, limit, control)?;
    let mut delta = reserved(n, &mut live, limit, control)?;
    let mut scores = reserved(n, &mut live, limit, control)?;
    for _ in 0..n {
        control()?;
        distances.push(usize::MAX);
        sigma.push(0.0f64);
        delta.push(0.0f64);
        scores.push(0.0f64);
    }
    let mut witness = witness();
    for source in 0..n {
        control()?;
        queue.clear();
        queue.push(source);
        sigma[source] = 1.0;
        distances[source] = 0;
        witness.queue_peak = witness.queue_peak.max(1);
        let mut head = 0;
        while head < queue.len() {
            control()?;
            let vertex = queue[head];
            head += 1;
            let next_distance = add(distances[vertex], 1)?;
            let sigma_vertex = sigma[vertex];
            let mut row = graph.open(vertex)?;
            let (mut previous, mut seen) = (None, 0);
            while let Some(neighbor) = next(&mut row, vertex, n, &mut previous, &mut seen, control)?
            {
                if distances[neighbor] == usize::MAX {
                    distances[neighbor] = next_distance;
                    // Every vertex is enqueued once; reserved backing cannot
                    // grow while a row cursor is live.
                    if queue.len() == n {
                        return Err(ExecutionError::InvalidUpstreamResult.into());
                    }
                    queue.push(neighbor);
                    witness.queue_peak = witness.queue_peak.max(queue.len() - head);
                }
                if distances[neighbor] == next_distance {
                    sigma[neighbor] += sigma_vertex;
                    // Finite/Inf evaluates to zero, so a finite final score
                    // does not prove that its path ratios were valid. Stop at
                    // the overflowing addition before dependency propagation.
                    if !sigma[neighbor].is_finite() {
                        return Err(ExecutionError::InvalidNumericResult.into());
                    }
                }
            }
            check_degree(graph, vertex, seen)?;
            witness.edges_scanned = add(witness.edges_scanned, seen)?;
            witness.nodes_touched = add(witness.nodes_touched, 1)?;
        }

        for &vertex in queue.iter().rev() {
            control()?;
            if distances[vertex] <= 1 {
                // Level-one dependencies only update the excluded source.
                // This is the decoded Brandes kernel's exact skip policy.
                if vertex == source {
                    delta[vertex] = 0.0;
                }
                continue;
            }
            let sigma_vertex = sigma[vertex];
            let delta_vertex = delta[vertex];
            if sigma_vertex > 0.0 {
                let previous_distance = distances[vertex] - 1;
                let mut row = graph.open(vertex)?;
                let (mut previous, mut seen) = (None, 0);
                while let Some(predecessor) =
                    next(&mut row, vertex, n, &mut previous, &mut seen, control)?
                {
                    if distances[predecessor] == previous_distance {
                        // Division, then multiplication, then addition in
                        // reverse FIFO order. Hoisting a reciprocal changes
                        // canonical f64 bits on non-associative path counts.
                        delta[predecessor] +=
                            (sigma[predecessor] / sigma_vertex) * (1.0 + delta_vertex);
                    }
                }
                check_degree(graph, vertex, seen)?;
                witness.edges_scanned = add(witness.edges_scanned, seen)?;
            }
        }
        for &vertex in &queue {
            control()?;
            // Targets are independent. Each target still receives its source
            // contributions in ascending source order, exactly as the decoded
            // parallel source windows do. Omitted unreachable additions are +0.
            scores[vertex] += delta[vertex];
            if !scores[vertex].is_finite() {
                // Nonnegative future contributions and the finite positive
                // normalization cannot repair NaN/Inf. Refuse the same invalid
                // numeric result as the decoded final-score check, without
                // running every remaining source after the failure is known.
                return Err(ExecutionError::InvalidNumericResult.into());
            }
            distances[vertex] = usize::MAX;
            sigma[vertex] = 0.0;
            delta[vertex] = 0.0;
        }
    }
    let pair_base = n.saturating_sub(1);
    let scale = if pair_base < 2 {
        1.0
    } else {
        1.0 / mul(pair_base, pair_base - 1)? as f64
    };
    for score in &mut scores {
        control()?;
        *score *= scale;
        if !score.is_finite() {
            return Err(ExecutionError::InvalidNumericResult.into());
        }
    }
    control()?;
    Ok((
        KernelOutput {
            values: KernelValues::Scores(scores),
            row_count: n,
            witness,
        },
        live,
    ))
}

pub(super) fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<usize> {
    let n = graph.node_count();
    admission.rows(n)?;
    let pass = directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?;
    let estimated = work(n, graph.adjacency_entry_count(), pass, call.outputs().len())?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    admit(
        "kernel workspace bytes",
        workspace(n)?,
        memory.max_kernel_workspace_bytes,
    )?;
    require_loop_free_degree(graph.edge_count(), graph.adjacency_entry_count())?;
    Ok(estimated)
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Finished> {
    let estimated = prepare(call, graph, limits, memory, admission)?;
    let rows = SealedRows { cx, graph };
    let mut control = || checkpoint(cx);
    let (output, peak) = run(&rows, memory.max_kernel_workspace_bytes, &mut control)?;
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-betweenness-brandes-v1",
        estimated,
        peak,
        admission,
        &mut control,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Directedness, FoundationAlgorithm, GraphView, ParallelEdgePolicy, ProjectionEdge,
        ProjectionLimits, ProjectionSpec, SealedProjectionError, SelfLoopPolicy, SnapshotBinding,
        SnapshotGraphView,
    };
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
    use std::cell::Cell;
    use std::convert::Infallible;

    struct Fixture {
        rows: Vec<Vec<usize>>,
        opens: Vec<Cell<usize>>,
        steps: Cell<usize>,
        fail_at: usize,
        degrees: Option<Vec<usize>>,
    }

    impl Fixture {
        fn new(rows: Vec<Vec<usize>>) -> Self {
            Self {
                opens: (0..rows.len()).map(|_| Cell::new(0)).collect(),
                rows,
                steps: Cell::new(0),
                fail_at: usize::MAX,
                degrees: None,
            }
        }

        fn step(&self) -> Result<()> {
            let next = self.steps.get() + 1;
            self.steps.set(next);
            if next == self.fail_at {
                Err(stopped())
            } else {
                Ok(())
            }
        }
    }

    struct Row<'a> {
        fixture: &'a Fixture,
        values: std::slice::Iter<'a, usize>,
    }

    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.fixture.step()?;
            Ok(self.values.next().map(|&target| (target, -7.0)))
        }
    }

    impl Rows for Fixture {
        type Cursor<'a> = Row<'a>;
        fn node_count(&self) -> usize {
            self.rows.len()
        }
        fn degree(&self, source: usize) -> Option<usize> {
            self.degrees.as_ref().map_or_else(
                || self.rows.get(source).map(Vec::len),
                |degrees| degrees.get(source).copied(),
            )
        }
        fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
            self.step()?;
            let row = self
                .rows
                .get(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            self.opens[source].set(self.opens[source].get() + 1);
            Ok(Row {
                fixture: self,
                values: row.iter(),
            })
        }
    }

    fn stopped() -> super::super::Error {
        super::super::Error::Source(SealedProjectionError::UnknownOrdinal(usize::MAX))
    }

    fn rows(n: usize, edges: &[(usize, usize)]) -> Vec<Vec<usize>> {
        let mut rows = vec![Vec::new(); n];
        for &(left, right) in edges {
            rows[left].push(right);
            rows[right].push(left);
        }
        for row in &mut rows {
            row.sort_unstable();
        }
        rows
    }

    fn projection(n: usize, pairs: &[(usize, usize)]) -> SnapshotGraphView {
        let id = |at: usize| {
            if at + 1 == n {
                VId(u128::MAX)
            } else {
                VId((1u128 << 100) + ((at as u128) << 40) + 3)
            }
        };
        let vertices: Vec<_> = (0..n).rev().map(id).collect();
        let edges: Vec<_> = pairs
            .iter()
            .rev()
            .enumerate()
            .map(|(at, &(a, b))| ProjectionEdge {
                eid: EId(at as u128 + 1),
                source: id(b),
                target: id(a),
                weight: if at % 2 == 0 { -3.0 } else { 7.0 },
            })
            .collect();
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([19; 32]),
                as_of: CommitSeq(31),
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
                max_adjacency_entries: 2 * edges.len(),
                max_workspace_bytes: 1 << 28,
            },
        )
        .unwrap()
    }

    fn scores(output: KernelOutput) -> Vec<f64> {
        let KernelValues::Scores(values) = output.values else {
            panic!("BC produces scores")
        };
        values
    }
    fn bits(values: &[f64]) -> Vec<u64> {
        values.iter().map(|v| v.to_bits()).collect()
    }
    fn decoded(graph: &SnapshotGraphView) -> Vec<f64> {
        scores(
            crate::foundation::run(
                graph,
                FoundationAlgorithm::BetweennessCentrality,
                graph.edge_count() * 2,
                &mut || Ok::<(), Infallible>(()),
            )
            .unwrap()
            .0,
        )
    }

    // Independent successor-gather formulation inspired by GAPBS BCVerifier:
    // https://github.com/sbeamer/gapbs/blob/941462c7c42e786e4e006a9211b0576ef78d23f7/src/bc.cc
    // Rebuild depth groups, then gather each vertex's successors instead of
    // pushing predecessor dependencies in reverse FIFO order. The oracle uses
    // all sources, excludes source endpoints, and uses this registry's f64
    // pair normalization. GAPBS samples sources, includes source dependency,
    // and max-normalizes float scores, so unmodified GAPBS output is NOT an
    // exact oracle for this procedure. Different sum orders use a tolerance.
    fn level_successor_oracle(rows: &[Vec<usize>]) -> Vec<f64> {
        let n = rows.len();
        let mut scores = vec![0.0; n];
        for source in 0..n {
            let mut depths = vec![usize::MAX; n];
            let mut paths = vec![0.0; n];
            let mut queue = vec![source];
            depths[source] = 0;
            paths[source] = 1.0;
            let mut at = 0;
            while at < queue.len() {
                let vertex = queue[at];
                at += 1;
                let paths_vertex = paths[vertex];
                for &neighbor in &rows[vertex] {
                    if depths[neighbor] == usize::MAX {
                        depths[neighbor] = depths[vertex] + 1;
                        queue.push(neighbor);
                    }
                    if depths[neighbor] == depths[vertex] + 1 {
                        paths[neighbor] += paths_vertex;
                    }
                }
            }
            let max_depth = queue.iter().map(|&v| depths[v]).max().unwrap();
            let mut dependency = vec![0.0; n];
            for depth in (1..=max_depth).rev() {
                for vertex in 0..n {
                    if depths[vertex] != depth {
                        continue;
                    }
                    for &successor in &rows[vertex] {
                        if depths[successor] == depth + 1 {
                            let successor_dependency = dependency[successor];
                            dependency[vertex] +=
                                (paths[vertex] / paths[successor]) * (1.0 + successor_dependency);
                        }
                    }
                    scores[vertex] += dependency[vertex];
                }
            }
        }
        if n > 2 {
            for score in &mut scores {
                *score /= ((n - 1) * (n - 2)) as f64;
            }
        }
        scores
    }

    // Independent path enumeration. Floyd-Warshall determines shortest lengths;
    // every ordered source/target pair then enumerates its shortest paths and
    // counts interior vertices, without Brandes dependency accumulation.
    fn path_oracle(rows: &[Vec<usize>]) -> Vec<f64> {
        let n = rows.len();
        let mut distance = vec![vec![usize::MAX; n]; n];
        for source in 0..n {
            distance[source][source] = 0;
            for &target in &rows[source] {
                distance[source][target] = 1;
            }
        }
        for middle in 0..n {
            for source in 0..n {
                for target in 0..n {
                    if let Some(candidate) =
                        distance[source][middle].checked_add(distance[middle][target])
                    {
                        distance[source][target] = distance[source][target].min(candidate);
                    }
                }
            }
        }
        fn visit(
            rows: &[Vec<usize>],
            distance: &[Vec<usize>],
            target: usize,
            path: &mut Vec<usize>,
            total: &mut usize,
            interior: &mut [usize],
        ) {
            let vertex = *path.last().unwrap();
            if vertex == target {
                *total += 1;
                for &at in &path[1..path.len() - 1] {
                    interior[at] += 1;
                }
            } else {
                for &next in &rows[vertex] {
                    if distance[next][target].checked_add(1) == Some(distance[vertex][target]) {
                        path.push(next);
                        visit(rows, distance, target, path, total, interior);
                        path.pop();
                    }
                }
            }
        }
        let mut scores = vec![0.0; n];
        for source in 0..n {
            for target in 0..n {
                if source == target || distance[source][target] == usize::MAX {
                    continue;
                }
                let mut interior = vec![0; n];
                let mut total = 0;
                visit(
                    rows,
                    &distance,
                    target,
                    &mut vec![source],
                    &mut total,
                    &mut interior,
                );
                for (score, count) in scores.iter_mut().zip(interior) {
                    *score += count as f64 / total as f64;
                }
            }
        }
        if n > 2 {
            for score in &mut scores {
                *score /= ((n - 1) * (n - 2)) as f64;
            }
        }
        scores
    }

    #[test]
    fn every_small_undirected_topology_matches_decoded_bits_and_path_enumeration() {
        let mut cases = 0;
        for n in 0..=5 {
            let pairs: Vec<_> = (0..n)
                .flat_map(|a| (a + 1..n).map(move |b| (a, b)))
                .collect();
            for mask in 0usize..1usize << pairs.len() {
                let edges: Vec<_> = pairs
                    .iter()
                    .enumerate()
                    .filter(|(at, _)| mask & (1 << at) != 0)
                    .map(|(_, &pair)| pair)
                    .collect();
                let graph = projection(n, &edges);
                let fixture = Fixture::new(rows(n, &edges));
                let (actual, peak) = run(&fixture, workspace(n).unwrap(), &mut || Ok(())).unwrap();
                assert_eq!(actual.row_count, n);
                assert_eq!(peak, workspace(n).unwrap());
                let actual = scores(actual);
                assert_eq!(bits(&actual), bits(&decoded(&graph)), "n={n} mask={mask}");
                for oracle in [
                    path_oracle(&fixture.rows),
                    level_successor_oracle(&fixture.rows),
                ] {
                    for (value, expected) in actual.iter().zip(oracle) {
                        assert!(
                            (value - expected).abs() < 1e-14,
                            "n={n} mask={mask}: {value} != {expected}"
                        );
                    }
                }
                cases += 1;
            }
        }
        assert_eq!(cases, 1100);
    }

    #[test]
    fn source_order_matches_decoded_windows_above_parallel_threshold() {
        let n = 513;
        let mut edges = Vec::new();
        for source in 0..n - 1 {
            for gap in [1, 3, 7] {
                let target = source + gap;
                if target < n - 1 && source / 97 == target / 97 {
                    edges.push((source, target));
                }
            }
        }
        let graph = projection(n, &edges);
        let fixture = Fixture::new(rows(n, &edges));
        let (actual, peak) = run(&fixture, workspace(n).unwrap(), &mut || Ok(())).unwrap();
        let actual = scores(actual);
        assert_eq!(bits(&actual), bits(&decoded(&graph)));
        for (value, expected) in actual.iter().zip(level_successor_oracle(&fixture.rows)) {
            assert!((value - expected).abs() < 1e-13);
        }
        assert_eq!(actual[n - 1].to_bits(), 0.0f64.to_bits());
        assert_eq!(peak, workspace(n).unwrap());
    }

    #[test]
    fn non_associative_path_counts_keep_reverse_fifo_arithmetic() {
        let width = 12;
        let layers = 27;
        let n = 1 + width * layers;
        let mut edges: Vec<_> = (1..=width).map(|v| (0, v)).collect();
        for layer in 1..layers {
            for left in 0..width {
                for right in 0..width {
                    if (left * 13 + right * 7 + layer) % 11 < 9 {
                        edges.push((1 + (layer - 1) * width + left, 1 + layer * width + right));
                    }
                }
            }
        }
        let graph = projection(n, &edges);
        let fixture = Fixture::new(rows(n, &edges));
        let actual = scores(
            run(&fixture, workspace(n).unwrap(), &mut || Ok(()))
                .unwrap()
                .0,
        );
        assert_eq!(bits(&actual), bits(&decoded(&graph)));
    }

    #[test]
    fn every_cursor_and_scalar_refusal_stops_without_returning_a_prefix() {
        let edges = [(0, 1), (0, 2), (1, 3), (2, 3), (3, 4), (1, 4)];
        let fixture = Fixture::new(rows(6, &edges));
        let mut ticks = 0;
        let expected = scores(
            run(&fixture, usize::MAX, &mut || {
                ticks += 1;
                Ok(())
            })
            .unwrap()
            .0,
        );
        for stop in 1..=fixture.steps.get() {
            let mut failed = Fixture::new(rows(6, &edges));
            failed.fail_at = stop;
            assert!(run(&failed, usize::MAX, &mut || Ok(())).is_err());
            assert_eq!(failed.steps.get(), stop);
        }
        for stop in 1..=ticks {
            let fixture = Fixture::new(rows(6, &edges));
            let mut calls = 0;
            assert!(
                run(&fixture, usize::MAX, &mut || {
                    calls += 1;
                    if calls == stop {
                        Err(stopped())
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert_eq!(calls, stop);
        }
        assert_eq!(
            bits(&scores(
                run(&fixture, usize::MAX, &mut || Ok(())).unwrap().0
            )),
            bits(&expected)
        );
    }

    #[test]
    fn malformed_rows_loops_memory_limits_and_work_overflow_refuse() {
        for values in [
            vec![vec![2], vec![]],
            vec![vec![1, 1], vec![0]],
            vec![vec![2, 1], vec![0], vec![0]],
        ] {
            assert!(matches!(
                run(&Fixture::new(values), usize::MAX, &mut || Ok(())),
                Err(super::super::Error::Execution(
                    ExecutionError::InvalidUpstreamResult
                ))
            ));
        }
        assert!(matches!(
            run(&Fixture::new(vec![vec![0]]), usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::SelfLoopRefused
            ))
        ));
        for degree in [0, 2] {
            let mut fixture = Fixture::new(vec![vec![1], vec![0]]);
            fixture.degrees = Some(vec![degree, 1]);
            assert!(run(&fixture, usize::MAX, &mut || Ok(())).is_err());
        }
        let fixture = Fixture::new(rows(7, &[(0, 1), (1, 2), (3, 4)]));
        let limit = workspace(7).unwrap();
        assert!(run(&fixture, limit, &mut || Ok(())).is_ok());
        let untouched = Fixture::new(fixture.rows.clone());
        assert!(run(&untouched, limit - 1, &mut || Ok(())).is_err());
        assert_eq!(untouched.steps.get(), 0);
        assert!(workspace(usize::MAX).is_err());
        assert!(work(usize::MAX, 2, 1, 2).is_err());
        assert!(work(3, 2, usize::MAX, 2).is_err());
        assert!(work(3, usize::MAX, 1, 2).is_err());
        let pass = directional_pass_work(7, 1000, Directedness::Undirected).unwrap();
        let estimate = work(7, 6, pass, 2).unwrap();
        assert!(admit("estimated work", estimate, estimate).is_ok());
        assert!(admit("estimated work", estimate, estimate - 1).is_err());
        assert!(
            estimate
                > work(
                    7,
                    6,
                    directional_pass_work(7, 6, Directedness::Undirected).unwrap(),
                    2
                )
                .unwrap()
        );
    }

    #[test]
    fn isolated_population_and_sparse_hub_keep_linear_scratch_and_reached_resets() {
        let n = 8193;
        let fixture = Fixture::new(vec![Vec::new(); n]);
        let mut ticks = 0;
        let (output, peak) = run(&fixture, workspace(n).unwrap(), &mut || {
            ticks += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(output.witness.nodes_touched, n);
        assert_eq!(output.witness.edges_scanned, 0);
        assert_eq!(output.witness.queue_peak, 1);
        assert!(fixture.opens.iter().all(|value| value.get() == 1));
        assert_eq!(peak, workspace(n).unwrap());
        assert!(ticks < 10 * n + 32);
        assert!(scores(output).iter().all(|value| value.to_bits() == 0));
        let pass = directional_pass_work(n, 0, Directedness::Undirected).unwrap();
        assert!(work(n, 0, pass, 2).unwrap() < 100 * n);

        let leaves = 64;
        let edges: Vec<_> = (1..=leaves).map(|v| (0, v)).collect();
        let fixture = Fixture::new(rows(n, &edges));
        let (output, peak) = run(&fixture, workspace(n).unwrap(), &mut || Ok(())).unwrap();
        assert_eq!(
            output.witness.nodes_touched,
            (leaves + 1) * (leaves + 1) + n - leaves - 1
        );
        assert_eq!(peak, workspace(n).unwrap());
        let actual = scores(output);
        let scale = 1.0 / ((n - 1) * (n - 2)) as f64;
        assert_eq!(
            actual[0].to_bits(),
            ((leaves * (leaves - 1)) as f64 * scale).to_bits()
        );
        assert!(actual[1..].iter().all(|value| value.to_bits() == 0));
        assert!(
            fixture.opens[leaves + 1..]
                .iter()
                .all(|value| value.get() == 1)
        );
    }

    #[test]
    fn overflowing_shortest_path_counts_refuse_even_when_scores_would_stay_finite() {
        // Each two-vertex layer doubles the shortest-path count. At layer
        // 1025 only the terminal counts overflow: finite/Inf would erase their
        // contribution while leaving finite, incorrect scores. Extra layers
        // also produce Inf/Inf and NaN, which a final-score check could catch.
        for layers in [1025, 1030] {
            let n = 2 * layers + 1;
            let mut edges = vec![(0, 1), (0, 2)];
            for layer in 1..layers {
                for left in [2 * layer - 1, 2 * layer] {
                    for right in [2 * layer + 1, 2 * layer + 2] {
                        edges.push((left, right));
                    }
                }
            }
            let fixture = Fixture::new(rows(n, &edges));
            assert!(
                matches!(
                    run(&fixture, workspace(n).unwrap(), &mut || Ok(())),
                    Err(super::super::Error::Execution(
                        ExecutionError::InvalidNumericResult
                    ))
                ),
                "layers={layers}"
            );
            // The source-zero BFS refuses before any dependency row opens,
            // so no later source or invalid path-ratio result is consumed.
            assert!(fixture.opens.iter().all(|opens| opens.get() <= 1));
            let graph = projection(n, &edges);
            assert!(
                matches!(
                    crate::foundation::run(
                        &graph,
                        FoundationAlgorithm::BetweennessCentrality,
                        2 * edges.len(),
                        &mut || Ok::<(), Infallible>(())
                    ),
                    Err(ExecutionError::InvalidNumericResult)
                ),
                "decoded layers={layers}"
            );
        }
    }
}
