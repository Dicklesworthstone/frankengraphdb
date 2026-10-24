//! Graphalytics directed local clustering over two compressed incidence faces.
//!
//! Merge the reduced outgoing and incoming rows into distinct weak neighbors.
//! A reciprocal pair retains multiplicity two, but contributes one neighbor to
//! the denominator. Orient weak edges by their retained scan cost, enumerate
//! each weak triangle once, and add the opposite side's arc multiplicity to
//! each center. Only vertex-sized counters and stamps are retained; even a hub
//! row stays a pair of compressed cursors with constant-sized lookahead.

use super::{
    ComplexityWitness, Cursor, ExecutionError, FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits,
    FnxResult, KernelOutput, KernelValues, Result, ResultAdmission, Rows, SealedGraphView,
    SealedRows, add, admit, checkpoint, directional_pass_work, finish, mul, reserve,
};
use crate::Directedness;
use crate::sealed_control::Control;
use std::mem::size_of;

fn witness() -> ComplexityWitness {
    ComplexityWitness {
        algorithm: "directed_clustering_retained_cost_oriented_rows".to_owned(),
        complexity_claim:
            "O(H log(1+H) + V + sum_v c(v) + sum_weak_edges min(c(u),c(v))); c = retained two-face scan model"
                .to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    }
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<FnxResult> {
    let n = graph.node_count();
    admission.rows(n)?;
    let mut control = || checkpoint(cx);
    if n == 0 {
        // No selected row can contribute a coefficient. In particular, an
        // empty selection must not build an incoming index of excluded data.
        return finish(
            call,
            graph,
            KernelOutput {
                values: KernelValues::Scores(Vec::new()),
                row_count: 0,
                witness: witness(),
            },
            "fgdb-prism/sealed-directed-clustering-retained-cost-v1",
            0,
            0,
            admission,
            &mut control,
        );
    }
    let arrays = workspace(n)?;
    admit(
        "kernel workspace bytes",
        arrays,
        memory.max_kernel_workspace_bytes,
    )?;
    let reverse_work = reverse_work(n, graph.scan_incidence_bound())?;
    admit("estimated work", reverse_work, limits.max_estimated_work)?;
    // The additional projection copies only its vertex directory/degrees and,
    // when needed, compressed incoming locators. Its construction peak belongs
    // to this kernel, not to the already-admitted original projection.
    let opposite =
        graph.reverse_projection_controlled(cx, memory.max_kernel_workspace_bytes - arrays)?;
    let base = opposite.charged_workspace_bytes();
    let outgoing = SealedRows { cx, graph };
    let incoming = SealedRows {
        cx,
        graph: &opposite,
    };
    let plan = prepare(
        &outgoing,
        &incoming,
        graph.scan_incidence_bound(),
        |source| {
            add(
                graph.retained_row_incidence_bound(source)?,
                opposite.retained_row_incidence_bound(source)?,
            )
        },
        reverse_work,
        limits.max_estimated_work,
        base,
        memory.max_kernel_workspace_bytes,
        &mut control,
    )?;
    let estimated_work = plan.work;
    let (output, workspace) = run(
        &outgoing,
        &incoming,
        plan,
        memory.max_kernel_workspace_bytes,
        &mut control,
    )?;
    drop(opposite);
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-directed-clustering-retained-cost-v1",
        estimated_work,
        workspace,
        admission,
        &mut control,
    )
}

fn bits(value: usize) -> usize {
    (usize::BITS - value.leading_zeros()) as usize
}

fn reverse_work(n: usize, history: usize) -> Result<usize> {
    // Directory copying, reduction and hashing plus incoming-locator building
    // and sorting. Count retained source history, including excluded endpoints,
    // rather than the small visible graph that could hide a large construction.
    add(
        directional_pass_work(n, history, Directedness::Undirected)?,
        add(mul(history, add(64, mul(8, bits(history))?)?)?, mul(n, 32)?)?,
    )
}

fn workspace(n: usize) -> Result<usize> {
    let counting = mul(n, 4 * size_of::<usize>() + size_of::<u8>())?;
    let conversion = mul(n, 2 * size_of::<usize>() + size_of::<f64>())?;
    Ok(counting.max(conversion))
}

fn vector_bytes<T>(values: &Vec<T>) -> Result<usize> {
    mul(values.capacity(), size_of::<T>())
}

fn reserved<T>(n: usize, live: &mut usize, limit: usize) -> Result<Vec<T>> {
    admit(
        "kernel workspace bytes",
        add(*live, mul(n, size_of::<T>())?)?,
        limit,
    )?;
    let values = reserve(n)?;
    let actual = add(*live, vector_bytes(&values)?)?;
    // Allocators may provide more capacity than requested. Account for that
    // backing before initialization or another allocation, as other kernels do.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

fn row_work(n: usize, history: usize, incidences: usize) -> Result<usize> {
    let search = mul(8, bits(history))?;
    add(
        add(32, search)?,
        mul(incidences, add(64, add(search, mul(2, bits(n))?)?)?)?,
    )
}

fn charge(work: &mut usize, amount: usize, limit: usize) -> Result<()> {
    let next = add(*work, amount)?;
    admit("estimated work", next, limit)?;
    *work = next;
    Ok(())
}

fn lower(costs: &[usize], left: usize, right: usize) -> bool {
    (costs[left], left) < (costs[right], right)
}

struct Face<C> {
    cursor: C,
    head: Option<usize>,
    previous: Option<usize>,
    seen: usize,
    expected: usize,
    ended: bool,
}

impl<C: Cursor> Face<C> {
    fn new(cursor: C, expected: usize) -> Self {
        Self {
            cursor,
            head: None,
            previous: None,
            seen: 0,
            expected,
            ended: false,
        }
    }

    fn fill(&mut self, n: usize, control: &mut impl FnMut() -> Result<()>) -> Result<()> {
        if self.head.is_some() || self.ended {
            return Ok(());
        }
        control()?;
        match self.cursor.next()? {
            Some((target, _)) => {
                if target >= n || self.previous.is_some_and(|last| last >= target) {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                self.seen = add(self.seen, 1)?;
                if self.seen > self.expected {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                self.head = Some(target);
                self.previous = Some(target);
            }
            None => {
                if self.seen != self.expected {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                self.ended = true;
            }
        }
        Ok(())
    }
}

struct WeakNeighbors<Out, In> {
    outgoing: Face<Out>,
    incoming: Face<In>,
    source: usize,
    nodes: usize,
}

impl<Out: Cursor, In: Cursor> WeakNeighbors<Out, In> {
    fn next(&mut self, control: &mut impl FnMut() -> Result<()>) -> Result<Option<(usize, u8)>> {
        loop {
            self.outgoing.fill(self.nodes, control)?;
            self.incoming.fill(self.nodes, control)?;
            let neighbor = match (self.outgoing.head, self.incoming.head) {
                (Some(out), Some(input)) => out.min(input),
                (Some(out), None) => out,
                (None, Some(input)) => input,
                (None, None) => return Ok(None),
            };
            let out = self.outgoing.head == Some(neighbor);
            let input = self.incoming.head == Some(neighbor);
            if out {
                self.outgoing.head = None;
            }
            if input {
                self.incoming.head = None;
            }
            if neighbor != self.source {
                return Ok(Some((neighbor, u8::from(out) + u8::from(input))));
            }
        }
    }

    fn scans(&self) -> Result<usize> {
        add(self.outgoing.seen, self.incoming.seen)
    }
}

fn neighbors<'a, Out: Rows, In: Rows>(
    outgoing: &'a Out,
    incoming: &'a In,
    source: usize,
) -> Result<WeakNeighbors<Out::Cursor<'a>, In::Cursor<'a>>> {
    Ok(WeakNeighbors {
        outgoing: Face::new(
            outgoing.open(source)?,
            outgoing
                .degree(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)?,
        ),
        incoming: Face::new(
            incoming.open(source)?,
            incoming
                .degree(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)?,
        ),
        source,
        nodes: outgoing.node_count(),
    })
}

struct Plan {
    costs: Vec<usize>,
    degrees: Vec<usize>,
    work: usize,
    base: usize,
    live: usize,
}

#[allow(clippy::too_many_arguments)]
fn prepare(
    outgoing: &impl Rows,
    incoming: &impl Rows,
    history: usize,
    mut retained: impl FnMut(usize) -> Result<usize>,
    initial_work: usize,
    work_limit: usize,
    base: usize,
    memory_limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<Plan> {
    control()?;
    let n = outgoing.node_count();
    if incoming.node_count() != n {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    admit(
        "kernel workspace bytes",
        add(base, workspace(n)?)?,
        memory_limit,
    )?;
    let mut work = initial_work;
    charge(
        &mut work,
        mul(n, add(64, mul(8, bits(history))?)?)?,
        work_limit,
    )?;
    let mut live = base;
    let mut costs = reserved(n, &mut live, memory_limit)?;
    let mut degrees = reserved(n, &mut live, memory_limit)?;
    for source in 0..n {
        control()?;
        let cost = row_work(n, history, retained(source)?)?;
        // One planning scan, one mark scan, and one outer scan. Each includes
        // BOTH faces, invisible history and excluded endpoint probes.
        charge(&mut work, mul(cost, 3)?, work_limit)?;
        costs.push(cost);
    }
    for source in 0..n {
        control()?;
        let mut row = neighbors(outgoing, incoming, source)?;
        let mut degree = 0;
        while let Some((target, _)) = row.next(control)? {
            degree = add(degree, 1)?;
            if lower(&costs, target, source) {
                // Exactly one later inner scan per weak edge, independent of
                // whether that edge has one or two directed incidences.
                charge(&mut work, costs[target], work_limit)?;
            }
        }
        degrees.push(degree);
    }
    control()?;
    Ok(Plan {
        costs,
        degrees,
        work,
        base,
        live,
    })
}

fn run(
    outgoing: &impl Rows,
    incoming: &impl Rows,
    plan: Plan,
    memory_limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    let n = outgoing.node_count();
    let Plan {
        costs,
        degrees,
        base,
        mut live,
        ..
    } = plan;
    if incoming.node_count() != n || costs.len() != n || degrees.len() != n {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    let mut closed = reserved(n, &mut live, memory_limit)?;
    let mut marks = reserved(n, &mut live, memory_limit)?;
    let mut multiplicities = reserved(n, &mut live, memory_limit)?;
    for _ in 0..n {
        control()?;
        closed.push(0usize);
        marks.push(usize::MAX);
        multiplicities.push(0u8);
    }
    let mut peak = live;
    let mut witness = witness();
    for source in 0..n {
        control()?;
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        {
            let mut row = neighbors(outgoing, incoming, source)?;
            let mut marked = 0;
            while let Some((target, multiplicity)) = row.next(control)? {
                if lower(&costs, target, source) {
                    marks[target] = source;
                    multiplicities[target] = multiplicity;
                    marked = add(marked, 1)?;
                }
            }
            witness.queue_peak = witness.queue_peak.max(marked);
            witness.edges_scanned = add(witness.edges_scanned, row.scans()?)?;
        }
        let mut row = neighbors(outgoing, incoming, source)?;
        while let Some((target, multiplicity)) = row.next(control)? {
            if !lower(&costs, target, source) {
                continue;
            }
            let mut other = neighbors(outgoing, incoming, target)?;
            while let Some((third, opposite)) = other.next(control)? {
                if lower(&costs, third, target) && marks[third] == source {
                    // Distinct vertices follow from total orientation. Add the
                    // opposite side's number of directed arcs to each center.
                    closed[source] = add(closed[source], usize::from(opposite))?;
                    closed[target] = add(closed[target], usize::from(multiplicities[third]))?;
                    closed[third] = add(closed[third], usize::from(multiplicity))?;
                }
            }
            witness.edges_scanned = add(witness.edges_scanned, other.scans()?)?;
        }
        witness.edges_scanned = add(witness.edges_scanned, row.scans()?)?;
    }
    drop(costs);
    drop(marks);
    drop(multiplicities);
    live = add(base, add(vector_bytes(&degrees)?, vector_bytes(&closed)?)?)?;
    let mut scores = reserved(n, &mut live, memory_limit)?;
    peak = peak.max(live);
    for (degree, count) in degrees.into_iter().zip(closed) {
        control()?;
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        let score = if degree < 2 {
            if count != 0 {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            0.0
        } else {
            let pairs = mul(degree, degree - 1)?;
            if count > pairs {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            // Identical conversion order to the decoded directed LCC kernel.
            count as f64 / pairs as f64
        };
        scores.push(score);
    }
    control()?;
    Ok((
        KernelOutput {
            values: KernelValues::Scores(scores),
            row_count: n,
            witness,
        },
        peak,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        GraphView, ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
        SealedProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
    };
    use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
    use std::cell::Cell;
    use std::convert::Infallible;
    use std::rc::Rc;

    struct Fixture {
        rows: Vec<Vec<usize>>,
        opens: Vec<Cell<usize>>,
        steps: Rc<Cell<usize>>,
        fail_at: usize,
        degrees: Option<Vec<usize>>,
    }

    impl Fixture {
        fn new(rows: Vec<Vec<usize>>) -> Self {
            let opens = (0..rows.len()).map(|_| Cell::new(0)).collect();
            Self {
                rows,
                opens,
                steps: Rc::new(Cell::new(0)),
                fail_at: usize::MAX,
                degrees: None,
            }
        }

        fn step(&self) -> Result<()> {
            let at = self.steps.get() + 1;
            self.steps.set(at);
            if at == self.fail_at {
                return Err(stopped());
            }
            Ok(())
        }
    }

    struct Row<'a> {
        fixture: &'a Fixture,
        row: std::slice::Iter<'a, usize>,
    }

    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.fixture.step()?;
            // Clustering depends only on topology, not even weight sign.
            Ok(self.row.next().map(|&target| (target, -7.0)))
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

        fn open(&self, source: usize) -> Result<Row<'_>> {
            self.step()?;
            let row = self
                .rows
                .get(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            self.opens[source].set(self.opens[source].get() + 1);
            Ok(Row {
                fixture: self,
                row: row.iter(),
            })
        }
    }

    fn stopped() -> super::super::Error {
        super::super::Error::Source(SealedProjectionError::UnknownOrdinal(usize::MAX))
    }

    fn faces(graph: &SnapshotGraphView) -> (Fixture, Fixture) {
        let outgoing = Fixture::new(
            (0..graph.node_count())
                .map(|source| graph.neighbors_indices(source).unwrap().to_vec())
                .collect(),
        );
        let mut incoming = Fixture::new(
            (0..graph.node_count())
                .map(|source| graph.in_neighbors_indices(source).unwrap().to_vec())
                .collect(),
        );
        incoming.steps = Rc::clone(&outgoing.steps);
        (outgoing, incoming)
    }

    fn retained(outgoing: &Fixture, incoming: &Fixture) -> Vec<usize> {
        outgoing
            .rows
            .iter()
            .zip(&incoming.rows)
            .map(|(a, b)| a.len() + b.len())
            .collect()
    }

    fn evaluate(
        outgoing: &Fixture,
        incoming: &Fixture,
        retained: &[usize],
        work_limit: usize,
        memory_limit: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<(KernelOutput, usize, usize)> {
        let history = retained.iter().try_fold(0, |sum, &value| add(sum, value))?;
        let plan = prepare(
            outgoing,
            incoming,
            history,
            |source| Ok(retained[source]),
            0,
            work_limit,
            0,
            memory_limit,
            control,
        )?;
        let work = plan.work;
        let (output, peak) = run(outgoing, incoming, plan, memory_limit, control)?;
        Ok((output, work, peak))
    }

    fn scores(output: KernelOutput) -> Vec<f64> {
        let KernelValues::Scores(scores) = output.values else {
            panic!("clustering produces scores")
        };
        scores
    }

    fn score_bits(values: &[f64]) -> Vec<u64> {
        values.iter().map(|value| value.to_bits()).collect()
    }

    fn projection(
        vertices: &[VId],
        edges: &[ProjectionEdge],
        direction: Directedness,
        reduction: ParallelEdgePolicy,
    ) -> SnapshotGraphView {
        SnapshotGraphView::build(
            SnapshotBinding {
                root: ObjectId([9; 32]),
                as_of: CommitSeq(3),
            },
            vertices,
            edges,
            ProjectionSpec {
                directedness: direction,
                parallel_edges: reduction,
                self_loops: SelfLoopPolicy::Keep,
            },
            ProjectionLimits {
                max_vertices: vertices.len(),
                max_input_edges: edges.len(),
                max_adjacency_entries: 2 * edges.len(),
                max_workspace_bytes: 1 << 20,
            },
        )
        .unwrap()
    }

    // Independent cubic definition: for each center, enumerate ordered pairs
    // of distinct weak neighbors and count the directed arc between each pair.
    fn ordered_pair_oracle(outgoing: &Fixture, incoming: &Fixture) -> Vec<f64> {
        (0..outgoing.rows.len())
            .map(|center| {
                let neighbors: Vec<_> = (0..outgoing.rows.len())
                    .filter(|&vertex| {
                        vertex != center
                            && (outgoing.rows[center].contains(&vertex)
                                || incoming.rows[center].contains(&vertex))
                    })
                    .collect();
                if neighbors.len() < 2 {
                    return 0.0;
                }
                let mut count = 0usize;
                for &left in &neighbors {
                    for &right in &neighbors {
                        if left != right && outgoing.rows[left].contains(&right) {
                            count += 1;
                        }
                    }
                }
                count as f64 / (neighbors.len() * (neighbors.len() - 1)) as f64
            })
            .collect()
    }

    #[test]
    fn exhaustive_directed_loops_reciprocals_and_reductions_match_both_oracles() {
        let vertices = [VId(0), VId(1 << 97), VId(1 << 121), VId(u128::MAX)];
        let mut cases = 0;
        for mask in 0usize..512 {
            let mut edges = Vec::new();
            for bit in 0..9 {
                if mask & (1 << bit) != 0 {
                    edges.push(ProjectionEdge {
                        eid: EId((2 * bit) as u128),
                        source: vertices[bit / 3],
                        target: vertices[bit % 3],
                        weight: -3.0,
                    });
                    edges.push(ProjectionEdge {
                        eid: EId((2 * bit + 1) as u128),
                        source: vertices[bit / 3],
                        target: vertices[bit % 3],
                        weight: 7.0,
                    });
                }
            }
            for direction in [Directedness::Directed, Directedness::Reversed] {
                for reduction in [
                    ParallelEdgePolicy::CollapseUnit,
                    ParallelEdgePolicy::Minimum,
                    ParallelEdgePolicy::Maximum,
                    ParallelEdgePolicy::Sum,
                ] {
                    let graph = projection(&vertices, &edges, direction, reduction);
                    let (outgoing, incoming) = faces(&graph);
                    let retained = retained(&outgoing, &incoming);
                    let (actual, _, peak) = evaluate(
                        &outgoing,
                        &incoming,
                        &retained,
                        usize::MAX,
                        workspace(vertices.len()).unwrap(),
                        &mut || Ok(()),
                    )
                    .unwrap();
                    let (expected, _) =
                        crate::directed_clustering::run(&graph, &mut || Ok::<(), Infallible>(()))
                            .unwrap();
                    let actual = scores(actual);
                    assert_eq!(
                        score_bits(&actual),
                        score_bits(&scores(expected)),
                        "mask={mask} direction={direction:?} reduction={reduction:?}"
                    );
                    assert_eq!(
                        score_bits(&actual),
                        score_bits(&ordered_pair_oracle(&outgoing, &incoming))
                    );
                    assert_eq!(actual[3].to_bits(), 0.0f64.to_bits(), "isolate preserved");
                    assert_eq!(peak, workspace(vertices.len()).unwrap());
                    cases += 1;
                }
            }
        }
        assert_eq!(cases, 4096);
    }

    #[test]
    fn retained_history_controls_orientation_and_exact_admission() {
        let outgoing = Fixture::new(vec![vec![1, 2], vec![0, 2], vec![0, 1]]);
        let incoming = Fixture::new(outgoing.rows.clone());
        let retained = [100_000, 4, 4];
        let plan = prepare(
            &outgoing,
            &incoming,
            retained.iter().sum(),
            |v| Ok(retained[v]),
            0,
            usize::MAX,
            0,
            workspace(3).unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
        assert!(plan.costs[0] > plan.costs[1]);
        assert!(lower(&plan.costs, 1, 0));
        let work = plan.work;
        let (output, peak) = run(
            &outgoing,
            &incoming,
            plan,
            workspace(3).unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(scores(output), vec![1.0; 3]);
        assert_eq!(
            outgoing.opens[0].get(),
            3,
            "expensive history must never be an inner row"
        );
        assert_eq!(incoming.opens[0].get(), 3);
        assert_eq!(peak, workspace(3).unwrap());
        assert!(evaluate(&outgoing, &incoming, &retained, work, peak, &mut || Ok(())).is_ok());
        assert!(
            evaluate(&outgoing, &incoming, &retained, work - 1, peak, &mut || Ok(
                ()
            ))
            .is_err()
        );
        assert!(
            evaluate(&outgoing, &incoming, &retained, work, peak - 1, &mut || Ok(
                ()
            ))
            .is_err()
        );
        let untouched = Fixture::new(outgoing.rows.clone());
        let other = Fixture::new(incoming.rows.clone());
        assert!(evaluate(&untouched, &other, &retained, 0, peak, &mut || Ok(())).is_err());
        assert_eq!(
            untouched.steps.get(),
            0,
            "zero work refuses before source access"
        );
        assert_eq!(other.steps.get(), 0);
        assert!(workspace(usize::MAX).is_err());
        assert!(row_work(3, usize::MAX, usize::MAX).is_err());
        assert!(reverse_work(3, usize::MAX).is_err());
    }

    #[test]
    fn every_source_and_scalar_refusal_stops_without_a_result_prefix() {
        let rows = vec![vec![0, 1, 2], vec![2], vec![0], vec![]];
        let transposed = vec![vec![0, 2], vec![0], vec![0, 1], vec![]];
        let outgoing = Fixture::new(rows.clone());
        let mut incoming = Fixture::new(transposed.clone());
        incoming.steps = Rc::clone(&outgoing.steps);
        let retained = retained(&outgoing, &incoming);
        let mut ticks = 0;
        let (expected, _, _) = evaluate(
            &outgoing,
            &incoming,
            &retained,
            usize::MAX,
            usize::MAX,
            &mut || {
                ticks += 1;
                Ok(())
            },
        )
        .unwrap();
        let source_steps = outgoing.steps.get();
        let expected = score_bits(&scores(expected));
        assert!(source_steps > 0 && ticks > 0);
        for stop in 1..=source_steps {
            let mut outgoing = Fixture::new(rows.clone());
            let mut incoming = Fixture::new(transposed.clone());
            incoming.steps = Rc::clone(&outgoing.steps);
            outgoing.fail_at = stop;
            incoming.fail_at = stop;
            assert!(
                evaluate(
                    &outgoing,
                    &incoming,
                    &retained,
                    usize::MAX,
                    usize::MAX,
                    &mut || Ok(())
                )
                .is_err()
            );
            assert_eq!(outgoing.steps.get(), stop);
        }
        for stop in 1..=ticks {
            let outgoing = Fixture::new(rows.clone());
            let incoming = Fixture::new(transposed.clone());
            let mut calls = 0;
            assert!(
                evaluate(
                    &outgoing,
                    &incoming,
                    &retained,
                    usize::MAX,
                    usize::MAX,
                    &mut || {
                        calls += 1;
                        if calls == stop {
                            Err(stopped())
                        } else {
                            Ok(())
                        }
                    }
                )
                .is_err()
            );
            assert_eq!(calls, stop);
        }
        let (actual, _, _) = evaluate(
            &outgoing,
            &incoming,
            &retained,
            usize::MAX,
            usize::MAX,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(score_bits(&scores(actual)), expected);
    }

    #[test]
    fn malformed_faces_refuse_while_empty_and_loop_only_graphs_remain_exact() {
        for (out, input) in [
            (vec![vec![2], vec![]], vec![vec![], vec![]]),
            (vec![vec![1, 1], vec![]], vec![vec![], vec![0]]),
            (
                vec![vec![2, 1], vec![], vec![]],
                vec![vec![], vec![0], vec![0]],
            ),
        ] {
            let outgoing = Fixture::new(out);
            let incoming = Fixture::new(input);
            let retained = retained(&outgoing, &incoming);
            assert!(
                evaluate(
                    &outgoing,
                    &incoming,
                    &retained,
                    usize::MAX,
                    usize::MAX,
                    &mut || Ok(())
                )
                .is_err()
            );
        }
        for declared in [0, 2] {
            let mut outgoing = Fixture::new(vec![vec![1], vec![]]);
            outgoing.degrees = Some(vec![declared, 0]);
            let incoming = Fixture::new(vec![vec![], vec![0]]);
            assert!(
                evaluate(
                    &outgoing,
                    &incoming,
                    &[1, 1],
                    usize::MAX,
                    usize::MAX,
                    &mut || Ok(())
                )
                .is_err()
            );
        }
        let empty = Fixture::new(vec![]);
        let (output, work, peak) = evaluate(&empty, &empty, &[], 0, 0, &mut || Ok(())).unwrap();
        assert!(scores(output).is_empty());
        assert_eq!((work, peak), (0, 0));
        let loops = Fixture::new(vec![vec![0]]);
        let (output, _, _) = evaluate(
            &loops,
            &loops,
            &[2],
            usize::MAX,
            workspace(1).unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(scores(output), vec![0.0]);
    }

    #[test]
    fn hub_counting_scans_are_linear_without_materializing_adjacency() {
        let leaves = 4096;
        let n = leaves + 2;
        let mut out = vec![Vec::new(); n];
        out[0].extend(1..=leaves);
        let mut input = vec![Vec::new(); n];
        for row in &mut input[1..=leaves] {
            row.push(0);
        }
        let outgoing = Fixture::new(out);
        let incoming = Fixture::new(input);
        let retained = retained(&outgoing, &incoming);
        let (output, _, peak) = evaluate(
            &outgoing,
            &incoming,
            &retained,
            usize::MAX,
            workspace(n).unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(output.witness.edges_scanned, 5 * leaves);
        assert_eq!(outgoing.opens[0].get(), 3);
        assert_eq!(incoming.opens[0].get(), 3);
        assert_eq!(peak, n * (4 * size_of::<usize>() + size_of::<u8>()));
        assert_eq!(scores(output), vec![0.0; n]);
    }

    #[test]
    fn official_graphalytics_directed_example_matches_published_lcc() {
        // Published topology and expected values, pinned to:
        // https://github.com/ldbc/ldbc_graphalytics/tree/7b8bde76cf7aab5e90b25ecd4b38829e2f98b292
        // graphalytics-validation/src/main/resources/validation-graphs/example/
        // example-directed.e: 5689a64fb63bfb8fdcf9d32d6fb7b902acecaa13
        // example-directed.v: f00c965d8307308469e537302baa73048488f162
        // example-directed-LCC: 2be7e3d81e5e0c02f53af00c74043c99126856fa
        // Weights are omitted because directed LCC is defined over topology.
        let vertices: Vec<_> = (1..=10).map(VId).collect();
        let pairs = [
            (1, 3),
            (1, 5),
            (2, 4),
            (2, 5),
            (2, 10),
            (3, 1),
            (3, 5),
            (3, 8),
            (3, 10),
            (5, 3),
            (5, 4),
            (5, 8),
            (6, 3),
            (6, 4),
            (7, 4),
            (8, 1),
            (9, 4),
        ];
        let edges: Vec<_> = pairs
            .iter()
            .enumerate()
            .map(|(eid, &(source, target))| ProjectionEdge {
                eid: EId(eid as u128),
                source: VId(source),
                target: VId(target),
                weight: 1.0,
            })
            .collect();
        let graph = projection(
            &vertices,
            &edges,
            Directedness::Directed,
            ParallelEdgePolicy::Reject,
        );
        let expected = [
            0.666_666_666_666_666_6,
            0.166_666_666_666_666_7,
            0.15,
            0.05,
            0.25,
            0.0,
            0.0,
            0.833_333_333_333_333_4,
            0.0,
            0.0,
        ];
        let (outgoing, incoming) = faces(&graph);
        let retained = retained(&outgoing, &incoming);
        let (actual, _, _) = evaluate(
            &outgoing,
            &incoming,
            &retained,
            usize::MAX,
            workspace(10).unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
        let actual = scores(actual);
        for (index, (&actual, &expected)) in actual.iter().zip(&expected).enumerate() {
            assert!(
                (actual - expected).abs() <= 1e-4,
                "vertex {}: {actual} != {expected}",
                index + 1
            );
        }
        let (decoded, _) =
            crate::directed_clustering::run(&graph, &mut || Ok::<(), Infallible>(())).unwrap();
        assert_eq!(score_bits(&actual), score_bits(&scores(decoded)));
    }
}
