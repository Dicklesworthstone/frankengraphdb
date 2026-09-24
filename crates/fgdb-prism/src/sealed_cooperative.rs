//! Cooperative BFS and PageRank over the SAME admitted compressed projection.
//!
//! One fuel counter covers raw incidence decoding (including invisible history,
//! excluded endpoints and parallel reductions), scalar passes and result rows.
//! Exhaustion awaits the host's scheduler yield before replenishing that counter.
//! No task/thread is spawned, no row is reopened to resume it, and partial state
//! stays owned by the future. Dropping the future drops all of that state.
//!
//! The quantum bounds counted steps, not instructions or elapsed time. Individual
//! allocations, compressed lookups, bounded alias strings and the fixed kernel
//! source transcript remain indivisible. Sealing/projection preparation and the
//! other procedures are NOT made cooperative by this module.

use super::{
    ComplexityWitness, EncodedRows, Error, ExecutionError, FnxAlgorithm, FnxCallSpec,
    FnxExecutionLimits, FnxMemoryLimits, FnxResult, KernelOutput, KernelValues, PageRankOptions,
    QueryCx, Result, ResultAdmission, SealedGraphView, SealedNeighborCursor, add, admit,
    checkpoint, directional_pass_work, finish_encoded, mul, reserve,
};
use crate::SealedProjectionError;
use crate::sealed_control::Control;
use fgdb_strata::tiered::sealed::{SealedScanBudget, SealedScanStep};
use std::cell::RefCell;
use std::future::Future;
use std::mem::size_of;
use std::num::NonZeroUsize;

struct Cooperate<'cx, Yield> {
    cx: &'cx Control<'cx>,
    fuel: SealedScanBudget,
    quantum: NonZeroUsize,
    yield_now: Yield,
}

impl<Yield, YieldFuture> Cooperate<'_, Yield>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    async fn pause(&mut self) -> Result<()> {
        checkpoint(self.cx)?;
        (self.yield_now)().await;
        // The query can be cancelled or its live permission can expire while
        // the future is suspended. Neither the runtime nor guard is replaced.
        // Check BEFORE making another unit of fuel available to source work.
        checkpoint(self.cx)?;
        self.fuel = SealedScanBudget::new(self.quantum.get());
        Ok(())
    }

    async fn tick(&mut self) -> Result<()> {
        checkpoint(self.cx)?;
        if !self.fuel.spend() {
            self.pause().await?;
            debug_assert!(self.fuel.remaining() > 0);
            self.fuel.spend();
        }
        Ok(())
    }

    async fn next(&mut self, row: &mut SealedNeighborCursor<'_>) -> Result<Option<(usize, f64)>> {
        loop {
            match row.next_budgeted_controlled(self.cx, &mut self.fuel)? {
                SealedScanStep::Item(value) => return Ok(Some(value)),
                SealedScanStep::End => return Ok(None),
                SealedScanStep::Yield => self.pause().await?,
            }
        }
    }
}

impl FnxCallSpec {
    /// Whether this registered call has an explicitly cooperative compressed
    /// kernel. An unsupported request must not silently run synchronously.
    pub fn supports_cooperative_sealed_execution(&self) -> bool {
        matches!(
            self.algorithm(),
            FnxAlgorithm::PageRank(_) | FnxAlgorithm::SingleSourceShortestPathLength { .. }
        )
    }

    /// Execute an already admitted projection with a shared scheduling quantum.
    /// The trusted host supplies its runtime yield primitive (for example the
    /// pinned asupersync `runtime::yield_now::yield_now` function). Returning an
    /// immediately ready future is legal, but opts OUT of scheduler fairness.
    /// This callback grants no storage, authorization, task or clock authority.
    ///
    /// The quantum does not alter scalar evaluation order, rows, result digests
    /// or certificates. It is scheduling fuel, separate from execution admission.
    /// Certificates identify the cooperative kernel rather than its synchronous
    /// counterpart. No result prefix escapes on cancellation, failure or drop.
    /// Allocation and fixed/lookup costs are not a hard wall-clock bound.
    pub async fn execute_sealed_cooperative<Yield, YieldFuture>(
        &self,
        cx: &QueryCx,
        graph: &SealedGraphView,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
        quantum: NonZeroUsize,
        yield_now: Yield,
    ) -> Result<FnxResult>
    where
        Yield: FnMut() -> YieldFuture,
        YieldFuture: Future<Output = ()>,
    {
        self.execute_sealed_cooperative_with_checkpoint(
            cx,
            graph,
            limits,
            memory,
            quantum,
            yield_now,
            || Ok(()),
        )
        .await
    }

    /// Cooperatively execute under an additional live guard. The guard spans
    /// raw history, scalar work and result encoding, and is checked before AND
    /// after each suspension. No RefCell borrow or ambient restriction guard
    /// crosses an await. QueryCx cancellation always precedes caller policy.
    ///
    /// The guard is execution-local: neither it nor a permit enters the graph.
    /// A typed Guard cause is retained; no result prefix escapes on refusal.
    /// This is not a capability verifier. A trusted host must admit and keep
    /// private the frozen graph and retain its live allowance for this call.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_sealed_cooperative_with_checkpoint<Yield, YieldFuture>(
        &self,
        cx: &QueryCx,
        graph: &SealedGraphView,
        limits: FnxExecutionLimits,
        memory: FnxMemoryLimits,
        quantum: NonZeroUsize,
        yield_now: Yield,
        guard: impl FnMut() -> std::result::Result<(), SealedProjectionError>,
    ) -> Result<FnxResult>
    where
        Yield: FnMut() -> YieldFuture,
        YieldFuture: Future<Output = ()>,
    {
        // Install the existing role restriction for EACH poll, never across an
        // await via a synchronous guard or by recovering a wider ambient Cx.
        cx.with_restriction_async(async {
            let guard = RefCell::new(guard);
            let invoke = || (guard.borrow_mut())();
            let execution = Control::new(cx, &invoke);
            let cx = &execution;
            checkpoint(cx)?;
            if !self.supports_cooperative_sealed_execution() {
                return Err(Error::UnsupportedCooperativeAlgorithm(self.algorithm()));
            }
            self.validate_sealed_projection(graph.spec().directedness)?;
            let n = graph.node_count();
            let admission = ResultAdmission::new(self, limits, memory)?;
            let pass =
                directional_pass_work(n, graph.scan_incidence_bound(), graph.spec().directedness)?;
            let (kernel, estimated_work, workspace, source) = match self.algorithm() {
                FnxAlgorithm::PageRank(options) => {
                    admit("iterations", options.max_iter(), limits.max_iterations)?;
                    admission.rows(n)?;
                    (
                        "fgdb-prism/sealed-pagerank-cooperative-v1",
                        mul(pass, add(options.max_iter(), 1)?)?,
                        mul(n, 3 * size_of::<f64>())?,
                        None,
                    )
                }
                FnxAlgorithm::SingleSourceShortestPathLength { source, .. } => {
                    let ordinal = graph
                        .vertex_ordinal(source)
                        .ok_or(ExecutionError::UnknownSource(source))?;
                    admission.rows(1)?;
                    (
                        "fgdb-prism/sealed-bfs-cooperative-v1",
                        pass,
                        mul(n, size_of::<usize>() + size_of::<Option<usize>>())?,
                        Some(ordinal),
                    )
                }
                other => return Err(Error::UnsupportedCooperativeAlgorithm(other)),
            };
            admit("estimated work", estimated_work, limits.max_estimated_work)?;
            admit(
                "kernel workspace bytes",
                workspace,
                memory.max_kernel_workspace_bytes,
            )?;
            let mut control = Cooperate {
                cx,
                fuel: SealedScanBudget::new(quantum.get()),
                quantum,
                yield_now,
            };
            let output = match self.algorithm() {
                FnxAlgorithm::PageRank(options) => pagerank(graph, options, &mut control).await?,
                FnxAlgorithm::SingleSourceShortestPathLength { cutoff, .. } => {
                    bfs(
                        graph,
                        source.ok_or(ExecutionError::InvalidUpstreamResult)?,
                        cutoff,
                        &admission,
                        &mut control,
                    )
                    .await?
                }
                other => return Err(Error::UnsupportedCooperativeAlgorithm(other)),
            };
            control.tick().await?;
            let mut encoded = EncodedRows::new(self, output.row_count, &admission)?;
            for index in 0..n {
                control.tick().await?;
                encoded.push(self, graph, &output.values, index, &mut || checkpoint(cx))?;
            }
            control.tick().await?;
            finish_encoded(
                self,
                graph,
                encoded,
                output.witness,
                kernel,
                estimated_work,
                workspace,
                &mut || checkpoint(cx),
            )
        })
        .await
    }
}

async fn bfs<Yield, YieldFuture>(
    graph: &SealedGraphView,
    source: usize,
    cutoff: Option<usize>,
    admission: &ResultAdmission,
    control: &mut Cooperate<'_, Yield>,
) -> Result<KernelOutput>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let n = graph.node_count();
    let mut distances = reserve(n)?;
    let mut queue = reserve(n)?;
    for _ in 0..n {
        control.tick().await?;
        distances.push(None);
    }
    *distances
        .get_mut(source)
        .ok_or(ExecutionError::InvalidUpstreamResult)? = Some(0usize);
    queue.push(source);
    let mut head = 0usize;
    let mut witness = ComplexityWitness {
        algorithm: "single_source_shortest_path_length_bfs".to_owned(),
        complexity_claim: "O(|V| log(1+H) + H log(1+|V|)) compressed row visits".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 1,
    };
    while head < queue.len() {
        control.tick().await?;
        let vertex = queue[head];
        head += 1;
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        let depth = distances[vertex].ok_or(ExecutionError::InvalidUpstreamResult)?;
        if cutoff.is_some_and(|limit| depth >= limit) {
            continue;
        }
        let next_depth = add(depth, 1)?;
        let mut row = graph.neighbor_cursor_controlled(control.cx, vertex, None)?;
        while let Some((neighbor, _)) = control.next(&mut row).await? {
            control.tick().await?;
            witness.edges_scanned = add(witness.edges_scanned, 1)?;
            let distance = distances
                .get_mut(neighbor)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            if distance.is_none() {
                admission.rows(add(queue.len(), 1)?)?;
                *distance = Some(next_depth);
                queue.push(neighbor);
                witness.queue_peak = witness.queue_peak.max(queue.len() - head);
            }
        }
    }
    Ok(KernelOutput {
        values: KernelValues::Distances(distances),
        row_count: queue.len(),
        witness,
    })
}

async fn pagerank<Yield, YieldFuture>(
    graph: &SealedGraphView,
    options: PageRankOptions,
    control: &mut Cooperate<'_, Yield>,
) -> Result<KernelOutput>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let n = graph.node_count();
    let mut witness = ComplexityWitness {
        algorithm: "pagerank_power_iteration".to_owned(),
        complexity_claim: "O(k * (|V| log(1+H) + H log(1+|V|))) compressed row visits".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    if n == 0 {
        return Ok(KernelOutput {
            values: KernelValues::Scores(Vec::new()),
            row_count: 0,
            witness,
        });
    }
    let mut sums = reserve(n)?;
    let mut ranks = reserve(n)?;
    let mut next = reserve(n)?;
    let population = n as f64;
    for source in 0..n {
        control.tick().await?;
        let mut sum = 0.0;
        if options.weighted() {
            let mut row = graph.neighbor_cursor_controlled(control.cx, source, None)?;
            while let Some((_, weight)) = control.next(&mut row).await? {
                control.tick().await?;
                if weight < 0.0 {
                    return Err(ExecutionError::NegativeWeight.into());
                }
                sum += weight;
                if !sum.is_finite() {
                    return Err(ExecutionError::NonFiniteWeightSum.into());
                }
            }
        } else {
            sum = graph
                .degree(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)? as f64;
        }
        sums.push(sum);
        ranks.push(1.0 / population);
        next.push(0.0);
    }
    let base = (1.0 - options.alpha()) / population;
    for _ in 0..options.max_iter() {
        control.tick().await?;
        let mut dangling_mass = 0.0;
        for source in 0..n {
            control.tick().await?;
            if sums[source] == 0.0 {
                dangling_mass += ranks[source];
            }
        }
        let initial = base + options.alpha() * dangling_mass / population;
        for value in &mut next {
            control.tick().await?;
            *value = initial;
        }
        for source in 0..n {
            control.tick().await?;
            let push = options.alpha() * ranks[source];
            let sum = sums[source];
            let mut row = graph.neighbor_cursor_controlled(control.cx, source, None)?;
            while let Some((target, weight)) = control.next(&mut row).await? {
                control.tick().await?;
                // Preserve the synchronous/fnx scalar order across EVERY pause:
                // divide before multiply, ascending targets and ordered sums.
                let share = if options.weighted() {
                    if sum > 0.0 { weight / sum } else { weight }
                } else if sum > 0.0 {
                    1.0 / sum
                } else {
                    0.0
                };
                let value = next
                    .get_mut(target)
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                *value += push * share;
                witness.edges_scanned = add(witness.edges_scanned, 1)?;
            }
        }
        let mut delta = 0.0;
        for index in 0..n {
            control.tick().await?;
            if !next[index].is_finite() || next[index] < 0.0 {
                return Err(ExecutionError::InvalidNumericResult.into());
            }
            delta += (next[index] - ranks[index]).abs();
        }
        std::mem::swap(&mut ranks, &mut next);
        witness.nodes_touched = add(witness.nodes_touched, n)?;
        if delta < population * options.tolerance() {
            return Ok(KernelOutput {
                values: KernelValues::Scores(ranks),
                row_count: n,
                witness,
            });
        }
    }
    Err(ExecutionError::NotConverged {
        max_iterations: options.max_iter(),
        witness,
    }
    .into())
}
