//! Cooperative ranks, weighted/hop distances and connectivity on one projection.
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
use crate::{DijkstraOptions, SealedProjectionError};
use crate::sealed_control::Control;
use crate::shortest_path::{Entry, IndexedHeap};
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

    async fn offer(&mut self, heap: &mut IndexedHeap, node: usize, cost: f64) -> Result<()> {
        self.tick().await?;
        let mut repair = heap.offer_steps::<Error>(node, cost)?;
        loop {
            self.tick().await?;
            if repair.step().is_some() {
                return Ok(());
            }
        }
    }

    async fn pop(&mut self, heap: &mut IndexedHeap) -> Result<Option<Entry>> {
        self.tick().await?;
        let mut repair = heap.pop_steps();
        loop {
            self.tick().await?;
            if let Some(entry) = repair.step() {
                return Ok(entry);
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
            FnxAlgorithm::PageRank(_)
                | FnxAlgorithm::SingleSourceShortestPathLength { .. }
                | FnxAlgorithm::SingleSourceDijkstraPathLength(_)
                | FnxAlgorithm::ConnectedComponents
                | FnxAlgorithm::WeaklyConnectedComponents
                | FnxAlgorithm::StronglyConnectedComponents
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
                FnxAlgorithm::SingleSourceDijkstraPathLength(options) => {
                    let ordinal = graph
                        .vertex_ordinal(options.source())
                        .ok_or(ExecutionError::UnknownSource(options.source()))?;
                    admission.rows(1)?;
                    (
                        "fgdb-prism/sealed-dijkstra-indexed-heap-cooperative-v1",
                        super::shortest_path::work(n, graph.adjacency_entry_count(), pass)?,
                        super::shortest_path::workspace(n)?,
                        Some(ordinal),
                    )
                }
                algorithm @ (FnxAlgorithm::ConnectedComponents
                | FnxAlgorithm::WeaklyConnectedComponents
                | FnxAlgorithm::StronglyConnectedComponents) => {
                    admission.rows(n)?;
                    let strong = matches!(algorithm, FnxAlgorithm::StronglyConnectedComponents);
                    (
                        if strong {
                            "fgdb-prism/sealed-tarjan-cooperative-v1"
                        } else {
                            "fgdb-prism/sealed-union-find-cooperative-v1"
                        },
                        super::components::work(n, graph.adjacency_entry_count(), pass, strong)?,
                        super::components::workspace(n, strong)?,
                        None,
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
                FnxAlgorithm::SingleSourceDijkstraPathLength(options) => {
                    dijkstra(
                        graph,
                        source.ok_or(ExecutionError::InvalidUpstreamResult)?,
                        options,
                        &admission,
                        &mut control,
                    )
                    .await?
                }
                FnxAlgorithm::ConnectedComponents | FnxAlgorithm::WeaklyConnectedComponents => {
                    weak(graph, &mut control).await?
                }
                FnxAlgorithm::StronglyConnectedComponents => strong(graph, &mut control).await?,
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

// The heap and its exclusively borrowed repair both belong to this future.
// A guard/source error or dropping during a sift discards them together; no
// partially repaired queue, finalized prefix or partial row reaches the host.
async fn dijkstra<Yield, YieldFuture>(
    graph: &SealedGraphView,
    source: usize,
    options: DijkstraOptions,
    admission: &ResultAdmission,
    control: &mut Cooperate<'_, Yield>,
) -> Result<KernelOutput>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let n = graph.node_count();
    if source >= n {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    admission.rows(1)?;
    // A cutoff or unreachable component must not hide an incompatible weight.
    // Validate the whole selected graph before allocating the queue. Raw cursor
    // fuel still covers excluded endpoints, invisible versions and reductions.
    for vertex in 0..n {
        control.tick().await?;
        let mut row = graph.neighbor_cursor_controlled(control.cx, vertex, None)?;
        while let Some((target, weight)) = control.next(&mut row).await? {
            control.tick().await?;
            if target >= n {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            if !weight.is_finite() {
                return Err(ExecutionError::InvalidNumericResult.into());
            }
            if weight < 0.0 {
                return Err(ExecutionError::NegativeWeight.into());
            }
        }
    }
    control.tick().await?;
    let mut distances = reserve(n)?;
    let mut overflowed = reserve(n)?;
    let mut initialization = IndexedHeap::initialize::<Error>(n, options.comparison())?;
    loop {
        control.tick().await?;
        if initialization.step() {
            break;
        }
    }
    let mut heap = initialization.finish().ok_or(ExecutionError::InvalidUpstreamResult)?;
    for _ in 0..n {
        control.tick().await?;
        distances.push(None);
        overflowed.push(false);
    }
    control.offer(&mut heap, source, 0.0).await?;
    let mut discovered = 1usize;
    let mut witness = ComplexityWitness {
        algorithm: "single_source_dijkstra_compressed_indexed_heap".to_owned(),
        complexity_claim: "O(|V| log(1+H) + H log(1+|V|) + (|V|+|E|) log(1+|V|)) compressed row visits and queue work".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 1,
    };
    loop {
        control.tick().await?;
        let Some(Entry { node, cost, .. }) = control.pop(&mut heap).await? else {
            break;
        };
        if node >= n || distances[node].is_some() || !cost.is_finite() || cost < 0.0 {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        distances[node] = Some(cost);
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        let mut row = graph.neighbor_cursor_controlled(control.cx, node, None)?;
        while let Some((target, weight)) = control.next(&mut row).await? {
            control.tick().await?;
            witness.edges_scanned = add(witness.edges_scanned, 1)?;
            if !weight.is_finite() {
                return Err(ExecutionError::InvalidNumericResult.into());
            }
            if weight < 0.0 {
                return Err(ExecutionError::NegativeWeight.into());
            }
            if target >= n {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            if distances[target].is_some() {
                continue;
            }
            let candidate = cost + weight;
            if !candidate.is_finite() {
                if options.cutoff().is_none() {
                    overflowed[target] = true;
                }
                continue;
            }
            // Inclusive cost bound: a vertex exactly at the cutoff can still
            // reach new vertices through zero-weight edges and cycles.
            if options.cutoff().is_some_and(|limit| candidate > limit) {
                continue;
            }
            if !heap.contains(target).ok_or(ExecutionError::InvalidUpstreamResult)? {
                let requested = add(discovered, 1)?;
                admission.rows(requested)?;
                discovered = requested;
            }
            control.offer(&mut heap, target, candidate).await?;
            witness.queue_peak = witness.queue_peak.max(heap.len());
        }
    }
    for vertex in 0..n {
        control.tick().await?;
        if overflowed[vertex] && distances[vertex].is_none() {
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

// Keep union-by-size, ordinal tie breaks, path halving and canonical minimum
// labels identical to the synchronous kernel. A find is NOT an atomic step:
// every parent-link advance shares the raw-scan/scalar scheduling allowance.
async fn root<Yield, YieldFuture>(
    parents: &mut [usize],
    mut vertex: usize,
    control: &mut Cooperate<'_, Yield>,
) -> Result<usize>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    while parents[vertex] != vertex {
        control.tick().await?;
        parents[vertex] = parents[parents[vertex]];
        vertex = parents[vertex];
    }
    Ok(vertex)
}

async fn weak<Yield, YieldFuture>(
    graph: &SealedGraphView,
    control: &mut Cooperate<'_, Yield>,
) -> Result<KernelOutput>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let n = graph.node_count();
    let mut parents = reserve(n)?;
    let mut sizes = reserve(n)?;
    for vertex in 0..n {
        control.tick().await?;
        parents.push(vertex);
        sizes.push(1usize);
    }
    let mut witness = ComplexityWitness {
        algorithm: "weakly_connected_components_union_find".to_owned(),
        complexity_claim:
            "O(|V| log(1+H) + (H+|V|) log(1+|V|)) compressed row visits and bounded union-find"
                .to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    for source in 0..n {
        control.tick().await?;
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        let mut row = graph.neighbor_cursor_controlled(control.cx, source, None)?;
        while let Some((target, _)) = control.next(&mut row).await? {
            control.tick().await?;
            if target >= n {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            witness.edges_scanned = add(witness.edges_scanned, 1)?;
            let mut a = root(&mut parents, source, control).await?;
            let mut b = root(&mut parents, target, control).await?;
            if a == b {
                continue;
            }
            if sizes[a] < sizes[b] || (sizes[a] == sizes[b] && a > b) {
                std::mem::swap(&mut a, &mut b);
            }
            sizes[a] = add(sizes[a], sizes[b])?;
            parents[b] = a;
        }
    }
    // Do not replace representatives with canonical minima until the ENTIRE
    // forest has been flattened. Each of these passes cooperates even when
    // there are no edges, or when every vertex belongs to one component.
    for vertex in 0..n {
        control.tick().await?;
        let representative = root(&mut parents, vertex, control).await?;
        parents[vertex] = representative;
    }
    for size in &mut sizes {
        control.tick().await?;
        *size = usize::MAX;
    }
    for (vertex, &representative) in parents.iter().enumerate() {
        control.tick().await?;
        sizes[representative] = sizes[representative].min(vertex);
    }
    for representative in &mut parents {
        control.tick().await?;
        *representative = sizes[*representative];
    }
    control.tick().await?;
    Ok(KernelOutput {
        values: KernelValues::Components(parents),
        row_count: n,
        witness,
    })
}

// Iterative Tarjan retains the exact synchronous frame/cursor type. Each
// suspended frame keeps its raw history and parallel-edge reduction positions;
// an ancestor's row is never reopened when a child finishes. SCC membership
// discovery and both canonical-label passes share the same scheduling fuel.
async fn strong<Yield, YieldFuture>(
    graph: &SealedGraphView,
    control: &mut Cooperate<'_, Yield>,
) -> Result<KernelOutput>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    use super::components::Frame;
    use super::SealedRow;

    let n = graph.node_count();
    let mut indices = reserve(n)?;
    let mut lowlinks = reserve(n)?;
    let mut on_stack = reserve(n)?;
    let mut labels = reserve(n)?;
    let mut members = reserve(n)?;
    let mut frames: Vec<Frame<SealedRow<'_>>> = reserve(n)?;
    for _ in 0..n {
        control.tick().await?;
        indices.push(usize::MAX);
        lowlinks.push(usize::MAX);
        on_stack.push(false);
        labels.push(usize::MAX);
    }
    let mut next_index = 0usize;
    let mut witness = ComplexityWitness {
        algorithm: "strongly_connected_components_iterative_tarjan_cursor".to_owned(),
        complexity_claim: "O(|V| * (1+log(1+H)) + H log(1+|V|)) compressed row visits".to_owned(),
        nodes_touched: 0,
        edges_scanned: 0,
        queue_peak: 0,
    };
    for seed in 0..n {
        control.tick().await?;
        if indices[seed] != usize::MAX {
            continue;
        }
        let row = SealedRow {
            cx: control.cx,
            row: graph.neighbor_cursor_controlled(control.cx, seed, None)?,
        };
        indices[seed] = next_index;
        lowlinks[seed] = next_index;
        next_index = add(next_index, 1)?;
        on_stack[seed] = true;
        members.push(seed);
        frames.push(Frame { vertex: seed, row });
        witness.nodes_touched = add(witness.nodes_touched, 1)?;
        witness.queue_peak = witness.queue_peak.max(frames.len());
        while let Some(frame) = frames.last_mut() {
            control.tick().await?;
            let source = frame.vertex;
            match control.next(&mut frame.row.row).await? {
                Some((target, _)) => {
                    control.tick().await?;
                    let index = *indices.get(target).ok_or(ExecutionError::InvalidUpstreamResult)?;
                    witness.edges_scanned = add(witness.edges_scanned, 1)?;
                    if index == usize::MAX {
                        let row = SealedRow {
                            cx: control.cx,
                            row: graph.neighbor_cursor_controlled(control.cx, target, None)?,
                        };
                        indices[target] = next_index;
                        lowlinks[target] = next_index;
                        next_index = add(next_index, 1)?;
                        on_stack[target] = true;
                        members.push(target);
                        frames.push(Frame { vertex: target, row });
                        witness.nodes_touched = add(witness.nodes_touched, 1)?;
                        witness.queue_peak = witness.queue_peak.max(frames.len());
                    } else if on_stack[target] {
                        lowlinks[source] = lowlinks[source].min(index);
                    }
                }
                None => {
                    frames.pop();
                    if lowlinks[source] == indices[source] {
                        let mut start = members.len();
                        let mut minimum = source;
                        loop {
                            control.tick().await?;
                            start = start.checked_sub(1).ok_or(ExecutionError::InvalidUpstreamResult)?;
                            let member = members[start];
                            minimum = minimum.min(member);
                            if member == source { break; }
                        }
                        // A single SCC can contain the entire graph. Never
                        // hide these walks in one unbounded "component step".
                        for &member in &members[start..] {
                            control.tick().await?;
                            labels[member] = minimum;
                            on_stack[member] = false;
                        }
                        members.truncate(start);
                    }
                    if let Some(parent) = frames.last() {
                        lowlinks[parent.vertex] = lowlinks[parent.vertex].min(lowlinks[source]);
                    }
                }
            }
        }
    }
    for &label in &labels {
        control.tick().await?;
        if label == usize::MAX {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
    }
    control.tick().await?;
    Ok(KernelOutput {
        values: KernelValues::Components(labels),
        row_count: n,
        witness,
    })
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
