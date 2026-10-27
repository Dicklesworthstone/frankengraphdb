//! Direction-optimizing BFS over an admitted undirected compressed projection.
//!
//! A push frontier's projected arcs are a lower bound on its complete scan.
//! A pull's candidate pass, row opens and retained incidences are conservatively
//! priced before choosing it. Pull only when that bound is below half the push
//! lower bound; invisible history and excluded endpoints cannot make a cheap
//! visible degree conceal an expensive repeated row. Distances themselves are
//! the frontier membership map, so no bitmap or expanded adjacency is retained.
//!
//! The two drivers use the same state machine. Every graph-sized scalar pass
//! and compressed pull participates in the cooperative driver's shared fuel.
//! Tight output/workspace/work allowances retain the ordinary queue fallback.
//!
//! The push/pull technique follows GAPBS's direction-optimizing BFS, pinned at
//! https://github.com/sbeamer/gapbs/blob/941462c7c42e786e4e006a9211b0576ef78d23f7/src/bfs.cc
//! Its heuristic alpha/beta switch, parallel parent selection and bitmaps are
//! not used here: this distance-only policy prices compressed retained history
//! and keeps the registered deterministic result and resource contracts.

use super::{
    ComplexityWitness, Cursor, Delivery, EncodedRows, ExecutionError, Finished, FnxAlgorithm,
    FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits, KernelOutput, KernelValues, Result,
    ResultAdmission, Rows, SealedGraphView, SealedRows, add, checkpoint, finish_bfs,
    finish_encoded_bfs, mul, reserve,
};
use crate::sealed_control::Control;
use crate::{BfsDecisionCard, BfsPullDecision, Directedness};
use std::future::Future;
use std::mem::size_of;

fn bits(value: usize) -> usize {
    (usize::BITS - value.leading_zeros()) as usize
}

#[derive(Clone, Copy)]
struct Model {
    row_open: usize,
    incidence: usize,
    retained: usize,
    arcs: usize,
    records: usize,
    work: usize,
}

impl Model {
    fn new(n: usize, history: usize, arcs: usize, columns: usize) -> Result<Self> {
        // Each selected undirected row can visit both source faces. This is a
        // bound on all retained history, not the reduced projected edge count.
        let retained = mul(history, 2)?;
        let search = mul(4, bits(history))?;
        let row_open = add(16, search)?;
        let incidence = add(32, add(search, mul(2, bits(n))?)?)?;
        // Preparation performs bounded descriptor lookups for each vertex.
        // A complete push costs at most one row pass. Each pull replaces a
        // disjoint frontier's push visits under a stricter bound below, so
        // repeated pull rows do not require an additional full-pass multiplier.
        // Include initialization, frontier bookkeeping, result fields/evidence
        // and fixed phase transitions separately from the compressed pass.
        let scalar = add(mul(row_open, 2)?, add(columns, 48)?)?;
        let work = add(
            add(mul(n, scalar)?, mul(retained, incidence)?)?,
            add(columns, 32)?,
        )?;
        // All later frontiers are inside the current pull's unseen population.
        // Its retained count bounds their projected arcs. The strict factor-2
        // rule therefore more than halves the frontier arcs between pulls.
        // At most bits(arcs) positive frontiers can do this, even when empty
        // pull results terminate the traversal. This is <= usize::BITS, so the
        // evidence transcript is bounded independently of graph population.
        let records = n.min(bits(arcs));
        Ok(Self {
            row_open,
            incidence,
            retained,
            arcs,
            records,
            work,
        })
    }

    fn pull_bound(self, n: usize, unseen_vertices: usize, unseen_raw: usize) -> Result<usize> {
        add(
            add(n, 16)?,
            add(
                mul(unseen_vertices, self.row_open)?,
                mul(unseen_raw, self.incidence)?,
            )?,
        )
    }

    fn workspace(self, n: usize) -> Result<usize> {
        add(
            mul(n, size_of::<Option<usize>>() + 2 * size_of::<usize>())?,
            mul(self.records, size_of::<BfsPullDecision>())?,
        )
    }
}

/// Eligibility is an optimization decision, never a new resource refusal.
/// The existing queue path remains available when the complete output or this
/// additional planning/evidence state does not fit its caller's allowances.
pub(super) fn eligible(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> bool {
    if graph.spec().directedness != Directedness::Undirected
        || !matches!(
            call.algorithm(),
            // A source row has at most V projected arcs. It cannot beat a
            // complete V-candidate pull, so a zero/one-hop query never needs
            // the planning pass even when another component is very dense.
            FnxAlgorithm::SingleSourceShortestPathLength { cutoff, .. }
                if cutoff.is_none_or(|limit| limit > 1)
        )
    {
        return false;
    }
    let n = graph.node_count();
    if n == 0 || admission.rows(n).is_err() {
        return false;
    }
    if let FnxAlgorithm::SingleSourceShortestPathLength { source, .. } = call.algorithm()
        && graph
            .vertex_ordinal(source)
            .is_some_and(|vertex| graph.degree(vertex) == Some(0))
    {
        // A dense disconnected component must not make an isolated-source
        // lookup pay for a whole-population retained-history planning pass.
        return false;
    }
    let Some(minimum_pull) = n.checked_add(16) else {
        return false;
    };
    if graph.adjacency_entry_count() / 2 <= minimum_pull {
        return false;
    }
    let Ok(model) = Model::new(
        n,
        graph.scan_incidence_bound(),
        graph.adjacency_entry_count(),
        call.outputs().len(),
    ) else {
        return false;
    };
    model.work <= limits.max_estimated_work
        && model
            .workspace(n)
            .is_ok_and(|bytes| bytes <= memory.max_kernel_workspace_bytes)
}

fn reserved<T>(
    n: usize,
    live: &mut usize,
    limit: usize,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Vec<T>>> {
    control()?;
    if add(*live, mul(n, size_of::<T>())?)? > limit {
        return Ok(None);
    }
    let values = reserve(n)?;
    let actual = add(*live, mul(values.capacity(), size_of::<T>())?)?;
    // try_reserve_exact is allowed to provide extra capacity. Admit it before
    // initialization or another allocation; an optional path that no longer
    // fits drops its empty buffers and selects the existing queue fallback.
    if actual > limit {
        return Ok(None);
    }
    *live = actual;
    Ok(Some(values))
}

#[derive(Clone, Copy)]
enum Phase {
    Initialize(usize),
    Start,
    Frontier(usize),
    Choose,
    Push,
    PullCandidate(usize),
    PullRow(usize),
    Complete,
}

struct Traversal {
    distances: Vec<Option<usize>>,
    queue: Vec<usize>,
    raw: Vec<usize>,
    pulls: Vec<BfsPullDecision>,
    pending: Option<BfsPullDecision>,
    phase: Phase,
    n: usize,
    source: usize,
    cutoff: Option<usize>,
    model: Model,
    peak: usize,
    raw_sum: usize,
    degree_sum: usize,
    unseen_raw: usize,
    unseen_vertices: usize,
    head: usize,
    frontier_end: usize,
    frontier_arcs: usize,
    depth: usize,
    witness: ComplexityWitness,
}

impl Traversal {
    fn new(
        n: usize,
        source: usize,
        cutoff: Option<usize>,
        model: Model,
        limit: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Self>> {
        if source >= n {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        if model.workspace(n)? > limit {
            return Ok(None);
        }
        let mut live = 0;
        let Some(distances) = reserved(n, &mut live, limit, control)? else {
            return Ok(None);
        };
        let Some(queue) = reserved(n, &mut live, limit, control)? else {
            return Ok(None);
        };
        let Some(raw) = reserved(n, &mut live, limit, control)? else {
            return Ok(None);
        };
        let Some(pulls) = reserved(model.records, &mut live, limit, control)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            distances,
            queue,
            raw,
            pulls,
            pending: None,
            phase: Phase::Initialize(0),
            n,
            source,
            cutoff,
            model,
            peak: live,
            raw_sum: 0,
            degree_sum: 0,
            unseen_raw: 0,
            unseen_vertices: 0,
            head: 0,
            frontier_end: 0,
            frontier_arcs: 0,
            depth: 0,
            witness: ComplexityWitness {
                algorithm: "single_source_shortest_path_length_retained_pull_v2".to_owned(),
                complexity_claim: "O(V*(1+log(1+H)) + H*(1+log(1+H)+log(1+V))) bounded compressed visits and scalar work; O(V) traversal state plus at most usize::BITS pull records; H is retained source history".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 1,
            },
        }))
    }

    fn complete(&self) -> bool {
        matches!(self.phase, Phase::Complete)
    }

    fn row(&self) -> Option<usize> {
        match self.phase {
            Phase::Push if self.head < self.frontier_end => self.queue.get(self.head).copied(),
            Phase::PullRow(vertex) => Some(vertex),
            _ => None,
        }
    }

    fn start_frontier(&mut self) -> Result<()> {
        if self.head == self.queue.len() {
            self.phase = Phase::Complete;
        } else {
            self.frontier_end = self.queue.len();
            self.frontier_arcs = 0;
            self.depth = self.distances[self.queue[self.head]]
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            self.phase = Phase::Frontier(self.head);
        }
        Ok(())
    }

    fn scalar_step(
        &mut self,
        mut raw_count: impl FnMut(usize) -> Result<usize>,
        mut degree: impl FnMut(usize) -> Option<usize>,
    ) -> Result<()> {
        match self.phase {
            Phase::Initialize(vertex) if vertex < self.n => {
                let raw = raw_count(vertex)?;
                let projected = degree(vertex).ok_or(ExecutionError::InvalidUpstreamResult)?;
                if projected > raw {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                self.raw_sum = add(self.raw_sum, raw)?;
                self.degree_sum = add(self.degree_sum, projected)?;
                if self.raw_sum > self.model.retained || self.degree_sum > self.model.arcs {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                self.raw.push(raw);
                self.distances.push(None);
                self.phase = Phase::Initialize(vertex + 1);
            }
            Phase::Initialize(_) => {
                if self.degree_sum != self.model.arcs {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                self.phase = Phase::Start;
            }
            Phase::Start => {
                self.unseen_raw = self.raw_sum - self.raw[self.source];
                self.unseen_vertices = self.n - 1;
                self.distances[self.source] = Some(0);
                self.queue.push(self.source);
                self.start_frontier()?;
            }
            Phase::Frontier(at) if at < self.frontier_end => {
                let vertex = self.queue[at];
                self.frontier_arcs = add(
                    self.frontier_arcs,
                    degree(vertex).ok_or(ExecutionError::InvalidUpstreamResult)?,
                )?;
                self.witness.nodes_touched = add(self.witness.nodes_touched, 1)?;
                self.phase = Phase::Frontier(at + 1);
            }
            Phase::Frontier(_) => self.phase = Phase::Choose,
            Phase::Choose => {
                if self.cutoff.is_some_and(|cutoff| self.depth >= cutoff) {
                    self.phase = Phase::Complete;
                    return Ok(());
                }
                let pull_bound =
                    self.model
                        .pull_bound(self.n, self.unseen_vertices, self.unseen_raw)?;
                if self.frontier_arcs / 2 > pull_bound {
                    if self.pulls.len() == self.model.records {
                        return Err(ExecutionError::InvalidUpstreamResult.into());
                    }
                    self.pending = Some(BfsPullDecision {
                        depth: self.depth,
                        frontier_vertices: self.frontier_end - self.head,
                        frontier_arcs: self.frontier_arcs,
                        unseen_incoming_arcs: self.unseen_raw,
                        vertices: self.n,
                        benefit_lower: self.frontier_arcs - pull_bound,
                        benefit_upper: self.frontier_arcs - self.n,
                        candidate_visits: 0,
                        incoming_visits: 0,
                    });
                    self.head = self.frontier_end;
                    self.phase = Phase::PullCandidate(0);
                } else {
                    self.phase = Phase::Push;
                }
            }
            Phase::Push if self.head == self.frontier_end => self.start_frontier()?,
            Phase::PullCandidate(vertex) if vertex < self.n => {
                self.witness.nodes_touched = add(self.witness.nodes_touched, 1)?;
                let decision = self
                    .pending
                    .as_mut()
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                decision.candidate_visits = add(decision.candidate_visits, 1)?;
                self.phase = if self.distances[vertex].is_none() {
                    Phase::PullRow(vertex)
                } else {
                    Phase::PullCandidate(vertex + 1)
                };
            }
            Phase::PullCandidate(_) => {
                let decision = self
                    .pending
                    .take()
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                self.pulls.push(decision);
                self.start_frontier()?;
            }
            Phase::Complete => {}
            _ => return Err(ExecutionError::InvalidUpstreamResult.into()),
        }
        Ok(())
    }

    fn discover(&mut self, vertex: usize) -> Result<()> {
        if self.distances[vertex].is_none() {
            self.distances[vertex] = Some(add(self.depth, 1)?);
            self.queue.push(vertex);
            self.unseen_raw = self
                .unseen_raw
                .checked_sub(self.raw[vertex])
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            self.unseen_vertices = self
                .unseen_vertices
                .checked_sub(1)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            let consumed = self.head + usize::from(matches!(self.phase, Phase::Push));
            self.witness.queue_peak = self.witness.queue_peak.max(self.queue.len() - consumed);
        }
        Ok(())
    }

    /// True ends a pull row immediately after its first previous-level neighbor.
    fn visit(&mut self, neighbor: usize) -> Result<bool> {
        if neighbor >= self.n {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        self.witness.edges_scanned = add(self.witness.edges_scanned, 1)?;
        match self.phase {
            Phase::Push => {
                self.discover(neighbor)?;
                Ok(false)
            }
            Phase::PullRow(vertex) => {
                let decision = self
                    .pending
                    .as_mut()
                    .ok_or(ExecutionError::InvalidUpstreamResult)?;
                decision.incoming_visits = add(decision.incoming_visits, 1)?;
                if self.distances[neighbor] == Some(self.depth) {
                    self.discover(vertex)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            _ => Err(ExecutionError::InvalidUpstreamResult.into()),
        }
    }

    fn end_row(&mut self) -> Result<()> {
        match self.phase {
            Phase::Push => self.head += 1,
            Phase::PullRow(vertex) => self.phase = Phase::PullCandidate(vertex + 1),
            _ => return Err(ExecutionError::InvalidUpstreamResult.into()),
        }
        Ok(())
    }

    fn finish(self) -> Result<(KernelOutput, Option<BfsDecisionCard>, usize)> {
        if !self.complete() || self.pending.is_some() {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        let card = (!self.pulls.is_empty()).then_some(BfsDecisionCard {
            policy_version: 2,
            fallback: "top_down",
            dwell_levels: 1,
            pull_cost_multiplier: 2,
            pulls: self.pulls,
        });
        Ok((
            KernelOutput {
                values: KernelValues::Distances(self.distances),
                row_count: self.queue.len(),
                witness: self.witness,
            },
            card,
            self.peak,
        ))
    }
}

fn prepare(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Traversal>> {
    if !eligible(call, graph, limits, memory, admission) {
        return Ok(None);
    }
    let FnxAlgorithm::SingleSourceShortestPathLength { source, cutoff } = call.algorithm() else {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    };
    let source = graph
        .vertex_ordinal(source)
        .ok_or(ExecutionError::UnknownSource(source))?;
    let model = Model::new(
        graph.node_count(),
        graph.scan_incidence_bound(),
        graph.adjacency_entry_count(),
        call.outputs().len(),
    )?;
    Traversal::new(
        graph.node_count(),
        source,
        cutoff,
        model,
        memory.max_kernel_workspace_bytes,
        control,
    )
}

fn validate_neighbor(
    neighbor: usize,
    n: usize,
    previous: &mut Option<usize>,
    seen: &mut usize,
    degree: usize,
) -> Result<()> {
    if neighbor >= n || previous.is_some_and(|last| last >= neighbor) {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    *previous = Some(neighbor);
    *seen = add(*seen, 1)?;
    if *seen > degree {
        return Err(ExecutionError::InvalidUpstreamResult.into());
    }
    Ok(())
}

fn run(
    graph: &impl Rows,
    mut raw_count: impl FnMut(usize) -> Result<usize>,
    mut state: Traversal,
    control: &mut impl FnMut() -> Result<()>,
) -> Result<(KernelOutput, Option<BfsDecisionCard>, usize)> {
    while !state.complete() {
        control()?;
        if let Some(vertex) = state.row() {
            let degree = graph
                .degree(vertex)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            let mut row = graph.open(vertex)?;
            let mut previous = None;
            let mut seen = 0;
            let mut exhausted = true;
            while let Some((neighbor, _)) = row.next()? {
                control()?;
                validate_neighbor(neighbor, state.n, &mut previous, &mut seen, degree)?;
                if state.visit(neighbor)? {
                    exhausted = false;
                    break;
                }
            }
            if exhausted && seen != degree {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            state.end_row()?;
        } else {
            state.scalar_step(&mut raw_count, |vertex| graph.degree(vertex))?;
        }
    }
    control()?;
    state.finish()
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Option<Finished>> {
    let mut control = || checkpoint(cx);
    let Some(state) = prepare(call, graph, limits, memory, admission, &mut control)? else {
        return Ok(None);
    };
    let estimated = state.model.work;
    let rows = SealedRows { cx, graph };
    let (output, card, peak) = run(
        &rows,
        |vertex| {
            graph
                .retained_row_incidence_bound(vertex)
                .map_err(Into::into)
        },
        state,
        &mut control,
    )?;
    finish_bfs(
        call,
        graph,
        output,
        "fgdb-prism/sealed-bfs-retained-pull-v2",
        estimated,
        peak,
        admission,
        card,
        &mut control,
    )
    .map(Some)
}

pub(super) async fn execute_cooperative<Yield, YieldFuture>(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    delivery: Delivery,
    control: &mut super::cooperative::Cooperate<'_, Yield>,
) -> Result<Option<Finished>>
where
    Yield: FnMut() -> YieldFuture,
    YieldFuture: Future<Output = ()>,
{
    let admission = ResultAdmission::for_delivery(call, limits, memory, delivery)?;
    let Some(mut state) = prepare(call, graph, limits, memory, &admission, &mut || {
        checkpoint(control.cx)
    })?
    else {
        return Ok(None);
    };
    let estimated = state.model.work;
    while !state.complete() {
        control.tick().await?;
        if let Some(vertex) = state.row() {
            let degree = graph
                .degree(vertex)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            let mut row = graph.neighbor_cursor_controlled(control.cx, vertex, None)?;
            let mut previous = None;
            let mut seen = 0;
            let mut exhausted = true;
            while let Some((neighbor, _)) = control.next(&mut row).await? {
                control.tick().await?;
                validate_neighbor(neighbor, state.n, &mut previous, &mut seen, degree)?;
                if state.visit(neighbor)? {
                    exhausted = false;
                    break;
                }
            }
            if exhausted && seen != degree {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            state.end_row()?;
        } else {
            state.scalar_step(
                |vertex| {
                    graph
                        .retained_row_incidence_bound(vertex)
                        .map_err(Into::into)
                },
                |vertex| graph.degree(vertex),
            )?;
        }
    }
    control.tick().await?;
    let (output, card, peak) = state.finish()?;
    let mut encoded = EncodedRows::new(call, output.row_count, &admission)?;
    for vertex in 0..graph.node_count() {
        control.tick().await?;
        encoded.push(call, graph, &output.values, vertex, &mut || {
            checkpoint(control.cx)
        })?;
    }
    control.tick().await?;
    finish_encoded_bfs(
        call,
        graph,
        encoded,
        output.values,
        output.witness,
        "fgdb-prism/sealed-bfs-retained-pull-cooperative-v2",
        estimated,
        peak,
        card,
        &mut || checkpoint(control.cx),
    )
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::super::{Error, SealedProjectionError};
    use super::*;
    use std::cell::Cell;

    struct Fixture {
        rows: Vec<Vec<usize>>,
        degrees: Vec<usize>,
        raw: Vec<usize>,
        steps: Cell<usize>,
        fail_at: usize,
        opens: Vec<Cell<usize>>,
    }

    impl Fixture {
        fn new(n: usize, edges: &[(usize, usize)]) -> Self {
            let mut rows = vec![Vec::new(); n];
            for &(left, right) in edges {
                rows[left].push(right);
                if left != right {
                    rows[right].push(left);
                }
            }
            for row in &mut rows {
                row.sort_unstable();
                row.dedup();
            }
            let degrees: Vec<_> = rows.iter().map(Vec::len).collect();
            Self {
                rows,
                raw: degrees.clone(),
                degrees,
                steps: Cell::new(0),
                fail_at: usize::MAX,
                opens: (0..n).map(|_| Cell::new(0)).collect(),
            }
        }

        fn step(&self) -> Result<()> {
            let next = self.steps.get() + 1;
            self.steps.set(next);
            if next == self.fail_at {
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                    usize::MAX,
                )))
            } else {
                Ok(())
            }
        }

        fn model(&self) -> Model {
            Model::new(
                self.rows.len(),
                self.raw.iter().sum::<usize>().div_ceil(2),
                self.degrees.iter().sum(),
                2,
            )
            .unwrap()
        }

        fn execute(
            &self,
            source: usize,
            cutoff: Option<usize>,
            control: &mut impl FnMut() -> Result<()>,
        ) -> Result<(KernelOutput, Option<BfsDecisionCard>, usize)> {
            let state = Traversal::new(
                self.rows.len(),
                source,
                cutoff,
                self.model(),
                usize::MAX,
                control,
            )?
            .unwrap();
            run(
                self,
                |vertex| {
                    self.step()?;
                    Ok(self.raw[vertex])
                },
                state,
                control,
            )
        }

        fn reset(&self) {
            self.steps.set(0);
            for opens in &self.opens {
                opens.set(0);
            }
        }
    }

    struct FixtureRow<'a> {
        fixture: &'a Fixture,
        row: &'a [usize],
        at: usize,
    }

    impl Cursor for FixtureRow<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.fixture.step()?;
            let result = self.row.get(self.at).copied();
            self.at += 1;
            // BFS is topology-only; neither signed nor nonfinite weights may
            // change the path lengths after the projection has been admitted.
            Ok(result.map(|target| (target, f64::NAN)))
        }
    }

    impl Rows for Fixture {
        type Cursor<'a> = FixtureRow<'a>;

        fn node_count(&self) -> usize {
            self.rows.len()
        }
        fn degree(&self, source: usize) -> Option<usize> {
            self.degrees.get(source).copied()
        }
        fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
            self.step()?;
            let row = self
                .rows
                .get(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            self.opens[source].set(self.opens[source].get() + 1);
            Ok(FixtureRow {
                fixture: self,
                row,
                at: 0,
            })
        }
    }

    fn distances(output: &KernelOutput) -> &[Option<usize>] {
        let KernelValues::Distances(values) = &output.values else {
            panic!("distance output")
        };
        values
    }

    // Floyd-Warshall has no BFS frontier, queue or direction-selection logic.
    fn oracle(fixture: &Fixture, source: usize, cutoff: Option<usize>) -> Vec<Option<usize>> {
        let n = fixture.rows.len();
        let mut distance = vec![vec![n + 1; n]; n];
        for (vertex, row) in fixture.rows.iter().enumerate() {
            for &neighbor in row {
                distance[vertex][neighbor] = 1;
            }
            distance[vertex][vertex] = 0;
        }
        for middle in 0..n {
            for left in 0..n {
                for right in 0..n {
                    distance[left][right] =
                        distance[left][right].min(distance[left][middle] + distance[middle][right]);
                }
            }
        }
        distance[source]
            .iter()
            .map(|&value| {
                (value <= n && cutoff.is_none_or(|limit| value <= limit)).then_some(value)
            })
            .collect()
    }

    fn dense() -> Fixture {
        let width = 48;
        let mut edges = Vec::new();
        for vertex in 1..=width {
            edges.push((0, vertex));
            for target in vertex + 1..=width {
                edges.push((vertex, target));
            }
        }
        edges.extend([(1, width + 1), (width + 1, width + 2), (0, 0)]);
        Fixture::new(width + 4, &edges)
    }

    #[test]
    fn all_small_undirected_graphs_and_cutoffs_match_independent_shortest_paths() {
        let mut graphs = 0;
        for n in 1..=5 {
            let possible: Vec<_> = (0..n)
                .flat_map(|left| (left + 1..n).map(move |right| (left, right)))
                .collect();
            for mask in 0..1usize << possible.len() {
                let mut edges: Vec<_> = possible
                    .iter()
                    .enumerate()
                    .filter_map(|(bit, &edge)| (mask & (1 << bit) != 0).then_some(edge))
                    .collect();
                // Retained loops and hidden histories exercise a different
                // source representation without altering shortest distances.
                for vertex in 0..n {
                    if (mask + vertex) % 3 == 0 {
                        edges.push((vertex, vertex));
                    }
                }
                let mut fixture = Fixture::new(n, &edges);
                for (vertex, raw) in fixture.raw.iter_mut().enumerate() {
                    *raw += (mask + vertex) % 5;
                }
                for source in 0..n {
                    for cutoff in [None, Some(0), Some(1), Some(2), Some(usize::MAX)] {
                        let (actual, card, _) =
                            fixture.execute(source, cutoff, &mut || Ok(())).unwrap();
                        let expected = oracle(&fixture, source, cutoff);
                        assert_eq!(distances(&actual), expected);
                        assert_eq!(actual.row_count, expected.iter().flatten().count());
                        assert!(card.is_none(), "small graph has no profitable pull");
                    }
                }
                graphs += 1;
            }
        }
        assert_eq!(graphs, 1099);
    }

    #[test]
    fn dense_pull_skips_frontier_rows_and_prices_retained_unseen_history() {
        let mut fixture = dense();
        let (actual, card, peak) = fixture.execute(0, None, &mut || Ok(())).unwrap();
        assert_eq!(distances(&actual), oracle(&fixture, 0, None));
        let card = card.expect("dense frontier selects pull");
        assert_eq!(card.policy_version, 2);
        assert_eq!(card.pulls.len(), 1);
        assert_eq!(card.pulls[0].depth, 1);
        assert_eq!(card.pulls[0].candidate_visits, fixture.rows.len());
        assert!(card.pulls[0].incoming_visits <= card.pulls[0].unseen_incoming_arcs);
        assert!(card.pulls[0].benefit_lower <= card.pulls[0].benefit_upper);
        assert!(actual.witness.edges_scanned < fixture.model().arcs / 10);
        assert_eq!(peak, fixture.model().workspace(fixture.rows.len()).unwrap());
        for vertex in 1..=48 {
            assert_eq!(
                fixture.opens[vertex].get(),
                0,
                "dense frontier row is not opened"
            );
        }
        let pull = &card.pulls[0];
        let bound = fixture
            .model()
            .pull_bound(fixture.rows.len(), 3, pull.unseen_incoming_arcs)
            .unwrap();
        assert!(pull.frontier_arcs / 2 > bound);
        assert_eq!(pull.benefit_lower, pull.frontier_arcs - bound);

        // The isolate can have no visible edge and still carry expensive
        // invisible/excluded history. It must prevent the seemingly cheap
        // candidate scan; observed rows remain identical to the dense result.
        let last = fixture.raw.len() - 1;
        fixture.raw[last] = 100_000;
        fixture.reset();
        let (historical, card, _) = fixture.execute(0, None, &mut || Ok(())).unwrap();
        assert_eq!(distances(&actual), distances(&historical));
        assert!(card.is_none());
        assert_eq!(historical.witness.edges_scanned, fixture.model().arcs);
        assert_eq!(fixture.opens[last].get(), 0);

        for cutoff in [Some(0), Some(1), Some(2)] {
            let (actual, _, _) = fixture.execute(0, cutoff, &mut || Ok(())).unwrap();
            assert_eq!(distances(&actual), oracle(&fixture, 0, cutoff));
        }
    }

    #[test]
    fn every_scalar_and_cursor_refusal_aborts_the_complete_adaptive_result() {
        let mut fixture = dense();
        let mut scalar = 0;
        let expected = fixture
            .execute(0, None, &mut || {
                scalar += 1;
                Ok(())
            })
            .unwrap();
        let source_steps = fixture.steps.get();
        for stop in 1..=source_steps {
            fixture.reset();
            fixture.fail_at = stop;
            assert!(matches!(
                fixture.execute(0, None, &mut || Ok(())),
                Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                    usize::MAX
                )))
            ));
            assert_eq!(fixture.steps.get(), stop);
        }
        fixture.fail_at = usize::MAX;
        for stop in 1..=scalar {
            fixture.reset();
            let mut units = 0;
            let actual = fixture.execute(0, None, &mut || {
                units += 1;
                if units == stop {
                    Err(Error::Source(SealedProjectionError::UnknownOrdinal(
                        usize::MAX,
                    )))
                } else {
                    Ok(())
                }
            });
            assert!(actual.is_err());
            assert_eq!(units, stop);
        }
        fixture.reset();
        let actual = fixture.execute(0, None, &mut || Ok(())).unwrap();
        assert_eq!(distances(&actual.0), distances(&expected.0));
        assert_eq!(actual.1, expected.1);
    }

    #[test]
    fn malformed_rows_counts_and_arithmetic_fail_without_unchecked_indexing() {
        for row in [vec![1, 1], vec![2, 1], vec![3]] {
            let mut fixture = Fixture::new(3, &[(0, 1), (0, 2)]);
            fixture.rows[0] = row;
            assert!(matches!(
                fixture.execute(0, None, &mut || Ok(())),
                Err(Error::Execution(ExecutionError::InvalidUpstreamResult))
            ));
        }
        let mut fixture = Fixture::new(3, &[(0, 1), (0, 2)]);
        let _ = fixture.rows[0].pop();
        assert!(fixture.execute(0, None, &mut || Ok(())).is_err());
        fixture.raw[1] = 0;
        assert!(fixture.execute(0, None, &mut || Ok(())).is_err());
        assert!(Model::new(usize::MAX, 1, 1, 2).is_err());
        assert!(Model::new(1, usize::MAX, 1, 2).is_err());
        assert!(Model::new(1, 1, 1, usize::MAX).is_err());
        assert!(fixture.model().pull_bound(1, usize::MAX, 1).is_err());
        assert!(fixture.model().pull_bound(1, 1, usize::MAX).is_err());
        assert!(
            Traversal::new(
                0,
                0,
                None,
                Model::new(0, 0, 0, 2).unwrap(),
                usize::MAX,
                &mut || Ok(())
            )
            .is_err()
        );
    }

    #[test]
    fn exact_capacity_admission_and_sparse_populations_keep_bounded_state() {
        let fixture = dense();
        let model = fixture.model();
        let bytes = model.workspace(fixture.rows.len()).unwrap();
        assert!(
            Traversal::new(
                fixture.rows.len(),
                0,
                None,
                model,
                bytes - 1,
                &mut || Ok(())
            )
            .unwrap()
            .is_none()
        );
        let state = Traversal::new(fixture.rows.len(), 0, None, model, bytes, &mut || Ok(()))
            .unwrap()
            .unwrap();
        assert_eq!(state.peak, bytes);
        assert!(state.pulls.capacity() <= usize::BITS as usize);
        let output = run(
            &fixture,
            |vertex| Ok(fixture.raw[vertex]),
            state,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(distances(&output.0), oracle(&fixture, 0, None));

        let sparse = Fixture::new(
            8193,
            &(1..=64).map(|vertex| (0, vertex)).collect::<Vec<_>>(),
        );
        let (actual, card, peak) = sparse.execute(0, None, &mut || Ok(())).unwrap();
        assert_eq!(actual.row_count, 65);
        assert_eq!(distances(&actual)[0], Some(0));
        assert!(
            distances(&actual)[1..=64]
                .iter()
                .all(|distance| *distance == Some(1))
        );
        assert!(distances(&actual)[65..].iter().all(Option::is_none));
        assert!(card.is_none());
        assert_eq!(actual.witness.edges_scanned, 128);
        assert_eq!(peak, sparse.model().workspace(8193).unwrap());
        assert_eq!(sparse.opens.iter().map(Cell::get).sum::<usize>(), 65);
    }
}
