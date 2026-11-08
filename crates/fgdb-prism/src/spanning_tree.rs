//! Deterministic minimum spanning forests without a retained edge list.
//!
//! Prim's cut rule uses the strict edge preference (weight, smaller VId,
//! larger VId). This is the same unique forest selected by pinned FNX Kruskal
//! on canonical projection rows. Signed zeros compare equal, as in FNX. An
//! indexed heap retains one candidate per vertex; only the accepted forest
//! edges are sorted, by incremental heapsort, into Kruskal's delivery order.
//! Every state transition does bounded scalar work and owns no adjacency.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{ComplexityWitness, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView};
use std::cmp::Ordering;
use std::mem::size_of;

#[derive(Clone, Copy, Debug)]
pub(crate) struct WeightedEndpoint {
    pub(crate) vertex: usize,
    pub(crate) neighbor: usize,
    pub(crate) weight: f64,
}

impl WeightedEndpoint {
    fn new(a: usize, b: usize, weight: f64) -> Self {
        Self {
            vertex: a.min(b),
            neighbor: a.max(b),
            weight,
        }
    }

    pub(crate) fn compare(self, other: Self) -> Ordering {
        // Inputs are finite before entering the state. In particular, do not
        // use total_cmp: FNX treats -0.0 and +0.0 as an equal-weight tie.
        if self.weight < other.weight {
            Ordering::Less
        } else if self.weight > other.weight {
            Ordering::Greater
        } else {
            (self.vertex, self.neighbor).cmp(&(other.vertex, other.neighbor))
        }
    }
}

pub(crate) fn workspace<C>(n: usize) -> Result<usize, FnxExecutionError<C>> {
    n.checked_mul(3 * size_of::<usize>() + size_of::<f64>() + size_of::<bool>())
        .and_then(|bytes| {
            n.saturating_sub(1)
                .checked_mul(size_of::<WeightedEndpoint>())
                .and_then(|output| bytes.checked_add(output))
        })
        .ok_or(FnxExecutionError::SizeOverflow)
}

pub(crate) fn estimated_work<C>(
    n: usize,
    arcs: usize,
    pass: usize,
    columns: usize,
) -> Result<usize, FnxExecutionError<C>> {
    let height = (usize::BITS - n.leading_zeros()) as usize;
    let row_factor = height
        .checked_add(5)
        .ok_or(FnxExecutionError::SizeOverflow)?;
    let vertex_factor = height
        .checked_mul(4)
        .and_then(|h| h.checked_add(columns))
        .and_then(|h| h.checked_add(20))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    arcs.checked_mul(row_factor)
        .and_then(|work| {
            n.checked_mul(vertex_factor)
                .and_then(|v| work.checked_add(v))
        })
        .and_then(|work| work.checked_add(pass))
        .and_then(|work| work.checked_add(64))
        .ok_or(FnxExecutionError::SizeOverflow)
}

fn reserved<T, C>(
    n: usize,
    live: &mut usize,
    limit: usize,
) -> Result<Vec<T>, FnxExecutionError<C>> {
    let bytes = n
        .checked_mul(size_of::<T>())
        .and_then(|bytes| live.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", bytes, limit)?;
    let values = reserve::<T, C>(n)?;
    let actual = values
        .capacity()
        .checked_mul(size_of::<T>())
        .and_then(|bytes| live.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", actual, limit)?;
    *live = actual;
    Ok(values)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Scalar,
    Open(usize),
    Row(usize),
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortResume {
    Build(usize),
    Take(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Initialize,
    Choose,
    Open(usize),
    Row(usize),
    Rise {
        at: usize,
        row: usize,
    },
    Sink {
        at: usize,
        vertex: usize,
    },
    Sort(SortResume),
    SortSift {
        at: usize,
        end: usize,
        resume: SortResume,
    },
    Complete,
}

pub(crate) struct Forest {
    weights: Vec<f64>,
    parents: Vec<usize>,
    positions: Vec<usize>,
    selected: Vec<bool>,
    heap: Vec<usize>,
    edges: Vec<WeightedEndpoint>,
    nodes: usize,
    next_root: usize,
    row_limit: usize,
    peak: usize,
    phase: Phase,
    witness: ComplexityWitness,
}

impl Forest {
    pub(crate) fn new<C>(
        nodes: usize,
        limit: usize,
        row_limit: usize,
    ) -> Result<Self, FnxExecutionError<C>> {
        admit("kernel workspace bytes", workspace::<C>(nodes)?, limit)?;
        let mut peak = 0;
        let weights = reserved(nodes, &mut peak, limit)?;
        let parents = reserved(nodes, &mut peak, limit)?;
        let positions = reserved(nodes, &mut peak, limit)?;
        let selected = reserved(nodes, &mut peak, limit)?;
        let heap = reserved(nodes, &mut peak, limit)?;
        let edges = reserved(nodes.saturating_sub(1), &mut peak, limit)?;
        Ok(Self {
            weights,
            parents,
            positions,
            selected,
            heap,
            edges,
            nodes,
            next_root: 0,
            row_limit,
            peak,
            phase: Phase::Initialize,
            witness: ComplexityWitness {
                algorithm: "minimum_spanning_forest_indexed_prim".to_owned(),
                complexity_claim:
                    "O((|V|+|E|) log(1+|V|)); O(|V|) workspace; canonical Kruskal edge order"
                        .to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
        })
    }

    pub(crate) fn workspace_bytes(&self) -> usize {
        self.peak
    }
    pub(crate) fn row_count(&self) -> usize {
        self.edges.len()
    }

    fn candidate(&self, vertex: usize) -> WeightedEndpoint {
        WeightedEndpoint::new(vertex, self.parents[vertex], self.weights[vertex])
    }

    fn less(&self, a: usize, b: usize) -> bool {
        self.candidate(self.heap[a])
            .compare(self.candidate(self.heap[b]))
            == Ordering::Less
    }

    fn swap_heap(&mut self, a: usize, b: usize) {
        self.heap.swap(a, b);
        self.positions[self.heap[a]] = a;
        self.positions[self.heap[b]] = b;
    }

    pub(crate) fn step<C>(&mut self) -> Result<Step, FnxExecutionError<C>> {
        match self.phase {
            Phase::Initialize => {
                if self.parents.len() == self.nodes {
                    self.phase = Phase::Choose;
                } else {
                    self.weights.push(0.0);
                    self.parents.push(usize::MAX);
                    self.positions.push(usize::MAX);
                    self.selected.push(false);
                }
            }
            Phase::Choose => {
                if let Some(&vertex) = self.heap.first() {
                    let rows = self
                        .edges
                        .len()
                        .checked_add(1)
                        .ok_or(FnxExecutionError::SizeOverflow)?;
                    admit("result rows", rows, self.row_limit)?;
                    self.edges.push(self.candidate(vertex));
                    self.selected[vertex] = true;
                    self.witness.nodes_touched += 1;
                    self.positions[vertex] = usize::MAX;
                    let tail = self
                        .heap
                        .pop()
                        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                    if self.heap.is_empty() {
                        self.phase = Phase::Open(vertex);
                    } else {
                        self.heap[0] = tail;
                        self.positions[tail] = 0;
                        self.phase = Phase::Sink { at: 0, vertex };
                    }
                } else if self.next_root < self.nodes {
                    let vertex = self.next_root;
                    self.next_root += 1;
                    if !self.selected[vertex] {
                        self.selected[vertex] = true;
                        self.witness.nodes_touched += 1;
                        self.phase = Phase::Open(vertex);
                    }
                } else {
                    self.phase = Phase::Sort(SortResume::Build(self.edges.len() / 2));
                }
            }
            Phase::Open(vertex) => {
                self.phase = Phase::Row(vertex);
                return Ok(Step::Open(vertex));
            }
            Phase::Row(vertex) => return Ok(Step::Row(vertex)),
            Phase::Rise { at, row } => {
                if at != 0 && self.less(at, (at - 1) / 2) {
                    let parent = (at - 1) / 2;
                    self.swap_heap(at, parent);
                    self.phase = Phase::Rise { at: parent, row };
                } else {
                    self.phase = Phase::Row(row);
                }
            }
            Phase::Sink { at, vertex } => {
                let child = at
                    .checked_mul(2)
                    .and_then(|v| v.checked_add(1))
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                if child >= self.heap.len() {
                    self.phase = Phase::Open(vertex);
                } else {
                    let child = if child + 1 < self.heap.len() && self.less(child + 1, child) {
                        child + 1
                    } else {
                        child
                    };
                    if self.less(child, at) {
                        self.swap_heap(at, child);
                        self.phase = Phase::Sink { at: child, vertex };
                    } else {
                        self.phase = Phase::Open(vertex);
                    }
                }
            }
            Phase::Sort(SortResume::Build(start)) => {
                self.phase = if start == 0 {
                    Phase::Sort(SortResume::Take(self.edges.len()))
                } else {
                    Phase::SortSift {
                        at: start - 1,
                        end: self.edges.len(),
                        resume: SortResume::Build(start - 1),
                    }
                };
            }
            Phase::Sort(SortResume::Take(end)) => {
                if end <= 1 {
                    self.phase = Phase::Complete;
                } else {
                    self.edges.swap(0, end - 1);
                    self.phase = Phase::SortSift {
                        at: 0,
                        end: end - 1,
                        resume: SortResume::Take(end - 1),
                    };
                }
            }
            Phase::SortSift { at, end, resume } => {
                let child = at
                    .checked_mul(2)
                    .and_then(|v| v.checked_add(1))
                    .ok_or(FnxExecutionError::SizeOverflow)?;
                if child >= end {
                    self.phase = Phase::Sort(resume);
                } else {
                    let child = if child + 1 < end
                        && self.edges[child + 1].compare(self.edges[child]) == Ordering::Greater
                    {
                        child + 1
                    } else {
                        child
                    };
                    if self.edges[child].compare(self.edges[at]) == Ordering::Greater {
                        self.edges.swap(child, at);
                        self.phase = Phase::SortSift {
                            at: child,
                            end,
                            resume,
                        };
                    } else {
                        self.phase = Phase::Sort(resume);
                    }
                }
            }
            Phase::Complete => return Ok(Step::Complete),
        }
        Ok(Step::Scalar)
    }

    pub(crate) fn visit<C>(
        &mut self,
        neighbor: usize,
        weight: f64,
    ) -> Result<(), FnxExecutionError<C>> {
        let Phase::Row(vertex) = self.phase else {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        };
        if neighbor >= self.nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        if !weight.is_finite() {
            return Err(FnxExecutionError::InvalidNumericResult);
        }
        self.witness.edges_scanned = self
            .witness
            .edges_scanned
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        // Selected endpoints include this row's vertex, so retained loops are
        // ignored after numeric validation. Every source row is still drained.
        if self.selected[neighbor] {
            return Ok(());
        }
        let incoming = WeightedEndpoint::new(vertex, neighbor, weight);
        let absent = self.positions[neighbor] == usize::MAX;
        if absent || incoming.compare(self.candidate(neighbor)) == Ordering::Less {
            self.parents[neighbor] = vertex;
            self.weights[neighbor] = weight;
            if absent {
                self.positions[neighbor] = self.heap.len();
                self.heap.push(neighbor);
                self.witness.queue_peak = self.witness.queue_peak.max(self.heap.len());
            }
            self.phase = Phase::Rise {
                at: self.positions[neighbor],
                row: vertex,
            };
        }
        Ok(())
    }

    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        if !matches!(self.phase, Phase::Row(_)) {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        self.phase = Phase::Choose;
        Ok(())
    }

    pub(crate) fn finish<C>(self) -> Result<KernelOutput, FnxExecutionError<C>> {
        if self.phase != Phase::Complete
            || !self.heap.is_empty()
            || self.witness.nodes_touched != self.nodes
        {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        Ok(KernelOutput {
            row_count: self.edges.len(),
            values: KernelValues::WeightedEndpointPairs(self.edges),
            witness: self.witness,
        })
    }
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let mut state = Forest::new(graph.node_count(), usize::MAX, row_limit)?;
    let mut position = 0;
    loop {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        match state.step()? {
            Step::Scalar => {}
            Step::Open(_) => position = 0,
            Step::Row(vertex) => {
                let (neighbors, weights) = graph
                    .projected_row(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                if let Some(&neighbor) = neighbors.get(position) {
                    let weight = *weights
                        .get(position)
                        .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                    position += 1;
                    state.visit(neighbor, weight)?;
                } else {
                    state.end_row()?;
                }
            }
            Step::Complete => break,
        }
    }
    let peak = state.workspace_bytes();
    Ok((state.finish()?, peak))
}
