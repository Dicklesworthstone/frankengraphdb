//! Exact core numbers by stable bucket peeling over compressed adjacency.
//!
//! The immutable degree directory initializes four O(|V|) integer vectors.
//! Each reduced adjacency row is opened once, in peeling order, and every
//! neighbor update takes bounded scalar work. Neither driver materializes a
//! row or another graph. One shared state owns all bucket mutations; the two
//! drivers differ only in their source pulls and scheduling checkpoints.

use super::{
    ComplexityWitness, Cursor, Delivery, ExecutionError, Finished, FnxCallSpec, FnxExecutionLimits,
    FnxMemoryLimits, KernelOutput, KernelValues, Result, ResultAdmission, Rows, SealedGraphView,
    SealedRows, add, admit, checkpoint, directional_pass_work, finish, finish_encoded, mul,
    require_loop_free_degree, reserve,
};
use crate::sealed_control::Control;
use std::future::Future;
use std::mem::size_of;

fn workspace(n: usize) -> Result<usize> {
    let peeling = mul(n, 4 * size_of::<usize>())?;
    let conversion = mul(n, size_of::<usize>() + size_of::<u64>())?;
    Ok(peeling.max(conversion))
}

fn work(n: usize, arcs: usize, pass: usize, columns: usize) -> Result<usize> {
    // Five linear preparation phases, one peel and one conversion per vertex,
    // output fields, and one bounded bucket update per reduced arc. `pass`
    // accounts for BOTH compressed faces and all excluded/retained history.
    add(
        pass,
        add(arcs, add(mul(n, add(16, columns)?)?, add(32, columns)?)?)?,
    )
}

fn vector_bytes<T>(values: &Vec<T>) -> Result<usize> {
    mul(values.capacity(), size_of::<T>())
}

fn reserved<T>(length: usize, live: &mut usize, limit: usize) -> Result<Vec<T>> {
    admit(
        "kernel workspace bytes",
        add(*live, mul(length, size_of::<T>())?)?,
        limit,
    )?;
    let values = reserve(length)?;
    let actual = add(*live, vector_bytes(&values)?)?;
    // try_reserve_exact can still return a larger capacity. Check that actual
    // backing before initialization or another allocation, and drop on refusal.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

#[derive(Clone, Copy)]
enum Preparation {
    Initialize(usize),
    Count(usize),
    Prefix { at: usize, start: usize },
    Scatter(usize),
    Restore(usize),
    Ready,
}

struct Peeling {
    degrees: Vec<usize>,
    bins: Vec<usize>,
    vertices: Vec<usize>,
    positions: Vec<usize>,
    preparation: Preparation,
    nodes: usize,
    arcs: usize,
    peak: usize,
}

impl Peeling {
    fn new(nodes: usize, limit: usize) -> Result<Self> {
        admit("kernel workspace bytes", workspace(nodes)?, limit)?;
        let mut live = 0;
        let degrees = reserved(nodes, &mut live, limit)?;
        let bins = reserved(nodes, &mut live, limit)?;
        let vertices = reserved(nodes, &mut live, limit)?;
        let positions = reserved(nodes, &mut live, limit)?;
        Ok(Self {
            degrees,
            bins,
            vertices,
            positions,
            preparation: Preparation::Initialize(0),
            nodes,
            arcs: 0,
            peak: live,
        })
    }

    fn prepared(&self) -> bool {
        matches!(self.preparation, Preparation::Ready)
    }

    // Every call performs at most one vertex/bucket operation, including on
    // an edgeless population. No uninterruptible fill/sort hides in preparation.
    fn prepare_step(&mut self, degree: impl FnOnce(usize) -> Option<usize>) -> Result<()> {
        self.preparation = match self.preparation {
            Preparation::Initialize(at) if at < self.nodes => {
                let count = degree(at).ok_or(ExecutionError::InvalidUpstreamResult)?;
                if count >= self.nodes {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                self.degrees.push(count);
                self.bins.push(0);
                self.vertices.push(0);
                self.positions.push(0);
                Preparation::Initialize(at + 1)
            }
            Preparation::Initialize(_) => Preparation::Count(0),
            Preparation::Count(at) if at < self.nodes => {
                self.bins[self.degrees[at]] += 1;
                Preparation::Count(at + 1)
            }
            Preparation::Count(_) => Preparation::Prefix { at: 0, start: 0 },
            Preparation::Prefix { at, start } if at < self.nodes => {
                let next = add(start, self.bins[at])?;
                self.bins[at] = start;
                Preparation::Prefix {
                    at: at + 1,
                    start: next,
                }
            }
            Preparation::Prefix { .. } => Preparation::Scatter(0),
            Preparation::Scatter(at) if at < self.nodes => {
                let count = self.degrees[at];
                let slot = self.bins[count];
                self.positions[at] = slot;
                self.vertices[slot] = at;
                self.bins[count] += 1;
                Preparation::Scatter(at + 1)
            }
            Preparation::Scatter(_) => Preparation::Restore(self.nodes),
            Preparation::Restore(at) if at > 1 => {
                self.bins[at - 1] = self.bins[at - 2];
                Preparation::Restore(at - 1)
            }
            Preparation::Restore(_) => {
                if let Some(first) = self.bins.first_mut() {
                    *first = 0;
                }
                Preparation::Ready
            }
            Preparation::Ready => Preparation::Ready,
        };
        Ok(())
    }

    fn vertex(&self, slot: usize) -> Result<usize> {
        self.vertices
            .get(slot)
            .copied()
            .ok_or_else(|| ExecutionError::InvalidUpstreamResult.into())
    }

    fn visit(&mut self, vertex: usize, neighbor: usize) -> Result<()> {
        if vertex >= self.nodes || neighbor >= self.nodes {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        if vertex == neighbor {
            return Err(ExecutionError::SelfLoopRefused.into());
        }
        self.arcs = add(self.arcs, 1)?;
        if self.degrees[neighbor] > self.degrees[vertex] {
            let count = self.degrees[neighbor];
            let from = self.positions[neighbor];
            let to = self.bins[count];
            let first = self.vertices[to];
            if neighbor != first {
                self.vertices[from] = first;
                self.vertices[to] = neighbor;
                self.positions[neighbor] = to;
                self.positions[first] = from;
            }
            self.bins[count] += 1;
            self.degrees[neighbor] -= 1;
        }
        Ok(())
    }

    fn into_conversion(self, limit: usize) -> Result<Conversion> {
        let Self {
            degrees,
            bins,
            vertices,
            positions,
            nodes,
            arcs,
            peak,
            ..
        } = self;
        // A final degree is its core number. Release all bucket indexes BEFORE
        // reserving the u64 result so the conversion peak is only two vectors.
        drop(bins);
        drop(vertices);
        drop(positions);
        let mut live = vector_bytes(&degrees)?;
        let counts = reserved(nodes, &mut live, limit)?;
        Ok(Conversion {
            degrees,
            counts,
            nodes,
            arcs,
            peak: peak.max(live),
        })
    }
}

struct Conversion {
    degrees: Vec<usize>,
    counts: Vec<u64>,
    nodes: usize,
    arcs: usize,
    peak: usize,
}

impl Conversion {
    fn push(&mut self, vertex: usize) -> Result<()> {
        self.counts
            .push(u64::try_from(self.degrees[vertex]).map_err(|_| ExecutionError::SizeOverflow)?);
        Ok(())
    }

    fn finish(self) -> (KernelOutput, usize) {
        (
            KernelOutput {
                values: KernelValues::Counts(self.counts),
                row_count: self.nodes,
                witness: ComplexityWitness {
                    algorithm: "core_number_bucket_peeling_cursor".to_owned(),
                    complexity_claim: "O(|V| + |E|) bucket peeling plus O(|V| log(1+H) + H log(1+|V|)) compressed row visits".to_owned(),
                    nodes_touched: self.nodes,
                    edges_scanned: self.arcs,
                    queue_peak: 0,
                },
            },
            self.peak,
        )
    }
}

fn run(
    graph: &impl Rows,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, usize)> {
    control()?;
    let mut state = Peeling::new(graph.node_count(), limit)?;
    while !state.prepared() {
        control()?;
        state.prepare_step(|vertex| graph.degree(vertex))?;
    }
    for slot in 0..graph.node_count() {
        control()?;
        let vertex = state.vertex(slot)?;
        let mut row = graph.open(vertex)?;
        while let Some((neighbor, _)) = row.next()? {
            control()?;
            state.visit(vertex, neighbor)?;
        }
    }
    control()?;
    let mut conversion = state.into_conversion(limit)?;
    for vertex in 0..graph.node_count() {
        control()?;
        conversion.push(vertex)?;
    }
    control()?;
    Ok(conversion.finish())
}

fn prepare(
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
        "fgdb-prism/sealed-core-bucket-v1",
        estimated,
        peak,
        admission,
        &mut control,
    )
}

pub(super) async fn execute_cooperative<Yield, YieldFuture>(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    delivery: Delivery,
    control: &mut super::cooperative::Cooperate<'_, Yield>,
) -> Result<Finished>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let admission = ResultAdmission::for_delivery(call, limits, memory, delivery)?;
    let estimated = prepare(call, graph, limits, memory, &admission)?;
    control.tick().await?;
    let n = graph.node_count();
    let mut state = Peeling::new(n, memory.max_kernel_workspace_bytes)?;
    while !state.prepared() {
        control.tick().await?;
        state.prepare_step(|vertex| graph.degree(vertex))?;
    }
    for slot in 0..n {
        control.tick().await?;
        let vertex = state.vertex(slot)?;
        let mut row = graph.neighbor_cursor_controlled(control.cx, vertex, None)?;
        while let Some((neighbor, _)) = control.next(&mut row).await? {
            control.tick().await?;
            state.visit(vertex, neighbor)?;
        }
    }
    control.tick().await?;
    let mut conversion = state.into_conversion(memory.max_kernel_workspace_bytes)?;
    for vertex in 0..n {
        control.tick().await?;
        conversion.push(vertex)?;
    }
    control.tick().await?;
    let (output, peak) = conversion.finish();
    let mut encoded = super::EncodedRows::new(call, n, &admission)?;
    for vertex in 0..n {
        control.tick().await?;
        encoded.push(call, graph, &output.values, vertex, &mut || {
            checkpoint(control.cx)
        })?;
    }
    control.tick().await?;
    finish_encoded(
        call,
        graph,
        encoded,
        output.values,
        output.witness,
        "fgdb-prism/sealed-core-bucket-cooperative-v1",
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SealedProjectionError;
    use std::cell::Cell;

    struct Fixture {
        rows: Vec<Vec<usize>>,
        opens: Vec<Cell<usize>>,
        source_steps: Cell<usize>,
        fail_at: usize,
    }

    impl Fixture {
        fn new(rows: Vec<Vec<usize>>) -> Self {
            Self {
                opens: (0..rows.len()).map(|_| Cell::new(0)).collect(),
                rows,
                source_steps: Cell::new(0),
                fail_at: usize::MAX,
            }
        }

        fn step(&self) -> Result<()> {
            let next = self.source_steps.get() + 1;
            self.source_steps.set(next);
            if next == self.fail_at {
                return Err(super::super::Error::Source(
                    SealedProjectionError::UnknownOrdinal(usize::MAX),
                ));
            }
            Ok(())
        }
    }

    struct Row<'a> {
        graph: &'a Fixture,
        targets: std::slice::Iter<'a, usize>,
    }

    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.graph.step()?;
            // Core numbers ignore finite signed weights.
            Ok(self.targets.next().map(|&target| (target, -7.0)))
        }
    }

    impl Rows for Fixture {
        type Cursor<'a> = Row<'a>;

        fn node_count(&self) -> usize {
            self.rows.len()
        }

        fn degree(&self, source: usize) -> Option<usize> {
            self.rows.get(source).map(Vec::len)
        }

        fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
            self.step()?;
            self.opens[source].set(self.opens[source].get() + 1);
            Ok(Row {
                graph: self,
                targets: self.rows[source].iter(),
            })
        }
    }

    // Independent threshold elimination, with no degree buckets or shared
    // peeling order. Membership in each maximal k-core defines the answer.
    fn elimination(rows: &[Vec<usize>]) -> Vec<u64> {
        let n = rows.len();
        let mut cores = vec![0; n];
        for k in 1..n {
            let mut remaining = vec![true; n];
            loop {
                let remove: Vec<_> = (0..n)
                    .filter(|&v| {
                        remaining[v] && rows[v].iter().filter(|&&u| remaining[u]).count() < k
                    })
                    .collect();
                if remove.is_empty() {
                    break;
                }
                for vertex in remove {
                    remaining[vertex] = false;
                }
            }
            for vertex in 0..n {
                if remaining[vertex] {
                    cores[vertex] = k as u64;
                }
            }
        }
        cores
    }

    #[test]
    fn every_small_core_matches_pinned_fnx_and_independent_elimination_in_one_row_pass() {
        let mut cases = 0;
        for n in 0..=5 {
            let pairs: Vec<_> = (0..n)
                .flat_map(|s| ((s + 1)..n).map(move |t| (s, t)))
                .collect();
            for mask in 0usize..(1 << pairs.len()) {
                let mut rows = vec![Vec::new(); n];
                let mut oracle = fnx_classes::Graph::strict();
                for v in 0..n {
                    oracle.add_node(format!("n{v}"));
                }
                for (bit, &(s, t)) in pairs.iter().enumerate() {
                    if mask & (1 << bit) != 0 {
                        rows[s].push(t);
                        rows[t].push(s);
                        oracle.add_edge(format!("n{s}"), format!("n{t}")).unwrap();
                    }
                }
                for row in &mut rows {
                    row.sort_unstable();
                }
                let independent = elimination(&rows);
                let mut expected = vec![0; n];
                for entry in fnx_algorithms::core_number(&oracle).core_numbers {
                    let vertex = entry
                        .node
                        .strip_prefix('n')
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    expected[vertex] = entry.core as u64;
                }
                assert_eq!(expected, independent);
                let graph = Fixture::new(rows);
                let mut ticks = 0;
                let (output, peak) = run(&graph, workspace(n).unwrap(), &mut || {
                    ticks += 1;
                    Ok(())
                })
                .unwrap();
                let KernelValues::Counts(actual) = output.values else {
                    panic!("core counts")
                };
                assert_eq!(actual, expected, "n={n}, mask={mask}");
                assert_eq!(output.row_count, n);
                assert_eq!(output.witness.nodes_touched, n);
                let arcs = graph.rows.iter().map(Vec::len).sum::<usize>();
                assert_eq!(output.witness.edges_scanned, arcs);
                assert_eq!(output.witness.queue_peak, 0);
                assert!(graph.opens.iter().all(|opens| opens.get() == 1));
                assert_eq!(peak, workspace(n).unwrap());
                assert!(ticks <= work(n, arcs, 0, 2).unwrap());
                cases += 1;
            }
        }
        assert_eq!(cases, 1100);
    }

    #[test]
    fn every_core_scalar_and_source_refusal_stops_without_publishing_counts() {
        let rows = vec![
            vec![1, 2, 3],
            vec![0, 2],
            vec![0, 1, 3],
            vec![0, 2, 4],
            vec![3],
            vec![],
        ];
        let graph = Fixture::new(rows.clone());
        let mut total = 0;
        run(&graph, usize::MAX, &mut || {
            total += 1;
            Ok(())
        })
        .unwrap();
        let source_total = graph.source_steps.get();
        for at in 1..=total {
            let graph = Fixture::new(rows.clone());
            let mut seen = 0;
            let error = run(&graph, usize::MAX, &mut || {
                seen += 1;
                if seen == at {
                    Err(super::super::Error::Cancelled(
                        std::io::Error::other("core stop").into(),
                    ))
                } else {
                    Ok(())
                }
            })
            .err()
            .expect("scalar refusal");
            assert!(matches!(error, super::super::Error::Cancelled(_)));
            assert_eq!(seen, at);
        }
        for at in 1..=source_total {
            let mut graph = Fixture::new(rows.clone());
            graph.fail_at = at;
            let error = run(&graph, usize::MAX, &mut || Ok(()))
                .err()
                .expect("source refusal");
            assert!(matches!(
                error,
                super::super::Error::Source(SealedProjectionError::UnknownOrdinal(usize::MAX))
            ));
            assert_eq!(graph.source_steps.get(), at);
        }
    }

    #[test]
    fn core_quota_degree_and_target_refusals_precede_any_success() {
        let graph = Fixture::new(vec![vec![1], vec![0], vec![]]);
        let exact = workspace(graph.node_count()).unwrap();
        assert!(matches!(
            run(&graph, exact - 1, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::LimitExceeded {
                    resource: "kernel workspace bytes",
                    ..
                }
            ))
        ));
        assert_eq!(graph.source_steps.get(), 0);
        run(&graph, exact, &mut || Ok(())).unwrap();
        assert!(workspace(usize::MAX).is_err());
        assert!(work(usize::MAX, 0, 0, 2).is_err());
        assert!(work(1, usize::MAX, 1, 2).is_err());
        for rows in [vec![vec![1]], vec![vec![2], vec![]]] {
            assert!(matches!(
                run(&Fixture::new(rows), usize::MAX, &mut || Ok(())),
                Err(super::super::Error::Execution(
                    ExecutionError::InvalidUpstreamResult
                ))
            ));
        }
        assert!(matches!(
            run(
                &Fixture::new(vec![vec![0], vec![]]),
                usize::MAX,
                &mut || Ok(())
            ),
            Err(super::super::Error::Execution(
                ExecutionError::SelfLoopRefused
            ))
        ));
    }

    #[test]
    fn core_large_hub_and_isolates_keep_linear_storage_and_one_cursor_per_vertex() {
        let n = 8193;
        let mut rows = vec![Vec::new(); n + 3];
        for v in 1..n {
            rows[0].push(v);
            rows[v].push(0);
        }
        let graph = Fixture::new(rows);
        let (output, peak) = run(&graph, workspace(n + 3).unwrap(), &mut || Ok(())).unwrap();
        let KernelValues::Counts(counts) = output.values else {
            panic!("core counts")
        };
        assert_eq!(&counts[..n], vec![1; n]);
        assert_eq!(&counts[n..], &[0, 0, 0]);
        assert_eq!(peak, workspace(n + 3).unwrap());
        assert_eq!(output.witness.edges_scanned, 2 * (n - 1));
        assert!(graph.opens.iter().all(|opens| opens.get() == 1));
    }
}
