//! Graphalytics synchronous label updates over compressed rows.
//!
//! Labels and epoch-tagged vote counters occupy four O(|V|) integer vectors.
//! Directed graphs visit separately reduced outgoing and incoming rows, so a
//! reciprocal neighbor contributes twice. The reverse projection retains a
//! bounded compressed locator index, never a decoded adjacency copy. Its
//! construction peak participates in the kernel's workspace allowance.
//! Immediate and cooperative drivers share vote arithmetic and round commits;
//! scheduling never changes which previous-round labels a vertex observes.
//!
//! When optional frontier storage fits, only changed vertices and their weak
//! neighbors need another update. Changed vertices form a frozen queue prefix:
//! their ordinary vote scans also enqueue affected neighbors, so notification
//! never opens another row. All candidates still read the previous round.
//! Including changed vertices themselves ensures every inactive next-buffer
//! slot already equals its current label when the buffers are swapped.

use super::{
    ComplexityWitness, Cursor, Delivery, Directedness, ExecutionError, Finished, FnxAlgorithm,
    FnxCallSpec, FnxExecutionLimits, FnxMemoryLimits, KernelOutput, KernelValues, Result,
    ResultAdmission, Rows, SealedGraphView, SealedRows, add, admit, checkpoint,
    directional_pass_work, finish, finish_encoded, mul, reserve,
};
use crate::sealed_control::Control;
use std::future::Future;
use std::mem::size_of;

fn workspace(n: usize, iterations: usize) -> Result<usize> {
    mul(
        n,
        mul(if iterations == 0 { 1 } else { 4 }, size_of::<usize>())?,
    )
}

fn work(
    n: usize,
    retained: usize,
    arcs: usize,
    direction: Directedness,
    iterations: usize,
    columns: usize,
) -> Result<usize> {
    let forward = directional_pass_work(n, retained, direction)?;
    // Loop validation, buffer initialization, and final canonical output.
    let base = add(
        forward,
        add(arcs, add(mul(n, add(12, columns)?)?, add(32, columns)?)?)?,
    )?;
    if n == 0 || iterations == 0 {
        return Ok(base);
    }
    let (reverse, setup, incidences) = if direction == Directedness::Undirected {
        (0, 0, arcs)
    } else {
        let reversed = if direction == Directedness::Directed {
            Directedness::Reversed
        } else {
            Directedness::Directed
        };
        let reverse = directional_pass_work(n, retained, reversed)?;
        // The temporary reverse projection authenticates/counts its rows.
        // A new incoming locator index also collects, heapsorts and encodes
        // retained incidences. Charge that construction even when an already
        // reversed source only needs its original outgoing face.
        let height = (usize::BITS - retained.leading_zeros()) as usize;
        let index = mul(retained, add(32, mul(height, 4)?)?)?;
        let setup = add(mul(reverse, 2)?, add(index, mul(n, 8)?)?)?;
        (reverse, setup, mul(arcs, 2)?)
    };
    let round = add(
        add(forward, reverse)?,
        add(incidences, add(mul(n, 2)?, 4)?)?,
    )?;
    // The optional frontier opens each row at most once per round. Scheduling
    // is part of that row's existing vote step; its extra scalar clear visits
    // at most one touched bitmap word per updated vertex. Thus 2V + 4 still
    // covers row steps, word clears and round boundaries. Initialization adds
    // V queue entries and ceil(V/64) word pairs inside the base's 12V allowance.
    add(base, add(setup, mul(round, iterations)?)?)
}

fn reserved(n: usize, live: &mut usize, limit: usize) -> Result<Vec<usize>> {
    admit(
        "kernel workspace bytes",
        add(*live, mul(n, size_of::<usize>())?)?,
        limit,
    )?;
    let values = reserve(n)?;
    let actual = add(*live, mul(values.capacity(), size_of::<usize>())?)?;
    // An allocator may exceed try_reserve_exact's request. Check the actual
    // backing before initializing it or allocating the next vector.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

fn bitmap_words(n: usize) -> usize {
    n.div_ceil(64)
}

fn frontier_workspace(n: usize) -> Result<usize> {
    add(
        mul(n, size_of::<usize>())?,
        mul(bitmap_words(n), 2 * (size_of::<u64>() + size_of::<usize>()))?,
    )
}

fn optional_reserved<T>(n: usize, live: &mut usize, limit: usize) -> Result<Option<Vec<T>>> {
    optional_reserved_with(n, live, limit, reserve)
}

fn optional_reserved_with<T>(
    n: usize,
    live: &mut usize,
    limit: usize,
    allocate: impl FnOnce(usize) -> Result<Vec<T>>,
) -> Result<Option<Vec<T>>> {
    if add(*live, mul(n, size_of::<T>())?)? > limit {
        return Ok(None);
    }
    let values = match allocate(n) {
        Ok(values) => values,
        Err(super::Error::Execution(ExecutionError::AllocationFailed)) => return Ok(None),
        Err(error) => return Err(error),
    };
    let actual = add(*live, mul(values.capacity(), size_of::<T>())?)?;
    // A real allocation above the allowance cannot become a successful
    // fallback: its transient backing already exceeded the admitted bound.
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(Some(values))
}

struct Marks {
    words: Vec<u64>,
    touched: Vec<usize>,
}

impl Marks {
    fn reserve(n: usize, live: &mut usize, limit: usize) -> Result<Option<Self>> {
        let n = bitmap_words(n);
        let Some(words) = optional_reserved(n, live, limit)? else {
            return Ok(None);
        };
        let Some(touched) = optional_reserved(n, live, limit)? else {
            return Ok(None);
        };
        Ok(Some(Self { words, touched }))
    }

    fn insert(&mut self, vertex: usize) -> bool {
        let word = vertex / 64;
        let bit = 1u64 << (vertex % 64);
        if self.words[word] & bit != 0 {
            return false;
        }
        if self.words[word] == 0 {
            self.touched.push(word);
        }
        self.words[word] |= bit;
        true
    }

    fn clear_one(&mut self) {
        if let Some(word) = self.touched.pop() {
            self.words[word] = 0;
        }
    }
}

struct ActiveRows {
    queue: Vec<usize>,
    queued: Marks,
    changed: Marks,
    head: usize,
    changes: usize,
    notify_end: usize,
}

impl ActiveRows {
    fn reserve(n: usize, peak: &mut usize, limit: usize) -> Result<Option<Self>> {
        Self::reserve_with_queue(n, peak, limit, reserve)
    }

    fn reserve_with_queue(
        n: usize,
        peak: &mut usize,
        limit: usize,
        allocate_queue: impl FnOnce(usize) -> Result<Vec<usize>>,
    ) -> Result<Option<Self>> {
        if add(*peak, frontier_workspace(n)?)? > limit {
            return Ok(None);
        }
        // Each successful allocation raises the caller's high-water charge
        // immediately. A later fallback drops the partial frontier but must
        // still report the temporary backing that was actually live.
        let Some(queue) = optional_reserved_with(n, peak, limit, allocate_queue)? else {
            return Ok(None);
        };
        let Some(queued) = Marks::reserve(n, peak, limit)? else {
            return Ok(None);
        };
        let Some(changed) = Marks::reserve(n, peak, limit)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            queue,
            queued,
            changed,
            head: 0,
            changes: 0,
            // Every row is initially queued; the first full sweep needs
            // no notification work, even though initial labels differ.
            notify_end: 0,
        }))
    }

    fn initialize_word(&mut self) {
        self.queued.words.push(0);
        self.changed.words.push(0);
    }

    fn initialize_vertex(&mut self, vertex: usize) {
        self.queued.insert(vertex);
        self.queue.push(vertex);
    }

    fn pop(&mut self) -> Option<(usize, bool)> {
        let source = *self.queue.get(self.head)?;
        let notify = self.head < self.notify_end;
        self.head += 1;
        Some((source, notify))
    }

    fn schedule(&mut self, vertex: usize, n: usize) -> Result<()> {
        if self.queued.insert(vertex) {
            if self.queue.len() == n {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            self.queue.push(vertex);
        }
        Ok(())
    }

    fn record_change(&mut self, source: usize) -> Result<()> {
        // Only consumed queue slots may be overwritten. Unvisited members of
        // the frozen notification prefix and newly appended rows stay intact.
        if self.changes >= self.head || self.head > self.queue.len() {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        self.queue[self.changes] = source;
        self.changes += 1;
        self.changed.insert(source);
        Ok(())
    }

    fn finish_round(&mut self) -> Result<()> {
        if self.head != self.queue.len() {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        self.queue.truncate(self.changes);
        self.head = 0;
        self.changes = 0;
        self.notify_end = self.queue.len();
        std::mem::swap(&mut self.queued, &mut self.changed);
        Ok(())
    }
}

struct Labels {
    labels: Vec<usize>,
    next: Vec<usize>,
    counts: Vec<usize>,
    generations: Vec<usize>,
    generation: usize,
    frontier: Option<ActiveRows>,
    peak: usize,
    witness: ComplexityWitness,
}

impl Labels {
    fn new(n: usize, iterations: usize, limit: usize) -> Result<Self> {
        admit("kernel workspace bytes", workspace(n, iterations)?, limit)?;
        let mut live = 0;
        let labels = reserved(n, &mut live, limit)?;
        let (next, counts, generations) = if iterations == 0 || n == 0 {
            (Vec::new(), Vec::new(), Vec::new())
        } else {
            (
                reserved(n, &mut live, limit)?,
                reserved(n, &mut live, limit)?,
                reserved(n, &mut live, limit)?,
            )
        };
        Ok(Self {
            labels,
            next,
            counts,
            generations,
            generation: 0,
            frontier: None,
            peak: live,
            witness: ComplexityWitness {
                algorithm: "cdlp_synchronous_minimum_vid_cursor".to_owned(),
                complexity_claim: "O(|V| + H*(1+log(1+H)) + k*(|V|*(1+log(1+H)) + H*(1+log(1+|V|)))) compressed incidence visits; O(|V|) label workspace plus a bounded compressed reverse index".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
        })
    }

    fn reserve_frontier(&mut self, iterations: usize, limit: usize) -> Result<()> {
        if iterations < 2 || self.labels.is_empty() {
            return Ok(());
        }
        if let Some(frontier) = ActiveRows::reserve(self.labels.len(), &mut self.peak, limit)? {
            self.frontier = Some(frontier);
            self.witness.algorithm = "cdlp_synchronous_minimum_vid_active_cursor".to_owned();
            self.witness.queue_peak = self.labels.len();
        }
        Ok(())
    }

    fn initialize_frontier(&mut self, control: &mut impl FnMut() -> Result<()>) -> Result<()> {
        if let Some(frontier) = &mut self.frontier {
            for _ in 0..bitmap_words(self.labels.len()) {
                control()?;
                frontier.initialize_word();
            }
            for vertex in 0..self.labels.len() {
                control()?;
                frontier.initialize_vertex(vertex);
            }
        }
        Ok(())
    }

    fn next_row(&mut self, source: &mut usize) -> Option<(usize, bool)> {
        if let Some(frontier) = &mut self.frontier {
            frontier.pop()
        } else if *source < self.labels.len() {
            let vertex = *source;
            *source += 1;
            Some((vertex, false))
        } else {
            None
        }
    }

    fn prepare(
        &mut self,
        graph: &impl Rows,
        iterations: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        // The registered loop-free graph law also applies to zero updates.
        for source in 0..graph.node_count() {
            control()?;
            let mut row = graph.open(source)?;
            while let Some((target, _)) = row.next()? {
                control()?;
                if target >= graph.node_count() {
                    return Err(ExecutionError::InvalidUpstreamResult.into());
                }
                if target == source {
                    return Err(ExecutionError::SelfLoopRefused.into());
                }
            }
        }
        for vertex in 0..graph.node_count() {
            control()?;
            self.labels.push(vertex);
        }
        if iterations != 0 {
            for values in [&mut self.next, &mut self.counts, &mut self.generations] {
                for _ in 0..graph.node_count() {
                    control()?;
                    values.push(0);
                }
            }
        }
        Ok(())
    }

    fn votes(
        &mut self,
        mut row: impl Cursor,
        best: &mut usize,
        best_count: &mut usize,
        notify: bool,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        while let Some((neighbor, _)) = row.next()? {
            control()?;
            self.vote(neighbor, best, best_count, notify)?;
        }
        Ok(())
    }

    fn begin_row(&mut self, source: usize) -> Result<(usize, usize)> {
        self.generation = add(self.generation, 1)?;
        let best = *self
            .labels
            .get(source)
            .ok_or(ExecutionError::InvalidUpstreamResult)?;
        Ok((best, 0))
    }

    fn vote(
        &mut self,
        neighbor: usize,
        best: &mut usize,
        best_count: &mut usize,
        notify: bool,
    ) -> Result<()> {
        let label = *self
            .labels
            .get(neighbor)
            .ok_or(ExecutionError::InvalidUpstreamResult)?;
        if notify {
            self.frontier
                .as_mut()
                .ok_or(ExecutionError::InvalidUpstreamResult)?
                .schedule(neighbor, self.labels.len())?;
        }
        if self.generations[label] != self.generation {
            self.generations[label] = self.generation;
            self.counts[label] = 0;
        }
        let count = add(self.counts[label], 1)?;
        self.counts[label] = count;
        if count > *best_count || (count == *best_count && label < *best) {
            *best = label;
            *best_count = count;
        }
        self.witness.edges_scanned = add(self.witness.edges_scanned, 1)?;
        Ok(())
    }

    fn finish_row(&mut self, source: usize, best: usize) -> Result<()> {
        *self
            .next
            .get_mut(source)
            .ok_or(ExecutionError::InvalidUpstreamResult)? = best;
        if best != self.labels[source]
            && let Some(frontier) = &mut self.frontier
        {
            frontier.record_change(source)?;
        }
        self.witness.nodes_touched = add(self.witness.nodes_touched, 1)?;
        Ok(())
    }

    fn finish_round(&mut self) -> Result<()> {
        // No vertex in this round can see another vertex's new label.
        // Every inactive vertex was unchanged last round AND has no changed
        // neighbor. Its old next-buffer entry is already the exact new label.
        std::mem::swap(&mut self.labels, &mut self.next);
        if let Some(frontier) = &mut self.frontier {
            frontier.finish_round()?;
        }
        Ok(())
    }

    fn finish(self) -> (KernelOutput, usize) {
        let row_count = self.labels.len();
        (
            KernelOutput {
                values: KernelValues::Components(self.labels),
                row_count,
                witness: self.witness,
            },
            self.peak,
        )
    }

    fn iterate<G: Rows>(
        mut self,
        graph: &G,
        reverse: Option<&G>,
        iterations: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<(KernelOutput, usize)> {
        let n = graph.node_count();
        if reverse.is_some_and(|reverse| reverse.node_count() != n) {
            return Err(ExecutionError::InvalidUpstreamResult.into());
        }
        if n != 0 {
            for _ in 0..iterations {
                control()?;
                let mut fallback_source = 0;
                while let Some((source, notify)) = self.next_row(&mut fallback_source) {
                    control()?;
                    let (mut best, mut best_count) = self.begin_row(source)?;
                    self.votes(
                        graph.open(source)?,
                        &mut best,
                        &mut best_count,
                        notify,
                        control,
                    )?;
                    if let Some(reverse) = reverse {
                        self.votes(
                            reverse.open(source)?,
                            &mut best,
                            &mut best_count,
                            notify,
                            control,
                        )?;
                    }
                    self.finish_row(source, best)?;
                }
                self.finish_round()?;
                if let Some(frontier) = &mut self.frontier {
                    while !frontier.changed.touched.is_empty() {
                        control()?;
                        frontier.changed.clear_one();
                    }
                    if frontier.queue.is_empty() {
                        break;
                    }
                }
            }
        }
        control()?;
        Ok(self.finish())
    }
}

fn admit_call(
    call: &FnxCallSpec,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    admission: &ResultAdmission,
) -> Result<(usize, usize)> {
    let FnxAlgorithm::Cdlp { max_iter } = call.algorithm() else {
        return Err(super::Error::UnsupportedAlgorithm(call.algorithm()));
    };
    let n = graph.node_count();
    admission.rows(n)?;
    admit("iterations", max_iter, limits.max_iterations)?;
    let estimated = work(
        n,
        graph.scan_incidence_bound(),
        graph.adjacency_entry_count(),
        graph.spec().directedness,
        max_iter,
        call.outputs().len(),
    )?;
    admit("estimated work", estimated, limits.max_estimated_work)?;
    Ok((max_iter, estimated))
}

pub(super) fn execute(
    call: &FnxCallSpec,
    cx: &Control<'_>,
    graph: &SealedGraphView,
    limits: FnxExecutionLimits,
    memory: FnxMemoryLimits,
    admission: &ResultAdmission,
) -> Result<Finished> {
    let (max_iter, estimated) = admit_call(call, graph, limits, admission)?;
    let n = graph.node_count();
    let mut control = || checkpoint(cx);
    let rows = SealedRows { cx, graph };
    let mut state = Labels::new(n, max_iter, memory.max_kernel_workspace_bytes)?;
    state.prepare(&rows, max_iter, &mut control)?;
    let reverse =
        if n != 0 && max_iter != 0 && graph.spec().directedness != Directedness::Undirected {
            let remaining = memory
                .max_kernel_workspace_bytes
                .checked_sub(state.peak)
                .ok_or(ExecutionError::SizeOverflow)?;
            let reverse = graph.reverse_projection_controlled(cx, remaining)?;
            state.peak = add(state.peak, reverse.charged_workspace_bytes())?;
            admit(
                "kernel workspace bytes",
                state.peak,
                memory.max_kernel_workspace_bytes,
            )?;
            Some(reverse)
        } else {
            None
        };
    let reverse_rows = reverse.as_ref().map(|graph| SealedRows { cx, graph });
    // The mandatory reverse projection keeps its existing admission first;
    // optional frontier storage must never make a formerly admitted call fail.
    state.reserve_frontier(max_iter, memory.max_kernel_workspace_bytes)?;
    state.initialize_frontier(&mut control)?;
    let (output, peak) = state.iterate(&rows, reverse_rows.as_ref(), max_iter, &mut control)?;
    drop(reverse);
    finish(
        call,
        graph,
        output,
        "fgdb-prism/sealed-cdlp-synchronous-v2",
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
    let (max_iter, estimated) = admit_call(call, graph, limits, &admission)?;
    let n = graph.node_count();
    control.tick().await?;
    let mut state = Labels::new(n, max_iter, memory.max_kernel_workspace_bytes)?;

    // Zero updates still enforce the same loop-free graph law. Raw history
    // and excluded endpoints consume scheduling fuel inside the cursor even
    // when this scan produces no projected neighbor.
    for source in 0..n {
        control.tick().await?;
        let mut row = graph.neighbor_cursor_controlled(control.cx, source, None)?;
        while let Some((target, _)) = control.next(&mut row).await? {
            control.tick().await?;
            if target >= n {
                return Err(ExecutionError::InvalidUpstreamResult.into());
            }
            if target == source {
                return Err(ExecutionError::SelfLoopRefused.into());
            }
        }
    }
    for vertex in 0..n {
        control.tick().await?;
        state.labels.push(vertex);
    }
    if max_iter != 0 {
        for values in [&mut state.next, &mut state.counts, &mut state.generations] {
            for _ in 0..n {
                control.tick().await?;
                values.push(0);
            }
        }
    }
    let reverse =
        if n != 0 && max_iter != 0 && graph.spec().directedness != Directedness::Undirected {
            let remaining = memory
                .max_kernel_workspace_bytes
                .checked_sub(state.peak)
                .ok_or(ExecutionError::SizeOverflow)?;
            let reverse = control.reverse_projection(graph, remaining).await?;
            state.peak = add(state.peak, reverse.charged_workspace_bytes())?;
            admit(
                "kernel workspace bytes",
                state.peak,
                memory.max_kernel_workspace_bytes,
            )?;
            Some(reverse)
        } else {
            None
        };
    state.reserve_frontier(max_iter, memory.max_kernel_workspace_bytes)?;
    if let Some(frontier) = &mut state.frontier {
        for _ in 0..bitmap_words(n) {
            control.tick().await?;
            frontier.initialize_word();
        }
        for vertex in 0..n {
            control.tick().await?;
            frontier.initialize_vertex(vertex);
        }
    }
    if n != 0 {
        for _ in 0..max_iter {
            control.tick().await?;
            let mut fallback_source = 0;
            while let Some((source, notify)) = state.next_row(&mut fallback_source) {
                control.tick().await?;
                let (mut best, mut best_count) = state.begin_row(source)?;
                for face in std::iter::once(graph).chain(reverse.as_ref()) {
                    let mut row = face.neighbor_cursor_controlled(control.cx, source, None)?;
                    while let Some((neighbor, _)) = control.next(&mut row).await? {
                        control.tick().await?;
                        state.vote(neighbor, &mut best, &mut best_count, notify)?;
                    }
                }
                state.finish_row(source, best)?;
            }
            state.finish_round()?;
            if let Some(frontier) = &mut state.frontier {
                while !frontier.changed.touched.is_empty() {
                    control.tick().await?;
                    frontier.changed.clear_one();
                }
                if frontier.queue.is_empty() {
                    break;
                }
            }
        }
    }
    control.tick().await?;
    let (output, peak) = state.finish();
    drop(reverse);
    let mut encoded = super::EncodedRows::new(call, output.row_count, &admission)?;
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
        "fgdb-prism/sealed-cdlp-cooperative-v2",
        estimated,
        peak,
        &mut || checkpoint(control.cx),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
        SealedProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
    };
    use fgdb_types::{CommitSeq, EId, ObjectId, VId};
    use fnx_algorithms::GraphView;
    use std::cell::Cell;
    use std::convert::Infallible;

    struct Fixture {
        rows: Vec<Vec<usize>>,
        source_steps: Cell<usize>,
        opens: Vec<Cell<usize>>,
        fail_at: usize,
    }

    impl Fixture {
        fn new(rows: Vec<Vec<usize>>) -> Self {
            let opens = (0..rows.len()).map(|_| Cell::new(0)).collect();
            Self {
                rows,
                source_steps: Cell::new(0),
                opens,
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
        values: std::slice::Iter<'a, usize>,
    }

    impl Cursor for Row<'_> {
        fn next(&mut self) -> Result<Option<(usize, f64)>> {
            self.graph.step()?;
            Ok(self.values.next().map(|&target| (target, -17.0)))
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

        fn open(&self, source: usize) -> Result<Row<'_>> {
            self.step()?;
            let values = self
                .rows
                .get(source)
                .ok_or(ExecutionError::InvalidUpstreamResult)?;
            self.opens[source].set(self.opens[source].get() + 1);
            Ok(Row {
                graph: self,
                values: values.iter(),
            })
        }
    }

    fn run(
        graph: &Fixture,
        reverse: Option<&Fixture>,
        iterations: usize,
        limit: usize,
        control: &mut impl FnMut() -> Result<()>,
    ) -> Result<(KernelOutput, usize)> {
        let mut state = Labels::new(graph.node_count(), iterations, limit)?;
        state.prepare(graph, iterations, control)?;
        state.reserve_frontier(iterations, limit)?;
        state.initialize_frontier(control)?;
        state.iterate(graph, reverse, iterations, control)
    }

    fn component_labels(output: KernelOutput) -> Vec<usize> {
        let KernelValues::Components(labels) = output.values else {
            panic!("component labels")
        };
        labels
    }

    // Deliberately uses a fresh map per vertex and stable identities, not the
    // production ordinal histogram or its epoch state.
    fn reference(graph: &SnapshotGraphView, iterations: usize) -> Vec<VId> {
        let mut labels = graph.vertex_ids().to_vec();
        for _ in 0..iterations {
            let mut next = labels.clone();
            for (vertex, label) in next.iter_mut().enumerate() {
                let mut votes = std::collections::BTreeMap::<VId, usize>::new();
                for row in [
                    graph.neighbors_indices(vertex).unwrap(),
                    if graph.is_directed() {
                        graph.in_neighbors_indices(vertex).unwrap()
                    } else {
                        &[]
                    },
                ] {
                    for &neighbor in row {
                        *votes.entry(labels[neighbor]).or_default() += 1;
                    }
                }
                if let Some((&winner, _)) = votes
                    .iter()
                    .max_by(|(a, x), (b, y)| x.cmp(y).then_with(|| b.cmp(a)))
                {
                    *label = winner;
                }
            }
            labels = next;
        }
        labels
    }

    #[test]
    fn every_small_directed_reversed_and_undirected_topology_matches_decoded_and_identity_oracle() {
        let ids = [VId(0), VId(1 << 96), VId(u128::MAX)];
        let mut cases = 0;
        for n in 0..=3 {
            let pairs: Vec<_> = (0..n)
                .flat_map(|s| (0..n).filter(move |&t| t != s).map(move |t| (s, t)))
                .collect();
            for mask in 0usize..(1 << pairs.len()) {
                let edges: Vec<_> = pairs
                    .iter()
                    .enumerate()
                    .filter(|(bit, _)| mask & (1 << bit) != 0)
                    .map(|(bit, &(s, t))| ProjectionEdge {
                        eid: EId(bit as u128),
                        source: ids[s],
                        target: ids[t],
                        weight: -3.0,
                    })
                    .collect();
                for direction in [
                    Directedness::Directed,
                    Directedness::Reversed,
                    Directedness::Undirected,
                ] {
                    let graph = SnapshotGraphView::build(
                        SnapshotBinding {
                            root: ObjectId([5; 32]),
                            as_of: CommitSeq(7),
                        },
                        &ids[..n],
                        &edges,
                        ProjectionSpec {
                            directedness: direction,
                            parallel_edges: ParallelEdgePolicy::CollapseUnit,
                            self_loops: SelfLoopPolicy::Reject,
                        },
                        ProjectionLimits {
                            max_vertices: n,
                            max_input_edges: edges.len(),
                            max_adjacency_entries: 2 * edges.len(),
                            max_workspace_bytes: 1 << 20,
                        },
                    )
                    .unwrap();
                    let outgoing = Fixture::new(
                        (0..n)
                            .map(|v| graph.neighbors_indices(v).unwrap().to_vec())
                            .collect(),
                    );
                    let incoming = Fixture::new(
                        (0..n)
                            .map(|v| graph.in_neighbors_indices(v).unwrap().to_vec())
                            .collect(),
                    );
                    for iterations in [0, 1, 2, 5] {
                        let mut ticks = 0;
                        let (actual, peak) = run(
                            &outgoing,
                            graph.is_directed().then_some(&incoming),
                            iterations,
                            workspace(n, iterations).unwrap(),
                            &mut || {
                                ticks += 1;
                                Ok(())
                            },
                        )
                        .unwrap();
                        let (expected, _) = crate::traversal::cdlp(&graph, iterations, &mut || {
                            Ok::<(), Infallible>(())
                        })
                        .unwrap();
                        assert_eq!(actual.witness.nodes_touched, expected.witness.nodes_touched);
                        assert_eq!(actual.witness.edges_scanned, expected.witness.edges_scanned);
                        let labels = component_labels(actual);
                        assert_eq!(
                            labels,
                            component_labels(expected),
                            "n={n} mask={mask} direction={direction:?} k={iterations}"
                        );
                        assert_eq!(
                            labels.iter().map(|&v| ids[v]).collect::<Vec<_>>(),
                            reference(&graph, iterations)
                        );
                        assert_eq!(peak, workspace(n, iterations).unwrap());
                        // The same fixture also exercises the optional queue,
                        // including exact iteration parity on oscillators.
                        let mut active_ticks = 0;
                        let (active, active_peak) = run(
                            &outgoing,
                            graph.is_directed().then_some(&incoming),
                            iterations,
                            usize::MAX,
                            &mut || {
                                active_ticks += 1;
                                Ok(())
                            },
                        )
                        .unwrap();
                        assert_eq!(component_labels(active), labels);
                        assert_eq!(
                            active_peak,
                            workspace(n, iterations).unwrap()
                                + if n != 0 && iterations >= 2 {
                                    frontier_workspace(n).unwrap()
                                } else {
                                    0
                                }
                        );
                        assert!(
                            ticks
                                <= work(
                                    n,
                                    edges.len(),
                                    outgoing.rows.iter().map(Vec::len).sum(),
                                    direction,
                                    iterations,
                                    2,
                                )
                                .unwrap()
                        );
                        assert!(
                            active_ticks
                                <= work(
                                    n,
                                    edges.len(),
                                    outgoing.rows.iter().map(Vec::len).sum(),
                                    direction,
                                    iterations,
                                    2,
                                )
                                .unwrap()
                        );
                        cases += 1;
                    }
                }
            }
        }
        assert_eq!(cases, 840);
    }

    #[test]
    fn settled_cliques_leave_only_oscillating_rows_active_without_extra_source_scans() {
        for d in [3, 17, 64, 129] {
            let n = d + 3;
            for iterations in [3, 4, 17, 100] {
                let mut rows: Vec<_> = (0..d)
                    .map(|source| (0..d).filter(|&target| target != source).collect())
                    .collect();
                rows.extend([vec![d + 1], vec![d], vec![]]);
                let graph = Fixture::new(rows);
                let mut ticks = 0;
                let (output, peak) = run(&graph, None, iterations, usize::MAX, &mut || {
                    ticks += 1;
                    Ok(())
                })
                .unwrap();
                assert_eq!(
                    output.witness.algorithm,
                    "cdlp_synchronous_minimum_vid_active_cursor"
                );
                assert_eq!(output.witness.nodes_touched, 3 * d + 2 * iterations + 1);
                assert_eq!(output.witness.queue_peak, n);
                assert_eq!(
                    output.witness.edges_scanned,
                    3 * d * (d - 1) + 2 * iterations
                );
                let visits = output.witness.edges_scanned;
                let opened = output.witness.nodes_touched + n;
                assert_eq!(graph.opens.iter().map(Cell::get).sum::<usize>(), opened);
                assert!(graph.opens[..d].iter().all(|opens| opens.get() == 4));
                assert_eq!(graph.opens[d].get(), iterations + 1);
                assert_eq!(graph.opens[d + 1].get(), iterations + 1);
                assert_eq!(graph.opens[d + 2].get(), 2);
                // Every open has its real EOF pull. Planning visits every arc;
                // notifications add no row opens or pulls to the vote scans.
                let arcs = d * (d - 1) + 2;
                assert_eq!(graph.source_steps.get(), 2 * opened + arcs + visits);
                let labels = component_labels(output);
                assert!(labels[..d].iter().all(|&label| label == 0));
                assert_eq!(labels[d], d + iterations % 2);
                assert_eq!(labels[d + 1], d + 1 - iterations % 2);
                assert_eq!(labels[d + 2], d + 2);
                assert_eq!(
                    peak,
                    workspace(n, iterations).unwrap() + frontier_workspace(n).unwrap()
                );
                assert!(
                    ticks
                        <= work(n, arcs / 2, arcs, Directedness::Undirected, iterations, 2)
                            .unwrap()
                );
            }
        }
    }

    #[test]
    fn sparse_dense_and_relabelled_frontiers_match_independent_full_round_votes() {
        let mut random = 0x6364_6c70_6163_7469u64;
        for n in [9, 17, 65] {
            for density in [1, 5] {
                let mut original = vec![Vec::new(); n];
                for (source, row) in original.iter_mut().enumerate() {
                    for target in 0..n {
                        random = random
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1);
                        if source != target && (random >> 32) % 8 < density {
                            row.push(target);
                        }
                    }
                }
                for relabel in [false, true] {
                    let ordinal = |vertex| if relabel { n - 1 - vertex } else { vertex };
                    let mut outgoing = vec![Vec::new(); n];
                    let mut incoming = vec![Vec::new(); n];
                    for (source, row) in original.iter().enumerate() {
                        for &target in row.iter().rev() {
                            outgoing[ordinal(source)].push(ordinal(target));
                            incoming[ordinal(target)].push(ordinal(source));
                        }
                    }
                    for rows in [&mut outgoing, &mut incoming] {
                        for row in rows {
                            row.sort_unstable();
                        }
                    }
                    let outgoing = Fixture::new(outgoing);
                    let incoming = Fixture::new(incoming);
                    for iterations in [0, 1, 2, 3, 8, 17] {
                        let mut expected: Vec<_> = (0..n).collect();
                        for _ in 0..iterations {
                            expected = (0..n)
                                .map(|source| {
                                    let mut votes =
                                        std::collections::BTreeMap::<usize, usize>::new();
                                    for &neighbor in
                                        outgoing.rows[source].iter().chain(&incoming.rows[source])
                                    {
                                        *votes.entry(expected[neighbor]).or_default() += 1;
                                    }
                                    votes
                                        .into_iter()
                                        .min_by_key(|&(label, count)| {
                                            (std::cmp::Reverse(count), label)
                                        })
                                        .map_or(expected[source], |(label, _)| label)
                                })
                                .collect();
                        }
                        let (actual, _) = run(
                            &outgoing,
                            Some(&incoming),
                            iterations,
                            usize::MAX,
                            &mut || Ok(()),
                        )
                        .unwrap();
                        assert!(actual.witness.nodes_touched <= n * iterations);
                        assert!(
                            actual.witness.edges_scanned
                                <= 2 * outgoing.rows.iter().map(Vec::len).sum::<usize>()
                                    * iterations
                        );
                        assert_eq!(
                            component_labels(actual),
                            expected,
                            "n={n}, density={density}, relabel={relabel}, iterations={iterations}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn optional_frontier_preserves_the_exact_full_sweep_memory_fallback() {
        let graph = Fixture::new(vec![vec![1, 2], vec![0, 2], vec![0, 1], vec![]]);
        let n = graph.node_count();
        let iterations = 20;
        let baseline = workspace(n, iterations).unwrap();
        let active = baseline + frontier_workspace(n).unwrap();
        for limit in [baseline, active - 1, active] {
            let (actual, peak) = run(&graph, None, iterations, limit, &mut || Ok(())).unwrap();
            let frontier = limit == active;
            assert_eq!(peak, if frontier { active } else { baseline });
            assert_eq!(
                actual.witness.nodes_touched,
                if frontier { 10 } else { n * iterations }
            );
            assert_eq!(actual.witness.queue_peak, if frontier { n } else { 0 });
            assert_eq!(component_labels(actual), [0, 0, 0, 3]);
        }
        // Vec cannot represent a byte allocation above isize::MAX. This
        // deterministically exercises allocation refusal without exhausting
        // the test process; optional storage must leave the sweep admissible.
        let mut live = 0;
        assert!(
            optional_reserved::<u8>(usize::MAX, &mut live, usize::MAX)
                .unwrap()
                .is_none()
        );
        assert_eq!(live, 0);
        assert!(matches!(
            optional_reserved::<usize>(usize::MAX, &mut live, usize::MAX),
            Err(super::super::Error::Execution(ExecutionError::SizeOverflow))
        ));
        assert_eq!(live, 0);
        assert!(matches!(
            run(&graph, None, iterations, baseline - 1, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::LimitExceeded {
                    resource: "kernel workspace bytes",
                    ..
                }
            ))
        ));

        // The mandatory validation still reaches a late loop before any
        // frontier can omit a settled or isolated population.
        let loops = Fixture::new(vec![vec![1, 2], vec![0, 2], vec![0, 1], vec![3]]);
        assert!(matches!(
            run(&loops, None, iterations, usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::SelfLoopRefused
            ))
        ));
        assert_eq!(loops.opens[3].get(), 1);
    }

    #[test]
    fn optional_partial_allocation_keeps_its_peak_and_over_capacity_refuses() {
        let graph = Fixture::new(vec![vec![1, 2], vec![0, 2], vec![0, 1], vec![]]);
        let n = graph.node_count();
        let iterations = 20;
        let baseline = workspace(n, iterations).unwrap();
        let extra = frontier_workspace(n).unwrap();
        let limit = baseline + extra;
        let mut state = Labels::new(n, iterations, limit).unwrap();
        state.prepare(&graph, iterations, &mut || Ok(())).unwrap();

        // An allocator may return more capacity than requested. Fill the
        // optional allowance with the queue alone, so the next bitmap cannot
        // fit. The queue is dropped, but its observed peak must survive into
        // the successfully completed full-sweep result.
        assert!(
            ActiveRows::reserve_with_queue(n, &mut state.peak, limit, |requested| {
                assert_eq!(requested, n);
                reserve(extra / size_of::<usize>())
            })
            .unwrap()
            .is_none()
        );
        let (actual, peak) = state
            .iterate(&graph, None, iterations, &mut || Ok(()))
            .unwrap();
        assert_eq!(peak, limit);
        assert_eq!(actual.witness.nodes_touched, n * iterations);
        assert_eq!(actual.witness.queue_peak, 0);
        assert_eq!(component_labels(actual), [0, 0, 0, 3]);

        // Returning even one more integer of capacity exceeds the real
        // admitted peak. A successful fallback would hide that violation.
        let mut peak = baseline;
        assert!(matches!(
            ActiveRows::reserve_with_queue(n, &mut peak, limit, |_| {
                reserve(extra / size_of::<usize>() + 1)
            }),
            Err(super::super::Error::Execution(ExecutionError::LimitExceeded {
                resource: "kernel workspace bytes",
                requested,
                limit: actual_limit,
            })) if requested == limit + size_of::<usize>() && actual_limit == limit
        ));
    }

    #[test]
    fn reciprocal_votes_are_not_collapsed_into_a_neighbor_union() {
        // Vertex 0 sees label 2 twice and label 1 once. An undirected union
        // would choose the smaller label 1, which is the wrong directed answer.
        let graph = Fixture::new(vec![vec![1, 2], vec![], vec![0], vec![]]);
        let reverse = Fixture::new(vec![vec![2], vec![0], vec![0], vec![]]);
        let (output, _) = run(&graph, Some(&reverse), 1, usize::MAX, &mut || Ok(())).unwrap();
        assert_eq!(component_labels(output), vec![2, 0, 0, 3]);
        let (output, _) = run(&graph, Some(&reverse), 2, usize::MAX, &mut || Ok(())).unwrap();
        assert_eq!(component_labels(output), vec![0, 2, 2, 3]);
    }

    #[test]
    fn official_graphalytics_examples_match_published_two_iteration_labels() {
        // Independent published inputs/results, pinned at:
        // https://github.com/ldbc/ldbc_graphalytics/tree/7b8bde76cf7aab5e90b25ecd4b38829e2f98b292
        // graphalytics-validation/src/main/resources/validation-graphs/example/
        // example-directed.e: 5689a64fb63bfb8fdcf9d32d6fb7b902acecaa13
        // example-directed.v: f00c965d8307308469e537302baa73048488f162
        // example-directed-CDLP: 3b5b10516c9bfc5e6981b4b1a80dedd1ab627a85
        // example-undirected.e: cbe8180fc77ed7629d690e2e019f20f90c5b23b8
        // example-undirected.v: 4fd875adfd59ece89ca3d725c0714e945ce446cc
        // example-undirected-CDLP: e14119f8a81a13ad163ca9143567bfce454727c0
        // config-template/graphs/example-{directed,undirected}.properties
        // set cdlp.max-iterations=2 (blobs ececa9b7711fe426ce88b116752e1ef9ab5437f0
        // and 67d41b99669e69d587fffc1542eb35dc469307b6). Weights are irrelevant.
        fn check(vertices: &[u128], edges: &[(u128, u128)], expected: &[u128], directed: bool) {
            let mut outgoing = vec![Vec::new(); vertices.len()];
            let mut incoming = vec![Vec::new(); vertices.len()];
            for &(source, target) in edges {
                let source = vertices.binary_search(&source).unwrap();
                let target = vertices.binary_search(&target).unwrap();
                outgoing[source].push(target);
                incoming[target].push(source);
                if !directed {
                    outgoing[target].push(source);
                }
            }
            for rows in [&mut outgoing, &mut incoming] {
                for row in rows {
                    row.sort_unstable();
                }
            }
            let outgoing = Fixture::new(outgoing);
            let incoming = Fixture::new(incoming);
            let (actual, _) = run(
                &outgoing,
                directed.then_some(&incoming),
                2,
                workspace(vertices.len(), 2).unwrap(),
                &mut || Ok(()),
            )
            .unwrap();
            assert_eq!(
                component_labels(actual)
                    .into_iter()
                    .map(|ordinal| vertices[ordinal])
                    .collect::<Vec<_>>(),
                expected
            );
        }
        check(
            &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            &[
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
            ],
            &[1, 2, 3, 4, 1, 1, 2, 3, 2, 1],
            true,
        );
        check(
            &[2, 3, 4, 5, 6, 7, 8, 9, 10],
            &[
                (2, 3),
                (2, 4),
                (3, 4),
                (3, 5),
                (3, 8),
                (5, 6),
                (5, 8),
                (6, 7),
                (6, 8),
                (6, 9),
                (6, 10),
                (7, 9),
            ],
            &[2, 3, 2, 2, 6, 5, 2, 5, 5],
            false,
        );
    }

    #[test]
    fn every_scalar_checkpoint_and_both_source_faces_can_refuse_without_a_result() {
        let rows = vec![vec![1, 2], vec![2], vec![0], vec![]];
        let reverse = vec![vec![2], vec![0], vec![0, 1], vec![]];
        let outgoing = Fixture::new(rows.clone());
        let incoming = Fixture::new(reverse.clone());
        let mut ticks = 0;
        let (expected, _) = run(&outgoing, Some(&incoming), 3, usize::MAX, &mut || {
            ticks += 1;
            Ok(())
        })
        .unwrap();
        let expected = component_labels(expected);
        let source_steps = [outgoing.source_steps.get(), incoming.source_steps.get()];
        assert!(ticks > 20);
        assert!(source_steps.iter().all(|&steps| steps > 10));
        for stop in 1..=ticks {
            let mut seen = 0;
            assert!(
                run(&outgoing, Some(&incoming), 3, usize::MAX, &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(super::super::Error::Source(
                            SealedProjectionError::UnknownOrdinal(usize::MAX),
                        ))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert_eq!(seen, stop);
        }
        for (face, steps) in source_steps.into_iter().enumerate() {
            for stop in 1..=steps {
                let mut outgoing = Fixture::new(rows.clone());
                let mut incoming = Fixture::new(reverse.clone());
                if face == 0 {
                    outgoing.fail_at = stop;
                } else {
                    incoming.fail_at = stop;
                }
                assert!(run(&outgoing, Some(&incoming), 3, usize::MAX, &mut || Ok(())).is_err());
                let stopped = if face == 0 { &outgoing } else { &incoming };
                assert_eq!(stopped.source_steps.get(), stop);
            }
        }
        assert_eq!(
            component_labels(
                run(&outgoing, Some(&incoming), 3, usize::MAX, &mut || Ok(()))
                    .unwrap()
                    .0
            ),
            expected
        );
    }

    #[test]
    fn zero_rounds_loops_exact_workspace_and_checked_arithmetic_are_enforced() {
        let graph = Fixture::new(vec![vec![1], vec![], vec![]]);
        let reverse = Fixture::new(vec![vec![], vec![0], vec![]]);
        for iterations in [0, 2] {
            let bytes = workspace(3, iterations).unwrap();
            let (output, peak) =
                run(&graph, Some(&reverse), iterations, bytes, &mut || Ok(())).unwrap();
            assert_eq!(peak, bytes);
            if iterations == 0 {
                assert_eq!(component_labels(output), vec![0, 1, 2]);
            }
            assert!(matches!(
                run(
                    &graph,
                    Some(&reverse),
                    iterations,
                    bytes - 1,
                    &mut || Ok(())
                ),
                Err(super::super::Error::Execution(
                    ExecutionError::LimitExceeded {
                        resource: "kernel workspace bytes",
                        ..
                    }
                ))
            ));
            let loops = Fixture::new(vec![vec![0]]);
            assert!(matches!(
                run(&loops, None, iterations, usize::MAX, &mut || Ok(())),
                Err(super::super::Error::Execution(
                    ExecutionError::SelfLoopRefused
                ))
            ));
        }
        let empty = Fixture::new(vec![]);
        let (output, peak) = run(&empty, None, usize::MAX, 0, &mut || Ok(())).unwrap();
        assert!(component_labels(output).is_empty());
        assert_eq!(peak, 0);
        let invalid = Fixture::new(vec![vec![2], vec![]]);
        assert!(matches!(
            run(&invalid, None, 1, usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Execution(
                ExecutionError::InvalidUpstreamResult
            ))
        ));
        assert!(workspace(usize::MAX, 1).is_err());
        assert!(work(2, 2, 2, Directedness::Directed, usize::MAX, 2).is_err());
        assert!(work(2, usize::MAX, 2, Directedness::Directed, 1, 2).is_err());
        let mut state = Labels::new(3, 1, usize::MAX).unwrap();
        state.prepare(&graph, 1, &mut || Ok(())).unwrap();
        state.generation = usize::MAX;
        assert!(matches!(
            state.iterate(&graph, Some(&reverse), 1, &mut || Ok(())),
            Err(super::super::Error::Execution(ExecutionError::SizeOverflow))
        ));
    }

    #[test]
    fn hub_and_isolates_use_fixed_linear_label_workspace() {
        let leaves = 8191;
        let n = leaves + 2;
        let mut rows = vec![Vec::new(); n];
        let mut reverse = vec![Vec::new(); n];
        rows[0].extend(1..=leaves);
        for neighbors in reverse.iter_mut().take(leaves + 1).skip(1) {
            neighbors.push(0);
        }
        let outgoing = Fixture::new(rows);
        let incoming = Fixture::new(reverse);
        let (output, peak) = run(
            &outgoing,
            Some(&incoming),
            2,
            workspace(n, 2).unwrap(),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(peak, 4 * n * size_of::<usize>());
        assert_eq!(output.witness.nodes_touched, 2 * n);
        assert_eq!(output.witness.edges_scanned, 4 * leaves);
        let labels = component_labels(output);
        assert_eq!(labels[0], 0);
        assert!(labels[1..=leaves].iter().all(|&label| label == 1));
        assert_eq!(labels[n - 1], n - 1);
    }
}
