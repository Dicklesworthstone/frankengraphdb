//! Directed acyclicity and the pinned FNX DFS reverse-postorder ordering.
//!
//! A single iterative state machine serves decoded and compressed drivers.
//! It owns colors, parents and (only for sorting) an ordinal permutation;
//! adjacency stays with the admitted projection. Every scalar transition,
//! cursor pull and reversal step remains checkpointable. A detected cycle
//! does not hide a later source failure: the traversal completes before any
//! predicate or ordering is published.

use crate::execute::{KernelOutput, KernelValues, admit, reserve};
use crate::{ComplexityWitness, FnxExecutionError, FnxMeter, GraphView, SnapshotGraphView};
use std::mem::size_of;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Scalar,
    Open(usize),
    Row(usize),
    Complete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Initialize,
    Roots,
    Open,
    Row,
    Reverse(usize),
    Complete,
}

pub(crate) struct Dag {
    colors: Vec<u8>,
    parents: Vec<usize>,
    order: Vec<usize>,
    predicate: bool,
    cyclic: bool,
    nodes: usize,
    next_root: usize,
    current: usize,
    depth: usize,
    workspace: usize,
    phase: Phase,
    witness: ComplexityWitness,
}

fn reserved<T, C>(
    n: usize,
    workspace: &mut usize,
    limit: usize,
) -> Result<Vec<T>, FnxExecutionError<C>> {
    let bytes = n
        .checked_mul(size_of::<T>())
        .and_then(|bytes| workspace.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", bytes, limit)?;
    let values = reserve(n)?;
    let actual = values
        .capacity()
        .checked_mul(size_of::<T>())
        .and_then(|bytes| workspace.checked_add(bytes))
        .ok_or(FnxExecutionError::SizeOverflow)?;
    admit("kernel workspace bytes", actual, limit)?;
    *workspace = actual;
    Ok(values)
}

impl Dag {
    pub(crate) fn workspace(nodes: usize, predicate: bool) -> Option<usize> {
        let words = if predicate { 1 } else { 2 };
        nodes.checked_mul(size_of::<u8>() + words * size_of::<usize>())
    }

    pub(crate) fn estimated_work(nodes: usize, arcs: usize, columns: usize) -> Option<usize> {
        // Population admission, initialization, roots, DFS transitions,
        // reverse postorder and output. The constant also covers the one
        // Boolean row when the population is empty.
        nodes
            .checked_mul(12usize.checked_add(columns)?)?
            .checked_add(arcs.checked_mul(2)?)?
            .checked_add(columns)?
            .checked_add(32)
    }

    pub(crate) fn new<C>(
        nodes: usize,
        predicate: bool,
        limit: usize,
        row_limit: usize,
    ) -> Result<Self, FnxExecutionError<C>> {
        admit("result rows", if predicate { 1 } else { nodes }, row_limit)?;
        admit(
            "kernel workspace bytes",
            Self::workspace(nodes, predicate).ok_or(FnxExecutionError::SizeOverflow)?,
            limit,
        )?;
        let mut workspace = 0;
        let colors = reserved(nodes, &mut workspace, limit)?;
        let parents = reserved(nodes, &mut workspace, limit)?;
        let order = reserved(if predicate { 0 } else { nodes }, &mut workspace, limit)?;
        Ok(Self {
            colors,
            parents,
            order,
            predicate,
            cyclic: false,
            nodes,
            next_root: 0,
            current: 0,
            depth: 0,
            workspace,
            phase: Phase::Initialize,
            witness: ComplexityWitness {
                algorithm: if predicate {
                    "directed_acyclic_iterative_dfs"
                } else {
                    "topological_sort_iterative_dfs"
                }
                .to_owned(),
                complexity_claim:
                    "O(|V| + |E|) time and O(|V|) integer workspace; no adjacency copy".to_owned(),
                nodes_touched: 0,
                edges_scanned: 0,
                queue_peak: 0,
            },
        })
    }

    pub(crate) fn workspace_bytes(&self) -> usize {
        self.workspace
    }

    pub(crate) fn step<C>(&mut self) -> Result<Step, FnxExecutionError<C>> {
        match self.phase {
            Phase::Initialize => {
                if self.colors.len() == self.nodes {
                    self.phase = Phase::Roots;
                } else {
                    self.colors.push(0);
                    self.parents.push(usize::MAX);
                }
                Ok(Step::Scalar)
            }
            Phase::Roots => {
                if self.next_root == self.nodes {
                    self.phase = if self.predicate || self.cyclic {
                        Phase::Complete
                    } else {
                        Phase::Reverse(0)
                    };
                } else {
                    let root = self.next_root;
                    self.next_root += 1;
                    if self.colors[root] == 0 {
                        self.enter(root)?;
                    }
                }
                Ok(Step::Scalar)
            }
            Phase::Open => {
                self.phase = Phase::Row;
                Ok(Step::Open(self.current))
            }
            Phase::Row => Ok(Step::Row(self.current)),
            Phase::Reverse(index) => {
                if index == self.order.len() / 2 {
                    self.phase = Phase::Complete;
                } else {
                    let last = self.order.len() - index - 1;
                    self.order.swap(index, last);
                    self.phase = Phase::Reverse(index + 1);
                }
                Ok(Step::Scalar)
            }
            Phase::Complete => Ok(Step::Complete),
        }
    }

    fn enter<C>(&mut self, vertex: usize) -> Result<(), FnxExecutionError<C>> {
        self.witness.nodes_touched = self
            .witness
            .nodes_touched
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        self.colors[vertex] = 1;
        self.current = vertex;
        self.depth = self
            .depth
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        self.witness.queue_peak = self.witness.queue_peak.max(self.depth);
        self.phase = Phase::Open;
        Ok(())
    }

    pub(crate) fn visit<C>(&mut self, neighbor: usize) -> Result<(), FnxExecutionError<C>> {
        if self.phase != Phase::Row || neighbor >= self.nodes {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        self.witness.edges_scanned = self
            .witness
            .edges_scanned
            .checked_add(1)
            .ok_or(FnxExecutionError::SizeOverflow)?;
        match self.colors[neighbor] {
            0 => {
                self.parents[neighbor] = self.current;
                self.enter(neighbor)?;
            }
            1 => self.cyclic = true,
            2 => {}
            _ => return Err(FnxExecutionError::InvalidUpstreamResult),
        }
        Ok(())
    }

    pub(crate) fn end_row<C>(&mut self) -> Result<(), FnxExecutionError<C>> {
        if self.phase != Phase::Row || self.depth == 0 {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        let vertex = self.current;
        self.colors[vertex] = 2;
        if !self.predicate {
            self.order.push(vertex);
        }
        let parent = self.parents[vertex];
        self.depth -= 1;
        self.phase = if parent == usize::MAX {
            Phase::Roots
        } else {
            self.current = parent;
            Phase::Row
        };
        Ok(())
    }

    pub(crate) fn finish<C>(self) -> Result<KernelOutput, FnxExecutionError<C>> {
        if self.phase != Phase::Complete
            || self.depth != 0
            || self.witness.nodes_touched != self.nodes
            || (!self.predicate && self.order.len() != self.nodes)
        {
            return Err(FnxExecutionError::InvalidUpstreamResult);
        }
        if !self.predicate && self.cyclic {
            return Err(FnxExecutionError::DirectedCycle);
        }
        Ok(KernelOutput {
            values: if self.predicate {
                KernelValues::Acyclic(!self.cyclic)
            } else {
                KernelValues::TopologicalOrder(self.order)
            },
            row_count: if self.predicate { 1 } else { self.nodes },
            witness: self.witness,
        })
    }
}

pub(crate) fn run<C>(
    graph: &SnapshotGraphView,
    predicate: bool,
    row_limit: usize,
    checkpoint: &mut impl FnxMeter<C>,
) -> Result<(KernelOutput, usize), FnxExecutionError<C>> {
    checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
    let nodes = graph.node_count();
    let mut state = Dag::new(nodes, predicate, usize::MAX, row_limit)?;
    let mut workspace = state.workspace_bytes();
    // Immutable decoded slices resume by ordinal position. The compressed
    // driver instead retains authenticated cursors only along the DFS path.
    let mut positions = reserved(nodes, &mut workspace, usize::MAX)?;
    for _ in 0..nodes {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        positions.push(0usize);
    }
    loop {
        checkpoint.unit().map_err(FnxExecutionError::Cancelled)?;
        match state.step()? {
            Step::Scalar | Step::Open(_) => {}
            Step::Complete => break,
            Step::Row(vertex) => {
                let row = graph
                    .neighbors_indices(vertex)
                    .ok_or(FnxExecutionError::InvalidUpstreamResult)?;
                if let Some(&neighbor) = row.get(positions[vertex]) {
                    positions[vertex] += 1;
                    state.visit(neighbor)?;
                } else {
                    state.end_row()?;
                }
            }
        }
    }
    Ok((state.finish()?, workspace))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(rows: &[Vec<usize>], predicate: bool) -> Result<KernelOutput, FnxExecutionError<()>> {
        let mut state = Dag::new(rows.len(), predicate, usize::MAX, usize::MAX)?;
        let mut positions = vec![0; rows.len()];
        let mut steps = 0;
        loop {
            steps += 1;
            match state.step()? {
                Step::Scalar | Step::Open(_) => {}
                Step::Complete => break,
                Step::Row(vertex) => {
                    if let Some(&neighbor) = rows[vertex].get(positions[vertex]) {
                        positions[vertex] += 1;
                        state.visit(neighbor)?;
                    } else {
                        state.end_row()?;
                    }
                }
            }
        }
        assert!(
            steps <= Dag::estimated_work(rows.len(), rows.iter().map(Vec::len).sum(), 2).unwrap()
        );
        let output = state.finish()?;
        assert_eq!(output.witness.nodes_touched, rows.len());
        assert_eq!(
            output.witness.edges_scanned,
            rows.iter().map(Vec::len).sum::<usize>()
        );
        Ok(output)
    }

    #[test]
    fn deep_dependency_chain_uses_no_rust_recursion() {
        let nodes = 32768;
        let mut rows = vec![Vec::new(); nodes];
        for (vertex, row) in rows.iter_mut().enumerate().skip(1) {
            row.push(vertex - 1);
        }
        let result = drive(&rows, false).unwrap();
        let KernelValues::TopologicalOrder(order) = result.values else {
            panic!("ordering")
        };
        assert_eq!(order, (0..nodes).rev().collect::<Vec<_>>());
        // Reverse the path so one DFS actually reaches the entire depth.
        let mut rows = vec![Vec::new(); nodes];
        for (vertex, row) in rows.iter_mut().enumerate().take(nodes - 1) {
            row.push(vertex + 1);
        }
        let result = drive(&rows, false).unwrap();
        assert_eq!(result.witness.queue_peak, nodes);
        let KernelValues::TopologicalOrder(order) = result.values else {
            panic!("ordering")
        };
        assert_eq!(order, (0..nodes).collect::<Vec<_>>());
    }

    #[test]
    fn workspace_and_invalid_late_ordinals_are_checked_even_after_a_cycle() {
        for predicate in [false, true] {
            let minimum = Dag::workspace(100, predicate).unwrap();
            let state = Dag::new::<()>(100, predicate, minimum, 100).unwrap();
            assert_eq!(state.workspace_bytes(), minimum);
            assert!(matches!(
                Dag::new::<()>(100, predicate, minimum - 1, 100),
                Err(FnxExecutionError::LimitExceeded {
                    resource: "kernel workspace bytes",
                    ..
                })
            ));
            assert!(matches!(
                drive(&[vec![0], vec![], vec![3]], predicate),
                Err(FnxExecutionError::InvalidUpstreamResult)
            ));
        }
    }
}
